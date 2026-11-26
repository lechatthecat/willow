//! Opt-in integration-test clock. Normal builds contain none of this state or ABI.
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);
static MILLIS: AtomicU64 = AtomicU64::new(u64::MAX);

pub(crate) fn now() -> Instant {
    match MILLIS.load(Ordering::Acquire) {
        u64::MAX => Instant::now(),
        millis => *EPOCH + Duration::from_millis(millis),
    }
}

pub(crate) fn elapsed() -> Duration {
    let epoch = *EPOCH;
    now().saturating_duration_since(epoch)
}

/// Advance only when the fixture has observed the relevant registered wait.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_test_clock_set_millis(millis: i64) {
    assert!(millis >= 0);
    let _ = *EPOCH;
    let old = MILLIS.load(Ordering::Acquire);
    assert!(
        old == u64::MAX || millis as u64 >= old,
        "test clock regressed"
    );
    MILLIS.store(millis as u64, Ordering::Release);
    crate::scheduler::test_clock_advanced();
}

static SYNC_SEND_WAITS: AtomicU64 = AtomicU64::new(0);
static SYNC_RECV_WAITS: AtomicU64 = AtomicU64::new(0);
pub(crate) fn sync_send_wait() {
    SYNC_SEND_WAITS.fetch_add(1, Ordering::Release);
}
pub(crate) fn sync_recv_wait() {
    SYNC_RECV_WAITS.fetch_add(1, Ordering::Release);
}
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_test_sync_send_waits() -> i64 {
    SYNC_SEND_WAITS.load(Ordering::Acquire) as i64
}
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_test_sync_recv_waits() -> i64 {
    SYNC_RECV_WAITS.load(Ordering::Acquire) as i64
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_test_frames_reclaimed() -> i64 {
    crate::frame_lifetime::snapshot().task_frames_reclaimed as i64
}
