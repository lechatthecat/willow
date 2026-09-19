// WillowString — GC-managed string heap object.
//
// Payload layout (pointed to by a *mut u8 from the central GC allocator):
//   offset  0: len: i64     — byte count (UTF-8, excluding the NUL terminator)
//   offset  8: bytes...     — UTF-8 encoded content
//   offset  8+len: 0u8      — NUL terminator for C interop convenience
//
// gc_ref_mask = 0: no child GC references inside a string.
//
// String literals are allocated once and kept alive permanently via
// willow_gc_add_runtime_root so that gc_collect() never frees them.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, TryLockError};

use crate::gc::{GcObjectKind, willow_alloc_with_layout, willow_gc_add_runtime_root};

// ---------------------------------------------------------------------------
// Core allocation helpers
// ---------------------------------------------------------------------------

/// Include the length word and NUL without overflowing either size domain.
fn string_payload_size(len: usize) -> Option<i64> {
    let size = len.checked_add(9)?;
    i64::try_from(size).ok()
}

/// Allocate a new WillowString from a raw UTF-8 byte slice.
/// Invalid UTF-8 is a fatal runtime invariant violation.
/// Returns a pointer to the payload (the `len` field at offset 0).
#[unsafe(no_mangle)]
pub extern "C" fn willow_string_alloc(bytes: *const u8, len: i64) -> *mut u8 {
    if len < 0 {
        return std::ptr::null_mut();
    }
    let len_usize = len as usize;
    if len_usize > 0 && bytes.is_null() {
        return std::ptr::null_mut();
    }
    let Some(payload_size) = string_payload_size(len_usize) else {
        return std::ptr::null_mut();
    };
    if len_usize > 0 {
        let input = unsafe { std::slice::from_raw_parts(bytes, len_usize) };
        require_utf8(input);
    }
    unsafe { alloc_validated_bytes(bytes, len_usize, payload_size) }
}

// Caller guarantees valid UTF-8 bytes and a checked payload size.
unsafe fn alloc_validated_bytes(bytes: *const u8, len_usize: usize, payload_size: i64) -> *mut u8 {
    let ptr = willow_alloc_with_layout(GcObjectKind::String, 0, payload_size, 0);
    if ptr.is_null() {
        return ptr;
    }
    unsafe {
        *(ptr as *mut i64) = len_usize as i64;
        if len_usize > 0 {
            std::ptr::copy_nonoverlapping(bytes, ptr.add(8), len_usize);
        }
        *ptr.add(8 + len_usize) = 0; // NUL terminator
    }
    ptr
}

// ---------------------------------------------------------------------------
// Literal interning: lazily allocate once, root permanently.
// ---------------------------------------------------------------------------

// SAFETY: WillowString pointers in this cache are valid GC heap objects
// permanently registered via willow_gc_add_runtime_root, so they are never
// freed or moved. Sharing the raw pointer value across threads is safe
// because the GC is stop-the-world and the value itself is never mutated.
struct SendPtr(*mut u8);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

static LITERAL_CACHE: Mutex<Option<HashMap<usize, SendPtr>>> = Mutex::new(None);

fn lock_literal_cache() -> MutexGuard<'static, Option<HashMap<usize, SendPtr>>> {
    loop {
        match LITERAL_CACHE.try_lock() {
            Ok(guard) => return guard,
            Err(TryLockError::Poisoned(poisoned)) => return poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                // Literal initialization can allocate (and therefore collect)
                // while holding this cache lock. A competing mutator must keep
                // reaching safepoints instead of blocking in the OS mutex, or
                // a stop-the-world collector can wait forever for it to park.
                crate::gc::willow_gc_safepoint();
                std::thread::yield_now();
            }
        }
    }
}

/// Clear the string literal cache.
/// Must be called from willow_gc_init / reset_internal so that stale pointers
/// from a previous GC lifetime are never returned after the heap is reset.
pub(crate) fn clear_string_literal_cache() {
    let mut guard = lock_literal_cache();
    if let Some(cache) = guard.as_mut() {
        cache.clear();
    }
}

/// Allocate or retrieve a permanently-rooted WillowString for a string literal.
///
/// `bytes` must point to static read-only data for the lifetime of the process.
/// `len` is the byte length of the string (excluding NUL).
///
/// The first call with a given `bytes` pointer allocates a WillowString and
/// registers it as a permanent GC root.  Subsequent calls return the same
/// pointer.
#[unsafe(no_mangle)]
pub extern "C" fn willow_string_literal(bytes: *const u8, len: i64) -> *mut u8 {
    let key = bytes as usize;
    let mut guard = lock_literal_cache();
    let cache = guard.get_or_insert_with(HashMap::new);
    if let Some(p) = cache.get(&key) {
        return p.0;
    }
    let ptr = willow_string_alloc(bytes, len);
    willow_gc_add_runtime_root(ptr);
    cache.insert(key, SendPtr(ptr));
    ptr
}

// ---------------------------------------------------------------------------
// Concatenation
// ---------------------------------------------------------------------------

/// Read the byte-slice from a WillowString payload pointer.
/// Returns `(ptr_to_bytes, len)` or `(null, 0)` if `s` is null.
#[inline]
unsafe fn ws_as_bytes(s: *const u8) -> (*const u8, usize) {
    if s.is_null() {
        return (std::ptr::null(), 0);
    }
    let len = unsafe { *(s as *const i64) } as usize;
    (unsafe { s.add(8) }, len)
}

/// Content equality of two strings (willow-rpxh): `==` on `String` must
/// compare bytes, not pointers. Null-safe: two nils are equal, nil never
/// equals a real string (this also gives `s == nil` the right meaning).
#[unsafe(no_mangle)]
pub extern "C" fn willow_string_eq(lhs: *const u8, rhs: *const u8) -> i64 {
    if lhs == rhs {
        return 1;
    }
    if lhs.is_null() || rhs.is_null() {
        return 0;
    }
    let l = unsafe { willow_string_as_str(lhs) };
    let r = unsafe { willow_string_as_str(rhs) };
    i64::from(l == r)
}

/// Concatenate two WillowStrings and return a new GC-managed WillowString.
#[unsafe(no_mangle)]
pub extern "C" fn willow_string_concat(lhs: *const u8, rhs: *const u8) -> *mut u8 {
    let (lhs_bytes, lhs_len) = unsafe { ws_as_bytes(lhs) };
    let (rhs_bytes, rhs_len) = unsafe { ws_as_bytes(rhs) };
    let Some((total_len, payload_size)) = lhs_len
        .checked_add(rhs_len)
        .and_then(|len| string_payload_size(len).map(|size| (len, size)))
    else {
        crate::panic_context::raise_language_message("string concatenation size overflow");
        return std::ptr::null_mut();
    };
    let ptr = willow_alloc_with_layout(GcObjectKind::String, 0, payload_size, 0);
    if ptr.is_null() {
        return ptr;
    }
    unsafe {
        *(ptr as *mut i64) = total_len as i64;
        if lhs_len > 0 {
            std::ptr::copy_nonoverlapping(lhs_bytes, ptr.add(8), lhs_len);
        }
        if rhs_len > 0 {
            std::ptr::copy_nonoverlapping(rhs_bytes, ptr.add(8 + lhs_len), rhs_len);
        }
        *ptr.add(8 + total_len) = 0;
    }
    ptr
}

// ---------------------------------------------------------------------------
// Conversion helpers used by print, math, args
// ---------------------------------------------------------------------------

/// Allocate a WillowString from a Rust `&str`.
pub fn willow_string_from_str(s: &str) -> *mut u8 {
    let Some(payload_size) = string_payload_size(s.len()) else {
        return std::ptr::null_mut();
    };
    // Rust strings already guarantee UTF-8: do not rescan formatted/file text.
    unsafe { alloc_validated_bytes(s.as_ptr(), s.len(), payload_size) }
}

fn require_utf8(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes)
        .unwrap_or_else(|_| crate::panic_context::fatal_invariant("invalid UTF-8 in WillowString"))
}

/// Read a WillowString payload as a Rust `&str`.
/// Returns `""` on null; invalid UTF-8 is a fatal runtime invariant violation.
///
/// # Safety
/// `s` must be null or a valid pointer to a WillowString allocated by this
/// runtime; the returned slice borrows that allocation for `'a`.
pub unsafe fn willow_string_as_str<'a>(s: *const u8) -> &'a str {
    if s.is_null() {
        return "";
    }
    let len = unsafe { *(s as *const i64) } as usize;
    let bytes = unsafe { std::slice::from_raw_parts(s.add(8), len) };
    require_utf8(bytes)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{runtime_test_guard, willow_gc_init};

    #[test]
    fn invalid_utf8_is_process_fatal() {
        const CHILD: &str = "WILLOW_TEST_INVALID_STRING_UTF8";
        if let Ok(case) = std::env::var(CHILD) {
            let _guard = runtime_test_guard();
            willow_gc_init();
            match case.as_str() {
                "alloc" => {
                    willow_string_alloc([0xff].as_ptr(), 1);
                }
                "literal" => {
                    willow_string_literal(b"\xc0\x80".as_ptr(), 2);
                }
                "truncated" => {
                    willow_string_alloc(b"\xe2\x82".as_ptr(), 2);
                }
                "corrupt" => {
                    let ptr = willow_string_from_str("a");
                    unsafe {
                        *ptr.add(8) = 0xff;
                        willow_string_as_str(ptr);
                    }
                }
                _ => panic!("unknown child case"),
            }
            return;
        }
        for case in ["alloc", "literal", "truncated", "corrupt"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "string::tests::invalid_utf8_is_process_fatal",
                    "--nocapture",
                ])
                .env(CHILD, case)
                .output()
                .expect("run invalid UTF-8 subprocess");
            assert!(
                !output.status.success(),
                "{case}: invalid UTF-8 was accepted"
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("runtime fatal: invalid UTF-8 in WillowString"),
                "{case}: {stderr}"
            );
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(output.status.signal(), Some(libc::SIGABRT));
            }
        }
    }

    #[test]
    fn utf8_byte_entries_preserve_content() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        for repeats in [0, 1, 16, 256, 4096] {
            let text = "a\0é水🦀".repeat(repeats);
            let ptr = willow_string_alloc(text.as_ptr(), text.len() as i64);
            assert!(!ptr.is_null());
            assert_eq!(unsafe { willow_string_as_str(ptr) }, text);
            assert_eq!(unsafe { *ptr.add(8 + text.len()) }, 0);
        }
        let text = "literal é水🦀";
        let ptr = willow_string_literal(text.as_ptr(), text.len() as i64);
        assert_eq!(unsafe { willow_string_as_str(ptr) }, text);
        assert_eq!(willow_string_literal(text.as_ptr(), text.len() as i64), ptr);
        assert_eq!(unsafe { willow_string_as_str(std::ptr::null()) }, "");
    }

    #[test]
    fn string_payload_size_boundaries() {
        assert_eq!(string_payload_size(0), Some(9));
        assert_eq!(string_payload_size(i64::MAX as usize - 9), Some(i64::MAX));
        assert_eq!(string_payload_size(i64::MAX as usize - 8), None);
        assert_eq!(string_payload_size(usize::MAX), None);
    }

    #[test]
    fn concat_increasing_sizes_preserves_bytes_and_terminator() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        for (left_len, right_len) in [(0, 0), (1, 1), (16, 256), (4096, 1)] {
            let left = "a".repeat(left_len);
            let right = "b".repeat(right_len);
            let mut lhs = willow_string_from_str(&left);
            crate::gc::willow_push_root(&mut lhs);
            let mut rhs = willow_string_from_str(&right);
            crate::gc::willow_push_root(&mut rhs);
            let result = willow_string_concat(lhs, rhs);
            assert_eq!(unsafe { willow_string_as_str(result) }, left + &right);
            assert_eq!(unsafe { *result.add(8 + left_len + right_len) }, 0);
            crate::gc::willow_pop_roots(2);
        }
    }

    #[test]
    fn concat_overflow_raises_before_reading_content() {
        use crate::panic_context::*;
        let _guard = runtime_test_guard();
        willow_gc_init();
        let previous = replace_current_context(Some(std::sync::Arc::new(PanicContext::new(944))));
        // Only the length words exist: every case must fail before copying bytes.
        for (left, right) in [(i64::MAX, 1_i64), (i64::MAX - 8, 0), (-1, 1)] {
            let result =
                willow_string_concat((&left as *const i64).cast(), (&right as *const i64).cast());
            assert!(result.is_null());
            assert_eq!(willow_panic_depth(), 1);
            willow_panic_enter_defer();
            let info = willow_panic_recover();
            willow_panic_leave_defer();
            assert_eq!(
                unsafe { panic_info_message(info) },
                "string concatenation size overflow"
            );
            willow_panic_release_recovered(info);
        }
        replace_current_context(previous);
    }

    unsafe fn ws_to_string(ptr: *const u8) -> String {
        unsafe { willow_string_as_str(ptr) }.to_string()
    }

    #[test]
    fn string_unit_01_alloc_roundtrip() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let s = b"hello";
        let ptr = willow_string_alloc(s.as_ptr(), 5);
        assert!(!ptr.is_null());
        assert_eq!(unsafe { ws_to_string(ptr) }, "hello");
    }

    #[test]
    fn string_unit_02_empty_string() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let ptr = willow_string_alloc(std::ptr::null(), 0);
        assert!(!ptr.is_null());
        assert_eq!(unsafe { ws_to_string(ptr) }, "");
    }

    #[test]
    fn string_unit_03_concat_two_strings() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let a = willow_string_alloc(b"hello".as_ptr(), 5);
        let b = willow_string_alloc(b" world".as_ptr(), 6);
        let c = willow_string_concat(a, b);
        assert_eq!(unsafe { ws_to_string(c) }, "hello world");
    }

    #[test]
    fn string_unit_04_concat_null_lhs_is_rhs() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let b = willow_string_alloc(b"rhs".as_ptr(), 3);
        let c = willow_string_concat(std::ptr::null(), b);
        assert_eq!(unsafe { ws_to_string(c) }, "rhs");
    }

    #[test]
    fn string_unit_05_concat_null_rhs_is_lhs() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let a = willow_string_alloc(b"lhs".as_ptr(), 3);
        let c = willow_string_concat(a, std::ptr::null());
        assert_eq!(unsafe { ws_to_string(c) }, "lhs");
    }

    #[test]
    fn string_unit_06_literal_is_stable_across_gc() {
        use crate::gc::willow_gc_collect;
        let _guard = runtime_test_guard();
        willow_gc_init();
        let bytes = b"stable";
        let p1 = willow_string_literal(bytes.as_ptr(), 6);
        willow_gc_collect();
        // Second call should return the same pointer
        let p2 = willow_string_literal(bytes.as_ptr(), 6);
        assert_eq!(p1, p2);
        assert_eq!(unsafe { ws_to_string(p1) }, "stable");
    }

    #[test]
    fn string_unit_07_nul_terminator_present() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let ptr = willow_string_alloc(b"ab".as_ptr(), 2);
        // NUL at offset 8+2
        let nul = unsafe { *ptr.add(10) };
        assert_eq!(nul, 0);
    }

    // Fix 1: willow_string_alloc hardening tests
    #[test]
    fn string_unit_08_negative_len_returns_null() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        assert!(willow_string_alloc(b"abc".as_ptr(), -1).is_null());
    }

    #[test]
    fn string_unit_09_positive_len_null_bytes_returns_null() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        assert!(willow_string_alloc(std::ptr::null(), 3).is_null());
    }

    #[test]
    fn string_unit_10_zero_len_null_bytes_is_empty() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let ptr = willow_string_alloc(std::ptr::null(), 0);
        assert!(!ptr.is_null());
        assert_eq!(unsafe { willow_string_as_str(ptr) }, "");
    }

    // Fix 4: string literal cache cleared on GC reset
    #[test]
    fn string_unit_11_literal_cache_safe_after_gc_init() {
        use crate::gc::{willow_gc_collect, willow_gc_init};
        let _guard = runtime_test_guard();
        willow_gc_init();
        let bytes = b"abc";
        let p1 = willow_string_literal(bytes.as_ptr(), 3);
        assert!(!p1.is_null());
        // Reset GC — this must clear the cache so p1 is no longer returned.
        willow_gc_init();
        let p2 = willow_string_literal(bytes.as_ptr(), 3);
        assert!(!p2.is_null());
        // p2 must be a fresh, valid allocation regardless of whether it
        // happens to reuse the same address.
        assert_eq!(unsafe { willow_string_as_str(p2) }, "abc");
        willow_gc_collect();
        assert_eq!(unsafe { willow_string_as_str(p2) }, "abc");
    }
}
