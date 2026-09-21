use super::*;

// ── GC-managed blocking cells (willow-9tls.5) ───────────────────────────────
// BlockingCell<T> / BlockingRwCell<T> are ordinary GC objects: a cell is
// allocated on the GC heap, its protected word is traced through a runtime
// hook, and an unreachable cell is swept with whatever only it held. The
// program-lifetime leak and the global root registry are gone.
//
// Perspectives: 1 cell churn bounded under alloc stress (String), 2 rw cell
// churn bounded (String), 3 churn with class values under minor stress, 4 the
// heap returns to baseline after churn + collect, 5 a cell in a class field
// keeps a young value that a minor collection moves, 6 `set` of a young value
// into an old cell is followed after a move, 7 `write` likewise, 8 the initial
// value survives a collection inside `new` under alloc stress, 9 an
// `Array<BlockingCell<String>>` traces every cell, 10 a cell handed across a
// task boundary stays alive, 11 a cell returned from a function outlives the
// frame that made it, 12 two cells sharing one value keep it alive until both
// die, 13 a scalar cell never confuses the collector, 14 a cell in a Map value
// is traced, 15 the runnable example prints its transcript.

#[test]
fn bcgc_01_blocking_cell_churn_bounded_under_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        "fn main() { let mut i = 0; let mut hits = 0; while i < 3000 { let c = BlockingCell::new(\"x\" + i.toString()); if c.get() == \"x\" + i.toString() { hits = hits + 1; } i = i + 1; } println(hits); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3000\n");
}

#[test]
fn bcgc_02_blocking_rw_cell_churn_bounded_under_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        "fn main() { let mut i = 0; let mut hits = 0; while i < 3000 { let c = BlockingRwCell::new(\"y\"); c.write(c.read() + i.toString()); if c.read() == \"y\" + i.toString() { hits = hits + 1; } i = i + 1; } println(hits); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3000\n");
}

#[test]
fn bcgc_03_class_value_churn_under_minor_stress() {
    let (out, ok) = compile_and_run_gc_stress_mode(
        "class Node { pub v: i64; }\nfn main() { let mut i = 0; let mut sum = 0; while i < 2000 { let c = BlockingCell::new(new Node(i)); let r = BlockingRwCell::new(new Node(i * 2)); sum = sum + c.get().v + r.read().v; i = i + 1; } println(sum); }",
        "minor",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5997000\n");
}

#[test]
fn bcgc_04_heap_returns_to_baseline_after_churn() {
    // Warm up once so interned literals exist before the baseline is taken.
    let (out, ok) = compile_and_run(
        "class Node { pub v: i64; }\nfn churn(n: i64) { let mut i = 0; while i < n { let c = BlockingCell::new(new Node(i)); let r = BlockingRwCell::new(\"s\" + i.toString()); c.set(new Node(c.get().v + 1)); r.write(r.read() + \"!\"); i = i + 1; } }\nfn main() { churn(1); gc_collect(); let base = gc_allocated_bytes(); churn(1000); let grown = gc_allocated_bytes() > base; gc_collect(); println(grown); println(gc_allocated_bytes() == base); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\ntrue\n");
}

#[test]
fn bcgc_05_field_cell_follows_a_moved_young_value() {
    let (out, ok) = compile_and_run(
        "class Node { pub v: i64; }\nclass Holder { pub cell: BlockingCell<Node>; }\nfn main() { let h = new Holder(BlockingCell::new(new Node(5))); gc_minor_collect(); let mut i = 0; while i < 100 { let pad = new Node(i); i = i + 1; } gc_minor_collect(); gc_collect(); println(h.cell.get().v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

#[test]
fn bcgc_06_set_of_young_value_is_followed_after_a_move() {
    let (out, ok) = compile_and_run(
        "class Node { pub v: i64; }\nfn main() { let c = BlockingCell::new(new Node(0)); gc_collect(); c.set(new Node(41)); gc_minor_collect(); c.set(new Node(c.get().v + 1)); gc_minor_collect(); gc_collect(); println(c.get().v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn bcgc_07_write_of_young_value_is_followed_after_a_move() {
    let (out, ok) = compile_and_run(
        "class Node { pub v: i64; }\nfn main() { let r = BlockingRwCell::new(new Node(0)); gc_collect(); r.write(new Node(7)); gc_minor_collect(); r.write(new Node(r.read().v * 6)); gc_minor_collect(); gc_collect(); println(r.read().v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn bcgc_08_initial_value_survives_collection_inside_new() {
    // Under alloc stress the cell's own allocation collects while the fresh
    // Node is held only by the constructor argument; the runtime roots it.
    let (out, ok) = compile_and_run_gc_stress_all(
        "class Node { pub v: i64; }\nfn main() { let c = BlockingCell::new(new Node(11)); let r = BlockingRwCell::new(new Node(31)); gc_collect(); println(c.get().v + r.read().v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn bcgc_09_array_of_cells_traces_every_cell() {
    let (out, ok) = compile_and_run_gc_stress(
        "import std::collections::Array;\nfn main() { let mut cells: Array<BlockingCell<String>> = []; let mut i = 0; while i < 50 { cells.push(BlockingCell::new(\"n\" + i.toString())); i = i + 1; } gc_collect(); let mut hits = 0; let mut k = 0; while k < cells.len() { if cells[k].get() == \"n\" + k.toString() { hits = hits + 1; } k = k + 1; } println(hits); println(cells[49].get()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "50\nn49\n");
}

#[test]
fn bcgc_10_cell_across_task_boundary_stays_alive() {
    let (out, ok) = compile_and_run_gc_stress(
        "async fn fill(c: BlockingCell<String>) { await sleep(5); c.set(c.get() + \"-done\"); }\nasync fn main() { let c = BlockingCell::new(\"job\"); let t = fill(c); gc_collect(); await t; gc_collect(); println(c.get()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "job-done\n");
}

#[test]
fn bcgc_11_cell_returned_from_function_outlives_its_frame() {
    let (out, ok) = compile_and_run_gc_stress(
        "class Node { pub v: i64; }\nfn make(v: i64) -> BlockingRwCell<Node> { let r = BlockingRwCell::new(new Node(v)); gc_collect(); return r; }\nfn main() { let r = make(9); gc_collect(); let mut i = 0; while i < 20 { let pad = new Node(i); i = i + 1; } gc_collect(); println(r.read().v); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

#[test]
fn bcgc_12_two_cells_sharing_a_value_keep_it_alive_until_both_die() {
    let (out, ok) = compile_and_run(
        "class Node { pub v: i64; }\nfn second(n: Node) -> i64 { let c = BlockingCell::new(n); gc_collect(); return c.get().v; }\nfn main() { let a = BlockingCell::new(new Node(3)); let b = BlockingCell::new(a.get()); gc_collect(); a.set(new Node(100)); gc_collect(); println(b.get().v); println(second(b.get())); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n3\n");
}

#[test]
fn bcgc_13_scalar_cells_never_confuse_the_collector() {
    // A scalar word that happens to look like a pointer must not be traced.
    let (out, ok) = compile_and_run_gc_stress_all(
        "fn main() { let c = BlockingCell::new(140737488355328); let r = BlockingRwCell::new(-1); let f = BlockingCell::new(2.5); let mut i = 0; while i < 200 { c.set(c.get() + 1); r.write(r.read() - 1); i = i + 1; } gc_collect(); println(c.get()); println(r.read()); println(f.get()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "140737488355528\n-201\n2.5\n");
}

#[test]
fn bcgc_14_cell_as_map_value_is_traced() {
    let (out, ok) = compile_and_run_gc_stress(
        "import std::collections::Map;\nfn main() { let mut m: Map<i64, BlockingCell<String>> = Map::new(); let mut i = 0; while i < 20 { m.insert(i, BlockingCell::new(\"k\" + i.toString())); i = i + 1; } gc_collect(); match m.get(17) { Option::Some(c) => { println(c.get()); } Option::None => { println(\"missing\"); } } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "k17\n");
}

#[test]
fn bcgc_15_runnable_example_prints_its_transcript() {
    let source = std::fs::read_to_string("example/blocking_cell_gc.wi").unwrap();
    let (out, ok) = compile_and_run_gc_stress_mode(&source, "minor");
    assert!(ok, "{out}");
    assert_eq!(out, "1999000\n6\ntrue\n");
}
