use super::*;

// ── Top-level panic policy (panic and cancellation policy, spec §18) ────────────────────────
// Willow has no recover: the FINAL policy is abort-the-program. A panic
// anywhere — any task, any worker thread, any depth — prints the located
// `runtime panic:` message, the call stack, and the async chain (task id +
// spawn site), then aborts. The Panicked task state stays RESERVED for a
// possible future recoverable policy; nothing sets it today.
// 10 perspectives: 1 panic in an awaited task aborts with message, 2 exit is
// abnormal (abort), 3 panic in main task, 4 panic in a deep await chain
// reports the full chain, 5 panic with format args interpolates, 6 panic in
// a fire-and-forget task that is never awaited still aborts the program when
// it runs, 7 panic inside a loop inside async, 8 panic in a class method
// called from a task, 9 the message appears exactly once (no double
// report from scheduler re-entry), 10 panic in one of several workers'
// tasks aborts even while siblings are mid-sleep.

#[test]
fn ppol_01_awaited_task_aborts_with_message() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() -> i64 { await yield(); panic(\"boom\"); }\nasync fn main() { let h = t(); println(await h); }",
    );
    assert!(!ok);
    assert!(out.contains("runtime panic: boom"), "{out}");
}

#[test]
fn ppol_02_abnormal_exit() {
    let (_, ok) = compile_and_run_check_exit(
        "async fn t() { await yield(); panic(\"x\"); }\nasync fn main() { let h = t(); await h; println(1); }",
    );
    assert!(!ok);
}

#[test]
fn ppol_03_panic_in_main_task() {
    let (out, ok) =
        compile_and_run_check_exit("async fn main() { await yield(); panic(\"main-boom\"); }");
    assert!(!ok);
    assert!(out.contains("runtime panic: main-boom"), "{out}");
}

#[test]
fn ppol_04_deep_chain_reported() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn a() -> i64 { await yield(); panic(\"deep\"); }\nasync fn b() -> i64 { return await a(); }\nasync fn c() -> i64 { return await b(); }\nasync fn main() { println(await c()); }",
    );
    assert!(!ok);
    assert!(out.contains("async a"), "{out}");
    assert!(out.contains("async b"), "{out}");
    assert!(out.contains("async c"), "{out}");
}

#[test]
fn ppol_05_format_args() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t(n: i64) { await yield(); panic(\"worker {} died\", n); }\nasync fn main() { let h = t(42); await h; }",
    );
    assert!(!ok);
    assert!(out.contains("worker 42 died"), "{out}");
}

#[test]
fn ppol_06_unawaited_task_panic_aborts() {
    // Main parks on an unreleased channel without awaiting the panicking task.
    let (out, ok) = compile_and_run_with_test_sync(
        "async fn t(main_task: i64) { while !test_task_parked(main_task) { await yield(); } panic(\"orphan\"); }\nasync fn main() { let gate = Channel<i64>::new(); let h = t(test_current_task()); gate.recv(); println(9); }",
        &[("WILLOW_WORKERS", "5")],
    );
    assert!(!ok);
    assert!(out.contains("orphan"), "{out}");
}

#[test]
fn ppol_07_panic_inside_async_loop() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() { for i in 0..10 { await yield(); if i == 3 { panic(\"at {}\", i); } } }\nasync fn main() { let h = t(); await h; }",
    );
    assert!(!ok);
    assert!(out.contains("at 3"), "{out}");
}

#[test]
fn ppol_08_panic_in_method_from_task() {
    let (out, ok) = compile_and_run_check_exit(
        "class W { pub fn go(self) -> i64 { panic(\"method-boom\"); } }\nasync fn t() -> i64 { await yield(); let w = new W(); return w.go(); }\nasync fn main() { let h = t(); println(await h); }",
    );
    assert!(!ok);
    assert!(out.contains("method-boom"), "{out}");
}

#[test]
fn ppol_09_message_exactly_once() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() { await yield(); panic(\"once\"); }\nasync fn main() { let h = t(); await h; }",
    );
    assert!(!ok);
    assert_eq!(out.matches("runtime panic: once").count(), 1, "{out}");
}

#[test]
fn ppol_10_panic_while_siblings_sleep() {
    let (out, ok) = compile_and_run_with_virtual_time(
        "async fn sleeper(ready: Channel<i64>) { ready.send(test_current_task()); await sleep(30); }\nasync fn bad() { await yield(); panic(\"among-workers\"); }\nasync fn main() { test_set_time(0); let ready = Channel<i64>::new(); let a = sleeper(ready); let b = sleeper(ready); let first = ready.recv(); let second = ready.recv(); while !test_task_parked(first) || !test_task_parked(second) { await yield(); } let c = bad(); await c; }",
        &[("WILLOW_WORKERS", "5")],
    );
    assert!(!ok);
    assert!(out.contains("among-workers"), "{out}");
}
