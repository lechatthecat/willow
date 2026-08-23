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

// --- `await f(..)` on a cooperative leaf, split from LIR (willow-0g8j.2.11) ---

#[test]
fn async_lir_12_leaf_call_await_in_let_initialiser() {
    assert_lir_logged(
        r#"
async fn twice(x: i64) -> i64 { return x * 2; }
async fn main() {
    let r = await twice(21);
    println(r);
}
"#,
        "42\n",
        &["twice", "main"],
    );
}

#[test]
fn async_lir_13_void_leaf_call_await_is_a_statement() {
    assert_lir_logged(
        r#"
async fn shout(message: String) { println(message); }
async fn main() {
    await shout("hi");
    println("done");
}
"#,
        "hi\ndone\n",
        &["shout", "main"],
    );
}

#[test]
fn async_lir_14_leaf_call_await_result_is_discarded() {
    assert_lir_logged(
        r#"
async fn effect(x: i64) -> i64 {
    println(x);
    return x;
}
async fn main() {
    await effect(7);
    println("after");
}
"#,
        "7\nafter\n",
        &["effect", "main"],
    );
}

#[test]
fn async_lir_15_leaf_call_await_returns_a_gc_value() {
    assert_lir_logged(
        r#"
async fn tag(message: String) -> String { return message; }
async fn main() {
    let s = await tag("gc");
    println(s);
}
"#,
        "gc\n",
        &["tag", "main"],
    );
}

#[test]
fn async_lir_16_leaf_call_await_arguments_are_evaluated_first() {
    assert_lir_logged(
        r#"
async fn add(a: i64, b: i64) -> i64 { return a + b; }
async fn main() {
    let x = 2;
    let r = await add(x * 3, x + 4);
    println(r);
}
"#,
        "12\n",
        &["add", "main"],
    );
}

#[test]
fn async_lir_17_sequential_leaf_call_awaits_keep_order() {
    assert_lir_logged(
        r#"
async fn step(label: String, value: i64) -> i64 {
    println(label);
    return value;
}
async fn main() {
    let a = await step("first", 1);
    let b = await step("second", 2);
    println(a * 10 + b);
}
"#,
        "first\nsecond\n12\n",
        &["step", "main"],
    );
}

#[test]
fn async_lir_18_leaf_call_await_assigns_an_existing_local() {
    assert_lir_logged(
        r#"
async fn bump(x: i64) -> i64 { return x + 1; }
async fn main() {
    let mut n = 0;
    n = await bump(n);
    n = await bump(n);
    println(n);
}
"#,
        "2\n",
        &["bump", "main"],
    );
}

#[test]
fn async_lir_19_leaf_call_await_inside_a_loop_accumulates() {
    assert_lir_logged(
        r#"
async fn step(x: i64) -> i64 {
    await sleep(1);
    return x + 1;
}
async fn main() {
    let mut acc = 0;
    let mut i = 0;
    while i < 3 {
        acc = await step(acc);
        i = i + 1;
    }
    println(acc);
}
"#,
        "3\n",
        &["step", "main"],
    );
}

#[test]
fn async_lir_20_leaf_call_await_inside_a_branch() {
    assert_lir_logged(
        r#"
async fn pick(c: bool) -> i64 {
    if c { return 1; }
    return 2;
}
async fn main() {
    let mut v = 0;
    if 1 < 2 { v = await pick(true); } else { v = await pick(false); }
    println(v);
}
"#,
        "1\n",
        &["pick", "main"],
    );
}

#[test]
fn async_lir_21_leaf_call_await_after_a_real_suspension_reloads_locals() {
    assert_lir_logged(
        r#"
async fn one() -> i64 {
    await yield();
    return 1;
}
async fn main() {
    let keep = "alive";
    let a = await one();
    await sleep(1);
    let b = await one();
    println(keep);
    println(a + b);
}
"#,
        "alive\n2\n",
        &["one", "main"],
    );
}

#[test]
fn async_lir_22_leaf_awaits_another_leaf() {
    assert_lir_logged(
        r#"
async fn inner(x: i64) -> i64 { return x + 1; }
async fn outer(x: i64) -> i64 {
    let seen = await inner(x);
    return seen * 10;
}
async fn main() {
    let r = await outer(4);
    println(r);
}
"#,
        "50\n",
        &["inner", "outer", "main"],
    );
}

#[test]
fn async_lir_23_leaf_call_await_survives_budget_one() {
    let source = r#"
async fn step(x: i64) -> i64 {
    await sleep(1);
    return x * 3;
}
async fn main() {
    let keep = "kept";
    let a = await step(2);
    let b = await step(a);
    println(keep);
    println(b);
}
"#;
    assert_lir_logged(source, "kept\n18\n", &["step", "main"]);
    let (out, ok) = compile_and_run_with_env(
        source,
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_TASK_BUDGET", "1")],
    );
    assert!(ok, "budget=1 leaf-call await failed: {out}");
    assert_eq!(out, "kept\n18\n");
}

#[test]
fn async_lir_24_leaf_call_await_matches_the_ast_backend() {
    let source = r#"
async fn work(label: String, value: i64) -> i64 {
    await sleep(1);
    println(label);
    return value * 2;
}
async fn main() {
    let mut total = 0;
    let mut i = 1;
    while i <= 3 {
        total = await work("step", i);
        i = i + 1;
    }
    println(total);
}
"#;
    let (lir, lir_ok) = compile_and_run_with_env(source, &[("WILLOW_LIR_BACKEND", "1")]);
    assert!(lir_ok, "LIR run failed: {lir}");
    let (ast, ast_ok) = compile_and_run_with_env(source, &[("WILLOW_LIR_BACKEND", "0")]);
    assert!(ast_ok, "AST run failed: {ast}");
    assert_eq!(lir, ast, "the two backends disagree");
    assert_eq!(lir, "step\nstep\nstep\n6\n");
}

#[test]
fn async_lir_25_nested_leaf_call_await_is_split_out_of_its_statement() {
    // `println(await f(..))` is an await in VALUE position. The walker splits
    // the await out ahead of the rest of the statement so no Cranelift value
    // computed before the park has to survive the poll return.
    let source = r#"
async fn twice(x: i64) -> i64 { return x * 2; }
async fn main() { println(await twice(21)); }
"#;
    assert_lir_logged(source, "42\n", &["twice", "main"]);
}

#[test]
fn async_lir_26_await_example_is_fully_lir() {
    // The await example (willow-0g8j.2.11) exists to be compiled this way:
    // every one of its `async fn`s — leaf calls, builtin awaits, suspending
    // loops and branches — must stay on the LIR path.
    let source = include_str!("../../example/lir_async_await.wi");
    let (ok, stderr) = compile_with_compiler_env(
        source,
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_LOG", "1")],
    );
    assert!(ok, "example/lir_async_await.wi did not compile: {stderr}");
    assert!(
        !stderr.contains("stays on the AST backend"),
        "example/lir_async_await.wi must be entirely LIR-compiled: {stderr}"
    );
}

// --- preemption safepoints sit on loop back edges only (willow-0g8j.2.11) ---
//
// A safepoint parks the poll fn, and a local the AST liveness pass left in an
// SSA value does not survive that park. The pass models suspension at loop back
// edges and at statements that execute a call, so a safepoint anywhere else is
// a resume past a definition. `WILLOW_TASK_BUDGET=1` trips every safepoint, so
// these run the same program twice and require the same answer.

fn assert_same_under_budget_one(source: &str, expected: &str) {
    let (ast, ast_ok) = compile_and_run_with_env(
        source,
        &[("WILLOW_LIR_BACKEND", "0"), ("WILLOW_TASK_BUDGET", "1")],
    );
    assert!(ast_ok, "AST run failed: {ast}");
    let (lir, lir_ok) = compile_and_run_with_env(
        source,
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_TASK_BUDGET", "1")],
    );
    assert!(lir_ok, "LIR run failed: {lir}");
    assert_eq!(lir, ast, "the two backends disagree under budget=1");
    assert_eq!(lir, expected);
}

#[test]
fn async_lir_27_else_arm_await_result_survives_the_join() {
    // The regression: HIR lowering numbers the `if` join block BEFORE the
    // `else` arm, so the else arm's jump to the join runs backwards by block
    // id. Reading that as a loop back edge put a safepoint on it, and the
    // awaited result — an SSA value, since the AST pass frames the hoisted
    // temp under its own span — was lost across the park.
    assert_same_under_budget_one(
        r#"
async fn delayed(x: i64) -> i64 {
    await sleep(1);
    return x + 1;
}
async fn branch(flag: bool) -> i64 {
    let mut value = 0;
    if flag { value = 42; } else { value = await delayed(0); }
    return value;
}
async fn main() { println(await branch(false)); }
"#,
        "1\n",
    );
}

#[test]
fn async_lir_28_then_arm_await_result_survives_the_join() {
    assert_same_under_budget_one(
        r#"
async fn delayed(x: i64) -> i64 {
    await sleep(1);
    return x + 1;
}
async fn branch(flag: bool) -> i64 {
    let mut value = 0;
    if flag { value = await delayed(6); } else { value = 42; }
    return value;
}
async fn main() { println(await branch(true)); }
"#,
        "7\n",
    );
}

#[test]
fn async_lir_29_branch_local_survives_the_join() {
    // The same join edge, with an ordinary call rather than an await before
    // it: the call is what the AST pass puts a safepoint on, and `value` still
    // has to reach the join.
    assert_same_under_budget_one(
        r#"
fn plain(x: i64) -> i64 { return x + 1; }
async fn branch(flag: bool) -> i64 {
    let mut value = 0;
    if flag { value = 42; } else { value = plain(8); }
    return value;
}
async fn main() { println(await branch(false)); }
"#,
        "9\n",
    );
}

#[test]
fn async_lir_30_loop_back_edge_still_preempts() {
    // The fix must not remove the safepoint a real loop needs: two tasks that
    // never await still have to make progress in turns, and the sums have to
    // come out right across every park the back edge causes. Which label
    // reaches the terminal first is the scheduler's business — under
    // `WILLOW_TASK_BUDGET=1` it varies run to run, which is itself only
    // possible because the back edge parks — so this checks the multiset of
    // lines. `s4_while_back_edge_asks_for_a_safepoint` in `lir_gen` is what
    // pins the safepoint to the back edge itself.
    let source = r#"
async fn spin(label: String, rounds: i64) -> i64 {
    let mut i = 0;
    let mut total = 0;
    while i < rounds {
        total = total + i;
        i = i + 1;
    }
    println(label);
    return total;
}
async fn main() {
    let a = spin("a", 200);
    let b = spin("b", 200);
    let first = await a;
    let second = await b;
    println(first + second);
}
"#;
    let (out, ok) = compile_and_run_with_env(
        source,
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_TASK_BUDGET", "1")],
    );
    assert!(ok, "budget=1 spin program failed: {out}");
    let mut lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines.pop(),
        Some("39800"),
        "wrong total under budget=1: {out}"
    );
    lines.sort_unstable();
    assert_eq!(lines, ["a", "b"], "a spin task did not finish: {out}");
}

#[test]
fn async_lir_31_loop_carried_locals_survive_the_back_edge() {
    assert_same_under_budget_one(
        r#"
async fn work(x: i64) -> i64 {
    await sleep(1);
    return x + 2;
}
async fn main() {
    let tag = "loop";
    let mut total = 0;
    let mut i = 0;
    while i < 3 {
        total = await work(total);
        i = i + 1;
    }
    println(tag);
    println(total);
}
"#,
        "loop\n6\n",
    );
}

#[test]
fn async_lir_32_nested_branch_joins_keep_their_locals() {
    assert_same_under_budget_one(
        r#"
async fn step(x: i64) -> i64 {
    await yield();
    return x * 2;
}
async fn classify(n: i64) -> i64 {
    let mut out = 0;
    if n > 10 {
        if n > 100 { out = await step(n); } else { out = await step(n + 1); }
    } else {
        out = await step(n + 2);
    }
    return out;
}
async fn main() {
    println(await classify(1));
    println(await classify(11));
    println(await classify(111));
}
"#,
        "6\n24\n222\n",
    );
}

// --- awaits in value position (willow-0g8j.2.11) -------------------------
//
// `let s = await f() + "!"` suspends in the MIDDLE of a statement. A Cranelift
// value computed before the park does not survive the poll function's return,
// so the walker splits the await out and emits it first, then emits the rest
// of the statement against the resumed value. That reorder is only legal while
// everything the statement would have evaluated ahead of the await can be
// evaluated again afterwards — the same rule `coop_anf`'s `bind` applies on
// the AST path, which is why the two backends still agree. Anything else keeps
// falling back, and these tests pin both halves of that line.

/// Both backends, plus every scheduler/GC stress switch, produce `expected`.
fn assert_value_position_agrees(source: &str, expected: &str) {
    let configs: [&[(&str, &str)]; 5] = [
        &[("WILLOW_LIR_BACKEND", "0")],
        &[("WILLOW_LIR_BACKEND", "1")],
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_TASK_BUDGET", "1")],
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_GC_STRESS", "alloc")],
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_ASYNC_FRAME_ALL", "1")],
    ];
    for env in configs {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "run failed under {env:?}: {out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
}

#[test]
fn async_lir_33_binary_operand_await_reads_its_variable_again() {
    // `n` is evaluated before the await on the AST path too, and re-read after
    // it, because `coop_anf` refuses to hoist a bare `Var` into a temp. The
    // walker reproduces exactly that reorder.
    assert_lir_logged(
        r#"
async fn twice(n: i64) -> i64 { return n * 2; }
async fn main() {
    let n = 5;
    let r = n + await twice(n);
    println(r);
}
"#,
        "15\n",
        &["main"],
    );
}

#[test]
fn async_lir_34_literal_operand_before_the_await_is_rematerialized() {
    assert_lir_logged(
        r#"
async fn twice(n: i64) -> i64 { return n * 2; }
async fn main() { println(100 + await twice(21)); }
"#,
        "142\n",
        &["main"],
    );
}

#[test]
fn async_lir_35_gc_managed_hoisted_result_stays_rooted() {
    // The awaited `String` is live while the concatenation that follows
    // allocates, so the split has to root it — and pop that root again before
    // the next statement's preemption safepoint, which asserts the temporary
    // root stack is empty.
    assert_lir_logged(
        r#"
async fn tag(s: String) -> String { return "[" + s + "]"; }
async fn main() {
    let s = await tag("hi") + "!";
    println(s);
}
"#,
        "[hi]!\n",
        &["main"],
    );
}

#[test]
fn async_lir_36_assignment_statement_splits_its_await() {
    assert_lir_logged(
        r#"
async fn twice(n: i64) -> i64 { return n * 2; }
async fn main() {
    let mut acc = 0;
    acc = acc + await twice(4);
    println(acc);
}
"#,
        "8\n",
        &["main"],
    );
}

#[test]
fn async_lir_37_await_as_one_of_several_call_arguments() {
    assert_lir_logged(
        r#"
fn add(a: i64, b: i64) -> i64 { return a + b; }
async fn seven() -> i64 { return 7; }
async fn main() { println(add(1, await seven())); }
"#,
        "8\n",
        &["main"],
    );
}

#[test]
fn async_lir_38_await_in_index_position() {
    assert_lir_logged(
        r#"
import std::collections::Array;
async fn pick(n: i64) -> i64 { return n; }
async fn main() {
    let xs: Array<i64> = [10, 20, 30];
    println(xs[await pick(2)]);
}
"#,
        "30\n",
        &["main"],
    );
}

#[test]
fn async_lir_39_await_in_an_array_literal_element() {
    assert_lir_logged(
        r#"
import std::collections::Array;
async fn seven() -> i64 { return 7; }
async fn main() {
    let xs: Array<i64> = [1, await seven()];
    println(xs[1]);
}
"#,
        "7\n",
        &["main"],
    );
}

#[test]
fn async_lir_40_awaited_value_is_the_method_receiver() {
    assert_lir_logged(
        r#"
async fn seven() -> i64 { return 7; }
async fn main() { println("v=" + (await seven()).toString()); }
"#,
        "v=7\n",
        &["main"],
    );
}

#[test]
fn async_lir_41_return_operand_await_is_split() {
    assert_lir_logged(
        r#"
async fn twice(n: i64) -> i64 { return n * 2; }
async fn calc(v: i64) -> i64 { return await twice(v) + 1; }
async fn main() { println(await calc(4)); }
"#,
        "9\n",
        &["calc", "main"],
    );
}

#[test]
fn async_lir_42_gc_managed_return_operand_await_is_split() {
    // The returned root is released by the poll return's own unwind, so the
    // split must NOT emit its own pop here.
    assert_lir_logged(
        r#"
async fn tag(s: String) -> String { return "[" + s + "]"; }
async fn wrap(s: String) -> String { return await tag(s) + "!"; }
async fn main() { println(await wrap("x")); }
"#,
        "[x]!\n",
        &["wrap", "main"],
    );
}

#[test]
fn async_lir_43_branch_condition_is_a_root_await() {
    assert_lir_logged(
        r#"
async fn flag() -> bool { return true; }
async fn main() {
    if await flag() { println(1); } else { println(0); }
}
"#,
        "1\n",
        &["main"],
    );
}

#[test]
fn async_lir_44_branch_condition_contains_an_await() {
    assert_lir_logged(
        r#"
async fn seven() -> i64 { return 7; }
async fn main() {
    let mut v = 0;
    if await seven() > 3 { v = 1; }
    println(v);
}
"#,
        "1\n",
        &["main"],
    );
}

#[test]
fn async_lir_45_loop_header_await_parks_every_iteration() {
    // The condition sits in the header block, which the back edge re-enters,
    // so the split has to be re-emitted per iteration rather than hoisted out.
    assert_lir_logged(
        r#"
async fn more(n: i64) -> bool { return n < 3; }
async fn main() {
    let mut i = 0;
    while await more(i) {
        println(i);
        i = i + 1;
    }
    println(99);
}
"#,
        "0\n1\n2\n99\n",
        &["main"],
    );
}

#[test]
fn async_lir_46_loop_body_accumulates_through_a_split_await() {
    assert_lir_logged(
        r#"
async fn twice(n: i64) -> i64 { return n * 2; }
async fn main() {
    let mut acc = 0;
    let mut i = 0;
    while i < 4 {
        acc = acc + await twice(i);
        i = i + 1;
    }
    println(acc);
}
"#,
        "12\n",
        &["main"],
    );
}

#[test]
fn async_lir_47_two_awaits_in_one_statement_use_lir() {
    assert_lir_logged(
        r#"
async fn one() -> i64 { return 1; }
async fn two() -> i64 { return 2; }
async fn main() { println(await one() + await two()); }
"#,
        "3\n",
        &["one", "two", "main"],
    );
}

#[test]
fn async_lir_48_short_circuit_await_uses_lir() {
    assert_lir_logged(
        r#"
async fn flag() -> bool { return false; }
async fn main() {
    let b = false && await flag();
    println(b);
}
"#,
        "false\n",
        &["flag", "main"],
    );
}

#[test]
fn async_lir_49_ternary_await_uses_lir() {
    assert_lir_logged(
        r#"
async fn seven() -> i64 { return 7; }
async fn main() {
    let f = true;
    let v = f ? await seven() : 0;
    println(v);
}
"#,
        "7\n",
        &["seven", "main"],
    );
}

#[test]
fn async_lir_50_match_scrutinee_await_uses_lir() {
    assert_lir_logged(
        r#"
async fn seven() -> i64 { return 7; }
async fn main() {
    let v = match await seven() { 7 => 1, _ => 0 };
    println(v);
}
"#,
        "1\n",
        &["seven", "main"],
    );
}

#[test]
fn async_lir_51_unrepeatable_operand_before_the_await_uses_lir() {
    // `count()` is captured into a frame-backed synthetic local before the
    // park, so it executes exactly once and in source order.
    assert_lir_logged(
        r#"
fn count() -> i64 { println("side"); return 1; }
async fn seven() -> i64 { return 7; }
async fn main() { println(count() + await seven()); }
"#,
        "side\n8\n",
        &["seven", "main"],
    );
}

#[test]
fn async_lir_52_split_result_survives_a_later_suspension() {
    // `a` is defined from a split await and then read after a SECOND await, so
    // the liveness pass has to have given it a frame slot.
    assert_value_position_agrees(
        r#"
async fn bump(x: i64) -> i64 { return x + 1; }
async fn main() {
    let a = await bump(1) + 1;
    let b = await bump(a);
    println(a + b);
}
"#,
        "7\n",
    );
}

#[test]
fn async_lir_53_value_position_mix_agrees_under_every_switch() {
    assert_value_position_agrees(
        r#"
async fn twice(n: i64) -> i64 { return n * 2; }
async fn tag(s: String) -> String { return "[" + s + "]"; }
async fn main() {
    let n = 5;
    println(n + await twice(n));
    println(await tag("a") + "!");
    let mut i = 0;
    let mut acc = 0;
    while i < 3 {
        acc = acc + await twice(i);
        i = i + 1;
    }
    println(acc);
    if await twice(2) > 3 { println("yes"); }
}
"#,
        "15\n[a]!\n6\nyes\n",
    );
}

#[test]
fn async_lir_54_gc_stress_keeps_the_split_string_alive() {
    // Every allocation collects, so an unrooted split result would be freed
    // between the resume and the concatenation that consumes it.
    assert_value_position_agrees(
        r#"
async fn tag(s: String) -> String { return "[" + s + "]"; }
async fn main() {
    let mut i = 0;
    while i < 20 {
        let s = await tag("x") + "!";
        if i == 19 { println(s); }
        i = i + 1;
    }
}
"#,
        "[x]!\n",
    );
}

#[test]
fn async_lir_55_select_cfg_matches_ast_and_survives_gc_stress() {
    let source = r#"
fn pick(ch: Channel<String>) -> Channel<String> {
    println("operand");
    return ch;
}
async fn produce(ch: Channel<String>) {
    await sleep(10);
    ch.send("payload");
}
async fn tag(value: String) -> String {
    await yield();
    return value + "!";
}
async fn main() {
    let ch = Channel<String>::new();
    let producer = produce(ch);
    select {
        let value = pick(ch).recv() => { println(await tag(value)); }
        sleep(5000) => { println("late"); }
    }
    await producer;
}
"#;
    let configs: [&[(&str, &str)]; 4] = [
        &[("WILLOW_LIR_BACKEND", "0")],
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")],
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_TASK_BUDGET", "1"),
        ],
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_GC_STRESS", "alloc"),
        ],
    ];
    for env in configs {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "select run failed under {env:?}: {out}");
        assert_eq!(out, "operand\npayload!\n", "wrong output under {env:?}");
    }
    assert_lir_logged(source, "operand\npayload!\n", &["produce", "tag", "main"]);
}

#[test]
fn async_lir_56_select_randomizes_ready_cases_on_lir() {
    let source = r#"
async fn main() {
    let a = Channel<i64>::new();
    let b = Channel<i64>::new();
    a.send(1);
    b.send(2);
    let mut saw_a = false;
    let mut saw_b = false;
    let mut i = 0;
    while i < 30 {
        select {
            let _ = a.recv() => { saw_a = true; a.send(1); }
            let _ = b.recv() => { saw_b = true; b.send(2); }
        }
        i = i + 1;
    }
    println(saw_a);
    println(saw_b);
}
"#;
    assert_lir_logged(source, "true\ntrue\n", &["main"]);
}

#[test]
fn async_lir_57_defer_registration_and_flush_are_lir_owned() {
    let source = r#"
async fn worker() -> i64 {
    let mut x = 1;
    defer println(x);
    x = 9;
    await sleep(1);
    return x;
}
async fn main() { println(await worker()); }
"#;
    assert_lir_logged(source, "1\n9\n", &["worker", "main"]);
}

#[test]
fn async_lir_58_defer_cancel_entry_survives_gc_stress() {
    let source = r#"
async fn worker() {
    let value = "a" + "b";
    defer println(value + "!");
    await sleep(5000);
}
async fn main() {
    let task = worker();
    await sleep(20);
    task.cancel();
    await sleep(60);
    println("done");
}
"#;
    let (out, ok) = compile_and_run_with_env(
        source,
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_GC_STRESS", "alloc"),
        ],
    );
    assert!(ok, "LIR defer cancellation failed under GC stress: {out}");
    assert_eq!(out, "ab!\ndone\n");
    let (compiled, stderr) = compile_with_compiler_env(
        source,
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_LOG", "1")],
    );
    assert!(compiled, "logged defer compile failed: {stderr}");
    assert!(
        stderr.contains("[lir] compiling async `worker` from lowered IR"),
        "deferred worker did not use LIR: {stderr}"
    );
}

#[test]
fn async_lir_59_channel_send_coerces_class_to_interface_element() {
    let source = r#"
interface Greeter extends Send {
    fn greet(self) -> String;
}
class Dog implements Greeter {
    pub name: String;
    pub fn greet(self) -> String { return "hello " + self.name; }
}
async fn main() {
    let ch = Channel<Greeter>::with_capacity(1);
    ch.send(new Dog("Rex"));
    let greeter = ch.recv();
    println(greeter.greet());
}
"#;
    assert_lir_logged(source, "hello Rex\n", &["main"]);
}

#[test]
fn async_lir_60_try_propagation_flushes_registered_defers() {
    let source = r#"
fn bad() -> Result<i64, String> { return Err("bad"); }
async fn worker() -> Result<i64, String> {
    defer println(9);
    if true {
        defer println(1);
        let value = bad()?;
        return Ok(value);
    }
    return Ok(0);
}
async fn main() {
    let result = await worker();
    match result {
        Ok(value) => println(value),
        Err(error) => println(error),
    }
}
"#;
    assert_lir_logged(source, "1\n9\nbad\n", &["worker", "main"]);
}

#[test]
fn async_lir_61_multiple_value_suspensions_preserve_order() {
    let source = r#"
async fn mark(value: i64) -> i64 {
    println(value);
    await yield();
    return value;
}
async fn main() {
    let total = (await mark(1)) + (await mark(2));
    println(total);
}
"#;
    assert_lir_logged(source, "1\n2\n3\n", &["mark", "main"]);
}

#[test]
fn async_lir_62_task_cancellation_methods_use_lir() {
    // `cancel`/`is_cancelled` read the frame header the handle already points
    // at, so the walker emits them without touching the scheduler task table.
    //
    // The worker parks on a channel nobody sends to, so it can never reach
    // `Completed` first and turn the second probe into a scheduling race.
    let source = r#"
async fn worker(ch: Channel<i64>) -> i64 {
    let value = ch.recv();
    return value;
}
async fn main() {
    let ch = Channel<i64>::new();
    let task = worker(ch);
    println(task.is_cancelled());
    task.cancel();
    println(task.is_cancelled());
}
"#;
    assert_lir_logged(source, "false\ntrue\n", &["worker", "main"]);
}

#[test]
fn async_lir_63_task_result_reports_cancellation_as_a_value() {
    // `Task<T>` and `TaskResult<T>` are the same frame pointer, so only the
    // `cancel_aware` flag lowering records keeps `await t.result()` from
    // raising the way a plain `await t` on a cancelled task does.
    //
    // `stopped` parks on a channel nobody sends to, so whether it reaches
    // `Cancelled` cannot depend on when the scheduler first polled it.
    let source = r#"
async fn worker(n: i64) -> i64 { return n * 2; }
async fn blocked(ch: Channel<i64>) -> i64 { return ch.recv(); }
async fn main() {
    let done = worker(21);
    match await done.result() {
        Ok(value) => println(value),
        Err(cancelled) => println("cancelled"),
    }
    let ch = Channel<i64>::new();
    let stopped = blocked(ch);
    stopped.cancel();
    match await stopped.result() {
        Ok(value) => println(value),
        Err(cancelled) => println("cancelled"),
    }
}
"#;
    assert_lir_logged(source, "42\ncancelled\n", &["worker", "blocked", "main"]);
}

#[test]
fn async_lir_64_task_result_survives_a_suspension_and_gc_stress() {
    // The handle must be frame-backed across the park, and the adapter must not
    // make the collector see a second object where there is only one frame.
    let source = r#"
async fn worker(label: String) -> String { await sleep(1); return label + "!"; }
async fn main() {
    let task = worker("kept");
    await sleep(2);
    gc_collect();
    match await task.result() {
        Ok(value) => println(value),
        Err(cancelled) => println("cancelled"),
    }
}
"#;
    let configs: [&[(&str, &str)]; 4] = [
        &[("WILLOW_LIR_BACKEND", "0")],
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")],
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_TASK_BUDGET", "1"),
        ],
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_GC_STRESS", "alloc"),
        ],
    ];
    for env in configs {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "task-result run failed under {env:?}: {out}");
        assert_eq!(out, "kept!\n", "wrong output under {env:?}");
    }
    assert_lir_logged(source, "kept!\n", &["worker", "main"]);
}

#[test]
fn async_lir_65_select_join_over_task_result_binds_the_wrapped_type() {
    // The case binds `Result<T, Cancelled>`, a type no HIR node in the case
    // holds — which is why the eligibility binding map owns its values. The
    // payload `T` is what reaches the terminal-value emitter; handing it the
    // wrapper instead would allocate an `Ok` whose payload slot claims the
    // wrapper's representation, and for a scalar `T` the collector would then
    // trace the integer as a pointer.
    let source = r#"
async fn worker(n: i64) -> i64 { await sleep(1); return n * 2; }
async fn main() {
    let good = worker(21);
    select {
        let r = await good.result() => {
            match r {
                Ok(value) => { gc_collect(); println(value); }
                Err(cancelled) => println("cancelled"),
            }
        }
        sleep(5000) => { println("timeout"); }
    }
    let stopped = worker(3);
    stopped.cancel();
    select {
        let r = await stopped.result() => {
            match r {
                Ok(value) => println(value),
                Err(cancelled) => println("cancelled"),
            }
        }
        sleep(5000) => { println("timeout"); }
    }
}
"#;
    let configs: [&[(&str, &str)]; 4] = [
        &[("WILLOW_LIR_BACKEND", "0")],
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")],
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_TASK_BUDGET", "1"),
        ],
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_GC_STRESS", "alloc"),
        ],
    ];
    for env in configs {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "select-join run failed under {env:?}: {out}");
        assert_eq!(out, "42\ncancelled\n", "wrong output under {env:?}");
    }
    assert_lir_logged(source, "42\ncancelled\n", &["worker", "main"]);
}

#[test]
fn async_lir_66_task_result_payload_is_not_traced_as_a_pointer() {
    // Regression: a scalar payload wrapped by `await t.result()` must not reach
    // the collector as a GC reference.
    let source = r#"
async fn worker(n: i64) -> i64 { await sleep(1); return n * 2; }
async fn main() {
    let task = worker(21);
    match await task.result() {
        Ok(value) => { gc_collect(); println(value); }
        Err(cancelled) => println("cancelled"),
    }
}
"#;
    for env in [
        &[("WILLOW_LIR_BACKEND", "0")][..],
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")][..],
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_GC_STRESS", "alloc"),
        ][..],
    ] {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "scalar task-result run failed under {env:?}: {out}");
        assert_eq!(out, "42\n", "wrong output under {env:?}");
    }
    assert_lir_logged(source, "42\n", &["worker", "main"]);
}

#[test]
fn async_lir_67_a_parked_select_probe_does_not_consume_a_rotation() {
    // `willow_select_rotation` is a PROCESS-WIDE counter: every call advances
    // the fairness sequence seen by every other select in the program. The AST
    // emitter calls it only on a probe that actually picks a case, so the LIR
    // walker must too — otherwise a select that parks before anything is ready
    // silently rotates the picks made by later selects.
    //
    // The first select here has nothing ready and must wait for `late`, so it
    // probes an unready set at least once. The loop that follows always has
    // BOTH cases ready, so its picks are decided purely by the rotation counter
    // and expose any extra call the parked probe made.
    let source = r#"
async fn late(ch: Channel<i64>) -> i64 {
    await sleep(5);
    ch.send(7);
    return 0;
}

async fn main() {
    let slow = Channel<i64>::new();
    let waiter = late(slow);
    select {
        let v = slow.recv() => { println("woke " + v.toString()); }
    }
    await waiter;

    let a = Channel<i64>::with_capacity(16);
    let b = Channel<i64>::with_capacity(16);
    let mut i = 0;
    while i < 8 {
        a.send(1);
        b.send(2);
        select {
            let x = a.recv() => { println(x); }
            let y = b.recv() => { println(y); }
        }
        i = i + 1;
    }
}
"#;
    let (ast, ok) = compile_and_run_with_env(source, &[("WILLOW_LIR_BACKEND", "0")]);
    assert!(ok, "AST run failed: {ast}");
    assert_eq!(ast.lines().count(), 9, "unexpected AST output: {ast}");

    // Repeated runs pin the other half of the property: with the parked probe
    // no longer consuming a rotation, the pick sequence stops depending on how
    // many times the first select waited, so it is stable across runs and
    // across scheduler pressure.
    for env in [
        &LIR_REQUIRED[..],
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_TASK_BUDGET", "1"),
        ][..],
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_GC_STRESS", "alloc"),
        ][..],
    ] {
        for run in 0..3 {
            let (out, ok) = compile_and_run_with_env(source, env);
            assert!(ok, "LIR run {run} failed under {env:?}: {out}");
            assert_eq!(
                out, ast,
                "select fairness sequence diverged on run {run} under {env:?}"
            );
        }
    }
}

// Recovery-capable cooperative `defer` on the LIR path (willow-0g8j.2.11).
//
// `defer match recover() { .. }` gives its scope a second exit: the panic ends
// there and the statement after the scope runs. In an async body that
// continuation is reached after a poll return, so a local read only on that
// path must still be in the task frame. Lowering records the edge on every
// block of the scope (`LirBlock::recovery`) and the frame layout is computed
// with it; before that, such a local looked dead across the suspension, lost
// its slot, and read back as zero.
//
// 20 perspectives, each asserted to agree with the AST emitter and to run
// unchanged under scheduler pressure and GC stress:
//   1 scalar live only on the recovery path   11 recovery inside a loop
//   2 String (traced pointer) ditto           12 loop keeps iterating after one
//   3 Array (traced object) ditto             13 channel-resume continuation
//   4 class object ditto                      14 select-resume continuation
//   5 recovery before the first suspension    15 awaited task in the scope
//   6 recovery after a resume                 16 sibling task is undisturbed
//   7 value written after the recovery        17 cancellation beside recovery
//   8 non-recovering defer still runs         18 defer consumed exactly once
//   9 two defers run in reverse order         19 panic raised inside a defer
//  10 nested scopes, inner recovers           20 early `return` out of a scope
const RECOVERY_ENVS: [&[(&str, &str)]; 4] = [
    &[("WILLOW_LIR_BACKEND", "0")],
    &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")],
    &[
        ("WILLOW_LIR_BACKEND", "1"),
        ("WILLOW_LIR_REQUIRE", "1"),
        ("WILLOW_TASK_BUDGET", "1"),
    ],
    &[
        ("WILLOW_LIR_BACKEND", "1"),
        ("WILLOW_LIR_REQUIRE", "1"),
        ("WILLOW_GC_STRESS", "alloc"),
    ],
];

/// Every configuration must produce `expected`, and the LIR path must actually
/// be the one compiling `functions` rather than a fallback that agrees by
/// virtue of being the same emitter.
fn assert_recovery(source: &str, expected: &str, functions: &[&str]) {
    for env in RECOVERY_ENVS {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "recovery run failed under {env:?}: {out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
    assert_lir_logged(source, expected, functions);
}

#[test]
fn async_lir_68_recovery_keeps_a_scalar_read_only_on_the_recovered_path() {
    assert_recovery(
        r#"
async fn run(fail: bool) -> i64 {
    let mut count = 7;
    if true {
        defer match recover() {
            Some(info) => println("d: " + info.message),
            None => println("d: clean")
        }
        await sleep(1);
        if fail { panic("stop"); }
        count = 100;
    }
    return count;
}
async fn main() {
    println(await run(false));
    println(await run(true));
}
"#,
        "d: clean\n100\nd: stop\n7\n",
        &["run", "main"],
    );
}

#[test]
fn async_lir_69_recovery_keeps_a_string_read_only_on_the_recovered_path() {
    // A String is a traced pointer: losing the frame slot reads back null and
    // prints as empty rather than as the value bound before the scope.
    assert_recovery(
        r#"
async fn run(fail: bool) -> String {
    let mut label = "kept";
    if true {
        defer match recover() {
            Some(info) => println("d: " + info.message),
            None => println("d: clean")
        }
        await sleep(1);
        if fail { panic("stop"); }
        label = "replaced";
    }
    return label;
}
async fn main() {
    println(await run(false));
    println(await run(true));
}
"#,
        "d: clean\nreplaced\nd: stop\nkept\n",
        &["run", "main"],
    );
}

#[test]
fn async_lir_70_recovery_keeps_an_array_read_only_on_the_recovered_path() {
    assert_recovery(
        r#"
import std::collections::Array;
async fn run(fail: bool) -> i64 {
    let mut values: Array<i64> = [2, 3, 4];
    if true {
        defer match recover() {
            Some(info) => println("d: " + info.message),
            None => println("d: clean")
        }
        await sleep(1);
        if fail { panic("stop"); }
        values = [20];
    }
    let mut sum = 0;
    let mut i = 0;
    while i < values.len() {
        sum = sum + values[i];
        i = i + 1;
    }
    return sum;
}
async fn main() {
    println(await run(false));
    println(await run(true));
}
"#,
        "d: clean\n20\nd: stop\n9\n",
        &["run", "main"],
    );
}

#[test]
fn async_lir_71_recovery_keeps_an_object_read_only_on_the_recovered_path() {
    assert_recovery(
        r#"
class Account { pub owner: String; pub balance: i64; }
async fn run(fail: bool) -> String {
    let mut account = new Account("Alice", 10);
    if true {
        defer match recover() {
            Some(info) => println("d: " + info.message),
            None => println("d: clean")
        }
        await sleep(1);
        if fail { panic("stop"); }
        account = new Account("Bob", 20);
    }
    return account.owner + ":" + account.balance.toString();
}
async fn main() {
    println(await run(false));
    println(await run(true));
}
"#,
        "d: clean\nBob:20\nd: stop\nAlice:10\n",
        &["run", "main"],
    );
}

#[test]
fn async_lir_72_recovery_before_the_first_suspension_still_completes_the_task() {
    // Nothing has parked yet, so the recovery continuation is reached inside
    // the FIRST poll rather than after a re-entry.
    assert_recovery(
        r#"
async fn main() {
    if true {
        defer match recover() {
            Some(info) => println(info.message),
            None => println("none")
        }
        panic("early");
    }
    println("after");
}
"#,
        "early\nafter\n",
        &["main"],
    );
}

#[test]
fn async_lir_73_recovery_after_a_resume_keeps_the_task_running() {
    assert_recovery(
        r#"
async fn main() {
    let mut stage = 0;
    if true {
        defer match recover() {
            Some(info) => println(info.message),
            None => println("none")
        }
        await sleep(1);
        stage = 1;
        await sleep(1);
        panic("late");
    }
    println(stage);
    await sleep(1);
    println("still running");
}
"#,
        "late\n1\nstill running\n",
        &["main"],
    );
}

#[test]
fn async_lir_74_a_value_written_after_the_recovery_is_the_one_returned() {
    assert_recovery(
        r#"
async fn run() -> i64 {
    let mut v = 1;
    if true {
        defer match recover() {
            Some(_) => println("caught"),
            None => println("none")
        }
        await sleep(1);
        panic("stop");
    }
    v = v + 41;
    await sleep(1);
    return v;
}
async fn main() { println(await run()); }
"#,
        "caught\n42\n",
        &["run", "main"],
    );
}

#[test]
fn async_lir_75_a_plain_defer_in_the_recovering_scope_still_runs() {
    assert_recovery(
        r#"
async fn main() {
    if true {
        defer println("cleanup");
        defer match recover() {
            Some(info) => println(info.message),
            None => println("none")
        }
        await sleep(1);
        panic("stop");
    }
    println("after");
}
"#,
        "stop\ncleanup\nafter\n",
        &["main"],
    );
}

#[test]
fn async_lir_76_defers_in_one_scope_run_in_reverse_registration_order() {
    assert_recovery(
        r#"
async fn main() {
    if true {
        defer println("first");
        defer println("second");
        defer match recover() {
            Some(info) => println(info.message),
            None => println("none")
        }
        await sleep(1);
        panic("stop");
    }
    println("after");
}
"#,
        "stop\nsecond\nfirst\nafter\n",
        &["main"],
    );
}

#[test]
fn async_lir_77_an_inner_recovery_scope_keeps_the_panic_from_the_outer_one() {
    assert_recovery(
        r#"
async fn run(step: i64) -> i64 {
    let mut trail = 0;
    if true {
        defer match recover() {
            Some(info) => println("outer: " + info.message),
            None => println("outer: clean")
        }
        if true {
            defer match recover() {
                Some(info) => println("inner: " + info.message),
                None => println("inner: clean")
            }
            await sleep(1);
            if step == 1 { panic("inner"); }
            trail = trail + 1;
        }
        await yield();
        if step == 2 { panic("outer"); }
        trail = trail + 10;
    }
    return trail;
}
async fn main() {
    println(await run(0));
    println(await run(1));
    println(await run(2));
}
"#,
        "inner: clean\nouter: clean\n11\ninner: inner\nouter: clean\n10\n\
         inner: clean\nouter: outer\n1\n",
        &["run", "main"],
    );
}

#[test]
fn async_lir_78_a_panic_passes_a_non_recovering_scope_to_the_recovering_one() {
    assert_recovery(
        r#"
async fn run() -> i64 {
    let mut seen = 5;
    if true {
        defer match recover() {
            Some(info) => println("outer: " + info.message),
            None => println("outer: clean")
        }
        if true {
            defer println("inner cleanup");
            await sleep(1);
            panic("through");
        }
        seen = 99;
    }
    return seen;
}
async fn main() { println(await run()); }
"#,
        "inner cleanup\nouter: through\n5\n",
        &["run", "main"],
    );
}

#[test]
fn async_lir_79_a_recovery_scope_in_a_loop_keeps_iterating() {
    assert_recovery(
        r#"
async fn run(n: i64) -> i64 {
    let mut done = 0;
    let mut i = 0;
    while i < n {
        if true {
            defer match recover() {
                Some(info) => println("loop: " + info.message),
                None => println("loop: ok")
            }
            await sleep(1);
            if i == 1 { panic("iteration " + i.toString()); }
            done = done + 1;
        }
        i = i + 1;
    }
    return done;
}
async fn main() { println(await run(3)); }
"#,
        "loop: ok\nloop: iteration 1\nloop: ok\n2\n",
        &["run", "main"],
    );
}

#[test]
fn async_lir_80_a_recovery_scope_in_a_for_loop_keeps_its_induction_variable() {
    // The induction variable is a LIR-synthesized local, so it has no source
    // span to key a frame slot on - only a `LirLocalId`.
    assert_recovery(
        r#"
async fn run() -> i64 {
    let mut sum = 0;
    for i in 0..4 {
        if true {
            defer match recover() {
                Some(_) => println("skip " + i.toString()),
                None => println("take " + i.toString())
            }
            await sleep(1);
            if i == 2 { panic("bad"); }
            sum = sum + i;
        }
    }
    return sum;
}
async fn main() { println(await run()); }
"#,
        "take 0\ntake 1\nskip 2\ntake 3\n4\n",
        &["run", "main"],
    );
}

#[test]
fn async_lir_81_recovery_reached_from_a_channel_resume_state() {
    assert_recovery(
        r#"
async fn feed(ch: Channel<i64>) -> i64 {
    await sleep(1);
    ch.send(5);
    return 0;
}
async fn run(fail: bool) -> i64 {
    let mut seen = -1;
    let ch = Channel<i64>::new();
    let producer = feed(ch);
    if true {
        defer match recover() {
            Some(info) => println("d: " + info.message),
            None => println("d: clean")
        }
        let got = ch.recv();
        if fail { panic("dropped " + got.toString()); }
        seen = got;
    }
    await producer;
    return seen;
}
async fn main() {
    println(await run(false));
    println(await run(true));
}
"#,
        "d: clean\n5\nd: dropped 5\n-1\n",
        &["feed", "run", "main"],
    );
}

#[test]
fn async_lir_82_recovery_reached_from_a_select_resume_state() {
    assert_recovery(
        r#"
async fn feed(ch: Channel<i64>) -> i64 {
    await sleep(1);
    ch.send(9);
    return 0;
}
async fn run(fail: bool) -> i64 {
    let mut seen = -1;
    let ch = Channel<i64>::new();
    let producer = feed(ch);
    if true {
        defer match recover() {
            Some(info) => println("d: " + info.message),
            None => println("d: clean")
        }
        select {
            let v = ch.recv() => {
                if fail { panic("dropped " + v.toString()); }
                seen = v;
            }
        }
    }
    await producer;
    return seen;
}
async fn main() {
    println(await run(false));
    println(await run(true));
}
"#,
        "d: clean\n9\nd: dropped 9\n-1\n",
        &["feed", "run", "main"],
    );
}

#[test]
fn async_lir_83_recovery_after_awaiting_a_task_inside_the_scope() {
    assert_recovery(
        r#"
async fn double(n: i64) -> i64 { await sleep(1); return n * 2; }
async fn run(fail: bool) -> i64 {
    let mut kept = 3;
    if true {
        defer match recover() {
            Some(info) => println("d: " + info.message),
            None => println("d: clean")
        }
        let doubled = await double(20);
        if fail { panic("drop " + doubled.toString()); }
        kept = doubled;
    }
    return kept;
}
async fn main() {
    println(await run(false));
    println(await run(true));
}
"#,
        "d: clean\n40\nd: drop 40\n3\n",
        &["double", "run", "main"],
    );
}

#[test]
fn async_lir_84_a_sibling_task_is_undisturbed_by_a_recovery() {
    assert_recovery(
        r#"
async fn quiet(rounds: i64) -> i64 {
    let mut i = 0;
    while i < rounds {
        await sleep(1);
        i = i + 1;
    }
    return rounds;
}
async fn noisy() -> i64 {
    let mut v = 1;
    if true {
        defer match recover() {
            Some(info) => println("d: " + info.message),
            None => println("d: clean")
        }
        await sleep(1);
        panic("stop");
    }
    return v;
}
async fn main() {
    let other = quiet(3);
    println(await noisy());
    println(await other);
}
"#,
        "d: stop\n1\n3\n",
        &["quiet", "noisy", "main"],
    );
}

#[test]
fn async_lir_85_cancelling_a_task_with_an_open_recovery_scope_runs_its_defer_once() {
    // Cancellation walks the frame's registered defer sites. A scope that was
    // never left must still be cleaned up, and exactly once.
    assert_recovery(
        r#"
async fn slow() -> i64 {
    if true {
        defer match recover() {
            Some(info) => println("d: " + info.message),
            None => println("d: cancelled path")
        }
        await sleep(200);
        println("never");
    }
    return 1;
}
async fn main() {
    let t = slow();
    await sleep(1);
    t.cancel();
    match await t.result() {
        Ok(v) => println(v),
        Err(cancelled) => println("cancelled"),
    }
}
"#,
        "d: cancelled path\ncancelled\n",
        &["slow", "main"],
    );
}

#[test]
fn async_lir_86_a_defer_consumed_by_the_recovery_is_not_rerun_by_a_later_cancel() {
    assert_recovery(
        r#"
async fn run() -> i64 {
    if true {
        defer match recover() {
            Some(info) => println("d: " + info.message),
            None => println("d: clean")
        }
        await sleep(1);
        panic("stop");
    }
    await sleep(50);
    return 7;
}
async fn main() {
    let t = run();
    await sleep(5);
    t.cancel();
    match await t.result() {
        Ok(v) => println(v),
        Err(cancelled) => println("cancelled"),
    }
}
"#,
        "d: stop\ncancelled\n",
        &["run", "main"],
    );
}

#[test]
fn async_lir_87_a_panic_raised_inside_a_defer_body_reaches_the_outer_scope() {
    assert_recovery(
        r#"
async fn main() {
    if true {
        defer match recover() {
            Some(info) => println("outer: " + info.message),
            None => println("outer: clean")
        }
        if true {
            // Block form: a bare `defer panic(...)` is an expression body the
            // walker does not accept (it registers no value), which is a
            // separate limit of `supported_expr` and not about recovery.
            defer {
                println("cleanup");
                panic("from cleanup");
            }
            await sleep(1);
        }
        println("unreached");
    }
    println("after");
}
"#,
        "cleanup\nouter: from cleanup\nafter\n",
        &["main"],
    );
}

#[test]
fn async_lir_88_an_early_return_out_of_a_recovery_scope_skips_the_continuation() {
    // The scope has two exits that are NOT the recovery: the `return` flushes
    // its defers and leaves, so the statement after the scope never runs.
    assert_recovery(
        r#"
async fn run(early: bool) -> i64 {
    let mut v = 0;
    if true {
        defer match recover() {
            Some(info) => println("d: " + info.message),
            None => println("d: clean")
        }
        await sleep(1);
        if early { return 11; }
        v = 22;
    }
    println("continued");
    return v;
}
async fn main() {
    println(await run(true));
    println(await run(false));
}
"#,
        "d: clean\n11\nd: clean\ncontinued\n22\n",
        &["run", "main"],
    );
}
