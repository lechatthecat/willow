//! A finished program must actually exit (willow-ihr5).
//!
//! A Willow program exits when `main` returns; the runtime deliberately does
//! not drain unawaited tasks first, since a non-terminating background task
//! would then hang the exit (willow-bsqy). Scheduler persistent workers, the
//! GC coordinator, GC marker workers and the blocking pool are therefore all
//! still alive at that moment, and the platform terminates them wherever they
//! happen to be.
//!
//! On Windows that used to deadlock. `ExitProcess` terminates every other
//! thread and then runs `DLL_PROCESS_DETACH` and the rest of the CRT teardown,
//! which take locks — the process heap lock above all — that a terminated
//! thread may have been holding. A marker or scheduler worker caught inside
//! `malloc`/`free` while tracing stranded the exiting thread for good: one
//! thread left in a lock wait, the exit status already published, and no
//! further output ever. Two Windows CI jobs burned their whole budget there,
//! one of them three hours, AFTER the program had printed everything it was
//! expected to print. `exit::terminate_without_teardown` now hands the
//! teardown to the kernel instead, which takes no user-mode lock.
//!
//! Driving this from the examples that caught it keeps the perspective honest:
//! both programs finish their work and the only thing under test is whether
//! the process goes away afterwards. Two properties make a run reproduce it:
//!
//!   * a GC stress mode that includes `scheduler`, so a collection runs on
//!     every scheduler loop iteration and a worker thread is likely to be
//!     inside a Rust allocation exactly when `main` returns — `alloc`,
//!     `minor` and `relocate` collect from mutator allocation sites instead,
//!     leave the workers parked, and never reproduced it in 600 runs;
//!   * a contended machine, which is why the runs go out in parallel lanes.
//!
//! Windows-only: the hazard is specific to a platform that terminates threads
//! before running user-mode teardown, and the run counts would otherwise cost
//! CI time on targets that cannot exhibit it.
#![cfg(windows)]

use super::support::*;
use std::time::Duration;

/// Generous enough that a loaded CI runner cannot fail it by being slow: the
/// stall it catches never ends, while these programs finish in well under a
/// second even under stress.
const EXIT_DEADLINE: Duration = Duration::from_secs(30);

/// Lanes, not speed: the stall needs the machine contended to appear.
const LANES: usize = 4;

#[test]
fn assignable_representations_example_exits_under_scheduler_gc_stress() {
    let project = TestProject::new(
        "exit_teardown_repr",
        &[(
            "main.wi",
            include_str!("../../example/assignable_representations.wi"),
        )],
    );
    let expected = "value:8\nvalue:7\n9\ntrue\n";
    // `all` is the mode set that includes `scheduler`; the narrower modes do
    // not reach the exit window.
    let env = [("WILLOW_GC_STRESS", "all")];
    for release in [false, true] {
        let args = if release { vec!["--release"] } else { vec![] };
        let compiled = project.compile_with_args("main.wi", &args);
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        if let Err(report) =
            project.run_repeatedly_with_env_timeout(&env, expected, 80, LANES, EXIT_DEADLINE)
        {
            panic!("release={release}: {report}");
        }
    }
}

#[test]
fn task_sync_cancel_example_exits_under_scheduler_gc_stress() {
    let project = TestProject::new(
        "exit_teardown_cancel",
        &[("main.wi", include_str!("../../example/task_sync_cancel.wi"))],
    );
    let expected = "sync inner\nsync outer\nlock defer\nasync outer\n42\n";
    for release in [false, true] {
        let args = if release { vec!["--release"] } else { vec![] };
        let compiled = project.compile_with_args("main.wi", &args);
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        // The same environment the CI failure ran under, at both worker
        // counts: one worker and a pool both stalled at exit.
        for workers in ["1", "4"] {
            let env = [
                ("WILLOW_WORKERS", workers),
                ("WILLOW_TASK_BUDGET", "1"),
                ("WILLOW_GC_STRESS", "alloc,scheduler"),
            ];
            if let Err(report) =
                project.run_repeatedly_with_env_timeout(&env, expected, 150, LANES, EXIT_DEADLINE)
            {
                panic!("release={release} workers={workers}: {report}");
            }
        }
    }
}
