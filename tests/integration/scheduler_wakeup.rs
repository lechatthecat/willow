//! Scheduler wake-up and blocking-`select` behaviour (willow-atth).
//!
//! A claim pops a task out of every run queue before it registers as an active
//! poll. A worker idling in that window used to observe an empty queue, zero
//! active polls, no timer and no netpoll waiter, conclude that the run was
//! globally idle, and stop the pool — stranding a runnable task. Two symptoms
//! were seen under full-workspace load: an `await` (and the top-level drive of
//! `main`) returning with its target still pending, so the program exited 0
//! with partial output; and a `select` with no `default` falling through to the
//! merge block, running no case at all.
//!
//! Perspectives 21-36 continue the numbering of the runtime-side unit
//! perspectives in `crates/willow_runtime/src/scheduler.rs` (01-20). They cover
//! the blocking-select contract, the deadlock diagnostic that replaced the
//! fall-through, the unchanged `default`/timeout behaviour, the sync channel
//! diagnostics that share the same "a drive completed nothing" signal, and
//! repeat-runs that would surface the stranding race.

use super::support::*;
use std::time::Duration;

const REPEATS: usize = 20;
const RUN_TIMEOUT: Duration = Duration::from_secs(60);

/// Compile once, then run the binary `runs` times. The stranding race is
/// load- and timing-dependent, so a perspective that guards it must re-run the
/// same program instead of re-compiling it.
fn compile_and_run_repeatedly(source: &str, runs: usize, env: &[(&str, &str)]) -> Vec<String> {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_sched_wake_{id}.wi"));
    let bin_path = temp_path(format!("willow_sched_wake_{id}"));
    fs::write(&src_path, source).expect("write scheduler fixture");

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let compiled = Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("compile scheduler fixture");
    assert!(
        compiled.status.success(),
        "compile failed: {}{}",
        String::from_utf8_lossy(&compiled.stdout),
        String::from_utf8_lossy(&compiled.stderr)
    );

    let mut outputs = Vec::with_capacity(runs);
    for run in 0..runs {
        let output = Command::new(&bin_path)
            .envs(env.iter().map(|(k, v)| (*k, *v)))
            .output()
            .expect("run scheduler fixture");
        assert!(
            output.status.success(),
            "run {run} failed ({}): {}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        outputs.push(String::from_utf8_lossy(&output.stdout).into_owned());
    }

    let _ = fs::remove_file(&src_path);
    remove_output_artifacts(&bin_path);
    outputs
}

fn assert_every_run(outputs: &[String], expected: &str) {
    for (run, out) in outputs.iter().enumerate() {
        assert_eq!(out, expected, "run {run} produced different output");
    }
}

/// Perspective 21. A synchronous `select` with no `default` must BLOCK until a
/// case is ready. Its producer only sends after a timer, so the first drive
/// completes nothing — the old lowering fell through here and printed nothing.
#[test]
fn sched_wake_21_sync_select_without_default_blocks_for_a_producer() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1);
    ch.send(99);
    return 0;
}
fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    select {
        let v = ch.recv() => { println(v); }
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "99\nafter\n");
}

/// Perspective 22. When nothing can ever make a case ready, a select with no
/// `default` would block forever. That is a diagnosable program error, so the
/// runtime raises it instead of silently skipping the select.
#[test]
fn sched_wake_22_select_without_default_reports_a_real_deadlock() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    println("before");
    select {
        let v = ch.recv() => { println(v); }
    }
    println("after");
}
"#,
    );
    assert!(!ok, "a blocking-forever select must not exit successfully");
    assert!(
        out.contains("select would block forever"),
        "missing deadlock diagnostic: {out}"
    );
    assert!(out.contains("before"), "the program ran up to the select");
    assert!(
        !out.contains("after"),
        "execution must not continue past a select that took no case: {out}"
    );
}

/// Perspective 23. The deadlock is a language panic like any other runtime
/// fault, so a deferred `recover()` can observe it with its message.
#[test]
fn sched_wake_23_select_deadlock_is_recoverable() {
    let (out, ok) = compile_and_run(
        r#"
fn blocked() {
    let ch = Channel<i64>::new();
    select {
        let v = ch.recv() => { println(v); }
    }
}
fn main() {
    if true {
        defer match recover() {
            Some(info) => println("recovered:" + info.message),
            None => println("none")
        }
        blocked();
    }
    println("after");
}
"#,
    );
    assert!(ok, "{out}");
    assert!(
        out.contains("recovered:select would block forever"),
        "recover must see the select diagnostic: {out}"
    );
    assert!(out.ends_with("after\n"), "execution resumes after recovery");
}

/// Perspective 24. The deadlock diagnostic carries the select's own source
/// location in debug builds, not `:0:0`.
#[test]
fn sched_wake_24_deadlock_diagnostic_reports_the_select_line() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    select {
        let v = ch.recv() => { println(v); }
    }
}
"#,
    );
    assert!(!ok, "{out}");
    assert!(
        out.contains(":4:5"),
        "the diagnostic must point at the select statement: {out}"
    );
}

/// Perspective 25. A `default` case still short-circuits: nothing ready means
/// the default runs, and the idle wait is never reached.
#[test]
fn sched_wake_25_default_case_still_wins_when_nothing_is_ready() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    select {
        let v = ch.recv() => { println(v); }
        default => { println(-1); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-1\n");
}

/// Perspective 26. A timeout case still bounds the wait; that lowering owns a
/// deadline-bounded drive and must not be routed through the blocking idle
/// wait.
#[test]
fn sched_wake_26_timeout_case_still_fires() {
    let (out, ok) = compile_and_run(
        r#"
async fn main() {
    let ch = Channel<i64>::new();
    select {
        let v = ch.recv() => { println(v); }
        sleep(5) => { println("late"); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "late\n");
}

/// Perspective 27. A ready case is still taken immediately: the blocking path
/// must not add a wait to a select that can proceed at once.
#[test]
fn sched_wake_27_ready_case_is_taken_without_waiting() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    ch.send(42);
    select {
        let v = ch.recv() => { println(v); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

/// Perspective 28. The selected case binds the value that was actually sent.
/// A fall-through would have skipped the body with the binding unwritten, so
/// the value pins that the case really ran.
#[test]
fn sched_wake_28_blocking_select_binds_the_produced_value() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1);
    ch.send(7);
    return 0;
}
fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    select {
        let v = ch.recv() => { println(v * 6); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

/// Perspective 29. A blocking select inside a loop keeps working: every
/// iteration re-probes, waits, and takes its own value.
#[test]
fn sched_wake_29_blocking_select_in_a_loop_drains_a_producer() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    let mut i = 0;
    while i < 5 {
        await sleep(1);
        ch.send(i);
        i = i + 1;
    }
    return 0;
}
fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    let mut seen = 0;
    while seen < 5 {
        select {
            let v = ch.recv() => { println(v); }
        }
        seen = seen + 1;
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n2\n3\n4\n");
}

/// Perspective 30. A synchronous `recv` whose producer is still sleeping must
/// keep helping the scheduler. "This drive completed nothing" is not proof that
/// the value will never arrive.
#[test]
fn sched_wake_30_sync_recv_waits_for_a_sleeping_producer() {
    let (out, ok) = compile_and_run(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(5);
    ch.send(11);
    return 0;
}
fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    println(ch.recv());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "11\n");
}

/// Perspective 31. The genuine blocking diagnostic survives the change: a recv
/// with no possible producer still aborts with its own message rather than
/// hanging or inventing a default value.
#[test]
fn sched_wake_31_sync_recv_without_a_producer_still_reports_blocking() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() {
    let ch = Channel<i64>::new();
    println(ch.recv());
}
"#,
    );
    assert!(!ok, "{out}");
    assert!(
        out.contains("recv on empty open channel would block"),
        "missing recv diagnostic: {out}"
    );
}

/// Perspective 32. Same for the send side of a full bounded channel with no
/// consumer.
#[test]
fn sched_wake_32_sync_send_on_a_full_channel_still_reports_blocking() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() {
    let ch = Channel<i64>::with_capacity(1);
    ch.send(1);
    ch.send(2);
    println("unreachable");
}
"#,
    );
    assert!(!ok, "{out}");
    assert!(
        out.contains("send on full bounded channel would block"),
        "missing send diagnostic: {out}"
    );
}

/// Perspective 33. The top-level drive of `async fn main` must not return while
/// main is still pending. Repeated runs of the exact program that failed under
/// full-workspace load: every run must print the complete output.
#[test]
fn sched_wake_33_async_main_select_is_stable_across_runs() {
    let outputs = compile_and_run_repeatedly(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1);
    ch.send(99);
    return 0;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    select {
        let v = ch.recv() => { println(v); }
    }
    await p;
}
"#,
        REPEATS,
        &[],
    );
    assert_every_run(&outputs, "99\n");
}

/// Perspective 34. The awaiting shape of the same race: interleaved workers,
/// awaits and allocation. A drive that stopped early produced a PREFIX of this
/// output with exit status 0, so a partial run is the failure mode to catch.
#[test]
fn sched_wake_34_interleaved_awaits_complete_on_every_run() {
    let outputs = compile_and_run_repeatedly(
        r#"
async fn worker(id: i64) -> i64 {
    await sleep(1);
    let s = "worker-" + id.toString();
    await sleep(1);
    return id * 10;
}
async fn main() {
    let a = worker(1);
    let b = worker(2);
    let x = await a;
    println(x);
    let y = await b;
    println(y);
    println(x + y);
    println("done");
}
"#,
        REPEATS,
        &[],
    );
    assert_every_run(&outputs, "10\n20\n30\ndone\n");
}

/// Perspective 35. Worker-count variations must not change the result. The
/// runtime clamps `WILLOW_WORKERS` up to its five-worker minimum, so this
/// exercises the configured maximum as well as the clamped request.
#[test]
fn sched_wake_35_worker_count_does_not_change_the_result() {
    let source = r#"
async fn producer(ch: Channel<i64>, n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        await sleep(1);
        ch.send(i);
        i = i + 1;
    }
    return 0;
}
async fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch, 4);
    let mut seen = 0;
    while seen < 4 {
        select {
            let v = ch.recv() => { println(v); }
        }
        seen = seen + 1;
    }
    await p;
}
"#;
    for workers in ["1", "5", "16"] {
        let outputs = compile_and_run_repeatedly(source, 4, &[("WILLOW_WORKERS", workers)]);
        for (run, out) in outputs.iter().enumerate() {
            assert_eq!(
                out, "0\n1\n2\n3\n",
                "WILLOW_WORKERS={workers} run {run} diverged"
            );
        }
    }
}

/// Perspective 36. The blocking select must also hold in release builds, where
/// the debug fault-site instrumentation is absent: the case still runs, and the
/// program still exits successfully.
#[test]
fn sched_wake_36_blocking_select_holds_in_release_builds() {
    let (out, ok) = compile_and_run_release(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(1);
    ch.send(5);
    return 0;
}
fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    select {
        let v = ch.recv() => { println(v); }
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

/// Perspective 37. A cooperative (async) select without `default` suspends
/// rather than block-driving, and must still resume exactly once when its
/// producer sends — under GC stress, where every allocation can collect.
#[test]
fn sched_wake_37_cooperative_select_resumes_under_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn producer(ch: Channel<String>) -> i64 {
    await sleep(1);
    ch.send("payload");
    return 0;
}
async fn main() {
    let ch = Channel<String>::new();
    let p = producer(ch);
    select {
        let v = ch.recv() => { println(v); }
    }
    await p;
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "payload\n");
}

/// Perspective 38. A long-running fixture must not be turned into a hang by the
/// blocking idle wait: the select finishes well inside a generous timeout.
#[test]
fn sched_wake_38_blocking_select_does_not_hang() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn producer(ch: Channel<i64>) -> i64 {
    await sleep(50);
    ch.send(1);
    return 0;
}
fn main() {
    let ch = Channel<i64>::new();
    let p = producer(ch);
    select {
        let v = ch.recv() => { println(v); }
    }
}
"#,
        &[],
        RUN_TIMEOUT,
    );
    assert!(!timed_out, "blocking select hung: {out}");
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}
