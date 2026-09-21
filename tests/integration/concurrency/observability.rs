use crate::support::*;

// ── Runtime production observability (willow-2vq) ──────────────────────────

#[test]
fn observability_01_scheduler_trace_reports_lifecycle_and_timer_events() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn work() -> i64 { await sleep(2); return 42; }
async fn main() { println(await work()); }
"#,
        &[("WILLOW_SCHED_TRACE", "1"), ("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(15),
    );
    assert!(!timed_out, "scheduler trace run timed out: {out}");
    assert!(ok, "{out}");
    assert!(out.starts_with("42\n"), "program output changed: {out}");
    assert!(out.contains("[sched]"), "{out}");
    assert!(out.contains("event=task_spawn"), "{out}");
    assert!(out.contains("event=task_poll"), "{out}");
    assert!(out.contains("event=timer_wake"), "{out}");
    assert!(out.contains("event=task_complete"), "{out}");
}

#[test]
fn observability_02_task_trace_reports_cancelled_task_lifecycle() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn work() { await sleep(10000); }
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
