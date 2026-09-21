use std::ptr::NonNull;

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

    pub(super) fn reclaim_in_place(self) {
        // SAFETY: the closed mark epoch proved this payload unreachable;
        // its readers have quiesced and the heap mutex owns its metadata.
        unsafe {
            let size = self.size();
            let header = &mut *self.as_ptr();
            if header.allocated {
                if header.descriptor_owned {
                    super::layouts::release(header.descriptor);
                }
                header.descriptor = size;
                header.descriptor_owned = false;
                header.allocated = false;
            }
            header.marked = false;
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

    fn descriptor(self) -> willow_abi::GcLayoutDescriptor {
        // SAFETY: live headers own a registry reference or point to static
        // generated data; collection excludes readers before reclamation.
        unsafe { *((*self.as_ptr()).descriptor as *const willow_abi::GcLayoutDescriptor) }
    }

    pub(super) fn type_id(self) -> u32 {
        // SAFETY: `Object` refers to a live heap allocation.
        self.descriptor().type_id as u32
    }

    pub(super) fn generation(self) -> u8 {
        // SAFETY: `Object` refers to a live heap allocation.
        unsafe { (*self.as_ptr()).generation }
    }

    pub(super) fn set_generation(self, generation: u8) {
        // SAFETY: collection has exclusive access while mutators are stopped.
        unsafe {
            (*self.as_ptr()).generation = generation;
            (*self.as_ptr()).age = 0;
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
