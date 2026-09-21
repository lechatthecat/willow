use super::*;

// ---------------------------------------------------------------------------
// Frame-backed task status, end to end (willow-ezs.1.3).
//
// The runtime unit tests cover the status word itself; these cover what the
// compiled language actually does with it, which unit tests cannot reach:
// the compiler must pass the awaitee's FRAME (not just its id) to every
// status query, and the answers must stay correct under multiple workers.
//
//   33. a finished task's status is stable across repeated queries
//   34. awaiting an already-finished task returns its value again, and does
//       not re-run the body
//   35. cancel() is visible on the handle before the scheduler runs again
//   36. cancelling a task that already completed does not make it cancelled
//   37. await task.result() distinguishes Ok / Err(Cancelled) with 5 workers
//   38. many live handles keep INDEPENDENT statuses (no cross-talk between
//       frames)
//   39. status survives unrelated task churn (frames are not clobbered by
//       later allocations)
//   40. status queries are unaffected by GC stress
//   41. eager/nested await of a cancelled task aborts before reading result
//   42. cooperative await of a task variable does the same
// ---------------------------------------------------------------------------

/// 33. Status is a property of the frame, so asking twice gives the same
///     answer — it is not consumed by the first read.
#[test]
fn frame_status_33_finished_status_is_stable() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn work(n: i64) -> i64 {
    await sleep(2);
    return n + 1;
}

async fn main() {
    let t = work(1);
    println(t.is_cancelled());
    println(await t);
    println(t.is_cancelled());
    println(t.is_cancelled());
    println(t.is_cancelled());
}
"#,
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(30),
    );
    assert!(!timed_out, "status query hung:\n{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "false\n2\nfalse\nfalse\nfalse\n");
}

/// 34. A second `await` of a finished task must read the stored result, not
///     re-run the body (the counter would go up if it did) and not park.
#[test]
fn frame_status_34_repeated_await_does_not_rerun() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn work(runs: Channel<i64>) -> i64 {
    await sleep(2);
    runs.send(1);
    return 7;
}

async fn main() {
    let runs = Channel<i64>::with_capacity(8);
    let t = work(runs);
    println(await t);
    println(await t);
    println(await t);
    // Exactly one body execution, so exactly one value is queued: the first
    // recv takes it and the second case finds the channel empty.
    println(runs.recv());
    select {
        let v = runs.recv() => { println(v); }
        default => { println(0); }
    }
}
"#,
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(30),
    );
    assert!(!timed_out, "repeated await hung:\n{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "7\n7\n7\n1\n0\n");
}

/// 35. `cancel()` writes the request into the frame, so the very next query
///     sees it without the scheduler having run in between.
#[test]
fn frame_status_35_cancel_is_visible_immediately() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn slow() -> i64 {
    await sleep(5000);
    return 1;
}

async fn main() {
    let t = slow();
    println(t.is_cancelled());
    t.cancel();
    println(t.is_cancelled());
}
"#,
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(30),
    );
    assert!(!timed_out, "cancel visibility hung:\n{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "false\ntrue\n");
}

/// 36. Cancelling a task that already finished is a no-op: its published
///     status stays Completed and its result stays readable.
#[test]
fn frame_status_36_cancel_after_completion_is_ignored() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn work() -> i64 {
    return 5;
}

async fn main() {
    let t = work();
    println(await t);
    t.cancel();
    println(t.is_cancelled());
    println(await t);
}
"#,
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(30),
    );
    assert!(!timed_out, "late cancel hung:\n{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "5\nfalse\n5\n");
}

/// 37. `await task.result()` reads the terminal code out of the frame:
///     Completed -> Ok, Cancelled -> Err, using the normal Task wait path.
#[test]
fn frame_status_37_task_result_reads_the_terminal_code() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn work(n: i64) -> i64 {
    await sleep(n);
    return n;
}

async fn main() {
    let fine = work(2);
    let stopped = work(5000);
    stopped.cancel();
    match await fine.result() {
        Ok(v) => println(v),
        Err(e) => println(-1),
    }
    match await stopped.result() {
        Ok(v) => println(v),
        Err(e) => println(-1),
    }
}
"#,
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(30),
    );
    assert!(!timed_out, "task-result await hung:\n{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "2\n-1\n");
}

/// 38. Each handle answers from ITS OWN frame: cancelling every other task in
///     a batch must not disturb the ones left alone.
#[test]
fn frame_status_38_statuses_are_independent_per_handle() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn work(n: i64) -> i64 {
    await sleep(2);
    return n;
}

async fn main() {
    let mut spawned = 0;
    let mut cancelled_count = 0;
    let mut sum = 0;
    while spawned < 40 {
        let t = work(spawned);
        if spawned % 2 == 0 {
            t.cancel();
            if t.is_cancelled() {
                cancelled_count = cancelled_count + 1;
            }
        } else {
            sum = sum + await t;
            if t.is_cancelled() {
                cancelled_count = cancelled_count + 100;
            }
        }
        spawned = spawned + 1;
    }
    println(cancelled_count);
    println(sum);
}
"#,
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(30),
    );
    assert!(!timed_out, "per-handle status hung:\n{out}");
    assert!(ok, "{out}");
    // 20 cancelled, none of the awaited 20 mislabelled; 1+3+...+39 = 400.
    assert_eq!(out, "20\n400\n");
}

/// 39. A finished task's status must survive later task churn — its frame
///     stays rooted through the handle, so hundreds of subsequent tasks
///     cannot overwrite the answer.
#[test]
fn frame_status_39_status_survives_task_churn() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn work(n: i64) -> i64 {
    await sleep(1);
    return n;
}

async fn main() {
    let early = work(99);
    println(await early);

    let mut spawned = 0;
    while spawned < 300 {
        let t = work(spawned);
        let _ = await t;
        spawned = spawned + 1;
    }

    println(await early);
    println(early.is_cancelled());
}
"#,
        &[("WILLOW_WORKERS", "5")],
        std::time::Duration::from_secs(60),
    );
    assert!(!timed_out, "task churn hung:\n{out}");
    assert!(ok, "{out}");
    assert_eq!(out, "99\n99\nfalse\n");
}

/// 40. The status word lives in the frame header, which the GC never treats as
///     a pointer — collection under stress must not perturb it.
#[test]
fn frame_status_40_status_is_stable_under_gc_stress() {
    let (out, ok, timed_out) = compile_and_run_with_env_timeout(
        r#"
async fn work(n: i64) -> i64 {
    await sleep(1);
    return n * 2;
}

async fn main() {
    let done = work(4);
    println(await done);

    let stopped = work(5000);
    stopped.cancel();

    let mut spawned = 0;
    let mut sum = 0;
    while spawned < 20 {
        let t = work(spawned);
        sum = sum + await t;
        spawned = spawned + 1;
    }

    println(sum);
    println(await done);
    println(stopped.is_cancelled());
    println(done.is_cancelled());
}
"#,
        &[("WILLOW_WORKERS", "5"), ("WILLOW_GC_STRESS", "alloc")],
        std::time::Duration::from_secs(60),
    );
    assert!(!timed_out, "gc stress status hung:\n{out}");
    assert!(ok, "{out}");
    // 2 * (0 + 1 + ... + 19) = 380.
    assert_eq!(out, "8\n380\n8\ntrue\nfalse\n");
}

/// Regression 41: a nested await uses the eager `run_until` emission path.
/// Cancelled is a terminal status, but it has no result slot to print.
#[test]
fn frame_status_41_eager_await_of_cancelled_task_aborts() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
async fn slow() -> i64 {
    await sleep(5000);
    return 7;
}

async fn main() {
    let stopped = slow();
    stopped.cancel();
    println(await stopped);
}
"#,
    );
    assert!(!ok, "cancelled eager await must abort, output: {out}");
    assert!(
        out.contains("awaited a cancelled task"),
        "missing cancelled-await diagnostic: {out}"
    );
}

/// Regression 42: a statement-position await of a task variable uses the
/// cooperative suspend/resume path and must perform the same terminal check on
/// resume.
#[test]
fn frame_status_42_cooperative_await_of_cancelled_task_aborts() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
async fn slow() -> i64 {
    await sleep(5000);
    return 7;
}

async fn main() {
    let stopped = slow();
    stopped.cancel();
    let value = await stopped;
    println(value);
}
"#,
    );
    assert!(!ok, "cancelled cooperative await must abort, output: {out}");
    assert!(
        out.contains("awaited a cancelled task"),
        "missing cancelled-await diagnostic: {out}"
    );
}
