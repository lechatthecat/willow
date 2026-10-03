use super::*;

#[test]
fn allocation_failures_terminate_before_returning_to_callers() {
    const KEY: &str = "WILLOW_TEST_FAILURE_POLICY_CASE";
    if let Ok(case) = std::env::var(KEY) {
        willow_gc_init();
        let mut tls = tlab_state_for_test();
        if case.starts_with("storage") || case.starts_with("retry") {
            STORAGE_FAILURES.set(2);
        }
        if case.starts_with("retry") {
            willow_gc_set_memory_limit(u64::MAX);
        }
        match case.as_str() {
            "storage-old" | "retry-old" | "limit-old" => {
                willow_alloc(8);
            }
            "storage-large" | "retry-large" | "limit-large" => {
                willow_alloc(GC_LARGE_OBJECT_THRESHOLD as i64);
            }
            "storage-tlab" | "retry-tlab" | "limit-tlab" => {
                willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
            }
            "negative-old" => {
                willow_alloc(-1);
            }
            "negative-tlab" => {
                willow_gc_alloc_slow(&mut tls, 0, 0, -1, 0);
            }
            "null-tlab" => {
                willow_gc_alloc_slow(std::ptr::null_mut(), 0, 0, 8, 0);
            }
            "null-bitmap" => {
                willow_gc_alloc_bitmap(1, 8, std::ptr::null());
            }
            "oversize" => {
                willow_alloc(i64::MAX);
            }
            _ => panic!("unknown subprocess case"),
        }
        eprintln!("FAILURE_RETURNED_TO_CALLER");
        return;
    }
    for (case, diagnostic) in [
        (
            "storage-old",
            "runtime resource exhausted: managed allocation failed",
        ),
        (
            "storage-tlab",
            "runtime resource exhausted: managed allocation failed",
        ),
        (
            "storage-large",
            "runtime resource exhausted: managed allocation failed",
        ),
        (
            "retry-old",
            "runtime resource exhausted: managed allocation failed",
        ),
        (
            "retry-tlab",
            "runtime resource exhausted: managed allocation failed",
        ),
        (
            "retry-large",
            "runtime resource exhausted: managed allocation failed",
        ),
        (
            "limit-old",
            "runtime resource exhausted: GC memory limit exceeded",
        ),
        (
            "limit-tlab",
            "runtime resource exhausted: GC memory limit exceeded",
        ),
        (
            "limit-large",
            "runtime resource exhausted: GC memory limit exceeded",
        ),
        (
            "negative-old",
            "runtime fatal: negative managed allocation size",
        ),
        (
            "negative-tlab",
            "runtime fatal: invalid TLAB allocation arguments",
        ),
        (
            "null-tlab",
            "runtime fatal: invalid TLAB allocation arguments",
        ),
        (
            "null-bitmap",
            "runtime fatal: Rust panic in willow_gc_alloc_bitmap",
        ),
        (
            "oversize",
            "runtime resource exhausted: managed allocation failed",
        ),
    ] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "gc::failure_policy_tests::allocation_failures_terminate_before_returning_to_callers", "--nocapture"])
            .env(KEY, case)
            .env_remove("WILLOW_GC_MEMORY_LIMIT")
            .env_remove("WILLOW_GC_STRESS");
        if case.starts_with("limit") {
            command.env("WILLOW_GC_MEMORY_LIMIT", "1");
        }
        let result = command.output().unwrap();
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(!result.status.success(), "{case}: returned successfully");
        if diagnostic.contains("resource exhausted") {
            assert_eq!(result.status.code(), Some(1), "{case}: {stderr}");
        }
        assert!(stderr.contains(diagnostic), "{case}: {stderr}");
        assert!(
            !stderr.contains("FAILURE_RETURNED_TO_CALLER"),
            "{case}: {stderr}"
        );
        assert!(
            !stderr.contains("panic in a function that cannot unwind"),
            "{case}: {stderr}"
        );
    }
}

pub(super) const GC_FATAL_CASE: &str = "WILLOW_TEST_GC_FATAL_CASE";

pub(super) fn assert_fatal_child(test: &str, case: &str, diagnostic: &str) -> String {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(GC_FATAL_CASE, case)
        .env("RUST_BACKTRACE", "0")
        .env("WILLOW_GC_MARK_THREADS", "1")
        .env_remove("WILLOW_GC_MEMORY_LIMIT")
        .env_remove("WILLOW_GC_STRESS")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut timed_out = false;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            timed_out = true;
            child.kill().unwrap();
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        !timed_out,
        "{case}: GC panic left process hanging: {stderr}"
    );
    assert!(
        !output.status.success(),
        "{case}: continued after GC panic: {stderr}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            output.status.signal(),
            Some(libc::SIGABRT),
            "{case}: {stderr}"
        );
    }
    assert!(stderr.contains(diagnostic), "{case}: {stderr}");
    assert!(!stderr.contains("GC_PANIC_RETURNED"), "{case}: {stderr}");
    stderr
}
