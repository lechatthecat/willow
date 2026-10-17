use super::support::*;

#[test]
fn assignable_representations_example_differential_gc_and_placement() {
    let source = include_str!("../../example/assignable_representations.wi");
    let expected = "value:8\nvalue:7\n9\ntrue\n";
    // Arguments/results, stack/frame locals, fields, array elements and enum
    // payloads cross runtime FFI allocation/collection/print/await boundaries.
    // A Windows CI run once parked here until the job timeout. Bound each mode
    // and, on a stall, rerun it with scheduler/task tracing for the report.
    let timeout = std::time::Duration::from_secs(60);
    for mode in ["", "alloc", "minor", "relocate", "all"] {
        let env = [("WILLOW_GC_STRESS", mode)];
        let (out, ok, timed_out) = compile_and_run_with_env_timeout(source, &env, timeout);
        if timed_out {
            let traced = [
                ("WILLOW_GC_STRESS", mode),
                ("WILLOW_SCHED_TRACE", "1"),
                ("WILLOW_TASK_TRACE", "1"),
            ];
            let (trace, _, trace_timed_out) =
                compile_and_run_with_env_timeout(source, &traced, timeout);
            panic!(
                "mode {mode:?} timed out after {timeout:?}; partial output:\n{out}\n\
                 traced rerun (timed_out={trace_timed_out}):\n{trace}"
            );
        }
        assert!(ok, "{mode}: {out}");
        assert_eq!(out, expected, "{mode}");
    }
    let (out, ok) = compile_and_run_release(source);
    assert!(ok, "release: {out}");
    assert_eq!(out, expected);
}
