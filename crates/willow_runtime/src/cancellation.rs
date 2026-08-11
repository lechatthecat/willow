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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use crate::gc::{GcObjectKind, GcStoreDestination, willow_gc_write_barrier};

const CANCELLATION_TOKEN_TYPE_ID: u32 = 0x4341_4E01;
const TASK_SCOPE_TYPE_ID: u32 = 0x5343_5001;
const TASK_ID_SLOT: usize = 1;
const SCOPE_FINISH_RESULT_SLOT: usize = 0;
const SCOPE_FINISH_TASK_ID_SLOT: usize = 1;
const SCOPE_FINISH_HANDLE_SLOT: usize = 2;
const SCOPE_FINISH_STATE_SLOT: usize = 3;

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
        if self.cancelled.swap(true, Ordering::AcqRel) {
            return;
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
        for task in tasks {
            crate::scheduler::willow_sched_cancel(task);
        }
        for child in children {
            child.cancel();
        }
    }
}

struct ScopedTask {
    id: u64,
    // A trace callback returns the ADDRESS of this slot to the collector. Box
    // it so growing `ScopeState::tasks` cannot invalidate an address after the
    // state lock is released and before the collector dereferences it.
    frame: Box<*mut u8>,
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
                        frame: Box::new(frame),
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
        if self.cancelled.swap(true, Ordering::AcqRel) {
            return;
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
        for task_id in tasks {
            crate::scheduler::willow_sched_cancel(task_id);
        }
        for child in children {
            child.cancel();
        }
    }

    fn begin_finish(&self) {
        let children = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.closing = true;
            state.children.clone()
        };
        for child in children {
            child.begin_finish();
        }
    }

    fn snapshot(&self, tasks: &mut Vec<(u64, *mut u8)>, saw_cancelled: &mut bool) {
        let (own_tasks, children, cancelled) = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (
                state
                    .tasks
                    .iter()
                    .map(|task| (task.id, *task.frame))
                    .collect::<Vec<_>>(),
                state.children.clone(),
                state.saw_cancelled || self.cancelled.load(Ordering::Acquire),
            )
        };
        tasks.extend(own_tasks);
        *saw_cancelled |= cancelled;
        for child in children {
            child.snapshot(tasks, saw_cancelled);
        }
    }

    fn finish_and_release(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.finished {
            return state.saw_cancelled;
        }
        let mut saw_cancelled = state.saw_cancelled || self.cancelled.load(Ordering::Acquire);
        saw_cancelled |= state.tasks.iter().any(|task| {
            crate::async_frame::frame_terminal_status((*task.frame).cast())
                == crate::async_frame::WILLOW_FRAME_STATUS_CANCELLED
        });
        for child in &state.children {
            saw_cancelled |= child.finish_and_release();
        }
        state.finished = true;
        state.saw_cancelled |= saw_cancelled;
        // Keep the boxed slot allocations and child cores stable until this
        // ScopeCore itself is dropped. A collector may already hold their slot
        // addresses from `trace_frames`; clearing either vector here would turn
        // those addresses into dangling pointers. Nulling releases the actual
        // frame roots without retaining completed frames.
        for task in &mut state.tasks {
            *task.frame = std::ptr::null_mut();
        }
        state.saw_cancelled
    }

    fn trace_frames(&self, slots: &mut Vec<*mut *mut u8>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for task in &mut state.tasks {
            slots.push(task.frame.as_mut() as *mut *mut u8);
        }
        // `children` is append-only until ScopeCore drop, for the same slot
        // lifetime reason as `tasks`. Cloning lets recursive locking proceed
        // without imposing parent->child lock nesting on other operations.
        let children = state.children.clone();
        drop(state);
        for child in children {
            child.trace_frames(slots);
        }
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

static REGISTERED_GENERATION: AtomicU64 = AtomicU64::new(0);
static REGISTRATION_LOCK: Mutex<()> = Mutex::new(());

fn ensure_registered() {
    let generation = crate::gc::registry_generation();
    if REGISTERED_GENERATION.load(Ordering::Acquire) == generation {
        return;
    }
    let _registration = REGISTRATION_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let generation = crate::gc::registry_generation();
    if REGISTERED_GENERATION.load(Ordering::Acquire) == generation {
        return;
    }
    crate::gc::willow_register_drop(CANCELLATION_TOKEN_TYPE_ID, drop_cancellation_token);
    crate::gc::willow_register_type(TASK_SCOPE_TYPE_ID, trace_task_scope);
    crate::gc::willow_register_drop(TASK_SCOPE_TYPE_ID, drop_task_scope);
    REGISTERED_GENERATION.store(generation, Ordering::Release);
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
    let result = crate::gc::willow_alloc_with_layout(GcObjectKind::Enum, 0, 16, 0);
    if !result.is_null() {
        unsafe {
            *result.cast::<i64>() = 1; // Result::Err
            *result.cast::<i64>().add(1) = 0; // Cancelled (fieldless tag 0)
        }
    }
    result
}

unsafe fn finish_state(frame: *mut c_void) -> Option<&'static mut Arc<ScopeCore>> {
    let raw = unsafe { *frame_slot::<*mut Arc<ScopeCore>>(frame, SCOPE_FINISH_STATE_SLOT) };
    unsafe { raw.as_mut() }
}

unsafe fn finish_scope_task(frame: *mut c_void, result: *mut u8) -> i32 {
    unsafe {
        willow_gc_write_barrier(
            frame.cast::<u8>(),
            result,
            GcStoreDestination::AsyncFrameSlot as i64,
        );
        *frame_slot::<*mut u8>(frame, SCOPE_FINISH_RESULT_SLOT) = result;
        let slot = frame_slot::<*mut Arc<ScopeCore>>(frame, SCOPE_FINISH_STATE_SLOT);
        let raw = *slot;
        *slot = std::ptr::null_mut();
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
        let slot = frame_slot::<*mut Arc<ScopeCore>>(frame, SCOPE_FINISH_STATE_SLOT);
        let raw = *slot;
        *slot = std::ptr::null_mut();
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
    let frame = crate::async_frame::willow_async_frame_alloc(4, 0b0101);
    if frame.is_null() {
        return frame;
    }
    unsafe {
        *((frame as *mut u8)
            .add(crate::async_frame::ASYNC_FRAME_SLOT_COUNT_OFFSET)
            .cast::<i64>()) = 4;
        willow_gc_write_barrier(
            frame.cast::<u8>(),
            scope_handle,
            GcStoreDestination::AsyncFrameSlot as i64,
        );
        *frame_slot::<*mut u8>(frame, SCOPE_FINISH_HANDLE_SLOT) = scope_handle;
        *frame_slot::<*mut Arc<ScopeCore>>(frame, SCOPE_FINISH_STATE_SLOT) =
            Box::into_raw(Box::new(Arc::clone(&scope.core)));
    }
    crate::scheduler::spawn_global_task_initialized(
        poll_scope_finish,
        frame,
        Some(cancel_scope_finish),
        |task_id| unsafe {
            *frame_slot::<u64>(frame, SCOPE_FINISH_TASK_ID_SLOT) = task_id;
        },
    );
    frame
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

        // Force several Vec reallocations after the collector has received the
        // first slot address. Boxed slots must stay at the same address.
        for id in 2..=4096 {
            core.add(id, (0x1000usize + id as usize * 16) as *mut u8);
        }
        assert_eq!(slots[0], first_slot);
        assert_eq!(unsafe { *first_slot }, first_frame);
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
}
