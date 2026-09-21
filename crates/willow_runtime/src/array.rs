//! GC-managed growable arrays.
//!
//! An array value is a small fixed **handle** that points at a separately
//! allocated **buffer**, so the array can grow (`push`) without changing the
//! handle pointer that user code holds:
//!
//! ```text
//!   handle payload:  [ len(i64), cap(i64), is_ref(i64), buffer ]   (gc_ref_mask = 0b1000)
//!   buffer payload:  [ len(atomic i64), elem0, elem1, ... elem_{cap-1} ]
//! ```
//!
//! Each element occupies one 64-bit word. Scalars are stored directly (`bool`
//! zero-extended, `f64` bit-cast); reference elements store the GC pointer.
//! The handle traces `buffer` through `gc_ref_mask`; a reference buffer uses a
//! dedicated `type_id` + trace function that scans only its logical length.
//! Capacity is kept in the handle; new spare slots are zeroed and pop clears
//! removed slots. Generated calls root any independently borrowed reference slots.
//!
//! Index access is bounds-checked against `len`; out-of-range aborts.
//!
//! NOTE: `willow_array_element_addr` returns a pointer **into the current
//! buffer**. A `push` that grows the array reallocates the buffer, so any
//! element address taken before such a `push` is invalidated.

use std::sync::atomic::{AtomicI64, Ordering};

use crate::gc::{
    GcObjectKind, GcStoreDestination, willow_alloc_with_layout, willow_gc_write_barrier,
    willow_pop_roots, willow_push_root,
};

use willow_abi::runtime_type_ids::ARRAY_REF_TYPE_ID;

use willow_abi::array_layout::{
    BUFFER_HEADER_WORDS, H_BUF, H_CAP, H_IS_REF, H_LEN, HANDLE_MASK, HANDLE_WORDS, WORD_BYTES,
};
const WORD: i64 = WORD_BYTES as i64;

/// Read the published live prefix; the buffer remains independently traceable
/// when a captured element reference keeps it alive after handle growth.
unsafe fn buffer_len(buffer: *mut u8) -> i64 {
    unsafe { AtomicI64::from_ptr(buffer.cast()).load(Ordering::Acquire) }
}

/// Publish length after initializing newly live slots or clearing popped slots.
/// The concurrent marker may observe an older prefix; insertion barriers retain
/// every newly stored reference even when its slot is outside that snapshot.
unsafe fn set_buffer_len(buffer: *mut u8, len: i64) {
    unsafe { AtomicI64::from_ptr(buffer.cast()).store(len, Ordering::Release) };
}

/// Trace only the live element slots (word 0 is the published length).
///
/// # Safety
/// `payload` must point at a buffer allocated by [`alloc_buffer`].
unsafe fn trace_array_ref(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    let len = unsafe { buffer_len(payload) };
    for index in 0..len {
        slots.push(unsafe { buf_slot(payload, index).cast() });
    }
}

/// Both length and reference slots use atomic publication for concurrent GC.
unsafe fn snapshot_array_ref(payload: *mut u8, children: &mut Vec<*mut u8>) {
    let len = unsafe { buffer_len(payload) };
    for index in 0..len {
        children.push(unsafe { crate::gc::load_gc_reference(buf_slot(payload, index).cast()) });
    }
}

unsafe fn snapshot_array_ref_slice(
    payload: *mut u8,
    cursor: usize,
    limit: usize,
    children: &mut Vec<*mut u8>,
) -> crate::gc::TraceSliceProgress {
    let len = unsafe { buffer_len(payload) }.max(0) as usize;
    let end = cursor.saturating_add(limit).min(len);
    for index in cursor..end {
        children
            .push(unsafe { crate::gc::load_gc_reference(buf_slot(payload, index as i64).cast()) });
    }
    if end < len {
        crate::gc::TraceSliceProgress::Continue(end)
    } else {
        crate::gc::TraceSliceProgress::Done
    }
}

/// Register the ref-buffer trace. Called on every reference-buffer allocation
/// (idempotent): `willow_gc_init` clears the type registry, so a process-global
/// `Once` would fail to re-register after the first reset (e.g. in multi-init
/// test runs). Real programs init once, so the repeated insert is harmless.
static ARRAY_REGISTRATION: crate::gc::NativeGcRegistration = crate::gc::NativeGcRegistration::new();
const ARRAY_GC_TYPES: &[crate::gc::NativeGcType] =
    &[
        crate::gc::NativeGcType::new(ARRAY_REF_TYPE_ID, Some(trace_array_ref), None)
            .with_concurrent_trace(snapshot_array_ref)
            .with_concurrent_slice(snapshot_array_ref_slice),
    ];

fn ensure_trace_registered() {
    ARRAY_REGISTRATION.ensure(ARRAY_GC_TYPES);
}

/// Allocate an empty, zero-initialized buffer of `cap` slots (`[len=0, e0..]`).
fn alloc_buffer(cap: i64, is_ref: bool) -> *mut u8 {
    // length word + one word per element, with overflow checked end-to-end.
    let payload = match cap
        .checked_add(BUFFER_HEADER_WORDS as i64)
        .and_then(|words| words.checked_mul(WORD))
    {
        Some(p) => p,
        None => {
            raise_with(&format!("array capacity too large: {cap}"));
            return std::ptr::null_mut();
        }
    };
    let buf = if is_ref {
        ensure_trace_registered();
        willow_alloc_with_layout(GcObjectKind::ArrayBuffer, ARRAY_REF_TYPE_ID, payload, 0)
    } else {
        willow_alloc_with_layout(GcObjectKind::ArrayBuffer, 0, payload, 0)
    };
    if buf.is_null() {
        raise_with("array buffer allocation failed");
        return buf;
    }
    unsafe { set_buffer_len(buf, 0) };
    buf
}

/// Address of element slot `index` in a buffer (unchecked).
///
/// # Safety
/// `buffer` must be a buffer with at least `index + 1` slots.
unsafe fn buf_slot(buffer: *mut u8, index: i64) -> *mut i64 {
    unsafe { (buffer as *mut i64).add(BUFFER_HEADER_WORDS + index as usize) }
}

unsafe fn store_buffer_slot(buffer: *mut u8, index: i64, value: i64, is_ref: bool) {
    if is_ref {
        willow_gc_write_barrier(
            buffer,
            unsafe { crate::gc::load_gc_reference(buf_slot(buffer, index).cast()) },
            value as *mut u8,
            GcStoreDestination::ArrayElement as i64,
        );
    }
    if is_ref {
        unsafe { crate::gc::store_gc_reference(buf_slot(buffer, index).cast(), value as *mut u8) };
    } else {
        unsafe { *buf_slot(buffer, index) = value };
    }
}

unsafe fn handle_word(arr: *mut u8, w: usize) -> i64 {
    unsafe { *((arr as *const i64).add(w)) }
}
unsafe fn set_handle_word(arr: *mut u8, w: usize, v: i64) {
    if w == H_BUF {
        unsafe { crate::gc::store_gc_reference((arr as *mut i64).add(w).cast(), v as *mut u8) };
    } else {
        unsafe { *((arr as *mut i64).add(w)) = v };
    }
}
unsafe fn handle_buffer(arr: *mut u8) -> *mut u8 {
    unsafe { handle_word(arr, H_BUF) as *mut u8 }
}

/// Allocate an array of `len` elements (all zero). `elem_is_ref` marks
/// GC-managed element types. Returns the handle, or aborts on a negative length.
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_new(len: i64, elem_is_ref: i64) -> *mut u8 {
    if len < 0 {
        raise_with(&format!(
            "cannot create an array with negative length {len}"
        ));
        return std::ptr::null_mut();
    }
    let is_ref = elem_is_ref != 0;
    // Allocate the handle first (zero-filled, buffer slot null), root it, then
    // allocate the buffer — so a collection during the buffer allocation cannot
    // free the handle, and the still-null buffer slot traces safely.
    let mut handle = willow_alloc_with_layout(
        GcObjectKind::ArrayHandle,
        0,
        HANDLE_WORDS * WORD,
        HANDLE_MASK,
    );
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
        set_buffer_len(buffer, len);
        set_handle_word(handle, H_LEN, len);
        set_handle_word(handle, H_CAP, len);
        set_handle_word(handle, H_IS_REF, elem_is_ref);
        willow_gc_write_barrier(
            handle,
            std::ptr::null_mut(),
            buffer,
            GcStoreDestination::ContainerInternal as i64,
        );
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
pub extern "C" fn willow_array_copy(mut arr: *mut u8) -> *mut u8 {
    if arr.is_null() {
        raise_with("cannot freeze a null array");
        return std::ptr::null_mut();
    }
    let len = willow_array_len(arr);
    let is_ref = unsafe { handle_word(arr, H_IS_REF) };
    willow_push_root(&mut arr);
    let mut copy = willow_array_new(len, is_ref);
    if copy.is_null() {
        willow_pop_roots(1);
        return copy;
    }
    willow_push_root(&mut copy as *mut *mut u8);
    let mut i = 0;
    while i < len {
        willow_array_set(copy, i, willow_array_get(arr, i));
        i += 1;
    }
    willow_pop_roots(2);
    copy
}

/// Number of elements in `arr`.
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_len(arr: *mut u8) -> i64 {
    if arr.is_null() {
        raise_with("cannot take the length of a null array");
        return 0;
    }
    unsafe { handle_word(arr, H_LEN) }
}

/// Read the raw 64-bit word at `index`. Callers interpret the bits according to
/// the element type (`i64` directly, `bool`/`f64` via the generated cast).
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_get(arr: *mut u8, index: i64) -> i64 {
    if !check_bounds(arr, index) {
        return 0;
    }
    unsafe { *buf_slot(handle_buffer(arr), index) }
}

/// Write the raw 64-bit `value` word at `index`.
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_set(arr: *mut u8, index: i64, value: i64) {
    if !check_bounds(arr, index) {
        return;
    }
    let is_ref = unsafe { handle_word(arr, H_IS_REF) } != 0;
    let buffer = unsafe { handle_buffer(arr) };
    unsafe { store_buffer_slot(buffer, index, value, is_ref) };
}

/// Return the address of the raw 64-bit element slot at `index`.
///
/// Used by compiler-generated `&xs[i]` / `&mut xs[i]` reference calls. NOTE: the
/// returned address points into the current buffer; a `push` that grows the
/// array reallocates the buffer and invalidates any address taken earlier.
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_element_addr(arr: *mut u8, index: i64) -> *mut u8 {
    if !check_bounds(arr, index) {
        return std::ptr::null_mut();
    }
    unsafe { buf_slot(handle_buffer(arr), index) as *mut u8 }
}

/// Capture the allocation that currently owns an indexed reference. Unlike
/// the array handle, this allocation remains the same owner after a resize.
/// Generated code roots the returned base and keeps the original index, then
/// recomputes the interior address after any moving collection or suspension.
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_reference_owner(arr: *mut u8, index: i64) -> *mut u8 {
    if !check_bounds(arr, index) {
        return std::ptr::null_mut();
    }
    unsafe { handle_buffer(arr) }
}

/// Append `value`, growing the buffer (doubling, min 4) when full.
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_push(mut arr: *mut u8, mut value: i64) {
    if arr.is_null() {
        raise_with("cannot push to a null array");
        return;
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
        // make the collector treat it as an object pointer and crash. Use the
        // updated root slots after allocation in case the collector moves them.
        willow_push_root(&mut arr);
        let mut val = value as *mut u8;
        let root_val = is_ref && !val.is_null();
        if root_val {
            willow_push_root(&mut val as *mut *mut u8);
        }
        let new_buf = alloc_buffer(new_cap, is_ref);
        if new_buf.is_null() {
            willow_pop_roots(1 + i32::from(root_val));
            return;
        }
        if root_val {
            value = val as i64;
        }
        unsafe {
            let old_buf = handle_buffer(arr);
            if is_ref {
                for i in 0..len {
                    store_buffer_slot(new_buf, i, *buf_slot(old_buf, i), true);
                }
            } else if len != 0 {
                // Separate live allocations; copy only initialized scalar slots.
                // Empty arrays may have a null old buffer.
                std::ptr::copy_nonoverlapping(
                    buf_slot(old_buf, 0),
                    buf_slot(new_buf, 0),
                    len as usize,
                );
            }
            set_buffer_len(new_buf, len);
            willow_gc_write_barrier(
                arr,
                old_buf,
                new_buf,
                GcStoreDestination::ContainerInternal as i64,
            );
            set_handle_word(arr, H_BUF, new_buf as i64);
            set_handle_word(arr, H_CAP, new_cap);
        }
        if root_val {
            willow_pop_roots(1);
        }
        willow_pop_roots(1);
    }
    unsafe {
        let buffer = handle_buffer(arr);
        store_buffer_slot(buffer, len, value, is_ref);
        set_buffer_len(buffer, len + 1);
        set_handle_word(arr, H_LEN, len + 1);
    }
}

/// Remove and return the last element. Aborts on an empty array. The freed slot
/// is nulled so a popped reference can be reclaimed.
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_pop(arr: *mut u8) -> i64 {
    if arr.is_null() {
        raise_with("cannot pop from a null array");
        return 0;
    }
    let len = unsafe { handle_word(arr, H_LEN) };
    if len == 0 {
        raise_with("cannot pop from an empty array");
        return 0;
    }
    let last = len - 1;
    unsafe {
        let buffer = handle_buffer(arr);
        let slot = buf_slot(buffer, last);
        let value = *slot;
        let is_ref = handle_word(arr, H_IS_REF) != 0;
        store_buffer_slot(buffer, last, 0, is_ref); // allow the GC to reclaim it
        set_buffer_len(buffer, last);
        set_handle_word(arr, H_LEN, last);
        value
    }
}

fn check_bounds(arr: *mut u8, index: i64) -> bool {
    if arr.is_null() {
        raise_with("cannot index a null array");
        return false;
    }
    let len = unsafe { handle_word(arr, H_LEN) };
    if index < 0 || index >= len {
        raise_with(&format!(
            "array index out of bounds: the length is {len} but the index is {index}"
        ));
        return false;
    }
    true
}

/// Raise a recoverable language fault and let the generated caller branch
/// before observing the neutral return value.
fn raise_with(message: &str) {
    crate::panic_context::raise_language_message(message);
}

/// Element-kind tags for `willow_array_to_string` (willow-vwn6). Must match
/// the compiler's `collection_elem_kind`.
const ELEM_KIND_I64: i64 = 0;
const ELEM_KIND_F64: i64 = 1;
const ELEM_KIND_BOOL: i64 = 2;
const ELEM_KIND_STRING: i64 = 3;

pub(crate) fn element_word_to_string(word: i64, kind: i64) -> String {
    match kind {
        ELEM_KIND_F64 => crate::math::format_f64_shortest(f64::from_bits(word as u64)),
        ELEM_KIND_BOOL => if word != 0 { "true" } else { "false" }.to_string(),
        ELEM_KIND_STRING => {
            let s = unsafe { crate::string::willow_string_as_str(word as *const u8) };
            format!("{s:?}")
        }
        _ => word.to_string(),
    }
    .clone()
}

/// Debug display of a whole array: `[1, 2, 3]` (strings quoted, f64 shortest
/// round-trip). `elem_kind`: 0=i64, 1=f64, 2=bool, 3=String (willow-vwn6).
/// Returns a newly allocated WillowString.
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_to_string(arr: *mut u8, elem_kind: i64) -> *mut u8 {
    if arr.is_null() {
        raise_with("cannot convert a null array to String");
        return std::ptr::null_mut();
    }
    let len = willow_array_len(arr);
    let mut out = String::from("[");
    for index in 0..len {
        if index > 0 {
            out.push_str(", ");
        }
        let word = willow_array_get(arr, index);
        out.push_str(&element_word_to_string(word, elem_kind));
    }
    out.push(']');
    crate::string::willow_string_from_str(&out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{
        runtime_test_guard, willow_gc_collect, willow_gc_init, willow_pop_roots, willow_push_root,
    };
    use crate::string::{willow_string_as_str, willow_string_from_str};

    #[test]
    fn array_shared_layout_matches_allocated_payload() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let array = willow_array_new(3, 0);
        willow_array_push(array, 42);
        unsafe {
            assert_eq!(WORD_BYTES as usize, std::mem::size_of::<i64>());
            assert_eq!(handle_word(array, H_LEN), 4);
            assert!(handle_word(array, H_CAP) > 4);
            assert_eq!(handle_word(array, H_IS_REF), 0);
            let buffer = handle_buffer(array);
            assert_eq!(buffer_len(buffer), 4);
            assert_eq!(buf_slot(buffer, 0).byte_offset_from(buffer), 8);
            assert_eq!(*buf_slot(buffer, 3), 42);
            assert_eq!(HANDLE_MASK, 1 << H_BUF);
        }
    }

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

    // Deterministic slot counts isolate tracing cost from allocation and GC timing.
    #[test]
    fn array_trace_work_tracks_length_after_pop_and_regrowth() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        for capacity in [16, 256, 4096] {
            let mut arr = willow_array_new(capacity, 1);
            willow_push_root(&mut arr);
            let value = willow_string_from_str("retained");
            for index in 0..capacity {
                willow_array_set(arr, index, value as i64);
            }
            for _ in 3..capacity {
                assert_eq!(willow_array_pop(arr), value as i64);
            }
            let buffer = unsafe { handle_buffer(arr) };
            for length in [3, 0, 1, 8, 16] {
                while willow_array_len(arr) > length {
                    willow_array_pop(arr);
                }
                while willow_array_len(arr) < length {
                    willow_array_push(arr, value as i64);
                }
                let mut slots = Vec::new();
                let mut children = Vec::new();
                unsafe {
                    trace_array_ref(buffer, &mut slots);
                    snapshot_array_ref(buffer, &mut children);
                }
                assert_eq!(slots.len(), length as usize, "capacity={capacity}");
                assert_eq!(children, vec![value; length as usize]);
                println!(
                    "capacity={capacity} length={length} trace_slots={} snapshot_children={}",
                    slots.len(),
                    children.len()
                );
                for (index, slot) in slots.into_iter().enumerate() {
                    assert_eq!(slot, unsafe { buf_slot(buffer, index as i64).cast() });
                }
            }
            willow_gc_collect();
            assert_eq!(
                unsafe { willow_string_as_str(willow_array_get(arr, 0) as *mut u8) },
                "retained"
            );
            willow_pop_roots(1);
        }
    }

    #[test]
    fn array_trace_preserves_captured_reference_buffer_after_growth() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let mut arr = willow_array_new(1, 1);
        willow_push_root(&mut arr);
        let value = willow_string_from_str("captured");
        willow_array_set(arr, 0, value as i64);
        let mut captured = willow_array_reference_owner(arr, 0);
        willow_push_root(&mut captured);
        willow_array_push(arr, value as i64);
        assert_ne!(captured, unsafe { handle_buffer(arr) });
        willow_array_pop(arr);
        willow_array_pop(arr);
        willow_gc_collect();
        let mut children = Vec::new();
        unsafe { snapshot_array_ref(captured, &mut children) };
        assert_eq!(children.len(), 1);
        assert_eq!(unsafe { willow_string_as_str(children[0]) }, "captured");
        children.clear();
        unsafe { snapshot_array_ref(handle_buffer(arr), &mut children) };
        assert!(children.is_empty());
        willow_pop_roots(2);
    }

    #[test]
    fn array_snapshot_length_publication_during_push_pop() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let mut arr = willow_array_new(256, 1);
        willow_push_root(&mut arr);
        let mut value = willow_string_from_str("published");
        willow_push_root(&mut value);
        for _ in 0..256 {
            willow_array_pop(arr);
        }
        let buffer = unsafe { handle_buffer(arr) } as usize;
        let expected = value as usize;
        let start = std::sync::Barrier::new(2);
        // No allocations/collections while the reader borrows this buffer.
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                start.wait();
                let mut children = Vec::new();
                for _ in 0..4096 {
                    children.clear();
                    unsafe { snapshot_array_ref(buffer as *mut u8, &mut children) };
                    assert!(children.len() <= 256);
                    assert!(
                        children
                            .iter()
                            .all(|child| child.is_null() || *child as usize == expected)
                    );
                }
            });
            start.wait();
            for _ in 0..16 {
                for _ in 0..256 {
                    willow_array_push(arr, value as i64);
                }
                for _ in 0..256 {
                    willow_array_pop(arr);
                }
            }
            reader.join().unwrap();
        });
        assert_eq!(unsafe { buffer_len(buffer as *mut u8) }, 0);
        willow_pop_roots(2);
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
        // Guarded because the stress override is global to the test binary.
        let collections = crate::gc::willow_gc_major_collections();
        crate::gc::set_gc_stress_for_test(Some("alloc"));
        for i in 0..12 {
            willow_array_push(arr, i * 7); // crosses several growth points
        }
        crate::gc::set_gc_stress_for_test(None);
        assert!(crate::gc::willow_gc_major_collections() > collections);
        assert_eq!(willow_array_len(arr), 12);
        assert_eq!(willow_array_get(arr, 0), 0);
        assert_eq!(willow_array_get(arr, 11), 77);
        willow_pop_roots(1);
    }

    #[test]
    fn array_copy_and_reference_growth_keep_temporary_roots_under_gc_stress() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let mut arr = willow_array_new(0, 1);
        willow_push_root(&mut arr);
        let depth = crate::gc::willow_root_depth();
        crate::gc::set_gc_stress_for_test(Some("alloc"));
        for i in 0..12 {
            let value = willow_string_from_str(&format!("temporary-{i}"));
            willow_array_push(arr, value as i64);
            assert_eq!(crate::gc::willow_root_depth(), depth);
        }
        // Leave source liveness to array_copy's own root during its allocations.
        willow_pop_roots(1);
        let mut copy = willow_array_copy(arr);
        crate::gc::set_gc_stress_for_test(None);
        assert_eq!(crate::gc::willow_root_depth(), depth - 1);
        willow_push_root(&mut copy);
        willow_gc_collect();
        assert_eq!(willow_array_len(copy), 12);
        for i in 0..12 {
            assert_eq!(
                unsafe { willow_string_as_str(willow_array_get(copy, i) as *mut u8) },
                format!("temporary-{i}")
            );
        }
        willow_pop_roots(1);
    }

    #[test]
    fn array_null_push_preserves_error_and_root_depth() {
        use crate::panic_context::*;
        let _guard = runtime_test_guard();
        willow_gc_init();
        let previous = replace_current_context(Some(std::sync::Arc::new(PanicContext::new(903))));
        let depth = crate::gc::willow_root_depth();
        willow_array_push(std::ptr::null_mut(), 42);
        assert_eq!(crate::gc::willow_root_depth(), depth);
        assert_eq!(willow_panic_depth(), 1);
        willow_panic_enter_defer();
        let info = willow_panic_recover();
        willow_panic_leave_defer();
        assert_eq!(
            unsafe { panic_info_message(info) },
            "cannot push to a null array"
        );
        willow_panic_release_recovered(info);
        replace_current_context(previous);
    }

    // Synthetic handle lengths force checked overflow without huge allocations.
    // The real buffer retains its valid logical length for GC tracing throughout.
    #[test]
    fn array_allocation_overflow_preserves_state_and_roots() {
        use crate::panic_context::*;
        let _guard = runtime_test_guard();
        willow_gc_init();
        let previous = replace_current_context(Some(std::sync::Arc::new(PanicContext::new(902))));
        for (is_ref, nonnull_value) in [(false, false), (true, false), (true, true)] {
            let mut arr = willow_array_new(1, i64::from(is_ref));
            willow_push_root(&mut arr);
            let value = if nonnull_value {
                willow_string_from_str("rooted") as i64
            } else {
                0
            };
            let buffer = unsafe { handle_buffer(arr) };
            unsafe {
                set_handle_word(arr, H_LEN, i64::MAX);
                set_handle_word(arr, H_CAP, i64::MAX);
            }
            let depth = crate::gc::willow_root_depth();
            willow_array_push(arr, value);
            assert_eq!(crate::gc::willow_root_depth(), depth);
            assert_eq!(willow_panic_depth(), 1);
            assert_eq!(unsafe { handle_buffer(arr) }, buffer);
            assert_eq!(unsafe { handle_word(arr, H_LEN) }, i64::MAX);
            assert_eq!(unsafe { handle_word(arr, H_CAP) }, i64::MAX);
            willow_panic_enter_defer();
            let info = willow_panic_recover();
            willow_panic_leave_defer();
            assert!(unsafe { panic_info_message(info) }.starts_with("array capacity too large:"));
            willow_panic_release_recovered(info);

            assert!(willow_array_copy(arr).is_null());
            assert_eq!(crate::gc::willow_root_depth(), depth);
            assert_eq!(willow_panic_depth(), 1);
            willow_panic_enter_defer();
            let info = willow_panic_recover();
            willow_panic_leave_defer();
            assert!(unsafe { panic_info_message(info) }.starts_with("array capacity too large:"));
            willow_panic_release_recovered(info);
            unsafe {
                set_handle_word(arr, H_LEN, 1);
                set_handle_word(arr, H_CAP, 1);
            }
            assert_eq!(willow_array_get(arr, 0), 0);
            willow_pop_roots(1);
        }
        replace_current_context(previous);
    }
}
