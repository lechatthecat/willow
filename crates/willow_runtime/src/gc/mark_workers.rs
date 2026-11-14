//! Dedicated reusable marker threads. Idle threads hold no heap references and
//! are unregistered; active jobs register, publish work before safepoints, and
//! unregister before acknowledging completion. Reclamation must wait for that
//! acknowledgement. Trace panics retire their queue consumer and request stopped fallback.
//! Unexpected worker infrastructure panics terminate the process.
use super::*;
use crate::gc_telemetry::workers::{Failure, JobMeasurement};
use std::thread::JoinHandle;

pub(super) trait MarkerObservation {
    fn drained<'a>(_owner: impl FnOnce() -> &'a Self)
    where
        Self: 'a,
    {
    }
    fn completed(&mut self) {}
    fn notified(&mut self) {}
}
#[derive(Default)]
pub(super) struct NoMarkerObservation {
    _disabled: (),
}
impl MarkerObservation for NoMarkerObservation {}
#[cfg(not(test))]
pub(super) type RuntimeMarkerObservation = NoMarkerObservation;
#[cfg(test)]
pub(super) type RuntimeMarkerObservation = termination_tests::MarkerCounts;

const BACKGROUND_BATCH: usize = 1024;

struct Job {
    runtime: super::coordinator::Lease,
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
    observation: RuntimeMarkerObservation,
}

pub(super) struct Pool {
    telemetry: Arc<crate::gc_telemetry::workers::WorkerTelemetry>,
    shared: Arc<(Mutex<State>, Condvar)>,
    threads: Vec<JoinHandle<()>>,
}

// Sample once per runtime, without initializing its collector or scheduler.
pub(super) fn configured_count() -> usize {
    *crate::runtime::current().marker_count
}

pub(crate) const fn marker_configuration() -> LazyLock<usize> {
    LazyLock::new(resolve_configured_count)
}

fn resolve_configured_count() -> usize {
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
    #[cfg(test)]
    pub(super) fn new(count: usize) -> std::io::Result<Self> {
        Self::new_for_runtime(count, crate::runtime::current())
    }

    pub(super) fn new_in(
        count: usize,
        telemetry: &Arc<crate::gc_telemetry::workers::WorkerTelemetry>,
    ) -> std::io::Result<Self> {
        Self::new_with_measurements(
            count,
            telemetry,
            Arc::new(crate::gc_telemetry::measurement::Measurements::new()),
            false,
        )
    }

    pub(super) fn new_for_runtime(
        count: usize,
        owner: &crate::runtime::Runtime,
    ) -> std::io::Result<Self> {
        Self::new_with_measurements(
            count,
            &owner.telemetry.workers,
            Arc::clone(&owner.telemetry.measurements),
            owner.diagnostics.gc_profile.one,
        )
    }

    fn new_with_measurements(
        count: usize,
        telemetry: &Arc<crate::gc_telemetry::workers::WorkerTelemetry>,
        measurements: Arc<crate::gc_telemetry::measurement::Measurements>,
        profile_enabled: bool,
    ) -> std::io::Result<Self> {
        let mut pool = Self {
            telemetry: Arc::clone(telemetry),
            shared: Arc::new((
                Mutex::new(State {
                    generation: 0,
                    job: None,
                    remaining: 0,
                    shutdown: false,
                    remaining_workers: 0,
                    observation: RuntimeMarkerObservation::default(),
                }),
                Condvar::new(),
            )),
            threads: Vec::with_capacity(count),
        };
        for index in 0..count {
            let shared = pool.shared.clone();
            let measurements = measurements.clone();
            shared.0.lock().unwrap().remaining_workers += 1;
            match crate::runtime::spawn_serving(
                std::thread::Builder::new().name(format!("willow-gc-mark-{index}")),
                move || worker_main_indexed(shared, index, measurements, profile_enabled),
            ) {
                Ok(thread) => pool.threads.push(thread),
                Err(error) => {
                    pool.shared.0.lock().unwrap().remaining_workers -= 1;
                    telemetry.record_failure(Failure::Spawn);
                    pool.stop(std::time::Duration::from_secs(2));
                    return Err(error);
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
    pub(super) fn run(
        &self,
        owner: &crate::runtime::Runtime,
        cycle: &Arc<ConcurrentCycle>,
        budget: usize,
    ) {
        // Own the scheduled job before publishing it, including workers not
        // yet awake. Completion is published only after all readers unregister.
        let runtime = super::coordinator::Lease::for_runtime(owner)
            .expect("background marking requires a shared runtime owner");
        let pacing = owner.gc.heap.lock().unwrap().pacer.enabled();
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
                runtime,
                cycle: cycle.clone(),
                pacing,
                remaining_work: AtomicUsize::new(cycle.objects.len().saturating_mul(4).max(1024)),
            }));
            state.remaining = self.threads.len();
            cv.notify_all();
        }
        let _completion = PublishedJob {
            shared: &self.shared,
        };
        cycle.drain_checked_until_in(owner, budget, None);
    }
}

// Every exit, including collector unwind, retires the published owner lease
// only after workers have dropped all borrowed job state.
struct PublishedJob<'a> {
    shared: &'a (Mutex<State>, Condvar),
}
impl Drop for PublishedJob<'_> {
    fn drop(&mut self) {
        let (lock, cv) = self.shared;
        let mut state = lock.lock().unwrap_or_else(|p| p.into_inner());
        while state.remaining != 0 {
            state = cv.wait(state).unwrap_or_else(|p| p.into_inner());
        }
        state.job = None;
    }
}

impl Pool {
    pub(super) fn stop(&mut self, timeout: std::time::Duration) {
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
                    self.telemetry.record_failure(Failure::JoinTimeout);
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
    worker_main_indexed(
        shared,
        0,
        Arc::new(crate::gc_telemetry::measurement::Measurements::new()),
        false,
    );
}

fn worker_main_indexed(
    shared: Arc<(Mutex<State>, Condvar)>,
    index: usize,
    measurements: Arc<crate::gc_telemetry::measurement::Measurements>,
    profile_enabled: bool,
) {
    let _lifetime = crate::gc_telemetry::measurement::WorkerLifetime::new_in(&measurements);
    crate::failure::ffi_boundary("GC marker worker", || {
        worker_loop(shared.clone(), index, profile_enabled);
        drop(_lifetime);
        // worker_loop has dropped its scratch storage and all epoch owners.
        let mut state = shared.0.lock().unwrap();
        state.remaining_workers -= 1;
        state.observation.completed();
        if state.remaining_workers == 0 {
            state.observation.notified();
            shared.1.notify_all();
        }
    });
}

fn worker_loop(shared: Arc<(Mutex<State>, Condvar)>, index: usize, profile_enabled: bool) {
    let _role = crate::gc_telemetry::observations::MarkerRole::new();
    let (lock, cv) = &*shared;
    let mut generation = 0;
    let mut children = Vec::new();
    loop {
        let idle_start = profile_enabled.then(std::time::Instant::now);
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
        let runtime = &*job.runtime;
        let profile = runtime.diagnostics.gc_profile.one.then(|| {
            crate::gc_telemetry::collector_profile::WorkerJob::begin_in(
                &runtime.telemetry.collector_profile,
                index,
                idle_start.map_or(0, |start| {
                    start.elapsed().as_nanos().min(u64::MAX as u128) as u64
                }),
            )
        });
        let owner = cycle.termination.enter();
        let measurement = JobMeasurement::begin_in(&runtime.telemetry);
        let duty = if job.pacing {
            pacer::worker_duty(pacer::cpu_capacity(), index)
        } else {
            pacer::SCALE
        };
        let registration = safepoint::MarkerRegistration::new(runtime);
        {
            // Retry and sliced objects can republish work indefinitely. Spend
            // a shared finite budget, leaving all remaining publications for
            // closure. Neither an empty sample nor budget exhaustion ends the
            // epoch. Unused claims are not refunded, avoiding empty-queue spin.
            loop {
                if duty == 0
                    || cycle.worker_failed.load(Ordering::Acquire)
                    || minor::requested(&runtime.gc)
                {
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
                let scanned = cycle.drain_recovering_in(runtime, || {
                    cycle.drain_background_in(runtime, budget, &mut children)
                });
                RuntimeMarkerObservation::drained(|| &runtime.gc.marker_observation);
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
                    registration.poll();
                    let remaining = delay.saturating_sub(crate::gc_telemetry::elapsed_ns(waiting));
                    std::thread::sleep(std::time::Duration::from_nanos(remaining.min(100_000)));
                }
                runtime
                    .telemetry
                    .pacing
                    .duty(cpu, crate::gc_telemetry::elapsed_ns(waiting));
                registration.poll();
                if scanned == 0 {
                    break;
                }
            }
        }
        children.clear();
        drop(registration);
        drop(owner);
        // Drop the epoch reference before publishing that this reader is gone.
        drop(measurement);
        drop(profile);
        drop(job);
        let mut state = lock.lock().unwrap();
        state.remaining -= 1;
        cv.notify_all();
    }
}

#[cfg(test)]
mod termination_tests {
    #[derive(Default)]
    pub(crate) struct MarkerCounts {
        pub(crate) drains: std::sync::atomic::AtomicUsize,
        pub(crate) completions: usize,
        pub(crate) notifications: usize,
    }
    impl super::MarkerObservation for MarkerCounts {
        fn drained<'a>(owner: impl FnOnce() -> &'a Self) {
            owner()
                .drains
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        fn completed(&mut self) {
            self.completions += 1;
        }
        fn notified(&mut self) {
            self.notifications += 1;
        }
    }
    #[test]
    fn marker_observations_keep_drain_and_shutdown_counts_owned() {
        use super::MarkerObservation;
        let mut first = MarkerCounts::default();
        let mut second = MarkerCounts::default();
        std::thread::scope(|scope| {
            let handles: Vec<_> = [(&mut first, 1), (&mut second, 3)]
                .into_iter()
                .map(|(owner, stride)| {
                    scope.spawn(crate::runtime::serving(move || {
                        for _ in 0..64 * stride {
                            MarkerCounts::drained(|| owner);
                        }
                        for _ in 0..stride {
                            owner.completed();
                        }
                        owner.notified();
                    }))
                })
                .collect();
            for handle in handles {
                handle.join().unwrap();
            }
        });
        assert_eq!(first.drains.load(std::sync::atomic::Ordering::Relaxed), 64);
        assert_eq!(
            second.drains.load(std::sync::atomic::Ordering::Relaxed),
            192
        );
        assert_eq!((first.completions, second.completions), (1, 3));
        assert_eq!((first.notifications, second.notifications), (1, 1));
        super::NoMarkerObservation::drained(|| {
            panic!("disabled marker observation resolved owner")
        });
        let mut disabled = super::NoMarkerObservation::default();
        disabled.completed();
        disabled.notified();
    }
    use super::*;

    #[test]
    fn private_service_shutdown_preserves_other_idle_marker_pool() {
        let first = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        let second = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        let first_pool = Pool::new_in(1, &first.telemetry.workers).unwrap();
        let second_pool = Pool::new_in(2, &second.telemetry.workers).unwrap();
        let first_state = first_pool.shared.clone();
        let second_state = second_pool.shared.clone();
        *first.gc.marker_workers.lock().unwrap() = Some(first_pool);
        *second.gc.marker_workers.lock().unwrap() = Some(second_pool);
        super::super::marking::shutdown_mark_workers_in(&first);
        assert!(first.gc.marker_workers.lock().unwrap().is_none());
        assert!(first_state.0.lock().unwrap().shutdown);
        {
            let other = second_state.0.lock().unwrap();
            assert!(!other.shutdown);
            assert_eq!(other.remaining_workers, 2);
        }
        super::super::marking::shutdown_mark_workers_in(&first);
        super::super::marking::shutdown_mark_workers_in(&second);
        let stopped = second_state.0.lock().unwrap();
        assert!(stopped.shutdown);
        assert_eq!(stopped.remaining_workers, 0);
    }

    #[test]
    fn private_collector_drop_stops_only_its_idle_marker_pool() {
        let first = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        let second = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        let (first_pool, second_pool) = std::thread::scope(|scope| {
            let first = scope.spawn(crate::runtime::serving(|| {
                Pool::new_in(1, &first.telemetry.workers).unwrap()
            }));
            let second = scope.spawn(crate::runtime::serving(|| {
                Pool::new_in(3, &second.telemetry.workers).unwrap()
            }));
            (first.join().unwrap(), second.join().unwrap())
        });
        let first_telemetry = Arc::downgrade(&first.telemetry.workers);
        let second_telemetry = Arc::downgrade(&second.telemetry.workers);
        let first_state = first_pool.shared.clone();
        let second_state = second_pool.shared.clone();
        *first.gc.marker_workers.lock().unwrap() = Some(first_pool);
        *second.gc.marker_workers.lock().unwrap() = Some(second_pool);
        drop(first);
        assert!(first_telemetry.upgrade().is_none());
        assert!(second_telemetry.upgrade().is_some());
        {
            let stopped = first_state.0.lock().unwrap();
            assert!(stopped.shutdown);
            assert_eq!(stopped.remaining_workers, 0);
            assert_eq!(stopped.observation.completions, 1);
        }
        {
            let running = second_state.0.lock().unwrap();
            assert!(!running.shutdown);
            assert_eq!(running.remaining_workers, 3);
            assert_eq!(running.observation.completions, 0);
        }
        drop(second);
        assert!(second_telemetry.upgrade().is_none());
        let stopped = second_state.0.lock().unwrap();
        assert!(stopped.shutdown);
        assert_eq!(stopped.remaining_workers, 0);
        assert_eq!(stopped.observation.completions, 3);
    }

    #[test]
    fn shutdown_notification_drains_idle_and_used_pools() {
        let _guard = isolated_runtime();
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
                    pool.run(crate::runtime::current(), &cycle, 0);
                }
                assert_eq!(Arc::strong_count(&cycle), 1);
                let start = std::time::Instant::now();
                drop(pool);
                let state = shared.0.lock().unwrap();
                assert_eq!(state.remaining_workers, 0);
                assert_eq!(state.observation.completions, count);
                assert_eq!(state.observation.notifications, usize::from(count != 0));
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
        let telemetry = Arc::new(crate::gc_telemetry::workers::WorkerTelemetry::new());
        let other = crate::gc_telemetry::workers::WorkerTelemetry::new();
        for count in [1, 4, 16] {
            let mut pool = Pool::new_in(0, &telemetry).unwrap();
            let shared = pool.shared.clone();
            let (release, released) = std::sync::mpsc::channel();
            let released = Arc::new(Mutex::new(released));
            let (done, completed) = std::sync::mpsc::channel();
            shared.0.lock().unwrap().remaining_workers = count;
            for _ in 0..count {
                let shared = shared.clone();
                let released = released.clone();
                let done = done.clone();
                pool.threads
                    .push(std::thread::spawn(crate::runtime::serving(move || {
                        // An unscheduled idle worker: no job or epoch is owned.
                        released.lock().unwrap().recv().unwrap();
                        let mut state = shared.0.lock().unwrap();
                        assert!(state.shutdown);
                        assert!(state.job.is_none());
                        state.remaining_workers -= 1;
                        shared.1.notify_all();
                        done.send(()).unwrap();
                    })));
            }
            let before = telemetry.snapshot().join_timeouts;
            let start = std::time::Instant::now();
            pool.stop(std::time::Duration::from_millis(10));
            assert!(start.elapsed() < std::time::Duration::from_secs(1));
            assert_eq!(shared.0.lock().unwrap().remaining_workers, count);
            assert_eq!(telemetry.snapshot().join_timeouts - before, count as u64);
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
        assert_eq!(other.snapshot().join_timeouts, 0);
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
        let _guard = isolated_runtime();
        reset_internal_for_test();
        let cycle = Arc::new(ConcurrentCycle::new(
            [],
            IdSet::default(),
            IdMap::default(),
            0,
        ));
        let pool = Pool::new(2).unwrap();
        let shared = pool.shared.clone();
        let notifier = std::thread::spawn(crate::runtime::serving(move || {
            for _ in 0..1024 {
                shared.1.notify_all();
                std::thread::yield_now();
            }
        }));
        for _ in 0..16 {
            pool.run(crate::runtime::current(), &cycle, 0);
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

#[cfg(test)]
mod configuration_tests {
    use super::*;

    #[test]
    fn configured_count_preserves_environment_policy_in_fresh_processes() {
        const CHILD: &str = "WILLOW_TEST_MARK_COUNT_EXPECTED";
        if let Ok(expected) = std::env::var(CHILD) {
            let expected = if expected == "auto" {
                std::thread::available_parallelism()
                    .map(|count| count.get().saturating_sub(1).clamp(1, 4))
                    .unwrap_or(1)
            } else {
                expected.parse::<usize>().unwrap()
            };
            for _ in 0..1_000 {
                assert_eq!(configured_count(), expected);
            }
            return;
        }
        for (setting, expected) in [
            (None, "auto"),
            (Some(""), "auto"),
            (Some("invalid"), "auto"),
            (Some("-1"), "auto"),
            (Some(" 3"), "auto"),
            (Some("999999999999999999999999999999999999"), "auto"),
            (Some("0"), "0"),
            (Some("1"), "1"),
            (Some("3"), "3"),
            (Some("64"), "64"),
            (Some("65"), "64"),
        ] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args([
                "--exact",
                "gc::mark_workers::configuration_tests::configured_count_preserves_environment_policy_in_fresh_processes",
            ]).env(CHILD, expected).env_remove("WILLOW_GC_MARK_THREADS");
            if let Some(setting) = setting {
                command.env("WILLOW_GC_MARK_THREADS", setting);
            }
            let output = command.output().unwrap();
            assert!(output.status.success(), "setting={setting:?}: {output:?}");
        }
    }

    #[test]
    fn configured_count_keeps_private_runtime_sources_independent() {
        let mut first = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        let mut second = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        first.marker_count = LazyLock::new(|| 3);
        second.marker_count = LazyLock::new(|| 7);
        assert!(LazyLock::get(&first.marker_count).is_none());
        assert!(LazyLock::get(&second.marker_count).is_none());
        for calls in [1, 10, 1_000, 10_000] {
            std::thread::scope(|scope| {
                for _ in 0..16 {
                    let first = &first;
                    let second = &second;
                    scope.spawn(crate::runtime::serving(move || {
                        for _ in 0..calls {
                            assert_eq!(*first.marker_count, 3);
                            assert_eq!(*second.marker_count, 7);
                        }
                    }));
                }
            });
        }
        assert_eq!(LazyLock::get(&first.marker_count), Some(&3));
        assert_eq!(LazyLock::get(&second.marker_count), Some(&7));
        assert!(LazyLock::get(&first.gc).is_none());
        assert!(LazyLock::get(&second.gc).is_none());
    }
}
