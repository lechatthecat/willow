//! Foreign-owner deferral diagnostics, independent of stable telemetry layouts.
use super::*;

/// A skip is a safe deferral, not a successful cycle. Reasons distinguish the
/// early check from a registration transition detected at the stopped boundary.
#[repr(u64)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GcSkipReason {
    #[default]
    None = 0,
    ForeignOwner = 1,
    ForeignOwnerAtHandshake = 2,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GcProgressV1 {
    pub skipped_collections: u64,
    pub last_skip_reason: u64,
    /// Consecutive skip interval; zero once any collection succeeds.
    pub skip_duration_ns: u64,
    /// Since last successful major OR minor collection (initialization if none).
    pub progress_gap_ns: u64,
    pub allocation_bytes_since_success: u64,
    pub successful_collections: u64,
}

pub(super) struct Progress {
    last_success_ns: u64,
    allocated_at_success: u64,
    skip_started: Option<u64>,
    last_reason: GcSkipReason,
    successes: u64,
}
impl Default for Progress {
    fn default() -> Self {
        Self {
            last_success_ns: crate::gc_telemetry::timestamp_ns(),
            allocated_at_success: 0,
            skip_started: None,
            last_reason: GcSkipReason::None,
            successes: 0,
        }
    }
}

pub(super) fn record_skip(owner: &GcRuntime, reason: GcSkipReason) {
    let mut progress = owner.progress.lock().unwrap();
    owner
        .skipped_foreign_owner_collections
        .fetch_add(1, Ordering::Relaxed);
    progress
        .skip_started
        .get_or_insert_with(crate::gc_telemetry::timestamp_ns);
    progress.last_reason = reason;
}

// Caller has synchronized allocation accounting under heap. No scans added.
pub(super) fn record_success(owner: &GcRuntime, allocated: u64) {
    let mut progress = owner.progress.lock().unwrap();
    progress.last_success_ns = crate::gc_telemetry::timestamp_ns();
    progress.allocated_at_success = allocated;
    progress.skip_started = None;
    progress.successes = progress.successes.saturating_add(1);
}

/// Coherent snapshot. Allocation accounting merges TLAB deltas once; this
/// diagnostic is O(TLAB records + old allocation buffers), never on the allocation hot path.
pub fn gc_progress() -> GcProgressV1 {
    let owner = runtime();
    let mut heap = owner.heap.lock().unwrap();
    sync_tlab_bytes(&mut heap);
    let progress = owner.progress.lock().unwrap();
    let now = crate::gc_telemetry::timestamp_ns();
    GcProgressV1 {
        skipped_collections: owner
            .skipped_foreign_owner_collections
            .load(Ordering::Relaxed),
        last_skip_reason: progress.last_reason as u64,
        skip_duration_ns: progress
            .skip_started
            .map_or(0, |start| now.saturating_sub(start)),
        progress_gap_ns: now.saturating_sub(progress.last_success_ns),
        allocation_bytes_since_success: heap
            .total_allocated_bytes
            .saturating_sub(progress.allocated_at_success),
        successful_collections: progress.successes,
    }
}

/// Copy a V1 snapshot to caller-owned storage. Null returns false.
/// # Safety
/// A non-null output must be writable and aligned for GcProgressV1.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_gc_progress_v1(output: *mut GcProgressV1) -> bool {
    if output.is_null() {
        return false;
    }
    unsafe {
        output.write(gc_progress());
    }
    true
}

pub(super) fn reset(owner: &GcRuntime) {
    *owner.progress.lock().unwrap() = Progress::default();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_owner_progress_epoch_precedes_first_query() {
        let owner = GcRuntime::default();
        let before_query = crate::gc_telemetry::timestamp_ns();
        assert!(owner.progress.lock().unwrap().last_success_ns <= before_query);
    }

    #[test]
    fn private_collectors_keep_progress_and_reset_independent() {
        let first = GcRuntime::default();
        let second = GcRuntime::default();
        std::thread::scope(|scope| {
            for (owner, count) in [(&first, 64), (&second, 192)] {
                scope.spawn(crate::runtime::serving(move || {
                    for allocated in 1..=count {
                        record_skip(owner, GcSkipReason::ForeignOwner);
                        record_success(owner, allocated);
                    }
                    record_skip(owner, GcSkipReason::ForeignOwnerAtHandshake);
                }));
            }
        });
        for (owner, count) in [(&first, 64), (&second, 192)] {
            let progress = owner.progress.lock().unwrap();
            assert_eq!(progress.successes, count);
            assert_eq!(progress.allocated_at_success, count);
            assert!(progress.skip_started.is_some());
            assert_eq!(progress.last_reason, GcSkipReason::ForeignOwnerAtHandshake);
            assert_eq!(
                owner
                    .skipped_foreign_owner_collections
                    .load(Ordering::Relaxed),
                count + 1
            );
        }
        reset(&first);
        assert_eq!(first.progress.lock().unwrap().successes, 0);
        assert_eq!(second.progress.lock().unwrap().successes, 192);
    }
}
