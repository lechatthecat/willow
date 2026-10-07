//! Scalar element stores inside GC-free runs (willow-ijui.10): the receiver
//! copy of a store needs no root, and a store reuses the owner, length and
//! buffer an earlier access of the same run validated.

use super::support::*;
use std::time::Duration;

const EXPECTED: &str = "499500\ntrue\ntrue\n21\n69\n9\n41\n78\n41\n81\n21\n14\n31\n18\nfilled\n18\n23\n21\n60\n210\n200\n6\ntrue\n7\n101\n303\n";

#[test]
fn array_store_runs_example_matches_in_every_mode() {
    let source = include_str!("../../example/array_store_runs.wi");
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

/// A store that reuses facts validated earlier in its run still faults with
/// the exact operands, after the value was evaluated, and unwinds through
/// `defer` and `recover`.
#[test]
fn array_store_faults_keep_operands_and_evaluation_order() {
    let mut source = String::from(
        "import std::collections::Array;\nfn noisy(v: i64) -> i64 { println(v); return v; }\n",
    );
    let mut calls = String::new();
    let mut expected = String::new();
    let cases: [(&str, &str, &str, &str, &str); 9] = [
        // A store at the length after a valid store of the same run.
        (
            "Array<i64>",
            "[1, 2, 3]",
            "xs[2] = 5; xs[xs.len()] = 6;",
            "",
            "array index out of bounds: the length is 3 but the index is 3",
        ),
        // A negative index after a length read of the same run.
        (
            "Array<i64>",
            "[1, 2, 3]",
            "let n = xs.len(); xs[n - 4] = 1;",
            "",
            "array index out of bounds: the length is 3 but the index is -1",
        ),
        // The loop condition's facts reach the store in the body.
        (
            "Array<i64>",
            "[1, 2]",
            "let mut i: i64 = 0; while i <= xs.len() { xs[i] = i; i = i + 1; }",
            "",
            "array index out of bounds: the length is 2 but the index is 2",
        ),
        // Empty array.
        (
            "Array<i64>",
            "[]",
            "xs[0] = 1;",
            "",
            "array index out of bounds: the length is 0 but the index is 0",
        ),
        // f64 and bool stores.
        (
            "Array<f64>",
            "[1.5]",
            "xs[0] = 2.5; xs[1] = 3.5;",
            "",
            "array index out of bounds: the length is 1 but the index is 1",
        ),
        (
            "Array<bool>",
            "[true]",
            "xs[0] = false; xs[-7] = true;",
            "",
            "array index out of bounds: the length is 1 but the index is -7",
        ),
        // A pop between two stores ends the run: the second store sees the
        // shorter length.
        (
            "Array<i64>",
            "[1, 2, 3]",
            "xs[2] = 9; xs.pop(); xs[2] = 10;",
            "",
            "array index out of bounds: the length is 2 but the index is 2",
        ),
        // The value is evaluated before the bounds check: its call runs, and
        // a zero divisor faults first.
        (
            "Array<i64>",
            "[1]",
            "xs[5] = noisy(42);",
            "42\n",
            "array index out of bounds: the length is 1 but the index is 5",
        ),
        (
            "Array<i64>",
            "[1]",
            "let z = xs[0] - 1; xs[5] = 7 / z;",
            "",
            "division by zero",
        ),
    ];
    for (k, (ty, init, body, before, message)) in cases.into_iter().enumerate() {
        source.push_str(&format!(
            r#"
fn case{k}() {{
    defer match recover() {{ Some(info) => println(info.message), None => println("missing") }}
    defer println("cleanup {k}");
    let xs: {ty} = {init};
    {body}
    println("unreachable");
}}
"#
        ));
        calls.push_str(&format!("    case{k}();\n"));
        expected.push_str(&format!("{before}cleanup {k}\n{message}\n"));
    }
    // A write loop after the recovered faults still runs normally.
    source.push_str(&format!(
        "fn main() {{\n{calls}    let ys: Array<i64> = [0, 0, 0];\n    let mut i: i64 = 0;\n    while i < ys.len() {{ ys[i] = i + 4; i = i + 1; }}\n    println(ys[0] + ys[1] + ys[2]);\n}}\n"
    ));
    expected.push_str("15\n");
    for (out, ok) in [compile_and_run(&source), compile_and_run_release(&source)] {
        assert!(ok, "{out}");
        assert_eq!(out, expected);
    }
}

/// A write loop over a `static mut` array that another task grows at the
/// loop's polls must store into the current owner and length after every
/// poll.
#[test]
fn array_store_loop_rereads_static_array_grown_by_preempting_task() {
    let source = r#"
import std::collections::Array;
class Store {
    pub static mut values: Array<i64> = [];
}
async fn grow() {
    let mut k: i64 = 0;
    while k < 100 {
        Store::values.push(0);
        k = k + 1;
    }
}
fn fill() -> i64 {
    let mut i: i64 = 0;
    while i < Store::values.len() {
        Store::values[i] = 2;
        i = i + 1;
    }
    return i;
}
async fn main() {
    let mut k: i64 = 0;
    while k < 1000 { Store::values.push(0); k = k + 1; }
    let task = grow();
    let n = fill();
    await task;
    let mut sum: i64 = 0;
    k = 0;
    while k < Store::values.len() { sum = sum + Store::values[k]; k = k + 1; }
    println(n);
    println(sum);
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
        assert_eq!(out, "1100\n2200\n");
    }
}

/// A write loop on a task stack keeps its countdown, so it still observes
/// cancellation.
#[test]
fn array_store_loop_on_task_stack_remains_cancellable() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
import std::collections::Array;
fn spin(ready: Channel<i64>) -> i64 {
    let xs = [0, 0, 0];
    defer println("cleanup");
    ready.send(1);
    let mut i: i64 = 0;
    while i < xs.len() {
        xs[i] = xs[i] + 1;
        i = (i + 1) % xs.len();
    }
    return xs[0];
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
