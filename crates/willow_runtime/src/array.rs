//! GC-managed arrays.
//!
//! Memory layout (payload, past the `GcHeader`):
//!
//! ```text
//!   word 0      : length (i64)
//!   word 1..=N  : element slots, one 64-bit word each
//! ```
//!
//! Every element occupies one 64-bit word. Scalar element types are stored
//! directly (`bool` zero-extended, `f64` bit-cast); reference element types
//! (`String`, class instances, nested arrays) store the GC pointer.
//!
//! Two flavors of allocation keep tracing cheap:
//!
//! * Non-reference arrays (`i64`/`bool`/`f64`) are allocated with `type_id = 0`
//!   and `gc_ref_mask = 0`, so the collector never scans their payload.
//! * Reference arrays use a dedicated `type_id` with a registered trace
//!   function that walks `length` element slots. This handles arrays of any
//!   length, unlike the 64-slot `gc_ref_mask`.
//!
//! Index access is bounds-checked; an out-of-range index aborts the program
//! through the standard panic path (no exceptions in the MVP).

use crate::gc::{willow_alloc_object, willow_alloc_typed, willow_register_type};

/// `type_id` for reference-element arrays. Chosen well above the small,
/// sequentially-assigned class type ids so it cannot collide with one.
const ARRAY_REF_TYPE_ID: u32 = 0xA22A_0001;

const WORD: i64 = std::mem::size_of::<i64>() as i64;

/// Trace function for reference-element arrays: scan `length` element slots
/// (skipping the length word) and report the non-null GC pointers.
///
/// # Safety
/// `payload` must point at an array payload allocated by [`willow_array_new`].
unsafe fn trace_array_ref(payload: *mut u8, children: &mut Vec<*mut u8>) {
    let len = unsafe { *(payload as *const i64) };
    if len <= 0 {
        return;
    }
    let words = payload as *const *mut u8;
    for i in 0..len as usize {
        let elem = unsafe { *words.add(1 + i) };
        if !elem.is_null() {
            children.push(elem);
        }
    }
}

/// Register the ref-array trace. Called on every reference-array allocation
/// (idempotent): `willow_gc_init` clears the type registry, so a process-global
/// `Once` would fail to re-register after the first reset (e.g. in multi-init
/// test runs). Real programs init once, so the repeated insert is harmless.
fn ensure_trace_registered() {
    willow_register_type(ARRAY_REF_TYPE_ID, trace_array_ref);
}

/// Allocate a zero-initialized array of `len` elements. `elem_is_ref` is
/// nonzero when the element type is GC-managed (so the array participates in
/// tracing). Returns a pointer to the payload, or aborts on a negative length.
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_new(len: i64, elem_is_ref: i64) -> *mut u8 {
    if len < 0 {
        abort_with(&format!(
            "cannot create an array with negative length {len}"
        ));
    }
    // length word + one word per element.
    let payload = (len + 1).saturating_mul(WORD);
    let arr = if elem_is_ref != 0 {
        ensure_trace_registered();
        willow_alloc_object(ARRAY_REF_TYPE_ID as i64, payload)
    } else {
        willow_alloc_typed(payload, 0)
    };
    if arr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: `arr` points at a freshly allocated payload of at least one word.
    unsafe { *(arr as *mut i64) = len };
    arr
}

/// Number of elements in `arr`.
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_len(arr: *mut u8) -> i64 {
    if arr.is_null() {
        abort_with("cannot take the length of a null array");
    }
    // SAFETY: non-null array pointers carry the length in word 0.
    unsafe { *(arr as *const i64) }
}

/// Read the raw 64-bit word at `index`. Callers interpret the bits according to
/// the element type (`i64` directly, `bool`/`f64` via the generated cast).
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_get(arr: *mut u8, index: i64) -> i64 {
    check_bounds(arr, index);
    // SAFETY: bounds checked; element slots start at word 1.
    unsafe { *((arr as *const i64).add(1 + index as usize)) }
}

/// Write the raw 64-bit `value` word at `index`.
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_set(arr: *mut u8, index: i64, value: i64) {
    check_bounds(arr, index);
    // SAFETY: bounds checked; element slots start at word 1.
    unsafe { *((arr as *mut i64).add(1 + index as usize)) = value };
}

/// Return the address of the raw 64-bit element slot at `index`.
///
/// This is used by compiler-generated `&xs[i]` / `&mut xs[i]` reference calls.
/// It performs the same null and bounds checks as get/set before exposing the
/// slot address to generated code.
#[unsafe(no_mangle)]
pub extern "C" fn willow_array_element_addr(arr: *mut u8, index: i64) -> *mut u8 {
    check_bounds(arr, index);
    // SAFETY: bounds checked; element slots start at word 1.
    unsafe { (arr as *mut i64).add(1 + index as usize) as *mut u8 }
}

fn check_bounds(arr: *mut u8, index: i64) {
    if arr.is_null() {
        abort_with("cannot index a null array");
    }
    // SAFETY: non-null array pointers carry the length in word 0.
    let len = unsafe { *(arr as *const i64) };
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
    use crate::gc::{willow_gc_collect, willow_gc_init, willow_pop_roots, willow_push_root};
    use crate::string::{willow_string_as_str, willow_string_from_str};

    #[test]
    fn array_unit_01_new_sets_length() {
        unsafe { willow_gc_init() };
        let arr = willow_array_new(3, 0);
        assert!(!arr.is_null());
        assert_eq!(willow_array_len(arr), 3);
    }

    #[test]
    fn array_unit_02_zero_length_array() {
        unsafe { willow_gc_init() };
        let arr = willow_array_new(0, 0);
        assert_eq!(willow_array_len(arr), 0);
    }

    #[test]
    fn array_unit_03_scalar_get_set_roundtrip() {
        unsafe { willow_gc_init() };
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
        unsafe { willow_gc_init() };
        let arr = willow_array_new(2, 1);
        let s = willow_string_from_str("hello");
        willow_array_set(arr, 0, s as i64);
        let got = willow_array_get(arr, 0) as *mut u8;
        assert_eq!(unsafe { willow_string_as_str(got) }, "hello");
    }

    #[test]
    fn array_unit_05_reference_elements_survive_collection_when_rooted() {
        unsafe { willow_gc_init() };
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
        unsafe { willow_gc_init() };
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
}
