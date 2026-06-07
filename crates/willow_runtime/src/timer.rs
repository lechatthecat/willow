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

/// Returns a WillowFutureVoid that becomes ready after `ms` milliseconds.
/// Non-blocking: does not sleep the calling thread.
/// Use willow_future_is_ready_void to poll, willow_future_await_void to block.
#[unsafe(no_mangle)]
pub extern "C" fn willow_runtime_sleep(ms: i64) -> *mut std::ffi::c_void {
    future::void_future_into_raw_pub(future::WillowFutureVoid::sleep_after_millis(ms))
}

/// Returns a ready void future for non-cooperative `yield()` expressions. The
/// scheduler-aware yield path is `await yield()`, lowered to `willow_sched_yield`.
#[unsafe(no_mangle)]
pub extern "C" fn willow_runtime_yield() -> *mut std::ffi::c_void {
    future::void_future_into_raw_pub(future::WillowFutureVoid::ready())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_timer_is_ready_immediately() {
        assert!(RuntimeTimer::after_millis(0).is_ready());
    }

    #[test]
    fn timer_unit_01_negative_sleep_returns_ready_without_panic() {
        assert_eq!(
            future::willow_future_await_void(willow_runtime_sleep(-1)),
            0
        );
    }

    #[test]
    fn timer_unit_02_zero_sleep_returns_ready_without_panic() {
        assert_eq!(future::willow_future_await_void(willow_runtime_sleep(0)), 0);
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
        assert_eq!(future::willow_future_await_void(willow_runtime_sleep(0)), 0);
        assert_eq!(future::willow_future_await_void(willow_runtime_sleep(1)), 0);
    }

    #[test]
    fn timer_unit_13_runtime_yield_returns_ready_void_future() {
        assert_eq!(future::willow_future_await_void(willow_runtime_yield()), 0);
    }
}
