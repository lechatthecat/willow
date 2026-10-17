//! Process exit that does not depend on the state of runtime threads.
//!
//! A Willow program exits when `main` returns; the runtime deliberately does
//! not drain unawaited tasks first, because a non-terminating background task
//! would then hang the exit (willow-bsqy). So there are always runtime-owned
//! threads alive at exit — scheduler persistent workers, the GC coordinator,
//! GC marker workers, the blocking pool — and the platform terminates them
//! wherever they happen to be.
//!
//! On Windows that is a deadlock: `ExitProcess` terminates every other thread
//! first and then runs `DLL_PROCESS_DETACH` and the rest of the CRT teardown,
//! which take locks (the process heap lock above all) that a terminated thread
//! may have been holding. A GC marker or scheduler worker caught inside
//! `malloc`/`free` while tracing therefore strands the exiting thread forever:
//! the process keeps one thread, in a `WrAlertByThreadId` lock wait, with its
//! exit status already published and its image still mapped. willow-ihr5 is
//! two Windows CI jobs that spent their whole budget there, one of them three
//! hours, after the program had printed all of its expected output.
//!
//! Asking the kernel to tear the process down instead takes no user-mode lock,
//! so it cannot deadlock against a thread the exit itself just terminated.
//! Nothing in the runtime relies on the teardown being skipped: every Willow
//! `print` flushes before it returns, and the runtime registers no `atexit`
//! handlers and keeps no exit-time reporting.

/// Exit the process with `code`, without running user-mode process teardown.
///
/// Callers must have flushed anything they need durable. Every Willow print
/// already flushes, so only Rust-side buffering would need care, and the
/// runtime writes its own diagnostics to unbuffered stderr.
#[cold]
pub(crate) fn terminate_without_teardown(code: i32) -> ! {
    #[cfg(windows)]
    {
        use std::ffi::c_void;

        // Same declarations as the stack-overflow handler's, which already
        // ends a process this way.
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut c_void;
            fn TerminateProcess(process: *mut c_void, code: u32) -> i32;
            fn ExitProcess(code: u32) -> !;
        }
        // SAFETY: a pseudo-handle for the current process with full access;
        // the call cannot fail for it.
        unsafe {
            TerminateProcess(GetCurrentProcess(), code as u32);
        }
        // TerminateProcess only REQUESTS termination, so returning into the
        // CRT's teardown from here would reintroduce the very deadlock this
        // function exists to avoid. Wait to be terminated instead.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
            std::thread::park_timeout(remaining);
        }
        // Unreachable in practice: the only way here is a TerminateProcess the
        // kernel refused. Teardown may deadlock, but a process that might exit
        // beats one that certainly never does.
        // SAFETY: a plain exit request, with no preconditions.
        unsafe { ExitProcess(code as u32) }
    }
    // Other targets keep the platform's ordinary exit: the hazard is specific
    // to Windows terminating threads before running user-mode teardown.
    #[cfg(not(windows))]
    std::process::exit(code)
}
