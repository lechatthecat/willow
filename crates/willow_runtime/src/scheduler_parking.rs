//! Interruptible idle waits. Registration, selection and removal are O(1).
use super::*;
use std::sync::atomic::AtomicU64;

#[derive(Debug, Default)]
struct WaitList {
    slots: Vec<WaitSlot>,
    free: Vec<usize>,
    head: Option<usize>,
    /// Latest linked slot registered under each worker index, so a targeted
    /// wake finds its owner in O(1).
    by_worker: Vec<Option<usize>>,
}

#[derive(Debug)]
struct WaitSlot {
    thread: std::thread::Thread,
    worker: usize,
    previous: Option<usize>,
    next: Option<usize>,
    linked: bool,
}

impl WaitList {
    fn insert(&mut self) -> usize {
        let worker = current_worker();
        let slot = WaitSlot {
            thread: std::thread::current(),
            worker,
            previous: None,
            next: self.head,
            linked: true,
        };
        let index = if let Some(index) = self.free.pop() {
            self.slots[index] = slot;
            index
        } else {
            self.slots.push(slot);
            self.slots.len() - 1
        };
        if let Some(head) = self.head {
            self.slots[head].previous = Some(index);
        }
        self.head = Some(index);
        if self.by_worker.len() <= worker {
            self.by_worker.resize(worker + 1, None);
        }
        self.by_worker[worker] = Some(index);
        index
    }

    fn unlink(&mut self, index: usize) {
        if !self.slots[index].linked {
            return;
        }
        let previous = self.slots[index].previous;
        let next = self.slots[index].next;
        if let Some(previous) = previous {
            self.slots[previous].next = next;
        } else {
            self.head = next;
        }
        if let Some(next) = next {
            self.slots[next].previous = previous;
        }
        self.slots[index].linked = false;
        let worker = self.slots[index].worker;
        if self.by_worker[worker] == Some(index) {
            self.by_worker[worker] = None;
        }
    }
}

#[derive(Debug, Default)]
struct IdleWaiters {
    generation: AtomicU64,
    registered: AtomicUsize,
    list: Mutex<WaitList>,
}

impl IdleWaiters {
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn notify(&self, all: bool) -> usize {
        self.publish(true, all)
    }

    fn publish(&self, wake_worker: bool, all: bool) -> usize {
        // SeqCst couples the lock-free no-waiter path with registration's
        // generation recheck: either the publisher sees registration or the
        // registering worker sees publication, including before park_timeout.
        // A sole local continuation is consumed by its active publisher; it
        // does not need either an unpark or a shared generation update.
        if !wake_worker {
            return 0;
        }
        self.generation.fetch_add(1, Ordering::SeqCst);
        if self.registered.load(Ordering::SeqCst) == 0 {
            return 0;
        }
        let mut list = self.list.lock().unwrap_or_else(|p| p.into_inner());
        let mut count = 0;
        while let Some(index) = list.head {
            list.unlink(index);
            self.registered.fetch_sub(1, Ordering::SeqCst);
            // Keep the slot reserved until its waiter returns. Unpark tokens
            // survive publication before the actual park.
            list.slots[index].thread.unpark();
            count += 1;
            if !all {
                break;
            }
        }
        count
    }

    /// Publish work for `worker` alone: advance the generation, so a worker
    /// that probed its queue before this publication does not park on it, and
    /// unpark that worker if it already waits. Other waiters stay parked. A
    /// non-pool thread sharing the index may be woken spuriously, or may hide
    /// the worker's slot, which then sees the generation at its timeout.
    fn notify_worker(&self, worker: usize) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        if self.registered.load(Ordering::SeqCst) == 0 {
            return;
        }
        let list = self.list.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(index) = list.by_worker.get(worker).copied().flatten() {
            // The waiter sees the new generation and unlinks itself.
            list.slots[index].thread.unpark();
        }
    }

    fn wait(&self, start: u64, timeout: Duration) -> bool {
        self.wait_with_backoff(start, timeout, || {
            for _ in 0..2 {
                std::hint::spin_loop();
            }
        })
    }

    fn wait_with_backoff(&self, start: u64, timeout: Duration, backoff: impl FnOnce()) -> bool {
        self.wait_inner(start, Some(timeout), backoff)
    }

    fn wait_inner(&self, start: u64, timeout: Option<Duration>, backoff: impl FnOnce()) -> bool {
        self.wait_inner_in(crate::runtime::current(), start, timeout, backoff)
    }

    fn wait_inner_in(
        &self,
        owner: &crate::runtime::Runtime,
        start: u64,
        timeout: Option<Duration>,
        backoff: impl FnOnce(),
    ) -> bool {
        // Bounded CPU backoff per failed probe, including publication races.
        // The backoff never waits for a condition or yields to the OS, and
        // it holds no lock. Then retry known work or register to park.
        backoff();
        // A publication after the failed probe requires an immediate retry,
        // not a yield or a contended waiter-list registration. This read is
        // only a fast path: registration still rechecks the generation below.
        if self.generation() != start {
            crate::observation::SchedulerHooks::idle(|| owner, 1);
            return true;
        }
        // SAFETY: the remaining interval only operates on native wait-list
        // metadata and parks. Drop resumes GC ownership after the list locks
        // are released, including both registration-race and timeout returns.
        let _idle_mutator = unsafe { crate::gc::IdleMutator::enter_in(owner) };
        let index = {
            let mut list = self.list.lock().unwrap_or_else(|p| p.into_inner());
            self.registered.fetch_add(1, Ordering::SeqCst);
            if self.generation() != start {
                self.registered.fetch_sub(1, Ordering::SeqCst);
                crate::observation::SchedulerHooks::idle(|| owner, 1);
                return true;
            }
            list.insert()
        };
        let deadline = timeout.map(|duration| Instant::now() + duration);
        loop {
            if let Some(deadline) = deadline {
                std::thread::park_timeout(deadline.saturating_duration_since(Instant::now()));
            } else {
                std::thread::park();
            }
            let list = self.list.lock().unwrap_or_else(|p| p.into_inner());
            if !list.slots[index].linked
                || self.generation() != start
                || deadline.is_some_and(|deadline| Instant::now() >= deadline)
            {
                break;
            }
            // park_timeout may return spuriously or consume another user's token.
        }
        let mut list = self.list.lock().unwrap_or_else(|p| p.into_inner());
        if list.slots[index].linked {
            list.unlink(index);
            self.registered.fetch_sub(1, Ordering::SeqCst);
        }
        list.free.push(index);
        self.generation() != start
    }
}

/// Wake lists belong to a runtime; equal worker indices in another runtime
/// must not consume or hide this runtime's targeted notification.
#[derive(Debug, Default)]
pub(crate) struct Parking {
    idle: IdleWaiters,
    completion: IdleWaiters,
}

impl Parking {
    pub(super) fn completion_generation(&self) -> u64 {
        self.completion.generation()
    }
    pub(super) fn wait_completion(&self, owner: &crate::runtime::Runtime, start: u64) {
        self.completion
            .wait_inner_in(owner, start, None, std::hint::spin_loop);
    }
    pub(super) fn generation(&self) -> u64 {
        self.idle.generation()
    }
    pub(super) fn notify_idle(&self) {
        self.idle.notify(false);
    }
    pub(super) fn notify_local(&self, backlog: usize) {
        self.idle.publish(backlog > 1, false);
    }
    pub(super) fn notify_worker(&self, worker: usize) {
        self.idle.notify_worker(worker);
    }
    pub(super) fn notify_all(&self) {
        self.idle.notify(true);
        self.completion.notify(true);
    }
}

fn default_parking() -> &'static Parking {
    &crate::runtime::current().parking
}

pub(super) fn completion_wake_generation() -> u64 {
    default_parking().completion.generation()
}
pub(super) fn wait_for_completion_event(start: u64) {
    default_parking()
        .completion
        .wait_inner(start, None, std::hint::spin_loop);
}
pub(super) fn notify_idle_waiters() {
    default_parking().notify_idle();
}
pub(super) fn notify_local_work(backlog: usize) {
    default_parking().notify_local(backlog);
}
pub(super) fn notify_worker_waiter(worker: usize) {
    default_parking().notify_worker(worker);
}
pub(super) fn notify_all_idle_waiters() {
    default_parking().notify_all();
}
pub(super) fn current_wake_generation() -> u64 {
    default_parking().generation()
}
pub(super) fn wait_for_wake_since(start: u64, timeout: Duration) -> bool {
    default_parking().idle.wait(start, timeout)
}
pub(super) fn wait_for_owner_event(
    owner: &crate::runtime::Runtime,
    start: u64,
    timeout: Option<Duration>,
) -> bool {
    owner.parking.idle.wait_inner_in(owner, start, timeout, || {
        std::hint::spin_loop();
        if timeout.is_some() {
            std::hint::spin_loop();
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_wait_lists_do_not_consume_peer_worker_notifications() {
        let first = crate::runtime::Runtime::new(environment_worker_config);
        let second = crate::runtime::Runtime::new(environment_worker_config);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            for (id, owner) in [(1, &first), (2, &second)] {
                let done_tx = done_tx.clone();
                scope.spawn(crate::runtime::serving(move || {
                    let _registration = crate::gc::MutatorRegistration::new_in(owner);
                    let generation = owner.parking.generation();
                    let woken =
                        wait_for_owner_event(owner, generation, Some(Duration::from_secs(5)));
                    done_tx.send((id, woken)).unwrap();
                }));
            }
            registered(&first.parking.idle, 1);
            registered(&second.parking.idle, 1);
            let first_generation = first.parking.generation();
            let first_completion = first.parking.completion_generation();
            second.parking.notify_worker(0);
            let completed = done_rx.recv_timeout(Duration::from_secs(2));
            let first_still_waiting = first.parking.idle.registered.load(Ordering::SeqCst) == 1;
            let first_unchanged = first.parking.generation() == first_generation
                && first.parking.completion_generation() == first_completion;
            first.parking.notify_all();
            assert_eq!(completed.unwrap(), (2, true));
            assert!(first_still_waiting && first_unchanged);
            assert_eq!(
                done_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
                (1, true)
            );
        });
    }

    fn registered(waiters: &IdleWaiters, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while waiters.registered.load(Ordering::SeqCst) != count {
            assert!(Instant::now() < deadline, "waiter registration timed out");
            std::thread::yield_now();
        }
    }

    /// Wait until `count` waiters of a shared queue registered. Another
    /// runtime test may publish an event on the same queue first, waking a
    /// waiter before it registers: that reports `false`.
    fn registered_since(waiters: &IdleWaiters, count: usize, generation: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while waiters.registered.load(Ordering::SeqCst) != count {
            if waiters.generation() != generation {
                return false;
            }
            assert!(Instant::now() < deadline, "waiter registration timed out");
            std::thread::yield_now();
        }
        true
    }

    #[test]
    fn idle_gc_stops_do_not_wake_workers_or_completion_drivers() {
        let _test = crate::gc::isolated_runtime();
        for waiters in [&default_parking().idle, &default_parking().completion] {
            for workers in [1, 4, 16] {
                for cycles in [1, 8, 64] {
                    // The queues are process-wide, so another test may publish
                    // an event during a round. Such a round says nothing about
                    // GC and is retried; GC publishing would disturb every one.
                    let mut attempts = 0;
                    let probes = loop {
                        attempts += 1;
                        assert!(attempts <= 100, "GC must not publish a scheduler event");
                        crate::gc::reset_internal_for_test();
                        let generation = waiters.generation();
                        let probes = AtomicUsize::new(0);
                        let parked = std::thread::scope(|scope| {
                            for _ in 0..workers {
                                let probes = &probes;
                                scope.spawn(crate::runtime::serving(move || {
                                    let _mutator = crate::gc::MutatorRegistration::new();
                                    assert!(waiters.wait_inner(generation, None, || {
                                        probes.fetch_add(1, Ordering::SeqCst);
                                    }));
                                }));
                            }
                            if !registered_since(waiters, workers, generation) {
                                waiters.notify(true);
                                return false;
                            }
                            for _ in 0..cycles {
                                crate::gc::minor_collect_internal();
                                crate::gc::willow_gc_collect();
                            }
                            let unchanged = waiters.generation() == generation;
                            let still_waiting = waiters.registered.load(Ordering::SeqCst);
                            // Always release workers before checking regressions.
                            waiters.notify(true);
                            if unchanged {
                                assert_eq!(still_waiting, workers);
                            }
                            unchanged
                        });
                        if parked {
                            break probes;
                        }
                    };
                    assert_eq!(probes.load(Ordering::SeqCst), workers);
                    assert_eq!(waiters.registered.load(Ordering::SeqCst), 0);
                    eprintln!(
                        "idle workers={workers} minor+major cycles={cycles} probes={workers} gc_wakes=0"
                    );
                }
            }
        }
        crate::gc::reset_internal_for_test();
    }

    #[test]
    fn event_wait_retains_publication_before_registration() {
        let waiters = IdleWaiters::default();
        let generation = waiters.generation();
        waiters.notify(true);
        assert!(waiters.wait_inner(generation, None, || {}));
        assert_eq!(waiters.registered.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn event_wait_has_no_periodic_backoff_and_wakes_at_pool_sizes() {
        for workers in [1, 2, 8, 32] {
            let waiters = IdleWaiters::default();
            let probes = AtomicUsize::new(0);
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    let waiters = &waiters;
                    let probes = &probes;
                    scope.spawn(crate::runtime::serving(move || {
                        assert!(waiters.wait_inner(waiters.generation(), None, || {
                            probes.fetch_add(1, Ordering::SeqCst);
                        }));
                    }));
                }
                registered(&waiters, workers);
                // The event wait has no timeout; one registration/probe per
                // worker regardless of how long the producer computes.
                assert_eq!(probes.load(Ordering::SeqCst), workers);
                waiters.notify(true);
            });
            assert_eq!(probes.load(Ordering::SeqCst), workers);
            assert_eq!(waiters.registered.load(Ordering::SeqCst), 0);
        }
    }

    #[test]
    fn notify_one_and_shutdown_at_increasing_pool_sizes() {
        for workers in [1, 2, 8, 32] {
            let waiters = IdleWaiters::default();
            assert_eq!(waiters.notify(false), 0);
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    let waiters = &waiters;
                    scope.spawn(crate::runtime::serving(move || {
                        assert!(waiters.wait(waiters.generation(), Duration::from_secs(5)));
                    }));
                }
                registered(&waiters, workers);
                // Publication changes the generation before taking the list lock.
                // Independently woken waiters can unregister before either call
                // selects them, so only synthetic registrations have exact counts.
                let selected = waiters.notify(false);
                assert!(selected <= 1);
                assert!(waiters.notify(true) <= workers - selected);
            });
            assert_eq!(waiters.registered.load(Ordering::SeqCst), 0);
            assert_eq!(waiters.notify(false), 0);
            assert!(waiters.list.lock().unwrap().head.is_none());
        }
    }

    #[test]
    fn independently_woken_waiters_can_leave_before_shutdown() {
        let waiters = IdleWaiters::default();
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(crate::runtime::serving(|| {
                        assert!(waiters.wait(waiters.generation(), Duration::from_secs(5)));
                    }))
                })
                .collect();
            registered(&waiters, 2);
            assert!(waiters.notify(false) <= 1);
            // Force an independent wake instead of relying on a spurious OS wake.
            for handle in &handles {
                handle.thread().unpark();
            }
            for handle in handles {
                handle.join().unwrap();
            }
            assert_eq!(waiters.notify(true), 0);
        });
        assert_eq!(waiters.registered.load(Ordering::SeqCst), 0);
        let list = waiters.list.lock().unwrap();
        assert!(list.head.is_none());
        assert_eq!(list.free.len(), 2);
    }

    #[test]
    fn notification_selects_exact_counts_without_concurrent_removal() {
        for workers in [1, 2, 8, 32] {
            let waiters = IdleWaiters::default();
            // Synthetic registrations cannot unregister between notifications.
            for _ in 0..workers {
                waiters.list.lock().unwrap().insert();
            }
            waiters.registered.store(workers, Ordering::SeqCst);
            assert_eq!(waiters.notify(false), 1);
            assert_eq!(waiters.notify(true), workers - 1);
            assert_eq!(waiters.registered.load(Ordering::SeqCst), 0);
            assert!(waiters.list.lock().unwrap().head.is_none());
        }
    }

    #[test]
    fn worker_notification_wakes_only_that_worker() {
        for workers in [2, 8, 32] {
            // An untargeted waiter that wakes spuriously after the generation
            // advanced also leaves; that disturbs a round but not every one.
            let mut attempts = 0;
            while !worker_notification_round(workers) {
                attempts += 1;
                assert!(
                    attempts < 100,
                    "worker notification woke untargeted waiters"
                );
            }
        }
    }

    fn worker_notification_round(workers: usize) -> bool {
        let waiters = IdleWaiters::default();
        let target = workers - 1;
        let only_target = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|worker| {
                    let waiters = &waiters;
                    scope.spawn(crate::runtime::serving(move || {
                        CURRENT_WORKER.with(|slot| slot.set(worker));
                        waiters.wait(waiters.generation(), Duration::from_secs(30))
                    }))
                })
                .collect();
            registered(&waiters, workers);
            let generation = waiters.generation();
            waiters.notify_worker(target);
            assert_eq!(waiters.generation(), generation + 1);
            let deadline = Instant::now() + Duration::from_secs(30);
            while !handles[target].is_finished() {
                assert!(Instant::now() < deadline, "targeted waiter did not wake");
                std::thread::yield_now();
            }
            let only_target = {
                let list = waiters.list.lock().unwrap();
                assert!(list.by_worker[target].is_none());
                (0..target).all(|worker| list.by_worker[worker].is_some())
            };
            // Release the untargeted waiters.
            waiters.notify(true);
            for handle in handles {
                assert!(handle.join().unwrap());
            }
            only_target
        });
        assert_eq!(waiters.registered.load(Ordering::SeqCst), 0);
        assert!(
            waiters
                .list
                .lock()
                .unwrap()
                .by_worker
                .iter()
                .all(Option::is_none)
        );
        only_target
    }

    #[test]
    fn worker_notification_without_waiters_only_advances_generation() {
        let waiters = IdleWaiters::default();
        waiters.notify_worker(3);
        assert_eq!(waiters.generation(), 1);
        assert!(waiters.list.lock().unwrap().slots.is_empty());
    }

    #[test]
    fn sole_local_continuation_does_not_unpark_a_thief() {
        let waiters = IdleWaiters::default();
        std::thread::scope(|scope| {
            scope.spawn(crate::runtime::serving(|| {
                waiters.wait(waiters.generation(), Duration::from_secs(5));
            }));
            registered(&waiters, 1);
            assert_eq!(waiters.publish(false, false), 0);
            assert!(waiters.list.lock().unwrap().head.is_some());
            assert!(waiters.notify(false) <= 1);
        });
    }

    #[test]
    fn sole_local_continuations_do_not_write_shared_generation() {
        for publishers in [1, 2, 8, 32] {
            for publications in [16, 256, 4096] {
                let waiters = IdleWaiters::default();
                std::thread::scope(|scope| {
                    for _ in 0..publishers {
                        let waiters = &waiters;
                        scope.spawn(crate::runtime::serving(move || {
                            for _ in 0..publications {
                                assert_eq!(waiters.publish(false, false), 0);
                            }
                        }));
                    }
                });
                assert_eq!(waiters.generation(), 0);
                println!("publishers={publishers} publications={publications} generation_writes=0");
            }
        }
    }

    #[test]
    fn selection_removes_parked_count_before_worker_resumes() {
        let waiters = IdleWaiters::default();
        let index = waiters.list.lock().unwrap().insert();
        waiters.registered.store(1, Ordering::SeqCst);
        assert_eq!(waiters.notify(false), 1);
        assert_eq!(waiters.registered.load(Ordering::SeqCst), 0);
        assert!(!waiters.list.lock().unwrap().slots[index].linked);
        assert_eq!(waiters.notify(false), 0);
    }

    #[test]
    fn registration_race_timeout_and_fragmented_slot_reuse() {
        let waiters = IdleWaiters::default();
        let generation = waiters.generation();
        waiters.notify(false);
        assert!(waiters.wait(generation, Duration::from_secs(5)));
        for _ in 0..1000 {
            assert!(!waiters.wait(waiters.generation(), Duration::ZERO));
        }
        let list = waiters.list.lock().unwrap();
        assert_eq!(list.slots.len(), 1);
        assert_eq!(list.free.len(), 1);
        assert!(list.head.is_none());
    }

    #[test]
    fn publication_before_registration_retries_without_retaining_slots() {
        // Every retry uses an obsolete probe generation. Even a sustained
        // stream of failed probes must neither sleep through known work nor
        // consume waiter slots. Exercise many thieves against one publisher.
        for workers in [1, 2, 8, 32] {
            for retries in [1, 16, 256] {
                let waiters = IdleWaiters::default();
                let generation = waiters.generation();
                waiters.notify(false);
                let backoffs = AtomicUsize::new(0);
                std::thread::scope(|scope| {
                    for _ in 0..workers {
                        let waiters = &waiters;
                        let backoffs = &backoffs;
                        scope.spawn(crate::runtime::serving(move || {
                            for _ in 0..retries {
                                assert!(waiters.wait_with_backoff(
                                    generation,
                                    Duration::from_secs(5),
                                    || {
                                        backoffs.fetch_add(1, Ordering::Relaxed);
                                    },
                                ));
                            }
                        }));
                    }
                });
                assert_eq!(waiters.registered.load(Ordering::SeqCst), 0);
                let list = waiters.list.lock().unwrap();
                assert!(list.slots.is_empty());
                assert!(list.free.is_empty());
                assert!(list.head.is_none());
                assert_eq!(backoffs.load(Ordering::Relaxed), workers * retries);
                println!(
                    "workers={workers} retries={retries} backoffs={} retained_slots=0",
                    workers * retries
                );
            }
        }
    }

    #[test]
    fn known_publication_backs_off_once_without_taking_waiter_lock() {
        let waiters = IdleWaiters::default();
        let generation = waiters.generation();
        waiters.notify(false);
        // Holding this lock proves the published-work fast path never takes it.
        let _list = waiters.list.lock().unwrap();
        let mut backoffs = 0;
        assert!(
            waiters.wait_with_backoff(generation, Duration::from_secs(5), || {
                backoffs += 1;
            })
        );
        assert_eq!(backoffs, 1);
        assert_eq!(waiters.registered.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn publication_during_backoff_is_observed_before_parking() {
        let waiters = IdleWaiters::default();
        let generation = waiters.generation();
        let mut backoffs = 0;
        assert!(
            waiters.wait_with_backoff(generation, Duration::from_secs(5), || {
                backoffs += 1;
                // Also proves backoff holds no waiter-list lock.
                assert!(waiters.list.try_lock().is_ok());
                waiters.notify(false);
            })
        );
        assert_eq!(backoffs, 1);
        assert_eq!(waiters.registered.load(Ordering::SeqCst), 0);
        assert!(waiters.list.lock().unwrap().slots.is_empty());
    }

    #[test]
    fn failed_probe_without_publication_backs_off_once_then_times_out() {
        let waiters = IdleWaiters::default();
        let mut backoffs = 0;
        assert!(
            !waiters.wait_with_backoff(waiters.generation(), Duration::ZERO, || {
                backoffs += 1;
            })
        );
        assert_eq!(backoffs, 1);
        assert_eq!(waiters.registered.load(Ordering::SeqCst), 0);
        assert_eq!(waiters.list.lock().unwrap().free.len(), 1);
    }

    #[test]
    fn removal_unlinks_middle_without_scanning() {
        for count in [4, 16, 256, 4096] {
            let mut list = WaitList::default();
            for _ in 0..count {
                list.insert();
            }
            for index in (1..count).step_by(2) {
                list.unlink(index);
            }
            let mut remaining = 0;
            while let Some(index) = list.head {
                assert_eq!(index % 2, 0);
                list.unlink(index);
                remaining += 1;
            }
            assert_eq!(remaining, count / 2);
        }
    }

    #[test]
    #[ignore = "release Linux latency measurement; host load dependent"]
    fn idle_wake_latency_under_100us() {
        let mut samples = Vec::new();
        for _ in 0..100 {
            let waiters = IdleWaiters::default();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                let waiters = &waiters;
                scope.spawn(crate::runtime::serving(move || {
                    assert!(waiters.wait(waiters.generation(), Duration::from_secs(5)));
                    tx.send(Instant::now()).unwrap();
                }));
                registered(waiters, 1);
                let start = Instant::now();
                assert!(waiters.notify(false) <= 1);
                samples.push(rx.recv_timeout(Duration::from_secs(5)).unwrap() - start);
            });
        }
        samples.sort();
        println!("wake latency p50={:?} p99={:?}", samples[50], samples[99]);
        assert!(samples[50] < Duration::from_micros(100));
    }
}
