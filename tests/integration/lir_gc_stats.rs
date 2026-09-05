//! The zero-argument GC statistic builtins through the LIR walker
//! (willow-0g8j.3.1).
//!
//! `gc_collect()` and `gc_minor_collect()` — the two VOID collection triggers —
//! were already in the walker's subset. Their twenty-two read-only siblings,
//! `gc_allocated_bytes()` and the rest, were not: each one took its enclosing
//! function out of the lowered IR and onto the AST emitter. They are the same
//! shape — a zero-argument runtime call with no AST-only metadata behind it,
//! declared `NONE; ([] -> Some(I64))` in the ABI schema — so the only thing that
//! separated them was the eligibility test naming two symbols instead of asking
//! the shared `builtin_call_runtime_name` table.
//!
//! A counter's VALUE depends on the allocator's schedule, so these tests assert
//! only what holds on every platform and under every collection schedule: a
//! counter is never negative, a cumulative counter never goes backwards, and two
//! reads with nothing between them agree. What each test really pins down is
//! that the walker COMPILED the function: since willow-0g8j.3 a body outside
//! its subset is a compile error, and the log perspectives name the functions
//! the walker had to take.
//!
//! 24 perspectives:
//!   1 the read is in the subset at all    13 read inside a `defer` body
//!   2 all twenty-two counters             14 read inside a class method
//!   3 the void pair still works           15 read inside a static method
//!   4 read in a `let` initializer         16 read inside a lambda
//!   5 read in statement position          17 read inside a module function
//!   6 read as a call argument             18 read inside an async function
//!   7 read in a condition                 19 read across a collection
//!   8 read in a `while` guard             20 one expression reads them all
//!   9 read in a `return`                  21 alloc stress
//!  10 read in arithmetic                  22 minor stress
//!  11 read in a `match` scrutinee         23 release build
//!  12 read in a ternary                   24 the example is fully lowered

use super::support::{
    compile_and_run_gc_stress_mode, compile_and_run_release, compile_and_run_with_env,
    compile_file_and_run, compile_temp_project_with_env_and_run,
    compile_temp_project_with_env_stderr, compile_with_compiler_env,
};

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];
const LOG: [(&str, &str); 1] = [("WILLOW_LIR_LOG", "1")];

/// Every counter the language exposes, in declaration order.
const COUNTERS: [&str; 22] = [
    "gc_allocated_bytes",
    "gc_tlab_fast_allocations",
    "gc_tlab_slow_allocations",
    "gc_tlab_refills",
    "gc_tlab_large_allocations",
    "gc_tlab_reserved_bytes",
    "gc_minor_collections",
    "gc_promoted_objects",
    "gc_moved_objects",
    "gc_remembered_set_size",
    "gc_dirty_card_count",
    "gc_write_barrier_hits",
    "gc_old_region_count",
    "gc_old_region_reserved_bytes",
    "gc_old_region_live_bytes",
    "gc_old_region_fragmentation_bytes",
    "gc_large_object_region_count",
    "gc_pinned_region_count",
    "gc_old_region_allocations",
    "gc_old_region_reuses",
    "gc_old_regions_released",
    "gc_major_collections",
];

/// The program builds and prints `expected`. Since willow-0g8j.3 a body outside
/// the walker's subset is a compile error, so a passing run is the proof that
/// the walker compiled it.
fn assert_project_output(source: &str, expected: &str) {
    let (out, ok) = compile_and_run_with_env(source, &PLAIN);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, expected, "wrong output");
}

/// Compile once with the selection log on and require the walker to have taken
/// each named function. Without this a coverage regression could leave a
/// function unlowered while the program still printed the right answer.
fn assert_walker_compiled(source: &str, functions: &[&str]) {
    let (ok, stderr) = compile_with_compiler_env(source, &LOG);
    assert!(ok, "logged LIR compile failed: {stderr}");
    for function in functions {
        let sync = format!("[lir] compiling `{function}` from lowered IR");
        let coop = format!("[lir] compiling async `{function}` from lowered IR");
        assert!(
            stderr.contains(&sync) || stderr.contains(&coop),
            "`{function}` did not use the LIR walker: {stderr}"
        );
    }
}

// 1. The shape that used to fall back: one counter read in an otherwise trivial
//    function. Under REQUIRE this is a compile error until the walker admits it.
#[test]
fn gcstat_01_a_single_read_is_in_the_subset() {
    let source = "fn probe() -> bool {
    return gc_allocated_bytes() >= 0;
}
fn main() {
    println(probe());
}
";
    assert_project_output(source, "true\n");
    assert_walker_compiled(source, &["probe", "main"]);
}

// 2. Every counter, one function each. A name missing from the walker's table
//    is a fallback, and REQUIRE turns that into a failure here rather than into
//    a silent AST compile years later.
#[test]
fn gcstat_02_every_counter_is_in_the_subset() {
    for counter in COUNTERS {
        let source = format!(
            "fn probe() -> bool {{
    return {counter}() >= 0;
}}
fn main() {{
    println(probe());
}}
"
        );
        let (out, ok) = compile_and_run_with_env(&source, &PLAIN);
        assert!(ok, "`{counter}` did not compile under the walker: {out}");
        assert_eq!(out, "true\n", "`{counter}` printed the wrong thing");
        assert_walker_compiled(&source, &["probe"]);
    }
}

// 3. The void pair beside them keeps working: widening the subset must not move
//    `gc_collect`/`gc_minor_collect` onto the value path, where their absent
//    return would be read.
#[test]
fn gcstat_03_the_void_collect_pair_still_works() {
    let source = "fn sweep() -> i64 {
    gc_collect();
    gc_minor_collect();
    return 7;
}
fn main() {
    println(sweep());
}
";
    assert_project_output(source, "7\n");
    assert_walker_compiled(source, &["sweep", "main"]);
}

// 4. A `let` initializer: the counter's value has to reach a stack slot with the
//    `i64` representation, not the `i8` placeholder the void pair returns.
#[test]
fn gcstat_04_read_in_a_let_initializer() {
    let source = "fn probe() -> bool {
    let bytes: i64 = gc_allocated_bytes();
    return bytes >= 0;
}
fn main() {
    println(probe());
}
";
    assert_project_output(source, "true\n");
    assert_walker_compiled(source, &["probe"]);
}

// 5. Statement position, where the value is discarded. The call still has to be
//    emitted — it is the only thing the statement does.
#[test]
fn gcstat_05_read_in_statement_position() {
    let source = "fn probe() -> i64 {
    gc_allocated_bytes();
    gc_moved_objects();
    return 3;
}
fn main() {
    println(probe());
}
";
    assert_project_output(source, "3\n");
    assert_walker_compiled(source, &["probe"]);
}

// 6. As an argument to another call, so the result crosses an argument slot.
#[test]
fn gcstat_06_read_as_a_call_argument() {
    let source = "fn nonneg(n: i64) -> bool {
    return n >= 0;
}
fn probe() -> bool {
    return nonneg(gc_tlab_refills());
}
fn main() {
    println(probe());
}
";
    assert_project_output(source, "true\n");
    assert_walker_compiled(source, &["nonneg", "probe"]);
}

// 7. In an `if` condition, where the value feeds a branch rather than a slot.
#[test]
fn gcstat_07_read_in_a_condition() {
    let source = "fn probe() -> i64 {
    if gc_minor_collections() >= 0 {
        return 1;
    }
    return 0;
}
fn main() {
    println(probe());
}
";
    assert_project_output(source, "1\n");
    assert_walker_compiled(source, &["probe"]);
}

// 8. In a `while` guard, so the call is re-emitted in a loop header block.
#[test]
fn gcstat_08_read_in_a_while_guard() {
    let source = "fn probe() -> i64 {
    let mut spins: i64 = 0;
    while spins < 3 && gc_allocated_bytes() >= 0 {
        spins = spins + 1;
    }
    return spins;
}
fn main() {
    println(probe());
}
";
    assert_project_output(source, "3\n");
    assert_walker_compiled(source, &["probe"]);
}

// 9. Returned directly, so the counter is the function's own result value.
#[test]
fn gcstat_09_read_in_a_return() {
    let source = "fn floor() -> i64 {
    return gc_old_region_count();
}
fn main() {
    println(floor() >= 0);
}
";
    assert_project_output(source, "true\n");
    assert_walker_compiled(source, &["floor", "main"]);
}

// 10. Inside arithmetic, mixed with a literal and another counter: the result
//     has to be a real `i64` operand, not a placeholder.
#[test]
fn gcstat_10_read_in_arithmetic() {
    let source = "fn probe() -> bool {
    let total: i64 = gc_tlab_fast_allocations() + gc_tlab_slow_allocations() * 1;
    return total >= 0;
}
fn main() {
    println(probe());
}
";
    assert_project_output(source, "true\n");
    assert_walker_compiled(source, &["probe"]);
}

// 11. As a `match` scrutinee, where the value drives a dispatch chain.
#[test]
fn gcstat_11_read_as_a_match_scrutinee() {
    let source = "fn probe() -> String {
    match gc_major_collections() {
        0 => { return \"fresh\"; }
        _ => { return \"collected\"; }
    }
}
fn main() {
    gc_collect();
    println(probe());
}
";
    assert_project_output(source, "collected\n");
    assert_walker_compiled(source, &["probe"]);
}

// 12. In a ternary, whose two arms are separate blocks joining at a merge.
#[test]
fn gcstat_12_read_in_a_ternary() {
    let source = "fn probe() -> i64 {
    return gc_allocated_bytes() >= 0 ? 5 : -5;
}
fn main() {
    println(probe());
}
";
    assert_project_output(source, "5\n");
    assert_walker_compiled(source, &["probe"]);
}

// 13. Inside a `defer` body, which is compiled as its own region and re-run on
//     every exit edge.
#[test]
fn gcstat_13_read_inside_a_defer_body() {
    let source = "fn probe() -> i64 {
    defer {
        println(gc_allocated_bytes() >= 0);
    }
    return 2;
}
fn main() {
    println(probe());
}
";
    assert_project_output(source, "true\n2\n");
    assert_walker_compiled(source, &["probe"]);
}

// 14. Inside a class method, where a live receiver shares the frame with the
//     counter's result.
#[test]
fn gcstat_14_read_inside_a_class_method() {
    let source = "class Meter {
    pub floor: i64;

    pub fn above(self) -> bool {
        return gc_allocated_bytes() >= self.floor;
    }
}
fn main() {
    println(new Meter(0).above());
}
";
    assert_project_output(source, "true\n");
    assert_walker_compiled(source, &["Meter::above"]);
}

// 15. Inside a static method, which has no receiver at all.
#[test]
fn gcstat_15_read_inside_a_static_method() {
    let source = "class Meter {
    pub static fn snapshot() -> i64 {
        return gc_promoted_objects();
    }
}
fn main() {
    println(Meter::snapshot() >= 0);
}
";
    assert_project_output(source, "true\n");
    assert_walker_compiled(source, &["Meter::snapshot"]);
}

// 16. Inside a capture-free lambda, which is lifted to its own top-level
//     function and compiled by the same walker.
#[test]
fn gcstat_16_read_inside_a_lambda() {
    let source = "fn main() {
    let probe: fn(i64) -> bool = |n| gc_dirty_card_count() >= n;
    println(probe(0));
}
";
    assert_project_output(source, "true\n");
}

// 17. Inside a module function. A module is lowered as its own unit, so the
//     builtin has to be admitted there too and not only in the entry.
#[test]
fn gcstat_17_read_inside_a_module_function() {
    let files = [
        (
            "stats.wi",
            "pub fn bytes_are_sane() -> bool {
    return gc_allocated_bytes() >= 0;
}
",
        ),
        (
            "main.wi",
            "import stats;

fn main() {
    println(stats::bytes_are_sane());
}
",
        ),
    ];
    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &PLAIN);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, "true\n", "wrong output");
    let (ok, stderr) = compile_temp_project_with_env_stderr(&files, "main.wi", &LOG);
    assert!(ok, "logged LIR compile failed: {stderr}");
    assert!(
        stderr.contains("[lir] compiling `stats.bytes_are_sane` from lowered IR"),
        "the module function did not use the LIR walker: {stderr}"
    );
}

// 18. Inside an async function, whose body is split across resume points. The
//     counter is not a suspension, so it must not become one.
#[test]
fn gcstat_18_read_inside_an_async_function() {
    let source = "async fn probe() -> bool {
    let before: i64 = gc_allocated_bytes();
    await yield();
    return gc_allocated_bytes() >= 0 && before >= 0;
}
async fn main() {
    println(await probe());
}
";
    assert_project_output(source, "true\n");
}

// 19. A read on both sides of a collection. `gc_allocated_bytes` is a LIVE-bytes
//     read, so this pins down only that the call survives a collection between
//     two reads — the counter itself may legitimately fall.
#[test]
fn gcstat_19_read_across_a_collection() {
    let source = "fn probe() -> bool {
    let before: i64 = gc_allocated_bytes();
    gc_collect();
    let after: i64 = gc_allocated_bytes();
    return before >= 0 && after >= 0;
}
fn main() {
    println(probe());
}
";
    assert_project_output(source, "true\n");
    assert_walker_compiled(source, &["probe"]);
}

// 20. Every counter in one expression: the walker has to resolve all of them
//     through `builtin_call_runtime_name`, not a hand-written pair of names.
#[test]
fn gcstat_20_a_single_expression_reads_every_counter() {
    let reads = COUNTERS
        .iter()
        .map(|c| format!("        && {c}() >= 0"))
        .collect::<Vec<_>>()
        .join("\n");
    let source = format!(
        "fn probe() -> bool {{
    return true
{reads};
}}
fn main() {{
    println(probe());
}}
"
    );
    assert_project_output(&source, "true\n");
    assert_walker_compiled(&source, &["probe"]);
}

// 21. Under a collection at every allocation. A counter read allocates nothing,
//     so nothing here may be lost to a collection that lands between two reads.
#[test]
fn gcstat_21_alloc_stress() {
    let source = "fn probe(label: String) -> String {
    let bytes: i64 = gc_allocated_bytes();
    let tag: String = label + \"!\";
    gc_collect();
    if bytes >= 0 {
        return tag;
    }
    return \"impossible\";
}
fn main() {
    println(probe(\"heap\"));
}
";
    let (out, ok) = compile_and_run_gc_stress_mode(source, "alloc");
    assert!(ok, "alloc stress run failed: {out}");
    assert_eq!(out, "heap!\n");
}

// 22. Under a minor collection at every allocation, which MOVES surviving
//     objects: the String local beside the counter must survive evacuation.
#[test]
fn gcstat_22_minor_stress() {
    let source = "fn probe(label: String) -> String {
    let tag: String = label + \"?\";
    let bytes: i64 = gc_minor_collections();
    gc_minor_collect();
    if bytes >= 0 {
        return tag;
    }
    return \"impossible\";
}
fn main() {
    println(probe(\"heap\"));
}
";
    let (out, ok) = compile_and_run_gc_stress_mode(source, "minor");
    assert!(ok, "minor stress run failed: {out}");
    assert_eq!(out, "heap?\n");
}

// 23. A release build, where the optimizer sees the call: the counter is a real
//     runtime call and cannot be folded to a constant.
#[test]
fn gcstat_23_release_build() {
    let source = "fn probe() -> bool {
    return gc_allocated_bytes() >= 0 && gc_write_barrier_hits() >= 0;
}
fn main() {
    println(probe());
}
";
    let (out, ok) = compile_and_run_release(source);
    assert!(ok, "release run failed: {out}");
    assert_eq!(out, "true\n");
}

// 24. The example program runs, and every function in it is compiled from
//     lowered IR.
#[test]
fn gcstat_24_the_example_is_fully_lowered() {
    let (out, ok) = compile_file_and_run("example/lir_gc_stats.wi");
    assert!(ok, "example run failed: {out}");
    assert_eq!(out, "true\ntrue\ntrue\ntrue\ntrue\n3\ntrue\nheap: ok\n");

    let source = std::fs::read_to_string("example/lir_gc_stats.wi").expect("example is readable");
    assert_walker_compiled(
        &source,
        &[
            "all_non_negative",
            "repeat_read_is_stable",
            "minor_collection_is_counted",
            "major_collection_is_counted",
            "allocation_is_counted",
            "counter_drives_control_flow",
            "Meter::above_floor",
            "measured",
            "main",
        ],
    );
}
