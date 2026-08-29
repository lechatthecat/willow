// Async future frame — GC-managed heap objects for suspended async fn coroutines.
//
// An async frame is allocated through the layout-aware GC allocation path.
// The compiler accesses frame fields directly via Cranelift load/store
// instructions using the layout below; no Rust accessor functions are needed
// (and would require unsafe raw pointer operations).
//
// Layout (each slot = one pointer-sized word, 8 bytes on 64-bit):
//
//   [WillowAsyncFrameHeader (3 words) | data slot 0 | data slot 1 | … ]
//
//   word 0 : state (i64)      — 0 = initial, N = after await N, i64::MAX = done
//   word 1 : slot_count (i64) — number of data slots following the header
//   word 2 : status (i64)     — terminal status + cancel-requested bit; see
//                               the status constants below (willow-ezs.1.3)
//   word 3 : slot 0           — first data slot (GC pointer or scalar)
//   word 4 : slot 1
//   …
//
// The allocation `gc_ref_mask` must have bit K set when payload
// word K contains a GC-managed pointer (bit 0 = word 0 = state → always 0).
//
// The central allocator uses `alloc_zeroed`, so all fields start at zero:
//   state = 0, slot_count = 0, status = Pending, all data slots = null / 0.
//
// After allocation the caller writes `slot_count` into word 1 via a Cranelift
// store, then uses normal GC-root mechanics to keep the frame alive.
//
// `status` deliberately lives in the fixed HEADER, not in a data slot: data
// slot 0 is the task result, slot 1 the task id, slot 2 the first compiler
// param/local (and `FS_TASK_JOB_SLOT` for blocking-fs tasks), so none of them
// is free. Keeping it in the header means every async frame has the field at a
// constant offset regardless of how the compiler laid the rest out.

use std::ffi::c_void;
use std::sync::atomic::{AtomicI64, Ordering};

// ---------------------------------------------------------------------------
// Frame header layout constants (exported for use by the compiler backend).
// ---------------------------------------------------------------------------

/// Number of payload words the fixed header occupies.
pub const ASYNC_FRAME_HEADER_WORDS: u64 = willow_abi::async_frame::HEADER_WORDS as u64;

/// Size in bytes of the fixed header (state + slot_count + status = 3 × 8).
pub const ASYNC_FRAME_HEADER_BYTES: usize =
    willow_abi::async_frame::header_bytes(std::mem::size_of::<usize>() as u32) as usize;

/// Byte offset of the `state` field within the payload.
pub const ASYNC_FRAME_STATE_OFFSET: usize =
    willow_abi::async_frame::STATE_WORD as usize * std::mem::size_of::<usize>();

/// Byte offset of the `slot_count` field within the payload.
pub const ASYNC_FRAME_SLOT_COUNT_OFFSET: usize =
    willow_abi::async_frame::SLOT_COUNT_WORD as usize * std::mem::size_of::<usize>();

/// Byte offset of the `status` field within the payload (willow-ezs.1.3).
pub const ASYNC_FRAME_STATUS_OFFSET: usize =
    willow_abi::async_frame::STATUS_WORD as usize * std::mem::size_of::<usize>();

// ---------------------------------------------------------------------------
// Frame status word (willow-ezs.1.3)
// ---------------------------------------------------------------------------
//
// Bits 0-2 hold the TERMINAL code; bit 8 is the cancel-requested flag. The
// terminal code is written exactly once, by the scheduler, at the transition
// into a terminal state — with `Release`, after the poll fn has already
// written the result slot, so an `Acquire` reader that sees a terminal code is
// guaranteed to see the result too.
//
// Language-visible queries (`await task`, `await task.result()`,
// `is_cancelled`) read this word instead of looking the task up in the
// scheduler's table, so they neither take the global scheduler lock nor depend
// on the heavy `RuntimeTask` record still existing.

/// Not finished: still ready/running/parked.
pub const WILLOW_FRAME_STATUS_PENDING: i64 = willow_abi::FrameTerminalStatus::Pending as i64;
/// Ran to completion; the result slot holds the value.
pub const WILLOW_FRAME_STATUS_COMPLETED: i64 = willow_abi::FrameTerminalStatus::Completed as i64;
/// Cooperatively cancelled; there is NO result to read.
pub const WILLOW_FRAME_STATUS_CANCELLED: i64 = willow_abi::FrameTerminalStatus::Cancelled as i64;
/// Terminated by a panic.
pub const WILLOW_FRAME_STATUS_PANICKED: i64 = willow_abi::FrameTerminalStatus::Panicked as i64;
/// Mask selecting the terminal code out of the status word.
pub const WILLOW_FRAME_STATUS_TERMINAL_MASK: i64 = willow_abi::frame_status::TERMINAL_MASK;
/// Cancellation was requested but the task has not reached the boundary yet.
pub const WILLOW_FRAME_STATUS_CANCEL_REQUESTED: i64 = willow_abi::frame_status::CANCEL_REQUESTED;

/// The status word of `frame` as an atomic, or `None` for a null frame.
///
/// # Safety
/// `frame` must be null or an async frame from [`willow_async_frame_alloc`].
unsafe fn status_word(frame: *mut c_void) -> Option<&'static AtomicI64> {
    if frame.is_null() {
        return None;
    }
    // The word is naturally aligned (the payload is word-aligned and the
    // offset is a multiple of 8), so the atomic view is valid.
    Some(unsafe {
        &*(frame as *mut u8)
            .add(ASYNC_FRAME_STATUS_OFFSET)
            .cast::<AtomicI64>()
    })
}

/// Read a frame's status word (0 when the frame is null).
pub fn frame_status(frame: *mut c_void) -> i64 {
    unsafe { status_word(frame) }.map_or(WILLOW_FRAME_STATUS_PENDING, |word| {
        word.load(Ordering::Acquire)
    })
}

/// The terminal code of `frame`, ignoring the cancel-requested bit.
pub fn frame_terminal_status(frame: *mut c_void) -> i64 {
    frame_status(frame) & WILLOW_FRAME_STATUS_TERMINAL_MASK
}

/// True once `frame`'s task has finished, however it finished.
pub fn frame_is_terminal(frame: *mut c_void) -> bool {
    frame_terminal_status(frame) != WILLOW_FRAME_STATUS_PENDING
}

/// Publish `frame`'s terminal code. Called by the scheduler at the single
/// terminal transition, AFTER the result slot is written; the `Release` here
/// pairs with the `Acquire` in [`frame_status`].
///
/// Idempotent: a frame that already carries a terminal code keeps it, so a
/// repeated or racing transition cannot rewrite a published result.
pub fn frame_publish_terminal(frame: *mut c_void, terminal: i64) {
    let Some(word) = (unsafe { status_word(frame) }) else {
        return;
    };
    let terminal = terminal & WILLOW_FRAME_STATUS_TERMINAL_MASK;
    let mut current = word.load(Ordering::Relaxed);
    loop {
        if current & WILLOW_FRAME_STATUS_TERMINAL_MASK != WILLOW_FRAME_STATUS_PENDING {
            return;
        }
        let next = (current & !WILLOW_FRAME_STATUS_TERMINAL_MASK) | terminal;
        match word.compare_exchange_weak(current, next, Ordering::Release, Ordering::Relaxed) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

/// Set the cancel-requested bit (`Task::cancel()`), leaving any terminal code
/// alone.
pub fn frame_request_cancel(frame: *mut c_void) {
    if let Some(word) = unsafe { status_word(frame) } {
        word.fetch_or(WILLOW_FRAME_STATUS_CANCEL_REQUESTED, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// ABI: frame status queries
// ---------------------------------------------------------------------------

/// Raw status word of a task frame — the compiler emits this wherever it has
/// the awaitee's frame pointer (willow-ezs.1.3).
#[unsafe(no_mangle)]
pub extern "C" fn willow_frame_status(frame: *mut c_void) -> i64 {
    frame_status(frame)
}

/// `Task::is_cancelled()`: 1 once cancellation was requested OR the task has
/// already finished as Cancelled.
#[unsafe(no_mangle)]
pub extern "C" fn willow_frame_is_cancelled(frame: *mut c_void) -> i64 {
    let status = frame_status(frame);
    let requested = status & WILLOW_FRAME_STATUS_CANCEL_REQUESTED != 0;
    let cancelled = status & WILLOW_FRAME_STATUS_TERMINAL_MASK == WILLOW_FRAME_STATUS_CANCELLED;
    i64::from(requested || cancelled)
}

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
/// (bit 0 = slot 0). The header words (state + slot_count + status) are never
/// GC pointers, so bits for them in the internal gc_ref_mask are always 0.
///
/// The returned pointer is the GC payload pointer (past the GcHeader).
/// All bytes are zero-initialized by the allocator.
#[unsafe(no_mangle)]
pub extern "C" fn willow_async_frame_alloc(slot_count: i64, gc_slot_mask: u64) -> *mut c_void {
    let slots = slot_count.max(0) as usize;
    let payload_bytes = ASYNC_FRAME_HEADER_BYTES + slots * std::mem::size_of::<usize>();

    // The header occupies 3 words (bits 0–2 of gc_ref_mask) and is never GC.
    // Data slot K maps to payload word (3 + K), i.e. bit (3 + K).
    let gc_ref_mask = gc_slot_mask << ASYNC_FRAME_HEADER_WORDS;

    crate::gc::willow_alloc_with_layout(
        crate::gc::GcObjectKind::AsyncFrame,
        0,
        payload_bytes as i64,
        gc_ref_mask,
    ) as *mut c_void
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
        // gc_slot_mask = 0b1 (slot 0 is GC) should map to gc_ref_mask bit 3
        // (the header is 3 words wide, so slots start at bit position 3).
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

    // -------------------------------------------------------------------------
    // Frame status word (willow-ezs.1.3).
    //
    // Terminal status lives in the fixed header so that `await task`,
    // `await task.result()` and `is_cancelled` can answer from the frame the
    // caller already holds, with no scheduler lock and no dependency on the
    // task record still being retained. Perspectives covered here:
    //
    //   FS1  a fresh frame reads Pending
    //   FS2  every accessor is null-frame safe
    //   FS3  publishing Completed is observable
    //   FS4  publishing Cancelled is observable
    //   FS5  publishing Panicked is observable
    //   FS6  publication is idempotent (first terminal code wins)
    //   FS7  an out-of-range code cannot spill into the flag bits
    //   FS8  cancel-requested does not fabricate a terminal code
    //   FS9  cancel-requested survives a later terminal publication
    //   FS10 requesting cancel after a terminal code keeps that code
    //   FS11 is_cancelled: pending + requested
    //   FS12 is_cancelled: terminal Cancelled with no explicit request
    //   FS13 is_cancelled: Completed alone is not cancelled
    //   FS14 the status word aliases neither the state/slot_count header
    //        words nor any data slot
    //   FS15 the header offsets the compiler hardcodes are the ones used here
    //   FS16 concurrent publishers: exactly one code wins and it is one of
    //        the codes actually published
    //   FS17 the status word is not traced as a GC pointer
    // -------------------------------------------------------------------------

    /// A stand-alone frame for status tests. Boxed `i64`s are 8-byte aligned,
    /// which is all the status word's atomic view needs, and it keeps these
    /// tests independent of the GC heap.
    fn status_frame() -> Box<[i64; 8]> {
        Box::new([0; 8])
    }

    fn as_frame(frame: &mut [i64; 8]) -> *mut c_void {
        frame.as_mut_ptr() as *mut c_void
    }

    #[test]
    fn frame_fs1_fresh_frame_is_pending() {
        let mut storage = status_frame();
        let frame = as_frame(&mut storage);
        assert_eq!(frame_status(frame), WILLOW_FRAME_STATUS_PENDING);
        assert_eq!(frame_terminal_status(frame), WILLOW_FRAME_STATUS_PENDING);
        assert!(!frame_is_terminal(frame));
    }

    #[test]
    fn frame_fs2_null_frame_is_safe() {
        let null = std::ptr::null_mut();
        assert_eq!(frame_status(null), WILLOW_FRAME_STATUS_PENDING);
        assert!(!frame_is_terminal(null));
        assert_eq!(willow_frame_is_cancelled(null), 0);
        // Neither writer may dereference a null frame.
        frame_publish_terminal(null, WILLOW_FRAME_STATUS_COMPLETED);
        frame_request_cancel(null);
        assert_eq!(willow_frame_status(null), WILLOW_FRAME_STATUS_PENDING);
    }

    #[test]
    fn frame_fs3_publish_completed() {
        let mut storage = status_frame();
        let frame = as_frame(&mut storage);
        frame_publish_terminal(frame, WILLOW_FRAME_STATUS_COMPLETED);
        assert_eq!(frame_terminal_status(frame), WILLOW_FRAME_STATUS_COMPLETED);
        assert!(frame_is_terminal(frame));
    }

    #[test]
    fn frame_fs4_publish_cancelled() {
        let mut storage = status_frame();
        let frame = as_frame(&mut storage);
        frame_publish_terminal(frame, WILLOW_FRAME_STATUS_CANCELLED);
        assert_eq!(frame_terminal_status(frame), WILLOW_FRAME_STATUS_CANCELLED);
        assert!(frame_is_terminal(frame));
    }

    #[test]
    fn frame_fs5_publish_panicked() {
        let mut storage = status_frame();
        let frame = as_frame(&mut storage);
        frame_publish_terminal(frame, WILLOW_FRAME_STATUS_PANICKED);
        assert_eq!(frame_terminal_status(frame), WILLOW_FRAME_STATUS_PANICKED);
        assert!(frame_is_terminal(frame));
    }

    #[test]
    fn frame_fs6_publication_is_idempotent() {
        let mut storage = status_frame();
        let frame = as_frame(&mut storage);
        frame_publish_terminal(frame, WILLOW_FRAME_STATUS_COMPLETED);
        // A second, later transition must not rewrite a published result.
        frame_publish_terminal(frame, WILLOW_FRAME_STATUS_CANCELLED);
        frame_publish_terminal(frame, WILLOW_FRAME_STATUS_COMPLETED);
        assert_eq!(frame_terminal_status(frame), WILLOW_FRAME_STATUS_COMPLETED);
    }

    #[test]
    fn frame_fs7_out_of_range_code_cannot_touch_flag_bits() {
        let mut storage = status_frame();
        let frame = as_frame(&mut storage);
        frame_publish_terminal(frame, WILLOW_FRAME_STATUS_CANCELLED | (1 << 8) | (1 << 40));
        assert_eq!(frame_terminal_status(frame), WILLOW_FRAME_STATUS_CANCELLED);
        assert_eq!(
            frame_status(frame) & WILLOW_FRAME_STATUS_CANCEL_REQUESTED,
            0,
            "a stray high bit in the terminal code must not set cancel-requested"
        );
    }

    #[test]
    fn frame_fs8_cancel_request_is_not_a_terminal_code() {
        let mut storage = status_frame();
        let frame = as_frame(&mut storage);
        frame_request_cancel(frame);
        assert_eq!(frame_terminal_status(frame), WILLOW_FRAME_STATUS_PENDING);
        assert!(
            !frame_is_terminal(frame),
            "a requested cancel is not a finished task"
        );
    }

    #[test]
    fn frame_fs9_cancel_request_survives_terminal_publication() {
        let mut storage = status_frame();
        let frame = as_frame(&mut storage);
        frame_request_cancel(frame);
        frame_publish_terminal(frame, WILLOW_FRAME_STATUS_CANCELLED);
        assert_eq!(frame_terminal_status(frame), WILLOW_FRAME_STATUS_CANCELLED);
        assert_ne!(
            frame_status(frame) & WILLOW_FRAME_STATUS_CANCEL_REQUESTED,
            0
        );
    }

    #[test]
    fn frame_fs10_cancel_request_after_completion_keeps_the_code() {
        let mut storage = status_frame();
        let frame = as_frame(&mut storage);
        frame_publish_terminal(frame, WILLOW_FRAME_STATUS_COMPLETED);
        frame_request_cancel(frame);
        assert_eq!(
            frame_terminal_status(frame),
            WILLOW_FRAME_STATUS_COMPLETED,
            "cancelling a task that already finished must not rewrite its result status"
        );
    }

    #[test]
    fn frame_fs11_is_cancelled_when_requested() {
        let mut storage = status_frame();
        let frame = as_frame(&mut storage);
        assert_eq!(willow_frame_is_cancelled(frame), 0);
        frame_request_cancel(frame);
        assert_eq!(willow_frame_is_cancelled(frame), 1);
    }

    #[test]
    fn frame_fs12_is_cancelled_when_terminally_cancelled() {
        let mut storage = status_frame();
        let frame = as_frame(&mut storage);
        // Finalized as Cancelled without the request bit (e.g. cancelled while
        // the request was recorded on the task record only).
        frame_publish_terminal(frame, WILLOW_FRAME_STATUS_CANCELLED);
        assert_eq!(willow_frame_is_cancelled(frame), 1);
    }

    #[test]
    fn frame_fs13_completed_is_not_cancelled() {
        let mut storage = status_frame();
        let frame = as_frame(&mut storage);
        frame_publish_terminal(frame, WILLOW_FRAME_STATUS_COMPLETED);
        assert_eq!(willow_frame_is_cancelled(frame), 0);
    }

    #[test]
    fn frame_fs14_status_word_aliases_nothing_else() {
        let mut storage = status_frame();
        let frame = as_frame(&mut storage);
        // Fill state, slot_count and the first data slots with markers.
        storage[0] = 11; // state
        storage[1] = 22; // slot_count
        storage[3] = 33; // data slot 0 (result)
        storage[4] = 44; // data slot 1 (task id)
        storage[5] = 55; // data slot 2 (first compiler slot)

        frame_publish_terminal(frame, WILLOW_FRAME_STATUS_CANCELLED);
        frame_request_cancel(frame);

        assert_eq!(storage[0], 11, "state word must be untouched");
        assert_eq!(storage[1], 22, "slot_count word must be untouched");
        assert_eq!(storage[3], 33, "result slot must be untouched");
        assert_eq!(storage[4], 44, "task id slot must be untouched");
        assert_eq!(storage[5], 55, "first compiler slot must be untouched");
        assert_eq!(
            storage[2],
            WILLOW_FRAME_STATUS_CANCELLED | WILLOW_FRAME_STATUS_CANCEL_REQUESTED,
            "the status must live in header word 2 and nowhere else"
        );
    }

    #[test]
    fn frame_fs15_header_offsets_match_the_compiler_constants() {
        assert_eq!(ASYNC_FRAME_HEADER_WORDS, 3);
        assert_eq!(ASYNC_FRAME_HEADER_BYTES, 24);
        assert_eq!(ASYNC_FRAME_STATE_OFFSET, 0);
        assert_eq!(ASYNC_FRAME_SLOT_COUNT_OFFSET, 8);
        assert_eq!(ASYNC_FRAME_STATUS_OFFSET, 16);
        assert_eq!(async_frame_slot_offset(0), 24);
        assert_eq!(async_frame_slot_offset(2), 40);
    }

    #[test]
    fn frame_fs16_concurrent_publishers_agree_on_one_code() {
        let mut storage = status_frame();
        let addr = storage.as_mut_ptr() as usize;
        std::thread::scope(|scope| {
            for terminal in [
                WILLOW_FRAME_STATUS_COMPLETED,
                WILLOW_FRAME_STATUS_CANCELLED,
                WILLOW_FRAME_STATUS_PANICKED,
            ] {
                scope.spawn(move || {
                    let frame = addr as *mut c_void;
                    for _ in 0..1000 {
                        frame_publish_terminal(frame, terminal);
                    }
                });
            }
        });
        let terminal = frame_terminal_status(as_frame(&mut storage));
        assert!(
            [
                WILLOW_FRAME_STATUS_COMPLETED,
                WILLOW_FRAME_STATUS_CANCELLED,
                WILLOW_FRAME_STATUS_PANICKED,
            ]
            .contains(&terminal),
            "the winning code must be one that was actually published: {terminal}"
        );
    }

    #[test]
    fn frame_fs17_status_word_is_not_traced_as_a_gc_pointer() {
        let _guard = runtime_test_guard();
        reset();

        // A GC-managed frame whose only GC slot is data slot 0.
        let obj = willow_alloc_object(1, 8);
        let frame_raw = willow_async_frame_alloc(1, 0b1) as *mut u8;
        let slot0 = unsafe { frame_raw.add(async_frame_slot_offset(0)).cast::<*mut u8>() };
        unsafe { slot0.write(obj) };

        // A terminal status leaves a small non-pointer integer in header word 2;
        // marking must not follow it.
        frame_publish_terminal(frame_raw as *mut c_void, WILLOW_FRAME_STATUS_PANICKED);
        frame_request_cancel(frame_raw as *mut c_void);

        let mut root: *mut u8 = frame_raw;
        willow_push_root(&mut root as *mut *mut u8);
        willow_gc_collect();
        assert_eq!(
            frame_terminal_status(frame_raw as *mut c_void),
            WILLOW_FRAME_STATUS_PANICKED,
            "collection must not disturb the status word"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset();
    }
}
