use std::time::{Duration, Instant};

use crate::future;
use crate::future::Poll;
use crate::trace::{GcRootSet, GcTrace, GcVisitor};

#[derive(Debug, Clone)]
pub struct RuntimeTimer {
    deadline: Instant,
}

impl RuntimeTimer {
    pub fn after_millis(ms: i64) -> Self {
        let millis = ms.max(0) as u64;
        Self {
            deadline: Instant::now() + Duration::from_millis(millis),
        }
    }

    pub fn is_ready(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub fn remaining(&self) -> Option<Duration> {
        self.deadline.checked_duration_since(Instant::now())
    }
}

impl GcTrace for RuntimeTimer {
    fn trace(&self, _visitor: &mut GcVisitor) {}
}

#[derive(Debug, Clone)]
pub struct RuntimeSleepFuture {
    timer: RuntimeTimer,
    roots: GcRootSet,
    completed: bool,
}

impl RuntimeSleepFuture {
    pub fn after_millis(ms: i64) -> Self {
        Self {
            timer: RuntimeTimer::after_millis(ms),
            roots: GcRootSet::default(),
            completed: false,
        }
    }

    pub fn roots(&self) -> &GcRootSet {
        &self.roots
    }

    pub fn roots_mut(&mut self) -> &mut GcRootSet {
        &mut self.roots
    }

    pub fn is_completed(&self) -> bool {
        self.completed
    }

    pub fn remaining(&self) -> Option<Duration> {
        self.timer.remaining()
    }

    pub fn poll(&mut self) -> Poll<()> {
        if self.completed || self.timer.is_ready() {
            self.completed = true;
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl GcTrace for RuntimeSleepFuture {
    fn trace(&self, visitor: &mut GcVisitor) {
        self.roots.trace(visitor);
    }
}

// Generated Future<void> values are monotonic millisecond deadlines, not
// pointers. Zero denotes ready. Copies own no native storage, so aliases,
// returns, discarded values, panic recovery and cancellation need no cleanup.
static VALUE_EPOCH: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

/// Make an allocation-free sleep value. Round the deadline up: millisecond
/// quantization must never shorten the requested delay. u64 accommodates every
/// nonnegative i64 duration plus process uptime (saturating at the ABI limit).
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_timer_value_sleep(ms: i64) -> u64 {
    if ms <= 0 {
        return 0;
    }
    let elapsed = VALUE_EPOCH.elapsed();
    let now = elapsed.as_millis().min(u64::MAX as u128) as u64;
    now.saturating_add(u64::from(!elapsed.subsec_nanos().is_multiple_of(1_000_000)))
        .saturating_add(ms as u64)
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_timer_value_yield() -> u64 {
    0
}

/// Await a copied deadline through the same scheduler-aware wait as native
/// futures. Bounded chunks avoid Instant overflow even for i64::MAX durations.
/// Native-stack cancellation ends the wait; generated call checks handle exit.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_timer_value_await(deadline: u64) -> u8 {
    loop {
        let now = VALUE_EPOCH.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let remaining = deadline.saturating_sub(now);
        if remaining == 0 {
            return 0;
        }
        let wait = future::WillowFutureVoid::sleep_after_millis(remaining.min(86_400_000) as i64);
        if !wait.block_until_ready() {
            return 0;
        }
    }
}

/// Returns a WillowFutureVoid that becomes ready after `ms` milliseconds.
/// Non-blocking: does not sleep the calling thread.
/// Use willow_future_is_ready_void to poll, willow_future_await_void to block.
/// The caller owns the handle and must release it with willow_future_release_void.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_runtime_sleep(ms: i64) -> *mut std::ffi::c_void {
    future::void_future_into_raw_pub(future::WillowFutureVoid::sleep_after_millis(ms))
}

/// Returns a ready void future for non-cooperative `yield()` expressions. The
/// scheduler-aware yield path is `await yield()`, lowered to `willow_sched_yield`.
/// The caller owns the handle and must release it with willow_future_release_void.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_runtime_yield() -> *mut std::ffi::c_void {
    future::void_future_into_raw_pub(future::WillowFutureVoid::ready())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_values_allocate_no_native_storage_at_increasing_sizes() {
        use crate::scheduler::scaling_measurements::counting_allocator as counter;
        // Initialize the epoch and TLS counters outside measurement.
        let _ = willow_timer_value_sleep(1);
        for count in [1, 16, 256, 4096] {
            let before = counter::thread_allocations();
            for _ in 0..count {
                std::hint::black_box(willow_timer_value_sleep(60_000));
                let ready = willow_timer_value_yield();
                assert_eq!(willow_timer_value_await(ready), 0);
                assert_eq!(willow_timer_value_await(ready), 0);
            }
            let allocations = counter::thread_allocations() - before;
            assert_eq!(allocations, 0);
            println!("future-values n={count} native_allocations={allocations}");
        }
    }

    #[test]
    fn timer_values_preserve_deadlines_and_copies() {
        for ms in [i64::MIN, -1, 0] {
            assert_eq!(willow_timer_value_sleep(ms), 0);
        }
        assert_eq!(willow_timer_value_yield(), 0);
        let before = Instant::now();
        let value = willow_timer_value_sleep(3);
        let alias = value;
        assert_eq!(willow_timer_value_await(alias), 0);
        assert!(before.elapsed() >= Duration::from_millis(3));
        assert_eq!(willow_timer_value_await(value), 0);
        let huge = willow_timer_value_sleep(i64::MAX);
        assert!(huge >= i64::MAX as u64);
    }

    fn await_and_release(raw: *mut std::ffi::c_void) -> u8 {
        let result = future::willow_future_await_void(raw);
        unsafe { future::willow_future_release_void(raw) };
        result
    }

    #[test]
    fn zero_timer_is_ready_immediately() {
        assert!(RuntimeTimer::after_millis(0).is_ready());
    }

    #[test]
    fn timer_unit_01_negative_sleep_returns_ready_without_panic() {
        assert_eq!(await_and_release(willow_runtime_sleep(-1)), 0);
    }

    #[test]
    fn timer_unit_02_zero_sleep_returns_ready_without_panic() {
        assert_eq!(await_and_release(willow_runtime_sleep(0)), 0);
    }

    #[test]
    fn timer_unit_03_negative_timer_is_ready_immediately() {
        assert!(RuntimeTimer::after_millis(-1).is_ready());
    }

    #[test]
    fn timer_unit_04_positive_timer_reports_remaining_duration() {
        let timer = RuntimeTimer::after_millis(50);
        assert!(timer.remaining().is_some());
    }

    #[test]
    fn timer_unit_05_sleep_future_zero_polls_ready() {
        let mut future = RuntimeSleepFuture::after_millis(0);
        assert_eq!(future.poll(), Poll::Ready(()));
        assert!(future.is_completed());
    }

    #[test]
    fn timer_unit_06_sleep_future_negative_polls_ready() {
        let mut future = RuntimeSleepFuture::after_millis(-10);
        assert_eq!(future.poll(), Poll::Ready(()));
    }

    #[test]
    fn timer_unit_07_sleep_future_positive_starts_pending() {
        let mut future = RuntimeSleepFuture::after_millis(50);
        assert_eq!(future.poll(), Poll::Pending);
        assert!(!future.is_completed());
    }

    #[test]
    fn timer_unit_08_sleep_future_ready_is_idempotent() {
        let mut future = RuntimeSleepFuture::after_millis(0);
        assert_eq!(future.poll(), Poll::Ready(()));
        assert_eq!(future.poll(), Poll::Ready(()));
    }

    #[test]
    fn timer_unit_09_sleep_future_traces_roots() {
        let mut future = RuntimeSleepFuture::after_millis(0);
        future.roots_mut().push(11);
        future.roots_mut().push(22);
        let mut visitor = GcVisitor::default();
        future.trace(&mut visitor);
        assert_eq!(visitor.roots(), &[11, 22]);
    }

    #[test]
    fn timer_unit_10_sleep_future_roots_start_empty() {
        let future = RuntimeSleepFuture::after_millis(0);
        assert!(future.roots().is_empty());
    }

    #[test]
    fn timer_unit_11_sleep_future_reports_remaining_duration() {
        let future = RuntimeSleepFuture::after_millis(50);
        assert!(future.remaining().is_some());
    }

    #[test]
    fn timer_unit_12_runtime_sleep_uses_executor_path() {
        assert_eq!(await_and_release(willow_runtime_sleep(0)), 0);
        assert_eq!(await_and_release(willow_runtime_sleep(1)), 0);
    }

    #[test]
    fn timer_unit_13_runtime_yield_returns_ready_void_future() {
        assert_eq!(await_and_release(willow_runtime_yield()), 0);
    }
}
