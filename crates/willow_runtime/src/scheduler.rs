use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::ffi::c_void;
use std::time::{Duration, Instant};

use crate::task::{
    RUNTIME_POLL_READY, RuntimePollFn, RuntimeTask, RuntimeTaskId, RuntimeTaskState,
};
use crate::trace::{GcTrace, GcVisitor};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TimerWake {
    deadline: Instant,
    task_id: RuntimeTaskId,
}

const COOPERATIVE_ACTIVE_WORKERS: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeWorkerConfig {
    requested_workers: usize,
    active_workers: usize,
}

impl RuntimeWorkerConfig {
    fn from_env_value(value: Option<&str>, default_workers: usize) -> Self {
        let default_workers = default_workers.max(1);
        let requested_workers = value
            .and_then(|raw| raw.trim().parse::<usize>().ok())
            .filter(|workers| *workers > 0)
            .unwrap_or(default_workers);

        Self {
            requested_workers,
            // The cooperative runtime is still single-worker. Keep this clamp
            // explicit so WILLOW_WORKERS has a stable contract before gyaa.4
            // enables true parallel workers.
            active_workers: requested_workers.min(COOPERATIVE_ACTIVE_WORKERS),
        }
    }

    pub fn requested_workers(self) -> usize {
        self.requested_workers
    }

    pub fn active_workers(self) -> usize {
        self.active_workers
    }

    pub fn is_single_worker(self) -> bool {
        self.active_workers == 1
    }
}

pub fn runtime_worker_config() -> RuntimeWorkerConfig {
    RuntimeWorkerConfig::from_env_value(
        std::env::var("WILLOW_WORKERS").ok().as_deref(),
        std::thread::available_parallelism()
            .map(|workers| workers.get())
            .unwrap_or(1),
    )
}

#[derive(Debug)]
pub struct RuntimeScheduler {
    next_task_id: RuntimeTaskId,
    tasks: HashMap<RuntimeTaskId, RuntimeTask>,
    /// Per-worker local run queues + a shared global queue, with work stealing
    /// (willow-gyaa.4). The cooperative runtime still drives only worker 0
    /// (active_workers == 1); the multi-queue structure is groundwork so true
    /// parallel workers can be enabled once GC mutator registration + STW lands
    /// (willow-6fv.5.6). New/woken tasks go to the global queue; an idle worker
    /// drains its local queue, then the global queue, then steals from the back
    /// of another worker's local queue.
    locals: Vec<VecDeque<RuntimeTaskId>>,
    global: VecDeque<RuntimeTaskId>,
    timers: BinaryHeap<Reverse<TimerWake>>,
    /// The task currently being polled (set by `set_running`), so a poll fn's
    /// `willow_sched_sleep` knows which task to attach the wake-deadline to.
    running: Option<RuntimeTaskId>,
}

impl Default for RuntimeScheduler {
    fn default() -> Self {
        Self::with_worker_count(runtime_worker_config().requested_workers())
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
            running: None,
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
        self.tasks.insert(id, task);
        self.enqueue_ready(id);
        id
    }

    /// The cooperative resume entry + frame for a task, if it is executable.
    pub fn task_work(&self, id: RuntimeTaskId) -> Option<(RuntimePollFn, *mut c_void)> {
        let task = self.tasks.get(&id)?;
        Some((task.poll?, task.frame))
    }

    pub fn set_running(&mut self, id: RuntimeTaskId) {
        self.running = Some(id);
        if let Some(task) = self.tasks.get_mut(&id) {
            task.state = RuntimeTaskState::Running;
        }
    }

    /// Clear the "currently running" marker once a poll returns. Guards
    /// `willow_sched_sleep` / `willow_sched_await` against attaching a deadline
    /// or waiter to a STALE task when called outside of a poll (willow-lpn.5.3).
    pub fn clear_running(&mut self) {
        self.running = None;
    }

    /// Attach a wake-deadline to the currently-running task (called via
    /// `willow_sched_sleep` from a poll fn before it returns Pending). The
    /// timer-aware run loop wakes the task once the deadline passes.
    pub fn set_running_wake_after_millis(&mut self, millis: i64) {
        let deadline = Instant::now() + Duration::from_millis(millis.max(0) as u64);
        if let Some(id) = self.running
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
            task.state == RuntimeTaskState::Parked && task.wake_deadline == Some(wake.deadline)
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
        if let Some(task) = self.tasks.get_mut(&id) {
            let was_parked = task.state == RuntimeTaskState::Parked;
            task.wake();
            if was_parked {
                self.enqueue_ready(id);
            }
        }
    }

    /// Requeue the currently-running task so a poll fn can cooperatively yield:
    /// after the poll returns Pending the run loop parks it, then this queued id
    /// makes it runnable again behind any already-ready work.
    pub fn requeue_running_for_yield(&mut self) {
        if let Some(id) = self.running
            && self.tasks.contains_key(&id)
            && !self.is_queued(id)
        {
            self.enqueue_ready(id);
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

// ─── Process-global cooperative scheduler (willow-fqg.1) ─────────────────────
//
// A single-threaded run queue that drives compiler-generated cooperative tasks.
// Each task owns a heap async frame; the frame is registered as a GC runtime
// root while the task is pending, so a parked/ready task's live values survive
// collection even though no native stack frame holds them (spec §8.2 / §9).

thread_local! {
    static GLOBAL_SCHEDULER: RefCell<RuntimeScheduler> = RefCell::new(RuntimeScheduler::default());
}

fn with_global<R>(f: impl FnOnce(&mut RuntimeScheduler) -> R) -> R {
    GLOBAL_SCHEDULER.with(|cell| f(&mut cell.borrow_mut()))
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
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_current_task() -> u64 {
    with_global(|sched| sched.running.unwrap_or(0))
}

/// Tag the currently-running task with its async fn name (raw static UTF-8 bytes
/// + length). Emitted at the top of each async poll fn so a panic can render the
/// async chain (willow-9lw). No-op when no task is running.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_tag_current_task(name: *const u8, name_len: i64) {
    if name.is_null() || name_len <= 0 {
        return;
    }
    let bytes = unsafe { std::slice::from_raw_parts(name, name_len as usize) };
    let name = String::from_utf8_lossy(bytes).into_owned();
    with_global(|sched| {
        if let Some(id) = sched.running
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
        let Some(mut id) = sched.running else {
            return String::new();
        };
        let mut lines = Vec::new();
        let mut seen = std::collections::HashSet::new();
        // Walk current task -> its awaiter -> ... via the reverse `waiters` link.
        while seen.insert(id) {
            let Some(task) = sched.task(id) else { break };
            let name = task.name.as_deref().unwrap_or("<async task>");
            lines.push(format!("  {}: async {}", lines.len(), name));
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

/// Requested worker count from WILLOW_WORKERS or logical CPU default.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_requested_workers() -> u64 {
    runtime_worker_config().requested_workers() as u64
}

/// Worker count the current cooperative runtime will actually run. This stays
/// at 1 until the multi-worker scheduler in gyaa.4 lands.
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
    with_global(|sched| sched.requeue_running_for_yield());
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
            if let Some(waiter) = sched.running {
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
/// 3 completed, 4 panicked, -1 unknown.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_task_state(id: u64) -> i32 {
    with_global(|sched| match sched.task_state(id) {
        Some(RuntimeTaskState::Ready) => 0,
        Some(RuntimeTaskState::Running) => 1,
        Some(RuntimeTaskState::Parked) => 2,
        Some(RuntimeTaskState::Completed) => 3,
        Some(RuntimeTaskState::Panicked) => 4,
        None => -1,
    })
}

/// Drive the global scheduler until no task is ready (idle). Each ready task is
/// polled once: `Ready` completes it (and unroots its frame); `Pending` parks it
/// (a waker must later re-queue it). Returns the number of tasks completed.
///
/// The poll function is invoked with **no scheduler borrow held**, so a task may
/// re-enter the scheduler (spawn/wake) from inside its own poll.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_run() -> i64 {
    let mut completed = 0i64;
    loop {
        let next = with_global(|sched| {
            let id = sched.pop_ready()?;
            sched.set_running(id);
            Some((id, sched.task_work(id)))
        });
        let Some((id, work)) = next else {
            // No ready task. If a parked task has a wake-deadline (e.g. it is
            // sleeping), block until the earliest one and wake it, then keep
            // running. If netpoll has parked I/O waiters, wait for readiness
            // first (bounded by the nearest timer deadline) and wake matching
            // tasks. Otherwise there is genuinely nothing left to do
            // (willow-lpn.5.3 / willow-lcw).
            let earliest = with_global(|sched| sched.next_timer_deadline());
            if crate::netpoll::has_waiters() {
                let timeout = earliest.map(|(_, deadline)| {
                    deadline
                        .checked_duration_since(Instant::now())
                        .unwrap_or_default()
                });
                if crate::netpoll::wait_and_wake(timeout) > 0 {
                    crate::gc::stress_collect("scheduler");
                    continue;
                }
            }
            match earliest {
                Some((_, deadline)) => {
                    let now = Instant::now();
                    if deadline > now {
                        std::thread::sleep(deadline - now);
                    }
                    if let Some(wake_id) = with_global(|sched| sched.pop_due_timer(Instant::now()))
                    {
                        with_global(|sched| sched.wake(wake_id));
                        crate::gc::stress_collect("scheduler");
                    }
                    continue;
                }
                None => break,
            }
        };
        let Some((poll, frame)) = work else {
            // Placeholder task with no executable work: just complete it.
            with_global(|sched| {
                sched.complete(id);
                sched.clear_running();
            });
            crate::gc::stress_collect("await");
            crate::gc::stress_collect("scheduler");
            completed += 1;
            continue;
        };
        crate::gc::stress_collect("await");
        let result = unsafe { poll(frame) };
        with_global(|sched| {
            if result == RUNTIME_POLL_READY {
                sched.complete(id);
                crate::gc::willow_gc_remove_runtime_root(frame as *mut u8);
            } else {
                sched.park(id);
            }
            // Done polling this task: drop the running marker so a later
            // out-of-poll willow_sched_sleep/await does not target a stale task.
            sched.clear_running();
        });
        crate::gc::stress_collect("await");
        crate::gc::stress_collect("scheduler");
        if result == RUNTIME_POLL_READY {
            completed += 1;
        }
    }
    completed
}

/// Test-only: reset the global scheduler between unit tests (the heap and
/// scheduler are process-global, so tests must run single-threaded).
#[cfg(test)]
pub fn reset_global_scheduler_for_test() {
    with_global(|sched| *sched = RuntimeScheduler::default());
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

    // ── Work-stealing run queues (willow-gyaa.4 groundwork) ─────────────────

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
        let b_id = unsafe { *(base.add(async_frame_slot_offset(0) as usize) as *const u64) };
        let state = unsafe { &mut *(base.add(async_frame_slot_offset(1) as usize) as *mut i64) };
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
            *(base.add(async_frame_slot_offset(0) as usize) as *mut u64) = b_id;
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
    fn scheduler_worker_config_defaults_to_parallelism_request() {
        let config = RuntimeWorkerConfig::from_env_value(None, 4);
        assert_eq!(config.requested_workers(), 4);
        assert_eq!(config.active_workers(), 1);
        assert!(config.is_single_worker());
    }

    #[test]
    fn scheduler_worker_config_parses_env_override() {
        let config = RuntimeWorkerConfig::from_env_value(Some("8"), 4);
        assert_eq!(config.requested_workers(), 8);
        assert_eq!(config.active_workers(), 1);
    }

    #[test]
    fn scheduler_worker_config_rejects_zero_and_invalid_override() {
        let zero = RuntimeWorkerConfig::from_env_value(Some("0"), 3);
        assert_eq!(zero.requested_workers(), 3);
        assert_eq!(zero.active_workers(), 1);

        let invalid = RuntimeWorkerConfig::from_env_value(Some("many"), 2);
        assert_eq!(invalid.requested_workers(), 2);
        assert_eq!(invalid.active_workers(), 1);
    }

    #[test]
    fn scheduler_active_worker_abi_reports_single_worker() {
        assert_eq!(willow_sched_active_workers(), 1);
        assert!(willow_sched_requested_workers() >= 1);
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
}
