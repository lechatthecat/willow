use super::support::*;
use std::time::Duration;

#[test]
fn async_frame_safepoints_preserve_values_and_control_flow() {
    let source = include_str!("../../example/async_frame_safepoints.wi");
    for (out, ok) in [
        compile_and_run(source),
        compile_and_run_release(source),
        compile_and_run_with_runtime_env(
            source,
            &[("WILLOW_WORKERS", "1"), ("WILLOW_TASK_BUDGET", "1")],
            Duration::from_secs(60),
        ),
        compile_and_run_with_runtime_env(
            source,
            &[
                ("WILLOW_WORKERS", "4"),
                ("WILLOW_TASK_BUDGET", "65"),
                ("WILLOW_GC_STRESS", "alloc,scheduler"),
            ],
            Duration::from_secs(60),
        ),
    ] {
        assert!(ok, "{out}");
        assert_eq!(out, "20\n20\n390\n6\n");
    }
}
