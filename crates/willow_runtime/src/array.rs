//! GC-managed growable arrays.
//!
//! An array value is a small fixed **handle** that points at a separately
//! allocated **buffer**, so the array can grow (`push`) without changing the
//! handle pointer that user code holds:
//!
//! ```text
//!   handle payload:  [ len(i64), cap(i64), is_ref(i64), buffer ]   (gc_ref_mask = 0b1000)
//!   buffer payload:  [ cap(i64), elem0, elem1, ... elem_{cap-1} ]
//! ```
//!
//! Each element occupies one 64-bit word. Scalars are stored directly (`bool`
//! zero-extended, `f64` bit-cast); reference elements store the GC pointer.
//! The handle traces `buffer` through `gc_ref_mask`; a reference buffer uses a
//! dedicated `type_id` + trace function that scans its `cap` slots (unused
//! slots are null and skipped). Logical length (`len`) ≤ capacity (`cap`).
//!
//! Index access is bounds-checked against `len`; out-of-range aborts.
//!
//! NOTE: `willow_array_element_addr` returns a pointer **into the current
//! buffer**. A `push` that grows the array reallocates the buffer, so any
//! element address taken before such a `push` is invalidated.

use crate::gc::{
    willow_alloc_object, willow_alloc_typed, willow_pop_roots, willow_push_root,
    willow_register_type,
};

/// `type_id` for reference-element buffers. Chosen well above the small,
/// sequentially-assigned class type ids so it cannot collide with one.
const ARRAY_REF_TYPE_ID: u32 = 0xA22A_0001;

const WORD: i64 = std::mem::size_of::<i64>() as i64;

// Handle word offsets.
const H_LEN: usize = 0;
const H_CAP: usize = 1;
const H_IS_REF: usize = 2;
const H_BUF: usize = 3;
const HANDLE_WORDS: i64 = 4;
const HANDLE_MASK: u64 = 0b1000; // only word 3 (the buffer pointer) is a GC ref

/// Trace a reference buffer: scan its `cap` element slots (word 0 is the
/// capacity); unused slots are null and skipped.
///
/// # Safety
/// `payload` must point at a buffer allocated by [`alloc_buffer`].
unsafe fn trace_array_ref(payload: *mut u8, children: &mut Vec<*mut u8>) {
    let cap = unsafe { *(payload as *const i64) };
    if cap <= 0 {
        return;
    }
    let words = payload as *const *mut u8;
    for i in 0..cap as usize {
        let elem = unsafe { *words.add(1 + i) };
        if !elem.is_null() {
            children.push(elem);
        }
    }
}

/// Register the ref-buffer trace. Called on every reference-buffer allocation
/// (idempotent): `willow_gc_init` clears the type registry, so a process-global
/// `Once` would fail to re-register after the first reset (e.g. in multi-init
/// test runs). Real programs init once, so the repeated insert is harmless.
fn ensure_trace_registered() {
    willow_register_type(ARRAY_REF_TYPE_ID, trace_array_ref);
}

/// Allocate a zero-initialized buffer of `cap` element slots (`[cap, e0..]`).
fn alloc_buffer(cap: i64, is_ref: bool) -> *mut u8 {
    // length word + one word per element, with overflow checked end-to-end.
    let payload = match cap.checked_add(1).and_then(|words| words.checked_mul(WORD)) {
        Some(p) => p,
        None => abort_with(&format!("array capacity too large: {cap}")),
    };
    let buf = if is_ref {
        ensure_trace_registered();
        willow_alloc_object(ARRAY_REF_TYPE_ID as i64, payload)
    } else {
        willow_alloc_typed(payload, 0)
    };
    if !buf.is_null() {
        unsafe { *(buf as *mut i64) = cap };
    }
    buf
}

/// Address of element slot `index` in a buffer (unchecked).
///
/// # Safety
/// `buffer` must be a buffer with at least `index + 1` slots.
unsafe fn buf_slot(buffer: *mut u8, index: i64) -> *mut i64 {
    unsafe { (buffer as *mut i64).add(1 + index as usize) }
}

unsafe fn handle_word(arr: *mut u8, w: usize) -> i64 {
    unsafe { *((arr as *const i64).add(w)) }
}
unsafe fn set_handle_word(arr: *mut u8, w: usize, v: i64) {
    unsafe { *((arr as *mut i64).add(w)) = v };
}
unsafe fn handle_buffer(arr: *mut u8) -> *mut u8 {
    unsafe { handle_word(arr, H_BUF) as *mut u8 }
}

/// Allocate an array of `len` elements (all zero). `elem_is_ref` marks
/// GC-managed element types. Returns the handle, or aborts on a negative length.
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_new(len: i64, elem_is_ref: i64) -> *mut u8 {
    if len < 0 {
        abort_with(&format!(
            "cannot create an array with negative length {len}"
        ));
    }
    let is_ref = elem_is_ref != 0;
    // Allocate the handle first (zero-filled, buffer slot null), root it, then
    // allocate the buffer — so a collection during the buffer allocation cannot
    // free the handle, and the still-null buffer slot traces safely.
    let mut handle = willow_alloc_typed(HANDLE_WORDS * WORD, HANDLE_MASK);
    if handle.is_null() {
        return std::ptr::null_mut();
    }
    willow_push_root(&mut handle as *mut *mut u8);
    let buffer = alloc_buffer(len, is_ref);
    if buffer.is_null() {
        willow_pop_roots(1);
        return std::ptr::null_mut();
    }
    unsafe {
        set_handle_word(handle, H_LEN, len);
        set_handle_word(handle, H_CAP, len);
        set_handle_word(handle, H_IS_REF, elem_is_ref);
        set_handle_word(handle, H_BUF, buffer as i64);
    }
    willow_pop_roots(1);
    handle
}

/// Allocate an independent copy of `arr` (same length, element ref-ness, and
/// element words). Backs `Array<T>::freeze()` -> `FrozenArray<T>` (willow-dgwo.7):
/// the copy has no mutation API and shares no buffer with the original, so it is
/// safe to treat as immutable. Shallow per the element word (ref elements share
/// their — Sync — referents).
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_copy(arr: *mut u8) -> *mut u8 {
    if arr.is_null() {
        abort_with("cannot freeze a null array");
    }
    let len = willow_array_len(arr);
    let is_ref = unsafe { handle_word(arr, H_IS_REF) };
    let mut copy = willow_array_new(len, is_ref);
    willow_push_root(&mut copy as *mut *mut u8);
    let mut i = 0;
    while i < len {
        willow_array_set(copy, i, willow_array_get(arr, i));
        i += 1;
    }
    willow_pop_roots(1);
    copy
}

/// Number of elements in `arr`.
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_len(arr: *mut u8) -> i64 {
    if arr.is_null() {
        abort_with("cannot take the length of a null array");
    }
    unsafe { handle_word(arr, H_LEN) }
}

/// Read the raw 64-bit word at `index`. Callers interpret the bits according to
/// the element type (`i64` directly, `bool`/`f64` via the generated cast).
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_get(arr: *mut u8, index: i64) -> i64 {
    check_bounds(arr, index);
    unsafe { *buf_slot(handle_buffer(arr), index) }
}

/// Write the raw 64-bit `value` word at `index`.
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_set(arr: *mut u8, index: i64, value: i64) {
    check_bounds(arr, index);
    unsafe { *buf_slot(handle_buffer(arr), index) = value };
}

/// Return the address of the raw 64-bit element slot at `index`.
///
/// Used by compiler-generated `&xs[i]` / `&mut xs[i]` reference calls. NOTE: the
/// returned address points into the current buffer; a `push` that grows the
/// array reallocates the buffer and invalidates any address taken earlier.
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_element_addr(arr: *mut u8, index: i64) -> *mut u8 {
    check_bounds(arr, index);
    unsafe { buf_slot(handle_buffer(arr), index) as *mut u8 }
}

/// Append `value`, growing the buffer (doubling, min 4) when full.
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_push(arr: *mut u8, value: i64) {
    if arr.is_null() {
        abort_with("cannot push to a null array");
    }
    let len = unsafe { handle_word(arr, H_LEN) };
    let cap = unsafe { handle_word(arr, H_CAP) };
    let is_ref = unsafe { handle_word(arr, H_IS_REF) } != 0;

    if len == cap {
        let new_cap = if cap == 0 { 4 } else { cap.saturating_mul(2) };
        // Root the handle and the (possibly reference) value across the buffer
        // allocation, which may trigger a collection. The old buffer stays
        // reachable through the rooted handle. Only root the pushed value when
        // it is a GC pointer — rooting a scalar word (e.g. an i64 like 42) would
        // make the collector treat it as an object pointer and crash. The GC is
        // non-moving, so `value` stays valid after the collection.
        let mut handle = arr;
        willow_push_root(&mut handle as *mut *mut u8);
        let mut val = value as *mut u8;
        let root_val = is_ref && !val.is_null();
        if root_val {
            willow_push_root(&mut val as *mut *mut u8);
        }
        let new_buf = alloc_buffer(new_cap, is_ref);
        unsafe {
            let old_buf = handle_buffer(arr);
            for i in 0..len {
                *buf_slot(new_buf, i) = *buf_slot(old_buf, i);
            }
            set_handle_word(arr, H_BUF, new_buf as i64);
            set_handle_word(arr, H_CAP, new_cap);
        }
        if root_val {
            willow_pop_roots(1);
        }
        willow_pop_roots(1);
    }
    unsafe {
        *buf_slot(handle_buffer(arr), len) = value;
        set_handle_word(arr, H_LEN, len + 1);
    }
}

/// Remove and return the last element. Aborts on an empty array. The freed slot
/// is nulled so a popped reference can be reclaimed.
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_pop(arr: *mut u8) -> i64 {
    if arr.is_null() {
        abort_with("cannot pop from a null array");
    }
    let len = unsafe { handle_word(arr, H_LEN) };
    if len == 0 {
        abort_with("cannot pop from an empty array");
    }
    let last = len - 1;
    unsafe {
        let slot = buf_slot(handle_buffer(arr), last);
        let value = *slot;
        *slot = 0; // drop the reference so the GC can reclaim it
        set_handle_word(arr, H_LEN, last);
        value
    }
}

fn check_bounds(arr: *mut u8, index: i64) {
    if arr.is_null() {
        abort_with("cannot index a null array");
    }
    let len = unsafe { handle_word(arr, H_LEN) };
    if index < 0 || index >= len {
        abort_with(&format!(
            "array index out of bounds: the length is {len} but the index is {index}"
        ));
    }
}

/// Report a fatal array error through the standard panic path, which prints the
/// message and aborts. Does not return.
fn abort_with(message: &str) -> ! {
    let ws = crate::string::willow_string_from_str(message);
    // willow_panic is a Rust-defined `extern "C"` function; calling it prints
    // the message and aborts the process.
    crate::panic::willow_panic(ws as *const u8);
    std::process::abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{
        runtime_test_guard, willow_gc_collect, willow_gc_init, willow_pop_roots, willow_push_root,
    };
    use crate::string::{willow_string_as_str, willow_string_from_str};

    #[test]
    fn array_unit_01_new_sets_length() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let arr = willow_array_new(3, 0);
        assert!(!arr.is_null());
        assert_eq!(willow_array_len(arr), 3);
    }

    #[test]
    fn array_unit_02_zero_length_array() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let arr = willow_array_new(0, 0);
        assert_eq!(willow_array_len(arr), 0);
    }

    #[test]
    fn array_unit_03_scalar_get_set_roundtrip() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let arr = willow_array_new(4, 0);
        willow_array_set(arr, 0, 10);
        willow_array_set(arr, 1, -20);
        willow_array_set(arr, 3, 99);
        assert_eq!(willow_array_get(arr, 0), 10);
        assert_eq!(willow_array_get(arr, 1), -20);
        // Slot 2 was never written — zero-initialized.
        assert_eq!(willow_array_get(arr, 2), 0);
        assert_eq!(willow_array_get(arr, 3), 99);
    }

    #[test]
    fn array_unit_04_reference_elements_roundtrip() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        // Root the array as generated code would (Array is GC-managed): the
        // string allocation below can trigger a collection under GC stress.
        let mut arr = willow_array_new(2, 1);
        willow_push_root(&mut arr as *mut *mut u8);
        let s = willow_string_from_str("hello");
        willow_array_set(arr, 0, s as i64);
        let got = willow_array_get(arr, 0) as *mut u8;
        assert_eq!(unsafe { willow_string_as_str(got) }, "hello");
        willow_pop_roots(1);
    }

    #[test]
    fn array_unit_05_reference_elements_survive_collection_when_rooted() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let mut arr = willow_array_new(1, 1);
        // Root the array slot so the collector can reach it.
        willow_push_root(&mut arr as *mut *mut u8 as *mut *mut u8);
        let s = willow_string_from_str("kept");
        willow_array_set(arr, 0, s as i64);
        willow_gc_collect();
        let got = willow_array_get(arr, 0) as *mut u8;
        assert!(
            !got.is_null(),
            "element string must survive GC via array trace"
        );
        assert_eq!(unsafe { willow_string_as_str(got) }, "kept");
        willow_pop_roots(1);
    }

    #[test]
    fn array_unit_06_large_reference_array_traces_all_elements() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        // More than 64 elements: exercises the trace function, not gc_ref_mask.
        let n = 100i64;
        let mut arr = willow_array_new(n, 1);
        willow_push_root(&mut arr as *mut *mut u8 as *mut *mut u8);
        for i in 0..n {
            let s = willow_string_from_str(&format!("e{i}"));
            willow_array_set(arr, i, s as i64);
        }
        willow_gc_collect();
        assert_eq!(
            unsafe { willow_string_as_str(willow_array_get(arr, 99) as *mut u8) },
            "e99"
        );
        willow_pop_roots(1);
    }

    #[test]
    fn array_unit_07_push_grows_and_reads() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let arr = willow_array_new(0, 0);
        assert_eq!(willow_array_len(arr), 0);
        for i in 0..10 {
            willow_array_push(arr, i * 100);
        }
        assert_eq!(willow_array_len(arr), 10);
        assert_eq!(willow_array_get(arr, 0), 0);
        assert_eq!(willow_array_get(arr, 9), 900);
    }

    #[test]
    fn array_unit_08_pop_returns_last_and_shrinks() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let arr = willow_array_new(0, 0);
        willow_array_push(arr, 1);
        willow_array_push(arr, 2);
        willow_array_push(arr, 3);
        assert_eq!(willow_array_pop(arr), 3);
        assert_eq!(willow_array_pop(arr), 2);
        assert_eq!(willow_array_len(arr), 1);
        assert_eq!(willow_array_get(arr, 0), 1);
    }

    #[test]
    fn array_unit_09_pushed_reference_values_survive_gc_across_growth() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let mut arr = willow_array_new(0, 1);
        willow_push_root(&mut arr as *mut *mut u8);
        // Push past several growth points; each push may reallocate the buffer.
        for i in 0..20 {
            let s = willow_string_from_str(&format!("v{i}"));
            willow_array_push(arr, s as i64);
        }
        willow_gc_collect();
        assert_eq!(willow_array_len(arr), 20);
        assert_eq!(
            unsafe { willow_string_as_str(willow_array_get(arr, 0) as *mut u8) },
            "v0"
        );
        assert_eq!(
            unsafe { willow_string_as_str(willow_array_get(arr, 19) as *mut u8) },
            "v19"
        );
        willow_pop_roots(1);
    }

    // Regression: pushing scalars while a collection runs during buffer growth
    // must not root the scalar word as an object pointer (previously SIGSEGV'd
    // under GC stress). Stress mode forces a collection on every allocation.
    #[test]
    fn array_unit_10_scalar_push_grow_under_gc_stress() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let mut arr = willow_array_new(0, 0); // scalar (non-reference) array
        willow_push_root(&mut arr as *mut *mut u8);
        // SAFETY: guarded because process environment is global to the test binary.
        unsafe { std::env::set_var("WILLOW_GC_STRESS", "alloc") };
        for i in 0..12 {
            willow_array_push(arr, i * 7); // crosses several growth points
        }
        unsafe { std::env::remove_var("WILLOW_GC_STRESS") };
        assert_eq!(willow_array_len(arr), 12);
        assert_eq!(willow_array_get(arr, 0), 0);
        assert_eq!(willow_array_get(arr, 11), 77);
        willow_pop_roots(1);
    }
}
