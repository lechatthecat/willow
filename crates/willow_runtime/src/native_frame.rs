//! Typed access to runtime-created async frames.
//!
//! Compiler-generated frames have function-specific layouts. Native runtime
//! tasks (filesystem, netpoll, parallel map, structured cancellation) instead
//! declare a small [`NativeFrameSpec`] and access slots through this wrapper.
//! Allocation, slot-count initialization, bounds/kind validation, GC barriers,
//! and pointer arithmetic therefore live in one place.

use std::ffi::c_void;
use std::marker::PhantomData;

use willow_abi::{NativeFrameLayout, SlotKind};

use crate::gc::{GcStoreDestination, willow_gc_write_barrier};

/// Static layout of one native poll frame.
pub trait NativeFrameSpec {
    const LAYOUT: NativeFrameLayout<'static>;
    const NAME: &'static str;
}

/// A borrowed typed view of a native async frame.
pub struct NativeTaskFrame<S: NativeFrameSpec> {
    raw: *mut c_void,
    marker: PhantomData<S>,
}

impl<S: NativeFrameSpec> NativeTaskFrame<S> {
    /// Allocate a zeroed frame and initialize its header from `S::LAYOUT`.
    pub fn allocate() -> Option<Self> {
        let layout = S::LAYOUT;
        let raw = crate::async_frame::willow_async_frame_alloc(
            i64::from(layout.slot_count()),
            layout.data_gc_ref_mask(),
        );
        if raw.is_null() {
            return None;
        }
        // SAFETY: allocation contains the fixed async header, and the slot
        // count word is part of that header.
        unsafe {
            *((raw as *mut u8)
                .add(crate::async_frame::ASYNC_FRAME_SLOT_COUNT_OFFSET)
                .cast::<i64>()) = i64::from(layout.slot_count());
        }
        Some(Self {
            raw,
            marker: PhantomData,
        })
    }

    /// View a raw pointer passed to a poll/cancel callback.
    ///
    /// # Safety
    /// `raw` must be a live frame allocated using `S::LAYOUT`.
    pub unsafe fn from_raw(raw: *mut c_void) -> Self {
        debug_assert!(!raw.is_null(), "{} frame is null", S::NAME);
        Self {
            raw,
            marker: PhantomData,
        }
    }

    pub fn as_raw(&self) -> *mut c_void {
        self.raw
    }

    fn check(&self, slot: usize, expected: SlotKind) {
        let actual = S::LAYOUT.slots.slots.get(slot).copied().unwrap_or_else(|| {
            panic!(
                "{} native frame slot {slot} is outside its {}-slot layout",
                S::NAME,
                S::LAYOUT.slot_count()
            )
        });
        assert_eq!(
            actual,
            expected,
            "{} native frame slot {slot} has ABI kind {actual:?}, accessed as {expected:?}",
            S::NAME
        );
    }

    unsafe fn slot<T>(&self, slot: usize) -> *mut T {
        let offset = S::LAYOUT.slot_offset(slot as u32, std::mem::size_of::<usize>() as u32);
        unsafe { (self.raw as *mut u8).add(offset as usize).cast::<T>() }
    }

    pub fn load_word(&self, slot: usize) -> i64 {
        self.check(slot, SlotKind::Word);
        // SAFETY: checked slot is a word in this live frame.
        unsafe { *self.slot::<i64>(slot) }
    }

    pub fn store_word(&self, slot: usize, value: i64) {
        self.check(slot, SlotKind::Word);
        // SAFETY: checked slot is a word in this live frame.
        unsafe { *self.slot::<i64>(slot) = value };
    }

    pub fn load_gc(&self, slot: usize) -> *mut u8 {
        self.check(slot, SlotKind::GcRef);
        // SAFETY: checked slot is a GC reference in this live frame.
        unsafe { *self.slot::<*mut u8>(slot) }
    }

    pub fn store_gc(&self, slot: usize, value: *mut u8) {
        self.check(slot, SlotKind::GcRef);
        willow_gc_write_barrier(
            self.raw.cast::<u8>(),
            value,
            GcStoreDestination::AsyncFrameSlot as i64,
        );
        // SAFETY: checked slot is a GC reference in this live frame.
        unsafe { *self.slot::<*mut u8>(slot) = value };
    }

    pub fn load_native<T>(&self, slot: usize) -> *mut T {
        self.check(slot, SlotKind::NativePtr);
        // SAFETY: checked slot is a native pointer in this live frame.
        unsafe { *self.slot::<*mut T>(slot) }
    }

    pub fn store_native<T>(&self, slot: usize, value: *mut T) {
        self.check(slot, SlotKind::NativePtr);
        // SAFETY: checked slot is a native pointer in this live frame.
        unsafe { *self.slot::<*mut T>(slot) = value };
    }

    pub fn take_native<T>(&self, slot: usize) -> *mut T {
        let value = self.load_native(slot);
        self.store_native(slot, std::ptr::null_mut::<T>());
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{reset_internal_for_test, runtime_test_guard};

    struct TestFrame;

    impl NativeFrameSpec for TestFrame {
        const LAYOUT: NativeFrameLayout<'static> =
            NativeFrameLayout::new(&[SlotKind::GcRef, SlotKind::Word, SlotKind::NativePtr]);
        const NAME: &'static str = "test";
    }

    #[test]
    fn typed_native_frame_initializes_layout_and_round_trips_slot_kinds() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let frame = NativeTaskFrame::<TestFrame>::allocate().expect("frame allocation");
        assert_eq!(
            unsafe {
                *((frame.as_raw() as *mut u8)
                    .add(crate::async_frame::ASYNC_FRAME_SLOT_COUNT_OFFSET)
                    .cast::<i64>())
            },
            3
        );
        frame.store_word(1, 42);
        assert_eq!(frame.load_word(1), 42);
        let boxed = Box::into_raw(Box::new(7u64));
        frame.store_native(2, boxed);
        assert_eq!(frame.load_native::<u64>(2), boxed);
        assert_eq!(frame.take_native::<u64>(2), boxed);
        assert!(frame.load_native::<u64>(2).is_null());
        unsafe { drop(Box::from_raw(boxed)) };
        reset_internal_for_test();
    }

    #[test]
    #[should_panic(expected = "accessed as Word")]
    fn typed_native_frame_rejects_slot_kind_confusion() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let frame = NativeTaskFrame::<TestFrame>::allocate().expect("frame allocation");
        frame.store_word(0, 1);
    }
}
