use super::*;

// ── Terminal task frames reclaimed during a long drive (willow-vjaf) ───────
// Finished task frames used to stay runtime roots until the outermost drive
// returned, so a long-running `async main` retained every frame it finished.
// Runtime-level perspectives live in willow_runtime's `frame_reclaim` and
// `scheduler_tests` (vjaf_*); these cover the generated-code paths.

// Completion and grace periods are separate: a poll that was already running
// when a task completed must end before its frame can be unrooted. Bound each
// batch, await its scope, then explicitly yield; do not rely on an await of an
// already-ready task suspending, or on a fixed sleep draining a loaded runner.
const LONG_AWAIT_LOOP: &str = r#"
async fn make(i: i64) -> i64 { return i; }
async fn label(i: i64) -> String { return format("v{}", i); }
async fn batch(start: i64, end: i64) -> i64 {
    let scope = TaskScope::new();
    let mut n = 0;
    let mut i = start;
    while i < end {
        n = n + await make(i) + (await label(i)).len();
        scope.add(make(i));
        i = i + 1;
    }
    await scope.finish();
    return n;
}
async fn main() {
    let mut n = 0;
    let mut i = 0;
    while i < 20000 {
        let end = if i + BATCH < 20000 { i + BATCH } else { 20000 };
        n = n + await batch(i, end);
        await yield();
        i = end;
    }
    let mut attempts = 0;
    gc_collect();
    while gc_allocated_bytes() >= 200000 && attempts < 1000 {
        await yield();
        gc_collect();
        attempts = attempts + 1;
    }
    println(n);
    let bytes = gc_allocated_bytes();
    println(bytes < 200000);
    if bytes >= 200000 { println(bytes); gc_task_frame_diagnostics(); }
}
"#;

#[test]
fn vjaf_int_01_parallel_workers_keep_heap_bounded() {
    // Completion batch size is independent of the runtime release threshold.
    for workers in ["1", "4", "8"] {
        for batch in [1, 7, 65, 257] {
            let source = LONG_AWAIT_LOOP.replace("BATCH", &batch.to_string());
            let (out, ok) = compile_and_run_with_env(&source, &[("WILLOW_WORKERS", workers)]);
            assert!(ok, "workers={workers} batch={batch}: {out}");
            assert_eq!(
                out, "200098890\ntrue\n",
                "workers={workers} batch={batch}: {out}"
            );
        }
    }
}

#[test]
fn vjaf_int_02_released_frames_survive_minor_gc_stress() {
    // Every allocation runs a minor collection while frames are released
    // mid-drive: awaited results stay readable and nothing dangles.
    for workers in ["1", "4", "8"] {
        let (out, ok) = compile_and_run_with_env(
            "async fn make(i: i64) -> i64 { return i; }\nasync fn label(i: i64) -> String { return format(\"v{}\", i); }\nasync fn main() { let mut n = 0; let mut i = 0; while i < 2000 { let a = make(i); let b = label(i); make(i); await yield(); n = n + await a + (await b).len(); i = i + 1; } println(n); }",
            &[("WILLOW_WORKERS", workers), ("WILLOW_GC_STRESS", "minor")],
        );
        assert!(ok, "{out}");
        assert_eq!(out, "2007890\n", "workers={workers}: {out}");
    }
}

const REPEATED_RESULT_AWAIT: &str = r#"
async fn value(i: i64) -> i64 { return i; }
async fn pending(ready: AtomicBool) -> i64 {
    ready.store(true);
    while true { await yield(); }
    return 0;
}
async fn batch(i: i64) -> i64 {
    let scope = TaskScope::new();
    let task = value(i);
    scope.add(task);
    await scope.finish();
    gc_collect();
    let mut n = 0;
    let mut j = 0;
    while j < 32 {
        n = n + await task;
        match await task.result() { Ok(v) => { n = n + v; }, Err(Cancelled) => { n = -99999; }, }
        j = j + 1;
    }
    let ready = AtomicBool::new(false);
    let stopped = pending(ready);
    while !ready.load() { await yield(); }
    stopped.cancel();
    j = 0;
    while j < 32 {
        match await stopped.result() { Ok(v) => { n = -99999; }, Err(Cancelled) => {}, }
        j = j + 1;
    }
    return n;
}
async fn main() {
    let mut i = 0;
    let mut n = 0;
    while i < 128 { n = n + await batch(i); await yield(); i = i + 1; }
    let mut attempts = 0;
    gc_collect();
    while gc_allocated_bytes() >= 16384 && attempts < 1000 {
        await yield(); gc_collect(); attempts = attempts + 1;
    }
    println(n);
    println(gc_allocated_bytes() < 16384);
    if gc_allocated_bytes() >= 16384 { println(gc_allocated_bytes()); gc_task_frame_diagnostics(); }
}
"#;

#[test]
fn m2_int_repeated_result_await_and_scope_handles_do_not_retain_batches() {
    for workers in ["1", "4", "8"] {
        let (out, ok) =
            compile_and_run_with_env(REPEATED_RESULT_AWAIT, &[("WILLOW_WORKERS", workers)]);
        assert!(ok, "workers={workers}: {out}");
        assert_eq!(out, "520192\ntrue\n", "workers={workers}: {out}");
    }
}

#[test]
fn m2_int_reclamation_failure_outputs_process_local_lifecycle_fields() {
    // Exercise the actual two subprocess failure branches. Only these fault
    // fixtures use an impossible predicate and one drain attempt; the normal
    // thresholds, workload sizes and bounded waits above remain unchanged.
    let failures = [
        (
            "long_await",
            LONG_AWAIT_LOOP.replace("BATCH", "7").replace("200000", "0"),
        ),
        (
            "repeated_result",
            REPEATED_RESULT_AWAIT.replace("16384", "0"),
        ),
    ];
    for (case, source) in failures {
        let source = source.replace("attempts < 1000", "attempts < 1");
        for workers in ["1", "4", "8"] {
            let (out, ok) = compile_and_run_with_env(&source, &[("WILLOW_WORKERS", workers)]);
            assert!(ok, "case={case} workers={workers}: {out}");
            assert!(
                out.contains("\nfalse\n"),
                "failure branch was not exercised: {out}"
            );
            let lines: Vec<_> = out
                .lines()
                .filter(|line| line.starts_with("task_frame_lifetime_v1 "))
                .collect();
            assert_eq!(lines.len(), 1, "case={case} workers={workers}: {out}");
            let diagnostic = lines[0];
            assert!(diagnostic.len() <= 2048, "unbounded diagnostic: {out}");
            let counter = |field: &str| -> u64 {
                let prefix = format!("{field}: ");
                let value = diagnostic
                    .split_once(&prefix)
                    .unwrap_or_else(|| panic!("missing {field}: {out}"))
                    .1;
                value
                    .split(|c: char| !c.is_ascii_digit())
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap_or_else(|_| panic!("invalid {field}: {out}"))
            };
            let allocated = counter("allocated_frame_bytes");
            let reclaimed = counter("reclaimed_frame_bytes");
            assert!(
                allocated > 0 && reclaimed > 0 && reclaimed <= allocated,
                "{out}"
            );
            for field in [
                "task_frames_running",
                "task_frames_terminal",
                "task_frames_rooted",
                "task_frames_retired",
                "task_frames_reclaimed",
                "quarantined_frames",
                "pending_drops",
                "frames_created",
                "metadata_removed_frames",
                "terminal_events",
                "terminal_frames_reclaimed",
                "scheduler_roots_added",
                "scheduler_roots_released",
                "quiescent_events",
                "scope_owner_releases",
                "quarantined_terminal_frames",
            ] {
                assert!(counter(field) <= counter("frames_created"), "{out}");
            }
            assert!(
                counter("task_frames_running") > 0,
                "snapshot must come from the running child process: {out}"
            );
            assert!(counter("task_frames_reclaimed") > 0, "{out}");
            let expected_frames = if case == "long_await" {
                3 * 20000 + 2 * 20000usize.div_ceil(7) + 1
            } else {
                4 * 128 + 1
            } as u64;
            assert_eq!(counter("frames_created"), expected_frames, "{out}");
            assert_eq!(
                counter("task_frames_reclaimed"),
                expected_frames - 1,
                "{out}"
            );
            assert_eq!(counter("task_frames_rooted"), 1, "{out}");
            assert_eq!(counter("task_frames_retired"), 0, "{out}");
            assert_eq!(counter("task_frames_terminal"), 0, "{out}");
            assert_eq!(counter("metadata_removed_bytes"), reclaimed, "{out}");
            assert_eq!(counter("quarantined_bytes"), 0, "{out}");
            assert!(
                counter("poll_exits") > 0 && counter("sweep_completions") > 0,
                "{out}"
            );
            println!("M2 failure case={case} workers={workers} {diagnostic}");
        }
    }
}
