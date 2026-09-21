//! Interruptible idle waits. Registration, selection and removal are O(1).
use super::*;
use std::sync::atomic::AtomicU64;

#[derive(Default)]
struct WaitList {
    slots: Vec<WaitSlot>,
    free: Vec<usize>,
    head: Option<usize>,
}

struct WaitSlot {
    thread: std::thread::Thread,
    previous: Option<usize>,
    next: Option<usize>,
    linked: bool,
}

impl WaitList {
    fn insert(&mut self) -> usize {
        let slot = WaitSlot {
            thread: std::thread::current(),
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
    }
}

#[derive(Default)]
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

    fn wait(&self, start: u64, timeout: Duration) -> bool {
        let index = {
            let mut list = self.list.lock().unwrap_or_else(|p| p.into_inner());
            self.registered.fetch_add(1, Ordering::SeqCst);
            if self.generation() != start {
                self.registered.fetch_sub(1, Ordering::SeqCst);
                return true;
            }
            list.insert()
        };
        let deadline = Instant::now() + timeout;
        loop {
            std::thread::park_timeout(deadline.saturating_duration_since(Instant::now()));
            let list = self.list.lock().unwrap_or_else(|p| p.into_inner());
            if !list.slots[index].linked || self.generation() != start || Instant::now() >= deadline
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

static IDLE_WAITERS: LazyLock<IdleWaiters> = LazyLock::new(IdleWaiters::default);

pub(super) fn notify_idle_waiters() {
    IDLE_WAITERS.notify(false);
}

pub(super) fn notify_local_work(backlog: usize) {
    // The publishing worker can consume one runnable task itself. Waking a
    // thief for that sole continuation creates cross-core ping-pong (and
    // native-stack affinity bounces) without adding useful parallelism.
    IDLE_WAITERS.publish(backlog > 1, false);
}

pub(super) fn notify_all_idle_waiters() {
    IDLE_WAITERS.notify(true);
}

pub(super) fn current_wake_generation() -> u64 {
    IDLE_WAITERS.generation()
}

pub(super) fn wait_for_wake_since(start: u64, timeout: Duration) -> bool {
    IDLE_WAITERS.wait(start, timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered(waiters: &IdleWaiters, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while waiters.registered.load(Ordering::SeqCst) != count {
            assert!(Instant::now() < deadline, "waiter registration timed out");
            std::thread::yield_now();
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
                    scope.spawn(move || {
                        assert!(waiters.wait(waiters.generation(), Duration::from_secs(5)));
                    });
                }
                registered(&waiters, workers);
                assert_eq!(waiters.notify(false), 1);
                assert_eq!(waiters.notify(true), workers - 1);
            });
            assert_eq!(waiters.registered.load(Ordering::SeqCst), 0);
            assert_eq!(waiters.notify(false), 0);
            assert!(waiters.list.lock().unwrap().head.is_none());
        }
    }

    #[test]
    fn sole_local_continuation_does_not_unpark_a_thief() {
        let waiters = IdleWaiters::default();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                waiters.wait(waiters.generation(), Duration::from_secs(5));
            });
            registered(&waiters, 1);
            assert_eq!(waiters.publish(false, false), 0);
            assert!(waiters.list.lock().unwrap().head.is_some());
            assert_eq!(waiters.notify(false), 1);
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
                        scope.spawn(move || {
                            for _ in 0..publications {
                                assert_eq!(waiters.publish(false, false), 0);
                            }
                        });
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
                scope.spawn(move || {
                    assert!(waiters.wait(waiters.generation(), Duration::from_secs(5)));
                    tx.send(Instant::now()).unwrap();
                });
                registered(waiters, 1);
                let start = Instant::now();
                assert_eq!(waiters.notify(false), 1);
                samples.push(rx.recv_timeout(Duration::from_secs(5)).unwrap() - start);
            });
        }
        samples.sort();
        println!("wake latency p50={:?} p99={:?}", samples[50], samples[99]);
        assert!(samples[50] < Duration::from_micros(100));
    }
}
