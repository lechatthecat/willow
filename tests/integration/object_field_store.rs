//! Inline reference field stores (willow-8hq4.22): object, enum-payload and
//! interface-box stores reuse the header-flag barrier filter of reference
//! array stores (willow-8hq4.15); globals keep the unconditional call.

use super::support::*;
use std::time::Duration;

const EXAMPLE_OUTPUT: &str =
    "11\nfield-store\n36\n5\n9\n3\n200\nnone\n200\nsubclass\n42\n1999000\n";

#[test]
fn object_field_store_example_in_debug_release_and_gc_stress() {
    let source = include_str!("../../example/object_field_store.wi");
    for (out, ok) in [
        compile_and_run(source),
        compile_and_run_release(source),
        compile_and_run_gc_stress(source),
        compile_and_run_gc_stress_mode(source, "minor"),
    ] {
        assert!(ok, "{out}");
        assert_eq!(out, EXAMPLE_OUTPUT);
    }
}

#[test]
fn object_field_store_example_passes_barrier_verification() {
    let source = include_str!("../../example/object_field_store.wi");
    for env in [
        &[("WILLOW_GC_VERIFY_BARRIER", "1")][..],
        &[
            ("WILLOW_GC_VERIFY_BARRIER", "1"),
            ("WILLOW_GC_STRESS", "minor"),
        ][..],
    ] {
        let (out, ok) = compile_and_run_with_runtime_env(source, env, Duration::from_secs(60));
        assert!(ok, "{out}");
        assert_eq!(out, EXAMPLE_OUTPUT);
    }
}

#[test]
fn object_field_store_remembers_old_owner_once_and_skips_young_owners() {
    // A young owner never records an edge. Old values stored into an old
    // owner record none either and skip the call by reading the value's
    // generation byte. An old owner is remembered by its first young store
    // (flag + set entry); re-stores stay on the inline path until a minor
    // collection clears the set after the child tenures.
    let source = r#"
class Node { pub value: i64; }
class Holder { pub node: Node; pub maybe: Option<Node>; }
fn main() {
    let young_hits = gc_write_barrier_hits();
    let young = new Holder(new Node(0), Option::None);
    young.node = new Node(1);
    young.maybe = Option::Some(new Node(2));
    println(gc_write_barrier_hits() == young_hits);
    let h = new Holder(new Node(0), Option::None);
    let old = new Node(4);
    let mut i = 0;
    while i < 4 {
        gc_minor_collect();
        i = i + 1;
    }
    let hits = gc_write_barrier_hits();
    let remembered = gc_remembered_set_size();
    // Old values into the old, unremembered owner skip the call inline.
    h.node = old;
    h.maybe = Option::Some(old);
    h.maybe = Option::None;
    println(gc_write_barrier_hits() == hits && gc_remembered_set_size() == remembered);
    h.node = new Node(7);
    println(gc_write_barrier_hits() == hits + 1);
    println(gc_remembered_set_size() == remembered + 1);
    h.node = new Node(8);
    h.maybe = Option::Some(new Node(3));
    println(gc_write_barrier_hits() == hits + 1);
    let mut j = 0;
    while j < 8 {
        gc_minor_collect();
        j = j + 1;
    }
    println(h.node.value);
    println(gc_remembered_set_size());
    h.node = new Node(9);
    println(gc_write_barrier_hits() == hits + 2);
    gc_minor_collect();
    println(h.node.value);
    match h.maybe { Some(n) => println(n.value), None => println("none") }
    // A young value through a nullable Option field takes the value check
    // and must still remember the owner.
    let o = new Holder(old, Option::None);
    let mut k = 0;
    while k < 4 {
        gc_minor_collect();
        k = k + 1;
    }
    let before = gc_write_barrier_hits();
    o.maybe = Option::Some(new Node(12));
    println(gc_write_barrier_hits() == before + 1);
    gc_minor_collect();
    match o.maybe { Some(n) => println(n.value), None => println("none") }
}
"#;
    for (out, ok) in [
        compile_and_run(source),
        compile_and_run_release(source),
        compile_and_run_with_runtime_env(
            source,
            &[("WILLOW_GC_VERIFY_BARRIER", "1")],
            Duration::from_secs(30),
        ),
    ] {
        assert!(ok, "{out}");
        assert_eq!(
            out,
            "true\ntrue\ntrue\ntrue\ntrue\n8\n0\ntrue\n9\n3\ntrue\n12\n"
        );
    }
}

#[test]
fn object_field_store_closure_enum_and_interface_owners_keep_heap_consistent() {
    // Closure environments, enum payloads and interface pairs are filtered
    // owners too; deletions and old-to-old stores must keep the heap valid.
    let source = r#"
interface Shape { fn area() -> i64; }
class Square implements Shape { pub side: i64; pub fn area() -> i64 { return self.side; } }
class Node { pub value: i64; }
enum Slot { Empty, Full(Node) }
class Holder { pub shape: Shape; pub slot: Slot; pub maybe: Option<Node>; }
fn main() {
    let keep = new Node(5);
    let h = new Holder(new Square(1), Slot::Empty, Option::Some(new Node(1)));
    gc_collect();
    h.shape = new Square(4);
    h.slot = Slot::Full(new Node(6));
    h.maybe = Option::None;
    h.maybe = Option::Some(keep);
    h.slot = h.slot;
    let f = |x: i64| x + keep.value + h.shape.area();
    gc_minor_collect();
    gc_collect();
    println(h.shape.area());
    match h.slot { Slot::Full(n) => println(n.value), Slot::Empty => println("empty") }
    match h.maybe { Some(n) => println(n.value), None => println("none") }
    println(f(0));
}
"#;
    for (out, ok) in [
        compile_and_run(source),
        compile_and_run_release(source),
        compile_and_run_gc_stress(source),
        compile_and_run_with_runtime_env(
            source,
            &[
                ("WILLOW_GC_VERIFY_BARRIER", "1"),
                ("WILLOW_GC_STRESS", "minor"),
            ],
            Duration::from_secs(30),
        ),
    ] {
        assert!(ok, "{out}");
        assert_eq!(out, "4\n6\n5\n9\n");
    }
}

#[test]
fn object_field_store_during_concurrent_marking_preserves_snapshot() {
    // Workers overwrite and delete fields while main runs major collections,
    // so stores observe an active SATB phase and must log overwritten values.
    let source = r#"
class Node { pub value: i64; }
class Pair { pub a: Node; pub b: Node; pub maybe: Option<Node>; }
async fn churn(seed: i64) -> i64 {
    let p = new Pair(new Node(0), new Node(0), Option::None);
    let mut i = 0;
    while i < 20000 {
        let moved = p.a;
        p.a = new Node(seed + i);
        p.b = moved;
        p.maybe = i % 2 == 0 ? Option::Some(moved) : Option::None;
        i = i + 1;
    }
    return p.a.value % 1000 + p.b.value % 1000;
}
async fn main() {
    let a = churn(0);
    let b = churn(1000000);
    let c = churn(2000000);
    let mut k = 0;
    while k < 20 {
        gc_collect();
        await sleep(1);
        k = k + 1;
    }
    let x = await a;
    let y = await b;
    let z = await c;
    println(x == y && y == z);
}
"#;
    for env in [
        &[("WILLOW_WORKERS", "4")][..],
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_VERIFY_BARRIER", "1")][..],
        &[("WILLOW_WORKERS", "4"), ("WILLOW_TASK_BUDGET", "1")][..],
    ] {
        let (out, ok) = compile_and_run_with_runtime_env(source, env, Duration::from_secs(60));
        assert!(ok, "{out}");
        assert_eq!(out, "true\n");
    }
}

#[test]
fn object_field_store_sites_add_one_cold_barrier_and_phase_load_each() {
    // Count relocation targets for 1/8/32 store sites. A reference field
    // store adds exactly one cold barrier call and one mark-phase load per
    // site relative to an i64 field store, and no root or panic bracket. A
    // static (global) store keeps one unconditional barrier call and never
    // reads the mark phase.
    const TRACKED: [&str; 5] = [
        "willow_push_root",
        "willow_pop_roots",
        "willow_panic_depth",
        "willow_gc_write_barrier",
        "willow_gc_mark_phase",
    ];
    let counts = |body: &str, sites: usize| {
        let source = format!(
            "class Node {{ pub v: i64; }} \
             class Box {{ pub n: Node; pub i: i64; pub static mut g: Node = new Node(0); }} \
             fn work(b: Box, n: Node, i: i64) {{ {} }} \
             fn main() {{ work(new Box(new Node(1), 0), new Node(2), 3); }}",
            body.repeat(sites),
        );
        let names = compile_and_collect_relocation_targets_mode(&source, &[], true);
        TRACKED.map(|target| names.iter().filter(|name| *name == target).count())
    };
    let scalar = [1, 8, 32].map(|sites| counts("b.i = i;", sites));
    let reference = [1, 8, 32].map(|sites| counts("b.n = n;", sites));
    let global = [1, 8, 32].map(|sites| counts("Box::g = n;", sites));
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
        assert_eq!(per_site(&reference, 3), added, "{reference:?}");
        assert_eq!(per_site(&reference, 4), added, "{reference:?}");
        assert_eq!(per_site(&global, 3), added, "{global:?}");
        assert_eq!(per_site(&global, 4), 0, "{global:?}");
    }
    assert_eq!(
        scalar[2][4], scalar[0][4],
        "scalar stores never read the mark phase"
    );
}

#[test]
fn old_string_and_array_owner_stores_keep_heap_consistent() {
    // willow-jz15.53: String values and opaque array-element owners are
    // headered GC payloads, so their stores into old fields and async frame
    // slots read the value's generation and skip the barrier call for old
    // values outside marking. Young values must still remember the owner, and
    // the snapshot must stay intact while marking.
    let source = r#"
import std::collections::Array;
class Holder { pub text: String; }
fn bump(x: &mut i64) { x = x + 1; }
async fn frame_stores(old: String) -> i64 {
    let mut text = old;
    let mut counts: Array<i64> = [0, 0, 0];
    let mut i = 0;
    while i < 40 {
        text = old;
        await sleep(0);
        bump(&counts[i % 3]);
        text = text + "!";
        await sleep(0);
        if i % 10 == 0 {
            gc_minor_collect();
        }
        if i % 13 == 0 {
            gc_collect();
        }
        i = i + 1;
    }
    return text.len() * 1000 + counts[0] * 100 + counts[1] * 10 + counts[2];
}
async fn main() {
    let h = new Holder("seed");
    let old = "o" + "ld";
    let mut i = 0;
    while i < 4 {
        gc_minor_collect();
        i = i + 1;
    }
    // Old string into an old owner: no edge, inline skip.
    h.text = old;
    println(h.text);
    // A young string still remembers the owner and survives a minor cycle.
    h.text = old + "-young";
    gc_minor_collect();
    println(h.text);
    println(await frame_stores(old));
}
"#;
    const EXPECTED: &str = "old\nold-young\n5543\n";
    for (out, ok) in [
        compile_and_run(source),
        compile_and_run_release(source),
        compile_and_run_gc_stress(source),
        compile_and_run_gc_stress_mode(source, "minor"),
        compile_and_run_with_runtime_env(
            source,
            &[("WILLOW_GC_VERIFY_BARRIER", "1")],
            Duration::from_secs(60),
        ),
        compile_and_run_with_runtime_env(
            source,
            &[
                ("WILLOW_GC_VERIFY_BARRIER", "1"),
                ("WILLOW_GC_STRESS", "minor"),
            ],
            Duration::from_secs(60),
        ),
    ] {
        assert!(ok, "{out}");
        assert_eq!(out, EXPECTED);
    }
}
