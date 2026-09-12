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

#[test]
fn native_sync_stack_boundary_value_shapes() {
    // Distinct boundary transports: scalar, float, string, closure, indirect
    // function, class receiver, interface receiver, reference, sequential calls,
    // and a call after a cooperative suspension.
    for (source, expected) in [
        (
            "fn plus(x: i64) -> i64 { return x + 1; } async fn main() { println(plus(41)); }",
            "42\n",
        ),
        (
            "fn plus(x: f64) -> f64 { return x + 0.5; } async fn main() { println(plus(1.5)); }",
            "2\n",
        ),
        (
            "fn suffix(x: String) -> String { return x + \"!\"; } async fn main() { println(suffix(\"hello\")); }",
            "hello!\n",
        ),
        (
            "fn capture(base: i64) -> i64 { let add = |x: i64| base + x; return add(2); } async fn main() { println(capture(40)); }",
            "42\n",
        ),
        (
            "fn add(x: i64) -> i64 { return x + 1; } fn invoke() -> i64 { let callback: fn(i64) -> i64 = add; return callback(41); } async fn main() { println(invoke()); }",
            "42\n",
        ),
        (
            "class Box { pub value: i64; pub fn get(self) -> i64 { return self.value; } } async fn main() { let b = new Box(42); println(b.get()); }",
            "42\n",
        ),
        (
            "interface Value { fn get(self) -> i64; } class Box implements Value { pub value: i64; pub fn get(self) -> i64 { return self.value; } } fn invoke() -> i64 { let b: Value = new Box(42); return b.get(); } async fn main() { println(invoke()); }",
            "42\n",
        ),
        (
            "fn update(x: &mut i64) { x = x + 1; } async fn main() { let mut x = 41; update(&x); println(x); }",
            "42\n",
        ),
        (
            "fn add(x: i64) -> i64 { return x + 1; } async fn main() { let a = add(40); println(add(a)); }",
            "42\n",
        ),
        (
            "fn add(x: i64) -> i64 { return x + 1; } async fn main() { let x = 41; await sleep(1); println(add(x)); }",
            "42\n",
        ),
    ] {
        let (out, ok) = compile_and_run_with_runtime_env(
            source,
            &[("WILLOW_TASK_BUDGET", "1"), ("WILLOW_GC_STRESS", "alloc")],
            Duration::from_secs(20),
        );
        assert!(ok, "{source}: {out}");
        assert_eq!(out, expected, "{source}");
    }
}

#[test]
fn native_sync_stack_async_normal_defer_is_cancellable() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
fn busy() { while true {} }
async fn worker() {
    defer { println("outer cleanup"); }
    defer { println("cleanup start"); busy(); println("must not resume"); }
}
async fn main() {
    let task = worker(); await sleep(20); task.cancel(); await sleep(30); println(9);
}
"#,
        &[("WILLOW_TASK_BUDGET", "1")],
        Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "cleanup start\nouter cleanup\n9\n");
}

#[test]
fn native_sync_stack_async_defer_captures_and_recovery() {
    for (source, expected) in [
        (
            r#"
fn add(value: i64) -> i64 { return value + 1; }
async fn main() {
    let mut value = 40;
    defer { println(value); }
    defer { value = add(value); }
    await yield();
}
"#,
            "41\n",
        ),
        (
            r#"
fn explode() { panic("cleanup failure"); }
async fn main() {
    defer match recover() { Some(info) => println(info.message), None => println("missing") }
    defer { explode(); }
}
"#,
            "cleanup failure\n",
        ),
        (
            r#"
fn add(value: i64) -> i64 { return value + 1; }
async fn main() {
    defer { defer { println(add(40)); } println(add(41)); }
}
"#,
            "42\n41\n",
        ),
    ] {
        let (out, ok) = compile_and_run_with_runtime_env(
            source,
            &[("WILLOW_TASK_BUDGET", "1")],
            Duration::from_secs(20),
        );
        assert!(ok, "{out}");
        assert_eq!(out, expected);
    }
}

#[test]
fn native_sync_stack_panic_survives_suspended_cleanup() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
fn explode() -> i64 {
    defer { let mut n = 0; while n < 100 { n = n + 1; } }
    panic("suspended panic"); return 7;
}
async fn main() {
    defer match recover() { Some(info) => println(info.message), None => println("missing") }
    println(explode());
    println("must not resume");
}
"#,
        &[("WILLOW_TASK_BUDGET", "1")],
        Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "suspended panic\n");
}

#[test]
fn native_sync_stack_option_callback_is_cancellable() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
fn busy(n: i64) -> i64 { while true {} return n; }
async fn worker() { let value: Option<i64> = Some(1); value.map(busy); }
async fn main() {
    let task = worker(); await sleep(20); task.cancel(); await sleep(30); println(42);
}
"#,
        &[("WILLOW_TASK_BUDGET", "1")],
        Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn native_sync_stack_error_conversion_is_cancellable() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
class Converted { pub code: i64; }
class Original implements Into<Converted> {
    pub code: i64;
    pub fn into(self) -> Converted { while true {} return new Converted(self.code); }
}
async fn worker() -> Result<i64, Converted> {
    let value: Result<i64, Original> = Err(new Original(7));
    let number = value?;
    return Ok(number);
}
async fn main() {
    let task = worker(); await sleep(20); task.cancel(); await sleep(30); println(42);
}
"#,
        &[("WILLOW_TASK_BUDGET", "1")],
        Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn native_sync_stack_async_cleanup_loop_is_cancellable() {
    let (out, ok) = compile_and_run_with_runtime_env(
        r#"
async fn worker() {
    defer { println("outer cleanup"); }
    defer { println("cleanup start"); while true {} }
}
async fn main() {
    let task = worker(); await sleep(20); task.cancel(); await sleep(30); println(42);
}
"#,
        &[("WILLOW_TASK_BUDGET", "1")],
        Duration::from_secs(20),
    );
    assert!(ok, "{out}");
    assert_eq!(out, "cleanup start\nouter cleanup\n42\n");
}
