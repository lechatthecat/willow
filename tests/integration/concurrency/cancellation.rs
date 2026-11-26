use super::*;

// ── Arithmetic ───────────────────────────────────────────────────────────────

// ── Cooperative task cancellation ───────────────────────────
// 20 perspectives: 1 cancel request visible immediately on an in-flight task, 2 cancel while parked on a
// timer (wakes + finalizes promptly), 3 is_cancelled true after request,
// 4 is_cancelled false for untouched task, 5 cancel is idempotent, 6 cancel
// after completion is a no-op (a repeated await still returns the value),
// 7 await on a cancelled task panics with the task id, 8 the panic aborts,
// 9 other tasks are unaffected, 10 cancelled task's post-await side effects
// never run, 11 fan-out with one cancelled member, 12 cancel in a loop over
// handles, 13 program exits cleanly with a cancelled task never awaited,
// 14 is_cancelled after finalization stays true, 15 parked far-future timer
// cancelled -> program terminates (parked-wake path), 16 cancel + GC
// stress (frame root released for Cancelled), 17 void-returning task cancel,
// 18 is_cancelled on completed-then-cancel-requested stays false-ish (no-op),
// 19 checker rejects cancel with arguments, 20 checker rejects cancel on a
// non-task receiver.

#[test]
fn cancel_01_request_visible_immediately() {
    // An unsent channel prevents completion before cancel even if another
    // worker runs the entire body before main resumes.
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { let gate = Channel<i64>::new(); gate.recv(); return 1; }\nfn main() { let h = t(); h.cancel(); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn cancel_02_while_parked_on_timer() {
    // The virtual clock stays at zero; only cancellation can release this timer.
    let (out, ok) = compile_and_run_with_virtual_time(
        r#"
async fn t(ready: Channel<i64>) -> i64 {
    ready.send(test_current_task());
    await sleep(30);
    return 1;
}
async fn main() {
    test_set_time(0);
    let ready = Channel<i64>::new();
    let h = t(ready);
    let id = ready.recv();
    while !test_task_parked(id) { await yield(); }
    h.cancel();
    println(h.is_cancelled());
    match await h.result() {
        Ok(value) => println("unexpected completion"),
        Err(Cancelled) => {},
    }
}
"#,
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn cancel_03_is_cancelled_after_request() {
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { let gate = Channel<i64>::new(); gate.recv(); return 1; }\nfn main() { let h = t(); h.cancel(); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn cancel_04_untouched_task_not_cancelled() {
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { return 1; }\nasync fn main() { let h = t(); println(h.is_cancelled()); println(await h); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\n1\n");
}

#[test]
fn cancel_05_idempotent() {
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { let gate = Channel<i64>::new(); gate.recv(); return 1; }\nfn main() { let h = t(); h.cancel(); h.cancel(); h.cancel(); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn cancel_06_after_completion_noop() {
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { return 42; }\nasync fn main() { let h = t(); let v = await h; h.cancel(); println(v); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\nfalse\n");
}

#[test]
fn cancel_07_await_cancelled_panics_with_id() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() -> i64 { let gate = Channel<i64>::new(); gate.recv(); return 1; }\nasync fn main() { let h = t(); h.cancel(); println(await h); }",
    );
    assert!(!ok);
    assert!(out.contains("awaited a cancelled task (task"), "{out}");
}

#[test]
fn cancel_08_await_cancelled_aborts() {
    let (_, ok) = compile_and_run_check_exit(
        "async fn t() -> i64 { let gate = Channel<i64>::new(); gate.recv(); return 1; }\nasync fn main() { let h = t(); h.cancel(); await h; }",
    );
    assert!(!ok);
}

#[test]
fn cancel_09_other_tasks_unaffected() {
    let (out, ok) = compile_and_run(
        "async fn t(n: i64) -> i64 { if n == 2 { let gate = Channel<i64>::new(); gate.recv(); } await yield(); return n * 10; }\nasync fn main() { let a = t(1); let b = t(2); let c = t(3); b.cancel(); println(await a); println(await c); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n30\n");
}

#[test]
fn cancel_10_post_await_side_effects_never_run() {
    // Cancellation completes before checking that the gated side effect never ran.
    let (out, ok) = compile_and_run(
        "async fn noisy(ready: AtomicBool) -> i64 { await yield(); ready.store(true); let gate = Channel<i64>::new(); gate.recv(); println(999); return 1; }\nasync fn main() { let ready = AtomicBool::new(false); let h = noisy(ready); while !ready.load() { await yield(); } h.cancel(); await h.result(); println(0); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

#[test]
fn cancel_11_fan_out_with_one_cancelled() {
    let (out, ok) = compile_and_run(
        "async fn t(n: i64) -> i64 { if n == 3 { let gate = Channel<i64>::new(); gate.recv(); } await yield(); return n; }\nasync fn main() { let a = t(1); let b = t(2); let c = t(3); let d = t(4); c.cancel(); println(await a + await b + await d); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn cancel_12_loop_over_handles() {
    let (out, ok) = compile_and_run(
        "async fn t(n: i64) -> i64 { let gate = Channel<i64>::new(); gate.recv(); return n; }\nfn main() { let a = t(1); let b = t(2); a.cancel(); b.cancel(); println(a.is_cancelled()); println(b.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\ntrue\n");
}

#[test]
fn cancel_13_program_exits_with_unawaited_cancelled() {
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { let gate = Channel<i64>::new(); gate.recv(); return 1; }\nfn main() { let h = t(); h.cancel(); println(7); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn cancel_14_stays_cancelled_after_finalization() {
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { let gate = Channel<i64>::new(); gate.recv(); return 1; }\nasync fn main() { let h = t(); h.cancel(); await h.result(); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn cancel_15_long_sleeper_finishes_fast() {
    // The timer is the behavior under test. Observe its parked state before
    // cancelling. Virtual time stays frozen, so completion must come from cancel.
    let (out, ok) = compile_and_run_with_virtual_time(
        r#"
async fn t(ready: Channel<i64>) -> i64 {
    ready.send(test_current_task());
    await sleep(30);
    return 1;
}
async fn main() {
    test_set_time(0);
    let ready = Channel<i64>::new();
    let h = t(ready);
    let id = ready.recv();
    while !test_task_parked(id) { await yield(); }
    h.cancel();
    match await h.result() {
        Ok(value) => println("unexpected completion"),
        Err(Cancelled) => println(h.is_cancelled()),
    }
}
"#,
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn cancel_16_gc_stress() {
    // Exercise immediate cancellation and cancellation after a definite yield.
    // The unsent gate prevents normal completion in either schedule.
    for started in [false, true] {
        let source = format!(
            r#"
async fn t(ready: AtomicBool, gate: Channel<i64>) -> String {{
    await yield();
    ready.store(true);
    gate.recv();
    return "kept";
}}
async fn main() {{
    let gate = Channel<i64>::new();
    let ready = AtomicBool::new(false);
    let h = t(ready, gate);
    if {started} {{ while !ready.load() {{ await yield(); }} }}
    h.cancel();
    await h.result();
    gc_collect();
    println(h.is_cancelled());
}}
"#
        );
        let (out, ok) = compile_and_run_gc_stress(&source);
        assert!(ok, "started={started}: {out}");
        assert_eq!(out, "true\n", "started={started}");
    }
}

#[test]
fn cancel_17_void_task() {
    let (out, ok) = compile_and_run(
        "async fn t() { let gate = Channel<i64>::new(); gate.recv(); }\nfn main() { let h = t(); h.cancel(); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn cancel_18_completed_then_request_reports_false() {
    let (out, ok) = compile_and_run(
        "async fn t() -> i64 { return 1; }\nasync fn main() { let h = t(); await h; h.cancel(); println(h.is_cancelled()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn cancel_19_arguments_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "async fn t() -> i64 { return 1; }\nfn main() { let h = t(); h.cancel(1); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("0 arguments"), "{stderr}");
}

#[test]
fn cancel_20_non_task_receiver_rejected() {
    let (ok, stderr) = compile_with_compiler_env("fn main() { let x = 5; x.cancel(); }", &[]);
    assert!(!ok);
    assert!(!stderr.is_empty());
}
