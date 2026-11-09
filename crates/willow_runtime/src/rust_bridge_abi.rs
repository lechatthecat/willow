//! Foreign-call admission; only explicitly reviewed bounded calls admit tasks.
pub(crate) mod aggregate;
#[cfg(test)]
mod handles;
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
/// Generated only for declarations whose review fingerprint the build checked.
/// The assertion covers bounded running time, native stack use, and no callbacks
/// into Willow. The body retains its GC idle region and panic boundary.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_rust_bridge_enter_nonblocking(revision: u32) {
    if revision != willow_abi::ffi::WILLOW_RUST_BRIDGE_ABI_REVISION {
        crate::failure::fatal_invariant("rust_bridge_abi_mismatch");
    }
}

/// Enter the GC safe region around user Rust code, which can block for any
/// length of time. Adapters decode arguments first and encode results after:
/// the foreign body sees only native values, pinned strings and handle leases.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_rust_bridge_region_enter() -> u32 {
    // SAFETY: the adapter contract above keeps managed state out of the region.
    u32::from(unsafe { crate::gc::IdleMutator::enter() }.into_raw())
}

/// Leave the region; parks here while a stop is active, including on unwind.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_rust_bridge_region_exit(region: u32) {
    // SAFETY: `region` is the value this thread's matching enter returned.
    drop(unsafe { crate::gc::IdleMutator::from_raw(region != 0) });
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
        if let Ok(kind) = std::env::var(KEY) {
            let enter = if kind == "bounded" {
                willow_rust_bridge_enter_nonblocking
            } else {
                willow_rust_bridge_enter
            };
            enter(willow_abi::ffi::WILLOW_RUST_BRIDGE_ABI_REVISION + 1);
            panic!("REVISION_GATE_RETURNED");
        }
        for kind in ["legacy", "bounded"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "rust_bridge_abi::tests::incompatible_revision_stops_before_foreign_execution",
                    "--nocapture",
                ])
                .env(KEY, kind)
                .output()
                .unwrap();
            assert!(!output.status.success());
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("rust_bridge_abi_mismatch"), "{stderr}");
            assert!(!stderr.contains("REVISION_GATE_RETURNED"), "{stderr}");
        }
    }
    #[test]
    fn synchronous_current_revision_enters_without_side_effects() {
        for _ in 0..1024 {
            willow_rust_bridge_enter(willow_abi::ffi::WILLOW_RUST_BRIDGE_ABI_REVISION);
        }
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_rust_bridge_handle_error(closed: u32, id: u64) -> ! {
    let kind = if closed != 0 {
        "rust_handle_closed"
    } else {
        "rust_handle_invalid"
    };
    crate::failure::fatal_invariant(&format!(
        "{kind}: id={id:#018x} slot={} generation={}",
        id as u32,
        id >> 32
    ));
}
