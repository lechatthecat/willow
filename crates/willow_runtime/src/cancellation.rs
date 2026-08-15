//! Cooperative cancellation tokens and structured task scopes (willow-2s3.3).
//!
//! `CancellationToken` is a fan-out cancellation source. `attach(task)` is an
//! identity operation on the Task handle that records its task id; cancelling
//! the token requests normal scheduler cancellation for every participant.
//! Child tokens inherit parent cancellation but never propagate upward.
//!
//! `TaskScope` strongly owns the task frames added to it (and child scopes) so
//! their completion can be observed after scheduler reaping. `finish()` closes
//! the scope to new children and returns a Task that parks on every unfinished
//! child through the ordinary task-waiter mechanism. It resolves to
//! `Ok(void)` when all children complete and `Err(Cancelled)` if any child or
//! the scope was cancelled. Task panics retain Willow's process-abort policy.

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::gc::GcObjectKind;
use crate::native_frame::{NativeFrameSpec, NativeTaskFrame};

const CANCELLATION_TOKEN_TYPE_ID: u32 = 0x4341_4E01;
const TASK_SCOPE_TYPE_ID: u32 = 0x5343_5001;
const TASK_ID_SLOT: usize = 1;
const SCOPE_FINISH_RESULT_SLOT: usize = 0;
const SCOPE_FINISH_TASK_ID_SLOT: usize = 1;
const SCOPE_FINISH_HANDLE_SLOT: usize = 2;
const SCOPE_FINISH_STATE_SLOT: usize = 3;

struct ScopeFinishFrame;

impl NativeFrameSpec for ScopeFinishFrame {
    const LAYOUT: willow_abi::NativeFrameLayout<'static> = willow_abi::NativeFrameLayout::new(&[
        willow_abi::SlotKind::GcRef,
        willow_abi::SlotKind::Word,
        willow_abi::SlotKind::GcRef,
        willow_abi::SlotKind::NativePtr,
    ]);
    const NAME: &'static str = "TaskScope::finish";
}

unsafe fn scope_finish_frame(frame: *mut c_void) -> NativeTaskFrame<ScopeFinishFrame> {
    unsafe { NativeTaskFrame::from_raw(frame) }
}

#[derive(Default)]
struct CancellationCore {
    cancelled: AtomicBool,
    tasks: Mutex<HashSet<u64>>,
    children: Mutex<Vec<Weak<CancellationCore>>>,
}

struct CancellationTokenHandle {
    core: Arc<CancellationCore>,
}

impl CancellationCore {
    fn attach(&self, task_id: u64) {
        if task_id == 0 {
            return;
        }
        let cancel_now = {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if self.cancelled.load(Ordering::Acquire) {
                true
            } else {
                // IDs are process-unique and completed task records are reaped.
                // Opportunistic pruning keeps a long-lived token from retaining
                // unbounded historical membership before it is cancelled.
                if tasks.len() >= 64 && tasks.len().is_multiple_of(64) {
                    tasks.retain(|id| crate::scheduler::willow_sched_task_state(*id) != -1);
                }
                tasks.insert(task_id);
                false
            }
        };
        if cancel_now {
            crate::scheduler::willow_sched_cancel(task_id);
        }
    }

    fn child(self: &Arc<Self>) -> Arc<Self> {
        let child = Arc::new(Self::default());
        let cancel_now = {
            let mut children = self
                .children
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            children.retain(|child| child.strong_count() > 0);
            if self.cancelled.load(Ordering::Acquire) {
                true
            } else {
                children.push(Arc::downgrade(&child));
                false
            }
        };
        if cancel_now {
            child.cancel();
        }
        child
    }

    fn cancel(&self) {
        let Some((tasks, children)) = self.take_cancel_work() else {
            return;
        };
        for task in tasks {
            crate::scheduler::willow_sched_cancel(task);
        }
        let mut pending = children;
        while let Some(core) = pending.pop() {
            let Some((tasks, children)) = core.take_cancel_work() else {
                continue;
            };
            for task in tasks {
                crate::scheduler::willow_sched_cancel(task);
            }
            pending.extend(children);
        }
    }

    fn take_cancel_work(&self) -> Option<(Vec<u64>, Vec<Arc<CancellationCore>>)> {
        if self.cancelled.swap(true, Ordering::AcqRel) {
            return None;
        }
        let tasks = {
            let mut tasks = self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *tasks)
        };
        let children = {
            let mut children = self
                .children
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let live = children
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>();
            children.clear();
            live
        };
        Some((tasks.into_iter().collect(), children))
    }
}

struct ScopedTask {
    id: u64,
    frame: crate::gc::GcRootHandle,
}

// SAFETY: frame pointers name GC-managed async frames. Active frames are also
// scheduler roots; completed frames are traced from the TaskScope handle.
unsafe impl Send for ScopedTask {}

#[derive(Default)]
struct ScopeState {
    tasks: Vec<ScopedTask>,
    children: Vec<Arc<ScopeCore>>,
    closing: bool,
    finished: bool,
    saw_cancelled: bool,
}

#[derive(Default)]
struct ScopeCore {
    cancelled: AtomicBool,
    state: Mutex<ScopeState>,
    roots: crate::gc::GcRootArena,
}

impl Drop for ScopeCore {
    fn drop(&mut self) {
        // A scope owns child cores strongly. Letting the derived field drop walk
        // a long single-child chain recursively can overflow the native stack
        // even though every operational traversal below is iterative. Drain
        // uniquely-owned descendants here; shared children retain their own
        // independent lifetime and run the same logic when their last Arc goes.
        let mut pending = {
            let state = self
                .state
                .get_mut()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut state.children)
        };
        while let Some(child) = pending.pop() {
            if let Ok(mut child) = Arc::try_unwrap(child) {
                let state = child
                    .state
                    .get_mut()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                pending.append(&mut state.children);
                // `child` now has an empty child list, so its own Drop is O(1).
            }
        }
    }
}

struct TaskScopeHandle {
    core: Arc<ScopeCore>,
}

impl ScopeCore {
    fn add(&self, id: u64, frame: *mut u8) {
        let cancel_now = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if self.cancelled.load(Ordering::Acquire) || state.closing || state.finished {
                true
            } else {
                if !state.tasks.iter().any(|existing| existing.id == id) {
                    state.tasks.push(ScopedTask {
                        id,
                        frame: self.roots.insert(frame),
                    });
                }
                false
            }
        };
        if cancel_now {
            crate::scheduler::willow_sched_cancel(id);
        }
    }

    fn child(self: &Arc<Self>) -> Arc<Self> {
        let child = Arc::new(Self::default());
        let cancel_now = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if self.cancelled.load(Ordering::Acquire) || state.closing || state.finished {
                true
            } else {
                state.children.push(Arc::clone(&child));
                false
            }
        };
        if cancel_now {
            child.cancel();
            child.begin_finish();
        }
        child
    }

    fn cancel(&self) {
        let Some((tasks, children)) = self.take_cancel_work() else {
            return;
        };
        for task_id in tasks {
            crate::scheduler::willow_sched_cancel(task_id);
        }
        let mut pending = children;
        while let Some(core) = pending.pop() {
            let Some((tasks, children)) = core.take_cancel_work() else {
                continue;
            };
            for task_id in tasks {
                crate::scheduler::willow_sched_cancel(task_id);
            }
            pending.extend(children);
        }
    }

    fn take_cancel_work(&self) -> Option<(Vec<u64>, Vec<Arc<ScopeCore>>)> {
        if self.cancelled.swap(true, Ordering::AcqRel) {
            return None;
        }
        let (tasks, children) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.saw_cancelled = true;
            (
                state.tasks.iter().map(|task| task.id).collect::<Vec<_>>(),
                state.children.clone(),
            )
        };
        Some((tasks, children))
    }

    fn begin_finish(&self) {
        let mut pending = self.close_and_children();
        while let Some(core) = pending.pop() {
            pending.extend(core.close_and_children());
        }
    }

    fn close_and_children(&self) -> Vec<Arc<ScopeCore>> {
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.closing = true;
            state.children.clone()
        }
    }

    fn snapshot(&self, tasks: &mut Vec<(u64, *mut u8)>, saw_cancelled: &mut bool) {
        let mut pending = Vec::new();
        self.append_snapshot(tasks, saw_cancelled, &mut pending);
        while let Some(core) = pending.pop() {
            core.append_snapshot(tasks, saw_cancelled, &mut pending);
        }
    }

    fn append_snapshot(
        &self,
        tasks: &mut Vec<(u64, *mut u8)>,
        saw_cancelled: &mut bool,
        pending: &mut Vec<Arc<ScopeCore>>,
    ) {
        let (own_tasks, children, cancelled) = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (
                state
                    .tasks
                    .iter()
                    .map(|task| (task.id, task.frame.load()))
                    .collect::<Vec<_>>(),
                state.children.clone(),
                state.saw_cancelled || self.cancelled.load(Ordering::Acquire),
            )
        };
        tasks.extend(own_tasks);
        *saw_cancelled |= cancelled;
        pending.extend(children);
    }

    fn finish_and_release(&self) -> bool {
        // Finish descendants in post-order without recursive Rust calls. Each
        // node is first closed, so the collected child set cannot grow behind
        // the traversal.
        let mut pending = self.close_and_children();
        let mut descendants = Vec::new();
        while let Some(core) = pending.pop() {
            pending.extend(core.close_and_children());
            descendants.push(core);
        }
        for core in descendants.into_iter().rev() {
            core.finish_local();
        }
        self.finish_local()
    }

    fn finish_local(&self) -> bool {
        let (task_frames, children, already_finished) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.closing = true;
            (
                state
                    .tasks
                    .iter()
                    .map(|task| task.frame.load())
                    .collect::<Vec<_>>(),
                state.children.clone(),
                state.finished.then_some(state.saw_cancelled),
            )
        };
        if let Some(cancelled) = already_finished {
            return cancelled;
        }
        let child_cancelled = children.iter().any(|child| {
            child
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .saw_cancelled
        });
        let terminal_cancelled = task_frames.iter().any(|frame| {
            crate::async_frame::frame_terminal_status((*frame).cast())
                == crate::async_frame::WILLOW_FRAME_STATUS_CANCELLED
        });

        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.finished {
            return state.saw_cancelled;
        }
        let saw_cancelled = state.saw_cancelled
            || self.cancelled.load(Ordering::Acquire)
            || terminal_cancelled
            || child_cancelled;
        state.finished = true;
        state.saw_cancelled |= saw_cancelled;
        // Logical release nulls the stable arena cells. Task metadata can now
        // be reclaimed immediately; the arena retains the physical cells until
        // ScopeCore finalization, so collector-held slot addresses stay valid.
        for task in &state.tasks {
            task.frame.release();
        }
        state.tasks.clear();
        state.saw_cancelled
    }

    fn trace_frames(&self, slots: &mut Vec<*mut *mut u8>) {
        let mut pending = Vec::new();
        self.append_trace_frames(slots, &mut pending);
        while let Some(core) = pending.pop() {
            core.append_trace_frames(slots, &mut pending);
        }
    }

    fn append_trace_frames(
        &self,
        slots: &mut Vec<*mut *mut u8>,
        pending: &mut Vec<Arc<ScopeCore>>,
    ) {
        self.roots.trace_slots(slots);
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Queueing clones avoids parent->child lock nesting while the iterative
        // traversal proceeds. Each child owns its own stable root arena.
        pending.extend(state.children.iter().cloned());
    }
}

unsafe fn trace_task_scope(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    let handle = unsafe { &*payload.cast::<TaskScopeHandle>() };
    handle.core.trace_frames(slots);
}

unsafe fn drop_cancellation_token(payload: *mut u8) {
    unsafe { std::ptr::drop_in_place(payload.cast::<CancellationTokenHandle>()) };
}

unsafe fn drop_task_scope(payload: *mut u8) {
    unsafe { std::ptr::drop_in_place(payload.cast::<TaskScopeHandle>()) };
}

static CANCELLATION_REGISTRATION: crate::gc::NativeGcRegistration =
    crate::gc::NativeGcRegistration::new();
const CANCELLATION_GC_TYPES: &[crate::gc::NativeGcType] = &[
    crate::gc::NativeGcType::new(
        CANCELLATION_TOKEN_TYPE_ID,
        None,
        Some(drop_cancellation_token),
    ),
    crate::gc::NativeGcType::new(
        TASK_SCOPE_TYPE_ID,
        Some(trace_task_scope),
        Some(drop_task_scope),
    ),
];

fn ensure_registered() {
    CANCELLATION_REGISTRATION.ensure(CANCELLATION_GC_TYPES);
}

fn alloc_token(core: Arc<CancellationCore>) -> *mut u8 {
    ensure_registered();
    let payload = crate::gc::willow_alloc_with_layout(
        GcObjectKind::Class,
        CANCELLATION_TOKEN_TYPE_ID,
        std::mem::size_of::<CancellationTokenHandle>() as i64,
        0,
    );
    if !payload.is_null() {
        unsafe {
            payload
                .cast::<CancellationTokenHandle>()
                .write(CancellationTokenHandle { core })
        };
    }
    payload
}

fn alloc_scope(core: Arc<ScopeCore>) -> *mut u8 {
    ensure_registered();
    let payload = crate::gc::willow_alloc_with_layout(
        GcObjectKind::Class,
        TASK_SCOPE_TYPE_ID,
        std::mem::size_of::<TaskScopeHandle>() as i64,
        0,
    );
    if !payload.is_null() {
        unsafe {
            payload
                .cast::<TaskScopeHandle>()
                .write(TaskScopeHandle { core })
        };
    }
    payload
}

unsafe fn token<'a>(payload: *mut u8) -> Option<&'a CancellationTokenHandle> {
    unsafe { payload.cast::<CancellationTokenHandle>().as_ref() }
}

unsafe fn scope<'a>(payload: *mut u8) -> Option<&'a TaskScopeHandle> {
    unsafe { payload.cast::<TaskScopeHandle>().as_ref() }
}

unsafe fn frame_slot<T>(frame: *mut c_void, slot: usize) -> *mut T {
    unsafe {
        (frame as *mut u8)
            .add(crate::async_frame::async_frame_slot_offset(slot))
            .cast()
    }
}

fn task_id(frame: *mut u8) -> u64 {
    if frame.is_null() {
        return 0;
    }
    unsafe { *frame_slot::<u64>(frame.cast(), TASK_ID_SLOT) }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_cancellation_token_new() -> *mut u8 {
    alloc_token(Arc::new(CancellationCore::default()))
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_cancellation_token_child(parent: *mut u8) -> *mut u8 {
    let Some(parent) = (unsafe { token(parent) }) else {
        return std::ptr::null_mut();
    };
    alloc_token(parent.core.child())
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_cancellation_token_attach(
    token_handle: *mut u8,
    task_frame: *mut u8,
) -> *mut u8 {
    if let Some(token) = unsafe { token(token_handle) } {
        token.core.attach(task_id(task_frame));
    }
    task_frame
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_cancellation_token_cancel(token_handle: *mut u8) {
    if let Some(token) = unsafe { token(token_handle) } {
        token.core.cancel();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_cancellation_token_is_cancelled(token_handle: *mut u8) -> i64 {
    unsafe { token(token_handle) }
        .is_some_and(|token| token.core.cancelled.load(Ordering::Acquire))
        .into()
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_task_scope_new() -> *mut u8 {
    alloc_scope(Arc::new(ScopeCore::default()))
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_task_scope_child(parent: *mut u8) -> *mut u8 {
    let Some(parent) = (unsafe { scope(parent) }) else {
        return std::ptr::null_mut();
    };
    alloc_scope(parent.core.child())
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_task_scope_add(scope_handle: *mut u8, task_frame: *mut u8) -> *mut u8 {
    if let Some(scope) = unsafe { scope(scope_handle) } {
        scope.core.add(task_id(task_frame), task_frame);
    }
    task_frame
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_task_scope_cancel(scope_handle: *mut u8) {
    if let Some(scope) = unsafe { scope(scope_handle) } {
        scope.core.cancel();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_task_scope_is_cancelled(scope_handle: *mut u8) -> i64 {
    unsafe { scope(scope_handle) }
        .is_some_and(|scope| scope.core.cancelled.load(Ordering::Acquire))
        .into()
}

fn alloc_cancelled_result() -> *mut u8 {
    crate::gc::willow_alloc_enum_variant(
        0,
        willow_abi::EnumVariantLayout::new(1, &[willow_abi::SlotKind::Word]),
        &[0], // fieldless Cancelled is the immediate tag 0
    )
}

unsafe fn finish_state(frame: *mut c_void) -> Option<&'static mut Arc<ScopeCore>> {
    let raw =
        unsafe { scope_finish_frame(frame) }.load_native::<Arc<ScopeCore>>(SCOPE_FINISH_STATE_SLOT);
    unsafe { raw.as_mut() }
}

unsafe fn finish_scope_task(frame: *mut c_void, result: *mut u8) -> i32 {
    unsafe {
        let frame = scope_finish_frame(frame);
        frame.store_gc(SCOPE_FINISH_RESULT_SLOT, result);
        let raw = frame.take_native::<Arc<ScopeCore>>(SCOPE_FINISH_STATE_SLOT);
        if !raw.is_null() {
            drop(Box::from_raw(raw));
        }
    }
    crate::task::RUNTIME_POLL_READY
}

unsafe extern "C" fn poll_scope_finish(frame: *mut c_void) -> i32 {
    let Some(core) = (unsafe { finish_state(frame) }) else {
        return crate::task::RUNTIME_POLL_READY;
    };
    let mut tasks = Vec::new();
    let mut saw_cancelled = false;
    core.snapshot(&mut tasks, &mut saw_cancelled);

    let mut pending = false;
    for (task_id, task_frame) in tasks {
        match crate::async_frame::frame_terminal_status(task_frame.cast()) {
            crate::async_frame::WILLOW_FRAME_STATUS_PENDING => {
                if crate::scheduler::willow_frame_await(task_frame.cast(), task_id) == 0 {
                    pending = true;
                }
            }
            crate::async_frame::WILLOW_FRAME_STATUS_CANCELLED => saw_cancelled = true,
            crate::async_frame::WILLOW_FRAME_STATUS_COMPLETED
            | crate::async_frame::WILLOW_FRAME_STATUS_PANICKED => {}
            _ => pending = true,
        }
    }
    if pending {
        return crate::task::RUNTIME_POLL_PENDING;
    }

    saw_cancelled |= core.finish_and_release();
    let result = if saw_cancelled {
        alloc_cancelled_result()
    } else {
        crate::fs::alloc_ok(0, false)
    };
    unsafe { finish_scope_task(frame, result) }
}

unsafe extern "C" fn cancel_scope_finish(frame: *mut c_void) {
    unsafe {
        let raw = scope_finish_frame(frame).take_native::<Arc<ScopeCore>>(SCOPE_FINISH_STATE_SLOT);
        if !raw.is_null() {
            drop(Box::from_raw(raw));
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_task_scope_finish(scope_handle: *mut u8) -> *mut c_void {
    let Some(scope) = (unsafe { scope(scope_handle) }) else {
        return std::ptr::null_mut();
    };
    scope.core.begin_finish();
    let Some(frame) = NativeTaskFrame::<ScopeFinishFrame>::allocate() else {
        return std::ptr::null_mut();
    };
    frame.store_gc(SCOPE_FINISH_HANDLE_SLOT, scope_handle);
    frame.store_native(
        SCOPE_FINISH_STATE_SLOT,
        Box::into_raw(Box::new(Arc::clone(&scope.core))),
    );
    let raw = frame.as_raw();
    crate::scheduler::spawn_global_task_initialized(
        poll_scope_finish,
        raw,
        Some(cancel_scope_finish),
        |task_id| frame.store_word(SCOPE_FINISH_TASK_ID_SLOT, task_id as i64),
    );
    raw
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{reset_internal_for_test, runtime_test_guard};
    use crate::scheduler::{
        reset_global_scheduler_for_test, willow_sched_run, willow_sched_run_until,
    };

    unsafe extern "C" fn pending_forever(_frame: *mut c_void) -> i32 {
        crate::task::RUNTIME_POLL_PENDING
    }

    #[test]
    fn cancelled_token_immediately_cancels_late_attachment() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let token = willow_cancellation_token_new();
        willow_cancellation_token_cancel(token);
        assert_eq!(willow_cancellation_token_is_cancelled(token), 1);
        let frame = crate::async_frame::willow_async_frame_alloc(2, 0);
        let id = crate::scheduler::willow_sched_spawn(pending_forever, frame);
        unsafe { *frame_slot::<u64>(frame, TASK_ID_SLOT) = id };
        willow_cancellation_token_attach(token, frame.cast());
        willow_sched_run_until(id);
        assert_eq!(
            crate::async_frame::frame_terminal_status(frame),
            crate::async_frame::WILLOW_FRAME_STATUS_CANCELLED
        );
        reset_global_scheduler_for_test();
        reset_internal_for_test();
    }

    #[test]
    fn attach_cancel_race_cannot_miss_a_participant() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        reset_global_scheduler_for_test();
        let core = Arc::new(CancellationCore::default());
        let mut frames = Vec::new();
        let mut ids = Vec::new();
        for _ in 0..64 {
            let frame = crate::async_frame::willow_async_frame_alloc(2, 0);
            let id = crate::scheduler::willow_sched_spawn(pending_forever, frame);
            unsafe { *frame_slot::<u64>(frame, TASK_ID_SLOT) = id };
            frames.push(frame);
            ids.push(id);
        }

        let barrier = Arc::new(std::sync::Barrier::new(ids.len() + 1));
        let mut workers = Vec::new();
        for id in ids {
            let core = Arc::clone(&core);
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                core.attach(id);
            }));
        }
        barrier.wait();
        core.cancel();
        for worker in workers {
            worker.join().unwrap();
        }
        willow_sched_run();
        for frame in frames {
            assert_eq!(
                crate::async_frame::frame_terminal_status(frame),
                crate::async_frame::WILLOW_FRAME_STATUS_CANCELLED
            );
        }
        reset_global_scheduler_for_test();
        reset_internal_for_test();
    }

    #[test]
    fn scope_trace_slots_survive_task_vector_growth() {
        let core = ScopeCore::default();
        let first_frame = 0x1000usize as *mut u8;
        core.add(1, first_frame);

        let mut slots = Vec::new();
        core.trace_frames(&mut slots);
        assert_eq!(slots.len(), 1);
        let first_slot = slots[0];

        // Force several arena-vector reallocations after the collector has
        // received the first slot address. Arc-owned cells stay at the same
        // address independently of the arena's storage growth.
        for id in 2..=4096 {
            core.add(id, (0x1000usize + id as usize * 16) as *mut u8);
        }
        assert_eq!(slots[0], first_slot);
        assert_eq!(unsafe { *first_slot }, first_frame);
    }

    #[test]
    fn scope_finish_releases_task_metadata_without_invalidating_trace_slots() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();

        let core = ScopeCore::default();
        let frame = crate::async_frame::willow_async_frame_alloc(1, 0);
        assert!(!frame.is_null());
        unsafe {
            *((frame as *mut u8)
                .add(crate::async_frame::ASYNC_FRAME_SLOT_COUNT_OFFSET)
                .cast::<i64>()) = 1;
            crate::async_frame::frame_publish_terminal(
                frame.cast(),
                crate::async_frame::WILLOW_FRAME_STATUS_COMPLETED,
            );
        }
        core.add(1, frame.cast());

        let mut slots = Vec::new();
        core.trace_frames(&mut slots);
        assert_eq!(slots.len(), 1);
        let traced_slot = slots[0];
        assert_eq!(unsafe { *traced_slot }, frame.cast());

        core.begin_finish();
        assert!(!core.finish_and_release());
        let state = core
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            state.tasks.is_empty(),
            "finished task metadata must be reaped"
        );
        drop(state);
        assert_eq!(core.roots.slot_count(), 1);
        assert!(
            unsafe { *traced_slot }.is_null(),
            "released stable slot remains addressable but no longer roots a frame"
        );

        reset_internal_for_test();
    }

    #[test]
    fn cancelled_result_uses_the_fieldless_enum_immediate_payload() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let mut result = alloc_cancelled_result();
        assert!(!result.is_null());
        unsafe {
            assert_eq!(*result.cast::<i64>(), 1, "outer Result tag is Err");
            assert_eq!(
                *result.cast::<i64>().add(1),
                0,
                "fieldless Cancelled is immediate variant tag 0, not a GC pointer"
            );
        }
        crate::gc::willow_push_root(&mut result);
        crate::gc::willow_gc_collect();
        crate::gc::willow_pop_roots(1);
        unsafe {
            assert_eq!(*result.cast::<i64>(), 1);
            assert_eq!(*result.cast::<i64>().add(1), 0);
        }
        reset_internal_for_test();
    }

    #[test]
    fn deep_cancellation_and_scope_trees_use_iterative_traversal() {
        const DEPTH: usize = 20_000;

        let token_root = Arc::new(CancellationCore::default());
        let mut token_nodes = Vec::with_capacity(DEPTH + 1);
        token_nodes.push(Arc::clone(&token_root));
        for _ in 0..DEPTH {
            let child = token_nodes.last().unwrap().child();
            token_nodes.push(child);
        }
        token_root.cancel();
        assert!(
            token_nodes
                .iter()
                .all(|node| node.cancelled.load(Ordering::Acquire))
        );

        let scope_root = Arc::new(ScopeCore::default());
        let mut scope_nodes = Vec::with_capacity(DEPTH + 1);
        scope_nodes.push(Arc::clone(&scope_root));
        for _ in 0..DEPTH {
            let child = scope_nodes.last().unwrap().child();
            scope_nodes.push(child);
        }
        scope_root.cancel();
        scope_root.begin_finish();
        let mut tasks = Vec::new();
        let mut saw_cancelled = false;
        scope_root.snapshot(&mut tasks, &mut saw_cancelled);
        let mut slots = Vec::new();
        scope_root.trace_frames(&mut slots);
        assert!(saw_cancelled);
        assert!(tasks.is_empty());
        assert!(slots.is_empty());
        assert!(scope_root.finish_and_release());
        assert!(scope_nodes.iter().all(|node| {
            let state = node
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.closing && state.finished && state.saw_cancelled
        }));
    }
}
