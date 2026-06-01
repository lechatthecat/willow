use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;

use crate::task::{
    RUNTIME_POLL_READY, RuntimePollFn, RuntimeTask, RuntimeTaskId, RuntimeTaskState,
};
use crate::trace::{GcTrace, GcVisitor};

#[derive(Debug, Default)]
pub struct RuntimeScheduler {
    next_task_id: RuntimeTaskId,
    tasks: HashMap<RuntimeTaskId, RuntimeTask>,
    ready: VecDeque<RuntimeTaskId>,
}

impl RuntimeScheduler {
    pub fn spawn_placeholder(&mut self) -> RuntimeTaskId {
        let id = self.next_task_id;
        self.next_task_id += 1;
        let task = RuntimeTask::new(id);
        self.tasks.insert(id, task);
        self.ready.push_back(id);
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
        self.ready.push_back(id);
        id
    }

    /// The cooperative resume entry + frame for a task, if it is executable.
    pub fn task_work(&self, id: RuntimeTaskId) -> Option<(RuntimePollFn, *mut c_void)> {
        let task = self.tasks.get(&id)?;
        Some((task.poll?, task.frame))
    }

    pub fn set_running(&mut self, id: RuntimeTaskId) {
        if let Some(task) = self.tasks.get_mut(&id) {
            task.state = RuntimeTaskState::Running;
        }
    }

    pub fn complete(&mut self, id: RuntimeTaskId) {
        if let Some(task) = self.tasks.get_mut(&id) {
            task.complete();
        }
    }

    pub fn pop_ready(&mut self) -> Option<RuntimeTaskId> {
        self.ready.pop_front()
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
        self.ready.len()
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
                self.ready.push_back(id);
            }
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
    with_global(|sched| sched.spawn_task(poll, frame))
}

/// Wake a parked task, re-queueing it as ready.
#[unsafe(no_mangle)]
pub extern "C" fn willow_sched_wake(id: u64) {
    with_global(|sched| sched.wake(id));
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
            break;
        };
        let Some((poll, frame)) = work else {
            // Placeholder task with no executable work: just complete it.
            with_global(|sched| sched.complete(id));
            completed += 1;
            continue;
        };
        let result = unsafe { poll(frame) };
        with_global(|sched| {
            if result == RUNTIME_POLL_READY {
                sched.complete(id);
                crate::gc::willow_gc_remove_runtime_root(frame as *mut u8);
            } else {
                sched.park(id);
            }
        });
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

    // ── Cooperative executable tasks (willow-fqg.1) ─────────────────────────

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
}
