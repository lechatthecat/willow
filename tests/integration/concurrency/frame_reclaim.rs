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
        let end = if i + 64 < 20000 { i + 64 } else { 20000 };
        n = n + await batch(i, end);
        await yield();
        i = end;
    }
    gc_collect();
    println(n);
    let bytes = gc_allocated_bytes();
    println(bytes < 200000);
    if bytes >= 200000 { println(bytes); }
}
"#;

#[test]
fn vjaf_int_01_parallel_workers_keep_heap_bounded() {
    // 40,000 directly awaited + 20,000 scope-owned tasks would retain >3 MB of frames
    // under the old outermost-drive rule.
    for workers in ["1", "4", "8"] {
        let (out, ok) = compile_and_run_with_env(LONG_AWAIT_LOOP, &[("WILLOW_WORKERS", workers)]);
        assert!(ok, "workers={workers}: {out}");
        assert_eq!(out, "200098890\ntrue\n", "workers={workers}");
    }
}

#[test]
fn vjaf_int_02_released_frames_survive_minor_gc_stress() {
    // Every allocation runs a minor collection while frames are released
    // mid-drive: awaited results stay readable and nothing dangles.
    let (out, ok) = compile_and_run_with_env(
        "async fn make(i: i64) -> i64 { return i; }\nasync fn label(i: i64) -> String { return format(\"v{}\", i); }\nasync fn main() { let mut n = 0; let mut i = 0; while i < 2000 { let a = make(i); let b = label(i); make(i); await yield(); n = n + await a + (await b).len(); i = i + 1; } println(n); }",
        &[("WILLOW_WORKERS", "4"), ("WILLOW_GC_STRESS", "minor")],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2007890\n");
}
