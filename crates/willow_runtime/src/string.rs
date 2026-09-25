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

use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, Ordering};

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

// Only initialized slots are retained, for heap reset. This is not a lookup
// cache: normal evaluation never takes this lock or searches this vector.
static INITIALIZED_LITERAL_SLOTS: Mutex<Vec<&'static AtomicPtr<u8>>> = Mutex::new(Vec::new());

// GC payloads are aligned and can never have this address.
const INITIALIZING: *mut u8 = std::ptr::without_provenance_mut(1);

/// Reset is quiescent: no compiled code or literal initializer may be running.
/// Registered slots have process lifetime, including across GC lifetimes.
pub(crate) fn clear_string_literal_slots() {
    for slot in INITIALIZED_LITERAL_SLOTS.lock().unwrap().drain(..) {
        slot.store(std::ptr::null_mut(), Ordering::Release);
    }
}

/// Retrieve a permanently rooted literal using compiler-owned static storage.
///
/// `slot` must point to a pointer-aligned, initially zero atomic pointer with
/// process lifetime. Every use of a slot must supply the same valid static UTF-8
/// bytes and length. Different literals (including prefixes) need distinct slots.
/// Heap reset must be quiescent; it clears every initialized slot before reuse.
#[unsafe(no_mangle)]
pub extern "C" fn willow_string_literal_slot(
    slot: *const AtomicPtr<u8>,
    bytes: *const u8,
    len: i64,
) -> *mut u8 {
    // SAFETY: compiler-generated writable static slot obeys the contract above.
    let slot: &'static AtomicPtr<u8> = unsafe { &*slot };
    loop {
        let value = slot.load(Ordering::Acquire);
        if value != INITIALIZING && !value.is_null() {
            return value;
        }
        if value.is_null()
            && slot
                .compare_exchange(value, INITIALIZING, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        {
            let ptr = willow_string_alloc(bytes, len);
            if !ptr.is_null() {
                willow_gc_add_runtime_root(ptr);
                // No GC allocation/safepoint while holding the reset registry
                // lock, and no scheduler preemption can strand its owner.
                let _no_preempt = crate::preempt::NoPreemptGuard::enter();
                INITIALIZED_LITERAL_SLOTS.lock().unwrap().push(slot);
            }
            // Root and fully initialized bytes become visible together. A
            // failed allocation restores empty storage so a later call retries.
            slot.store(ptr, Ordering::Release);
            return ptr;
        }
        // The winning mutator may allocate/collect. Losers must participate in
        // safepoints rather than block a collector waiting for them to park.
        crate::gc::willow_gc_safepoint();
        std::thread::yield_now();
    }
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
    let (_, lhs_len) = unsafe { ws_as_bytes(lhs) };
    let (_, rhs_len) = unsafe { ws_as_bytes(rhs) };
    let Some((total_len, payload_size)) = lhs_len
        .checked_add(rhs_len)
        .and_then(|len| string_payload_size(len).map(|size| (len, size)))
    else {
        crate::panic_context::raise_language_message("string concatenation size overflow");
        return std::ptr::null_mut();
    };
    let mut lhs = lhs.cast_mut();
    let mut rhs = rhs.cast_mut();
    crate::gc::willow_push_root(&mut lhs);
    crate::gc::willow_push_root(&mut rhs);
    let ptr = willow_alloc_with_layout(GcObjectKind::String, 0, payload_size, 0);
    if ptr.is_null() {
        crate::gc::willow_pop_roots(2);
        return ptr;
    }
    // Derive interior addresses from the root slots after the allocation.
    let (lhs_bytes, _) = unsafe { ws_as_bytes(lhs) };
    let (rhs_bytes, _) = unsafe { ws_as_bytes(rhs) };
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
    crate::gc::willow_pop_roots(2);
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
    #[ignore = "manual literal hot-path measurement"]
    fn literal_hot_path_measurement() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        static SLOT: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
        let text = b"literal-heavy loop";
        willow_string_literal_slot(&SLOT, text.as_ptr(), text.len() as i64);
        for hits in [1_000_000, 4_000_000, 16_000_000] {
            let start = std::time::Instant::now();
            for _ in 0..hits {
                std::hint::black_box(willow_string_literal_slot(
                    &SLOT,
                    std::hint::black_box(text.as_ptr()),
                    text.len() as i64,
                ));
            }
            println!("hits={hits} elapsed_ns={}", start.elapsed().as_nanos());
        }
    }

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
                    static SLOT: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
                    willow_string_literal_slot(&SLOT, b"\xc0\x80".as_ptr(), 2);
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
        static SLOT: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
        let text = "literal é水🦀";
        let ptr = willow_string_literal_slot(&SLOT, text.as_ptr(), text.len() as i64);
        assert_eq!(unsafe { willow_string_as_str(ptr) }, text);
        assert_eq!(
            willow_string_literal_slot(&SLOT, text.as_ptr(), text.len() as i64),
            ptr
        );
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
    fn concat_roots_inputs_across_allocation_and_balances_depth() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        struct RestoreStress;
        impl Drop for RestoreStress {
            fn drop(&mut self) {
                crate::gc::set_gc_stress_for_test(None);
            }
        }
        let _restore = RestoreStress;
        crate::gc::set_gc_stress_for_test(Some("alloc"));
        for (left, right) in [("", ""), ("left", "右"), ("same", "same")] {
            let mut lhs = willow_string_from_str(left);
            crate::gc::willow_push_root(&mut lhs);
            let rhs = willow_string_from_str(right);
            crate::gc::willow_pop_roots(1);
            // No caller roots remain: concat must protect its own inputs.
            let depth = crate::gc::willow_root_depth();
            let result = willow_string_concat(lhs, rhs);
            assert_eq!(crate::gc::willow_root_depth(), depth);
            assert_eq!(
                unsafe { willow_string_as_str(result) },
                format!("{left}{right}")
            );
            assert_eq!(unsafe { *result.add(8 + left.len() + right.len()) }, 0);
        }
        let depth = crate::gc::willow_root_depth();
        let result = willow_string_concat(std::ptr::null(), std::ptr::null());
        assert_eq!(unsafe { willow_string_as_str(result) }, "");
        assert_eq!(crate::gc::willow_root_depth(), depth);
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
    fn literal_slots_preserve_prefixes_content_gc_and_reset() {
        let _guard = runtime_test_guard();
        static SLOTS: [AtomicPtr<u8>; 3] = [const { AtomicPtr::new(std::ptr::null_mut()) }; 3];
        static TEXT: &str = "éclair";
        for _ in 0..3 {
            willow_gc_init();
            assert!(
                SLOTS
                    .iter()
                    .all(|slot| slot.load(Ordering::Acquire).is_null())
            );
            let pointers: Vec<_> = SLOTS
                .iter()
                .zip([0, 2, 7])
                .map(|(slot, len)| willow_string_literal_slot(slot, TEXT.as_ptr(), len))
                .collect();
            crate::gc::willow_gc_collect();
            for ((slot, len), ptr) in SLOTS.iter().zip([0, 2, 7]).zip(&pointers) {
                assert_eq!(willow_string_literal_slot(slot, TEXT.as_ptr(), len), *ptr);
                assert_eq!(unsafe { willow_string_as_str(*ptr) }, &TEXT[..len as usize]);
                assert_eq!(unsafe { *ptr.add(8 + len as usize) }, 0);
            }
            assert_ne!(pointers[0], pointers[1]);
            assert_ne!(pointers[1], pointers[2]);
        }
    }

    #[test]
    fn literal_slots_invalid_input_restores_empty_slot() {
        let _guard = runtime_test_guard();
        static SLOT: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
        willow_gc_init();
        for (bytes, len) in [(b"abc".as_ptr(), -1), (std::ptr::null(), 3)] {
            assert!(willow_string_literal_slot(&SLOT, bytes, len).is_null());
            assert!(SLOT.load(Ordering::Acquire).is_null());
            assert!(INITIALIZED_LITERAL_SLOTS.lock().unwrap().is_empty());
        }
        let ptr = willow_string_literal_slot(&SLOT, b"abc".as_ptr(), 3);
        assert_eq!(unsafe { willow_string_as_str(ptr) }, "abc");
    }

    #[test]
    fn literal_slots_repeated_hits_do_not_allocate() {
        let _guard = runtime_test_guard();
        static SLOTS: [AtomicPtr<u8>; 256] = [const { AtomicPtr::new(std::ptr::null_mut()) }; 256];
        for count in [16, 64, 256] {
            for repetitions in [1, 8, 32] {
                willow_gc_init();
                let before = crate::gc::telemetry_heap_snapshot().0.allocation_count;
                let pointers: Vec<_> = SLOTS[..count]
                    .iter()
                    .map(|slot| willow_string_literal_slot(slot, b"literal".as_ptr(), 7))
                    .collect();
                let after_misses = crate::gc::telemetry_heap_snapshot().0.allocation_count;
                assert_eq!(after_misses - before, count as u64);
                for _ in 0..repetitions {
                    for (slot, ptr) in SLOTS.iter().zip(&pointers) {
                        assert_eq!(
                            willow_string_literal_slot(slot, b"literal".as_ptr(), 7),
                            *ptr
                        );
                    }
                }
                assert_eq!(INITIALIZED_LITERAL_SLOTS.lock().unwrap().len(), count);
                assert_eq!(
                    crate::gc::telemetry_heap_snapshot().0.allocation_count,
                    after_misses
                );
                println!(
                    "literals={count} hits={} allocations={} entries={count}",
                    count * repetitions,
                    after_misses - before
                );
            }
        }
    }

    #[test]
    fn literal_slots_waiters_cooperate_with_gc() {
        use crate::gc::*;
        use std::sync::atomic::AtomicBool;
        let _guard = runtime_test_guard();
        willow_gc_init();
        static SLOT: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
        SLOT.store(INITIALIZING, Ordering::Release);
        let ready = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mutator = crate::gc::MutatorRegistration::new();
                ready.store(true, Ordering::Release);
                let ptr = willow_string_literal_slot(&SLOT, b"waiting".as_ptr(), 7);
                assert_eq!(unsafe { willow_string_as_str(ptr) }, "waiting");
                drop(mutator);
            });
            while !ready.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            // The only safepoint available to the registered worker is the
            // slot wait loop. This collection must finish before we release it.
            willow_gc_collect();
            SLOT.store(std::ptr::null_mut(), Ordering::Release);
        });
        assert_eq!(INITIALIZED_LITERAL_SLOTS.lock().unwrap().len(), 1);
    }

    #[test]
    fn literal_slots_concurrent_first_use_and_collection() {
        use crate::gc::*;
        let _guard = runtime_test_guard();
        static SLOTS: [AtomicPtr<u8>; 64] = [const { AtomicPtr::new(std::ptr::null_mut()) }; 64];
        for workers in [2, 8] {
            willow_gc_init();
            let before = telemetry_heap_snapshot().0.allocation_count;
            let barrier = std::sync::Barrier::new(workers);
            std::thread::scope(|scope| {
                for _ in 0..workers {
                    let barrier = &barrier;
                    scope.spawn(move || {
                        // Barrier before registration: blocking a registered
                        // mutator outside GC cooperation would deadlock STW.
                        barrier.wait();
                        let mutator = crate::gc::MutatorRegistration::new();
                        for slot in &SLOTS {
                            let ptr = willow_string_literal_slot(slot, b"racing".as_ptr(), 6);
                            willow_gc_collect();
                            assert_eq!(unsafe { willow_string_as_str(ptr) }, "racing");
                            assert_eq!(
                                willow_string_literal_slot(slot, b"racing".as_ptr(), 6),
                                ptr
                            );
                        }
                        drop(mutator);
                    });
                }
            });
            assert_eq!(telemetry_heap_snapshot().0.allocation_count - before, 64);
            assert_eq!(INITIALIZED_LITERAL_SLOTS.lock().unwrap().len(), 64);
        }
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
}
