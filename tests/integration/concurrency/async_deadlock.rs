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
        // Keep exercising the runtime fallback: a returned channel is outside
        // the closed local region checked by the compiler's E2607 analysis.
        let source = format!(
            "fn dynamic_inbox() -> Channel<i64> {{ return Channel<i64>::new(); }}\n{}",
            source.replace("Channel<i64>::new()", "dynamic_inbox()")
        );
        let project = TestProject::new("async_deadlock", &[("main.wi", &source)]);
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
fn permanent_wait_compile_witnesses() {
    let worker = "async fn waiter(ch: Channel<Task<void>>) { let t = ch.recv(); await t; }";
    let cases = [
        "fn main() { let ch = Channel<i64>::new(); println(ch.recv()); }".to_string(),
        format!(
            "{worker} async fn main() {{ let c = Channel<Task<void>>::new(); let t = waiter(c); c.send(t); await t; }}"
        ),
        format!(
            "{worker} async fn main() {{ let a = Channel<Task<void>>::new(); let b = Channel<Task<void>>::new(); let x = waiter(a); let y = waiter(b); a.send(y); b.send(x); await x; }}"
        ),
    ];
    for source in cases {
        let project = TestProject::new("permanent_wait", &[("main.wi", &source)]);
        // Repeat to exercise both fresh and cached type/body inputs.
        for _ in 0..2 {
            let output = project.compile("main.wi");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "{source}");
            assert!(
                stderr.contains("E2607") && stderr.contains("witness"),
                "{stderr}"
            );
            assert!(
                !stderr.contains("E2402") && !stderr.contains("E2403"),
                "{stderr}"
            );
        }
    }
}

#[test]
fn permanent_wait_valid_example() {
    let project = TestProject::new(
        "permanent_wait_escapes",
        &[(
            "main.wi",
            include_str!("../../../example/permanent_wait_escapes.wi"),
        )],
    );
    let output = project.compile("main.wi");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for workers in ["1", "4"] {
        let output = project.run_with_env_timeout(
            &[("WILLOW_WORKERS", workers)],
            std::time::Duration::from_secs(10),
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "42\ntrue\ntimeout\ndefault\ntask timeout\n"
        );
    }
}

#[test]
fn permanent_wait_imported_body_is_unknown() {
    let project = TestProject::new(
        "permanent_wait_import",
        &[
            ("main.wi", "import worker; fn main() { worker::block(); }"),
            (
                "worker.wi",
                "pub fn block() { let ch = Channel<i64>::new(); ch.recv(); }",
            ),
        ],
    );
    let output = project.compile("main.wi");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Do not execute: acceptance means unknown, not proven safe.
}

#[test]
fn permanent_wait_imported_main_is_not_entry() {
    let project = TestProject::new(
        "permanent_wait_imported_main",
        &[
            (
                "main.wi",
                "import worker; async fn wrapper() { worker::main(); } async fn main() { let t = wrapper(); select { let v = await t => {} sleep(1) => {} } }",
            ),
            (
                "worker.wi",
                "pub fn main() { let ch = Channel<i64>::new(); ch.recv(); }",
            ),
        ],
    );
    let output = project.compile("main.wi");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
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
