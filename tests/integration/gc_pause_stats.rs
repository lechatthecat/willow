//! `gc_pause_count`, `gc_pause_total_ns` and `gc_pause_max_ns`.
//!
//! Durations depend on the machine and the collection schedule, so these tests
//! assert relations that hold on every platform: a collection adds a stop, the
//! longest stop fits inside the total, and a read neither stops nor allocates.
//!
//! 24 perspectives:
//!   1 reads are never negative            13 read inside a class method
//!   2 an explicit minor collection        14 read inside a lambda
//!   3 an explicit major cycle             15 read inside an async function
//!   4 total grows with the count          16 stops caused by other tasks
//!   5 longest fits inside the total       17 reads across task suspension
//!   6 longest is at least the mean        18 read in a condition
//!   7 longest never decreases             19 read as a call argument
//!   8 count across many collections       20 alloc stress
//!   9 stops from allocation pressure      21 minor stress
//!  10 a phase delta by subtraction        22 release build, no constant folding
//!  11 reading is not a stop               23 arguments and result type rejected
//!  12 reading does not allocate           24 the example is fully lowered

use super::support::{
    assert_compile_error_contains, compile_and_run_gc_stress_mode, compile_and_run_release,
    compile_and_run_with_env, compile_file_and_run, compile_with_compiler_env,
};

const PLAIN: [(&str, &str); 0] = [];
const LOG: [(&str, &str); 1] = [("WILLOW_LIR_LOG", "1")];

/// Allocates enough short-lived objects to force minor collections.
const CHURN: &str = "class Node {
    pub value: i64;
}
fn churn(objects: i64) -> i64 {
    let mut kept: Node = new Node(0);
    let mut i: i64 = 0;
    while i < objects {
        kept = new Node(kept.value + 1);
        i = i + 1;
    }
    return kept.value;
}
";

fn assert_prints_true(source: &str) {
    let (out, ok) = compile_and_run_with_env(source, &PLAIN);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, "true\n", "wrong output");
}

fn assert_body_true(body: &str) {
    assert_prints_true(&format!("{CHURN}fn main() {{\n{body}\n}}\n"));
}

#[test]
fn gcpause_01_reads_are_never_negative() {
    assert_body_true(
        "println(gc_pause_count() >= 0 && gc_pause_total_ns() >= 0 && gc_pause_max_ns() >= 0);",
    );
}

#[test]
fn gcpause_02_an_explicit_minor_collection_is_a_pause() {
    assert_body_true(
        "let before = gc_pause_count();
gc_minor_collect();
println(gc_pause_count() > before);",
    );
}

#[test]
fn gcpause_03_an_explicit_major_cycle_is_a_pause() {
    assert_body_true(
        "let before = gc_pause_count();
gc_collect();
println(gc_pause_count() > before);",
    );
}

#[test]
fn gcpause_04_total_grows_with_the_count() {
    assert_body_true(
        "gc_minor_collect();
let first = gc_pause_total_ns();
gc_minor_collect();
println(first > 0 && gc_pause_total_ns() >= first);",
    );
}

#[test]
fn gcpause_05_longest_fits_inside_the_total() {
    assert_body_true(
        "gc_minor_collect();
gc_collect();
let longest = gc_pause_max_ns();
println(longest > 0 && longest <= gc_pause_total_ns());",
    );
}

#[test]
fn gcpause_06_longest_is_at_least_the_mean() {
    assert_body_true(
        "gc_minor_collect();
gc_minor_collect();
gc_collect();
let total = gc_pause_total_ns();
println(gc_pause_max_ns() * gc_pause_count() >= total);",
    );
}

#[test]
fn gcpause_07_longest_never_decreases() {
    assert_body_true(
        "let mut ok = true;
let mut longest = gc_pause_max_ns();
let mut i = 0;
while i < 20 {
    gc_minor_collect();
    let next = gc_pause_max_ns();
    ok = ok && next >= longest;
    longest = next;
    i = i + 1;
}
println(ok);",
    );
}

#[test]
fn gcpause_08_count_tracks_many_collections() {
    assert_body_true(
        "let before = gc_pause_count();
let mut i = 0;
while i < 50 {
    gc_minor_collect();
    i = i + 1;
}
println(gc_pause_count() - before >= 50);",
    );
}

#[test]
fn gcpause_09_allocation_pressure_stops_are_counted() {
    assert_body_true(
        "let minors = gc_minor_collections();
let before = gc_pause_count();
let done = churn(2000000);
let collected = gc_minor_collections() - minors;
println(done == 2000000 && collected > 0 && gc_pause_count() - before >= collected);",
    );
}

#[test]
fn gcpause_10_subtraction_isolates_one_phase() {
    assert_body_true(
        "gc_minor_collect();
let count = gc_pause_count();
let total = gc_pause_total_ns();
gc_minor_collect();
let phase_count = gc_pause_count() - count;
let phase_total = gc_pause_total_ns() - total;
println(phase_count >= 1 && phase_total > 0 && phase_total <= gc_pause_total_ns());",
    );
}

#[test]
fn gcpause_11_reading_is_not_a_stop() {
    assert_body_true(
        "gc_minor_collect();
let count = gc_pause_count();
let total = gc_pause_total_ns();
let longest = gc_pause_max_ns();
println(gc_pause_count() == count && gc_pause_total_ns() == total && gc_pause_max_ns() == longest);",
    );
}

#[test]
fn gcpause_12_reading_does_not_allocate() {
    assert_body_true(
        "let before = gc_total_allocated_bytes();
let sum = gc_pause_count() + gc_pause_total_ns() + gc_pause_max_ns();
println(gc_total_allocated_bytes() == before && sum >= 0);",
    );
}

#[test]
fn gcpause_13_read_inside_a_class_method() {
    assert_prints_true(
        "class Meter {
    pub floor: i64;
    pub fn above_floor(self) -> bool {
        return gc_pause_total_ns() >= self.floor;
    }
}
fn main() {
    println(new Meter(0).above_floor());
}
",
    );
}

#[test]
fn gcpause_14_read_inside_a_lambda() {
    assert_body_true(
        "let read = || gc_pause_count();
let before = read();
gc_minor_collect();
println(read() > before);",
    );
}

#[test]
fn gcpause_15_read_inside_an_async_function() {
    assert_prints_true(
        "async fn probe() -> bool {
    let before = gc_pause_count();
    gc_minor_collect();
    return gc_pause_count() > before;
}
async fn main() {
    println(await probe());
}
",
    );
}

#[test]
fn gcpause_16_stops_from_other_tasks_are_visible() {
    assert_prints_true(&format!(
        "{CHURN}async fn worker(objects: i64) -> i64 {{
    return churn(objects);
}}
async fn main() {{
    let minors = gc_minor_collections();
    let before = gc_pause_count();
    let first = worker(1000000);
    let second = worker(1000000);
    let done = await first + await second;
    let collected = gc_minor_collections() - minors;
    println(done == 2000000 && collected > 0 && gc_pause_count() - before >= collected);
}}
"
    ));
}

#[test]
fn gcpause_17_reads_hold_across_task_suspension() {
    assert_prints_true(
        "async fn main() {
    let count = gc_pause_count();
    gc_minor_collect();
    await yield();
    let later = gc_pause_count();
    await sleep(1);
    println(later > count && gc_pause_count() >= later);
}
",
    );
}

#[test]
fn gcpause_18_read_in_a_condition() {
    assert_body_true(
        "gc_minor_collect();
if gc_pause_count() > 0 && gc_pause_max_ns() > 0 {
    println(true);
} else {
    println(false);
}",
    );
}

#[test]
fn gcpause_19_read_as_a_call_argument() {
    assert_prints_true(
        "fn nonneg(n: i64) -> bool {
    return n >= 0;
}
fn main() {
    println(nonneg(gc_pause_total_ns()));
}
",
    );
}

#[test]
fn gcpause_20_alloc_stress() {
    let source = "fn main() {
    let label = \"heap\" + \"!\";
    let before = gc_pause_count();
    let tag = label + \"?\";
    println(gc_pause_count() > before);
    println(tag);
}
";
    let (out, ok) = compile_and_run_gc_stress_mode(source, "alloc");
    assert!(ok, "alloc stress run failed: {out}");
    assert_eq!(out, "true\nheap!?\n");
}

// Minor stress collects at every TLAB refill, so the program has to fill one.
#[test]
fn gcpause_21_minor_stress() {
    let source = format!(
        "{CHURN}fn main() {{
    let label = \"heap\" + \"!\";
    let minors = gc_minor_collections();
    let before = gc_pause_count();
    let done = churn(100000);
    let collected = gc_minor_collections() - minors;
    println(done == 100000 && collected > 0 && gc_pause_count() - before >= collected);
    println(label);
}}
"
    );
    let (out, ok) = compile_and_run_gc_stress_mode(&source, "minor");
    assert!(ok, "minor stress run failed: {out}");
    assert_eq!(out, "true\nheap!\n");
}

#[test]
fn gcpause_22_release_build_keeps_the_runtime_calls() {
    let source = "fn main() {
    let before = gc_pause_count();
    gc_minor_collect();
    println(gc_pause_count() > before && gc_pause_max_ns() > 0 && gc_pause_total_ns() > 0);
}
";
    let (out, ok) = compile_and_run_release(source);
    assert!(ok, "release run failed: {out}");
    assert_eq!(out, "true\n");
}

#[test]
fn gcpause_23_arguments_and_wrong_result_types_are_rejected() {
    assert_compile_error_contains("fn main() { gc_pause_count(1); }", &["gc_pause_count"]);
    assert_compile_error_contains(
        "fn main() { let value: String = gc_pause_max_ns(); println(value); }",
        &["String"],
    );
}

#[test]
fn gcpause_24_the_example_is_fully_lowered() {
    let (out, ok) = compile_file_and_run("example/gc_pause_stats.wi");
    assert!(ok, "example run failed: {out}");
    assert_eq!(out, "true\ntrue\ntrue\ntrue\ntrue\n");

    let source = std::fs::read_to_string("example/gc_pause_stats.wi").expect("example is readable");
    let (ok, stderr) = compile_with_compiler_env(&source, &LOG);
    assert!(ok, "logged LIR compile failed: {stderr}");
    for function in [
        "minor_stop_is_a_pause",
        "major_cycle_is_a_pause",
        "longest_fits_inside_total",
        "reading_is_not_a_stop",
        "phase_delta_covers_allocation_pressure",
    ] {
        let expected = format!("[lir] compiling `{function}` from lowered IR");
        assert!(stderr.contains(&expected), "{function}: {stderr}");
    }
}
