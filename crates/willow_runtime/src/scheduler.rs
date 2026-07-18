use std::cell::{Cell, RefCell};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::task::{
    RUNTIME_POLL_PREEMPTED, RUNTIME_POLL_READY, RUNTIME_POLL_YIELD, RuntimeCancelFn, RuntimePollFn,
    RuntimeTask, RuntimeTaskId, RuntimeTaskState,
};
use crate::trace::{GcTrace, GcVisitor};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TimerWake {
    deadline: Instant,
    task_id: RuntimeTaskId,
}

pub const DEFAULT_WORKERS: usize = 5;

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
}

pub fn runtime_worker_config() -> RuntimeWorkerConfig {
    RuntimeWorkerConfig::from_env_value(
        std::env::var("WILLOW_WORKERS").ok().as_deref(),
        DEFAULT_WORKERS,
    )
}

#[derive(Debug)]
pub struct RuntimeScheduler {
    next_task_id: RuntimeTaskId,
    tasks: HashMap<RuntimeTaskId, RuntimeTask>,
    /// Per-worker local run queues + a shared global queue, with work stealing
    /// (willow-gyaa.4). New/woken tasks go to the global queue; an idle worker
    /// drains its local queue, then the global queue, then steals from the back
    /// of another worker's local queue.
    locals: Vec<VecDeque<RuntimeTaskId>>,
    global: VecDeque<RuntimeTaskId>,
    /// Task ids finalized as Cancelled whose netpoll registrations still need
    /// purging. Drained OUTSIDE the scheduler lock by the run loop — and only
    /// there — so a LOCAL test scheduler never touches the process-global
    /// netpoll (willow-vynv.1 review fix).
    pending_netpoll_purge: Vec<RuntimeTaskId>,
    timers: BinaryHeap<Reverse<TimerWake>>,
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
        let workers = worker_count.max(1);
        Self {
            next_task_id: 1,
            tasks: HashMap::new(),
            locals: (0..workers).map(|_| VecDeque::new()).collect(),
            global: VecDeque::new(),
            timers: BinaryHeap::new(),
            pending_netpoll_purge: Vec::new(),
        }
    }

    /// Number of worker-local run queues (the configured worker count).
    pub fn worker_count(&self) -> usize {
        self.locals.len()
    }

    /// Enqueue a runnable task. New and woken tasks go to the shared global
    /// queue; any idle worker can then pick them up (willow-gyaa.4).
    fn enqueue_ready(&mut self, id: RuntimeTaskId) {
        self.global.push_back(id);
    }

    /// Push a task directly onto a specific worker's local queue. Used by future
    /// parallel workers (and the work-stealing tests) to model locality.
    pub fn enqueue_local(&mut self, worker: usize, id: RuntimeTaskId) {
        if let Some(queue) = self.locals.get_mut(worker) {
            queue.push_back(id);
        } else {
            self.global.push_back(id);
        }
    }

    /// Pop the next runnable task for `worker`: its own local queue first (FIFO),
    /// then the global queue, then steal from the back of another worker's local
    /// queue (LIFO steal, which tends to take the coldest work). Returns `None`
    /// when no worker has runnable tasks (willow-gyaa.4).
    pub fn pop_for_worker(&mut self, worker: usize) -> Option<RuntimeTaskId> {
        if let Some(queue) = self.locals.get_mut(worker)
            && let Some(id) = queue.pop_front()
        {
            return Some(id);
        }
        if let Some(id) = self.global.pop_front() {
            return Some(id);
        }
        // Steal from another worker's local queue (back = oldest pushed).
        let n = self.locals.len();
        for offset in 1..n {
            let victim = (worker + offset) % n;
            if let Some(id) = self.locals[victim].pop_back() {
                return Some(id);
            }
        }
        None
    }

    /// Atomically claim the next task that is still Ready. Queue entries can
    /// become stale when a wake races with a Running poll; discarding them here
    /// prevents two workers from polling the same async frame concurrently.
    fn claim_ready_for_worker(&mut self, worker: usize) -> Option<RuntimeTaskId> {
        while let Some(id) = self.pop_for_worker(worker) {
            if self.task_state(id) != Some(RuntimeTaskState::Ready) {
                continue;
            }
            // Cooperative cancellation boundary (willow-0a6k.7): a cancel-
            // requested task is finalized here instead of being polled. If it
            // has a cleanup entry, hand it to the worker as Cancelling first —
            // the worker runs cancel_fn WITHOUT the scheduler lock, then
            // finalizes (willow-vynv.3).
            if self
                .tasks
                .get(&id)
                .is_some_and(|task| task.cancel_requested)
            {
                let has_cleanup = self
                    .tasks
                    .get(&id)
                    .is_some_and(|task| task.cancel.is_some() && !task.frame.is_null());
                if has_cleanup {
                    if let Some(task) = self.tasks.get_mut(&id) {
                        task.state = RuntimeTaskState::Cancelling;
                    }
                    return Some(id);
                }
                self.finalize_cancelled(id);
                continue;
            }
            self.set_running(id);
            return Some(id);
        }
        None
    }

    /// Mark a cancel-requested task Cancelled without polling it, and WAKE its
    /// registered awaiters — a task Parked in `await`/`join` on this one would
    /// otherwise sleep forever (willow-vynv.1). The frame root is released by
    /// the outer drive's completed-frame sweep; netpoll registrations owned by
    /// the task are purged so its fds do not linger in the poller.
    fn finalize_cancelled(&mut self, id: RuntimeTaskId) {
        let waiters = if let Some(task) = self.tasks.get_mut(&id) {
            task.state = RuntimeTaskState::Cancelled;
            std::mem::take(&mut task.waiters)
        } else {
            return;
        };
        for waiter in waiters {
            self.wake(waiter);
        }
        self.pending_netpoll_purge.push(id);
    }

    /// The cleanup entry + frame for a task the claim just moved to
    /// Cancelling (willow-vynv.3). Consumes the entry so it runs once.
    pub fn take_cancel_work(
        &mut self,
        id: RuntimeTaskId,
    ) -> Option<(RuntimeCancelFn, *mut c_void)> {
        let task = self.tasks.get_mut(&id)?;
        if task.state != RuntimeTaskState::Cancelling {
            return None;
        }
        let cancel = task.cancel.take()?;
        Some((cancel, task.frame))
    }

    /// Take the cancelled task ids whose netpoll registrations still need
    /// purging (drained by the run loop OUTSIDE the scheduler lock).
    fn take_pending_netpoll_purge(&mut self) -> Vec<RuntimeTaskId> {
        std::mem::take(&mut self.pending_netpoll_purge)
    }

    /// True if `id` is queued anywhere (any local queue or the global queue).
    fn is_queued(&self, id: RuntimeTaskId) -> bool {
        self.global.contains(&id) || self.locals.iter().any(|q| q.contains(&id))
    }

    /// Total runnable tasks across all queues.
    fn ready_total(&self) -> usize {
        self.global.len() + self.locals.iter().map(|q| q.len()).sum::<usize>()
    }

    pub fn spawn_placeholder(&mut self) -> RuntimeTaskId {
        let id = self.next_task_id;
        self.next_task_id += 1;
        let task = RuntimeTask::new(id);
        self.tasks.insert(id, task);
        self.enqueue_ready(id);
        id
    }

    pub fn spawn_parked_placeholder(&mut self) -> RuntimeTaskId {
        let id = self.next_task_id;
        self.next_task_id += 1;
        let mut task = RuntimeTask::new(id);
        task.park();
        self.tasks.insert(id, task);
        id
    }

    /// Spawn a cooperative task that runs `poll` over `frame`. The task starts
    /// ready; the caller is responsible for keeping `frame` GC-reachable (the
    /// runtime ABI roots it).
    pub fn spawn_task(&mut self, poll: RuntimePollFn, frame: *mut c_void) -> RuntimeTaskId {
        let id = self.next_task_id;
        self.next_task_id += 1;
        let mut task = RuntimeTask::new(id);
        task.poll = Some(poll);
        task.frame = frame;
        task.frame_rooted = !frame.is_null();
        self.tasks.insert(id, task);
        self.enqueue_ready(id);
        id
    }

    /// The cooperative resume entry, frame, and stable preemption flag for an
    /// executable task.
    pub fn task_work(
        &self,
        id: RuntimeTaskId,
    ) -> Option<(RuntimePollFn, *mut c_void, *const c_void)> {
        let task = self.tasks.get(&id)?;
        Some((task.poll?, task.frame, task.preempt_flag_ptr()))
    }

    pub fn set_running(&mut self, id: RuntimeTaskId) {
        set_current_task(Some(id));
        if let Some(task) = self.tasks.get_mut(&id) {
            task.state = RuntimeTaskState::Running;
            task.wake_requested = false;
            task.yield_requested = false;
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
        let deadline = Instant::now() + Duration::from_millis(millis.max(0) as u64);
        if let Some(id) = current_task_id()
            && let Some(task) = self.tasks.get_mut(&id)
        {
            task.wake_deadline = Some(deadline);
            self.timers.push(Reverse(TimerWake {
                deadline,
                task_id: id,
            }));
        }
    }

    fn timer_is_current(&self, wake: TimerWake) -> bool {
        self.tasks.get(&wake.task_id).is_some_and(|task| {
            matches!(
                task.state,
                RuntimeTaskState::Parked | RuntimeTaskState::Running
            ) && task.wake_deadline == Some(wake.deadline)
        })
    }

    fn prune_stale_timers(&mut self) {
        while let Some(Reverse(wake)) = self.timers.peek().copied() {
            if self.timer_is_current(wake) {
                break;
            }
            self.timers.pop();
        }
    }

    /// The parked task with the earliest wake-deadline, if any. Backed by a
    /// min-heap so idle scheduling does not scan every parked task (willow-gyaa.3).
    fn next_timer_deadline(&mut self) -> Option<(RuntimeTaskId, Instant)> {
        self.prune_stale_timers();
        self.timers
            .peek()
            .map(|Reverse(wake)| (wake.task_id, wake.deadline))
    }

    fn pop_due_timer(&mut self, now: Instant) -> Option<RuntimeTaskId> {
        loop {
            let wake = self.timers.peek().copied()?.0;
            if !self.timer_is_current(wake) {
                self.timers.pop();
                continue;
            }
            if wake.deadline > now {
                return None;
            }
            self.timers.pop();
            return Some(wake.task_id);
        }
    }

    /// Move every due timer directly from the timer heap to the ready queue.
    ///
    /// This transition must happen under one scheduler lock. If a worker removes
    /// the last timer and releases the lock before waking its task, another
    /// worker can observe neither a timer nor runnable work and incorrectly
    /// return from `run_until` while the target is still parked.
    fn wake_due_timers(&mut self, now: Instant) -> usize {
        let mut woken = 0;
        while let Some(id) = self.pop_due_timer(now) {
            self.wake(id);
            woken += 1;
        }
        woken
    }

    pub fn complete(&mut self, id: RuntimeTaskId) {
        let waiters = if let Some(task) = self.tasks.get_mut(&id) {
            task.complete();
            std::mem::take(&mut task.waiters)
        } else {
            Vec::new()
        };
        // Dependency wake: tasks awaiting this one become runnable again
        // (willow-lpn.5.3).
        for waiter in waiters {
            self.wake(waiter);
        }
    }

    /// Register `waiter` to be woken when `awaitee` completes (for `await
    /// <task>`). No-op if `awaitee` is unknown.
    pub fn register_waiter(&mut self, awaitee: RuntimeTaskId, waiter: RuntimeTaskId) {
        if let Some(task) = self.tasks.get_mut(&awaitee)
            && !task.waiters.contains(&waiter)
        {
            task.waiters.push(waiter);
        }
    }

    pub fn pop_ready(&mut self) -> Option<RuntimeTaskId> {
        self.pop_for_worker(0)
    }

    pub fn task(&self, id: RuntimeTaskId) -> Option<&RuntimeTask> {
        self.tasks.get(&id)
    }

    pub fn task_mut(&mut self, id: RuntimeTaskId) -> Option<&mut RuntimeTask> {
        self.tasks.get_mut(&id)
    }

    pub fn tasks(&self) -> impl Iterator<Item = &RuntimeTask> {
        self.tasks.values()
    }

    pub fn task_count(&self) -> usize {
        self.tasks.len()
    }

    pub fn ready_len(&self) -> usize {
        self.ready_total()
    }

    pub fn task_state(&self, id: RuntimeTaskId) -> Option<RuntimeTaskState> {
        self.tasks.get(&id).map(|task| task.state)
    }

    pub fn park(&mut self, id: RuntimeTaskId) {
        if let Some(task) = self.tasks.get_mut(&id) {
            task.park();
        }
    }

    pub fn wake(&mut self, id: RuntimeTaskId) {
        let mut enqueue = false;
        if let Some(task) = self.tasks.get_mut(&id) {
            match task.state {
                RuntimeTaskState::Parked => {
                    task.wake();
                    enqueue = true;
                }
                RuntimeTaskState::Running => {
                    task.wake_requested = true;
                    task.wake_deadline = None;
                }
                _ => {}
            }
        }
        if enqueue {
            self.enqueue_ready(id);
        }
    }

    /// Mark the currently-running task for a cooperative yield. The actual
    /// requeue happens after the poll returns Pending, so another worker cannot
    /// pick up the same frame while it is still being polled.
    pub fn request_running_yield(&mut self) {
        if let Some(id) = current_task_id()
            && let Some(task) = self.tasks.get_mut(&id)
        {
            task.yield_requested = true;
        }
    }

    /// Requeue a task that returned a *runnable* poll code — `RUNTIME_POLL_YIELD`
    /// (voluntary) or `RUNTIME_POLL_PREEMPTED` (forced at a safepoint, spec §7).
    /// Unlike a Pending poll it is not waiting on an event, so it goes straight
    /// back on the ready queue instead of parking.
    pub fn requeue_runnable(&mut self, id: RuntimeTaskId) {
        if let Some(task) = self.tasks.get_mut(&id) {
            task.state = RuntimeTaskState::Ready;
            task.wake_requested = false;
            task.yield_requested = false;
        }
        if !self.is_queued(id) {
            self.enqueue_ready(id);
        }
    }

    /// Finish a Pending poll. If a wake/yield raced with the Running state, make
    /// the task Ready now; otherwise park it until a future wake.
    pub fn finish_pending_poll(&mut self, id: RuntimeTaskId) {
        let should_requeue = if let Some(task) = self.tasks.get_mut(&id) {
            // A cancel that landed while this task was RUNNING could only set
            // the flag; parking now would strand it (nothing re-queues it, and
            // a stray later wake could even re-POLL a half-cancelled task).
            // Re-queue instead so the next claim finalizes it (willow-vynv.1).
            let should_requeue =
                task.wake_requested || task.yield_requested || task.cancel_requested;
            task.wake_requested = false;
            task.yield_requested = false;
            task.park();
            should_requeue
        } else {
            false
        };
        if should_requeue && !self.is_queued(id) {
            self.wake(id);
        }
    }
}

impl GcTrace for RuntimeScheduler {
    fn trace(&self, visitor: &mut GcVisitor) {
        for task in self.tasks.values() {
            task.trace(visitor);
        }
    }
}

// ─── Process-global cooperative scheduler (willow-fqg.1 / willow-gyaa.4) ─────
//
// A shared run queue that drives compiler-generated cooperative tasks. Each task
// owns a heap async frame; the frame is registered as a GC runtime root while
// the task is pending/running, so a parked/ready task's live values survive
// collection even though no native stack frame holds them (spec §8.2 / §9).

static GLOBAL_SCHEDULER: LazyLock<Mutex<RuntimeScheduler>> =
    LazyLock::new(|| Mutex::new(RuntimeScheduler::default()));

fn with_global<R>(f: impl FnOnce(&mut RuntimeScheduler) -> R) -> R {
    let mut sched = GLOBAL_SCHEDULER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut sched)
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
}

fn current_worker() -> usize {
    CURRENT_WORKER.with(Cell::get)
}

/// Spawn a cooperative task on the global scheduler. The frame is rooted with
/// the GC so it (and the values it references) survives collection while the
/// task is pending. Returns the task id.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_spawn(poll: RuntimePollFn, frame: *mut c_void) -> u64 {
    // Keep the frame (and everything it transitively references) alive while the
    // task is pending. Removed on completion in `willow_sched_run`.
    crate::gc::willow_gc_add_runtime_root(frame as *mut u8);
    let id = with_global(|sched| sched.spawn_task(poll, frame));
    crate::gc::stress_collect("scheduler");
    id
}

/// Wake a parked task, re-queueing it as ready.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_wake(id: u64) {
    crate::gc::stress_collect("scheduler");
    with_global(|sched| sched.wake(id));
    crate::gc::stress_collect("scheduler");
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
    with_global(|sched| {
        let Some(task) = sched.tasks.get_mut(&id) else {
            return;
        };
        match task.state {
            RuntimeTaskState::Completed
            | RuntimeTaskState::Panicked
            | RuntimeTaskState::Cancelling
            | RuntimeTaskState::Cancelled => {}
            _ => {
                task.cancel_requested = true;
                if task.state == RuntimeTaskState::Parked {
                    task.state = RuntimeTaskState::Ready;
                    task.wake_deadline = None;
                    sched.enqueue_ready(id);
                }
            }
        }
    });
}

/// Record the source location of the call that spawned task `id` (file is a
/// WillowString; copied out of the GC heap). Shown in panic/debug traces
/// (willow-0a6k.7).
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_set_spawn_site(id: u64, file: *const u8, line: i64) {
    let file = unsafe { crate::string::willow_string_as_str(file) }.to_string();
    let id = id as RuntimeTaskId;
    with_global(|sched| {
        if let Some(task) = sched.tasks.get_mut(&id) {
            task.spawn_site = Some((file, line as u32));
        }
    });
}

/// True (1) if `id` was cancel-requested or already finalized as Cancelled.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_is_cancelled(id: u64) -> i64 {
    let id = id as RuntimeTaskId;
    with_global(|sched| {
        sched
            .tasks
            .get(&id)
            .is_some_and(|task| task.cancel_requested || task.state == RuntimeTaskState::Cancelled)
    }) as i64
}

/// Post-join check (willow-0a6k.7): joining a CANCELLED task has no result to
/// read, so it is a located runtime panic (mirrors the panic reporting style).
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_join_check(id: u64) {
    let id = id as RuntimeTaskId;
    let cancelled = with_global(|sched| {
        sched
            .tasks
            .get(&id)
            .is_some_and(|task| task.state == RuntimeTaskState::Cancelled)
    });
    if cancelled {
        eprintln!("runtime panic: awaited/joined a cancelled task (task {id})");
        crate::stack_trace::print_current_call_stack();
        let chain = async_chain_text();
        if !chain.is_empty() {
            eprintln!("{chain}");
        }
        std::process::abort();
    }
}

/// Attach the compiler-generated cancellation cleanup entry to a task
/// (willow-vynv.3). Called by the async-fn constructor right after spawn.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_set_cancel_fn(id: u64, cancel: RuntimeCancelFn) {
    let id = id as RuntimeTaskId;
    with_global(|sched| {
        if let Some(task) = sched.tasks.get_mut(&id) {
            task.cancel = Some(cancel);
        }
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
    with_global(|sched| {
        if let Some(id) = current_task_id()
            && let Some(task) = sched.task_mut(id)
        {
            task.name = Some(name);
        }
    });
}

/// Render the active async chain (currently-running task first, then the tasks
/// awaiting it, transitively) for panic diagnostics. Empty when no async task is
/// running (willow-9lw).
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
            let name = task.name.as_deref().unwrap_or("<async task>");
            let site = match &task.spawn_site {
                Some((file, line)) => format!(" [task {id}, spawned at {file}:{line}]"),
                None => format!(" [task {id}]"),
            };
            lines.push(format!("  {}: async {}{}", lines.len(), name, site));
            // The first waiter is the awaiter that suspended on this task.
            match task.waiters.first() {
                Some(&awaiter) => id = awaiter,
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

/// Register a wake-deadline on the currently-running task: after the poll fn
/// returns Pending, the timer-aware run loop wakes it once `millis` elapse.
/// Called by a cooperative poll fn that is awaiting a sleep (willow-lpn.5.3).
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_sleep(millis: i64) {
    with_global(|sched| sched.set_running_wake_after_millis(millis));
    crate::gc::stress_collect("await");
}

/// Cooperatively yield the currently-running task. The compiler emits this from
/// `await yield()` immediately before returning Pending from the poll fn.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_yield() {
    with_global(|sched| sched.request_running_yield());
    crate::gc::stress_collect("await");
}

/// Await another task's completion (for `await <task>`): returns 1 if `awaitee`
/// has already completed (the caller may read its result and continue), else
/// registers the currently-running task as a waiter and returns 0 — the caller
/// then returns Pending and is woken when `awaitee` completes (willow-lpn.5.3).
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_await(awaitee: u64) -> i32 {
    let ready = with_global(|sched| match sched.task_state(awaitee) {
        Some(RuntimeTaskState::Completed) => 1,
        Some(_) => {
            if let Some(waiter) = current_task_id() {
                sched.register_waiter(awaitee, waiter);
            }
            0
        }
        // Unknown task: treat as ready to avoid a permanent park.
        None => 1,
    });
    crate::gc::stress_collect("await");
    ready
}

/// Current state of a task as an integer: 0 ready, 1 running, 2 parked,
/// 3 completed, 4 panicked, 5 cancelled, -1 unknown.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_task_state(id: u64) -> i32 {
    with_global(|sched| match sched.task_state(id) {
        Some(RuntimeTaskState::Ready) => 0,
        Some(RuntimeTaskState::Running) => 1,
        Some(RuntimeTaskState::Parked) => 2,
        Some(RuntimeTaskState::Completed) => 3,
        Some(RuntimeTaskState::Panicked) => 4,
        Some(RuntimeTaskState::Cancelled) => 5,
        Some(RuntimeTaskState::Cancelling) => 6,
        None => -1,
    })
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
    sched_run_with_mutator(None)
}

/// Drive the scheduler only until `target` completes (or the scheduler goes
/// genuinely idle), then return — the `join()`/`await` of a concrete task
/// handle (willow-bsqy). Reuses the mutator-registration wrapper so GC
/// coordination is identical to `willow_sched_run`.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_run_until(target: u64) -> i64 {
    sched_run_with_mutator(Some(target))
}

fn sched_run_with_mutator(target: Option<RuntimeTaskId>) -> i64 {
    // Register the driver thread as a GC mutator while it drives tasks so a
    // future parallel collector can stop it at a safepoint. Single-mutator runs
    // have exactly one registered thread, so `multi_mutator_active()` stays false
    // and GC behavior is unchanged (willow-6fv.5.6).
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
        willow_sched_run_parallel(target, active_workers)
    } else if let Some(state) = shared_state.as_deref() {
        scheduler_run_loop(target, current_worker(), Some(state), false)
    } else {
        scheduler_run_loop(target, current_worker(), None, false)
    };
    if let Some(id) = saved_running {
        set_current_task(Some(id));
        let preempt_flag = with_global(|sched| {
            if let Some(task) = sched.task_mut(id) {
                task.state = RuntimeTaskState::Running;
                return Some(task.preempt_flag_ptr());
            }
            None
        });
        if let Some(flag) = preempt_flag {
            crate::preempt::willow_preempt_begin(flag);
        }
    }
    if paused_parallel_poll && let Some(state) = shared_state.as_ref() {
        let previous = state.paused_polls.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "parallel paused poll underflow");
        state.active_polls.fetch_add(1, Ordering::AcqRel);
    }
    if outermost {
        release_completed_frame_roots();
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

fn release_completed_frame_roots() {
    let frames = with_global(|sched| {
        sched
            .tasks
            .values_mut()
            .filter_map(|task| {
                if matches!(
                    task.state,
                    RuntimeTaskState::Completed | RuntimeTaskState::Cancelled
                ) && task.frame_rooted
                {
                    task.frame_rooted = false;
                    Some(task.frame as *mut u8)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
    });
    for frame in frames {
        crate::gc::willow_gc_remove_runtime_root(frame);
    }
}

#[derive(Debug, Default)]
struct ParallelRunState {
    stop: AtomicBool,
    active_polls: AtomicUsize,
    paused_polls: AtomicUsize,
    completed: AtomicI64,
}

fn willow_sched_run_parallel(target: Option<RuntimeTaskId>, workers: usize) -> i64 {
    let state = Arc::new(ParallelRunState::default());
    std::thread::scope(|scope| {
        for worker in 1..workers {
            let state = Arc::clone(&state);
            scope.spawn(move || {
                run_parallel_worker(worker, target, state);
            });
        }
        let main_state = Arc::clone(&state);
        with_parallel_context(0, main_state, || {
            scheduler_run_loop(target, 0, Some(state.as_ref()), true);
        });
        state.stop.store(true, Ordering::Release);
        // A worker can pass its loop-level stop check immediately before worker
        // 0 publishes the stop. Synchronize with the scheduler lock used for
        // claiming so no task can become active after this barrier, then remain
        // a cooperating mutator until every in-flight/nested poll has crossed
        // its post-poll GC boundaries.
        with_global(|_| ());
        while state.active_polls.load(Ordering::Acquire) > 0
            || state.paused_polls.load(Ordering::Acquire) > 0
        {
            crate::gc::willow_gc_safepoint();
            std::thread::yield_now();
        }
    });
    state.completed.load(Ordering::Acquire)
}

fn run_parallel_worker(worker: usize, target: Option<RuntimeTaskId>, state: Arc<ParallelRunState>) {
    SCHED_RUN_DEPTH.with(|depth| depth.set(1));
    crate::gc::willow_gc_register_mutator();
    let worker_state = Arc::clone(&state);
    with_parallel_context(worker, worker_state, || {
        scheduler_run_loop(target, worker, Some(state.as_ref()), true);
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
    with_global(|sched| {
        !matches!(
            sched.task_state(t),
            Some(RuntimeTaskState::Ready)
                | Some(RuntimeTaskState::Running)
                | Some(RuntimeTaskState::Parked)
                | Some(RuntimeTaskState::Cancelling)
        )
    })
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

fn duration_until(deadline: Instant) -> Duration {
    deadline
        .checked_duration_since(Instant::now())
        .unwrap_or_default()
}

fn bounded_parallel_wait(duration: Duration) -> Duration {
    std::cmp::min(duration, Duration::from_millis(1))
}

fn scheduler_idle_step(
    worker: usize,
    shared: Option<&ParallelRunState>,
    keep_alive_for_paused: bool,
) -> bool {
    let parallel = shared.is_some();

    // A worker may have claimed the last ready task immediately before this
    // worker observed an empty queue. Read the poll count only after that queue
    // observation: using a value captured earlier can falsely declare global
    // idle while the other worker is still publishing a timer/netpoll waiter.
    if shared.is_some_and(|state| state.active_polls.load(Ordering::Acquire) > 0) {
        std::thread::sleep(Duration::from_millis(1));
        return true;
    }

    let earliest = with_global(|sched| sched.next_timer_deadline());
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
        Some((_, deadline)) => {
            let wait = duration_until(deadline);
            if !wait.is_zero() {
                let wait = if parallel {
                    bounded_parallel_wait(wait)
                } else {
                    wait
                };
                std::thread::sleep(wait);
            }
            let woken = with_global(|sched| sched.wake_due_timers(Instant::now()));
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
        None => false,
    }
}

fn scheduler_run_loop(
    target: Option<RuntimeTaskId>,
    worker: usize,
    shared: Option<&ParallelRunState>,
    stop_pool_on_exit: bool,
) -> i64 {
    let mut completed = 0i64;
    loop {
        if shared.is_some_and(|state| state.stop.load(Ordering::Acquire)) {
            break;
        }
        // Stop as soon as the TARGET task (a `join()`/`await` of a concrete
        // handle) is done, instead of draining the whole scheduler to quiescence
        // — so joining one task does not run unrelated tasks to completion and
        // cannot hang on an unrelated non-terminating task (willow-bsqy). A
        // completed task may have been pruned (state None); treat that as done
        // too — the joiner reads the result from the frame, not the task.
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
                let stopped = with_global(|_| {
                    if state.active_polls.load(Ordering::Acquire) > 0
                        || state.paused_polls.load(Ordering::Acquire) > 0
                    {
                        false
                    } else {
                        state.stop.store(true, Ordering::Release);
                        true
                    }
                });
                if !stopped {
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
        let (woken_timers, next) = with_global(|sched| {
            // A runnable CPU task can keep the ready queue non-empty forever.
            // Promote expired timers before selecting work so those tasks still
            // get a turn without waiting for the scheduler to become idle.
            let woken_timers = sched.wake_due_timers(Instant::now());
            let next = (!shared.is_some_and(|state| state.stop.load(Ordering::Acquire)))
                .then(|| sched.claim_ready_for_worker(worker))
                .flatten()
                .map(|id| {
                    if let Some(state) = shared {
                        state.active_polls.fetch_add(1, Ordering::AcqRel);
                    }
                    (id, sched.task_work(id))
                });
            (woken_timers, next)
        });
        // Purge netpoll registrations of tasks finalized as Cancelled by the
        // claim above — outside the scheduler lock (willow-vynv.1).
        let purge = with_global(|sched| sched.take_pending_netpoll_purge());
        for cancelled in purge {
            crate::netpoll::purge_task(cancelled);
        }
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
            if scheduler_idle_step(worker, shared, stop_pool_on_exit || target.is_some()) {
                continue;
            }
            if stop_pool_on_exit && let Some(state) = shared {
                // Revalidate global idleness under the scheduler lock. Work can
                // be published between the earlier empty pop and this point;
                // stopping without this check strands that task in the queue.
                let stopped = with_global(|sched| {
                    if state.active_polls.load(Ordering::Acquire) > 0
                        || state.paused_polls.load(Ordering::Acquire) > 0
                        || sched.ready_total() > 0
                        || sched.next_timer_deadline().is_some()
                    {
                        false
                    } else {
                        state.stop.store(true, Ordering::Release);
                        true
                    }
                });
                if !stopped || crate::netpoll::has_waiters() {
                    state.stop.store(false, Ordering::Release);
                    continue;
                }
            }
            break;
        };
        // A task the claim moved to Cancelling: run its cleanup entry WITHOUT
        // the scheduler lock (poll-like), then finalize as Cancelled
        // (willow-vynv.3). The frame stays rooted until finalization.
        let cancel_work = with_global(|sched| sched.take_cancel_work(id));
        if let Some((cancel_fn, cancel_frame)) = cancel_work {
            unsafe { cancel_fn(cancel_frame) };
            with_global(|sched| {
                sched.finalize_cancelled(id);
                sched.clear_running();
            });
            let purge = with_global(|sched| sched.take_pending_netpoll_purge());
            for cancelled in purge {
                crate::netpoll::purge_task(cancelled);
            }
            crate::gc::stress_collect("scheduler");
            finish_active_poll(shared);
            continue;
        }
        let Some((poll, frame, preempt_flag)) = work else {
            // Placeholder task with no executable work: just complete it.
            with_global(|sched| {
                sched.complete(id);
                sched.clear_running();
            });
            crate::gc::stress_collect("await");
            crate::gc::stress_collect("scheduler");
            finish_active_poll(shared);
            record_completed_task(&mut completed, shared);
            continue;
        };
        crate::gc::stress_collect("await");
        crate::preempt::willow_preempt_begin(preempt_flag);
        let result = unsafe { poll(frame) };
        crate::preempt::willow_preempt_end();
        with_global(|sched| {
            if result == RUNTIME_POLL_READY {
                sched.complete(id);
            } else if result == RUNTIME_POLL_YIELD || result == RUNTIME_POLL_PREEMPTED {
                // Runnable outcome (spec §7): gave up the worker but is not
                // waiting on an event — requeue immediately. Emitted once
                // compiler-generated safepoints (willow-0a6k.2). (Panic
                // propagation for RUNTIME_POLL_PANICKED is willow-0a6k.7.)
                sched.requeue_runnable(id);
            } else {
                sched.finish_pending_poll(id);
            }
            // Done polling this task: drop the running marker so a later
            // out-of-poll willow_sched_sleep/await does not target a stale task.
            sched.clear_running();
        });
        crate::gc::stress_collect("await");
        crate::gc::stress_collect("scheduler");
        // Keep this worker visible as active through the post-poll GC
        // boundaries. Otherwise worker 0 can leave the scoped pool and wait to
        // join this worker while its collection is waiting for worker 0 to
        // reach a safepoint.
        finish_active_poll(shared);
        if result == RUNTIME_POLL_READY {
            record_completed_task(&mut completed, shared);
        }
    }
    completed
}

/// Test-only: reset the global scheduler between unit tests (the heap and
/// scheduler are process-global, so tests must run single-threaded).
#[cfg(test)]
pub fn reset_global_scheduler_for_test() {
    with_global(|sched| *sched = RuntimeScheduler::default());
    set_current_task(None);
    CURRENT_WORKER.with(|worker| worker.set(0));
    CURRENT_RUN_STATE.with(|state| {
        state.replace(None);
    });
}

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
    use std::sync::{LazyLock as TestLazyLock, Mutex as TestMutex};

    static NESTED_QUANTUM_TARGET: TestAtomicU64 = TestAtomicU64::new(0);
    static NESTED_QUANTUM_RESTORED: TestAtomicBool = TestAtomicBool::new(false);

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
        s.enqueue_local(0, 1);
        // pop_ready() is the worker-0 view used by the cooperative run loop.
        assert_eq!(s.pop_ready(), Some(1));
    }

    #[test]
    fn workqueue_claim_discards_duplicate_entry_for_running_task() {
        let mut scheduler = RuntimeScheduler::with_worker_count(5);
        let id = scheduler.spawn_placeholder();
        scheduler.enqueue_ready(id);

        assert_eq!(scheduler.claim_ready_for_worker(0), Some(id));
        assert_eq!(scheduler.task_state(id), Some(RuntimeTaskState::Running));
        assert_eq!(
            scheduler.claim_ready_for_worker(1),
            None,
            "a stale duplicate must not let another worker poll a Running task"
        );
        scheduler.clear_running();
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
        with_global(|sched| *sched = RuntimeScheduler::with_worker_count(2));
        PARALLEL_POLL_THREADS
            .lock()
            .expect("parallel poll thread log poisoned")
            .clear();
        PARALLEL_POLL_ENTERED.store(0, TestOrdering::SeqCst);

        let a = willow_sched_spawn(poll_record_parallel_worker, std::ptr::null_mut());
        let b = willow_sched_spawn(poll_record_parallel_worker, std::ptr::null_mut());

        crate::gc::willow_gc_register_mutator();
        let completed = willow_sched_run_parallel(None, 2);
        crate::gc::willow_gc_unregister_mutator();

        assert_eq!(completed, 2);
        assert_eq!(willow_sched_task_state(a), 3);
        assert_eq!(willow_sched_task_state(b), 3);
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
        with_global(|sched| *sched = RuntimeScheduler::with_worker_count(2));
        WAKE_RACE_WAITER_REGISTERED.store(0, TestOrdering::SeqCst);

        let b = willow_sched_spawn(poll_complete_after_waiter_registered, std::ptr::null_mut());
        let a_frame = willow_async_frame_alloc(2, 0) as *mut c_void;
        unsafe {
            let base = a_frame as *mut u8;
            *(base.add(async_frame_slot_offset(0)) as *mut u64) = b;
        }
        let a = willow_sched_spawn(poll_await_with_running_wake_race, a_frame);

        crate::gc::willow_gc_register_mutator();
        let completed = willow_sched_run_parallel(None, 2);
        crate::gc::willow_gc_unregister_mutator();

        assert_eq!(
            completed, 2,
            "awaiter must be requeued when its dependency wakes it before park"
        );
        assert_eq!(willow_sched_task_state(a), 3);
        assert_eq!(willow_sched_task_state(b), 3);
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
            sched.task_mut(inner).unwrap().name = Some("inner".to_string());
            sched.task_mut(main).unwrap().name = Some("main".to_string());
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
        assert_eq!(willow_sched_task_state(id), 3); // Completed
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
        assert_eq!(willow_sched_task_state(id), 3); // Completed
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
        assert_eq!(willow_sched_task_state(id), 3); // Completed
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
        assert_eq!(willow_sched_task_state(id), 3); // Completed
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
        assert_eq!(willow_sched_task_state(a_id), 3); // A Completed
        assert_eq!(willow_sched_task_state(b_id), 3); // B Completed
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
        assert_eq!(willow_sched_task_state(id), 3); // Completed

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
        assert_eq!(willow_sched_task_state(a), 3);
        assert_eq!(willow_sched_task_state(b), 3);
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
    fn scheduler_traces_all_task_roots() {
        let mut scheduler = RuntimeScheduler::default();
        let first = scheduler.spawn_placeholder();
        let second = scheduler.spawn_placeholder();
        scheduler.task_mut(first).unwrap().roots.push(10);
        scheduler.task_mut(second).unwrap().roots.push(20);

        let mut visitor = GcVisitor::default();
        scheduler.trace(&mut visitor);
        let mut roots = visitor.into_roots();
        roots.sort_unstable();

        assert_eq!(roots, vec![10, 20]);
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

        assert_eq!(scheduler.pop_due_timer(Instant::now()), Some(id));
        assert_eq!(scheduler.pop_due_timer(Instant::now()), None);

        scheduler.wake(id);
        assert_eq!(scheduler.task_state(id), Some(RuntimeTaskState::Ready));
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

    // ── willow-vynv.1: cancel runtime integrity ─────────────────────────────

    #[test]
    fn cancel_finalize_wakes_parked_awaiter() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let target = s.spawn_placeholder();
        let awaiter = s.spawn_parked_placeholder();
        s.register_waiter(target, awaiter);

        // Cancel-request the target, then let the claim boundary finalize it.
        if let Some(task) = s.tasks.get_mut(&target) {
            task.cancel_requested = true;
        }
        // The claim boundary finalizes the target (never claims it), wakes
        // the parked awaiter, and the SAME claim then picks the awaiter up.
        assert_eq!(
            s.claim_ready_for_worker(0),
            Some(awaiter),
            "finalize must wake the awaiter, which is then claimable"
        );
        assert_eq!(s.task_state(target), Some(RuntimeTaskState::Cancelled));
        assert_eq!(s.task_state(awaiter), Some(RuntimeTaskState::Running));
        s.clear_running();
    }

    #[test]
    fn cancel_cleared_deadline_invalidates_stale_timer_entry() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        s.park(id);
        let deadline = Instant::now();
        if let Some(task) = s.tasks.get_mut(&id) {
            task.wake_deadline = Some(deadline);
        }
        s.timers.push(Reverse(TimerWake {
            deadline,
            task_id: id,
        }));
        // Cancellation clears the deadline (willow_sched_cancel behavior).
        if let Some(task) = s.tasks.get_mut(&id) {
            task.cancel_requested = true;
            task.state = RuntimeTaskState::Ready;
            task.wake_deadline = None;
        }
        // The wheel's stale entry must be revalidated away, not fire a wake.
        assert_eq!(
            s.pop_due_timer(Instant::now() + std::time::Duration::from_secs(1)),
            None,
            "stale timer entry for a cancelled task must not fire"
        );
    }

    #[test]
    fn wake_is_a_noop_on_cancelled_tasks() {
        let mut s = RuntimeScheduler::with_worker_count(1);
        let id = s.spawn_placeholder();
        if let Some(task) = s.tasks.get_mut(&id) {
            task.state = RuntimeTaskState::Cancelled;
        }
        s.wake(id);
        assert_eq!(
            s.task_state(id),
            Some(RuntimeTaskState::Cancelled),
            "wake must not resurrect a cancelled task"
        );
        // A stale queue entry (from spawn) may remain; the claim boundary
        // must skip it rather than run the cancelled task.
        assert_eq!(s.claim_ready_for_worker(0), None);
    }
}
