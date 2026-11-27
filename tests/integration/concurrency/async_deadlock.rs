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
        r#"
async fn main() {
    let c = Channel<i64>::with_capacity(1);
    c.send(1); c.send(2);
    println("UNREACHABLE");
}
"#,
        r#"
async fn main() {
    let c = Channel<i64>::new();
    c.recv_opt();
    println("UNREACHABLE");
}
"#,
        r#"
async fn receiver(c: Channel<i64>) { c.recv(); }
async fn middle(c: Channel<i64>) { let t = receiver(c); await t; }
async fn main() {
    let c = Channel<i64>::new(); let t = middle(c); await t;
    println("UNREACHABLE");
}
"#,
        r#"
async fn main() {
    let a = Channel<i64>::new(); let b = Channel<i64>::new();
    select { let x = a.recv() => {} let y = b.recv() => {} }
    println("UNREACHABLE");
}
"#,
        r#"
async fn receiver(c: Channel<i64>) { c.recv(); }
async fn main() {
    let a = Channel<i64>::new(); let b = Channel<i64>::new();
    let t = receiver(a);
    select { let x = await t => {} let y = b.recv() => {} }
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
            let operation = if matches!(case, 0 | 4 | 7 | 9) {
                "blocked on await"
            } else {
                "blocked on channel/select"
            };
            assert!(stderr.contains(operation), "case={case}: {stderr}");
            assert_eq!(output.status.code(), Some(1), "{stderr}");
            assert!(stderr.contains("witness (registered waits"), "{stderr}");
            assert!(stderr.contains("-> channel 0x"), "case={case}: {stderr}");
            if matches!(case, 0 | 4 | 7 | 9) {
                assert!(stderr.contains("await -> task "), "{stderr}");
            }
            if case == 5 {
                assert!(stderr.contains("SendWait"), "{stderr}");
            }
            if case == 7 {
                assert_eq!(stderr.matches("await -> task ").count(), 2, "{stderr}");
                assert_eq!(stderr.matches("\n  task ").count(), 3, "{stderr}");
            }
            if case == 8 {
                assert_eq!(stderr.matches("RecvWait -> channel").count(), 2, "{stderr}");
            }
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
            include_str!("../../../tests/regressions/permanent_wait_escapes.wi"),
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
        // runnable tasks to finish. Observe the actual peer state before returning.
        (
            r#"
async fn blocked(c: Channel<i64>, ready: Channel<i64>) { ready.send(test_current_task()); c.recv(); }
async fn main() {
    let c = Channel<i64>::new(); let ready = Channel<i64>::new(); let t = blocked(c, ready);
    let id = ready.recv(); while !test_task_parked(id) { await yield(); } println(42);
}
"#,
            "42\n",
        ),
        (
            r#"
async fn later(ready: Channel<i64>) { ready.send(test_current_task()); await sleep(30); println("UNREACHABLE"); }
async fn main() { test_set_time(0); let ready = Channel<i64>::new(); let t = later(ready); let id = ready.recv(); while !test_task_parked(id) { await yield(); } println(42); }
"#,
            "42\n",
        ),
        (
            r#"
async fn spin(started: AtomicBool) { started.store(true); while true { await yield(); } }
async fn main() { let started = AtomicBool::new(false); let t = spin(started); while !started.load() { await yield(); } println(42); }
"#,
            "42\n",
        ),
        // A timer can make another task runnable, which then wakes main.
        (
            r#"
async fn producer(c: Channel<i64>, caller: i64) { while !test_task_parked(caller) { await yield(); } await sleep(10); c.send(42); }
async fn main() {
    let c = Channel<i64>::new(); let t = producer(c, test_current_task());
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
        if matches!(case, 3 | 4 | 6) {
            for workers in ["1", "4"] {
                let run = if case == 4 {
                    compile_and_run_with_virtual_time_output
                } else {
                    compile_and_run_with_test_sync_output
                };
                let (out, stderr, ok) = run(
                    source,
                    &[("WILLOW_WORKERS", workers), ("WILLOW_GC_STRESS", "alloc")],
                );
                assert!(ok, "case={case}, workers={workers}: {stderr}");
                assert_eq!(out, *expected, "case={case}");
                assert!(!stderr.contains("deadlock"), "{stderr}");
            }
            continue;
        }
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

#[test]
fn async_deadlock_valid_protocol_example() {
    let project = TestProject::new(
        "channel_wait_protocol",
        &[(
            "main.wi",
            include_str!("../../../example/channel_wait_protocol.wi"),
        )],
    );
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert!(!stderr.contains("deadlock"), "{stderr}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "42\ntrue\n");
    }
}
