use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::{Duration, Instant};
use willow_abi::workers::{default_worker_count, parse_worker_count};

use crate::lock_wait::{LockId, LockWaitLink, RegistrationToken};
use crate::task::{
    ChannelOwnershipToken, RUNTIME_POLL_BLOCKED_SYSCALL, RUNTIME_POLL_PANICKED,
    RUNTIME_POLL_PENDING, RUNTIME_POLL_PREEMPTED, RUNTIME_POLL_READY, RUNTIME_POLL_YIELD,
    RuntimeCancelFn, RuntimePollFn, RuntimeTask, RuntimeTaskId, RuntimeTaskState,
};
use crate::task_state::{BoundaryOutcome, CancelOutcome, ClaimOutcome, TaskLifecycle, WakeOutcome};
use crate::timer_queue::{TimerQueue, TimerWake};

/// Lock-free half of terminal task cleanup. The task record is already gone
/// when this runs, so channel addresses and the exact lock reverse link must be
/// captured before removal (willow-ezs.1.4, willow-38w.1.6).
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
/// alive, so process-global drives dispatch to one persistent worker.
///
/// This avoids mutating the process environment for tests that need a single
/// worker regardless of the host default. A test that asserts on *which* tasks
/// one drive reaped needs one: with multiple workers, a second
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
        default_worker_count(),
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
    metrics: crate::observability::RunQueueMetrics,
    worker_metrics: Vec<crate::observability::RunQueueMetrics>,
    #[cfg(test)]
    idle_notifications: AtomicUsize,
}

impl RunQueues {
    fn new(worker_count: usize) -> Self {
        let worker_count = worker_count.max(1);
        Self {
            #[cfg(test)]
            idle_notifications: AtomicUsize::new(0),
            locals: (0..worker_count)
                .map(|_| Mutex::new(VecDeque::new()))
                .collect(),
            prefer_global: (0..worker_count).map(|_| AtomicBool::new(false)).collect(),
            global: Mutex::new(VecDeque::new()),
            metrics: crate::observability::RunQueueMetrics::default(),
            worker_metrics: (0..worker_count)
                .map(|_| crate::observability::RunQueueMetrics::default())
                .collect(),
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

    fn notify_idle(&self) {
        #[cfg(test)]
        self.idle_notifications.fetch_add(1, Ordering::Relaxed);
        notify_idle_waiters();
    }

    fn push_global(&self, id: RuntimeTaskId) {
        self.push_global_batch(std::slice::from_ref(&id));
    }

    /// Publish already-granted queue tokens in slice order under one lock.
    /// Callers must own each token, granted by the task-state machine (e.g.
    /// `wake` or `claim_queue_slot`) or retained when returning an unclaimed id.
    /// This storage layer neither validates task lifecycle nor deduplicates ids.
    fn push_global_batch(&self, ids: &[RuntimeTaskId]) {
        self.publish_global_batch(ids, true);
    }

    fn push_spawned(&self, id: RuntimeTaskId) {
        let defer = SCHED_RUN_DEPTH.with(|depth| depth.get() > 0);
        self.publish_global_batch(std::slice::from_ref(&id), !defer);
        if defer {
            // Spawning cannot suspend its publishing worker. Coalesce all
            // spawns in this poll; flush before its next scheduler probe,
            // including entry into a nested drive. External spawns stay prompt.
            SPAWN_NOTIFICATION_PENDING.with(|pending| pending.set(true));
        }
    }

    fn publish_global_batch(&self, ids: &[RuntimeTaskId], notify: bool) {
        if ids.is_empty() {
            return;
        }
        let mut queue = Self::lock(&self.global);
        let previous_len = queue.len();
        queue.extend(ids.iter().copied());
        let backlog = queue.len();
        drop(queue);
        // A running publisher can consume a sole continuation itself. Wake
        // another worker when a burst first contains additional work. Keep
        // subsequent publications in that burst off the shared notifier.
        // Foreign publishers must wake a worker even for a single task.
        if notify {
            let threshold = if SCHED_RUN_DEPTH.with(|depth| depth.get() > 0) {
                2
            } else {
                1
            };
            if previous_len < threshold && backlog >= threshold {
                self.notify_idle();
            }
        }
        self.metrics
            .global_pushes
            .fetch_add(ids.len() as u64, Ordering::Relaxed);
    }

    fn push_local(&self, worker: usize, id: RuntimeTaskId) {
        match self.locals.get(worker) {
            Some(queue) => {
                let backlog = {
                    let mut queue = Self::lock(queue);
                    queue.push_back(id);
                    queue.len()
                };
                if SCHED_RUN_DEPTH.with(|depth| depth.get() > 0) && worker == current_worker() {
                    notify_local_work(backlog);
                } else {
                    self.notify_idle();
                }
                self.worker_metrics[worker]
                    .local_pushes
                    .fetch_add(1, Ordering::Relaxed);
            }
            None => self.push_global(id),
        }
    }

    fn push_woken_batch(&self, ids: &[RuntimeTaskId]) {
        if ids.is_empty() {
            return;
        }
        // CURRENT_WORKER defaults to zero even on foreign threads. The drive
        // depth, unlike that index, distinguishes a worker from an external waker.
        let worker = SCHED_RUN_DEPTH.with(|depth| (depth.get() > 0).then(current_worker));
        if let Some(worker) = worker.filter(|&worker| worker < self.locals.len()) {
            let backlog = {
                let mut queue = Self::lock(&self.locals[worker]);
                queue.extend(ids.iter().copied());
                queue.len()
            };
            self.worker_metrics[worker]
                .local_pushes
                .fetch_add(ids.len() as u64, Ordering::Relaxed);
            notify_local_work(backlog);
        } else {
            self.push_global_batch(ids);
        }
    }

    #[cfg(test)]
    fn push_local_front(&self, worker: usize, id: RuntimeTaskId) {
        match self.locals.get(worker) {
            Some(queue) => {
                Self::lock(queue).push_front(id);
                self.worker_metrics[worker]
                    .local_pushes
                    .fetch_add(1, Ordering::Relaxed);
            }
            None => {
                Self::lock(&self.global).push_front(id);
                self.metrics.global_pushes.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    #[cfg(test)]
    fn force_push_global(&self, id: RuntimeTaskId) {
        self.push_global(id);
    }

    fn pop_global(
        &self,
        worker: usize,
        metrics: &crate::observability::RunQueueMetrics,
        refill: bool,
    ) -> Option<RuntimeTaskId> {
        metrics.global_pop_attempts.fetch_add(1, Ordering::Relaxed);
        let mut global = Self::lock(&self.global);
        let id = global.pop_front()?;
        // Refill an empty local queue from half the remaining burst, capped
        // at 31 additional tokens. Do not accumulate global batches behind
        // existing local work: that changes task/GC lifetimes unnecessarily.
        let count = if refill && self.locals.len() > 1 && self.locals.get(worker).is_some() {
            (global.len() / 2).min(31)
        } else {
            0
        };
        // Lock order is global -> local; no other path holds both locks.
        let mut refilled = false;
        if count > 0 {
            let mut local = Self::lock(&self.locals[worker]);
            if local.is_empty() {
                local.extend(global.drain(..count));
                refilled = true;
                metrics
                    .local_pushes
                    .fetch_add(count as u64, Ordering::Relaxed);
            }
        }
        drop(global);
        if refilled {
            // A newly active worker shares a burst with one successor. This
            // keeps large injected batches parallel without notifying per ID.
            self.notify_idle();
        }
        metrics.global_pop_hits.fetch_add(1, Ordering::Relaxed);
        Some(id)
    }

    fn pop_for_worker(&self, worker: usize) -> Option<RuntimeTaskId> {
        let metrics = self.worker_metrics.get(worker).unwrap_or(&self.metrics);
        let prefer_global = self
            .prefer_global
            .get(worker)
            .map(|preference| preference.fetch_xor(true, Ordering::Relaxed))
            .unwrap_or(true);
        if prefer_global {
            if let Some(id) = self.pop_global(worker, metrics, false) {
                return Some(id);
            }
            if let Some(queue) = self.locals.get(worker)
                && let Some(id) = Self::lock(queue).pop_front()
            {
                metrics.local_pop_hits.fetch_add(1, Ordering::Relaxed);
                return Some(id);
            }
        } else {
            if let Some(queue) = self.locals.get(worker)
                && let Some(id) = Self::lock(queue).pop_front()
            {
                metrics.local_pop_hits.fetch_add(1, Ordering::Relaxed);
                return Some(id);
            }
            if let Some(id) = self.pop_global(worker, metrics, true) {
                return Some(id);
            }
        }
        // One attempt is one steal scan, including the empty single-worker scan.
        metrics.steal_attempts.fetch_add(1, Ordering::Relaxed);
        let count = self.locals.len();
        for offset in 1..count {
            let victim = (worker + offset) % count;
            metrics.victim_locks.fetch_add(1, Ordering::Relaxed);
            if let Some(id) = Self::lock(&self.locals[victim]).pop_back() {
                metrics.steal_successes.fetch_add(1, Ordering::Relaxed);
                return Some(id);
            }
        }
        metrics.steal_failures.fetch_add(1, Ordering::Relaxed);
        None
    }

    fn metrics_snapshot(&self) -> crate::observability::RunQueueMetricsSnapshot {
        let mut snapshot = self.metrics.snapshot();
        for metrics in &self.worker_metrics {
            snapshot.add(metrics.snapshot());
        }
        snapshot
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

    fn for_each(&self, mut read: impl FnMut(&RuntimeTask)) {
        for index in 0..self.shards.len() {
            for task in self.lock_shard(index).values() {
                read(task);
            }
        }
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

#[path = "scheduler_wake_batch.rs"]
mod wake_batch;
pub(crate) use wake_batch::{WakeBatchScratch, wake_channel_owners};
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
/// stale — the same answer it gets once the record is gone (willow-0a6k.7).
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
                if task
                    .native_stack
                    .as_ref()
                    .is_some_and(|stack| stack.is_suspended())
                {
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
        let mut blocked = 0;
        self.for_each_task(|task| {
            blocked += usize::from(task.runtime_state() == Some(RuntimeTaskState::BlockedSyscall));
        });
        self.blocked_syscall_count() == blocked
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
        // retaining one cleanup record per completed test/helper task.
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
    ///
    /// Global native-task spawning deliberately releases `GLOBAL_SCHEDULER`
    /// after this step, initializes frame/native state, then re-acquires the
    /// lock only for [`Self::publish_reserved_task`]. An id gap after an
    /// initializer panic is intentional: reusing it would create an ABA risk
    /// for native registrations that observed the reserved id.
    fn reserve_task_id(&mut self) -> RuntimeTaskId {
        let id = self.next_task_id;
        self.next_task_id = self
            .next_task_id
            .checked_add(1)
            .expect("runtime task id exhausted");
        id
    }

    /// Publish one fully initialized task. Queue insertion is the final step,
    /// so no worker can claim the record before poll/cancel/frame fields exist.
    fn publish_reserved_task(
        &mut self,
        id: RuntimeTaskId,
        poll: RuntimePollFn,
        frame: *mut c_void,
        cancel: Option<RuntimeCancelFn>,
        cooperative_poll: bool,
    ) {
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
            self.frame_roots += 1;
        }
        self.tasks.insert(id, task);
        self.enqueue_ready(id);
    }

    pub fn spawn_placeholder(&mut self) -> RuntimeTaskId {
        let id = self.reserve_task_id();
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
    pub fn spawn_task_initialized(
        &mut self,
        poll: RuntimePollFn,
        frame: *mut c_void,
        cancel: Option<RuntimeCancelFn>,
        initialize: impl FnOnce(RuntimeTaskId),
    ) -> RuntimeTaskId {
        let id = self.reserve_task_id();
        initialize(id);
        self.publish_reserved_task(id, poll, frame, cancel, false);
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

    pub fn with_task_mut<R>(
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

fn global_run_queues() -> Arc<RunQueues> {
    GLOBAL_RUN_QUEUES
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn global_task_table() -> Arc<ShardedTaskTable> {
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

fn with_global<R>(f: impl FnOnce(&mut RuntimeScheduler) -> R) -> R {
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
        |entry| {
            wake_task_matching_in(&tasks, &run_queues, entry.task_id, Some(entry.deadline));
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
            if task
                .native_stack
                .as_ref()
                .is_some_and(|stack| stack.is_suspended())
            {
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

thread_local! {
    /// The task currently being polled on this OS thread. Runtime primitives
    /// such as sleep/channel/await use it to attach wait state to the right task.
    static CURRENT_TASK: Cell<Option<RuntimeTaskId>> = const { Cell::new(None) };
    /// Worker-local index used for local-queue affinity and nested scheduler
    /// drives from inside a poll.
    static CURRENT_WORKER: Cell<usize> = const { Cell::new(0) };
    static SPAWN_NOTIFICATION_PENDING: Cell<bool> = const { Cell::new(false) };
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

pub(crate) fn current_worker() -> usize {
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
/// and cancellation cleanup has been initialized. The closure runs after its
/// id is reserved but outside `GLOBAL_SCHEDULER`, and before the task is visible
/// in either the task table or a run queue. It may call scheduler APIs; the
/// reserved task itself remains deliberately unobservable until publication.
pub(crate) fn spawn_global_task_initialized(
    poll: RuntimePollFn,
    frame: *mut c_void,
    cancel: Option<RuntimeCancelFn>,
    initialize: impl FnOnce(RuntimeTaskId),
) -> u64 {
    spawn_global_task_initialized_inner(poll, frame, cancel, initialize, false)
}

/// Spawn a generated poll whose synchronous calls enter a stack lazily.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_spawn_cooperative(poll: RuntimePollFn, frame: *mut c_void) -> i64 {
    spawn_global_task_initialized_inner(poll, frame, None, |_| {}, true) as i64
}

fn spawn_global_task_initialized_inner(
    poll: RuntimePollFn,
    frame: *mut c_void,
    cancel: Option<RuntimeCancelFn>,
    initialize: impl FnOnce(RuntimeTaskId),
    cooperative_poll: bool,
) -> u64 {
    // Reserve under the metadata lock, then initialize entirely outside it.
    // The frame is rooted before the callback because native initialization may
    // allocate or explicitly collect. A panic leaves an id gap but the guard
    // rolls the unpublished runtime root back.
    let id = with_global(RuntimeScheduler::reserve_task_id);
    let mut root = PendingSpawnRoot::new(frame as *mut u8);
    initialize(id);
    with_global(|sched| sched.publish_reserved_task(id, poll, frame, cancel, cooperative_poll));
    root.publish();
    crate::observability::record(
        crate::observability::RuntimeEventKind::TaskSpawn,
        current_task_id().map(|_| current_worker()),
        id,
        0,
    );
    crate::gc::stress_collect("scheduler");
    id
}

/// Owns the runtime root between id reservation and task publication.
/// Published tasks transfer that ownership to `RuntimeTask::frame_rooted`;
/// unwinding initializers drop it here instead.
struct PendingSpawnRoot {
    frame: *mut u8,
    published: bool,
}

impl PendingSpawnRoot {
    fn new(frame: *mut u8) -> Self {
        crate::gc::willow_gc_add_runtime_root(frame);
        Self {
            frame,
            published: false,
        }
    }

    fn publish(&mut self) {
        self.published = true;
    }
}

impl Drop for PendingSpawnRoot {
    fn drop(&mut self) {
        if !self.published {
            crate::gc::willow_gc_remove_runtime_root(self.frame);
        }
    }
}

/// Wake a parked task, re-queueing it as ready.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_wake(id: u64) {
    let _ = try_wake_parked_task(id);
}

/// Wake a contiguous array of task IDs. A zero length accepts a null pointer.
///
/// # Safety
/// For nonzero `len`, `ids` must reference `len` initialized, aligned u64 values
/// readable for this call; their storage must remain valid across GC safepoints.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn willow_sched_wake_many(ids: *const u64, len: usize) {
    if len == 0 {
        return;
    }
    let ids = unsafe { std::slice::from_raw_parts(ids, len) };
    wake_channel_owners(ids, &mut WakeBatchScratch::default());
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
    crate::gc::stress_collect("scheduler");
    transitioned
}

/// A channel reservation already published under its mutex remains valid for
/// a live task that is queued or polling: it may have observed readiness before
/// this wake. Only terminal owners may lose it; exact cleanup handles races.
pub(crate) fn wake_channel_owner(id: u64) -> bool {
    crate::gc::stress_collect("scheduler");
    let outcome = wake_global_task_outcome(id);
    if outcome == WakeOutcome::Enqueue {
        crate::observability::record(
            crate::observability::RuntimeEventKind::TaskWake,
            current_task_id().map(|_| current_worker()),
            id,
            0,
        );
    }
    crate::gc::stress_collect("scheduler");
    outcome != WakeOutcome::Terminal
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

/// Publish channel ownership under its task shard, without a safepoint.
pub(crate) fn install_channel_ownership(task_id: u64, token: ChannelOwnershipToken) -> bool {
    global_task_table()
        .with_mut(task_id, |task| {
            let state = task.state.load();
            if state.lifecycle().is_terminal()
                || state.lifecycle() == TaskLifecycle::Cancelling
                || state.cancel_requested()
            {
                return false;
            }
            task.install_channel_ownership(token)
        })
        .unwrap_or(false)
}

pub(crate) fn transition_channel_ownership(
    task_id: u64,
    old: ChannelOwnershipToken,
    new: ChannelOwnershipToken,
) -> bool {
    global_task_table()
        .with_mut(task_id, |task| {
            let state = task.state.load();
            if state.lifecycle().is_terminal()
                || state.lifecycle() == TaskLifecycle::Cancelling
                || state.cancel_requested()
            {
                return false;
            }
            task.transition_channel_ownership(old, new)
        })
        .unwrap_or(false)
}

pub(crate) fn clear_channel_ownership(task_id: u64, token: ChannelOwnershipToken) -> bool {
    global_task_table()
        .with_mut(task_id, |task| task.clear_channel_ownership(token))
        .unwrap_or(false)
}

pub(crate) fn take_channel_waits(task_id: u64) -> Vec<ChannelOwnershipToken> {
    global_task_table()
        .with_mut(task_id, RuntimeTask::take_wait_channels)
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

/// Attach cooperative cleanup with its compiler-proven native stack requirement.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_set_cancel_fn_cooperative(
    id: u64,
    cancel: RuntimeCancelFn,
    needs_stack: i32,
) {
    global_task_table().with_mut(id, |task| {
        task.cancel = Some(cancel);
        task.native_cancel_cleanup = needs_stack != 0;
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

#[path = "scheduler_parking.rs"]
mod parking;
use parking::{
    current_wake_generation, notify_all_idle_waiters, notify_idle_waiters, notify_local_work,
    wait_for_wake_since,
};

/// Render the active async chain for panic diagnostics (willow-9lw).
pub fn async_chain_text() -> String {
    with_global(|sched| {
        let Some(mut id) = current_task_id() else {
            return String::new();
        };
        let mut lines = Vec::new();
        let mut seen = std::collections::HashSet::new();
        // Walk current task -> its awaiter -> ... via the reverse `waiters` link.
        while seen.insert(id) {
            let Some((line, awaiter)) = sched.with_task(id, |task| {
                let name = task.name().unwrap_or("<async task>");
                let site = match task.spawn_site() {
                    Some((file, line)) => format!(" [task {id}, spawned at {file}:{line}]"),
                    None => format!(" [task {id}]"),
                };
                // Only inspect the first live waiter; do not copy the task's
                // other relationships while rendering its diagnostic line.
                (
                    format!("  {}: async {}{}", lines.len(), name, site),
                    task.first_live_waiter(),
                )
            }) else {
                break;
            };
            lines.push(line);
            match awaiter {
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

/// Requested worker count from `WILLOW_WORKERS`, or available parallelism.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_requested_workers() -> u64 {
    runtime_worker_config().requested_workers() as u64
}

/// Worker count the current runtime will run. Any positive `WILLOW_WORKERS`
/// value is honored; invalid values use available parallelism (fallback: one).
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
    let target = target as RuntimeTaskId;
    let mut completed = sched_run_with_mutator(Some(target), None);
    // An unbounded drive that returns while its target is still RUNNABLE has
    // not honoured its contract: the caller (`await handle`, and the program
    // entry point that drives `main`) reads the target's result from its frame
    // and would observe an uninitialized one. The scheduler stops the worker
    // pool on an idleness snapshot, and any residual race in that snapshot
    // strands exactly this way, so treat an early return as a signal to drive
    // again rather than as an answer. `scheduler_has_wake_source` is false once
    // the target is genuinely blocked forever, so this cannot spin: a real
    // deadlock still returns and the awaiter reports it (willow-6wd6).
    while !target_is_done(Some(target)) && scheduler_has_wake_source() {
        completed += sched_run_with_mutator(Some(target), None);
    }
    completed
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
/// queued work, a claim in flight, another actively polling task, an armed
/// timer, a parked netpoll waiter, or an outstanding blocking-pool syscall.
///
/// A drive returning "no task completed" is NOT the same as "nothing can ever
/// happen": the drive may simply have raced a claim, or stopped on its own
/// deadline. Callers that turn quiescence into a blocking-forever diagnostic
/// must consult this first (willow-atth).
pub(crate) fn scheduler_has_wake_source() -> bool {
    // Read producers before their destination queues, then claims and polls,
    // just as the parallel idle snapshot does. A deadline-limited nested drive
    // can return while another worker is still polling a producer.
    global_next_timer_deadline().is_some()
        || global_task_table().blocked_syscall_count() > 0
        || global_run_queues().len() > 0
        || claims_in_flight()
        || CURRENT_RUN_STATE.with(|slot| {
            slot.borrow().as_ref().is_some_and(|state| {
                // This caller may itself be a running task blocked in recv or
                // send. Counting it would turn genuine deadlocks into spins.
                let caller = usize::from(current_task_id().is_some());
                state.active_polls.load(Ordering::Acquire) > caller
            })
        })
        || crate::netpoll::has_waiters()
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
    crate::stack_overflow::protect_current_thread();
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
    let completed = if outermost {
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
        crate::channel::purge_task_from_tokens(cleanup.task_id, cleanup.channel_waits);
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

struct WorkerDrive {
    target: Option<RuntimeTaskId>,
    state: Arc<ParallelRunState>,
    deadline: Option<Instant>,
    finished: Arc<ParallelCompletion>,
}

#[derive(Default)]
struct PersistentWorkers {
    senders: Vec<std::sync::mpsc::Sender<WorkerDrive>>,
}

static PERSISTENT_WORKERS: std::sync::LazyLock<Mutex<PersistentWorkers>> =
    std::sync::LazyLock::new(|| Mutex::new(PersistentWorkers::default()));

fn willow_sched_run_parallel(
    target: Option<RuntimeTaskId>,
    workers: usize,
    deadline: Option<Instant>,
) -> i64 {
    // Hold drive ownership until all workers finish. Wait cooperatively because
    // another driver may be collecting while this caller is a registered mutator.
    let mut pool = loop {
        match PERSISTENT_WORKERS.try_lock() {
            Ok(pool) => break pool,
            Err(std::sync::TryLockError::Poisoned(error)) => break error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                crate::gc::willow_gc_safepoint();
                std::thread::yield_now();
            }
        }
    };
    let workers = workers.max(1);
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
    let workers = workers.max(crate::native_stack::required_workers());
    while pool.senders.len() < workers {
        let worker = pool.senders.len();
        let (sender, receiver) = std::sync::mpsc::channel::<WorkerDrive>();
        std::thread::Builder::new()
            .name(format!("willow-worker-{worker}"))
            .spawn(move || {
                while let Ok(drive) = receiver.recv() {
                    run_parallel_worker(worker, drive.target, drive.state, drive.deadline);
                    drive.finished.finish();
                }
            })
            .expect("cannot start persistent scheduler worker");
        pool.senders.push(sender);
    }
    let state = Arc::new(ParallelRunState::default());
    let finished = Arc::new(ParallelCompletion::new(workers));
    for sender in pool.senders.iter().take(workers) {
        sender
            .send(WorkerDrive {
                target,
                state: Arc::clone(&state),
                deadline,
                finished: Arc::clone(&finished),
            })
            .expect("scheduler worker terminated");
    }
    finished.wait();
    state.completed.load(Ordering::Acquire)
}

/// One completion counter per drive, independent of worker count. The mutex
/// couples the completion predicate to the wait so the final wake cannot be lost.
struct ParallelCompletion {
    remaining: Mutex<usize>,
    ready: std::sync::Condvar,
}

impl ParallelCompletion {
    fn new(workers: usize) -> Self {
        Self {
            remaining: Mutex::new(workers),
            ready: std::sync::Condvar::new(),
        }
    }

    fn finish(&self) {
        let mut remaining = self.remaining.lock().unwrap_or_else(|e| e.into_inner());
        *remaining -= 1;
        if *remaining == 0 {
            self.ready.notify_one();
        }
    }

    fn wait(&self) {
        loop {
            // The driver remains a registered mutator. Never hold the completion
            // mutex across a safepoint, and bound every wait so GC can stop it.
            crate::gc::willow_gc_safepoint();
            let remaining = self.remaining.lock().unwrap_or_else(|e| e.into_inner());
            if *remaining == 0 {
                return;
            }
            drop(
                self.ready
                    .wait_timeout(remaining, Duration::from_millis(1))
                    .unwrap_or_else(|e| e.into_inner()),
            );
        }
    }
}

fn run_parallel_worker(
    worker: usize,
    target: Option<RuntimeTaskId>,
    state: Arc<ParallelRunState>,
    deadline: Option<Instant>,
) {
    crate::stack_overflow::protect_current_thread();
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

fn task_requires_cancel_poll(task: &RuntimeTask) -> bool {
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
    if task
        .native_stack
        .as_ref()
        .is_some_and(|stack| stack.is_suspended())
    {
        return true;
    }
    task.cancel.is_some() && !task.frame.is_null()
}

/// Run the async frame cleanup after native synchronous callers have unwound.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sync_poll_cancel_cleanup() {
    let Some(id) = current_task_id() else {
        return;
    };
    let cleanup = global_task_table()
        .with_mut(id, |task| {
            task.cancel.take().map(|cancel| (cancel, task.frame))
        })
        .flatten();
    if let Some((cancel, frame)) = cleanup {
        crate::preempt::willow_sync_cleanup_enter();
        unsafe {
            cancel(frame);
        }
        crate::preempt::willow_sync_cleanup_leave();
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
        // Even a foreign-affinity reroute must resolve under the idle
        // snapshot gate. Otherwise the snapshot can read an empty queue,
        // then miss the claim after it requeues and clears its marker.
        let claim_guard = shared.map(|state| {
            state
                .claim_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        });
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
        if let Some(owner) = tasks
            .with(id, |task| {
                task.native_stack.as_ref().map(|stack| stack.worker)
            })
            .flatten()
            && owner != worker
        {
            queues.push_local(owner, id);
            return None;
        }
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
                ClaimOutcome::Cancel if task_requires_cancel_poll(task) => Claim::Work(
                    task.poll
                        .map(|poll| (poll, task.frame, task.preempt_flag_ptr())),
                    task.panic_context(),
                ),
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
                crate::observability::record(
                    crate::observability::RuntimeEventKind::TaskCancelled,
                    Some(worker),
                    id,
                    0,
                );
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

// Park is interruptible by work publication. Keep the existing 1ms upper
// bound because parked threads remain GC mutators and must reach safepoints.
fn idle_wait_bound(deadline: Option<Instant>) -> Duration {
    let wait = global_next_timer_deadline()
        .map(|(_, timer)| bounded_parallel_wait(duration_until(timer)))
        .unwrap_or(Duration::from_millis(1));
    deadline_bounded(wait, deadline)
}

fn scheduler_idle_step(
    worker: usize,
    shared: Option<&ParallelRunState>,
    keep_alive_for_paused: bool,
    deadline: Option<Instant>,
    generation: u64,
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
        wait_for_wake_since(generation, idle_wait_bound(deadline));
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
            wait_for_wake_since(generation, idle_wait_bound(deadline));
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
                    wait_for_wake_since(generation, wait);
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
            wait_for_wake_since(generation, idle_wait_bound(deadline));
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

/// A source that can hold — or produce — a runnable task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkSource {
    /// An armed wake-deadline in the global timer heap.
    Timer,
    /// A task the blocking pool still owes a completion wake.
    BlockedSyscall,
    /// A task sitting in the global or a worker-local run queue.
    RunQueue,
    /// A worker holding a popped-but-unclaimed task, or one mid-poll.
    Claim,
    /// A task parked on I/O readiness.
    Netpoll,
}

/// The order [`parallel_run_is_idle_locked`] must read the work sources in.
///
/// This is the whole correctness argument for ending a parallel drive, so it
/// is declared as data and checked by a test rather than left implicit in the
/// order of a few `if` statements. Every producer publishes a "still busy"
/// marker and its queue entry in a fixed order, and the snapshot has to read
/// the two in the opposite order so that no producer can slip through both:
///
/// * Timer promotion holds the heap lock until its queue entry is published,
///   so [`WorkSource::Timer`] is read BEFORE [`WorkSource::RunQueue`].
///   Reversing that permits: `idle: queue empty` / `timer worker: pop timer,
///   enqueue task, release heap` / `idle: heap empty -> stop`.
/// * Blocked-syscall completion publishes its queue entry before decrementing
///   the counter, so [`WorkSource::BlockedSyscall`] is read BEFORE
///   [`WorkSource::RunQueue`].
/// * A claim publishes its in-flight marker BEFORE it pops, so
///   [`WorkSource::Claim`] is read AFTER [`WorkSource::RunQueue`]. Reversing
///   that permits: `idle: no claims` / `worker: enter claim, pop the last
///   task, block on claim_gate` / `idle: queue empty -> stop`, after which the
///   worker sees the stop, requeues the task it is holding and leaves —
///   stranding a runnable task and returning from the drive with its target
///   still alive (willow-6wd6). `claim_gate` alone does not cover this: it
///   excludes claims that have not popped yet, not the pop itself
///   (willow-atth).
///
/// None of these independently synchronized structures requires
/// `GLOBAL_SCHEDULER` (willow-9ha4).
const IDLE_READ_ORDER: [WorkSource; 5] = [
    WorkSource::Timer,
    WorkSource::BlockedSyscall,
    WorkSource::RunQueue,
    WorkSource::Claim,
    WorkSource::Netpoll,
];

fn work_source_is_live(state: &ParallelRunState, source: WorkSource) -> bool {
    match source {
        WorkSource::Timer => global_next_timer_deadline().is_some(),
        WorkSource::BlockedSyscall => global_task_table().blocked_syscall_count() > 0,
        WorkSource::RunQueue => global_run_queues().len() > 0,
        WorkSource::Claim => {
            claims_in_flight()
                || state.active_polls.load(Ordering::Acquire) > 0
                || state.paused_polls.load(Ordering::Acquire) > 0
        }
        WorkSource::Netpoll => crate::netpoll::has_waiters(),
    }
}

/// Is this parallel run globally idle — nothing runnable now, and no source
/// that could make something runnable later?
///
/// The caller must already hold `state.claim_gate`; the reads are only
/// coherent with claims excluded, and the stop that follows a `true` must be
/// published under the same gate. See [`IDLE_READ_ORDER`] for why the order of
/// the reads is what makes this answer trustworthy.
fn parallel_run_is_idle_locked(state: &ParallelRunState) -> bool {
    !IDLE_READ_ORDER
        .iter()
        .any(|source| work_source_is_live(state, *source))
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
        if SPAWN_NOTIFICATION_PENDING.with(|pending| pending.replace(false)) {
            notify_idle_waiters();
        }
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
                        notify_all_idle_waiters();
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
        let wake_generation = current_wake_generation();
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
                wait_for_wake_since(wake_generation, idle_wait_bound(deadline));
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
                wake_generation,
            ) {
                continue;
            }
            if stop_pool_on_exit && let Some(state) = shared {
                // Revalidate global idleness while claims are excluded. Work
                // can be published between the earlier empty pop and this
                // point; stopping without this check strands that task in the
                // queue.
                let _claim_gate = state
                    .claim_gate
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if !parallel_run_is_idle_locked(state) {
                    continue;
                }
                // Publish the stop only once it is FINAL. An earlier version
                // set it optimistically and rolled it back when a later check
                // failed; any worker that read the flag inside that rollback
                // window left the pool for good, and any claim that read it
                // requeued the task it was holding and went idle (willow-6wd6).
                state.stop.store(true, Ordering::Release);
                notify_all_idle_waiters();
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
                let direct = global_task_table()
                    .with(id, |task| {
                        task.cooperative_poll && !task.native_cancel_cleanup
                    })
                    .unwrap_or(false);
                if direct {
                    unsafe { cancel_fn(cancel_frame) };
                } else {
                    let mut stack =
                        crate::native_stack::NativeStack::acquire_cleanup(cancel_fn, cancel_frame);
                    let flag = global_task_table()
                        .with(id, RuntimeTask::preempt_flag_ptr)
                        .unwrap_or(std::ptr::null());
                    crate::preempt::willow_preempt_begin(flag);
                    let result = unsafe { crate::native_stack::NativeStack::resume(&mut *stack) };
                    crate::preempt::willow_preempt_end();
                    if stack.is_suspended() {
                        global_task_table().with_mut(id, |task| task.native_stack = Some(stack));
                        let boundary = match result {
                            RUNTIME_POLL_PENDING => GlobalPollBoundary::Pending,
                            RUNTIME_POLL_BLOCKED_SYSCALL => GlobalPollBoundary::BlockedSyscall,
                            _ => GlobalPollBoundary::Runnable,
                        };
                        finish_global_poll_boundary(id, boundary);
                        let event = match result {
                            RUNTIME_POLL_PENDING => {
                                crate::observability::RuntimeEventKind::TaskPark
                            }
                            RUNTIME_POLL_BLOCKED_SYSCALL => {
                                crate::observability::RuntimeEventKind::BlockingDetach
                            }
                            _ => crate::observability::RuntimeEventKind::TaskPreempt,
                        };
                        crate::observability::record(event, Some(worker), id, i64::from(result));
                        set_current_task(None);
                        finish_active_poll(shared);
                        continue;
                    }
                    crate::native_stack::NativeStack::recycle(stack);
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
            unsafe {
                cancel_fn(cancel_frame)
            };
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
        let (result, native_cancelled) = {
            let direct = global_task_table()
                .with(id, |task| {
                    task.cooperative_poll
                        && !task
                            .native_stack
                            .as_ref()
                            .is_some_and(|stack| stack.is_cleanup())
                })
                .unwrap_or(false);
            if direct {
                (unsafe { poll(frame) }, false)
            } else {
                let mut stack = global_task_table()
                    .with_mut(id, |task| task.native_stack.take())
                    .flatten()
                    .unwrap_or_else(|| crate::native_stack::NativeStack::acquire(poll, frame));
                let result = unsafe { crate::native_stack::NativeStack::resume(&mut *stack) };
                let cancelled = stack.is_cancelled();
                if stack.is_suspended() {
                    global_task_table().with_mut(id, |task| task.native_stack = Some(stack));
                } else {
                    crate::native_stack::NativeStack::recycle(stack);
                }
                (result, cancelled)
            }
        };
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
        let (result, native_cancelled) = (unsafe { poll(frame) }, false);
        crate::preempt::willow_preempt_end();
        // Native runtime polls (e.g. parallel-map chunks) need not implement
        // the generated async cancellation epilogue. Preserve their registered
        // cleanup callback and requeue into the ordinary cancellation entry.
        let result = if native_cancelled
            && result != RUNTIME_POLL_PANICKED
            && global_task_table()
                .with(id, |task| task.cancel.is_some())
                .unwrap_or(false)
        {
            RUNTIME_POLL_PREEMPTED
        } else {
            result
        };
        let outcome = classify_poll_result(result);
        let fatal_chain = matches!(outcome, PollOutcome::Panicked | PollOutcome::Invalid(_))
            .then(async_chain_text);
        match outcome {
            PollOutcome::Ready => with_global(|sched| {
                if native_cancelled {
                    sched.finalize_cancelled(id);
                } else {
                    sched.complete(id);
                }
            }),
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
    // Drained tasks never run their cancel path, so drop their blocking-pool
    // slot reservations too (task ids restart and would alias them).
    crate::blocking::reset_slot_waiters_for_test();
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
    SPAWN_NOTIFICATION_PENDING.with(|pending| pending.set(false));
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

/// Enter or resume a task-owned synchronous helper callback.
#[unsafe(no_mangle)]
pub extern "C" fn willow_task_stack_enter(callback: RuntimePollFn, frame: *mut c_void) -> i32 {
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
        let mut stack = global_task_table()
            .with_mut(id, |task| task.native_stack.take())
            .flatten()
            .unwrap_or_else(|| crate::native_stack::NativeStack::acquire(callback, frame));
        let panic_depth = stack.entry_panic_depth();
        let result = unsafe { crate::native_stack::NativeStack::resume(&mut *stack) };
        // A recovered cleanup panic must not revoke sticky cancellation, and
        // an already-active outer panic is not a new helper failure.
        let new_panic = crate::panic_context::willow_panic_depth() > panic_depth;
        let pending = stack.is_suspended() || stack.is_cancelled();
        global_task_table().with_mut(id, |task| task.native_stack = Some(stack));
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
    unsafe {
        callback(frame)
    }
}

/// Release a completed helper stack after generated result/cleanup handling.
#[unsafe(no_mangle)]
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
        let stack = global_task_table()
            .with_mut(id, |task| {
                if task
                    .native_stack
                    .as_ref()
                    .is_some_and(|stack| !stack.is_suspended())
                {
                    task.native_stack.take()
                } else {
                    None
                }
            })
            .flatten();
        if let Some(stack) = stack {
            crate::native_stack::NativeStack::recycle(stack);
        }
    }
}

/// Idle-stop / drive-completion viewpoints (willow-6wd6).
#[cfg(test)]
#[path = "scheduler_idle_stop_tests.rs"]
mod idle_stop_tests;

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
/// gate (willow-ezs.2/.3). See the module's own documentation for how to run
/// them and how to read the table.
#[cfg(test)]
#[path = "scheduler_scaling_tests.rs"]
pub(crate) mod scaling_measurements;

#[cfg(test)]
#[path = "scheduler_run_queue_metrics_tests.rs"]
mod run_queue_metrics_tests;

#[cfg(test)]
#[path = "scheduler_tests.rs"]
mod tests;
