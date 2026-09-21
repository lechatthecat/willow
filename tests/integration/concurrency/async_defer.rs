use super::*;

// ── Async lexical defer + cancellation cleanup ──────────────────────────────
// Async defers belong to their lexical block. Registration sets a frame FLAG
// and stashes operands into GC-masked frame slots; each scope exit flushes LIFO
// after consuming its flags; cancellation runs the compiler-generated
// __coop_cancel entry on a worker WITHOUT the scheduler lock (Cancelling
// state), executing only still-flagged sites in reverse lexical order.
// 20 perspectives: 1 normal return flushes LIFO, 2 fallthrough (void) exit
// flushes, 3 return-position call-await flushes, 4 cancel runs pending
// defers, 5 cancel runs ONLY sites registered before the suspension point,
// 6 cancel before any registration runs nothing, 7 defers do not run twice
// when cancelled after completion, 8 operand stashed before await survives
// suspension into the cancel path, 9 receiver stash for method defer,
// 10 async METHOD defer, 11 string operand + GC stress across cancel,
// 12 print form, 13 two flagged sites run reverse-lexically on cancel,
// 14 await after cancel still panics AFTER cleanup ran, 15 cancel of a
// sleep-parked task runs defers promptly (10s sleeper), 16 defer in async
// loop body registers and flushes once per lexical iteration, 17 await inside
// defer rejected,
// 18 async callee inside defer rejected (async context too), 19 args
// evaluated at registration in async, 20 unawaited cancelled task's defers
// still run before program exit, 21 if-arm scope exits before its parent,
// 22 match-arm scope exits before its parent, 23 continue flushes the current
// iteration, 24 break flushes the current iteration but not the function,
// 25 cancellation unwinds all and only active nested scopes, 26 a completed
// inner scope is not rerun by later cancellation, 27 a GC-managed block-body
// capture survives allocation stress, 28 a repeated site is consumed before
// its next execution and remains cancellable, 29 `?` unwinds nested scopes,
// 30 nested return unwinds inner before outer, 31 a return expression is
// fixed before a mutating defer runs.

#[test]
fn adfr_01_normal_return_lifo() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nasync fn w() -> i64 { defer c(1); defer c(2); await sleep(1); return 7; }\nasync fn main() { println(await w()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n1\n7\n");
}

#[test]
fn adfr_02_fallthrough_flushes() {
    let (out, ok) = compile_and_run(
        "fn c() { println(3); }\nasync fn w() { defer c(); await sleep(1); println(1); }\nasync fn main() { await w(); println(9); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n3\n9\n");
}

#[test]
fn adfr_03_return_position_await_flushes() {
    let (out, ok) = compile_and_run(
        "fn c() { println(5); }\nasync fn inner() -> i64 { await sleep(1); return 4; }\nasync fn w() -> i64 { defer c(); return await inner(); }\nasync fn main() { println(await w()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n4\n");
}

#[test]
fn adfr_04_cancel_runs_pending() {
    let (out, ok) = compile_and_run(
        "fn c() { println(42); }\nasync fn w(ready: AtomicBool) { defer c(); ready.store(true); await sleep(5000); }\nasync fn main() { let ready = AtomicBool::new(false); let h = w(ready); while !ready.load() { await yield(); } h.cancel(); await h.result(); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\ntrue\n");
}

#[test]
fn adfr_05_cancel_only_registered_sites() {
    // Site 2 sits AFTER the suspension the cancel interrupts: never registered.
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nasync fn w(ready: AtomicBool) { defer c(10); ready.store(true); await sleep(5000); defer c(20); await sleep(1); }\nasync fn main() { let ready = AtomicBool::new(false); let h = w(ready); while !ready.load() { await yield(); } h.cancel(); await h.result(); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n0\n");
}

#[test]
fn adfr_06_cancel_before_registration() {
    // Cancelled while gated before the defer: no site registered, none run.
    let (out, ok) = compile_and_run(
        "fn c() { println(99); }\nasync fn w(ready: AtomicBool, proceed: AtomicBool) { ready.store(true); while !proceed.load() { await yield(); } defer c(); }\nasync fn main() { let ready = AtomicBool::new(false); let proceed = AtomicBool::new(false); let h = w(ready, proceed); while !ready.load() { await yield(); } h.cancel(); await h.result(); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

#[test]
fn adfr_07_no_double_run_after_completion() {
    let (out, ok) = compile_and_run(
        "fn c() { println(8); }\nasync fn w() -> i64 { defer c(); await sleep(1); return 1; }\nasync fn main() { let h = w(); println(await h); h.cancel(); await h.result(); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n1\n0\n");
}

#[test]
fn adfr_08_operand_survives_suspension() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nasync fn w(x: i64, ready: AtomicBool) { defer c(x * 7); ready.store(true); await sleep(5000); }\nasync fn main() { let ready = AtomicBool::new(false); let h = w(6, ready); while !ready.load() { await yield(); } h.cancel(); await h.result(); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n0\n");
}

#[test]
fn adfr_09_method_receiver_stash() {
    let (out, ok) = compile_and_run(
        "class R { pub v: i64; pub fn show(self) { println(self.v); } }\nasync fn w(ready: AtomicBool) { let r = new R(5); defer r.show(); ready.store(true); await sleep(5000); }\nasync fn main() { let ready = AtomicBool::new(false); let h = w(ready); while !ready.load() { await yield(); } h.cancel(); await h.result(); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n0\n");
}

#[test]
fn adfr_10_async_method_defer() {
    let (out, ok) = compile_and_run(
        "fn c() { println(4); }\nclass W { pub async fn go(self) -> i64 { defer c(); await sleep(1); return 6; } }\nasync fn main() { let w = new W(); println(await w.go()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n6\n");
}

#[test]
fn adfr_11_string_operand_gc_stress_cancel() {
    let (out, ok) = compile_and_run_gc_stress(
        "fn c(s: String) { println(s); }\nasync fn w(ready: AtomicBool) { let name = \"a\" + \"b\"; defer c(name + \"!\"); ready.store(true); await sleep(5000); }\nasync fn main() { let ready = AtomicBool::new(false); let h = w(ready); while !ready.load() { await yield(); } h.cancel(); await h.result(); println(\"end\"); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "ab!\nend\n");
}

#[test]
fn adfr_12_print_form() {
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { let x = 3; defer println(x); await sleep(1); return 1; }\nasync fn main() { println(await w()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n1\n");
}

#[test]
fn adfr_13_cancel_two_sites_reverse() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nasync fn w(ready: AtomicBool) { defer c(1); defer c(2); ready.store(true); await sleep(5000); }\nasync fn main() { let ready = AtomicBool::new(false); let h = w(ready); while !ready.load() { await yield(); } h.cancel(); await h.result(); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n1\n0\n");
}

#[test]
fn adfr_14_await_after_cancel_panics_after_cleanup() {
    let (out, ok) = compile_and_run_check_exit(
        "fn c() { println(7); }\nasync fn w(ready: AtomicBool) -> i64 { defer c(); ready.store(true); await sleep(5000); return 1; }\nasync fn main() { let ready = AtomicBool::new(false); let h = w(ready); while !ready.load() { await yield(); } h.cancel(); println(await h); }",
    );
    assert!(!ok);
    assert!(out.contains('7'), "{out}");
    assert!(out.contains("cancelled task"), "{out}");
}

#[test]
fn adfr_15_long_sleeper_cleanup_prompt() {
    let (out, ok) = compile_and_run(
        "fn c() { println(1); }\nasync fn w(ready: AtomicBool) { defer c(); ready.store(true); await sleep(10000); }\nasync fn main() { let ready = AtomicBool::new(false); let h = w(ready); while !ready.load() { await yield(); } h.cancel(); await h.result(); println(2); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n");
}

#[test]
fn adfr_16_loop_defer_flushes_each_lexical_iteration() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nasync fn w() { for i in 0..3 { defer c(i); await sleep(1); } println(8); }\nasync fn main() { await w(); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n2\n8\n");
}

#[test]
fn adfr_17_await_inside_defer_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "async fn g() -> i64 { return 1; }\nfn c(n: i64) {}\nasync fn w() { defer c(await g()); }\nasync fn main() { await w(); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("E0905"), "{stderr}");
}

#[test]
fn adfr_18_async_callee_rejected_in_async_too() {
    let (ok, stderr) = compile_with_compiler_env(
        "async fn cleanup() {}\nasync fn w() { defer cleanup(); }\nasync fn main() { await w(); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("async call"), "{stderr}");
}

#[test]
fn adfr_19_args_registration_time_async() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nasync fn w() -> i64 { let mut x = 1; defer c(x); x = 50; await sleep(1); return x; }\nasync fn main() { println(await w()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n50\n");
}

#[test]
fn adfr_20_unawaited_cancelled_defers_run() {
    let (out, ok) = compile_and_run(
        "fn c(done: AtomicBool) { println(3); done.store(true); }\nasync fn w(ready: AtomicBool, done: AtomicBool) { defer c(done); ready.store(true); await sleep(5000); }\nasync fn main() { let ready = AtomicBool::new(false); let done = AtomicBool::new(false); let h = w(ready, done); while !ready.load() { await yield(); } h.cancel(); while !done.load() { await yield(); } println(4); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n4\n");
}

#[test]
fn adfr_21_if_arm_is_a_lexical_scope() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nasync fn w() { defer c(9); if true { defer c(1); println(0); } println(2); }\nasync fn main() { await w(); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n2\n9\n");
}

#[test]
fn adfr_22_match_arm_is_a_lexical_scope() {
    let (out, ok) = compile_and_run(
        "async fn w() { defer println(9); match 1 { 1 => { defer println(1); println(0); }, _ => { } } println(2); }\nasync fn main() { await w(); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n2\n9\n");
}

#[test]
fn adfr_23_continue_flushes_current_iteration() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nasync fn w() { for i in 0..3 { defer c(i); if i == 1 { continue; } println(7); } }\nasync fn main() { await w(); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n0\n1\n7\n2\n");
}

#[test]
fn adfr_24_break_flushes_iteration_before_function_scope() {
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nasync fn w() { defer c(9); for i in 0..3 { defer c(i); if i == 1 { break; } } println(8); }\nasync fn main() { await w(); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n8\n9\n");
}

#[test]
fn adfr_25_cancel_unwinds_active_nested_scopes() {
    // Synchronize on registered defers and terminal cleanup, not wall-clock sleeps.
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nasync fn w(ready: AtomicBool) { defer c(1); if true { defer c(2); ready.store(true); await sleep(5000); } }\nasync fn main() { let ready = AtomicBool::new(false); let task = w(ready); while !ready.load() { await yield(); } task.cancel(); await task.result(); println(task.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n1\ntrue\n");
}

#[test]
fn adfr_26_cancel_does_not_rerun_completed_inner_scope() {
    // Synchronize on registered defers and terminal cleanup, not wall-clock sleeps.
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nasync fn w(ready: AtomicBool) { defer c(1); if true { defer c(2); } ready.store(true); await sleep(5000); }\nasync fn main() { let ready = AtomicBool::new(false); let task = w(ready); while !ready.load() { await yield(); } task.cancel(); await task.result(); println(task.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n1\ntrue\n");
}

#[test]
fn adfr_27_block_capture_survives_gc_and_preemption_stress() {
    let (out, ok) = compile_and_run_with_env(
        "async fn w() { if true { let label = \"lex\" + \"ical\"; defer { println(label); } println(0); } await sleep(0); println(1); }\nasync fn main() { await w(); }",
        &[("WILLOW_GC_STRESS", "alloc"), ("WILLOW_TASK_BUDGET", "1")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\nlexical\n1\n");
}

#[test]
fn adfr_28_repeated_site_is_consumed_then_cancellable() {
    // Synchronize on registered defers and terminal cleanup, not wall-clock sleeps.
    let (out, ok) = compile_and_run(
        "fn c(n: i64) { println(n); }\nasync fn w(ready: AtomicBool) { for i in 0..3 { defer c(i); if i == 1 { ready.store(true); await sleep(5000); } } }\nasync fn main() { let ready = AtomicBool::new(false); let task = w(ready); while !ready.load() { await yield(); } task.cancel(); await task.result(); println(task.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\ntrue\n");
}

#[test]
fn adfr_29_try_unwinds_nested_scopes() {
    let (out, ok) = compile_and_run(
        "fn bad() -> Result<i64, String> { return Err(\"bad\"); }\nasync fn w() -> Result<i64, String> { defer println(9); if true { defer println(1); let value = bad()?; return Ok(value); } return Ok(0); }\nasync fn main() { match await w() { Ok(value) => println(value), Err(error) => println(error), } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n9\nbad\n");
}

#[test]
fn adfr_30_return_unwinds_nested_scopes() {
    let (out, ok) = compile_and_run(
        "async fn w() -> i64 { defer println(9); if true { defer println(1); return 7; } return 0; }\nasync fn main() { println(await w()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n9\n7\n");
}

#[test]
fn adfr_31_nested_await_return_is_fixed_before_defer() {
    let (out, ok) = compile_and_run(
        "async fn one() -> i64 { return 1; }\nasync fn w() -> i64 { let mut x = 1; defer { x = 9; println(x); } return (await one()) + x; }\nasync fn main() { println(await w()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n2\n");
}
