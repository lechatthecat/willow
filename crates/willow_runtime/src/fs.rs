//! File-system runtime for `std::fs` (filesystem support Stage 5, v1).
//!
//! V1 is SYNCHRONOUS under the hood: regular files are not epoll-pollable, so
//! non-blocking file I/O needs the blocking-syscall pool.
//! Errors surface as Willow `Result<_, IoError>` values built here — the
//! layouts must match the compiler's enum lowering:
//!   Result::Ok(v)  -> [tag = 0, payload]
//!   Result::Err(e) -> [tag = 1, payload]
//!   IoError::Failed(msg) -> [tag = 0, WillowString]
//! GC masks mark reference payloads; a nested build roots the inner object
//! across the outer allocation.

use crate::gc::{willow_alloc_enum_variant, willow_pop_roots, willow_push_root};
use crate::native_frame::{NativeFrameSpec, NativeTaskFrame};
use crate::string::{willow_string_as_str, willow_string_from_str};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Build `Ok(payload_word)`; `payload_is_ref` marks GC payloads in the mask.
pub(crate) fn alloc_ok(payload_word: i64, payload_is_ref: bool) -> *mut u8 {
    const WORD: &[willow_abi::SlotKind] = &[willow_abi::SlotKind::Word];
    const REF: &[willow_abi::SlotKind] = &[willow_abi::SlotKind::GcRef];
    let slots = if payload_is_ref { REF } else { WORD };
    // SAFETY: The variant layout selects its initialized word or reference payload, including
    // the traced low word for a pair.
    unsafe {
        willow_alloc_enum_variant(
            0,
            willow_abi::EnumVariantLayout::new(0, slots),
            &[payload_word],
        )
    }
}

/// Build `Err(IoError::Failed(message))`.
pub(crate) fn alloc_io_err(message: &str) -> *mut u8 {
    let msg = willow_string_from_str(message);
    // Root the message across the IoError allocation.
    let mut msg_slot = msg;
    // SAFETY: The initialized slot stays at this address until its registration is popped;
    // collection may rewrite it there.
    unsafe { willow_push_root(&raw mut msg_slot) };
    const REF: &[willow_abi::SlotKind] = &[willow_abi::SlotKind::GcRef];
    // SAFETY: The variant layout selects its initialized word or reference payload, including
    // the traced low word for a pair.
    let ioerr = unsafe {
        willow_alloc_enum_variant(
            0,
            willow_abi::EnumVariantLayout::new(0, REF),
            &[msg_slot as i64],
        )
    };
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_roots(1) };
    if ioerr.is_null() {
        return std::ptr::null_mut();
    }
    let mut ioerr_slot = ioerr;
    // SAFETY: The initialized slot stays at this address until its registration is popped;
    // collection may rewrite it there.
    unsafe { willow_push_root(&raw mut ioerr_slot) };
    // SAFETY: The variant layout selects its initialized word or reference payload, including
    // the traced low word for a pair.
    let err = unsafe {
        willow_alloc_enum_variant(
            0,
            willow_abi::EnumVariantLayout::new(1, REF),
            &[ioerr_slot as i64],
        )
    };
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_roots(1) };
    err
}

/// `fs::temp_path(prefix) -> String`: a process-unique path under the OS
/// temp directory (`<tmp>/<prefix>_<pid>_<counter>`). Uniqueness across
/// processes comes from the pid, within the process from the counter — so
/// parallel test/example runs cannot collide.
///
/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_fs_temp_path(prefix: *const u8) -> *mut u8 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let prefix = unsafe { willow_string_as_str(prefix) };
    // SANITIZE to a single file-name component: an absolute prefix would
    // REPLACE the temp dir in `join`, and `..` would escape it — both break
    // the "under the OS temp directory" contract (review fix). Anything
    // outside [A-Za-z0-9_-] becomes '_'; empty falls back to "tmp".
    let mut clean: String = prefix
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() {
        clean = "tmp".to_string();
    }
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("{clean}_{}_{n}", std::process::id()));
    willow_string_from_str(&path.to_string_lossy())
}

/// Error for a sync call whose Task was cancelled while the OS call ran. The
/// operation may still finish; the Task stops at its next poll.
const CANCELLED: &str = "file operation cancelled";

/// Build the `Result<void, IoError>` of a sync file call.
fn unit_result(result: Option<Result<(), String>>) -> *mut u8 {
    match result {
        Some(Ok(())) => alloc_ok(0, false),
        Some(Err(message)) => alloc_io_err(&message),
        None => alloc_io_err(CANCELLED),
    }
}

/// `fs::read_to_string(path) -> Result<String, IoError>`.
///
/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_fs_read_to_string(path: *const u8) -> *mut u8 {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let path = unsafe { willow_string_as_str(path) }.to_string();
    let result = crate::blocking::run_sync_call(move || {
        std::fs::read_to_string(&path).map_err(|error| format!("{path}: {error}"))
    });
    match result {
        Some(Ok(contents)) => {
            let s = willow_string_from_str(&contents);
            let mut slot = s;
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut slot) };
            let out = alloc_ok(slot as i64, true);
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(1) };
            out
        }
        Some(Err(message)) => alloc_io_err(&message),
        None => alloc_io_err(CANCELLED),
    }
}

/// `fs::write_string(path, contents) -> Result<void, IoError>`.
///
/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_fs_write_string(path: *const u8, contents: *const u8) -> *mut u8 {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let path = unsafe { willow_string_as_str(path) }.to_string();
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let contents = unsafe { willow_string_as_str(contents) }.to_string();
    unit_result(crate::blocking::run_sync_call(move || {
        std::fs::write(&path, contents).map_err(|error| format!("{path}: {error}"))
    }))
}

/// `fs::exists(path) -> bool` (1/0).
///
/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
///
/// ```compile_fail,E0133
/// willow_runtime::fs::willow_fs_exists(std::ptr::null());
/// ```
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_fs_exists(path: *const u8) -> i64 {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let path = unsafe { willow_string_as_str(path) }.to_string();
    let exists = crate::blocking::run_sync_call(move || std::path::Path::new(&path).exists());
    i64::from(exists.unwrap_or(false))
}

/// `fs::remove_file(path) -> Result<void, IoError>`.
///
/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_fs_remove_file(path: *const u8) -> *mut u8 {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let path = unsafe { willow_string_as_str(path) }.to_string();
    unit_result(crate::blocking::run_sync_call(move || {
        std::fs::remove_file(&path).map_err(|error| format!("{path}: {error}"))
    }))
}

enum BlockingFsResult {
    Read(Result<String, String>),
    Unit(Result<(), String>),
    Exists(bool),
}

struct BlockingFsState {
    task_id: AtomicU64,
    result: Mutex<Option<BlockingFsResult>>,
    /// The pool job while the bounded queue had no room for it: the Task owns
    /// it and re-submits on each poll until admitted. The job
    /// captures an `Arc` of this state, so whoever drops the state without
    /// running the job must `take()` it first or the cycle leaks.
    pending: Mutex<Option<crate::blocking::BlockingWork>>,
}

impl BlockingFsState {
    fn new() -> Self {
        Self {
            task_id: AtomicU64::new(0),
            result: Mutex::new(None),
            pending: Mutex::new(None),
        }
    }

    fn take_pending(&self) -> Option<crate::blocking::BlockingWork> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    fn keep_pending(&self, work: crate::blocking::BlockingWork) {
        *self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(work);
    }

    fn finish(&self, result: BlockingFsResult) {
        *self
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
        let task_id = self.task_id.load(Ordering::Acquire);
        if task_id != 0 {
            crate::scheduler::willow_sched_wake(task_id);
        }
    }
}

const FS_TASK_RESULT_SLOT: usize = 0;
const FS_TASK_ID_SLOT: usize = 1;
const FS_TASK_JOB_SLOT: usize = 2;

struct FsScalarFrame;

impl NativeFrameSpec for FsScalarFrame {
    const LAYOUT: willow_abi::NativeFrameLayout<'static> = willow_abi::NativeFrameLayout::new(&[
        willow_abi::SlotKind::Word,
        willow_abi::SlotKind::Word,
        willow_abi::SlotKind::NativePtr,
    ]);
    const NAME: &'static str = "blocking fs scalar";
}

struct FsReferenceFrame;

impl NativeFrameSpec for FsReferenceFrame {
    const LAYOUT: willow_abi::NativeFrameLayout<'static> = willow_abi::NativeFrameLayout::new(&[
        willow_abi::SlotKind::GcRef,
        willow_abi::SlotKind::Word,
        willow_abi::SlotKind::NativePtr,
    ]);
    const NAME: &'static str = "blocking fs reference";
}

unsafe fn fs_frame(frame: *mut c_void) -> NativeTaskFrame<FsScalarFrame> {
    // SAFETY: The caller supplies a live filesystem frame. Both layouts have the
    // same ID and native slots. Only the scalar layout uses this view for the result.
    unsafe { NativeTaskFrame::from_raw(frame) }
}

unsafe fn blocking_state(frame: *mut c_void) -> Option<Arc<BlockingFsState>> {
    // SAFETY: The caller retains the filesystem frame while reading its native job slot.
    let raw = unsafe { fs_frame(frame) }.load_native::<Arc<BlockingFsState>>(FS_TASK_JOB_SLOT);
    // The frame may release its boxed Arc when this poll completes.
    // SAFETY: A non-null slot holds a Box::into_raw pointer to an Arc<BlockingFsState>.
    // The caller has exclusive callback access while cloning that Arc.
    unsafe { raw.as_ref() }.cloned()
}

#[willow_runtime_macros::ffi_boundary]
unsafe extern "C" fn poll_blocking_fs(frame: *mut c_void) -> i32 {
    // SAFETY: The scheduler retains the filesystem frame and excludes cancellation
    // while this poll clones the state from its job slot.
    let Some(state) = (unsafe { blocking_state(frame) }) else {
        return crate::task::RUNTIME_POLL_READY;
    };
    if let Some(work) = state.take_pending() {
        // Queue was full at spawn: this Task is (or becomes) a slot
        // waiter. Either way it parks BlockedSyscall — the pool owes it a
        // wake, for a freed slot now or for completion once admitted.
        let task_id = state.task_id.load(Ordering::Acquire);
        if let Err(work) = crate::blocking::try_submit_or_wait(task_id, work) {
            state.keep_pending(work);
        }
        return crate::task::RUNTIME_POLL_BLOCKED_SYSCALL;
    }
    let result = state
        .result
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    let Some(result) = result else {
        return crate::task::RUNTIME_POLL_BLOCKED_SYSCALL;
    };
    let (word, result_is_ref) = match result {
        BlockingFsResult::Read(Ok(contents)) => {
            let string = willow_string_from_str(&contents);
            let mut root = string;
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut root) };
            let result = alloc_ok(root as i64, true);
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(1) };
            (result as i64, true)
        }
        BlockingFsResult::Read(Err(error)) | BlockingFsResult::Unit(Err(error)) => {
            (alloc_io_err(&error) as i64, true)
        }
        BlockingFsResult::Unit(Ok(())) => (alloc_ok(0, false) as i64, true),
        BlockingFsResult::Exists(exists) => (i64::from(exists), false),
    };
    // SAFETY: The result variant selects the layout used at spawn. blocking_state
    // already read a non-null Box::into_raw pointer from this frame. The cloned Arc
    // leaves the slot intact, and the scheduler excludes other polls and cancellation.
    // take_native therefore clears and returns that same pointer exactly once.
    unsafe {
        if result_is_ref {
            NativeTaskFrame::<FsReferenceFrame>::from_raw(frame)
                .store_gc(FS_TASK_RESULT_SLOT, word as *mut u8);
        } else {
            fs_frame(frame).store_word(FS_TASK_RESULT_SLOT, word);
        }
        let raw = fs_frame(frame).take_native::<Arc<BlockingFsState>>(FS_TASK_JOB_SLOT);
        drop(Box::from_raw(raw));
    }
    crate::task::RUNTIME_POLL_READY
}

#[willow_runtime_macros::ffi_boundary]
unsafe extern "C" fn cancel_blocking_fs(frame: *mut c_void) {
    // SAFETY: The scheduler retains this frame and serializes cancel with poll.
    // take_native clears the job slot and transfers its Box::into_raw pointer, if
    // present, to this callback. The pool job holds its own Arc to the state.
    unsafe {
        let raw = fs_frame(frame).take_native::<Arc<BlockingFsState>>(FS_TASK_JOB_SLOT);
        if !raw.is_null() {
            let state = Box::from_raw(raw);
            // Work the pool never admitted is dropped unrun (it may still be
            // registered as a slot waiter). Admitted work runs to completion
            // on its own Arc; its completion wake is stale and ignored.
            if state.take_pending().is_some() {
                crate::blocking::forget_waiter(state.task_id.load(Ordering::Acquire));
            }
            drop(state);
        }
    }
}

fn spawn_blocking_fs(
    work: impl FnOnce() -> BlockingFsResult + Send + 'static,
    result_is_gc_ref: bool,
) -> *mut c_void {
    spawn_blocking_fs_after_publication(work, result_is_gc_ref, |_, _, _| {})
}

fn spawn_blocking_fs_after_publication(
    work: impl FnOnce() -> BlockingFsResult + Send + 'static,
    result_is_gc_ref: bool,
    after_publication: impl FnOnce(u64, *mut c_void, &Arc<BlockingFsState>),
) -> *mut c_void {
    let frame = if result_is_gc_ref {
        NativeTaskFrame::<FsReferenceFrame>::allocate().map(|frame| frame.as_raw())
    } else {
        NativeTaskFrame::<FsScalarFrame>::allocate().map(|frame| frame.as_raw())
    };
    let Some(frame) = frame else {
        return std::ptr::null_mut();
    };
    let state = Arc::new(BlockingFsState::new());
    let state_for_work = Arc::clone(&state);
    // Keep the caller's Arc through the completion recheck. After publication,
    // another worker can run poll or cancel and reclaim the frame's boxed Arc.
    let state_box = Box::into_raw(Box::new(Arc::clone(&state)));
    // SAFETY: The newly allocated frame has a native job slot in either layout.
    // It receives the boxed Arc before any poll or cancellation can run.
    unsafe { fs_frame(frame) }.store_native(FS_TASK_JOB_SLOT, state_box);
    if let Err(job) = crate::blocking::try_submit(Box::new(move || {
        let result = work();
        state_for_work.finish(result);
    })) {
        // Bounded queue is full: the Task keeps the job and
        // registers as a slot waiter on its first poll, once it has an id.
        // Nothing is refused, so the caller still gets a real Task.
        state.keep_pending(job);
    }
    // SAFETY: The scheduler roots the frame and calls this initializer before
    // publishing the task. Both filesystem layouts have a word at FS_TASK_ID_SLOT.
    let initialize = |task_id| unsafe {
        fs_frame(frame).store_word(FS_TASK_ID_SLOT, task_id as i64);
        state.task_id.store(task_id, Ordering::Release);
    };
    // SAFETY: The supplied callback matches this frame layout; the frame storage remains live
    // across scheduling and callback execution.
    let task_id = unsafe {
        crate::scheduler::spawn_global_task_initialized(
            poll_blocking_fs,
            frame,
            Some(cancel_blocking_fs),
            initialize,
        )
    };
    // The callback is normally a no-op; tests force task cleanup here before
    // the actual completion recheck to exercise this publication boundary.
    after_publication(task_id, frame, &state);
    // Close the publication race: if the pool job finished BEFORE the
    // task-id store above, its `finish()` loaded 0 and could not wake us —
    // and the task may already be parked BlockedSyscall with the result
    // sitting in the mutex. Re-check and wake unconditionally; a duplicate
    // wake of a Ready/Running task is a no-op / wake_requested consume.
    {
        let has_result = state
            .result
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some();
        if has_result {
            crate::scheduler::willow_sched_wake(task_id);
        }
    }
    frame
}

/// Non-blocking file read. The returned Task parks in `BlockedSyscall` while a
/// bounded blocking-pool thread performs the OS operation.
///
/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_fs_read_to_string_async(path: *const u8) -> *mut c_void {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let path = unsafe { willow_string_as_str(path) }.to_string();
    spawn_blocking_fs(
        move || {
            BlockingFsResult::Read(
                std::fs::read_to_string(&path).map_err(|error| format!("{path}: {error}")),
            )
        },
        true,
    )
}

/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_fs_write_string_async(
    path: *const u8,
    contents: *const u8,
) -> *mut c_void {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let path = unsafe { willow_string_as_str(path) }.to_string();
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let contents = unsafe { willow_string_as_str(contents) }.to_string();
    spawn_blocking_fs(
        move || {
            BlockingFsResult::Unit(
                std::fs::write(&path, contents).map_err(|error| format!("{path}: {error}")),
            )
        },
        true,
    )
}

/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_fs_exists_async(path: *const u8) -> *mut c_void {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let path = unsafe { willow_string_as_str(path) }.to_string();
    spawn_blocking_fs(
        move || BlockingFsResult::Exists(std::path::Path::new(&path).exists()),
        false,
    )
}

/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_fs_remove_file_async(path: *const u8) -> *mut c_void {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let path = unsafe { willow_string_as_str(path) }.to_string();
    spawn_blocking_fs(
        move || {
            BlockingFsResult::Unit(
                std::fs::remove_file(&path).map_err(|error| format!("{path}: {error}")),
            )
        },
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{GcRootHandle, isolated_runtime, reset_internal_for_test};

    fn read_tag(ptr: *mut u8) -> i64 {
        // SAFETY: The Result constructor reserves word-aligned GC storage for its tag and payload.
        unsafe { *(ptr as *const i64) }
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
        )
    ))]
    #[test]
    fn native_stack_sync_read_frees_the_worker_and_cancels() {
        use crate::scheduler::*;
        use crate::task::RUNTIME_POLL_READY;
        static PATH: Mutex<String> = Mutex::new(String::new());
        static TAG: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(-1);
        unsafe extern "C" fn blocked(_: *mut c_void) -> i32 {
            let path = PATH.lock().unwrap().clone();
            // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
            // during this call.
            let result = unsafe { willow_fs_read_to_string(willow_string_from_str(&path)) };
            TAG.store(read_tag(result), Ordering::SeqCst);
            RUNTIME_POLL_READY
        }
        let _guard = isolated_runtime();
        for cancel in [false, true] {
            reset_global_scheduler_for_test();
            let path = std::env::temp_dir()
                .join(format!("willow_sync_read_{}_{cancel}", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let name = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
            // SAFETY: The CString supplies a live NUL-terminated pathname for mkfifo.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            *PATH.lock().unwrap() = path.to_str().unwrap().to_string();
            TAG.store(-1, Ordering::SeqCst);
            // SAFETY: The callback accepts a null frame and uses no frame storage.
            let task = unsafe { willow_sched_spawn(blocked, std::ptr::null_mut()) };
            willow_sched_run_until_deadline(willow_monotonic_millis() + 20);
            assert_eq!(willow_sched_task_state(task), 7);
            if cancel {
                willow_sched_cancel(task);
                willow_sched_run_until(task);
                assert_eq!(TAG.load(Ordering::SeqCst), 1);
            }
            // The cancelled job still owns its copies and ends with the writer.
            std::fs::write(&path, "payload").unwrap();
            if !cancel {
                willow_sched_run_until(task);
                assert_eq!(TAG.load(Ordering::SeqCst), 0);
            }
            crate::blocking::test_support::wait_until("file job completes", || {
                crate::blocking::willow_blocking_active_jobs() == 0
                    && crate::blocking::willow_blocking_queued_jobs() == 0
            });
            std::fs::remove_file(&path).unwrap();
        }
        reset_global_scheduler_for_test();
    }

    #[test]
    fn poll_state_remains_owned_after_frame_cleanup() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        let frame = NativeTaskFrame::<FsScalarFrame>::allocate().unwrap();
        let core = Arc::new(BlockingFsState::new());
        let weak = Arc::downgrade(&core);
        frame.store_native(FS_TASK_JOB_SLOT, Box::into_raw(Box::new(core)));
        // SAFETY: This test just populated the scalar frame with a boxed Arc and has
        // exclusive access while blocking_state clones it.
        let state = unsafe { blocking_state(frame.as_raw()) }.unwrap();
        // SAFETY: The test invokes cleanup synchronously on the same live frame.
        // The retained state is a separate Arc from the one in the slot.
        unsafe { cancel_blocking_fs(frame.as_raw()) };
        assert!(
            frame
                .load_native::<Arc<BlockingFsState>>(FS_TASK_JOB_SLOT)
                .is_null()
        );
        assert!(weak.upgrade().is_some());
        drop(state);
        assert!(weak.upgrade().is_none());
        // Repeated cleanup must leave the cleared slot alone.
        // SAFETY: The same test frame is still live and its job slot is now null.
        unsafe { cancel_blocking_fs(frame.as_raw()) };
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
    }

    #[test]
    fn temp_path_sanitizes_escaping_prefixes() {
        let _guard = isolated_runtime();
        let tmp = std::env::temp_dir();
        for evil in ["/etc/passwd", "../../escape", "a/b", "", "日本語"] {
            // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
            // during this call.
            let p = unsafe { willow_fs_temp_path(willow_string_from_str(evil)) };
            // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
            let path = unsafe { willow_string_as_str(p) };
            let path = std::path::Path::new(path);
            assert!(
                path.parent() == Some(tmp.as_path()),
                "{evil:?} -> {path:?} must be directly under {tmp:?}"
            );
            assert!(
                !path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .contains(".."),
                "{evil:?} -> {path:?} must not contain traversal"
            );
        }
    }

    #[test]
    fn fs_roundtrip_ok_and_err_tags() {
        let _guard = isolated_runtime();
        // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
        // during this call.
        let unique = unsafe { willow_fs_temp_path(willow_string_from_str("willow_fs_test")) };
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        let dir = std::path::PathBuf::from(unsafe { willow_string_as_str(unique) });
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.txt");
        // Every Ok/Err result allocates, and an allocation may collect
        // (WILLOW_GC_STRESS=alloc); root the path for as long as it is reused.
        let mut path = willow_string_from_str(file.to_str().unwrap());
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut path) };

        let contents = willow_string_from_str("hello");
        // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
        // during this call.
        let w = unsafe { willow_fs_write_string(path, contents) };
        assert_eq!(read_tag(w), 0, "write must be Ok");

        // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
        // during this call.
        assert_eq!(unsafe { willow_fs_exists(path) }, 1);

        // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
        // during this call.
        let r = unsafe { willow_fs_read_to_string(path) };
        assert_eq!(read_tag(r), 0, "read must be Ok");
        // SAFETY: The Result constructor reserves word-aligned GC storage for its tag and payload.
        let payload = unsafe { *((r as *const i64).add(1)) } as *const u8;
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        assert_eq!(unsafe { willow_string_as_str(payload) }, "hello");

        // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
        // during this call.
        let rm = unsafe { willow_fs_remove_file(path) };
        assert_eq!(read_tag(rm), 0);
        // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
        // during this call.
        assert_eq!(unsafe { willow_fs_exists(path) }, 0);

        // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
        // during this call.
        let missing = unsafe { willow_fs_read_to_string(path) };
        assert_eq!(read_tag(missing), 1, "read of removed file must be Err");
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn async_read_runs_through_blocking_task() {
        use crate::async_frame::async_frame_slot_offset;
        use crate::scheduler::{
            reset_global_scheduler_for_test, willow_sched_run_until, willow_sched_task_state,
        };

        let _guard = isolated_runtime();
        reset_global_scheduler_for_test();
        // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
        // during this call.
        let unique = unsafe { willow_fs_temp_path(willow_string_from_str("willow_async_fs_test")) };
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        let path = unsafe { willow_string_as_str(unique) }.to_string();
        std::fs::write(&path, "from-pool").unwrap();

        // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
        // during this call.
        let frame = unsafe { willow_fs_read_to_string_async(unique) };
        assert!(!frame.is_null());
        let frame = GcRootHandle::new(frame.cast());
        // SAFETY: The rooted read frame stores the published task ID in word slot 1.
        // The frame allocator and word-stride offset align the u64 load.
        let task_id = unsafe { *frame.load().add(async_frame_slot_offset(1)).cast::<u64>() };
        assert!(task_id > 0);
        assert_eq!(willow_sched_run_until(task_id), 1);
        assert_eq!(willow_sched_task_state(task_id), -1);

        crate::gc::willow_gc_collect();
        // SAFETY: run_until completed the read and the test root retains the frame.
        // Reference slot 0 is pointer-aligned and now contains the Result.
        let result = unsafe {
            *frame
                .load()
                .add(async_frame_slot_offset(0))
                .cast::<*mut u8>()
        };
        assert_eq!(read_tag(result), 0);
        // SAFETY: The Result constructor reserves word-aligned GC storage for its tag and payload.
        let string = unsafe { *((result as *const i64).add(1)) } as *const u8;
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        assert_eq!(unsafe { willow_string_as_str(string) }, "from-pool");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn published_fs_state_survives_poll_cleanup_before_spawn_returns() {
        use crate::scheduler::{reset_global_scheduler_for_test, willow_sched_run_until};

        let _guard = isolated_runtime();
        reset_global_scheduler_for_test();
        let mut observed = false;
        spawn_blocking_fs_after_publication(
            || BlockingFsResult::Exists(true),
            false,
            |task_id, frame, state| {
                let frame = GcRootHandle::new(frame.cast());
                // Wait until this job has published its result and the pool
                // has dropped its closure's Arc. No scheduler drive has
                // started yet, so exactly caller and frame ownership remain.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    let finished = state.result.lock().unwrap().is_some();
                    if finished && crate::blocking::willow_blocking_active_jobs() == 0 {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "blocking fs job did not finish"
                    );
                    std::thread::yield_now();
                }
                assert_eq!(
                    Arc::strong_count(state),
                    2,
                    "caller and frame must independently own state"
                );
                // The caller has not yet performed its post-publication
                // recheck. Drive the real published task to completion first,
                // allowing its poll callback to free the frame's boxed Arc.
                assert_eq!(willow_sched_run_until(task_id), 1);
                crate::gc::willow_gc_collect();
                // SAFETY: The test root retains the scalar frame across completion and collection.
                // The poll cleared the native slot before freeing its boxed Arc.
                let frame = unsafe { fs_frame(frame.load().cast()) };
                assert!(
                    frame
                        .load_native::<Arc<BlockingFsState>>(FS_TASK_JOB_SLOT)
                        .is_null()
                );
                assert_eq!(frame.load_word(FS_TASK_RESULT_SLOT), 1);
                assert!(state.result.lock().unwrap().is_none());
                assert_eq!(
                    Arc::strong_count(state),
                    1,
                    "spawning caller still owns its state"
                );
                observed = true;
            },
        );
        assert!(observed);
    }

    /// Overload policy: with every pool thread stalled and the
    /// bounded queue full, `*_async` calls still return real Tasks. Those Tasks
    /// keep their job, park as slot waiters (observable as BlockedSyscall,
    /// `willow_blocking_slot_waiters`, and the `blocking_queue_full` metric),
    /// nothing is refused or dropped, a cancelled waiter's job never runs, and
    /// once the pool drains every surviving Task completes with its result.
    #[test]
    fn queue_full_parks_fs_tasks_as_slot_waiters_instead_of_failing() {
        use crate::async_frame::async_frame_slot_offset;
        use crate::blocking::test_support::{
            pool_threads, queue_full_metric, stall_pool_threads, wait_until,
        };
        use crate::blocking::{
            willow_blocking_completed_jobs, willow_blocking_queue_capacity,
            willow_blocking_queued_jobs, willow_blocking_slot_waiters,
        };
        use crate::scheduler::{
            reset_global_scheduler_for_test, willow_sched_cancel, willow_sched_run_until,
            willow_sched_task_state,
        };

        fn task_id_of(frame: *mut c_void) -> u64 {
            // SAFETY: The test passes a rooted filesystem frame. Its ID is stored at this
            // word-stride offset from the word-aligned frame base.
            unsafe {
                *(frame as *const u8)
                    .add(async_frame_slot_offset(FS_TASK_ID_SLOT))
                    .cast::<u64>()
            }
        }

        let _guard = isolated_runtime();
        reset_global_scheduler_for_test();
        let clock = crate::test_sync::SchedulerClock::new();
        let threads = pool_threads();
        let completed_before = willow_blocking_completed_jobs();
        let metric_before = queue_full_metric();

        // Stall every pool thread, then fill the queue with fillers.
        let mut stalled = stall_pool_threads();
        let capacity = willow_blocking_queue_capacity() as usize;
        for _ in 0..capacity {
            crate::blocking::try_submit(Box::new(|| {}))
                .ok()
                .expect("filler fits");
        }
        assert_eq!(willow_blocking_queued_jobs() as usize, capacity);

        // Over capacity: every call still yields a Task, no null, no error.
        // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
        // during this call.
        let unique = unsafe { willow_fs_temp_path(willow_string_from_str("willow_fs_queue_full")) };
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        let path = unsafe { willow_string_as_str(unique) }.to_string();
        std::fs::write(&path, "x").unwrap();
        const OVERFLOW: usize = 5;
        // Spawning may collect (GC stress) and `unique` is unrooted, so hand
        // each call its own fresh path string.
        let mut frames: Vec<*mut c_void> = (0..OVERFLOW)
            // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
            // during this call.
            .map(|_| unsafe { willow_fs_exists_async(willow_string_from_str(&path)) })
            .collect();
        assert!(frames.iter().all(|frame| !frame.is_null()));
        // A completed Task unroots its frame; an awaiter would hold the Task
        // handle as a root, so root the frames here until their results are
        // read (the Vec never reallocates, so the slots stay put).
        for frame in frames.iter_mut() {
            // SAFETY: The Vec is fully populated. Each slot remains at its address until
            // all frame results are read and the roots are popped.
            unsafe { willow_push_root((frame as *mut *mut c_void).cast::<*mut u8>()) };
        }
        let ids: Vec<u64> = frames.iter().map(|frame| task_id_of(*frame)).collect();
        assert_eq!(
            willow_blocking_queued_jobs() as usize,
            capacity,
            "queue did not grow"
        );

        // Their first poll registers them as waiters; they park BlockedSyscall.
        crate::test_sync::drive_until("filesystem slot waiters park", &clock, || {
            ids.iter().all(|&id| willow_sched_task_state(id) == 7)
        });
        for id in &ids {
            assert_eq!(
                willow_sched_task_state(*id),
                7,
                "task {id} must park BlockedSyscall"
            );
        }
        assert_eq!(willow_blocking_slot_waiters() as usize, OVERFLOW);
        assert_eq!(queue_full_metric(), metric_before + OVERFLOW as u64);
        assert_eq!(willow_blocking_queued_jobs() as usize, capacity);

        // Cancel the oldest waiter: its job is dropped unrun and it leaves
        // the list without stranding the next waiter.
        willow_sched_cancel(ids[0]);
        willow_sched_run_until(ids[0]);
        assert_eq!(
            willow_sched_task_state(ids[0]),
            -1,
            "cancelled waiter is terminal"
        );
        assert_eq!(willow_blocking_slot_waiters() as usize, OVERFLOW - 1);

        // Drain: every survivor completes with the real result.
        stalled.release_all();
        for (index, id) in ids.iter().enumerate().skip(1) {
            // Several waiters may complete inside one drive, so the count
            // is not per target; the terminal state is.
            willow_sched_run_until(*id);
            assert_eq!(willow_sched_task_state(*id), -1, "task {index} completes");
            // SAFETY: The registered root in frames[index] retains the completed exists
            // frame. Its word-aligned scalar result slot now holds the answer.
            let exists = unsafe {
                *(frames[index] as *const u8)
                    .add(async_frame_slot_offset(FS_TASK_RESULT_SLOT))
                    .cast::<i64>()
            };
            assert_eq!(exists, 1, "task {index} must observe the file");
        }
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(OVERFLOW as i32) };
        assert_eq!(willow_blocking_slot_waiters(), 0);
        wait_until("queue to drain", || willow_blocking_queued_jobs() == 0);
        wait_until("jobs to finish", || {
            willow_blocking_completed_jobs() - completed_before
                >= (threads + capacity + OVERFLOW - 1) as i64
        });
        assert_eq!(
            willow_blocking_completed_jobs() - completed_before,
            (threads + capacity + OVERFLOW - 1) as i64,
            "the cancelled waiter's job must never have run"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn blocked_syscall_does_not_hold_a_scheduler_worker() {
        use crate::async_frame::async_frame_slot_offset;
        use crate::scheduler::{
            reset_global_scheduler_for_test, willow_sched_run_until, willow_sched_task_state,
        };

        unsafe extern "C" fn ready_after_blocked(frame: *mut c_void) -> i32 {
            // SAFETY: The scheduler retains the one-slot frame during this poll. The test
            // wrote blocked_id into its word-aligned slot 0 before spawning the task.
            let blocked_id = unsafe {
                *(frame
                    .cast::<u8>()
                    .add(async_frame_slot_offset(0))
                    .cast::<u64>())
            };
            if willow_sched_task_state(blocked_id) == 7 {
                crate::task::RUNTIME_POLL_READY
            } else {
                // Task publication is Ready; it becomes BlockedSyscall only
                // after its first poll. Do not assume worker dispatch order.
                crate::task::RUNTIME_POLL_YIELD
            }
        }

        let _guard = isolated_runtime();
        reset_global_scheduler_for_test();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocked_frame = spawn_blocking_fs(
            move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                BlockingFsResult::Exists(true)
            },
            false,
        );
        assert!(!blocked_frame.is_null());
        // SAFETY: Spawn initialized the live filesystem frame's ID slot before publication.
        // The allocator aligns the base and the slot offset is a multiple of a word.
        let blocked_id = unsafe {
            *(blocked_frame as *const u8)
                .add(crate::async_frame::async_frame_slot_offset(FS_TASK_ID_SLOT))
                .cast::<u64>()
        };
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("blocking pool job did not start");

        let quick_frame = crate::async_frame::willow_async_frame_alloc(1, 0);
        assert!(!quick_frame.is_null());
        // SAFETY: This newly allocated frame has one word-aligned scalar slot. The test
        // has exclusive access until it publishes quick_frame to the scheduler.
        unsafe {
            *quick_frame
                .cast::<u8>()
                .add(async_frame_slot_offset(0))
                .cast::<u64>() = blocked_id;
        }
        // SAFETY: The supplied callback matches this frame layout; the frame storage remains
        // live across scheduling and callback execution.
        let quick_id =
            unsafe { crate::scheduler::willow_sched_spawn(ready_after_blocked, quick_frame) };
        assert_eq!(
            willow_sched_run_until(quick_id),
            1,
            "a runnable task must complete while native work is still blocked"
        );
        assert_eq!(willow_sched_task_state(quick_id), -1);
        assert_eq!(
            willow_sched_task_state(blocked_id),
            7,
            "the detached task must remain observable as BlockedSyscall"
        );

        release_tx.send(()).unwrap();
        assert_eq!(willow_sched_run_until(blocked_id), 1);
        assert_eq!(willow_sched_task_state(blocked_id), -1);
    }
}
