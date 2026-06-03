use crate::stack_trace::RuntimeStackTrace;
use crate::trace::{GcTrace, GcVisitor};
use std::alloc::{Layout, alloc_zeroed};
use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle as NativeJoinHandle;

pub use crate::trace::GcRootSet;

pub type RuntimeTaskId = u64;

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

std::thread_local! {
    static CURRENT_TASK_DATA: std::cell::Cell<*mut c_void> =
        const { std::cell::Cell::new(std::ptr::null_mut()) };
}

#[repr(C)]
pub struct WillowTaskHeader {
    thread: Mutex<Option<NativeJoinHandle<()>>>,
    done: Mutex<bool>,
    done_changed: Condvar,
    task_id: u64,
    spawn_file: *const c_char,
    spawn_line: i32,
    spawn_col: i32,
    data_size: usize,
    alloc_size: usize,
}

unsafe impl Send for WillowTaskHeader {}
unsafe impl Sync for WillowTaskHeader {}

impl WillowTaskHeader {
    fn new(data_size: usize, alloc_size: usize) -> Self {
        Self {
            thread: Mutex::new(None),
            done: Mutex::new(false),
            done_changed: Condvar::new(),
            task_id: NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed),
            spawn_file: std::ptr::null(),
            spawn_line: 0,
            spawn_col: 0,
            data_size,
            alloc_size,
        }
    }
}

fn task_layout(data_size: usize) -> Option<Layout> {
    let header_size = std::mem::size_of::<WillowTaskHeader>();
    let size = header_size.checked_add(data_size)?;
    Layout::from_size_align(size, std::mem::align_of::<WillowTaskHeader>()).ok()
}

fn header_from_data(data_ptr: *mut c_void) -> Option<*mut WillowTaskHeader> {
    if data_ptr.is_null() {
        return None;
    }
    let header_size = std::mem::size_of::<WillowTaskHeader>();
    Some(unsafe { (data_ptr as *mut u8).sub(header_size) as *mut WillowTaskHeader })
}

fn c_string(value: *const c_char) -> String {
    if value.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned()
    }
}

pub fn current_task_data() -> *mut c_void {
    CURRENT_TASK_DATA.with(|data| data.get())
}

pub fn task_context_text(data_ptr: *mut c_void) -> Option<String> {
    let header = header_from_data(data_ptr)?;
    let header = unsafe { &*header };
    let mut text = format!("  task #{}", header.task_id);
    if !header.spawn_file.is_null() {
        text.push_str(&format!(
            " (spawned from {}:{}:{})",
            c_string(header.spawn_file),
            header.spawn_line,
            header.spawn_col
        ));
    }
    Some(text)
}

pub fn print_current_task_context() {
    if let Some(text) = task_context_text(current_task_data()) {
        eprintln!("{text}");
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_task_alloc(data_size: i64) -> *mut c_void {
    let data_size = data_size.max(0) as usize;
    let Some(layout) = task_layout(data_size) else {
        std::process::abort();
    };
    let raw = unsafe { alloc_zeroed(layout) };
    if raw.is_null() {
        std::process::abort();
    }

    let header = raw as *mut WillowTaskHeader;
    unsafe {
        header.write(WillowTaskHeader::new(data_size, layout.size()));
        raw.add(std::mem::size_of::<WillowTaskHeader>()) as *mut c_void
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_task_set_spawn_location(
    data_ptr: *mut c_void,
    file: *const c_char,
    line: i32,
    col: i32,
) {
    let Some(header) = header_from_data(data_ptr) else {
        return;
    };
    unsafe {
        (*header).spawn_file = file;
        (*header).spawn_line = line;
        (*header).spawn_col = col;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_task_spawn(tramp_ptr: *mut c_void, data_ptr: *mut c_void) {
    let Some(header) = header_from_data(data_ptr) else {
        return;
    };
    if tramp_ptr.is_null() {
        std::process::abort();
    }

    let tramp_addr = tramp_ptr as usize;
    let data_addr = data_ptr as usize;
    let handle = std::thread::spawn(move || {
        let data = data_addr as *mut c_void;
        CURRENT_TASK_DATA.with(|current| current.set(data));
        let tramp: extern "C" fn(*mut c_void) = unsafe { std::mem::transmute(tramp_addr) };
        tramp(data);
        CURRENT_TASK_DATA.with(|current| current.set(std::ptr::null_mut()));
    });

    unsafe {
        *(*header).thread.lock().expect("task thread mutex poisoned") = Some(handle);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_task_complete(data_ptr: *mut c_void) {
    let Some(header) = header_from_data(data_ptr) else {
        return;
    };
    let header = unsafe { &*header };
    let mut done = header.done.lock().expect("task done mutex poisoned");
    *done = true;
    header.done_changed.notify_all();
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_task_join(data_ptr: *mut c_void) {
    let Some(header) = header_from_data(data_ptr) else {
        return;
    };
    let header = unsafe { &*header };
    let mut done = header.done.lock().expect("task done mutex poisoned");
    while !*done {
        done = header
            .done_changed
            .wait(done)
            .expect("task done mutex poisoned");
    }
    drop(done);

    if let Some(handle) = header
        .thread
        .lock()
        .expect("task thread mutex poisoned")
        .take()
    {
        let _ = handle.join();
    }
}

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

    extern "C" fn complete_trampoline(data: *mut c_void) {
        unsafe { *(data as *mut i64) = 99 };
        willow_task_complete(data);
    }

    #[test]
    fn task_unit_01_alloc_returns_writable_data_area() {
        let data = willow_task_alloc(8);
        assert!(!data.is_null());
        unsafe { *(data as *mut i64) = 42 };
        assert_eq!(unsafe { *(data as *mut i64) }, 42);
    }

    #[test]
    fn task_unit_02_spawn_join_runs_trampoline() {
        let data = willow_task_alloc(8);
        willow_task_spawn(complete_trampoline as *mut c_void, data);
        willow_task_join(data);
        assert_eq!(unsafe { *(data as *mut i64) }, 99);
    }

    #[test]
    fn task_unit_03_spawn_location_is_rendered_in_context_text() {
        let data = willow_task_alloc(8);
        let file = std::ffi::CString::new("spawn.wi").unwrap();
        willow_task_set_spawn_location(data, file.as_ptr(), 7, 8);
        let text = task_context_text(data).unwrap();
        assert!(text.contains("spawned from spawn.wi:7:8"));
    }
}
