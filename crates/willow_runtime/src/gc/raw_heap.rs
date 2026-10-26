use std::ptr::NonNull;
use std::sync::atomic::{AtomicU8, Ordering};

use super::{GC_STORAGE_WORD_BYTES, GcHeader};

#[derive(Clone, Copy)]
pub(super) struct Payload(NonNull<u8>);

impl Payload {
    pub(super) fn from_raw(raw: *mut u8) -> Option<Self> {
        NonNull::new(raw).map(Self)
    }

    pub(super) fn as_ptr(self) -> *mut u8 {
        self.0.as_ptr()
    }
}

#[derive(Clone, Copy)]
pub(super) struct Object(NonNull<GcHeader>);

#[derive(Clone, Copy)]
pub(super) struct TraceMetadata {
    pub(super) type_id: u32,
    pub(super) layout_id: u64,
    pub(super) gc_ref_mask: u64,
    pub(super) payload_size: usize,
}

impl TraceMetadata {
    pub(super) fn inline_ref_mask(self) -> u64 {
        if self.type_id == willow_abi::GC_BITMAP_TYPE_ID {
            let descriptor = self.gc_ref_mask as *const u64;
            // SAFETY: bitmap allocations validate immutable descriptors whose
            // lifetime covers the object, including collection snapshots.
            unsafe {
                if *descriptor == 0 {
                    0
                } else {
                    *descriptor.add(1)
                }
            }
        } else {
            self.gc_ref_mask
        }
    }
}

impl Object {
    pub(super) fn from_raw(raw: *mut GcHeader) -> Option<Self> {
        NonNull::new(raw).map(Self)
    }

    pub(super) fn from_payload(payload: Payload) -> Self {
        let header_size = std::mem::size_of::<GcHeader>();
        // SAFETY: GC payloads are returned immediately after their header.
        let header = unsafe { payload.as_ptr().sub(header_size) as *mut GcHeader };
        Self(NonNull::new(header).expect("non-null payload has a header address"))
    }

    pub(super) fn initialize_at(
        raw: *mut u8,
        size: usize,
        type_id: u32,
        layout_id: u64,
        gc_ref_mask: u64,
        generation: u8,
    ) -> Option<Self> {
        let mut header = NonNull::new(raw.cast::<GcHeader>())?;
        // SAFETY: the allocation is writable, aligned, and large enough for
        // one header followed by its zeroed payload.
        unsafe {
            let header = header.as_mut();
            header.marked = false;
            header.allocated = true;
            header.generation = generation;
            header.age = 0;
            header.remembered = 0;
            header.descriptor = super::layouts::acquire(willow_abi::GcLayoutDescriptor {
                type_id: u64::from(type_id),
                layout_id,
                gc_ref_mask,
                size: size as u64,
            });
            header.descriptor_owned = true;
        }
        Some(Self(header))
    }

    /// Initialize an evacuation destination, transferring the descriptor reference.
    /// The collector keeps the destination alive until source metadata is no
    /// longer read, and reclaims the source without releasing that reference.
    /// No interning or reference-count operation is needed for a move.
    pub(super) fn initialize_moved_at(raw: *mut u8, source: Self, generation: u8) -> Option<Self> {
        let target = NonNull::new(raw.cast::<GcHeader>())?;
        // SAFETY: under STW, raw is a distinct, aligned, writable destination
        // with source.size() bytes. Source remains readable until minor sweep;
        // the successfully evacuated target owns its descriptor lifetime.
        unsafe {
            let source = &mut *source.as_ptr();
            // The destination may be uninitialized storage; do not form a
            // reference to a header containing uninitialized bool fields.
            target.as_ptr().write(GcHeader {
                marked: false,
                allocated: true,
                generation,
                age: 0,
                remembered: 0,
                descriptor: source.descriptor,
                descriptor_owned: source.descriptor_owned,
            });
            source.descriptor_owned = false;
        }
        Some(Self(target))
    }

    pub(super) fn as_ptr(self) -> *mut GcHeader {
        self.0.as_ptr()
    }

    pub(super) fn payload(self) -> Payload {
        // SAFETY: the allocation contains a header followed by the payload.
        let raw = unsafe {
            self.as_ptr()
                .cast::<u8>()
                .add(std::mem::size_of::<GcHeader>())
        };
        Payload(NonNull::new(raw).expect("object payload address is non-null"))
    }

    pub(super) fn begin_trace(self) -> Option<TraceMetadata> {
        // SAFETY: stopped tracing has exclusive access; sweep-time black
        // allocation calls this only before the new object is published.
        let header = unsafe { &mut *self.as_ptr() };
        if !header.allocated || header.marked {
            return None;
        }
        header.marked = true;
        Some(self.trace_metadata())
    }

    pub(super) fn trace_metadata(self) -> TraceMetadata {
        // Immutable descriptor fields remain valid for live objects. Sweep
        // may clear the distinct mark byte, so do not borrow the header.
        let descriptor = self.descriptor();
        TraceMetadata {
            type_id: descriptor.type_id as u32,
            layout_id: descriptor.layout_id,
            gc_ref_mask: descriptor.gc_ref_mask,
            payload_size: descriptor.size as usize - std::mem::size_of::<GcHeader>(),
        }
    }

    pub(super) fn payload_word(self, index: usize) -> Option<Payload> {
        // SAFETY: the caller bounds `index` by the payload size.
        let child = unsafe {
            *self
                .payload()
                .as_ptr()
                .add(index * GC_STORAGE_WORD_BYTES)
                .cast::<*mut u8>()
        };
        Payload::from_raw(child)
    }

    pub(super) fn payload_slot(self, index: usize) -> *mut *mut u8 {
        // SAFETY: callers bound `index` by the payload word count.
        unsafe {
            self.payload()
                .as_ptr()
                .add(index * GC_STORAGE_WORD_BYTES)
                .cast::<*mut u8>()
        }
    }

    pub(super) fn marked(self) -> bool {
        // SAFETY: `Object` refers to a live heap allocation.
        unsafe { (*self.as_ptr()).marked }
    }

    pub(super) fn allocated(self) -> bool {
        // SAFETY: `Object` refers to storage containing a valid header.
        unsafe { (*self.as_ptr()).allocated }
    }

    pub(super) fn is_async_frame(self) -> bool {
        let d = self.descriptor();
        d.layout_id
            == willow_abi::gc_layout_id(
                willow_abi::GcObjectKind::AsyncFrame,
                (d.size as usize - std::mem::size_of::<GcHeader>()) as i64,
                d.type_id as i64,
                d.gc_ref_mask,
            )
    }

    /// No state changes on contention. A successful permit pins an interned
    /// descriptor through the liveness bookkeeping and header conversion.
    pub(super) fn try_reclaim_permit(self) -> Option<ReclaimPermit> {
        if unsafe { (*self.as_ptr()).descriptor_owned } && self.allocated() {
            super::layouts::try_release_guard().map(|guard| ReclaimPermit(Some(guard)))
        } else {
            Some(ReclaimPermit(None))
        }
    }
    pub(super) fn reclaim_with_permit(self, permit: ReclaimPermit) {
        self.reclaim_with_release(|address| {
            permit
                .0
                .as_ref()
                .expect("interned descriptor permit")
                .release(address);
        });
    }

    pub(super) fn reclaim_in_place(self) {
        self.reclaim_with_release(super::layouts::release);
    }

    fn reclaim_with_release(self, release: impl FnOnce(usize)) {
        // SAFETY: the closed mark epoch proved this payload unreachable;
        // its readers have quiesced and the heap mutex owns its metadata.
        unsafe {
            let size = self.size();
            let header = &mut *self.as_ptr();
            if header.allocated {
                if header.descriptor_owned {
                    release(header.descriptor);
                }
                header.descriptor = size;
                header.descriptor_owned = false;
                header.allocated = false;
            }
            header.marked = false;
            header.remembered = 0;
        }
    }

    pub(super) fn clear_mark(self) {
        // SAFETY: sweep has exclusive access under the heap lock.
        unsafe { (*self.as_ptr()).marked = false };
    }

    pub(super) fn size(self) -> usize {
        // SAFETY: `Object` refers to a live heap allocation.
        unsafe {
            if (*self.as_ptr()).allocated {
                self.descriptor().size as usize
            } else {
                (*self.as_ptr()).descriptor
            }
        }
    }

    /// Only evacuated source headers carry this tag, while minor STW owns
    /// every reader. Normal descriptors and payloads are pointer-aligned.
    pub(super) fn forwarded_to(self) -> Option<Payload> {
        // Read only the stable fields: other old-header flags may be updated
        // by concurrent collection, so do not borrow the complete header.
        let allocated = unsafe { (*self.as_ptr()).allocated };
        let descriptor = unsafe { (*self.as_ptr()).descriptor };
        (allocated && descriptor & 1 != 0)
            .then(|| Payload::from_raw((descriptor & !1) as *mut u8).unwrap())
    }

    pub(super) fn forward_to(self, target: Self) {
        debug_assert!(self.allocated() && target.allocated());
        debug_assert!(target.forwarded_to().is_none());
        // Ownership was transferred during destination initialization. Its
        // descriptor remains alive until all source metadata reads finish.
        unsafe {
            debug_assert!(!(*self.as_ptr()).descriptor_owned);
            (*self.as_ptr()).descriptor = target.payload().as_ptr() as usize | 1;
        }
    }

    fn descriptor(self) -> willow_abi::GcLayoutDescriptor {
        let address = if let Some(target) = self.forwarded_to() {
            let target = Self::from_payload(target);
            // Destinations are never sources in the same minor cycle. No
            // forwarding-chain traversal or additional registry reference.
            debug_assert!(target.forwarded_to().is_none());
            unsafe { (*target.as_ptr()).descriptor }
        } else {
            unsafe { (*self.as_ptr()).descriptor }
        };
        // SAFETY: live headers own a registry reference or point to static
        // generated data. Evacuated sources borrow their live destination's
        // descriptor, under STW, until the source storage is reclaimed.
        unsafe { *(address as *const willow_abi::GcLayoutDescriptor) }
    }

    pub(super) fn type_id(self) -> u32 {
        // SAFETY: `Object` refers to a live heap allocation.
        self.descriptor().type_id as u32
    }

    pub(super) fn generation(self) -> u8 {
        // SAFETY: `Object` refers to a live heap allocation.
        unsafe { (*self.as_ptr()).generation }
    }

    pub(super) fn remembered(self) -> bool {
        // SAFETY: `Object` refers to a live heap allocation; generated code
        // reads this byte concurrently, so access it atomically.
        unsafe {
            AtomicU8::from_ptr(&raw mut (*self.as_ptr()).remembered).load(Ordering::Relaxed) != 0
        }
    }

    /// Mirror remembered-set membership for the inline barrier filter. The
    /// heap lock (or a stop) orders this with the set update; stops order it
    /// with generated readers.
    pub(super) fn set_remembered(self, remembered: bool) {
        // SAFETY: as in `remembered`.
        unsafe {
            AtomicU8::from_ptr(&raw mut (*self.as_ptr()).remembered)
                .store(u8::from(remembered), Ordering::Relaxed);
        }
    }

    pub(super) fn set_generation(self, generation: u8) {
        // SAFETY: collection has exclusive access while mutators are stopped.
        unsafe {
            (*self.as_ptr()).generation = generation;
            (*self.as_ptr()).age = 0;
            // Generated TLAB headers leave this byte unwritten. A young
            // object is never remembered, so an in-place promotion starts
            // unremembered rather than inheriting stale chunk contents.
            (*self.as_ptr()).remembered = 0;
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct RootSlot(NonNull<*mut u8>);

impl RootSlot {
    pub(super) fn from_raw(raw: *mut *mut u8) -> Option<Self> {
        NonNull::new(raw).map(Self)
    }

    pub(super) fn load(self) -> Option<Payload> {
        // SAFETY: generated code keeps a registered root slot alive until
        // its matching pop. Foreign slots are read only while their owner
        // is parked under the stop-the-world coordinator.
        Payload::from_raw(unsafe { *self.0.as_ptr() })
    }
}

/// Optional registry guard: generated immutable descriptors need no release.
pub(super) struct ReclaimPermit(Option<super::layouts::ReleaseGuard>);

#[cfg(test)]
mod move_tests {
    use super::*;
    use crate::gc::{GC_GENERATION_OLD, GC_GENERATION_YOUNG, GC_HEADER_SIZE};

    #[test]
    fn moving_header_transfers_interned_descriptor_once() {
        let _guard = crate::gc::runtime_test_guard();
        let key = willow_abi::GcLayoutDescriptor {
            type_id: 789123,
            layout_id: 789123,
            gc_ref_mask: 0,
            size: GC_HEADER_SIZE as u64,
        };
        for count in [1, 100, 10000] {
            let before = crate::gc::layouts::references(key);
            let mut source_storage = std::mem::MaybeUninit::<GcHeader>::uninit();
            let source = Object::initialize_at(
                source_storage.as_mut_ptr().cast(),
                GC_HEADER_SIZE,
                key.type_id as u32,
                key.layout_id,
                0,
                GC_GENERATION_YOUNG,
            )
            .unwrap();
            assert_eq!(crate::gc::layouts::references(key), before + 1);
            assert!(
                Object::initialize_moved_at(std::ptr::null_mut(), source, GC_GENERATION_YOUNG)
                    .is_none()
            );
            assert!(unsafe { (*source.as_ptr()).descriptor_owned });
            let descriptor = unsafe { (*source.as_ptr()).descriptor };
            let mut a = std::mem::MaybeUninit::<GcHeader>::uninit();
            let mut b = std::mem::MaybeUninit::<GcHeader>::uninit();
            let mut current = source;
            // Holding the registry exclusively proves moves and moved-source
            // reclamation never consult it. The one live owner remains intact.
            crate::gc::layouts::with_exclusive_registry(|| {
                for index in 0..count {
                    let raw = if index % 2 == 0 {
                        a.as_mut_ptr()
                    } else {
                        b.as_mut_ptr()
                    };
                    let generation = if index + 1 == count {
                        GC_GENERATION_OLD
                    } else {
                        GC_GENERATION_YOUNG
                    };
                    let next =
                        Object::initialize_moved_at(raw.cast(), current, generation).unwrap();
                    assert_eq!(unsafe { (*next.as_ptr()).descriptor }, descriptor);
                    assert!(unsafe { (*next.as_ptr()).descriptor_owned });
                    assert!(!unsafe { (*current.as_ptr()).descriptor_owned });
                    current.forward_to(next);
                    assert_eq!(
                        current.forwarded_to().unwrap().as_ptr(),
                        next.payload().as_ptr()
                    );
                    assert_eq!(current.trace_metadata().type_id, key.type_id as u32);
                    assert_eq!(current.size(), GC_HEADER_SIZE);
                    current.reclaim_in_place();
                    assert!(current.forwarded_to().is_none());
                    current = next;
                }
            });
            assert_eq!(crate::gc::layouts::references(key), before + 1);
            assert_eq!(current.generation(), GC_GENERATION_OLD);
            current.reclaim_in_place();
            assert_eq!(crate::gc::layouts::references(key), before);
            eprintln!("descriptor_moves={count} registry_acquires=1 releases=1");
        }
    }

    #[test]
    fn moving_header_preserves_static_descriptor_and_resets_flags() {
        let descriptor = willow_abi::GcLayoutDescriptor {
            type_id: 42,
            layout_id: 7,
            gc_ref_mask: 0,
            size: GC_HEADER_SIZE as u64,
        };
        let mut header = GcHeader {
            marked: true,
            allocated: true,
            generation: GC_GENERATION_YOUNG,
            age: 1,
            descriptor_owned: false,
            remembered: 1,
            descriptor: (&descriptor as *const _) as usize,
        };
        let source = Object::from_raw(&mut header).unwrap();
        let mut target = std::mem::MaybeUninit::<GcHeader>::uninit();
        let moved =
            Object::initialize_moved_at(target.as_mut_ptr().cast(), source, GC_GENERATION_OLD)
                .unwrap();
        assert!(!moved.marked());
        assert!(!moved.remembered());
        assert_eq!(unsafe { (*moved.as_ptr()).age }, 0);
        assert!(!unsafe { (*moved.as_ptr()).descriptor_owned });
        source.forward_to(moved);
        assert_eq!(source.trace_metadata().layout_id, 7);
        source.reclaim_in_place();
        assert!(source.forwarded_to().is_none());
        assert_eq!(moved.trace_metadata().type_id, 42);
        assert_eq!(moved.trace_metadata().layout_id, 7);
        moved.reclaim_in_place();
        assert_eq!(moved.size(), GC_HEADER_SIZE);
    }
}
