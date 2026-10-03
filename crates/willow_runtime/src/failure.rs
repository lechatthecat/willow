//! Implementation of the crate-level runtime failure policy.

use std::io::Write;

fn diagnostic(class: &str, message: std::fmt::Arguments<'_>) {
    // Unlike eprintln!, an unavailable/broken stderr must not start a second
    // panic. Formatting uses borrowed arguments, without allocating a String.
    let _ = writeln!(std::io::stderr().lock(), "runtime {class}: {message}");
}

#[cold]
pub(crate) fn fatal_invariant(message: &str) -> ! {
    diagnostic("fatal", format_args!("{message}"));
    std::process::abort();
}

#[cold]
pub(crate) fn resource_exhausted(message: std::fmt::Arguments<'_>) -> ! {
    diagnostic("resource exhausted", message);
    std::process::exit(1);
}

#[inline]
pub(crate) fn ffi_boundary<R>(name: &'static str, body: impl FnOnce() -> R) -> R {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(value) => value,
        Err(payload) => abort_panic(name, payload),
    }
}

#[cold]
#[inline(never)]
fn abort_panic(name: &str, payload: Box<dyn std::any::Any + Send>) -> ! {
    // Share the fatal path across all closure instantiations. A user-defined
    // payload may panic in Drop; do not run it while terminating the process.
    std::mem::forget(payload);
    diagnostic("fatal", format_args!("Rust panic in {name}"));
    std::process::abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[willow_runtime_macros::ffi_boundary]
    extern "C" fn injected_failure(case: i32) -> usize {
        match case {
            0 => panic!("unexpected implementation panic"),
            1 => {
                let missing: Option<usize> = std::hint::black_box(None);
                missing.unwrap()
            }
            2 => {
                let error: Result<usize, &str> = std::hint::black_box(Err("error"));
                error.expect("unexpected error")
            }
            3 => {
                struct BadDrop;
                impl Drop for BadDrop {
                    fn drop(&mut self) {
                        panic!("PAYLOAD_DROP_RAN");
                    }
                }
                std::panic::panic_any(BadDrop);
            }
            4 => injected_failure(0),
            _ => 42,
        }
    }

    #[test]
    fn ffi_panics_abort_inside_the_boundary() {
        const KEY: &str = "WILLOW_TEST_FFI_PANIC_CASE";
        if let Ok(case) = std::env::var(KEY) {
            injected_failure(case.parse().unwrap());
            eprintln!("FAILURE_RETURNED_TO_CALLER");
            return;
        }
        for case in 0..5 {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "failure::tests::ffi_panics_abort_inside_the_boundary",
                    "--nocapture",
                ])
                .env(KEY, case.to_string())
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&result.stderr);
            assert!(!result.status.success(), "{stderr}");
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(result.status.signal(), Some(libc::SIGABRT), "{stderr}");
            }
            assert!(
                stderr.contains("runtime fatal: Rust panic in injected_failure"),
                "{stderr}"
            );
            assert!(!stderr.contains("PAYLOAD_DROP_RAN"), "{stderr}");
            assert!(!stderr.contains("FAILURE_RETURNED_TO_CALLER"), "{stderr}");
            assert!(
                !stderr.contains("panic in a function that cannot unwind"),
                "{stderr}"
            );
        }
    }

    #[test]
    fn nested_and_repeated_boundaries_execute_each_body_once() {
        fn descend(depth: usize, visits: &mut usize) {
            ffi_boundary("descend", || {
                *visits += 1;
                if depth != 0 {
                    descend(depth - 1, visits);
                }
            });
        }
        for depth in [1, 8, 32, 128] {
            for repeats in [1, 16, 64] {
                let mut visits = 0;
                for _ in 0..repeats {
                    descend(depth, &mut visits);
                }
                assert_eq!(visits, (depth + 1) * repeats);
                println!("depth={depth} repeats={repeats} body_visits={visits}");
            }
        }
    }

    #[test]
    fn successful_boundary_preserves_return_and_mutation() {
        assert_eq!(injected_failure(99), 42);
        let mut value = 0;
        assert_eq!(
            ffi_boundary("mutation", || {
                value += 1;
                17
            }),
            17
        );
        assert_eq!(value, 1);
    }
}
