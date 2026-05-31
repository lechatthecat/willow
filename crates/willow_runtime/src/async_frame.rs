// Async future frame — GC-managed heap objects for suspended async fn coroutines.
//
// An async frame is allocated on the GC heap via `willow_alloc_typed`.
// The compiler accesses frame fields directly via Cranelift load/store
// instructions using the layout below; no Rust accessor functions are needed
// (and would require unsafe raw pointer operations).
//
// Layout (each slot = one pointer-sized word, 8 bytes on 64-bit):
//
//   [WillowAsyncFrameHeader (2 words) | data slot 0 | data slot 1 | … ]
//
//   word 0 : state (i64)      — 0 = initial, N = after await N, i64::MAX = done
//   word 1 : slot_count (i64) — number of data slots following the header
//   word 2 : slot 0           — first data slot (GC pointer or scalar)
//   word 3 : slot 1
//   …
//
// The `gc_ref_mask` for `willow_alloc_typed` must have bit K set when payload
// word K contains a GC-managed pointer (bit 0 = word 0 = state → always 0).
//
// `willow_alloc_typed` uses `alloc_zeroed`, so all fields start at zero:
//   state = 0, slot_count = 0, all data slots = null / 0.
//
// After allocation the caller writes `slot_count` into word 1 via a Cranelift
// store, then uses normal GC-root mechanics to keep the frame alive.

use std::ffi::c_void;

// ---------------------------------------------------------------------------
// Frame header layout constants (exported for use by the compiler backend).
// ---------------------------------------------------------------------------

/// Size in bytes of the fixed header (state + slot_count = 2 × 8 = 16).
pub const ASYNC_FRAME_HEADER_BYTES: usize = 2 * std::mem::size_of::<i64>();

/// Byte offset of the `state` field within the payload.
pub const ASYNC_FRAME_STATE_OFFSET: usize = 0;

/// Byte offset of the `slot_count` field within the payload.
pub const ASYNC_FRAME_SLOT_COUNT_OFFSET: usize = 8;

/// Byte offset of data slot `n` within the payload.
pub const fn async_frame_slot_offset(n: usize) -> usize {
    ASYNC_FRAME_HEADER_BYTES + n * std::mem::size_of::<usize>()
}

// ---------------------------------------------------------------------------
// ABI: frame allocation
// ---------------------------------------------------------------------------

/// Allocate a GC-managed async frame with `slot_count` data slots.
///
/// `gc_slot_mask` has bit K = 1 when data slot K holds a GC-managed pointer
/// (bit 0 = slot 0).  The header words (state + slot_count) are never GC
/// pointers, so bits for them in the internal gc_ref_mask are always 0.
///
/// The returned pointer is the GC payload pointer (past the GcHeader).
/// All bytes are zero-initialized by the allocator.
#[unsafe(no_mangle)]
pub extern "C" fn willow_async_frame_alloc(slot_count: i64, gc_slot_mask: u64) -> *mut c_void {
    let slots = slot_count.max(0) as usize;
    let payload_bytes = ASYNC_FRAME_HEADER_BYTES + slots * std::mem::size_of::<usize>();

    // The header occupies 2 words (bits 0–1 of gc_ref_mask) and is never GC.
    // Data slot K maps to payload word (2 + K), i.e. bit (2 + K).
    let gc_ref_mask = gc_slot_mask << 2;

    crate::gc::willow_alloc_typed(payload_bytes as i64, gc_ref_mask) as *mut c_void
    // Zero-initialization is guaranteed by allocate_object (uses alloc_zeroed).
    // state = 0, slot_count = 0. Callers write slot_count via a Cranelift store.
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{
        header_size_for_test, reset_internal_for_test, runtime_test_guard, willow_alloc_object,
        willow_gc_allocated_bytes, willow_gc_collect, willow_pop_root, willow_push_root,
    };

    fn reset() {
        reset_internal_for_test();
    }

    fn frame_payload_bytes(slots: usize) -> usize {
        ASYNC_FRAME_HEADER_BYTES + slots * std::mem::size_of::<usize>()
    }

    // -------------------------------------------------------------------------
    // F1: willow_async_frame_alloc returns non-null
    // -------------------------------------------------------------------------
    #[test]
    fn frame_f1_alloc_returns_non_null() {
        let _guard = runtime_test_guard();
        reset();
        let frame = willow_async_frame_alloc(2, 0);
        assert!(!frame.is_null());
        reset();
    }

    // -------------------------------------------------------------------------
    // F2: state at offset 0 defaults to 0 (alloc_zeroed guarantees this)
    // -------------------------------------------------------------------------
    #[test]
    fn frame_f2_initial_state_is_zero() {
        let _guard = runtime_test_guard();
        reset();
        let frame = willow_async_frame_alloc(2, 0);
        let state = unsafe { *(frame as *const i64) };
        assert_eq!(state, 0);
        reset();
    }

    // -------------------------------------------------------------------------
    // F3: state can be written and read back via direct memory access
    // -------------------------------------------------------------------------
    #[test]
    fn frame_f3_state_read_write() {
        let _guard = runtime_test_guard();
        reset();
        let frame = willow_async_frame_alloc(2, 0);
        unsafe { *(frame as *mut i64) = 7 };
        assert_eq!(unsafe { *(frame as *const i64) }, 7);
        reset();
    }

    // -------------------------------------------------------------------------
    // F4: ASYNC_FRAME_STATE_OFFSET is 0
    // -------------------------------------------------------------------------
    #[test]
    fn frame_f4_state_offset_is_zero() {
        assert_eq!(ASYNC_FRAME_STATE_OFFSET, 0);
    }

    // -------------------------------------------------------------------------
    // F5: slot 0 is at ASYNC_FRAME_HEADER_BYTES offset
    // -------------------------------------------------------------------------
    #[test]
    fn frame_f5_slot_zero_offset() {
        assert_eq!(async_frame_slot_offset(0), ASYNC_FRAME_HEADER_BYTES);
    }

    // -------------------------------------------------------------------------
    // F6: slot 1 is one word after slot 0
    // -------------------------------------------------------------------------
    #[test]
    fn frame_f6_slot_one_offset() {
        assert_eq!(
            async_frame_slot_offset(1),
            ASYNC_FRAME_HEADER_BYTES + std::mem::size_of::<usize>()
        );
    }

    // -------------------------------------------------------------------------
    // F7: unrooted frame is collected by GC
    // -------------------------------------------------------------------------
    #[test]
    fn frame_f7_unrooted_frame_collected() {
        let _guard = runtime_test_guard();
        reset();
        let _ = willow_async_frame_alloc(2, 0);
        assert!(willow_gc_allocated_bytes() > 0);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset();
    }

    // -------------------------------------------------------------------------
    // F8: rooted frame survives GC
    // -------------------------------------------------------------------------
    #[test]
    fn frame_f8_rooted_frame_survives_gc() {
        let _guard = runtime_test_guard();
        reset();
        let frame_raw = willow_async_frame_alloc(2, 0) as *mut u8;
        let mut root: *mut u8 = frame_raw;
        willow_push_root(&mut root as *mut *mut u8);

        willow_gc_collect();
        assert!(willow_gc_allocated_bytes() > 0);

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset();
    }

    // -------------------------------------------------------------------------
    // F9: GC-pointer slot keeps referenced object alive (key acceptance test).
    //
    // An object stored in a GC-ptr slot of a rooted async frame survives
    // gc_collect() because the GC traces through the gc_ref_mask.
    // -------------------------------------------------------------------------
    #[test]
    fn frame_f9_gc_ptr_slot_keeps_object_alive() {
        let _guard = runtime_test_guard();
        reset();

        // Allocate the "local variable" the frame will keep alive.
        let local_obj = willow_alloc_object(1, 16);
        assert!(!local_obj.is_null());

        // Allocate frame with 1 GC-pointer slot (gc_slot_mask bit 0 = slot 0).
        let frame_raw = willow_async_frame_alloc(1, 0b1) as *mut u8;
        assert!(!frame_raw.is_null());

        // Write local_obj into slot 0 via direct memory access.
        let slot0_ptr = unsafe {
            (frame_raw as *mut u8)
                .add(async_frame_slot_offset(0))
                .cast::<*mut u8>()
        };
        unsafe { slot0_ptr.write(local_obj) };

        // Root only the frame.
        let mut frame_root: *mut u8 = frame_raw;
        willow_push_root(&mut frame_root as *mut *mut u8);

        // GC must keep both frame and local object alive via gc_ref_mask tracing.
        willow_gc_collect();
        let alive = willow_gc_allocated_bytes();
        let frame_size = (header_size_for_test() + frame_payload_bytes(1)) as i64;
        let local_size = (header_size_for_test() + 16) as i64;
        assert_eq!(
            alive,
            frame_size + local_size,
            "frame and local object must both survive (gc_ref_mask interior tracing)"
        );

        // Remove root → both collected.
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset();
    }

    // -------------------------------------------------------------------------
    // F10: clearing a slot allows the referenced object to be collected
    // -------------------------------------------------------------------------
    #[test]
    fn frame_f10_clearing_slot_allows_gc_of_referenced_object() {
        let _guard = runtime_test_guard();
        reset();

        let obj = willow_alloc_object(1, 8);
        let frame_raw = willow_async_frame_alloc(1, 0b1) as *mut u8;

        let slot0_ptr = unsafe {
            (frame_raw as *mut u8)
                .add(async_frame_slot_offset(0))
                .cast::<*mut u8>()
        };
        unsafe { slot0_ptr.write(obj) };

        let mut frame_root: *mut u8 = frame_raw;
        willow_push_root(&mut frame_root as *mut *mut u8);

        // Clear the slot → set to null.
        unsafe { slot0_ptr.write(std::ptr::null_mut()) };
        // Also mark frame state as done.
        unsafe { *(frame_raw as *mut i64) = i64::MAX };

        willow_gc_collect();
        // Only the frame remains; obj was cleared.
        let frame_size = (header_size_for_test() + frame_payload_bytes(1)) as i64;
        assert_eq!(
            willow_gc_allocated_bytes(),
            frame_size,
            "obj freed after slot cleared"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset();
    }

    // -------------------------------------------------------------------------
    // F11: zero-slot frame allocates correctly
    // -------------------------------------------------------------------------
    #[test]
    fn frame_f11_zero_slot_frame() {
        let _guard = runtime_test_guard();
        reset();
        let frame = willow_async_frame_alloc(0, 0);
        assert!(!frame.is_null());
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset();
    }

    // -------------------------------------------------------------------------
    // F12: gc_ref_mask is shifted correctly (header words are never GC)
    // -------------------------------------------------------------------------
    #[test]
    fn frame_f12_gc_ref_mask_shift_skips_header_words() {
        // gc_slot_mask = 0b1 (slot 0 is GC) should map to gc_ref_mask bit 2
        // (header is 2 words wide, so slots start at bit position 2).
        // We verify this indirectly: with mask 0b1 and slot 0 holding a GC ptr,
        // the interior tracing works (same as F9).
        let _guard = runtime_test_guard();
        reset();

        let obj = willow_alloc_object(1, 8);
        let frame_raw = willow_async_frame_alloc(1, 0b1) as *mut u8;
        let slot0 = unsafe {
            (frame_raw as *mut u8)
                .add(async_frame_slot_offset(0))
                .cast::<*mut u8>()
        };
        unsafe { slot0.write(obj) };

        let mut root: *mut u8 = frame_raw;
        willow_push_root(&mut root as *mut *mut u8);
        willow_gc_collect();

        // Both objects alive → gc_ref_mask tracing worked correctly.
        assert!(willow_gc_allocated_bytes() >= (header_size_for_test() + 8) as i64 * 2);

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset();
    }

    // -------------------------------------------------------------------------
    // F13: a STRING reference stored in a frame GC slot survives collection
    // while the frame is rooted (matches the compiler mask bit for a String
    // frame field — willow-lpn.4).
    // -------------------------------------------------------------------------
    #[test]
    fn frame_f13_string_ref_slot_survives() {
        let _guard = runtime_test_guard();
        reset();

        let s = crate::string::willow_string_from_str("hello");
        let frame_raw = willow_async_frame_alloc(1, 0b1) as *mut u8;
        let slot0 = unsafe { frame_raw.add(async_frame_slot_offset(0)).cast::<*mut u8>() };
        unsafe { slot0.write(s) };

        let baseline = willow_gc_allocated_bytes();
        let mut root: *mut u8 = frame_raw;
        willow_push_root(&mut root as *mut *mut u8);

        // The string is reachable only through the frame's GC slot; it must survive.
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            baseline,
            "string in a frame GC slot must survive while the frame is rooted"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset();
    }

    // -------------------------------------------------------------------------
    // F14: a MIXED frame (scalar slot + reference slot) traces only the
    // reference. The scalar slot holds a non-pointer value and must not be
    // dereferenced by marking (mask bit clear), while the reference survives.
    // -------------------------------------------------------------------------
    #[test]
    fn frame_f14_mixed_scalar_and_ref_slots() {
        let _guard = runtime_test_guard();
        reset();

        let s = crate::string::willow_string_from_str("world");
        // slot 0 = scalar (mask bit 0 clear), slot 1 = ref (mask bit 1 set).
        let frame_raw = willow_async_frame_alloc(2, 0b10) as *mut u8;
        let slot0 = unsafe { frame_raw.add(async_frame_slot_offset(0)).cast::<i64>() };
        let slot1 = unsafe { frame_raw.add(async_frame_slot_offset(1)).cast::<*mut u8>() };
        unsafe { slot0.write(42) }; // a bare integer, NOT a pointer
        unsafe { slot1.write(s) };

        let baseline = willow_gc_allocated_bytes();
        let mut root: *mut u8 = frame_raw;
        willow_push_root(&mut root as *mut *mut u8);

        // Must not crash on the scalar slot, and must keep the string alive.
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), baseline);
        assert_eq!(unsafe { *slot0 }, 42, "scalar slot must be untouched");

        // Clearing the reference slot lets the string be collected.
        unsafe { slot1.write(std::ptr::null_mut()) };
        willow_gc_collect();
        let frame_size = (header_size_for_test() + frame_payload_bytes(2)) as i64;
        assert_eq!(
            willow_gc_allocated_bytes(),
            frame_size,
            "string freed once its frame slot is cleared"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset();
    }

    // -------------------------------------------------------------------------
    // F15: an ARRAY reference stored in a frame GC slot survives collection
    // while the frame is rooted (matches the compiler mask bit for an Array
    // frame field — willow-lpn.4).
    // -------------------------------------------------------------------------
    #[test]
    fn frame_f15_array_ref_slot_survives() {
        let _guard = runtime_test_guard();
        reset();

        // A reference-element array (handle + buffer are GC objects).
        let arr = crate::array::willow_array_new(2, 1);
        let frame_raw = willow_async_frame_alloc(1, 0b1) as *mut u8;
        let slot0 = unsafe { frame_raw.add(async_frame_slot_offset(0)).cast::<*mut u8>() };
        unsafe { slot0.write(arr) };

        let baseline = willow_gc_allocated_bytes();
        let mut root: *mut u8 = frame_raw;
        willow_push_root(&mut root as *mut *mut u8);

        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            baseline,
            "array in a frame GC slot must survive while the frame is rooted"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset();
    }
}
