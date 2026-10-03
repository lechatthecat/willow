use super::*;

// ── Terminal task frames reclaimed during a long drive (willow-vjaf) ───────
// Finished task frames used to stay runtime roots until the outermost drive
// returned, so a long-running `async main` retained every frame it finished.
// Runtime-level perspectives live in willow_runtime's `frame_reclaim` and
// `scheduler_tests` (vjaf_*); these cover the generated-code paths.

const LONG_AWAIT_LOOP: &str = "async fn make(i: i64) -> i64 { return i; }\nasync fn label(i: i64) -> String { return format(\"v{}\", i); }\nasync fn main() { let mut n = 0; let mut i = 0; while i < 20000 { n = n + await make(i) + (await label(i)).len(); make(i); i = i + 1; } await sleep(10); gc_collect(); println(n); println(gc_allocated_bytes() < 200000); }";

#[test]
fn vjaf_int_01_parallel_workers_keep_heap_bounded() {
    // 40,000 awaited + 20,000 detached tasks would retain >3 MB of frames
    // under the old outermost-drive rule.
    let (out, ok) = compile_and_run_with_env(LONG_AWAIT_LOOP, &[("WILLOW_WORKERS", "4")]);
    assert!(ok, "{out}");
    assert_eq!(out, "200098890\ntrue\n");
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
