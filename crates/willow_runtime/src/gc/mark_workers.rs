//! Dedicated reusable marker threads. Idle threads hold no heap references and
//! are unregistered; active jobs register, publish work before safepoints, and
//! unregister before acknowledging completion. Reclamation must wait for that
//! acknowledgement. Trace panics retire their queue consumer and request stopped fallback.
//! Unexpected worker infrastructure panics terminate the process.
use super::*;
use crate::gc_telemetry::workers::{Failure, JobMeasurement, record_failure};
use std::thread::JoinHandle;

/// Background drain calls, so tests can bound per-claim batching.
#[cfg(test)]
pub(super) static BACKGROUND_DRAINS: AtomicUsize = AtomicUsize::new(0);

const BACKGROUND_BATCH: usize = 1024;

struct Job {
    cycle: Arc<ConcurrentCycle>,
    remaining_work: AtomicUsize,
    pacing: bool,
}

impl Job {
    fn claim(&self) -> usize {
        self.remaining_work
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                (remaining != 0).then(|| remaining.saturating_sub(BACKGROUND_BATCH))
            })
            .map_or(0, |remaining| remaining.min(BACKGROUND_BATCH))
    }
}

struct State {
    generation: u64,
    job: Option<Arc<Job>>,
    remaining: usize,
    shutdown: bool,
    remaining_workers: usize,
    #[cfg(test)]
    shutdown_completions: usize,
    #[cfg(test)]
    shutdown_notifications: usize,
}

pub(super) struct Pool {
    shared: Arc<(Mutex<State>, Condvar)>,
    threads: Vec<JoinHandle<()>>,
}

pub(super) fn configured_count() -> usize {
    std::env::var("WILLOW_GC_MARK_THREADS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .map(|count| count.min(64))
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|count| count.get().saturating_sub(1).clamp(1, 4))
                .unwrap_or(1)
        })
}

impl Pool {
    pub(super) fn new(count: usize) -> std::io::Result<Self> {
        let mut pool = Self {
            shared: Arc::new((
                Mutex::new(State {
                    generation: 0,
                    job: None,
                    remaining: 0,
                    shutdown: false,
                    remaining_workers: 0,
                    #[cfg(test)]
                    shutdown_completions: 0,
                    #[cfg(test)]
                    shutdown_notifications: 0,
                }),
                Condvar::new(),
            )),
            threads: Vec::with_capacity(count),
        };
        for index in 0..count {
            let shared = pool.shared.clone();
            shared.0.lock().unwrap().remaining_workers += 1;
            match std::thread::Builder::new()
                .name(format!("willow-gc-mark-{index}"))
                .spawn(move || worker_main_indexed(shared, index))
            {
                Ok(thread) => pool.threads.push(thread),
                Err(error) => {
                    pool.shared.0.lock().unwrap().remaining_workers -= 1;
                    record_failure(Failure::Spawn);
                    return Err(error); // Drop stops all already-created threads.
                }
            }
        }
        Ok(pool)
    }

    pub(super) fn count(&self) -> usize {
        self.threads.len()
    }

    /// The caller owns collection serialization and cannot request remark until
    /// this returns. Trace panics request stopped completion after readers leave.
    pub(super) fn run(&self, cycle: &Arc<ConcurrentCycle>, budget: usize) {
        // Own the scheduled job before publishing it, including workers not
        // yet awake. Completion is published only after all readers unregister.
        let pacing = runtime().heap.lock().unwrap().pacer.enabled();
        let _job_owner = cycle.termination.enter();
        let (lock, cv) = &*self.shared;
        {
            let mut state = lock.lock().unwrap();
            assert_eq!(state.remaining, 0);
            assert!(!state.shutdown);
            state.generation = state
                .generation
                .checked_add(1)
                .expect("worker generation exhausted");
            // One shared budget, not a full budget per worker. Continuations
            // and Retry count as work even when no new object is marked.
            state.job = Some(Arc::new(Job {
                cycle: cycle.clone(),
                pacing,
                remaining_work: AtomicUsize::new(cycle.objects.len().saturating_mul(4).max(1024)),
            }));
            state.remaining = self.threads.len();
            cv.notify_all();
        }
        cycle.drain_recovering(|| cycle.drain(budget));
        let mut state = lock.lock().unwrap();
        while state.remaining != 0 {
            state = cv.wait(state).unwrap();
        }
        state.job = None;
        drop(state);
    }
}

impl Pool {
    fn stop(&mut self, timeout: std::time::Duration) {
        if self.threads.is_empty() {
            return;
        }
        let (lock, cv) = &*self.shared;
        let mut state = lock.lock().unwrap();
        // Production shutdown occurs after run() has quiesced all readers.
        assert_eq!(state.remaining, 0, "cannot shut down active heap readers");
        state.shutdown = true;
        cv.notify_all();
        let deadline = std::time::Instant::now() + timeout;
        while state.remaining_workers != 0 {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                // Idle workers hold no epoch. One shared deadline bounds the
                // whole pool, including partial construction after spawn failure.
                for _ in 0..state.remaining_workers {
                    record_failure(Failure::JoinTimeout);
                }
                break;
            }
            state = cv.wait_timeout(state, remaining).unwrap().0;
        }
        drop(state);
        for thread in self.threads.drain(..) {
            // Notification precedes Rust/OS thread teardown. Never turn a
            // bounded completion wait into an unbounded TLS-destructor join.
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.stop(std::time::Duration::from_secs(2));
    }
}

#[cfg(test)]
fn worker_main(shared: Arc<(Mutex<State>, Condvar)>) {
    worker_main_indexed(shared, 0);
}

fn worker_main_indexed(shared: Arc<(Mutex<State>, Condvar)>, index: usize) {
    crate::failure::ffi_boundary("GC marker worker", || {
        worker_loop(shared.clone(), index);
        // worker_loop has dropped its scratch storage and all epoch owners.
        let mut state = shared.0.lock().unwrap();
        state.remaining_workers -= 1;
        #[cfg(test)]
        {
            state.shutdown_completions += 1;
        }
        if state.remaining_workers == 0 {
            #[cfg(test)]
            {
                state.shutdown_notifications += 1;
            }
            shared.1.notify_all();
        }
    });
}

fn worker_loop(shared: Arc<(Mutex<State>, Condvar)>, index: usize) {
    let _role = crate::gc_telemetry::observations::MarkerRole::new();
    let _lifetime = crate::gc_telemetry::measurement::WorkerLifetime::new();
    let (lock, cv) = &*shared;
    let mut generation = 0;
    let mut children = Vec::new();
    loop {
        let idle_start = crate::gc_telemetry::m3::enabled().then(std::time::Instant::now);
        let mut state = lock.lock().unwrap();
        while !state.shutdown && state.generation == generation {
            state = cv.wait(state).unwrap();
        }
        if state.shutdown {
            return;
        }
        generation = state.generation;
        let job = state.job.as_ref().unwrap().clone();
        let cycle = &job.cycle;
        drop(state);
        let profile = crate::gc_telemetry::m3::WorkerJob::begin(
            index,
            idle_start.map_or(0, |s| s.elapsed().as_nanos().min(u64::MAX as u128) as u64),
        );
        let owner = cycle.termination.enter();
        let measurement = JobMeasurement::begin();
        let duty = if job.pacing {
            pacer::worker_duty(pacer::cpu_capacity(), index)
        } else {
            pacer::SCALE
        };
        willow_gc_register_mutator();
        {
            // Retry and sliced objects can republish work indefinitely. Spend
            // a shared finite budget, leaving all remaining publications for
            // closure. Neither an empty sample nor budget exhaustion ends the
            // epoch. Unused claims are not refunded, avoiding empty-queue spin.
            loop {
                if duty == 0 || cycle.worker_failed.load(Ordering::Acquire) || minor::requested() {
                    break;
                }
                let budget = job.claim();
                if budget == 0 {
                    break;
                }
                // One drain per claim: per-drain consumer registration, batch
                // publication and CPU clock reads amortize over the claim. A
                // trace panic unwinds the drain, retiring its consumer and
                // publishing the batch; the loop head then observes failure.
                let started = std::time::Instant::now();
                let cpu_start = crate::gc_telemetry::workers::thread_cpu_ns();
                let scanned =
                    cycle.drain_recovering(|| cycle.drain_background(budget, &mut children));
                #[cfg(test)]
                BACKGROUND_DRAINS.fetch_add(1, Ordering::Relaxed);
                // Retire the consumer and publish private work before
                // polling; no queue or heap lock crosses this call.
                let cpu = cpu_start
                    .zip(crate::gc_telemetry::workers::thread_cpu_ns())
                    .and_then(|(a, b)| b.checked_sub(a));
                let elapsed = crate::gc_telemetry::elapsed_ns(started);
                let delay = pacer::duty_delay(cpu, elapsed, duty);
                let waiting = std::time::Instant::now();
                // Publish work before every poll. No queue/heap mutex crosses
                // the wait, and short slices keep minor rendezvous responsive.
                while crate::gc_telemetry::elapsed_ns(waiting) < delay {
                    willow_gc_safepoint();
                    let remaining = delay.saturating_sub(crate::gc_telemetry::elapsed_ns(waiting));
                    std::thread::sleep(std::time::Duration::from_nanos(remaining.min(100_000)));
                }
                crate::gc_telemetry::pacing::duty(cpu, crate::gc_telemetry::elapsed_ns(waiting));
                willow_gc_safepoint();
                if scanned == 0 {
                    break;
                }
            }
        }
        children.clear();
        willow_gc_unregister_mutator();
        drop(owner);
        // Drop the epoch reference before publishing that this reader is gone.
        drop(job);
        drop(measurement);
        drop(profile);
        let mut state = lock.lock().unwrap();
        state.remaining -= 1;
        cv.notify_all();
    }
}

#[cfg(test)]
mod termination_tests {
    use super::*;

    #[test]
    fn shutdown_notification_drains_idle_and_used_pools() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        for count in [0, 1, 4, 16, 64] {
            for used in [false, true] {
                let pool = Pool::new(count).unwrap();
                let shared = pool.shared.clone();
                let cycle = Arc::new(ConcurrentCycle::new(
                    [],
                    IdSet::default(),
                    IdMap::default(),
                    0,
                ));
                if used {
                    pool.run(&cycle, 0);
                }
                assert_eq!(Arc::strong_count(&cycle), 1);
                let start = std::time::Instant::now();
                drop(pool);
                let state = shared.0.lock().unwrap();
                assert_eq!(state.remaining_workers, 0);
                assert_eq!(state.shutdown_completions, count);
                assert_eq!(state.shutdown_notifications, usize::from(count != 0));
                assert_eq!(state.remaining, 0);
                assert!(state.job.is_none());
                assert!(start.elapsed() < std::time::Duration::from_secs(2));
                println!(
                    "shutdown workers={count} used={used} completions={count} elapsed_us={}",
                    start.elapsed().as_micros()
                );
            }
        }
        reset_internal_for_test();
    }

    #[test]
    fn shutdown_notification_timeout_detaches_only_idle_workers() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        for count in [1, 4, 16] {
            let mut pool = Pool::new(0).unwrap();
            let shared = pool.shared.clone();
            let (release, released) = std::sync::mpsc::channel();
            let released = Arc::new(Mutex::new(released));
            let (done, completed) = std::sync::mpsc::channel();
            shared.0.lock().unwrap().remaining_workers = count;
            for _ in 0..count {
                let shared = shared.clone();
                let released = released.clone();
                let done = done.clone();
                pool.threads.push(std::thread::spawn(move || {
                    // An unscheduled idle worker: no job or epoch is owned.
                    released.lock().unwrap().recv().unwrap();
                    let mut state = shared.0.lock().unwrap();
                    assert!(state.shutdown);
                    assert!(state.job.is_none());
                    state.remaining_workers -= 1;
                    shared.1.notify_all();
                    done.send(()).unwrap();
                }));
            }
            let before = crate::gc_telemetry::workers::snapshot().join_timeouts;
            let start = std::time::Instant::now();
            pool.stop(std::time::Duration::from_millis(10));
            assert!(start.elapsed() < std::time::Duration::from_secs(1));
            assert_eq!(shared.0.lock().unwrap().remaining_workers, count);
            assert_eq!(
                crate::gc_telemetry::workers::snapshot().join_timeouts - before,
                count as u64
            );
            drop(pool);
            for _ in 0..count {
                release.send(()).unwrap();
            }
            for _ in 0..count {
                completed
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap();
            }
            assert_eq!(shared.0.lock().unwrap().remaining_workers, 0);
        }
        reset_internal_for_test();
    }

    #[test]
    fn shutdown_notification_worker_infrastructure_panic_remains_fatal() {
        use super::super::failure_policy_tests::{GC_FATAL_CASE, assert_fatal_child};
        if std::env::var_os(GC_FATAL_CASE).is_some() {
            let pool = Pool::new(0).unwrap();
            let shared = pool.shared.clone();
            let _ = std::panic::catch_unwind(|| {
                let _state = shared.0.lock().unwrap();
                panic!("poison worker state");
            });
            worker_main(shared);
            return;
        }
        assert_fatal_child(
            "gc::mark_workers::termination_tests::shutdown_notification_worker_infrastructure_panic_remains_fatal",
            "marker shutdown panic",
            "runtime fatal: Rust panic in GC marker worker",
        );
    }

    #[test]
    fn termination_spurious_pool_wakes_do_not_duplicate_jobs_or_completion() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let cycle = Arc::new(ConcurrentCycle::new(
            [],
            IdSet::default(),
            IdMap::default(),
            0,
        ));
        let pool = Pool::new(2).unwrap();
        let shared = pool.shared.clone();
        let notifier = std::thread::spawn(move || {
            for _ in 0..1024 {
                shared.1.notify_all();
                std::thread::yield_now();
            }
        });
        for _ in 0..16 {
            pool.run(&cycle, 0);
            let state = pool.shared.0.lock().unwrap();
            assert_eq!(state.remaining, 0);
            assert!(state.job.is_none());
            assert!(cycle.termination.request(cycle.termination.sample(), true));
            cycle.termination.restart();
        }
        notifier.join().unwrap();
        assert_eq!(pool.shared.0.lock().unwrap().generation, 16);
        assert_eq!(cycle.queue.snapshot().counter_underflows, 0);
        drop(pool);
        reset_internal_for_test();
    }
}
