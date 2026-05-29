use std::ffi::c_void;
use std::time::Duration;

use crate::future::Poll;
use crate::scheduler::RuntimeScheduler;
use crate::task::{RuntimeTaskId, RuntimeTaskState};
use crate::timer::RuntimeSleepFuture;
use crate::trace::{GcRootSet, GcTrace, GcVisitor};

#[derive(Debug)]
pub struct TimerWaiter {
    task_id: RuntimeTaskId,
    future: RuntimeSleepFuture,
}

impl TimerWaiter {
    fn new(task_id: RuntimeTaskId, ms: i64) -> Self {
        Self {
            task_id,
            future: RuntimeSleepFuture::after_millis(ms),
        }
    }

    pub fn task_id(&self) -> RuntimeTaskId {
        self.task_id
    }

    pub fn roots(&self) -> &GcRootSet {
        self.future.roots()
    }

    pub fn roots_mut(&mut self) -> &mut GcRootSet {
        self.future.roots_mut()
    }

    pub fn remaining(&self) -> Option<Duration> {
        self.future.remaining()
    }
}

impl GcTrace for TimerWaiter {
    fn trace(&self, visitor: &mut GcVisitor) {
        self.future.trace(visitor);
    }
}

#[derive(Debug, Default)]
pub struct RuntimeExecutor {
    scheduler: RuntimeScheduler,
    timer_waiters: Vec<TimerWaiter>,
}

impl RuntimeExecutor {
    pub fn scheduler(&self) -> &RuntimeScheduler {
        &self.scheduler
    }

    pub fn scheduler_mut(&mut self) -> &mut RuntimeScheduler {
        &mut self.scheduler
    }

    pub fn spawn_placeholder(&mut self) -> RuntimeTaskId {
        self.scheduler.spawn_placeholder()
    }

    pub fn sleep(&mut self, ms: i64) -> RuntimeTaskId {
        let task_id = self.scheduler.spawn_parked_placeholder();
        self.timer_waiters.push(TimerWaiter::new(task_id, ms));
        task_id
    }

    pub fn timer_waiter_count(&self) -> usize {
        self.timer_waiters.len()
    }

    pub fn timer_waiters(&self) -> &[TimerWaiter] {
        &self.timer_waiters
    }

    pub fn timer_waiter_mut(&mut self, task_id: RuntimeTaskId) -> Option<&mut TimerWaiter> {
        self.timer_waiters
            .iter_mut()
            .find(|waiter| waiter.task_id == task_id)
    }

    pub fn poll_timers(&mut self) -> usize {
        let mut ready_task_ids = Vec::new();
        self.timer_waiters
            .retain_mut(|waiter| match waiter.future.poll() {
                Poll::Ready(()) => {
                    ready_task_ids.push(waiter.task_id);
                    false
                }
                Poll::Pending => true,
            });
        let ready = ready_task_ids.len();
        for task_id in ready_task_ids {
            self.scheduler.wake(task_id);
        }
        ready
    }

    pub fn next_timer_remaining(&self) -> Option<Duration> {
        self.timer_waiters
            .iter()
            .filter_map(TimerWaiter::remaining)
            .min()
    }

    pub fn block_on_sleep(&mut self, ms: i64) -> usize {
        self.sleep(ms);
        let mut completed = 0;
        while self.timer_waiter_count() > 0 {
            completed += self.run_until_idle();
            if self.timer_waiter_count() == 0 {
                break;
            }

            match self.next_timer_remaining() {
                Some(remaining) if remaining > Duration::ZERO => {
                    std::thread::sleep(remaining.min(Duration::from_millis(10)));
                }
                _ => std::thread::yield_now(),
            }
        }
        completed
    }

    pub fn run_until_idle(&mut self) -> usize {
        let mut completed = 0;
        loop {
            self.poll_timers();
            let mut made_progress = false;
            while let Some(task_id) = self.scheduler.pop_ready() {
                if let Some(task) = self.scheduler.task_mut(task_id) {
                    task.state = RuntimeTaskState::Running;
                    task.complete();
                    completed += 1;
                    made_progress = true;
                }
            }
            if !made_progress {
                break;
            }
        }
        completed
    }
}

fn executor_from_raw(raw: *mut c_void) -> Option<&'static mut RuntimeExecutor> {
    if raw.is_null() {
        None
    } else {
        Some(unsafe { &mut *(raw as *mut RuntimeExecutor) })
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_executor_new() -> *mut c_void {
    Box::into_raw(Box::new(RuntimeExecutor::default())) as *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_executor_free(raw: *mut c_void) {
    if !raw.is_null() {
        unsafe { drop(Box::from_raw(raw as *mut RuntimeExecutor)) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_executor_sleep(raw: *mut c_void, ms: i64) -> u64 {
    executor_from_raw(raw)
        .map(|executor| executor.sleep(ms))
        .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_executor_poll_timers(raw: *mut c_void) -> i64 {
    executor_from_raw(raw)
        .map(|executor| executor.poll_timers() as i64)
        .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_executor_run_until_idle(raw: *mut c_void) -> i64 {
    executor_from_raw(raw)
        .map(|executor| executor.run_until_idle() as i64)
        .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_executor_block_on_sleep(raw: *mut c_void, ms: i64) -> i64 {
    executor_from_raw(raw)
        .map(|executor| executor.block_on_sleep(ms) as i64)
        .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_executor_timer_waiter_count(raw: *mut c_void) -> i64 {
    executor_from_raw(raw)
        .map(|executor| executor.timer_waiter_count() as i64)
        .unwrap_or(0)
}

impl GcTrace for RuntimeExecutor {
    fn trace(&self, visitor: &mut GcVisitor) {
        self.scheduler.trace(visitor);
        for waiter in &self.timer_waiters {
            waiter.trace(visitor);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executor_drains_ready_tasks() {
        let mut executor = RuntimeExecutor::default();
        executor.spawn_placeholder();
        executor.spawn_placeholder();
        assert_eq!(executor.run_until_idle(), 2);
    }

    #[test]
    fn executor_unit_01_starts_without_timer_waiters() {
        let executor = RuntimeExecutor::default();
        assert_eq!(executor.timer_waiter_count(), 0);
    }

    #[test]
    fn executor_unit_02_sleep_registers_timer_waiter() {
        let mut executor = RuntimeExecutor::default();
        executor.sleep(100);
        assert_eq!(executor.timer_waiter_count(), 1);
    }

    #[test]
    fn executor_unit_03_sleep_parks_task_until_timer_ready() {
        let mut executor = RuntimeExecutor::default();
        let task_id = executor.sleep(100);
        assert_eq!(
            executor.scheduler().task_state(task_id),
            Some(RuntimeTaskState::Parked)
        );
    }

    #[test]
    fn executor_unit_04_zero_sleep_polls_ready() {
        let mut executor = RuntimeExecutor::default();
        let task_id = executor.sleep(0);
        assert_eq!(executor.poll_timers(), 1);
        assert_eq!(
            executor.scheduler().task_state(task_id),
            Some(RuntimeTaskState::Ready)
        );
    }

    #[test]
    fn executor_unit_05_negative_sleep_polls_ready() {
        let mut executor = RuntimeExecutor::default();
        let task_id = executor.sleep(-1);
        assert_eq!(executor.poll_timers(), 1);
        assert_eq!(
            executor.scheduler().task_state(task_id),
            Some(RuntimeTaskState::Ready)
        );
    }

    #[test]
    fn executor_unit_06_positive_sleep_remains_pending_before_deadline() {
        let mut executor = RuntimeExecutor::default();
        let task_id = executor.sleep(100);
        assert_eq!(executor.poll_timers(), 0);
        assert_eq!(
            executor.scheduler().task_state(task_id),
            Some(RuntimeTaskState::Parked)
        );
    }

    #[test]
    fn executor_unit_07_run_until_idle_completes_ready_timer_task() {
        let mut executor = RuntimeExecutor::default();
        let task_id = executor.sleep(0);
        assert_eq!(executor.run_until_idle(), 1);
        assert_eq!(
            executor.scheduler().task_state(task_id),
            Some(RuntimeTaskState::Completed)
        );
    }

    #[test]
    fn executor_unit_08_run_until_idle_leaves_pending_timer_waiter() {
        let mut executor = RuntimeExecutor::default();
        executor.sleep(100);
        assert_eq!(executor.run_until_idle(), 0);
        assert_eq!(executor.timer_waiter_count(), 1);
    }

    #[test]
    fn executor_unit_09_multiple_zero_timers_complete_fifo_set() {
        let mut executor = RuntimeExecutor::default();
        let first = executor.sleep(0);
        let second = executor.sleep(0);
        assert_eq!(executor.run_until_idle(), 2);
        assert_eq!(
            executor.scheduler().task_state(first),
            Some(RuntimeTaskState::Completed)
        );
        assert_eq!(
            executor.scheduler().task_state(second),
            Some(RuntimeTaskState::Completed)
        );
    }

    #[test]
    fn executor_unit_10_timer_waiter_roots_are_traced() {
        let mut executor = RuntimeExecutor::default();
        let task_id = executor.sleep(100);
        executor
            .timer_waiter_mut(task_id)
            .unwrap()
            .roots_mut()
            .push(42);
        let mut visitor = GcVisitor::default();
        executor.trace(&mut visitor);
        assert_eq!(visitor.roots(), &[42]);
    }

    #[test]
    fn executor_unit_11_scheduler_roots_and_timer_roots_are_traced() {
        let mut executor = RuntimeExecutor::default();
        let ready = executor.spawn_placeholder();
        executor
            .scheduler_mut()
            .task_mut(ready)
            .unwrap()
            .roots
            .push(7);
        let sleeping = executor.sleep(100);
        executor
            .timer_waiter_mut(sleeping)
            .unwrap()
            .roots_mut()
            .push(9);
        let mut visitor = GcVisitor::default();
        executor.trace(&mut visitor);
        let mut roots = visitor.into_roots();
        roots.sort_unstable();
        assert_eq!(roots, vec![7, 9]);
    }

    #[test]
    fn executor_unit_12_poll_timers_removes_ready_waiter() {
        let mut executor = RuntimeExecutor::default();
        executor.sleep(0);
        assert_eq!(executor.timer_waiter_count(), 1);
        executor.poll_timers();
        assert_eq!(executor.timer_waiter_count(), 0);
    }

    #[test]
    fn executor_unit_13_sleep_does_not_consume_existing_ready_task() {
        let mut executor = RuntimeExecutor::default();
        let ready = executor.spawn_placeholder();
        executor.sleep(100);
        assert_eq!(executor.scheduler().ready_len(), 1);
        assert_eq!(executor.scheduler_mut().pop_ready(), Some(ready));
    }

    #[test]
    fn executor_unit_14_next_timer_remaining_reports_pending_timer() {
        let mut executor = RuntimeExecutor::default();
        executor.sleep(50);
        assert!(executor.next_timer_remaining().is_some());
    }

    #[test]
    fn executor_unit_15_next_timer_remaining_is_none_without_waiters() {
        let executor = RuntimeExecutor::default();
        assert_eq!(executor.next_timer_remaining(), None);
    }

    #[test]
    fn executor_unit_16_block_on_zero_sleep_completes_task() {
        let mut executor = RuntimeExecutor::default();
        assert_eq!(executor.block_on_sleep(0), 1);
        assert_eq!(executor.timer_waiter_count(), 0);
    }

    #[test]
    fn executor_unit_17_block_on_negative_sleep_completes_task() {
        let mut executor = RuntimeExecutor::default();
        assert_eq!(executor.block_on_sleep(-1), 1);
        assert_eq!(executor.timer_waiter_count(), 0);
    }

    #[test]
    fn executor_unit_18_abi_new_starts_empty() {
        let raw = willow_executor_new();
        assert_eq!(willow_executor_timer_waiter_count(raw), 0);
        willow_executor_free(raw);
    }

    #[test]
    fn executor_unit_19_abi_sleep_registers_waiter() {
        let raw = willow_executor_new();
        let task_id = willow_executor_sleep(raw, 100);
        assert_eq!(task_id, 0);
        assert_eq!(willow_executor_timer_waiter_count(raw), 1);
        willow_executor_free(raw);
    }

    #[test]
    fn executor_unit_20_abi_poll_timers_wakes_zero_sleep() {
        let raw = willow_executor_new();
        willow_executor_sleep(raw, 0);
        assert_eq!(willow_executor_poll_timers(raw), 1);
        assert_eq!(willow_executor_timer_waiter_count(raw), 0);
        willow_executor_free(raw);
    }

    #[test]
    fn executor_unit_21_abi_run_until_idle_completes_zero_sleep() {
        let raw = willow_executor_new();
        willow_executor_sleep(raw, 0);
        assert_eq!(willow_executor_run_until_idle(raw), 1);
        willow_executor_free(raw);
    }

    #[test]
    fn executor_unit_22_abi_block_on_sleep_uses_executor_path() {
        let raw = willow_executor_new();
        assert_eq!(willow_executor_block_on_sleep(raw, 0), 1);
        assert_eq!(willow_executor_timer_waiter_count(raw), 0);
        willow_executor_free(raw);
    }

    #[test]
    fn executor_unit_23_null_executor_abi_calls_are_noops() {
        let raw = std::ptr::null_mut();
        assert_eq!(willow_executor_sleep(raw, 0), 0);
        assert_eq!(willow_executor_poll_timers(raw), 0);
        assert_eq!(willow_executor_run_until_idle(raw), 0);
        assert_eq!(willow_executor_block_on_sleep(raw, 0), 0);
        assert_eq!(willow_executor_timer_waiter_count(raw), 0);
    }
}
