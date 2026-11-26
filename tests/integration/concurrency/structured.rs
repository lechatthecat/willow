use crate::support::*;

// ── CancellationToken + TaskScope ────────────────────────────
// Token attachment and scope addition are identity adapters over an existing
// eager Task. Waiting remains `await task` / `await task.result()`; scope exit
// is the explicit `await scope.finish()` operation.

#[test]
fn structured_01_token_cancels_multiple_tasks_and_runs_defers() {
    let (out, ok) = compile_and_run_with_env(
        r#"
async fn worker(done: AtomicI64, ready: AtomicI64) {
    defer done.add(1);
    await yield();
    ready.add(1);
    let gate = Channel<i64>::new();
    gate.recv();
}

async fn main() {
    let done = AtomicI64::new(0);
    let ready = AtomicI64::new(0);
    let token = CancellationToken::new();
    let first = token.attach(worker(done, ready));
    let second = token.attach(worker(done, ready));
    while ready.load() < 2 { await yield(); }
    token.cancel();
    match await first.result() { Ok(value) => println("bad"), Err(Cancelled) => println("first"), }
    match await second.result() { Ok(value) => println("bad"), Err(Cancelled) => println("second"), }
    println(done.load());
}
"#,
        &[("WILLOW_WORKERS", "5")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "first\nsecond\n2\n");
}

#[test]
fn structured_02_late_attachment_to_cancelled_token_is_cancelled() {
    let (out, ok) = compile_and_run(
        r#"
async fn slow() -> i64 { let gate = Channel<i64>::new(); gate.recv(); return 1; }
async fn main() {
    let token = CancellationToken::new();
    token.cancel();
    let task = token.attach(slow());
    match await task.result() { Ok(value) => println(value), Err(Cancelled) => println("cancelled"), }
    println(token.is_cancelled());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "cancelled\ntrue\n");
}

#[test]
fn structured_03_parent_token_cancels_child_token_participants() {
    let (out, ok) = compile_and_run(
        r#"
async fn slow() { let gate = Channel<i64>::new(); gate.recv(); }
async fn main() {
    let parent = CancellationToken::new();
    let child = parent.child();
    let task = child.attach(slow());
    parent.cancel();
    match await task.result() { Ok(value) => println("bad"), Err(Cancelled) => println("child"), }
    println(child.is_cancelled());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "child\ntrue\n");
}

#[test]
fn structured_04_child_token_cancellation_does_not_propagate_upward() {
    let (out, ok) = compile_and_run(
        r#"
async fn value() -> i64 { await yield(); return 7; }
async fn main() {
    let parent = CancellationToken::new();
    let child = parent.child();
    let parent_task = parent.attach(value());
    child.cancel();
    println(await parent_task);
    println(parent.is_cancelled());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\nfalse\n");
}

#[test]
fn structured_05_scope_finish_waits_for_all_children() {
    let (out, ok) = compile_and_run_with_test_sync(
        r#"
async fn add(total: AtomicI64, value: i64, gate: Channel<i64>) {
    gate.recv();
    total.add(value);
}
async fn main() {
    let total = AtomicI64::new(0);
    let gate = Channel<i64>::new();
    let scope = TaskScope::new();
    let first = scope.add(add(total, 10, gate));
    let second = scope.add(add(total, 20, gate));
    test_observe_next_task();
    let finishing = scope.finish();
    let id = test_observed_task();
    if id <= 0 { panic("scope finish did not spawn its task"); }
    while !test_task_parked(id) { await yield(); }
    gate.send(1);
    gate.send(1);
    match await finishing { Ok(value) => println(total.load()), Err(Cancelled) => println(-1), }
    await first;
    await second;
}
"#,
        &[("WILLOW_WORKERS", "5")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "30\n");
}

#[test]
fn structured_06_scope_cancel_finish_returns_cancelled() {
    let (out, ok) = compile_and_run(
        r#"
async fn slow(ready: AtomicBool) { await yield(); ready.store(true); let gate = Channel<i64>::new(); gate.recv(); }
async fn main() {
    let ready = AtomicBool::new(false);
    let scope = TaskScope::new();
    let task = scope.add(slow(ready));
    while !ready.load() { await yield(); }
    scope.cancel();
    match await scope.finish() { Ok(value) => println("bad"), Err(Cancelled) => println("cancelled"), }
    println(task.is_cancelled());
    println(scope.is_cancelled());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "cancelled\ntrue\ntrue\n");
}

#[test]
fn structured_07_parent_scope_finish_includes_nested_scope() {
    let (out, ok) = compile_and_run_with_test_sync(
        r#"
async fn set(value: AtomicI64, gate: Channel<i64>) { gate.recv(); value.add(1); }
async fn main() {
    let value = AtomicI64::new(0);
    let gate = Channel<i64>::new();
    let parent = TaskScope::new();
    let child = parent.child();
    parent.add(set(value, gate));
    child.add(set(value, gate));
    test_observe_next_task();
    let finishing = parent.finish();
    let id = test_observed_task();
    if id <= 0 { panic("scope finish did not spawn its task"); }
    while !test_task_parked(id) { await yield(); }
    gate.send(1);
    gate.send(1);
    match await finishing { Ok(done) => println(value.load()), Err(Cancelled) => println(-1), }
}
"#,
        &[("WILLOW_WORKERS", "5")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

#[test]
fn structured_08_finish_closes_scope_to_late_tasks() {
    let (out, ok) = compile_and_run(
        r#"
async fn slow() -> i64 { let gate = Channel<i64>::new(); gate.recv(); return 1; }
async fn main() {
    let scope = TaskScope::new();
    let finishing = scope.finish();
    let late = scope.add(slow());
    match await finishing { Ok(value) => println("finished"), Err(Cancelled) => println("bad"), }
    match await late.result() { Ok(value) => println(value), Err(Cancelled) => println("late-cancelled"), }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "finished\nlate-cancelled\n");
}

#[test]
fn structured_09_scope_cancel_cleans_channel_waiter() {
    let (out, ok) = compile_and_run_with_test_sync(
        r#"
async fn receive(channel: Channel<i64>) -> i64 { return channel.recv(); }
async fn main() {
    let channel = Channel<i64>::new();
    let scope = TaskScope::new();
    test_observe_next_task();
    scope.add(receive(channel));
    let id = test_observed_task();
    if id <= 0 { panic("channel receiver did not spawn its task"); }
    while !test_task_parked(id) { await yield(); }
    scope.cancel();
    match await scope.finish() { Ok(value) => println("bad"), Err(Cancelled) => println("cancelled"), }
    channel.send(42);
    println(channel.recv());
}
"#,
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "cancelled\n42\n");
}

#[test]
fn structured_10_token_cancel_cleans_netpoll_waiter() {
    let (out, ok) = compile_and_run_with_test_sync(
        r#"
import std::net;
async fn main() {
    match net::bind("127.0.0.1:0") {
        Ok(listener) => {
            let token = CancellationToken::new();
            test_observe_next_task();
            let accepting = token.attach(net::accept_async(listener));
            let id = test_observed_task();
            if id <= 0 { panic("accept did not spawn its task"); }
            while !test_task_parked(id) { await yield(); }
            token.cancel();
            match await accepting.result() { Ok(stream) => println("bad"), Err(Cancelled) => println("cancelled"), }
        }
        Err(error) => println("bind failed"),
    }
}
"#,
        &[("WILLOW_WORKERS", "5")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "cancelled\n");
}

#[test]
fn structured_11_token_cancel_cleans_blocking_pool_task() {
    let (out, ok) = compile_and_run(
        r#"
import std::fs;
async fn main() {
    let path = fs::temp_path("willow_scope_blocking");
    fs::write_string(path, "payload");
    let token = CancellationToken::new();
    let reading = token.attach(fs::read_to_string_async(path));
    token.cancel();
    match await reading.result() { Ok(value) => println("completed"), Err(Cancelled) => println("cancelled"), }
    fs::remove_file(path);
}
"#,
    );
    assert!(ok, "{out}");
    assert!(
        out == "cancelled\n" || out == "completed\n",
        "completion racing cancellation must stay safe: {out}"
    );
}

#[test]
fn structured_12_nested_scope_gc_stress_multi_worker() {
    let (out, ok) = compile_and_run_with_env(
        r#"
async fn text(value: String) -> String { await yield(); return value + "!"; }
async fn main() {
    let parent = TaskScope::new();
    let child = parent.child();
    let first = parent.add(text("a" + "b"));
    let second = child.add(text("c" + "d"));
    match await parent.finish() { Ok(value) => println("done"), Err(Cancelled) => println("bad"), }
    println(await first);
    println(await second);
}
"#,
        &[
            ("WILLOW_WORKERS", "5"),
            ("WILLOW_GC_STRESS", "alloc"),
            ("WILLOW_TASK_BUDGET", "1"),
        ],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "done\nab!\ncd!\n");
}

#[test]
fn structured_13_non_task_attachment_is_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "fn main() { let token = CancellationToken::new(); token.attach(1); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("expects `Task<T>`"), "{stderr}");
}

#[test]
fn structured_14_concurrent_finish_calls_report_one_cancelled_outcome() {
    let (out, ok) = compile_and_run_with_env(
        r#"
async fn slow() { let gate = Channel<i64>::new(); gate.recv(); }
async fn main() {
    let scope = TaskScope::new();
    scope.add(slow());
    let first = scope.finish();
    let second = scope.finish();
    scope.cancel();
    match await first { Ok(value) => println("bad"), Err(Cancelled) => println("first"), }
    match await second { Ok(value) => println("bad"), Err(Cancelled) => println("second"), }
}
"#,
        &[("WILLOW_WORKERS", "5")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "first\nsecond\n");
}

#[test]
fn structured_15_scope_traces_unbound_completed_task_frames() {
    let (out, ok) = compile_and_run_with_test_sync(
        r#"
async fn text(ready: Channel<i64>) -> String {
    ready.send(test_current_task());
    await yield();
    return "kept" + "!";
}
async fn main() {
    let scope = TaskScope::new();
    let ready = Channel<i64>::new();
    scope.add(text(ready));
    let id = ready.recv();
    while !test_task_finished(id) { await yield(); }
    gc_collect();
    match await scope.finish() { Ok(value) => println("kept"), Err(Cancelled) => println("bad"), }
}
"#,
        &[("WILLOW_GC_STRESS", "alloc"), ("WILLOW_WORKERS", "5")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "kept\n");
}
