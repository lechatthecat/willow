use super::*;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn detached_worker_panics_terminate_without_hanging() {
    const KEY: &str = "WILLOW_TEST_BLOCKING_WORKER_PANIC";
    if let Ok(case) = std::env::var(KEY) {
        match case.as_str() {
            "initialize" => {
                std::thread::spawn(|| {
                    blocking_worker_entry(|| panic!("injected worker initialization panic"));
                });
            }
            "job" => {
                assert!(try_submit(Box::new(|| panic!("injected blocking job panic"))).is_ok());
            }
            _ => panic!("unexpected child case"),
        }
        // A lost worker must not look like success merely because the test
        // thread returned. The parent enforces a deadline and reaps the child.
        loop {
            std::thread::park();
        }
    }
    for (case, original) in [
        ("initialize", "injected worker initialization panic"),
        ("job", "injected blocking job panic"),
    ] {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "blocking::failure_tests::detached_worker_panics_terminate_without_hanging",
                "--nocapture",
            ])
            .env(KEY, case)
            .env("WILLOW_BLOCKING_THREADS", "1")
            .env("WILLOW_BLOCKING_QUEUE", "1")
            .env("RUST_BACKTRACE", "0")
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
        let result = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(
            !timed_out,
            "{case}: worker panic left process hanging: {stderr}"
        );
        assert!(!result.status.success(), "{case}: {stderr}");
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(
                result.status.signal(),
                Some(libc::SIGABRT),
                "{case}: {stderr}"
            );
        }
        assert!(stderr.contains(original), "{case}: {stderr}");
        assert!(
            stderr.contains("runtime fatal: Rust panic in blocking worker"),
            "{case}: {stderr}"
        );
    }
}
