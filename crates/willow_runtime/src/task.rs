use crate::stack_trace::RuntimeStackTrace;
use crate::trace::{GcTrace, GcVisitor};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

pub use crate::trace::GcRootSet;

pub type RuntimeTaskId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTaskState {
    Ready,
    Running,
    Parked,
    Completed,
    Panicked,
    /// Cooperatively cancelled (willow-0a6k.7): the task was cancel-requested
    /// and reached a scheduler boundary without being polled again. Joining a
    /// cancelled task is a runtime panic.
    Cancelled,
}

/// A compiler-generated cooperative resume entry point. Given the task's heap
/// async frame, it advances the state machine and returns a [`RuntimePoll`] code
/// (`0` = Pending, `1` = Ready). It must not block; to wait it registers a wake
/// (timer/channel/dependency) and returns Pending. Re-entrant into the scheduler
/// is allowed (e.g. to spawn or wake other tasks) — the driver holds no borrow
/// across the call.
pub type RuntimePollFn = unsafe extern "C" fn(frame: *mut c_void) -> i32;

/// Poll result codes returned by a [`RuntimePollFn`] (preemption spec §7).
///
/// `Pending`/`Ready` are the cooperative-async base codes; `Yield`/`Preempted`/
/// `Panicked` are the preemptible-task extension (willow-0a6k.1). `Yield` and
/// `Preempted` are both *runnable* outcomes (the scheduler requeues the task)
/// and differ only diagnostically: `Yield` is voluntary, `Preempted` is forced
/// by the runtime at a safepoint. They are emitted once compiler-inserted
/// safepoints land (willow-0a6k.2); the scheduler already honors them.
pub const RUNTIME_POLL_PENDING: i32 = 0;
pub const RUNTIME_POLL_READY: i32 = 1;
pub const RUNTIME_POLL_YIELD: i32 = 2;
pub const RUNTIME_POLL_PREEMPTED: i32 = 3;
pub const RUNTIME_POLL_PANICKED: i32 = 4;

#[derive(Debug)]
pub struct RuntimeTask {
    pub id: RuntimeTaskId,
    pub state: RuntimeTaskState,
    pub roots: GcRootSet,
    pub spawned_from: Option<RuntimeStackTrace>,
    pub stack_trace: RuntimeStackTrace,
    /// Source name of the async fn this task runs, tagged at poll-fn entry. Used
    /// to render async stack traces from the suspended future chain (willow-9lw).
    pub name: Option<String>,
    /// Cooperative resume entry, or `None` for a bookkeeping-only placeholder.
    pub poll: Option<RuntimePollFn>,
    /// Heap async frame passed to `poll`. Kept alive via a GC runtime root while
    /// the task is pending; `null` for placeholders.
    pub frame: *mut c_void,
    /// Whether `frame` still owns the runtime root installed at spawn. Completed
    /// frames stay rooted until the outer scheduler drive ends so an awaiter on
    /// another worker can copy the result before a concurrent collection.
    pub frame_rooted: bool,
    /// When `Some`, this (parked) task should be woken once the instant passes —
    /// set by `willow_sched_sleep` from a poll fn before it returns Pending, and
    /// honored by the timer-aware run loop (willow-lpn.5.3).
    pub wake_deadline: Option<std::time::Instant>,
    /// A wake arrived while this task was still being polled. Parallel workers
    /// cannot enqueue a Running task immediately, so the scheduler converts this
    /// into a ready requeue after the poll returns Pending (willow-gyaa.4).
    pub wake_requested: bool,
    /// Cooperative cancellation flag (willow-0a6k.7): checked when the
    /// scheduler would next poll this task; it is then Cancelled un-polled.
    pub cancel_requested: bool,
    /// Source location of the call that spawned this task (file, line), for
    /// panic/debug traces (willow-0a6k.7).
    pub spawn_site: Option<(String, u32)>,
    /// `await yield()` requested a cooperative requeue while the task was still
    /// Running. The scheduler publishes that requeue only after the poll returns
    /// Pending, avoiding a second worker polling the same frame concurrently.
    pub yield_requested: bool,
    /// Tasks parked awaiting THIS task's completion; woken when it completes
    /// (dependency wake for `await <task>`, willow-lpn.5.3).
    pub waiters: Vec<RuntimeTaskId>,
    /// Stable per-task preemption request flag. Boxed so its address remains
    /// valid while the scheduler releases its lock and polls the task.
    preempt_flag: Box<AtomicBool>,
}

// SAFETY: `RuntimeTask` is only moved between worker threads inside the global
// scheduler mutex. Its raw frame pointer refers to a GC-managed async frame that
// is kept alive by a runtime root while the task is pending/running; generated
// code may move a task between workers only after the Send/Sync checks.
unsafe impl Send for RuntimeTask {}

impl Clone for RuntimeTask {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            state: self.state,
            roots: self.roots.clone(),
            spawned_from: self.spawned_from.clone(),
            stack_trace: self.stack_trace.clone(),
            name: self.name.clone(),
            poll: self.poll,
            frame: self.frame,
            frame_rooted: self.frame_rooted,
            wake_deadline: self.wake_deadline,
            wake_requested: self.wake_requested,
            cancel_requested: self.cancel_requested,
            spawn_site: self.spawn_site.clone(),
            yield_requested: self.yield_requested,
            waiters: self.waiters.clone(),
            preempt_flag: Box::new(AtomicBool::new(self.preempt_flag.load(Ordering::Acquire))),
        }
    }
}

impl RuntimeTask {
    pub fn new(id: RuntimeTaskId) -> Self {
        Self {
            id,
            state: RuntimeTaskState::Ready,
            roots: GcRootSet::default(),
            spawned_from: None,
            stack_trace: RuntimeStackTrace::default(),
            name: None,
            poll: None,
            frame: std::ptr::null_mut(),
            frame_rooted: false,
            wake_deadline: None,
            wake_requested: false,
            cancel_requested: false,
            spawn_site: None,
            yield_requested: false,
            waiters: Vec::new(),
            preempt_flag: Box::new(AtomicBool::new(false)),
        }
    }

    /// Stable address passed to the worker's quantum lifecycle while polling.
    pub fn preempt_flag_ptr(&self) -> *const c_void {
        (&*self.preempt_flag as *const AtomicBool).cast()
    }

    pub fn park(&mut self) {
        self.state = RuntimeTaskState::Parked;
    }

    pub fn wake(&mut self) {
        if self.state == RuntimeTaskState::Parked {
            self.state = RuntimeTaskState::Ready;
            self.wake_deadline = None;
        }
    }

    pub fn complete(&mut self) {
        self.state = RuntimeTaskState::Completed;
        self.wake_deadline = None;
        self.wake_requested = false;
        self.yield_requested = false;
    }
}

impl GcTrace for RuntimeTask {
    fn trace(&self, visitor: &mut GcVisitor) {
        self.roots.trace(visitor);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinHandle<T> {
    task_id: RuntimeTaskId,
    result: Option<T>,
}

impl<T> JoinHandle<T> {
    pub fn pending(task_id: RuntimeTaskId) -> Self {
        Self {
            task_id,
            result: None,
        }
    }

    pub fn complete(task_id: RuntimeTaskId, result: T) -> Self {
        Self {
            task_id,
            result: Some(result),
        }
    }

    pub fn task_id(&self) -> RuntimeTaskId {
        self.task_id
    }

    pub fn join(self) -> Option<T> {
        self.result
    }
}

impl<T: GcTrace> GcTrace for JoinHandle<T> {
    fn trace(&self, visitor: &mut GcVisitor) {
        self.result.trace(visitor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_set_preserves_slots() {
        let mut roots = GcRootSet::default();
        roots.push(10);
        roots.push(20);
        assert_eq!(roots.slots(), &[10, 20]);
        assert_eq!(roots.pop(), Some(20));
    }

    #[test]
    fn task_state_transitions_are_explicit() {
        let mut task = RuntimeTask::new(7);
        task.park();
        assert_eq!(task.state, RuntimeTaskState::Parked);
        task.wake();
        assert_eq!(task.state, RuntimeTaskState::Ready);
        task.complete();
        assert_eq!(task.state, RuntimeTaskState::Completed);
    }

    #[test]
    fn task_traces_owned_roots() {
        let mut task = RuntimeTask::new(1);
        task.roots.push(100);
        task.roots.push(200);

        let mut visitor = GcVisitor::default();
        task.trace(&mut visitor);

        assert_eq!(visitor.roots(), &[100, 200]);
    }

    #[test]
    fn cloned_task_owns_an_independent_preempt_flag() {
        let task = RuntimeTask::new(1);
        let cloned = task.clone();
        assert_ne!(task.preempt_flag_ptr(), cloned.preempt_flag_ptr());

        crate::preempt::willow_preempt_request(task.preempt_flag_ptr());
        assert_eq!(
            crate::preempt::willow_preempt_requested(task.preempt_flag_ptr()),
            1
        );
        assert_eq!(
            crate::preempt::willow_preempt_requested(cloned.preempt_flag_ptr()),
            0
        );
    }
}
