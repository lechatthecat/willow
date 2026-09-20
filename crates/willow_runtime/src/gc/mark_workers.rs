//! Dedicated reusable marker threads. Idle threads hold no heap references and
//! are unregistered; active jobs register, publish work before safepoints, and
//! unregister before acknowledging completion. Reclamation must wait for that
//! acknowledgement, including on a trace-hook unwind.
use super::*;
use crate::gc_telemetry::workers::{Failure, JobMeasurement, record_failure};
use std::thread::JoinHandle;

struct Job {
    cycle: Arc<ConcurrentCycle>,
    remaining_work: AtomicUsize,
}

impl Job {
    fn claim(&self) -> usize {
        self.remaining_work
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                (remaining != 0).then(|| remaining.saturating_sub(32))
            })
            .map_or(0, |remaining| remaining.min(32))
    }
}

struct State {
    generation: u64,
    job: Option<Arc<Job>>,
    remaining: usize,
    shutdown: bool,
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
                }),
                Condvar::new(),
            )),
            threads: Vec::with_capacity(count),
        };
        for index in 0..count {
            let shared = pool.shared.clone();
            match std::thread::Builder::new()
                .name(format!("willow-gc-mark-{index}"))
                .spawn(move || worker_main(shared))
            {
                Ok(thread) => pool.threads.push(thread),
                Err(error) => {
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
    /// this returns. Even a collector-side unwind waits for all epoch readers.
    pub(super) fn run(&self, cycle: &Arc<ConcurrentCycle>, budget: usize) {
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
                remaining_work: AtomicUsize::new(cycle.objects.len().saturating_mul(4).max(1024)),
            }));
            state.remaining = self.threads.len();
            cv.notify_all();
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cycle.drain(budget)));
        let mut state = lock.lock().unwrap();
        while state.remaining != 0 {
            state = cv.wait(state).unwrap();
        }
        state.job = None;
        drop(state);
        if let Err(payload) = result {
            record_failure(Failure::MarkerPanic);
            cycle.worker_failed.store(true, Ordering::Release);
            discard_callback_panic(payload);
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        let (lock, cv) = &*self.shared;
        let mut state = lock.lock().unwrap();
        // Production shutdown occurs after run() has quiesced all readers.
        assert_eq!(state.remaining, 0, "cannot shut down active heap readers");
        state.shutdown = true;
        cv.notify_all();
        drop(state);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        for thread in self.threads.drain(..) {
            while !thread.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            if thread.is_finished() {
                let _ = thread.join();
            } else {
                // Shutdown was already requested, and remaining==0 proves this
                // idle thread holds no heap epoch. Never block process exit
                // indefinitely on an OS thread that is not being scheduled.
                record_failure(Failure::JoinTimeout);
            }
        }
    }
}

fn worker_main(shared: Arc<(Mutex<State>, Condvar)>) {
    let (lock, cv) = &*shared;
    let mut generation = 0;
    let mut children = Vec::new();
    loop {
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
        let measurement = JobMeasurement::begin();
        willow_gc_register_mutator();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // Retry and sliced objects can republish work indefinitely. Spend
            // a shared finite budget, leaving all remaining publications for
            // closure. Neither an empty sample nor budget exhaustion ends the
            // epoch. Unused claims are not refunded, avoiding empty-queue spin.
            loop {
                if cycle.worker_failed.load(Ordering::Acquire) {
                    break;
                }
                let budget = job.claim();
                if budget == 0 {
                    break;
                }
                let scanned = cycle.drain_background(budget, &mut children);
                willow_gc_safepoint();
                if scanned == 0 {
                    break;
                }
            }
        }));
        if let Err(payload) = result {
            record_failure(Failure::MarkerPanic);
            cycle.worker_failed.store(true, Ordering::Release);
            discard_callback_panic(payload);
        }
        children.clear();
        willow_gc_unregister_mutator();
        // Drop the epoch reference before publishing that this reader is gone.
        drop(job);
        drop(measurement);
        let mut state = lock.lock().unwrap();
        state.remaining -= 1;
        cv.notify_all();
    }
}
