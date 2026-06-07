use crate::stack_trace::RuntimeStackTrace;
use crate::trace::{GcTrace, GcVisitor};
use std::ffi::c_void;

pub use crate::trace::GcRootSet;

pub type RuntimeTaskId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTaskState {
    Ready,
    Running,
    Parked,
    Completed,
    Panicked,
}

/// A compiler-generated cooperative resume entry point. Given the task's heap
/// async frame, it advances the state machine and returns a [`RuntimePoll`] code
/// (`0` = Pending, `1` = Ready). It must not block; to wait it registers a wake
/// (timer/channel/dependency) and returns Pending. Re-entrant into the scheduler
/// is allowed (e.g. to spawn or wake other tasks) — the driver holds no borrow
/// across the call.
pub type RuntimePollFn = unsafe extern "C" fn(frame: *mut c_void) -> i32;

/// Poll result codes returned by a [`RuntimePollFn`].
pub const RUNTIME_POLL_PENDING: i32 = 0;
pub const RUNTIME_POLL_READY: i32 = 1;

#[derive(Debug, Clone)]
pub struct RuntimeTask {
    pub id: RuntimeTaskId,
    pub state: RuntimeTaskState,
    pub roots: GcRootSet,
    pub spawned_from: Option<RuntimeStackTrace>,
    pub stack_trace: RuntimeStackTrace,
    /// Cooperative resume entry, or `None` for a bookkeeping-only placeholder.
    pub poll: Option<RuntimePollFn>,
    /// Heap async frame passed to `poll`. Kept alive via a GC runtime root while
    /// the task is pending; `null` for placeholders.
    pub frame: *mut c_void,
    /// When `Some`, this (parked) task should be woken once the instant passes —
    /// set by `willow_sched_sleep` from a poll fn before it returns Pending, and
    /// honored by the timer-aware run loop (willow-lpn.5.3).
    pub wake_deadline: Option<std::time::Instant>,
    /// Tasks parked awaiting THIS task's completion; woken when it completes
    /// (dependency wake for `await <task>`, willow-lpn.5.3).
    pub waiters: Vec<RuntimeTaskId>,
}

impl RuntimeTask {
    pub fn new(id: RuntimeTaskId) -> Self {
        Self {
            id,
            state: RuntimeTaskState::Ready,
            roots: GcRootSet::default(),
            spawned_from: None,
            stack_trace: RuntimeStackTrace::default(),
            poll: None,
            frame: std::ptr::null_mut(),
            wake_deadline: None,
            waiters: Vec::new(),
        }
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
}
