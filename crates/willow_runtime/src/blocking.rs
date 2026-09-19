//! Bounded blocking-work pool used to isolate file I/O and conservative foreign
//! calls from scheduler workers (willow-0a6k.5).
//!
//! The pool is bounded in BOTH dimensions (willow-9tls.6):
//!
//! * threads: `WILLOW_BLOCKING_THREADS` (default [`DEFAULT_BLOCKING_THREADS`]);
//! * queued-but-not-started jobs: `WILLOW_BLOCKING_QUEUE` (default
//!   [`DEFAULT_BLOCKING_QUEUE_PER_THREAD`] per thread).
//!
//! # Overload policy: backpressure on the submitting Task
//!
//! A runtime call that starts blocking work (`willow_fs_*_async`) only
//! constructs and schedules a Task; it must never suspend the caller and it has
//! no null/error return that generated code would check. So when the queue is
//! full the pool neither blocks the calling worker thread nor refuses the job:
//! [`try_submit`] hands the job back, the owning Task keeps it and parks in
//! `BlockedSyscall` as a slot waiter ([`try_submit_or_wait`]).
//!
//! Waiters are served oldest first through slot reservations: with `free`
//! slots open, the `free` oldest waiters each hold one. Every dequeue by a
//! pool thread opens one more slot and wakes the waiter that just gained a
//! reservation, so a burst of freed slots wakes as many waiters and they can
//! re-submit in any order without waiting for the oldest one to be polled. A
//! newcomer (or a waiter without a reservation) is admitted only when a slot
//! is free that no older waiter holds. Cancelling a waiter that held a
//! reservation passes it on ([`forget_waiter`]). "Oldest" means first poll
//! that failed to submit, not spawn order: a Task whose spawn-time submit was
//! refused is not registered until it is polled, and a later spawn may take
//! an unreserved slot in between.
//!
//! Work that has not been admitted stays owned by its Task, so cancelling that
//! Task drops the job without ever running it. A burst of blocking calls
//! therefore costs one parked Task per outstanding operation — the same as any
//! other parked Task — and only ever shows up as latency; the pool itself never
//! holds more than the queue capacity. The overload is observable through the
//! `blocking_queue_full` counter/event (recorded once per Task that parks
//! waiting for a slot, `value` = waiters after registration), the
//! `willow_blocking_queued_jobs` depth gauge, `willow_blocking_slot_waiters`,
//! and `willow_blocking_queue_capacity`.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, LazyLock, Mutex};

pub(crate) type BlockingWork = Box<dyn FnOnce() + Send + 'static>;

pub const DEFAULT_BLOCKING_THREADS: usize = 4;
/// Default queued-job capacity, per pool thread.
pub const DEFAULT_BLOCKING_QUEUE_PER_THREAD: usize = 16;

/// Foreign-call classification. Unknown declarations are conservative and use
/// the blocking pool until explicitly audited as non-blocking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForeignCallClass {
    NonBlocking,
    Blocking,
    Unknown,
}

impl ForeignCallClass {
    pub fn must_isolate(self) -> bool {
        !matches!(self, Self::NonBlocking)
    }
}

struct PoolState {
    /// Admitted jobs, `len() <= capacity`. Pool threads pop from the front.
    queue: VecDeque<BlockingWork>,
    /// Tasks whose job is still owned by the Task, oldest first. An entry is
    /// removed only by that Task (admission) or its cancel path. The first
    /// `capacity - queue.len()` entries each hold a slot reservation.
    waiters: VecDeque<u64>,
}

impl PoolState {
    fn free_slots(&self, capacity: usize) -> usize {
        capacity - self.queue.len()
    }

    /// The waiter holding the newest reservation: the one that gains a slot
    /// when `free_slots` has just grown by one, or that inherits the
    /// reservation a removed waiter held.
    fn last_reserved(&self, capacity: usize) -> Option<u64> {
        self.waiters
            .get(self.free_slots(capacity).checked_sub(1)?)
            .copied()
    }
}

struct BlockingPool {
    state: Mutex<PoolState>,
    work_ready: Condvar,
    capacity: usize,
}

fn env_count(name: &str) -> Option<usize> {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|count| *count > 0)
}

impl BlockingPool {
    fn new() -> Self {
        let threads = env_count("WILLOW_BLOCKING_THREADS").unwrap_or(DEFAULT_BLOCKING_THREADS);
        let capacity = env_count("WILLOW_BLOCKING_QUEUE")
            .unwrap_or_else(|| threads.saturating_mul(DEFAULT_BLOCKING_QUEUE_PER_THREAD));
        let pool = Self {
            state: Mutex::new(PoolState {
                queue: VecDeque::with_capacity(capacity),
                waiters: VecDeque::new(),
            }),
            work_ready: Condvar::new(),
            capacity,
        };
        for index in 0..threads {
            std::thread::Builder::new()
                .name(format!("willow-blocking-{index}"))
                .spawn(move || {
                    crate::stack_overflow::protect_current_thread();
                    loop {
                        let (work, freed_slot_for) = BLOCKING_POOL.dequeue();
                        // Wake outside the pool lock: waking takes scheduler
                        // locks and may hit a GC stress collection.
                        if let Some(task_id) = freed_slot_for {
                            crate::scheduler::willow_sched_wake(task_id);
                        }
                        ACTIVE_JOBS.fetch_add(1, Ordering::AcqRel);
                        work();
                        ACTIVE_JOBS.fetch_sub(1, Ordering::AcqRel);
                        COMPLETED_JOBS.fetch_add(1, Ordering::AcqRel);
                    }
                })
                .expect("failed to start Willow blocking worker");
        }
        pool
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PoolState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Block a pool thread until a job is queued, then take it. Returns the
    /// waiter that gains a reservation on the freed slot, if any, so the
    /// caller can wake it after unlocking.
    fn dequeue(&self) -> (BlockingWork, Option<u64>) {
        let mut state = self.lock();
        loop {
            if let Some(work) = state.queue.pop_front() {
                QUEUED_JOBS.fetch_sub(1, Ordering::AcqRel);
                return (work, state.last_reserved(self.capacity));
            }
            state = self
                .work_ready
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    /// Admit `work` unless every free slot is reserved for an older waiter.
    /// `registered` is the submitter's index in `waiters`; an unregistered
    /// submitter counts as standing behind every waiter. Admitting a waiter
    /// consumes its own reservation, so no other waiter's changes.
    fn admit(
        &self,
        state: &mut PoolState,
        registered: Option<usize>,
        work: BlockingWork,
    ) -> Result<(), BlockingWork> {
        let position = registered.unwrap_or(state.waiters.len());
        if position >= state.free_slots(self.capacity) {
            return Err(work);
        }
        if let Some(index) = registered {
            state.waiters.remove(index);
            SLOT_WAITERS.store(state.waiters.len(), Ordering::Release);
        }
        state.queue.push_back(work);
        QUEUED_JOBS.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    fn try_submit(&self, work: BlockingWork) -> Result<(), BlockingWork> {
        let mut state = self.lock();
        self.admit(&mut state, None, work)?;
        drop(state);
        self.work_ready.notify_one();
        Ok(())
    }

    fn try_submit_or_wait(&self, task_id: u64, work: BlockingWork) -> Result<(), BlockingWork> {
        let mut state = self.lock();
        let registered = state.waiters.iter().position(|id| *id == task_id);
        match self.admit(&mut state, registered, work) {
            Ok(()) => {
                drop(state);
                self.work_ready.notify_one();
                Ok(())
            }
            Err(work) => {
                // A spurious wake re-polls a registered waiter; keep one entry.
                if registered.is_none() {
                    state.waiters.push_back(task_id);
                    let waiters = state.waiters.len();
                    SLOT_WAITERS.store(waiters, Ordering::Release);
                    drop(state);
                    crate::observability::record(
                        crate::observability::RuntimeEventKind::BlockingQueueFull,
                        None,
                        task_id,
                        waiters as i64,
                    );
                }
                Err(work)
            }
        }
    }

    fn forget_waiter(&self, task_id: u64) {
        let mut state = self.lock();
        let Some(index) = state.waiters.iter().position(|id| *id == task_id) else {
            return;
        };
        state.waiters.remove(index);
        SLOT_WAITERS.store(state.waiters.len(), Ordering::Release);
        // A removed waiter that held a reservation may already have been
        // woken for it; pass the reservation on so nobody is stranded.
        let next = if index < state.free_slots(self.capacity) {
            state.last_reserved(self.capacity)
        } else {
            None
        };
        drop(state);
        if let Some(next) = next {
            crate::scheduler::willow_sched_wake(next);
        }
    }
}

static BLOCKING_POOL: LazyLock<BlockingPool> = LazyLock::new(BlockingPool::new);
static ACTIVE_JOBS: AtomicUsize = AtomicUsize::new(0);
static QUEUED_JOBS: AtomicUsize = AtomicUsize::new(0);
static SLOT_WAITERS: AtomicUsize = AtomicUsize::new(0);
static COMPLETED_JOBS: AtomicU64 = AtomicU64::new(0);

/// Queue `work` if the bounded queue has a slot that no waiting Task holds a
/// reservation on; otherwise hand it back so the caller's Task can keep it and
/// wait via [`try_submit_or_wait`]. Never blocks the calling thread.
pub(crate) fn try_submit(work: BlockingWork) -> Result<(), BlockingWork> {
    BLOCKING_POOL.try_submit(work)
}

/// Re-submit a job that `try_submit` handed back. On `Err` the Task has been
/// registered (once) as a slot waiter and must park in `BlockedSyscall`; the
/// pool wakes it once a freed slot is reserved for it. Idempotent for spurious
/// re-polls of a registered waiter.
pub(crate) fn try_submit_or_wait(task_id: u64, work: BlockingWork) -> Result<(), BlockingWork> {
    BLOCKING_POOL.try_submit_or_wait(task_id, work)
}

/// Drop a Task's slot-waiter registration (cancel path). No-op for a Task that
/// never had to wait.
pub(crate) fn forget_waiter(task_id: u64) {
    BLOCKING_POOL.forget_waiter(task_id)
}

/// Forget every slot waiter. Test resets drop tasks without running their
/// cancel path, and the process-global pool would otherwise keep their ids
/// (which a reset scheduler even reuses) reserving slots forever.
#[cfg(test)]
pub(crate) fn reset_slot_waiters_for_test() {
    let Some(pool) = LazyLock::get(&BLOCKING_POOL) else {
        return;
    };
    let mut state = pool.lock();
    state.waiters.clear();
    SLOT_WAITERS.store(0, Ordering::Release);
}

/// Jobs currently running on pool threads.
#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_active_jobs() -> i64 {
    ACTIVE_JOBS.load(Ordering::Acquire) as i64
}

/// Jobs that have finished running on pool threads (monotonic).
#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_completed_jobs() -> i64 {
    COMPLETED_JOBS.load(Ordering::Acquire) as i64
}

/// Jobs admitted to the bounded queue but not yet started (depth gauge,
/// `0..=willow_blocking_queue_capacity()`).
#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_queued_jobs() -> i64 {
    QUEUED_JOBS.load(Ordering::Acquire) as i64
}

/// Tasks parked because no queue slot was available when they submitted
/// (gauge).
#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_slot_waiters() -> i64 {
    SLOT_WAITERS.load(Ordering::Acquire) as i64
}

/// Bounded queue capacity (`WILLOW_BLOCKING_QUEUE`).
#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_queue_capacity() -> i64 {
    BLOCKING_POOL.capacity as i64
}

/// Helpers for tests that must fill the process-global pool (also used by the
/// `fs` overload test).
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Every pool thread parked inside a job until released, plus a step gate
    /// that "gated" jobs block on. Releasing ONE thread leaves a single
    /// consumer that takes jobs in FIFO order, and with only gated jobs
    /// queued each [`Self::step`] lets it finish exactly one job and dequeue
    /// the next, so the test controls exactly when one slot frees. Drop
    /// releases everything and forgets any waiters so a failing test does
    /// not wedge the pool for later tests.
    pub(crate) struct StalledPool {
        threads: Vec<mpsc::Sender<()>>,
        step_tx: Option<mpsc::Sender<()>>,
        step_rx: Arc<Mutex<mpsc::Receiver<()>>>,
    }

    impl StalledPool {
        /// Let one stalled thread out of its stall job.
        pub(crate) fn release_one(&mut self) {
            if let Some(gate) = self.threads.pop() {
                let _ = gate.send(());
            }
        }

        pub(crate) fn release_all(&mut self) {
            while !self.threads.is_empty() {
                self.release_one();
            }
            self.open_gate();
        }

        /// A job that runs `before`, then blocks its thread until the next
        /// [`Self::step`] (or until the gate is opened).
        pub(crate) fn gated_job(&self, before: impl FnOnce() + Send + 'static) -> BlockingWork {
            let step_rx = Arc::clone(&self.step_rx);
            Box::new(move || {
                before();
                let _ = step_rx
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .recv();
            })
        }

        /// Queue `count` gated filler jobs. Must fit: the caller keeps
        /// `count` within the free capacity.
        pub(crate) fn fill_queue_gated(&self, count: usize) {
            for _ in 0..count {
                try_submit(self.gated_job(|| {}))
                    .ok()
                    .expect("gated filler fits");
            }
        }

        /// Let one gated job finish, so its thread dequeues one more job.
        pub(crate) fn step(&self) {
            if let Some(step_tx) = &self.step_tx {
                let _ = step_tx.send(());
            }
        }

        /// Let every gated job (queued now or later) finish without a step,
        /// while leaving stalled threads stalled.
        pub(crate) fn open_gate(&mut self) {
            self.step_tx = None;
        }
    }

    impl Drop for StalledPool {
        fn drop(&mut self) {
            self.release_all();
            reset_slot_waiters_for_test();
        }
    }

    /// Pool thread count as the pool itself resolves it.
    pub(crate) fn pool_threads() -> usize {
        env_count("WILLOW_BLOCKING_THREADS").unwrap_or(DEFAULT_BLOCKING_THREADS)
    }

    /// Block every pool thread until released. Returns once all threads are
    /// inside a job, so the queue is empty and nothing can drain. Stale
    /// waiters left by an earlier failed test are dropped first.
    pub(crate) fn stall_pool_threads() -> StalledPool {
        let threads = pool_threads();
        reset_slot_waiters_for_test();
        wait_until("pool to be idle", || willow_blocking_queued_jobs() == 0);
        let mut gates = Vec::with_capacity(threads);
        for _ in 0..threads {
            // One at a time: the queue may hold fewer jobs than there are
            // threads, so each stall job must be dequeued (slot freed) before
            // the next can be submitted.
            let (started_tx, started_rx) = mpsc::channel::<()>();
            let (gate, wait) = mpsc::channel::<()>();
            gates.push(gate);
            let mut job: BlockingWork = Box::new(move || {
                started_tx.send(()).unwrap();
                let _ = wait.recv();
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while let Err(returned) = try_submit(job) {
                assert!(Instant::now() < deadline, "stall job could not be queued");
                job = returned;
                std::thread::yield_now();
            }
            started_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("stall job did not start");
        }
        let (step_tx, step_rx) = mpsc::channel::<()>();
        StalledPool {
            threads: gates,
            step_tx: Some(step_tx),
            step_rx: Arc::new(Mutex::new(step_rx)),
        }
    }

    pub(crate) fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::yield_now();
        }
    }

    pub(crate) fn queue_full_metric() -> u64 {
        use crate::observability::{WillowRuntimeMetricsV1, willow_runtime_metrics_snapshot_v1};
        let mut snapshot = WillowRuntimeMetricsV1::default();
        willow_runtime_metrics_snapshot_v1(&mut snapshot);
        snapshot.blocking_queue_full
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::gc::runtime_test_guard;
    use crate::scheduler::{
        reset_global_scheduler_for_test, willow_monotonic_millis, willow_sched_run_until_deadline,
        willow_sched_spawn, willow_sched_task_state,
    };
    use std::ffi::c_void;
    use std::sync::mpsc;
    use std::time::Duration;

    const READY: i32 = 0;
    const BLOCKED_SYSCALL: i32 = 7;

    /// Stand-in for a parked fs Task: parks BlockedSyscall on every poll, so
    /// a pool wake is observable as the Ready state.
    unsafe extern "C" fn parks_forever(_frame: *mut c_void) -> i32 {
        crate::task::RUNTIME_POLL_BLOCKED_SYSCALL
    }

    /// Spawn `count` stand-in Tasks and park them all.
    fn parked_tasks(count: usize) -> Vec<u64> {
        let ids: Vec<u64> = (0..count)
            .map(|_| willow_sched_spawn(parks_forever, std::ptr::null_mut()))
            .collect();
        willow_sched_run_until_deadline(willow_monotonic_millis() + 200);
        for id in &ids {
            assert_eq!(willow_sched_task_state(*id), BLOCKED_SYSCALL, "task {id}");
        }
        ids
    }

    fn queued() -> usize {
        willow_blocking_queued_jobs() as usize
    }

    #[test]
    fn blocking_pool_executes_submitted_work() {
        let _guard = runtime_test_guard();
        let (sender, receiver) = mpsc::channel();
        assert!(try_submit(Box::new(move || sender.send(42).unwrap())).is_ok());
        assert_eq!(receiver.recv_timeout(Duration::from_secs(2)), Ok(42));
    }

    #[test]
    fn unknown_foreign_calls_are_conservatively_blocking() {
        assert!(!ForeignCallClass::NonBlocking.must_isolate());
        assert!(ForeignCallClass::Blocking.must_isolate());
        assert!(ForeignCallClass::Unknown.must_isolate());
    }

    #[test]
    fn queue_capacity_defaults_per_thread_and_is_positive() {
        let capacity = willow_blocking_queue_capacity();
        assert!(capacity > 0);
        if std::env::var_os("WILLOW_BLOCKING_QUEUE").is_none() {
            let threads = env_count("WILLOW_BLOCKING_THREADS").unwrap_or(DEFAULT_BLOCKING_THREADS);
            assert_eq!(
                capacity as usize,
                threads * DEFAULT_BLOCKING_QUEUE_PER_THREAD
            );
        }
    }

    /// The ticket's acceptance test: with every pool thread stalled, submit
    /// more jobs than the queue holds. Exactly `capacity` are queued; the rest
    /// are handed back (not dropped, not run, the caller not blocked) and the
    /// depth gauge never exceeds the capacity. Releasing the threads runs the
    /// queued jobs in FIFO order.
    #[test]
    fn overflow_hands_jobs_back_instead_of_growing_the_queue() {
        let _guard = runtime_test_guard();
        let mut stalled = stall_pool_threads();
        let capacity = willow_blocking_queue_capacity() as usize;
        assert_eq!(queued(), 0);

        let (sender, receiver) = mpsc::channel::<usize>();
        let mut refused = Vec::new();
        for index in 0..capacity + 3 {
            let sender = sender.clone();
            match try_submit(Box::new(move || sender.send(index).unwrap())) {
                Ok(()) => assert!(index < capacity, "job {index} exceeded the capacity"),
                Err(work) => {
                    assert!(index >= capacity, "job {index} was refused with room left");
                    refused.push(work);
                }
            }
            assert!(queued() <= capacity);
        }
        assert_eq!(refused.len(), 3);
        assert_eq!(queued(), capacity);
        assert_eq!(willow_blocking_slot_waiters(), 0, "no Task waited");

        stalled.release_one();
        let mut order = Vec::new();
        for _ in 0..capacity {
            order.push(receiver.recv_timeout(Duration::from_secs(5)).unwrap());
        }
        assert_eq!(order, (0..capacity).collect::<Vec<_>>(), "FIFO");
        assert_eq!(
            receiver.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "refused jobs must not have run"
        );
        drop(refused);
        wait_until("queue to drain", || queued() == 0);
    }

    /// Slot waiters: registration is FIFO and idempotent, the queue-full
    /// counter/event fires once per Task, the first freed slot is reserved
    /// for (and wakes) only the oldest waiter, and neither a younger waiter
    /// nor a newcomer can take it.
    #[test]
    fn slot_waiters_are_admitted_oldest_first() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let mut stalled = stall_pool_threads();
        let capacity = willow_blocking_queue_capacity() as usize;
        stalled.fill_queue_gated(capacity);
        let (sender, receiver) = mpsc::channel::<&'static str>();
        let ids = parked_tasks(2);
        let (first, second) = (ids[0], ids[1]);

        let before = queue_full_metric();
        let first_job = try_submit_or_wait(first, {
            let sender = sender.clone();
            stalled.gated_job(move || sender.send("first").unwrap())
        })
        .expect_err("queue is full");
        assert_eq!(queue_full_metric(), before + 1);
        assert_eq!(willow_blocking_slot_waiters(), 1);
        // A spurious re-poll re-submits; still one entry, no second event.
        let first_job = try_submit_or_wait(first, first_job).expect_err("still full");
        assert_eq!(queue_full_metric(), before + 1);
        assert_eq!(willow_blocking_slot_waiters(), 1);
        let second_job = try_submit_or_wait(second, {
            let sender = sender.clone();
            stalled.gated_job(move || sender.send("second").unwrap())
        })
        .expect_err("queue is full");
        assert_eq!(queue_full_metric(), before + 2);
        assert_eq!(willow_blocking_slot_waiters(), 2);

        // One slot frees: reserved for the oldest waiter, which is woken.
        stalled.release_one();
        wait_until("a slot to free", || queued() == capacity - 1);
        wait_until("oldest waiter to be woken", || {
            willow_sched_task_state(first) == READY
        });
        assert_eq!(willow_sched_task_state(second), BLOCKED_SYSCALL);
        let newcomer = try_submit(stalled.gated_job(|| {})).expect_err("reserved for a waiter");
        let second_job = try_submit_or_wait(second, second_job).expect_err("not the oldest");
        assert_eq!(willow_blocking_slot_waiters(), 2);
        try_submit_or_wait(first, first_job)
            .ok()
            .expect("oldest waiter takes the freed slot");
        assert_eq!(willow_blocking_slot_waiters(), 1);
        assert_eq!(queued(), capacity);

        // The next freed slot goes to the second waiter, then the newcomer.
        stalled.step();
        wait_until("second to become admissible", || queued() == capacity - 1);
        wait_until("second waiter to be woken", || {
            willow_sched_task_state(second) == READY
        });
        let newcomer = try_submit(newcomer).expect_err("still reserved for a waiter");
        try_submit_or_wait(second, second_job)
            .ok()
            .expect("second waiter next");
        assert_eq!(willow_blocking_slot_waiters(), 0);
        stalled.step();
        wait_until("newcomer to become admissible", || queued() == capacity - 1);
        try_submit(newcomer).ok().expect("no waiters left");

        // The lone consumer runs what remains in queue order.
        stalled.open_gate();
        let mut seen = Vec::new();
        for _ in 0..2 {
            seen.push(receiver.recv_timeout(Duration::from_secs(5)).unwrap());
        }
        assert_eq!(seen, ["first", "second"], "FIFO admission");
        stalled.release_all();
        wait_until("queue to drain", || queued() == 0);
        reset_global_scheduler_for_test();
    }

    /// No head-of-line blocking: each freed slot is reserved for the next
    /// waiter in line and wakes it, so several waiters can be admitted before
    /// the oldest one polls; slots no waiter holds are open to newcomers.
    #[test]
    fn each_freed_slot_is_reserved_for_and_wakes_the_next_waiter() {
        let _guard = runtime_test_guard();
        let capacity = willow_blocking_queue_capacity() as usize;
        if capacity < 2 {
            eprintln!("skipped: needs WILLOW_BLOCKING_QUEUE >= 2 to free two slots at once");
            return;
        }
        reset_global_scheduler_for_test();
        let mut stalled = stall_pool_threads();
        stalled.fill_queue_gated(capacity);
        let ids = parked_tasks(3);
        let (first, second, third) = (ids[0], ids[1], ids[2]);
        let first_job = try_submit_or_wait(first, stalled.gated_job(|| {})).expect_err("full");
        let second_job = try_submit_or_wait(second, stalled.gated_job(|| {})).expect_err("full");
        let third_job = try_submit_or_wait(third, stalled.gated_job(|| {})).expect_err("full");
        assert_eq!(willow_blocking_slot_waiters(), 3);

        // Two slots free while nobody polls: two distinct waiters are woken.
        stalled.release_one();
        wait_until("first slot", || queued() == capacity - 1);
        stalled.step();
        wait_until("second slot", || queued() == capacity - 2);
        wait_until("two waiters to be woken", || {
            willow_sched_task_state(first) == READY && willow_sched_task_state(second) == READY
        });
        assert_eq!(willow_sched_task_state(third), BLOCKED_SYSCALL);

        // The third waiter and a newcomer hold no reservation.
        let third_job = try_submit_or_wait(third, third_job).expect_err("both slots reserved");
        let newcomer = try_submit(stalled.gated_job(|| {})).expect_err("both slots reserved");
        // The second waiter need not wait for the first to be polled.
        try_submit_or_wait(second, second_job)
            .ok()
            .expect("reserved slot");
        assert_eq!(willow_blocking_slot_waiters(), 2);
        let third_job = try_submit_or_wait(third, third_job).expect_err("one slot, reserved");
        let newcomer = try_submit(newcomer).expect_err("one slot, reserved");
        try_submit_or_wait(first, first_job)
            .ok()
            .expect("reserved slot");
        assert_eq!(willow_blocking_slot_waiters(), 1);
        assert_eq!(queued(), capacity);

        // Two more slots: one reserved for the last waiter, one open.
        stalled.step();
        wait_until("third slot", || queued() == capacity - 1);
        wait_until("third waiter to be woken", || {
            willow_sched_task_state(third) == READY
        });
        let newcomer = try_submit(newcomer).expect_err("reserved for the last waiter");
        stalled.step();
        wait_until("fourth slot", || queued() == capacity - 2);
        try_submit(newcomer)
            .ok()
            .expect("a slot no waiter holds is open to newcomers");
        try_submit_or_wait(third, third_job)
            .ok()
            .expect("its reservation survived the newcomer");
        assert_eq!(willow_blocking_slot_waiters(), 0);

        stalled.release_all();
        wait_until("queue to drain", || queued() == 0);
        reset_global_scheduler_for_test();
    }

    /// A cancelled waiter is removed and never runs; if it held a reservation
    /// that passes (with a wake) to the next waiter instead of stranding it.
    #[test]
    fn forgetting_a_reserved_waiter_passes_the_slot_on() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let mut stalled = stall_pool_threads();
        let capacity = willow_blocking_queue_capacity() as usize;
        stalled.fill_queue_gated(capacity);
        let ids = parked_tasks(2);
        let (doomed, survivor) = (ids[0], ids[1]);
        let doomed_job =
            try_submit_or_wait(doomed, Box::new(|| unreachable!("cancelled"))).expect_err("full");
        let (sender, receiver) = mpsc::channel::<()>();
        let survivor_job = try_submit_or_wait(survivor, Box::new(move || sender.send(()).unwrap()))
            .expect_err("full");
        assert_eq!(willow_blocking_slot_waiters(), 2);

        // Forgetting a waiter without a reservation wakes nobody.
        forget_waiter(survivor);
        assert_eq!(willow_sched_task_state(doomed), BLOCKED_SYSCALL);
        let survivor_job = try_submit_or_wait(survivor, survivor_job).expect_err("re-registered");
        assert_eq!(willow_blocking_slot_waiters(), 2);

        stalled.release_one();
        wait_until("a slot to free", || queued() == capacity - 1);
        wait_until("doomed to be woken", || {
            willow_sched_task_state(doomed) == READY
        });
        assert_eq!(willow_sched_task_state(survivor), BLOCKED_SYSCALL);

        forget_waiter(doomed);
        drop(doomed_job);
        forget_waiter(doomed);
        assert_eq!(
            willow_blocking_slot_waiters(),
            1,
            "second forget is a no-op"
        );
        assert_eq!(
            willow_sched_task_state(survivor),
            READY,
            "the reservation passed to the survivor"
        );
        try_submit_or_wait(survivor, survivor_job)
            .ok()
            .expect("survivor holds the reservation");
        assert_eq!(willow_blocking_slot_waiters(), 0);

        stalled.release_all();
        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("survivor's job ran");
        wait_until("queue to drain", || queued() == 0);
        reset_global_scheduler_for_test();
    }
}
