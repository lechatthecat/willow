//! Perspectives on live-across-await narrowing (willow-lpn.10).
//!
//! Every test parses a real `async fn`, runs [`analyze_with`] over it, and
//! maps the resulting spans back to binding names, so the assertions read as
//! "which locals end up in the frame".
//!
//! The 42 perspectives below, in order. Most run with
//! [`SuspendModel::EXPLICIT_ONLY`], where only the suspensions the source spells
//! out count: compiler-inserted preemption safepoints would otherwise obscure
//! the source-level distinction some tests are making. The ones that are
//! *about* that model — 32, 33, 34, 38, 39, 40 —
//! run with [`SuspendModel::COOPERATIVE`] instead, and say so in their own
//! comment. A test whose claim depends on the backend's placement must use
//! COOPERATIVE, or it is checking a model the compiler does not emit.
//!
//!  1. no suspension anywhere → nothing framed
//!  2. a local that dies before the only await → not framed
//!  3. a local read after the await → framed
//!  4. a parameter read after the await → framed
//!  5. a parameter unused after the await → not framed
//!  6. a local both declared and read after the await → not framed
//!  7. a reassignment before the await still leaves a live value → framed
//!  8. a whole-variable overwrite after the await KILLS the old value → not framed
//!  9. the loop back edge: `while c { use(x); await; }` → framed (the trap)
//! 10. a loop-local used only before the await in its own iteration → not framed
//! 11. only one branch of an `if` reads it after the await → framed (union)
//! 12. neither branch reads it after the await → not framed
//! 13. the await is inside a branch, the read is after the whole `if` → framed
//! 14. `return` before a later await cuts liveness on that path
//! 15. `break` leaves the loop, so the exit set governs
//! 16. `continue` follows the back edge, not the fall-through
//! 17. nested loops propagate liveness outward
//! 18. shadowing: the inner and outer `x` are separate spans
//! 19. a method-call RECEIVER counts as a use
//! 20. a field store THROUGH a binding is a use, not a kill
//! 21. index-assign uses array, index and value
//! 22. a lambda is opaque: everything in scope is framed
//! 23. `defer` is opaque for the same reason
//! 24. `select` is opaque for the same reason
//! 25. `ch.recv()` suspends without an `await`
//! 26. `ch.send(v)` suspends without an `await`
//! 27. multiple awaits: live at ANY one is enough
//! 28. `let x = await ...;` is force-framed even when x dies immediately
//! 29. `_` is never framed
//! 30. ternary/array/index expression forms register their operand reads
//! 31. `all_binding_spans` (the `WILLOW_ASYNC_FRAME_ALL=1` set) is a superset
//! 32. cooperative: arithmetic stays narrow; a call boundary frames live data
//! 33. cooperative: call-free straight-line code needs no frame locals
//! 34. cooperative: the loop back edge carries its own safepoint
//! 35. `match` arms merge by union, like `if`/`else`
//! 36. a `for` body local dies inside its own iteration
//! 37. the `for` back edge carries the accumulator but kills the induction var
//! 38. cooperative: GC-managed locals are judged on liveness ALONE (no type
//!     special case, in either direction)
//! 39. nested loops converge
//! 40. an empty body is handled
//! 41. index-assignment operands reloaded after an awaited RHS are framed
//! 42. field-assignment receivers reloaded after an awaited RHS are framed

use std::collections::BTreeSet;

use super::*;
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::parser::ast::{FunctionDecl, Item};

/// Parse one program and hand back the function named `f`.
fn parse_fn(src: &str) -> FunctionDecl {
    let tokens = Lexer::new(src).tokenize().expect("lex");
    let (program, diags) = Parser::new(tokens).parse();
    assert!(diags.is_empty(), "parse errors: {diags:?}");
    program
        .items
        .into_iter()
        .find_map(|item| match item {
            Item::Function(f) if f.name == "f" => Some(f),
            _ => None,
        })
        .expect("no fn f")
}

/// Every binding this harness knows how to name, as (name, span). It covers the
/// declaration forms the tests below use; the analysis itself sees more.
fn declarations(f: &FunctionDecl) -> Vec<(String, Span)> {
    let mut out: Vec<(String, Span)> = f.params.iter().map(|p| (p.name.clone(), p.span)).collect();
    walk_block(&f.body, &mut out);
    out
}

fn walk_block(block: &Block, out: &mut Vec<(String, Span)>) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Let(l) => out.push((l.name.clone(), l.span)),
            Stmt::If(s) => {
                walk_block(&s.then_block, out);
                if let Some(e) = &s.else_block {
                    walk_block(e, out);
                }
            }
            Stmt::While(s) => walk_block(&s.body, out),
            Stmt::For(s) => {
                out.push((s.name.clone(), s.name_span));
                walk_block(&s.body, out);
            }
            _ => {}
        }
    }
}

/// Names of the bindings that end up framed, deduped and sorted so assertions
/// do not depend on traversal order.
///
/// Perspectives 1–31 use [`SuspendModel::EXPLICIT_ONLY`]: they are about the
/// dataflow itself, without compiler-inserted call/backedge preemption points
/// (see perspectives 32–40, which pin what the backend's own model produces).
fn framed(src: &str) -> BTreeSet<String> {
    framed_with(src, SuspendModel::EXPLICIT_ONLY)
}

fn framed_with(src: &str, model: SuspendModel) -> BTreeSet<String> {
    let f = parse_fn(src);
    let live = analyze_with(&f.params, &f.body, model);
    declarations(&f)
        .into_iter()
        .filter(|(_, span)| live.contains(span))
        .map(|(name, _)| name)
        .collect()
}

/// Like [`framed`], but disambiguates shadowed names by declaration line.
fn framed_at_lines(src: &str) -> BTreeSet<String> {
    let f = parse_fn(src);
    let live = analyze_with(&f.params, &f.body, SuspendModel::EXPLICIT_ONLY);
    declarations(&f)
        .into_iter()
        .filter(|(_, span)| live.contains(span))
        .map(|(name, span)| format!("{name}@{}", span.line))
        .collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

// 1. Nothing suspends, so nothing has to survive anything.
#[test]
fn liveness_01_no_await_frames_nothing() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let a = n + 1;
    let b = a * 2;
    return b;
}
"#,
    );
    assert_eq!(out, set(&[]));
}

// 2. `a` is consumed before the await and never read again: dead across it.
#[test]
fn liveness_02_local_dead_before_await_is_not_framed() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let a = n + 1;
    println(a);
    await sleep(1);
    return 0;
}
"#,
    );
    assert_eq!(out, set(&[]));
}

// 3. The textbook case: written before, read after.
#[test]
fn liveness_03_local_read_after_await_is_framed() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let a = n + 1;
    await sleep(1);
    return a;
}
"#,
    );
    assert_eq!(out, set(&["a"]));
}

// 4. Parameters are bindings too.
#[test]
fn liveness_04_param_read_after_await_is_framed() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    await sleep(1);
    return n;
}
"#,
    );
    assert_eq!(out, set(&["n"]));
}

// 5. A parameter used only before the suspension does not need a slot.
#[test]
fn liveness_05_param_unused_after_await_is_not_framed() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    println(n);
    await sleep(1);
    return 0;
}
"#,
    );
    assert_eq!(out, set(&[]));
}

// 6. Declared AFTER the suspension, so it never crosses one.
#[test]
fn liveness_06_local_declared_after_await_is_not_framed() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    await sleep(1);
    let a = 5;
    return a;
}
"#,
    );
    assert_eq!(out, set(&[]));
}

// 7. Reassignment is a use AND a def; the value in flight at the await is the
//    reassigned one, and it is read afterwards.
#[test]
fn liveness_07_reassign_before_await_then_read_is_framed() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let mut a = n;
    a = a + 1;
    await sleep(1);
    return a;
}
"#,
    );
    assert_eq!(out, set(&["a"]));
}

// 8. The overwrite after the await KILLS whatever `a` held across it, so
//    nothing has to survive it.
#[test]
fn liveness_08_overwrite_after_await_kills_the_old_value() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let mut a = n;
    await sleep(1);
    a = 7;
    return a;
}
"#,
    );
    assert_eq!(out, set(&[]));
}

// 9. THE TRAP. `x`'s only read is textually BEFORE the await, but the back edge
//    replays it after the await on every iteration except the last, so `x` is
//    live across the suspension. A source-order rule frames nothing here.
#[test]
fn liveness_09_loop_back_edge_makes_a_pre_await_use_live() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let x = n + 1;
    let mut i = 0;
    while i < 3 {
        println(x);
        await sleep(1);
        i = i + 1;
    }
    return 0;
}
"#,
    );
    // `i` too: the condition reads it again on the next iteration.
    assert_eq!(out, set(&["x", "i"]));
}

// 10. A local declared inside the loop and consumed before the await dies at
//     the end of its own iteration, back edge or not.
#[test]
fn liveness_10_loop_local_consumed_before_await_is_not_framed() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let mut i = 0;
    while i < 3 {
        let tmp = i * 2;
        println(tmp);
        i = i + 1;
        await sleep(1);
    }
    return n;
}
"#,
    );
    // `tmp` dies before the await; `i` and `n` cross it.
    assert_eq!(out, set(&["i", "n"]));
}

// 11. Liveness merges by UNION over branches: one reader is enough.
#[test]
fn liveness_11_one_branch_reads_after_await() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let a = n + 1;
    await sleep(1);
    if n > 0 {
        return a;
    } else {
        return 0;
    }
}
"#,
    );
    // `n` is read by the condition, which is after the await, so it crosses it
    // too — the union is over the branch READS, not over the branches.
    assert_eq!(out, set(&["a", "n"]));
}

// 12. Neither arm reads it, so the union is still empty.
#[test]
fn liveness_12_no_branch_reads_after_await() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let a = n + 1;
    println(a);
    await sleep(1);
    if n > 0 {
        return 1;
    } else {
        return 2;
    }
}
"#,
    );
    // `a` is dead across the await; `n` is not, because the condition reads it.
    assert_eq!(out, set(&["n"]));
}

// 13. The await sits inside one arm; the read is after the whole `if`, so it is
//     reachable from the await.
#[test]
fn liveness_13_await_inside_branch_read_after_the_if() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let a = n + 1;
    if n > 0 {
        await sleep(1);
    }
    return a;
}
"#,
    );
    assert_eq!(out, set(&["a"]));
}

// 14. `return` ends the path, so the read on it cannot reach a later await.
#[test]
fn liveness_14_return_cuts_liveness_on_that_path() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let a = n + 1;
    if n > 0 {
        return a;
    }
    await sleep(1);
    return 0;
}
"#,
    );
    assert_eq!(out, set(&[]));
}

// 15. `break` jumps to the loop EXIT, so what is live there is what matters —
//     not what follows the break textually inside the body.
#[test]
fn liveness_15_break_uses_the_loop_exit_live_set() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let a = n + 1;
    let mut i = 0;
    while i < 3 {
        await sleep(1);
        if i > 1 {
            break;
        }
        i = i + 1;
    }
    return a;
}
"#,
    );
    // `a` is read after the loop, so it crosses the await inside it; `i` is
    // read by the condition and by the branch.
    assert_eq!(out, set(&["a", "i"]));
}

// 16. `continue` jumps to the loop HEAD, so it picks up the back-edge set.
#[test]
fn liveness_16_continue_follows_the_back_edge() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let mut i = 0;
    let step = n + 1;
    while i < 3 {
        await sleep(1);
        if i > 1 {
            i = i + step;
            continue;
        }
        i = i + 1;
    }
    return 0;
}
"#,
    );
    // `step` is read after the await via the branch, `i` via both paths.
    assert_eq!(out, set(&["i", "step"]));
}

// 17. An inner loop's back edge has to propagate out through the outer one.
#[test]
fn liveness_17_nested_loops_propagate_liveness_outward() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let outerv = n + 1;
    let mut i = 0;
    while i < 2 {
        let mut j = 0;
        while j < 2 {
            println(outerv);
            await sleep(1);
            j = j + 1;
        }
        i = i + 1;
    }
    return 0;
}
"#,
    );
    assert_eq!(out, set(&["outerv", "i", "j"]));
}

// 18. Two `let x` declarations are two distinct spans (willow-lpn.11), so the
//     inner one being live says nothing about the outer one.
#[test]
fn liveness_18_shadowed_bindings_are_independent() {
    let out = framed_at_lines(
        r#"
async fn f(n: i64) -> i64 {
    let x = n + 1;
    println(x);
    if n > 0 {
        let x = n + 2;
        await sleep(1);
        return x;
    }
    return 0;
}
"#,
    );
    // Line 6 is the inner `let x`; the outer one on line 3 is already dead.
    assert_eq!(out, set(&["x@6"]));
}

// 19. `obj.method()` reads `obj`.
#[test]
fn liveness_19_method_receiver_is_a_use() {
    let out = framed(
        r#"
async fn f(n: i64) -> String {
    let s = n.toString();
    await sleep(1);
    return s + "!";
}
"#,
    );
    assert_eq!(out, set(&["s"]));
}

// 20. `holder.field = v;` READS `holder` — it does not redefine it, so the
//     binding stays live across an earlier await.
#[test]
fn liveness_20_field_store_through_a_binding_is_a_use() {
    let out = framed(
        r#"
class Box { pub v: i64; }
async fn f(n: i64) -> i64 {
    let b = new Box(n);
    await sleep(1);
    b.v = 9;
    return 0;
}
"#,
    );
    assert_eq!(out, set(&["b"]));
}

// 21. All three operands of an index-assign are uses.
#[test]
fn liveness_21_index_assign_uses_array_index_and_value() {
    let out = framed(
        r#"
import std::collections::Array;
async fn f(n: i64) -> i64 {
    let xs: Array<i64> = [1, 2, 3];
    let idx = n;
    let val = n + 1;
    await sleep(1);
    xs[idx] = val;
    return 0;
}
"#,
    );
    assert_eq!(out, set(&["xs", "idx", "val"]));
}

// 22. A lambda body runs later and may capture anything, so the pass refuses to
//     narrow across it: everything in scope stays framed.
#[test]
fn liveness_22_lambda_is_opaque() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let a = n + 1;
    let g = |x: i64| -> i64 { return x + 1; };
    return g(a);
}
"#,
    );
    // No `await` in this body at all, yet the lambda still forces framing.
    // `g` itself is in there because a `let` whose initializer can suspend is
    // force-framed (perspective 28), and an opaque initializer can.
    assert_eq!(out, set(&["n", "a", "g"]));
}

// 23. A deferred body runs at scope exit, after an unknown number of awaits.
#[test]
fn liveness_23_defer_is_opaque() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let a = n + 1;
    defer println(a);
    return 0;
}
"#,
    );
    assert_eq!(out, set(&["n", "a"]));
}

// 24. `select` picks a case through scheduler machinery this pass does not
//     model, so it is a suspension AND opaque.
#[test]
fn liveness_24_select_is_opaque() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let a = n + 1;
    let ch = Channel<i64>::new();
    select {
        let v = ch.recv() => { return v + a; }
        default => { return 0; }
    }
}
"#,
    );
    assert_eq!(out, set(&["n", "a", "ch"]));
}

// 25. `ch.recv()` parks on an empty channel without any `await` keyword.
#[test]
fn liveness_25_channel_recv_suspends_without_await() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let a = n + 1;
    let ch = Channel<i64>::new();
    let got = ch.recv();
    return a + got;
}
"#,
    );
    // `a` crosses the recv, `ch` is read by it, and `got` is bound from a
    // suspending initializer so it is force-framed (perspective 28).
    assert_eq!(out, set(&["a", "ch", "got"]));
}

// 26. `ch.send(v)` parks on a full bounded channel, likewise with no `await`.
#[test]
fn liveness_26_channel_send_suspends_without_await() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let a = n + 1;
    let ch = Channel<i64>::new();
    ch.send(7);
    return a;
}
"#,
    );
    // `a` crosses the send; `ch` is an operand of it, and the resume re-reads
    // the operands, so it crosses it too.
    assert_eq!(out, set(&["a", "ch"]));
}

// 27. Live at the SECOND await only is still framed.
#[test]
fn liveness_27_live_at_any_one_of_several_awaits() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    await sleep(1);
    let a = n + 1;
    await sleep(1);
    return a;
}
"#,
    );
    // `a` crosses the second await; `n` crosses the first, since `let a = n + 1`
    // reads it after that one.
    assert_eq!(out, set(&["a", "n"]));
}

// 28. The awaited result is stored into the binding AFTER the resume, through a
//     frame offset, so the binding must have one even though it is read
//     immediately and never crosses another suspension.
#[test]
fn liveness_28_binding_of_a_suspending_init_is_force_framed() {
    let out = framed(
        r#"
async fn g(x: i64) -> i64 { return x; }
async fn f(n: i64) -> i64 {
    let r = await g(n);
    return r;
}
"#,
    );
    // `n` is an operand of the awaited call, and the resume re-reads the
    // operands, so it is framed alongside the forced `r`.
    assert_eq!(out, set(&["n", "r"]));
}

// 29. `let _ = await ...;` binds nothing readable, but the code generator still
//     stores the awaited result through `async_frame_offsets[&l.span]`, so the
//     wildcard needs a slot too. A wildcard whose initializer cannot suspend
//     needs none.
#[test]
fn liveness_29_wildcard_binding_of_a_suspending_init_is_still_framed() {
    let suspending = parse_fn(
        r#"
async fn f(n: i64) -> i64 {
    let _ = await sleep(n);
    return 0;
}
"#,
    );
    let live = analyze_with(
        &suspending.params,
        &suspending.body,
        SuspendModel::EXPLICIT_ONLY,
    );
    let wildcard = declarations(&suspending)
        .into_iter()
        .find(|(name, _)| name == "_")
        .expect("the wildcard let");
    assert!(live.contains(&wildcard.1));

    let plain = parse_fn(
        r#"
async fn f(n: i64) -> i64 {
    let _ = n + 1;
    await sleep(1);
    return 0;
}
"#,
    );
    let live = analyze_with(&plain.params, &plain.body, SuspendModel::EXPLICIT_ONLY);
    let wildcard = declarations(&plain)
        .into_iter()
        .find(|(name, _)| name == "_")
        .expect("the wildcard let");
    assert!(!live.contains(&wildcard.1));
}

// 30. Ternary arms, array elements and indexing all have to register their
//     operand reads, or the operands would be under-framed.
#[test]
fn liveness_30_expression_forms_register_their_uses() {
    let out = framed(
        r#"
import std::collections::Array;
async fn f(n: i64) -> i64 {
    let t = n + 1;
    let u = n + 2;
    let v = n + 3;
    await sleep(1);
    let picked = t > 0 ? u : v;
    let xs: Array<i64> = [picked];
    return xs[0];
}
"#,
    );
    assert_eq!(out, set(&["t", "u", "v"]));
}

// 31. The `WILLOW_ASYNC_FRAME_ALL=1` set is the old frame-everything behaviour,
//     so it must contain everything the narrowed set does.
#[test]
fn liveness_31_all_binding_spans_is_a_superset() {
    let src = r#"
async fn f(n: i64) -> i64 {
    let a = n + 1;
    let mut i = 0;
    while i < 3 {
        let tmp = i;
        println(tmp);
        await sleep(1);
        i = i + 1;
    }
    return a;
}
"#;
    let f = parse_fn(src);
    let live = analyze_with(&f.params, &f.body, SuspendModel::EXPLICIT_ONLY);
    let all = all_binding_spans(&f.params, &f.body);
    assert!(live.is_subset(&all), "narrowed set escaped the full set");
    // And it really is narrower here: `tmp` dies inside its own iteration.
    assert!(live.len() < all.len());

    // The same must hold for the model the backend actually uses.
    let coop = analyze_with(&f.params, &f.body, SuspendModel::COOPERATIVE);
    assert!(coop.is_subset(&all), "cooperative set escaped the full set");
}

// ── the model the backend actually uses ───────────────────────────────────
//
// Perspectives 1–31 above run with SuspendModel::EXPLICIT_ONLY so that the
// dataflow is visible. The ones below run with SuspendModel::COOPERATIVE, which
// is what `set_coop_live_spans` passes, and pin the consequences of
// call-bearing statements and loop backedges carrying preemption safepoints.

// 32. Call-free arithmetic does not create suspension boundaries. A local live
//     at a later call does cross that call's preemption check; values dead before
//     it and values declared after it do not.
#[test]
fn liveness_32_cooperative_call_safepoint_only_frames_values_live_at_the_call() {
    let src = r#"
async fn f(n: i64) -> i64 {
    let a = n + 1;
    println(a);
    let b = a * 2;
    return b;
}
"#;
    assert_eq!(framed(src), set(&[]));
    assert_eq!(framed_with(src, SuspendModel::COOPERATIVE), set(&["a"]));
}

// 33. A call-free straight-line function has no suspension point at all, so
//     even values read by later arithmetic/return statements stay in SSA.
#[test]
fn liveness_33_cooperative_call_free_straight_line_code_needs_no_frame_locals() {
    let out = framed_with(
        r#"
async fn f(n: i64) -> i64 {
    let unused_a = 1;
    let unused_b = 2;
    return n;
}
"#,
        SuspendModel::COOPERATIVE,
    );
    assert_eq!(out, set(&[]));
}

// 34. The loop back edge carries its own safepoint, so a value that is only
//     read on the NEXT iteration crosses a suspension.
#[test]
fn liveness_34_cooperative_loop_back_edge_is_a_safepoint() {
    let out = framed_with(
        r#"
async fn f(n: i64) -> i64 {
    let mut i = 0;
    let step = n;
    while i < 10 {
        i = i + step;
    }
    return i;
}
"#,
        SuspendModel::COOPERATIVE,
    );
    assert!(
        out.contains("step"),
        "loop-carried read must be framed: {out:?}"
    );
    assert!(out.contains("i"), "loop counter must be framed: {out:?}");
}

// 35. `match` lowers to a `Node::Branch`, so a binding read after the await in
//     ONE arm is framed: arms merge by union exactly like `if`/`else`.
#[test]
fn liveness_35_match_arms_merge_by_union() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let picked = n + 1;
    let ignored = n + 2;
    match n > 0 {
        true => {
            await sleep(1);
            return picked;
        },
        false => {
            await sleep(1);
            return 0;
        },
    }
}
"#,
    );
    assert!(
        out.contains("picked"),
        "read after an await in one arm: {out:?}"
    );
    assert!(
        !out.contains("ignored"),
        "read in no arm after an await: {out:?}"
    );
}

// 36. A `for` body's own local dies inside the iteration that created it, so the
//     back edge does not carry it. The accumulator around the loop does.
#[test]
fn liveness_36_for_body_local_dies_within_its_iteration() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let mut total = n;
    for x in 0..3 {
        let doubled = x * 2;
        total = total + doubled;
        await sleep(1);
    }
    return total;
}
"#,
    );
    assert!(
        !out.contains("doubled"),
        "consumed before the await in its own iteration: {out:?}"
    );
    assert!(
        out.contains("total"),
        "read after the await through the back edge: {out:?}"
    );
}

// 37. The loop trap again, for `for`: `carried` is read TEXTUALLY BEFORE the
//     await, but the back edge makes that read execute after it on every
//     iteration but the first. The induction variable is re-bound at the top of
//     each iteration (a `Node::Def`), which KILLS the previous value, so it is
//     not carried across.
#[test]
fn liveness_37_for_back_edge_carries_the_accumulator_but_not_the_induction_var() {
    let out = framed(
        r#"
async fn f(n: i64) -> i64 {
    let mut carried = n;
    for x in 0..3 {
        carried = carried + x;
        await sleep(1);
    }
    return carried;
}
"#,
    );
    assert!(out.contains("carried"), "loop-carried value: {out:?}");
    assert!(
        !out.contains("x"),
        "rebound at the top of every iteration: {out:?}"
    );
}

// 38. Liveness is the WHOLE frame question — the binding's TYPE never enters
//     into it. This pass used to publish a second, wider `scoped_over_suspend`
//     set so the emitter could keep GC-managed locals framed regardless of
//     liveness; asserting the narrow answer here is what stops that
//     over-approximation coming back.
//
//     Run under COOPERATIVE, the model the backend actually emits, so the claim
//     is checked against reality and not against a relaxed model. That model
//     places preemption points at call-bearing statements and loop backedges.
//     A String that is never read remains unframed, while an `i64` parameter
//     read after an explicit suspension is framed: type buys neither a place in
//     the frame nor an escape from it. willow-p42j makes the first case safe by
//     balancing shadow-stack roots at every poll return and handing the resume
//     trampoline a null-initialized slot.
#[test]
fn liveness_38_gc_managed_locals_are_judged_on_liveness_alone() {
    let read_after_suspension = framed_with(
        r#"
async fn f(n: i64) -> String {
    let kept = "hello";
    await sleep(1);
    return kept;
}
"#,
        SuspendModel::COOPERATIVE,
    );
    assert_eq!(
        read_after_suspension,
        set(&["kept"]),
        "a GC-managed local that is read across a suspension is framed like any other"
    );

    let never_read = framed_with(
        r#"
async fn f(n: i64) -> i64 {
    let unread = "task";
    await sleep(1);
    return n;
}
"#,
        SuspendModel::COOPERATIVE,
    );
    assert_eq!(
        never_read,
        set(&["n"]),
        "a String nobody reads stays out of the frame, while a live i64 goes in"
    );
}

// 39. Deeply nested loops must still terminate: the fixpoint is bounded, and the
//     fallback frames the whole loop rather than under-approximating.
#[test]
fn liveness_39_nested_loops_converge() {
    let out = framed_with(
        r#"
async fn f(n: i64) -> i64 {
    let mut a = n;
    let mut b = 0;
    let mut c = 0;
    while a > 0 {
        while b > 0 {
            while c > 0 {
                c = c - 1;
                await sleep(1);
            }
            b = b - 1;
        }
        a = a - 1;
    }
    return a + b + c;
}
"#,
        SuspendModel::COOPERATIVE,
    );
    assert_eq!(out, set(&["a", "b", "c"]));
}

// 40. An empty body has no bindings and no suspensions: the set is empty and
//     nothing panics on the degenerate case.
#[test]
fn liveness_40_empty_body_is_handled() {
    let f = parse_fn("async fn f() {}\n");
    let live = analyze_with(&f.params, &f.body, SuspendModel::COOPERATIVE);
    assert!(live.is_empty());
}

// 41. Cooperative codegen awaits the RHS first and reloads BOTH `arr` and
//     `index` after resume. They must be live across the source-level await even
//     when compiler-inserted preemption safepoints are disabled.
#[test]
fn liveness_41_index_assignment_rereads_destination_after_await() {
    let out = framed(
        r#"
async fn f(arr: Array<i64>, index: i64, task: Task<i64>) {
    arr[index] = await task;
}
"#,
    );
    assert_eq!(out, set(&["arr", "index", "task"]));
}

// 42. Field assignment follows the same await-then-reload lowering. The
//     receiver therefore belongs to the live-across-await set too.
#[test]
fn liveness_42_field_assignment_rereads_receiver_after_await() {
    let out = framed(
        r#"
async fn f(holder: Holder, task: Task<i64>) {
    holder.value = await task;
}
"#,
    );
    assert_eq!(out, set(&["holder", "task"]));
}

// 43. A match is one expression statement in the outer cooperative emitter.
//     Calls in block arms therefore have to make that OUTER statement
//     call-bearing; the ordinary arm block emitter has no separate preemption
//     pass of its own.
#[test]
fn liveness_43_match_block_arm_call_requests_a_preemption_safepoint() {
    let f = parse_fn(
        r#"
async fn f(value: i64) {
    match value {
        0 => { println(value); },
        _ => {},
    }
}
"#,
    );
    assert!(statement_needs_preempt_safepoint(&f.body.stmts[0]));
}

// 44. Deferred blocks are emitted later by the ordinary block emitter. The
//     registration is conservatively treated as call-bearing so narrowing
//     cannot rely on the old "defer blocks contain no calls" premise.
#[test]
fn liveness_44_defer_block_call_requests_a_preemption_safepoint() {
    let f = parse_fn(
        r#"
async fn f() {
    defer { println("cleanup"); }
}
"#,
    );
    assert!(statement_needs_preempt_safepoint(&f.body.stmts[0]));
}
