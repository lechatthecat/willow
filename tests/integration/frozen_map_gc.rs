//! `Map::freeze()` under a moving collection (willow-9tls.8).
//!
//! A map's reference values are GC children reported by its trace hook. A
//! class instance made by compiled code is young, and a minor collection MOVES
//! a young value that is reachable only through the map: the rooted map is
//! pinned, the value is copied to the old generation, and the map's slot is
//! rewritten. `freeze()` allocates the frozen copy, and any allocation can run
//! a collection — on the freezing thread, or by parking at a safepoint while a
//! pool worker stops the world.
//!
//! The runtime used to snapshot the value words out of the source BEFORE that
//! allocation and insert the pre-move addresses into the copy, so a frozen map
//! made at the wrong moment named reclaimed nursery memory. The deterministic
//! window is a runtime unit test (`map::tests::copy_stores_values_as_relocated
//! _by_a_collection_inside_its_allocation`); these perspectives cover the
//! shapes that reach `freeze()` from Willow code with young values, and read
//! every value back through the FROZEN copy after further collections, so a
//! stale word is a wrong number or an abort rather than a flake.
//!
//! Only the pool perspectives (7, 8, 15, 16, 17) can reach the window: a
//! runtime allocation never minor-collects on its own thread, and `alloc`
//! stress is a non-moving major collection, so the single-threaded
//! perspectives guard the copy's contents rather than its timing.
//!
//! 17 perspectives:
//!   1 class values, minor stress          10 string keys with class values
//!   2 string values, minor stress         11 an empty map frozen under stress
//!   3 explicit collect between the two    12 two freezes of one source agree
//!   4 100 freezes in a loop               13 the frozen copy in an object field
//!   5 the source mutated after freeze     14 the example program, plain
//!   6 frozen map across an await          15 the example, minor stress, 4 workers
//!   7 pool workers under alloc stress     16 the example, alloc stress, 4 workers
//!   8 pool workers under minor stress     17 pool workers, string values, minor
//!   9 nested class values, two hops

use super::support::{compile_and_run, compile_and_run_with_env};

const PLAIN: [(&str, &str); 0] = [];
const MINOR_STRESS: [(&str, &str); 1] = [("WILLOW_GC_STRESS", "minor")];
const ALLOC_STRESS: [(&str, &str); 1] = [("WILLOW_GC_STRESS", "alloc")];
const POOL_MINOR: [(&str, &str); 2] = [("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "minor")];
const POOL_ALLOC: [(&str, &str); 2] = [("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "alloc")];

const PRELUDE: &str = r#"
import std::collections::Array;
import std::collections::Map;
import std::parallel;

class Node { pub v: i64; }
class Cell { pub n: Node; }
class Holder { pub frozen: FrozenMap<i64, Node>; }

fn node_value(nodes: FrozenMap<i64, Node>, key: i64) -> i64 {
    return match nodes.get(key) {
        Option::Some(n) => n.v,
        Option::None => -1,
    };
}
fn cell_value(cells: FrozenMap<i64, Cell>, key: i64) -> i64 {
    return match cells.get(key) {
        Option::Some(c) => c.n.v,
        Option::None => -1,
    };
}
fn text_value(names: FrozenMap<i64, String>, key: i64) -> String {
    return match names.get(key) {
        Option::Some(s) => s,
        Option::None => "?",
    };
}
fn named_value(nodes: FrozenMap<String, Node>, key: String) -> i64 {
    return match nodes.get(key) {
        Option::Some(n) => n.v,
        Option::None => -1,
    };
}
fn collect_then(v: i64) -> i64 { gc_minor_collect(); return v; }
fn build(count: i64) -> Map<i64, Node> {
    let mut nodes: Map<i64, Node> = Map::new();
    let mut i = 0;
    while i < count {
        nodes.insert(i, new Node(i * 10));
        i = i + 1;
    }
    return nodes;
}
fn sum_frozen(frozen: FrozenMap<i64, Node>, count: i64) -> i64 {
    let mut sum = 0;
    let mut k = 0;
    while k < count {
        sum = sum + node_value(frozen, k);
        k = k + 1;
    }
    return sum;
}
"#;

fn run(body: &str, env: &[(&str, &str)], expected: &str) {
    let source = format!("{PRELUDE}\n{body}");
    let (output, ok) = compile_and_run_with_env(&source, env);
    assert!(ok, "program failed under {env:?}:\n{output}");
    assert_eq!(output, expected, "under {env:?}");
}

/// 1: young class values are read back through the frozen copy after a
/// collection that moves them.
#[test]
fn fmgc_01_class_values_survive_minor_stress() {
    run(
        r#"
fn main() {
    let nodes = build(10);
    let frozen = nodes.freeze();
    gc_minor_collect();
    println(sum_frozen(frozen, 10));
    println(node_value(frozen, 10));
}
"#,
        &MINOR_STRESS,
        "450\n-1\n",
    );
}

/// 2: string values built by the program go through the same slots.
#[test]
fn fmgc_02_string_values_survive_minor_stress() {
    run(
        r#"
fn main() {
    let mut names: Map<i64, String> = Map::new();
    let mut i = 0;
    while i < 8 {
        names.insert(i, "n" + i.toString());
        i = i + 1;
    }
    let texts = names.freeze();
    gc_minor_collect();
    gc_collect();
    println(text_value(texts, 0));
    println(text_value(texts, 7));
    println(text_value(texts, 8));
    println(texts.len());
}
"#,
        &MINOR_STRESS,
        "n0\nn7\n?\n8\n",
    );
}

/// 3: the collection that moves the values sits between the freeze and the
/// first read, in a callee, with no stress mode at all.
#[test]
fn fmgc_03_explicit_collect_between_freeze_and_read() {
    run(
        r#"
fn main() {
    let nodes = build(6);
    let frozen = nodes.freeze();
    let marker = collect_then(7);
    println(marker + sum_frozen(frozen, 6));
}
"#,
        &PLAIN,
        "157\n",
    );
}

/// 4: many freezes, each followed by a collection, in one loop.
#[test]
fn fmgc_04_hundred_freezes_in_a_loop() {
    run(
        r#"
fn main() {
    let mut total = 0;
    let mut round = 0;
    while round < 100 {
        let nodes = build(4);
        nodes.insert(4, new Node(round));
        let frozen = nodes.freeze();
        gc_minor_collect();
        total = total + sum_frozen(frozen, 5);
        round = round + 1;
    }
    println(total);
}
"#,
        &MINOR_STRESS,
        "10950\n",
    );
}

/// 5: the source keeps changing after the freeze; the copy holds the values
/// it was made from, wherever the collector has moved them since.
#[test]
fn fmgc_05_source_mutated_after_freeze() {
    run(
        r#"
fn main() {
    let nodes = build(5);
    let frozen = nodes.freeze();
    nodes.insert(0, new Node(-500));
    nodes.insert(9, new Node(900));
    gc_minor_collect();
    println(sum_frozen(frozen, 5));
    println(node_value(frozen, 9));
    println(nodes.len());
    println(frozen.len());
}
"#,
        &MINOR_STRESS,
        "100\n-1\n6\n5\n",
    );
}

/// 6: the frozen copy crosses an await and is read after collections on both
/// sides of the park.
#[test]
fn fmgc_06_frozen_map_across_an_await() {
    run(
        r#"
async fn later(frozen: FrozenMap<i64, Node>) -> i64 {
    gc_minor_collect();
    await sleep(1);
    gc_minor_collect();
    return sum_frozen(frozen, 8);
}
async fn main() {
    let nodes = build(8);
    let frozen = nodes.freeze();
    gc_minor_collect();
    println(await later(frozen));
}
"#,
        &MINOR_STRESS,
        "280\n",
    );
}

const POOL_BODY: &str = r#"
fn freeze_under_collections(seed: i64) -> i64 {
    if seed % 3 == 0 {
        gc_minor_collect();
    }
    let mut nodes: Map<i64, Node> = Map::new();
    let mut i = 0;
    while i < 16 {
        nodes.insert(i, new Node(seed + i));
        i = i + 1;
    }
    let frozen = nodes.freeze();
    if seed % 2 == 0 {
        gc_minor_collect();
    }
    return sum_frozen(frozen, 16);
}
async fn main() {
    let seeds: Array<i64> = [1, 2, 3, 4, 5, 6, 7, 8];
    let mut total = 0;
    let mut round = 0;
    while round < 3 {
        let sums = await parallel::map(seeds.freeze(), freeze_under_collections);
        let mut i = 0;
        while i < sums.len() {
            total = total + sums[i];
            i = i + 1;
        }
        round = round + 1;
    }
    println(total);
}
"#;

/// 7: pool workers freeze maps of young values while other workers stop the
/// world; every allocation collects. 16*36 + 8*120 = 1536 per round.
#[test]
fn fmgc_07_pool_workers_under_alloc_stress() {
    run(POOL_BODY, &POOL_ALLOC, "4608\n");
}

/// 8: the same pool under minor stress, where the collections move.
#[test]
fn fmgc_08_pool_workers_under_minor_stress() {
    run(POOL_BODY, &POOL_MINOR, "4608\n");
}

/// 9: the copied word is one hop; the object behind it holds another young
/// object, so the read after collection follows two rewritten edges.
#[test]
fn fmgc_09_nested_class_values_two_hops() {
    run(
        r#"
fn main() {
    let mut cells: Map<i64, Cell> = Map::new();
    let mut i = 0;
    while i < 6 {
        cells.insert(i, new Cell(new Node(i + 100)));
        i = i + 1;
    }
    let frozen = cells.freeze();
    gc_minor_collect();
    gc_minor_collect();
    println(cell_value(frozen, 0));
    println(cell_value(frozen, 5));
    println(cell_value(frozen, 6));
}
"#,
        &MINOR_STRESS,
        "100\n105\n-1\n",
    );
}

/// 10: string keys are copied by content and class values by reference.
#[test]
fn fmgc_10_string_keys_with_class_values() {
    run(
        r#"
fn main() {
    let mut nodes: Map<String, Node> = Map::new();
    nodes.insert("a", new Node(1));
    nodes.insert("bb", new Node(22));
    nodes.insert("a" + "a", new Node(11));
    let frozen = nodes.freeze();
    gc_minor_collect();
    gc_collect();
    println(named_value(frozen, "a"));
    println(named_value(frozen, "aa"));
    println(named_value(frozen, "bb"));
    println(named_value(frozen, "c"));
}
"#,
        &MINOR_STRESS,
        "1\n11\n22\n-1\n",
    );
}

/// 11: an empty map has nothing to copy and still freezes under stress.
#[test]
fn fmgc_11_empty_map_frozen_under_stress() {
    run(
        r#"
fn main() {
    let nodes: Map<i64, Node> = Map::new();
    let frozen = nodes.freeze();
    gc_minor_collect();
    println(frozen.len());
    println(node_value(frozen, 0));
}
"#,
        &ALLOC_STRESS,
        "0\n-1\n",
    );
}

/// 12: two copies of one source, one before and one after a collection that
/// moves the values, agree on every entry.
#[test]
fn fmgc_12_two_freezes_of_one_source_agree() {
    run(
        r#"
fn main() {
    let nodes = build(7);
    let first = nodes.freeze();
    gc_minor_collect();
    let second = nodes.freeze();
    gc_minor_collect();
    println(sum_frozen(first, 7));
    println(sum_frozen(second, 7));
    println(sum_frozen(first, 7) == sum_frozen(second, 7));
}
"#,
        &MINOR_STRESS,
        "210\n210\ntrue\n",
    );
}

/// 13: the frozen copy lives in an object field; the holder is the only root.
#[test]
fn fmgc_13_frozen_copy_in_an_object_field() {
    run(
        r#"
fn make_holder() -> Holder {
    let nodes = build(9);
    return new Holder(nodes.freeze());
}
fn main() {
    let holder = make_holder();
    gc_minor_collect();
    gc_collect();
    println(sum_frozen(holder.frozen, 9));
}
"#,
        &MINOR_STRESS,
        "360\n",
    );
}

const EXAMPLE_OUTPUT: &str = "449\ntwo!\n?\n1536\n2\n";

/// 14: the example program, as the example catalog runs it.
#[test]
fn fmgc_14_the_example_program() {
    let source = include_str!("../../example/frozen_map_gc.wi");
    let (output, ok) = compile_and_run(source);
    assert!(ok, "example failed:\n{output}");
    assert_eq!(output, EXAMPLE_OUTPUT);
}

/// 15: the example with moving collections on every nursery refill and four
/// pool workers stopping the world under each other.
#[test]
fn fmgc_15_the_example_under_minor_stress_with_workers() {
    let source = include_str!("../../example/frozen_map_gc.wi");
    let (output, ok) = compile_and_run_with_env(source, &POOL_MINOR);
    assert!(ok, "example failed under minor stress:\n{output}");
    assert_eq!(output, EXAMPLE_OUTPUT);
}

/// 16: the example with a full collection at every allocation and four workers.
#[test]
fn fmgc_16_the_example_under_alloc_stress_with_workers() {
    let source = include_str!("../../example/frozen_map_gc.wi");
    let (output, ok) = compile_and_run_with_env(source, &POOL_ALLOC);
    assert!(ok, "example failed under alloc stress:\n{output}");
    assert_eq!(output, EXAMPLE_OUTPUT);
}

/// 17: string values on pool workers under moving collections — the value
/// words are WillowString pointers, the keys are copied by content.
#[test]
fn fmgc_17_pool_workers_string_values_under_minor_stress() {
    run(
        r#"
fn freeze_names(seed: i64) -> i64 {
    let mut names: Map<i64, String> = Map::new();
    let mut i = 0;
    while i < 8 {
        names.insert(i, "s" + (seed * 100 + i).toString());
        i = i + 1;
    }
    let frozen = names.freeze();
    if seed % 2 == 0 {
        gc_minor_collect();
    }
    let mut hits = 0;
    let mut k = 0;
    while k < 8 {
        if text_value(frozen, k) == "s" + (seed * 100 + k).toString() {
            hits = hits + 1;
        }
        k = k + 1;
    }
    return hits;
}
async fn main() {
    let seeds: Array<i64> = [1, 2, 3, 4, 5, 6, 7, 8];
    let hits = await parallel::map(seeds.freeze(), freeze_names);
    let mut total = 0;
    let mut i = 0;
    while i < hits.len() {
        total = total + hits[i];
        i = i + 1;
    }
    println(total);
}
"#,
        &POOL_MINOR,
        "64\n",
    );
}
