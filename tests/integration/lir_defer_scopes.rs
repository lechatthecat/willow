//! Synchronous `defer` scopes across LIR blocks (willow-0g8j.2.15).
//!
//! A `defer` scope is left by more than fallthrough. An explicit `return`, a
//! `break`, a `continue` and a `?` that propagates all leave early, and each has
//! to run the pending registrations of every scope it unwinds, innermost first.
//! The LIR spells those exits `flush defers`, and on the synchronous emitter
//! that instruction did nothing at all — it was matched and dropped, so every
//! defer on an early-exit path was silently skipped. Eligibility hid the bug by
//! rejecting most of these functions first: it required a block's own scope
//! pushes and pops to balance within that one block.
//!
//! Three things changed. `FlushDefers` now emits the unwinding for the scopes
//! its site list covers. The emitter keeps its defer state per LIR block instead
//! of carrying one stack through the whole body, because LIR blocks are NOT
//! emitted in the order control flows through them — a loop latch is emitted
//! before the body that jumps to it, and a block after a `return` can open with
//! a different set of scopes. And a scope's panic-cleanup block is sealed only
//! once the body is finished, since an inner scope's cleanup jumps to its
//! parent's and block order can finish the inner one last.
//!
//! Every test is differential: the same program under the AST emitter and under
//! the walker must print the same thing. The walker side sets
//! `WILLOW_LIR_REQUIRE=1`, so a silent fallback fails the test instead of
//! quietly comparing the AST emitter against itself, and each program also runs
//! under `WILLOW_GC_STRESS=alloc`, because a defer body allocates while the
//! value being returned past it is still live.
//!
//! 24 perspectives:
//!   1 `return` in the scope's own block     13 a defer per if/else arm
//!   2 `return` out of a nested `if`         14 three scopes deep, inner return
//!   3 `return` out of a loop body           15 a GC value returned past a defer
//!   4 `break` runs that iteration's defer   16 `main`'s own early `return`
//!   5 `continue` runs it every iteration    17 an unregistered defer stays quiet
//!   6 `while` + `break`                     18 a panic still runs no defers
//!   7 two defers in one scope, LIFO         19 a defer body with its own defers
//!   8 `?` on an Err unwinds the scope       20 the walker really compiled these
//!   9 `?` on a `None` does too              21 async keeps the coop path
//!  10 a binding made after registration     22 a succeeding `?` runs nothing
//!  11 the body sees the captured value      23 defers run in source order
//!  12 fallthrough leaves only the inner     24 the example is fully LIR

use super::support::{compile_and_run_with_env, compile_with_compiler_env};

const AST: [(&str, &str); 1] = [("WILLOW_LIR_BACKEND", "0")];
const LIR: [(&str, &str); 2] = [("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")];
const LIR_STRESS: [(&str, &str); 3] = [
    ("WILLOW_LIR_BACKEND", "1"),
    ("WILLOW_LIR_REQUIRE", "1"),
    ("WILLOW_GC_STRESS", "alloc"),
];

/// A `Result`-returning helper, so `?` has something to propagate.
const PARSE: &str = "fn parse(n: i64) -> Result<i64, String> {
    if n < 0 { return Err(\"bad\"); }
    return Ok(n);
}
";

/// Run `source` under all three configurations and require identical output.
/// `WILLOW_LIR_REQUIRE=1` on two of them turns a fallback into a compile error,
/// which is what makes this a comparison of two emitters rather than one.
fn assert_defers(source: &str, expected: &str) {
    for env in [&AST[..], &LIR[..], &LIR_STRESS[..]] {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "program failed under {env:?}: {out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
}

/// Compile once with the selection log on and require the walker to have taken
/// each named function. Without this a coverage regression would still print the
/// right answer — from the AST emitter.
fn assert_walker_compiled(source: &str, functions: &[&str]) {
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
        let sync = format!("[lir] compiling `{function}` from lowered IR");
        let coop = format!("[lir] compiling async `{function}` from lowered IR");
        assert!(
            stderr.contains(&sync) || stderr.contains(&coop),
            "`{function}` did not use the LIR walker: {stderr}"
        );
    }
}

// 1. The smallest shape there is, and the one the old eligibility check already
//    rejected: an explicit `return` inside a `defer` scope lowers to `flush
//    defers` followed by `return`, both in the entry block.
#[test]
fn lir_defer_scopes_01_return_in_the_scopes_own_block() {
    assert_defers(
        "fn doubled(n: i64) -> i64 {
    defer println(\"out\");
    return n * 2;
}
fn main() { println(doubled(3)); }
",
        "out\n6\n",
    );
}

// 2. Two scopes at once. The flush names both scopes' registrations, and they
//    run innermost first — the `if` body's before the function's.
#[test]
fn lir_defer_scopes_02_return_out_of_a_nested_if() {
    assert_defers(
        "fn pick(n: i64) -> i64 {
    defer println(\"outer\");
    if n > 0 {
        defer println(\"inner\");
        return 1;
    }
    return 0;
}
fn main() { println(pick(5)); println(pick(-1)); }
",
        "inner\nouter\n1\nouter\n0\n",
    );
}

// 3. A `return` out of a loop body. The loop's own blocks are emitted out of
//    control-flow order — the latch before the body — so this is the shape that
//    needs per-block defer state rather than one carried stack.
#[test]
fn lir_defer_scopes_03_return_out_of_a_loop_body() {
    assert_defers(
        "fn first_big(limit: i64) -> i64 {
    defer println(\"fn\");
    for i in 0..limit {
        defer println(i);
        if i * i > 5 { return i; }
    }
    return -1;
}
fn main() { println(first_big(9)); }
",
        "0\n1\n2\n3\nfn\n3\n",
    );
}

// 4. `break` leaves only the loop body's scope. The iteration that breaks still
//    runs its own registration, and no later iteration exists to run one.
#[test]
fn lir_defer_scopes_04_break_runs_that_iterations_defer() {
    assert_defers(
        "fn stop(limit: i64) -> i64 {
    let mut seen = 0;
    for i in 0..limit {
        defer println(i);
        if i == 2 { break; }
        seen = seen + 1;
    }
    return seen;
}
fn main() { println(stop(5)); }
",
        "0\n1\n2\n2\n",
    );
}

// 5. `continue` is an exit from the body scope too, so the defer runs on the
//    skipped iteration exactly like on a completed one.
#[test]
fn lir_defer_scopes_05_continue_runs_it_every_iteration() {
    assert_defers(
        "fn skip(limit: i64) -> i64 {
    let mut total = 0;
    for i in 0..limit {
        defer println(i);
        if i == 1 { continue; }
        total = total + i;
    }
    return total;
}
fn main() { println(skip(4)); }
",
        "0\n1\n2\n3\n5\n",
    );
}

// 6. The same for a `while`, whose LIR has a different block shape than the
//    counted `for` above.
#[test]
fn lir_defer_scopes_06_while_with_break() {
    assert_defers(
        "fn loopy() -> i64 {
    let mut i = 0;
    while true {
        defer println(i);
        i = i + 1;
        if i == 3 { break; }
    }
    return i;
}
fn main() { println(loopy()); }
",
        "0\n1\n2\n3\n",
    );
}

// 7. Ordering WITHIN one scope, which the flush has to preserve: the last
//    registration runs first.
#[test]
fn lir_defer_scopes_07_two_defers_in_one_scope_are_lifo() {
    assert_defers(
        "fn two() -> i64 {
    defer println(\"a\");
    defer println(\"b\");
    return 7;
}
fn main() { println(two()); }
",
        "b\na\n7\n",
    );
}

// 8. `?` is an exit like any other. The walker used to reject `defer` combined
//    with `?` outright; now the propagating path unwinds the open scopes with
//    the failure value kept reachable across cleanup that may allocate.
#[test]
fn lir_defer_scopes_08_try_on_an_err_unwinds_the_scope() {
    assert_defers(
        &format!(
            "{PARSE}
fn sum(a: i64, b: i64) -> Result<i64, String> {{
    defer println(\"leave sum\");
    let x = parse(a)?;
    let y = parse(b)?;
    return Ok(x + y);
}}
fn main() {{
    match sum(1, 2) {{ Ok(v) => println(v), Err(e) => println(e) }}
    match sum(1, -1) {{ Ok(v) => println(v), Err(e) => println(e) }}
}}
"
        ),
        "leave sum\n3\nleave sum\nbad\n",
    );
}

// 9. The `Option` form of the same exit, which builds a `None` rather than
//    re-wrapping a payload before unwinding.
#[test]
fn lir_defer_scopes_09_try_on_a_none_unwinds_too() {
    assert_defers(
        "fn head(n: i64) -> Option<i64> {
    if n < 0 { return None; }
    return Some(n);
}
fn twice(n: i64) -> Option<i64> {
    defer println(\"leave twice\");
    let v = head(n)?;
    return Some(v * 2);
}
fn main() {
    match twice(4) { Some(v) => println(v), None => println(\"none\") }
    match twice(-1) { Some(v) => println(v), None => println(\"none\") }
}
",
        "leave twice\n8\nleave twice\nnone\n",
    );
}

// 10. The flush emits each registration under the bindings it captured, which
//     rewrites the emitter's variable table. Whatever follows the flush — here
//     the `return` of a value bound AFTER the registration — still needs its
//     own bindings, so the table is restored. Without that this crashed the
//     compiler with an unbound variable, on BOTH emitters.
#[test]
fn lir_defer_scopes_10_a_binding_made_after_registration_still_returns() {
    assert_defers(
        "fn later() -> i64 {
    let a = 1;
    defer println(\"d\");
    let b = a + 41;
    return b;
}
fn main() { println(later()); }
",
        "d\n42\n",
    );
}

// 11. The other half of that: the deferred body must still see the value the
//     registration captured, not the one the variable holds at exit.
#[test]
fn lir_defer_scopes_11_the_body_sees_the_captured_value() {
    assert_defers(
        "fn captured() -> i64 {
    let mut n = 1;
    defer println(n);
    n = 99;
    return n;
}
fn main() { println(captured()); }
",
        "1\n99\n",
    );
}

// 12. Falling out of an inner scope is not an exit from the outer one: only the
//     inner registration runs there, and the outer waits for the function to
//     end.
#[test]
fn lir_defer_scopes_12_fallthrough_leaves_only_the_inner_scope() {
    assert_defers(
        "fn nested(n: i64) -> i64 {
    defer println(\"outer\");
    if n > 0 {
        defer println(\"inner\");
        println(\"body\");
    }
    return n;
}
fn main() { println(nested(1)); }
",
        "body\ninner\nouter\n1\n",
    );
}

// 13. Two sibling scopes, each with its own exit. Only the arm that runs
//     registers anything, so only its defer can run.
#[test]
fn lir_defer_scopes_13_a_defer_per_if_else_arm() {
    assert_defers(
        "fn branch(n: i64) -> i64 {
    if n > 0 {
        defer println(\"then\");
        return 1;
    } else {
        defer println(\"else\");
        return 2;
    }
}
fn main() { println(branch(1)); println(branch(-1)); }
",
        "then\n1\nelse\n2\n",
    );
}

// 14. Three scopes deep with the `return` in the innermost. A scope's panic
//     cleanup jumps to its PARENT's cleanup block, and here the LIR emits the
//     block that closes the middle scope before the one that closes the inner
//     one — so sealing a cleanup when its scope finished sealed a block a later
//     child still had to name, and Cranelift asserted.
#[test]
fn lir_defer_scopes_14_three_scopes_deep_with_an_inner_return() {
    assert_defers(
        "fn deep(limit: i64) -> i64 {
    defer println(\"fn\");
    for i in 0..limit {
        defer println(i);
        if i > 0 {
            defer println(\"inner\");
            if i == 2 { return i; }
        }
    }
    return -1;
}
fn main() { println(deep(5)); }
",
        "0\ninner\n1\ninner\n2\nfn\n2\n",
    );
}

// 15. The returned object is heap-allocated and the defer body allocates while
//     unwinding past it, so the value has to be rooted across the flush. Under
//     `WILLOW_GC_STRESS=alloc` every allocation collects, which is what makes
//     this perspective bite.
#[test]
fn lir_defer_scopes_15_a_gc_value_returned_past_a_defer() {
    assert_defers(
        "class Box { pub v: i64; }
fn make(n: i64) -> Box {
    defer println(\"leave make\");
    let b = new Box(n);
    return b;
}
fn main() { println(make(9).v); }
",
        "leave make\n9\n",
    );
}

// 16. `main` is compiled like any other function here, and a bare `return`
//     leaving its scope still runs what it registered.
#[test]
fn lir_defer_scopes_16_main_with_an_early_return() {
    assert_defers(
        "fn main() {
    defer println(\"bye\");
    if true {
        println(\"hi\");
        return;
    }
    println(\"unreachable\");
}
",
        "hi\nbye\n",
    );
}

// 17. A registration is reached, or it is not. The branch that never enters the
//     scope has nothing to run, which is a run-time property of the flag the
//     registration sets rather than of the scope's shape.
#[test]
fn lir_defer_scopes_17_an_unregistered_defer_stays_quiet() {
    assert_defers(
        "fn maybe(n: i64) -> i64 {
    if n > 0 {
        defer println(\"registered\");
        return 1;
    }
    return 0;
}
fn main() { println(maybe(-1)); println(maybe(1)); }
",
        "0\nregistered\n1\n",
    );
}

// 18. Unchanged by all of this: a panic aborts, and the defers of the frame it
//     is leaving do NOT run as an ordinary exit — the panic cleanup CFG owns
//     that path. The call that succeeds first proves the normal exit still runs
//     its defer, so this is not vacuous.
#[test]
fn lir_defer_scopes_18_a_panic_still_runs_no_defers() {
    let source = "fn boom(n: i64) -> i64 {
    defer println(\"cleanup\");
    if n == 0 { panic(\"no\"); }
    return n;
}
fn main() { println(boom(1)); println(boom(0)); println(\"after\"); }
";
    for env in [&AST[..], &LIR[..], &LIR_STRESS[..]] {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(!ok, "a panicking program must not succeed under {env:?}");
        assert!(
            out.starts_with("cleanup\n1\ncleanup\n"),
            "wrong output under {env:?}: {out}"
        );
        assert!(
            out.contains("runtime panic: no"),
            "the panic was not reported under {env:?}: {out}"
        );
        assert!(
            !out.contains("after"),
            "execution continued past the panic under {env:?}: {out}"
        );
    }
}

// 19. The deferred call has `defer`s of its own. The inner function's scope is
//     an ordinary one that happens to be entered from cleanup code, so its
//     unwinding must not disturb the flush that called it.
#[test]
fn lir_defer_scopes_19_a_defer_body_with_its_own_defers() {
    assert_defers(
        "fn inner() {
    defer println(\"inner defer\");
    println(\"inner body\");
}
fn outer(n: i64) -> i64 {
    defer inner();
    if n > 0 { return 1; }
    return 0;
}
fn main() { println(outer(1)); }
",
        "inner body\ninner defer\n1\n",
    );
}

// 20. Coverage, not behavior. Every function above would print the same thing
//     from the AST emitter, so the suite is only meaningful while the walker is
//     the one compiling them.
#[test]
fn lir_defer_scopes_20_the_walker_really_compiled_these() {
    assert_walker_compiled(
        &format!(
            "{PARSE}
fn doubled(n: i64) -> i64 {{
    defer println(\"out\");
    return n * 2;
}}
fn first_big(limit: i64) -> i64 {{
    defer println(\"fn\");
    for i in 0..limit {{
        defer println(i);
        if i * i > 5 {{ return i; }}
    }}
    return -1;
}}
fn sum(a: i64, b: i64) -> Result<i64, String> {{
    defer println(\"leave sum\");
    let x = parse(a)?;
    return Ok(x + b);
}}
fn main() {{
    defer println(\"bye\");
    println(doubled(1));
    println(first_big(9));
    match sum(1, 2) {{ Ok(v) => println(v), Err(e) => println(e) }}
}}
"
        ),
        &["parse", "doubled", "first_big", "sum", "main"],
    );
}

// 21. Async functions rebuild every scope from the LIR at each exit, guarded by
//     heap flags, and none of that changed. Pinned here so the synchronous work
//     cannot quietly reroute the cooperative path.
#[test]
fn lir_defer_scopes_21_async_keeps_the_cooperative_path() {
    let source = "async fn work(n: i64) -> i64 {
    defer println(\"leave work\");
    if n > 0 { return n * 2; }
    return 0;
}
async fn main() {
    defer println(\"leave main\");
    println(await work(3));
}
";
    assert_defers(source, "leave work\n6\nleave main\n");
    assert_walker_compiled(source, &["work", "main"]);
}

// 22. A `?` that succeeds is not an exit: the scope stays open and the rest of
//     the function runs before anything is unwound.
#[test]
fn lir_defer_scopes_22_a_succeeding_try_runs_nothing() {
    assert_defers(
        &format!(
            "{PARSE}
fn ok_path(n: i64) -> Result<i64, String> {{
    defer println(\"leave\");
    let v = parse(n)?;
    println(\"still here\");
    return Ok(v);
}}
fn main() {{ match ok_path(5) {{ Ok(v) => println(v), Err(e) => println(e) }} }}
"
        ),
        "still here\nleave\n5\n",
    );
}

// 23. Registrations are consumed once. A loop that exits early after several
//     completed iterations must not re-run the earlier iterations' defers along
//     with the one it is leaving.
#[test]
fn lir_defer_scopes_23_defers_are_consumed_once() {
    assert_defers(
        "fn run(limit: i64) -> i64 {
    defer println(-1);
    for i in 0..limit {
        defer println(i);
        defer println(i * 100);
        if i == 2 { return i; }
    }
    return 0;
}
fn main() { println(run(5)); }
",
        "0\n0\n100\n1\n200\n2\n-1\n2\n",
    );
}

// 24. The runnable example compiles entirely from the LIR. `tests/integration/
//     runtime.rs` already pins its output; this pins the emitter that produced
//     it.
#[test]
fn lir_defer_scopes_24_the_example_is_fully_lir() {
    let source = std::fs::read_to_string("example/lir_defer_scopes.wi")
        .expect("example/lir_defer_scopes.wi is part of the repository");
    assert_walker_compiled(
        &source,
        &[
            "doubled",
            "pick",
            "first_big",
            "deep",
            "scan",
            "merged",
            "merged_scope",
            "merged_loop",
            "parse",
            "scaled",
            "main",
        ],
    );
}
