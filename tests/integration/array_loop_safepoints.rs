//! Array loops after willow-nzsg: per-trip reuse of the owner, length and
//! bounds facts, cold fault paths that reload their operands, and the
//! frame's spare poll countdown outside task stacks.

use super::support::*;
use std::time::Duration;

const EXPECTED: &str =
    "499500\nsummed\n499500\n3\n5\n16\n6\n31415\n8\n12\ntrue\n3\ntrue\n12\n180\n460\n6765\n";

#[test]
fn array_loop_safepoints_example_matches_in_every_mode() {
    let source = include_str!("../../example/array_loop_safepoints.wi");
    for (out, ok) in [
        compile_and_run(source),
        compile_and_run_release(source),
        compile_and_run_gc_stress(source),
        compile_and_run_with_runtime_env(
            source,
            &[("WILLOW_GC_STRESS", "alloc,scheduler")],
            Duration::from_secs(60),
        ),
    ] {
        assert!(ok, "{out}");
        assert_eq!(out, EXPECTED);
    }
}

/// The fault paths spill their operands and reload them after the panic
/// bookkeeping call, so the messages must still name the faulting length and
/// index, for every element kind and for reads, writes and lengths.
#[test]
fn array_loop_faults_report_operands_and_recover() {
    let mut source = String::from("import std::collections::Array;\n");
    let mut calls = String::new();
    let mut expected = String::new();
    let cases = [
        // Index equal to the length, reached by the loop condition `<=`.
        (
            "Array<i64>",
            "[1, 2, 3]",
            "while i <= xs.len() { total = total + xs[i]; i = i + 1; }",
            "the length is 3 but the index is 3",
        ),
        // Negative index computed in the body.
        (
            "Array<i64>",
            "[1, 2, 3]",
            "while i < xs.len() { total = total + xs[i - 1]; i = i + 1; }",
            "the length is 3 but the index is -1",
        ),
        // Write past the end.
        (
            "Array<i64>",
            "[1, 2]",
            "while i < 5 { xs[i] = i; i = i + 1; }",
            "the length is 2 but the index is 2",
        ),
        // Float read and bool write.
        (
            "Array<f64>",
            "[1.5]",
            "while i < 4 { if xs[i] > 0.0 { total = total + 1; } i = i + 1; }",
            "the length is 1 but the index is 1",
        ),
        (
            "Array<bool>",
            "[true, false]",
            "while i < 9 { xs[i + 1] = true; i = i + 1; }",
            "the length is 2 but the index is 2",
        ),
        // Empty array, large index.
        (
            "Array<i64>",
            "[]",
            "while i < 1 { total = xs[i + 1000000]; i = i + 1; }",
            "the length is 0 but the index is 1000000",
        ),
    ];
    for (k, (ty, init, body, message)) in cases.into_iter().enumerate() {
        source.push_str(&format!(
            r#"
fn case{k}() {{
    defer match recover() {{ Some(info) => println(info.message), None => println("missing") }}
    defer println("cleanup {k}");
    let xs: {ty} = {init};
    let mut i: i64 = 0;
    let mut total: i64 = 0;
    {body}
    println("unreachable {{total}}");
}}
"#
        ));
        calls.push_str(&format!("    case{k}();\n"));
        expected.push_str(&format!(
            "cleanup {k}\narray index out of bounds: {message}\n"
        ));
    }
    // A loop after the recovered faults still runs normally.
    source.push_str(&format!(
        "fn main() {{\n{calls}    let ys: Array<i64> = [4, 5];\n    let mut i: i64 = 0;\n    let mut t: i64 = 0;\n    while i < ys.len() {{ t = t + ys[i]; i = i + 1; }}\n    println(t);\n}}\n"
    ));
    expected.push_str("9\n");
    for (out, ok) in [compile_and_run(&source), compile_and_run_release(&source)] {
        assert!(ok, "{out}");
        assert_eq!(out, expected);
    }
}

/// An unrecovered fault inside a hot loop still terminates with the
/// runtime's message.
#[test]
fn array_loop_unrecovered_fault_exits_with_message() {
    let source = r#"
import std::collections::Array;
fn main() {
    let xs: Array<i64> = [7, 8];
    let mut i: i64 = 0;
    let mut total: i64 = 0;
    while i < 10 { total = total + xs[i]; i = i + 1; }
    println(total);
}
"#;
    let (ok, stderr) = compile_temp_project_release_run_stderr(&[("main.wi", source)], "main.wi");
    assert!(ok, "{stderr}");
    assert!(
        stderr.contains("array index out of bounds: the length is 2 but the index is 2"),
        "{stderr}"
    );
}

/// A `static mut` array is the one array another task can change while a
/// synchronous loop runs: the loop may be preempted at its safepoint, so it
/// must reload the length and owner after every poll.
#[test]
fn array_loop_rereads_static_array_grown_by_preempting_task() {
    let source = r#"
import std::collections::Array;
class Store {
    pub static mut values: Array<i64> = [];
}
async fn grow() {
    let mut k: i64 = 0;
    while k < 100 {
        Store::values.push(1);
        k = k + 1;
    }
}
fn count() -> i64 {
    let mut i: i64 = 0;
    let mut n: i64 = 0;
    while i < Store::values.len() {
        n = n + Store::values[i];
        i = i + 1;
    }
    return n;
}
async fn main() {
    let mut k: i64 = 0;
    while k < 1000 { Store::values.push(1); k = k + 1; }
    let task = grow();
    let n = count();
    println(n);
    await task;
    println(Store::values.len());
}
"#;
    for env in [
        &[("WILLOW_WORKERS", "1"), ("WILLOW_TASK_BUDGET", "1")][..],
        &[
            ("WILLOW_WORKERS", "1"),
            ("WILLOW_TASK_BUDGET", "1"),
            ("WILLOW_GC_STRESS", "alloc,scheduler"),
        ][..],
    ] {
        let (out, ok) = compile_and_run_with_runtime_env(source, env, Duration::from_secs(120));
        assert!(ok, "{out}");
        // One worker and a one-trip budget: `grow` runs to completion at
        // the loop's polls well before the loop reaches index 1000, so a
        // loop that kept the length or owner across a poll would print 1000.
        assert_eq!(out, "1100\n1100\n");
    }
}

/// A read-only loop on a task stack decrements the stack's countdown, so it
/// still reaches the runtime and observes cancellation.
#[test]
fn array_read_loop_on_task_stack_remains_cancellable() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
import std::collections::Array;
fn spin(ready: Channel<i64>) -> i64 {
    let xs = [1, 2, 3];
    defer println("cleanup");
    ready.send(1);
    let mut i: i64 = 0;
    let mut total: i64 = 0;
    while i < xs.len() {
        total = total + xs[i];
        i = (i + 1) % xs.len();
    }
    return total;
}
async fn worker(ready: Channel<i64>) { spin(ready); }
async fn main() {
    let ready = Channel<i64>::new();
    let task = worker(ready);
    ready.recv();
    task.cancel();
    await task.result();
    println("done");
}
"#,
        &[("WILLOW_WORKERS", "2"), ("WILLOW_TASK_BUDGET", "1")],
        Duration::from_secs(30),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "cleanup\ndone\n");
}

/// Outside task stacks a loop poll decrements the frame's spare countdown,
/// armed to `i32::MAX`. A loop longer than that must take the slow path,
/// rearm the spare and carry on.
#[test]
fn main_thread_loop_rearms_spare_countdown() {
    let source = r#"
fn main() {
    let mut i: i64 = 0;
    let mut odd: i64 = 0;
    while i < 2200000000 {
        odd = odd + (i & 1);
        i = i + 1;
    }
    println(odd);
}
"#;
    let (out, ok) = compile_and_run_release(source);
    assert!(ok, "{out}");
    assert_eq!(out, "1100000000\n");
}

/// A synchronous main loop still stops for a collection another thread's
/// allocation requests: the GC gate is checked before the countdown.
#[test]
fn main_thread_array_loop_stops_for_collections() {
    let source = r#"
import std::collections::Array;
async fn churn() -> i64 {
    let mut k: i64 = 0;
    let mut kept: Array<Array<i64>> = [];
    while k < 20000 {
        kept.push([k]);
        if kept.len() > 64 { kept = []; }
        k = k + 1;
    }
    return k;
}
async fn main() {
    let task = churn();
    let xs: Array<i64> = [1, 2, 3, 4];
    let mut i: i64 = 0;
    let mut total: i64 = 0;
    while i < 400000 {
        total = total + xs[i % xs.len()];
        i = i + 1;
    }
    println(total);
    println(await task);
}
"#;
    let (out, ok) = compile_and_run_with_runtime_env(
        source,
        &[("WILLOW_WORKERS", "2"), ("WILLOW_GC_STRESS", "alloc")],
        Duration::from_secs(120),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1000000\n20000\n");
}
