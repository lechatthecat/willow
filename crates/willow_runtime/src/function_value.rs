//! Function values that capture nothing.
//!
//! Every Willow function value is a managed object whose word 0 is its code
//! address (bit 0 tags a body that may raise) and whose later words hold its
//! captures. A named function or a capture-free lambda has no captures, so all
//! its uses can share one permanently rooted, never-moving object, built once
//! per runtime in compiler-owned static storage, as a string literal is.

use std::sync::atomic::AtomicPtr;

use crate::gc::{GcObjectKind, willow_alloc_with_layout};

/// Retrieve the shared function value for `code`, building it on first use.
///
/// `slot` must point to a pointer-aligned, initially zero atomic pointer with
/// process lifetime, and every use of one slot must pass the same `code` word.
/// Heap reset must be quiescent; it clears every initialized slot before reuse.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_function_value_slot(
    slot: *const AtomicPtr<u8>,
    code: *const u8,
) -> *mut u8 {
    // SAFETY: compiler-generated writable static slot obeys the contract above.
    let slot: &'static AtomicPtr<u8> = unsafe { &*slot };
    crate::string::initialize_rooted_slot(slot, || {
        let word = willow_abi::storage_word_bytes(std::mem::size_of::<usize>() as u32);
        // Old-space allocation: the object never moves, so the slot may keep
        // its raw address across collections.
        let object = willow_alloc_with_layout(GcObjectKind::Closure, 0, i64::from(word), 0);
        if !object.is_null() {
            // SAFETY: a fresh payload of one storage word.
            unsafe { (object as *mut *const u8).write(code) };
        }
        object
    })
}
