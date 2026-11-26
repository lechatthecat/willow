//! Test-only waits for an observed protocol state. Time is only a hang guard.
use std::time::{Duration, Instant};

pub(crate) fn until(message: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(Instant::now() < deadline, "{message}");
        std::thread::yield_now();
    }
}

static POINTS: crate::runtime::PerRuntime<
    std::sync::Mutex<std::collections::HashMap<&'static str, usize>>,
> = crate::runtime::PerRuntime::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

pub(crate) fn reached(point: &'static str) {
    *POINTS.lock().unwrap().entry(point).or_default() += 1;
}

pub(crate) fn count(point: &'static str) -> usize {
    POINTS.lock().unwrap().get(point).copied().unwrap_or(0)
}

/// Signal a *failed acquisition*, rather than guessing that a caller ran.
pub(crate) fn lock<'a, T>(
    mutex: &'a std::sync::Mutex<T>,
    point: &'static str,
) -> std::sync::LockResult<std::sync::MutexGuard<'a, T>> {
    match mutex.try_lock() {
        Ok(guard) => Ok(guard),
        Err(std::sync::TryLockError::Poisoned(error)) => Err(error),
        Err(std::sync::TryLockError::WouldBlock) => {
            reached(point);
            mutex.lock()
        }
    }
}

/// A timeout that can actually stop a wedged native scheduler test.
/// Successful tests drop the guard immediately; elapsed time never selects success.
pub(crate) struct Watchdog {
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Watchdog {
    pub(crate) fn new(label: &'static str) -> Self {
        let (stop, done) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            if done.recv_timeout(Duration::from_secs(60)).is_err() {
                eprintln!("test hung: {label}");
                std::process::abort();
            }
        });
        Self {
            stop: Some(stop),
            thread: Some(thread),
        }
    }
}
impl Drop for Watchdog {
    fn drop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

static SCHEDULER_TIME: crate::runtime::PerRuntime<std::sync::Mutex<Option<(Instant, Instant)>>> =
    crate::runtime::PerRuntime::new(|| std::sync::Mutex::new(None));
pub(crate) fn scheduler_time() -> Option<Instant> {
    SCHEDULER_TIME.lock().unwrap().map(|(_, now)| now)
}
pub(crate) fn scheduler_elapsed() -> Option<Duration> {
    SCHEDULER_TIME
        .lock()
        .unwrap()
        .map(|(start, now)| now.duration_since(start))
}
pub(crate) struct SchedulerClock;
impl SchedulerClock {
    pub(crate) fn new() -> Self {
        let _ = crate::scheduler::willow_monotonic_millis();
        let now = Instant::now();
        assert!(SCHEDULER_TIME.lock().unwrap().replace((now, now)).is_none());
        Self
    }
    pub(crate) fn advance(&self, by: Duration) {
        SCHEDULER_TIME.lock().unwrap().as_mut().unwrap().1 += by;
        crate::scheduler::test_clock_advanced();
    }
}
impl Drop for SchedulerClock {
    fn drop(&mut self) {
        *SCHEDULER_TIME.lock().unwrap() = None;
    }
}

/// Drive until a protocol observation, advancing the artificial stop deadline
/// only after that observation. The real deadline in `until` only fails hangs.
pub(crate) fn drive_until(
    message: &str,
    clock: &SchedulerClock,
    mut ready: impl FnMut() -> bool + Send,
) -> i64 {
    let deadline = crate::scheduler::willow_monotonic_millis() + 1;
    std::thread::scope(|scope| {
        scope.spawn(crate::runtime::serving(move || {
            until(message, &mut ready);
            clock.advance(Duration::from_millis(1));
        }));
        crate::scheduler::willow_sched_run_until_deadline(deadline)
    })
}
