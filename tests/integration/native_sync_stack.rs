use super::support::*;
use std::time::Duration;

#[test]
fn native_sync_stack_recursive_helpers_return_exact_results() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
fn fib(n: i64) -> i64 {
    if n <= 1 { return n; }
    return fib(n - 1) + fib(n - 2);
}
async fn main() { println(fib(15)); }
"#,
        &[("WILLOW_TASK_BUDGET", "3")],
        Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "610\n");
}

#[test]
fn native_sync_stack_more_busy_helpers_than_workers_do_not_starve_timers() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
fn busy() { while true {} }
async fn worker() { busy(); }
async fn main() {
    let a = worker(); let b = worker(); let c = worker();
    let d = worker(); let e = worker(); let f = worker();
    await sleep(20);
    println(42);
    a.cancel(); b.cancel(); c.cancel(); d.cancel(); e.cancel(); f.cancel();
    await sleep(20);
}
"#,
        &[("WILLOW_WORKERS", "5"), ("WILLOW_TASK_BUDGET", "3")],
        Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn native_sync_stack_cancellation_runs_sync_and_async_defers_once() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
fn helper() {
    defer { println(1); }
    while true {}
}
async fn worker() { defer { println(2); } helper(); }
async fn main() {
    let task = worker();
    await sleep(20);
    task.cancel();
    await sleep(30);
    println(3);
}
"#,
        &[("WILLOW_TASK_BUDGET", "3")],
        Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n");
}

#[test]
fn native_sync_stack_recursive_string_roots_survive_gc_stress() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
fn recurse(n: i64) -> i64 {
    let text = "alive" + "!";
    if n == 0 { if text == "alive!" { return 6; } return 0; }
    let result = recurse(n - 1);
    if text == "alive!" { return result + 6; }
    return 0;
}
async fn main() { println(recurse(50)); }
"#,
        &[
            ("WILLOW_TASK_BUDGET", "3"),
            ("WILLOW_GC_STRESS", "alloc,scheduler"),
        ],
        Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "306\n");
}

#[test]
fn native_sync_stack_panic_in_cancellation_defer_is_fatal() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
fn helper() { defer { panic("cancel-cleanup-panic"); } while true {} }
async fn worker() { helper(); }
async fn main() {
    let task = worker(); await sleep(20); task.cancel(); await sleep(30);
}
"#,
        &[("WILLOW_TASK_BUDGET", "3")],
        Duration::from_secs(20),
    );
    assert!(!ok);
    assert!(!timed_out, "{out}");
    assert!(out.contains("cancel-cleanup-panic"), "{out}");
}

#[test]
fn native_sync_stack_recover_in_defer_does_not_revoke_cancellation() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
fn cleanup() {
    defer match recover() {
        Some(info) => println(info.message),
        None => println("missing panic")
    }
    panic("recovered-cleanup");
}
fn helper() { defer { cleanup(); } while true {} }
async fn worker() { helper(); println("must not resume"); }
async fn main() {
    let task = worker(); await sleep(20); task.cancel(); await sleep(30); println(9);
}
"#,
        &[("WILLOW_TASK_BUDGET", "3")],
        Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "recovered-cleanup\n9\n");
}

#[test]
fn native_sync_stack_imported_typed_method_with_loop_is_supported() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "worker.wi",
                r#"
pub class Work {
    pub init(self) {}
    pub fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n { i = i + 1; }
        return i;
    }
}
"#,
            ),
            (
                "main.wi",
                r#"
import worker::Work;
async fn run(work: Work) -> i64 { return work.heavy(100); }
async fn main() { let work = new Work(); println(await run(work)); }
"#,
            ),
        ],
        "main.wi",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "100\n");
}

#[test]
fn native_sync_stack_cancellation_inside_normal_defer_preserves_outer_cleanup() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
fn lengthy_cleanup() {
    println("cleanup-start");
    let mut i = 0;
    while i < 1000000000 { i = i + 1; }
    println("must not finish cleanup");
}
fn normal_return() { defer { lengthy_cleanup(); } }
async fn worker() {
    defer match recover() {
        Some(info) => println(info.message),
        None => println("no panic")
    }
    normal_return();
    println("must not resume");
}
async fn main() {
    let task = worker(); await sleep(20); task.cancel(); await sleep(30); println(9);
}
"#,
        &[("WILLOW_TASK_BUDGET", "1")],
        Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "cleanup-start\nno panic\n9\n");
}

#[test]
fn prepared_method_frame_survives_await_and_ternary_argument_panic() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
class Sink { pub init(self) {} pub fn consume(self, value: i64) -> i64 { return value; } }
async fn number() -> i64 { await yield(); return 1; }
fn explode() -> i64 { panic("prepared-argument"); return 0; }
async fn main() {
    let sink = new Sink();
    println(sink.consume((await number()) == 1 ? explode() : 0));
}
"#,
        &[("WILLOW_TASK_BUDGET", "1")],
        Duration::from_secs(20),
    );
    assert!(!ok, "{out}");
    assert!(!timed_out, "{out}");
    assert!(out.contains("runtime panic: prepared-argument"), "{out}");
    assert!(out.contains(": explode"), "{out}");
    assert!(out.contains(": consume"), "{out}");
}

#[test]
fn prepared_method_frame_does_not_leak_from_suspended_task() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
class Sink { pub init(self) {} pub fn waiting_method(self, value: i64) -> i64 { return value; } }
async fn slow() -> i64 { await sleep(10000); return 1; }
async fn parked() { let sink = new Sink(); println(sink.waiting_method(await slow())); }
fn unrelated_failure() { panic("separate-task"); }
async fn main() { let task = parked(); await sleep(20); unrelated_failure(); }
"#,
        &[("WILLOW_WORKERS", "1"), ("WILLOW_TASK_BUDGET", "1")],
        Duration::from_secs(20),
    );
    assert!(!ok, "{out}");
    assert!(!timed_out, "{out}");
    assert!(out.contains("runtime panic: separate-task"), "{out}");
    assert!(out.contains(": unrelated_failure"), "{out}");
    assert!(!out.contains(": waiting_method"), "{out}");
}

#[test]
fn native_sync_stack_scalar_recursion_remains_cancellable() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
fn fib(n: i64) -> i64 {
    if n < 2 { return n; }
    return fib(n - 1) + fib(n - 2);
}
fn run(ready: Channel<i64>) {
    defer { println("sync cleanup"); }
    ready.send(1);
    println(fib(40));
}
async fn worker(ready: Channel<i64>) { defer { println("async cleanup"); } run(ready); }
async fn main() {
    let ready = Channel<i64>::new();
    let task = worker(ready);
    ready.recv();
    task.cancel();
    await task.result();
    println("done");
}
"#,
        &[("WILLOW_WORKERS", "1"), ("WILLOW_TASK_BUDGET", "3")],
        Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "sync cleanup\nasync cleanup\ndone\n");
}
