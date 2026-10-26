//! GC conditions avoid OS wake calls when no thread is waiting. Registration
//! precedes the atomic unlock-and-wait; notifiers hold the same predicate mutex,
//! so they cannot miss a waiter between its predicate check and registration.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, LockResult, MutexGuard, WaitTimeoutResult};
use std::time::Duration;

#[derive(Default)]
pub(crate) struct GcCondvar {
    inner: Condvar,
    waiters: AtomicUsize,
    #[cfg(test)]
    notifications: AtomicUsize,
}

struct Waiter<'a>(&'a AtomicUsize);
impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl GcCondvar {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn notifications(&self) -> usize {
        self.notifications.load(Ordering::Relaxed)
    }

    fn register(&self) -> Waiter<'_> {
        self.waiters.fetch_add(1, Ordering::Relaxed);
        Waiter(&self.waiters)
    }

    pub(crate) fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> LockResult<MutexGuard<'a, T>> {
        let _waiter = self.register();
        self.inner.wait(guard)
    }

    pub(crate) fn wait_timeout<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        duration: Duration,
    ) -> LockResult<(MutexGuard<'a, T>, WaitTimeoutResult)> {
        let _waiter = self.register();
        self.inner.wait_timeout(guard, duration)
    }

    /// Call while holding the mutex used by wait/wait_timeout. The mutex orders
    /// registration and this check, so the count needs no independent fence.
    pub(crate) fn notify_all(&self) {
        if self.waiters.load(Ordering::Relaxed) != 0 {
            #[cfg(test)]
            self.notifications.fetch_add(1, Ordering::Relaxed);
            self.inner.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn empty_gc_conditions_emit_no_notifications_at_increasing_counts() {
        let cv = GcCondvar::new();
        let lock = Mutex::new(());
        for calls in [1, 64, 4096] {
            let _guard = lock.lock().unwrap();
            for _ in 0..calls {
                cv.notify_all();
            }
            assert_eq!(cv.notifications.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn gc_condition_notifies_registered_waiters_and_balances_timeouts() {
        for count in [1, 4, 16] {
            let cv = GcCondvar::new();
            let lock = Mutex::new(false);
            std::thread::scope(|scope| {
                for _ in 0..count {
                    scope.spawn(|| {
                        let mut done = lock.lock().unwrap();
                        while !*done {
                            done = cv.wait(done).unwrap();
                        }
                    });
                }
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                loop {
                    let mut done = lock.lock().unwrap();
                    if cv.waiters.load(Ordering::Relaxed) == count {
                        *done = true;
                        cv.notify_all();
                        break;
                    }
                    drop(done);
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::yield_now();
                }
            });
            assert_eq!(cv.waiters.load(Ordering::Relaxed), 0);
            assert_eq!(cv.notifications.load(Ordering::Relaxed), 1);
            let (guard, result) = cv
                .wait_timeout(lock.lock().unwrap(), Duration::ZERO)
                .unwrap();
            assert!(result.timed_out());
            assert_eq!(cv.waiters.load(Ordering::Relaxed), 0);
            cv.notify_all();
            assert_eq!(cv.notifications.load(Ordering::Relaxed), 1);
            drop(guard);
        }
    }

    #[test]
    fn gc_condition_balances_poisoned_wait_return() {
        let cv = GcCondvar::new();
        let lock = Mutex::new(());
        let guard = lock.lock().unwrap();
        std::thread::scope(|scope| {
            let worker = scope.spawn(|| {
                let _guard = lock.lock().unwrap();
                cv.notify_all();
                panic!("poisoned predicate");
            });
            let mut waiting = guard;
            while let Ok(next) = cv.wait(waiting) {
                waiting = next;
            }
            assert!(worker.join().is_err());
        });
        assert_eq!(cv.waiters.load(Ordering::Relaxed), 0);
    }
}
