use super::*;

#[test]
fn async_deadlock_reports_unfinished_main() {
    let cases = [
        r#"
async fn worker(c: Channel<i64>) -> i64 { return c.recv(); }
async fn main() {
    let c = Channel<i64>::new();
    let t = worker(c);
    println("waiting");
    println(await t);
    println("UNREACHABLE");
}
"#,
        r#"
async fn main() {
    let c = Channel<i64>::new();
    println(c.recv());
    println("UNREACHABLE");
}
"#,
        r#"
async fn main() -> Result<void, String> {
    let c = Channel<i64>::new();
    println(c.recv());
    println("UNREACHABLE");
    return Result::Ok();
}
"#,
        r#"
async fn main() {
    let c = Channel<i64>::new();
    select { let v = c.recv() => { println(v); } }
    println("UNREACHABLE");
}

"#,
        r#"
async fn relay(x: Channel<i64>, y: Channel<i64>) { y.send(x.recv()); }
async fn main() {
    let x = Channel<i64>::new(); let y = Channel<i64>::new();
    let a = relay(x, y); let b = relay(y, x);
    await a; await b;
    println("UNREACHABLE");
}
"#,
    ];
    for (case, source) in cases.iter().enumerate() {
        let project = TestProject::new("async_deadlock", &[("main.wi", source)]);
        let compiled = project.compile("main.wi");
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        for workers in ["1", "4"] {
            let output = project.run_with_env_timeout(
                &[("WILLOW_WORKERS", workers)],
                std::time::Duration::from_secs(10),
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "case={case}, workers={workers}");
            assert!(stderr.contains("deadlock"), "{stderr}");
            let operation = if case == 0 || case == 4 {
                "blocked on await"
            } else {
                "blocked on channel/select"
            };
            assert!(stderr.contains(operation), "case={case}: {stderr}");
            assert!(
                stderr.contains("main") && stderr.contains("task"),
                "{stderr}"
            );
            assert!(!String::from_utf8_lossy(&output.stdout).contains("UNREACHABLE"));
        }
    }
}

#[test]
fn async_deadlock_preserves_wake_sources_and_detached_exit() {
    let cases = [
        // Empty main and completed/retired main must not look blocked.
        ("async fn main() {}", ""),
        ("async fn main() { await yield(); println(42); }", "42\n"),
        (include_str!("../../../example/async_sleep.wi"), "42\n"),
        // Returning main never waits for detached blocked, sleeping, or
        // runnable tasks to finish. Yield first to let the peer start.
        (
            r#"
async fn blocked(c: Channel<i64>) { c.recv(); }
async fn main() {
    let c = Channel<i64>::new(); let t = blocked(c);
    await yield(); println(42);
}
"#,
            "42\n",
        ),
        (
            r#"
async fn later() { await sleep(60000); println("UNREACHABLE"); }
async fn main() { let t = later(); await yield(); println(42); }
"#,
            "42\n",
        ),
        (
            r#"
async fn spin() { while true { await yield(); } }
async fn main() { let t = spin(); await yield(); println(42); }
"#,
            "42\n",
        ),
        // A timer can make another task runnable, which then wakes main.
        (
            r#"
async fn producer(c: Channel<i64>) { await sleep(10); c.send(42); }
async fn main() {
    let c = Channel<i64>::new(); let t = producer(c);
    println(c.recv()); await t;
}
"#,
            "42\n",
        ),
        (
            r#"
async fn main() {
    let c = Channel<i64>::new();
    select { let v = c.recv() => { println(v); } sleep(10) => { println(42); } }
}
"#,
            "42\n",
        ),
        (
            r#"
async fn main() {
    let c = Channel<i64>::new();
    select { let v = c.recv() => { println(v); } default => { println(42); } }
}
"#,
            "42\n",
        ),
    ];
    for (case, (source, expected)) in cases.iter().enumerate() {
        let project = TestProject::new("async_main_progress", &[("main.wi", source)]);
        let compiled = project.compile("main.wi");
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        for workers in ["1", "4"] {
            let output = project.run_with_env_timeout(
                &[("WILLOW_WORKERS", workers), ("WILLOW_GC_STRESS", "alloc")],
                std::time::Duration::from_secs(10),
            );
            assert!(
                output.status.success(),
                "case={case}, workers={workers}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                *expected,
                "case={case}"
            );
            assert!(!String::from_utf8_lossy(&output.stderr).contains("deadlock"));
        }
    }
}
