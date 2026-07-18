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
}
