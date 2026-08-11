use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::lock_wait::{LockId, LockWaitLink, RegistrationToken};
use crate::task::{
    RUNTIME_POLL_BLOCKED_SYSCALL, RUNTIME_POLL_PANICKED, RUNTIME_POLL_PENDING,
    RUNTIME_POLL_PREEMPTED, RUNTIME_POLL_READY, RUNTIME_POLL_YIELD, RuntimeCancelFn, RuntimePollFn,
    RuntimeTask, RuntimeTaskId, RuntimeTaskState,
};
use crate::task_state::{BoundaryOutcome, CancelOutcome, ClaimOutcome, TaskLifecycle, WakeOutcome};
use crate::timer_queue::{TimerQueue, TimerWake};

/// Lock-free half of terminal task cleanup. The task record is already gone
/// when this runs, so channel addresses and the exact lock reverse link must be
/// captured before removal (willow-ezs.1.4, willow-38w.1.6).
#[derive(Debug)]
struct TerminalCleanup {
    task_id: RuntimeTaskId,
    channel_waits: Vec<usize>,
    lock_wait: Option<LockWaitLink>,
}

/// Deterministic scheduler metadata counters used by the repeated-10k
/// acceptance suite. These deliberately count scheduler ownership, not RSS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerMetadataSnapshot {
    pub heavy_tasks: usize,
    pub queue_entries: usize,
    pub pending_cleanups: usize,
    pub frame_roots: usize,
    pub blocked_syscalls: usize,
}

pub const DEFAULT_WORKERS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PollOutcome {
    Pending,
    Ready,
    Yield,
    Preempted,
    Panicked,
    BlockedSyscall,
    Invalid(i32),
}

type ClaimedTaskWork = Option<(RuntimePollFn, *mut c_void, *const c_void)>;

fn classify_poll_result(result: i32) -> PollOutcome {
    match result {
        RUNTIME_POLL_PENDING => PollOutcome::Pending,
        RUNTIME_POLL_READY => PollOutcome::Ready,
        RUNTIME_POLL_YIELD => PollOutcome::Yield,
        RUNTIME_POLL_PREEMPTED => PollOutcome::Preempted,
        RUNTIME_POLL_PANICKED => PollOutcome::Panicked,
        RUNTIME_POLL_BLOCKED_SYSCALL => PollOutcome::BlockedSyscall,
        other => PollOutcome::Invalid(other),
    }
}

/// The frame-header status code for a terminal task state, or `None` while the
/// task can still run (willow-ezs.1.3).
fn terminal_frame_status(state: RuntimeTaskState) -> Option<i64> {
    match state {
        RuntimeTaskState::Completed => Some(crate::async_frame::WILLOW_FRAME_STATUS_COMPLETED),
        RuntimeTaskState::Cancelled => Some(crate::async_frame::WILLOW_FRAME_STATUS_CANCELLED),
        RuntimeTaskState::Panicked => Some(crate::async_frame::WILLOW_FRAME_STATUS_PANICKED),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeWorkerConfig {
    requested_workers: usize,
    active_workers: usize,
}

impl RuntimeWorkerConfig {
    fn from_env_value(value: Option<&str>, default_workers: usize) -> Self {
        let default_workers = default_workers.max(DEFAULT_WORKERS);
        let env_workers = value
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .filter(|workers| *workers > 0)
            .map(|workers| workers.max(DEFAULT_WORKERS));
        let requested_workers = env_workers.unwrap_or(default_workers);

        Self {
            requested_workers,
            active_workers: requested_workers,
        }
    }

    pub fn requested_workers(self) -> usize {
        self.requested_workers
    }

    pub fn active_workers(self) -> usize {
        self.active_workers
    }

    #[cfg(test)]
    fn single_worker() -> Self {
        Self {
            requested_workers: 1,
            active_workers: 1,
        }
    }
}

/// Set by the worker that is finalizing an unhandled language panic (or a poll
/// ABI violation) and will abort the process. Spec §23 requires the terminal
/// publication, relationship detach and root release to happen before the
/// abort, and that publication wakes the panicked task's awaiters — so the
/// claim path must stop handing out work first, or a sibling worker runs
/// ordinary code past `await <panicked task>` in the window before the abort
/// (willow-s9ej.7).
static FATAL_PANIC_PENDING: AtomicBool = AtomicBool::new(false);

/// Close the claim gate. Only the fatal path calls this, and that path always
/// ends in `std::process::abort`, so the gate is never reopened.
fn begin_fatal_panic() {
    FATAL_PANIC_PENDING.store(true, Ordering::Release);
}

fn fatal_panic_pending() -> bool {
    FATAL_PANIC_PENDING.load(Ordering::Acquire)
}

/// Stop this worker for good: the thread that closed the gate is formatting an
/// unhandled-panic report and ends in `std::process::abort`. Waiting is the
/// point — the woken awaiters of the panicked task must never get a turn.
fn park_until_fatal_abort() -> ! {
    loop {
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Nesting depth of live `SingleWorkerForTest` guards.
#[cfg(test)]
static TEST_SINGLE_WORKER: AtomicUsize = AtomicUsize::new(0);

/// Test-only: make the worker config report a single worker while the guard is
/// alive, so process-global drives take the single-threaded run loop.
///
/// `from_env_value` clamps every override up to `DEFAULT_WORKERS`, so a test
/// cannot request a single-threaded drive through `WILLOW_WORKERS`. A test that
/// asserts on *which* tasks one drive reaped needs one: with the pool, a second
/// worker can claim a task that the drive itself woke — a terminal purge
/// compensating a cancelled channel handoff, say — and complete it before the
/// run loop observes that its target is already done, so the drive returns a
/// completion count the test never asked for (willow-tcrg).
///
/// Hold this alongside `crate::gc::runtime_test_guard()`, and install it before
/// `reset_global_scheduler_for_test()` so the fresh run queues are sized for one
/// worker.
#[cfg(test)]
pub struct SingleWorkerForTest(());

#[cfg(test)]
pub fn single_worker_for_test() -> SingleWorkerForTest {
    TEST_SINGLE_WORKER.fetch_add(1, Ordering::AcqRel);
    SingleWorkerForTest(())
}

#[cfg(test)]
impl Drop for SingleWorkerForTest {
    fn drop(&mut self) {
        let previous = TEST_SINGLE_WORKER.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "single-worker test guard depth underflow");
    }
}

pub fn runtime_worker_config() -> RuntimeWorkerConfig {
    #[cfg(test)]
    if TEST_SINGLE_WORKER.load(Ordering::Acquire) > 0 {
        return RuntimeWorkerConfig::single_worker();
    }
    RuntimeWorkerConfig::from_env_value(
        std::env::var("WILLOW_WORKERS").ok().as_deref(),
        DEFAULT_WORKERS,
    )
}

/// Independently synchronized run queues (willow-8agm).
///
/// The process-global scheduler no longer owns queue storage behind its task
/// metadata mutex. Workers pop/steal here first, then take the task-table lock
/// only to validate the atomic state and acquire the frame. Publishers take a
/// short queue lock only after the state CAS has granted one queue token.
#[derive(Debug)]
struct RunQueues {
    locals: Vec<Mutex<VecDeque<RuntimeTaskId>>>,
    /// Alternate local and overflow priority per worker. Always preferring the
    /// local queue can starve newly spawned or externally woken tasks when
    /// every worker repeatedly requeues CPU-bound work to itself.
    prefer_global: Vec<AtomicBool>,
    global: Mutex<VecDeque<RuntimeTaskId>>,
}

impl RunQueues {
    fn new(worker_count: usize) -> Self {
        let worker_count = worker_count.max(1);
        Self {
            locals: (0..worker_count)
                .map(|_| Mutex::new(VecDeque::new()))
                .collect(),
            prefer_global: (0..worker_count).map(|_| AtomicBool::new(false)).collect(),
            global: Mutex::new(VecDeque::new()),
        }
    }

    fn lock(
        queue: &Mutex<VecDeque<RuntimeTaskId>>,
    ) -> std::sync::MutexGuard<'_, VecDeque<RuntimeTaskId>> {
        queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn worker_count(&self) -> usize {
        self.locals.len()
    }

    fn push_global(&self, id: RuntimeTaskId) {
        Self::lock(&self.global).push_back(id);
    }

    fn push_local(&self, worker: usize, id: RuntimeTaskId) {
        match self.locals.get(worker) {
            Some(queue) => Self::lock(queue).push_back(id),
            None => self.push_global(id),
        }
    }

    #[cfg(test)]
    fn push_local_front(&self, worker: usize, id: RuntimeTaskId) {
        match self.locals.get(worker) {
            Some(queue) => Self::lock(queue).push_front(id),
            None => Self::lock(&self.global).push_front(id),
        }
    }

    #[cfg(test)]
    fn force_push_global(&self, id: RuntimeTaskId) {
        Self::lock(&self.global).push_back(id);
    }

    fn pop_for_worker(&self, worker: usize) -> Option<RuntimeTaskId> {
        let prefer_global = self
            .prefer_global
            .get(worker)
            .map(|preference| preference.fetch_xor(true, Ordering::Relaxed))
            .unwrap_or(true);
        if prefer_global {
            if let Some(id) = Self::lock(&self.global).pop_front() {
                return Some(id);
            }
            if let Some(queue) = self.locals.get(worker)
                && let Some(id) = Self::lock(queue).pop_front()
            {
                return Some(id);
            }
        } else {
            if let Some(queue) = self.locals.get(worker)
                && let Some(id) = Self::lock(queue).pop_front()
            {
                return Some(id);
            }
            if let Some(id) = Self::lock(&self.global).pop_front() {
                return Some(id);
            }
        }
        let count = self.locals.len();
        for offset in 1..count {
            let victim = (worker + offset) % count;
            if let Some(id) = Self::lock(&self.locals[victim]).pop_back() {
                return Some(id);
            }
        }
        None
    }

    fn remove(&self, id: RuntimeTaskId) -> bool {
        {
            let mut global = Self::lock(&self.global);
            if let Some(index) = global.iter().position(|queued| *queued == id) {
                global.remove(index);
                return true;
            }
        }
        for queue in &self.locals {
            let mut queue = Self::lock(queue);
            if let Some(index) = queue.iter().position(|queued| *queued == id) {
                queue.remove(index);
                return true;
            }
        }
        false
    }

    fn len(&self) -> usize {
        let global = Self::lock(&self.global).len();
        global
            + self
                .locals
                .iter()
                .map(|queue| Self::lock(queue).len())
                .sum::<usize>()
    }

    fn contains(&self, id: RuntimeTaskId) -> bool {
        Self::lock(&self.global).contains(&id)
            || self
                .locals
                .iter()
                .any(|queue| Self::lock(queue).contains(&id))
    }

    #[cfg(test)]
    fn snapshot(&self) -> Vec<RuntimeTaskId> {
        let mut ids = Self::lock(&self.global).iter().copied().collect::<Vec<_>>();
        for queue in &self.locals {
            ids.extend(Self::lock(queue).iter().copied());
        }
        ids
    }

    fn clear(&self) {
        Self::lock(&self.global).clear();
        for queue in &self.locals {
            Self::lock(queue).clear();
        }
    }
}

const TASK_TABLE_SHARDS: usize = 32;

/// Task records partitioned by task id (willow-6qtv).
///
/// A one-task operation takes one shard. A relationship transaction takes the
/// two participating shards in ascending index order; same-shard transactions
/// take one lock. The scheduler metadata mutex is therefore no longer the task
/// table's ownership lock.
#[derive(Debug)]
struct ShardedTaskTable {
    shards: Vec<Mutex<HashMap<RuntimeTaskId, RuntimeTask>>>,
    /// Exact O(1) summary of tasks in `BlockedSyscall`.
    ///
    /// This lives beside the sharded task states instead of in
    /// `RuntimeScheduler`, so a state transition and its accounting can be
    /// published while the same task shard is locked. Readers use it without
    /// taking the scheduler metadata mutex.
    blocked_syscall: AtomicUsize,
}

impl ShardedTaskTable {
    fn new() -> Self {
        Self {
            shards: (0..TASK_TABLE_SHARDS)
                .map(|_| Mutex::new(HashMap::new()))
                .collect(),
            blocked_syscall: AtomicUsize::new(0),
        }
    }

    fn shard_index(&self, id: RuntimeTaskId) -> usize {
        id as usize % self.shards.len()
    }

    fn lock_shard(
        &self,
        index: usize,
    ) -> std::sync::MutexGuard<'_, HashMap<RuntimeTaskId, RuntimeTask>> {
        self.shards[index]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn with<R>(&self, id: RuntimeTaskId, read: impl FnOnce(&RuntimeTask) -> R) -> Option<R> {
        let shard = self.lock_shard(self.shard_index(id));
        shard.get(&id).map(read)
    }

    fn with_mut<R>(
        &self,
        id: RuntimeTaskId,
        mutate: impl FnOnce(&mut RuntimeTask) -> R,
    ) -> Option<R> {
        let mut shard = self.lock_shard(self.shard_index(id));
        shard.get_mut(&id).map(mutate)
    }

    fn with_two_mut<R>(
        &self,
        first_id: RuntimeTaskId,
        second_id: RuntimeTaskId,
        mutate: impl FnOnce(Option<&mut RuntimeTask>, Option<&mut RuntimeTask>) -> R,
    ) -> R {
        let first_shard = self.shard_index(first_id);
        let second_shard = self.shard_index(second_id);
        if first_shard == second_shard {
            let mut shard = self.lock_shard(first_shard);
            if first_id == second_id {
                let first = shard.get_mut(&first_id);
                return mutate(first, None);
            }
            // Temporarily remove the second record so Rust can hand the
            // transaction two disjoint mutable references under one shard lock.
            let mut second = shard.remove(&second_id);
            let result = mutate(shard.get_mut(&first_id), second.as_mut());
            if let Some(second) = second {
                shard.insert(second_id, second);
            }
            return result;
        }

        let (low_index, high_index) = if first_shard < second_shard {
            (first_shard, second_shard)
        } else {
            (second_shard, first_shard)
        };
        let mut low = self.lock_shard(low_index);
        let mut high = self.lock_shard(high_index);
        if first_shard == low_index {
            mutate(low.get_mut(&first_id), high.get_mut(&second_id))
        } else {
            mutate(high.get_mut(&first_id), low.get_mut(&second_id))
        }
    }

    fn insert(&self, id: RuntimeTaskId, task: RuntimeTask) {
        self.lock_shard(self.shard_index(id)).insert(id, task);
    }

    fn remove(&self, id: RuntimeTaskId) -> Option<RuntimeTask> {
        self.lock_shard(self.shard_index(id)).remove(&id)
    }

    fn len(&self) -> usize {
        self.shards
            .iter()
            .enumerate()
            .map(|(index, _)| self.lock_shard(index).len())
            .sum()
    }

    fn snapshots(&self) -> Vec<RuntimeTask> {
        self.shards
            .iter()
            .enumerate()
            .flat_map(|(index, _)| self.lock_shard(index).values().cloned().collect::<Vec<_>>())
            .collect()
    }

    fn drain(&self) -> Vec<RuntimeTask> {
        let mut tasks = Vec::new();
        for index in 0..self.shards.len() {
            tasks.extend(self.lock_shard(index).drain().map(|(_, task)| task));
        }
        self.blocked_syscall.store(0, Ordering::Release);
        tasks
    }

    fn reconcile_blocked_transition(&self, before: TaskLifecycle, after: TaskLifecycle) {
        if before == after {
            return;
        }
        if before == TaskLifecycle::BlockedSyscall {
            let previous = self.blocked_syscall.fetch_sub(1, Ordering::AcqRel);
            assert!(previous > 0, "blocked-syscall counter underflow");
        }
        if after == TaskLifecycle::BlockedSyscall {
            self.blocked_syscall.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn blocked_syscall_count(&self) -> usize {
        self.blocked_syscall.load(Ordering::Acquire)
    }

    /// Transition one frame owner to Terminal and publish its frame status
    /// before the task shard becomes observable again.
    ///
    /// `after_transition` is normally a no-op. Tests use it to hold the exact
    /// historical race point (Terminal state with a still-Pending frame) and
    /// prove that an await slow path cannot pass the shard lock there.
    fn finish_terminal_and_publish(
        &self,
        id: RuntimeTaskId,
        terminal_status: i64,
        after_transition: impl FnOnce(),
    ) -> Option<bool> {
        self.with(id, |task| {
            let before = task.state.lifecycle();
            let transitioned = task.state.finish_terminal();
            if transitioned {
                after_transition();
                crate::async_frame::frame_publish_terminal(task.frame, terminal_status);
            }
            self.reconcile_blocked_transition(before, task.state.lifecycle());
            transitioned
        })
    }
}

fn register_waiter_sharded(
    tasks: &ShardedTaskTable,
    awaitee: RuntimeTaskId,
    waiter: RuntimeTaskId,
) -> bool {
    tasks.with_two_mut(awaitee, waiter, |awaitee_task, waiter_task| {
        let Some(awaitee_task) = awaitee_task else {
            return false;
        };
        if awaitee_task.state.lifecycle() == TaskLifecycle::Terminal {
            return false;
        }
        let registered = awaitee_task.register_waiter(waiter);
        if registered && let Some(task) = waiter_task {
            task.add_awaiting(awaitee);
        }
        true
    })
}

fn unregister_waiter_sharded(
    tasks: &ShardedTaskTable,
    awaitee: RuntimeTaskId,
    waiter: RuntimeTaskId,
) {
    tasks.with_two_mut(awaitee, waiter, |awaitee_task, waiter_task| {
        if let Some(task) = awaitee_task {
            task.remove_waiter(waiter);
        }
        if let Some(task) = waiter_task {
            task.remove_awaiting(awaitee);
        }
    });
}

/// Publish one wake against a task table + run queues, with no scheduler
/// metadata mutex involved.
///
/// The state transition, the blocked-syscall accounting and the queue
/// publication all complete while the task's shard is held (willow-6qtv/8agm),
/// and the queue entry is published *before* the blocked count drops so an idle
/// observer always sees at least one reason to stay alive.
///
/// May be called while the timer heap lock is held (that is the documented lock
/// order in [`crate::timer_queue`]); never the other way round.
fn wake_task_outcome_in(
    tasks: &ShardedTaskTable,
    run_queues: &RunQueues,
    id: RuntimeTaskId,
) -> WakeOutcome {
    tasks
        .with_mut(id, |task| {
            let before = task.state.lifecycle();
            let outcome = task.state.wake();
            if !matches!(outcome, WakeOutcome::Terminal) {
                task.wake_deadline = None;
            }
            if outcome == WakeOutcome::Enqueue {
                run_queues.push_global(id);
            }
            tasks.reconcile_blocked_transition(before, task.state.lifecycle());
            outcome
        })
        .unwrap_or(WakeOutcome::Terminal)
}

/// Boolean compatibility wrapper for call sites that only care whether this
/// caller published a new run-queue entry.
fn wake_task_in(tasks: &ShardedTaskTable, run_queues: &RunQueues, id: RuntimeTaskId) -> bool {
    wake_task_outcome_in(tasks, run_queues, id) == WakeOutcome::Enqueue
}

/// Does this heap entry still describe the deadline its task is waiting on?
///
/// A task that was woken early, re-armed its sleep, or finished leaves entries
/// behind; the timer heap prunes them lazily through this predicate instead of
/// searching for and removing them at wake time.
///
/// This reads the lifecycle rather than [`RuntimeTask::runtime_state`] because a
/// terminal record is observable here: `finish_terminal` publishes Terminal under
/// the task's shard, releases it, and only then re-takes the shard to remove the
/// record. A promoting thread that looks in that gap must treat the entry as
/// stale — the same answer it gets once the record is gone (willow-0a6k.7).
fn timer_entry_is_current(tasks: &ShardedTaskTable, wake: TimerWake) -> bool {
    tasks
        .with(wake.task_id, |task| {
            matches!(
                task.state.lifecycle(),
                TaskLifecycle::Parked | TaskLifecycle::BlockedSyscall | TaskLifecycle::Running
            ) && task.wake_deadline == Some(wake.deadline)
        })
        .unwrap_or(false)
}

/// Record `millis` as the running task's wake-deadline and register the timer.
///
/// The shard guard is released by `with_mut` before the timer heap is touched:
/// the lock order is timer heap -> task shard, so taking the heap while holding
/// a shard would invert it (willow-9ha4).
fn set_wake_after_millis_in(tasks: &ShardedTaskTable, timers: &TimerQueue, millis: i64) {
    let deadline = Instant::now() + Duration::from_millis(millis.max(0) as u64);
    let Some(id) = current_task_id() else { return };
    if tasks
        .with_mut(id, |task| task.wake_deadline = Some(deadline))
        .is_some()
    {
        timers.push(id, deadline);
    }
}

#[derive(Debug)]
pub struct RuntimeScheduler {
    next_task_id: RuntimeTaskId,
    tasks: Arc<ShardedTaskTable>,
    /// Per-worker local run queues + a shared global queue, with work stealing
    /// (willow-gyaa.4). New/woken tasks go to the global queue; an idle worker
    /// drains its local queue, then the global queue, then steals from the back
    /// of another worker's local queue.
    run_queues: Arc<RunQueues>,
    /// Terminal tasks whose channel/netpoll registrations must be purged
    /// OUTSIDE the scheduler lock. Channel addresses are captured before the
    /// heavy task record is removed (willow-ezs.1.4).
    pending_terminal_cleanups: Vec<TerminalCleanup>,
    /// Frame runtime roots retired by terminal tasks. The heavy task record is
    /// removed immediately, but these roots remain until the outermost
    /// scheduler drive has quiesced every worker.
    pending_frame_unroots: Vec<usize>,
    /// Total frame roots currently owned by the scheduler: active task frames
    /// plus terminal frames waiting at the outermost unroot boundary.
    frame_roots: usize,
    /// Wake-deadlines, behind their own lock rather than this scheduler's
    /// metadata mutex (willow-9ha4). The run loop promotes expired timers once
    /// per iteration, so keeping them here cost one global-mutex acquisition
    /// per poll on every worker even when nothing had ever slept.
    timers: Arc<TimerQueue>,
}

impl Default for RuntimeScheduler {
    fn default() -> Self {
        Self::with_worker_count(runtime_worker_config().active_workers())
    }
}

impl RuntimeScheduler {
    /// Build a scheduler with `worker_count` worker-local run queues (at least
    /// one). Task ids start at 1 (id 0 is the `willow_sched_current_task()`
    /// "no running task" sentinel).
    pub fn with_worker_count(worker_count: usize) -> Self {
        Self::with_run_queues(Arc::new(RunQueues::new(worker_count)))
    }

    fn with_run_queues(run_queues: Arc<RunQueues>) -> Self {
        Self::with_components(
            run_queues,
            Arc::new(ShardedTaskTable::new()),
            Arc::new(TimerQueue::new()),
        )
    }

    fn with_components(
        run_queues: Arc<RunQueues>,
        tasks: Arc<ShardedTaskTable>,
        timers: Arc<TimerQueue>,
    ) -> Self {
        Self {
            next_task_id: 1,
            tasks,
            run_queues,
            timers,
            pending_terminal_cleanups: Vec::new(),
            pending_frame_unroots: Vec::new(),
            frame_roots: 0,
        }
    }

    /// Reconcile scheduler counters around one atomic task-state transition.
    ///
    /// The lifecycle itself lives in `AtomicTaskState`; this wrapper only keeps
    /// the O(1) blocked-syscall summary exact. Returns `None` for an unknown id.
    fn with_task_state<R>(
        &mut self,
        id: RuntimeTaskId,
        mutate: impl FnOnce(&mut RuntimeTask) -> R,
    ) -> Option<R> {
        let tasks = Arc::clone(&self.tasks);
        tasks.with_mut(id, |task| {
            let before = task.state.lifecycle();
            let result = mutate(task);
            let after = task.state.lifecycle();
            tasks.reconcile_blocked_transition(before, after);
            result
        })
    }

    /// Number of worker-local run queues (the configured worker count).
    pub fn worker_count(&self) -> usize {
        self.run_queues.worker_count()
    }

    /// The one place a task id enters a run queue (willow-ezs.1.1).
    ///
    /// The atomic `queued` bit means exactly one runnable claim is outstanding,
    /// so enqueue never scans the queues. Returns whether the caller won the
    /// right to push:
    ///
    /// * already queued or not Ready → `false`;
    /// * no task record → `true`, because the queue-level unit tests push
    ///   synthetic ids that own no [`RuntimeTask`] to model locality; those
    ///   ids have no flag to track and no state to violate.
    fn mark_queued(&mut self, id: RuntimeTaskId) -> bool {
        self.tasks
            .with(id, |task| task.state.claim_queue_slot())
            .unwrap_or(true)
    }

    /// Enqueue a runnable task. New and woken tasks go to the shared global
    /// queue; any idle worker can then pick them up (willow-gyaa.4).
    fn enqueue_ready(&mut self, id: RuntimeTaskId) {
        if self.mark_queued(id) {
            self.run_queues.push_global(id);
        }
    }

    /// Push a task directly onto a specific worker's local queue. Used by future
    /// parallel workers (and the work-stealing tests) to model locality. Obeys
    /// the same membership invariant as the global enqueue.
    pub fn enqueue_local(&mut self, worker: usize, id: RuntimeTaskId) {
        if !self.mark_queued(id) {
            return;
        }
        self.run_queues.push_local(worker, id);
    }

    /// Pop the next runnable task for `worker`: its own local queue first (FIFO),
    /// then the global queue, then steal from the back of another worker's local
    /// queue (LIFO steal, which tends to take the coldest work). Returns `None`
    /// when no worker has runnable tasks (willow-gyaa.4).
    pub fn pop_for_worker(&mut self, worker: usize) -> Option<RuntimeTaskId> {
        // Physical pop deliberately leaves `queued` set. `claim_for_poll`
        // consumes the queue right together with Ready -> Running/Cancelling,
        // closing the pop/wake lost-wake window (willow-ezs.4 review).
        self.pop_queue_entry(worker)
    }

    fn pop_queue_entry(&mut self, worker: usize) -> Option<RuntimeTaskId> {
        self.run_queues.pop_for_worker(worker)
    }

    /// Remove the physical queue entry owned by `id` without changing its
    /// atomic queue token. This is used only by the bookkeeping-placeholder
    /// helpers (`set_running`, direct `park`, and direct terminal completion);
    /// production workers use `pop_for_worker` and never scan a queue.
    fn remove_placeholder_queue_entry(&mut self, id: RuntimeTaskId) {
        self.run_queues.remove(id);
    }

    /// Atomically claim the next task that is still Ready. Queue entries can
    /// become stale when a wake races with a Running poll; discarding them here
    /// prevents two workers from polling the same async frame concurrently.
    fn claim_ready_for_worker(&mut self, worker: usize) -> Option<RuntimeTaskId> {
        while let Some(id) = self.pop_for_worker(worker) {
            if let Some(id) = self.claim_popped(id) {
                return Some(id);
            }
        }
        None
    }

    /// Validate and acquire one id that has already been physically removed
    /// from a run queue. Production workers call this only after popping from
    /// [`GLOBAL_RUN_QUEUES`] without holding the scheduler metadata mutex.
    fn claim_popped(&mut self, id: RuntimeTaskId) -> Option<RuntimeTaskId> {
        let (outcome, has_cleanup, panic_context) = self.tasks.with_mut(id, |task| {
            let outcome = task.claim_for_poll();
            if outcome == ClaimOutcome::Poll {
                task.yield_requested = false;
            }
            let has_cleanup =
                outcome == ClaimOutcome::Cancel && task.cancel.is_some() && !task.frame.is_null();
            (outcome, has_cleanup, task.panic_context())
        })?;
        match outcome {
            ClaimOutcome::Drop => return None,
            ClaimOutcome::Poll => {
                set_current_task_context(id, panic_context);
                return Some(id);
            }
            ClaimOutcome::Cancel => {}
        }

        // Cooperative cancellation boundary (willow-0a6k.7): the atomic claim
        // moved the task to Cancelling and consumed the request.
        if has_cleanup {
            set_current_task_context(id, panic_context);
            return Some(id);
        }
        self.finalize_cancelled(id);
        if current_task_id() == Some(id) {
            set_current_task(None);
        }
        None
    }

    /// Mark a cancel-requested task Cancelled without polling it.
    fn finalize_cancelled(&mut self, id: RuntimeTaskId) {
        self.finish_terminal(id, RuntimeTaskState::Cancelled);
    }

    /// The cleanup entry + frame for a task the claim just moved to
    /// Cancelling (willow-vynv.3). Consumes the entry so it runs once.
    pub fn take_cancel_work(
        &mut self,
        id: RuntimeTaskId,
    ) -> Option<(RuntimeCancelFn, *mut c_void)> {
        self.tasks
            .with_mut(id, |task| {
                if task.state.lifecycle() != TaskLifecycle::Cancelling {
                    return None;
                }
                let cancel = task.cancel.take()?;
                Some((cancel, task.frame))
            })
            .flatten()
    }

    /// True while any task is parked in `BlockedSyscall` — a blocking-pool
    /// job will wake it, so the scheduler must NOT declare idle/stop on the
    /// strength of empty queues alone (willow-0a6k.5 review fix).
    ///
    /// O(1): the idle path asks this on every park cycle, so it reads the
    /// maintained counter instead of scanning the task table (willow-ezs.1.2).
    fn has_blocked_syscall_tasks(&self) -> bool {
        self.blocked_syscall_count() > 0
    }

    fn blocked_syscall_count(&self) -> usize {
        self.tasks.blocked_syscall_count()
    }

    /// Test-only cross-check that the O(1) blocked-syscall counter agrees with
    /// the task table it summarizes.
    #[cfg(test)]
    fn blocked_syscall_invariant_holds(&self) -> bool {
        self.blocked_syscall_count()
            == self
                .tasks
                .snapshots()
                .into_iter()
                .filter(|task| task.runtime_state() == Some(RuntimeTaskState::BlockedSyscall))
                .count()
    }

    /// The single terminal transition for Completed/Cancelled/Panicked
    /// (willow-ezs.1.4).
    ///
    /// Under the scheduler lock this publishes the frame status, detaches both
    /// directions of await relationships, wakes awaiters, captures external
    /// wait registrations, retires the frame root, and removes the heavy
    /// [`RuntimeTask`]. Channel/netpoll cleanup and actual frame unrooting are
    /// deliberately left to the two lock-free phases.
    fn finish_terminal(&mut self, id: RuntimeTaskId, state: RuntimeTaskState) -> bool {
        let Some(terminal_status) = terminal_frame_status(state) else {
            debug_assert!(false, "finish_terminal requires a terminal state");
            return false;
        };
        self.prepare_placeholder_terminal_owner(id);
        let tasks = Arc::clone(&self.tasks);
        let transitioned = tasks
            .finish_terminal_and_publish(id, terminal_status, || {})
            .unwrap_or(false);
        if !transitioned {
            return false;
        }
        let Some(mut task) = self.tasks.remove(id) else {
            return false;
        };

        // Both directions are detached in O(registrations), not O(all tasks):
        // `waiters` pops live entries in registration order and `awaiting` is a
        // set, so neither side rescans a vector per relation (willow-ezs.2).
        let waiters = task.take_waiters();
        let awaiting = task.take_awaiting();
        let channel_waits = task.take_wait_channels();
        let lock_wait = task.take_lock_wait();

        for awaitee in awaiting {
            self.tasks.with_mut(awaitee, |task| {
                task.remove_waiter(id);
            });
        }
        for waiter in waiters {
            self.tasks.with_mut(waiter, |task| {
                task.remove_awaiting(id);
            });
            self.wake(waiter);
        }

        if task.frame_rooted && !task.frame.is_null() {
            task.frame_rooted = false;
            self.pending_frame_unroots.push(task.frame as usize);
        }
        // A bookkeeping-only placeholder cannot register with netpoll, and an
        // empty channel list means it has no external cleanup at all. Avoid
        // retaining one cleanup record per completed RuntimeExecutor task.
        if task.poll.is_some() || !channel_waits.is_empty() || lock_wait.is_some() {
            self.pending_terminal_cleanups.push(TerminalCleanup {
                task_id: id,
                channel_waits,
                lock_wait,
            });
        }

        // `RuntimeTask::roots` and all diagnostic/wait metadata are active-task
        // ownership. They may be dropped now that the poll has returned and
        // terminal status is published. The result frame has its independent
        // runtime root until the outermost post-quiescence boundary.
        drop(task);
        true
    }

    /// Bookkeeping-only placeholders have no poll function and therefore no
    /// external frame owner. The executor and scheduler tests complete these
    /// directly, so acquire their frame token through the same atomic
    /// Ready/Parked/Blocked -> Running path before the terminal CAS. Executable
    /// tasks (`poll.is_some()`) are never force-claimed here.
    fn prepare_placeholder_terminal_owner(&mut self, id: RuntimeTaskId) {
        let Some((is_executable, lifecycle, is_queued)) = self.tasks.with(id, |task| {
            (
                task.poll.is_some(),
                task.state.lifecycle(),
                task.state.load().is_queued(),
            )
        }) else {
            return;
        };
        if is_executable || lifecycle.owns_frame() {
            return;
        }
        if lifecycle == TaskLifecycle::Ready && is_queued {
            self.remove_placeholder_queue_entry(id);
        }
        let _ = self.with_task_state(id, |task| {
            match task.state.lifecycle() {
                TaskLifecycle::Ready => {
                    if !task.state.load().is_queued() {
                        let _ = task.state.claim_queue_slot();
                    }
                }
                TaskLifecycle::Parked | TaskLifecycle::BlockedSyscall => {
                    let _ = task.state.wake();
                }
                TaskLifecycle::Running | TaskLifecycle::Cancelling | TaskLifecycle::Terminal => {
                    return;
                }
            }
            let _ = task.state.claim_for_poll();
        });
    }

    fn take_pending_terminal_cleanups(&mut self) -> Vec<TerminalCleanup> {
        std::mem::take(&mut self.pending_terminal_cleanups)
    }

    fn take_pending_frame_unroots(&mut self) -> Vec<usize> {
        let frames = std::mem::take(&mut self.pending_frame_unroots);
        self.frame_roots = self
            .frame_roots
            .checked_sub(frames.len())
            .expect("scheduler frame-root counter underflow");
        frames
    }

    pub fn metadata_snapshot(&self) -> SchedulerMetadataSnapshot {
        SchedulerMetadataSnapshot {
            heavy_tasks: self.tasks.len(),
            queue_entries: self.ready_total(),
            pending_cleanups: self.pending_terminal_cleanups.len(),
            frame_roots: self.frame_roots,
            blocked_syscalls: self.blocked_syscall_count(),
        }
    }

    /// True if `id` is queued anywhere (any local queue or the global queue).
    /// O(1): the flag is the invariant, not a scan (willow-ezs.1.1).
    pub fn is_queued(&self, id: RuntimeTaskId) -> bool {
        self.tasks
            .with(id, |task| task.state.load().is_queued())
            .unwrap_or(false)
    }

    /// Test-only cross-check of the [`Self::is_queued`] invariant against the
    /// actual queue contents: the flag must agree with a full scan for every
    /// known task, and no id may appear in two queues (or twice in one).
    #[cfg(test)]
    fn queue_invariant_holds(&self) -> bool {
        let mut seen = std::collections::HashMap::<RuntimeTaskId, usize>::new();
        for id in self.run_queues.snapshot() {
            *seen.entry(id).or_default() += 1;
        }
        if seen.values().any(|count| *count > 1) {
            return false;
        }
        self.tasks
            .snapshots()
            .into_iter()
            .all(|task| task.state.load().is_queued() == seen.contains_key(&task.id))
    }

    /// Total runnable tasks across all queues.
    fn ready_total(&self) -> usize {
        self.run_queues.len()
    }

    pub fn spawn_placeholder(&mut self) -> RuntimeTaskId {
        let id = self.next_task_id;
        self.next_task_id += 1;
        let task = RuntimeTask::new(id);
        self.tasks.insert(id, task);
        self.enqueue_ready(id);
        id
    }

    /// Test-only: run `f` against one task record under its shard lock, for
    /// fixtures that must drive the state word directly (willow-38w.1.2).
    #[cfg(test)]
    pub fn with_task_for_test<R>(
        &self,
        id: RuntimeTaskId,
        f: impl FnOnce(&mut RuntimeTask) -> R,
    ) -> Option<R> {
        self.tasks.with_mut(id, f)
    }

    /// Test-only: reap a task record the way terminal cleanup would, so a lock
    /// queue entry can be left pointing at a task that no longer exists.
    #[cfg(test)]
    pub fn remove_task_for_test(&self, id: RuntimeTaskId) -> bool {
        self.tasks.remove(id).is_some()
    }

    pub fn spawn_parked_placeholder(&mut self) -> RuntimeTaskId {
        let id = self.next_task_id;
        self.next_task_id += 1;
        let task = RuntimeTask::new(id);
        assert!(task.state.claim_queue_slot());
        assert_eq!(task.state.claim_for_poll(), ClaimOutcome::Poll);
        assert_eq!(task.state.park_after_poll(), BoundaryOutcome::Suspended);
        self.tasks.insert(id, task);
        id
    }

    /// Spawn a cooperative task that runs `poll` over `frame`. The task starts
    /// ready; the caller is responsible for keeping `frame` GC-reachable (the
    /// runtime ABI roots it).
    pub fn spawn_task_initialized(
        &mut self,
        poll: RuntimePollFn,
        frame: *mut c_void,
        cancel: Option<RuntimeCancelFn>,
        initialize: impl FnOnce(RuntimeTaskId),
    ) -> RuntimeTaskId {
        let id = self.next_task_id;
        self.next_task_id += 1;
        let mut task = RuntimeTask::new(id);
        task.poll = Some(poll);
        task.cancel = cancel;
        task.frame = frame;
        task.frame_rooted = !frame.is_null();
        if task.frame_rooted {
            self.frame_roots += 1;
        }
        // Publish every frame/native-side field that a first poll or
        // cancellation cleanup can observe before the task becomes reachable
        // from a run queue. `enqueue_ready` is the final publication step.
        #[cfg(debug_assertions)]
        let _initializer_guard = SpawnInitializerGuard::enter();
        initialize(id);
        #[cfg(debug_assertions)]
        drop(_initializer_guard);
        self.tasks.insert(id, task);
        self.enqueue_ready(id);
        id
    }

    pub fn spawn_task(&mut self, poll: RuntimePollFn, frame: *mut c_void) -> RuntimeTaskId {
        self.spawn_task_initialized(poll, frame, None, |_| {})
    }

    /// The cooperative resume entry, frame, and stable preemption flag for an
    /// executable task.
    pub fn task_work(
        &self,
        id: RuntimeTaskId,
    ) -> Option<(RuntimePollFn, *mut c_void, *const c_void)> {
        self.tasks
            .with(id, |task| {
                task.poll
                    .map(|poll| (poll, task.frame, task.preempt_flag_ptr()))
            })
            .flatten()
    }

    pub fn set_running(&mut self, id: RuntimeTaskId) {
        if self
            .tasks
            .with(id, |task| {
                task.state.lifecycle() == TaskLifecycle::Ready && task.state.load().is_queued()
            })
            .unwrap_or(false)
        {
            self.remove_placeholder_queue_entry(id);
        }
        let claimed = self
            .with_task_state(id, |task| {
                match task.state.lifecycle() {
                    TaskLifecycle::Ready => {
                        if !task.state.load().is_queued() {
                            let _ = task.state.claim_queue_slot();
                        }
                    }
                    TaskLifecycle::Parked | TaskLifecycle::BlockedSyscall => {
                        let _ = task.state.wake();
                    }
                    TaskLifecycle::Running | TaskLifecycle::Cancelling => return true,
                    TaskLifecycle::Terminal => return false,
                }
                task.yield_requested = false;
                matches!(
                    task.state.claim_for_poll(),
                    ClaimOutcome::Poll | ClaimOutcome::Cancel
                )
            })
            .unwrap_or(false);
        if claimed {
            set_current_task(Some(id));
        }
    }

    /// Clear the "currently running" marker once a poll returns. Guards
    /// `willow_sched_sleep` / `willow_sched_await` against attaching a deadline
    /// or waiter to a STALE task when called outside of a poll (willow-lpn.5.3).
    pub fn clear_running(&mut self) {
        set_current_task(None);
    }

    /// Attach a wake-deadline to the currently-running task (called via
    /// `willow_sched_sleep` from a poll fn before it returns Pending). The
    /// timer-aware run loop wakes the task once the deadline passes.
    pub fn set_running_wake_after_millis(&mut self, millis: i64) {
        set_wake_after_millis_in(&self.tasks, &self.timers, millis);
    }

    /// The parked task with the earliest wake-deadline, if any. Backed by a
    /// min-heap so idle scheduling does not scan every parked task (willow-gyaa.3).
    fn next_timer_deadline(&self) -> Option<(RuntimeTaskId, Instant)> {
        let tasks = &*self.tasks;
        self.timers
            .next_deadline(|wake| timer_entry_is_current(tasks, wake))
    }

    /// Move every due timer directly from the timer heap to the ready queue.
    ///
    /// This transition must happen under ONE lock — now the timer heap's own
    /// lock rather than the scheduler metadata mutex (willow-9ha4). If a worker
    /// removed the last timer and released the lock before waking its task,
    /// another worker could observe neither a timer nor runnable work and
    /// incorrectly return from `run_until` while the target was still parked.
    fn wake_due_timers(&self, now: Instant) -> usize {
        let tasks = &*self.tasks;
        let run_queues = &*self.run_queues;
        self.timers.wake_due(
            now,
            |wake| timer_entry_is_current(tasks, wake),
            |id| {
                wake_task_in(tasks, run_queues, id);
            },
        )
    }

    pub fn complete(&mut self, id: RuntimeTaskId) {
        self.finish_terminal(id, RuntimeTaskState::Completed);
    }

    fn finalize_panicked(&mut self, id: RuntimeTaskId) {
        self.finish_terminal(id, RuntimeTaskState::Panicked);
    }

    /// Register `waiter` to be woken when `awaitee` completes (for `await
    /// <task>`). No-op if `awaitee` is unknown.
    pub fn register_waiter(&mut self, awaitee: RuntimeTaskId, waiter: RuntimeTaskId) {
        // `register` answers "already a waiter?" from its membership map, so a
        // 10,000-task fan-in costs expected O(1) per registration instead of a
        // linear scan of the waiter list (willow-ezs.2).
        let _ = register_waiter_sharded(&self.tasks, awaitee, waiter);
    }

    /// Remove `waiter` from `awaitee`'s waiter list (and the reverse reference).
    /// Both sides are O(1); a re-registration afterwards appends the waiter at
    /// the FIFO tail rather than reviving its old position.
    pub fn unregister_waiter(&mut self, awaitee: RuntimeTaskId, waiter: RuntimeTaskId) {
        unregister_waiter_sharded(&self.tasks, awaitee, waiter);
    }

    pub fn pop_ready(&mut self) -> Option<RuntimeTaskId> {
        self.claim_ready_for_worker(0)
    }

    pub fn task(&self, id: RuntimeTaskId) -> Option<RuntimeTask> {
        self.tasks.with(id, Clone::clone)
    }

    pub fn with_task_mut<R>(
        &self,
        id: RuntimeTaskId,
        mutate: impl FnOnce(&mut RuntimeTask) -> R,
    ) -> Option<R> {
        self.tasks.with_mut(id, mutate)
    }

    pub fn tasks(&self) -> impl Iterator<Item = RuntimeTask> {
        self.tasks.snapshots().into_iter()
    }

    pub fn task_count(&self) -> usize {
        self.tasks.len()
    }

    pub fn ready_len(&self) -> usize {
        self.ready_total()
    }

    /// The live state of `id`, or `None` if it is unknown — including the brief
    /// window where its record is terminal but not yet reaped. Both answer the
    /// same question a caller has: there is no live task here.
    pub fn task_state(&self, id: RuntimeTaskId) -> Option<RuntimeTaskState> {
        self.tasks.with(id, RuntimeTask::runtime_state).flatten()
    }

    pub fn park(&mut self, id: RuntimeTaskId) {
        if self
            .tasks
            .with(id, |task| {
                task.poll.is_none()
                    && task.state.lifecycle() == TaskLifecycle::Ready
                    && task.state.load().is_queued()
            })
            .unwrap_or(false)
        {
            self.set_running(id);
        }
        self.with_task_state(id, |task| {
            let _ = task.state.park_after_poll();
        });
    }

    /// Request a wake. Returns true only when a parked/blocked task was
    /// transitioned to Ready and published to a run queue.
    pub fn wake(&self, id: RuntimeTaskId) -> bool {
        wake_task_in(&self.tasks, &self.run_queues, id)
    }

    /// Mark the currently-running task for a cooperative yield. The actual
    /// requeue happens after the poll returns Pending, so another worker cannot
    /// pick up the same frame while it is still being polled.
    pub fn request_running_yield(&mut self) {
        if let Some(id) = current_task_id() {
            self.tasks.with_mut(id, |task| task.yield_requested = true);
        }
    }

    /// Requeue a task that returned a *runnable* poll code — `RUNTIME_POLL_YIELD`
    /// (voluntary) or `RUNTIME_POLL_PREEMPTED` (forced at a safepoint, spec §7).
    /// Unlike a Pending poll it is not waiting on an event, so it goes straight
    /// back on the ready queue instead of parking.
    pub fn requeue_runnable(&mut self, id: RuntimeTaskId) {
        let tasks = Arc::clone(&self.tasks);
        let run_queues = Arc::clone(&self.run_queues);
        tasks.with_mut(id, |task| {
            let before = task.state.lifecycle();
            let outcome = {
                task.yield_requested = false;
                task.state.requeue_after_poll()
            };
            if outcome == BoundaryOutcome::Requeue {
                run_queues.push_global(id);
            }
            tasks.reconcile_blocked_transition(before, task.state.lifecycle());
        });
    }

    /// Finish a Pending poll. If a wake/yield raced with the Running state, make
    /// the task Ready now; otherwise park it until a future wake.
    pub fn finish_pending_poll(&mut self, id: RuntimeTaskId) {
        self.finish_waiting_poll(id, false);
    }

    /// Finish a poll that detached native blocking work. Completion wakes race
    /// exactly like ordinary Pending wakes, but the distinct state makes worker
    /// isolation observable and lets GC/STW treat the task as already safe.
    pub fn finish_blocked_syscall_poll(&mut self, id: RuntimeTaskId) {
        self.finish_waiting_poll(id, true);
    }

    fn finish_waiting_poll(&mut self, id: RuntimeTaskId, blocked_syscall: bool) {
        let tasks = Arc::clone(&self.tasks);
        let run_queues = Arc::clone(&self.run_queues);
        tasks.with_mut(id, |task| {
            let before = task.state.lifecycle();
            let outcome = {
                let yielded = task.yield_requested;
                task.yield_requested = false;
                if yielded {
                    task.state.requeue_after_poll()
                } else if blocked_syscall {
                    task.state.block_on_syscall()
                } else {
                    task.state.park_after_poll()
                }
            };
            if outcome == BoundaryOutcome::Requeue {
                run_queues.push_global(id);
            }
            tasks.reconcile_blocked_transition(before, task.state.lifecycle());
        });
    }
}

// The scheduler exposes no `GcTrace` impl (willow-ezs.3). Task-owned GC values
// live in the async frame, which stays reachable through the runtime root
// registry while `frame_rooted` holds; collection scans that registry, never
// the task table. The previous impl walked a per-task root set that nothing
// ever populated.

// ─── Process-global cooperative scheduler (willow-fqg.1 / willow-gyaa.4) ─────
//
// A shared run queue that drives compiler-generated cooperative tasks. Each task
// owns a heap async frame; the frame is registered as a GC runtime root while
// the task is pending/running, so a parked/ready task's live values survive
// collection even though no native stack frame holds them (spec §8.2 / §9).

/// Swappable only by test reset; ordinary scheduling clones this pointer under
/// a read lock and performs queue operations without `GLOBAL_SCHEDULER`.
static GLOBAL_RUN_QUEUES: LazyLock<RwLock<Arc<RunQueues>>> = LazyLock::new(|| {
    RwLock::new(Arc::new(RunQueues::new(
        runtime_worker_config().active_workers(),
    )))
});

static GLOBAL_TASK_TABLE: LazyLock<RwLock<Arc<ShardedTaskTable>>> =
    LazyLock::new(|| RwLock::new(Arc::new(ShardedTaskTable::new())));

fn global_run_queues() -> Arc<RunQueues> {
    debug_assert_not_in_spawn_initializer("access the global run queues");
    GLOBAL_RUN_QUEUES
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn global_task_table() -> Arc<ShardedTaskTable> {
    debug_assert_not_in_spawn_initializer("access the global task table");
    GLOBAL_TASK_TABLE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Tasks popped from a run queue but not yet registered as an active poll.
///
/// A claim pops the id BEFORE it takes `claim_gate` and increments
/// `active_polls`, so for that window the work is in no queue and in no poll
/// counter. Another worker idling in exactly that window sees an empty queue,
/// zero active polls, no timer and no netpoll waiter, declares the run globally
/// idle and stops the pool; the claiming worker then observes the stop, pushes
/// the id back and returns — leaving a runnable task stranded and the drive
/// reporting quiescence. That is the lost-wakeup behind an `await` returning
/// early and a `select` giving up (willow-atth).
///
/// Counted globally rather than per `ParallelRunState` so a nested drive and a
/// foreign driver thread observe each other's claims too.
static CLAIMS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// RAII marker for the pop→claim window. Dropped only after the claim has
/// resolved: either `active_polls` has been incremented (under `claim_gate`),
/// or the id has been pushed back / dropped, so idleness is never observable
/// between the two states.
struct ClaimInFlight;

impl ClaimInFlight {
    fn enter() -> Self {
        CLAIMS_IN_FLIGHT.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Drop for ClaimInFlight {
    fn drop(&mut self) {
        let previous = CLAIMS_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "claim-in-flight underflow");
    }
}

/// True while any thread holds a popped-but-unclaimed task. Idle detection must
/// treat this as pending work.
fn claims_in_flight() -> bool {
    CLAIMS_IN_FLIGHT.load(Ordering::Acquire) > 0
}

/// Wake-deadlines, shared with the scheduler instance the same way the run
/// queues and the task table are (willow-9ha4). Timer work reaches this through
/// its own lock, so the run loop's per-iteration timer promotion no longer
/// serializes every worker on `GLOBAL_SCHEDULER`.
static GLOBAL_TIMERS: LazyLock<RwLock<Arc<TimerQueue>>> =
    LazyLock::new(|| RwLock::new(Arc::new(TimerQueue::new())));

fn global_timers() -> Arc<TimerQueue> {
    debug_assert_not_in_spawn_initializer("access the global timer queue");
    GLOBAL_TIMERS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

static GLOBAL_SCHEDULER: LazyLock<Mutex<RuntimeScheduler>> = LazyLock::new(|| {
    Mutex::new(RuntimeScheduler::with_components(
        global_run_queues(),
        global_task_table(),
        global_timers(),
    ))
});

thread_local! {
    /// Native task initializers run while GLOBAL_SCHEDULER is held and before
    /// the task exists in the sharded table. Re-entering any scheduler surface
    /// from there can deadlock or silently target an unpublished id.
    static SPAWN_INITIALIZER_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

#[cfg(debug_assertions)]
struct SpawnInitializerGuard;

#[cfg(debug_assertions)]
impl SpawnInitializerGuard {
    fn enter() -> Self {
        SPAWN_INITIALIZER_ACTIVE.with(|active| {
            assert!(
                !active.replace(true),
                "scheduler spawn initializer re-entered recursively"
            );
        });
        Self
    }
}

#[cfg(debug_assertions)]
impl Drop for SpawnInitializerGuard {
    fn drop(&mut self) {
        SPAWN_INITIALIZER_ACTIVE.with(|active| active.set(false));
    }
}

#[inline]
fn debug_assert_not_in_spawn_initializer(operation: &str) {
    #[cfg(debug_assertions)]
    SPAWN_INITIALIZER_ACTIVE.with(|active| {
        assert!(
            !active.get(),
            "scheduler spawn initializer must not {operation}"
        );
    });
    #[cfg(not(debug_assertions))]
    let _ = operation;
}

fn with_global<R>(f: impl FnOnce(&mut RuntimeScheduler) -> R) -> R {
    debug_assert_not_in_spawn_initializer("re-enter the global scheduler");
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let mut sched = GLOBAL_SCHEDULER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut sched)
}

/// Hot wake path: state, blocked-syscall accounting, and queue publication are
/// completed while one task shard is locked (willow-6qtv/8agm).
fn wake_global_task(id: RuntimeTaskId) -> bool {
    wake_task_in(&global_task_table(), &global_run_queues(), id)
}

fn wake_global_task_outcome(id: RuntimeTaskId) -> WakeOutcome {
    wake_task_outcome_in(&global_task_table(), &global_run_queues(), id)
}

/// Register the running task's sleep deadline without the scheduler metadata
/// mutex (willow-9ha4).
fn set_global_wake_after_millis(millis: i64) {
    set_wake_after_millis_in(&global_task_table(), &global_timers(), millis);
}

/// The earliest live wake-deadline, for the idle path's "how long may I sleep?"
/// decision. Always reads the heap under its own lock — never the lock-free
/// hint, which is allowed to be transiently stale-empty.
fn global_next_timer_deadline() -> Option<(RuntimeTaskId, Instant)> {
    let tasks = global_task_table();
    global_timers().next_deadline(|wake| timer_entry_is_current(&tasks, wake))
}

/// Promote every timer due at `now` onto a run queue.
///
/// The lock-free hint short-circuits the overwhelmingly common case — no task
/// has a deadline, or the earliest one is still in the future — so the run
/// loop's per-iteration timer check costs one atomic load instead of a
/// process-global mutex acquisition (willow-9ha4). Missing a just-pushed timer
/// here is harmless: the idle path re-reads the heap under its lock before it
/// can conclude that the scheduler has nothing to do.
fn wake_global_due_timers(now: Instant) -> usize {
    let timers = global_timers();
    if !timers.maybe_due(now) {
        return 0;
    }
    let tasks = global_task_table();
    let run_queues = global_run_queues();
    let woken = timers.wake_due(
        now,
        |wake| timer_entry_is_current(&tasks, wake),
        |id| {
            wake_task_in(&tasks, &run_queues, id);
        },
    );
    if woken > 0 {
        crate::observability::record(
            crate::observability::RuntimeEventKind::TimerWake,
            current_task_id().map(|_| current_worker()),
            0,
            woken as i64,
        );
    }
    woken
}

#[derive(Clone, Copy)]
enum GlobalPollBoundary {
    Pending,
    Runnable,
    BlockedSyscall,
}

/// Return frame ownership after a poll without taking the scheduler metadata
/// mutex. Only the blocked-syscall aggregate needs reconciliation afterwards;
/// ordinary Pending/yield/preempt transitions are task-shard + run-queue
/// operations (willow-8agm/6qtv).
fn finish_global_poll_boundary(id: RuntimeTaskId, boundary: GlobalPollBoundary) {
    let tasks = global_task_table();
    let run_queues = global_run_queues();
    tasks.with_mut(id, |task| {
        let before = task.state.lifecycle();
        let yielded = task.yield_requested;
        task.yield_requested = false;
        let outcome = match boundary {
            GlobalPollBoundary::Pending if yielded => task.state.requeue_after_poll(),
            GlobalPollBoundary::Pending => task.state.park_after_poll(),
            GlobalPollBoundary::Runnable => task.state.requeue_after_poll(),
            GlobalPollBoundary::BlockedSyscall => task.state.block_on_syscall(),
        };
        if outcome == BoundaryOutcome::Requeue {
            // A task that remains runnable after its poll keeps worker
            // affinity. External wakes and newly spawned work enter the shared
            // overflow queue, while other workers can still steal from this
            // local queue.
            run_queues.push_local(current_worker(), id);
        }
        tasks.reconcile_blocked_transition(before, task.state.lifecycle());
    });
}

fn take_global_cancel_work(id: RuntimeTaskId) -> Option<(RuntimeCancelFn, *mut c_void)> {
    global_task_table()
        .with_mut(id, |task| {
            if task.state.lifecycle() != TaskLifecycle::Cancelling {
                return None;
            }
            let cancel = task.cancel.take()?;
            Some((cancel, task.frame))
        })
        .flatten()
}

thread_local! {
    /// The task currently being polled on this OS thread. Runtime primitives
    /// such as sleep/channel/await use it to attach wait state to the right task.
    static CURRENT_TASK: Cell<Option<RuntimeTaskId>> = const { Cell::new(None) };
    /// Worker-local index used for local-queue affinity and nested scheduler
    /// drives from inside a poll.
    static CURRENT_WORKER: Cell<usize> = const { Cell::new(0) };
    /// The active parallel run, if this thread is inside a worker pool.
    static CURRENT_RUN_STATE: RefCell<Option<Arc<ParallelRunState>>> = const { RefCell::new(None) };
}

fn current_task_id() -> Option<RuntimeTaskId> {
    CURRENT_TASK.with(Cell::get)
}

fn set_current_task(id: Option<RuntimeTaskId>) {
    CURRENT_TASK.with(|current| current.set(id));
    let context =
        id.and_then(|task_id| global_task_table().with(task_id, RuntimeTask::panic_context));
    crate::panic_context::replace_current_context(context);
}

/// Install a context already cloned while the task shard was held. Production
/// claim paths use this to avoid a second task-table lock solely for TLS setup.
fn set_current_task_context(id: RuntimeTaskId, context: Arc<crate::panic_context::PanicContext>) {
    CURRENT_TASK.with(|current| current.set(Some(id)));
    crate::panic_context::replace_current_context(Some(context));
}

fn current_worker() -> usize {
    CURRENT_WORKER.with(Cell::get)
}

/// Spawn a cooperative task on the global scheduler. The frame is rooted with
/// the GC so it (and the values it references) survives collection while the
/// task is pending. Returns the task id.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_spawn(poll: RuntimePollFn, frame: *mut c_void) -> u64 {
    spawn_global_task_initialized(poll, frame, None, |_| {})
}

/// Spawn a native runtime task only after all state needed by its first poll
/// and cancellation cleanup has been initialized. The closure runs while the
/// scheduler allocates the task id and before the task is inserted into any run
/// queue; it must not call back into the scheduler.
pub(crate) fn spawn_global_task_initialized(
    poll: RuntimePollFn,
    frame: *mut c_void,
    cancel: Option<RuntimeCancelFn>,
    initialize: impl FnOnce(RuntimeTaskId),
) -> u64 {
    // Keep the frame (and everything it transitively references) alive while the
    // task is pending. Removed on completion in `willow_sched_run`.
    crate::gc::willow_gc_add_runtime_root(frame as *mut u8);
    let id = with_global(|sched| sched.spawn_task_initialized(poll, frame, cancel, initialize));
    crate::observability::record(
        crate::observability::RuntimeEventKind::TaskSpawn,
        current_task_id().map(|_| current_worker()),
        id,
        0,
    );
    crate::gc::stress_collect("scheduler");
    id
}

/// Wake a parked task, re-queueing it as ready.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_wake(id: u64) {
    let _ = try_wake_parked_task(id);
}

/// Wake a channel waiter and report whether it actually consumed the wake.
/// A caller can skip stale/cancelled/already-runnable waiter ids until one
/// parked task becomes Ready, avoiding both lost capacity and a thundering
/// herd when a bounded channel frees one slot.
pub(crate) fn try_wake_parked_task(id: u64) -> bool {
    crate::gc::stress_collect("scheduler");
    let transitioned = wake_global_task(id);
    if transitioned {
        crate::observability::record(
            crate::observability::RuntimeEventKind::TaskWake,
            current_task_id().map(|_| current_worker()),
            id,
            0,
        );
    }
    // Signal idle keep-alive waiters (blocked-syscall arm) that new work may
    // exist (willow-5if8).
    notify_idle_waiters();
    crate::gc::stress_collect("scheduler");
    transitioned
}

/// The id of the currently-running task (0 if none). Used by blocking runtime
/// primitives (e.g. cooperative channel `recv`) to register the running task as
/// a waiter before it suspends (willow-dsw).
/// Request cooperative cancellation of `id` (willow-0a6k.7). A parked task is
/// re-queued so the cancellation is observed promptly; the task is finalized
/// (state Cancelled, never polled again) at the next scheduler claim.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_cancel(id: u64) {
    let id = id as RuntimeTaskId;
    let tasks = global_task_table();
    let run_queues = global_run_queues();
    let changed = tasks
        .with_mut(id, |task| {
            let before = task.state.lifecycle();
            let outcome = task.state.request_cancel();
            if outcome != CancelOutcome::NoChange {
                // Mirror the request into the frame header so
                // `Task::is_cancelled()` is a plain load (willow-ezs.1.3).
                crate::async_frame::frame_request_cancel(task.frame);
            }
            if outcome == CancelOutcome::Enqueue {
                task.wake_deadline = None;
                run_queues.push_global(id);
            }
            tasks.reconcile_blocked_transition(before, task.state.lifecycle());
            outcome != CancelOutcome::NoChange
        })
        .unwrap_or(false);
    if changed {
        crate::observability::record(
            crate::observability::RuntimeEventKind::TaskCancelRequested,
            current_task_id().map(|_| current_worker()),
            id,
            0,
        );
    }
}

/// Record the source location of the call that spawned task `id` (file is a
/// WillowString; copied out of the GC heap). Shown in panic/debug traces
/// (willow-0a6k.7).
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_set_spawn_site(id: u64, file: *const u8, line: i64) {
    let file = unsafe { crate::string::willow_string_as_str(file) }.to_string();
    let id = id as RuntimeTaskId;
    global_task_table().with_mut(id, |task| {
        task.set_spawn_site(file, line as u32);
    });
}

/// Record that `task_id` registered as a waiter on the channel at `addr`
/// (deduplicated; willow-p4er reverse reference for O(registered)
/// cancellation cleanup).
pub(crate) fn record_channel_wait(task_id: u64, addr: usize) {
    global_task_table().with_mut(task_id as RuntimeTaskId, |task| {
        task.add_wait_channel(addr);
    });
}

/// Remove one channel reverse reference after normal unregister/wake. Without
/// this, cancellation cost grows with every distinct channel the task has ever
/// waited on and can retain stale, already-swept channel addresses.
pub(crate) fn remove_channel_wait(task_id: u64, addr: usize) {
    global_task_table().with_mut(task_id as RuntimeTaskId, |task| {
        task.remove_wait_channel(addr);
    });
}

/// Take (and clear) the channels `task_id` registered on (willow-p4er).
pub(crate) fn take_channel_waits(task_id: u64) -> Vec<usize> {
    global_task_table()
        .with_mut(task_id as RuntimeTaskId, RuntimeTask::take_wait_channels)
        .unwrap_or_default()
}

// ── Lock waiter reverse links (willow-38w.1.2) ──────────────────────────────
//
// The task-shard half of the `LockState -> TaskShard` protocol in
// `lock_wait.rs`. Each of these takes exactly one shard lock and releases it
// before returning, so a caller holding a lock's state never holds a shard
// across anything else, and a caller holding no lock state (cancellation) never
// acquires one while a shard is held.

/// Publish `link` as `task_id`'s lock registration. Returns whether it was
/// installed: an unknown, terminal, or cancel-requested task is not eligible to
/// join a wait queue, and neither is one that already has a registration.
///
/// The eligibility check happens inside the shard's critical section, so the
/// lock cannot enqueue a task that became terminal a moment earlier.
pub(crate) fn install_lock_wait_link(
    task_id: RuntimeTaskId,
    link: LockWaitLink,
    publish_lock_side: impl FnOnce() -> bool,
) -> bool {
    let mut publish_lock_side = Some(publish_lock_side);
    global_task_table()
        .with_mut(task_id, |task| {
            let state = task.state.load();
            let lifecycle = state.lifecycle();
            if lifecycle.is_terminal()
                || lifecycle == TaskLifecycle::Cancelling
                || state.cancel_requested()
            {
                return false;
            }
            if task.lock_wait().is_some() {
                return false;
            }
            if !(publish_lock_side.take().expect("lock publish callback"))() {
                return false;
            }
            assert!(
                task.install_lock_wait(link),
                "lock wait link changed while its TaskShard was held"
            );
            true
        })
        .unwrap_or(false)
}

/// Move `task_id`'s registration `Waiting -> HandoffOwned` for exactly
/// `(lock_id, token)`. Returns whether the transition happened; the lock treats
/// `false` as "this candidate was cancelled or reaped, skip it".
///
/// A terminal task is refused here rather than in the lock: handing ownership to
/// a task that will never be polled again would strand the lock.
pub(crate) fn promote_lock_wait_link(
    task_id: RuntimeTaskId,
    lock_id: LockId,
    token: RegistrationToken,
    reserve_lock_side: impl FnOnce(),
) -> bool {
    let mut reserve_lock_side = Some(reserve_lock_side);
    global_task_table()
        .with_mut(task_id, |task| {
            if task.state.lifecycle().is_terminal() {
                return false;
            }
            if !task.promote_lock_wait(lock_id, token) {
                return false;
            }
            (reserve_lock_side.take().expect("lock reserve callback"))();
            true
        })
        .unwrap_or(false)
}

/// Reconcile a handoff whose scheduler wake observed a terminal task. The lock
/// side callback runs while the task shard is still held when the record exists;
/// when it has already been reaped, task ids are never reused and the callback
/// runs immediately afterwards. The caller holds LockState in both cases.
pub(crate) fn revoke_terminal_lock_handoff(
    task_id: RuntimeTaskId,
    lock_id: LockId,
    token: RegistrationToken,
    clear_lock_side: impl FnOnce(),
) -> bool {
    let mut clear_lock_side = Some(clear_lock_side);
    let removed = global_task_table().with_mut(task_id, |task| {
        let removed = task.take_lock_handoff(lock_id, token).is_some();
        (clear_lock_side.take().expect("lock clear callback"))();
        removed
    });
    if removed.is_none() {
        (clear_lock_side.take().expect("lock clear callback"))();
    }
    removed.unwrap_or(false)
}

/// Take `task_id`'s lock registration, whatever phase it is in. The caller
/// reconciles it at the lock afterwards, with no shard held.
pub(crate) fn take_lock_wait_link(task_id: RuntimeTaskId) -> Option<LockWaitLink> {
    global_task_table()
        .with_mut(task_id, RuntimeTask::take_lock_wait)
        .flatten()
}

/// Drop `task_id`'s registration only if it is exactly `(lock_id, token)` in
/// `HandoffOwned` — the resumed frame consuming its reserved ownership.
pub(crate) fn consume_lock_handoff_link(
    task_id: RuntimeTaskId,
    lock_id: LockId,
    token: RegistrationToken,
) -> bool {
    global_task_table()
        .with_mut(task_id, |task| {
            task.take_lock_handoff(lock_id, token).is_some()
        })
        .unwrap_or(false)
}

/// `task_id`'s current registration, for cleanup paths and diagnostics.
pub(crate) fn lock_wait_link(task_id: RuntimeTaskId) -> Option<LockWaitLink> {
    global_task_table()
        .with(task_id, RuntimeTask::lock_wait)
        .flatten()
}

#[cfg(test)]
pub(crate) fn task_waiter_count_for_test(task_id: RuntimeTaskId) -> usize {
    global_task_table()
        .with(task_id, RuntimeTask::waiter_count)
        .unwrap_or(0)
}

/// Wake a task that was just handed a lock. Deliberately the ordinary wake path:
/// the lock runtime does not resolve the wake/park race itself, it relies on the
/// task state word (a wake landing during a poll is recorded and consumed at the
/// poll boundary rather than lost).
pub(crate) fn wake_lock_waiter(task_id: RuntimeTaskId) -> WakeOutcome {
    crate::gc::stress_collect("scheduler");
    let outcome = wake_global_task_outcome(task_id);
    notify_idle_waiters();
    crate::gc::stress_collect("scheduler");
    outcome
}

/// True (1) if `id` was cancel-requested or already finalized as Cancelled.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_is_cancelled(id: u64) -> i64 {
    let id = id as RuntimeTaskId;
    global_task_table()
        .with(id, |task| {
            let state = task.state.load();
            state.cancel_requested() || state.lifecycle() == TaskLifecycle::Cancelling
        })
        .unwrap_or(false) as i64
}

/// Post-await fallback for a null frame (willow-0a6k.7): a CANCELLED task has
/// no result to read, so it is a located runtime panic.
fn sched_await_check(id: u64) {
    // The lifecycle collapses every terminal outcome into `Terminal`, so the
    // id-only path cannot tell Panicked from Completed. The gate answers the
    // question that matters: an unhandled panic is being published, and no
    // awaiter may resume before the abort (willow-s9ej.7).
    if fatal_panic_pending() {
        park_until_fatal_abort();
    }
    let id = id as RuntimeTaskId;
    let cancelled = global_task_table()
        .with(id, |task| {
            task.state.lifecycle() == TaskLifecycle::Cancelling
        })
        .unwrap_or(false);
    if cancelled {
        report_await_of_cancelled_task(id);
    }
}

/// The recoverable language fault shared by the id-only and frame-backed await
/// checks. Cancellation itself remains an ordinary terminal task state;
/// strict `await` is the operation that turns it into a panic.
fn report_await_of_cancelled_task(id: u64) {
    crate::panic_context::raise_language_message(&format!("awaited a cancelled task (task {id})"));
}

/// A poll ABI violation cannot be treated as an ordinary Pending task: no
/// event is guaranteed to wake it. Publish Panicked/reap first, then terminate
/// the process according to Willow's current non-recoverable panic policy.
fn report_poll_failure(id: u64, invalid_result: Option<i32>, async_chain: &str) -> ! {
    match invalid_result {
        Some(result) => {
            eprintln!("runtime panic: task {id} returned invalid poll status {result}");
        }
        None => {
            eprintln!("runtime panic: task {id} returned panicked poll status");
        }
    }
    crate::stack_trace::print_current_call_stack();
    if !async_chain.is_empty() {
        eprintln!("{async_chain}");
    }
    std::process::abort();
}

/// Attach the compiler-generated cancellation cleanup entry to a task
/// (willow-vynv.3). Called by the async-fn constructor right after spawn.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_set_cancel_fn(id: u64, cancel: RuntimeCancelFn) {
    let id = id as RuntimeTaskId;
    global_task_table().with_mut(id, |task| {
        task.cancel = Some(cancel);
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_current_task() -> u64 {
    current_task_id().unwrap_or(0)
}

/// Tag the currently-running task with its async fn name (raw static UTF-8 bytes
/// plus length). Emitted at the top of each async poll fn so a panic can render
/// the async chain (willow-9lw). No-op when no task is running.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_tag_current_task(name: *const u8, name_len: i64) {
    if name.is_null() || name_len <= 0 {
        return;
    }
    let bytes = unsafe { std::slice::from_raw_parts(name, name_len as usize) };
    let name = String::from_utf8_lossy(bytes).into_owned();
    if let Some(id) = current_task_id() {
        global_task_table().with_mut(id, |task| task.set_name(name));
    }
}

/// Render the active async chain (currently-running task first, then the tasks
/// awaiting it, transitively) for panic diagnostics. Empty when no async task is
/// running (willow-9lw).
/// Idle notification (willow-5if8): a generation counter + condvar bumped by
/// `willow_sched_wake`, so a worker keeping the scheduler alive for a
/// BlockedSyscall task WAITS for the completion signal instead of spinning at
/// 1ms. A 50ms bounded wait remains as a portable fallback/timeout.
static IDLE_GEN: std::sync::Mutex<u64> = std::sync::Mutex::new(0);
static IDLE_CONDVAR: std::sync::Condvar = std::sync::Condvar::new();

fn notify_idle_waiters() {
    let mut generation = IDLE_GEN.lock().unwrap_or_else(|p| p.into_inner());
    *generation = generation.wrapping_add(1);
    IDLE_CONDVAR.notify_all();
}

fn current_wake_generation() -> u64 {
    *IDLE_GEN.lock().unwrap_or_else(|p| p.into_inner())
}

/// Wait until a wake advances beyond the caller's snapshot, bounded by
/// `timeout`. A wake that occurs between the scheduler-state check and this
/// function is observed immediately instead of being lost.
fn wait_for_wake_since(start: u64, timeout: Duration) -> bool {
    let generation = IDLE_GEN.lock().unwrap_or_else(|p| p.into_inner());
    if *generation != start {
        return true;
    }
    let (generation, _) = IDLE_CONDVAR
        .wait_timeout_while(generation, timeout, |generation| *generation == start)
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *generation != start
}

pub fn async_chain_text() -> String {
    with_global(|sched| {
        let Some(mut id) = current_task_id() else {
            return String::new();
        };
        let mut lines = Vec::new();
        let mut seen = std::collections::HashSet::new();
        // Walk current task -> its awaiter -> ... via the reverse `waiters` link.
        while seen.insert(id) {
            let Some(task) = sched.task(id) else { break };
            let name = task.name().unwrap_or("<async task>");
            let site = match task.spawn_site() {
                Some((file, line)) => format!(" [task {id}, spawned at {file}:{line}]"),
                None => format!(" [task {id}]"),
            };
            lines.push(format!("  {}: async {}{}", lines.len(), name, site));
            // The first waiter is the awaiter that suspended on this task.
            // `first_live` scans tombstones, which is fine here: this runs once
            // while rendering a panic trace, never on a scheduling hot path.
            match task.first_live_waiter() {
                Some(awaiter) => id = awaiter,
                None => break,
            }
        }
        if lines.is_empty() {
            return String::new();
        }
        let mut out = String::from("async stack (current task first):");
        for line in lines {
            out.push('\n');
            out.push_str(&line);
        }
        out
    })
}

/// Requested worker count from `WILLOW_WORKERS`, or 5 by default.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_requested_workers() -> u64 {
    runtime_worker_config().requested_workers() as u64
}

/// Worker count the current runtime will actually run. Defaults to 5;
/// `WILLOW_WORKERS=N` overrides it; values below 5 are clamped to 5.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_active_workers() -> u64 {
    runtime_worker_config().active_workers() as u64
}

/// Deterministic scheduler ownership counters for diagnostics and plateau
/// tests (willow-ezs.1.5). They intentionally do not claim to measure RSS.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_heavy_task_count() -> u64 {
    with_global(|sched| sched.metadata_snapshot().heavy_tasks as u64)
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_queue_entry_count() -> u64 {
    with_global(|sched| sched.metadata_snapshot().queue_entries as u64)
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_pending_cleanup_count() -> u64 {
    with_global(|sched| sched.metadata_snapshot().pending_cleanups as u64)
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_frame_root_count() -> u64 {
    with_global(|sched| sched.metadata_snapshot().frame_roots as u64)
}

/// Register a wake-deadline on the currently-running task: after the poll fn
/// returns Pending, the timer-aware run loop wakes it once `millis` elapse.
/// Called by a cooperative poll fn that is awaiting a sleep (willow-lpn.5.3).
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_sleep(millis: i64) {
    set_global_wake_after_millis(millis);
    crate::gc::stress_collect("await");
}

/// Cooperatively yield the currently-running task. The compiler emits this from
/// `await yield()` immediately before returning Pending from the poll fn.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_yield() {
    if let Some(id) = current_task_id() {
        global_task_table().with_mut(id, |task| task.yield_requested = true);
    }
    crate::gc::stress_collect("await");
}

/// Await another task's completion (for `await <task>`): returns 1 if `awaitee`
/// has already completed (the caller may read its result and continue), else
/// registers the currently-running task as a waiter and returns 0 — the caller
/// then returns Pending and is woken when `awaitee` completes (willow-lpn.5.3).
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_await(awaitee: u64) -> i32 {
    let tasks = global_task_table();
    let ready = match current_task_id() {
        Some(waiter) if register_waiter_sharded(&tasks, awaitee, waiter) => 0,
        Some(_) => 1,
        None => i32::from(tasks.with(awaitee, |_| ()).is_none()),
    };
    crate::gc::stress_collect("await");
    ready
}

/// Frame-backed `await <task>` (willow-ezs.1.3).
///
/// Same contract as [`willow_sched_await`], but the terminal check is an
/// `Acquire` load of the awaitee's frame-header status instead of a scheduler
/// table lookup: a finished task is observed without taking the global
/// scheduler lock, and the answer no longer depends on the heavy `RuntimeTask`
/// record still being retained. The scheduler is consulted only on the slow
/// path, to register the waiter.
///
/// `frame` null falls back to the id-only path so a synthetic/placeholder task
/// still behaves.
#[unsafe(no_mangle)]
pub extern "C" fn willow_frame_await(frame: *mut c_void, awaitee: u64) -> i32 {
    if frame.is_null() {
        return willow_sched_await(awaitee);
    }
    if crate::async_frame::frame_is_terminal(frame) {
        crate::gc::stress_collect("await");
        return 1;
    }
    willow_sched_await(awaitee)
}

/// Frame-backed post-await check (willow-ezs.1.3): the cancelled test is a
/// header load, and the id is carried only for the diagnostic message.
#[unsafe(no_mangle)]
pub extern "C" fn willow_frame_await_check(frame: *mut c_void, id: u64) {
    if frame.is_null() {
        sched_await_check(id);
        return;
    }
    if fatal_panic_pending() {
        // Awaiting a task whose panic escaped is not recoverable and has no
        // result to read: the process is aborting, so this task stops here
        // instead of running its continuation. The gate closes before the
        // PANICKED status is published, so a woken awaiter always observes it
        // (willow-s9ej.7). Frame status alone is NOT the trigger: a PANICKED
        // frame with no abort in flight (a recovered panic republished by a
        // test harness) must still return normally.
        park_until_fatal_abort();
    }
    let status = crate::async_frame::frame_terminal_status(frame);
    if status == crate::async_frame::WILLOW_FRAME_STATUS_CANCELLED {
        report_await_of_cancelled_task(id);
    }
}

/// Milliseconds since process start (monotonic), for select timeout cases:
/// the deadline is fixed once at select entry and re-checked on every
/// (re-)probe (willow-soro).
#[unsafe(no_mangle)]
pub extern "C" fn willow_monotonic_millis() -> i64 {
    static START: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);
    START.elapsed().as_millis() as i64
}

/// Sleep the CALLING OS THREAD until the monotonic deadline (sync select's
/// timeout wait when nothing else can progress; willow-soro). No-op if the
/// deadline already passed.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sleep_until_monotonic(deadline_ms: i64) {
    let now = willow_monotonic_millis();
    if deadline_ms > now {
        std::thread::sleep(Duration::from_millis((deadline_ms - now) as u64));
    }
}

/// Remove the currently-running task from `awaitee`'s waiter list — a select
/// that registered on a task-completion case must unregister when another
/// case wins, exactly like channel waiters (willow-soro).
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_unregister_task_waiter(awaitee: u64) {
    let Some(current) = current_task_id() else {
        return;
    };
    unregister_waiter_sharded(&global_task_table(), awaitee, current);
}

/// Current state of a task as an integer: 0 ready, 1 running, 2 parked,
/// 3 completed, 4 panicked, 5 cancelled, 6 cancelling, 7 blocked-syscall,
/// -1 unknown.
///
/// This is the ID-ONLY query, and it is answered from the scheduler's task
/// table. It is therefore a DIAGNOSTIC: once a finished task is reaped the
/// record is gone and this returns -1 (Unknown), with no tombstone kept
/// (willow-ezs.1.3). Language-visible questions about a task that the caller
/// holds a handle to must use the frame-backed queries — `willow_frame_status`,
/// `willow_frame_await`, `willow_frame_await_check`, `willow_frame_is_cancelled`
/// — whose answer is stored in the frame the handle already points at and so
/// survives reaping.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_task_state(id: u64) -> i32 {
    match global_task_table().with(id, |task| task.state.lifecycle()) {
        Some(TaskLifecycle::Ready) => 0,
        Some(TaskLifecycle::Running) => 1,
        Some(TaskLifecycle::Parked) => 2,
        Some(TaskLifecycle::Cancelling) => 6,
        Some(TaskLifecycle::BlockedSyscall) => 7,
        // Terminal records are removed immediately and the atomic lifecycle
        // intentionally does not duplicate Completed/Cancelled/Panicked. The
        // frame status is the authoritative terminal diagnostic.
        Some(TaskLifecycle::Terminal) | None => -1,
    }
}

// Drive the global scheduler until no task is ready (idle). Each ready task is
// polled once: `Ready` completes it (and unroots its frame); `Pending` parks it
// (a waker must later re-queue it). Returns the number of tasks completed.
//
// The poll function is invoked with no scheduler borrow held, so a task may
// re-enter the scheduler (spawn/wake) from inside its own poll.
thread_local! {
    /// Re-entrancy depth of `willow_sched_run` on this thread. `await` block-runs
    /// the scheduler recursively, so the driver registers as a GC mutator on the
    /// OUTERMOST entry and unregisters on the matching exit (willow-6fv.5.6).
    static SCHED_RUN_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_run() -> i64 {
    sched_run_with_mutator(None, None)
}

/// Drive the scheduler only until `target` completes (or the scheduler goes
/// genuinely idle), then return — the `await` of a concrete task
/// handle (willow-bsqy). Reuses the mutator-registration wrapper so GC
/// coordination is identical to `willow_sched_run`.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_run_until(target: u64) -> i64 {
    sched_run_with_mutator(Some(target), None)
}

/// Drive the scheduler, but no longer than until the absolute monotonic
/// deadline `deadline_ms` (the `willow_monotonic_millis` base).
///
/// A sync `select` with a `sleep(ms)` case owns a deadline that belongs to no
/// task, so an unbounded `willow_sched_run()` could sit inside a five-second
/// task before ever re-checking a thirty-millisecond timeout. This variant
/// stops the run loop once the deadline passes and clamps every idle wait to
/// it, so the caller always gets its turn back in time (willow-o038 review).
///
/// Like `willow_sched_run`, it returns as soon as the scheduler has nothing
/// left to do — the caller then decides whether to wait out the deadline.
/// Returns the number of tasks completed.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_run_until_deadline(deadline_ms: i64) -> i64 {
    let remaining = (deadline_ms - willow_monotonic_millis()).max(0) as u64;
    let deadline = Instant::now() + Duration::from_millis(remaining);
    sched_run_with_mutator(None, Some(deadline))
}

/// True while some source can still make a parked or blocked task runnable:
/// queued work, a claim in flight, an armed timer, a parked netpoll waiter, or
/// an outstanding blocking-pool syscall.
///
/// A drive returning "no task completed" is NOT the same as "nothing can ever
/// happen": the drive may simply have raced a claim, or stopped on its own
/// deadline. Callers that turn quiescence into a blocking-forever diagnostic
/// must consult this first (willow-atth).
pub(crate) fn scheduler_has_wake_source() -> bool {
    claims_in_flight()
        || global_run_queues().len() > 0
        || global_next_timer_deadline().is_some()
        || crate::netpoll::has_waiters()
        || global_task_table().blocked_syscall_count() > 0
}

/// Park a synchronous caller briefly while it waits for a wake source to fire.
/// Bounded so a missed notification degrades into a re-probe rather than a
/// hang, and cut short by the next wake.
pub(crate) fn wait_for_any_wake_briefly() {
    let generation = current_wake_generation();
    wait_for_wake_since(generation, Duration::from_millis(1));
}

/// Idle step for a `select` with no `default` and no timeout case.
///
/// Such a select must block until one of its cases becomes ready: falling
/// through would run no case at all and silently continue past the select. The
/// synchronous lowering re-probes in a loop and calls this whenever a scheduler
/// drive reported no progress. While any wake source is live this waits briefly
/// and returns, so the caller re-probes. When nothing can make a case ready the
/// select would block forever, so it raises a language panic instead of
/// spinning — the same policy sync channel `recv`/`send` use for a blocking
/// operation with no counterpart (willow-atth).
#[unsafe(no_mangle)]
pub extern "C" fn willow_select_idle_wait() {
    if scheduler_has_wake_source() {
        wait_for_any_wake_briefly();
        return;
    }
    // Re-check once after a grace wait: a wake source can be published by
    // another worker in the window between its drive returning and this check.
    wait_for_any_wake_briefly();
    if scheduler_has_wake_source() {
        return;
    }
    crate::panic_context::raise_language_message(
        "select would block forever: no case can become ready and there is no default case",
    );
}

fn sched_run_with_mutator(target: Option<RuntimeTaskId>, deadline: Option<Instant>) -> i64 {
    if crate::panic_context::panic_unwind_cleanup_active() {
        crate::panic_context::fatal_invariant(
            "scheduler re-entry attempted from panic-unwinding defer",
        );
    }
    // Register the driver thread as a GC mutator while it drives tasks so a
    // future parallel collector can stop it at a safepoint. Single-mutator runs
    // have exactly one registered thread, so `multi_mutator_active()` stays false
    // and GC behavior is unchanged (willow-6fv.5.6).
    let saved_panic_context = crate::panic_context::current_context();
    let outermost = SCHED_RUN_DEPTH.with(|d| {
        let depth = d.get();
        d.set(depth + 1);
        depth == 0
    });
    let saved_running = if outermost { None } else { current_task_id() };
    let shared_state = CURRENT_RUN_STATE.with(|slot| slot.borrow().clone());
    let paused_parallel_poll = !outermost && shared_state.is_some() && saved_running.is_some();
    if paused_parallel_poll && let Some(state) = shared_state.as_ref() {
        let previous = state.active_polls.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "parallel poll depth underflow");
        state.paused_polls.fetch_add(1, Ordering::AcqRel);
    }
    if outermost {
        crate::gc::willow_gc_register_mutator();
    }
    let active_workers = runtime_worker_config().active_workers();
    let completed = if outermost && active_workers > 1 {
        willow_sched_run_parallel(target, active_workers, deadline)
    } else if let Some(state) = shared_state.as_deref() {
        scheduler_run_loop(target, current_worker(), Some(state), false, deadline)
    } else {
        scheduler_run_loop(target, current_worker(), None, false, deadline)
    };
    // External registrations never require the worker pool to remain alive,
    // so purge them promptly after each drive, including nested drives.
    drain_terminal_cleanups();
    if let Some(id) = saved_running {
        set_current_task(Some(id));
        // A nested drive temporarily replaces only the thread-local
        // current-task marker. The outer task continues to own its frame, so
        // its atomic lifecycle remains Running/Cancelling throughout.
        let preempt_flag = global_task_table().with(id, RuntimeTask::preempt_flag_ptr);
        if let Some(flag) = preempt_flag {
            crate::preempt::willow_preempt_begin(flag);
        }
    } else {
        // Restore the synchronous entry context (or the caller's explicit
        // context) after an outer scheduler drive. Worker task switches clear
        // TLS between polls, so restoration belongs to the drive boundary.
        crate::panic_context::replace_current_context(saved_panic_context);
    }
    if paused_parallel_poll && let Some(state) = shared_state.as_ref() {
        let previous = state.paused_polls.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "parallel paused poll underflow");
        state.active_polls.fetch_add(1, Ordering::AcqRel);
    }
    if outermost {
        // The parallel pool has joined and nested polls have resumed or
        // quiesced. It is now safe to remove every terminal frame runtime root
        // retired during this outer drive.
        release_pending_frame_roots();
    }
    if SCHED_RUN_DEPTH.with(|d| {
        let depth = d.get() - 1;
        d.set(depth);
        depth == 0
    }) {
        crate::gc::willow_gc_unregister_mutator();
    }
    completed
}

fn drain_terminal_cleanups() {
    let cleanups = with_global(|sched| sched.take_pending_terminal_cleanups());
    for cleanup in cleanups {
        crate::netpoll::purge_task(cleanup.task_id);
        crate::channel::purge_task_from_addresses(cleanup.task_id, cleanup.channel_waits);
        // A task that dies while queued on (or holding a reservation for) a
        // scheduler-aware lock must not strand it: phase-driven cleanup removes
        // a `Waiting` entry, and re-hands ownership that was already reserved
        // for this task (willow-38w.1.3, spec §12.3).
        crate::async_mutex::purge_captured_task_lock_wait(cleanup.task_id, cleanup.lock_wait);
    }
}

fn release_pending_frame_roots() {
    let frames = with_global(|sched| sched.take_pending_frame_unroots());
    for frame in frames {
        crate::gc::willow_gc_remove_runtime_root(frame as *mut u8);
    }
}

#[derive(Debug, Default)]
struct ParallelRunState {
    stop: AtomicBool,
    claim_gate: Mutex<()>,
    active_polls: AtomicUsize,
    paused_polls: AtomicUsize,
    completed: AtomicI64,
}

fn willow_sched_run_parallel(
    target: Option<RuntimeTaskId>,
    workers: usize,
    deadline: Option<Instant>,
) -> i64 {
    let state = Arc::new(ParallelRunState::default());
    std::thread::scope(|scope| {
        for worker in 1..workers {
            let state = Arc::clone(&state);
            scope.spawn(move || {
                run_parallel_worker(worker, target, state, deadline);
            });
        }
        let main_state = Arc::clone(&state);
        with_parallel_context(0, main_state, || {
            scheduler_run_loop(target, 0, Some(state.as_ref()), true, deadline);
        });
        state.stop.store(true, Ordering::Release);
        // A worker can pass its loop-level stop check immediately before worker
        // 0 publishes the stop. Synchronize with the claim gate so no task can
        // become active after this barrier, then remain
        // a cooperating mutator until every in-flight/nested poll has crossed
        // its post-poll GC boundaries.
        drop(
            state
                .claim_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        while state.active_polls.load(Ordering::Acquire) > 0
            || state.paused_polls.load(Ordering::Acquire) > 0
        {
            crate::gc::willow_gc_safepoint();
            std::thread::yield_now();
        }
    });
    state.completed.load(Ordering::Acquire)
}

fn run_parallel_worker(
    worker: usize,
    target: Option<RuntimeTaskId>,
    state: Arc<ParallelRunState>,
    deadline: Option<Instant>,
) {
    SCHED_RUN_DEPTH.with(|depth| depth.set(1));
    crate::gc::willow_gc_register_mutator();
    let worker_state = Arc::clone(&state);
    with_parallel_context(worker, worker_state, || {
        scheduler_run_loop(target, worker, Some(state.as_ref()), true, deadline);
    });
    set_current_task(None);
    crate::gc::willow_gc_unregister_mutator();
    SCHED_RUN_DEPTH.with(|depth| depth.set(0));
}

fn with_parallel_context<R>(
    worker: usize,
    state: Arc<ParallelRunState>,
    f: impl FnOnce() -> R,
) -> R {
    let previous_worker = CURRENT_WORKER.with(|slot| {
        let previous = slot.get();
        slot.set(worker);
        previous
    });
    let previous_state = CURRENT_RUN_STATE.with(|slot| slot.replace(Some(state)));
    let result = f();
    CURRENT_RUN_STATE.with(|slot| {
        slot.replace(previous_state);
    });
    CURRENT_WORKER.with(|slot| slot.set(previous_worker));
    result
}

fn target_is_done(target: Option<RuntimeTaskId>) -> bool {
    let Some(t) = target else {
        return false;
    };
    !global_task_table()
        .with(t, |task| !task.state.lifecycle().is_terminal())
        .unwrap_or(false)
}

fn finish_active_poll(shared: Option<&ParallelRunState>) {
    if let Some(state) = shared {
        let previous = state.active_polls.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "parallel poll depth underflow");
    }
}

fn record_completed_task(completed: &mut i64, shared: Option<&ParallelRunState>) {
    *completed += 1;
    if let Some(state) = shared {
        state.completed.fetch_add(1, Ordering::AcqRel);
    }
}

/// Pop/steal without the scheduler metadata mutex, then take only the task
/// shard needed for atomic state validation and task-work lookup (willow-8agm).
fn claim_global_ready_for_worker(
    worker: usize,
    shared: Option<&ParallelRunState>,
) -> Option<(RuntimeTaskId, ClaimedTaskWork)> {
    enum Claim {
        Work(ClaimedTaskWork, Arc<crate::panic_context::PanicContext>),
        FinalizeCancelled,
        Drop,
    }

    let queues = global_run_queues();
    let tasks = global_task_table();
    loop {
        // Publish the claim BEFORE the pop removes the id from every queue, and
        // keep it published until the claim has resolved into an active poll or
        // a requeue (willow-atth).
        let _in_flight = ClaimInFlight::enter();
        let id = queues.pop_for_worker(worker)?;
        let claim_guard = shared.map(|state| {
            state
                .claim_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        });
        if shared.is_some_and(|state| state.stop.load(Ordering::Acquire)) {
            drop(claim_guard);
            // The queue token is still set because no claim occurred.
            queues.push_global(id);
            return None;
        }
        let claim = tasks
            .with_mut(id, |task| match task.claim_for_poll() {
                ClaimOutcome::Drop => Claim::Drop,
                ClaimOutcome::Poll => {
                    task.yield_requested = false;
                    Claim::Work(
                        task.poll
                            .map(|poll| (poll, task.frame, task.preempt_flag_ptr())),
                        task.panic_context(),
                    )
                }
                ClaimOutcome::Cancel if task.cancel.is_some() && !task.frame.is_null() => {
                    Claim::Work(
                        task.poll
                            .map(|poll| (poll, task.frame, task.preempt_flag_ptr())),
                        task.panic_context(),
                    )
                }
                ClaimOutcome::Cancel => Claim::FinalizeCancelled,
            })
            .unwrap_or(Claim::Drop);
        if matches!(claim, Claim::Work(..))
            && let Some(state) = shared
        {
            state.active_polls.fetch_add(1, Ordering::AcqRel);
        }
        drop(claim_guard);
        match claim {
            Claim::Work(work, panic_context) => {
                set_current_task_context(id, panic_context);
                return Some((id, work));
            }
            Claim::FinalizeCancelled => {
                with_global(|sched| sched.finalize_cancelled(id));
                set_current_task(None);
            }
            Claim::Drop => {}
        }
    }
}

fn duration_until(deadline: Instant) -> Duration {
    deadline
        .checked_duration_since(Instant::now())
        .unwrap_or_default()
}

fn bounded_parallel_wait(duration: Duration) -> Duration {
    std::cmp::min(duration, Duration::from_millis(1))
}

/// Clamp an idle wait to a drive deadline, so a drive bounded by the caller's
/// own deadline never blocks past it on an unrelated (possibly far-off) timer.
fn deadline_bounded(wait: Duration, deadline: Option<Instant>) -> Duration {
    match deadline {
        Some(d) => std::cmp::min(wait, duration_until(d)),
        None => wait,
    }
}

fn scheduler_idle_step(
    worker: usize,
    shared: Option<&ParallelRunState>,
    keep_alive_for_paused: bool,
    deadline: Option<Instant>,
) -> bool {
    let parallel = shared.is_some();

    // A worker may have claimed the last ready task immediately before this
    // worker observed an empty queue. Read the poll count only after that queue
    // observation: using a value captured earlier can falsely declare global
    // idle while the other worker is still publishing a timer/netpoll waiter.
    // A claim that has popped its task but not yet reached `active_polls` is
    // equally in-flight work, and is invisible in every other check
    // (willow-atth).
    if claims_in_flight()
        || shared.is_some_and(|state| state.active_polls.load(Ordering::Acquire) > 0)
    {
        std::thread::sleep(Duration::from_millis(1));
        return true;
    }

    let earliest = global_next_timer_deadline();
    if crate::netpoll::has_waiters() {
        if !parallel || worker == 0 {
            let timeout = if parallel {
                Some(
                    earliest
                        .map(|(_, deadline)| bounded_parallel_wait(duration_until(deadline)))
                        .unwrap_or_else(|| Duration::from_millis(1)),
                )
            } else {
                earliest.map(|(_, deadline)| duration_until(deadline))
            };
            let timeout = match (timeout, deadline) {
                (Some(wait), _) => Some(deadline_bounded(wait, deadline)),
                // An unbounded netpoll wait must still respect a drive deadline.
                (None, Some(d)) => Some(duration_until(d)),
                (None, None) => None,
            };
            if crate::netpoll::wait_and_wake(timeout) > 0 {
                crate::gc::stress_collect("scheduler");
                return true;
            }
            // Parallel polling uses a bounded wait so worker 0 can also service
            // timers and scheduler state. A timeout is not global idleness:
            // the registered I/O task may simply not be ready yet.
            if parallel {
                return true;
            }
        } else {
            std::thread::sleep(Duration::from_millis(1));
            return true;
        }
    }

    match earliest {
        Some((_, timer_deadline)) => {
            let wait = duration_until(timer_deadline);
            if !wait.is_zero() {
                let wait = if parallel {
                    bounded_parallel_wait(wait)
                } else {
                    wait
                };
                let wait = deadline_bounded(wait, deadline);
                if !wait.is_zero() {
                    std::thread::sleep(wait);
                }
            }
            let woken = wake_global_due_timers(Instant::now());
            for _ in 0..woken {
                crate::gc::stress_collect("scheduler");
            }
            true
        }
        None if parallel
            && keep_alive_for_paused
            && shared.is_some_and(|state| state.paused_polls.load(Ordering::Acquire) > 0) =>
        {
            std::thread::sleep(Duration::from_millis(1));
            true
        }
        None => {
            // Snapshot BEFORE checking BlockedSyscall state. If completion
            // races after the check, wait_for_wake_since observes the changed
            // generation and returns immediately instead of sleeping 50ms.
            let generation = current_wake_generation();
            // Wake publishes its queue entry before decrementing the blocked
            // count. Observe those in the matching order: if the count is
            // already zero, the subsequent queue read must see the handoff.
            let has_blocked_syscall = global_task_table().blocked_syscall_count() > 0;
            if has_blocked_syscall {
                // The blocking-pool completion wake is the only signal, so
                // keep the scheduler alive. The 50ms bound is only a portable
                // fallback for missed/foreign notifications.
                wait_for_wake_since(
                    generation,
                    deadline_bounded(Duration::from_millis(50), deadline),
                );
                true
            } else if global_run_queues().len() > 0 {
                // Completion moved the task to Ready after this idle worker's
                // earlier empty-queue observation.
                true
            } else {
                false
            }
        }
    }
}

fn scheduler_run_loop(
    target: Option<RuntimeTaskId>,
    worker: usize,
    shared: Option<&ParallelRunState>,
    stop_pool_on_exit: bool,
    deadline: Option<Instant>,
) -> i64 {
    let mut completed = 0i64;
    loop {
        if fatal_panic_pending() {
            park_until_fatal_abort();
        }
        if shared.is_some_and(|state| state.stop.load(Ordering::Acquire)) {
            break;
        }
        // A drive deadline belongs to the CALLER (sync `select` with a
        // `sleep(ms)` case), not to any task: give the caller its turn back on
        // time instead of running unrelated tasks to quiescence first
        // (willow-o038 review).
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        // Stop as soon as the TARGET task (an `await` of a concrete
        // handle) is done, instead of draining the whole scheduler to quiescence
        // — so awaiting one task does not run unrelated tasks to completion and
        // cannot hang on an unrelated non-terminating task (willow-bsqy). A
        // completed task may have been pruned (state None); treat that as done
        // too — the awaiter reads the result from the frame, not the task.
        if target_is_done(target) {
            // The task state becomes Completed before its worker runs the
            // post-poll GC boundaries. Do not tear down the scoped pool while
            // that worker may still be collecting: the collector would wait
            // for worker 0 at a safepoint while worker 0 waits to join it.
            if stop_pool_on_exit && let Some(state) = shared {
                // Publish the stop while holding the same lock used to claim
                // work. Either an in-flight claim increments active_polls
                // before us, or it observes stop after us; there is no gap in
                // which worker 0 can start joining a newly active collector.
                let stopped = {
                    let _claim_gate = state
                        .claim_gate
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if state.active_polls.load(Ordering::Acquire) > 0
                        || state.paused_polls.load(Ordering::Acquire) > 0
                    {
                        false
                    } else {
                        state.stop.store(true, Ordering::Release);
                        true
                    }
                };
                if !stopped {
                    // Never enter the GC while holding `claim_gate`: a
                    // collector can be waiting for a worker that needs this
                    // gate before it reaches its own safepoint.
                    crate::gc::willow_gc_safepoint();
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
            }
            break;
        }
        // Cooperative GC safepoint: cheap (one atomic load) when no collection is
        // pending; lets a parallel collector stop this driver between task polls
        // (willow-6fv.5.6).
        crate::gc::willow_gc_safepoint();
        // A runnable CPU task can keep the ready queue non-empty forever.
        // Promote expired timers before selecting work so those tasks still get
        // a turn without waiting for the scheduler to become idle. This runs on
        // every worker on every iteration, so it must stay cheap when no timer
        // exists: `wake_global_due_timers` answers that case with one atomic
        // load and takes no lock at all (willow-9ha4).
        let woken_timers = wake_global_due_timers(Instant::now());
        let next = if shared.is_some_and(|state| state.stop.load(Ordering::Acquire)) {
            None
        } else {
            claim_global_ready_for_worker(worker, shared)
        };
        // A claim can finalize a cancel-requested task. Purge its captured
        // external registrations outside the scheduler lock before selecting
        // more work.
        drain_terminal_cleanups();
        for _ in 0..woken_timers {
            crate::gc::stress_collect("scheduler");
        }
        let Some((id, work)) = next else {
            // No ready task. If a parked task has a wake-deadline (e.g. it is
            // sleeping), block until the earliest one and wake it, then keep
            // running. If netpoll has parked I/O waiters, wait for readiness
            // first (bounded by the nearest timer deadline) and wake matching
            // tasks. Otherwise there is genuinely nothing left to do
            // (willow-lpn.5.3 / willow-lcw).
            // Only worker 0 decides that a parallel run is globally idle.
            // Letting any worker stop the pool races with another worker that
            // is publishing a timer/netpoll waiter as its poll returns Pending.
            if stop_pool_on_exit && shared.is_some() && worker != 0 {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            // A nested run_until may wait on a target whose poll is itself
            // paused inside another nested scheduler drive. Keep waiting while
            // any such target chain is paused instead of returning a zero/
            // uninitialized result to its awaiter.
            if scheduler_idle_step(
                worker,
                shared,
                stop_pool_on_exit || target.is_some(),
                deadline,
            ) {
                continue;
            }
            if stop_pool_on_exit && let Some(state) = shared {
                // Revalidate global idleness while claims are excluded. Work
                // can be published between the earlier empty pop and this
                // point; stopping without this check strands that task in the
                // queue.
                //
                // Check the timer heap BEFORE the run queue. Timer promotion
                // holds the heap lock until its queue entry is published, so
                // this ordering observes either the still-live timer or the
                // resulting runnable task. Reversing the reads permits this
                // lost-work interleaving:
                //
                //   idle: queue empty
                //   timer worker: pop timer, enqueue task, release timer lock
                //   idle: timer heap empty, stop
                //
                // Blocked-syscall completion similarly publishes its queue
                // entry before decrementing the counter, so read the counter
                // before the queue. None of these independently synchronized
                // structures requires GLOBAL_SCHEDULER (willow-9ha4).
                let _claim_gate = state
                    .claim_gate
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                // `claim_gate` excludes claims that have not popped yet, but a
                // claim that popped BEFORE this gate acquisition is waiting on
                // the gate right now with the task in hand: its id is in no
                // queue and in no poll counter, so only the in-flight marker
                // reveals it (willow-atth).
                let has_active_poll = claims_in_flight()
                    || state.active_polls.load(Ordering::Acquire) > 0
                    || state.paused_polls.load(Ordering::Acquire) > 0;
                let has_timer = global_next_timer_deadline().is_some();
                let has_blocked_syscall = global_task_table().blocked_syscall_count() > 0;
                let has_runnable = global_run_queues().len() > 0;
                let stopped = if has_active_poll || has_timer || has_blocked_syscall || has_runnable
                {
                    false
                } else {
                    state.stop.store(true, Ordering::Release);
                    true
                };
                if !stopped || crate::netpoll::has_waiters() {
                    state.stop.store(false, Ordering::Release);
                    continue;
                }
            }
            break;
        };
        // The gate can close between the check above and this claim, and the
        // task in hand may be exactly the awaiter that the fatal publication
        // just woke. Re-check before dispatching any work.
        if fatal_panic_pending() {
            park_until_fatal_abort();
        }
        // A task the claim moved to Cancelling: run its cleanup entry WITHOUT
        // the scheduler lock (poll-like), then finalize as Cancelled
        // (willow-vynv.3). The frame stays rooted until finalization.
        let cancel_work = take_global_cancel_work(id);
        if let Some((cancel_fn, cancel_frame)) = cancel_work {
            unsafe { cancel_fn(cancel_frame) };
            let cleanup_panicked = crate::panic_context::willow_panic_active() != 0;
            let panic_chain = cleanup_panicked.then(async_chain_text);
            if cleanup_panicked {
                // Same ordering rule as the poll path: close the claim gate
                // before the terminal publication wakes any awaiter.
                begin_fatal_panic();
            }
            with_global(|sched| {
                if cleanup_panicked {
                    sched.finalize_panicked(id);
                } else {
                    sched.finalize_cancelled(id);
                }
            });
            crate::observability::record(
                if cleanup_panicked {
                    crate::observability::RuntimeEventKind::TaskPanicked
                } else {
                    crate::observability::RuntimeEventKind::TaskCancelled
                },
                Some(worker),
                id,
                0,
            );
            drain_terminal_cleanups();
            crate::gc::stress_collect("scheduler");
            finish_active_poll(shared);
            if cleanup_panicked {
                crate::panic_context::finish_unhandled_with_async_chain(
                    panic_chain.as_deref().unwrap_or_default(),
                );
            }
            set_current_task(None);
            continue;
        }
        let Some((poll, frame, preempt_flag)) = work else {
            // Placeholder task with no executable work: just complete it.
            with_global(|sched| {
                sched.complete(id);
                sched.clear_running();
            });
            crate::observability::record(
                crate::observability::RuntimeEventKind::TaskComplete,
                Some(worker),
                id,
                0,
            );
            drain_terminal_cleanups();
            crate::gc::stress_collect("await");
            crate::gc::stress_collect("scheduler");
            finish_active_poll(shared);
            record_completed_task(&mut completed, shared);
            continue;
        };
        crate::gc::stress_collect("await");
        crate::observability::record(
            crate::observability::RuntimeEventKind::TaskPoll,
            Some(worker),
            id,
            0,
        );
        crate::preempt::willow_preempt_begin(preempt_flag);
        let result = unsafe { poll(frame) };
        crate::preempt::willow_preempt_end();
        let outcome = classify_poll_result(result);
        let fatal_chain = matches!(outcome, PollOutcome::Panicked | PollOutcome::Invalid(_))
            .then(async_chain_text);
        match outcome {
            PollOutcome::Ready => with_global(|sched| sched.complete(id)),
            PollOutcome::Yield | PollOutcome::Preempted => {
                // Runnable outcome (spec §7): gave up the worker but is not
                // waiting on an event. This hot boundary stays off the global
                // scheduler metadata lock.
                finish_global_poll_boundary(id, GlobalPollBoundary::Runnable);
            }
            PollOutcome::BlockedSyscall => {
                finish_global_poll_boundary(id, GlobalPollBoundary::BlockedSyscall);
            }
            PollOutcome::Pending => {
                finish_global_poll_boundary(id, GlobalPollBoundary::Pending);
            }
            PollOutcome::Panicked | PollOutcome::Invalid(_) => {
                // Close the claim gate BEFORE the terminal publication. Spec
                // §23 requires publishing PANICKED, detaching relationships and
                // releasing task roots before the abort, but that publication
                // wakes this task's awaiters — without the gate another worker
                // claims one and runs ordinary code past `await <panicked
                // task>` in the window before the abort lands (willow-s9ej.7).
                begin_fatal_panic();
                with_global(|sched| sched.finalize_panicked(id));
            }
        }
        let event = match outcome {
            PollOutcome::Ready => crate::observability::RuntimeEventKind::TaskComplete,
            PollOutcome::Yield => crate::observability::RuntimeEventKind::TaskYield,
            PollOutcome::Preempted => crate::observability::RuntimeEventKind::TaskPreempt,
            PollOutcome::Pending => crate::observability::RuntimeEventKind::TaskPark,
            PollOutcome::BlockedSyscall => crate::observability::RuntimeEventKind::BlockingDetach,
            PollOutcome::Panicked | PollOutcome::Invalid(_) => {
                crate::observability::RuntimeEventKind::TaskPanicked
            }
        };
        crate::observability::record(event, Some(worker), id, i64::from(result));
        // Done polling this task: drop the running marker so a later
        // out-of-poll willow_sched_sleep/await does not target a stale task.
        if outcome != PollOutcome::Panicked {
            set_current_task(None);
        }
        if matches!(
            outcome,
            PollOutcome::Ready | PollOutcome::Panicked | PollOutcome::Invalid(_)
        ) {
            // Only terminal outcomes can have appended cleanup work. Pending,
            // blocked, yield, and preempt boundaries therefore stay entirely
            // off the scheduler metadata mutex.
            drain_terminal_cleanups();
        }
        crate::gc::stress_collect("await");
        crate::gc::stress_collect("scheduler");
        // Keep this worker visible as active through the post-poll GC
        // boundaries. Otherwise worker 0 can leave the scoped pool and wait to
        // join this worker while its collection is waiting for worker 0 to
        // reach a safepoint.
        finish_active_poll(shared);
        if outcome == PollOutcome::Ready {
            record_completed_task(&mut completed, shared);
        }
        match outcome {
            PollOutcome::Panicked => crate::panic_context::finish_unhandled_with_async_chain(
                fatal_chain.as_deref().unwrap_or_default(),
            ),
            PollOutcome::Invalid(value) => {
                report_poll_failure(id, Some(value), fatal_chain.as_deref().unwrap_or_default())
            }
            _ => {}
        }
    }
    completed
}

/// Test-only: reset the global scheduler between unit tests (the heap and
/// scheduler are process-global, so tests must run single-threaded).
/// Test-only: run `f` against the global scheduler (for cross-crate-module
/// unit fixtures that must register real task ids, e.g. channel purge tests).
#[cfg(test)]
pub fn with_global_for_test<R>(f: impl FnOnce(&mut RuntimeScheduler) -> R) -> R {
    with_global(f)
}

/// Test-only: run `f` with `id` installed as this thread's current task, so
/// runtime primitives that attach wait state to the running task (channel
/// waiter registration/unregistration) can be exercised without a real poll.
#[cfg(test)]
pub fn with_current_task_for_test<R>(id: u64, f: impl FnOnce() -> R) -> R {
    let previous = current_task_id();
    set_current_task(Some(id));
    let result = f();
    set_current_task(previous);
    result
}

#[cfg(test)]
pub fn reset_global_scheduler_for_test() {
    let frames = with_global(|sched| {
        let mut frames = std::mem::take(&mut sched.pending_frame_unroots);
        frames.extend(sched.tasks.drain().into_iter().filter_map(|mut task| {
            if task.frame_rooted && !task.frame.is_null() {
                task.frame_rooted = false;
                Some(task.frame as usize)
            } else {
                None
            }
        }));
        let run_queues = Arc::new(RunQueues::new(runtime_worker_config().active_workers()));
        let tasks = Arc::new(ShardedTaskTable::new());
        let timers = Arc::new(TimerQueue::new());
        *GLOBAL_RUN_QUEUES
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::clone(&run_queues);
        *GLOBAL_TASK_TABLE
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::clone(&tasks);
        *GLOBAL_TIMERS
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::clone(&timers);
        *sched = RuntimeScheduler::with_components(run_queues, tasks, timers);
        frames
    });
    for frame in frames {
        crate::gc::willow_gc_remove_runtime_root(frame as *mut u8);
    }
    set_current_task(None);
    CURRENT_WORKER.with(|worker| worker.set(0));
    CURRENT_RUN_STATE.with(|state| {
        state.replace(None);
    });
    // In a real process the fatal gate is one-way (the closer aborts), but a
    // unit test can drive a panicking poll without the abort. Reopen it so the
    // next test is not parked forever.
    FATAL_PANIC_PENDING.store(false, Ordering::Release);
    // Claims are strictly scoped to a pop; a leftover count would make every
    // later test's idle detection report pending work.
    CLAIMS_IN_FLIGHT.store(0, Ordering::Release);
}

#[cfg(test)]
fn replace_global_scheduler_for_test(worker_count: usize) {
    with_global(|sched| {
        let run_queues = Arc::new(RunQueues::new(worker_count));
        let tasks = Arc::new(ShardedTaskTable::new());
        let timers = Arc::new(TimerQueue::new());
        *GLOBAL_RUN_QUEUES
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::clone(&run_queues);
        *GLOBAL_TASK_TABLE
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::clone(&tasks);
        *GLOBAL_TIMERS
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Arc::clone(&timers);
        *sched = RuntimeScheduler::with_components(run_queues, tasks, timers);
    });
}

/// Opt-in scaling and footprint measurements, kept out of the deterministic
/// gate (willow-ezs.2/.3). See the module's own documentation for how to run
/// them and how to read the table.
#[cfg(test)]
#[path = "scheduler_scaling_tests.rs"]
mod scaling_measurements;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::async_frame::{async_frame_slot_offset, willow_async_frame_alloc};
    use crate::gc::{
        reset_internal_for_test, runtime_test_guard, willow_alloc_typed, willow_gc_allocated_bytes,
        willow_gc_collect,
    };
    use crate::task::RUNTIME_POLL_PENDING;
    use std::collections::HashSet;
    use std::sync::atomic::{
        AtomicBool as TestAtomicBool, AtomicU64 as TestAtomicU64, AtomicUsize as TestAtomicUsize,
        Ordering as TestOrdering,
    };
    use std::sync::{Barrier, LazyLock as TestLazyLock, Mutex as TestMutex};

    static NESTED_QUANTUM_TARGET: TestAtomicU64 = TestAtomicU64::new(0);
    static NESTED_QUANTUM_RESTORED: TestAtomicBool = TestAtomicBool::new(false);

    #[test]
    fn idle_wait_observes_wake_between_snapshot_and_wait() {
        let generation = current_wake_generation();
        notify_idle_waiters();
        assert!(
            wait_for_wake_since(generation, Duration::ZERO),
            "a wake after the snapshot must prevent the fallback timeout"
        );
    }

    unsafe extern "C" fn poll_nested_then_check_quantum(_frame: *mut c_void) -> i32 {
        let target = NESTED_QUANTUM_TARGET.load(TestOrdering::SeqCst);
        willow_sched_run_until(target);
        for _ in 0..crate::preempt::willow_preempt_task_budget() {
            if crate::preempt::willow_preempt_check() != 0 {
                NESTED_QUANTUM_RESTORED.store(true, TestOrdering::SeqCst);
                break;
            }
        }
        RUNTIME_POLL_READY
    }

    #[test]
    fn nested_scheduler_restores_outer_task_quantum() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        NESTED_QUANTUM_RESTORED.store(false, TestOrdering::SeqCst);

        let target = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
        NESTED_QUANTUM_TARGET.store(target, TestOrdering::SeqCst);
        willow_sched_spawn(poll_nested_then_check_quantum, std::ptr::null_mut());

        assert_eq!(willow_sched_run(), 2);
        assert!(
            NESTED_QUANTUM_RESTORED.load(TestOrdering::SeqCst),
            "nested run_until must rebind the outer task's quantum"
        );
        reset_internal_for_test();
    }

    // ── Work-stealing run queues (willow-gyaa.4) ────────────────────────────

    #[test]
    fn workqueue_pops_local_before_global() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        s.enqueue_local(0, 10);
        s.enqueue_ready(20); // global
        assert_eq!(s.pop_for_worker(0), Some(10), "local queue drains first");
        assert_eq!(s.pop_for_worker(0), Some(20), "then the global queue");
        assert_eq!(s.pop_for_worker(0), None);
    }

    #[test]
    fn workqueue_repeated_local_work_cannot_starve_global_overflow() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        s.enqueue_local(0, 10);
        s.enqueue_local(0, 11);
        s.enqueue_local(0, 12);
        s.enqueue_ready(20);

        assert_eq!(s.pop_for_worker(0), Some(10), "first turn keeps locality");
        assert_eq!(
            s.pop_for_worker(0),
            Some(20),
            "next turn must service newly spawned or externally woken work"
        );
    }

    #[test]
    fn workqueue_idle_worker_steals_from_other_local() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        // Only worker 1 has local work; worker 0 (idle) must steal it.
        s.enqueue_local(1, 7);
        assert_eq!(
            s.pop_for_worker(0),
            Some(7),
            "idle worker steals sibling work"
        );
        assert_eq!(s.pop_for_worker(0), None);
    }

    #[test]
    fn workqueue_steal_takes_back_of_victim_queue() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        s.enqueue_local(1, 1);
        s.enqueue_local(1, 2); // back of victim
        // Steal takes the back (oldest-pushed / coldest) item first.
        assert_eq!(s.pop_for_worker(0), Some(2));
        // Worker 1 still pops its own from the front.
        assert_eq!(s.pop_for_worker(1), Some(1));
    }

    #[test]
    fn workqueue_ready_total_counts_all_queues() {
        let mut s = RuntimeScheduler::with_worker_count(3);
        s.enqueue_local(0, 1);
        s.enqueue_local(2, 2);
        s.enqueue_ready(3);
        assert_eq!(s.ready_total(), 3);
        assert_eq!(s.worker_count(), 3);
    }

    #[test]
    fn workqueue_empty_pop_returns_none() {
        let mut s = RuntimeScheduler::with_worker_count(3);
        assert_eq!(s.pop_for_worker(0), None);
        assert_eq!(s.pop_for_worker(2), None);
        assert_eq!(s.ready_total(), 0);
    }

    #[test]
    fn workqueue_enqueue_local_out_of_range_falls_to_global() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        s.enqueue_local(99, 5); // no such worker -> global
        // Any worker can pick it up from the global queue.
        assert_eq!(s.pop_for_worker(1), Some(5));
    }

    #[test]
    fn workqueue_steal_scans_workers_in_round_robin_order() {
        let mut s = RuntimeScheduler::with_worker_count(3);
        // Worker 0 is idle; both worker 1 and worker 2 have work. The steal scan
        // starts at the next worker (1) and takes from there first.
        s.enqueue_local(1, 11);
        s.enqueue_local(2, 22);
        assert_eq!(s.pop_for_worker(0), Some(11), "steal nearest victim first");
        assert_eq!(s.pop_for_worker(0), Some(22), "then the next victim");
        assert_eq!(s.pop_for_worker(0), None);
    }

    #[test]
    fn workqueue_pop_ready_uses_worker_zero() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_placeholder();
        // pop_ready() is the worker-0 view used by the cooperative run loop.
        assert_eq!(s.pop_ready(), Some(id));
    }

    #[test]
    fn workqueue_claim_discards_duplicate_entry_for_running_task() {
        let mut scheduler = RuntimeScheduler::with_worker_count(5);
        let id = scheduler.spawn_placeholder();
        // Forced at the queue level: `enqueue_ready` itself can no longer
        // create a duplicate (willow-ezs.1.1), but the claim-side guard must
        // still discard one if it ever appears.
        scheduler.run_queues.force_push_global(id);

        assert_eq!(scheduler.claim_ready_for_worker(0), Some(id));
        assert_eq!(scheduler.task_state(id), Some(RuntimeTaskState::Running));
        assert_eq!(
            scheduler.claim_ready_for_worker(1),
            None,
            "a stale duplicate must not let another worker poll a Running task"
        );
        scheduler.clear_running();
    }

    // ── O(1) run-queue membership invariant (willow-ezs.1.1) ────────────────
    //
    // `queued == true` means exactly one runnable claim owns the task id. The flag
    // replaces the `VecDeque::contains()` scans the enqueue/requeue path used
    // to run across the global queue and every local one, so the cost of
    // publishing work no longer grows with the number of queued tasks.
    //
    // Every transition that can publish or consume a queue entry is covered:
    //
    //  1. a spawn publishes exactly one entry and sets the flag
    //  2. physical popping keeps the flag until claim/state validation
    //  3. a repeated global enqueue cannot create a second entry
    //  4. a local enqueue of an already-queued task cannot either
    //  5. an out-of-range local enqueue still obeys the invariant
    //  6. a steal clears the flag just like a local pop
    //  7. claiming sets Running and leaves the task unqueued
    //  8. a non-Ready (Running) task is never published by an enqueue
    //  9. a parked task is published by exactly one wake
    // 10. a second wake of an already-queued task adds nothing
    // 11. a wake DURING a poll records `wake_requested` instead of queueing
    // 12. the post-poll transition publishes that deferred wake once
    // 13. a Pending poll with no wake parks the task and queues nothing
    // 14. a blocked-syscall poll parks in its own state, unqueued
    // 15. waking a blocked-syscall task publishes one entry
    // 16. `yield` requeues exactly once
    // 17. a preemption requeue is idempotent
    // 18. a due timer wake publishes exactly one entry
    // 19. finalizing a queued task as Cancelled leaves a stale entry that the
    //     claim discards, and the pop clears the flag
    // 20. a completed task's stale entry is discarded the same way
    // 21. `is_queued` agrees with a full scan of every queue
    // 22. a long mixed sequence of transitions keeps flags and queues in sync,
    //     with no duplicate entry anywhere
    // 23. cancelling a parked task on the global scheduler queues it once
    // 24. no task is polled concurrently twice: two workers claiming the same
    //     stale duplicate cannot both get it

    #[test]
    fn runq_01_spawn_publishes_one_entry() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_placeholder();
        assert!(s.is_queued(id));
        assert_eq!(s.ready_total(), 1);
        assert!(s.queue_invariant_holds());
    }

    #[test]
    fn runq_02_physical_pop_keeps_the_claim_token_until_atomic_claim() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_placeholder();
        assert_eq!(s.pop_for_worker(0), Some(id));
        assert!(
            s.is_queued(id),
            "the popped worker owns the outstanding claim until claim_for_poll"
        );
        assert_eq!(
            s.with_task_mut(id, |task| task.claim_for_poll()),
            Some(ClaimOutcome::Poll)
        );
        assert!(!s.is_queued(id));
    }

    #[test]
    fn runq_03_repeated_global_enqueue_is_idempotent() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_placeholder();
        s.enqueue_ready(id);
        s.enqueue_ready(id);
        assert_eq!(s.ready_total(), 1, "one queue entry per queued task");
        assert!(s.queue_invariant_holds());
    }

    #[test]
    fn runq_04_local_enqueue_of_queued_task_is_idempotent() {
        let mut s = RuntimeScheduler::with_worker_count(3);
        let id = s.spawn_placeholder(); // global
        s.enqueue_local(2, id);
        assert_eq!(s.ready_total(), 1);
        assert!(s.queue_invariant_holds());
    }

    #[test]
    fn runq_05_out_of_range_local_enqueue_obeys_invariant() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_parked_placeholder();
        s.wake(id); // -> Ready, global
        s.enqueue_local(99, id); // no such worker: would fall to global
        assert_eq!(s.ready_total(), 1);
        assert!(s.queue_invariant_holds());
    }

    #[test]
    fn runq_06_steal_clears_the_flag() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_parked_placeholder();
        s.wake(id);
        // Move it to worker 1's local queue, then have worker 0 steal it.
        assert_eq!(s.pop_for_worker(0), Some(id));
        assert_eq!(
            s.with_task_mut(id, |task| task.claim_for_poll()),
            Some(ClaimOutcome::Poll)
        );
        assert_eq!(
            s.with_task_mut(id, |task| task.state.requeue_after_poll()),
            Some(BoundaryOutcome::Requeue)
        );
        s.run_queues.push_local_front(1, id);
        assert!(s.is_queued(id));
        assert_eq!(s.pop_for_worker(0), Some(id), "worker 0 steals it");
        assert!(s.is_queued(id));
        assert_eq!(
            s.with_task_mut(id, |task| task.claim_for_poll()),
            Some(ClaimOutcome::Poll)
        );
        assert!(!s.is_queued(id));
    }

    #[test]
    fn runq_07_claim_leaves_running_task_unqueued() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_placeholder();
        assert_eq!(s.claim_ready_for_worker(0), Some(id));
        assert_eq!(s.task_state(id), Some(RuntimeTaskState::Running));
        assert!(!s.is_queued(id));
        assert!(s.queue_invariant_holds());
        s.clear_running();
    }

    #[test]
    fn runq_08_enqueue_never_publishes_a_running_task() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_placeholder();
        assert_eq!(s.claim_ready_for_worker(0), Some(id));
        s.enqueue_ready(id);
        s.enqueue_local(1, id);
        assert_eq!(
            s.ready_total(),
            0,
            "only a Ready task may own a queue entry"
        );
        assert!(s.queue_invariant_holds());
        s.clear_running();
    }

    #[test]
    fn runq_09_wake_publishes_parked_task_once() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_parked_placeholder();
        assert!(!s.is_queued(id));
        s.wake(id);
        assert!(s.is_queued(id));
        assert_eq!(s.ready_total(), 1);
        assert!(s.queue_invariant_holds());
    }

    #[test]
    fn runq_10_second_wake_adds_nothing() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_parked_placeholder();
        s.wake(id);
        s.wake(id);
        assert_eq!(s.ready_total(), 1);
        assert!(s.queue_invariant_holds());
    }

    #[test]
    fn runq_11_wake_during_poll_defers_instead_of_queueing() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_placeholder();
        assert_eq!(s.claim_ready_for_worker(0), Some(id));
        s.wake(id); // arrives while Running
        assert!(!s.is_queued(id), "a Running task must not be queued");
        assert!(s.task(id).unwrap().state.load().wake_requested());
        assert!(s.queue_invariant_holds());
        s.clear_running();
    }

    #[test]
    fn runq_12_post_poll_publishes_deferred_wake_once() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_placeholder();
        assert_eq!(s.claim_ready_for_worker(0), Some(id));
        s.wake(id);
        s.finish_pending_poll(id);
        assert_eq!(s.task_state(id), Some(RuntimeTaskState::Ready));
        assert_eq!(s.ready_total(), 1);
        assert!(s.queue_invariant_holds());
        s.clear_running();
    }

    #[test]
    fn runq_13_pending_poll_without_wake_parks_unqueued() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_placeholder();
        assert_eq!(s.claim_ready_for_worker(0), Some(id));
        s.finish_pending_poll(id);
        assert_eq!(s.task_state(id), Some(RuntimeTaskState::Parked));
        assert_eq!(s.ready_total(), 0);
        assert!(s.queue_invariant_holds());
        s.clear_running();
    }

    #[test]
    fn runq_14_blocked_syscall_poll_parks_unqueued() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_placeholder();
        assert_eq!(s.claim_ready_for_worker(0), Some(id));
        s.finish_blocked_syscall_poll(id);
        assert_eq!(s.task_state(id), Some(RuntimeTaskState::BlockedSyscall));
        assert!(!s.is_queued(id));
        assert!(s.queue_invariant_holds());
        s.clear_running();
    }

    #[test]
    fn runq_15_blocked_syscall_wake_publishes_once() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_placeholder();
        assert_eq!(s.claim_ready_for_worker(0), Some(id));
        s.finish_blocked_syscall_poll(id);
        s.wake(id);
        s.wake(id);
        assert_eq!(s.task_state(id), Some(RuntimeTaskState::Ready));
        assert_eq!(s.ready_total(), 1);
        assert!(s.queue_invariant_holds());
        s.clear_running();
    }

    #[test]
    fn runq_16_yield_requeues_exactly_once() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_placeholder();
        assert_eq!(s.claim_ready_for_worker(0), Some(id));
        set_current_task(Some(id));
        s.request_running_yield();
        s.requeue_runnable(id);
        set_current_task(None);
        assert_eq!(s.task_state(id), Some(RuntimeTaskState::Ready));
        assert_eq!(s.ready_total(), 1);
        assert!(s.queue_invariant_holds());
        s.clear_running();
    }

    #[test]
    fn runq_17_preemption_requeue_is_idempotent() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_placeholder();
        assert_eq!(s.claim_ready_for_worker(0), Some(id));
        s.requeue_runnable(id);
        s.requeue_runnable(id);
        assert_eq!(s.ready_total(), 1);
        assert!(s.queue_invariant_holds());
        s.clear_running();
    }

    #[test]
    fn runq_18_due_timer_wake_publishes_once() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_placeholder();
        assert_eq!(s.claim_ready_for_worker(0), Some(id));
        set_current_task(Some(id));
        s.set_running_wake_after_millis(0);
        set_current_task(None);
        s.finish_pending_poll(id);
        assert_eq!(
            s.wake_due_timers(Instant::now() + Duration::from_millis(5)),
            1
        );
        assert_eq!(s.ready_total(), 1);
        assert!(s.queue_invariant_holds());
        s.clear_running();
    }

    #[test]
    fn runq_19_cancelled_task_leaves_a_discardable_entry() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_placeholder();
        s.finalize_cancelled(id);
        assert_eq!(s.task_state(id), None, "terminal metadata is reaped");
        // The entry is still in the queue but no longer claimable; popping it
        // clears the flag, so nothing leaks into the next enqueue decision.
        assert_eq!(s.claim_ready_for_worker(0), None);
        assert!(!s.is_queued(id));
        assert!(s.queue_invariant_holds());
    }

    #[test]
    fn runq_20_completed_task_entry_is_discarded() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_placeholder();
        s.complete(id);
        assert_eq!(s.claim_ready_for_worker(0), None);
        assert!(!s.is_queued(id));
        assert!(s.queue_invariant_holds());
    }

    #[test]
    fn runq_21_is_queued_agrees_with_a_full_scan() {
        let mut s = RuntimeScheduler::with_worker_count(3);
        let a = s.spawn_placeholder();
        let b = s.spawn_parked_placeholder();
        let c = s.spawn_placeholder();
        assert_eq!(s.claim_ready_for_worker(0), Some(a));
        s.clear_running();
        for id in [a, b, c] {
            let scanned = s.run_queues.contains(id);
            assert_eq!(s.is_queued(id), scanned, "flag disagrees for task {id}");
        }
        assert!(s.queue_invariant_holds());
    }

    #[test]
    fn runq_22_mixed_transition_sequence_keeps_queues_consistent() {
        let mut s = RuntimeScheduler::with_worker_count(4);
        let ids: Vec<_> = (0..24).map(|_| s.spawn_placeholder()).collect();
        for (step, &id) in ids.iter().enumerate() {
            match step % 6 {
                0 => {
                    s.claim_ready_for_worker(step % 4);
                    s.finish_pending_poll(id);
                    s.wake(id);
                }
                1 => {
                    s.claim_ready_for_worker(step % 4);
                    s.requeue_runnable(id);
                }
                2 => {
                    s.claim_ready_for_worker(step % 4);
                    s.wake(id); // during poll
                    s.finish_blocked_syscall_poll(id);
                }
                3 => {
                    s.enqueue_local(step % 4, id);
                    s.enqueue_ready(id);
                }
                4 => {
                    s.claim_ready_for_worker(step % 4);
                    s.complete(id);
                }
                _ => {
                    s.claim_ready_for_worker(step % 4);
                    s.finalize_cancelled(id);
                }
            }
            assert!(
                s.queue_invariant_holds(),
                "invariant broken after step {step}"
            );
        }
        while let Some(id) = s.claim_ready_for_worker(0) {
            s.complete(id);
            assert!(s.queue_invariant_holds());
        }
        s.clear_running();
    }

    #[test]
    fn runq_23_cancel_publishes_parked_task_once() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let id = with_global_for_test(|sched| sched.spawn_parked_placeholder());
        willow_sched_cancel(id);
        willow_sched_cancel(id);
        with_global_for_test(|sched| {
            assert_eq!(sched.ready_len(), 1, "one entry for a cancelled wake-up");
            assert!(sched.queue_invariant_holds());
        });
        reset_global_scheduler_for_test();
        reset_internal_for_test();
    }

    #[test]
    fn runq_24_duplicate_entry_cannot_be_claimed_twice() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_placeholder();
        s.run_queues.force_push_global(id); // forced duplicate
        assert_eq!(s.claim_ready_for_worker(0), Some(id));
        assert_eq!(
            s.claim_ready_for_worker(1),
            None,
            "the second worker must not poll the same frame"
        );
        s.clear_running();
    }

    // ── O(1) blocked-syscall accounting (willow-ezs.1.2) ─────────────────────
    //
    // `has_blocked_syscall_tasks()` used to scan every task on every idle/stop
    // decision, so a 10k-task workload paid O(tasks) per park cycle. It now
    // reads a counter that `with_task_state` maintains, and these tests pin the
    // counter to the table it summarizes. Perspectives 16-28 of willow-ezs.1.2
    // (1-15 cover the channel waiter queue, in `channel.rs`):
    //
    // 16. a fresh scheduler reports no blocked-syscall task
    // 17. a Pending poll that detached native work marks exactly one
    // 18. a completion wake clears it
    // 19. cancelling a blocked task clears it (the Parked/Blocked → Ready arm)
    // 20. completing a blocked task clears it
    // 21. finalizing a blocked task as Cancelled clears it
    // 22. re-entering BlockedSyscall from BlockedSyscall does not double count
    // 23. N blocked tasks count N, and waking them all returns to 0
    // 24. `park()` on a blocked task decrements
    // 25. `requeue_runnable` on a blocked task decrements
    // 26. `set_running` on a blocked task decrements
    // 27. a long mixed transition sequence keeps counter == scan
    // 28. presence still suppresses the idle/stop decision (behavior preserved)
    // 29. block/wake accounting and queue publication need no global mutex

    /// Drive `id` to `BlockedSyscall` exactly as a worker does: claim it, poll
    /// it, and report that the poll detached native blocking work.
    fn block_on_syscall(s: &mut RuntimeScheduler, id: RuntimeTaskId) {
        s.set_running(id);
        s.finish_blocked_syscall_poll(id);
        s.clear_running();
    }

    #[test]
    fn bsq_16_fresh_scheduler_has_no_blocked_syscall_tasks() {
        let s = RuntimeScheduler::with_worker_count(2);
        assert!(!s.has_blocked_syscall_tasks());
        assert!(s.blocked_syscall_invariant_holds());
    }

    #[test]
    fn bsq_17_detached_poll_marks_exactly_one() {
        let mut s = RuntimeScheduler::with_worker_count(2);
        let id = s.spawn_placeholder();
        let other = s.spawn_placeholder();
        block_on_syscall(&mut s, id);
        assert!(s.has_blocked_syscall_tasks());
        assert_eq!(s.blocked_syscall_count(), 1);
        assert!(s.blocked_syscall_invariant_holds());
        assert_eq!(s.task_state(other), Some(RuntimeTaskState::Ready));
    }

    #[test]
    fn bsq_18_completion_wake_clears_the_count() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        block_on_syscall(&mut s, id);
        s.wake(id);
        assert_eq!(s.task_state(id), Some(RuntimeTaskState::Ready));
        assert!(!s.has_blocked_syscall_tasks());
        assert!(s.blocked_syscall_invariant_holds());
    }

    #[test]
    fn bsq_19_cancel_of_a_blocked_task_clears_the_count() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let id = with_global_for_test(|s| {
            let id = s.spawn_placeholder();
            block_on_syscall(s, id);
            assert!(s.has_blocked_syscall_tasks());
            id
        });
        willow_sched_cancel(id);
        with_global_for_test(|s| {
            assert_eq!(s.task_state(id), Some(RuntimeTaskState::Ready));
            assert!(!s.has_blocked_syscall_tasks());
            assert!(s.blocked_syscall_invariant_holds());
        });
    }

    #[test]
    fn bsq_20_completing_a_blocked_task_clears_the_count() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        block_on_syscall(&mut s, id);
        s.complete(id);
        assert_eq!(s.task_state(id), None);
        assert!(!s.has_blocked_syscall_tasks());
        assert!(s.blocked_syscall_invariant_holds());
    }

    #[test]
    fn bsq_21_finalizing_a_blocked_task_as_cancelled_clears_the_count() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        block_on_syscall(&mut s, id);
        s.finalize_cancelled(id);
        assert_eq!(s.task_state(id), None);
        assert!(!s.has_blocked_syscall_tasks());
        assert!(s.blocked_syscall_invariant_holds());
    }

    #[test]
    fn bsq_22_reentering_blocked_syscall_does_not_double_count() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        block_on_syscall(&mut s, id);
        // A second report without an intervening wake: the state is unchanged,
        // so the counter must not move.
        s.finish_blocked_syscall_poll(id);
        assert_eq!(s.blocked_syscall_count(), 1);
        assert!(s.blocked_syscall_invariant_holds());
    }

    #[test]
    fn bsq_23_many_blocked_tasks_count_and_drain_to_zero() {
        let mut s = RuntimeScheduler::with_worker_count(4);
        const TASKS: usize = 2_000;
        let ids: Vec<_> = (0..TASKS).map(|_| s.spawn_placeholder()).collect();
        for &id in &ids {
            block_on_syscall(&mut s, id);
        }
        assert_eq!(s.blocked_syscall_count(), TASKS);
        assert!(s.blocked_syscall_invariant_holds());
        for &id in &ids {
            s.wake(id);
        }
        assert_eq!(s.blocked_syscall_count(), 0);
        assert!(!s.has_blocked_syscall_tasks());
        assert!(s.blocked_syscall_invariant_holds());
        assert!(s.queue_invariant_holds());
    }

    #[test]
    fn bsq_24_parking_a_blocked_task_decrements() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        block_on_syscall(&mut s, id);
        s.wake(id);
        assert_eq!(s.claim_ready_for_worker(0), Some(id));
        s.park(id);
        assert_eq!(s.task_state(id), Some(RuntimeTaskState::Parked));
        assert_eq!(s.blocked_syscall_count(), 0);
        assert!(s.blocked_syscall_invariant_holds());
    }

    #[test]
    fn bsq_25_requeue_runnable_from_blocked_decrements() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        block_on_syscall(&mut s, id);
        s.wake(id);
        assert_eq!(s.claim_ready_for_worker(0), Some(id));
        s.requeue_runnable(id);
        assert_eq!(s.task_state(id), Some(RuntimeTaskState::Ready));
        assert_eq!(s.blocked_syscall_count(), 0);
        assert!(s.blocked_syscall_invariant_holds());
        assert!(s.queue_invariant_holds());
    }

    #[test]
    fn bsq_26_set_running_from_blocked_decrements() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        block_on_syscall(&mut s, id);
        s.set_running(id);
        assert_eq!(s.blocked_syscall_count(), 0);
        assert!(s.blocked_syscall_invariant_holds());
        s.clear_running();
    }

    #[test]
    fn bsq_27_mixed_transition_sequence_keeps_counter_exact() {
        let mut s = RuntimeScheduler::with_worker_count(3);
        let ids: Vec<_> = (0..64).map(|_| s.spawn_placeholder()).collect();
        for (index, &id) in ids.iter().enumerate() {
            match index % 8 {
                0 => block_on_syscall(&mut s, id),
                1 => {
                    block_on_syscall(&mut s, id);
                    s.wake(id);
                }
                2 => {
                    block_on_syscall(&mut s, id);
                    s.complete(id);
                }
                3 => {
                    block_on_syscall(&mut s, id);
                    s.finalize_cancelled(id);
                }
                4 => s.park(id),
                5 => {
                    s.set_running(id);
                    s.finish_pending_poll(id);
                    s.clear_running();
                }
                6 => {
                    block_on_syscall(&mut s, id);
                    s.wake(id);
                    block_on_syscall(&mut s, id);
                }
                _ => s.complete(id),
            }
            assert!(
                s.blocked_syscall_invariant_holds(),
                "counter drifted after transition {index}"
            );
            assert!(s.queue_invariant_holds(), "queues drifted at {index}");
        }
    }

    #[test]
    fn bsq_28_blocked_presence_suppresses_the_idle_decision() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        // Parked at spawn, so no queue entry exists to confuse `ready_total`.
        let id = s.spawn_parked_placeholder();
        block_on_syscall(&mut s, id);
        // No runnable work, but the blocking pool still owes a wake: the
        // scheduler must not treat empty queues as "done".
        assert_eq!(s.ready_total(), 0);
        assert!(s.has_blocked_syscall_tasks());
        s.wake(id);
        assert_eq!(s.ready_total(), 1);
        assert!(!s.has_blocked_syscall_tasks());
    }

    #[test]
    fn bsq_29_global_block_and_wake_do_not_need_scheduler_metadata_lock() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let id = with_global_for_test(RuntimeScheduler::spawn_placeholder);
        assert_eq!(
            claim_global_ready_for_worker(0, None).map(|(id, _)| id),
            Some(id)
        );

        // Hold the old global accounting lock across both operations. The
        // sharded implementation must still be able to enter BlockedSyscall,
        // publish a wake queue entry, and leave the blocked count.
        let scheduler_lock = GLOBAL_SCHEDULER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (blocked_tx, blocked_rx) = std::sync::mpsc::channel();
        let blocker = std::thread::spawn(move || {
            finish_global_poll_boundary(id, GlobalPollBoundary::BlockedSyscall);
            blocked_tx.send(()).unwrap();
        });
        blocked_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("BlockedSyscall publication must not take GLOBAL_SCHEDULER");
        blocker.join().unwrap();
        assert_eq!(global_task_table().blocked_syscall_count(), 1);
        assert_eq!(global_run_queues().len(), 0);

        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let waker = std::thread::spawn(move || {
            wake_tx.send(wake_global_task(id)).unwrap();
        });
        assert!(
            wake_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("blocked wake must not take GLOBAL_SCHEDULER")
        );
        waker.join().unwrap();
        assert_eq!(
            global_task_table().blocked_syscall_count(),
            0,
            "the count is removed only after the queue handoff is published"
        );
        assert_eq!(global_run_queues().len(), 1);

        drop(scheduler_lock);
        reset_global_scheduler_for_test();
    }

    #[test]
    fn sched_run_registers_driver_as_mutator_without_leaking() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        crate::gc::reset_internal_for_test();
        let before = crate::gc::registered_mutator_count();
        // Driving an empty scheduler registers the driver for the duration and
        // unregisters on the outermost exit (willow-6fv.5.6): no net leak.
        assert_eq!(willow_sched_run(), 0);
        assert_eq!(
            crate::gc::registered_mutator_count(),
            before,
            "willow_sched_run must not leak a mutator registration"
        );
    }

    static PARALLEL_POLL_THREADS: TestLazyLock<TestMutex<Vec<std::thread::ThreadId>>> =
        TestLazyLock::new(|| TestMutex::new(Vec::new()));
    static PARALLEL_POLL_ENTERED: TestAtomicUsize = TestAtomicUsize::new(0);

    unsafe extern "C" fn poll_record_parallel_worker(_frame: *mut c_void) -> i32 {
        PARALLEL_POLL_THREADS
            .lock()
            .expect("parallel poll thread log poisoned")
            .push(std::thread::current().id());
        PARALLEL_POLL_ENTERED.fetch_add(1, TestOrdering::SeqCst);
        let start = Instant::now();
        while PARALLEL_POLL_ENTERED.load(TestOrdering::SeqCst) < 2
            && start.elapsed() < Duration::from_millis(200)
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        RUNTIME_POLL_READY
    }

    #[test]
    fn parallel_worker_pool_polls_tasks_on_multiple_threads() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        replace_global_scheduler_for_test(2);
        PARALLEL_POLL_THREADS
            .lock()
            .expect("parallel poll thread log poisoned")
            .clear();
        PARALLEL_POLL_ENTERED.store(0, TestOrdering::SeqCst);

        let a = willow_sched_spawn(poll_record_parallel_worker, std::ptr::null_mut());
        let b = willow_sched_spawn(poll_record_parallel_worker, std::ptr::null_mut());

        crate::gc::willow_gc_register_mutator();
        let completed = willow_sched_run_parallel(None, 2, None);
        crate::gc::willow_gc_unregister_mutator();

        assert_eq!(completed, 2);
        assert_eq!(willow_sched_task_state(a), -1);
        assert_eq!(willow_sched_task_state(b), -1);
        let threads = PARALLEL_POLL_THREADS
            .lock()
            .expect("parallel poll thread log poisoned");
        let unique = threads.iter().copied().collect::<HashSet<_>>();
        assert!(
            unique.len() >= 2,
            "expected two worker threads to poll tasks, got {threads:?}"
        );
        reset_internal_for_test();
    }

    static WAKE_RACE_WAITER_REGISTERED: TestAtomicUsize = TestAtomicUsize::new(0);

    unsafe extern "C" fn poll_complete_after_waiter_registered(_frame: *mut c_void) -> i32 {
        let start = Instant::now();
        while WAKE_RACE_WAITER_REGISTERED.load(TestOrdering::SeqCst) == 0
            && start.elapsed() < Duration::from_millis(200)
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        RUNTIME_POLL_READY
    }

    unsafe extern "C" fn poll_await_with_running_wake_race(frame: *mut c_void) -> i32 {
        let base = frame as *mut u8;
        let b_id = unsafe { *(base.add(async_frame_slot_offset(0)) as *const u64) };
        let state = unsafe { &mut *(base.add(async_frame_slot_offset(1)) as *mut i64) };
        *state += 1;
        if *state == 1 {
            assert_eq!(willow_sched_await(b_id), 0);
            WAKE_RACE_WAITER_REGISTERED.store(1, TestOrdering::SeqCst);
            std::thread::sleep(Duration::from_millis(30));
            RUNTIME_POLL_PENDING
        } else {
            RUNTIME_POLL_READY
        }
    }

    #[test]
    fn parallel_wake_while_waiter_running_requeues_after_pending() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        replace_global_scheduler_for_test(2);
        WAKE_RACE_WAITER_REGISTERED.store(0, TestOrdering::SeqCst);

        let b = willow_sched_spawn(poll_complete_after_waiter_registered, std::ptr::null_mut());
        let a_frame = willow_async_frame_alloc(2, 0) as *mut c_void;
        unsafe {
            let base = a_frame as *mut u8;
            *(base.add(async_frame_slot_offset(0)) as *mut u64) = b;
        }
        let a = willow_sched_spawn(poll_await_with_running_wake_race, a_frame);

        crate::gc::willow_gc_register_mutator();
        let completed = willow_sched_run_parallel(None, 2, None);
        crate::gc::willow_gc_unregister_mutator();

        assert_eq!(
            completed, 2,
            "awaiter must be requeued when its dependency wakes it before park"
        );
        assert_eq!(willow_sched_task_state(a), -1);
        assert_eq!(willow_sched_task_state(b), -1);
        reset_internal_for_test();
    }

    #[test]
    fn workqueue_single_worker_preserves_fifo() {
        // With one worker, spawn order == pop order (no behavior change vs. the
        // old single VecDeque).
        let mut s = RuntimeScheduler::with_worker_count(1);
        let a = s.spawn_task(poll_ready_now, std::ptr::null_mut());
        let b = s.spawn_task(poll_ready_now, std::ptr::null_mut());
        assert_eq!(s.pop_for_worker(0), Some(a));
        assert_eq!(s.pop_for_worker(0), Some(b));
    }

    unsafe extern "C" fn cancel_noop(_frame: *mut c_void) {}

    #[test]
    fn initialized_spawn_runs_initializer_before_task_or_queue_publication() {
        let mut scheduler = RuntimeScheduler::with_worker_count(1);
        let tasks = Arc::clone(&scheduler.tasks);
        let run_queues = Arc::clone(&scheduler.run_queues);
        let mut published_id = 0;

        let id = scheduler.spawn_task_initialized(
            poll_ready_now,
            std::ptr::null_mut(),
            Some(cancel_noop),
            |id| {
                assert!(
                    tasks.with(id, |_| ()).is_none(),
                    "initializer must run before task-table publication"
                );
                assert!(
                    !run_queues.snapshot().contains(&id),
                    "initializer must run before run-queue publication"
                );
                published_id = id;
            },
        );

        assert_eq!(published_id, id);
        assert_eq!(run_queues.snapshot(), vec![id]);
        assert!(
            tasks
                .with(id, |task| task.cancel.map(|cancel| cancel as usize))
                .flatten()
                .is_some_and(|cancel| cancel == cancel_noop as *const () as usize)
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "scheduler spawn initializer must not access the global task table")]
    fn initialized_spawn_rejects_scheduler_reentry_in_debug_builds() {
        let mut scheduler = RuntimeScheduler::with_worker_count(1);
        scheduler.spawn_task_initialized(
            poll_ready_now,
            std::ptr::null_mut(),
            Some(cancel_noop),
            |_id| {
                let _ = global_task_table();
            },
        );
    }

    // ── Cooperative executable tasks (willow-fqg.1) ─────────────────────────

    #[test]
    fn async_chain_text_walks_awaiter_links() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        // main(id=1) awaits inner(id=2): register main as a waiter of inner, then
        // mark inner the running task. The chain is inner -> main (willow-9lw).
        let (inner, main) = with_global(|sched| {
            let inner = sched.spawn_task(poll_ready_now, std::ptr::null_mut());
            let main = sched.spawn_task(poll_ready_now, std::ptr::null_mut());
            sched.register_waiter(inner, main);
            sched.with_task_mut(inner, |task| task.set_name("inner".to_string()));
            sched.with_task_mut(main, |task| task.set_name("main".to_string()));
            sched.set_running(inner);
            (inner, main)
        });
        let text = async_chain_text();
        let i = text.find("inner").expect("chain names inner");
        let m = text.find("main").expect("chain names main");
        assert!(
            i < m,
            "current task (inner) must come before its awaiter (main): {text}"
        );
        let _ = (inner, main);
        with_global(|sched| sched.clear_running());
    }

    /// Completes on the first poll.
    unsafe extern "C" fn poll_ready_now(_frame: *mut c_void) -> i32 {
        RUNTIME_POLL_READY
    }

    /// Uses the frame's state word (offset 0) as a counter: Pending on the first
    /// poll, Ready on the second.
    unsafe extern "C" fn poll_ready_on_second(frame: *mut c_void) -> i32 {
        let state = unsafe { &mut *(frame as *mut i64) };
        *state += 1;
        if *state >= 2 {
            RUNTIME_POLL_READY
        } else {
            RUNTIME_POLL_PENDING
        }
    }

    #[test]
    fn coop_01_ready_task_runs_to_completion() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let id = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
        assert_eq!(willow_sched_run(), 1);
        assert_eq!(willow_sched_task_state(id), -1); // terminal record reaped
    }

    #[test]
    fn coop_02_pending_parks_then_wake_resumes() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        // A frame with just the [state, slot_count] header; poll uses the state word.
        let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
        let id = willow_sched_spawn(poll_ready_on_second, frame);
        // First poll: state 0->1 -> Pending -> parked.
        assert_eq!(willow_sched_run(), 0);
        assert_eq!(willow_sched_task_state(id), 2); // Parked
        // A parked task is not re-run while idle.
        assert_eq!(willow_sched_run(), 0);
        assert_eq!(willow_sched_task_state(id), 2);
        // Wake re-queues it; the second poll completes it.
        willow_sched_wake(id);
        assert_eq!(willow_sched_task_state(id), 0); // Ready
        assert_eq!(willow_sched_run(), 1);
        assert_eq!(willow_sched_task_state(id), -1); // terminal record reaped
        reset_internal_for_test();
    }

    /// First poll registers a 5ms sleep then returns Pending; second poll
    /// (after the timer fires) returns Ready.
    unsafe extern "C" fn poll_sleep_then_ready(frame: *mut c_void) -> i32 {
        let state = unsafe { &mut *(frame as *mut i64) };
        *state += 1;
        if *state >= 2 {
            RUNTIME_POLL_READY
        } else {
            willow_sched_sleep(5);
            RUNTIME_POLL_PENDING
        }
    }

    /// First poll requests a cooperative yield then returns Pending; second poll
    /// returns Ready.
    unsafe extern "C" fn poll_yield_then_ready(frame: *mut c_void) -> i32 {
        let state = unsafe { &mut *(frame as *mut i64) };
        *state += 1;
        if *state >= 2 {
            RUNTIME_POLL_READY
        } else {
            willow_sched_yield();
            RUNTIME_POLL_PENDING
        }
    }

    #[test]
    fn coop_timer_wake_resumes_parked_task() {
        // willow-lpn.5.3: a task that parks with a wake-deadline (sleep) is woken
        // by the timer-aware run loop and resumes to completion — no manual wake.
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
        let id = willow_sched_spawn(poll_sleep_then_ready, frame);
        let start = std::time::Instant::now();
        // Single run: first poll -> sleep+Pending -> parked with deadline; the
        // loop blocks ~5ms, wakes it, second poll -> Ready -> completed.
        let completed = willow_sched_run();
        assert_eq!(completed, 1, "timer should resume and complete the task");
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(4),
            "run loop should have waited for the wake-deadline"
        );
        assert_eq!(willow_sched_task_state(id), -1); // terminal record reaped
        reset_internal_for_test();
    }

    #[test]
    fn coop_yield_requeues_running_task_without_manual_wake() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
        let id = willow_sched_spawn(poll_yield_then_ready, frame);
        assert_eq!(willow_sched_run(), 1);
        assert_eq!(willow_sched_task_state(id), -1); // terminal record reaped
        reset_internal_for_test();
    }

    /// Awaits the task whose id is stored in frame slot 0; resumes once it
    /// completes (slot 1 is a poll counter).
    unsafe extern "C" fn poll_await_dependency(frame: *mut c_void) -> i32 {
        let base = frame as *mut u8;
        let b_id = unsafe { *(base.add(async_frame_slot_offset(0)) as *const u64) };
        let state = unsafe { &mut *(base.add(async_frame_slot_offset(1)) as *mut i64) };
        *state += 1;
        if *state == 1 {
            if willow_sched_await(b_id) == 1 {
                RUNTIME_POLL_READY
            } else {
                RUNTIME_POLL_PENDING // registered as a waiter of b_id
            }
        } else {
            RUNTIME_POLL_READY // resumed after the awaited task completed
        }
    }

    #[test]
    fn coop_dependency_wake_resumes_awaiter() {
        // willow-lpn.5.3: task A awaits task B. B sleeps then completes (timer
        // wake); B's completion wakes A (dependency wake); A resumes. No manual
        // wake — the scheduler drives both to completion in one run.
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        // B: sleeps 5ms on the first poll, ready on the second.
        let b_frame = willow_async_frame_alloc(0, 0) as *mut c_void;
        let b_id = willow_sched_spawn(poll_sleep_then_ready, b_frame);
        // A: awaits B. Store B's id in slot 0; slot 1 is A's poll counter.
        let a_frame = willow_async_frame_alloc(2, 0) as *mut c_void;
        unsafe {
            let base = a_frame as *mut u8;
            *(base.add(async_frame_slot_offset(0)) as *mut u64) = b_id;
        }
        let a_id = willow_sched_spawn(poll_await_dependency, a_frame);
        let completed = willow_sched_run();
        assert_eq!(
            completed, 2,
            "both the awaited task and the awaiter complete"
        );
        assert_eq!(willow_sched_task_state(a_id), -1); // A terminal record reaped
        assert_eq!(willow_sched_task_state(b_id), -1); // B terminal record reaped
        reset_internal_for_test();
    }

    #[test]
    fn coop_clear_running_prevents_stale_sleep() {
        // willow-lpn.5.3: after a poll returns, `running` is cleared, so a
        // willow_sched_sleep called OUTSIDE a poll does not attach a phantom
        // wake-deadline to the just-parked (now stale) task and spuriously wake
        // it on the next run.
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
        let id = willow_sched_spawn(poll_ready_on_second, frame);
        assert_eq!(willow_sched_run(), 0); // parks (no deadline); running cleared
        assert_eq!(willow_sched_task_state(id), 2); // Parked
        // Outside any poll (running == None): must be a no-op.
        willow_sched_sleep(5);
        assert_eq!(
            willow_sched_run(),
            0,
            "stale task must not be woken by an out-of-poll sleep"
        );
        assert_eq!(willow_sched_task_state(id), 2); // still Parked, not woken/completed
        reset_internal_for_test();
    }

    #[test]
    fn coop_parked_without_deadline_stays_idle() {
        // A task parked WITHOUT a deadline is not spuriously woken by the timer
        // loop (regression guard for the willow-lpn.5.3 run-loop change).
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
        let id = willow_sched_spawn(poll_ready_on_second, frame);
        assert_eq!(willow_sched_run(), 0); // parks, no deadline
        assert_eq!(willow_sched_task_state(id), 2); // Parked
        assert_eq!(willow_sched_run(), 0); // stays parked (loop breaks, no timer)
        assert_eq!(willow_sched_task_state(id), 2);
        reset_internal_for_test();
    }

    #[test]
    fn coop_03_suspended_frame_keeps_referenced_object_alive() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();

        // Frame with one GC-reference data slot (mask bit 0).
        let frame = willow_async_frame_alloc(1, 0b1) as *mut u8;
        // A heap object reachable ONLY through the frame's GC slot.
        let obj = willow_alloc_typed(16, 0);
        let slot0 = unsafe { frame.add(async_frame_slot_offset(0)).cast::<*mut u8>() };
        unsafe { slot0.write(obj) };

        let live = willow_gc_allocated_bytes(); // frame + obj
        // Unreferenced garbage that must be collected.
        let _garbage = willow_alloc_typed(16, 0);
        assert!(willow_gc_allocated_bytes() > live);

        // Spawning roots the frame; the first poll parks the task (Pending). The
        // poll counter uses the state word, leaving the data slot untouched.
        let id = willow_sched_spawn(poll_ready_on_second, frame as *mut c_void);
        assert_eq!(willow_sched_run(), 0);
        assert_eq!(willow_sched_task_state(id), 2); // Parked

        // Collection while suspended: the frame (a runtime root) and the object it
        // references survive; the unrooted garbage is freed.
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            live,
            "a suspended task's frame must keep its referenced object alive across GC"
        );

        // Resume to completion, which unroots the frame.
        willow_sched_wake(id);
        assert_eq!(willow_sched_run(), 1);
        assert_eq!(willow_sched_task_state(id), -1); // terminal record reaped

        // Nothing roots the frame/object now; both are collected.
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_internal_for_test();
    }

    #[test]
    fn coop_04_unknown_task_state_is_minus_one() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        assert_eq!(willow_sched_task_state(999), -1);
    }

    #[test]
    fn coop_05_multiple_ready_tasks_all_complete() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let a = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
        let b = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
        assert_eq!(willow_sched_run(), 2);
        assert_eq!(willow_sched_task_state(a), -1);
        assert_eq!(willow_sched_task_state(b), -1);
    }

    #[test]
    fn scheduler_assigns_ready_task_ids() {
        let mut scheduler = RuntimeScheduler::default();
        let first = scheduler.spawn_placeholder();
        let second = scheduler.spawn_placeholder();
        assert_eq!(scheduler.pop_ready(), Some(first));
        assert_eq!(scheduler.pop_ready(), Some(second));
    }

    #[test]
    fn scheduler_reports_task_and_ready_counts() {
        let mut scheduler = RuntimeScheduler::default();
        assert_eq!(scheduler.task_count(), 0);
        assert_eq!(scheduler.ready_len(), 0);
        scheduler.spawn_placeholder();
        scheduler.spawn_placeholder();
        assert_eq!(scheduler.task_count(), 2);
        assert_eq!(scheduler.ready_len(), 2);
    }

    #[test]
    fn scheduler_worker_config_defaults_to_five_active_workers() {
        let config = RuntimeWorkerConfig::from_env_value(None, DEFAULT_WORKERS);
        assert_eq!(config.requested_workers(), 5);
        assert_eq!(config.active_workers(), 5);
    }

    #[test]
    fn scheduler_worker_config_parses_env_override() {
        let config = RuntimeWorkerConfig::from_env_value(Some("8"), 4);
        assert_eq!(config.requested_workers(), 8);
        assert_eq!(config.active_workers(), 8);
    }

    #[test]
    fn scheduler_worker_config_clamps_small_overrides_to_five() {
        for value in ["1", "2", "4"] {
            let config = RuntimeWorkerConfig::from_env_value(Some(value), DEFAULT_WORKERS);
            assert_eq!(config.requested_workers(), 5);
            assert_eq!(config.active_workers(), 5);
        }
    }

    #[test]
    fn scheduler_worker_config_rejects_zero_and_invalid_override() {
        let zero = RuntimeWorkerConfig::from_env_value(Some("0"), DEFAULT_WORKERS);
        assert_eq!(zero.requested_workers(), 5);
        assert_eq!(zero.active_workers(), 5);

        let invalid = RuntimeWorkerConfig::from_env_value(Some("many"), DEFAULT_WORKERS);
        assert_eq!(invalid.requested_workers(), 5);
        assert_eq!(invalid.active_workers(), 5);
    }

    // -----------------------------------------------------------------------
    // `single_worker_for_test()` — deterministic single-threaded drives
    // (willow-tcrg). `bounded_unit_10_cancelled_handoff_wakes_the_next_producer`
    // failed intermittently because the worker pool claimed and completed a
    // placeholder that the drive itself had just woken, so `run_until` returned
    // a completion the test never asked for. Perspectives:
    //
    //  1. the guard reports one active worker
    //  2. the guard reports one requested worker
    //  3. dropping the guard restores the 5-worker default
    //  4. the default (no guard) really is a pool, so the guard is required
    //  5. nested guards keep the single-worker view until the outermost drops
    //  6. sequential guards re-arm cleanly
    //  7. the guard does not touch `from_env_value` parsing/clamping
    //  8. the ABI accessors agree with the guarded config
    //  9. `reset_global_scheduler_for_test` under the guard sizes run queues
    //     for exactly one worker
    // 10. a guarded drive polls on the calling thread (no pool threads)
    // 11. a guarded drive still completes an ordinary ready task and counts it
    // 12. `run_until(target)` stops at the target and does not reap a task the
    //     drive itself woke (the willow-tcrg failure mode)
    // 13. that woken bystander is left READY, not completed
    // 14. an untargeted `willow_sched_run()` still drains everything ready
    // 15. a guarded drive reaps a cancelled placeholder without counting it as
    //     a completion
    // 16. the cancelled task is gone from the table afterwards
    // 17. the guard is RAII, so a panicking test cannot leak the override
    // 18. the guard leaves task ids/state transitions otherwise unchanged
    // 19. the override is invisible outside `cfg(test)` (compile-time: the
    //     static, the guard, and the branch are all `#[cfg(test)]`)
    // 20. repeated guarded drives stay deterministic under stress
    // -----------------------------------------------------------------------

    unsafe extern "C" fn poll_ready_noop(_frame: *mut c_void) -> i32 {
        RUNTIME_POLL_READY
    }

    #[test]
    fn single_worker_guard_reports_one_active_and_requested_worker() {
        let _guard = runtime_test_guard();
        let _single = single_worker_for_test();
        let config = runtime_worker_config();
        assert_eq!(config.active_workers(), 1, "perspective 1");
        assert_eq!(config.requested_workers(), 1, "perspective 2");
        assert_eq!(willow_sched_active_workers(), 1, "perspective 8");
        assert_eq!(willow_sched_requested_workers(), 1, "perspective 8");
    }

    #[test]
    fn single_worker_guard_is_scoped_and_nestable() {
        let _guard = runtime_test_guard();
        // Perspective 4: without a guard the runtime picks the pool, which is
        // exactly why the override exists.
        assert!(
            runtime_worker_config().active_workers() > 1,
            "perspective 4"
        );

        {
            let outer = single_worker_for_test();
            {
                let _inner = single_worker_for_test();
                assert_eq!(runtime_worker_config().active_workers(), 1);
            }
            // Perspective 5: the inner drop must not re-enable the pool.
            assert_eq!(runtime_worker_config().active_workers(), 1, "perspective 5");
            drop(outer);
        }
        // Perspective 3: the outermost drop restores the default.
        assert_eq!(
            runtime_worker_config().active_workers(),
            DEFAULT_WORKERS,
            "perspective 3"
        );

        // Perspective 6: a fresh guard re-arms.
        let _again = single_worker_for_test();
        assert_eq!(runtime_worker_config().active_workers(), 1, "perspective 6");
    }

    #[test]
    fn single_worker_guard_does_not_change_env_parsing() {
        let _guard = runtime_test_guard();
        let _single = single_worker_for_test();
        // Perspective 7: only `runtime_worker_config()` consults the override;
        // the parser keeps clamping to DEFAULT_WORKERS.
        assert_eq!(
            RuntimeWorkerConfig::from_env_value(Some("1"), DEFAULT_WORKERS).active_workers(),
            DEFAULT_WORKERS,
            "perspective 7"
        );
        assert_eq!(
            RuntimeWorkerConfig::from_env_value(Some("8"), DEFAULT_WORKERS).active_workers(),
            8,
            "perspective 7"
        );
    }

    #[test]
    fn single_worker_guard_sizes_fresh_run_queues_for_one_worker() {
        let _guard = runtime_test_guard();
        let _single = single_worker_for_test();
        reset_global_scheduler_for_test();
        // Perspective 9: install the guard BEFORE the reset and the fresh
        // queues have one local deque, so nothing is stranded on a local queue
        // that the single driver never scans.
        assert_eq!(global_run_queues().locals.len(), 1, "perspective 9");
    }

    #[test]
    fn single_worker_guard_polls_on_the_calling_thread() {
        thread_local! {
            static IS_DRIVER_THREAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        }
        static POLLED_ON_DRIVER: AtomicBool = AtomicBool::new(false);
        static POLLED_AT_ALL: AtomicBool = AtomicBool::new(false);

        unsafe extern "C" fn poll_records_thread(_frame: *mut c_void) -> i32 {
            POLLED_AT_ALL.store(true, Ordering::Release);
            POLLED_ON_DRIVER.store(IS_DRIVER_THREAD.with(|f| f.get()), Ordering::Release);
            RUNTIME_POLL_READY
        }

        let _guard = runtime_test_guard();
        let _single = single_worker_for_test();
        reset_global_scheduler_for_test();
        IS_DRIVER_THREAD.with(|f| f.set(true));
        POLLED_ON_DRIVER.store(false, Ordering::Release);
        POLLED_AT_ALL.store(false, Ordering::Release);

        let id = willow_sched_spawn(poll_records_thread, std::ptr::null_mut());
        // Perspective 11: an ordinary ready task still completes and counts.
        assert_eq!(willow_sched_run_until(id), 1, "perspective 11");
        assert!(POLLED_AT_ALL.load(Ordering::Acquire));
        // Perspective 10: the poll ran inline, not on a pool worker thread.
        assert!(POLLED_ON_DRIVER.load(Ordering::Acquire), "perspective 10");
        IS_DRIVER_THREAD.with(|f| f.set(false));
    }

    #[test]
    fn single_worker_run_until_does_not_reap_a_task_the_drive_woke() {
        static WOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

        unsafe extern "C" fn poll_wakes_bystander(_frame: *mut c_void) -> i32 {
            willow_sched_wake(WOKEN.load(Ordering::Acquire));
            RUNTIME_POLL_READY
        }

        let _guard = runtime_test_guard();
        let _single = single_worker_for_test();
        reset_global_scheduler_for_test();

        let bystander = with_global_for_test(|sched| sched.spawn_parked_placeholder());
        WOKEN.store(bystander, Ordering::Release);
        let target = willow_sched_spawn(poll_wakes_bystander, std::ptr::null_mut());

        // Perspective 12: the drive stops at its target. With the pool a second
        // worker could claim `bystander` the moment the target's poll woke it
        // and complete it too, making this count 2 (willow-tcrg).
        assert_eq!(
            willow_sched_run_until(target),
            1,
            "perspective 12: run_until must reap only its target"
        );
        // Perspective 13 / 18: the bystander is left runnable, untouched.
        with_global_for_test(|sched| {
            assert_eq!(
                sched.task_state(bystander),
                Some(RuntimeTaskState::Ready),
                "perspective 13"
            );
            assert_eq!(sched.task_state(target), None, "perspective 18");
        });

        // Perspective 14: an untargeted drive still drains what is left.
        assert_eq!(willow_sched_run(), 1, "perspective 14");
    }

    #[test]
    fn single_worker_drive_reaps_a_cancelled_placeholder_without_counting_it() {
        let _guard = runtime_test_guard();
        let _single = single_worker_for_test();
        reset_global_scheduler_for_test();

        let id = with_global_for_test(|sched| sched.spawn_parked_placeholder());
        willow_sched_cancel(id);
        // Perspective 15: cancellation is terminal but is not a completion.
        assert_eq!(willow_sched_run_until(id), 0, "perspective 15");
        // Perspective 16: and the task is reaped out of the table.
        with_global_for_test(|sched| {
            assert_eq!(sched.task_state(id), None, "perspective 16");
        });
    }

    #[test]
    fn single_worker_guard_is_released_when_a_scope_unwinds() {
        let _guard = runtime_test_guard();
        // Perspective 17: the override is RAII, so a panicking test body cannot
        // leave every later test pinned to one worker.
        let panicked = std::panic::catch_unwind(|| {
            let _single = single_worker_for_test();
            assert_eq!(runtime_worker_config().active_workers(), 1);
            panic!("unwind with the guard live");
        });
        assert!(panicked.is_err());
        assert_eq!(
            runtime_worker_config().active_workers(),
            DEFAULT_WORKERS,
            "perspective 17"
        );
    }

    #[test]
    fn single_worker_drives_are_repeatable() {
        let _guard = runtime_test_guard();
        let _single = single_worker_for_test();
        // Perspective 20: the determinism is not a one-shot fluke.
        for _ in 0..32 {
            reset_global_scheduler_for_test();
            let id = willow_sched_spawn(poll_ready_noop, std::ptr::null_mut());
            assert_eq!(willow_sched_run_until(id), 1);
            assert_eq!(willow_sched_task_state(id), -1);
        }
    }

    #[test]
    fn scheduler_active_worker_abi_reports_requested_workers() {
        let active = willow_sched_active_workers();
        let requested = willow_sched_requested_workers();
        assert!(active >= 1);
        assert!(requested >= 1);
        assert_eq!(active, requested);
    }

    #[test]
    fn scheduler_park_removes_task_from_running_state_only() {
        let mut scheduler = RuntimeScheduler::default();
        let id = scheduler.spawn_placeholder();
        scheduler.park(id);
        assert_eq!(scheduler.task_state(id), Some(RuntimeTaskState::Parked));
    }

    #[test]
    fn scheduler_wake_requeues_parked_task() {
        let mut scheduler = RuntimeScheduler::default();
        let id = scheduler.spawn_placeholder();
        assert_eq!(scheduler.pop_ready(), Some(id));
        scheduler.park(id);
        scheduler.wake(id);
        assert_eq!(scheduler.task_state(id), Some(RuntimeTaskState::Ready));
        assert_eq!(scheduler.pop_ready(), Some(id));
    }

    #[test]
    fn scheduler_wake_ready_task_does_not_duplicate_ready_queue() {
        let mut scheduler = RuntimeScheduler::default();
        let id = scheduler.spawn_placeholder();
        scheduler.wake(id);
        assert_eq!(scheduler.ready_len(), 1);
        assert_eq!(scheduler.pop_ready(), Some(id));
        assert_eq!(scheduler.pop_ready(), None);
    }

    #[test]
    fn scheduler_spawn_parked_placeholder_does_not_enter_ready_queue() {
        let mut scheduler = RuntimeScheduler::default();
        let id = scheduler.spawn_parked_placeholder();
        assert_eq!(scheduler.ready_len(), 0);
        assert_eq!(scheduler.task_state(id), Some(RuntimeTaskState::Parked));
    }

    fn park_with_sleep(
        scheduler: &mut RuntimeScheduler,
        id: RuntimeTaskId,
        millis: i64,
    ) -> Instant {
        assert_eq!(scheduler.pop_ready(), Some(id));
        scheduler.set_running(id);
        scheduler.set_running_wake_after_millis(millis);
        scheduler.clear_running();
        scheduler.park(id);
        scheduler.task(id).unwrap().wake_deadline.unwrap()
    }

    #[test]
    fn scheduler_timer_heap_selects_earliest_deadline() {
        let mut scheduler = RuntimeScheduler::default();
        let slow = scheduler.spawn_placeholder();
        let fast = scheduler.spawn_placeholder();

        park_with_sleep(&mut scheduler, slow, 50);
        let fast_deadline = park_with_sleep(&mut scheduler, fast, 0);

        assert_eq!(scheduler.timers.len(), 2);
        assert_eq!(scheduler.next_timer_deadline(), Some((fast, fast_deadline)));
    }

    #[test]
    fn scheduler_timer_heap_prunes_stale_woken_task() {
        let mut scheduler = RuntimeScheduler::default();
        let id = scheduler.spawn_placeholder();
        park_with_sleep(&mut scheduler, id, 50);
        assert_eq!(scheduler.timers.len(), 1);

        scheduler.wake(id);

        assert_eq!(scheduler.next_timer_deadline(), None);
        assert_eq!(scheduler.timers.len(), 0);
    }

    #[test]
    fn scheduler_timer_heap_pops_due_timer_once() {
        let mut scheduler = RuntimeScheduler::default();
        let id = scheduler.spawn_placeholder();
        park_with_sleep(&mut scheduler, id, 0);

        assert_eq!(scheduler.wake_due_timers(Instant::now()), 1);
        assert_eq!(scheduler.task_state(id), Some(RuntimeTaskState::Ready));
        // The entry is consumed: a second sweep finds nothing to promote.
        assert_eq!(scheduler.wake_due_timers(Instant::now()), 0);
        assert_eq!(scheduler.timers.len(), 0);
    }

    #[test]
    fn scheduler_due_timer_transition_publishes_ready_task_atomically() {
        let mut scheduler = RuntimeScheduler::default();
        let id = scheduler.spawn_placeholder();
        park_with_sleep(&mut scheduler, id, 0);

        assert_eq!(scheduler.wake_due_timers(Instant::now()), 1);
        assert_eq!(scheduler.next_timer_deadline(), None);
        assert_eq!(scheduler.task_state(id), Some(RuntimeTaskState::Ready));
        assert_eq!(scheduler.pop_ready(), Some(id));
    }

    // -----------------------------------------------------------------------
    // willow-9ha4: timer accounting lives behind its OWN lock, not the global
    // scheduler metadata mutex. The 26 perspectives below:
    //
    //   Timer-queue semantics (deterministic, single-threaded)
    //     01 registering a sleep publishes an entry and arms the lock-free hint
    //     02 sleep ORDERING across three deadlines is unchanged
    //     03 `sleep(0)` is immediately due
    //     04 a negative duration clamps to zero instead of wrapping
    //     05 a sleep requested outside a poll registers nothing
    //     06 an early wake prunes the stale entry and restores the empty hint
    //     07 a re-armed sleep supersedes the earlier entry
    //     08 an empty queue answers from the sentinel hint, taking no lock
    //     09 a reaped task's entry is pruned, never woken
    //     09b a terminal record still in the table reads as stale, not a panic
    //     10 promotion publishes Ready + the queue entry together
    //     11 the entry count returns to zero — no per-sleep leak
    //     12 timers are no longer part of the scheduler metadata snapshot
    //
    //   Lock decomposition (the point of the issue)
    //     13 registration completes while GLOBAL_SCHEDULER is held elsewhere
    //     14 promotion completes while GLOBAL_SCHEDULER is held elsewhere
    //     15 the earliest-deadline query completes while it is held elsewhere
    //     16 the empty-queue sweep answers from one atomic load while it is held
    //     17 the per-test reset installs a fresh, empty timer queue
    //     18 concurrent registration + drain fires every timer exactly once
    //     19 no observable window with neither a timer nor a ready entry
    //
    //   Idle notification (acceptance criterion — must not regress)
    //     20 a blocking-pool completion wake releases the idle waiter, no spin
    //     21 a wake between the snapshot and the wait is not lost
    //
    //   End-to-end through the ABI
    //     22 a sleeping task completes under `willow_sched_run`, leaving no entry
    //     23 `willow_sched_run_until` returns when the sleeping target completes
    //     24 cancelling a sleeper clears its deadline and the drive terminates
    //     25 a drive deadline still wins over a far-off sleeper
    //     26 staggered sleepers complete in DEADLINE order, not spawn order
    // -----------------------------------------------------------------------

    #[test]
    fn t9ha4_01_registering_a_sleep_publishes_an_entry_and_arms_the_hint() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        let before = Instant::now();
        let deadline = park_with_sleep(&mut s, id, 5);

        assert_eq!(s.timers.len(), 1);
        assert_eq!(s.next_timer_deadline(), Some((id, deadline)));
        assert_ne!(
            s.timers.earliest_hint_nanos(),
            TimerQueue::empty_hint(),
            "the lock-free hint must arm so the run loop stops short-circuiting"
        );
        assert!(
            !s.timers.maybe_due(before),
            "a future deadline must not read as due"
        );
        assert!(s.timers.maybe_due(deadline));
    }

    #[test]
    fn t9ha4_02_sleep_ordering_across_three_deadlines_is_unchanged() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let first = s.spawn_placeholder();
        let second = s.spawn_placeholder();
        let third = s.spawn_placeholder();

        // Registered in ascending order, but the heap — not the registration
        // order — is what decides who fires first.
        let d0 = park_with_sleep(&mut s, first, 0);
        let d1 = park_with_sleep(&mut s, second, 30);
        let d2 = park_with_sleep(&mut s, third, 60);
        assert!(d0 < d1 && d1 < d2);

        assert_eq!(s.next_timer_deadline(), Some((first, d0)));
        assert_eq!(s.wake_due_timers(d0), 1);
        assert_eq!(s.next_timer_deadline(), Some((second, d1)));
        assert_eq!(s.wake_due_timers(d1), 1);
        assert_eq!(s.next_timer_deadline(), Some((third, d2)));
        assert_eq!(s.wake_due_timers(d2), 1);
        assert_eq!(s.next_timer_deadline(), None);
        assert_eq!(s.timers.len(), 0);
    }

    #[test]
    fn t9ha4_03_zero_millisecond_sleep_is_immediately_due() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        let deadline = park_with_sleep(&mut s, id, 0);

        assert!(deadline <= Instant::now());
        assert!(s.timers.maybe_due(Instant::now()));
        assert_eq!(s.wake_due_timers(Instant::now()), 1);
        assert_eq!(s.task_state(id), Some(RuntimeTaskState::Ready));
    }

    #[test]
    fn t9ha4_04_negative_sleep_clamps_to_zero_instead_of_wrapping() {
        // `Duration::from_millis` takes a u64: a raw cast of -5 would be a
        // ~584-million-year deadline that never fires.
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        let deadline = park_with_sleep(&mut s, id, -5);

        assert!(
            deadline <= Instant::now(),
            "negative sleep must clamp to now"
        );
        assert_eq!(s.wake_due_timers(Instant::now()), 1);
        assert_eq!(s.task_state(id), Some(RuntimeTaskState::Ready));
    }

    #[test]
    fn t9ha4_05_sleep_outside_a_poll_registers_nothing() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        s.clear_running();

        s.set_running_wake_after_millis(5);

        assert_eq!(
            s.timers.len(),
            0,
            "no running task means there is nobody to wake"
        );
        assert_eq!(s.timers.earliest_hint_nanos(), TimerQueue::empty_hint());
        assert_eq!(s.task(id).unwrap().wake_deadline, None);
    }

    #[test]
    fn t9ha4_06_early_wake_prunes_the_stale_entry_and_restores_the_empty_hint() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        park_with_sleep(&mut s, id, 50);
        assert_eq!(s.timers.len(), 1);

        // A channel/join wake beats the deadline. The heap entry is now stale.
        assert!(s.wake(id));

        assert_eq!(s.next_timer_deadline(), None);
        assert_eq!(s.timers.len(), 0);
        assert_eq!(
            s.timers.earliest_hint_nanos(),
            TimerQueue::empty_hint(),
            "pruning must republish the hint or idle workers keep waking for nothing"
        );
    }

    #[test]
    fn t9ha4_07_re_armed_sleep_supersedes_the_earlier_entry() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        let stale = park_with_sleep(&mut s, id, 50);

        // Woken early, then it sleeps again with a NEARER deadline. Both
        // entries are in the heap; only the second one is current.
        assert!(s.wake(id));
        let current = park_with_sleep(&mut s, id, 5);
        assert!(current < stale);

        assert_eq!(s.next_timer_deadline(), Some((id, current)));
        assert_eq!(
            s.wake_due_timers(stale),
            1,
            "the task must fire exactly once"
        );
        assert_eq!(
            s.timers.len(),
            0,
            "the superseded entry is pruned, not kept"
        );
    }

    #[test]
    fn t9ha4_08_empty_queue_answers_from_the_sentinel_hint() {
        // The run loop calls this on every worker on every iteration, so the
        // no-timer case must cost one atomic load and take no lock at all.
        let s = RuntimeScheduler::with_worker_count(1);
        assert_eq!(s.timers.len(), 0);
        assert_eq!(s.timers.earliest_hint_nanos(), TimerQueue::empty_hint());
        assert!(!s.timers.maybe_due(Instant::now()));
        assert_eq!(s.next_timer_deadline(), None);
        assert_eq!(s.wake_due_timers(Instant::now()), 0);
    }

    #[test]
    fn t9ha4_09_reaped_task_entry_is_pruned_never_woken() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        park_with_sleep(&mut s, id, 50);
        s.complete(id);
        assert_eq!(s.task_state(id), None, "terminal records are reaped");

        assert_eq!(s.next_timer_deadline(), None);
        assert_eq!(s.timers.len(), 0);
        assert_eq!(
            s.wake_due_timers(Instant::now() + Duration::from_secs(1)),
            0,
            "a timer must never resurrect a reaped task"
        );
    }

    #[test]
    fn t9ha4_09b_terminal_but_unreaped_entry_is_stale_not_a_panic() {
        // 09 covers the record being gone. `finish_terminal` gets there in two
        // steps: it publishes Terminal under the task's shard, releases it, then
        // re-takes the shard to remove the record. A promoting thread that looks
        // between the two sees a Terminal record that is STILL in the table and
        // must read it as a stale entry — it used to hit an `unreachable!` in
        // `runtime_state` and abort the process (willow-0a6k.7).
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        let deadline = park_with_sleep(&mut s, id, 0);
        // The first half of `finish_terminal`, without the `remove` that follows
        // it: the record stays in the table, carrying its leftover deadline.
        s.prepare_placeholder_terminal_owner(id);
        assert!(
            s.tasks
                .with(id, |task| task.state.finish_terminal())
                .unwrap(),
            "the record must still be in the table, now terminal"
        );
        assert_eq!(
            s.tasks.with(id, |task| task.wake_deadline),
            Some(Some(deadline)),
            "the timer entry the promoter will find must still be there"
        );

        assert_eq!(s.next_timer_deadline(), None);
        assert_eq!(
            s.wake_due_timers(Instant::now() + Duration::from_secs(1)),
            0,
            "a terminal record must not be woken by its own leftover timer"
        );
    }

    #[test]
    fn t9ha4_10_promotion_publishes_ready_and_the_queue_entry_together() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        park_with_sleep(&mut s, id, 0);
        assert_eq!(s.ready_total(), 0);

        assert_eq!(s.wake_due_timers(Instant::now()), 1);

        assert_eq!(s.task_state(id), Some(RuntimeTaskState::Ready));
        assert!(
            s.is_queued(id),
            "a Ready task that is not queued is invisible to every worker"
        );
        assert_eq!(s.ready_total(), 1);
        assert_eq!(s.task(id).unwrap().wake_deadline, None);
    }

    #[test]
    fn t9ha4_11_entry_count_returns_to_zero_after_every_sleeper_fires() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let ids = (0..64).map(|_| s.spawn_placeholder()).collect::<Vec<_>>();
        for &id in &ids {
            park_with_sleep(&mut s, id, 0);
        }
        assert_eq!(s.timers.len(), 64);

        assert_eq!(s.wake_due_timers(Instant::now()), 64);

        assert_eq!(
            s.timers.len(),
            0,
            "the heap must not retain drained entries"
        );
        assert_eq!(s.timers.earliest_hint_nanos(), TimerQueue::empty_hint());
        assert_eq!(s.ready_total(), 64);
    }

    #[test]
    fn t9ha4_12_timers_are_not_part_of_the_scheduler_metadata_snapshot() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        park_with_sleep(&mut s, id, 50);

        // The snapshot counts what the metadata mutex owns. A pending timer is
        // no longer one of those things (willow-9ha4).
        assert_eq!(
            s.metadata_snapshot(),
            SchedulerMetadataSnapshot {
                heavy_tasks: 1,
                queue_entries: 0,
                pending_cleanups: 0,
                frame_roots: 0,
                blocked_syscalls: 0,
            }
        );
        assert_eq!(s.timers.len(), 1);
    }

    /// Run `body` on another thread while THIS thread holds the global
    /// scheduler metadata mutex, and fail if it does not finish promptly.
    ///
    /// Every willow-9ha4 timer operation must complete here; before the split
    /// they all serialized on the lock this helper is squatting on.
    fn without_scheduler_lock<T: Send>(what: &str, body: impl FnOnce() -> T + Send) -> T {
        let scheduler_lock = GLOBAL_SCHEDULER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (tx, rx) = std::sync::mpsc::channel();
        let result = std::thread::scope(|scope| {
            scope.spawn(|| {
                tx.send(body()).unwrap();
            });
            rx.recv_timeout(Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("{what} must not need GLOBAL_SCHEDULER"))
        });
        drop(scheduler_lock);
        result
    }

    #[test]
    fn t9ha4_13_sleep_registration_does_not_need_the_scheduler_lock() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let id = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);

        without_scheduler_lock("timer registration", move || {
            with_current_task_for_test(id, || set_global_wake_after_millis(20));
        });

        assert_eq!(global_timers().len(), 1);
        assert_ne!(
            global_timers().earliest_hint_nanos(),
            TimerQueue::empty_hint()
        );
        reset_global_scheduler_for_test();
    }

    #[test]
    fn t9ha4_14_timer_promotion_does_not_need_the_scheduler_lock() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let id = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);
        with_current_task_for_test(id, || set_global_wake_after_millis(0));

        let woken =
            without_scheduler_lock("timer promotion", || wake_global_due_timers(Instant::now()));

        assert_eq!(woken, 1);
        assert_eq!(willow_sched_task_state(id), 0, "the task must be Ready");
        assert_eq!(global_run_queues().len(), 1);
        reset_global_scheduler_for_test();
    }

    #[test]
    fn t9ha4_15_earliest_deadline_query_does_not_need_the_scheduler_lock() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let id = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);
        with_current_task_for_test(id, || set_global_wake_after_millis(50));

        // The idle path asks this question before deciding how long to wait; it
        // must not block behind whatever is holding the metadata mutex.
        let earliest = without_scheduler_lock("the earliest-deadline query", || {
            global_next_timer_deadline()
        });

        assert_eq!(earliest.map(|(id, _)| id), Some(id));
        reset_global_scheduler_for_test();
    }

    #[test]
    fn t9ha4_16_empty_sweep_is_an_atomic_load_even_while_the_scheduler_lock_is_held() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        assert_eq!(
            global_timers().earliest_hint_nanos(),
            TimerQueue::empty_hint()
        );

        // This is the per-iteration, per-worker call in `scheduler_run_loop`.
        let swept = without_scheduler_lock("the empty timer sweep", || {
            wake_global_due_timers(Instant::now())
        });

        assert_eq!(swept, 0);
        reset_global_scheduler_for_test();
    }

    #[test]
    fn t9ha4_17_reset_installs_a_fresh_empty_timer_queue() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let id = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);
        with_current_task_for_test(id, || set_global_wake_after_millis(5_000));
        let leaked = global_timers();
        assert_eq!(leaked.len(), 1);

        reset_global_scheduler_for_test();

        assert_eq!(
            global_timers().len(),
            0,
            "a stale timer must not leak into the next test"
        );
        assert_eq!(
            global_timers().earliest_hint_nanos(),
            TimerQueue::empty_hint()
        );
        assert!(
            !Arc::ptr_eq(&leaked, &global_timers()),
            "reset must install a NEW queue, not drain the old one in place"
        );
    }

    #[test]
    fn t9ha4_18_concurrent_registration_and_drain_fire_every_timer_once() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        const SLEEPERS: usize = 256;
        const REGISTRARS: usize = 4;
        const DRAINERS: usize = 4;

        let ids = (0..SLEEPERS)
            .map(|_| with_global_for_test(RuntimeScheduler::spawn_parked_placeholder))
            .collect::<Vec<_>>();
        let woken = AtomicUsize::new(0);
        let start = Barrier::new(REGISTRARS + DRAINERS);

        // Registration takes the heap lock then a task shard; promotion takes
        // the heap lock and calls into a shard from inside it. Running both at
        // once is what would deadlock if either side inverted the order.
        std::thread::scope(|scope| {
            for chunk in ids.chunks(SLEEPERS / REGISTRARS) {
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    for &id in chunk {
                        with_current_task_for_test(id, || set_global_wake_after_millis(0));
                    }
                });
            }
            for _ in 0..DRAINERS {
                let (woken, start) = (&woken, &start);
                scope.spawn(move || {
                    start.wait();
                    let give_up = Instant::now() + Duration::from_secs(10);
                    while woken.load(Ordering::Acquire) < SLEEPERS && Instant::now() < give_up {
                        let fired = wake_global_due_timers(Instant::now());
                        woken.fetch_add(fired, Ordering::AcqRel);
                        std::hint::spin_loop();
                    }
                });
            }
        });

        assert_eq!(
            woken.load(Ordering::Acquire),
            SLEEPERS,
            "each timer must be promoted exactly once, by exactly one drainer"
        );
        assert_eq!(global_timers().len(), 0);
        assert_eq!(global_run_queues().len(), SLEEPERS);
        reset_global_scheduler_for_test();
    }

    #[test]
    fn t9ha4_19_promotion_never_exposes_a_window_with_neither_timer_nor_ready_entry() {
        // The invariant that forces the wake to happen UNDER the heap lock: if
        // the entry were popped and the lock released before the task was made
        // Ready, another worker could see no timer and no runnable work and
        // wrongly conclude that the scheduler is idle.
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let id = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);
        with_current_task_for_test(id, || set_global_wake_after_millis(0));

        let violated = TestAtomicBool::new(false);
        let done = TestAtomicBool::new(false);
        std::thread::scope(|scope| {
            let (violated, done) = (&violated, &done);
            scope.spawn(move || {
                while !done.load(TestOrdering::Acquire) {
                    if global_next_timer_deadline().is_none()
                        && global_run_queues().len() == 0
                        && willow_sched_task_state(id) == 2
                    {
                        violated.store(true, TestOrdering::Release);
                    }
                }
            });
            assert_eq!(wake_global_due_timers(Instant::now()), 1);
            done.store(true, TestOrdering::Release);
        });

        assert!(
            !violated.load(TestOrdering::Acquire),
            "a parked sleeper was observed with neither a live timer nor a queue entry"
        );
        reset_global_scheduler_for_test();
    }

    #[test]
    fn t9ha4_20_blocking_completion_wake_releases_the_idle_waiter_without_spinning() {
        // Acceptance criterion: moving timers off the metadata mutex must not
        // disturb the BlockedSyscall keep-alive arm, which parks on the
        // generation counter instead of polling at 1ms (willow-5if8).
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let id = with_global_for_test(RuntimeScheduler::spawn_placeholder);
        assert_eq!(
            claim_global_ready_for_worker(0, None).map(|(id, _)| id),
            Some(id)
        );
        finish_global_poll_boundary(id, GlobalPollBoundary::BlockedSyscall);
        assert_eq!(global_task_table().blocked_syscall_count(), 1);
        // No timer exists, so the idle path takes the keep-alive arm.
        assert_eq!(global_timers().len(), 0);

        let generation = current_wake_generation();
        let start = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                assert!(try_wake_parked_task(id));
            });
            assert!(
                wait_for_wake_since(generation, Duration::from_secs(5)),
                "the completion wake must release the waiter before the fallback timeout"
            );
        });
        let waited = start.elapsed();

        assert!(
            waited >= Duration::from_millis(15),
            "the waiter returned before the completion could arrive: {waited:?}"
        );
        assert!(
            waited < Duration::from_secs(2),
            "the waiter sat through the fallback instead of being signalled: {waited:?}"
        );
        assert_eq!(global_task_table().blocked_syscall_count(), 0);
        assert_eq!(global_run_queues().len(), 1);
        reset_global_scheduler_for_test();
    }

    #[test]
    fn t9ha4_21_wake_between_snapshot_and_wait_is_not_lost() {
        // The idle worker snapshots the generation, then checks scheduler
        // state, then waits. A completion landing in that gap must be observed
        // immediately — otherwise the worker sleeps out the 50ms fallback with
        // runnable work already queued.
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let id = with_global_for_test(RuntimeScheduler::spawn_placeholder);
        assert_eq!(
            claim_global_ready_for_worker(0, None).map(|(id, _)| id),
            Some(id)
        );
        finish_global_poll_boundary(id, GlobalPollBoundary::BlockedSyscall);

        let generation = current_wake_generation();
        assert!(try_wake_parked_task(id));
        let start = Instant::now();
        assert!(wait_for_wake_since(generation, Duration::from_secs(5)));

        assert!(
            start.elapsed() < Duration::from_secs(1),
            "a wake that already happened must return from the wait at once"
        );
        reset_global_scheduler_for_test();
    }

    #[test]
    fn t9ha4_22_sleeping_task_completes_under_run_and_leaves_no_entry() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
        let id = willow_sched_spawn(poll_sleep_then_ready, frame);

        assert_eq!(willow_sched_run(), 1);

        assert_eq!(willow_sched_task_state(id), -1);
        assert_eq!(
            global_timers().len(),
            0,
            "a completed sleeper must not leave a heap entry behind"
        );
        assert_eq!(
            global_timers().earliest_hint_nanos(),
            TimerQueue::empty_hint()
        );
        reset_global_scheduler_for_test();
        reset_internal_for_test();
    }

    #[test]
    fn t9ha4_23_run_until_returns_when_the_sleeping_target_completes() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
        let target = willow_sched_spawn(poll_sleep_then_ready, frame);

        let start = Instant::now();
        assert_eq!(willow_sched_run_until(target), 1);

        assert!(
            start.elapsed() >= Duration::from_millis(4),
            "the drive must actually have waited out the 5ms deadline"
        );
        assert_eq!(willow_sched_task_state(target), -1);
        assert_eq!(global_timers().len(), 0);
        reset_global_scheduler_for_test();
        reset_internal_for_test();
    }

    #[test]
    fn t9ha4_24_cancelling_a_sleeper_clears_its_deadline_and_the_drive_terminates() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
        let id = willow_sched_spawn(poll_long_sleep_then_ready, frame);
        // A short bounded drive gives it its first poll — which parks it on a
        // 2s deadline — and returns without waiting that sleep out.
        assert_eq!(
            willow_sched_run_until_deadline(willow_monotonic_millis() + 30),
            0
        );
        assert_eq!(willow_sched_task_state(id), 2);
        assert_eq!(global_timers().len(), 1);

        willow_sched_cancel(id);

        assert_eq!(
            global_task_table()
                .with(id, |task| task.wake_deadline)
                .flatten(),
            None,
            "cancellation must clear the deadline so the entry is stale"
        );
        let start = Instant::now();
        assert_eq!(willow_sched_run(), 0, "a cancelled task never completes");
        assert!(
            start.elapsed() < Duration::from_millis(1_500),
            "the drive waited out the cancelled 2s sleep: {:?}",
            start.elapsed()
        );
        assert_eq!(willow_sched_task_state(id), -1);
        assert_eq!(global_timers().len(), 0);
        reset_global_scheduler_for_test();
        reset_internal_for_test();
    }

    #[test]
    fn t9ha4_25_drive_deadline_still_wins_over_a_far_off_sleeper() {
        // The idle wait is computed from the timer queue; clamping it to the
        // caller's deadline must survive the move to the separate lock.
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
        let id = willow_sched_spawn(poll_long_sleep_then_ready, frame);

        let start = Instant::now();
        assert_eq!(
            willow_sched_run_until_deadline(willow_monotonic_millis() + 30),
            0
        );
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_millis(1_500),
            "the 2s timer outranked the 30ms drive deadline: {elapsed:?}"
        );
        assert_eq!(willow_sched_task_state(id), 2, "the sleeper stays parked");
        assert_eq!(global_timers().len(), 1, "its timer stays registered");
        reset_global_scheduler_for_test();
        reset_internal_for_test();
    }

    static T9HA4_COMPLETION_ORDER: TestMutex<Vec<i64>> = TestMutex::new(Vec::new());

    /// Slot 0: poll count. Slot 1: tag. Slot 2: sleep duration in ms.
    /// Records its tag on completion so a test can assert the order.
    unsafe extern "C" fn poll_tagged_sleep_then_ready(frame: *mut c_void) -> i32 {
        let base = frame as *mut u8;
        let polls = unsafe { &mut *(base.add(async_frame_slot_offset(0)) as *mut i64) };
        let tag = unsafe { *(base.add(async_frame_slot_offset(1)) as *const i64) };
        let millis = unsafe { *(base.add(async_frame_slot_offset(2)) as *const i64) };
        *polls += 1;
        if *polls >= 2 {
            T9HA4_COMPLETION_ORDER
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(tag);
            RUNTIME_POLL_READY
        } else {
            willow_sched_sleep(millis);
            RUNTIME_POLL_PENDING
        }
    }

    #[test]
    fn t9ha4_26_staggered_sleepers_complete_in_deadline_order() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        T9HA4_COMPLETION_ORDER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();

        // Spawn order deliberately disagrees with deadline order.
        for (tag, millis) in [(1i64, 120i64), (2, 0), (3, 60)] {
            let frame = willow_async_frame_alloc(3, 0) as *mut c_void;
            unsafe {
                let base = frame as *mut u8;
                *(base.add(async_frame_slot_offset(1)) as *mut i64) = tag;
                *(base.add(async_frame_slot_offset(2)) as *mut i64) = millis;
            }
            willow_sched_spawn(poll_tagged_sleep_then_ready, frame);
        }

        assert_eq!(willow_sched_run(), 3);

        assert_eq!(
            *T9HA4_COMPLETION_ORDER
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            vec![2, 3, 1],
            "sleepers must resume in deadline order, not spawn order"
        );
        assert_eq!(global_timers().len(), 0);
        reset_global_scheduler_for_test();
        reset_internal_for_test();
    }

    // ── willow-vynv.1: cancel runtime integrity ─────────────────────────────

    #[test]
    fn cancel_finalize_wakes_parked_awaiter() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let target = s.spawn_placeholder();
        let awaiter = s.spawn_parked_placeholder();
        s.register_waiter(target, awaiter);

        // Cancel-request the target, then let the claim boundary finalize it.
        assert_eq!(
            s.with_task_mut(target, |task| task.state.request_cancel()),
            Some(CancelOutcome::Deferred)
        );
        // The claim boundary finalizes the target (never claims it), wakes
        // the parked awaiter, and the SAME claim then picks the awaiter up.
        assert_eq!(
            s.claim_ready_for_worker(0),
            Some(awaiter),
            "finalize must wake the awaiter, which is then claimable"
        );
        assert_eq!(s.task_state(target), None, "cancelled task is reaped");
        assert_eq!(s.task_state(awaiter), Some(RuntimeTaskState::Running));
        s.clear_running();
    }

    #[test]
    fn cancel_cleared_deadline_invalidates_stale_timer_entry() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        s.park(id);
        let deadline = Instant::now();
        s.tasks.with_mut(id, |task| {
            task.wake_deadline = Some(deadline);
        });
        s.timers.push(id, deadline);
        // Cancellation clears the deadline (willow_sched_cancel behavior).
        s.tasks.with_mut(id, |task| {
            assert_eq!(task.state.request_cancel(), CancelOutcome::Enqueue);
            task.wake_deadline = None;
        });
        // The wheel's stale entry must be revalidated away, not fire a wake.
        assert_eq!(
            s.wake_due_timers(Instant::now() + std::time::Duration::from_secs(1)),
            0,
            "stale timer entry for a cancelled task must not fire"
        );
        assert_eq!(s.timers.len(), 0, "the stale entry is dropped, not kept");
    }

    #[test]
    fn wake_is_a_noop_on_cancelled_tasks() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        assert_eq!(s.claim_ready_for_worker(0), Some(id));
        s.finalize_cancelled(id);
        s.wake(id);
        assert_eq!(
            s.task_state(id),
            None,
            "wake must not resurrect a reaped task"
        );
        assert_eq!(s.claim_ready_for_worker(0), None);
    }

    // -----------------------------------------------------------------------
    // Deadline-bounded scheduler drive + task-waiter reverse references
    // (willow-o038 review). Perspectives: an absolute deadline becomes a real
    // timer; the drive returns at that deadline instead of draining a far-off
    // task; an already-past deadline returns immediately; the placeholder never
    // outlives the call; registration records both directions; unregister,
    // completion, and cancellation all clear both directions.
    // -----------------------------------------------------------------------

    /// Parks with a long sleep on the first poll, completes on the second.
    unsafe extern "C" fn poll_long_sleep_then_ready(frame: *mut c_void) -> i32 {
        let state = unsafe { &mut *(frame as *mut i64) };
        *state += 1;
        if *state >= 2 {
            RUNTIME_POLL_READY
        } else {
            willow_sched_sleep(2_000);
            RUNTIME_POLL_PENDING
        }
    }

    #[test]
    fn deadline_01_empty_scheduler_returns_immediately() {
        // The deadline is a CEILING, not a sleep: with nothing to run the drive
        // must return at once so the caller can decide how to wait.
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let start = Instant::now();
        assert_eq!(
            willow_sched_run_until_deadline(willow_monotonic_millis() + 5_000),
            0
        );
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "an idle scheduler must not sleep out the deadline"
        );
        reset_global_scheduler_for_test();
        reset_internal_for_test();
    }

    #[test]
    fn deadline_02_ready_work_still_runs_before_the_deadline() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let id = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
        assert_eq!(
            willow_sched_run_until_deadline(willow_monotonic_millis() + 5_000),
            1,
            "a runnable task must still be driven"
        );
        assert_eq!(willow_sched_task_state(id), -1); // terminal record reaped
        reset_global_scheduler_for_test();
        reset_internal_for_test();
    }

    #[test]
    fn deadline_03_run_returns_at_deadline_not_after_long_task() {
        // The bug this guards: an unbounded `willow_sched_run` inside sync
        // select drains a 2s task before ever re-checking a 30ms timeout.
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
        let long = willow_sched_spawn(poll_long_sleep_then_ready, frame);
        let start = Instant::now();
        willow_sched_run_until_deadline(willow_monotonic_millis() + 30);
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(20),
            "drive returned before the deadline: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(1_500),
            "drive waited for the 2s task instead of the 30ms deadline: {elapsed:?}"
        );
        assert_eq!(
            willow_sched_task_state(long),
            2,
            "the long task must remain parked, not be reaped as completed"
        );
        reset_global_scheduler_for_test();
        reset_internal_for_test();
    }

    #[test]
    fn deadline_04_past_deadline_returns_promptly() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
        willow_sched_spawn(poll_long_sleep_then_ready, frame);
        let start = Instant::now();
        willow_sched_run_until_deadline(willow_monotonic_millis() - 1);
        assert!(
            start.elapsed() < Duration::from_millis(1_500),
            "an already-expired deadline must not block on unrelated tasks"
        );
        reset_global_scheduler_for_test();
        reset_internal_for_test();
    }

    #[test]
    fn deadline_05_bounded_drive_leaves_no_scheduler_state_behind() {
        // The bound is caller-local: it must not register timers or tasks that
        // a later unbounded drive would then wait on.
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        willow_sched_run_until_deadline(willow_monotonic_millis() + 10);
        assert!(
            with_global(|sched| sched.next_timer_deadline()).is_none(),
            "a bounded drive must leave no timer behind"
        );
        let start = Instant::now();
        assert_eq!(willow_sched_run(), 0);
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "an empty scheduler must go idle immediately after a bounded drive"
        );
        reset_global_scheduler_for_test();
        reset_internal_for_test();
    }

    #[test]
    fn deadline_06_parked_task_timer_still_fires_before_a_far_deadline() {
        // Clamping idle waits must not SKIP a nearer task timer: a 5ms sleeper
        // still runs to completion under a 5s drive deadline.
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
        let id = willow_sched_spawn(poll_sleep_then_ready, frame);
        assert_eq!(
            willow_sched_run_until_deadline(willow_monotonic_millis() + 5_000),
            1
        );
        assert_eq!(willow_sched_task_state(id), -1); // terminal record reaped
        reset_global_scheduler_for_test();
        reset_internal_for_test();
    }

    #[test]
    fn waiter_reverse_01_register_records_both_directions() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let awaitee = s.spawn_parked_placeholder();
        let waiter = s.spawn_parked_placeholder();
        s.register_waiter(awaitee, waiter);
        assert_eq!(s.task(awaitee).unwrap().live_waiters(), vec![waiter]);
        assert!(s.task(waiter).unwrap().is_awaiting(awaitee));
    }

    #[test]
    fn sharded_relationships_take_opposite_requests_in_one_lock_order() {
        let tasks = Arc::new(ShardedTaskTable::new());
        // 1 and 2 are in different shards. Each thread presents the pair in
        // the opposite logical order; `with_two_mut` must still acquire shard
        // 1 before shard 2, so the barrier-pinned race cannot deadlock.
        tasks.insert(1, RuntimeTask::new(1));
        tasks.insert(2, RuntimeTask::new(2));
        let start = Arc::new(Barrier::new(3));
        std::thread::scope(|scope| {
            for (awaitee, waiter) in [(1, 2), (2, 1)] {
                let tasks = Arc::clone(&tasks);
                let start = Arc::clone(&start);
                scope.spawn(move || {
                    start.wait();
                    for _ in 0..2_000 {
                        assert!(register_waiter_sharded(&tasks, awaitee, waiter));
                        unregister_waiter_sharded(&tasks, awaitee, waiter);
                    }
                });
            }
            start.wait();
        });
        for id in [1, 2] {
            let task = tasks.with(id, Clone::clone).unwrap();
            assert_eq!(task.waiter_count(), 0);
            assert_eq!(task.awaiting_count(), 0);
        }
    }

    #[test]
    fn waiter_reverse_02_register_is_idempotent() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let awaitee = s.spawn_parked_placeholder();
        let waiter = s.spawn_parked_placeholder();
        s.register_waiter(awaitee, waiter);
        s.register_waiter(awaitee, waiter);
        assert_eq!(s.task(awaitee).unwrap().waiter_count(), 1);
        assert_eq!(s.task(waiter).unwrap().awaiting_count(), 1);
    }

    #[test]
    fn waiter_reverse_03_unregister_clears_both_directions() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let awaitee = s.spawn_parked_placeholder();
        let waiter = s.spawn_parked_placeholder();
        s.register_waiter(awaitee, waiter);
        s.unregister_waiter(awaitee, waiter);
        assert!(s.task(awaitee).unwrap().live_waiters().is_empty());
        assert!(s.task(waiter).unwrap().awaiting_count() == 0);
    }

    #[test]
    fn waiter_reverse_04_cancel_purges_the_waiter_registration() {
        // A cancelled select task waiter must not stay in its awaitee's list:
        // the awaitee would otherwise try to wake a dead task on completion.
        let mut s = RuntimeScheduler::with_worker_count(1);
        let awaitee = s.spawn_parked_placeholder();
        let waiter = s.spawn_parked_placeholder();
        s.register_waiter(awaitee, waiter);
        s.finalize_cancelled(waiter);
        assert!(
            s.task(awaitee).unwrap().live_waiters().is_empty(),
            "cancellation must deregister the task-completion waiter"
        );
        assert!(s.task(waiter).is_none(), "cancelled waiter must be reaped");
    }

    #[test]
    fn waiter_reverse_05_completion_clears_the_reverse_reference() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let awaitee = s.spawn_parked_placeholder();
        let waiter = s.spawn_parked_placeholder();
        s.register_waiter(awaitee, waiter);
        s.complete(awaitee);
        assert!(
            s.task(waiter).unwrap().awaiting_count() == 0,
            "a completed awaitee leaves no reverse reference behind"
        );
        assert_eq!(s.task_state(waiter), Some(RuntimeTaskState::Ready));
    }

    #[test]
    fn waiter_reverse_06_multiple_awaitees_all_purged_on_cancel() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let a = s.spawn_parked_placeholder();
        let b = s.spawn_parked_placeholder();
        let waiter = s.spawn_parked_placeholder();
        s.register_waiter(a, waiter);
        s.register_waiter(b, waiter);
        assert_eq!(s.task(waiter).unwrap().awaiting_count(), 2);
        s.finalize_cancelled(waiter);
        assert!(s.task(a).unwrap().live_waiters().is_empty());
        assert!(s.task(b).unwrap().live_waiters().is_empty());
    }

    #[test]
    fn waiter_reverse_07_unknown_awaitee_records_nothing() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let waiter = s.spawn_parked_placeholder();
        s.register_waiter(9_999, waiter);
        assert!(
            s.task(waiter).unwrap().awaiting_count() == 0,
            "no reverse reference for a registration that did not happen"
        );
    }

    // -------------------------------------------------------------------------
    // O(1) completion-waiter relationships for 10k fan-in (willow-ezs.2).
    //
    // `waiters` is a FIFO `WaitQueue` with a membership map and `awaiting` is a
    // set, so neither registration nor cancellation rescans a vector. These
    // tests pin the observable contract of that change; the queue's own
    // structural behavior is covered in `wait_queue`, and scaling cost is
    // measured by the scheduler benchmark rather than asserted here.
    //
    // Perspectives:
    //   FI1  10,000 distinct waiters on one awaitee all register, in order
    //   FI2  re-registering all 10,000 creates no second relation
    //   FI3  a duplicate registration does not add a reverse reference either
    //   FI4  unregister then re-register moves the waiter to the FIFO tail
    //   FI5  completion wakes every live waiter exactly once
    //   FI6  completion clears both directions for all 10,000
    //   FI7  cancelling half of 10,000 waiters leaves exactly 5,000 to wake
    //   FI8  cancelled waiters are gone, not merely skipped
    //   FI9  repeated losing select arms cannot grow the waiter queue
    //   FI10 repeated losing select arms cannot grow the reverse references
    //   FI11 one waiter on 1,000 awaitees is purged from all of them on cancel
    //   FI12 waiters are woken in registration order
    //   FI13 a waiter reaped before the wake is skipped without disturbing others
    //   FI14 unregistering a waiter that was never registered is a no-op
    //   FI15 10k fan-in returns the scheduler to its metadata baseline
    // -------------------------------------------------------------------------

    const FAN_IN: usize = 10_000;

    /// FI1, FI2, FI3.
    #[test]
    fn fanin_01_ten_thousand_waiters_register_once_each() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let awaitee = s.spawn_parked_placeholder();
        let waiters: Vec<RuntimeTaskId> =
            (0..FAN_IN).map(|_| s.spawn_parked_placeholder()).collect();
        for &waiter in &waiters {
            s.register_waiter(awaitee, waiter);
        }
        assert_eq!(s.task(awaitee).unwrap().live_waiters(), waiters);

        for &waiter in &waiters {
            s.register_waiter(awaitee, waiter);
        }
        assert_eq!(s.task(awaitee).unwrap().waiter_count(), FAN_IN);
        assert_eq!(s.task(awaitee).unwrap().queued_waiter_entries(), FAN_IN);
        for &waiter in &waiters {
            assert_eq!(
                s.task(waiter).unwrap().awaiting_count(),
                1,
                "a duplicate registration must not add a second reverse reference"
            );
        }
    }

    /// FI4.
    #[test]
    fn fanin_02_reregistration_moves_the_waiter_to_the_tail() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let awaitee = s.spawn_parked_placeholder();
        let first = s.spawn_parked_placeholder();
        let second = s.spawn_parked_placeholder();
        let third = s.spawn_parked_placeholder();
        for waiter in [first, second, third] {
            s.register_waiter(awaitee, waiter);
        }
        s.unregister_waiter(awaitee, first);
        s.register_waiter(awaitee, first);
        assert_eq!(
            s.task(awaitee).unwrap().live_waiters(),
            vec![second, third, first]
        );
        assert!(s.task(first).unwrap().is_awaiting(awaitee));
    }

    /// FI5, FI6.
    #[test]
    fn fanin_03_completion_wakes_every_waiter_exactly_once() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let awaitee = s.spawn_parked_placeholder();
        let waiters: Vec<RuntimeTaskId> =
            (0..FAN_IN).map(|_| s.spawn_parked_placeholder()).collect();
        for &waiter in &waiters {
            s.register_waiter(awaitee, waiter);
        }

        s.complete(awaitee);

        let mut claimed = Vec::with_capacity(FAN_IN);
        while let Some(id) = s.claim_ready_for_worker(0) {
            claimed.push(id);
            s.clear_running();
        }
        assert_eq!(
            claimed.len(),
            FAN_IN,
            "every waiter must be woken exactly once"
        );
        for &waiter in &waiters {
            assert!(
                s.task(waiter).unwrap().awaiting_count() == 0,
                "the reverse reference must be cleared for every waiter"
            );
        }
        assert!(s.task(awaitee).is_none(), "the awaitee is reaped");
    }

    /// FI7, FI8.
    #[test]
    fn fanin_04_cancelling_half_leaves_exactly_half_to_wake() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let awaitee = s.spawn_parked_placeholder();
        let waiters: Vec<RuntimeTaskId> =
            (0..FAN_IN).map(|_| s.spawn_parked_placeholder()).collect();
        for &waiter in &waiters {
            s.register_waiter(awaitee, waiter);
        }
        for (index, &waiter) in waiters.iter().enumerate() {
            if index % 2 == 0 {
                s.finalize_cancelled(waiter);
            }
        }
        assert_eq!(s.task(awaitee).unwrap().waiter_count(), FAN_IN / 2);

        s.complete(awaitee);
        let mut woken = 0;
        while let Some(_id) = s.claim_ready_for_worker(0) {
            woken += 1;
            s.clear_running();
        }
        assert_eq!(woken, FAN_IN / 2);
        for (index, &waiter) in waiters.iter().enumerate() {
            if index % 2 == 0 {
                assert!(s.task(waiter).is_none(), "cancelled waiters are reaped");
            }
        }
    }

    /// FI9, FI10.
    #[test]
    fn fanin_05_losing_select_arms_do_not_grow_the_relationship_tables() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let awaitee = s.spawn_parked_placeholder();
        let resident = s.spawn_parked_placeholder();
        let churner = s.spawn_parked_placeholder();
        s.register_waiter(awaitee, resident);

        // A select arm that loses unregisters and parks again next iteration.
        for _ in 0..FAN_IN {
            s.register_waiter(awaitee, churner);
            s.unregister_waiter(awaitee, churner);
        }

        let awaitee_task = s.task(awaitee).unwrap();
        assert_eq!(awaitee_task.waiter_count(), 1);
        assert_eq!(awaitee_task.live_waiters(), vec![resident]);
        assert!(
            awaitee_task.queued_waiter_entries() <= 16,
            "tombstones from losing arms must be compacted, saw {}",
            awaitee_task.queued_waiter_entries()
        );
        assert!(
            s.task(churner).unwrap().awaiting_count() == 0,
            "a losing arm must leave no reverse reference"
        );
    }

    /// FI11.
    #[test]
    fn fanin_06_one_waiter_on_many_awaitees_is_purged_from_all() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let waiter = s.spawn_parked_placeholder();
        let awaitees: Vec<RuntimeTaskId> =
            (0..1_000).map(|_| s.spawn_parked_placeholder()).collect();
        for &awaitee in &awaitees {
            s.register_waiter(awaitee, waiter);
        }
        assert_eq!(s.task(waiter).unwrap().awaiting_count(), awaitees.len());

        s.finalize_cancelled(waiter);
        for &awaitee in &awaitees {
            assert!(
                s.task(awaitee).unwrap().live_waiters().is_empty(),
                "cancellation must deregister from every awaitee"
            );
        }
    }

    /// FI12.
    #[test]
    fn fanin_07_waiters_are_woken_in_registration_order() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let awaitee = s.spawn_parked_placeholder();
        let waiters: Vec<RuntimeTaskId> = (0..8).map(|_| s.spawn_parked_placeholder()).collect();
        for &waiter in &waiters {
            s.register_waiter(awaitee, waiter);
        }
        s.complete(awaitee);

        let mut claimed = Vec::new();
        while let Some(id) = s.claim_ready_for_worker(0) {
            claimed.push(id);
            s.clear_running();
        }
        assert_eq!(claimed, waiters, "completion wakes waiters FIFO");
    }

    /// FI13, FI14.
    #[test]
    fn fanin_08_stale_and_unknown_waiters_are_handled_quietly() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let awaitee = s.spawn_parked_placeholder();
        let live = s.spawn_parked_placeholder();
        s.register_waiter(awaitee, live);

        // Never registered: unregistering must not disturb the live relation.
        s.unregister_waiter(awaitee, 4_242);
        assert_eq!(s.task(awaitee).unwrap().live_waiters(), vec![live]);

        // Registered then reaped behind the awaitee's back.
        let reaped = s.spawn_parked_placeholder();
        s.register_waiter(awaitee, reaped);
        s.tasks.remove(reaped);

        s.complete(awaitee);
        assert_eq!(s.task_state(live), Some(RuntimeTaskState::Ready));
    }

    /// FI15.
    #[test]
    fn fanin_09_ten_thousand_fan_in_returns_to_the_metadata_baseline() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let awaitee = s.spawn_parked_placeholder();
        let waiters: Vec<RuntimeTaskId> =
            (0..FAN_IN).map(|_| s.spawn_parked_placeholder()).collect();
        for &waiter in &waiters {
            s.register_waiter(awaitee, waiter);
        }
        s.complete(awaitee);
        while let Some(id) = s.claim_ready_for_worker(0) {
            s.complete(id);
            s.clear_running();
        }
        assert_eq!(
            s.metadata_snapshot(),
            SchedulerMetadataSnapshot {
                heavy_tasks: 0,
                queue_entries: 0,
                pending_cleanups: 0,
                frame_roots: 0,
                blocked_syscalls: 0,
            }
        );
        assert!(s.take_pending_terminal_cleanups().is_empty());
        assert_eq!(s.metadata_snapshot(), empty_metadata());
    }

    // -------------------------------------------------------------------------
    // Frame-backed terminal status (willow-ezs.1.3).
    //
    // The scheduler publishes a task's terminal status into its async frame's
    // header, and the language-visible queries (`await task`,
    // `await task.result()`, `is_cancelled`) read it from there. Perspectives
    // covered here:
    //
    //   FST1  the state -> status mapping is exactly the three terminal states
    //   FST2  `complete` publishes Completed
    //   FST3  `finalize_cancelled` publishes Cancelled
    //   FST4  a Panicked transition publishes Panicked
    //   FST5  non-terminal transitions leave the frame Pending
    //   FST6  publication happens once: a later transition cannot rewrite it
    //   FST7  a task with a null frame completes without touching memory
    //   FST8  `willow_sched_cancel` mirrors the request bit into the frame
    //   FST9  cancel + finalize leaves BOTH the request bit and Cancelled
    //   FST10 cancelling an already-completed task does not disturb its status
    //   FST11 `willow_frame_await` reports done from a terminal frame even
    //         when the scheduler no longer knows the id (post-reaping contract)
    //   FST12 `willow_frame_await` on a pending frame still registers a waiter
    //   FST13 `willow_frame_await` with a null frame falls back to the id path
    //   FST14 `willow_frame_await_check` is a no-op for pending/completed frames
    //   FST15 `willow_sched_task_state` stays the id-only diagnostic: it goes
    //         unknown for a reaped id while the frame still answers
    //   FST16 an await slow path cannot observe Terminal before frame status
    //         publication, even when paused at that exact transition
    // -------------------------------------------------------------------------

    /// A stand-alone async frame for status tests (8-byte aligned words; the
    /// scheduler only ever touches header word 2).
    use crate::async_frame::willow_frame_is_cancelled;

    fn status_frame() -> Box<[i64; 8]> {
        Box::new([0; 8])
    }

    /// Spawn a ready placeholder that carries `frame`, as a real spawn would.
    fn spawn_with_frame(s: &mut RuntimeScheduler, frame: &mut [i64; 8]) -> RuntimeTaskId {
        let id = s.spawn_placeholder();
        s.with_task_mut(id, |task| {
            task.frame = frame.as_mut_ptr() as *mut c_void;
        });
        id
    }

    fn frame_ptr(frame: &mut [i64; 8]) -> *mut c_void {
        frame.as_mut_ptr() as *mut c_void
    }

    fn terminal_of(frame: &mut [i64; 8]) -> i64 {
        crate::async_frame::frame_terminal_status(frame_ptr(frame))
    }

    #[test]
    fn fst_01_only_terminal_states_map_to_a_status() {
        use crate::async_frame::{
            WILLOW_FRAME_STATUS_CANCELLED, WILLOW_FRAME_STATUS_COMPLETED,
            WILLOW_FRAME_STATUS_PANICKED,
        };
        assert_eq!(
            terminal_frame_status(RuntimeTaskState::Completed),
            Some(WILLOW_FRAME_STATUS_COMPLETED)
        );
        assert_eq!(
            terminal_frame_status(RuntimeTaskState::Cancelled),
            Some(WILLOW_FRAME_STATUS_CANCELLED)
        );
        assert_eq!(
            terminal_frame_status(RuntimeTaskState::Panicked),
            Some(WILLOW_FRAME_STATUS_PANICKED)
        );
        for state in [
            RuntimeTaskState::Ready,
            RuntimeTaskState::Running,
            RuntimeTaskState::Parked,
            RuntimeTaskState::Cancelling,
            RuntimeTaskState::BlockedSyscall,
        ] {
            assert_eq!(
                terminal_frame_status(state),
                None,
                "{state:?} is still runnable and must not publish a terminal status"
            );
        }
    }

    #[test]
    fn fst_02_complete_publishes_completed() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let mut frame = status_frame();
        let id = spawn_with_frame(&mut s, &mut frame);
        assert_eq!(terminal_of(&mut frame), 0);
        s.complete(id);
        assert_eq!(
            terminal_of(&mut frame),
            crate::async_frame::WILLOW_FRAME_STATUS_COMPLETED
        );
    }

    #[test]
    fn fst_03_finalize_cancelled_publishes_cancelled() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let mut frame = status_frame();
        let id = spawn_with_frame(&mut s, &mut frame);
        s.finalize_cancelled(id);
        assert_eq!(
            terminal_of(&mut frame),
            crate::async_frame::WILLOW_FRAME_STATUS_CANCELLED
        );
    }

    #[test]
    fn fst_04_panicked_transition_publishes_panicked() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let mut frame = status_frame();
        let id = spawn_with_frame(&mut s, &mut frame);
        s.finalize_panicked(id);
        assert_eq!(
            terminal_of(&mut frame),
            crate::async_frame::WILLOW_FRAME_STATUS_PANICKED
        );
        assert_eq!(
            s.task_state(id),
            None,
            "the panicked task's heavy metadata must be reaped"
        );
    }

    #[test]
    fn fst_05_non_terminal_transitions_stay_pending() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let mut frame = status_frame();
        let id = spawn_with_frame(&mut s, &mut frame);
        s.set_running(id);
        s.finish_pending_poll(id);
        s.wake(id);
        s.set_running(id);
        s.clear_running();
        assert_eq!(
            terminal_of(&mut frame),
            crate::async_frame::WILLOW_FRAME_STATUS_PENDING,
            "run/park/wake churn must not look like a finished task"
        );
    }

    #[test]
    fn fst_06_first_terminal_status_wins() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let mut frame = status_frame();
        let id = spawn_with_frame(&mut s, &mut frame);
        s.complete(id);
        // A late cancellation path must not rewrite a published result.
        s.finalize_cancelled(id);
        assert_eq!(
            terminal_of(&mut frame),
            crate::async_frame::WILLOW_FRAME_STATUS_COMPLETED
        );
    }

    #[test]
    fn fst_07_null_frame_task_completes_safely() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        assert!(s.task(id).unwrap().frame.is_null());
        s.complete(id);
        assert_eq!(s.task_state(id), None);
    }

    #[test]
    fn fst_08_cancel_request_is_mirrored_into_the_frame() {
        let _guard = crate::gc::runtime_test_guard();
        reset_global_scheduler_for_test();
        let mut frame = status_frame();
        let id = with_global_for_test(|s| {
            let id = s.spawn_placeholder();
            s.with_task_mut(id, |task| {
                task.frame = frame.as_mut_ptr() as *mut c_void;
            });
            s.park(id);
            id
        });
        willow_sched_cancel(id);
        assert_eq!(
            willow_frame_is_cancelled(frame_ptr(&mut frame)),
            1,
            "is_cancelled() must answer from the frame as soon as cancel is requested"
        );
        assert_eq!(
            terminal_of(&mut frame),
            crate::async_frame::WILLOW_FRAME_STATUS_PENDING,
            "a requested cancel is not yet a finished task"
        );
        reset_global_scheduler_for_test();
    }

    #[test]
    fn fst_09_cancel_then_finalize_keeps_both_bits() {
        let _guard = crate::gc::runtime_test_guard();
        reset_global_scheduler_for_test();
        let mut frame = status_frame();
        let id = with_global_for_test(|s| {
            let id = s.spawn_placeholder();
            s.with_task_mut(id, |task| {
                task.frame = frame.as_mut_ptr() as *mut c_void;
            });
            s.park(id);
            id
        });
        willow_sched_cancel(id);
        with_global_for_test(|s| s.finalize_cancelled(id));
        let status = crate::async_frame::frame_status(frame_ptr(&mut frame));
        assert_eq!(
            status & crate::async_frame::WILLOW_FRAME_STATUS_TERMINAL_MASK,
            crate::async_frame::WILLOW_FRAME_STATUS_CANCELLED
        );
        assert_ne!(
            status & crate::async_frame::WILLOW_FRAME_STATUS_CANCEL_REQUESTED,
            0
        );
        reset_global_scheduler_for_test();
    }

    #[test]
    fn fst_10_cancelling_a_completed_task_keeps_completed() {
        let _guard = crate::gc::runtime_test_guard();
        reset_global_scheduler_for_test();
        let mut frame = status_frame();
        let id = with_global_for_test(|s| {
            let id = s.spawn_placeholder();
            s.with_task_mut(id, |task| {
                task.frame = frame.as_mut_ptr() as *mut c_void;
            });
            s.complete(id);
            id
        });
        willow_sched_cancel(id);
        assert_eq!(
            terminal_of(&mut frame),
            crate::async_frame::WILLOW_FRAME_STATUS_COMPLETED
        );
        assert_eq!(
            willow_frame_is_cancelled(frame_ptr(&mut frame)),
            0,
            "cancelling a task that already finished must not make it look cancelled"
        );
        reset_global_scheduler_for_test();
    }

    #[test]
    fn fst_11_await_of_a_terminal_frame_needs_no_task_record() {
        let _guard = crate::gc::runtime_test_guard();
        reset_global_scheduler_for_test();
        let mut frame = status_frame();
        crate::async_frame::frame_publish_terminal(
            frame_ptr(&mut frame),
            crate::async_frame::WILLOW_FRAME_STATUS_COMPLETED,
        );
        // Id 9_999 was never spawned here: this is the shape a reaped task
        // leaves behind, and the frame alone must answer "done".
        assert_eq!(willow_frame_await(frame_ptr(&mut frame), 9_999), 1);
        reset_global_scheduler_for_test();
    }

    #[test]
    fn fst_12_await_of_a_pending_frame_registers_a_waiter() {
        let _guard = crate::gc::runtime_test_guard();
        reset_global_scheduler_for_test();
        let mut frame = status_frame();
        let (awaitee, waiter) = with_global_for_test(|s| {
            let awaitee = s.spawn_placeholder();
            s.with_task_mut(awaitee, |task| {
                task.frame = frame.as_mut_ptr() as *mut c_void;
            });
            let waiter = s.spawn_parked_placeholder();
            (awaitee, waiter)
        });
        let done = with_current_task_for_test(waiter, || {
            willow_frame_await(frame_ptr(&mut frame), awaitee)
        });
        assert_eq!(done, 0, "a pending frame must fall through to registration");
        with_global_for_test(|s| {
            assert_eq!(s.task(awaitee).unwrap().live_waiters(), vec![waiter]);
        });
        // Completing the awaitee both wakes the waiter and publishes the status.
        with_global_for_test(|s| s.complete(awaitee));
        assert_eq!(
            terminal_of(&mut frame),
            crate::async_frame::WILLOW_FRAME_STATUS_COMPLETED
        );
        assert_eq!(willow_frame_await(frame_ptr(&mut frame), awaitee), 1);
        reset_global_scheduler_for_test();
    }

    #[test]
    fn fst_13_null_frame_await_falls_back_to_the_id_path() {
        let _guard = crate::gc::runtime_test_guard();
        reset_global_scheduler_for_test();
        // Unknown id on the id path is treated as ready, so no permanent park.
        assert_eq!(willow_frame_await(std::ptr::null_mut(), 9_999), 1);
        let id = with_global_for_test(|s| s.spawn_parked_placeholder());
        let waiter = with_global_for_test(|s| s.spawn_parked_placeholder());
        let done =
            with_current_task_for_test(waiter, || willow_frame_await(std::ptr::null_mut(), id));
        assert_eq!(done, 0);
        with_global_for_test(|s| assert_eq!(s.task(id).unwrap().live_waiters(), vec![waiter]));
        reset_global_scheduler_for_test();
    }

    #[test]
    fn fst_14_await_check_passes_for_non_cancelled_frames() {
        let _guard = crate::gc::runtime_test_guard();
        reset_global_scheduler_for_test();
        // Pending, Completed and Panicked frames must all return normally; only
        // a Cancelled frame aborts (an abort cannot be exercised in-process).
        let mut pending = status_frame();
        willow_frame_await_check(frame_ptr(&mut pending), 1);
        let mut completed = status_frame();
        crate::async_frame::frame_publish_terminal(
            frame_ptr(&mut completed),
            crate::async_frame::WILLOW_FRAME_STATUS_COMPLETED,
        );
        willow_frame_await_check(frame_ptr(&mut completed), 2);
        let mut panicked = status_frame();
        crate::async_frame::frame_publish_terminal(
            frame_ptr(&mut panicked),
            crate::async_frame::WILLOW_FRAME_STATUS_PANICKED,
        );
        willow_frame_await_check(frame_ptr(&mut panicked), 3);
        // A cancel REQUEST that never finalized is not a cancelled await either.
        let mut requested = status_frame();
        crate::async_frame::frame_request_cancel(frame_ptr(&mut requested));
        willow_frame_await_check(frame_ptr(&mut requested), 4);
        reset_global_scheduler_for_test();
    }

    #[test]
    fn fst_15_task_state_stays_the_id_only_diagnostic() {
        let _guard = crate::gc::runtime_test_guard();
        reset_global_scheduler_for_test();
        let mut frame = status_frame();
        let id = with_global_for_test(|s| {
            let id = s.spawn_placeholder();
            s.with_task_mut(id, |task| {
                task.frame = frame.as_mut_ptr() as *mut c_void;
            });
            s.complete(id);
            id
        });
        assert_eq!(
            willow_sched_task_state(id),
            -1,
            "the id-only diagnostic becomes Unknown immediately after reaping"
        );
        assert_eq!(
            crate::async_frame::frame_terminal_status(frame_ptr(&mut frame)),
            crate::async_frame::WILLOW_FRAME_STATUS_COMPLETED
        );
        assert_eq!(willow_frame_await(frame_ptr(&mut frame), id), 1);
        reset_global_scheduler_for_test();
    }

    #[test]
    fn fst_16_terminal_transition_holds_shard_through_frame_publication() {
        let mut scheduler = RuntimeScheduler::with_worker_count(1);
        let mut frame = status_frame();
        let awaitee = spawn_with_frame(&mut scheduler, &mut frame);
        scheduler.set_running(awaitee);
        let waiter = scheduler.spawn_parked_placeholder();
        let tasks = Arc::clone(&scheduler.tasks);

        let (transitioned_tx, transitioned_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let publisher_tasks = Arc::clone(&tasks);
        let publisher = std::thread::spawn(move || {
            publisher_tasks
                .finish_terminal_and_publish(
                    awaitee,
                    crate::async_frame::WILLOW_FRAME_STATUS_CANCELLED,
                    || {
                        transitioned_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                    },
                )
                .unwrap()
        });

        transitioned_rx.recv().unwrap();
        assert_eq!(
            terminal_of(&mut frame),
            crate::async_frame::WILLOW_FRAME_STATUS_PENDING,
            "the barrier must hold the historical state/status race point"
        );

        let (attempted_tx, attempted_rx) = std::sync::mpsc::channel();
        let (registered_tx, registered_rx) = std::sync::mpsc::channel();
        let observer_tasks = Arc::clone(&tasks);
        let observer = std::thread::spawn(move || {
            attempted_tx.send(()).unwrap();
            registered_tx
                .send(register_waiter_sharded(&observer_tasks, awaitee, waiter))
                .unwrap();
        });
        attempted_rx.recv().unwrap();
        assert!(
            registered_rx
                .recv_timeout(Duration::from_millis(20))
                .is_err(),
            "the await slow path must remain behind the terminal publisher's shard lock"
        );

        release_tx.send(()).unwrap();
        assert!(publisher.join().unwrap());
        assert!(
            !registered_rx.recv().unwrap(),
            "after publication, Terminal must refuse waiter registration"
        );
        observer.join().unwrap();
        assert_eq!(
            terminal_of(&mut frame),
            crate::async_frame::WILLOW_FRAME_STATUS_CANCELLED
        );
    }

    // ---------------------------------------------------------------------
    // Bounded scheduler metadata under 10,000-Task workloads
    // (willow-ezs.1.4/.1.5).
    //
    // These are deterministic ownership assertions, not RSS benchmarks. Each
    // global workload uses the production scheduler entry point, whose default
    // and minimum worker count is five. The blocked-syscall workload drives a
    // local five-worker scheduler because real blocking jobs would make 10,000
    // native syscalls the subject of the test instead of scheduler metadata.
    // ---------------------------------------------------------------------

    const STRESS_TASKS: usize = 10_000;

    fn empty_metadata() -> SchedulerMetadataSnapshot {
        SchedulerMetadataSnapshot {
            heavy_tasks: 0,
            queue_entries: 0,
            pending_cleanups: 0,
            frame_roots: 0,
            blocked_syscalls: 0,
        }
    }

    fn assert_global_metadata_reaped(workload: &str) {
        let snapshot = with_global_for_test(|scheduler| scheduler.metadata_snapshot());
        assert_eq!(
            snapshot,
            empty_metadata(),
            "{workload}: scheduler-owned metadata must return to its baseline"
        );
        assert_eq!(willow_sched_heavy_task_count(), 0);
        assert_eq!(willow_sched_queue_entry_count(), 0);
        assert_eq!(willow_sched_pending_cleanup_count(), 0);
        assert_eq!(willow_sched_frame_root_count(), 0);
    }

    fn reset_stress_fixture() {
        reset_global_scheduler_for_test();
        reset_internal_for_test();
        assert_eq!(DEFAULT_WORKERS, 5);
        assert!(
            runtime_worker_config().active_workers() >= DEFAULT_WORKERS,
            "the scheduler must retain its minimum five-worker contract"
        );
    }

    unsafe extern "C" fn poll_zero_sleep_then_ready(frame: *mut c_void) -> i32 {
        let state =
            unsafe { &mut *((frame as *mut u8).add(async_frame_slot_offset(0)) as *mut i64) };
        *state += 1;
        if *state >= 2 {
            RUNTIME_POLL_READY
        } else {
            willow_sched_sleep(0);
            RUNTIME_POLL_PENDING
        }
    }

    unsafe extern "C" fn poll_yield_preempt_churn(frame: *mut c_void) -> i32 {
        let state =
            unsafe { &mut *((frame as *mut u8).add(async_frame_slot_offset(0)) as *mut i64) };
        *state += 1;
        match *state {
            1 | 3 => RUNTIME_POLL_YIELD,
            2 | 4 => RUNTIME_POLL_PREEMPTED,
            _ => RUNTIME_POLL_READY,
        }
    }

    #[test]
    fn reaping_01_poll_result_classification_is_exhaustive() {
        assert_eq!(
            classify_poll_result(RUNTIME_POLL_PENDING),
            PollOutcome::Pending
        );
        assert_eq!(classify_poll_result(RUNTIME_POLL_READY), PollOutcome::Ready);
        assert_eq!(classify_poll_result(RUNTIME_POLL_YIELD), PollOutcome::Yield);
        assert_eq!(
            classify_poll_result(RUNTIME_POLL_PREEMPTED),
            PollOutcome::Preempted
        );
        assert_eq!(
            classify_poll_result(RUNTIME_POLL_PANICKED),
            PollOutcome::Panicked
        );
        assert_eq!(
            classify_poll_result(RUNTIME_POLL_BLOCKED_SYSCALL),
            PollOutcome::BlockedSyscall
        );
        assert_eq!(classify_poll_result(99), PollOutcome::Invalid(99));
    }

    #[test]
    fn reaping_02_finish_terminal_captures_cleanup_before_task_removal() {
        let mut scheduler = RuntimeScheduler::with_worker_count(DEFAULT_WORKERS);
        let id = scheduler.spawn_placeholder();
        scheduler.with_task_mut(id, |task| {
            task.add_wait_channel(0x1234);
            task.add_wait_channel(0x5678);
        });

        scheduler.complete(id);

        assert_eq!(scheduler.task_state(id), None);
        let cleanups = scheduler.take_pending_terminal_cleanups();
        assert_eq!(cleanups.len(), 1);
        assert_eq!(cleanups[0].task_id, id);
        assert_eq!(cleanups[0].channel_waits, vec![0x1234, 0x5678]);
        assert!(cleanups[0].lock_wait.is_none());
    }

    #[test]
    fn reaping_02b_terminal_cleanup_carries_lock_link_past_task_removal() {
        use crate::async_mutex::{AsyncMutex, MutexAcquire, MutexRelease};

        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let mutex = AsyncMutex::new(0, false);
        let (owner, victim) = with_global_for_test(|scheduler| {
            (
                scheduler.spawn_parked_placeholder(),
                scheduler.spawn_parked_placeholder(),
            )
        });
        let MutexAcquire::Acquired(owner_token) = mutex.acquire(owner) else {
            panic!("owner should acquire");
        };
        let MutexAcquire::Pending(_) = mutex.acquire(victim) else {
            panic!("victim should register behind the owner");
        };
        assert_eq!(mutex.waiter_count(), 1);

        with_global_for_test(|scheduler| scheduler.finalize_cancelled(victim));
        assert_eq!(
            willow_sched_task_state(victim),
            -1,
            "the heavy task record must already be removed"
        );
        drain_terminal_cleanups();

        assert_eq!(
            mutex.waiter_count(),
            0,
            "captured reverse link must remove the waiter without waiting for a future release"
        );
        assert_eq!(mutex.owner(), Some((owner, owner_token)));
        assert!(matches!(
            mutex.release(owner, owner_token),
            MutexRelease::Released { handed_to: None }
        ));
    }

    static NESTED_ROOT_TARGET: TestAtomicU64 = TestAtomicU64::new(0);
    static NESTED_ROOTS_RETAINED: TestAtomicBool = TestAtomicBool::new(false);

    unsafe extern "C" fn poll_nested_then_observe_frame_roots(_frame: *mut c_void) -> i32 {
        willow_sched_run_until(NESTED_ROOT_TARGET.load(TestOrdering::SeqCst));
        NESTED_ROOTS_RETAINED.store(willow_sched_frame_root_count() == 2, TestOrdering::SeqCst);
        RUNTIME_POLL_READY
    }

    #[test]
    fn reaping_03_nested_drive_retains_terminal_frame_until_outer_quiescence() {
        let _guard = runtime_test_guard();
        reset_stress_fixture();
        NESTED_ROOTS_RETAINED.store(false, TestOrdering::SeqCst);

        let inner_frame = willow_async_frame_alloc(0, 0) as *mut c_void;
        let inner = willow_sched_spawn(poll_ready_now, inner_frame);
        NESTED_ROOT_TARGET.store(inner, TestOrdering::SeqCst);
        let outer_frame = willow_async_frame_alloc(0, 0) as *mut c_void;
        willow_sched_spawn(poll_nested_then_observe_frame_roots, outer_frame);

        assert_eq!(willow_sched_run(), 2);
        assert!(
            NESTED_ROOTS_RETAINED.load(TestOrdering::SeqCst),
            "the completed inner frame must stay rooted until the outer drive quiesces"
        );
        assert_global_metadata_reaped("nested frame-root lifetime");
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_stress_fixture();
    }

    #[test]
    fn reaping_04_executable_task_keeps_netpoll_cleanup_even_without_channels() {
        let mut scheduler = RuntimeScheduler::with_worker_count(DEFAULT_WORKERS);
        let id = scheduler.spawn_task(poll_ready_now, std::ptr::null_mut());
        assert_eq!(scheduler.claim_ready_for_worker(0), Some(id));
        scheduler.complete(id);
        scheduler.clear_running();

        let cleanups = scheduler.take_pending_terminal_cleanups();
        assert_eq!(cleanups.len(), 1);
        assert_eq!(cleanups[0].task_id, id);
        assert!(cleanups[0].channel_waits.is_empty());
    }

    #[test]
    fn stress_10k_sleeping_tasks_reap_all_scheduler_metadata() {
        let _guard = runtime_test_guard();
        reset_stress_fixture();

        for _ in 0..STRESS_TASKS {
            let frame = willow_async_frame_alloc(1, 0) as *mut c_void;
            willow_sched_spawn(poll_zero_sleep_then_ready, frame);
        }
        assert_eq!(willow_sched_run(), STRESS_TASKS as i64);
        assert_global_metadata_reaped("10k sleeping tasks");
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_stress_fixture();
    }

    #[test]
    fn stress_10k_tasks_on_one_channel_reap_all_scheduler_metadata() {
        let _guard = runtime_test_guard();
        reset_stress_fixture();
        let channel = crate::channel::willow_channel_new(0);
        let task_ids = with_global_for_test(|scheduler| {
            (0..STRESS_TASKS)
                .map(|_| scheduler.spawn_parked_placeholder())
                .collect::<Vec<_>>()
        });

        for task_id in task_ids {
            assert_eq!(
                with_current_task_for_test(task_id, || {
                    crate::channel::willow_channel_recv_ready(channel)
                }),
                0
            );
        }
        crate::channel::willow_channel_close(channel);
        assert_eq!(willow_sched_run(), STRESS_TASKS as i64);
        assert_global_metadata_reaped("10k tasks on one channel");
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_stress_fixture();
    }

    #[test]
    fn stress_10k_bounded_send_waiters_pass_on_defected_handoffs() {
        let _guard = runtime_test_guard();
        reset_stress_fixture();
        let channel = crate::channel::willow_channel_new_bounded(0, 1);
        assert_eq!(crate::channel::willow_channel_try_send_i64(channel, 1), 1);
        let task_ids = with_global_for_test(|scheduler| {
            (0..STRESS_TASKS)
                .map(|_| scheduler.spawn_parked_placeholder())
                .collect::<Vec<_>>()
        });
        for &task_id in &task_ids {
            assert_eq!(
                with_current_task_for_test(task_id, || {
                    crate::channel::willow_channel_send_ready(channel)
                }),
                0
            );
        }

        // Free one slot. Each awakened select then defects to another arm;
        // unregister must pass the unconsumed handoff to the next producer
        // until every one of the 10,000 waiters has had a turn.
        assert_eq!(crate::channel::willow_channel_recv_i64(channel), 1);
        for task_id in task_ids {
            with_current_task_for_test(task_id, || {
                crate::channel::willow_channel_unregister_waiter(channel);
            });
        }

        assert_eq!(willow_sched_run(), STRESS_TASKS as i64);
        assert_global_metadata_reaped("10k bounded-send handoff defections");
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_stress_fixture();
    }

    #[test]
    fn stress_10k_yield_preempt_tasks_reap_all_scheduler_metadata() {
        let _guard = runtime_test_guard();
        reset_stress_fixture();

        for _ in 0..STRESS_TASKS {
            let frame = willow_async_frame_alloc(1, 0) as *mut c_void;
            willow_sched_spawn(poll_yield_preempt_churn, frame);
        }
        assert_eq!(willow_sched_run(), STRESS_TASKS as i64);
        assert_global_metadata_reaped("10k yield/preempt tasks");
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_stress_fixture();
    }

    #[test]
    fn stress_10k_blocked_syscall_tasks_reap_all_scheduler_metadata() {
        let mut scheduler = RuntimeScheduler::with_worker_count(DEFAULT_WORKERS);
        let task_ids = (0..STRESS_TASKS)
            .map(|_| scheduler.spawn_placeholder())
            .collect::<Vec<_>>();

        for &task_id in &task_ids {
            assert_eq!(scheduler.claim_ready_for_worker(0), Some(task_id));
            scheduler.finish_blocked_syscall_poll(task_id);
            scheduler.clear_running();
        }
        assert_eq!(
            scheduler.metadata_snapshot(),
            SchedulerMetadataSnapshot {
                heavy_tasks: STRESS_TASKS,
                queue_entries: 0,
                pending_cleanups: 0,
                frame_roots: 0,
                blocked_syscalls: STRESS_TASKS,
            }
        );
        assert!(scheduler.blocked_syscall_invariant_holds());

        for &task_id in &task_ids {
            assert!(scheduler.wake(task_id));
        }
        for &task_id in &task_ids {
            assert_eq!(scheduler.claim_ready_for_worker(0), Some(task_id));
            scheduler.complete(task_id);
            scheduler.clear_running();
        }
        assert!(scheduler.take_pending_terminal_cleanups().is_empty());
        assert_eq!(scheduler.metadata_snapshot(), empty_metadata());
        assert!(scheduler.blocked_syscall_invariant_holds());
    }

    #[test]
    fn stress_repeated_10k_short_task_batches_plateau_at_zero() {
        let _guard = runtime_test_guard();
        reset_stress_fixture();

        for batch in 0..3 {
            for _ in 0..STRESS_TASKS {
                willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
            }
            assert_eq!(willow_sched_run(), STRESS_TASKS as i64);
            assert_global_metadata_reaped(&format!("10k short-task batch {batch}"));
        }
        reset_stress_fixture();
    }

    /// Install a fresh panic context for the duration of the returned guard, so
    /// a runtime fault raised by a fixture is recorded and consumed here instead
    /// of leaking into the next test.
    struct TestPanicContext(Option<Arc<crate::panic_context::PanicContext>>);

    fn install_test_panic_context(owner: u64) -> TestPanicContext {
        TestPanicContext(crate::panic_context::replace_current_context(Some(
            Arc::new(crate::panic_context::PanicContext::new(owner)),
        )))
    }

    impl Drop for TestPanicContext {
        fn drop(&mut self) {
            crate::panic_context::replace_current_context(self.0.take());
        }
    }

    /// Consume the pending panic (as a deferred `recover()` would) and return
    /// its message.
    fn take_test_panic_message() -> String {
        crate::panic_context::willow_panic_enter_defer();
        let info = crate::panic_context::willow_panic_recover();
        let message = if info.is_null() {
            String::new()
        } else {
            let message = unsafe { crate::panic_context::panic_info_message(info) }.to_string();
            crate::panic_context::willow_panic_release_recovered(info);
            message
        };
        crate::panic_context::willow_panic_leave_defer();
        message
    }

    // ── Lost-wakeup: the pop→claim window (willow-atth) ──────────────────────
    //
    // A claim pops a task id out of every run queue BEFORE it takes
    // `claim_gate` and increments `active_polls`. In that window the work is
    // invisible to every idle check, so a concurrently idling worker could
    // declare global quiescence, stop the pool, and strand a runnable task —
    // observed as `await` returning early (partial output, exit 0) and as a
    // no-default `select` giving up. These perspectives pin the in-flight
    // marker, the checks that consult it, and the select idle policy.

    /// Perspective 01. A quiet scheduler has no claim in flight.
    #[test]
    fn sched_wake_01_no_claim_in_flight_when_idle() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        assert!(!claims_in_flight());
    }

    /// Perspective 02. The marker is published for the whole guard scope and
    /// retracted on drop.
    #[test]
    fn sched_wake_02_guard_publishes_and_retracts() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        {
            let _claim = ClaimInFlight::enter();
            assert!(claims_in_flight());
        }
        assert!(!claims_in_flight());
    }

    /// Perspective 03. Several workers can be inside the window at once, so the
    /// marker counts rather than latches.
    #[test]
    fn sched_wake_03_guards_nest() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let outer = ClaimInFlight::enter();
        let inner = ClaimInFlight::enter();
        assert!(claims_in_flight());
        drop(inner);
        assert!(claims_in_flight(), "one claim is still in flight");
        drop(outer);
        assert!(!claims_in_flight());
    }

    /// Perspective 04. The marker is process-global, not thread-local: the
    /// worker that observes idleness is never the one holding the claim.
    #[test]
    fn sched_wake_04_marker_is_visible_across_threads() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let entered = Arc::new(Barrier::new(2));
        let observed = Arc::new(Barrier::new(2));
        let holder_entered = Arc::clone(&entered);
        let holder_observed = Arc::clone(&observed);
        let holder = std::thread::spawn(move || {
            let _claim = ClaimInFlight::enter();
            holder_entered.wait();
            holder_observed.wait();
        });
        entered.wait();
        let seen = claims_in_flight();
        observed.wait();
        holder.join().expect("claim holder thread");
        assert!(seen, "another thread's claim must be observable");
        assert!(!claims_in_flight());
    }

    /// Perspective 05. An empty scheduler genuinely has no wake source.
    #[test]
    fn sched_wake_05_no_wake_source_when_empty() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        assert!(!scheduler_has_wake_source());
    }

    /// Perspective 06. The pop→claim window alone counts as a wake source. This
    /// is the exact state that used to read as "nothing left to do".
    #[test]
    fn sched_wake_06_in_flight_claim_is_a_wake_source() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let _claim = ClaimInFlight::enter();
        assert!(
            scheduler_has_wake_source(),
            "a popped-but-unclaimed task is pending work"
        );
    }

    /// Perspective 07. Queued work is a wake source.
    #[test]
    fn sched_wake_07_queued_task_is_a_wake_source() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
        assert!(scheduler_has_wake_source());
        willow_sched_run();
    }

    /// Perspective 08. An armed timer is a wake source even with empty queues.
    #[test]
    fn sched_wake_08_armed_timer_is_a_wake_source() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let id = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);
        with_current_task_for_test(id, || set_global_wake_after_millis(50));
        assert!(
            scheduler_has_wake_source(),
            "a sleeping task will become runnable"
        );
        reset_global_scheduler_for_test();
    }

    /// Perspective 09. Idle detection keeps the run loop alive while a claim is
    /// in flight instead of reporting quiescence.
    #[test]
    fn sched_wake_09_idle_step_waits_for_an_in_flight_claim() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let _claim = ClaimInFlight::enter();
        assert!(
            scheduler_idle_step(0, None, false, None),
            "the run loop must keep going while a claim is in flight"
        );
    }

    /// Perspective 10. Without a claim, the same empty scheduler still reports
    /// idle — the fix must not turn every drive into a spin.
    #[test]
    fn sched_wake_10_idle_step_still_reports_genuine_idle() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        assert!(
            !scheduler_idle_step(0, None, false, None),
            "an empty scheduler with no claim is genuinely idle"
        );
    }

    /// Perspective 11. A drive over an empty scheduler leaves no claim behind,
    /// so the marker cannot leak into later drives.
    #[test]
    fn sched_wake_11_empty_drive_leaves_no_claim() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        assert_eq!(willow_sched_run(), 0);
        assert!(!claims_in_flight());
    }

    /// Perspective 12. Every claim path (poll, requeue, drop) retracts the
    /// marker: a completed batch ends at zero.
    #[test]
    fn sched_wake_12_completed_batch_leaves_no_claim() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        for _ in 0..64 {
            willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
        }
        assert_eq!(willow_sched_run(), 64);
        assert!(!claims_in_flight(), "claims are scoped to the pop");
        assert_eq!(global_run_queues().len(), 0, "no task is stranded");
    }

    /// Perspective 13. The test reset clears the marker, so a leaked claim in
    /// one test cannot make every later test's scheduler look busy.
    #[test]
    fn sched_wake_13_reset_clears_the_marker() {
        let _guard = runtime_test_guard();
        std::mem::forget(ClaimInFlight::enter());
        assert!(claims_in_flight());
        reset_global_scheduler_for_test();
        assert!(!claims_in_flight());
    }

    /// Perspective 14. `select` idle wait with work still queued returns
    /// quietly so the caller re-probes.
    #[test]
    fn sched_wake_14_select_idle_wait_returns_when_work_is_queued() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let _context = install_test_panic_context(101);
        willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
        willow_select_idle_wait();
        assert_eq!(
            crate::panic_context::willow_panic_active(),
            0,
            "queued work means a case can still become ready"
        );
        willow_sched_run();
    }

    /// Perspective 15. The same holds for the pop→claim window: a racing claim
    /// must not be mistaken for a deadlocked select.
    #[test]
    fn sched_wake_15_select_idle_wait_returns_for_an_in_flight_claim() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let _context = install_test_panic_context(102);
        let _claim = ClaimInFlight::enter();
        willow_select_idle_wait();
        assert_eq!(crate::panic_context::willow_panic_active(), 0);
    }

    /// Perspective 16. With no wake source at all the select would block
    /// forever, so it raises a language panic instead of spinning or (as
    /// before) falling through and running no case at all.
    #[test]
    fn sched_wake_16_select_idle_wait_raises_on_real_deadlock() {
        let _guard = runtime_test_guard();
        crate::gc::willow_gc_init();
        reset_global_scheduler_for_test();
        let _context = install_test_panic_context(103);
        willow_select_idle_wait();
        assert_eq!(crate::panic_context::willow_panic_active(), 1);
        let message = take_test_panic_message();
        assert!(
            message.contains("select would block forever"),
            "unexpected diagnostic: {message}"
        );
    }

    /// Perspective 17. The deadlock diagnostic is bounded: it must not hang the
    /// program it is meant to explain.
    #[test]
    fn sched_wake_17_select_idle_wait_is_bounded() {
        let _guard = runtime_test_guard();
        crate::gc::willow_gc_init();
        reset_global_scheduler_for_test();
        let _context = install_test_panic_context(104);
        let started = Instant::now();
        willow_select_idle_wait();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "idle wait must stay bounded, took {:?}",
            started.elapsed()
        );
        take_test_panic_message();
    }

    /// Perspective 18. A drive must not report quiescence while another thread
    /// is still spawning work into it: every task spawned before the drive
    /// returns is either completed or still queued, never lost.
    #[test]
    fn sched_wake_18_concurrent_spawns_are_never_stranded() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        const ROUNDS: usize = 16;
        const PER_ROUND: usize = 32;
        for _ in 0..ROUNDS {
            for _ in 0..PER_ROUND {
                willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
            }
            let completed = willow_sched_run();
            assert_eq!(completed, PER_ROUND as i64, "every spawned task must run");
            assert_eq!(global_run_queues().len(), 0);
            assert!(!claims_in_flight());
        }
    }

    /// Perspective 19. `run_until` returns only once its target is terminal,
    /// repeated enough times to cross the pop→claim window under the default
    /// five-worker pool. Before the fix this could return with the target still
    /// pending, and the caller then read an unwritten result slot.
    #[test]
    fn sched_wake_19_run_until_never_returns_with_a_pending_target() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        for round in 0..64 {
            // Background load so several workers are claiming concurrently.
            for _ in 0..8 {
                willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
            }
            // Parks on a real timer, so the target is woken by the run loop
            // rather than depending on the drive that spawned it.
            let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
            let target = willow_sched_spawn(poll_sleep_then_ready, frame);
            willow_sched_run_until(target);
            assert!(
                target_is_done(Some(target)),
                "run_until returned with the target pending in round {round}"
            );
        }
        willow_sched_run();
    }

    /// Perspective 20. Repeated parallel drives neither leak the marker nor
    /// leave queued work behind, so the fix adds no steady-state cost.
    #[test]
    fn sched_wake_20_repeated_parallel_drives_settle_at_zero() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        for _ in 0..32 {
            for _ in 0..16 {
                willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
            }
            willow_sched_run();
        }
        assert!(!claims_in_flight());
        assert_eq!(global_run_queues().len(), 0);
        assert_eq!(willow_sched_run(), 0, "a settled scheduler drives to zero");
    }
}
