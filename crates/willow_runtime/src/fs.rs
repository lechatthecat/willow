//! File-system runtime for `std::fs` (willow-2s3 Stage 5, v1).
//!
//! V1 is SYNCHRONOUS under the hood: regular files are not epoll-pollable, so
//! non-blocking file I/O needs the blocking-syscall pool (willow-0a6k.5).
//! Errors surface as Willow `Result<_, IoError>` values built here — the
//! layouts must match the compiler's enum lowering:
//!   Result::Ok(v)  -> [tag = 0, payload]
//!   Result::Err(e) -> [tag = 1, payload]
//!   IoError::Failed(msg) -> [tag = 0, WillowString]
//! GC masks mark reference payloads; a nested build roots the inner object
//! across the outer allocation.

use crate::gc::{willow_alloc_typed, willow_pop_roots, willow_push_root};
use crate::string::{willow_string_as_str, willow_string_from_str};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Build `Ok(payload_word)`; `payload_is_ref` marks GC payloads in the mask.
fn alloc_ok(payload_word: i64, payload_is_ref: bool) -> *mut u8 {
    let mask: u64 = if payload_is_ref { 0b10 } else { 0 };
    let ptr = willow_alloc_typed(16, mask);
    if ptr.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        *(ptr as *mut i64) = 0;
        *((ptr as *mut i64).add(1)) = payload_word;
    }
    ptr
}

/// Build `Err(IoError::Failed(message))`.
fn alloc_io_err(message: &str) -> *mut u8 {
    let msg = willow_string_from_str(message);
    // Root the message across the IoError allocation.
    let mut msg_slot = msg;
    willow_push_root(&mut msg_slot as *mut *mut u8);
    let ioerr = willow_alloc_typed(16, 0b10);
    willow_pop_roots(1);
    if ioerr.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        *(ioerr as *mut i64) = 0; // IoError::Failed
        *((ioerr as *mut i64).add(1)) = msg_slot as i64;
    }
    let mut ioerr_slot = ioerr;
    willow_push_root(&mut ioerr_slot as *mut *mut u8);
    let err = willow_alloc_typed(16, 0b10);
    willow_pop_roots(1);
    if err.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        *(err as *mut i64) = 1; // Result::Err
        *((err as *mut i64).add(1)) = ioerr_slot as i64;
    }
    err
}

/// `fs::temp_path(prefix) -> String`: a process-unique path under the OS
/// temp directory (`<tmp>/<prefix>_<pid>_<counter>`). Uniqueness across
/// processes comes from the pid, within the process from the counter — so
/// parallel test/example runs cannot collide (willow-2s3 review fix).
#[unsafe(no_mangle)]
pub extern "C" fn willow_fs_temp_path(prefix: *const u8) -> *mut u8 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
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

/// `fs::read_to_string(path) -> Result<String, IoError>`.
#[unsafe(no_mangle)]
pub extern "C" fn willow_fs_read_to_string(path: *const u8) -> *mut u8 {
    let path = unsafe { willow_string_as_str(path) };
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let s = willow_string_from_str(&contents);
            let mut slot = s;
            willow_push_root(&mut slot as *mut *mut u8);
            let out = alloc_ok(slot as i64, true);
            willow_pop_roots(1);
            out
        }
        Err(e) => alloc_io_err(&format!("{path}: {e}")),
    }
}

/// `fs::write_string(path, contents) -> Result<void, IoError>`.
#[unsafe(no_mangle)]
pub extern "C" fn willow_fs_write_string(path: *const u8, contents: *const u8) -> *mut u8 {
    let path = unsafe { willow_string_as_str(path) };
    let contents = unsafe { willow_string_as_str(contents) };
    match std::fs::write(path, contents) {
        Ok(()) => alloc_ok(0, false),
        Err(e) => alloc_io_err(&format!("{path}: {e}")),
    }
}

/// `fs::exists(path) -> bool` (1/0).
#[unsafe(no_mangle)]
pub extern "C" fn willow_fs_exists(path: *const u8) -> i64 {
    let path = unsafe { willow_string_as_str(path) };
    i64::from(std::path::Path::new(path).exists())
}

/// `fs::remove_file(path) -> Result<void, IoError>`.
#[unsafe(no_mangle)]
pub extern "C" fn willow_fs_remove_file(path: *const u8) -> *mut u8 {
    let path = unsafe { willow_string_as_str(path) };
    match std::fs::remove_file(path) {
        Ok(()) => alloc_ok(0, false),
        Err(e) => alloc_io_err(&format!("{path}: {e}")),
    }
}

enum BlockingFsResult {
    Read(Result<String, String>),
    Unit(Result<(), String>),
    Exists(bool),
}

struct BlockingFsState {
    task_id: AtomicU64,
    result: Mutex<Option<BlockingFsResult>>,
}

impl BlockingFsState {
    fn new() -> Self {
        Self {
            task_id: AtomicU64::new(0),
            result: Mutex::new(None),
        }
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

unsafe fn frame_slot<T>(frame: *mut c_void, slot: usize) -> *mut T {
    unsafe {
        (frame as *mut u8)
            .add(crate::async_frame::async_frame_slot_offset(slot))
            .cast()
    }
}

unsafe fn blocking_state(frame: *mut c_void) -> Option<&'static Arc<BlockingFsState>> {
    let raw = unsafe { *frame_slot::<*mut Arc<BlockingFsState>>(frame, FS_TASK_JOB_SLOT) };
    unsafe { raw.as_ref() }
}

unsafe extern "C" fn poll_blocking_fs(frame: *mut c_void) -> i32 {
    let Some(state) = (unsafe { blocking_state(frame) }) else {
        return crate::task::RUNTIME_POLL_READY;
    };
    let result = state
        .result
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    let Some(result) = result else {
        return crate::task::RUNTIME_POLL_BLOCKED_SYSCALL;
    };
    let word = match result {
        BlockingFsResult::Read(Ok(contents)) => {
            let string = willow_string_from_str(&contents);
            let mut root = string;
            willow_push_root(&mut root as *mut *mut u8);
            let result = alloc_ok(root as i64, true);
            willow_pop_roots(1);
            result as i64
        }
        BlockingFsResult::Read(Err(error)) | BlockingFsResult::Unit(Err(error)) => {
            alloc_io_err(&error) as i64
        }
        BlockingFsResult::Unit(Ok(())) => alloc_ok(0, false) as i64,
        BlockingFsResult::Exists(exists) => i64::from(exists),
    };
    unsafe {
        *frame_slot::<i64>(frame, FS_TASK_RESULT_SLOT) = word;
        let raw = *frame_slot::<*mut Arc<BlockingFsState>>(frame, FS_TASK_JOB_SLOT);
        *frame_slot::<*mut Arc<BlockingFsState>>(frame, FS_TASK_JOB_SLOT) = std::ptr::null_mut();
        drop(Box::from_raw(raw));
    }
    crate::task::RUNTIME_POLL_READY
}

unsafe extern "C" fn cancel_blocking_fs(frame: *mut c_void) {
    unsafe {
        let slot = frame_slot::<*mut Arc<BlockingFsState>>(frame, FS_TASK_JOB_SLOT);
        let raw = *slot;
        if !raw.is_null() {
            *slot = std::ptr::null_mut();
            drop(Box::from_raw(raw));
        }
    }
}

fn spawn_blocking_fs(
    work: impl FnOnce() -> BlockingFsResult + Send + 'static,
    result_is_gc_ref: bool,
) -> *mut c_void {
    let mask = u64::from(result_is_gc_ref) << FS_TASK_RESULT_SLOT;
    let frame = crate::async_frame::willow_async_frame_alloc(3, mask);
    if frame.is_null() {
        return frame;
    }
    unsafe {
        *((frame as *mut u8)
            .add(crate::async_frame::ASYNC_FRAME_SLOT_COUNT_OFFSET)
            .cast::<i64>()) = 3;
    }
    let state = Arc::new(BlockingFsState::new());
    let state_for_work = Arc::clone(&state);
    let state_box = Box::into_raw(Box::new(state));
    unsafe {
        *frame_slot::<*mut Arc<BlockingFsState>>(frame, FS_TASK_JOB_SLOT) = state_box;
    }
    if !crate::blocking::submit(move || {
        let result = work();
        state_for_work.finish(result);
    }) {
        unsafe { cancel_blocking_fs(frame) };
        return std::ptr::null_mut();
    }
    let task_id = crate::scheduler::willow_sched_spawn(poll_blocking_fs, frame);
    unsafe {
        *frame_slot::<u64>(frame, FS_TASK_ID_SLOT) = task_id;
        (&*state_box).task_id.store(task_id, Ordering::Release);
    }
    crate::scheduler::willow_sched_set_cancel_fn(task_id, cancel_blocking_fs);
    // Close the publication race: if the pool job finished BEFORE the
    // task-id store above, its `finish()` loaded 0 and could not wake us —
    // and the task may already be parked BlockedSyscall with the result
    // sitting in the mutex. Re-check and wake unconditionally; a duplicate
    // wake of a Ready/Running task is a no-op / wake_requested consume.
    {
        let has_result = unsafe { &*state_box }
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
#[unsafe(no_mangle)]
pub extern "C" fn willow_fs_read_to_string_async(path: *const u8) -> *mut c_void {
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

#[unsafe(no_mangle)]
pub extern "C" fn willow_fs_write_string_async(
    path: *const u8,
    contents: *const u8,
) -> *mut c_void {
    let path = unsafe { willow_string_as_str(path) }.to_string();
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

#[unsafe(no_mangle)]
pub extern "C" fn willow_fs_exists_async(path: *const u8) -> *mut c_void {
    let path = unsafe { willow_string_as_str(path) }.to_string();
    spawn_blocking_fs(
        move || BlockingFsResult::Exists(std::path::Path::new(&path).exists()),
        false,
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_fs_remove_file_async(path: *const u8) -> *mut c_void {
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
    use crate::gc::runtime_test_guard;

    fn read_tag(ptr: *mut u8) -> i64 {
        unsafe { *(ptr as *const i64) }
    }

    #[test]
    fn temp_path_sanitizes_escaping_prefixes() {
        let _guard = runtime_test_guard();
        let tmp = std::env::temp_dir();
        for evil in ["/etc/passwd", "../../escape", "a/b", "", "日本語"] {
            let p = willow_fs_temp_path(willow_string_from_str(evil));
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
        let _guard = runtime_test_guard();
        let unique = willow_fs_temp_path(willow_string_from_str("willow_fs_test"));
        let dir = std::path::PathBuf::from(unsafe { willow_string_as_str(unique) });
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.txt");
        let path = willow_string_from_str(file.to_str().unwrap());

        let contents = willow_string_from_str("hello");
        let w = willow_fs_write_string(path, contents);
        assert_eq!(read_tag(w), 0, "write must be Ok");

        assert_eq!(willow_fs_exists(path), 1);

        let r = willow_fs_read_to_string(path);
        assert_eq!(read_tag(r), 0, "read must be Ok");
        let payload = unsafe { *((r as *const i64).add(1)) } as *const u8;
        assert_eq!(unsafe { willow_string_as_str(payload) }, "hello");

        let rm = willow_fs_remove_file(path);
        assert_eq!(read_tag(rm), 0);
        assert_eq!(willow_fs_exists(path), 0);

        let missing = willow_fs_read_to_string(path);
        assert_eq!(read_tag(missing), 1, "read of removed file must be Err");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn async_read_runs_through_blocking_task() {
        use crate::async_frame::async_frame_slot_offset;
        use crate::scheduler::{
            reset_global_scheduler_for_test, willow_sched_run_until, willow_sched_task_state,
        };

        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        let unique = willow_fs_temp_path(willow_string_from_str("willow_async_fs_test"));
        let path = unsafe { willow_string_as_str(unique) }.to_string();
        std::fs::write(&path, "from-pool").unwrap();

        let frame = willow_fs_read_to_string_async(unique);
        let task_id = unsafe {
            *(frame as *const u8)
                .add(async_frame_slot_offset(1))
                .cast::<u64>()
        };
        assert!(task_id > 0);
        assert_eq!(willow_sched_run_until(task_id), 1);
        assert_eq!(willow_sched_task_state(task_id), 3);

        let result = unsafe {
            *(frame as *const u8)
                .add(async_frame_slot_offset(0))
                .cast::<*mut u8>()
        };
        assert_eq!(read_tag(result), 0);
        let string = unsafe { *((result as *const i64).add(1)) } as *const u8;
        assert_eq!(unsafe { willow_string_as_str(string) }, "from-pool");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn blocked_syscall_does_not_hold_a_scheduler_worker() {
        use crate::scheduler::{
            reset_global_scheduler_for_test, willow_sched_run_until, willow_sched_task_state,
        };

        unsafe extern "C" fn ready_immediately(_frame: *mut c_void) -> i32 {
            crate::task::RUNTIME_POLL_READY
        }

        let _guard = runtime_test_guard();
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
        let blocked_id = unsafe {
            *(blocked_frame as *const u8)
                .add(crate::async_frame::async_frame_slot_offset(FS_TASK_ID_SLOT))
                .cast::<u64>()
        };
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("blocking pool job did not start");

        let quick_id =
            crate::scheduler::willow_sched_spawn(ready_immediately, std::ptr::null_mut());
        assert_eq!(
            willow_sched_run_until(quick_id),
            1,
            "a runnable task must complete while native work is still blocked"
        );
        assert_eq!(willow_sched_task_state(quick_id), 3);
        assert_eq!(
            willow_sched_task_state(blocked_id),
            7,
            "the detached task must remain observable as BlockedSyscall"
        );

        release_tx.send(()).unwrap();
        assert_eq!(willow_sched_run_until(blocked_id), 1);
        assert_eq!(willow_sched_task_state(blocked_id), 3);
    }
}
