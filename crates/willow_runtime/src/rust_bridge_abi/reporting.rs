//! Bridges opt into normal-exit diagnostics when their table initializes.
//! No bridge symbol is linked or resolved by the runtime.
use std::sync::Mutex;

static REPORTERS: Mutex<Vec<extern "C" fn()>> = Mutex::new(Vec::new());

/// Register once per bridge table. The callback must remain valid for the
/// process lifetime and must not unwind or change the process exit status.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_rust_bridge_register_reporter(report: extern "C" fn()) {
    REPORTERS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(report);
}

pub(crate) fn report_normal_exit() {
    if std::thread::panicking() {
        return;
    }
    let reporters = REPORTERS.lock().unwrap_or_else(|p| p.into_inner()).clone();
    for report in reporters {
        report();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_registers_reporters_without_a_bridge_dependency() {
        const CHILD: &str = "WILLOW_REPORTER_CHILD";
        if std::env::var_os(CHILD).is_some() {
            extern "C" fn first() {
                eprintln!("FIRST_REPORT");
            }
            extern "C" fn second() {
                eprintln!("SECOND_REPORT");
            }
            report_normal_exit(); // pure runtime: no bridge and no callbacks
            willow_rust_bridge_register_reporter(first);
            willow_rust_bridge_register_reporter(second);
            report_normal_exit();
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "rust_bridge_abi::reporting::tests::test_build_registers_reporters_without_a_bridge_dependency", "--nocapture"])
            .env(CHILD, "1").output().unwrap();
        assert!(output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(stderr.matches("FIRST_REPORT").count(), 1, "{stderr}");
        assert_eq!(stderr.matches("SECOND_REPORT").count(), 1, "{stderr}");
    }
}
