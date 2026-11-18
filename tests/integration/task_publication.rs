//! Task-owned young regions and publication: values become shared (marked
//! in place) exactly when another task can reach them, the
//! generated store filter sends stores into shared owners to the publishing
//! barrier, and the publication verifier accepts every run.

use super::support::*;
use std::time::Duration;

const EXAMPLE_OUTPUT: &str = "499500\n2190\n50\n26\n1006\n2000\n";
const VERIFY: (&str, &str) = ("WILLOW_GC_VERIFY_PUBLICATION", "1");
const STATS: (&str, &str) = ("WILLOW_GC_PUBLICATION_STATS", "1");

/// The `[gc-publication]` summary line's `key=value` counters.
fn stats(stderr: &str) -> std::collections::HashMap<String, u64> {
    let line = stderr
        .lines()
        .find(|line| line.starts_with("[gc-publication]"))
        .unwrap_or_else(|| panic!("no publication summary in stderr: {stderr}"));
    let mut counters = std::collections::HashMap::new();
    for field in line.split_whitespace().skip(1) {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        if key == "per_point" {
            for point in value.split(',') {
                let (name, calls) = point.split_once('=').unwrap();
                counters.insert(name.to_string(), calls.parse().unwrap());
            }
        } else if let Ok(value) = value.parse() {
            counters.insert(key.to_string(), value);
        }
    }
    counters
}

fn run_with_stats(
    source: &str,
    env: &[(&str, &str)],
) -> (String, std::collections::HashMap<String, u64>) {
    let mut env = env.to_vec();
    env.push(STATS);
    let run = compile_and_run_with_runtime_env_split(source, &env, Duration::from_secs(120));
    assert!(run.ok, "{env:?}: {}{}", run.stdout, run.stderr);
    let counters = stats(&run.stderr);
    (run.stdout, counters)
}

#[test]
fn task_publication_example_in_debug_release_and_gc_stress() {
    let source = include_str!("../../tests/regressions/task_publication.wi");
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
fn task_publication_example_passes_the_verifier_at_every_point() {
    let source = include_str!("../../tests/regressions/task_publication.wi");
    for env in [
        &[VERIFY][..],
        &[VERIFY, ("WILLOW_GC_STRESS", "minor")][..],
        &[
            VERIFY,
            ("WILLOW_GC_STRESS", "minor"),
            ("WILLOW_WORKERS", "4"),
        ][..],
        &[
            VERIFY,
            ("WILLOW_GC_STRESS", "scheduler"),
            ("WILLOW_WORKERS", "4"),
        ][..],
        &[("WILLOW_GC_STRESS", "all"), ("WILLOW_WORKERS", "4")][..],
    ] {
        let (out, counters) = run_with_stats(source, env);
        assert_eq!(out, EXAMPLE_OUTPUT, "{env:?}");
        // Each point the example exercises published at least once: 20
        // sends, the producer spawn, the lock-protected array and the stores
        // into shared objects. Task results (the builder's 1000-item list,
        // the producer's count) are handed over to main, never traversed.
        assert!(counters["ChannelSend"] >= 20, "{env:?} {counters:?}");
        assert!(counters["Spawn"] >= 1, "{env:?} {counters:?}");
        assert!(counters["LockedState"] >= 1, "{env:?} {counters:?}");
        assert!(counters["SharedStore"] >= 50, "{env:?} {counters:?}");
        assert!(counters["handed_over"] >= 2, "{env:?} {counters:?}");
        assert_eq!(counters["TaskReturn"], 0, "{env:?} {counters:?}");
        // Only the 20 two-item messages and the stored values are marked:
        // the handed-over list never is. Items that were young when
        // published count as promoted, and only those count when they die.
        assert!(counters["marked_objects"] >= 40, "{env:?} {counters:?}");
        assert!(counters["marked_objects"] < 1000, "{env:?} {counters:?}");
        assert!(
            counters["promoted_died_bytes"] <= counters["promoted_bytes"],
            "{env:?} {counters:?}"
        );
    }
}

#[test]
fn private_long_lived_container_promotes_nothing() {
    let source = "import std::collections::Array; \
        class Item { pub value: i64; pub next: Option<Item>; } \
        fn main() { \
            let private: Array<Item> = []; \
            let mut i = 0; \
            while i < 20000 { \
                private.push(new Item(i, Option::None)); \
                if private.len() > 1000 { private.pop(); } \
                i = i + 1; \
            } \
            gc_minor_collect(); \
            println(private.len()); \
        }";
    for env in [&[VERIFY][..], &[VERIFY, ("WILLOW_GC_STRESS", "minor")][..]] {
        let (out, counters) = run_with_stats(source, env);
        assert_eq!(out, "1000\n", "{env:?}");
        assert_eq!(counters["calls"], 0, "{env:?} {counters:?}");
        assert_eq!(counters["promoted_bytes"], 0, "{env:?} {counters:?}");
        assert_eq!(counters["marked_objects"], 0, "{env:?} {counters:?}");
    }
}

/// A program that receives one object through a channel (so it is shared),
/// runs `setup` once and then `body` `sites` times on it.
fn shared_store_program(setup: &str, body: &str, sites: usize) -> String {
    format!(
        "class Item {{ pub value: i64; pub next: Option<Item>; }} \
         async fn source(results: Channel<Item>) -> i64 {{ \
             results.send(new Item(1, Option::None)); return 0; }} \
         async fn main() {{ \
             let results = Channel<Item>::new(); \
             let done = source(results); \
             let shared = results.recv(); \
             await done; \
             let kept = new Item(5, Option::None); \
             {setup} {} \
             gc_minor_collect(); \
             println(shared.value); \
         }}",
        body.repeat(sites)
    )
}

#[test]
fn generated_stores_into_shared_owners_publish_each_young_value() {
    // Each store of a fresh young value into the shared owner reaches the
    // publishing barrier: SharedStore grows by exactly one per site.
    let counts = [1, 8, 32].map(|sites| {
        let source = shared_store_program(
            "",
            "shared.next = Option::Some(new Item(2, Option::None)); shared.value = 3;",
            sites,
        );
        run_with_stats(&source, &[VERIFY, ("WILLOW_GC_STRESS", "minor")]).1
    });
    for (index, sites) in [1u64, 8, 32].into_iter().enumerate() {
        assert_eq!(
            counts[index]["SharedStore"] - counts[0]["SharedStore"],
            sites - 1,
            "{counts:?}"
        );
    }
}

#[test]
fn generated_store_of_a_shared_value_skips_the_barrier() {
    // Once `kept` is shared, storing it again into a shared owner is filtered
    // inline (the value's SHARED byte): no call, no republish, no scan.
    let counts = [1, 8, 32].map(|sites| {
        let store = "shared.next = Option::Some(kept);";
        let source = shared_store_program(store, store, sites);
        run_with_stats(&source, &[VERIFY]).1
    });
    for counters in &counts {
        assert_eq!(counters["calls"], counts[0]["calls"], "{counts:?}");
        assert_eq!(
            counters["scanned_slots"], counts[0]["scanned_slots"],
            "{counts:?}"
        );
    }
}

#[test]
fn stores_into_private_owners_never_publish() {
    // Young and private old owners: no publication however many stores.
    let source = "class Item { pub value: i64; pub next: Option<Item>; } \
        fn main() { \
            let old = new Item(0, Option::None); \
            gc_minor_collect(); gc_minor_collect(); gc_minor_collect(); gc_minor_collect(); \
            let mut i = 0; \
            while i < 1000 { \
                old.next = Option::Some(new Item(i, Option::None)); \
                let young = new Item(i, Option::None); \
                young.next = Option::Some(new Item(i, Option::None)); \
                i = i + 1; \
            } \
            gc_minor_collect(); \
            println(old.value); \
        }";
    let (out, counters) = run_with_stats(source, &[VERIFY, ("WILLOW_GC_STRESS", "minor")]);
    assert_eq!(out, "0\n");
    assert_eq!(counters["calls"], 0, "{counters:?}");
    assert_eq!(counters["promoted_bytes"], 0, "{counters:?}");
}

#[test]
fn global_store_publishes_its_value() {
    let source = "class Item { pub value: i64; pub next: Option<Item>; } \
        class Box { pub static mut g: Item = new Item(0, Option::None); } \
        fn main() { \
            Box::g = new Item(1, Option::Some(new Item(2, Option::None))); \
            gc_minor_collect(); \
            println(Box::g.value); \
        }";
    let (out, counters) = run_with_stats(
        source,
        &[
            VERIFY,
            ("WILLOW_GC_STRESS", "minor"),
            ("WILLOW_WORKERS", "1"),
        ],
    );
    assert_eq!(out, "1\n");
    assert!(counters["Global"] >= 1, "{counters:?}");
    assert!(counters["promoted_objects"] >= 2, "{counters:?}");
}

#[test]
fn deep_task_result_is_handed_over_then_publishes_without_stack_exhaustion() {
    // A 200k-node list returned by a task is handed over to the awaiting
    // task in O(1); sending it on then publishes it with the iterative
    // traversal on the worker's ordinary stack.
    let source = "class Item { pub value: i64; pub next: Option<Item>; } \
        async fn build(count: i64) -> Item { \
            let mut head = new Item(0, Option::None); \
            let mut i = 1; \
            while i < count { head = new Item(i, Option::Some(head)); i = i + 1; } \
            return head; \
        } \
        async fn count(ch: Channel<Item>) -> i64 { \
            let mut n = 0; \
            let mut cursor = Option::Some(ch.recv()); \
            while true { match cursor { Some(item) => { n = n + 1; cursor = item.next; } None => { break; } } } \
            return n; \
        } \
        async fn main() { \
            let head = await build(200000); \
            let ch = Channel<Item>::new(); \
            let counter = count(ch); \
            ch.send(head); \
            println(await counter); \
        }";
    for env in [
        &[VERIFY][..],
        &[VERIFY, ("WILLOW_WORKERS", "4")][..],
        &[
            VERIFY,
            ("WILLOW_GC_STRESS", "minor"),
            ("WILLOW_WORKERS", "4"),
        ][..],
    ] {
        let (out, counters) = run_with_stats(source, env);
        assert_eq!(out, "200000\n", "{env:?}");
        assert_eq!(counters["TaskReturn"], 0, "{env:?} {counters:?}");
        assert!(counters["handed_over"] >= 2, "{env:?} {counters:?}");
        assert!(counters["ChannelSend"] >= 1, "{env:?} {counters:?}");
        assert!(counters["marked_objects"] >= 200000, "{env:?} {counters:?}");
    }
}

#[test]
fn blocking_send_of_shared_values_survives_moving_minors() {
    // A synchronous send into a full bounded channel drives the scheduler
    // (a GC point; the allocating consumer triggers minors under stress)
    // while it holds the channel and the published value. Shared values move
    // like any young object, so the send reloads both from runtime roots.
    let source = "class Item { pub value: i64; pub next: Option<Item>; } \
        async fn consume(ch: Channel<Item>, done: Channel<i64>, rounds: i64) { \
            let mut sum = 0; \
            let mut i = 0; \
            while i < rounds { \
                let item = ch.recv(); \
                let pad = new Item(i, Option::None); \
                match item.next { Some(inner) => { sum = sum + item.value + inner.value + pad.value - i; } None => {} } \
                i = i + 1; \
            } \
            done.send(sum); \
        } \
        fn main() { \
            let ch = Channel<Item>::with_capacity(1); \
            let done = Channel<i64>::new(); \
            let consumer = consume(ch, done, 2000); \
            let mut i = 0; \
            while i < 2000 { ch.send(new Item(i, Option::Some(new Item(i, Option::None)))); i = i + 1; } \
            println(done.recv()); \
        }";
    for env in [
        &[VERIFY][..],
        &[VERIFY, ("WILLOW_GC_STRESS", "minor")][..],
        &[
            VERIFY,
            ("WILLOW_GC_STRESS", "scheduler"),
            ("WILLOW_WORKERS", "4"),
        ][..],
    ] {
        let (out, counters) = run_with_stats(source, env);
        assert_eq!(out, "3998000\n", "{env:?}");
        assert!(counters["ChannelSend"] >= 2000, "{env:?} {counters:?}");
    }
}
