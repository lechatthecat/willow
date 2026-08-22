//! Cooperative async bodies compiled from Lowered IR (willow-0g8j.2.11).
//!
//! This first vertical slice covers bodies without language-level suspension
//! operations. The poll function still inserts cooperative preemption points,
//! uses frame-backed live locals, and returns through the scheduler ABI.

use super::support::{compile_and_run_with_env, compile_with_compiler_env};

const LIR_REQUIRED: [(&str, &str); 2] = [("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")];

fn assert_lir(source: &str, expected: &str) {
    let (out, ok) = compile_and_run_with_env(source, &LIR_REQUIRED);
    if !ok {
        let (compiled, stderr) = compile_with_compiler_env(source, &LIR_REQUIRED);
        panic!("LIR-required async program failed (compiled={compiled}): {out}{stderr}");
    }
    assert_eq!(out, expected);
}

fn assert_lir_logged(source: &str, expected: &str, functions: &[&str]) {
    assert_lir(source, expected);
    let (ok, stderr) = compile_with_compiler_env(
        source,
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_LIR_LOG", "1"),
        ],
    );
    assert!(ok, "logged LIR compile failed: {stderr}");
    for function in functions {
        assert!(
            stderr.contains(&format!(
                "[lir] compiling async `{function}` from lowered IR"
            )),
            "async function `{function}` did not use LIR: {stderr}"
        );
    }
}

#[test]
fn async_lir_01_main_uses_poll_abi() {
    assert_lir_logged("async fn main() { println(41 + 1); }", "42\n", &["main"]);
}

#[test]
fn async_lir_02_leaf_stores_result_in_task_frame() {
    assert_lir_logged(
        r#"
async fn answer(x: i64) -> i64 {
    println(x * 2);
    return x * 2;
}
async fn main() {
    answer(21);
    let mut i = 0;
    while i < 10000 { i = i + 1; }
}
"#,
        "42\n",
        &["answer", "main"],
    );
}

#[test]
fn async_lir_03_branch_and_loop_survive_preemption() {
    assert_lir_logged(
        r#"
async fn sum(limit: i64) -> i64 {
    let mut i = 0;
    let mut total = 0;
    while i < limit {
        if i % 2 == 0 { total = total + i; }
        i = i + 1;
    }
    println(total);
    return total;
}
async fn main() {
    sum(10);
    let mut i = 0;
    while i < 10000 { i = i + 1; }
}
"#,
        "20\n",
        &["sum", "main"],
    );
}

#[test]
fn async_lir_04_gc_local_is_frame_backed_across_preemption() {
    assert_lir_logged(
        r#"
async fn keep() -> String {
    let value = "still alive";
    let mut i = 0;
    while i < 100 { i = i + 1; }
    println(value);
    return value;
}
async fn main() {
    keep();
    let mut i = 0;
    while i < 10000 { i = i + 1; }
}
"#,
        "still alive\n",
        &["keep", "main"],
    );
}

#[test]
fn async_lir_05_sleep_splits_and_resumes_main() {
    assert_lir_logged(
        r#"
async fn main() {
    println("before");
    await sleep(1);
    println("after");
}
"#,
        "before\nafter\n",
        &["main"],
    );
}

#[test]
fn async_lir_06_yield_splits_and_resumes_main() {
    assert_lir_logged(
        r#"
async fn main() {
    println(1);
    await yield();
    println(2);
}
"#,
        "1\n2\n",
        &["main"],
    );
}

#[test]
fn async_lir_07_multiple_builtin_await_states_keep_order() {
    assert_lir_logged(
        r#"
async fn main() {
    println(1);
    await yield();
    println(2);
    await sleep(1);
    println(3);
}
"#,
        "1\n2\n3\n",
        &["main"],
    );
}

#[test]
fn async_lir_08_locals_survive_sleep_resume() {
    assert_lir_logged(
        r#"
async fn main() {
    let message = "kept";
    let number = 40;
    await sleep(1);
    println(message);
    println(number + 2);
}
"#,
        "kept\n42\n",
        &["main"],
    );
}

#[test]
fn async_lir_09_loop_resume_reloads_frame_locals() {
    assert_lir_logged(
        r#"
async fn main() {
    let mut i = 0;
    while i < 3 {
        println(i);
        await yield();
        i = i + 1;
    }
}
"#,
        "0\n1\n2\n",
        &["main"],
    );
}

#[test]
fn async_lir_10_leaf_sleep_resumes_and_keeps_gc_param() {
    assert_lir_logged(
        r#"
async fn worker(message: String) -> i64 {
    await sleep(1);
    println(message);
    return 42;
}
async fn main() {
    worker("leaf");
    await sleep(10);
}
"#,
        "leaf\n",
        &["worker", "main"],
    );
}

#[test]
fn async_lir_11_budget_one_does_not_resume_past_ssa_local() {
    let source = r#"
async fn value() -> i64 {
    let x = 42;
    return x;
}
async fn main() { println(await value()); }
"#;
    // `main` still uses the mature Task-await path, but the log proves the
    // vulnerable `let x; return x` leaf is compiled from LIR.
    let (compiled, stderr) = compile_with_compiler_env(
        source,
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_LOG", "1")],
    );
    assert!(
        compiled,
        "budget regression fixture did not compile: {stderr}"
    );
    assert!(
        stderr.contains("[lir] compiling async `value` from lowered IR"),
        "the vulnerable leaf did not use LIR: {stderr}"
    );

    let (out, ok) = compile_and_run_with_env(
        source,
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_TASK_BUDGET", "1")],
    );
    assert!(ok, "budget=1 async LIR program failed: {out}");
    assert_eq!(out, "42\n");
}
