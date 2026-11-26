use crate::support::*;

// ── Runtime production observability ──────────────────────────

#[test]
fn observability_01_scheduler_trace_reports_lifecycle_and_timer_events() {
    let (out, trace, ok) = compile_and_run_with_virtual_time_output(
        r#"
async fn work(ready: Channel<i64>) -> i64 {
    ready.send(test_current_task());
    await sleep(2);
    return 42;
}
async fn main() {
    test_set_time(0);
    let ready = Channel<i64>::new();
    let task = work(ready);
    let id = ready.recv();
    while !test_task_parked(id) { await yield(); }
    // Cooperative sleep rounds its positive duration up by one clock tick.
    test_set_time(3);
    println(await task);
}
"#,
        &[("WILLOW_SCHED_TRACE", "1"), ("WILLOW_WORKERS", "5")],
    );
    assert!(
        ok,
        "scheduler trace fixture failed or timed out: {out}{trace}"
    );
    assert_eq!(out, "42\n");
    assert!(trace.contains("[sched]"), "{trace}");
    assert!(trace.contains("event=task_spawn"), "{trace}");
    assert!(trace.contains("event=task_poll"), "{trace}");
    assert!(trace.contains("event=timer_wake"), "{trace}");
    assert!(trace.contains("event=task_complete"), "{trace}");
}

#[test]
fn observability_02_task_trace_reports_cancelled_task_lifecycle() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn work() { let hold_until_cancel = Channel<i64>::new(); hold_until_cancel.recv(); }
async fn main() {
    let task = work();
    task.cancel();
    match await task.result() {
        Ok(value) => println("bad"),
        Err(Cancelled) => println("cancelled"),
    }
}
"#,
        &[("WILLOW_TASK_TRACE", "1"), ("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(15),
    );
    assert!(!timed_out, "task trace run timed out: {out}");
    assert!(ok, "{out}");
    assert!(
        out.starts_with("cancelled\n"),
        "program output changed: {out}"
    );
    assert!(out.contains("[task]"), "{out}");
    assert!(out.contains("event=task_cancel_requested"), "{out}");
    assert!(out.contains("event=task_cancelled"), "{out}");
}
