mod run_queues;
use run_queues::*;
#[path = "scheduler_parking.rs"]
mod parking;
pub(crate) use parking::Parking;
#[cfg(test)]
use parking::notify_all_idle_waiters;
use parking::{current_wake_generation, wait_for_wake_since};
mod task_api;
pub use task_api::*;
mod drive;
#[cfg(all(test, debug_assertions))]
#[path = "scheduler_worker_failure_tests.rs"]
mod worker_failure_tests;
pub use drive::*;

#[path = "scheduler_mutex_observation.rs"]
mod mutex_observation;
use mutex_observation::{MutexObservation, RuntimeMutexObservation};

#[path = "scheduler_retry_observation.rs"]
mod retry_observation;
use retry_observation::{RetryObservation, RetrySample, RuntimeRetryObservation};

#[path = "scheduler_queue_observation.rs"]
mod queue_observation;
use queue_observation::{QueueObservation, RuntimeQueueObservation};

use crossbeam_deque::{Injector, Steal, Stealer, Worker};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use willow_abi::workers::default_worker_count;
#[cfg(test)]
use willow_abi::workers::parse_worker_count;

use crate::lock_wait::{LockId, LockWaitLink, RegistrationToken};
use crate::task::{
    ChannelOwnershipToken, RUNTIME_POLL_BLOCKED_SYSCALL, RUNTIME_POLL_PANICKED,
    RUNTIME_POLL_PENDING, RUNTIME_POLL_PREEMPTED, RUNTIME_POLL_READY, RUNTIME_POLL_YIELD,
    RuntimeCancelFn, RuntimePollFn, RuntimeTask, RuntimeTaskId, RuntimeTaskState,
};
use crate::task_state::{BoundaryOutcome, CancelOutcome, ClaimOutcome, TaskLifecycle, WakeOutcome};
use crate::timer_queue::{TimerQueue, TimerWake};

/// A terminal task's frame awaiting the end of its grace period.
#[derive(Debug, Clone, Copy)]
struct RetiredFrame {
    frame: usize,
    tag: u64,
}

/// The ordinary run loop scans only once this batch is reached. Explicit
/// major collection and outermost drive exit also release smaller batches.
const FRAME_RELEASE_MIN_BATCH: usize = 64;

/// Lock-free half of terminal task cleanup. The task record is already gone
/// when this runs, so channel addresses and the exact lock reverse link must be
/// captured before removal.
#[derive(Debug)]
struct TerminalCleanup {
    task_id: RuntimeTaskId,
    channel_waits: Vec<ChannelOwnershipToken>,
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

// Fixed fixture size for local multi-worker scheduler tests.
#[cfg(test)]
const TEST_WORKERS: usize = 5;

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

struct ClaimedTaskWork {
    poll: Option<(RuntimePollFn, *mut c_void, *const c_void)>,
    cancel: Option<(RuntimeCancelFn, *mut c_void)>,
    direct_poll: bool,
    direct_cancel: bool,
}

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
/// task can still run.
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
    #[cfg(test)]
    fn from_env_value(value: Option<&str>, default_workers: usize) -> Self {
        let requested_workers = parse_worker_count(value).unwrap_or(default_workers.max(1));

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

    pub(crate) fn with_workers(workers: usize) -> Self {
        Self {
            requested_workers: workers.max(1),
            active_workers: workers.max(1),
        }
    }
}

/// Set by the worker that is finalizing an unhandled language panic (or a poll
/// ABI violation) and will end the process: a language panic exits with
/// `PANIC_EXIT_CODE`, an ABI violation aborts. Spec §23 requires the terminal
/// publication, relationship detach and root release to happen before the
/// process ends, and that publication wakes the panicked task's awaiters — so
/// the claim path must stop handing out work first, or a sibling worker runs
/// ordinary code past `await <panicked task>` in the window before the exit.
///
/// The gate is `Runtime::fatal_panic_pending`: the siblings that must stop are
/// that runtime's workers.
///
/// Close the claim gate. Only the fatal path calls this, and that path always
/// ends the process, so the gate is never reopened.
fn begin_fatal_panic(owner: &crate::runtime::Runtime) {
    owner.fatal_panic_pending.store(true, Ordering::Release);
}

fn fatal_panic_pending(owner: &crate::runtime::Runtime) -> bool {
    owner.fatal_panic_pending.load(Ordering::Acquire)
}

/// Stop this worker for good: the thread that closed the gate is formatting an
/// unhandled-panic report and then ends the process. Waiting is the
/// point — the woken awaiters of the panicked task must never get a turn.
fn park_until_fatal_exit() -> ! {
    loop {
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Test-only: make the worker config report a single worker while the guard is
/// alive, so process-global drives dispatch to one persistent worker.
///
/// This avoids mutating the process environment for tests that need a single
/// worker regardless of the host default. A test that asserts on *which* tasks
/// one drive reaped needs one: with multiple workers, a second
/// worker can claim a task that the drive itself woke — a terminal purge
/// compensating a cancelled channel handoff, say — and complete it before the
/// run loop observes that its target is already done, so the drive returns a
/// completion count the test never asked for.
///
/// Hold this alongside `crate::gc::isolated_runtime()`, and install it before
/// `reset_global_scheduler_for_test()` so the fresh run queues are sized for one
/// worker.
#[cfg(test)]
pub type SingleWorkerForTest = crate::runtime::SingleWorkerLease<'static>;

#[cfg(test)]
pub fn single_worker_for_test() -> SingleWorkerForTest {
    crate::runtime::current().workers.single_worker()
}

pub fn runtime_worker_config() -> RuntimeWorkerConfig {
    crate::runtime::current().workers.effective()
}

pub(crate) fn environment_worker_config() -> RuntimeWorkerConfig {
    RuntimeWorkerConfig::with_workers(
        willow_abi::settings::worker_count().unwrap_or(default_worker_count()),
    )
}

/// Every poll locks its task's shard several times (claim, affinity, poll
/// boundary), so workers polling distinct tasks collide on a shard with
/// probability about workers / shards per lock. At 32 shards, 8 workers of
/// `yield_switch` spent 2-3% of their samples in contended shard locks. Whole-table walks
/// (`len`, `for_each`, `drain`) are
/// diagnostic or teardown paths; a wake batch groups by sort below this
/// count, so no per-operation path scales with it.
const TASK_TABLE_SHARDS: usize = 256;

#[path = "scheduler_task_shard.rs"]
mod task_shard;
use task_shard::TaskShard;

// Independent task locks must not invalidate a neighbour's lock/map metadata.
// Use the same conservative cache-line separation as the worker queues.
#[derive(Debug)]
#[repr(align(128))]
struct PaddedTaskShard(Mutex<TaskShard>);

// Completion writes must not invalidate the shard directory read by every
// task operation (or the unrelated blocked-syscall counter).
#[derive(Debug)]
#[repr(align(128))]
struct TerminalEpoch(AtomicU64);

/// Task records partitioned by task id.
///
/// A one-task operation takes one shard. A relationship transaction takes the
/// two participating shards in ascending index order; same-shard transactions
/// take one lock. The scheduler metadata mutex is therefore no longer the task
/// table's ownership lock.
#[derive(Debug)]
struct ShardedTaskTable {
    lock_observation: RuntimeMutexObservation,
    shards: Vec<PaddedTaskShard>,
    /// Exact O(1) summary of tasks in `BlockedSyscall`.
    ///
    /// This lives beside the sharded task states instead of in
    /// `RuntimeScheduler`, so a state transition and its accounting can be
    /// published while the same task shard is locked. Readers use it without
    /// taking the scheduler metadata mutex.
    blocked_syscall: AtomicUsize,
    /// Advances after every `Terminal` transition and every record removal.
    ///
    ///
    /// A drive whose target is still live keeps the epoch it read BEFORE its
    /// last look at the target and looks again only once the epoch moves, so
    /// the workers of a drive stop locking the target's shard on every loop
    /// iteration. A transition publishes the state, then advances the epoch;
    /// a watcher reads the epoch, then the state. Either the watcher's state
    /// read sees the transition, or its epoch read precedes the advance and
    /// the next iteration looks again.
    terminal_epoch: TerminalEpoch,
    /// Cancel requests recorded in this table; running tasks cache their
    /// cancellation against it, so other runtimes' requests never invalidate it.
    cancel_epoch: crate::task_state::CancelEpoch,
}

impl ShardedTaskTable {
    fn new() -> Self {
        Self {
            lock_observation: RuntimeMutexObservation::default(),
            shards: (0..TASK_TABLE_SHARDS)
                .map(|_| PaddedTaskShard(Mutex::new(TaskShard::default())))
                .collect(),
            blocked_syscall: AtomicUsize::new(0),
            terminal_epoch: TerminalEpoch(AtomicU64::new(0)),
            cancel_epoch: crate::task_state::CancelEpoch::new(),
        }
    }

    fn terminal_epoch(&self) -> u64 {
        self.terminal_epoch.0.load(Ordering::Acquire)
    }

    fn advance_terminal_epoch(&self) {
        self.terminal_epoch.0.fetch_add(1, Ordering::Release);
    }

    fn shard_index(&self, id: RuntimeTaskId) -> usize {
        id as usize % self.shards.len()
    }

    fn lock_shard(&self, index: usize) -> std::sync::MutexGuard<'_, TaskShard> {
        self.lock_observation.acquired();
        #[cfg(test)]
        {
            crate::test_sync::lock(&self.shards[index].0, "scheduler_task_shard")
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }
        #[cfg(not(test))]
        {
            self.shards[index]
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }
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
            // `get_disjoint_mut` keeps both records in the table while the
            // callback runs.  The previous remove/callback/reinsert sequence
            // permanently lost `second_id` if the callback unwound.
            let [first, second] = shard.get_disjoint_mut([&first_id, &second_id]);
            return mutate(first, second);
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

    #[cfg(test)]
    fn remove(&self, id: RuntimeTaskId) -> Option<RuntimeTask> {
        let removed = self.lock_shard(self.shard_index(id)).remove(&id);
        if removed.is_some() {
            self.advance_terminal_epoch();
        }
        removed
    }

    fn len(&self) -> usize {
        self.shards
            .iter()
            .enumerate()
            .map(|(index, _)| self.lock_shard(index).len())
            .sum()
    }

    fn for_each(&self, mut read: impl FnMut(&RuntimeTask)) {
        for index in 0..self.shards.len() {
            for task in self.lock_shard(index).values() {
                read(task);
            }
        }
    }

    #[cfg(test)]
    fn drain(&self) -> Vec<RuntimeTask> {
        let mut tasks = Vec::new();
        for index in 0..self.shards.len() {
            tasks.extend(self.lock_shard(index).drain().map(|(_, task)| task));
        }
        self.blocked_syscall.store(0, Ordering::Release);
        self.advance_terminal_epoch();
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
    /// Publish the terminal state and remove the record under one shard lock.
    /// Returns the record only when this call made the transition.
    fn finish_terminal_and_remove(
        &self,
        id: RuntimeTaskId,
        terminal_status: i64,
    ) -> Option<RuntimeTask> {
        self.finish_terminal_and_remove_after(id, terminal_status, || {})
    }

    /// `after_transition` runs between the state change and the frame status
    /// publication, still under the shard lock.
    fn finish_terminal_and_remove_after(
        &self,
        id: RuntimeTaskId,
        terminal_status: i64,
        after_transition: impl FnOnce(),
    ) -> Option<RuntimeTask> {
        let mut shard = self.lock_shard(self.shard_index(id));
        let task = shard.get_mut(&id)?;
        TaskExecution::publish(task);
        let before = task.state.lifecycle();
        let transitioned = task.state.finish_terminal();
        if transitioned {
            after_transition();
            // SAFETY: The frame owner retains the initialized ABI status word through this
            // atomic access.
            unsafe { crate::async_frame::frame_publish_terminal(task.frame, terminal_status) };
        }
        self.reconcile_blocked_transition(before, task.state.lifecycle());
        if !transitioned {
            return None;
        }
        let task = shard.remove(&id);
        drop(shard);
        self.advance_terminal_epoch();
        task
    }
}

fn register_waiter_sharded(
    tasks: &ShardedTaskTable,
    awaitee: RuntimeTaskId,
    waiter: RuntimeTaskId,
) -> bool {
    // A registering task is suspending at its latest published site; copy its
    // diagnostics where an awaitee's panic chain can read them from another
    // thread.
    let (name, site) = if current_task_id() == Some(waiter) {
        crate::stack_trace::async_trace_of(waiter)
    } else {
        (None, None)
    };
    tasks.with_two_mut(awaitee, waiter, |awaitee_task, waiter_task| {
        let Some(awaitee_task) = awaitee_task else {
            return false;
        };
        if awaitee_task.state.lifecycle() == TaskLifecycle::Terminal {
            return false;
        }
        let registered = awaitee_task.register_waiter(waiter);
        if let Some(task) = waiter_task {
            if registered {
                task.add_awaiting(awaitee);
            }
            if let Some(name) = name {
                task.set_name(name);
            }
            if let Some(site) = site {
                task.set_execution_site(crate::stack_trace::ExecutionSite::Published(site));
            }
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

#[path = "scheduler_terminal.rs"]
mod terminal;
#[path = "scheduler_wake_batch.rs"]
mod wake_batch;
use terminal::{TerminalRetirements, TerminalTarget};
pub(crate) use wake_batch::{WakeBatchScratch, wake_channel_owners};
/// Publish one wake against a task table + run queues, with no scheduler
/// metadata mutex involved.
///
/// The state transition, the blocked-syscall accounting and the queue
/// publication all complete while the task's shard is held,
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
    wake_task_matching_in(tasks, run_queues, id, None).unwrap_or(WakeOutcome::Terminal)
}

/// Shared state transition for single, timer-matched, and batched wakes.
/// Caller holds the task shard through queue publication and accounting.
fn wake_task_state(task: &mut RuntimeTask) -> (TaskLifecycle, WakeOutcome) {
    let before = task.state.lifecycle();
    let outcome = task.state.wake();
    if outcome != WakeOutcome::Terminal {
        task.wake_deadline = None;
    }
    (before, outcome)
}

/// Match timer identity and publish its wake under the same task shard lock.
/// A popped timer must not wake a newly re-armed sleep or a finished task.
fn wake_task_matching_in(
    tasks: &ShardedTaskTable,
    run_queues: &RunQueues,
    id: RuntimeTaskId,
    deadline: Option<Instant>,
) -> Option<WakeOutcome> {
    tasks
        .with_mut(id, |task| {
            if let Some(deadline) = deadline
                && !timer_matches_task(task, deadline)
            {
                return None;
            }
            let (before, outcome) = wake_task_state(task);
            if outcome == WakeOutcome::Enqueue {
                run_queues.push_woken_batch(std::slice::from_ref(&id));
            }
            tasks.reconcile_blocked_transition(before, task.state.lifecycle());
            Some(outcome)
        })
        .flatten()
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
/// stale — the same answer it gets once the record is gone.
fn timer_entry_is_current(tasks: &ShardedTaskTable, wake: TimerWake) -> bool {
    tasks
        .with(wake.task_id, |task| timer_matches_task(task, wake.deadline))
        .unwrap_or(false)
}

fn timer_matches_task(task: &RuntimeTask, deadline: Instant) -> bool {
    matches!(
        task.state.lifecycle(),
        TaskLifecycle::Parked | TaskLifecycle::BlockedSyscall | TaskLifecycle::Running
    ) && task.wake_deadline == Some(deadline)
}

/// Record `millis` as the running task's wake-deadline and register the timer.
///
/// The shard guard is released by `with_mut` before the timer heap is touched:
/// the lock order is timer heap -> task shard, so taking the heap while holding
/// a shard would invert it.
fn set_wake_after_millis_in(tasks: &ShardedTaskTable, timers: &TimerQueue, millis: i64) {
    let deadline = crate::timer::scheduler_now() + Duration::from_millis(millis.max(0) as u64);
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
    /// Next never-reused task id, shared with [`SchedulerComponents`] so global
    /// spawns reserve ids without `GLOBAL_SCHEDULER`.
    next_task_id: Arc<AtomicU64>,
    tasks: Arc<ShardedTaskTable>,
    /// Per-worker local run queues + a shared global queue, with work stealing. New/woken
    /// tasks go to the global queue; an idle worker
    /// drains its local queue, then the global queue, then steals from the back
    /// of another worker's local queue.
    run_queues: Arc<RunQueues>,
    /// Retired frames and external cleanups of terminal tasks, shared with
    /// [`SchedulerComponents`] so completions need no scheduler lock.
    retirements: Arc<TerminalRetirements>,
    /// Total frame roots currently owned by the scheduler: active task frames
    /// plus terminal frames still inside their grace period. Atomic and shared
    /// with [`SchedulerComponents`] because global spawns publish without
    /// `GLOBAL_SCHEDULER`; retirement still decrements under it.
    frame_roots: Arc<AtomicUsize>,
    /// Wake-deadlines, behind their own lock rather than this scheduler's
    /// metadata mutex. The run loop promotes expired timers once
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
        Self::from_components(&SchedulerComponents::with_run_queues(run_queues))
    }

    fn from_components(components: &SchedulerComponents) -> Self {
        Self {
            next_task_id: Arc::clone(&components.next_task_id),
            tasks: Arc::clone(&components.tasks),
            run_queues: Arc::clone(&components.run_queues),
            timers: Arc::clone(&components.timers),
            retirements: Arc::clone(&components.retirements),
            frame_roots: Arc::clone(&components.frame_roots),
        }
    }

    fn spawn_target(&self) -> SpawnTarget<'_> {
        SpawnTarget {
            next_task_id: &self.next_task_id,
            tasks: &self.tasks,
            run_queues: &self.run_queues,
            frame_roots: &self.frame_roots,
        }
    }

    /// Reconcile scheduler counters around one atomic task-state transition.
    ///
    /// The lifecycle itself lives in `AtomicTaskState`; this wrapper only keeps
    /// the O(1) blocked-syscall summary exact. Returns `None` for an unknown id.
    fn with_task_state<R>(
        &mut self,
        id: RuntimeTaskId,
        mutate: impl FnOnce(&mut RuntimeTask, &ShardedTaskTable) -> R,
    ) -> Option<R> {
        let tasks = Arc::clone(&self.tasks);
        tasks.with_mut(id, |task| {
            let before = task.state.lifecycle();
            let result = mutate(task, &tasks);
            let after = task.state.lifecycle();
            tasks.reconcile_blocked_transition(before, after);
            result
        })
    }

    /// Number of worker-local run queues (the configured worker count).
    pub fn worker_count(&self) -> usize {
        self.run_queues.worker_count()
    }

    /// The one place a task id enters a run queue.
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

    /// Publish a newly spawned task to the injection queue. Worker spawns
    /// share one notification at the next scheduler boundary.
    fn enqueue_ready(&mut self, id: RuntimeTaskId) {
        if self.mark_queued(id) {
            self.run_queues.push_spawned(id);
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
    /// when no worker has runnable tasks.
    pub fn pop_for_worker(&mut self, worker: usize) -> Option<RuntimeTaskId> {
        // Physical pop deliberately leaves `queued` set. `claim_for_poll`
        // consumes the queue right together with Ready -> Running/Cancelling,
        // closing the pop/wake lost-wake window.
        self.pop_queue_entry(worker)
    }

    fn pop_queue_entry(&mut self, worker: usize) -> Option<RuntimeTaskId> {
        self.run_queues.pop_for_worker(worker)
    }

    /// Best-effort removal of the physical queue entry owned by `id` without changing its
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
    /// [`global_run_queues`] without holding the scheduler metadata mutex.
    fn claim_popped(&mut self, id: RuntimeTaskId) -> Option<RuntimeTaskId> {
        let (outcome, has_cleanup, panic_context) = self.tasks.with_mut(id, |task| {
            let outcome = task.claim_for_poll();
            if outcome == ClaimOutcome::Poll {
                task.yield_requested = false;
                task.started = true;
            }
            let has_cleanup =
                outcome == ClaimOutcome::Cancel && task.cancel.is_some() && !task.frame.is_null();
            if outcome == ClaimOutcome::Poll || (outcome == ClaimOutcome::Cancel && has_cleanup) {
                TaskExecution::claim(task, &self.tasks.cancel_epoch);
            }
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

        // Cooperative cancellation boundary: the atomic claim
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
    /// Cancelling. Consumes the entry so it runs once.
    pub fn take_cancel_work(
        &mut self,
        id: RuntimeTaskId,
    ) -> Option<(RuntimeCancelFn, *mut c_void)> {
        self.tasks
            .with_mut(id, |task| {
                #[cfg(any(
                    all(
                        target_os = "linux",
                        target_env = "gnu",
                        any(target_arch = "x86_64", target_arch = "aarch64")
                    ),
                    all(
                        target_os = "macos",
                        any(target_arch = "x86_64", target_arch = "aarch64")
                    ),
                    all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
                ))]
                if CURRENT_EXECUTION.with(|slot| {
                    let current = slot.borrow();
                    if current.owns(task) {
                        current
                            .stack
                            .as_ref()
                            .is_some_and(|stack| stack.is_suspended())
                    } else {
                        task.native_stack
                            .as_ref()
                            .is_some_and(|stack| stack.is_suspended())
                    }
                }) {
                    return None;
                }
                if task.state.lifecycle() != TaskLifecycle::Cancelling {
                    return None;
                }
                let cancel = task.cancel.take()?;
                Some((cancel, task.frame))
            })
            .flatten()
    }

    #[cfg(test)]
    /// True while any task is parked in `BlockedSyscall` — a blocking-pool
    /// job will wake it, so the scheduler must NOT declare idle/stop on the
    /// strength of empty queues alone.
    ///
    /// O(1): the idle path asks this on every park cycle, so it reads the
    /// maintained counter instead of scanning the task table.
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
        let mut blocked = 0;
        self.for_each_task(|task| {
            blocked += usize::from(task.runtime_state() == Some(RuntimeTaskState::BlockedSyscall));
        });
        self.blocked_syscall_count() == blocked
    }

    /// The single terminal transition for Completed/Cancelled/Panicked.
    ///
    ///
    /// Under the scheduler lock this publishes the frame status, detaches both
    /// directions of await relationships, wakes awaiters, captures external
    /// wait registrations, retires the frame root, and removes the heavy
    /// [`RuntimeTask`]. Channel/netpoll cleanup and actual frame unrooting are
    /// deliberately left to the two lock-free phases.
    fn finish_terminal(&mut self, id: RuntimeTaskId, state: RuntimeTaskState) -> bool {
        self.prepare_placeholder_terminal_owner(id);
        TerminalTarget {
            tasks: &self.tasks,
            run_queues: &self.run_queues,
            retirements: &self.retirements,
        }
        .finish(id, state)
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
        let _ = self.with_task_state(id, |task, _| {
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

    #[cfg(test)]
    fn take_pending_terminal_cleanups(&mut self) -> Vec<TerminalCleanup> {
        self.retirements.take_cleanups()
    }

    #[cfg(test)]
    fn take_quiescent_frame_unroots(&mut self, bound: u64) -> Vec<RetiredFrame> {
        self.retirements
            .take_quiescent_frames(bound, &self.frame_roots)
    }

    pub fn metadata_snapshot(&self) -> SchedulerMetadataSnapshot {
        SchedulerMetadataSnapshot {
            heavy_tasks: self.tasks.len(),
            queue_entries: self.ready_total(),
            pending_cleanups: self.retirements.pending_cleanups(),
            frame_roots: self.frame_roots.load(Ordering::Relaxed),
            blocked_syscalls: self.blocked_syscall_count(),
        }
    }

    /// True if `id` is queued anywhere (any local queue or the global queue).
    /// O(1): the flag is the invariant, not a scan.
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
        let mut matches = true;
        self.for_each_task(|task| {
            matches &= task.state.load().is_queued() == seen.contains_key(&task.id);
        });
        matches
    }

    /// Total runnable tasks across all queues.
    fn ready_total(&self) -> usize {
        self.run_queues.len()
    }

    /// Reserve a never-reused task id without publishing a task record.
    fn reserve_task_id(&self) -> RuntimeTaskId {
        self.spawn_target().reserve_task_id()
    }

    pub fn spawn_placeholder(&mut self) -> RuntimeTaskId {
        let id = self.reserve_task_id();
        let task = RuntimeTask::new(id);
        self.tasks.insert(id, task);
        self.enqueue_ready(id);
        id
    }

    /// Test-only: run `f` against one task record under its shard lock, for
    /// fixtures that must drive the state word directly.
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
        let id = self.reserve_task_id();
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
    ///
    /// # Safety
    /// The caller retains the frame until retirement. Each callback must accept that frame,
    /// including null for frameless tasks. A non-null frame must implement the async-frame ABI.
    pub unsafe fn spawn_task_initialized(
        &mut self,
        poll: RuntimePollFn,
        frame: *mut c_void,
        cancel: Option<RuntimeCancelFn>,
        initialize: impl FnOnce(RuntimeTaskId),
    ) -> RuntimeTaskId {
        let target = self.spawn_target();
        let id = target.reserve_task_id();
        initialize(id);
        target.publish(id, poll, frame, cancel, false);
        id
    }

    ///
    /// # Safety
    /// The caller retains the frame until retirement. Each callback must accept that frame,
    /// including null for frameless tasks. A non-null frame must implement the async-frame ABI.
    pub unsafe fn spawn_task(&mut self, poll: RuntimePollFn, frame: *mut c_void) -> RuntimeTaskId {
        // SAFETY: The supplied callback matches this frame layout; the frame storage remains
        // live across scheduling and callback execution.
        unsafe { self.spawn_task_initialized(poll, frame, None, |_| {}) }
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
        let context = self
            .with_task_state(id, |task, tasks| {
                match task.state.lifecycle() {
                    TaskLifecycle::Ready => {
                        if !task.state.load().is_queued() {
                            let _ = task.state.claim_queue_slot();
                        }
                    }
                    TaskLifecycle::Parked | TaskLifecycle::BlockedSyscall => {
                        let _ = task.state.wake();
                    }
                    TaskLifecycle::Running | TaskLifecycle::Cancelling => {
                        if !CURRENT_EXECUTION.with(|slot| slot.borrow().owns(task)) {
                            TaskExecution::claim(task, &tasks.cancel_epoch);
                        }
                        return Some(task.panic_context());
                    }
                    TaskLifecycle::Terminal => return None,
                }
                task.yield_requested = false;
                task.started = true;
                let claimed = matches!(
                    task.state.claim_for_poll(),
                    ClaimOutcome::Poll | ClaimOutcome::Cancel
                );
                if claimed {
                    TaskExecution::claim(task, &tasks.cancel_epoch);
                    Some(task.panic_context())
                } else {
                    None
                }
            })
            .flatten();
        if let Some(context) = context {
            // Resolve through this scheduler while its task shard is held.
            // Independent schedulers may have the same numeric task id.
            set_current_task_context(id, context);
        }
    }

    /// Clear the "currently running" marker once a poll returns. Guards
    /// `willow_sched_sleep` / `willow_sched_await` against attaching a deadline
    /// or waiter to a STALE task when called outside of a poll.
    pub fn clear_running(&mut self) {
        set_current_task(None);
    }

    /// Attach a wake-deadline to the currently-running task (called via
    /// `willow_sched_sleep` from a poll fn before it returns Pending). The
    /// timer-aware run loop wakes the task once the deadline passes.
    pub fn set_running_wake_after_millis(&mut self, millis: i64) {
        set_wake_after_millis_in(&self.tasks, &self.timers, millis);
    }

    #[cfg(test)]
    /// The parked task with the earliest wake-deadline, if any. Backed by a
    /// min-heap so idle scheduling does not scan every parked task.
    fn next_timer_deadline(&self) -> Option<(RuntimeTaskId, Instant)> {
        let tasks = &*self.tasks;
        self.timers
            .next_deadline(|wake| timer_entry_is_current(tasks, wake))
    }

    #[cfg(test)]
    /// Move every due timer directly from the timer heap to the ready queue.
    ///
    /// TimerQueue keeps popped batches visible to idle detection until every
    /// wake finishes, allowing callbacks to run outside the timer heap lock.
    fn wake_due_timers(&self, now: Instant) -> usize {
        let tasks = &*self.tasks;
        let run_queues = &*self.run_queues;
        self.timers.wake_due(
            now,
            |wake| timer_entry_is_current(tasks, wake),
            |entry| {
                wake_task_matching_in(tasks, run_queues, entry.task_id, Some(entry.deadline));
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
        // linear scan of the waiter list.
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

    /// Inspect a borrowed task under its shard lock, returning only the data
    /// the caller needs. The callback must not re-enter the scheduler or task
    /// table: doing so can deadlock on this lock.
    pub fn with_task<R>(
        &self,
        id: RuntimeTaskId,
        read: impl FnOnce(&RuntimeTask) -> R,
    ) -> Option<R> {
        self.tasks.with(id, read)
    }

    /// # Safety
    /// Preserve the task identity, frame lifetime and root ownership. Its poll and cancel
    /// callbacks must continue to accept that frame until retirement. Do not move a task
    /// record or transfer its execution resources out of the scheduler.
    pub unsafe fn with_task_mut<R>(
        &self,
        id: RuntimeTaskId,
        mutate: impl FnOnce(&mut RuntimeTask) -> R,
    ) -> Option<R> {
        self.tasks.with_mut(id, mutate)
    }

    /// Inspect each task once, holding one shard lock at a time. This is not
    /// an atomic snapshot of the table. Callbacks must not re-enter the
    /// scheduler or task table, and task references cannot escape the callback.
    pub fn for_each_task(&self, read: impl FnMut(&RuntimeTask)) {
        self.tasks.for_each(read);
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
        self.with_task_state(id, |task, _| {
            TaskExecution::publish(task);
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
            TaskExecution::publish(task);
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
            TaskExecution::publish(task);
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

// The scheduler exposes no `GcTrace` impl. Task-owned GC values
// live in the async frame, which stays reachable through the runtime root
// registry while `frame_rooted` holds; collection scans that registry, never
// the task table. The previous impl walked a per-task root set that nothing
// ever populated.

// ─── Process-global cooperative scheduler ─────
//
// A shared run queue that drives compiler-generated cooperative tasks. Each task
// owns a heap async frame; the frame is registered as a GC runtime root while
// the task is pending/running, so a parked/ready task's live values survive
// collection even though no native stack frame holds them (spec §8.2 / §9).

/// The run queues, task table and timers shared within one scheduler service.
struct SchedulerComponents {
    affinities: crate::worker_affinity::Registry,
    run_queues: Arc<RunQueues>,
    tasks: Arc<ShardedTaskTable>,
    timers: Arc<TimerQueue>,
    retirements: Arc<TerminalRetirements>,
    next_task_id: Arc<AtomicU64>,
    frame_roots: Arc<AtomicUsize>,
    claims: Arc<AccountingWords>,
}

impl SchedulerComponents {
    /// Task ids start at 1 (id 0 is the `willow_sched_current_task()` "no
    /// running task" sentinel).
    fn with_run_queues(run_queues: Arc<RunQueues>) -> Self {
        Self::with_domain(
            run_queues,
            Arc::clone(&crate::runtime::current().frame_reclaim),
            crate::worker_affinity::Registry::Default,
        )
    }

    fn with_domain(
        run_queues: Arc<RunQueues>,
        reclaim: Arc<crate::frame_reclaim::Domain>,
        affinities: crate::worker_affinity::Registry,
    ) -> Self {
        Self {
            affinities,
            run_queues,
            tasks: Arc::new(ShardedTaskTable::new()),
            timers: Arc::new(TimerQueue::new()),
            retirements: Arc::new(TerminalRetirements::new(reclaim)),
            next_task_id: Arc::new(AtomicU64::new(1)),
            frame_roots: Arc::new(AtomicUsize::new(0)),
            claims: Arc::new(AccountingWords::new()),
        }
    }

    fn spawn_target(&self) -> SpawnTarget<'_> {
        SpawnTarget {
            next_task_id: &self.next_task_id,
            tasks: &self.tasks,
            run_queues: &self.run_queues,
            frame_roots: &self.frame_roots,
        }
    }

    fn terminal_target(&self) -> TerminalTarget<'_> {
        TerminalTarget {
            tasks: &self.tasks,
            run_queues: &self.run_queues,
            retirements: &self.retirements,
        }
    }
}

/// The scheduler state a spawn touches. Every field is internally
/// synchronized, so global spawns reserve and publish without
/// `GLOBAL_SCHEDULER`; that lock serialized every spawning worker twice per
/// spawn.
struct SpawnTarget<'a> {
    next_task_id: &'a AtomicU64,
    tasks: &'a ShardedTaskTable,
    run_queues: &'a RunQueues,
    frame_roots: &'a AtomicUsize,
}

impl SpawnTarget<'_> {
    /// Reserve a never-reused task id without publishing a task record.
    ///
    /// Global native-task spawning initializes frame/native state between this
    /// step and [`Self::publish`]. An id gap after an initializer panic is
    /// intentional: reusing it would create an ABA risk for native
    /// registrations that observed the reserved id.
    fn reserve_task_id(&self) -> RuntimeTaskId {
        // `with_global` used to provide this guard (see `NoPreemptGuard`).
        let _no_preempt = crate::preempt::NoPreemptGuard::enter();
        self.next_task_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .expect("runtime task id exhausted")
    }

    /// Publish one fully initialized task. The record enters the table already
    /// holding its queue slot and queue insertion is the final step, so no
    /// worker can claim it before poll/cancel/frame fields exist and no waker
    /// can enqueue it a second time.
    fn publish(
        &self,
        id: RuntimeTaskId,
        poll: RuntimePollFn,
        frame: *mut c_void,
        cancel: Option<RuntimeCancelFn>,
        cooperative_poll: bool,
    ) {
        let _no_preempt = crate::preempt::NoPreemptGuard::enter();
        debug_assert!(
            self.tasks.with(id, |_| ()).is_none(),
            "reserved task id was already published"
        );
        let mut task = RuntimeTask::new(id);
        task.poll = Some(poll);
        task.cooperative_poll = cooperative_poll;
        // Generated constructors run inside the caller's debug call frame.
        // The later explicit setter can race the first poll on another worker,
        // so install the location before the task table or queue can expose it.
        if cooperative_poll && let Some((file, line)) = crate::stack_trace::current_call_site() {
            task.set_spawn_site(file, line);
        }
        task.cancel = cancel;
        task.frame = frame;
        task.frame_rooted = !frame.is_null();
        if task.frame_rooted {
            self.frame_roots.fetch_add(1, Ordering::Relaxed);
            crate::frame_lifetime::record(
                crate::frame_lifetime::FrameEventKind::SchedulerRootAdded,
                frame as usize,
                crate::frame_reclaim::current_epoch(),
                0,
            );
        }
        let claimed = task.state.claim_queue_slot();
        debug_assert!(claimed, "a fresh task must own its queue slot");
        self.tasks.insert(id, task);
        self.run_queues.push_spawned(id);
    }
}

/// Owns scheduler storage and its metadata facade. Readers borrow the service,
/// not a process-lifetime leaked allocation. The hot lookup is one acquire
/// load, without a mutex or shared reference-count write.
///
/// Compatibility resets retain prior generations until this service drops:
/// an outstanding reader may still borrow one. Production creates one
/// generation. A private instance releases all generations at teardown.
pub(crate) struct SchedulerServices {
    scheduler: Mutex<RuntimeScheduler>,
    current: AtomicPtr<SchedulerComponents>,
    _retained_generations: Mutex<Vec<Arc<SchedulerComponents>>>,
}

impl SchedulerServices {
    #[cfg(test)]
    pub(crate) fn new(worker_count: usize) -> Self {
        Self::new_in(
            worker_count,
            Arc::clone(&crate::runtime::current().frame_reclaim),
            Arc::clone(&crate::runtime::current().parking),
            crate::worker_affinity::Registry::Default,
        )
    }

    pub(crate) fn new_in(
        worker_count: usize,
        reclaim: Arc<crate::frame_reclaim::Domain>,
        parking: Arc<parking::Parking>,
        affinities: crate::worker_affinity::Registry,
    ) -> Self {
        let components = Arc::new(SchedulerComponents::with_domain(
            Arc::new(RunQueues::with_parking(worker_count, parking)),
            reclaim,
            affinities,
        ));
        Self {
            scheduler: Mutex::new(RuntimeScheduler::from_components(&components)),
            current: AtomicPtr::new(Arc::as_ptr(&components).cast_mut()),
            _retained_generations: Mutex::new(vec![components]),
        }
    }

    fn components(&self) -> &SchedulerComponents {
        // SAFETY: every published pointer is retained in `_retained_generations` until
        // self drops. Moving the Vec or its Arcs never moves their allocations.
        // The return lifetime is bounded by self, excluding owner destruction.
        unsafe { &*self.current.load(Ordering::Acquire) }
    }

    fn with_scheduler<'a, R>(
        owner: impl FnOnce() -> &'a Self,
        f: impl FnOnce(&mut RuntimeScheduler) -> R,
    ) -> R {
        // Lazy initialization, as well as the metadata lock, must happen with
        // preemption disabled: reentry during initialization could deadlock.
        let _no_preempt = crate::preempt::NoPreemptGuard::enter();
        let mut scheduler = owner().scheduler.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut scheduler)
    }

    #[cfg(test)]
    fn replace_components(&self, worker_count: usize) -> &SchedulerComponents {
        let fresh = Arc::new(SchedulerComponents::with_domain(
            Arc::new(RunQueues::with_parking(
                worker_count,
                Arc::clone(&self.components().run_queues.parking),
            )),
            Arc::clone(&self.components().retirements.reclaim),
            self.components().affinities.clone(),
        ));
        let pointer = Arc::as_ptr(&fresh).cast_mut();
        let mut generations = self
            ._retained_generations
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        generations.push(fresh);
        self.current.store(pointer, Ordering::Release);
        // SAFETY: the owning Arc was retained before publication, and will not
        // be removed while any borrow of this service can exist.
        unsafe { &*pointer }
    }
}

fn global_components() -> &'static SchedulerComponents {
    crate::runtime::current().scheduler().components()
}

#[cfg(test)]
fn replace_global_components(worker_count: usize) -> &'static SchedulerComponents {
    crate::runtime::current()
        .scheduler()
        .replace_components(worker_count)
}

pub(crate) fn run_queue_global_pushes() -> u64 {
    global_run_queues()
        .metrics
        .global_pushes
        .load(Ordering::Relaxed)
}

pub(crate) fn read_run_queue_metric(
    read: impl Fn(&crate::observability::RunQueueMetrics) -> u64,
) -> u64 {
    let queues = global_run_queues();
    queues
        .worker_metrics
        .iter()
        .fold(read(&queues.metrics), |sum, metrics| {
            sum.wrapping_add(read(metrics))
        })
}

pub(crate) fn run_queue_metrics_snapshot() -> crate::observability::RunQueueMetricsSnapshot {
    global_run_queues().metrics_snapshot()
}

fn global_run_queues() -> &'static Arc<RunQueues> {
    &global_components().run_queues
}

fn global_task_table() -> &'static Arc<ShardedTaskTable> {
    &global_components().tasks
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
/// early and a `select` giving up.
///
/// Counted per scheduler rather than per `ParallelRunState` so nested drives
/// and foreign driver threads using that scheduler observe each other's claims.
/// Independent schedulers never participate in each other's stop decisions.
///
/// Each shard's low [`CLAIM_COUNT_BITS`] bits count claims in flight. The high
/// bits are an epoch advanced by popped claims and active/paused transfers. A
/// stop decision reads every word before and after its snapshot and accepts
/// only unchanged words with zero counts, so no claim can pop, resolve and
/// republish work between its reads unseen. All claim-word
/// accesses are `SeqCst`: they form one half of the store/load handshake with
/// [`ParallelRunState::stop_intent`] that lets claims skip `claim_gate`.
///
/// Claims and poll accounting use fixed, cache-separated thread shards. Each
/// hot update is O(1); only cold idle/stop snapshots visit the fixed array.
/// Collisions after 64 registered threads preserve correctness (atomic updates).
const ACCOUNTING_SHARDS: usize = 64;
static NEXT_ACCOUNTING_SHARD: AtomicUsize = AtomicUsize::new(0);
thread_local! {
    static ACCOUNTING_SHARD: usize =
        NEXT_ACCOUNTING_SHARD.fetch_add(1, Ordering::Relaxed) % ACCOUNTING_SHARDS;
}

#[repr(align(128))]
#[derive(Debug)]
struct AccountingWord(AtomicU64);

#[derive(Debug)]
struct AccountingWords([AccountingWord; ACCOUNTING_SHARDS]);

impl Default for AccountingWords {
    fn default() -> Self {
        Self::new()
    }
}

impl AccountingWords {
    const fn new() -> Self {
        Self([const { AccountingWord(AtomicU64::new(0)) }; ACCOUNTING_SHARDS])
    }

    fn local(&self) -> &AtomicU64 {
        &self.0[ACCOUNTING_SHARD.with(|index| *index)].0
    }

    fn snapshot(&self) -> [u64; ACCOUNTING_SHARDS] {
        std::array::from_fn(|index| self.0[index].0.load(Ordering::SeqCst))
    }
}

fn global_claim_words() -> &'static Arc<AccountingWords> {
    &global_components().claims
}

const CLAIM_COUNT_BITS: u32 = 32;
const CLAIM_COUNT_MASK: u64 = (1 << CLAIM_COUNT_BITS) - 1;
/// Leave the pop→claim window and advance the epoch in one step.
const CLAIM_RESOLVE: u64 = (1 << CLAIM_COUNT_BITS) - 1;

#[cfg(test)]
fn claim_word() -> u64 {
    global_claim_words()
        .snapshot()
        .into_iter()
        .fold(0, u64::wrapping_add)
}

/// RAII marker for the pop→claim window. Dropped only after the claim has
/// resolved: either `active_polls` has been incremented, or the id has been
/// pushed back / dropped, so idleness is never observable between the two
/// states.
struct ClaimInFlight<'a> {
    word: &'a AtomicU64,
    advance_epoch: bool,
}

#[cfg(test)]
impl ClaimInFlight<'static> {
    fn enter() -> Self {
        Self::enter_in(global_claim_words())
    }
}

impl<'a> ClaimInFlight<'a> {
    fn enter_in(claims: &'a AccountingWords) -> Self {
        let word = claims.local();
        word.fetch_add(1, Ordering::SeqCst);
        Self {
            word,
            advance_epoch: false,
        }
    }

    /// A paused/active transfer can invalidate a liveness scan without
    /// popping a task. Publish the in-flight marker before changing the poll
    /// word and advance the epoch after it, just like a resolved claim.
    fn poll_transition(claims: &'a AccountingWords) -> Self {
        let mut transition = Self::enter_in(claims);
        transition.advance_epoch = true;
        transition
    }

    /// The pop returned an id. Its resolution may publish work (a requeue, a
    /// reroute, a wake from cancellation), so leaving must advance the epoch.
    /// A claim that popped nothing changed nothing and leaves without it.
    fn popped(&mut self) {
        self.advance_epoch = true;
    }
}

impl Drop for ClaimInFlight<'_> {
    fn drop(&mut self) {
        let delta = if self.advance_epoch {
            CLAIM_RESOLVE
        } else {
            u64::MAX
        };
        // `u64::MAX` wraps to a plain decrement of the count.
        let previous = self.word.fetch_add(delta, Ordering::SeqCst);
        debug_assert!(previous & CLAIM_COUNT_MASK > 0, "claim-in-flight underflow");
    }
}

#[cfg(test)]
/// True while any thread holds a popped-but-unclaimed task. Idle detection must
/// treat this as pending work. A concurrent resolution may conservatively
/// report work too; a subsequent quiet scan returns false.
fn claims_in_flight() -> bool {
    global_claim_words().in_flight()
}

impl AccountingWords {
    fn in_flight(&self) -> bool {
        self.changed_or_in_flight(&self.snapshot())
    }

    fn changed_or_in_flight(&self, before: &[u64; ACCOUNTING_SHARDS]) -> bool {
        before.iter().any(|word| word & CLAIM_COUNT_MASK > 0) || self.snapshot() != *before
    }
}

/// Wake-deadlines, shared with the scheduler instance the same way the run
/// queues and the task table are. Timer work reaches this through
/// its own lock, so the run loop's per-iteration timer promotion no longer
/// serializes every worker on `GLOBAL_SCHEDULER`.
fn global_timers() -> &'static Arc<TimerQueue> {
    &global_components().timers
}

fn with_global<R>(f: impl FnOnce(&mut RuntimeScheduler) -> R) -> R {
    SchedulerServices::with_scheduler(|| crate::runtime::current().scheduler(), f)
}

/// Hot wake path: state, blocked-syscall accounting, and queue publication are
/// completed while one task shard is locked.
fn wake_global_task(id: RuntimeTaskId) -> bool {
    wake_task_in(global_task_table(), global_run_queues(), id)
}

fn wake_global_task_outcome(id: RuntimeTaskId) -> WakeOutcome {
    wake_task_outcome_in(global_task_table(), global_run_queues(), id)
}

/// Register the running task's sleep deadline without the scheduler metadata
/// mutex.
fn set_global_wake_after_millis(millis: i64) {
    set_wake_after_millis_in(global_task_table(), global_timers(), millis);
}

/// The earliest live wake-deadline, for the idle path's "how long may I sleep?"
/// decision. Always reads the heap under its own lock — never the lock-free
/// hint, which is allowed to be transiently stale-empty.
fn global_next_timer_deadline() -> Option<(RuntimeTaskId, Instant)> {
    next_timer_deadline_in(global_components())
}

fn next_timer_deadline_in(components: &SchedulerComponents) -> Option<(RuntimeTaskId, Instant)> {
    components
        .timers
        .next_deadline(|wake| timer_entry_is_current(&components.tasks, wake))
}

/// Promote every timer due at `now` onto a run queue.
///
/// The lock-free hint short-circuits the overwhelmingly common case — no task
/// has a deadline, or the earliest one is still in the future — so the run
/// loop's per-iteration timer check costs one atomic load instead of a
/// process-global mutex acquisition. Missing a just-pushed timer
/// here is harmless: the idle path re-reads the heap under its lock before it
/// can conclude that the scheduler has nothing to do.
fn wake_due_timers_now_in(components: &SchedulerComponents) -> usize {
    if components.timers.maybe_armed() {
        wake_due_timers_in(components, crate::timer::scheduler_now())
    } else {
        0
    }
}

#[cfg(test)]
fn wake_global_due_timers(now: Instant) -> usize {
    wake_due_timers_in(global_components(), now)
}

fn wake_due_timers_in(components: &SchedulerComponents, now: Instant) -> usize {
    let timers = &components.timers;
    if !timers.maybe_due(now) {
        return 0;
    }
    let tasks = &components.tasks;
    let run_queues = &components.run_queues;
    let woken = timers.wake_due(
        now,
        |wake| timer_entry_is_current(tasks, wake),
        |entry| {
            wake_task_matching_in(tasks, run_queues, entry.task_id, Some(entry.deadline));
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
    Preempted,
    BlockedSyscall,
}

/// Return frame ownership after a poll without taking the scheduler metadata
/// mutex. Only the blocked-syscall aggregate needs reconciliation afterwards;
/// ordinary Pending/yield/preempt transitions are task-shard + run-queue
/// operations.
#[cfg(test)]
fn finish_global_poll_boundary(id: RuntimeTaskId, boundary: GlobalPollBoundary) {
    finish_poll_boundary_in(global_components(), id, boundary);
}

fn finish_poll_boundary_in(
    components: &SchedulerComponents,
    id: RuntimeTaskId,
    boundary: GlobalPollBoundary,
) {
    let tasks = &components.tasks;
    let run_queues = &components.run_queues;
    tasks.with_mut(id, |task| {
        TaskExecution::publish(task);
        let before = task.state.lifecycle();
        let yielded = task.yield_requested;
        task.yield_requested = false;
        // Source suspension permits transfer of the compiler-checked Send
        // frame. Quantum expiration does not transfer source-local ownership.
        // Install/release the lease under the existing boundary shard lock,
        // before making the task stealable again.
        match boundary {
            GlobalPollBoundary::Preempted => {
                task.worker_affinity
                    .get_or_insert_with(|| components.affinities.acquire(current_worker()));
            }
            GlobalPollBoundary::Pending | GlobalPollBoundary::Runnable => {
                task.worker_affinity = None
            }
            GlobalPollBoundary::BlockedSyscall => {}
        }
        let outcome = match boundary {
            GlobalPollBoundary::Pending if yielded => task.state.requeue_after_poll(),
            GlobalPollBoundary::Pending => task.state.park_after_poll(),
            GlobalPollBoundary::Runnable | GlobalPollBoundary::Preempted => {
                task.state.requeue_after_poll()
            }
            GlobalPollBoundary::BlockedSyscall => task.state.block_on_syscall(),
        };
        #[cfg(feature = "borrow-migration-probe")]
        if task.state.lifecycle() == TaskLifecycle::Parked {
            crate::borrow_migration_probe::parked(task.frame as usize);
        }
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

/// Snapshot dispatch metadata while the claim owns the task shard. Once
/// claimed, cancellation requests cannot move a running poll to Cancelling;
/// they are reconciled at its boundary. Native stack ownership is likewise
/// exclusive to the worker that claims the task.
fn claimed_task_work(
    task: &mut RuntimeTask,
    cancel_epoch: &crate::task_state::CancelEpoch,
) -> ClaimedTaskWork {
    let stack_state = (false, false);
    #[cfg(any(
        all(
            target_os = "linux",
            target_env = "gnu",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "macos",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
    ))]
    let stack_state = task.native_stack.as_ref().map_or(stack_state, |stack| {
        (stack.is_suspended(), stack.is_cleanup())
    });
    let (suspended, cleanup) = stack_state;
    let work = ClaimedTaskWork {
        poll: task
            .poll
            .map(|poll| (poll, task.frame, task.preempt_flag_ptr())),
        cancel: if !suspended && task.state.lifecycle() == TaskLifecycle::Cancelling {
            task.cancel.take().map(|cancel| (cancel, task.frame))
        } else {
            None
        },
        direct_poll: task.cooperative_poll && !cleanup,
        direct_cancel: task.cooperative_poll && !task.native_cancel_cleanup,
    };
    TaskExecution::claim(task, cancel_epoch);
    work
}

/// Exclusive execution state moved out of the task shard while a worker owns
/// the poll. No pointer into the movable task table and no TLS borrow survives
/// a callback. A nested drive saves this entire context and restores it on exit.
#[derive(Default)]
struct TaskExecution {
    owner: Option<RuntimeTaskId>,
    /// Address identity only: the panic context's allocation stays fixed while
    /// its task lives. Numeric ids alone can alias between scheduler instances.
    owner_identity: usize,
    /// Epoch of the claimed task's table. The table outlives every poll it
    /// claims; nested drives save and restore it with the rest of this state.
    cancel_epoch_source: *const crate::task_state::CancelEpoch,
    cancel_epoch: u64,
    cancelled: bool,
    #[cfg(any(
        all(
            target_os = "linux",
            target_env = "gnu",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "macos",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
    ))]
    stack: Option<Box<crate::native_stack::NativeStack>>,
}

impl TaskExecution {
    const fn new() -> Self {
        Self {
            owner: None,
            owner_identity: 0,
            cancel_epoch_source: std::ptr::null(),
            cancel_epoch: 0,
            cancelled: false,
            #[cfg(any(
                all(
                    target_os = "linux",
                    target_env = "gnu",
                    any(target_arch = "x86_64", target_arch = "aarch64")
                ),
                all(
                    target_os = "macos",
                    any(target_arch = "x86_64", target_arch = "aarch64")
                ),
                all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
            ))]
            stack: None,
        }
    }

    fn owns(&self, task: &RuntimeTask) -> bool {
        self.owner == Some(task.id) && self.owner_identity == task.preempt_flag_ptr() as usize
    }

    fn claim(task: &mut RuntimeTask, cancel_epoch: &crate::task_state::CancelEpoch) {
        CURRENT_EXECUTION.with(|slot| {
            let mut current = slot.borrow_mut();
            #[cfg(any(
                all(
                    target_os = "linux",
                    target_env = "gnu",
                    any(target_arch = "x86_64", target_arch = "aarch64")
                ),
                all(
                    target_os = "macos",
                    any(target_arch = "x86_64", target_arch = "aarch64")
                ),
                all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
            ))]
            {
                assert!(
                    current.stack.is_none(),
                    "previous task still owns its stack"
                );
                current.stack = task.native_stack.take();
            }
            current.owner = Some(task.id);
            current.owner_identity = task.preempt_flag_ptr() as usize;
            // Observe the epoch before the state, so a later request always
            // invalidates this snapshot before a helper trusts it.
            current.cancel_epoch_source = cancel_epoch;
            current.cancel_epoch = cancel_epoch.load();
            let state = task.state.load();
            current.cancelled =
                state.cancel_requested() || state.lifecycle() == TaskLifecycle::Cancelling;
        });
    }

    /// Called under the task shard, before publishing a lifecycle transition
    /// that allows another worker to claim the task or retire its frame.
    fn publish(task: &mut RuntimeTask) {
        CURRENT_EXECUTION.with(|slot| {
            let mut current = slot.borrow_mut();
            if !current.owns(task) {
                return;
            }
            #[cfg(any(
                all(
                    target_os = "linux",
                    target_env = "gnu",
                    any(target_arch = "x86_64", target_arch = "aarch64")
                ),
                all(
                    target_os = "macos",
                    any(target_arch = "x86_64", target_arch = "aarch64")
                ),
                all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
            ))]
            {
                assert!(task.native_stack.is_none(), "task stack has two owners");
                task.native_stack = current.stack.take();
            }
            current.owner = None;
            current.owner_identity = 0;
        });
    }
}

thread_local! {
    static CURRENT_EXECUTION: RefCell<TaskExecution> = const { RefCell::new(TaskExecution::new()) };
}

thread_local! {
    /// The task currently being polled on this OS thread. Runtime primitives
    /// such as sleep/channel/await use it to attach wait state to the right task.
    static CURRENT_TASK: Cell<Option<RuntimeTaskId>> = const { Cell::new(None) };
    /// Worker-local index used for local-queue affinity and nested scheduler
    /// drives from inside a poll.
    static CURRENT_WORKER: Cell<usize> = const { Cell::new(0) };
    static SPAWN_NOTIFICATION_PENDING: Cell<bool> = const { Cell::new(false) };
    static LOCAL_WAKE_NOTIFICATION_PENDING: Cell<bool> = const { Cell::new(false) };
    /// The active parallel run, if this thread is inside a worker pool.
    static CURRENT_RUN_STATE: RefCell<Option<Arc<ParallelRunState>>> = const { RefCell::new(None) };
}

fn current_task_id() -> Option<RuntimeTaskId> {
    CURRENT_TASK.with(Cell::get)
}

/// Record the task running on this thread. Its young allocations from here
/// on belong to it: a task resumed on another worker continues on that
/// worker's region (its earlier young objects stay its own), and a finished,
/// cancelled or panicked task hands the cursor back to owner zero.
#[inline]
fn install_current_task(id: Option<RuntimeTaskId>) {
    CURRENT_TASK.with(|current| current.set(id));
    crate::gc::switch_task_owner(id.map_or(0, crate::gc::task_owner));
}

/// Claim-time cancellation snapshot of the running task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CancelSnapshot {
    /// Epoch of the task's own table; valid while the task's poll runs.
    pub(crate) source: *const crate::task_state::CancelEpoch,
    pub(crate) epoch: u64,
    pub(crate) cancelled: bool,
}

/// Cancel epoch of the task table of the runtime this thread serves.
pub(crate) fn current_cancel_epoch() -> *const crate::task_state::CancelEpoch {
    &global_task_table().cancel_epoch
}

/// Claim-time snapshot; callers compare the epoch before reusing it. Stack
/// leases acquired repeatedly in one poll need no repeated task-table lookup.
pub(crate) fn current_cancel_snapshot() -> Option<CancelSnapshot> {
    CURRENT_EXECUTION.with(|slot| {
        let current = slot.borrow();
        (current.owner.is_some() && current.owner == current_task_id()).then_some(CancelSnapshot {
            source: current.cancel_epoch_source,
            epoch: current.cancel_epoch,
            cancelled: current.cancelled,
        })
    })
}

/// `willow_sched_is_cancelled(task) != 0` for the running `task`, without a
/// task-table lookup while the cancel epoch has not moved since its claim
/// snapshot. A moved epoch refreshes the snapshot with one lookup; the epoch is
/// loaded first, so the refreshed answer covers every request it counts.
pub(crate) fn current_task_cancel_pending(task: u64) -> bool {
    let cached = CURRENT_EXECUTION.with(|slot| {
        let current = slot.borrow();
        // Ids alone can alias between runtimes: trust only this runtime's table.
        if current.owner != Some(task) || current.cancel_epoch_source != current_cancel_epoch() {
            return Err(None);
        }
        // SAFETY: the epoch source is the claiming table's, valid while the
        // owning task's poll runs on this thread.
        let epoch = unsafe { (*current.cancel_epoch_source).load() };
        if epoch == current.cancel_epoch {
            Ok(current.cancelled)
        } else {
            Err(Some(epoch))
        }
    });
    match cached {
        Ok(cancelled) => cancelled,
        Err(epoch) => {
            let cancelled = willow_sched_is_cancelled(task) != 0;
            if let Some(epoch) = epoch {
                CURRENT_EXECUTION.with(|slot| {
                    let mut current = slot.borrow_mut();
                    if current.owner == Some(task) {
                        current.cancel_epoch = epoch;
                        current.cancelled = cancelled;
                    }
                });
            }
            cancelled
        }
    }
}

/// Every poll boundary drops the published fault location: release synchronous
/// code publishes none, so a site left by the previous task (or by an async
/// poll that returned to synchronous code) must not label its faults.
///
fn set_current_task(id: Option<RuntimeTaskId>) {
    crate::panic_context::willow_fault_site_clear();
    install_current_task(id);
    let context = id.and_then(|task_id| {
        let tasks = global_task_table();
        tasks.with_mut(task_id, |task| {
            if !CURRENT_EXECUTION.with(|slot| slot.borrow().owns(task)) {
                TaskExecution::claim(task, &tasks.cancel_epoch);
            }
            task.panic_context()
        })
    });
    if id.is_none() {
        CURRENT_EXECUTION.with(|slot| {
            // Every runnable/terminal boundary publishes the stack first.
            // Dropping a suspended stack here would violate worker ownership.
            slot.replace(TaskExecution::new());
        });
    }
    crate::panic_context::replace_current_context(context);
}

/// Install a context already cloned while the task shard was held. Production
/// claim paths use this to avoid a second task-table lock solely for TLS setup.
fn set_current_task_context(id: RuntimeTaskId, context: Arc<crate::panic_context::PanicContext>) {
    crate::panic_context::willow_fault_site_clear();
    install_current_task(Some(id));
    crate::panic_context::replace_current_context(Some(context));
}

pub(crate) fn current_worker() -> usize {
    CURRENT_WORKER.with(Cell::get)
}

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

/// Test-only: reset the global scheduler between unit tests. Callers must
/// serialize access to the process-global scheduler and heap.
#[cfg(test)]
pub fn reset_global_scheduler_for_test() {
    // Drained tasks never run their cancel path, so drop their blocking-pool
    // slot reservations too (task ids restart and would alias them).
    crate::blocking::reset_slot_waiters_for_test();
    let frames = with_global(|sched| {
        let mut frames = sched.retirements.take_all_frames();
        frames.extend(sched.tasks.drain().into_iter().filter_map(|mut task| {
            if task.frame_rooted && !task.frame.is_null() {
                task.frame_rooted = false;
                Some(task.frame as usize)
            } else {
                None
            }
        }));
        *sched = RuntimeScheduler::from_components(replace_global_components(
            runtime_worker_config().active_workers(),
        ));
        frames
    });
    for frame in frames {
        // SAFETY: The runtime keeps this nonmoving payload at its registered address through
        // removal.
        unsafe { crate::gc::willow_gc_remove_runtime_root(frame as *mut u8) };
    }
    set_current_task(None);
    CURRENT_WORKER.with(|worker| worker.set(0));
    SPAWN_NOTIFICATION_PENDING.with(|pending| pending.set(false));
    LOCAL_WAKE_NOTIFICATION_PENDING.with(|pending| pending.set(false));
    CURRENT_RUN_STATE.with(|state| {
        state.replace(None);
    });
    // In a real process the fatal gate is one-way (the closer ends the process), but
    // a unit test can drive a panicking poll without the exit. Reopen it so the
    // next test is not parked forever.
    crate::runtime::current()
        .fatal_panic_pending
        .store(false, Ordering::Release);
}

#[cfg(test)]
fn replace_global_scheduler_for_test(worker_count: usize) {
    with_global(|sched| {
        *sched = RuntimeScheduler::from_components(replace_global_components(worker_count));
    });
}

/// Enter or resume a task-owned synchronous helper callback.
///
/// # Safety
/// The callback must accept this frame and both must remain valid across any suspension
/// until the callback returns.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_task_stack_enter(
    callback: RuntimePollFn,
    frame: *mut c_void,
) -> i32 {
    task_stack_enter(callback, frame)
}

/// Generated-helper ABI: synchronous frames remain on their owning OS thread
/// across implicit preemption, including frames holding non-Send local values.
///
/// # Safety
/// The callback must accept this frame and both must remain valid across any suspension
/// until the callback returns.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_task_stack_enter_generated(
    callback: RuntimePollFn,
    frame: *mut c_void,
) -> i32 {
    task_stack_enter(callback, frame)
}

fn task_stack_enter(callback: RuntimePollFn, frame: *mut c_void) -> i32 {
    #[cfg(any(
        all(
            target_os = "linux",
            target_env = "gnu",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "macos",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
    ))]
    {
        let id = current_task_id().expect("task stack entry outside a task");
        let mut stack = CURRENT_EXECUTION
            .with(|slot| {
                let mut current = slot.borrow_mut();
                assert_eq!(current.owner, Some(id), "task execution context missing");
                current.stack.take()
            })
            .unwrap_or_else(|| crate::native_stack::NativeStack::acquire(callback, frame));
        let panic_depth = stack.entry_panic_depth();
        // SAFETY: The claimed task retains its frame and pairs it with the installed callback for
        // this execution.
        let result = unsafe { crate::native_stack::NativeStack::resume(&mut *stack) };
        // A recovered cleanup panic must not revoke sticky cancellation, and
        // an already-active outer panic is not a new helper failure.
        let new_panic = crate::panic_context::willow_panic_depth() > panic_depth;
        let pending = stack.is_suspended() || stack.is_cancelled();
        CURRENT_EXECUTION.with(|slot| slot.borrow_mut().stack = Some(stack));
        if pending && !(result == RUNTIME_POLL_PANICKED && new_panic) {
            match result {
                RUNTIME_POLL_PENDING | RUNTIME_POLL_BLOCKED_SYSCALL => result,
                _ => RUNTIME_POLL_PREEMPTED,
            }
        } else {
            result
        }
    }
    #[cfg(not(any(
        all(
            target_os = "linux",
            target_env = "gnu",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "macos",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
    )))]
    // SAFETY: the task-stack entry ABI supplies the synchronous helper with its
    // matching live frame. The direct call completes within that frame's lifetime.
    unsafe {
        crate::generated_context::invoke(callback as *const (), frame)
    }
}

/// Release a completed helper stack after generated result/cleanup handling.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_task_stack_leave() {
    #[cfg(any(
        all(
            target_os = "linux",
            target_env = "gnu",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "macos",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
    ))]
    {
        let id = current_task_id().expect("task stack exit outside a task");
        let stack = CURRENT_EXECUTION.with(|slot| {
            let mut current = slot.borrow_mut();
            assert_eq!(current.owner, Some(id), "task execution context missing");
            if current
                .stack
                .as_ref()
                .is_some_and(|stack| !stack.is_suspended())
            {
                current.stack.take()
            } else {
                None
            }
        });
        if let Some(stack) = stack {
            crate::native_stack::NativeStack::recycle(stack);
        }
    }
}

/// Idle-stop / drive-completion viewpoints.
#[cfg(test)]
#[path = "scheduler_idle_stop_tests.rs"]
mod idle_stop_tests;

#[cfg(test)]
#[path = "scheduler_idle_backoff_tests.rs"]
mod idle_backoff_tests;

#[cfg(all(
    test,
    any(
        all(
            target_os = "linux",
            target_env = "gnu",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "macos",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
    )
))]
#[path = "scheduler_native_affinity_tests.rs"]
mod native_affinity_tests;

/// Opt-in scaling and footprint measurements, kept out of the deterministic
/// gate. See the module's own documentation for how to run
/// them and how to read the table.
#[cfg(test)]
#[path = "scheduler_scaling_tests.rs"]
pub(crate) mod scaling_measurements;

#[cfg(test)]
#[path = "scheduler_run_queue_metrics_tests.rs"]
mod run_queue_metrics_tests;

#[cfg(test)]
#[path = "scheduler_spread_tests.rs"]
mod spread_tests;

#[cfg(test)]
#[path = "scheduler_tests.rs"]
mod tests;

#[cfg(all(
    test,
    any(
        all(
            target_os = "linux",
            target_env = "gnu",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "macos",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
    )
))]
#[path = "scheduler_stack_migration_tests.rs"]
mod stack_migration_tests;

#[cfg(test)]
#[path = "scheduler_yield_accounting_tests.rs"]
mod yield_accounting_tests;

#[cfg(test)]
#[path = "scheduler_cooperative_affinity_tests.rs"]
mod cooperative_affinity_tests;

#[cfg(test)]
#[path = "scheduler_retry_measurements.rs"]
mod retry_measurements;

/// Advisory versioned scheduling metrics. Atomic queue counters only; the caller
/// receives an owned value and never holds a scheduler lock while acquiring GC
/// locks. Queue emptiness is not a measurement of system or process CPU idle.
#[derive(Clone, Copy, Debug)]
pub struct GcMetricsV1 {
    pub version: u32,
    pub sampled_ns: u64,
    pub runnable: usize,
}
pub fn gc_metrics_v1() -> GcMetricsV1 {
    GcMetricsV1 {
        version: 1,
        sampled_ns: crate::gc_telemetry::timestamp_ns(),
        runnable: global_run_queues().len(),
    }
}

#[cfg(all(
    test,
    any(
        all(
            target_os = "linux",
            target_env = "gnu",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(
            target_os = "macos",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ),
        all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
    )
))]
#[path = "scheduler_switch_lock_tests.rs"]
pub(crate) mod switch_lock_tests;

#[cfg(any(test, feature = "deterministic-test-clock"))]
pub(crate) fn test_clock_advanced() {
    parking::notify_all_idle_waiters();
}

#[cfg(test)]
#[path = "scheduler_select_tests.rs"]
mod select_tests;

#[path = "scheduler_deadlock.rs"]
mod deadlock;
