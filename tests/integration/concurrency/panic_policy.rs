use super::*;

// ── Top-level panic policy (willow-0a6k.7, spec §18) ────────────────────────
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
        "async fn t() -> i64 { await sleep(1); panic(\"boom\"); }\nasync fn main() { let h = t(); println(await h); }",
    );
    assert!(!ok);
    assert!(out.contains("runtime panic: boom"), "{out}");
}

#[test]
fn ppol_02_abnormal_exit() {
    let (_, ok) = compile_and_run_check_exit(
        "async fn t() { await sleep(1); panic(\"x\"); }\nasync fn main() { let h = t(); await h; println(1); }",
    );
    assert!(!ok);
}

#[test]
fn ppol_03_panic_in_main_task() {
    let (out, ok) =
        compile_and_run_check_exit("async fn main() { await sleep(1); panic(\"main-boom\"); }");
    assert!(!ok);
    assert!(out.contains("runtime panic: main-boom"), "{out}");
}

#[test]
fn ppol_04_deep_chain_reported() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn a() -> i64 { await sleep(1); panic(\"deep\"); }\nasync fn b() -> i64 { return await a(); }\nasync fn c() -> i64 { return await b(); }\nasync fn main() { println(await c()); }",
    );
    assert!(!ok);
    assert!(out.contains("async a"), "{out}");
    assert!(out.contains("async b"), "{out}");
    assert!(out.contains("async c"), "{out}");
}

#[test]
fn ppol_05_format_args() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t(n: i64) { await sleep(1); panic(\"worker {} died\", n); }\nasync fn main() { let h = t(42); await h; }",
    );
    assert!(!ok);
    assert!(out.contains("worker 42 died"), "{out}");
}

#[test]
fn ppol_06_unawaited_task_panic_aborts() {
    // Fire-and-forget panicking task: main sleeps long enough for it to run.
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() { await sleep(1); panic(\"orphan\"); }\nasync fn main() { let h = t(); await sleep(200); println(9); }",
    );
    assert!(!ok);
    assert!(out.contains("orphan"), "{out}");
}

#[test]
fn ppol_07_panic_inside_async_loop() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() { for i in 0..10 { await sleep(1); if i == 3 { panic(\"at {}\", i); } } }\nasync fn main() { let h = t(); await h; }",
    );
    assert!(!ok);
    assert!(out.contains("at 3"), "{out}");
}

#[test]
fn ppol_08_panic_in_method_from_task() {
    let (out, ok) = compile_and_run_check_exit(
        "class W { pub fn go(self) -> i64 { panic(\"method-boom\"); } }\nasync fn t() -> i64 { await sleep(1); let w = new W(); return w.go(); }\nasync fn main() { let h = t(); println(await h); }",
    );
    assert!(!ok);
    assert!(out.contains("method-boom"), "{out}");
}

#[test]
fn ppol_09_message_exactly_once() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn t() { await sleep(1); panic(\"once\"); }\nasync fn main() { let h = t(); await h; }",
    );
    assert!(!ok);
    assert_eq!(out.matches("runtime panic: once").count(), 1, "{out}");
}

#[test]
fn ppol_10_panic_while_siblings_sleep() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn sleeper() { await sleep(5000); }\nasync fn bad() { await sleep(5); panic(\"among-workers\"); }\nasync fn main() { let a = sleeper(); let b = sleeper(); let c = bad(); await c; }",
    );
    assert!(!ok);
    assert!(out.contains("among-workers"), "{out}");
}
