//! LIR block emission order for synchronous `defer` scopes (willow-fvt4).
//!
//! The synchronous emitter does not rebuild a `defer` scope at every exit the
//! way the cooperative path does. It keeps the open scopes on the Rust side and
//! hands each LIR block the state its predecessor exited with, so a block has to
//! be emitted AFTER something has told it what is open.
//!
//! Walking `f.blocks` by index does not guarantee that. A LIR block index is not
//! a position in the control-flow graph: `if/else` lowers as then -> merge ->
//! else, so a merge block whose only live predecessor is the `else` arm carries a
//! LOWER index than that arm, and `prune_unreachable()` keeps the original index
//! order rather than sorting topologically. Under an index-order walk such a
//! merge opened with an empty scope stack, `flush defers` computed its depth from
//! that stack and unwound nothing, and every `defer` on that path was silently
//! skipped — the wrong output, not a fallback. Where the merge was instead the
//! block that LEAVES an enclosing scope, the same hole could reach the explicit
//! `LIR defer scope underflow` panic.
//!
//! Eligibility never rejected any of this: `lir_sync_defer_stacks_agree` is a
//! proper worklist and correctly concluded the merge had a scope open. The fix is
//! only in the walk — blocks are emitted in an order the edges permit, and a
//! block no edge reaches is emitted last with the empty state. Cranelift blocks
//! are all created before the walk and nothing is sealed inside it, so the order
//! is free.
//!
//! Every test is differential: the same program under the AST emitter and under
//! the walker must print the same thing, with `WILLOW_LIR_REQUIRE=1` on the
//! walker side so a fallback fails instead of quietly comparing the AST emitter
//! against itself, and a third run under `WILLOW_GC_STRESS=alloc` because
//! reordering emission also reorders the GC root bookkeeping.
//!
//! 22 perspectives:
//!   1 the reported shape                    12 `while` + `break` past a merge
//!   2 the mirror image, which index order    13 a defer registered in the arm
//!     already got right                     14 `main`'s own merge
//!   3 both arms exit, no merge survives     15 `match` is unaffected
//!   4 the merge is what leaves a scope      16 three scopes open at the merge
//!   5 two registrations, still LIFO         17 registrations consumed once
//!   6 a merge nested inside an arm          18 async keeps the coop path
//!   7 an else-chain ladder                  19 two merges back to back
//!   8 `?` inside the else arm               20 the walker really compiled these
//!   9 `continue` in the then arm            21 the example's merges are LIR
//!  10 `break` in the then arm               22 a loop nested inside the arm
//!  11 a GC value returned past a merge

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
fn assert_defers(source: &str, expected: &str) {
    for env in [&AST[..], &LIR[..], &LIR_STRESS[..]] {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert!(ok, "program failed under {env:?}: {out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
}

/// Compile once with the selection log on and require the walker to have taken
/// each named function, so a coverage regression cannot pass by printing the
/// right answer from the AST emitter.
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

// 1. The reported shape, and the smallest one there is. The `then` arm returns,
//    so the merge block after the `if` is reached only from the `else` arm —
//    which lowers to a HIGHER block index than the merge itself. Under index
//    order the merge ran with no scope open and printed `else` / `2`.
#[test]
fn lir_defer_block_order_01_a_merge_reached_only_from_the_else_arm() {
    assert_defers(
        "fn f(flag: bool) -> i64 {
    defer println(\"done\");
    if flag { return 1; } else { println(\"else\"); }
    return 2;
}
fn main() { println(f(false)); println(f(true)); }
",
        "else\ndone\n2\ndone\n1\n",
    );
}

// 2. The mirror image: the `else` arm is the one that exits, so the merge's only
//    live predecessor is the `then` arm, which does come first by index. This
//    already worked, and pins that the reordering did not break the easy case.
#[test]
fn lir_defer_block_order_02_a_merge_reached_only_from_the_then_arm() {
    assert_defers(
        "fn f(flag: bool) -> i64 {
    defer println(\"done\");
    if flag { println(\"then\"); } else { return 1; }
    return 2;
}
fn main() { println(f(true)); println(f(false)); }
",
        "then\ndone\n2\ndone\n1\n",
    );
}

// 3. Both arms exit, so nothing reaches the merge and `prune_unreachable()`
//    drops it. No block is left without a state to arrive.
#[test]
fn lir_defer_block_order_03_both_arms_exit_and_the_merge_is_pruned() {
    assert_defers(
        "fn f(flag: bool) -> i64 {
    defer println(\"done\");
    if flag { return 1; } else { return 2; }
}
fn main() { println(f(true)); println(f(false)); }
",
        "done\n1\ndone\n2\n",
    );
}

// 4. The other half of the hole. Here the merge is not just inside a scope, it
//    is the block that LEAVES the loop body's scope by fallthrough, so an empty
//    incoming stack is a `LeaveDeferScope` with nothing to pop.
#[test]
fn lir_defer_block_order_04_the_merge_is_what_leaves_the_scope() {
    assert_defers(
        "fn f(flag: bool) -> i64 {
    defer println(\"outer\");
    for i in 0..1 {
        defer println(\"body\");
        if flag { return 1; } else { println(\"else\"); }
    }
    return 2;
}
fn main() { println(f(false)); println(f(true)); }
",
        "else\nbody\nouter\n2\nbody\nouter\n1\n",
    );
}

// 5. Two registrations in one scope. The merge path has to unwind both, and in
//    reverse order of registration, which only holds if the state that reached
//    it is the predecessor's and not a default.
#[test]
fn lir_defer_block_order_05_two_registrations_still_unwind_lifo() {
    assert_defers(
        "fn f(flag: bool) -> i64 {
    defer println(\"a\");
    defer println(\"b\");
    if flag { return 1; } else { println(\"else\"); }
    return 2;
}
fn main() { println(f(false)); println(f(true)); }
",
        "else\nb\na\n2\nb\na\n1\n",
    );
}

// 6. A merge nested one level in: the inner `if/else` sits inside the outer
//    `then` arm, so its merge is reached only from the inner `else` and then
//    falls through to a `return` that still owes the function's scope.
#[test]
fn lir_defer_block_order_06_a_merge_nested_inside_an_arm() {
    assert_defers(
        "fn f(n: i64) -> i64 {
    defer println(\"fn\");
    if n > 0 {
        if n > 10 { return 10; } else { println(\"small\"); }
        return 1;
    } else {
        return 0;
    }
}
fn main() { println(f(5)); println(f(50)); println(f(-1)); }
",
        "small\nfn\n1\nfn\n10\nfn\n0\n",
    );
}

// 7. An else-chain ladder. Willow has no `else if`, so each rung is a nested
//    `if/else` and the chain produces a merge per level, all of them below their
//    own predecessor by index.
#[test]
fn lir_defer_block_order_07_an_else_chain_ladder() {
    assert_defers(
        "fn f(n: i64) -> i64 {
    defer println(\"fn\");
    if n == 0 {
        return 0;
    } else {
        if n == 1 { return 1; } else { println(\"other\"); }
    }
    return -1;
}
fn main() { println(f(0)); println(f(1)); println(f(7)); }
",
        "fn\n0\nfn\n1\nother\nfn\n-1\n",
    );
}

// 8. A propagating `?` inside the `else` arm. It exits from the arm rather than
//    from the merge, so the merge is still reached — and only from the arm.
#[test]
fn lir_defer_block_order_08_a_try_inside_the_else_arm() {
    assert_defers(
        &format!(
            "{PARSE}
fn f(n: i64) -> Result<i64, String> {{
    defer println(\"fn\");
    if n > 100 {{ return Ok(0); }} else {{ let v = parse(n)?; println(v); }}
    return Ok(1);
}}
fn main() {{
    match f(5) {{ Ok(v) => println(v), Err(e) => println(e) }}
    match f(-5) {{ Ok(v) => println(v), Err(e) => println(e) }}
    match f(500) {{ Ok(v) => println(v), Err(e) => println(e) }}
}}
"
        ),
        "5\nfn\n1\nfn\nbad\nfn\n0\n",
    );
}

// 9. `continue` in the `then` arm. The merge is the rest of the loop body, its
//    only predecessor is the `else` arm, and the iteration's own scope is left
//    there on every pass that does not continue.
#[test]
fn lir_defer_block_order_09_continue_in_the_then_arm() {
    assert_defers(
        "fn f(limit: i64) -> i64 {
    defer println(\"fn\");
    let mut total = 0;
    for i in 0..limit {
        defer println(i);
        if i == 1 { continue; } else { total = total + i; }
    }
    return total;
}
fn main() { println(f(4)); }
",
        "0\n1\n2\n3\nfn\n5\n",
    );
}

// 10. `break` in the `then` arm. Same merge, but the exit leaves the loop
//     entirely, so the two paths out of the body owe different amounts.
#[test]
fn lir_defer_block_order_10_break_in_the_then_arm() {
    assert_defers(
        "fn f(limit: i64) -> i64 {
    defer println(\"fn\");
    let mut total = 0;
    for i in 0..limit {
        defer println(i * 10);
        if i == 2 { break; } else { total = total + i; }
    }
    return total;
}
fn main() { println(f(5)); }
",
        "0\n10\n20\nfn\n1\n",
    );
}

// 11. A heap value built after the merge and returned past the defer. Emission
//     order also orders the GC root bookkeeping, so this runs under allocation
//     stress like the rest.
#[test]
fn lir_defer_block_order_11_a_gc_value_returned_past_a_merge() {
    assert_defers(
        "class Box { pub v: i64; }
fn make(n: i64, flag: bool) -> Box {
    defer println(\"made\");
    if flag { return new Box(1); } else { println(\"else\"); }
    return new Box(n);
}
fn main() { println(make(9, false).v); println(make(9, true).v); }
",
        "else\nmade\n9\nmade\n1\n",
    );
}

// 12. A `while` loop rather than a range `for`. Its latch and body land in a
//     different index order again, and the merge inside the body still has to
//     receive the body's scope.
#[test]
fn lir_defer_block_order_12_a_while_loop_with_a_merge_in_its_body() {
    assert_defers(
        "fn f(limit: i64) -> i64 {
    defer println(\"fn\");
    let mut i = 0;
    while i < limit {
        defer println(i);
        if i == 1 { break; } else { i = i + 1; }
    }
    return i;
}
fn main() { println(f(4)); }
",
        "0\n1\nfn\n1\n",
    );
}

// 13. A `defer` registered inside the `else` arm itself. That scope opens and
//     closes within the arm, so what reaches the merge is the state from BEFORE
//     it — one scope, not two.
#[test]
fn lir_defer_block_order_13_a_defer_registered_in_the_else_arm() {
    assert_defers(
        "fn f(flag: bool) -> i64 {
    defer println(\"fn\");
    if flag { return 1; } else { defer println(\"arm\"); println(\"else\"); }
    return 2;
}
fn main() { println(f(false)); println(f(true)); }
",
        "else\narm\nfn\n2\nfn\n1\n",
    );
}

// 14. `main` is compiled like any other function, including the ordering. Its
//     merge owes the same unwinding on the way to the implicit return.
#[test]
fn lir_defer_block_order_14_main_has_its_own_merge() {
    assert_defers(
        "fn main() {
    defer println(\"bye\");
    let flag = false;
    if flag { return; } else { println(\"else\"); }
    println(\"end\");
}
",
        "else\nend\nbye\n",
    );
}

// 15. A `match` lowers its arms and continuation differently from `if/else`, and
//     was never affected. Pinned so the reordering is visibly a superset.
#[test]
fn lir_defer_block_order_15_a_match_continuation_is_unaffected() {
    assert_defers(
        "fn f(n: i64) -> i64 {
    defer println(\"fn\");
    match n {
        0 => println(\"zero\"),
        _ => println(\"other\"),
    }
    return 1;
}
fn main() { println(f(0)); println(f(3)); }
",
        "zero\nfn\n1\nother\nfn\n1\n",
    );
}

// 16. Three scopes open at the merge — function, loop body and an inner `if` —
//     unwound innermost first on both the exit and the fallthrough path.
#[test]
fn lir_defer_block_order_16_three_scopes_open_at_the_merge() {
    assert_defers(
        "fn f(flag: bool, gate: bool) -> i64 {
    defer println(\"a\");
    for i in 0..1 {
        defer println(\"b\");
        if gate {
            defer println(\"c\");
            if flag { return 1; } else { println(\"else\"); }
            println(\"after\");
        }
    }
    return 2;
}
fn main() { println(f(false, true)); println(f(true, true)); println(f(false, false)); }
",
        "else\nafter\nc\nb\na\n2\nc\nb\na\n1\nb\na\n2\n",
    );
}

// 17. Registrations are consumed once. Re-seeding a merge from a predecessor
//     must not re-run what earlier iterations already ran.
#[test]
fn lir_defer_block_order_17_registrations_are_still_consumed_once() {
    assert_defers(
        "fn f(limit: i64) -> i64 {
    defer println(-1);
    let mut total = 0;
    for i in 0..limit {
        defer println(i);
        defer println(i * 100);
        if i == 2 { return total; } else { total = total + i; }
    }
    return total;
}
fn main() { println(f(5)); }
",
        "0\n0\n100\n1\n200\n2\n-1\n1\n",
    );
}

// 18. Async functions rebuild every scope from the LIR at each exit and never
//     depended on emission order, so they are emitted in plain index order
//     still. Pinned here so the synchronous change cannot reroute them.
#[test]
fn lir_defer_block_order_18_async_keeps_the_cooperative_path() {
    let source = "async fn work(flag: bool) -> i64 {
    defer println(\"leave work\");
    if flag { return 1; } else { println(\"else\"); }
    return 2;
}
async fn main() {
    defer println(\"leave main\");
    println(await work(false));
    println(await work(true));
}
";
    assert_defers(source, "else\nleave work\n2\nleave work\n1\nleave main\n");
    assert_walker_compiled(source, &["work", "main"]);
}

// 19. Two merges back to back. The second one's predecessor is the first one's
//     continuation, so the state has to travel two hops in the right order.
#[test]
fn lir_defer_block_order_19_two_merges_back_to_back() {
    assert_defers(
        "fn f(a: bool, b: bool) -> i64 {
    defer println(\"fn\");
    if a { return 1; } else { println(\"outer else\"); }
    if b { return 2; } else { println(\"inner else\"); }
    return 3;
}
fn main() { println(f(false, false)); println(f(false, true)); println(f(true, true)); }
",
        "outer else\ninner else\nfn\n3\nouter else\nfn\n2\nfn\n1\n",
    );
}

// 20. The walker really took these. Without this a coverage regression would
//     still print the right answer, from the AST emitter.
#[test]
fn lir_defer_block_order_20_the_walker_really_compiled_these() {
    assert_walker_compiled(
        &format!(
            "{PARSE}
fn merged(flag: bool) -> i64 {{
    defer println(\"done\");
    if flag {{ return 1; }} else {{ println(\"else\"); }}
    return 2;
}}
fn scoped(flag: bool) -> i64 {{
    defer println(\"outer\");
    for i in 0..1 {{
        defer println(\"body\");
        if flag {{ return 1; }} else {{ println(\"else\"); }}
    }}
    return 2;
}}
fn tried(n: i64) -> Result<i64, String> {{
    defer println(\"fn\");
    if n > 100 {{ return Ok(0); }} else {{ let v = parse(n)?; println(v); }}
    return Ok(1);
}}
fn main() {{
    defer println(\"bye\");
    println(merged(false));
    println(scoped(false));
    match tried(5) {{ Ok(v) => println(v), Err(e) => println(e) }}
}}
"
        ),
        &["parse", "merged", "scoped", "tried", "main"],
    );
}

// 21. The runnable example carries the shape too. `tests/integration/runtime.rs`
//     pins its output; this pins the emitter that produced it.
#[test]
fn lir_defer_block_order_21_the_examples_merges_are_lir() {
    let source = std::fs::read_to_string("example/lir_defer_scopes.wi")
        .expect("example/lir_defer_scopes.wi is part of the repository");
    assert_walker_compiled(&source, &["merged", "merged_scope", "merged_loop"]);
}

// 22. A whole loop nested inside the `else` arm. The arm's blocks are emitted
//     after the merge that follows the `if`, so the loop's own body, latch and
//     inner merge all reach the walk before the block they eventually flow to.
#[test]
fn lir_defer_block_order_22_a_loop_nested_inside_the_else_arm() {
    assert_defers(
        "fn f(a: bool, limit: i64) -> i64 {
    defer println(\"fn\");
    let mut total = 0;
    if a {
        return -1;
    } else {
        for i in 0..limit {
            defer println(i);
            if i == 1 { continue; } else { total = total + i; }
        }
    }
    return total;
}
fn main() { println(f(false, 4)); println(f(true, 4)); }
",
        "0\n1\n2\n3\nfn\n5\nfn\n-1\n",
    );
}
