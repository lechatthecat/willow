use std::mem::MaybeUninit;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU8, Ordering};

use super::{GC_STORAGE_WORD_BYTES, GcHeader};

#[derive(Clone, Copy)]
pub(super) struct Payload(NonNull<u8>);

impl GcHeader {
    /// The payload must begin immediately after its GC header.
    #[inline]
    unsafe fn preceding(payload: *const u8) -> *const Self {
        // SAFETY: region and TLAB allocators align each object for GcHeader.
        // Subtracting one header recovers the original allocation address.
        unsafe { payload.cast::<Self>().sub(1) }
    }

    /// Mutator-side metadata for a live, unforwarded payload. Only read the
    /// descriptor field, since marking can update other header bytes.
    #[inline]
    pub(crate) unsafe fn live_descriptor<'a>(
        payload: *const u8,
    ) -> &'a willow_abi::GcLayoutDescriptor {
        // SAFETY: the caller retains the live allocation during this borrow.
        // Its untagged descriptor points to interned or compiler-static storage.
        unsafe {
            &*((*Self::preceding(payload)).descriptor as *const willow_abi::GcLayoutDescriptor)
        }
    }

    /// Read the generation of a live, unforwarded payload outside a moving collection.
    #[inline]
    pub(crate) unsafe fn live_generation(payload: *const u8) -> u8 {
        // SAFETY: the caller retains this live payload; generation changes only under STW.
        unsafe { (*Self::preceding(payload)).generation }
    }
}

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
        // SAFETY: Payload denotes the start of the GC object's data.
        let header = unsafe { GcHeader::preceding(payload.as_ptr()) }.cast_mut();
        Self(NonNull::new(header).expect("non-null payload has a header address"))
    }

    /// # Safety
    /// A non-null address must be exclusive header-aligned space for the complete header and
    /// payload. The type, payload extent and tracing mask must agree, including any required
    /// bitmap descriptor or native hooks. Initialize native fields before the object can be
    /// traced or finalized.
    pub(super) unsafe fn initialize_at(
        raw: *mut u8,
        size: usize,
        type_id: u32,
        layout_id: u64,
        gc_ref_mask: u64,
        generation: u8,
    ) -> Option<Self> {
        // SAFETY: a non-null raw address is exclusive, GcHeader-aligned storage from
        // the allocator. MaybeUninit permits the slot to contain arbitrary bytes.
        let slot = unsafe { raw.cast::<MaybeUninit<GcHeader>>().as_mut()? };
        let header = slot.write(GcHeader {
            marked: false,
            allocated: true,
            generation,
            age: 0,
            descriptor_owned: false,
            remembered: 0,
            shared: 0,
            descriptor: 0,
        });
        header.descriptor = super::layouts::acquire(willow_abi::GcLayoutDescriptor {
            type_id: u64::from(type_id),
            layout_id,
            gc_ref_mask,
            size: size as u64,
        });
        header.descriptor_owned = true;
        Some(Self(NonNull::from(header)))
    }

    /// Initialize an evacuation destination, transferring the descriptor reference.
    /// The collector keeps the destination alive until source metadata is no
    /// longer read, and reclaims the source without releasing that reference.
    /// No interning or reference-count operation is needed for a move.
    /// # Safety
    /// Supply a distinct, exclusive, header-aligned destination spanning the source object.
    /// For evacuation, the mutators must be stopped, the source must be live and match the
    /// metadata, and ownership of its descriptor must transfer exactly once.
    pub(super) unsafe fn initialize_moved_at(
        raw: *mut u8,
        source: Self,
        generation: u8,
    ) -> Option<Self> {
        // SAFETY: Allocation metadata identifies a header-aligned object start within this live
        // region or chunk.
        let target = NonNull::new(raw.cast::<GcHeader>())?;
        // SAFETY: under STW, raw is a distinct, aligned, writable destination
        // with source.size() bytes. Source remains readable until minor sweep;
        // the successfully evacuated target owns its descriptor lifetime.
        unsafe {
            let source = &mut *source.as_ptr();
            // The destination may be uninitialized storage; do not form a
            // reference to a header containing uninitialized bool fields.
            // The flag bytes are composed in a register and written with one
            // word store: narrow stores followed by the collector's later
            // header reads would defeat store forwarding on every copy.
            let mut flags = [0u8; MOVED_FLAG_BYTES];
            flags[std::mem::offset_of!(GcHeader, allocated)] = 1;
            flags[std::mem::offset_of!(GcHeader, generation)] = generation;
            flags[std::mem::offset_of!(GcHeader, descriptor_owned)] =
                u8::from(source.descriptor_owned);
            // A moved object keeps its publication state: every holder of
            // a shared object is stopped and its slots are rewritten.
            flags[std::mem::offset_of!(GcHeader, shared)] = source.shared;
            let header = target.as_ptr();
            header.cast::<u64>().write(u64::from_ne_bytes(flags));
            (&raw mut (*header).descriptor).write(source.descriptor);
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
            // SAFETY: GC payloads are word-aligned and the bounded word index preserves pointer
            // alignment.
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
            // SAFETY: GC payloads are word-aligned and the bounded word index preserves pointer
            // alignment.
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
        is_async_frame_layout(&self.descriptor())
    }

    /// Inspect an unforwarded object's descriptor while the object is live.
    /// Only generated descriptors have a process-lifetime address.
    #[inline(always)]
    pub(super) fn with_live_layout<R>(
        self,
        inspect: impl FnOnce(&willow_abi::GcLayoutDescriptor, bool) -> R,
    ) -> R {
        let header = self.as_ptr();
        // SAFETY: publication keeps the object live through this inspection.
        // Its descriptor is static or retained by the header's registry reference.
        // These two fields are stable even while sweep writes the mark byte.
        let (layout, owned) = unsafe {
            let address = (&raw const (*header).descriptor).read();
            let owned = (&raw const (*header).descriptor_owned).read();
            (&*(address as *const willow_abi::GcLayoutDescriptor), owned)
        };
        inspect(layout, !owned)
    }

    /// This object's layout descriptor, read once.
    pub(super) fn layout(self) -> willow_abi::GcLayoutDescriptor {
        self.descriptor()
    }

    /// No state changes on contention. A successful permit pins an interned
    /// descriptor through the liveness bookkeeping and header conversion.
    pub(super) fn try_reclaim_permit(self) -> Option<ReclaimPermit> {
        // SAFETY: The collector owns this live header through the metadata access.
        if unsafe { (*self.as_ptr()).descriptor_owned } && self.allocated() {
            // SAFETY: The collector owns this live header through the metadata access.
            super::layouts::try_release_guard(unsafe { (*self.as_ptr()).descriptor })
                .map(|guard| ReclaimPermit(Some(guard)))
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
            header.shared = 0;
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
        // SAFETY: The collector owns this live header through the metadata access.
        let allocated = unsafe { (*self.as_ptr()).allocated };
        // SAFETY: The collector owns this live header through the metadata access.
        let descriptor = unsafe { (*self.as_ptr()).descriptor };
        (allocated && descriptor & 1 != 0)
            .then(|| Payload::from_raw((descriptor & !1) as *mut u8).unwrap())
    }

    pub(super) fn forward_to(self, target: Self) {
        debug_assert!(self.allocated() && target.allocated());
        debug_assert!(target.forwarded_to().is_none());
        // Ownership was transferred during destination initialization. Its
        // descriptor remains alive until all source metadata reads finish.
        // SAFETY: The collector owns this live header through the metadata access.
        unsafe {
            debug_assert!(!(*self.as_ptr()).descriptor_owned);
            (*self.as_ptr()).descriptor = target.payload().as_ptr() as usize | 1;
        }
    }

    pub(super) fn descriptor_address(self) -> usize {
        if let Some(target) = self.forwarded_to() {
            let target = Self::from_payload(target);
            // Destinations are never sources in the same minor cycle. No
            // forwarding-chain traversal or additional registry reference.
            debug_assert!(target.forwarded_to().is_none());
            // SAFETY: The collector owns this live header through the metadata access.
            unsafe { (*target.as_ptr()).descriptor }
        } else {
            // SAFETY: The collector owns this live header through the metadata access.
            unsafe { (*self.as_ptr()).descriptor }
        }
    }

    fn descriptor(self) -> willow_abi::GcLayoutDescriptor {
        let address = self.descriptor_address();
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

    /// Set the remembered byte if it was clear. Exactly one of any number of
    /// racing callers wins, which deduplicates lock-free edge logging.
    pub(super) fn try_claim_remembered(self) -> bool {
        // SAFETY: as in `remembered`.
        unsafe {
            AtomicU8::from_ptr(&raw mut (*self.as_ptr()).remembered)
                .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        }
    }

    /// Whether this object has been published to shared state.
    pub(crate) fn shared(self) -> bool {
        // SAFETY: `Object` refers to a live heap allocation; other threads
        // read the byte concurrently, so access it atomically.
        unsafe { AtomicU8::from_ptr(&raw mut (*self.as_ptr()).shared).load(Ordering::Relaxed) != 0 }
    }

    /// Mark this object shared. Called only by the publication routine of
    /// the task that owns the object (an unshared object is unreachable from
    /// other tasks), so a plain atomic store suffices; the byte is never
    /// cleared while the object is allocated. `promoted` records that the
    /// object was young when published (`SHARED_PROMOTED`): minor copies
    /// keep it, and its death is accounted as promoted-then-died.
    pub(crate) fn set_shared(self, promoted: bool) {
        let value = if promoted {
            SHARED_PROMOTED
        } else {
            SHARED_PUBLISHED
        };
        // SAFETY: as in `shared`.
        unsafe {
            AtomicU8::from_ptr(&raw mut (*self.as_ptr()).shared).store(value, Ordering::Relaxed);
        }
    }

    /// Whether this object was young when it was published.
    pub(crate) fn promoted_by_publication(self) -> bool {
        // SAFETY: as in `shared`.
        unsafe {
            AtomicU8::from_ptr(&raw mut (*self.as_ptr()).shared).load(Ordering::Relaxed)
                == SHARED_PROMOTED
        }
    }

    pub(super) fn set_generation(self, generation: u8) {
        // SAFETY: collection has exclusive access while mutators are stopped.
        unsafe {
            (*self.as_ptr()).generation = generation;
            (*self.as_ptr()).age = 0;
            // The generated TLAB flag word initializes remembered to zero.
            // In-place promotion starts with no entry in the old generation's
            // remembered set, so establish the matching flag here as well.
            (*self.as_ptr()).remembered = 0;
        }
    }
}

/// The header flag bytes precede the word-aligned descriptor, so a moved
/// header's flags are one aligned word (`Object::initialize_moved_at`).
const MOVED_FLAG_BYTES: usize = std::mem::offset_of!(GcHeader, descriptor);
const _: () = assert!(
    MOVED_FLAG_BYTES == std::mem::size_of::<u64>()
        && std::mem::align_of::<GcHeader>() >= std::mem::align_of::<u64>()
);

/// Values of the SHARED header byte (`willow_abi::gc_header::SHARED_OFFSET`):
/// zero while private; any nonzero value means shared.
const SHARED_PUBLISHED: u8 = 1;
const SHARED_PROMOTED: u8 = 2;

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

/// Whether `d` describes an async frame (its fingerprint says so).
pub(super) fn is_async_frame_layout(d: &willow_abi::GcLayoutDescriptor) -> bool {
    d.layout_id
        == willow_abi::gc_layout_id(
            willow_abi::GcObjectKind::AsyncFrame,
            (d.size as usize - std::mem::size_of::<GcHeader>()) as i64,
            d.type_id as i64,
            d.gc_ref_mask,
        )
}

#[cfg(test)]
mod move_tests {
    use super::*;
    use crate::gc::{GC_GENERATION_OLD, GC_GENERATION_YOUNG, GC_HEADER_SIZE};

    #[test]
    fn moving_header_transfers_interned_descriptor_once() {
        let _guard = crate::gc::isolated_runtime();
        let key = willow_abi::GcLayoutDescriptor {
            type_id: 789123,
            layout_id: 789123,
            gc_ref_mask: 0,
            size: GC_HEADER_SIZE as u64,
        };
        for count in [1, 100, 10000] {
            let before = crate::gc::layouts::references(key);
            let mut source_storage = MaybeUninit::<GcHeader>::uninit();
            // SAFETY: The fixture owns fresh, correctly sized and aligned header storage for
            // this descriptor.
            let source = unsafe {
                Object::initialize_at(
                    source_storage.as_mut_ptr().cast(),
                    GC_HEADER_SIZE,
                    key.type_id as u32,
                    key.layout_id,
                    0,
                    GC_GENERATION_YOUNG,
                )
            }
            .unwrap();
            assert_eq!(crate::gc::layouts::references(key), before + 1);
            assert!(
                // SAFETY: The fixture owns distinct aligned header storage and transfers the
                // live source descriptor to the destination before reclaiming the source.
                unsafe {
                    Object::initialize_moved_at(std::ptr::null_mut(), source, GC_GENERATION_YOUNG)
                }
                .is_none()
            );
            // SAFETY: The collector owns this live header through the metadata access.
            assert!(unsafe { (*source.as_ptr()).descriptor_owned });
            // SAFETY: The collector owns this live header through the metadata access.
            let descriptor = unsafe { (*source.as_ptr()).descriptor };
            let mut a = MaybeUninit::<GcHeader>::uninit();
            let mut b = MaybeUninit::<GcHeader>::uninit();
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
                        // SAFETY: The fixture owns distinct aligned header storage and
                        // transfers the live source descriptor to the destination before
                        // reclaiming the source.
                        unsafe { Object::initialize_moved_at(raw.cast(), current, generation) }.unwrap();
                    // SAFETY: The collector owns this live header through the metadata access.
                    assert_eq!(unsafe { (*next.as_ptr()).descriptor }, descriptor);
                    // SAFETY: The collector owns this live header through the metadata access.
                    assert!(unsafe { (*next.as_ptr()).descriptor_owned });
                    // SAFETY: The collector owns this live header through the metadata access.
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
            shared: 0,
            descriptor: (&descriptor as *const _) as usize,
        };
        let source = Object::from_raw(&mut header).unwrap();
        let mut target = MaybeUninit::<GcHeader>::uninit();
        let moved =
            // SAFETY: The fixture owns distinct aligned header storage and transfers the live
            // source descriptor to the destination before reclaiming the source.
            unsafe { Object::initialize_moved_at(target.as_mut_ptr().cast(), source, GC_GENERATION_OLD) }
                .unwrap();
        assert!(!moved.marked());
        assert!(!moved.remembered());
        // SAFETY: The collector owns this live header through the metadata access.
        assert_eq!(unsafe { (*moved.as_ptr()).age }, 0);
        // SAFETY: The collector owns this live header through the metadata access.
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

    /// The one-word flag store keeps the publication state and generation,
    /// and clears every other flag byte even over stale target bytes.
    #[test]
    fn moving_header_keeps_publication_state_over_stale_target_bytes() {
        let descriptor = willow_abi::GcLayoutDescriptor {
            type_id: 42,
            layout_id: 7,
            gc_ref_mask: 0,
            size: GC_HEADER_SIZE as u64,
        };
        for (shared, promoted) in [(0, None), (1, Some(false)), (2, Some(true))] {
            for generation in [GC_GENERATION_YOUNG, GC_GENERATION_OLD] {
                let mut header = GcHeader {
                    marked: true,
                    allocated: true,
                    generation: GC_GENERATION_YOUNG,
                    age: 3,
                    descriptor_owned: false,
                    remembered: 1,
                    shared: 0,
                    descriptor: (&descriptor as *const _) as usize,
                };
                let source = Object::from_raw(&mut header).unwrap();
                if let Some(promoted) = promoted {
                    source.set_shared(promoted);
                }
                assert_eq!(header.shared, shared);
                let source = Object::from_raw(&mut header).unwrap();
                let mut target = MaybeUninit::<GcHeader>::uninit();
                let raw_target = target.as_mut_ptr().cast::<u8>();
                // SAFETY: target owns exactly GC_HEADER_SIZE bytes with GcHeader alignment.
                // Keep the stale bytes untyped until initialize_moved_at initializes the header.
                unsafe { raw_target.write_bytes(0xff, GC_HEADER_SIZE) };
                // SAFETY: The fixture owns distinct aligned header storage and transfers the
                // live source descriptor to the destination before reclaiming the source.
                let moved =
                    unsafe { Object::initialize_moved_at(raw_target, source, generation) }.unwrap();
                // SAFETY: The collector owns this live header through the metadata access.
                let raw = unsafe { &*moved.as_ptr() };
                assert!(!raw.marked);
                assert!(raw.allocated);
                assert_eq!(raw.generation, generation);
                assert_eq!(raw.age, 0);
                assert!(!raw.descriptor_owned);
                assert_eq!(raw.remembered, 0);
                assert_eq!(raw.shared, shared);
                assert_eq!(moved.shared(), shared != 0);
                assert_eq!(moved.promoted_by_publication(), promoted == Some(true));
                assert_eq!(raw.descriptor, (&descriptor as *const _) as usize);
            }
        }
    }
}
