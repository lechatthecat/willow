//! Inline reference-element array stores (willow-8hq4.15): bounds check,
//! header-flag barrier filter, and no per-store root or panic bracket.

use super::support::*;
use std::time::Duration;

const EXAMPLE_OUTPUT: &str = "33\n200\n2101\nleft-right\n36\n9\n5\n8\n42\n12\n79990\ncaught\n";

#[test]
fn array_reference_store_example_in_debug_release_and_gc_stress() {
    let source = include_str!("../../example/array_reference_store.wi");
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
fn array_reference_store_example_passes_barrier_verification() {
    let source = include_str!("../../example/array_reference_store.wi");
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
fn array_reference_store_bounds_panic_for_every_reference_kind() {
    let mut source = String::from(
        "import std::collections::Array;\n\
         interface Shape { fn area() -> i64; }\n\
         class Square implements Shape { pub side: i64; pub fn area() -> i64 { return self.side; } }\n\
         fn main() {\n",
    );
    let mut expected = String::new();
    for (ty, array, value, index) in [
        ("Array<Square>", "[new Square(1)]", "new Square(2)", "-1"),
        ("Array<Square>", "[new Square(1)]", "new Square(2)", "1"),
        ("Array<Square>", "[]", "new Square(2)", "0"),
        ("Array<String>", "[\"a\"]", "\"b\"", "1"),
        ("Array<String>", "[\"a\"]", "\"b\"", "-9223372036854775807"),
        ("Array<Shape>", "[new Square(1)]", "new Square(2)", "5"),
        (
            "Array<Option<Square>>",
            "[Option::None]",
            "Option::Some(new Square(2))",
            "1",
        ),
        ("Array<Array<i64>>", "[[1]]", "[2]", "1"),
    ] {
        source.push_str(&format!(
            r#"
    if true {{
        defer match recover() {{ Some(_) => println("caught"), None => println("missing") }}
        defer println("cleanup");
        let xs: {ty} = {array};
        let i = {index};
        xs[i] = {value};
        println("unreachable");
    }}
"#
        ));
        expected.push_str("cleanup\ncaught\n");
    }
    source.push('}');
    for (out, ok) in [
        compile_and_run(&source),
        compile_and_run_release(&source),
        compile_and_run_gc_stress(&source),
    ] {
        assert!(ok, "{out}");
        assert_eq!(out, expected);
    }
}

#[test]
fn array_reference_store_remembers_old_buffer_and_tenures_child() {
    // The buffer is old. A young store must remember it (flag + set entry);
    // a repeated store into the remembered buffer stays on the inline path
    // and the remembered child survives minor collections until it tenures.
    let source = r#"
import std::collections::Array;
class Node { pub value: i64; }
fn main() {
    let xs: Array<Node> = [new Node(0)];
    gc_minor_collect();
    gc_minor_collect();
    let hits = gc_write_barrier_hits();
    let remembered = gc_remembered_set_size();
    xs[0] = new Node(7);
    println(gc_write_barrier_hits() == hits + 1);
    println(gc_remembered_set_size() == remembered + 1);
    xs[0] = new Node(8);
    println(gc_write_barrier_hits() == hits + 1);
    let mut i = 0;
    while i < 8 {
        gc_minor_collect();
        i = i + 1;
    }
    println(xs[0].value);
    println(gc_remembered_set_size());
    xs[0] = new Node(9);
    println(gc_write_barrier_hits() == hits + 2);
    gc_minor_collect();
    println(xs[0].value);
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
        assert_eq!(out, "true\ntrue\ntrue\n8\n0\ntrue\n9\n");
    }
}

#[test]
fn array_reference_store_old_values_and_deletions_keep_heap_consistent() {
    // Old-to-old stores, deletions through Option::None, and self-aliasing
    // stores all take the inline path outside marking.
    let source = r#"
import std::collections::Array;
class Node { pub value: i64; }
fn main() {
    let keep = new Node(5);
    let xs: Array<Option<Node>> = [Option::Some(new Node(1)), Option::None];
    gc_collect();
    xs[1] = xs[0];
    xs[0] = Option::None;
    xs[0] = Option::Some(keep);
    xs[0] = xs[0];
    gc_minor_collect();
    gc_collect();
    match xs[0] { Some(n) => println(n.value), None => println("none") }
    match xs[1] { Some(n) => println(n.value), None => println("none") }
}
"#;
    for (out, ok) in [
        compile_and_run(source),
        compile_and_run_release(source),
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
        assert_eq!(out, "5\n1\n");
    }
}

#[test]
fn array_reference_store_during_concurrent_marking_preserves_snapshot() {
    // Workers overwrite and delete elements while main runs major
    // collections, so stores observe an active SATB phase and must call the
    // runtime barrier to log the overwritten values.
    let source = r#"
import std::collections::Array;
class Node { pub value: i64; }
async fn churn(seed: i64) -> i64 {
    let xs: Array<Node> = [new Node(0), new Node(0), new Node(0), new Node(0)];
    let mut i = 0;
    while i < 20000 {
        let moved = xs[(i + 1) % 4];
        xs[i % 4] = new Node(seed + i);
        xs[(i + 2) % 4] = moved;
        i = i + 1;
    }
    let mut total = 0;
    let mut j = 0;
    while j < 4 {
        total = total + xs[j].value % 1000;
        j = j + 1;
    }
    return total;
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
fn array_reference_store_sites_match_scalar_store_runtime_calls() {
    // Count relocation targets for 1/8/32 store sites of `Array<i64>` and
    // `Array<Node>`. The reference store must add exactly the per-site calls
    // of the i64 store (its cold `willow_array_set` slow path) plus the frame
    // slot of its loaded array temporary, one cold barrier call and one
    // mark-phase load: no extra root pair and no hot panic-depth bracket.
    // Scalar stores form one GC-free run, so their array temporaries need no
    // root at all (willow-ijui.10).
    const TRACKED: [&str; 6] = [
        "willow_push_root",
        "willow_pop_roots",
        "willow_panic_depth",
        "willow_array_set",
        "willow_gc_write_barrier",
        "willow_gc_mark_phase",
    ];
    let counts = |element: &str, array: &str, value: &str, sites: usize| {
        let source = format!(
            "import std::collections::Array; class Node {{ pub v: i64; }} \
             fn work(xs: Array<{element}>, n: {element}, i: i64) {{ {} }} \
             fn main() {{ work({array}, {value}, 0); }}",
            "xs[i] = n;".repeat(sites),
        );
        let names = compile_and_collect_relocation_targets_mode(&source, &[], true);
        TRACKED.map(|target| names.iter().filter(|name| *name == target).count())
    };
    let scalar = [1, 8, 32].map(|sites| counts("i64", "[1]", "2", sites));
    let reference = [1, 8, 32].map(|sites| counts("Node", "[new Node(1)]", "new Node(2)", sites));
    for sites in 1..3 {
        let per_site =
            |rows: &[[usize; 6]; 3], column: usize| rows[sites][column] - rows[0][column];
        for (column, target) in TRACKED.iter().enumerate().take(4).skip(1) {
            assert_eq!(
                per_site(&reference, column),
                per_site(&scalar, column),
                "{target}: scalar={scalar:?} reference={reference:?}"
            );
        }
        assert_eq!(
            per_site(&reference, 1),
            0,
            "per-store pop_roots: {reference:?}"
        );
        let added = [1, 8, 32][sites] - 1;
        assert_eq!(per_site(&scalar, 0), 0, "scalar store roots: {scalar:?}");
        assert_eq!(per_site(&reference, 0), added, "{reference:?}");
        assert_eq!(per_site(&reference, 4), added, "{reference:?}");
        assert_eq!(per_site(&reference, 5), added, "{reference:?}");
    }
    assert_eq!(scalar[0][5], 0, "scalar stores never read the mark phase");
}
