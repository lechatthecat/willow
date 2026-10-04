//! Async frame reference stores (willow-jz15.51): frames are old-region
//! objects addressed by payload start, so stores into their slots reuse the
//! inline header-flag barrier filter of object field stores (willow-8hq4.22)
//! instead of calling `willow_gc_write_barrier`, whose heap lock serialized
//! parallel async workers, for every store.
//!
//! Perspectives covered here and by `example/async_frame_store.wi`:
//! 1. young value into a frame slot across awaits (`repeated_stores`);
//! 2. a young chain built through one slot stays reachable;
//! 3. minor collections between stores clear and re-arm the remembered flag;
//! 4. class, String, Option and Array locals in frames (`shapes`);
//! 5. Option::None (null-like) and Some stores;
//! 6. values stored after the frame was remembered survive a minor collection;
//! 7. parallel workers storing into their own frames (`build`);
//! 8. stores during concurrent major marking (SATB deletions);
//! 9. debug build; 10. release build;
//! 11. `WILLOW_GC_STRESS=all`; 12. `minor`; 13. `relocate`;
//! 14. `WILLOW_GC_VERIFY_BARRIER` (remembered-set completeness);
//! 15. barrier verification under minor stress;
//! 16. four workers with a one-poll task budget (frequent migration);
//! 17. barrier verification with four workers during marking;
//! 18. one mark-phase load per frame reference barrier site (filter present);
//! 19. a constant number of cold barrier calls per source store site;
//! 20. scalar frame stores read no mark phase and call no barrier;
//! 21. no root push/pop or panic bracket added per site;
//! 22. code-size scaling is linear in store sites (1/8/32 sites).

use super::support::*;
use std::time::Duration;

const EXAMPLE_OUTPUT: &str = "201200\n106\n1835976\n";

#[test]
fn async_frame_store_example_in_debug_release_and_gc_stress() {
    let source = include_str!("../../example/async_frame_store.wi");
    for (out, ok) in [
        compile_and_run(source),
        compile_and_run_release(source),
        compile_and_run_gc_stress(source),
        compile_and_run_gc_stress_mode(source, "minor"),
        compile_and_run_gc_stress_mode(source, "relocate"),
    ] {
        assert!(ok, "{out}");
        assert_eq!(out, EXAMPLE_OUTPUT);
    }
}

#[test]
fn async_frame_store_example_passes_barrier_verification() {
    let source = include_str!("../../example/async_frame_store.wi");
    for env in [
        &[("WILLOW_GC_VERIFY_BARRIER", "1")][..],
        &[
            ("WILLOW_GC_VERIFY_BARRIER", "1"),
            ("WILLOW_GC_STRESS", "minor"),
        ][..],
        &[("WILLOW_WORKERS", "4"), ("WILLOW_TASK_BUDGET", "1")][..],
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_VERIFY_BARRIER", "1")][..],
    ] {
        let (out, ok) = compile_and_run_with_runtime_env(source, env, Duration::from_secs(120));
        assert!(ok, "{env:?}: {out}");
        assert_eq!(out, EXAMPLE_OUTPUT, "{env:?}");
    }
}

#[test]
fn async_frame_store_sites_add_one_cold_barrier_and_phase_load_each() {
    // Count relocation targets for 1/8/32 frame store sites. Each site stores
    // a local that lives across an await, so the store goes to the frame.
    // Reference stores add a constant number of filtered barrier sites per
    // source site relative to an i64 store, and no root or panic bracket.
    const TRACKED: [&str; 5] = [
        "willow_push_root",
        "willow_pop_roots",
        "willow_panic_depth",
        "willow_gc_write_barrier",
        "willow_gc_mark_phase",
    ];
    let counts = |init: &str, body: &str, result: &str, sites: usize| {
        let source = format!(
            "class Node {{ pub v: i64; }} \
             async fn work(n: Node, i: i64) -> i64 {{ {init} {} return {result}; }} \
             async fn main() {{ println(await work(new Node(2), 3)); }}",
            body.repeat(sites),
        );
        let names = compile_and_collect_relocation_targets_mode(&source, &[], true);
        TRACKED.map(|target| names.iter().filter(|name| *name == target).count())
    };
    let scalar = [1, 8, 32].map(|sites| {
        counts(
            "let mut local = 0;",
            "local = i; await sleep(0);",
            "local",
            sites,
        )
    });
    let reference = [1, 8, 32].map(|sites| {
        counts(
            "let mut local = n;",
            "local = n; await sleep(0);",
            "local.v",
            sites,
        )
    });
    for sites in 1..3 {
        let added = [1, 8, 32][sites] - 1;
        let per_site =
            |rows: &[[usize; 5]; 3], column: usize| rows[sites][column] - rows[0][column];
        for (column, target) in TRACKED.iter().enumerate().take(3) {
            assert_eq!(
                per_site(&reference, column),
                per_site(&scalar, column),
                "{target}: scalar={scalar:?} reference={reference:?}"
            );
        }
        // Every frame reference store site (the assignment, plus the slot
        // re-store around each await) is filtered: one cold barrier call and
        // one mark-phase load each. Before willow-jz15.51 these sites called
        // the barrier unconditionally and loaded no mark phase.
        let barriers = per_site(&reference, 3);
        assert!(barriers >= added, "{scalar:?} {reference:?}");
        assert_eq!(barriers % added, 0, "{reference:?}");
        assert_eq!(per_site(&reference, 4), barriers, "{reference:?}");
        assert_eq!(per_site(&scalar, 3), 0, "{scalar:?}");
        assert_eq!(per_site(&scalar, 4), 0, "{scalar:?}");
    }
}
