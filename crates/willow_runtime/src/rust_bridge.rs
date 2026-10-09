//! R2 permits synchronous foreign calls only, outside scheduler task execution.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_rust_bridge_enter(revision: u32) {
    if revision != willow_abi::ffi::WILLOW_RUST_BRIDGE_ABI_REVISION {
        crate::failure::fatal_invariant("rust_bridge_abi_mismatch");
    }
    if crate::scheduler::willow_sched_current_task() != 0 {
        crate::failure::fatal_invariant(
            "rust_bridge_worker_blocking: Rust calls require synchronous context",
        );
    }
}
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_rust_bridge_panic() -> ! {
    crate::failure::fatal_invariant("RustPanic");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn incompatible_revision_stops_before_foreign_execution() {
        const KEY: &str = "WILLOW_TEST_RUST_BRIDGE_REVISION";
        if std::env::var_os(KEY).is_some() {
            willow_rust_bridge_enter(willow_abi::ffi::WILLOW_RUST_BRIDGE_ABI_REVISION + 1);
            panic!("REVISION_GATE_RETURNED");
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "rust_bridge::tests::incompatible_revision_stops_before_foreign_execution",
                "--nocapture",
            ])
            .env(KEY, "1")
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("rust_bridge_abi_mismatch"), "{stderr}");
        assert!(!stderr.contains("REVISION_GATE_RETURNED"), "{stderr}");
    }
    #[test]
    fn synchronous_current_revision_enters_without_side_effects() {
        for _ in 0..1024 {
            willow_rust_bridge_enter(willow_abi::ffi::WILLOW_RUST_BRIDGE_ABI_REVISION);
        }
    }
}
