// GC Runtime — non-moving old generation + copying young generation
//
// Object layout in memory:
//   [ GcHeader | payload bytes ... ]
//
// The GcHeader is immediately before the payload.  `willow_alloc_object`
// returns a pointer to the payload start, just like malloc.
//
// Root stack: a thread-local Vec of *mut *mut u8.  Each entry points to a
// stack slot that holds a GC-managed pointer.  Generated code pushes a slot
// on entry and pops it on exit.  The mark phase reads through each slot to
// reach the live object.
//
// Old objects live in non-moving regular/large regions. Region metadata owns
// allocation enumeration, storage, mark bits, free spans, and liveness accounting.
// Generated young objects live
// in nursery TLAB regions; directly rooted survivors retain that storage as
// pinned old regions.

mod address_index;
mod assist;
mod concurrent_bitmap;
mod coordinator;
mod epoch_index;
mod free_spans;
mod mark_closure;
mod mark_workers;
mod memory_control;
mod minor;
mod pacer;
mod root_handshake;
mod satb;
mod sweep;

use free_spans::FreeSpans;
use minor::minor_collect_internal;

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::thread::ThreadId;

pub use willow_abi::{GcObjectKind, GcStoreDestination};

// Inline and bitmap masks index mixed scalar/reference storage words.
// Shadow root arrays and custom trace callbacks retain their native layouts.
const GC_STORAGE_WORD_BYTES: usize =
    willow_abi::storage_word_bytes(std::mem::size_of::<usize>() as u32) as usize;

const GC_GENERATION_YOUNG: u8 = 0;
const GC_GENERATION_OLD: u8 = 1;
const GC_NURSERY_THRESHOLD_BYTES: usize = 256 * 1024;
const GC_CARD_SIZE: usize = 512;
const GC_OLD_REGION_SIZE: usize = 256 * 1024;
const GC_LARGE_OBJECT_THRESHOLD: usize = GC_OLD_REGION_SIZE / 2;
const GC_REGION_MARK_GRANULE: usize = willow_abi::tlab::MARK_GRANULE_BYTES as usize;

// ---------------------------------------------------------------------------
// Object header
// ---------------------------------------------------------------------------

/// Derive the current opaque layout fingerprint. The compiler uses the same
/// Stage-2 algorithm for generated allocations.
pub fn gc_layout_id(
    kind: GcObjectKind,
    payload_size: i64,
    runtime_type_id: i64,
    gc_ref_mask: u64,
) -> u64 {
    willow_abi::gc_layout_id(kind, payload_size, runtime_type_id, gc_ref_mask)
}

/// Rust-runtime allocation entry for a known object shape.
///
/// Runtime containers use this instead of choosing between legacy
/// `willow_alloc_typed`/`willow_alloc_object` calls themselves.
pub fn willow_alloc_with_layout(
    kind: GcObjectKind,
    type_id: u32,
    payload_size: i64,
    gc_ref_mask: u64,
) -> *mut u8 {
    let layout_id = gc_layout_id(kind, payload_size, type_id as i64, gc_ref_mask);
    willow_gc_alloc_layout(layout_id, type_id as i64, payload_size, gc_ref_mask)
}

/// Allocate and initialize one boxed enum variant from the shared,
/// type-instantiated ABI descriptor.
///
/// GC payload words are copied into stable local slots and rooted across the
/// allocation, so a moving minor collection rewrites the values this helper
/// later stores. This avoids the subtle stale-copy bug caused by rooting a
/// caller variable while passing its pre-collection pointer value by value.
/// The helper also owns tag/offset/mask interpretation and applies the barrier
/// for every slot described as [`willow_abi::SlotKind::GcRef`].
pub(crate) fn willow_alloc_enum_variant(
    type_id: u32,
    layout: willow_abi::EnumVariantLayout<'_>,
    payload_words: &[i64],
) -> *mut u8 {
    assert_eq!(
        payload_words.len(),
        layout.payload.word_count() as usize,
        "enum payload does not match its instantiated ABI layout"
    );
    let mut words = payload_words.to_vec();
    let mut rooted = 0usize;
    for (index, kind) in layout.payload.slots.iter().enumerate() {
        if matches!(kind, willow_abi::SlotKind::GcRef) {
            // SAFETY: `words` is not resized while the root is registered and
            // each i64 word is one pointer-sized Willow slot on supported
            // targets.
            willow_push_root(unsafe { words.as_mut_ptr().add(index).cast::<*mut u8>() });
            rooted += 1;
        }
    }

    let pointer_bytes = std::mem::size_of::<usize>() as u32;
    let payload_bytes = layout.payload_bytes(pointer_bytes) as i64;
    let value = willow_alloc_with_layout(
        GcObjectKind::Enum,
        type_id,
        payload_bytes,
        layout.gc_ref_mask(),
    );
    willow_pop_roots(rooted as i32);
    if value.is_null() {
        return value;
    }
    // SAFETY: the allocation has exactly the tag plus payload word count
    // described by `layout`; all writes are within that initialized payload.
    unsafe {
        *value.cast::<i64>() = i64::from(layout.tag);
        for (index, (&word, kind)) in words.iter().zip(layout.payload.slots.iter()).enumerate() {
            if matches!(kind, willow_abi::SlotKind::GcRef) {
                willow_gc_write_barrier(
                    value,
                    std::ptr::null_mut(),
                    word as *mut u8,
                    GcStoreDestination::EnumPayload as i64,
                );
            }
            if matches!(kind, willow_abi::SlotKind::GcRef) {
                store_gc_reference(value.cast::<i64>().add(1 + index).cast(), word as *mut u8);
            } else {
                *value.cast::<i64>().add(1 + index) = word;
            }
        }
    }
    value
}

#[repr(C)]
pub struct GcHeader {
    /// Mark bit used during mark phase.
    pub marked: bool,
    /// False after a dead object in a retained TLAB chunk has been finalized.
    pub allocated: bool,
    /// TLAB objects start young. Region-allocated and promoted objects are old
    /// and never move again.
    pub generation: u8,
    /// Reserved for survivor aging. Stage 4 promotes every minor survivor.
    pub age: u8,
    /// Runtime type identifier (0 = unknown/opaque for now).
    pub type_id: u32,
    /// Opaque compiler/runtime layout identifier. Stage 2 records it now so
    /// later TLAB, generational, and moving collectors can select layout-aware
    /// fast paths without changing the object ABI again.
    pub layout_id: u64,
    /// Bit mask for the first 64 pointer-sized payload slots that contain GC refs.
    pub gc_ref_mask: u64,
    /// Total allocation size in bytes (header + payload).
    pub size: usize,
    /// Reserved ABI word, always null. Allocation enumeration uses region metadata.
    pub next: *mut GcHeader,
}

/// Generated-code-facing TLS allocation state.
///
/// The compiler defines one zero-initialized TLS instance of this layout in
/// each Willow executable. The runtime receives its address only on the slow
/// path, registers the owning chunk, and may invalidate cursor/limit while the
/// mutator is stopped for collection.
#[repr(C)]
pub struct GcTlabState {
    cursor: AtomicUsize,
    limit: AtomicUsize,
    fast_allocations: AtomicU64,
    fast_allocated_bytes: AtomicU64,
    /// Stable pointer to this chunk's atomic object-start words. Published
    /// before cursor; generated allocation sets its bit after header writes.
    start_bits: AtomicUsize,
}

pub const GC_TLAB_STATE_SIZE: usize = std::mem::size_of::<GcTlabState>();
pub const GC_HEADER_SIZE: usize = std::mem::size_of::<GcHeader>();
pub const GC_TLAB_CHUNK_SIZE: usize = willow_abi::tlab::CHUNK_SIZE as usize;
const _: () = assert!(GC_TLAB_CHUNK_SIZE <= u16::MAX as usize + 1);
pub const GC_TLAB_MAX_OBJECT_SIZE: usize = 4 * 1024;

/// Raw allocation and pointer arithmetic boundary for the collector. The rest
/// of the GC works with `Object`/`Payload`/`RootSlot` and cannot directly
/// dereference a header or stack-slot pointer.
mod raw_heap {
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
                header.type_id = type_id;
                header.layout_id = layout_id;
                header.gc_ref_mask = gc_ref_mask;
                header.size = size;
                header.next = std::ptr::null_mut();
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
            // SAFETY: immutable allocation fields stay valid for live objects.
            // Read individual places, without borrowing the whole header while
            // a sweeper can clear its distinct transient mark byte.
            unsafe {
                let header = self.as_ptr();
                TraceMetadata {
                    type_id: (*header).type_id,
                    layout_id: (*header).layout_id,
                    gc_ref_mask: (*header).gc_ref_mask,
                    payload_size: (*header).size - std::mem::size_of::<GcHeader>(),
                }
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
                (*self.as_ptr()).allocated = false;
                (*self.as_ptr()).marked = false;
            }
        }

        pub(super) fn clear_mark(self) {
            // SAFETY: sweep has exclusive access under the heap lock.
            unsafe { (*self.as_ptr()).marked = false };
        }

        pub(super) fn size(self) -> usize {
            // SAFETY: `Object` refers to a live heap allocation.
            unsafe { (*self.as_ptr()).size }
        }

        pub(super) fn type_id(self) -> u32 {
            // SAFETY: `Object` refers to a live heap allocation.
            unsafe { (*self.as_ptr()).type_id }
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
            // its matching pop, and only its owning thread reads it.
            Payload::from_raw(unsafe { *self.0.as_ptr() })
        }
    }
}

use raw_heap::{Object as HeapObject, Payload as GcPayload, RootSlot};

// ---------------------------------------------------------------------------
// GC state
// ---------------------------------------------------------------------------

struct GcState {
    concurrent_cycle: Option<Arc<ConcurrentCycle>>,
    sweeping: Option<ThreadId>,
    satb: satb::SatbBuffers,
    /// Bump-allocation chunks. Active chunks are owned by one TLS state;
    /// collection retires them before walking their object headers.
    tlab_chunks: Vec<TlabChunk>,
    tlab_addresses: address_index::AddressIndex,
    /// Non-moving old-generation storage. Regular regions serve old/runtime
    /// allocations from a bump tail or region-local free spans. Large objects
    /// receive one dedicated region. Object addresses never change.
    old_regions: Vec<OldRegion>,
    old_addresses: address_index::AddressIndex,
    /// (largest available span, region index); excludes dedicated/full regions.
    old_region_candidates: BinaryHeap<(usize, usize)>,
    old_reserved_bytes: usize,
    /// Generated TLS states observed on allocation slow paths.
    tlab_states: HashMap<usize, TlabStateRecord>,
    tlab_owners: HashMap<ThreadId, HashSet<usize>>,
    /// Total bytes currently allocated (header + payload).
    allocated_bytes: usize,
    /// Trigger a collection when allocated_bytes exceeds this threshold.
    threshold_bytes: usize,
    /// Hard cap on GC-owned region reservations; native container storage is excluded.
    memory_limit_bytes: Option<usize>,
    soft_memory: memory_control::Controller,
    last_major_live_bytes: u64,
    last_major_mark_work: u64,
    pacer: pacer::Sampler,
    pacer_trigger: u64,
    /// Bytes occupied by allocated young objects in retired or active TLABs.
    young_allocated_bytes: usize,
    /// Trigger a minor collection at the next TLAB refill after this threshold.
    nursery_threshold_bytes: usize,
    /// Total objects allocated lifetime.
    total_allocs: u64,
    total_allocated_bytes: u64,
    released_bytes: u64,
    /// Total objects freed lifetime.
    total_frees: u64,
    /// TLAB tuning counters.
    tlab_fast_allocations: u64,
    tlab_slow_allocations: u64,
    tlab_refills: u64,
    tlab_large_allocations: u64,
    tlab_fast_allocated_bytes: u64,
    tlab_reserved_bytes: usize,
    /// Old objects that may contain at least one young reference. Owners are
    /// payload addresses and remain stable because the old generation does not
    /// move.
    remembered_set: HashSet<usize>,
    /// Sparse card table keyed by absolute old-heap card. Region bounds make
    /// each dirty card attributable to one old/pinned region without changing
    /// the Stage-4 barrier ABI.
    dirty_cards: HashSet<usize>,
    write_barrier_hits: u64,
    minor_collections: u64,
    promoted_objects: u64,
    promoted_bytes: u64,
    moved_objects: u64,
    old_region_allocations: u64,
    old_region_reuses: u64,
    old_regions_released: u64,
    major_collections: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionKind {
    Nursery,
    Old,
    LargeObject,
    Pinned,
}

struct RegionMarkBitmap {
    bits: Arc<concurrent_bitmap::ConcurrentMarkBits>,
}

impl RegionMarkBitmap {
    fn new(capacity: usize) -> Self {
        Self {
            bits: Arc::new(concurrent_bitmap::ConcurrentMarkBits::new(
                capacity.div_ceil(GC_REGION_MARK_GRANULE),
            )),
        }
    }
    fn clear(&mut self) {
        self.bits.clear();
    }
    fn mark(&mut self, offset: usize) {
        self.bits.set(offset / GC_REGION_MARK_GRANULE);
    }
    fn is_marked(&self, offset: usize) -> bool {
        self.bits.contains(offset / GC_REGION_MARK_GRANULE)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RegionFreeSpan {
    offset: usize,
    size: usize,
}

/// Metadata for one regular or large-object old-generation region.
///
/// Invariants:
/// - `[base, base + capacity)` is one allocator-owned, header-aligned block.
/// - `used <= capacity`; the unallocated bump tail is `[used, capacity)`.
/// - `allocations` maps object-header offsets to aligned physical spans.
/// - free spans are disjoint holes below `used` and never overlap allocations.
/// - `live_bytes` is the sum of logical header+payload sizes for allocations.
/// - mark bits identify object starts retained by the latest major mark/sweep.
struct OldRegion {
    base: *mut u8,
    capacity: usize,
    used: usize,
    kind: RegionKind,
    live_bytes: usize,
    allocations: BTreeMap<usize, usize>,
    free_spans: FreeSpans,
    largest_free_span: usize,
    sweep_pending: bool,
    sweep_quarantined: bool,
    #[cfg(test)]
    allocation_attempts: usize,
    mark_bitmap: RegionMarkBitmap,
    concurrent_marks: Arc<concurrent_bitmap::ConcurrentMarkBits>,
}

struct TlabChunk {
    base: *mut u8,
    capacity: usize,
    /// Allocated prefix in bytes. For an active chunk this is refreshed from
    /// its owner's atomic cursor when the TLAB is retired.
    used: usize,
    owner_state: Option<usize>,
    kind: RegionKind,
    live_bytes: usize,
    mark_bitmap: RegionMarkBitmap,
    concurrent_marks: Arc<concurrent_bitmap::ConcurrentMarkBits>,
    /// All physical headers in a retired chunk, sorted by offset. Reclaimed
    /// headers remain valid until chunk release; lookups check allocated().
    header_offsets: Vec<u16>,
}

struct TlabStateRecord {
    address: usize,
    owner: ThreadId,
    current_chunk: Option<usize>,
    observed_fast_allocations: u64,
    observed_fast_allocated_bytes: u64,
    assist_observed_fast_bytes: u64,
}

#[cfg(test)]
impl RegionMarkBitmap {
    fn unmark(&mut self, offset: usize) {
        self.bits.unset(offset / GC_REGION_MARK_GRANULE);
    }
}

impl OldRegion {
    fn new(kind: RegionKind, capacity: usize) -> Option<Self> {
        debug_assert!(matches!(kind, RegionKind::Old | RegionKind::LargeObject));
        let layout = Layout::from_size_align(capacity, std::mem::align_of::<GcHeader>()).ok()?;
        // SAFETY: `layout` is nonzero, aligned, and owned by the returned region.
        let base = unsafe { allocate_region_storage(layout) };
        if base.is_null() {
            return None;
        }
        let bitmap_capacity = if kind == RegionKind::LargeObject {
            GC_REGION_MARK_GRANULE
        } else {
            capacity
        };
        Some(Self {
            base,
            capacity,
            used: 0,
            kind,
            live_bytes: 0,
            allocations: BTreeMap::new(),
            free_spans: FreeSpans::default(),
            largest_free_span: 0,
            sweep_pending: false,
            sweep_quarantined: false,
            #[cfg(test)]
            allocation_attempts: 0,
            mark_bitmap: RegionMarkBitmap::new(bitmap_capacity),
            concurrent_marks: Arc::new(concurrent_bitmap::ConcurrentMarkBits::new(
                bitmap_capacity.div_ceil(GC_REGION_MARK_GRANULE),
            )),
        })
    }

    fn start(&self) -> usize {
        self.base as usize
    }

    fn end(&self) -> usize {
        self.start().saturating_add(self.capacity)
    }

    fn contains(&self, address: usize) -> bool {
        address >= self.start() && address < self.end()
    }

    fn allocate_object(
        &mut self,
        type_id: u32,
        layout_id: u64,
        gc_ref_mask: u64,
        payload_size: usize,
    ) -> Option<(HeapObject, bool)> {
        assert!(
            !self.sweep_quarantined,
            "reclaimed spans are not reusable before sweep completion"
        );
        #[cfg(test)]
        {
            self.allocation_attempts += 1;
        }
        let total_size = GC_HEADER_SIZE.checked_add(payload_size)?;
        let span_size = total_size.checked_next_multiple_of(GC_REGION_MARK_GRANULE)?;
        // The sweep-built index preserves address-ordered first fit without
        // scanning holes or shifting the remaining spans after every reuse.
        let mut reused = false;
        let offset = if let Some(offset) = self.free_spans.take(span_size) {
            reused = true;
            offset
        } else {
            let end = self.used.checked_add(span_size)?;
            if end > self.capacity {
                return None;
            }
            let offset = self.used;
            self.used = end;
            offset
        };

        self.largest_free_span = self.free_spans.largest();

        // SAFETY: the chosen span is exclusively owned by this allocation.
        let raw = unsafe { self.base.add(offset) };
        unsafe { std::ptr::write_bytes(raw, 0, span_size) };
        let object = HeapObject::initialize_at(
            raw,
            total_size,
            type_id,
            layout_id,
            gc_ref_mask,
            GC_GENERATION_OLD,
        )?;
        self.allocations.insert(offset, span_size);
        self.live_bytes = self.live_bytes.saturating_add(total_size);
        // Header initialization and black color precede publishing the start bit.
        // An epoch reader that observes the start may safely read immutable metadata.
        if GC_MARK_PHASE.load(Ordering::Acquire) != 0 {
            self.concurrent_marks.set(offset / GC_REGION_MARK_GRANULE);
        }
        self.mark_bitmap.mark(offset);
        if self.sweep_pending {
            // Marking is closed, but this region has not yet been swept. New
            // allocations must survive its pending visit, including reused holes.
            object.begin_trace();
        }
        Some((object, reused))
    }

    fn object_for_address(&self, address: usize, interior: bool) -> Option<HeapObject> {
        if !self.contains(address) && (interior || address != self.end()) {
            return None;
        }
        let relative = address - self.start();
        let (&offset, _) = self.allocations.range(..=relative).next_back()?;
        // SAFETY: allocation metadata contains a live object at this offset.
        let object = HeapObject::from_raw(unsafe { self.base.add(offset) }.cast())?;
        let payload = object.payload().as_ptr() as usize;
        let payload_end = object.as_ptr() as usize + object.size();
        ((!interior && payload == address)
            || (interior && address >= payload && address < payload_end))
            .then_some(object)
    }

    #[cfg(test)]
    fn record_marked_object(&mut self, object: HeapObject) {
        let offset = object.as_ptr() as usize - self.start();
        self.mark_bitmap.mark(offset);
    }

    #[cfg(test)]
    fn release_object(&mut self, object: HeapObject) {
        let offset = object.as_ptr() as usize - self.start();
        let Some(span_size) = self.allocations.remove(&offset) else {
            panic!(
                "willow gc: old object 0x{:x} is missing region allocation metadata",
                object.as_ptr() as usize
            );
        };
        self.live_bytes = self.live_bytes.saturating_sub(object.size());
        self.mark_bitmap.unmark(offset);
        self.free_spans.push(RegionFreeSpan {
            offset,
            size: span_size,
        });
        self.coalesce_free_spans();
    }

    #[cfg(test)]
    fn coalesce_free_spans(&mut self) {
        let mut spans: Vec<_> = self.free_spans.iter().copied().collect();
        spans.sort_unstable_by_key(|span| span.offset);
        let mut merged: Vec<RegionFreeSpan> = Vec::with_capacity(spans.len());
        for span in spans {
            if let Some(last) = merged.last_mut()
                && last.offset + last.size == span.offset
            {
                last.size += span.size;
                continue;
            }
            merged.push(span);
        }
        while merged
            .last()
            .is_some_and(|span| span.offset + span.size == self.used)
        {
            self.used = merged.pop().expect("tail span exists").offset;
        }
        self.largest_free_span = merged.iter().map(|span| span.size).max().unwrap_or(0);
        self.free_spans = FreeSpans::from(merged);
    }

    fn available_span(&self) -> usize {
        self.largest_free_span.max(self.capacity - self.used)
    }

    fn fragmentation_bytes(&self) -> usize {
        self.used.saturating_sub(self.live_bytes)
    }
}

#[cfg(test)]
thread_local! {
    static STORAGE_FAILURES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static STORAGE_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Allocate a validated, nonzero region layout. The test hook injects actual
/// storage failures without affecting Rust metadata allocations or other threads.
unsafe fn allocate_region_storage(layout: Layout) -> *mut u8 {
    #[cfg(test)]
    {
        STORAGE_ATTEMPTS.set(STORAGE_ATTEMPTS.get() + 1);
        if STORAGE_FAILURES.get() != 0 {
            STORAGE_FAILURES.set(STORAGE_FAILURES.get() - 1);
            return std::ptr::null_mut();
        }
    }
    unsafe { alloc_zeroed(layout) }
}

impl Drop for OldRegion {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.capacity, std::mem::align_of::<GcHeader>())
            .expect("old-region allocation layout remains valid");
        // SAFETY: each region owns one block and `Drop` runs exactly once.
        unsafe { dealloc(self.base, layout) };
    }
}

fn gc_memory_limit_from_env() -> Option<usize> {
    let value = std::env::var("WILLOW_GC_MEMORY_LIMIT").ok()?;
    Some(
        value
            .parse::<usize>()
            .ok()
            .filter(|value| *value > 0)
            .expect("WILLOW_GC_MEMORY_LIMIT must be a positive byte count"),
    )
}

fn can_reserve(state: &GcState, additional: usize) -> bool {
    state.memory_limit_bytes.is_none_or(|limit| {
        state
            .old_reserved_bytes
            .checked_add(state.tlab_reserved_bytes)
            .and_then(|total| total.checked_add(additional))
            .is_some_and(|total| total <= limit)
    })
}

#[cfg(test)]
static SWEEP_BUDGET_WAITING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static ELECTION_BUDGET_WAITING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Give reclamation one opportunity before treating reservation pressure as
/// exhaustion. Waiting mutators cooperate with remark and wait through sweep.
fn collect_for_budget() {
    let mut observed_or_requested_cycle = false;
    loop {
        let (collector, _sweeping) = {
            let state = runtime().heap.lock().unwrap();
            let collector = state
                .concurrent_cycle
                .as_ref()
                .map(|cycle| cycle.collector)
                .or(state.sweeping);
            (collector, state.sweeping.is_some())
        };
        match collector {
            Some(collector) if collector == std::thread::current().id() => return,
            Some(_) => {
                #[cfg(test)]
                if _sweeping {
                    SWEEP_BUDGET_WAITING.store(true, Ordering::Release);
                }
                observed_or_requested_cycle = true;
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            None if observed_or_requested_cycle => {
                // An elected collector can own serialization before publishing
                // its initial mark state. Likewise it may be finishing accounting
                // after clearing sweep state. Neither gap is an idle heap.
                let busy = match runtime().collect_lock.try_lock() {
                    Ok(_) | Err(std::sync::TryLockError::Poisoned(_)) => false,
                    Err(std::sync::TryLockError::WouldBlock) => true,
                };
                if !busy {
                    return;
                }
                #[cfg(test)]
                ELECTION_BUDGET_WAITING.store(true, Ordering::Release);
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            None => {
                observed_or_requested_cycle = true;
                collect_internal();
                // Election may only have joined an initial safepoint. Recheck
                // the published cycle and wait through its sweep before retry.
            }
        }
    }
}

fn allocation_failure(state: &GcState) -> *mut u8 {
    if let Some(limit) = state.memory_limit_bytes {
        eprintln!(
            "runtime fatal: GC memory limit exceeded ({limit} bytes of managed region reservations)"
        );
        std::process::exit(1);
    }
    std::ptr::null_mut()
}

fn major_trigger(state: &GcState) -> usize {
    let soft_trigger = state
        .soft_memory
        .decide(memory_inputs(state))
        .trigger
        .min(usize::MAX as u64) as usize;
    let soft_trigger = if state.pacer.enabled() {
        soft_trigger.min(state.pacer_trigger.min(usize::MAX as u64) as usize)
    } else {
        soft_trigger
    };
    state.memory_limit_bytes.map_or(soft_trigger, |limit| {
        soft_trigger.min((limit / 4).saturating_mul(3).max(1))
    })
}

fn pacer_inputs(state: &GcState) -> pacer::Inputs {
    pacer::Inputs {
        live: state.last_major_live_bytes,
        previous_goal: state.threshold_bytes as u64,
        expected_work: state.last_major_mark_work.max(state.last_major_live_bytes),
        allocation_per_second: 0,
        mark_per_cpu_second: None,
        // Until fractional worker duty is actuated, assume only one CPU of
        // progress. More workers may finish early, never justify a later start.
        cpu_capacity: 1,
        active: state.concurrent_cycle.is_some() || state.sweeping.is_some(),
    }
}

fn memory_inputs(state: &GcState) -> memory_control::Inputs {
    memory_control::Inputs {
        unlimited_goal: state.threshold_bytes as u64,
        live: state.last_major_live_bytes,
        occupied: state.allocated_bytes as u64,
        committed: state
            .old_reserved_bytes
            .saturating_add(state.tlab_reserved_bytes) as u64,
        allocated_total: state.total_allocated_bytes,
        // Our process provider reports RSS, not a compatible commit scope.
        non_heap_commit: None,
    }
}

impl Default for GcState {
    fn default() -> Self {
        Self {
            concurrent_cycle: None,
            sweeping: None,
            satb: satb::SatbBuffers::default(),
            tlab_chunks: Vec::new(),
            tlab_addresses: address_index::AddressIndex::default(),
            old_regions: Vec::new(),
            old_addresses: address_index::AddressIndex::default(),
            old_region_candidates: BinaryHeap::new(),
            old_reserved_bytes: 0,
            tlab_states: HashMap::new(),
            tlab_owners: HashMap::new(),
            allocated_bytes: 0,
            threshold_bytes: 1024 * 1024,
            memory_limit_bytes: gc_memory_limit_from_env(),
            soft_memory: memory_control::Controller::from_env(),
            last_major_live_bytes: 0,
            last_major_mark_work: 0,
            pacer: pacer::Sampler::default(),
            pacer_trigger: 1024 * 1024,
            young_allocated_bytes: 0,
            nursery_threshold_bytes: GC_NURSERY_THRESHOLD_BYTES,
            total_allocs: 0,
            total_allocated_bytes: 0,
            released_bytes: 0,
            total_frees: 0,
            tlab_fast_allocations: 0,
            tlab_slow_allocations: 0,
            tlab_refills: 0,
            tlab_large_allocations: 0,
            tlab_fast_allocated_bytes: 0,
            tlab_reserved_bytes: 0,
            remembered_set: HashSet::new(),
            dirty_cards: HashSet::new(),
            write_barrier_hits: 0,
            minor_collections: 0,
            promoted_objects: 0,
            promoted_bytes: 0,
            moved_objects: 0,
            old_region_allocations: 0,
            old_region_reuses: 0,
            old_regions_released: 0,
            major_collections: 0,
        }
    }
}

// SAFETY: region ownership and allocation/sweep/reset mutations are protected
// by `GcRuntime::heap`. Concurrent marking holds an epoch reader and observes
// initialized headers through atomic start bits; relocation is serialized.
unsafe impl Send for GcState {}

#[cfg(test)]
static RUNTIME_TEST_LOCK: Mutex<()> = Mutex::new(());

// Root stack — per-thread explicit shadow stack.
std::thread_local! {
    static ROOT_STACK: std::cell::RefCell<Vec<*mut *mut u8>> =
        const { std::cell::RefCell::new(Vec::new()) };
    // Only this mutator changes its shadow stack. Keep the hot depth query
    // independent of the Vec's destructor-bearing TLS and RefCell borrow.
    // Stack parking/resumption must publish the transferred depth as well.
    static ROOT_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

// ---------------------------------------------------------------------------
// Multi-mutator coordination: registration + stop-the-world safepoints
// (willow-6fv.5.6).
//
// The single-mutator runtime keeps using the thread-local ROOT_STACK directly.
// When more than one mutator thread is registered (for example, a
// `WILLOW_WORKERS=N` worker pool), a collection stops the world: it asks every
// other registered mutator to reach a safepoint, where the mutator publishes a
// SNAPSHOT of its own root pointers under `COORD`'s lock and parks. The
// collector then scans every registered mutator's published roots. Each thread
// only ever reads its OWN thread-local stack, so there is no cross-thread
// TLS/RefCell aliasing — the shared state is just `Vec<usize>` address snapshots
// behind a mutex.
//
// Major collection uses an independent root-publication handshake initially
// and this parking protocol for final remark. Tracing reads atomic references
// and concurrent container snapshots while SATB/insertion barriers retain edges.
#[derive(Default)]
struct GcCoord {
    /// Registered mutator threads → their most recently published root snapshot
    /// (object payload addresses). Empty vec until the thread parks at a safepoint.
    mutators: HashMap<ThreadId, Vec<usize>>,
    /// A collector has requested all mutators to reach a safepoint and park.
    stop_requested: bool,
    /// Mutators currently parked at a safepoint.
    parked: HashSet<ThreadId>,
    handshake: Option<root_handshake::Handshake>,
}

/// Reference-counted roots owned by runtime structures. Collection takes a
/// distinct-object snapshot; repeated owners retain one entry until the last
/// owner releases it. Keep registry locking behind this boundary.
#[derive(Default)]
struct RuntimeRootSet {
    roots: Mutex<HashMap<usize, usize>>,
}

impl RuntimeRootSet {
    fn add(&self, object: *mut u8) {
        if object.is_null() {
            return;
        }
        let mut roots = self.roots.lock().unwrap();
        *roots.entry(object as usize).or_insert(0) += 1;
    }

    fn remove(&self, object: *mut u8) {
        if object.is_null() {
            return;
        }
        let root = object as usize;
        let mut roots = self.roots.lock().unwrap();
        if let Some(count) = roots.get_mut(&root) {
            if *count > 1 {
                *count -= 1;
            } else {
                roots.remove(&root);
            }
        }
    }

    fn snapshot(&self) -> Vec<*mut u8> {
        self.roots
            .lock()
            .unwrap()
            .keys()
            .map(|&root| root as *mut u8)
            .collect()
    }

    fn len(&self) -> usize {
        self.roots.lock().unwrap().len()
    }

    fn clear(&self) {
        self.roots.lock().unwrap().clear();
    }
}

/// Process-wide GC services. Keeping the heap, roots, registries, and STW
/// coordinator behind one explicit owner makes lock ordering visible and keeps
/// runtime entry points from reaching into unrelated globals.
struct GcRuntime {
    heap: Mutex<GcState>,
    write_barrier_calls: AtomicU64,
    /// Conservative nursery-presence gate: set before the first TLAB can
    /// publish an object, and cleared only by the quiescent runtime reset.
    /// Keeping it set after collection avoids racing a concurrent refill.
    tlab_ever_allocated: std::sync::atomic::AtomicBool,
    /// Always acquired before `heap`; marking temporarily releases the heap
    /// lock while registered trace callbacks run.
    collect_lock: Mutex<()>,
    root_stack_owner: Mutex<Option<ThreadId>>,
    skipped_foreign_owner_collections: std::sync::atomic::AtomicU64,
    runtime_roots: RuntimeRootSet,
    parked_stack_roots: Mutex<HashMap<u64, Vec<usize>>>,
    next_parked_stack: AtomicU64,
    coord: (Mutex<GcCoord>, Condvar),
    /// Lock-free fast-path mirror of `GcCoord::stop_requested`.
    stop_requested: std::sync::atomic::AtomicBool,
    poll_requested: std::sync::atomic::AtomicBool,
    trace_registry: Mutex<HashMap<u32, TraceFn>>,
    concurrent_trace_registry: Mutex<HashMap<u32, ConcurrentTraceFn>>,
    concurrent_slice_registry: Mutex<HashMap<u32, ConcurrentTraceSliceFn>>,
    drop_registry: Mutex<HashMap<u32, DropFn>>,
    /// Advances only when registered hooks are invalidated. Runtime container
    /// types use this to cache per-generation registration without taking the
    /// registry mutex on every allocation.
    registry_generation: std::sync::atomic::AtomicU64,
}

impl Default for GcRuntime {
    fn default() -> Self {
        Self {
            heap: Mutex::new(GcState::default()),
            write_barrier_calls: AtomicU64::new(0),
            tlab_ever_allocated: std::sync::atomic::AtomicBool::new(false),
            collect_lock: Mutex::new(()),
            root_stack_owner: Mutex::new(None),
            skipped_foreign_owner_collections: std::sync::atomic::AtomicU64::new(0),
            runtime_roots: RuntimeRootSet::default(),
            parked_stack_roots: Mutex::new(HashMap::new()),
            next_parked_stack: AtomicU64::new(1),
            coord: (Mutex::new(GcCoord::default()), Condvar::new()),
            stop_requested: std::sync::atomic::AtomicBool::new(false),
            poll_requested: std::sync::atomic::AtomicBool::new(false),
            trace_registry: Mutex::new(HashMap::new()),
            concurrent_trace_registry: Mutex::new(HashMap::new()),
            concurrent_slice_registry: Mutex::new(HashMap::new()),
            drop_registry: Mutex::new(HashMap::new()),
            registry_generation: std::sync::atomic::AtomicU64::new(1),
        }
    }
}

static GC_RUNTIME: LazyLock<GcRuntime> = LazyLock::new(GcRuntime::default);

fn runtime() -> &'static GcRuntime {
    &GC_RUNTIME
}

/// Snapshot this thread's live root object pointers (as addresses) from its
/// thread-local stack. Reads only this thread's TLS, so it is race-free.
fn snapshot_local_roots() -> Vec<usize> {
    ROOT_STACK.with(|rs| {
        rs.borrow()
            .iter()
            .filter(|&&slot| !slot.is_null())
            .filter_map(|&slot| {
                RootSlot::from_raw(slot)
                    .and_then(RootSlot::load)
                    .map(|payload| payload.as_ptr() as usize)
            })
            .collect()
    })
}

/// True when at least one mutator OTHER than the current thread is registered,
/// so a collection must stop the world rather than scan only the local stack.
#[cfg(test)]
fn multi_mutator_active() -> bool {
    let current = std::thread::current().id();
    let (lock, _) = &runtime().coord;
    lock.lock()
        .unwrap()
        .mutators
        .keys()
        .any(|&id| id != current)
}

/// Register the current thread as a GC mutator (willow-6fv.5.6). A mutator that
/// can allocate or hold GC references on worker threads must register so a
/// stop-the-world collection scans its roots.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_register_mutator() {
    let (lock, _) = &runtime().coord;
    {
        let mut coord = lock.lock().unwrap();
        let id = std::thread::current().id();
        coord.mutators.entry(id).or_default();
        if let Some(handshake) = coord.handshake.as_mut() {
            handshake.pending.insert(id);
        }
    }
    // Registration can race with a collection that has already requested a
    // stop. Join that stop before executing any mutator work so the collector
    // never waits on a newly registered thread that has not published roots.
    willow_gc_safepoint();
}

/// Unregister the current thread as a GC mutator. Must be called before the
/// thread stops allocating/holding GC references (e.g. at worker shutdown).
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_unregister_mutator() {
    assist::reset();
    flush_satb_current(true);
    let id = std::thread::current().id();
    let (lock, cv) = &runtime().coord;
    let mut coord = lock.lock().unwrap();
    root_handshake::publish_current(&mut coord);
    coord.mutators.remove(&id);
    coord.parked.remove(&id);
    // A legacy owner can register, then empty its stack while registered.
    // Retire that ownership before releasing coord, using coord -> owner order.
    // Nonempty legacy stacks must still block foreign collections.
    clear_root_stack_owner_if_empty();
    // Keep the coordination lock while retiring TLS state: collectors acquire
    // coord then heap, so this preserves lock ordering and prevents a collector
    // from missing this thread while it still mutates its chunk metadata.
    retire_tlabs_for_thread(id);
    // A collector may be waiting for this thread to park; it no longer needs to.
    cv.notify_all();
}

/// Process-lifetime address of the GC poll gate for generated atomic byte loads.
/// A set gate requests either independent root publication or a global stop.
/// Generated code must reload this flag at every poll, with acquire ordering or
/// stronger; caching the flag value would prevent a collector from stopping it.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_stop_flag() -> *const u8 {
    runtime().poll_requested.as_ptr().cast::<u8>()
}

/// A cooperative GC safepoint (willow-6fv.5.6). Cheap when no collection is
/// pending. When a stop-the-world collection is in progress, the calling mutator
/// publishes a snapshot of its roots and parks here until the collector resumes
/// it. The scheduler polls this between task polls; future compiler-inserted
/// safepoints can add loop-backedge coverage.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_safepoint() {
    flush_satb_current(false);
    // Hot-path: a single relaxed atomic load. No collection pending → return
    // immediately without touching the coordination lock.
    if !runtime()
        .poll_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return;
    }
    // Past the fast path this thread is about to park. Parking while holding a
    // mark-queue lock deadlocks the collector (willow-6fv.5.6.1): the world
    // cannot restart until this thread runs, and this thread cannot run until
    // the world restarts. Checking here makes that rule enforced rather than
    // merely documented, and costs one TLS read on the already-slow path.
    crate::gc_mark_queue::assert_no_queue_lock_held("willow_gc_safepoint");
    let (lock, cv) = &runtime().coord;
    let mut coord = lock.lock().unwrap();
    root_handshake::publish_current(&mut coord);
    if !coord.stop_requested {
        return;
    }
    let id = std::thread::current().id();
    // Publish our roots so the collector can scan them while we are parked, then
    // park until the world resumes.
    let roots = snapshot_local_roots();
    if let Some(slot) = coord.mutators.get_mut(&id) {
        *slot = roots;
    }
    coord.parked.insert(id);
    cv.notify_all(); // wake the collector waiting for everyone to park
    while coord.stop_requested {
        coord = cv.wait(coord).unwrap();
    }
    coord.parked.remove(&id);
}

/// Run `collect` with the world stopped: request a safepoint, wait until every
/// other registered mutator has parked, then run `collect` (which scans all
/// published roots), then resume the world (willow-6fv.5.6).
fn with_stw<R>(
    reason: crate::gc_telemetry::stops::StopReason,
    collect: impl FnOnce(&GcCoord, &mut crate::gc_telemetry::stops::StopWorkV2) -> R,
) -> R {
    let mut measurement = crate::gc_telemetry::stops::StopMeasurement::begin(reason);
    let (lock, cv) = &runtime().coord;
    let me = std::thread::current().id();
    // Publish the stop request on the lock-free gate first so mutators on the
    // hot path observe it at their next safepoint.
    runtime()
        .stop_requested
        .store(true, std::sync::atomic::Ordering::Release);
    runtime().poll_requested.store(true, Ordering::Release);
    let mut coord = lock.lock().unwrap_or_else(|poison| poison.into_inner());
    coord.stop_requested = true;
    loop {
        let all_parked = coord
            .mutators
            .keys()
            .filter(|&&id| id != me)
            .all(|id| coord.parked.contains(id));
        if all_parked {
            break;
        }
        coord = cv.wait(coord).unwrap_or_else(|poison| poison.into_inner());
    }
    // A collection can panic: the debug pointer validation aborts the cycle on
    // a corrupt root, and callers catch that. The world has to be resumed
    // either way — an unwind that leaves `stop_requested` set parks every other
    // mutator forever, and one that unwinds out of the guard poisons the
    // registry, taking every later collection down with it (willow-v6k0).
    measurement.stopped();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        collect(&coord, measurement.work())
    }));
    coord.stop_requested = false;
    runtime()
        .stop_requested
        .store(false, std::sync::atomic::Ordering::Release);
    runtime().poll_requested.store(false, Ordering::Release);
    cv.notify_all();
    drop(coord);
    measurement.finish(result.is_err());
    match result {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// All roots to scan under stop-the-world: this (collector) thread's LIVE
/// thread-local roots plus every OTHER registered mutator's published snapshot.
fn all_registered_stack_roots(coord: &GcCoord) -> Vec<*mut u8> {
    let me = std::thread::current().id();
    let mut roots: Vec<*mut u8> = snapshot_local_roots()
        .into_iter()
        .map(|a| a as *mut u8)
        .collect();
    for (&id, published) in coord.mutators.iter() {
        if id == me {
            continue; // self uses the live snapshot above, not a stale publish
        }
        roots.extend(published.iter().map(|&a| a as *mut u8));
    }
    roots
}

/// Trace the GC graph from `worklist` (the marked-set fixpoint via the TypeInfo
/// registry + gc_ref_mask interior pointers). Shared by the single-mutator and
/// stop-the-world collection paths.
/// An epoch owns views of persistent region metadata. References are read from
/// the live heap while mutators execute; each generated allocation publishes
/// its start bit after initialization. New old allocations publish black.
struct ConcurrentCycle {
    collector: ThreadId,
    assist_epoch: u64,
    assist_rate: u64,
    objects: epoch_index::EpochIndex,
    legacy_traces: HashSet<u32>,
    traces: HashMap<u32, ConcurrentTraceFn>,
    slices: HashMap<u32, ConcurrentTraceSliceFn>,
    worker_failed: std::sync::atomic::AtomicBool,
    closing: std::sync::atomic::AtomicBool,
    tracing_enabled: std::sync::atomic::AtomicBool,
    active_drains: AtomicUsize,
    deferred: Mutex<Vec<usize>>,
    unindexed: Mutex<HashSet<usize>>,
    queue: Arc<crate::gc_mark_queue::MarkWorkQueue>,
    work: Mutex<crate::gc_telemetry::MarkWork>,
}

// Publish accounting and legacy/unindexed exceptions once per bounded drain,
// including unwind. Ordinary object tracing takes no cycle-global mutex.
struct MarkBatch<'a> {
    cycle: &'a ConcurrentCycle,
    work: crate::gc_telemetry::MarkWork,
    deferred: Vec<usize>,
    unindexed: HashSet<usize>,
    cpu_start: Option<u64>,
}
impl Drop for MarkBatch<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // Publish failure before the reader can disappear. The outer catch
            // may run after queue retirement makes outstanding reach zero.
            self.cycle.worker_failed.store(true, Ordering::Release);
        }
        let cpu = self
            .cpu_start
            .zip(crate::gc_telemetry::workers::thread_cpu_ns())
            .and_then(|(start, end)| end.checked_sub(start));
        {
            let mut total = self.cycle.work.lock().unwrap();
            total.marked_bytes = total.marked_bytes.saturating_add(self.work.marked_bytes);
            total.scanned_bytes = total.scanned_bytes.saturating_add(self.work.scanned_bytes);
            total.descriptor_bytes = total
                .descriptor_bytes
                .saturating_add(self.work.descriptor_bytes);
            if let Some(cpu) = cpu.and_then(|cpu| total.cpu_ns.checked_add(cpu)) {
                total.cpu_ns = cpu;
            } else {
                total.cpu_incomplete = true;
            }
        }
        if !self.deferred.is_empty() {
            self.cycle
                .deferred
                .lock()
                .unwrap()
                .append(&mut self.deferred);
        }
        if !self.unindexed.is_empty() {
            self.cycle
                .unindexed
                .lock()
                .unwrap()
                .extend(self.unindexed.drain());
        }
        self.cycle.active_drains.fetch_sub(1, Ordering::Release);
    }
}

impl ConcurrentCycle {
    #[cfg(test)]
    fn new(
        objects: impl IntoIterator<Item = (usize, raw_heap::TraceMetadata)>,
        legacy_traces: HashSet<u32>,
        traces: HashMap<u32, ConcurrentTraceFn>,
        roots: usize,
    ) -> Self {
        Self::with_index(
            epoch_index::EpochIndex::synthetic(objects),
            legacy_traces,
            traces,
            roots,
        )
    }

    fn with_index(
        objects: epoch_index::EpochIndex,
        legacy_traces: HashSet<u32>,
        traces: HashMap<u32, ConcurrentTraceFn>,
        roots: usize,
    ) -> Self {
        // One slot per allowed background worker, plus the collector. Assists
        // beyond these slots use the queue's existing slotless consumer path.
        let queue = crate::gc_mark_queue::MarkWorkQueue::new(mark_workers::configured_count() + 1);
        queue.begin_epoch();
        static ASSIST_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let assist_epoch = ASSIST_EPOCH
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |epoch| {
                epoch.checked_add(1)
            })
            .expect("assist epoch exhausted");
        Self {
            collector: std::thread::current().id(),
            assist_epoch,
            assist_rate: assist::SCALE,
            objects,
            worker_failed: std::sync::atomic::AtomicBool::new(false),
            closing: std::sync::atomic::AtomicBool::new(false),
            tracing_enabled: std::sync::atomic::AtomicBool::new(true),
            active_drains: AtomicUsize::new(0),
            legacy_traces,
            traces,
            slices: HashMap::new(),
            deferred: Mutex::new(Vec::new()),
            unindexed: Mutex::new(HashSet::new()),
            queue,
            work: Mutex::new(crate::gc_telemetry::MarkWork::roots(roots)),
        }
    }

    fn is_marked(&self, address: usize) -> bool {
        self.objects.is_marked(address)
    }

    fn claim_or_defer(&self, address: usize) -> bool {
        if address == 0 {
            return false;
        }
        if self.objects.claim(address) {
            return true;
        }
        if !self.objects.contains(address) {
            // A captured active TLAB may not have published its starts yet.
            // Never discard SATB/roots just because retirement has not arrived.
            self.unindexed.lock().unwrap().insert(address);
        }
        false
    }

    fn recheck_unindexed(&self) {
        let pending = std::mem::take(&mut *self.unindexed.lock().unwrap());
        for address in pending {
            self.enqueue(address as *mut u8);
        }
    }

    fn enqueue(&self, value: *mut u8) {
        use crate::gc_mark_queue::{MarkWork, ObjectRef};
        if self.claim_or_defer(value as usize) {
            // Claim at publication, not at consumption: repeated deletions or
            // equivalent edges must not fill the queue with duplicate jobs.
            // Producers never safepoint between claiming and injecting. Remark
            // stops all producers before checking queue termination.
            let object = ObjectRef::from_ptr(value).expect("candidate is non-null");
            self.queue
                .inject(MarkWork::object(self.queue.current_epoch(), object))
                .expect("mark publication belongs to active cycle");
        }
    }

    fn enqueue_satb_batch(&self, values: &[usize]) {
        use crate::gc_mark_queue::{MarkWork, MarkWorkItem, ObjectRef};
        // Bound one owned queue item even with the maximum configured SATB
        // buffer. Claim and publication cannot safepoint or lose epoch ownership.
        const MAX_BATCH: usize = 32;
        for chunk in values.chunks(MAX_BATCH) {
            let objects: Vec<_> = chunk
                .iter()
                .copied()
                .filter(|&address| self.claim_or_defer(address))
                .map(|address| {
                    ObjectRef::from_ptr(address as *mut u8).expect("candidate is non-null")
                })
                .collect();
            if !objects.is_empty() {
                self.queue
                    .inject(MarkWork::new(
                        self.queue.current_epoch(),
                        MarkWorkItem::ObjectBatch(objects.into_boxed_slice()),
                    ))
                    .expect("SATB publication belongs to active cycle");
            }
        }
    }

    fn trace(&self, value: usize, children: &mut Vec<*mut u8>, batch: &mut MarkBatch<'_>) {
        // Also discard a partial snapshot left by an unwinding native hook.
        children.clear();
        let Some(metadata) = self.objects.metadata(value) else {
            return;
        };
        if self.legacy_traces.contains(&metadata.type_id)
            && !self.traces.contains_key(&metadata.type_id)
            && !self.slices.contains_key(&metadata.type_id)
        {
            // Legacy extension callbacks expose mutable slots with an STW-only
            // contract. Preserve that contract instead of racing their payloads.
            batch.deferred.push(value);
            return;
        }
        let payload = value as *mut u8;
        let words = metadata.payload_size / GC_STORAGE_WORD_BYTES;
        let mut slots = 0;
        for index in 0..words.min(64) {
            if metadata.gc_ref_mask & (1u64 << index) != 0 {
                // SAFETY: immutable epoch metadata bounds the live allocation;
                // generated and native reference stores use atomic publication.
                children.push(unsafe {
                    load_gc_reference(payload.add(index * GC_STORAGE_WORD_BYTES).cast::<*mut u8>())
                });
                slots += 1;
            }
        }
        if metadata.type_id == willow_abi::GC_BITMAP_TYPE_ID {
            let descriptor = metadata.layout_id as *const u64;
            // SAFETY: bitmap descriptors are validated immutable static data.
            let count = unsafe { *descriptor } as usize;
            if count.min(words.div_ceil(64)) > 1 {
                self.enqueue_trace_slice(value, 1);
            }
        }
        if self.slices.contains_key(&metadata.type_id) {
            slots += self.snapshot_native_slice(value, metadata.type_id, 0, children);
        } else if let Some(trace) = self.traces.get(&metadata.type_id) {
            let before = children.len();
            // SAFETY: concurrent hooks copy values under their own locks or
            // atomics, and no object is reclaimed before final remark.
            unsafe {
                trace(payload, children);
            }
            slots += children.len() - before;
        }
        for child in children.drain(..) {
            if !child.is_null() && !self.objects.contains(child as usize) {
                batch.unindexed.insert(child as usize);
            }
            self.enqueue(child);
        }
        batch
            .work
            .object(GC_HEADER_SIZE + metadata.payload_size, slots);
    }

    fn enqueue_trace_slice(&self, value: usize, word: usize) {
        use crate::gc_mark_queue::{MarkWork, MarkWorkItem, ObjectRef};
        self.queue
            .inject(MarkWork::new(
                self.queue.current_epoch(),
                MarkWorkItem::ObjectSlice {
                    object: ObjectRef::from_addr(value).unwrap(),
                    word,
                },
            ))
            .expect("bitmap continuation belongs to active cycle");
    }

    fn snapshot_native_slice(
        &self,
        value: usize,
        type_id: u32,
        offset: usize,
        children: &mut Vec<*mut u8>,
    ) -> usize {
        const MAX_SLOTS: usize = 512;
        let trace = self.slices.get(&type_id).expect("captured slice callback");
        let before = children.len();
        // SAFETY: an epoch reader retains payload storage; this registered hook
        // snapshots values through atomics/container locks with a stable cursor.
        let next = unsafe { trace(value as *mut u8, offset, MAX_SLOTS, children) };
        let slots = children.len() - before;
        assert!(slots <= MAX_SLOTS, "native trace exceeded its slot budget");
        match next {
            TraceSliceProgress::Done => {}
            TraceSliceProgress::Continue(next) => {
                assert!(next > offset, "native trace did not advance its cursor");
                self.enqueue_trace_slice(value, next);
            }
            TraceSliceProgress::Retry => {
                assert_eq!(slots, 0, "retry must not publish a partial native slice");
                self.enqueue_trace_slice(value, offset);
                std::thread::yield_now();
            }
        }
        slots
    }

    fn trace_native_slice(
        &self,
        value: usize,
        offset: usize,
        children: &mut Vec<*mut u8>,
        batch: &mut MarkBatch<'_>,
    ) {
        children.clear();
        let metadata = self
            .objects
            .metadata(value)
            .expect("native epoch retains its object");
        let slots = self.snapshot_native_slice(value, metadata.type_id, offset, children);
        for child in children.drain(..) {
            self.enqueue(child);
        }
        batch.work.object(0, slots);
    }

    fn trace_bitmap_slice(&self, value: usize, start: usize, batch: &mut MarkBatch<'_>) {
        const DESCRIPTOR_WORDS_PER_SLICE: usize = 8;
        let metadata = self
            .objects
            .metadata(value)
            .expect("bitmap epoch retains its object");
        assert_eq!(metadata.type_id, willow_abi::GC_BITMAP_TYPE_ID);
        let descriptor = metadata.layout_id as *const u64;
        let words = metadata.payload_size / GC_STORAGE_WORD_BYTES;
        // SAFETY: allocation validates the immutable descriptor. Start and
        // end are engine-generated and bounded by both descriptor and payload.
        let count = (unsafe { *descriptor } as usize).min(words.div_ceil(64));
        let end = start.saturating_add(DESCRIPTOR_WORDS_PER_SLICE).min(count);
        let mut slots = 0;
        for word in start..end {
            let mut bits = unsafe { *descriptor.add(word + 1) };
            while bits != 0 {
                let index = word * 64 + bits.trailing_zeros() as usize;
                if index < words {
                    let child = unsafe {
                        load_gc_reference(
                            (value as *mut u8)
                                .add(index * GC_STORAGE_WORD_BYTES)
                                .cast::<*mut u8>(),
                        )
                    };
                    self.enqueue(child);
                    slots += 1;
                }
                bits &= bits - 1;
            }
        }
        batch.work.object(0, slots);
        batch.work.descriptor_bytes = batch
            .work
            .descriptor_bytes
            .saturating_add((end - start) as u64 * 8);
        if end < count {
            self.enqueue_trace_slice(value, end);
        }
    }

    #[cfg(test)]
    fn drain_checked(&self, limit: usize) -> u64 {
        self.drain_checked_until(limit, None)
    }

    fn drain_checked_until(&self, limit: usize, deadline: Option<std::time::Instant>) -> u64 {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.drain_until(limit, deadline)
        }));
        match result {
            Ok(work) => work,
            Err(payload) => {
                crate::gc_telemetry::workers::record_failure(
                    crate::gc_telemetry::workers::Failure::MarkerPanic,
                );
                self.worker_failed.store(true, Ordering::Release);
                discard_callback_panic(payload);
                0
            }
        }
    }

    fn drain(&self, limit: usize) -> u64 {
        self.drain_until(limit, None)
    }

    fn drain_until(&self, limit: usize, deadline: Option<std::time::Instant>) -> u64 {
        // Concurrent hooks may neither allocate GC memory nor reach a safepoint,
        // so marking cannot reenter this thread's scratch-buffer borrow. Keep
        // capacity across bounded assists; successful drains leave the buffer empty.
        std::thread_local! {
            static CHILDREN: std::cell::RefCell<Vec<*mut u8>> =
                const { std::cell::RefCell::new(Vec::new()) };
        }
        CHILDREN.with_borrow_mut(|children| {
            let mut worker = self.queue.register_assist();
            self.drain_worker_until(limit, children, &mut worker, deadline)
                .1
        })
    }

    fn drain_background(&self, limit: usize, children: &mut Vec<*mut u8>) -> usize {
        let Some(mut worker) = self.queue.register_worker() else {
            // Mutator assists may temporarily occupy every slot. The collector
            // has a slotless consumer and will drain work not taken here.
            return 0;
        };
        self.drain_worker(limit, children, &mut worker).0
    }

    fn drain_worker(
        &self,
        limit: usize,
        children: &mut Vec<*mut u8>,
        worker: &mut crate::gc_mark_queue::MarkWorker,
    ) -> (usize, u64) {
        self.drain_worker_until(limit, children, worker, None)
    }

    fn drain_worker_until(
        &self,
        limit: usize,
        children: &mut Vec<*mut u8>,
        worker: &mut crate::gc_mark_queue::MarkWorker,
        deadline: Option<std::time::Instant>,
    ) -> (usize, u64) {
        use crate::gc_mark_queue::MarkWorkItem;
        // Barriers may enqueue before activation completes, but scanning then
        // could miss a store whose insertion barrier ran before phase=1.
        if !self.tracing_enabled.load(Ordering::Acquire) {
            return (0, 0);
        }
        let expired = || deadline.is_some_and(|end| std::time::Instant::now() >= end);
        self.active_drains.fetch_add(1, Ordering::AcqRel);
        let mut scanned = 0;
        let mut batch = MarkBatch {
            cycle: self,
            work: Default::default(),
            deferred: Vec::new(),
            unindexed: HashSet::new(),
            cpu_start: crate::gc_telemetry::workers::thread_cpu_ns(),
        };
        while scanned < limit {
            // Always permit one work item, even after descheduling at entry.
            if scanned != 0 && expired() {
                break;
            }
            let Some(work) = worker.next_work() else {
                break;
            };
            match work.item {
                MarkWorkItem::Object(object) => {
                    self.trace(object.addr(), children, &mut batch);
                    scanned += 1;
                }
                MarkWorkItem::ObjectBatch(objects) => {
                    // SATB queue items contain at most 32 objects. A drain can
                    // exceed its object budget by at most 31, never by B.
                    let mut objects = objects.into_vec().into_iter();
                    while let Some(object) = objects.next() {
                        self.trace(object.addr(), children, &mut batch);
                        scanned += 1;
                        if expired() && objects.len() != 0 {
                            // Publish the unfinished tail before completing the
                            // original item; closure must never observe a gap.
                            self.queue
                                .inject(crate::gc_mark_queue::MarkWork::new(
                                    self.queue.current_epoch(),
                                    MarkWorkItem::ObjectBatch(
                                        objects.collect::<Vec<_>>().into_boxed_slice(),
                                    ),
                                ))
                                .expect("assist tail belongs to active epoch");
                            break;
                        }
                    }
                }
                MarkWorkItem::ObjectSlice { object, word } => {
                    let metadata = self
                        .objects
                        .metadata(object.addr())
                        .expect("live continuation");
                    if metadata.type_id == willow_abi::GC_BITMAP_TYPE_ID {
                        self.trace_bitmap_slice(object.addr(), word, &mut batch);
                    } else {
                        self.trace_native_slice(object.addr(), word, children, &mut batch);
                    }
                    scanned += 1;
                }
                _ => unreachable!("major marker queues only object work"),
            }
            worker.complete_current();
        }
        // Drop publishes remaining private work and publishes private work before a
        // mutator can reach its next safepoint.
        (
            scanned,
            batch
                .work
                .marked_bytes
                .saturating_add(batch.work.scanned_bytes)
                .saturating_add(batch.work.descriptor_bytes),
        )
    }
}

static MARK_WORKERS: Mutex<Option<mark_workers::Pool>> = Mutex::new(None);

pub(crate) fn shutdown_mark_workers() {
    coordinator::shutdown();
    // Called after user main returns, or by isolated runtime reset. No active
    // cycle can outlive Pool::run(), which holds this mutex until readers leave.
    let pool = MARK_WORKERS.lock().unwrap().take();
    drop(pool);
}

/// Enumerate allocation headers only, never payload graph edges. All generated
/// TLABs must be retired and mutators stopped while this index is captured.
fn epoch_objects(
    state: &GcState,
    work: &mut crate::gc_telemetry::stops::StopWorkV2,
) -> Vec<HeapObject> {
    let mut objects = Vec::new();
    for object in old_region_objects(state) {
        work.metadata_objects += 1;
        work.metadata_bytes += GC_HEADER_SIZE as u64;
        objects.push(object);
    }
    for chunk in &state.tlab_chunks {
        assert!(
            chunk.owner_state.is_none(),
            "epoch index requires retired TLABs"
        );
        for &offset in &chunk.header_offsets {
            // Retirement validates physical headers once; subsequent snapshots
            // use its persistent ordered index rather than reparsing boundaries.
            let object =
                HeapObject::from_raw(unsafe { chunk.base.add(usize::from(offset)) }.cast())
                    .unwrap();
            work.metadata_objects += 1;
            work.metadata_bytes += GC_HEADER_SIZE as u64;
            if object.allocated() {
                objects.push(object);
            }
        }
    }
    objects
}

/// Bounded allocation assistance. Only call outside runtime container locks.
fn assist_concurrent_mark(allocated: u64) {
    if GC_MARK_PHASE.load(Ordering::Acquire) != 1 {
        assist::reset();
        return;
    }
    let cycle = runtime().heap.lock().unwrap().concurrent_cycle.clone();
    if let Some(cycle) = cycle
        && !cycle.closing.load(Ordering::Acquire)
        && cycle.assist_rate != 0
        && assist::charge(cycle.assist_epoch, allocated, cycle.assist_rate)
    {
        // One bounded object batch per slow allocation. An empty queue cannot
        // block allocation or manufacture credit; background marking proceeds.
        // A scheduling budget, not a hard callback-duration guarantee: legacy
        // native hooks can overrun it. Bitmap/array/map slices have bounded work.
        let deadline = std::time::Instant::now() + std::time::Duration::from_micros(250);
        let work = cycle.drain_checked_until(8, Some(deadline));
        assist::credit(cycle.assist_epoch, work);
    }
}

fn mark_worklist(
    mut worklist: Vec<*mut u8>,
    completed: Option<(&epoch_index::EpochIndex, &HashSet<usize>)>,
) -> crate::gc_telemetry::MarkWork {
    let started = std::time::Instant::now();
    let mut work = crate::gc_telemetry::MarkWork::roots(worklist.len());
    while let Some(obj_ptr) = worklist.pop() {
        let header = checked_payload_to_header(obj_ptr, "GC root graph");
        let object = HeapObject::from_raw(header).expect("validated payload has a header");
        let Some(metadata) = object.begin_trace() else {
            continue; // already visited — handles cycles
        };
        // Concurrently completed objects need no second graph walk. Deferred
        // legacy hooks still trace even though their mark bit was claimed.
        if completed.is_some_and(|(index, deferred)| {
            let address = obj_ptr as usize;
            (!index.contains(address) || index.is_marked(address)) && !deferred.contains(&address)
        }) {
            continue;
        }
        let payload_words = metadata.payload_size / GC_STORAGE_WORD_BYTES;
        let mut scanned_slots = 0usize;
        for i in 0..payload_words.min(64) {
            if (metadata.gc_ref_mask & (1u64 << i)) != 0 {
                scanned_slots += 1;
                if let Some(child) = object.payload_word(i) {
                    worklist.push(child.as_ptr());
                }
            }
        }
        let mut bitmap_slots = Vec::new();
        append_bitmap_slots(object, &mut bitmap_slots);
        for slot in bitmap_slots {
            scanned_slots += 1;
            // SAFETY: bitmap slots belong to this live object's payload.
            let child = unsafe { *slot };
            if !child.is_null() {
                worklist.push(child);
            }
        }
        let trace_fn = type_registry()
            .lock()
            .unwrap()
            .get(&metadata.type_id)
            .copied();
        if let Some(trace) = trace_fn {
            let mut child_slots: Vec<*mut *mut u8> = Vec::new();
            // SAFETY: trace is the registered function for this type_id.
            unsafe { trace(object.payload().as_ptr(), &mut child_slots) };
            for slot in child_slots.into_iter().filter(|slot| !slot.is_null()) {
                scanned_slots += 1;
                // SAFETY: registered trace callbacks expose live GC-reference
                // slots owned by this object or its runtime payload.
                let child = unsafe { *slot };
                if !child.is_null() {
                    worklist.push(child);
                }
            }
        }
        work.object(object.size(), scanned_slots);
    }
    work.mark_ns = crate::gc_telemetry::elapsed_ns(started);
    work
}

// ---------------------------------------------------------------------------
// TypeInfo registry
// ---------------------------------------------------------------------------

/// Trace function: given a payload pointer, expose the addresses of all mutable
/// GC-reference slots it owns. Full marking loads the slots; minor collection
/// can additionally replace a moved young pointer in place.
pub type TraceFn = unsafe fn(payload: *mut u8, slots: &mut Vec<*mut *mut u8>);

/// Snapshot child VALUES while holding any required container locks. Unlike
/// TraceFn this callback must be safe while mutators run; exported slots cannot
/// outlive a lock guard. It must not allocate GC memory or reach a safepoint.
/// Objects without this hook are traced during remark.
pub type ConcurrentTraceFn = unsafe fn(payload: *mut u8, children: &mut Vec<*mut u8>);

/// Bounded concurrent snapshot. Append at most `limit` child values and return
/// a strictly advancing cursor, Done, or Retry without children when a native
/// lock is busy. Cursors must survive mutation and have a finite epoch bound:
/// stable indexed slots plus deletion/insertion barriers are sufficient; restarting
/// a mutable hash iterator or skipping a changing prefix is not. The callback
/// must bound all work (including empty slots) and may not allocate GC memory,
/// safepoint, retain a lock across calls or return mutable slot addresses.
pub type ConcurrentTraceSliceFn = unsafe fn(
    payload: *mut u8,
    cursor: usize,
    limit: usize,
    children: &mut Vec<*mut u8>,
) -> TraceSliceProgress;

pub enum TraceSliceProgress {
    Done,
    Continue(usize),
    Retry,
}

/// Load a GC reference shared with the concurrent marker.
/// # Safety
/// The aligned slot must stay allocated; all concurrent writes must be atomic.
pub(crate) unsafe fn load_gc_reference(slot: *mut *mut u8) -> *mut u8 {
    unsafe { std::sync::atomic::AtomicPtr::from_ptr(slot).load(Ordering::Acquire) }
}

/// Publish a GC reference shared with the concurrent marker. Call the write
/// barrier before publishing any non-null edge.
/// # Safety
/// The aligned slot must stay allocated and contain a reference-sized word.
pub(crate) unsafe fn store_gc_reference(slot: *mut *mut u8, value: *mut u8) {
    unsafe { std::sync::atomic::AtomicPtr::from_ptr(slot).store(value, Ordering::Release) }
}

fn type_registry() -> &'static Mutex<HashMap<u32, TraceFn>> {
    &runtime().trace_registry
}

/// Register a trace function for `type_id`.  Call once per class at startup.
pub fn willow_register_type(type_id: u32, trace: TraceFn) {
    type_registry().lock().unwrap().insert(type_id, trace);
    // A replacement legacy hook must not inherit another implementation's
    // concurrent contract. Native registration installs its matched hook next.
    runtime()
        .concurrent_trace_registry
        .lock()
        .unwrap()
        .remove(&type_id);
    runtime()
        .concurrent_slice_registry
        .lock()
        .unwrap()
        .remove(&type_id);
}

/// Unregister the trace function for `type_id`.
pub fn willow_unregister_type(type_id: u32) {
    type_registry().lock().unwrap().remove(&type_id);
    runtime()
        .concurrent_trace_registry
        .lock()
        .unwrap()
        .remove(&type_id);
    runtime()
        .concurrent_slice_registry
        .lock()
        .unwrap()
        .remove(&type_id);
    runtime()
        .registry_generation
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

/// Finalizer: given a payload pointer, release any non-GC resources the object
/// owns (e.g. a boxed Rust collection) just before the object is freed by the
/// sweep phase.  Must not allocate GC memory or touch GC state.
pub type DropFn = unsafe fn(payload: *mut u8);

/// One runtime-native GC payload's trace/finalizer hooks.
///
/// Native containers use this target-independent descriptor with
/// [`NativeGcRegistration`] instead of open-coding generation atomics, a
/// registration mutex, and two registry calls in every module.
#[derive(Clone, Copy)]
pub struct NativeGcType {
    pub type_id: u32,
    pub trace: Option<TraceFn>,
    pub drop_fn: Option<DropFn>,
    pub concurrent_trace: Option<ConcurrentTraceFn>,
    pub concurrent_slice: Option<ConcurrentTraceSliceFn>,
}

impl NativeGcType {
    pub const fn new(type_id: u32, trace: Option<TraceFn>, drop_fn: Option<DropFn>) -> Self {
        Self {
            type_id,
            trace,
            drop_fn,
            concurrent_trace: None,
            concurrent_slice: None,
        }
    }
    pub const fn with_concurrent_trace(mut self, trace: ConcurrentTraceFn) -> Self {
        self.concurrent_trace = Some(trace);
        self
    }
    pub const fn with_concurrent_slice(mut self, trace: ConcurrentTraceSliceFn) -> Self {
        self.concurrent_slice = Some(trace);
        self
    }
}

/// Installs a module's native GC hooks at most once per registry generation.
///
/// `willow_gc_init` and explicit unregistration invalidate the runtime
/// registries by advancing their generation. The allocation hot path pays one
/// acquire load; only the first allocation in a generation takes this mutex.
pub struct NativeGcRegistration {
    registered_generation: AtomicU64,
    lock: Mutex<()>,
}

impl NativeGcRegistration {
    pub const fn new() -> Self {
        Self {
            registered_generation: AtomicU64::new(0),
            lock: Mutex::new(()),
        }
    }

    /// Ensure every descriptor is installed. Returns `true` only to the caller
    /// that performed registration for this generation.
    pub fn ensure(&self, types: &[NativeGcType]) -> bool {
        let generation = registry_generation();
        if self.registered_generation.load(Ordering::Acquire) == generation {
            return false;
        }
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let generation = registry_generation();
        if self.registered_generation.load(Ordering::Acquire) == generation {
            return false;
        }
        for native in types {
            if let Some(trace) = native.trace {
                willow_register_type(native.type_id, trace);
            }
            if let Some(trace) = native.concurrent_trace {
                runtime()
                    .concurrent_trace_registry
                    .lock()
                    .unwrap()
                    .insert(native.type_id, trace);
            }
            if let Some(trace) = native.concurrent_slice {
                runtime()
                    .concurrent_slice_registry
                    .lock()
                    .unwrap()
                    .insert(native.type_id, trace);
            }
            if let Some(drop_fn) = native.drop_fn {
                willow_register_drop(native.type_id, drop_fn);
            }
        }
        self.registered_generation
            .store(generation, Ordering::Release);
        true
    }
}

impl Default for NativeGcRegistration {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable mutable GC-reference slot owned by a [`GcRootArena`].
///
/// The collector receives the address of `value` and may rewrite it when a
/// young object moves. `Arc` ownership makes that address independent of arena
/// vector growth and keeps it alive after a logical root is released.
struct GcRootCell(std::sync::atomic::AtomicPtr<u8>);

// Atomic publication permits concurrent marking; the Arc allocation keeps
// slot addresses stable until owner finalization. Relocation writes stay STW.

/// Handle for one slot in a [`GcRootArena`]. Dropping the handle does not free
/// the slot because the arena retains its own Arc until the owning runtime
/// object is finalized.
pub(crate) struct GcRootHandle {
    cell: Arc<GcRootCell>,
}

impl GcRootHandle {
    pub(crate) fn load(&self) -> *mut u8 {
        // SAFETY: see `GcRootCell`'s synchronization contract.
        self.cell.0.load(Ordering::Acquire)
    }

    pub(crate) fn release(&self) {
        // SAFETY: see `GcRootCell`'s synchronization contract. Null is the GC
        // root protocol's explicit inactive-slot value.
        satb_delete(self.cell.0.load(Ordering::Acquire));
        self.cell.0.store(std::ptr::null_mut(), Ordering::Release);
    }
}

/// Stable arena for runtime-owned GC root slots.
///
/// Slots are retained until the arena itself is dropped, so a collector that
/// obtained an address from a trace callback never observes freed Rust heap
/// storage. Logical release nulls the slot immediately and task metadata may be
/// reclaimed; physical slot reclamation is synchronized with owner finalization.
pub(crate) struct GcRootArena {
    cells: Mutex<Vec<Arc<GcRootCell>>>,
}

impl Default for GcRootArena {
    fn default() -> Self {
        let cells = Mutex::new(Vec::new());
        // Some platforms lazily allocate native mutex storage on first use,
        // including try_lock(). Pay that cost before publishing the arena so
        // even its first, empty snapshot remains allocation-free.
        drop(cells.lock().unwrap_or_else(|error| error.into_inner()));
        Self { cells }
    }
}

impl GcRootArena {
    pub(crate) fn insert(&self, owner: *mut u8, value: *mut u8) -> GcRootHandle {
        willow_gc_write_barrier(
            owner,
            std::ptr::null_mut(),
            value,
            GcStoreDestination::ContainerInternal as i64,
        );
        let cell = Arc::new(GcRootCell(std::sync::atomic::AtomicPtr::new(value)));
        self.cells
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Arc::clone(&cell));
        GcRootHandle { cell }
    }

    pub(crate) fn trace_slots(&self, slots: &mut Vec<*mut *mut u8>) {
        let cells = self
            .cells
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slots.extend(cells.iter().map(|cell| cell.0.as_ptr()));
    }

    /// Copy a bounded prefix of stable cells without waiting on an arena writer.
    /// `bound` fixes the initial extent; insertions publish new edges separately.
    pub(crate) fn snapshot_slice(
        &self,
        cursor: usize,
        limit: usize,
        bound: usize,
        children: &mut Vec<*mut u8>,
    ) -> Option<(usize, usize)> {
        let cells = match self.cells.try_lock() {
            Ok(cells) => cells,
            Err(std::sync::TryLockError::WouldBlock) => return None,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        };
        let bound = bound.min(cells.len());
        let end = cursor.saturating_add(limit).min(bound);
        children.extend(
            cells[cursor.min(bound)..end]
                .iter()
                .map(|cell| cell.0.load(Ordering::Acquire)),
        );
        Some((end, bound))
    }

    #[cfg(test)]
    pub(crate) fn slot_count(&self) -> usize {
        self.cells
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

/// Publish an edge stored outside GC-managed storage. Does not safepoint.
pub(crate) fn publish_native_reference(value: *mut u8) {
    if let Some(cycle) = runtime().heap.lock().unwrap().concurrent_cycle.as_ref() {
        cycle.enqueue(value);
    }
}

fn drop_registry() -> &'static Mutex<HashMap<u32, DropFn>> {
    &runtime().drop_registry
}

/// Register a finalizer for `type_id`, run by the sweep phase before an object
/// of that type is deallocated.
pub fn willow_register_drop(type_id: u32, drop_fn: DropFn) {
    drop_registry().lock().unwrap().insert(type_id, drop_fn);
}

fn lookup_drop(type_id: u32) -> Option<DropFn> {
    drop_registry().lock().unwrap().get(&type_id).copied()
}

/// Dead storage must be reclaimed exactly once even when a native destructor
/// unwinds. Retrying a partially executed destructor can double-free its native
/// resources. Hooks must remain leaf operations: no GC access or safepoints.
unsafe fn run_drop_hook(drop_fn: DropFn, payload: *mut u8) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: caller owns a dead, still-allocated payload for this hook.
        unsafe { drop_fn(payload) };
    }));
    if let Err(panic_payload) = result {
        crate::gc_telemetry::workers::record_failure(
            crate::gc_telemetry::workers::Failure::DropPanic,
        );
        discard_callback_panic(panic_payload);
    }
}

fn discard_callback_panic(payload: Box<dyn std::any::Any + Send>) {
    // A panic-payload destructor must not escape a recovered native callback.
    if let Err(secondary) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)))
    {
        std::mem::forget(secondary);
    }
}

/// Current hook-registry generation. This changes only when existing
/// registrations are invalidated, never when another type is merely added.
pub(crate) fn registry_generation() -> u64 {
    runtime()
        .registry_generation
        .load(std::sync::atomic::Ordering::Acquire)
}

// ---------------------------------------------------------------------------
// TLAB state and chunk management
// ---------------------------------------------------------------------------

unsafe fn tlab_state_at(address: usize) -> &'static GcTlabState {
    // SAFETY: generated code passes the address of its aligned, zero-initialized
    // TLS block whose layout is locked by the compiler/runtime ABI tests.
    unsafe { &*(address as *const GcTlabState) }
}

#[cfg(test)]
static TLAB_ACCOUNTING_RECORD_VISITS: AtomicUsize = AtomicUsize::new(0);

fn read_tlab_delta(record: &mut TlabStateRecord) -> (u64, u64) {
    #[cfg(test)]
    TLAB_ACCOUNTING_RECORD_VISITS.fetch_add(1, Ordering::Relaxed);
    // SAFETY: records are removed before their owner's generated TLS expires.
    let tls = unsafe { tlab_state_at(record.address) };
    let allocations = tls.fast_allocations.load(Ordering::Acquire);
    let bytes = tls.fast_allocated_bytes.load(Ordering::Acquire);
    let delta = (
        allocations.saturating_sub(record.observed_fast_allocations),
        bytes.saturating_sub(record.observed_fast_allocated_bytes),
    );
    record.observed_fast_allocations = allocations;
    record.observed_fast_allocated_bytes = bytes;
    delta
}
fn add_tlab_accounting(state: &mut GcState, allocations: u64, bytes: u64) {
    state.total_allocs = state.total_allocs.saturating_add(allocations);
    state.total_allocated_bytes = state.total_allocated_bytes.saturating_add(bytes);
    state.tlab_fast_allocations = state.tlab_fast_allocations.saturating_add(allocations);
    state.tlab_fast_allocated_bytes = state.tlab_fast_allocated_bytes.saturating_add(bytes);
    state.allocated_bytes = state.allocated_bytes.saturating_add(bytes as usize);
    state.young_allocated_bytes = state.young_allocated_bytes.saturating_add(bytes as usize);
}
fn sync_tlab_accounting(state: &mut GcState) {
    let (mut allocations, mut bytes) = (0u64, 0u64);
    for record in state.tlab_states.values_mut() {
        let delta = read_tlab_delta(record);
        allocations = allocations.saturating_add(delta.0);
        bytes = bytes.saturating_add(delta.1);
    }
    add_tlab_accounting(state, allocations, bytes);
}

thread_local! {
    // Marker workers normally own no generated TLAB. Their unregister path
    // must not rescan every mutator's allocation counters/records each cycle.
    static HAS_REGISTERED_TLAB: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn register_tlab_state(state: &mut GcState, address: usize) {
    HAS_REGISTERED_TLAB.set(true);
    state.tlab_states.entry(address).or_insert_with(|| {
        state
            .tlab_owners
            .entry(std::thread::current().id())
            .or_default()
            .insert(address);
        TlabStateRecord {
            address,
            owner: std::thread::current().id(),
            current_chunk: None,
            assist_observed_fast_bytes: 0,
            observed_fast_allocations: 0,
            observed_fast_allocated_bytes: 0,
        }
    });
}

fn retire_tlab_locked(state: &mut GcState, address: usize) -> usize {
    let Some(record) = state.tlab_states.get_mut(&address) else {
        return 0;
    };
    // SAFETY: the record owns this generated TLS state until unregister/reset.
    let tls = unsafe { tlab_state_at(record.address) };
    let cursor = tls.cursor.swap(0, Ordering::AcqRel);
    tls.limit.store(0, Ordering::Release);
    tls.start_bits.store(0, Ordering::Release);
    let current_chunk = record.current_chunk.take();
    if let Some(base) = current_chunk
        && let Some(index) = state.tlab_addresses.exact(base)
    {
        let chunk = &mut state.tlab_chunks[index];
        let start = chunk.base as usize;
        let end = start.saturating_add(chunk.capacity);
        chunk.used = cursor.clamp(start, end).saturating_sub(start);
        chunk.owner_state = None;
        assert!(
            chunk.header_offsets.is_empty(),
            "TLAB indexed more than once"
        );
        // The owner is stopped or retiring its own chunk; generated headers
        // are now immutable except for collector liveness/generation fields.
        let mut offset = 0;
        while offset < chunk.used {
            let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast()).unwrap();
            let size = object.size();
            assert!(
                size >= GC_HEADER_SIZE
                    && size <= chunk.used - offset
                    && size.is_multiple_of(GC_REGION_MARK_GRANULE),
                "corrupt retired TLAB header"
            );
            assert!(
                chunk.mark_bitmap.is_marked(offset),
                "generated TLAB header was not published"
            );
            chunk
                .header_offsets
                .push(u16::try_from(offset).expect("TLAB offset fits bounded chunk"));
            offset += size;
        }
        return chunk.header_offsets.len();
    }
    0
}

fn retire_all_tlabs_locked(state: &mut GcState) -> usize {
    sync_tlab_accounting(state);
    let addresses: Vec<usize> = state.tlab_states.keys().copied().collect();
    let mut headers = 0;
    for address in addresses {
        headers += retire_tlab_locked(state, address);
    }
    headers
}

fn retire_tlabs_with_work(state: &mut GcState, work: &mut crate::gc_telemetry::stops::StopWorkV2) {
    let headers = retire_all_tlabs_locked(state) as u64;
    work.metadata_objects += headers;
    work.metadata_bytes += headers * GC_HEADER_SIZE as u64;
}

fn retire_owned_tlabs_locked(state: &mut GcState, owner: ThreadId, unregister: bool) {
    let addresses: Vec<_> = state
        .tlab_owners
        .get(&owner)
        .into_iter()
        .flat_map(|addresses| addresses.iter().copied())
        .collect();
    for address in addresses {
        let record = state
            .tlab_states
            .get_mut(&address)
            .expect("owner index has a TLS record");
        debug_assert_eq!(record.owner, owner);
        let (allocations, bytes) = read_tlab_delta(record);
        // Retirement at the initial root handshake starts this owner's epoch
        // after all pre-snapshot allocation. Do not bill it to the new cycle.
        record.assist_observed_fast_bytes = record.observed_fast_allocated_bytes;
        add_tlab_accounting(state, allocations, bytes);
        retire_tlab_locked(state, address);
        if unregister {
            state.tlab_states.remove(&address);
        }
    }
    if unregister {
        state.tlab_owners.remove(&owner);
    }
}

fn retire_tlabs_for_thread(owner: ThreadId) {
    debug_assert_eq!(owner, std::thread::current().id());
    if !HAS_REGISTERED_TLAB.replace(false) {
        return;
    }
    retire_owned_tlabs_locked(&mut runtime().heap.lock().unwrap(), owner, true);
}

fn allocate_tlab_chunk(state: &mut GcState, owner_state: usize) -> Option<*mut u8> {
    if !can_reserve(state, GC_TLAB_CHUNK_SIZE) {
        return None;
    }
    let layout =
        Layout::from_size_align(GC_TLAB_CHUNK_SIZE, std::mem::align_of::<GcHeader>()).ok()?;
    // SAFETY: the layout is nonzero and valid. Fresh zeroing makes every
    // unallocated payload byte safe before generated code publishes a header.
    let base = unsafe { allocate_region_storage(layout) };
    if base.is_null() {
        return None;
    }
    runtime().tlab_ever_allocated.store(true, Ordering::Release);
    state
        .tlab_addresses
        .insert(base as usize, state.tlab_chunks.len());
    state.tlab_chunks.push(TlabChunk {
        base,
        capacity: GC_TLAB_CHUNK_SIZE,
        used: 0,
        owner_state: Some(owner_state),
        kind: RegionKind::Nursery,
        live_bytes: 0,
        mark_bitmap: RegionMarkBitmap::new(GC_TLAB_CHUNK_SIZE),
        concurrent_marks: Arc::new(concurrent_bitmap::ConcurrentMarkBits::new(
            GC_TLAB_CHUNK_SIZE / GC_REGION_MARK_GRANULE,
        )),
        header_offsets: Vec::new(),
    });
    let chunk = state.tlab_chunks.last().unwrap();
    // SAFETY: the generated TLS is owned by this allocation's mutator.
    unsafe { tlab_state_at(owner_state) }.start_bits.store(
        chunk.mark_bitmap.bits.words_ptr() as usize,
        Ordering::Release,
    );
    state.tlab_reserved_bytes = state.tlab_reserved_bytes.saturating_add(GC_TLAB_CHUNK_SIZE);
    state.tlab_refills = state.tlab_refills.saturating_add(1);
    state
        .tlab_states
        .get_mut(&owner_state)
        .expect("TLAB state is registered before refill")
        .current_chunk = Some(base as usize);
    Some(base)
}

fn initialize_object_at(
    header: *mut u8,
    total_size: usize,
    type_id: u32,
    layout_id: u64,
    gc_ref_mask: u64,
) -> Option<HeapObject> {
    HeapObject::initialize_at(
        header,
        total_size,
        type_id,
        layout_id,
        gc_ref_mask,
        GC_GENERATION_YOUNG,
    )
}

fn allocation_should_collect() -> bool {
    let stress = gc_stress_enabled("alloc");
    let mut state = runtime().heap.lock().unwrap();
    if !stress && (state.concurrent_cycle.is_some() || state.sweeping.is_some()) {
        return false;
    }
    sync_tlab_accounting(&mut state);
    if state.pacer.sample_due(state.total_allocated_bytes) {
        let bytes = state.total_allocated_bytes;
        state
            .pacer
            .allocation(crate::gc_telemetry::timestamp_ns(), bytes);
        // Sampling may advance the trigger, but cannot move the current goal.
        let decision = state.pacer.decision(pacer_inputs(&state));
        state.pacer_trigger = (state.threshold_bytes as u64)
            .saturating_sub(decision.runway)
            .max(state.last_major_live_bytes.saturating_add(256 * 1024))
            .min(state.threshold_bytes as u64);
    }
    let hard_pressure = state
        .memory_limit_bytes
        .is_some_and(|limit| state.allocated_bytes >= (limit / 4).saturating_mul(3).max(1));
    let memory = state.soft_memory.decide(memory_inputs(&state));
    let paced = state.pacer.enabled()
        && state.allocated_bytes as u64 >= state.pacer_trigger
        && memory.reason != memory_control::Reason::Relief;
    stress || hard_pressure || paced || memory.collect
}

fn allocation_should_minor_collect() -> bool {
    let stress = gc_stress_enabled("minor");
    let mut state = runtime().heap.lock().unwrap();
    sync_tlab_accounting(&mut state);
    stress || state.young_allocated_bytes >= state.nursery_threshold_bytes
}

// ---------------------------------------------------------------------------
// Public runtime API
// ---------------------------------------------------------------------------

/// Set the soft managed-memory limit in bytes; zero disables it. Returns the
/// previous limit. Collection is considered at the next allocation slow path;
/// this setter never parks mutators or changes the legacy hard reservation cap.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_set_memory_limit(bytes: u64) -> u64 {
    runtime().heap.lock().unwrap().soft_memory.set_limit(bytes)
}

/// Initialize the GC runtime.
///
/// Production code calls this once at process startup, before any allocation.
/// Calling it again resets the single process-global heap and invalidates
/// existing GC pointers, so it is not a general-purpose runtime reset API.
/// Unit tests may intentionally reset the heap, but they must hold
/// `runtime_test_guard()` while doing so because the Rust test harness runs
/// tests in parallel in one process.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_init() {
    reset_internal();
}

/// Register a root slot.  `slot` must point to a stack location that holds
/// a GC-managed pointer.  The slot must remain valid until the matching pop.
#[unsafe(no_mangle)]
pub extern "C" fn willow_push_root(slot: *mut *mut u8) {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    claim_root_stack_owner();
    ROOT_STACK.with(|rs| {
        let mut stack = rs.borrow_mut();
        stack.push(slot);
        ROOT_DEPTH.set(stack.len());
    });
}

/// Unregister the most recently pushed root slot.
#[unsafe(no_mangle)]
pub extern "C" fn willow_pop_root() {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    ROOT_STACK.with(|rs| {
        let mut stack = rs.borrow_mut();
        stack.pop();
        ROOT_DEPTH.set(stack.len());
    });
    release_root_stack_owner_if_empty();
}

/// Unregister `count` root slots from the top of the root stack.
#[unsafe(no_mangle)]
pub extern "C" fn willow_pop_roots(count: i32) {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    ROOT_STACK.with(|rs| {
        let mut stack = rs.borrow_mut();
        let remove = (count as usize).min(stack.len());
        let new_len = stack.len() - remove;
        stack.truncate(new_len);
        ROOT_DEPTH.set(stack.len());
    });
    release_root_stack_owner_if_empty();
}

/// Current generated-code shadow-root depth for this mutator. Panic cleanup
/// records a lexical scope's entry depth and restores it on every shared
/// unwind edge, where the number of roots pushed before the panic is otherwise
/// path-dependent (willow-s9ej.3).
#[unsafe(no_mangle)]
pub extern "C" fn willow_root_depth() -> i32 {
    i32::try_from(ROOT_DEPTH.get()).unwrap_or_else(|_| {
        eprintln!("runtime fatal: generated-code root depth overflow");
        std::process::abort();
    })
}

/// Number of shadow roots on the running native stack.
pub(crate) fn gc_thread_root_depth() -> usize {
    ROOT_DEPTH.get()
}

// The caller owns live local slots or holds the suspended-stack registry lock.
// Publication neither traces nor safepoints, so stack transfer stays indivisible
// with respect to this thread's root handshake.
fn retain_transferred_roots(slots: impl IntoIterator<Item = *mut *mut u8>) {
    if GC_MARK_PHASE.load(Ordering::Acquire) == 0 {
        return;
    }
    let state = runtime().heap.lock().unwrap();
    if let Some(cycle) = &state.concurrent_cycle {
        for slot in slots {
            if let Some(value) = RootSlot::from_raw(slot).and_then(RootSlot::load) {
                cycle.enqueue(value.as_ptr());
            }
        }
    }
}

/// Transfer a native task stack's roots to the collector before suspending it.
///
/// # Safety
/// Every slot in the suffix must remain allocated and unchanged until resume
/// or discard. The caller must switch stacks without a intervening safepoint.
pub(crate) unsafe fn park_current_roots(depth: usize) -> u64 {
    let token = runtime().next_parked_stack.fetch_add(1, Ordering::Relaxed);
    assert_ne!(token, 0, "parked native stack token exhausted");
    let mut parked = runtime().parked_stack_roots.lock().unwrap();
    ROOT_STACK.with(|roots| {
        let mut roots = roots.borrow_mut();
        assert!(depth <= roots.len(), "native stack root depth mismatch");
        retain_transferred_roots(roots[depth..].iter().copied());
        parked.insert(
            token,
            roots[depth..].iter().map(|slot| *slot as usize).collect(),
        );
        roots.truncate(depth);
        ROOT_DEPTH.set(roots.len());
    });
    release_root_stack_owner_if_empty();
    token
}

/// Reattach a suspended stack's shadow roots immediately before resuming it.
///
/// # Safety
/// `token` must own a still-live suspended native stack. The caller must resume
/// that stack without executing a safepoint against the wrong stack's slots.
pub(crate) unsafe fn resume_parked_roots(token: u64) {
    let mut parked = runtime().parked_stack_roots.lock().unwrap();
    let slots = parked.remove(&token).expect("unknown parked native stack");
    retain_transferred_roots(slots.iter().map(|&slot| slot as *mut *mut u8));
    if !slots.is_empty() {
        claim_root_stack_owner();
    }
    ROOT_STACK.with(|roots| {
        let mut roots = roots.borrow_mut();
        roots.extend(slots.into_iter().map(|slot| slot as *mut *mut u8));
        ROOT_DEPTH.set(roots.len());
    });
}

/// Release roots only after the corresponding suspended stack was unwound.
///
/// # Safety
/// No live Willow frame may still depend on any root belonging to `token`.
pub(crate) unsafe fn discard_parked_roots(token: u64) {
    runtime()
        .parked_stack_roots
        .lock()
        .unwrap()
        .remove(&token)
        .expect("unknown parked native stack");
}

/// Keep a GC-managed object alive through a runtime-owned structure such as a
/// scheduler task, future frame, task handle, or wait queue.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_add_runtime_root(object: *mut u8) {
    if object.is_null() {
        return;
    }

    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    // Registry publication cannot safepoint before the insertion finishes.
    // The activation handshake crosses it before taking runtime roots, just
    // as it crosses the corresponding phase-gated SATB deletion below.
    if GC_MARK_PHASE.load(Ordering::Acquire) != 0
        && let Some(cycle) = runtime().heap.lock().unwrap().concurrent_cycle.as_ref()
    {
        cycle.enqueue(object);
    }
    runtime().runtime_roots.add(object);
}

/// Remove a persistent runtime root when the owning runtime structure no
/// longer needs to retain the object.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_remove_runtime_root(object: *mut u8) {
    if object.is_null() {
        return;
    }

    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    satb_delete(object);
    runtime().runtime_roots.remove(object);
}

/// Allocate a GC-managed object of `payload_size` bytes with the given
/// `type_id`.  Returns a pointer to the **payload** (past the header), or
/// null on allocation failure.
///
/// This function may trigger a collection if the heap threshold is exceeded.
#[unsafe(no_mangle)]
pub extern "C" fn willow_alloc_object(type_id: i64, payload_size: i64) -> *mut u8 {
    allocate_object(0, type_id as u32, payload_size, 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_alloc_typed(payload_size: i64, gc_ref_mask: u64) -> *mut u8 {
    allocate_object(0, 0, payload_size, gc_ref_mask)
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_alloc(payload_size: i64) -> *mut u8 {
    willow_alloc_typed(payload_size, 0)
}

/// Compatibility layout-aware allocation ABI used by runtime-owned values.
///
/// Generated Willow code uses its inlined TLS bump path and calls
/// `willow_gc_alloc_slow` only on refill/large/stress paths. Runtime containers
/// without access to the generated TLS block use the old-region slow path.
/// Allocate an object with scalable tracing metadata. `descriptor` points to
/// immutable, aligned static u64 data `[count, bits...]`, alive until all such
/// objects have been reclaimed. Payload and bitmap sizes must agree.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_alloc_bitmap(
    _type_id: i64,
    payload_size: i64,
    descriptor: *const u64,
) -> *mut u8 {
    assert!(!descriptor.is_null());
    assert!(payload_size >= 0 && payload_size % GC_STORAGE_WORD_BYTES as i64 == 0);
    let count = unsafe { *descriptor } as usize;
    assert_eq!(
        count,
        (payload_size as usize / GC_STORAGE_WORD_BYTES).div_ceil(64)
    );
    let mask = if count == 0 {
        0
    } else {
        unsafe { *descriptor.add(1) }
    };
    allocate_object(
        descriptor as u64,
        willow_abi::GC_BITMAP_TYPE_ID,
        payload_size,
        mask,
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_alloc_layout(
    layout_id: u64,
    type_id: i64,
    payload_size: i64,
    gc_ref_mask: u64,
) -> *mut u8 {
    allocate_object(layout_id, type_id as u32, payload_size, gc_ref_mask)
}

/// Allocation slow path for compiler-generated TLAB lowering.
///
/// `tlab_state` is the current thread's generated TLS block. Small allocations
/// retire an exhausted chunk, coordinate collection/threshold checks, refill,
/// initialize the first object, and return its payload. Large and stress-mode
/// allocations stay on the old/large-region path.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_alloc_slow(
    tlab_state: *mut GcTlabState,
    layout_id: u64,
    type_id: i64,
    payload_size: i64,
    gc_ref_mask: u64,
) -> *mut u8 {
    if tlab_state.is_null() || payload_size < 0 {
        return std::ptr::null_mut();
    }
    let Some(total_size) = (GC_HEADER_SIZE)
        .checked_add(payload_size as usize)
        .and_then(|size| size.checked_next_multiple_of(std::mem::align_of::<GcHeader>()))
    else {
        return std::ptr::null_mut();
    };
    let state_address = tlab_state as usize;
    let stress = gc_stress_enabled("alloc");
    let small = total_size <= GC_TLAB_MAX_OBJECT_SIZE;
    let fast_bytes = {
        let mut state = runtime().heap.lock().unwrap();
        register_tlab_state(&mut state, state_address);
        let record = state.tlab_states.get_mut(&state_address).unwrap();
        let fast = unsafe { tlab_state_at(state_address) }
            .fast_allocated_bytes
            .load(Ordering::Acquire);
        let fast_bytes = fast.saturating_sub(record.assist_observed_fast_bytes);
        record.assist_observed_fast_bytes = fast;
        sync_tlab_accounting(&mut state);
        if small {
            // A small-object miss means the active chunk has insufficient tail
            // space. Seal it before collection/refill.
            retire_tlab_locked(&mut state, state_address);
        }
        fast_bytes
    };
    assist_concurrent_mark(fast_bytes.saturating_add(total_size as u64));

    if !stress && allocation_should_minor_collect() {
        minor_collect_internal();
    }

    if stress || allocation_should_collect() {
        automatic_collect(stress);
    }

    if stress || !small {
        return allocate_old(layout_id, type_id as u32, payload_size, gc_ref_mask);
    }

    let mut state = runtime().heap.lock().unwrap();
    register_tlab_state(&mut state, state_address);
    let mut base = allocate_tlab_chunk(&mut state, state_address);
    if base.is_none() && (state.memory_limit_bytes.is_some() || state.soft_memory.enabled()) {
        drop(state);
        collect_for_budget();
        state = runtime().heap.lock().unwrap();
        register_tlab_state(&mut state, state_address);
        base = allocate_tlab_chunk(&mut state, state_address);
    }
    let Some(base) = base else {
        return allocation_failure(&state);
    };
    let Some(header) =
        initialize_object_at(base, total_size, type_id as u32, layout_id, gc_ref_mask)
    else {
        return std::ptr::null_mut();
    };
    state.tlab_chunks.last_mut().unwrap().mark_bitmap.mark(0);
    // SAFETY: the generated TLS block is aligned and remains alive for this
    // thread. Publish limit before cursor; generated code resumes only after
    // this slow-path call returns.
    let tls = unsafe { tlab_state_at(state_address) };
    tls.limit
        .store(base as usize + GC_TLAB_CHUNK_SIZE, Ordering::Release);
    tls.cursor
        .store(base as usize + total_size, Ordering::Release);
    state.allocated_bytes = state.allocated_bytes.saturating_add(total_size);
    state.young_allocated_bytes = state.young_allocated_bytes.saturating_add(total_size);
    state.total_allocs = state.total_allocs.saturating_add(1);
    state.total_allocated_bytes = state
        .total_allocated_bytes
        .saturating_add(total_size as u64);
    state.tlab_slow_allocations = state.tlab_slow_allocations.saturating_add(1);
    if state.allocated_bytes >= state.threshold_bytes {
        state.threshold_bytes = state.threshold_bytes.saturating_mul(2);
    }
    header.payload().as_ptr()
}

fn chunk_used_bytes(state: &GcState, chunk: &TlabChunk) -> usize {
    let start = chunk.base as usize;
    chunk
        .owner_state
        .and_then(|address| state.tlab_states.get(&address))
        .map(|record| {
            // SAFETY: an active chunk's registered TLS state remains valid.
            let cursor = unsafe { tlab_state_at(record.address) }
                .cursor
                .load(Ordering::Acquire);
            cursor
                .clamp(start, start.saturating_add(chunk.capacity))
                .saturating_sub(start)
        })
        .unwrap_or(chunk.used)
}

#[cfg(test)]
thread_local! { static RETIRED_LOOKUP_COMPARISONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

fn object_in_retired_chunk(
    chunk: &TlabChunk,
    address: usize,
    interior: bool,
) -> Option<HeapObject> {
    let relative = address.checked_sub(chunk.base as usize + GC_HEADER_SIZE)?;
    let index = if interior {
        chunk
            .header_offsets
            .partition_point(|&offset| {
                #[cfg(test)]
                RETIRED_LOOKUP_COMPARISONS.set(RETIRED_LOOKUP_COMPARISONS.get() + 1);
                usize::from(offset) <= relative
            })
            .checked_sub(1)?
    } else {
        chunk
            .header_offsets
            .binary_search_by(|offset| {
                #[cfg(test)]
                RETIRED_LOOKUP_COMPARISONS.set(RETIRED_LOOKUP_COMPARISONS.get() + 1);
                usize::from(*offset).cmp(&relative)
            })
            .ok()?
    };
    let offset = usize::from(chunk.header_offsets[index]);
    // SAFETY: retirement validated this immutable physical-header index; chunk
    // storage cannot be released while the caller holds the heap mutex.
    let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast())?;
    if !object.allocated() {
        return None;
    }
    let payload = object.payload().as_ptr() as usize;
    ((!interior && payload == address)
        || (interior && address >= payload && address < object.as_ptr() as usize + object.size()))
    .then_some(object)
}

fn old_region_objects(state: &GcState) -> impl Iterator<Item = HeapObject> + '_ {
    state.old_regions.iter().flat_map(|region| {
        region.allocations.keys().map(|&offset| {
            // SAFETY: the region allocation map owns the header at this offset.
            HeapObject::from_raw(unsafe { region.base.add(offset) }.cast()).unwrap()
        })
    })
}

fn find_old_region_object(state: &GcState, address: usize, interior: bool) -> Option<HeapObject> {
    let index = state
        .old_addresses
        .candidate(address.checked_sub(GC_HEADER_SIZE)?)?;
    state.old_regions[index].object_for_address(address, interior)
}

fn find_tlab_chunk(state: &GcState, address: usize) -> Option<&TlabChunk> {
    let chunk = &state.tlab_chunks[state
        .tlab_addresses
        .candidate(address.checked_sub(GC_HEADER_SIZE)?)?];
    let start = chunk.base as usize;
    let end = start.saturating_add(chunk_used_bytes(state, chunk));
    (address >= start + GC_HEADER_SIZE && address <= end).then_some(chunk)
}

fn payload_generation(state: &GcState, payload: *mut u8) -> Option<u8> {
    if payload.is_null() {
        return None;
    }
    let address = payload as usize;
    if let Some(object) = find_old_region_object(state, address, false) {
        return Some(object.generation());
    }
    tlab_payload_generation(state, address)
}

// Old-region objects cannot be young. Callers testing only for a young edge
// must not search the old-region and per-region allocation indexes first.
fn tlab_payload_generation(state: &GcState, address: usize) -> Option<u8> {
    if let Some(chunk) = find_tlab_chunk(state, address) {
        // Active generated chunks are exclusively young. Their header prefix
        // is still advancing, so do not inspect unpublished headers here.
        if chunk.owner_state.is_some() {
            return Some(GC_GENERATION_YOUNG);
        }
        return object_in_retired_chunk(chunk, address, false).map(|object| object.generation());
    }
    None
}

fn barrier_owner_payload(
    state: &GcState,
    owner_or_slot: *mut u8,
    destination_kind: i64,
) -> Option<usize> {
    if owner_or_slot.is_null() || destination_kind == GcStoreDestination::GlobalStatic as i64 {
        return None;
    }
    let address = owner_or_slot as usize;
    let interior = destination_kind == GcStoreDestination::IndirectReference as i64;
    if let Some(object) = find_old_region_object(state, address, interior) {
        return (object.generation() == GC_GENERATION_OLD)
            .then_some(object.payload().as_ptr() as usize);
    }
    if let Some(chunk) = find_tlab_chunk(state, address) {
        if chunk.owner_state.is_some() {
            return None;
        }
        if let Some(object) = object_in_retired_chunk(chunk, address, interior) {
            return (object.generation() == GC_GENERATION_OLD)
                .then_some(object.payload().as_ptr() as usize);
        }
    }
    None
}

// Acquire observes the epoch published before phase=1. Initial root handshakes
// run with SATB active; stopped remark flushes every producer before phase=0.
// 0 = inactive, 1 = concurrent mark, 2 = stopped remark.
static GC_MARK_PHASE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn record_satb_locked(state: &mut GcState, old: *mut u8) {
    let GcState {
        concurrent_cycle,
        satb,
        ..
    } = state;
    if let Some(cycle) = concurrent_cycle {
        if cycle.closing.load(Ordering::Acquire) {
            cycle.enqueue(old);
            return;
        }
        satb.record(std::thread::current().id(), old as usize, |value| {
            cycle.enqueue_satb_batch(value)
        });
    }
}

/// Log a logical reference deletion without a stable physical slot. Does not
/// reach a safepoint or trace payloads, so callers may hold a container lock.
pub(crate) fn satb_delete(old: *mut u8) {
    if !old.is_null() && GC_MARK_PHASE.load(Ordering::Acquire) != 0 {
        record_satb_locked(&mut runtime().heap.lock().unwrap(), old);
    }
}

fn flush_satb_current(retire: bool) {
    if !retire && GC_MARK_PHASE.load(Ordering::Acquire) == 0 {
        return;
    }
    crate::gc_mark_queue::assert_no_queue_lock_held("SATB buffer flush");
    let mut state = runtime().heap.lock().unwrap();
    let GcState {
        concurrent_cycle,
        satb,
        ..
    } = &mut *state;
    satb.flush_thread(std::thread::current().id(), retire, |value| {
        concurrent_cycle
            .as_ref()
            .expect("pending SATB entries require an active epoch")
            .enqueue_satb_batch(value);
    });
}

fn flush_satb_all_locked(state: &mut GcState) {
    let GcState {
        concurrent_cycle,
        satb,
        ..
    } = state;
    satb.flush_all(|value| {
        concurrent_cycle
            .as_ref()
            .expect("pending SATB entries require an active epoch")
            .enqueue_satb_batch(value);
    });
}

/// Fused pre-store barrier: retain the overwritten reference for SATB, publish
/// the new edge for the existing incremental marker, and remember old-to-young
/// edges. The caller must capture `old_value` before overwriting, including
/// removals/null stores. Only proven-null initialization passes null as old.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_write_barrier(
    owner: *mut u8,
    old_value: *mut u8,
    value: *mut u8,
    destination_kind: i64,
) {
    let marking_active = GC_MARK_PHASE.load(Ordering::Acquire) != 0;
    // A null store creates no generational edge; its deletion is relevant
    // only during SATB marking. Like null/null, an inactive deletion is a
    // no-op and is excluded from the processed-barrier telemetry counter.
    if value.is_null() && (old_value.is_null() || !marking_active) {
        return;
    }
    let _ =
        runtime()
            .write_barrier_calls
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |calls| {
                Some(calls.saturating_add(1))
            });
    // With no nursery ever published there are no old-to-young edges.
    // The activation handshake crosses pre-activation barrier/store pairs
    // before root snapshots; active epochs still take the full SATB path.
    if !marking_active && !runtime().tlab_ever_allocated.load(Ordering::Acquire) {
        return;
    }
    let mut state = runtime().heap.lock().unwrap();
    if marking_active {
        record_satb_locked(&mut state, old_value);
    }
    if let Some(cycle) = &state.concurrent_cycle {
        cycle.enqueue(value);
    }
    if tlab_payload_generation(&state, value as usize) != Some(GC_GENERATION_YOUNG) {
        return;
    }
    if let Some(owner_payload) = barrier_owner_payload(&state, owner, destination_kind) {
        state.dirty_cards.insert(owner_payload / GC_CARD_SIZE);
        let inserted = state.remembered_set.insert(owner_payload);
        if inserted {
            state.write_barrier_hits = state.write_barrier_hits.saturating_add(1);
        }
    }
}

fn allocate_object(layout_id: u64, type_id: u32, payload_size: i64, gc_ref_mask: u64) -> *mut u8 {
    if payload_size < 0 {
        return std::ptr::null_mut();
    }
    assist_concurrent_mark((payload_size as u64).saturating_add(GC_HEADER_SIZE as u64));
    if allocation_should_collect() {
        automatic_collect(gc_stress_enabled("alloc"));
    }
    allocate_old(layout_id, type_id, payload_size, gc_ref_mask)
}

/// Add a newly reserved regular region if it still has usable space.
fn index_old_region(state: &mut GcState, index: usize) {
    let region = &state.old_regions[index];
    if region.kind == RegionKind::Old {
        let available = region.available_span();
        if available >= GC_HEADER_SIZE {
            state.old_region_candidates.push((available, index));
        }
    }
}

fn allocate_old_region_object_locked(
    state: &mut GcState,
    layout_id: u64,
    type_id: u32,
    payload_size: usize,
    gc_ref_mask: u64,
    count_logical_allocation: bool,
) -> Option<HeapObject> {
    let total_size = GC_HEADER_SIZE.checked_add(payload_size)?;
    let span_size = total_size.checked_next_multiple_of(GC_REGION_MARK_GRANULE)?;
    let large = total_size > GC_LARGE_OBJECT_THRESHOLD;

    while state
        .old_region_candidates
        .peek()
        .is_some_and(|&(_, index)| state.old_regions[index].sweep_quarantined)
    {
        // Each stale candidate is discarded at most once during the sweep.
        state.old_region_candidates.pop();
    }
    let candidate = state
        .old_region_candidates
        .peek()
        .copied()
        .filter(|(available, _)| !large && *available >= span_size);
    let (object, reused) = if let Some((_, index)) = candidate {
        let allocated = state.old_regions[index].allocate_object(
            type_id,
            layout_id,
            gc_ref_mask,
            payload_size,
        )?;
        let available = state.old_regions[index].available_span();
        let mut entry = state
            .old_region_candidates
            .peek_mut()
            .expect("candidate exists");
        if available >= GC_HEADER_SIZE {
            *entry = (available, index);
        } else {
            std::collections::binary_heap::PeekMut::pop(entry);
        }
        allocated
    } else {
        let capacity = if large { span_size } else { GC_OLD_REGION_SIZE };
        if !can_reserve(state, capacity) {
            return None;
        }
        let kind = if large {
            RegionKind::LargeObject
        } else {
            RegionKind::Old
        };
        let mut region = OldRegion::new(kind, capacity)?;
        let allocated = region.allocate_object(type_id, layout_id, gc_ref_mask, payload_size)?;
        state
            .old_addresses
            .insert(region.base as usize, state.old_regions.len());
        state.old_regions.push(region);
        state.old_reserved_bytes += capacity;
        index_old_region(state, state.old_regions.len() - 1);
        allocated
    };

    state.allocated_bytes = state.allocated_bytes.saturating_add(object.size());
    state.old_region_allocations = state.old_region_allocations.saturating_add(1);
    if reused {
        state.old_region_reuses = state.old_region_reuses.saturating_add(1);
    }
    if count_logical_allocation {
        state.total_allocs = state.total_allocs.saturating_add(1);
        state.total_allocated_bytes = state
            .total_allocated_bytes
            .saturating_add(object.size() as u64);
    }
    Some(object)
}

fn allocate_old(layout_id: u64, type_id: u32, payload_size: i64, gc_ref_mask: u64) -> *mut u8 {
    if payload_size < 0 {
        return std::ptr::null_mut();
    }
    let payload_size = payload_size as usize;
    let mut state = runtime().heap.lock().unwrap();
    sync_tlab_accounting(&mut state);
    let mut header = allocate_old_region_object_locked(
        &mut state,
        layout_id,
        type_id,
        payload_size,
        gc_ref_mask,
        true,
    );
    if header.is_none() && (state.memory_limit_bytes.is_some() || state.soft_memory.enabled()) {
        drop(state);
        collect_for_budget();
        state = runtime().heap.lock().unwrap();
        header = allocate_old_region_object_locked(
            &mut state,
            layout_id,
            type_id,
            payload_size,
            gc_ref_mask,
            true,
        );
    }
    let Some(header) = header else {
        return allocation_failure(&state);
    };
    state.tlab_slow_allocations = state.tlab_slow_allocations.saturating_add(1);
    if header.size() > GC_TLAB_MAX_OBJECT_SIZE {
        state.tlab_large_allocations = state.tlab_large_allocations.saturating_add(1);
    }
    if state.allocated_bytes >= state.threshold_bytes {
        state.threshold_bytes = state.threshold_bytes.saturating_mul(2);
    }
    header.payload().as_ptr()
}

/// Trigger a full collection. Normal old marking/closure/sweep are concurrent;
/// legacy trace callbacks or worker failures use the stopped recovery path.
///
/// # GC root semantics — why local objects survive an inner gc_collect()
///
/// Every GC-managed local variable is backed by a stack slot registered with
/// `willow_push_root`.  The slot is popped only when the variable's scope ends
/// (i.e. when the function returns or the block exits).  While the variable is
/// in scope, the object is reachable from the root graph and the collector
/// correctly keeps it alive.
///
/// Consequence: calling `gc_collect()` from **inside** a function that holds
/// live GC-managed locals will **not** free those locals.  They will be freed
/// on the first `gc_collect()` that runs **after** the function has returned
/// and the root slots have been popped.
///
/// This is intentional and correct.  The GC cannot distinguish "I'm done with
/// this variable" from "I might use it again later in the same scope".  To
/// reclaim an object eagerly, arrange for it to go out of scope (return from
/// the function, or wrap the allocation in a smaller scope if block-scoped
/// roots are supported) before calling `gc_collect()`.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_collect() {
    collect_internal();
}

/// Trigger a stop-the-world minor collection. Explicit roots are promoted
/// in-place because current generated SSA aliases are not reloaded after every
/// allocation; young objects reachable only through heap slots are copied to
/// the non-moving old generation and those slots are updated.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_minor_collect() {
    minor_collect_internal();
}

/// Whether the GC stress mode `kind` is active via the `WILLOW_GC_STRESS`
/// environment variable. The variable is a comma-separated list of modes; `all`
/// enables every mode (willow-lpn.8).
///
/// Modes (for local test runs / CI):
/// - `alloc`     — collect at every heap allocation boundary.
/// - `minor`     — force a minor collection at every TLAB refill.
/// - `await`     — collect around await boundaries: before/after the scheduler
///   polls a task (so suspend/resume and task-completion are stressed).
/// - `scheduler` — collect around scheduler operations: spawn, wake, park,
///   completion, and channel-waiter registration.
/// - `all`       — enable all of the above.
///
/// Example: `WILLOW_GC_STRESS=alloc cargo test`, or `WILLOW_GC_STRESS=all`.
///
/// The variable is read once: every slow-path allocation asks for it several
/// times, and `std::env::var` costs a `getenv` scan plus, on Windows, a heap
/// allocation for the key on each call (willow-ssl7.12).
pub(crate) fn gc_stress_enabled(kind: &str) -> bool {
    static MODES: LazyLock<Vec<String>> = LazyLock::new(|| {
        parse_gc_stress_modes(&std::env::var("WILLOW_GC_STRESS").unwrap_or_default())
    });
    #[cfg(test)]
    if let Some(modes) = &*gc_stress_override()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
    {
        return modes.iter().any(|mode| mode == "all" || mode == kind);
    }
    MODES.iter().any(|mode| mode == "all" || mode == kind)
}

fn parse_gc_stress_modes(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|mode| !mode.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Process-wide stress-mode override for tests that cannot restart the process
/// with a different `WILLOW_GC_STRESS`. `None` restores the environment value.
#[cfg(test)]
fn gc_stress_override() -> &'static Mutex<Option<Vec<String>>> {
    static OVERRIDE: Mutex<Option<Vec<String>>> = Mutex::new(None);
    &OVERRIDE
}

#[cfg(test)]
pub(crate) fn set_gc_stress_for_test(modes: Option<&str>) {
    *gc_stress_override()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = modes.map(parse_gc_stress_modes);
}

/// Scoped override; callers must hold `runtime_test_guard` until this drops.
#[cfg(test)]
struct GcStressTestScope(Option<Vec<String>>);

#[cfg(test)]
impl GcStressTestScope {
    fn normal() -> Self {
        let mut modes = gc_stress_override()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Self(modes.replace(Vec::new()))
    }
}

#[cfg(test)]
impl Drop for GcStressTestScope {
    fn drop(&mut self) {
        *gc_stress_override()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = self.0.take();
    }
}

pub(crate) fn stress_collect(kind: &str) {
    if gc_stress_enabled(kind) {
        collect_internal();
    }
}

/// Return the total bytes currently on the GC heap (header + payload).
pub(crate) fn telemetry_heap_snapshot() -> (
    crate::gc_telemetry::GcCountersV1,
    crate::gc_telemetry::GcHeapV1,
) {
    use crate::gc_telemetry::{GcCountersV1, GcHeapV1};
    let mut state = runtime().heap.lock().unwrap_or_else(|p| p.into_inner());
    sync_tlab_accounting(&mut state);
    let old_reserved = state.old_reserved_bytes as u64;
    let reserved = old_reserved.saturating_add(state.tlab_reserved_bytes as u64);
    (
        GcCountersV1 {
            allocation_count: state.total_allocs,
            allocation_bytes: state.total_allocated_bytes,
            freed_objects: state.total_frees,
            released_bytes: state.released_bytes,
            tlab_fast_allocations: state.tlab_fast_allocations,
            tlab_slow_allocations: state.tlab_slow_allocations,
            tlab_refills: state.tlab_refills,
            promoted_objects: state.promoted_objects,
            promoted_bytes: state.promoted_bytes,
            moved_objects: state.moved_objects,
            barrier_calls: runtime().write_barrier_calls.load(Ordering::Relaxed),
            barrier_hits: state.write_barrier_hits,
        },
        GcHeapV1 {
            occupied_bytes: state.allocated_bytes as u64,
            young_occupied_bytes: state.young_allocated_bytes as u64,
            reserved_bytes: reserved,
            committed_bytes: reserved,
            old_reserved_bytes: old_reserved,
            nursery_reserved_bytes: state.tlab_reserved_bytes as u64,
            old_regions: state.old_regions.len() as u64,
            remembered_objects: state.remembered_set.len() as u64,
            dirty_cards: state.dirty_cards.len() as u64,
            major_trigger_bytes: major_trigger(&state) as u64,
            minor_trigger_bytes: state.nursery_threshold_bytes as u64,
        },
    )
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_allocated_bytes() -> i64 {
    let mut state = runtime().heap.lock().unwrap();
    sync_tlab_accounting(&mut state);
    state.allocated_bytes as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_tlab_fast_allocations() -> i64 {
    let mut state = runtime().heap.lock().unwrap();
    sync_tlab_accounting(&mut state);
    state.tlab_fast_allocations as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_tlab_slow_allocations() -> i64 {
    runtime().heap.lock().unwrap().tlab_slow_allocations as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_tlab_refills() -> i64 {
    runtime().heap.lock().unwrap().tlab_refills as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_tlab_large_allocations() -> i64 {
    runtime().heap.lock().unwrap().tlab_large_allocations as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_tlab_reserved_bytes() -> i64 {
    runtime().heap.lock().unwrap().tlab_reserved_bytes as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_minor_collections() -> i64 {
    runtime().heap.lock().unwrap().minor_collections as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_promoted_objects() -> i64 {
    runtime().heap.lock().unwrap().promoted_objects as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_moved_objects() -> i64 {
    runtime().heap.lock().unwrap().moved_objects as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_remembered_set_size() -> i64 {
    runtime().heap.lock().unwrap().remembered_set.len() as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_dirty_card_count() -> i64 {
    runtime().heap.lock().unwrap().dirty_cards.len() as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_write_barrier_hits() -> i64 {
    runtime().heap.lock().unwrap().write_barrier_hits as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_old_region_count() -> i64 {
    let state = runtime().heap.lock().unwrap();
    let pinned = state
        .tlab_chunks
        .iter()
        .filter(|chunk| chunk.kind == RegionKind::Pinned)
        .count();
    (state.old_regions.len() + pinned) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_old_region_reserved_bytes() -> i64 {
    let state = runtime().heap.lock().unwrap();
    let regular: usize = state.old_regions.iter().map(|region| region.capacity).sum();
    let pinned: usize = state
        .tlab_chunks
        .iter()
        .filter(|chunk| chunk.kind == RegionKind::Pinned)
        .map(|chunk| chunk.capacity)
        .sum();
    regular.saturating_add(pinned) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_old_region_live_bytes() -> i64 {
    let state = runtime().heap.lock().unwrap();
    let regular: usize = state
        .old_regions
        .iter()
        .map(|region| region.live_bytes)
        .sum();
    let pinned: usize = state
        .tlab_chunks
        .iter()
        .filter(|chunk| chunk.kind == RegionKind::Pinned)
        .map(|chunk| chunk.live_bytes)
        .sum();
    regular.saturating_add(pinned) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_old_region_fragmentation_bytes() -> i64 {
    let state = runtime().heap.lock().unwrap();
    let regular: usize = state
        .old_regions
        .iter()
        .map(OldRegion::fragmentation_bytes)
        .sum();
    let pinned: usize = state
        .tlab_chunks
        .iter()
        .filter(|chunk| chunk.kind == RegionKind::Pinned)
        .map(|chunk| chunk.used.saturating_sub(chunk.live_bytes))
        .sum();
    regular.saturating_add(pinned) as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_large_object_region_count() -> i64 {
    runtime()
        .heap
        .lock()
        .unwrap()
        .old_regions
        .iter()
        .filter(|region| region.kind == RegionKind::LargeObject)
        .count() as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_pinned_region_count() -> i64 {
    runtime()
        .heap
        .lock()
        .unwrap()
        .tlab_chunks
        .iter()
        .filter(|chunk| chunk.kind == RegionKind::Pinned)
        .count() as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_old_region_allocations() -> i64 {
    runtime().heap.lock().unwrap().old_region_allocations as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_old_region_reuses() -> i64 {
    runtime().heap.lock().unwrap().old_region_reuses as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_old_regions_released() -> i64 {
    runtime().heap.lock().unwrap().old_regions_released as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_major_collections() -> i64 {
    runtime().heap.lock().unwrap().major_collections as i64
}

/// Number of collections skipped because a foreign thread owned the root stack
/// (willow-6fv.2). Lets a GC-stress test assert it is actually collecting rather
/// than silently skipping most of the time.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_skipped_collections() -> i64 {
    runtime()
        .skipped_foreign_owner_collections
        .load(std::sync::atomic::Ordering::Relaxed) as i64
}

/// Test-only: number of currently registered GC mutators (willow-6fv.5.6).
#[cfg(test)]
pub(crate) fn registered_mutator_count() -> usize {
    let (lock, _) = &runtime().coord;
    lock.lock().unwrap().mutators.len()
}

// ---------------------------------------------------------------------------
// Internal collection
// ---------------------------------------------------------------------------

/// Extend tracing beyond the inline mask. Bitmap descriptors are immutable
/// static data whose lifetime covers every object using them, including moves.
fn append_bitmap_slots(object: HeapObject, slots: &mut Vec<*mut *mut u8>) {
    let metadata = object.trace_metadata();
    if metadata.type_id != willow_abi::GC_BITMAP_TYPE_ID {
        return;
    }
    let descriptor = metadata.layout_id as *const u64;
    // SAFETY: only willow_gc_alloc_bitmap installs the reserved tracing type, after validating a
    // compiler-owned, process-lifetime descriptor. Moving GC copies the pointer.
    let count = unsafe { *descriptor } as usize;
    let payload_words = metadata.payload_size / GC_STORAGE_WORD_BYTES;
    for word in 1..count {
        let mut bits = unsafe { *descriptor.add(word + 1) };
        while bits != 0 {
            let bit = bits.trailing_zeros() as usize;
            let index = word * 64 + bit;
            if index < payload_words {
                slots.push(object.payload_slot(index));
            }
            bits &= bits - 1;
        }
    }
}

fn object_reference_slots(
    object: HeapObject,
    trace_registry: &HashMap<u32, TraceFn>,
) -> Vec<*mut *mut u8> {
    let metadata = object.trace_metadata();
    let payload_words = metadata.payload_size / GC_STORAGE_WORD_BYTES;
    let mut slots = Vec::new();
    for index in 0..payload_words.min(64) {
        if (metadata.gc_ref_mask & (1u64 << index)) != 0 {
            slots.push(object.payload_slot(index));
        }
    }
    append_bitmap_slots(object, &mut slots);
    if let Some(trace) = trace_registry.get(&metadata.type_id).copied() {
        // SAFETY: trace is registered for this runtime type and exposes mutable
        // reference slots without allocating GC objects.
        unsafe { trace(object.payload().as_ptr(), &mut slots) };
    }
    slots
}

fn verify_remembered_set(
    state: &GcState,
    trace_registry: &HashMap<u32, TraceFn>,
) -> Result<(), String> {
    let mut old_objects = Vec::new();
    for object in old_region_objects(state) {
        if object.allocated() && object.generation() == GC_GENERATION_OLD {
            old_objects.push(object);
        }
    }
    for chunk in &state.tlab_chunks {
        let mut offset = 0usize;
        while offset < chunk.used {
            // SAFETY: barrier verification runs after every TLAB is retired.
            let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast())
                .expect("TLAB header address is non-null");
            if object.allocated() && object.generation() == GC_GENERATION_OLD {
                old_objects.push(object);
            }
            offset += object.size();
        }
    }
    for object in old_objects {
        let owner = object.payload().as_ptr() as usize;
        for slot in object_reference_slots(object, trace_registry) {
            if slot.is_null() {
                continue;
            }
            // SAFETY: trace/layout slots are readable under stop-the-world.
            let child = unsafe { *slot };
            if payload_generation(state, child) == Some(GC_GENERATION_YOUNG)
                && !state.remembered_set.contains(&owner)
            {
                return Err(format!(
                    "old object 0x{owner:x} contains young reference 0x{:x} without a remembered-set entry",
                    child as usize
                ));
            }
        }
    }
    Ok(())
}

fn verify_old_region_metadata(state: &GcState) -> Result<(), String> {
    if !state
        .old_addresses
        .matches(state.old_regions.len(), |index| {
            state.old_regions.get(index).map(OldRegion::start)
        })
    {
        return Err("old-region address index mismatch".into());
    }
    if !state
        .tlab_addresses
        .matches(state.tlab_chunks.len(), |index| {
            state
                .tlab_chunks
                .get(index)
                .map(|chunk| chunk.base as usize)
        })
    {
        return Err("TLAB address index mismatch".into());
    }
    for region in &state.old_regions {
        if region.used > region.capacity {
            return Err(format!(
                "{:?} region 0x{:x} used {} bytes beyond capacity {}",
                region.kind,
                region.start(),
                region.used,
                region.capacity
            ));
        }
        if region.kind == RegionKind::LargeObject && region.allocations.len() != 1 {
            return Err(format!(
                "large-object region 0x{:x} owns {} allocations instead of one",
                region.start(),
                region.allocations.len()
            ));
        }

        let mut intervals: Vec<(usize, usize, &'static str)> = Vec::new();
        let mut computed_live = 0usize;
        for (&offset, &span_size) in &region.allocations {
            if !offset.is_multiple_of(GC_REGION_MARK_GRANULE)
                || !span_size.is_multiple_of(GC_REGION_MARK_GRANULE)
                || offset.saturating_add(span_size) > region.used
            {
                return Err(format!(
                    "region 0x{:x} has invalid allocation span offset={offset} size={span_size} used={}",
                    region.start(),
                    region.used
                ));
            }
            // SAFETY: the allocation map owns a header at `offset`.
            let object = HeapObject::from_raw(unsafe { region.base.add(offset) }.cast())
                .expect("region object address is non-null");
            if !object.allocated()
                || object.generation() != GC_GENERATION_OLD
                || object.size() > span_size
            {
                return Err(format!(
                    "region object 0x{:x} has inconsistent header metadata",
                    object.as_ptr() as usize
                ));
            }
            if !region.mark_bitmap.is_marked(offset) {
                return Err(format!(
                    "region object 0x{:x} is absent from its mark bitmap",
                    object.as_ptr() as usize
                ));
            }
            computed_live = computed_live.saturating_add(object.size());
            intervals.push((offset, offset + span_size, "allocation"));
        }
        if computed_live != region.live_bytes {
            return Err(format!(
                "region 0x{:x} live-byte mismatch: metadata={}, computed={computed_live}",
                region.start(),
                region.live_bytes
            ));
        }
        for span in &region.free_spans {
            if span.size == 0 || span.offset.saturating_add(span.size) > region.used {
                return Err(format!(
                    "region 0x{:x} has invalid free span offset={} size={}",
                    region.start(),
                    span.offset,
                    span.size
                ));
            }
            intervals.push((span.offset, span.offset + span.size, "free"));
        }
        intervals.sort_unstable_by_key(|interval| interval.0);
        for pair in intervals.windows(2) {
            if pair[0].1 > pair[1].0 {
                return Err(format!(
                    "region 0x{:x} has overlapping {} and {} spans",
                    region.start(),
                    pair[0].2,
                    pair[1].2
                ));
            }
        }
    }

    for chunk in &state.tlab_chunks {
        if chunk.used > chunk.capacity {
            return Err(format!(
                "{:?} region 0x{:x} used {} bytes beyond capacity {}",
                chunk.kind, chunk.base as usize, chunk.used, chunk.capacity
            ));
        }
        if chunk.kind == RegionKind::Pinned {
            let mut offset = 0usize;
            let mut live = 0usize;
            while offset < chunk.used {
                // SAFETY: pinned regions retain the sequential TLAB layout.
                let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast())
                    .expect("pinned-region object address is non-null");
                if object.allocated() {
                    if object.generation() != GC_GENERATION_OLD
                        || !chunk.mark_bitmap.is_marked(offset)
                    {
                        return Err(format!(
                            "pinned-region object 0x{:x} has inconsistent generation/mark metadata",
                            object.as_ptr() as usize
                        ));
                    }
                    live = live.saturating_add(object.size());
                }
                offset += object.size();
            }
            if live != chunk.live_bytes {
                return Err(format!(
                    "pinned region 0x{:x} live-byte mismatch: metadata={}, computed={live}",
                    chunk.base as usize, chunk.live_bytes
                ));
            }
        }
    }
    for region in &state.old_regions {
        if !region.free_spans.is_consistent() {
            return Err("old-region free span index mismatch".into());
        }
        if region.largest_free_span
            != region
                .free_spans
                .iter()
                .map(|span| span.size)
                .max()
                .unwrap_or(0)
        {
            return Err("old-region largest free span mismatch".into());
        }
    }
    let expected: HashSet<_> = state
        .old_regions
        .iter()
        .enumerate()
        .filter(|(_, region)| region.kind == RegionKind::Old)
        .map(|(index, region)| (region.available_span(), index))
        .filter(|(available, _)| *available >= GC_HEADER_SIZE)
        .collect();
    if expected.len() != state.old_region_candidates.len()
        || expected != state.old_region_candidates.iter().copied().collect()
    {
        return Err("old-region allocation candidate index mismatch".into());
    }
    if state
        .old_regions
        .iter()
        .map(|region| region.capacity)
        .sum::<usize>()
        != state.old_reserved_bytes
    {
        return Err("old-region reservation accounting mismatch".into());
    }
    Ok(())
}

fn automatic_collect(stress: bool) {
    if stress || !coordinator::request() {
        collect_internal();
    }
}

fn collect_internal() {
    // Collector election (willow-6fv.5.6): only one thread collects at a time.
    // A thread that cannot become the collector must NOT block on runtime().collect_lock —
    // if a stop-the-world collection is in progress, the holder is waiting for
    // this thread to reach a safepoint, so blocking here would deadlock. Instead
    // reach a safepoint (parking if a STW is pending) and let the active
    // collector proceed.
    if runtime()
        .stop_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        willow_gc_safepoint();
        return;
    }
    let _serialize = match runtime().collect_lock.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(poison)) => poison.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            // Another collector is active; cooperate by reaching a safepoint.
            willow_gc_safepoint();
            return;
        }
    };
    // A legacy root owner outside the registry cannot publish a snapshot.
    // Other registered workers do not make that foreign stack safe to scan.
    if foreign_root_stack_owner_active() {
        // Cannot scan another thread's root stack, so skip (safe). Count it so a
        // GC-stress run can detect when it is mostly skipping rather than
        // collecting (willow-6fv.2).
        let skipped = runtime()
            .skipped_foreign_owner_collections
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if std::env::var("WILLOW_GC_LOG").is_ok() {
            eprintln!(
                "[gc] collection skipped because a foreign root stack owner is active (total skipped={skipped})"
            );
        }
        return;
    }
    let gc_log = std::env::var("WILLOW_GC_LOG").is_ok();

    let cycle = crate::gc_telemetry::Cycle::begin(crate::gc_telemetry::CycleKind::Major);

    // First every mutator crosses barrier activation, then each publishes roots
    // independently. Payload tracing is gated until the activation round ends;
    // late TLAB starts are retried after the root-publication round.
    let started = std::time::Instant::now();
    let (heap_before, marking) = root_handshake::begin();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // Bounded chunks allow a producer-heavy workload to reach remark;
        // finishing relies on quiescing producers, never a racy empty sample.
        let budget = marking.objects.len().saturating_mul(4).max(1024);
        {
            let mut workers = MARK_WORKERS.lock().unwrap();
            if workers.is_none() {
                *workers = mark_workers::Pool::new(mark_workers::configured_count()).ok();
            }
            if let Some(pool) = workers.as_ref().filter(|pool| pool.count() != 0) {
                pool.run(&marking, budget);
            } else {
                // Disabled/unavailable background workers use the stopped
                // graph tracer. Never silently substitute unregistered readers.
                marking.worker_failed.store(true, Ordering::Release);
            }
        }
        let closed = mark_closure::finish(&marking, started);
        let mut pause_ns = 0;
        let result = if let Some((work, plan)) = closed {
            (work, Some(plan), 0)
        } else {
            let remark_started = std::time::Instant::now();
            let result = with_stw(
                crate::gc_telemetry::stops::StopReason::Remark,
                |coord, stop_work| {
                    GC_MARK_PHASE.store(2, Ordering::Release);
                    let mut state = runtime().heap.lock().unwrap();
                    flush_satb_all_locked(&mut state);
                    retire_tlabs_with_work(&mut state, stop_work);
                    drop(state);
                    let mut roots = all_registered_stack_roots(coord);
                    roots.extend(runtime_roots_snapshot());
                    stop_work.root_values += roots.len() as u64;
                    for &root in &roots {
                        checked_payload_to_header(root, "GC remark root");
                        marking.enqueue(root);
                    }
                    let fallback = marking.worker_failed.load(Ordering::Acquire);
                    if fallback {
                        crate::gc_telemetry::workers::record_failure(
                            crate::gc_telemetry::workers::Failure::Fallback,
                        );
                    }
                    marking.queue.flush_all_locals();
                    if !fallback {
                        marking.drain(usize::MAX);
                        assert!(
                            marking.queue.snapshot().is_drained(),
                            "remark left outstanding marking work"
                        );
                    }
                    for &address in marking.unindexed.lock().unwrap().iter() {
                        checked_payload_to_header(address as *mut u8, "GC concurrent graph edge");
                    }
                    // This remaining remark pass also supplies fallback candidates;
                    // never enumerate the entire heap a second time on failure.
                    let objects = if fallback {
                        epoch_objects(&runtime().heap.lock().unwrap(), stop_work)
                    } else {
                        Vec::new()
                    };
                    let deferred = if fallback {
                        // Re-trace every discovered object, including SATB-deleted
                        // roots and a job whose concurrent hook unwound. The STW
                        // hook is the authoritative fallback for native containers.
                        objects
                            .iter()
                            .map(|object| object.payload().as_ptr() as usize)
                            .filter(|&address| marking.is_marked(address))
                            .collect()
                    } else {
                        marking.deferred.lock().unwrap().clone()
                    };
                    let deferred_set: HashSet<_> = deferred.iter().copied().collect();
                    {
                        for object in objects {
                            let address = object.payload().as_ptr() as usize;
                            if (!marking.objects.contains(address) || marking.is_marked(address))
                                && !deferred_set.contains(&address)
                            {
                                object.begin_trace();
                            }
                        }
                    }
                    // Extension callbacks without a concurrent contract remain sound.
                    let legacy = mark_worklist(
                        deferred.into_iter().map(|value| value as *mut u8).collect(),
                        (!fallback).then_some((&marking.objects, &deferred_set)),
                    );
                    let mut work = *marking.work.lock().unwrap();
                    work.marked_bytes = work.marked_bytes.saturating_add(legacy.marked_bytes);
                    work.scanned_bytes = work.scanned_bytes.saturating_add(legacy.scanned_bytes);
                    work.root_scan_bytes = work
                        .root_scan_bytes
                        .saturating_add(roots.len() as u64 * std::mem::size_of::<usize>() as u64);
                    work.mark_ns = crate::gc_telemetry::elapsed_ns(started);
                    GC_MARK_PHASE.store(0, Ordering::Release);
                    runtime().heap.lock().unwrap().concurrent_cycle = None;
                    let remaining = marking.queue.end_epoch();
                    assert!(
                        fallback || remaining == 0,
                        "completed mark epoch retained work"
                    );
                    if fallback {
                        (work, None, sweep_with_work(stop_work))
                    } else {
                        let plan = sweep::prepare(
                            &mut runtime().heap.lock().unwrap(),
                            Some(marking.clone()),
                        );
                        (work, Some(plan), 0)
                    }
                },
            );
            pause_ns = crate::gc_telemetry::elapsed_ns(remark_started);
            result
        };
        let (work, plan, stopped_freed) = result;
        let freed = plan.map_or(stopped_freed, sweep::concurrent);
        let mut state = runtime().heap.lock().unwrap();
        sync_tlab_accounting(&mut state);
        let after = state.allocated_bytes as u64;
        let previous_goal = state.threshold_bytes as u64;
        state.threshold_bytes = state.allocated_bytes.saturating_mul(2).max(1024 * 1024);
        state.last_major_live_bytes = after;
        state.last_major_mark_work = work
            .marked_bytes
            .saturating_add(work.scanned_bytes)
            .saturating_add(work.descriptor_bytes)
            .saturating_add(work.root_scan_bytes);
        let cpu = (!work.cpu_incomplete && pause_ns == 0).then_some(work.cpu_ns);
        state.pacer.mark(
            work.marked_bytes
                .saturating_add(work.scanned_bytes)
                .saturating_add(work.descriptor_bytes),
            cpu,
        );
        let bytes = state.total_allocated_bytes;
        state
            .pacer
            .allocation(crate::gc_telemetry::timestamp_ns(), bytes);
        if state.pacer.enabled() {
            let decision = state.pacer.decision(pacer::Inputs {
                previous_goal,
                ..pacer_inputs(&state)
            });
            state.threshold_bytes = decision.goal.min(usize::MAX as u64) as usize;
            state.pacer_trigger = decision.trigger;
        }
        let memory_input = memory_inputs(&state);
        state.soft_memory.completed(heap_before, memory_input);
        if gc_log {
            let decision = state.soft_memory.decide(memory_input);
            eprintln!(
                "[gc] soft_memory goal={} trigger={} overshoot={} reason={:?}",
                decision.goal, decision.trigger, decision.overshoot, decision.reason
            );
        }
        drop(state);
        (after, freed, work, pause_ns)
    }));
    let (heap_after, freed, work, pause_ns) = match result {
        Ok(result) => result,
        Err(payload) => {
            // Failure leaves the graph allocated. Remove transient marks under
            // another stop so the next collection can safely retry.
            with_stw(
                crate::gc_telemetry::stops::StopReason::Recovery,
                |_, stop_work| {
                    let mut state = runtime().heap.lock().unwrap();
                    flush_satb_all_locked(&mut state);
                    GC_MARK_PHASE.store(0, Ordering::Release);
                    state.concurrent_cycle = None;
                    retire_tlabs_with_work(&mut state, stop_work);
                    for object in epoch_objects(&state, stop_work) {
                        object.clear_mark();
                    }
                    marking.queue.end_epoch();
                },
            );
            std::panic::resume_unwind(payload)
        }
    };
    let event = cycle.finish_concurrent(heap_before, heap_after, work, pause_ns, freed as u64);

    if gc_log {
        let state = runtime().heap.lock().unwrap();
        eprintln!(
            "gc: heap_before={}B freed={}B heap_after={}B total_allocs={} total_frees={}",
            heap_before, freed, state.allocated_bytes, state.total_allocs, state.total_frees,
        );
    }
    drop(_serialize);
    crate::gc_telemetry::emit_cycle(event);
}

#[cfg(test)]
static SWEEP_REGION_VISITS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static SWEEP_OBJECT_VISITS: AtomicUsize = AtomicUsize::new(0);

/// Sweep the region-backed old-object index and nursery/pinned regions without
/// moving survivors. Dead old spans return to their owning region's free list;
/// completely empty regions are released. Returns total logical bytes freed.
fn sweep() -> usize {
    sweep_with_work(&mut crate::gc_telemetry::stops::StopWorkV2::default())
}

fn sweep_with_work(stop_work: &mut crate::gc_telemetry::stops::StopWorkV2) -> usize {
    sweep::stopped(stop_work)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Given a payload pointer, return the GcHeader pointer just before it.
fn payload_to_header(payload: *mut u8) -> *mut GcHeader {
    GcPayload::from_raw(payload)
        .map(HeapObject::from_payload)
        .map(HeapObject::as_ptr)
        .unwrap_or(std::ptr::null_mut())
}

#[cfg(debug_assertions)]
fn checked_payload_to_header(payload: *mut u8, context: &str) -> *mut GcHeader {
    validate_payload_pointer(payload, context).unwrap_or_else(|message| {
        panic!("willow gc: invalid GC pointer in {context}: {message}");
    })
}

#[cfg(not(debug_assertions))]
fn checked_payload_to_header(payload: *mut u8, _context: &str) -> *mut GcHeader {
    payload_to_header(payload)
}

#[cfg(debug_assertions)]
fn validate_payload_pointer(payload: *mut u8, _context: &str) -> Result<*mut GcHeader, String> {
    validate_payload_pointer_locked(&runtime().heap.lock().unwrap(), payload)
}

#[cfg(debug_assertions)]
fn validate_payload_pointer_locked(
    state: &GcState,
    payload: *mut u8,
) -> Result<*mut GcHeader, String> {
    if payload.is_null() {
        return Err("null is not a traceable GC payload pointer".to_string());
    }
    let payload_addr = payload as usize;
    let header_size = std::mem::size_of::<GcHeader>();
    let header_align = std::mem::align_of::<GcHeader>();
    if let Some(object) = find_old_region_object(state, payload_addr, false) {
        let header_addr = object.as_ptr() as usize;
        let size = object.size();
        if !header_addr.is_multiple_of(header_align) {
            return Err(format!(
                "header for payload 0x{payload_addr:x} is not {header_align}-byte aligned"
            ));
        }
        if size < header_size {
            return Err(format!(
                "header for payload 0x{payload_addr:x} has invalid size {size}"
            ));
        }
        return Ok(object.as_ptr());
    }
    if let Some(chunk) = find_tlab_chunk(state, payload_addr) {
        let offset = payload_addr - GC_HEADER_SIZE - chunk.base as usize;
        if offset.is_multiple_of(GC_REGION_MARK_GRANULE) && chunk.mark_bitmap.is_marked(offset) {
            // Acquire of the start bit observes completed header initialization,
            // even for a concurrently advancing generated TLAB. Never walk an
            // active prefix: its cursor can precede initialization of that tail.
            let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast()).unwrap();
            let size = object.size();
            if object.allocated() && size >= GC_HEADER_SIZE && size <= chunk.capacity - offset {
                return Ok(object.as_ptr());
            }
        }
    }
    Err(format!(
        "0x{payload_addr:x} is not the payload pointer of any object in the current GC heap"
    ))
}

/// True when the current thread is a registered GC mutator (willow-6fv.5.6).
/// Registered mutators each legitimately own their own thread-local root stack;
/// cross-thread safety is handled by stop-the-world scanning, so they bypass the
/// legacy single-mutator `runtime().root_stack_owner` guard below.
fn current_thread_is_registered() -> bool {
    let current = std::thread::current().id();
    let (lock, _) = &runtime().coord;
    lock.lock().unwrap().mutators.contains_key(&current)
}

fn claim_root_stack_owner() {
    // Registered mutators are coordinated via the registry + STW, not the
    // single-owner guard (willow-6fv.5.6).
    if current_thread_is_registered() {
        return;
    }
    let current = std::thread::current().id();
    let mut owner = runtime().root_stack_owner.lock().unwrap();
    match *owner {
        Some(existing) if existing != current => {
            eprintln!("willow gc: explicit root stacks are single-mutator in the current runtime");
            std::process::abort();
        }
        _ => *owner = Some(current),
    }
}

fn release_root_stack_owner_if_empty() {
    if current_thread_is_registered() {
        return;
    }
    clear_root_stack_owner_if_empty();
}

// Does not reacquire coord: unregister calls this while holding that lock.
fn clear_root_stack_owner_if_empty() {
    let is_empty = ROOT_STACK.with(|rs| rs.borrow().is_empty());
    if !is_empty {
        return;
    }
    let current = std::thread::current().id();
    let mut owner = runtime().root_stack_owner.lock().unwrap();
    if owner.as_ref().is_some_and(|existing| *existing == current) {
        *owner = None;
    }
}

fn foreign_root_stack_owner_active() -> bool {
    let current = std::thread::current().id();
    let coord = runtime().coord.0.lock().unwrap();
    runtime()
        .root_stack_owner
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|owner| *owner != current && !coord.mutators.contains_key(owner))
}

/// Number of distinct runtime-rooted objects. Acceptance tests use this to
/// prove that panic/recover releases every root it took, instead of only
/// checking that the program printed the right text (willow-s9ej.7).
pub fn runtime_root_count() -> usize {
    runtime().runtime_roots.len()
}

fn runtime_roots_snapshot() -> Vec<*mut u8> {
    let mut roots = runtime().runtime_roots.snapshot();
    let parked = runtime().parked_stack_roots.lock().unwrap();
    for slots in parked.values() {
        for &slot in slots {
            // SAFETY: the park contract retains these immutable stack slots;
            // active stack transitions and collection are serialized by STW.
            if slot == 0 {
                continue;
            }
            let value = unsafe { *(slot as *mut *mut u8) };
            if !value.is_null() {
                roots.push(value);
            }
        }
    }
    roots
}

fn reset_internal() {
    coordinator::shutdown();
    // Exclude a concurrent collection (see runtime().collect_lock).
    let _serialize = runtime()
        .collect_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    GC_MARK_PHASE.store(0, Ordering::Release);
    let mut state = runtime().heap.lock().unwrap();
    state.concurrent_cycle = None;
    state.sweeping = None;
    state.satb = satb::SatbBuffers::default();
    for record in state.tlab_states.values() {
        // SAFETY: reset is serialized against mutator activity in production
        // and tests hold the runtime test guard.
        let tls = unsafe { tlab_state_at(record.address) };
        tls.cursor.store(0, Ordering::Release);
        tls.limit.store(0, Ordering::Release);
        tls.start_bits.store(0, Ordering::Release);
        tls.fast_allocations.store(0, Ordering::Release);
        tls.fast_allocated_bytes.store(0, Ordering::Release);
    }
    state.old_regions.clear();
    state.old_addresses.clear();
    state.tlab_addresses.clear();
    state.old_region_candidates.clear();
    state.old_reserved_bytes = 0;
    for chunk in state.tlab_chunks.drain(..) {
        let layout = Layout::from_size_align(chunk.capacity, std::mem::align_of::<GcHeader>())
            .expect("TLAB chunk layout remains valid");
        // SAFETY: reset owns and releases each registered chunk exactly once.
        unsafe { dealloc(chunk.base, layout) };
    }
    state.tlab_states.clear();
    state.tlab_owners.clear();
    state.allocated_bytes = 0;
    state.threshold_bytes = 1024 * 1024;
    state.memory_limit_bytes = gc_memory_limit_from_env();
    state.soft_memory = memory_control::Controller::from_env();
    state.last_major_live_bytes = 0;
    state.last_major_mark_work = 0;
    state.pacer = pacer::Sampler::default();
    state.pacer_trigger = 1024 * 1024;
    assist::reset();
    state.young_allocated_bytes = 0;
    state.nursery_threshold_bytes = GC_NURSERY_THRESHOLD_BYTES;
    state.total_allocs = 0;
    state.total_allocated_bytes = 0;
    state.released_bytes = 0;
    crate::gc_telemetry::reset_for_test();
    state.total_frees = 0;
    state.tlab_fast_allocations = 0;
    state.tlab_slow_allocations = 0;
    state.tlab_refills = 0;
    state.tlab_large_allocations = 0;
    state.tlab_fast_allocated_bytes = 0;
    state.tlab_reserved_bytes = 0;
    state.remembered_set.clear();
    state.dirty_cards.clear();
    runtime().write_barrier_calls.store(0, Ordering::Relaxed);
    runtime()
        .tlab_ever_allocated
        .store(false, Ordering::Release);
    state.write_barrier_hits = 0;
    state.minor_collections = 0;
    state.promoted_objects = 0;
    state.promoted_bytes = 0;
    state.moved_objects = 0;
    state.old_region_allocations = 0;
    state.old_region_reuses = 0;
    state.old_regions_released = 0;
    state.major_collections = 0;
    runtime().runtime_roots.clear();
    runtime().parked_stack_roots.lock().unwrap().clear();
    *runtime().root_stack_owner.lock().unwrap() = None;
    {
        let (lock, cv) = &runtime().coord;
        let mut coord = lock.lock().unwrap();
        *coord = GcCoord::default();
        runtime()
            .stop_requested
            .store(false, std::sync::atomic::Ordering::Release);
        cv.notify_all();
    }
    type_registry().lock().unwrap().clear();
    runtime().concurrent_trace_registry.lock().unwrap().clear();
    runtime().concurrent_slice_registry.lock().unwrap().clear();
    drop_registry().lock().unwrap().clear();
    runtime()
        .registry_generation
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    ROOT_STACK.with(|rs| rs.borrow_mut().clear());
    runtime().poll_requested.store(false, Ordering::Release);
    ROOT_DEPTH.set(0);
    HAS_REGISTERED_TLAB.set(false);
    // Clear the string literal interning cache: cached pointers are into the
    // heap that was just freed above and must not be returned again.
    crate::string::clear_string_literal_cache();
}

// ---------------------------------------------------------------------------
// Cross-module test helpers
// ---------------------------------------------------------------------------

/// Size of the GC object header in bytes (header + payload accounting). Exposed
/// for tests in sibling modules (e.g. `async_frame`) that compute expected heap
/// sizes.
#[cfg(test)]
pub fn header_size_for_test() -> usize {
    std::mem::size_of::<GcHeader>()
}

/// Reset the GC heap/root/registry state. Exposed for tests in sibling modules
/// so they can isolate from one another on the shared global heap.
#[cfg(test)]
pub fn reset_internal_for_test() {
    reset_internal();
}

/// A fresh, empty generated-TLS block for tests in sibling modules that need a
/// YOUNG object: `willow_gc_alloc_slow` is the only runtime entry that allocates
/// into the nursery, every `willow_alloc_*` helper goes straight to the old
/// generation. Container tests use it to give a container young children that a
/// minor collection will move.
#[cfg(test)]
pub(crate) fn tlab_state_for_test() -> GcTlabState {
    GcTlabState {
        cursor: AtomicUsize::new(0),
        limit: AtomicUsize::new(0),
        fast_allocations: AtomicU64::new(0),
        fast_allocated_bytes: AtomicU64::new(0),
        start_bits: AtomicUsize::new(0),
    }
}

#[cfg(test)]
fn publish_tlab_start_for_test(tls: &GcTlabState, header: *mut u8) {
    let base = tls.limit.load(Ordering::Acquire) - GC_TLAB_CHUNK_SIZE;
    let bit = (header as usize - base) / GC_REGION_MARK_GRANULE;
    let words = tls.start_bits.load(Ordering::Acquire) as *const AtomicU64;
    assert!(!words.is_null());
    // SAFETY: fixture owns the current chunk, exactly as generated code does.
    unsafe { &*words.add(bit / 64) }.fetch_or(1u64 << (bit % 64), Ordering::Release);
}

/// Whether a collector has requested a stop that `willow_gc_safepoint` would
/// park on. The lock-free gate (`willow_gc_stop_flag`) is published FIRST and
/// the coordination flag second, so a test that must enter a runtime call
/// after the stop is pending has to watch the coordination flag: a thread that
/// polls between the two publications returns from the safepoint without
/// parking, and a collector waiting for it never resumes. The collector holds
/// the coordination lock only while it checks who has parked, then waits on
/// the condvar, so this lock is uncontended while the stop is pending.
#[cfg(test)]
pub(crate) fn stop_pending_for_test() -> bool {
    let (lock, _) = &runtime().coord;
    lock.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .stop_requested
}

/// How many objects the collector has freed since the last heap reset. Exposed
/// for tests in sibling modules that register their own root sources and need
/// to show a collection actually reclaimed something — without it, "my object
/// survived" is equally true of a collector that frees nothing.
#[cfg(test)]
pub fn total_frees_for_test() -> u64 {
    runtime().heap.lock().unwrap().total_frees
}

/// Hold this for runtime tests that touch the process-global GC heap or other
/// runtime globals that allocate on it.
///
/// This lock is test-only and is not part of production synchronization. It
/// prevents one test from resetting the shared heap with `willow_gc_init` while
/// another test is still using pointers allocated from that heap.
#[cfg(test)]
pub fn runtime_test_guard() -> std::sync::MutexGuard<'static, ()> {
    RUNTIME_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "gc_region_tests.rs"]
mod region_viewpoint_tests;

#[cfg(test)]
#[path = "gc_stress_tests.rs"]
mod stress_viewpoint_tests;

#[cfg(test)]
#[path = "gc_contract_tests.rs"]
mod contract_viewpoint_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn gc_test_guard() -> std::sync::MutexGuard<'static, ()> {
        runtime_test_guard()
    }

    #[test]
    fn empty_root_arena_first_snapshot_is_allocation_free() {
        use crate::scheduler::scaling_measurements::counting_allocator as counter;

        let arena = GcRootArena::default();
        // Move the initialized arena to a collector thread before its first
        // snapshot; do not warm up the snapshot path inside the measurement.
        std::thread::spawn(move || {
            let mut children = Vec::new();
            let allocations = counter::thread_allocations();
            let progress = arena.snapshot_slice(0, 512, usize::MAX, &mut children);
            let allocated = counter::thread_allocations() - allocations;
            assert_eq!(progress, Some((0, 0)));
            assert!(children.is_empty());
            assert_eq!(allocated, 0);
        })
        .join()
        .unwrap();
    }

    // willow-ssl7.12: stress-mode checks sit on the allocation slow path, so
    // they must not read the environment (a Windows `getenv` allocates) per
    // call. The environment is parsed once; tests switch modes through the
    // override without touching the process environment.
    #[test]
    fn gc_stress_mode_checks_are_allocation_free_after_first_use() {
        use crate::scheduler::scaling_measurements::counting_allocator as counter;
        let _guard = gc_test_guard();
        set_gc_stress_for_test(None);
        let _ = gc_stress_enabled("alloc");
        let allocations = counter::thread_allocations();
        let bytes = counter::thread_bytes();
        for kind in ["alloc", "minor", "await", "scheduler"] {
            for _ in 0..1_000 {
                std::hint::black_box(gc_stress_enabled(kind));
            }
        }
        assert_eq!(
            (
                counter::thread_allocations() - allocations,
                counter::thread_bytes() - bytes
            ),
            (0, 0)
        );
    }

    #[test]
    fn gc_stress_modes_parse_lists_and_override_replaces_the_environment() {
        let _guard = gc_test_guard();
        assert!(parse_gc_stress_modes("").is_empty());
        assert_eq!(
            parse_gc_stress_modes(" alloc , minor,,await"),
            ["alloc", "minor", "await"]
        );
        set_gc_stress_for_test(Some("minor, scheduler"));
        assert!(gc_stress_enabled("minor"));
        assert!(gc_stress_enabled("scheduler"));
        assert!(!gc_stress_enabled("alloc"));
        assert!(!gc_stress_enabled("await"));
        set_gc_stress_for_test(Some("all"));
        assert!(
            ["alloc", "minor", "await", "scheduler"]
                .into_iter()
                .all(gc_stress_enabled)
        );
        set_gc_stress_for_test(Some(""));
        assert!(!gc_stress_enabled("alloc"));
        set_gc_stress_for_test(None);
        let from_environment = std::env::var("WILLOW_GC_STRESS")
            .map(|value| parse_gc_stress_modes(&value))
            .unwrap_or_default();
        assert_eq!(
            gc_stress_enabled("alloc"),
            from_environment
                .iter()
                .any(|mode| mode == "all" || mode == "alloc")
        );
    }

    #[test]
    fn runtime_state_starts_owned_and_empty() {
        let runtime = GcRuntime::default();
        let heap = runtime.heap.lock().unwrap();
        assert!(heap.old_regions.is_empty());
        assert_eq!(heap.allocated_bytes, 0);
        assert_eq!(runtime.runtime_roots.len(), 0);
        assert!(runtime.trace_registry.lock().unwrap().is_empty());
        assert!(runtime.drop_registry.lock().unwrap().is_empty());
    }

    #[test]
    fn legacy_root_owner_released_after_late_registration() {
        let _guard = gc_test_guard();
        for count in [1, 16, 256] {
            reset_gc();
            let before = willow_gc_skipped_collections();
            std::thread::spawn(move || {
                let mut slot = willow_alloc_object(2, 32);
                for _ in 0..count {
                    willow_push_root(&mut slot);
                }
                willow_gc_register_mutator();
                for _ in 0..count {
                    willow_pop_root();
                }
                willow_gc_unregister_mutator();
            })
            .join()
            .unwrap();
            // The owner has exited; collection runs on a different thread.
            willow_gc_collect();
            assert_eq!(willow_gc_skipped_collections(), before, "roots={count}");
            assert_eq!(willow_gc_allocated_bytes(), 0, "roots={count}");
        }
        reset_gc();
    }

    #[test]
    fn legacy_root_owner_retained_after_nonempty_unregister() {
        let _guard = gc_test_guard();
        reset_gc();
        let before = willow_gc_skipped_collections();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let owner = std::thread::spawn(move || {
            let mut slot = willow_alloc_object(2, 32);
            willow_push_root(&mut slot);
            willow_gc_register_mutator();
            willow_gc_unregister_mutator();
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            willow_pop_root();
        });
        ready_rx.recv().unwrap();
        willow_gc_collect();
        willow_gc_minor_collect();
        let skipped = willow_gc_skipped_collections();
        let retained = willow_gc_allocated_bytes();
        release_tx.send(()).unwrap();
        owner.join().unwrap();
        assert_eq!(skipped - before, 2);
        assert_eq!(retained, obj_size(32));
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    #[test]
    fn gc_counts_collections_skipped_for_foreign_root_owner() {
        let _guard = gc_test_guard();
        reset_gc();
        let before = willow_gc_skipped_collections();
        // This thread claims root-stack ownership by pushing a root.
        let mut slot: *mut u8 = std::ptr::null_mut();
        willow_push_root(&mut slot as *mut *mut u8);
        // A foreign thread cannot scan our root stack, so its collection must be
        // skipped — and counted (willow-6fv.2).
        std::thread::spawn(|| willow_gc_collect()).join().unwrap();
        willow_pop_root();
        let after = willow_gc_skipped_collections();
        assert!(
            after > before,
            "a foreign-owner collection should be counted as skipped (before={before}, after={after})"
        );
    }

    // ── Multi-mutator coordination + STW (willow-6fv.5.6) ───────────────────

    #[test]
    fn coord_register_makes_multi_mutator_active_from_other_thread() {
        let _guard = gc_test_guard();
        reset_gc();
        assert!(!multi_mutator_active(), "no mutators registered yet");
        // A second registered thread makes this thread see a foreign mutator.
        let handle = std::thread::spawn(|| {
            willow_gc_register_mutator();
            // Keep the registration alive until the main thread observes it.
            std::thread::sleep(std::time::Duration::from_millis(20));
            willow_gc_unregister_mutator();
        });
        // Spin briefly until the worker has registered.
        let mut saw = false;
        for _ in 0..200 {
            if multi_mutator_active() {
                saw = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        handle.join().unwrap();
        assert!(
            saw,
            "a registered worker thread should be a foreign mutator"
        );
        assert!(!multi_mutator_active(), "unregister clears it");
    }

    #[test]
    fn coord_safepoint_is_noop_when_no_stop_requested() {
        let _guard = gc_test_guard();
        reset_gc();
        // No collection in progress: a safepoint poll must return immediately.
        willow_gc_safepoint();
        willow_gc_register_mutator();
        willow_gc_safepoint();
        willow_gc_unregister_mutator();
    }

    #[test]
    fn multi_mutator_stw_keeps_other_thread_roots_alive_through_generated_gate() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let _guard = gc_test_guard();
        reset_gc();
        willow_gc_register_mutator(); // main is a mutator too

        // Main holds root A.
        let a = willow_alloc_object(0, 8);
        let mut a_slot = a;
        willow_push_root(&mut a_slot as *mut *mut u8);

        let ready = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (r2, d2) = (ready.clone(), done.clone());
        let worker = std::thread::spawn(move || {
            willow_gc_register_mutator();
            // Worker holds root B and keeps polling safepoints (so it parks
            // during the collector's stop-the-world).
            let b = willow_alloc_object(0, 8);
            let mut b_slot = b;
            willow_push_root(&mut b_slot as *mut *mut u8);
            assert_eq!(crate::preempt::willow_sync_native_active(), 0);
            let stop = willow_gc_stop_flag();
            assert_eq!(stop, willow_gc_stop_flag());
            r2.store(true, Ordering::SeqCst);
            while !d2.load(Ordering::SeqCst) {
                // Model generated non-task code: cache the address, but reload
                // the atomic gate so a later collector can stop this worker.
                // SAFETY: the accessor returns this process-lifetime AtomicBool.
                if unsafe { &*stop.cast::<AtomicBool>() }.load(Ordering::Acquire) {
                    crate::preempt::willow_sync_safepoint();
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            willow_pop_root();
            willow_gc_unregister_mutator();
        });

        while !ready.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        let before = willow_gc_allocated_bytes();
        // Stop-the-world collection: scans main's A AND the worker's published B.
        willow_gc_collect();
        let after = willow_gc_allocated_bytes();
        assert_eq!(
            after, before,
            "STW collection must keep both mutators' rooted objects alive"
        );

        done.store(true, Ordering::SeqCst);
        worker.join().unwrap();
        willow_pop_root();
        willow_gc_unregister_mutator();
    }

    #[test]
    fn multi_mutator_concurrent_collection_does_not_deadlock() {
        let _guard = gc_test_guard();
        reset_gc();
        // Two registered mutators each allocate garbage and trigger collections
        // concurrently. The collector-election (try_lock + safepoint) must keep
        // this deadlock-free: when one thread is collecting, the other parks at a
        // safepoint instead of blocking on the collect lock (willow-6fv.5.6).
        let handles: Vec<_> = (0..2)
            .map(|_| {
                std::thread::spawn(|| {
                    willow_gc_register_mutator();
                    for _ in 0..30 {
                        let _garbage = willow_alloc_object(0, 8); // unrooted
                        willow_gc_safepoint();
                        willow_gc_collect();
                    }
                    willow_gc_unregister_mutator();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // With no live roots, a final collection reclaims everything.
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            0,
            "concurrent collection should reclaim all garbage without deadlock"
        );
    }

    #[test]
    fn multi_mutator_stw_frees_unrooted_object() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let _guard = gc_test_guard();
        reset_gc();
        willow_gc_register_mutator();

        let ready = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (r2, d2) = (ready.clone(), done.clone());
        let worker = std::thread::spawn(move || {
            willow_gc_register_mutator();
            // Worker registers but holds NO root; it just parks at safepoints.
            r2.store(true, Ordering::SeqCst);
            while !d2.load(Ordering::SeqCst) {
                willow_gc_safepoint();
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            willow_gc_unregister_mutator();
        });
        while !ready.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        // An unrooted object must be collected even under multi-mutator STW.
        let _garbage = willow_alloc_object(0, 8);
        assert!(willow_gc_allocated_bytes() > 0);
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            0,
            "unrooted object must be freed by the STW collection"
        );

        done.store(true, Ordering::SeqCst);
        worker.join().unwrap();
        willow_gc_unregister_mutator();
    }

    // ── A registration that races a collection (willow-v6k0) ────────────────
    //
    // The collector used to ask `multi_mutator_active()` and, when it was the
    // only registered mutator, scan just its own root stack and sweep without
    // stopping the world. That question is a snapshot: a thread registering
    // right after it runs unseen for the rest of the cycle, so the sweep frees
    // the objects it has just allocated and rooted. The program keeps running
    // on the dangling pointers until a later cycle traces one and aborts, or
    // the reused memory produces a wrong answer.

    /// Allocate `count` rooted objects, each stamped with a sentinel, into
    /// `slots`, whose storage the caller keeps alive for the roots' lifetime.
    fn root_stamped_objects(slots: &mut [*mut u8], sentinel: i64) {
        for (index, slot) in slots.iter_mut().enumerate() {
            *slot = willow_alloc_object(0, 8);
            // SAFETY: a freshly allocated 8-byte payload.
            unsafe { *(slot.cast::<i64>()) = sentinel + index as i64 };
            willow_push_root(slot as *mut *mut u8);
        }
    }

    /// Every slot still names a live object holding its sentinel.
    fn assert_stamped_objects_live(slots: &[*mut u8], sentinel: i64, context: &str) {
        for (index, &slot) in slots.iter().enumerate() {
            #[cfg(debug_assertions)]
            if let Err(message) = validate_payload_pointer(slot, "test") {
                panic!("{context}: rooted object {index} was collected: {message}");
            }
            // SAFETY: the object above is rooted, so it is still live.
            let stamp = unsafe { *(slot.cast::<i64>()) };
            assert_eq!(
                stamp,
                sentinel + index as i64,
                "{context}: rooted object {index} lost its payload"
            );
        }
    }

    /// A worker that registers, roots objects, verifies them across another
    /// thread's collections, and leaves. Returns once it has done `rounds`.
    fn racing_root_worker(rounds: usize, sentinel: i64, minor: bool) {
        const PER_ROUND: usize = 4;
        for round in 0..rounds {
            willow_gc_register_mutator();
            let mut slots = [std::ptr::null_mut::<u8>(); PER_ROUND];
            root_stamped_objects(&mut slots, sentinel);
            // Give the collector a window in which this thread is registered
            // and holding roots it must scan.
            for _ in 0..4 {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            if minor {
                willow_gc_minor_collect();
            }
            assert_stamped_objects_live(&slots, sentinel, &format!("round {round}"));
            willow_pop_roots(PER_ROUND as i32);
            willow_gc_unregister_mutator();
        }
    }

    /// Hammer collections on one thread while `workers` threads register,
    /// allocate rooted objects and unregister underneath it.
    fn run_registration_race(workers: usize, rounds: usize, minor: bool) {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = Arc::new(AtomicBool::new(false));
        let collector_stop = stop.clone();
        let collector = std::thread::spawn(move || {
            while !collector_stop.load(Ordering::Acquire) {
                // This thread never registers, so it is the one that used to
                // take the "I am alone" fast path.
                if minor {
                    willow_gc_minor_collect();
                } else {
                    willow_gc_collect();
                }
                std::thread::yield_now();
            }
        });
        let handles: Vec<_> = (0..workers)
            .map(|worker| {
                std::thread::spawn(move || {
                    racing_root_worker(rounds, 1_000 + worker as i64 * 1_000, minor)
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("worker thread panicked");
        }
        stop.store(true, Ordering::Release);
        collector.join().expect("collector thread panicked");
    }

    #[test]
    fn coord_registration_racing_a_major_collection_keeps_the_newcomers_roots() {
        let _guard = gc_test_guard();
        reset_gc();
        run_registration_race(1, 200, false);
        reset_gc();
    }

    #[test]
    fn coord_registration_racing_a_minor_collection_keeps_the_newcomers_roots() {
        let _guard = gc_test_guard();
        reset_gc();
        run_registration_race(1, 200, true);
        reset_gc();
    }

    #[test]
    fn coord_several_mutators_registering_at_once_keep_their_roots() {
        let _guard = gc_test_guard();
        reset_gc();
        run_registration_race(4, 100, false);
        reset_gc();
    }

    #[test]
    fn coord_a_registered_mutators_runtime_root_survives_a_racing_collection() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let _guard = gc_test_guard();
        reset_gc();
        // The same race reaches runtime roots — the scheduler's hold on a task
        // frame — because the sweep, not the scan, is what frees the object.
        let stop = Arc::new(AtomicBool::new(false));
        let collector_stop = stop.clone();
        let collector = std::thread::spawn(move || {
            while !collector_stop.load(Ordering::Acquire) {
                willow_gc_collect();
                std::thread::yield_now();
            }
        });
        for round in 0..200 {
            willow_gc_register_mutator();
            let object = willow_alloc_object(0, 8);
            willow_gc_add_runtime_root(object);
            // SAFETY: the runtime root keeps this payload alive.
            unsafe { *(object.cast::<i64>()) = 77 };
            for _ in 0..4 {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            #[cfg(debug_assertions)]
            if let Err(message) = validate_payload_pointer(object, "test") {
                panic!("round {round}: runtime root was collected: {message}");
            }
            // SAFETY: still rooted.
            assert_eq!(unsafe { *(object.cast::<i64>()) }, 77, "round {round}");
            willow_gc_remove_runtime_root(object);
            willow_gc_unregister_mutator();
        }
        stop.store(true, Ordering::Release);
        collector.join().expect("collector thread panicked");
        reset_gc();
    }

    #[test]
    fn coord_a_collection_racing_registration_still_frees_garbage() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let _guard = gc_test_guard();
        reset_gc();
        // Stopping the world for every cycle must not make the collector
        // conservative: unrooted objects allocated by a registering worker are
        // still reclaimed.
        let stop = Arc::new(AtomicBool::new(false));
        let collector_stop = stop.clone();
        let collector = std::thread::spawn(move || {
            while !collector_stop.load(Ordering::Acquire) {
                willow_gc_collect();
                std::thread::yield_now();
            }
        });
        for _ in 0..200 {
            willow_gc_register_mutator();
            let _garbage = willow_alloc_object(0, 8);
            willow_gc_safepoint();
            willow_gc_unregister_mutator();
        }
        stop.store(true, Ordering::Release);
        collector.join().expect("collector thread panicked");
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            0,
            "unrooted objects from a registering worker must still be freed"
        );
        reset_gc();
    }

    #[cfg(debug_assertions)]
    #[test]
    fn coord_a_panicking_collection_resumes_the_world() {
        let _guard = gc_test_guard();
        reset_gc();
        // Now that every cycle stops the world, a cycle that panics has to put
        // the world back: an unwind that leaves `stop_requested` set parks every
        // other mutator forever, and one that escapes the registry guard
        // poisons it, so no later collection can run at all.
        willow_gc_add_runtime_root(std::ptr::dangling_mut::<u8>());
        let result = std::panic::catch_unwind(collect_internal);
        assert!(
            result.is_err(),
            "an invalid runtime root must fail the cycle"
        );
        assert!(
            !runtime()
                .stop_requested
                .load(std::sync::atomic::Ordering::Acquire),
            "the world must be resumed after a panicking cycle"
        );
        reset_gc();
        let mut slots = [std::ptr::null_mut::<u8>(); 2];
        root_stamped_objects(&mut slots, 9);
        willow_gc_collect();
        assert_stamped_objects_live(&slots, 9, "after a panicking cycle");
        willow_pop_roots(2);
        reset_gc();
    }

    #[test]
    fn coord_a_lone_thread_still_collects_without_registering() {
        let _guard = gc_test_guard();
        reset_gc();
        // The fast path is gone, so check the ordinary single-threaded program
        // still collects: an unregistered thread with no other mutators must
        // reclaim its garbage and keep its roots.
        let mut slots = [std::ptr::null_mut::<u8>(); 2];
        root_stamped_objects(&mut slots, 5);
        let _garbage = willow_alloc_object(0, 8);
        willow_gc_collect();
        assert_stamped_objects_live(&slots, 5, "lone thread");
        willow_pop_roots(2);
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            0,
            "a lone thread's collection reclaims everything once the roots go"
        );
        reset_gc();
    }

    fn reset_gc() {
        reset_internal();
    }

    fn set_threshold(bytes: usize) {
        runtime().heap.lock().unwrap().threshold_bytes = bytes;
    }

    fn total_allocs() -> u64 {
        runtime().heap.lock().unwrap().total_allocs
    }

    fn total_frees() -> u64 {
        runtime().heap.lock().unwrap().total_frees
    }

    fn header_size() -> usize {
        std::mem::size_of::<GcHeader>()
    }

    fn obj_size(payload: usize) -> i64 {
        (header_size() + payload) as i64
    }

    fn new_tlab_state() -> GcTlabState {
        tlab_state_for_test()
    }

    #[test]
    fn telemetry_merges_fast_tlab_deltas_once_and_on_unregister() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_gc_register_mutator();
        let mut tls = new_tlab_state();
        let first = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
        assert!(!first.is_null());
        let bytes = GC_HEADER_SIZE + 8;
        let cursor = tls.cursor.load(Ordering::Acquire);
        // Reproduce the generated bump fast path, including its TLS counters.
        initialize_object_at(cursor as *mut u8, bytes, 0, 1, 0).unwrap();
        publish_tlab_start_for_test(&tls, cursor as *mut u8);
        tls.cursor.store(cursor + bytes, Ordering::Release);
        tls.fast_allocations.store(1, Ordering::Release);
        tls.fast_allocated_bytes
            .store(bytes as u64, Ordering::Release);
        let first = crate::gc_telemetry::snapshot();
        let second = crate::gc_telemetry::snapshot();
        assert_eq!(first.counters.allocation_count, 2);
        assert_eq!(first.counters.allocation_bytes, (bytes * 2) as u64);
        assert_eq!(first.counters.tlab_fast_allocations, 1);
        assert_eq!(
            second.counters.allocation_bytes,
            first.counters.allocation_bytes
        );
        willow_gc_unregister_mutator();
        // `GcTlabState` has no `Drop` hook, so letting the state die can merge
        // nothing: the deltas above are in the totals only because
        // `willow_gc_unregister_mutator` merged them, and the snapshot below
        // stays put no matter how long the state lives.
        assert_eq!(
            crate::gc_telemetry::snapshot().counters.allocation_bytes,
            first.counters.allocation_bytes
        );
        willow_gc_collect();
        assert_eq!(crate::gc_telemetry::snapshot().heap.occupied_bytes, 0);
        reset_gc();
    }

    #[test]
    fn telemetry_promotion_is_not_a_second_logical_allocation() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut tls = new_tlab_state();
        let young = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
        let mut parent = willow_alloc_typed(8, 1);
        willow_push_root(&mut parent);
        willow_gc_write_barrier(
            parent,
            std::ptr::null_mut(),
            young,
            GcStoreDestination::ObjectField as i64,
        );
        unsafe {
            *parent.cast::<*mut u8>() = young;
        }
        let before = crate::gc_telemetry::snapshot();
        willow_gc_minor_collect();
        let after = crate::gc_telemetry::snapshot();
        assert_eq!(
            after.counters.allocation_count,
            before.counters.allocation_count
        );
        assert_eq!(
            after.counters.allocation_bytes,
            before.counters.allocation_bytes
        );
        assert_eq!(after.counters.promoted_objects, 1);
        assert_eq!(after.counters.promoted_bytes, (GC_HEADER_SIZE + 8) as u64);
        assert_eq!(after.last_cycle.reclaimed_bytes, 0);
        assert_eq!(after.counters.released_bytes, GC_TLAB_CHUNK_SIZE as u64);
        assert_eq!(
            after.last_cycle.marked_bytes,
            (2 * (GC_HEADER_SIZE + 8)) as u64
        );
        willow_pop_root();
        reset_gc();
    }

    #[test]
    fn test_gc_generated_header_and_tlab_abi_layout() {
        assert_eq!(GC_HEADER_SIZE, 40);
        assert_eq!(std::mem::offset_of!(GcHeader, marked), 0);
        assert_eq!(std::mem::offset_of!(GcHeader, allocated), 1);
        assert_eq!(std::mem::offset_of!(GcHeader, generation), 2);
        assert_eq!(std::mem::offset_of!(GcHeader, age), 3);
        assert_eq!(std::mem::offset_of!(GcHeader, type_id), 4);
        assert_eq!(std::mem::offset_of!(GcHeader, layout_id), 8);
        assert_eq!(std::mem::offset_of!(GcHeader, gc_ref_mask), 16);
        assert_eq!(std::mem::offset_of!(GcHeader, size), 24);
        assert_eq!(std::mem::offset_of!(GcHeader, next), 32);
        assert_eq!(GC_TLAB_STATE_SIZE, 40);
        assert_eq!(std::mem::offset_of!(GcTlabState, cursor), 0);
        assert_eq!(std::mem::offset_of!(GcTlabState, limit), 8);
        assert_eq!(std::mem::offset_of!(GcTlabState, fast_allocations), 16);
        assert_eq!(std::mem::offset_of!(GcTlabState, fast_allocated_bytes), 24);
        assert_eq!(std::mem::offset_of!(GcTlabState, start_bits), 32);
    }

    #[test]
    fn test_gc_tlab_large_object_uses_old_region_slow_path() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut tls = new_tlab_state();
        let payload_size = GC_TLAB_MAX_OBJECT_SIZE as i64;
        let ptr = willow_gc_alloc_slow(&mut tls, 7, 9, payload_size, 0);
        assert!(!ptr.is_null());
        assert_eq!(willow_gc_tlab_fast_allocations(), 0);
        assert_eq!(willow_gc_tlab_slow_allocations(), 1);
        assert_eq!(willow_gc_tlab_large_allocations(), 1);
        assert_eq!(willow_gc_tlab_refills(), 0);
        assert_eq!(willow_gc_tlab_reserved_bytes(), 0);
        assert_eq!(
            willow_gc_allocated_bytes(),
            GC_HEADER_SIZE as i64 + payload_size
        );
        reset_gc();
    }

    #[test]
    fn test_gc_tlab_refill_slow_path_coordinates_threshold_collection() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut tls = new_tlab_state();
        let first = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
        assert!(!first.is_null());
        assert_eq!(willow_gc_tlab_refills(), 1);
        set_threshold(1);
        let second = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
        assert!(!second.is_null());
        assert_eq!(
            willow_gc_allocated_bytes(),
            (GC_HEADER_SIZE + 8) as i64,
            "the unrooted object in the retired first chunk was collected"
        );
        assert_eq!(willow_gc_tlab_refills(), 2);
        assert_eq!(total_frees(), 1);
        reset_gc();
    }

    #[test]
    fn test_gc_minor_collection_moves_heap_reachable_young_and_updates_slot() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut tls = new_tlab_state();
        let young = willow_gc_alloc_slow(&mut tls, 11, 0, 8, 0);
        assert!(!young.is_null());
        unsafe { *(young as *mut i64) = 0x1234 };

        let mut parent = willow_gc_alloc_layout(12, 0, 8, 0b1);
        willow_gc_write_barrier(
            parent,
            std::ptr::null_mut(),
            young,
            GcStoreDestination::ObjectField as i64,
        );
        unsafe { *(parent as *mut *mut u8) = young };
        willow_push_root(&mut parent as *mut *mut u8);

        willow_gc_minor_collect();

        let moved = unsafe { *(parent as *mut *mut u8) };
        assert_ne!(moved, young, "heap-only young child should be copied");
        assert_eq!(unsafe { *(moved as *mut i64) }, 0x1234);
        assert_eq!(
            unsafe { (*payload_to_header(moved)).generation },
            GC_GENERATION_OLD
        );
        assert_eq!(willow_gc_moved_objects(), 1);
        assert_eq!(willow_gc_remembered_set_size(), 0);
        assert_eq!(willow_gc_tlab_reserved_bytes(), 0);

        willow_pop_root();
        willow_gc_collect();
        reset_gc();
    }

    #[test]
    fn test_gc_minor_collection_pins_direct_young_root_for_ssa_compatibility() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut tls = new_tlab_state();
        let mut young = willow_gc_alloc_slow(&mut tls, 21, 0, 8, 0);
        unsafe { *(young as *mut i64) = 77 };
        willow_push_root(&mut young as *mut *mut u8);

        willow_gc_minor_collect();

        assert_eq!(unsafe { *(young as *mut i64) }, 77);
        assert_eq!(
            unsafe { (*payload_to_header(young)).generation },
            GC_GENERATION_OLD
        );
        assert_eq!(willow_gc_moved_objects(), 0);
        assert_eq!(willow_gc_promoted_objects(), 1);
        assert!(willow_gc_tlab_reserved_bytes() > 0);
        assert_eq!(willow_gc_pinned_region_count(), 1);
        assert_eq!(willow_gc_old_region_count(), 1);

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        assert_eq!(willow_gc_tlab_reserved_bytes(), 0);
        reset_gc();
    }

    #[test]
    fn test_gc_minor_collection_pins_runtime_root_until_owner_releases_it() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut tls = new_tlab_state();
        let young = willow_gc_alloc_slow(&mut tls, 22, 0, 8, 0);
        unsafe { *(young as *mut i64) = 78 };
        willow_gc_add_runtime_root(young);

        willow_gc_minor_collect();

        assert_eq!(unsafe { *(young as *mut i64) }, 78);
        assert_eq!(
            unsafe { (*payload_to_header(young)).generation },
            GC_GENERATION_OLD
        );
        willow_gc_remove_runtime_root(young);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    #[test]
    fn test_gc_minor_collection_reclaims_unreachable_nursery() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut tls = new_tlab_state();
        let young = willow_gc_alloc_slow(&mut tls, 31, 0, 8, 0);
        assert!(!young.is_null());

        willow_gc_minor_collect();

        assert_eq!(willow_gc_allocated_bytes(), 0);
        assert_eq!(willow_gc_tlab_reserved_bytes(), 0);
        assert_eq!(willow_gc_minor_collections(), 1);
        reset_gc();
    }

    #[test]
    fn test_gc_minor_collection_updates_array_reference_slots() {
        let _guard = gc_test_guard();
        // This fixture specifically verifies nursery evacuation and its barrier.
        let _stress = GcStressTestScope::normal();
        reset_gc();
        let mut array = crate::array::willow_array_new(1, 1);
        willow_push_root(&mut array as *mut *mut u8);
        let mut tls = new_tlab_state();
        let young = willow_gc_alloc_slow(&mut tls, 41, 0, 8, 0);
        unsafe { *(young as *mut i64) = 901 };
        crate::array::willow_array_set(array, 0, young as i64);
        assert!(willow_gc_remembered_set_size() > 0);
        assert!(willow_gc_dirty_card_count() > 0);

        willow_gc_minor_collect();

        let moved = crate::array::willow_array_get(array, 0) as *mut u8;
        assert_ne!(moved, young);
        assert_eq!(unsafe { *(moved as *mut i64) }, 901);
        assert_eq!(willow_gc_dirty_card_count(), 0);
        willow_pop_root();
        willow_gc_collect();
        reset_gc();
    }

    #[test]
    fn test_gc_minor_collection_updates_map_reference_slots() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut map = crate::map::willow_map_new(0, 3, 1);
        willow_push_root(&mut map as *mut *mut u8);
        let mut tls = new_tlab_state();
        let young = willow_gc_alloc_slow(&mut tls, 42, 0, 8, 0);
        unsafe { *(young as *mut i64) = 902 };
        crate::map::willow_map_insert(map, 7, 0, young as i64, 1);
        assert!(willow_gc_remembered_set_size() > 0);

        willow_gc_minor_collect();

        let option = crate::map::willow_map_get(map, 7, 0, 0);
        let moved = unsafe { *((option as *mut *mut u8).add(1)) };
        assert_ne!(moved, young);
        assert_eq!(unsafe { *(moved as *mut i64) }, 902);
        willow_pop_root();
        willow_gc_collect();
        reset_gc();
    }

    #[test]
    fn test_gc_minor_collection_updates_channel_queue_reference_slots() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut channel = crate::channel::willow_channel_new(1) as *mut u8;
        willow_push_root(&mut channel as *mut *mut u8);
        let mut tls = new_tlab_state();
        let young = willow_gc_alloc_slow(&mut tls, 43, 0, 8, 0);
        unsafe { *(young as *mut i64) = 903 };
        crate::channel::willow_channel_send_ptr(channel.cast(), young.cast());
        assert!(willow_gc_remembered_set_size() > 0);

        willow_gc_minor_collect();

        let moved = crate::channel::willow_channel_recv_ptr(channel.cast()).cast::<u8>();
        assert_ne!(moved, young);
        assert_eq!(unsafe { *(moved as *mut i64) }, 903);
        willow_pop_root();
        willow_gc_collect();
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 基本: alloc
    // -------------------------------------------------------------------------

    #[test]
    fn test_gc_alloc_returns_non_null() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 16);
        assert!(!ptr.is_null());
        reset_gc();
    }

    #[test]
    fn test_gc_alloc_zero_size_object() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(0, 0);
        assert!(!ptr.is_null(), "zero-payload allocation should succeed");
        reset_gc();
    }

    #[test]
    fn test_gc_allocated_bytes_increases() {
        let _guard = gc_test_guard();
        reset_gc();
        let before = willow_gc_allocated_bytes();
        willow_alloc_object(1, 64);
        let after = willow_gc_allocated_bytes();
        assert!(after > before);
        reset_gc();
    }

    /// allocated_bytes はヘッダ込みのサイズを追跡していること
    #[test]
    fn test_gc_allocated_bytes_includes_header_overhead() {
        let _guard = gc_test_guard();
        reset_gc();
        let payload: i64 = 40;
        willow_alloc_object(1, payload);
        let expected = (header_size() as i64) + payload;
        assert_eq!(
            willow_gc_allocated_bytes(),
            expected,
            "allocated_bytes must include GcHeader overhead"
        );
        reset_gc();
    }

    /// total_allocs カウンタが増える
    #[test]
    fn test_gc_total_allocs_counter() {
        let _guard = gc_test_guard();
        reset_gc();
        let before = total_allocs();
        willow_alloc_object(1, 8);
        willow_alloc_object(1, 8);
        willow_alloc_object(1, 8);
        assert_eq!(total_allocs(), before + 3);
        reset_gc();
    }

    #[test]
    fn test_gc_alloc_wrapper_uses_opaque_type_and_zero_mask() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc(16);
        let header = payload_to_header(ptr);
        assert_eq!(unsafe { (*header).type_id }, 0);
        assert_eq!(unsafe { (*header).layout_id }, 0);
        assert_eq!(unsafe { (*header).gc_ref_mask }, 0);
        reset_gc();
    }

    #[test]
    fn test_gc_alloc_typed_records_ref_mask() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_typed(16, 0b10);
        let header = payload_to_header(ptr);
        assert_eq!(unsafe { (*header).gc_ref_mask }, 0b10);
        reset_gc();
    }

    #[test]
    fn test_gc_bitmap_and_inline_slots_follow_mixed_storage_layout() {
        let _guard = gc_test_guard();
        reset_gc();
        // Inline bit 1 and extended bit 65 have a scalar word immediately
        // before them. Both paths must locate the same storage-word stride.
        static BITMAP: [u64; 3] = [2, 0b10, 0b10];
        let bytes = 66 * GC_STORAGE_WORD_BYTES;
        let parent = willow_gc_alloc_bitmap(0, bytes as i64, BITMAP.as_ptr());
        let child = willow_alloc(8);
        let object = HeapObject::from_raw(payload_to_header(parent)).unwrap();
        let offsets = [
            willow_abi::WordLayout::new(&[willow_abi::SlotKind::Word])
                .byte_size(std::mem::size_of::<usize>() as u32) as usize,
            65 * GC_STORAGE_WORD_BYTES,
        ];
        for &offset in &offsets {
            unsafe {
                parent
                    .add(offset - GC_STORAGE_WORD_BYTES)
                    .cast::<i64>()
                    .write(i64::MAX);
                parent.add(offset).cast::<*mut u8>().write(child);
            }
        }
        let slots = object_reference_slots(object, &HashMap::new());
        assert_eq!(slots.len(), 2);
        for (slot, offset) in slots.iter().zip(offsets) {
            assert_eq!(*slot as usize - parent as usize, offset);
            assert_eq!(unsafe { **slot }, child);
        }
        assert_eq!(object.payload_word(1).unwrap().as_ptr(), child);
        assert_eq!(object.payload_word(65).unwrap().as_ptr(), child);
        let mut root = parent;
        willow_push_root(&mut root);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), obj_size(bytes) + obj_size(8));
        willow_pop_root();
        reset_gc();
    }

    #[test]
    fn test_gc_layout_allocation_records_central_metadata() {
        let _guard = gc_test_guard();
        reset_gc();
        assert_eq!(
            gc_layout_id(GcObjectKind::Enum, 16, 0, 0b10),
            0x17b2_8090_98b9_7b2d,
            "compiler/runtime layout fingerprint contract changed"
        );
        let ptr = willow_gc_alloc_layout(0xCAFE, 42, 24, 0b101);
        assert!(!ptr.is_null());
        let header = payload_to_header(ptr);
        assert_eq!(unsafe { (*header).layout_id }, 0xCAFE);
        assert_eq!(unsafe { (*header).type_id }, 42);
        assert_eq!(unsafe { (*header).gc_ref_mask }, 0b101);
        assert_eq!(unsafe { (*header).size }, header_size() + 24);
        reset_gc();
    }

    #[test]
    fn test_gc_write_barrier_ignores_old_to_old_store() {
        let _guard = gc_test_guard();
        reset_gc();
        let child = willow_alloc(8);
        let parent = willow_gc_alloc_layout(7, 0, 8, 0b1);
        willow_gc_write_barrier(parent, std::ptr::null_mut(), child, 1);
        assert_eq!(willow_gc_remembered_set_size(), 0);
        unsafe { *(parent as *mut *mut u8) = child };
        let mut slot = parent;
        willow_push_root(&mut slot as *mut *mut u8);
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(8) * 2,
            "old-to-old store must preserve mask-based tracing semantics"
        );
        willow_pop_root();
        reset_gc();
    }

    #[test]
    fn test_gc_barrier_verifier_rejects_unremembered_old_to_young_edge() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut tls = new_tlab_state();
        let young = willow_gc_alloc_slow(&mut tls, 51, 0, 8, 0);
        let parent = willow_gc_alloc_layout(52, 0, 8, 0b1);
        // Intentionally bypass the central barrier to prove verification catches it.
        unsafe { *(parent as *mut *mut u8) = young };
        let trace_registry = type_registry().lock().unwrap().clone();
        let result = {
            let mut state = runtime().heap.lock().unwrap();
            retire_all_tlabs_locked(&mut state);
            verify_remembered_set(&state, &trace_registry)
        };
        assert!(
            result
                .expect_err("missing barrier entry must be rejected")
                .contains("without a remembered-set entry")
        );
        reset_gc();
    }

    // Exercise the production sweep directly to isolate sweep work from mark
    // discovery and allocation costs. All fixture pointers remain GC-owned.
    #[test]
    fn test_gc_sweep_scaling_and_fragmentation() {
        let _guard = gc_test_guard();
        for large in [false, true] {
            for n in [32, 64, 128, 256] {
                reset_gc();
                set_threshold(usize::MAX);
                let payload_size = if large { GC_LARGE_OBJECT_THRESHOLD } else { 8 };
                let objects: Vec<_> = (0..2 * n)
                    .map(|_| willow_alloc_object(61, payload_size as i64))
                    .collect();
                for (index, &payload) in objects.iter().enumerate() {
                    unsafe {
                        *payload = (index % 251) as u8;
                        (*payload_to_header(payload)).marked = index % 2 == 1;
                    }
                }
                SWEEP_REGION_VISITS.store(0, Ordering::Relaxed);
                SWEEP_OBJECT_VISITS.store(0, Ordering::Relaxed);
                let start = std::time::Instant::now();
                assert_eq!(sweep(), n * (GC_HEADER_SIZE + payload_size));
                let elapsed = start.elapsed();
                assert_eq!(
                    SWEEP_REGION_VISITS.load(Ordering::Relaxed),
                    if large { 2 * n } else { 1 }
                );
                assert_eq!(SWEEP_OBJECT_VISITS.load(Ordering::Relaxed), 2 * n);
                let state = runtime().heap.lock().unwrap();
                verify_old_region_metadata(&state).unwrap();
                assert_eq!(state.allocated_bytes, n * (GC_HEADER_SIZE + payload_size));
                assert_eq!(state.old_regions.len(), if large { n } else { 1 });
                if !large {
                    assert_eq!(state.old_regions[0].free_spans.len(), n);
                }
                for (index, &payload) in objects.iter().enumerate().filter(|(i, _)| i % 2 == 1) {
                    assert_eq!(unsafe { *payload }, (index % 251) as u8);
                }
                drop(state);
                // The visit counts above are the proof; this line is the
                // separate pause measurement, and it carries the build
                // configuration it was taken in because the two are not
                // comparable across profiles (willow-ssl7.1, willow-tqzq).
                eprintln!(
                    "sweep profile={} large={large} dead={n} regions={} objects={} elapsed_ns={}",
                    if cfg!(debug_assertions) {
                        "debug"
                    } else {
                        "release"
                    },
                    SWEEP_REGION_VISITS.load(Ordering::Relaxed),
                    SWEEP_OBJECT_VISITS.load(Ordering::Relaxed),
                    elapsed.as_nanos()
                );
                // Cleared marks make the next sweep reclaim every survivor.
                assert_eq!(sweep(), n * (GC_HEADER_SIZE + payload_size));
                assert_eq!(willow_gc_old_region_count(), 0);
                assert_eq!(willow_gc_allocated_bytes(), 0);
            }
        }
        reset_gc();
    }

    #[test]
    fn test_gc_sweep_adjacent_gaps_tail_finalizers_and_cards() {
        static DROPS: AtomicUsize = AtomicUsize::new(0);
        unsafe fn count_drop(_: *mut u8) {
            DROPS.fetch_add(1, Ordering::Relaxed);
        }
        let _guard = gc_test_guard();
        reset_gc();
        set_threshold(usize::MAX);
        DROPS.store(0, Ordering::Relaxed);
        willow_register_drop(9876, count_drop);
        let objects: Vec<_> = (0..8).map(|_| willow_alloc_object(9876, 8)).collect();
        {
            let mut state = runtime().heap.lock().unwrap();
            for &payload in &objects {
                state.remembered_set.insert(payload as usize);
                state.dirty_cards.insert(payload as usize / GC_CARD_SIZE);
            }
        }
        for index in [0, 3, 5] {
            unsafe {
                (*payload_to_header(objects[index])).marked = true;
            }
        }
        sweep();
        assert_eq!(DROPS.load(Ordering::Relaxed), 5);
        {
            let state = runtime().heap.lock().unwrap();
            verify_old_region_metadata(&state).unwrap();
            let region = &state.old_regions[0];
            let span = GC_HEADER_SIZE + 8;
            assert_eq!(region.used, 6 * span);
            assert_eq!(region.free_spans.len(), 2);
            assert_eq!(region.free_spans[0].offset, span);
            assert_eq!(region.free_spans[0].size, 2 * span);
            assert_eq!(region.free_spans[1].offset, 4 * span);
            assert_eq!(region.free_spans[1].size, span);
            let expected: HashSet<_> = [0, 3, 5].map(|i| objects[i] as usize).into();
            assert_eq!(state.remembered_set, expected);
            assert_eq!(
                state.dirty_cards,
                expected.iter().map(|p| p / GC_CARD_SIZE).collect()
            );
        }
        assert_eq!(
            willow_alloc_object(61, (GC_HEADER_SIZE + 16) as i64),
            objects[1]
        );
        sweep();
        assert_eq!(DROPS.load(Ordering::Relaxed), 8);
        sweep();
        assert_eq!(DROPS.load(Ordering::Relaxed), 8);
        let state = runtime().heap.lock().unwrap();
        assert!(state.old_regions.is_empty());
        assert!(state.remembered_set.is_empty());
        assert!(state.dirty_cards.is_empty());
        assert_eq!(state.allocated_bytes, 0);
        drop(state);
        reset_gc();
    }

    #[test]
    fn test_gc_old_region_metadata_tracks_regular_allocation() {
        let _guard = gc_test_guard();
        reset_gc();
        let object = willow_alloc_object(61, 24);
        assert!(!object.is_null());

        let state = runtime().heap.lock().unwrap();
        assert_eq!(state.old_regions.len(), 1);
        let region = &state.old_regions[0];
        assert_eq!(region.kind, RegionKind::Old);
        assert!(region.contains(object as usize));
        assert_eq!(region.capacity, GC_OLD_REGION_SIZE);
        assert_eq!(region.allocations.len(), 1);
        assert_eq!(region.live_bytes, GC_HEADER_SIZE + 24);
        let offset = payload_to_header(object) as usize - region.start();
        assert!(region.mark_bitmap.is_marked(offset));
        drop(state);

        assert_eq!(willow_gc_old_region_count(), 1);
        assert_eq!(
            willow_gc_old_region_reserved_bytes(),
            GC_OLD_REGION_SIZE as i64
        );
        assert_eq!(
            willow_gc_old_region_live_bytes(),
            (GC_HEADER_SIZE + 24) as i64
        );
        reset_gc();
    }

    #[test]
    fn test_gc_old_region_sweep_creates_and_reuses_middle_hole() {
        let _guard = gc_test_guard();
        reset_gc();
        let a = willow_alloc_object(62, 8);
        let dead = willow_alloc_object(63, 8);
        let c = willow_alloc_object(64, 8);
        unsafe {
            *(a as *mut i64) = 10;
            *(c as *mut i64) = 30;
        }
        let mut a_root = a;
        let mut c_root = c;
        willow_push_root(&mut a_root);
        willow_push_root(&mut c_root);

        willow_gc_collect();

        assert_eq!(unsafe { *(a as *mut i64) }, 10);
        assert_eq!(unsafe { *(c as *mut i64) }, 30);
        assert_eq!(willow_gc_old_region_count(), 1);
        assert_eq!(
            willow_gc_old_region_fragmentation_bytes(),
            (GC_HEADER_SIZE + 8) as i64
        );
        let reuses = willow_gc_old_region_reuses();
        let replacement = willow_alloc_object(65, 8);
        assert_eq!(
            replacement, dead,
            "same-sized allocation should reuse the swept region-local span"
        );
        assert_eq!(willow_gc_old_region_reuses(), reuses + 1);
        assert_eq!(willow_gc_old_region_fragmentation_bytes(), 0);

        willow_pop_roots(2);
        willow_gc_collect();
        reset_gc();
    }

    #[test]
    fn test_gc_empty_old_region_is_released_after_major_sweep() {
        let _guard = gc_test_guard();
        reset_gc();
        let released = willow_gc_old_regions_released();
        let _garbage = willow_alloc_object(66, 8);
        assert_eq!(willow_gc_old_region_count(), 1);

        willow_gc_collect();

        assert_eq!(willow_gc_old_region_count(), 0);
        assert_eq!(willow_gc_old_region_reserved_bytes(), 0);
        assert_eq!(willow_gc_old_region_live_bytes(), 0);
        assert_eq!(willow_gc_old_regions_released(), released + 1);
        assert_eq!(willow_gc_major_collections(), 1);
        reset_gc();
    }

    #[test]
    fn test_gc_large_object_uses_dedicated_region_and_stays_non_moving() {
        let _guard = gc_test_guard();
        reset_gc();
        let payload_size = GC_LARGE_OBJECT_THRESHOLD;
        let mut large = willow_alloc_object(67, payload_size as i64);
        assert!(!large.is_null());
        unsafe {
            *large = 0xA5;
            *large.add(payload_size - 1) = 0x5A;
        }
        willow_push_root(&mut large);

        assert_eq!(willow_gc_large_object_region_count(), 1);
        assert_eq!(willow_gc_old_region_count(), 1);
        let address = large;
        willow_gc_collect();
        assert_eq!(large, address, "major collection must not move old objects");
        assert_eq!(unsafe { *large }, 0xA5);
        assert_eq!(unsafe { *large.add(payload_size - 1) }, 0x5A);
        assert_eq!(willow_gc_large_object_region_count(), 1);

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_large_object_region_count(), 0);
        assert_eq!(willow_gc_old_region_count(), 0);
        reset_gc();
    }

    #[test]
    fn test_gc_regular_old_allocation_rolls_over_to_multiple_regions() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut roots = Vec::with_capacity(6000);
        for value in 0..6000i64 {
            let object = willow_alloc_object(70, 8);
            unsafe { *(object as *mut i64) = value };
            roots.push(object);
        }
        for root in &mut roots {
            willow_push_root(root);
        }
        assert!(
            willow_gc_old_region_count() >= 2,
            "regular allocations must roll over at the region bound"
        );
        let first = roots[0];
        let last = roots[5999];

        willow_gc_collect();

        assert_eq!(roots[0], first);
        assert_eq!(roots[5999], last);
        assert_eq!(unsafe { *(roots[0] as *mut i64) }, 0);
        assert_eq!(unsafe { *(roots[5999] as *mut i64) }, 5999);
        willow_pop_roots(roots.len() as i32);
        willow_gc_collect();
        assert_eq!(willow_gc_old_region_count(), 0);
        reset_gc();
    }

    #[test]
    fn test_gc_minor_promotion_target_is_old_region_backed() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut tls = new_tlab_state();
        let young = willow_gc_alloc_slow(&mut tls, 68, 0, 8, 0);
        let mut parent = willow_gc_alloc_layout(69, 0, 8, 0b1);
        willow_gc_write_barrier(
            parent,
            std::ptr::null_mut(),
            young,
            GcStoreDestination::ObjectField as i64,
        );
        unsafe { *(parent as *mut *mut u8) = young };
        willow_push_root(&mut parent);

        willow_gc_minor_collect();

        let promoted = unsafe { *(parent as *mut *mut u8) };
        let state = runtime().heap.lock().unwrap();
        let region = state
            .old_regions
            .iter()
            .find(|region| region.contains(promoted as usize))
            .expect("copied young survivor must be allocated in an old region");
        assert_eq!(region.kind, RegionKind::Old);
        drop(state);
        assert_eq!(willow_gc_remembered_set_size(), 0);

        willow_pop_root();
        willow_gc_collect();
        reset_gc();
    }

    #[test]
    fn test_gc_alloc_typed_mask_traces_child_pointer_slot() {
        let _guard = gc_test_guard();
        reset_gc();
        let child = willow_alloc(8);
        let parent = willow_alloc_typed(8, 0b1);
        unsafe { *(parent as *mut *mut u8) = child };
        let mut slot = parent;
        willow_push_root(&mut slot as *mut *mut u8);
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(8) * 2,
            "mask-traced child should survive with rooted parent"
        );
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 回収: unreachable objects
    // -------------------------------------------------------------------------

    #[test]
    fn test_gc_collect_frees_unreachable_objects() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_alloc_object(1, 128);
        let before = willow_gc_allocated_bytes();
        assert!(before > 0);
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            0,
            "unrooted object should be freed"
        );
        reset_gc();
    }

    /// collect 後に total_frees が増えること
    #[test]
    fn test_gc_total_frees_counter_after_collect() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_alloc_object(1, 16);
        willow_alloc_object(1, 16);
        let before = total_frees();
        willow_gc_collect();
        assert_eq!(
            total_frees(),
            before + 2,
            "two unrooted objects should be freed"
        );
        reset_gc();
    }

    /// collect を複数回呼んでも壊れない (2回目以降は何も回収しない)
    #[test]
    fn test_gc_collect_idempotent_on_empty_heap() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_gc_collect();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    /// collect 後に生き残ったオブジェクトの mark bit がリセットされること
    /// (リセットされないと次サイクルで全部生存扱いになる)
    #[test]
    fn test_gc_survivor_mark_bit_cleared_after_collection() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 16);
        let mut slot: *mut u8 = ptr;
        willow_push_root(&mut slot as *mut *mut u8);

        // 1回目のGC: 生き残る
        willow_gc_collect();
        // mark bit が false に戻っているかヘッダで確認
        let hdr = payload_to_header(ptr);
        assert!(
            !unsafe { (*hdr).marked },
            "mark bit must be cleared after collection"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // ルート管理
    // -------------------------------------------------------------------------

    #[test]
    fn test_gc_collect_preserves_rooted_objects() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(2, 32);
        let mut slot: *mut u8 = ptr;
        willow_push_root(&mut slot as *mut *mut u8);
        willow_gc_collect();
        assert!(
            willow_gc_allocated_bytes() > 0,
            "rooted object must survive"
        );
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            0,
            "unrooted object freed after pop"
        );
        reset_gc();
    }

    #[test]
    fn test_gc_runtime_root_preserves_object_without_stack_root() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(2, 32);

        willow_gc_add_runtime_root(ptr);
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(32),
            "persistent runtime root must keep object alive"
        );

        willow_gc_remove_runtime_root(ptr);
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            0,
            "object should be collectible after runtime root removal"
        );
        reset_gc();
    }

    #[test]
    fn runtime_root_set_distinct_snapshots_and_balanced_owners() {
        for distinct in [16, 64, 256] {
            let roots = RuntimeRootSet::default();
            let mut objects = vec![0u8; distinct];
            let pointers: Vec<_> = objects.iter_mut().map(|object| object as *mut u8).collect();
            roots.add(std::ptr::null_mut());
            roots.remove(std::ptr::null_mut());
            assert_eq!(roots.len(), 0);
            for &pointer in &pointers {
                for _ in 0..4 {
                    roots.add(pointer);
                }
            }
            assert_eq!(roots.len(), distinct);
            let mut snapshot = roots.snapshot();
            snapshot.sort_unstable();
            assert_eq!(snapshot, pointers);
            for remaining_owners in (0..4).rev() {
                for &pointer in &pointers {
                    roots.remove(pointer);
                }
                assert_eq!(
                    roots.len(),
                    if remaining_owners == 0 { 0 } else { distinct }
                );
            }
            assert!(roots.snapshot().is_empty());
            // Removing an absent root must remain a no-op.
            roots.remove(pointers[0]);
            roots.add(pointers[0]);
            roots.clear();
            assert_eq!(roots.len(), 0);
            assert!(roots.snapshot().is_empty());
        }
    }

    #[test]
    fn test_gc_runtime_root_ignores_null_and_ref_counts_retentions() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(2, 32);

        willow_gc_add_runtime_root(std::ptr::null_mut());
        willow_gc_add_runtime_root(ptr);
        willow_gc_add_runtime_root(ptr);
        assert_eq!(runtime().runtime_roots.len(), 1);
        assert_eq!(runtime().runtime_roots.snapshot(), vec![ptr]);

        willow_gc_remove_runtime_root(std::ptr::null_mut());
        assert_eq!(runtime().runtime_roots.len(), 1);
        assert_eq!(runtime().runtime_roots.snapshot(), vec![ptr]);

        willow_gc_remove_runtime_root(ptr);
        assert_eq!(runtime().runtime_roots.len(), 1);
        assert_eq!(runtime().runtime_roots.snapshot(), vec![ptr]);
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(32),
            "one remaining runtime retention must keep the object alive"
        );

        willow_gc_remove_runtime_root(ptr);
        assert_eq!(runtime().runtime_roots.len(), 0);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    #[test]
    fn test_gc_foreign_thread_collect_skips_live_root_stack_owner() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(2, 32);
        let ptr_addr = ptr as usize;
        let (rooted_tx, rooted_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();

        let owner_thread = std::thread::spawn(move || {
            let mut slot = ptr_addr as *mut u8;
            willow_push_root(&mut slot as *mut *mut u8);
            rooted_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            willow_pop_root();
        });

        rooted_rx.recv().unwrap();
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(32),
            "a foreign-thread collection must not scan only its own root stack"
        );

        release_tx.send(()).unwrap();
        owner_thread.join().unwrap();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    #[cfg(debug_assertions)]
    #[test]
    fn test_gc_debug_validation_rejects_invalid_runtime_root_pointer() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_gc_add_runtime_root(std::ptr::dangling_mut::<u8>());

        let result = std::panic::catch_unwind(collect_internal);
        let err = result.expect_err("invalid runtime root must fail clearly");
        let message = err
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| err.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(message.contains("invalid GC pointer"), "{message}");
        assert!(message.contains("current GC heap"), "{message}");
        reset_gc();
    }

    #[test]
    fn test_gc_root_push_pop_symmetry() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut slot1: *mut u8 = std::ptr::null_mut();
        let mut slot2: *mut u8 = std::ptr::null_mut();
        willow_push_root(&mut slot1 as *mut *mut u8);
        willow_push_root(&mut slot2 as *mut *mut u8);
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 2));
        willow_pop_root();
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 1));
        willow_pop_roots(1);
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 0));
        reset_gc();
    }

    #[test]
    fn test_gc_root_depth_mirror_tracks_mutations_and_stack_transfers() {
        let _guard = gc_test_guard();
        reset_gc();
        let check = |expected: usize| {
            ROOT_STACK.with(|stack| assert_eq!(stack.borrow().len(), expected));
            assert_eq!(gc_thread_root_depth(), expected);
            assert_eq!(willow_root_depth(), expected as i32);
        };
        check(0);
        let mut slots = [std::ptr::null_mut(); 3];
        for (index, slot) in slots.iter_mut().enumerate() {
            willow_push_root(slot);
            check(index + 1);
        }
        // A new worker's view must not inherit this worker's three roots.
        std::thread::spawn(|| {
            assert_eq!(willow_root_depth(), 0);
            assert_eq!(gc_thread_root_depth(), 0);
        })
        .join()
        .unwrap();
        check(3);
        willow_pop_roots(0);
        check(3);
        // Slots stay alive and no collection/safepoint occurs while detached.
        let token = unsafe { park_current_roots(1) };
        check(1);
        unsafe { resume_parked_roots(token) };
        check(3);
        let token = unsafe { park_current_roots(0) };
        check(0);
        unsafe { resume_parked_roots(token) };
        check(3);
        willow_pop_root();
        check(2);
        willow_pop_roots(1);
        check(1);
        willow_pop_roots(100);
        check(0);
        willow_pop_root();
        check(0);
        willow_push_root(&mut slots[0]);
        willow_pop_roots(-1); // Preserve the existing clamp behavior.
        check(0);
        willow_push_root(&mut slots[0]);
        reset_gc();
        check(0);
    }

    #[test]
    fn test_gc_root_depth_mirror_overflow_is_fatal() {
        const CHILD: &str = "WILLOW_ROOT_DEPTH_OVERFLOW_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            ROOT_DEPTH.set(i32::MAX as usize + 1);
            willow_root_depth();
            panic!("root depth overflow unexpectedly returned");
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "gc::tests::test_gc_root_depth_mirror_overflow_is_fatal",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("runtime fatal: generated-code root depth overflow")
        );
    }

    /// pop_roots(0) はスタックを変えない
    #[test]
    fn test_gc_pop_roots_zero_is_noop() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut slot: *mut u8 = std::ptr::null_mut();
        willow_push_root(&mut slot as *mut *mut u8);
        willow_pop_roots(0);
        ROOT_STACK
            .with(|rs| assert_eq!(rs.borrow().len(), 1, "pop_roots(0) must not change stack"));
        willow_pop_root();
        reset_gc();
    }

    /// pop_roots(n > stack size) はアンダーフローしない
    #[test]
    fn test_gc_pop_roots_excess_clamps_to_zero() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut slot: *mut u8 = std::ptr::null_mut();
        willow_push_root(&mut slot as *mut *mut u8);
        // スタックに1つしかないのに100個pop → 0になるだけ、クラッシュしない
        willow_pop_roots(100);
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 0, "stack must clamp to 0"));
        reset_gc();
    }

    /// スロットの値が null のルートがあっても GC はクラッシュしない
    #[test]
    fn test_gc_null_root_value_does_not_crash() {
        let _guard = gc_test_guard();
        reset_gc();
        // slot は非null だが、slot の指す先 (ポインタ値) が null
        let mut slot: *mut u8 = std::ptr::null_mut();
        willow_push_root(&mut slot as *mut *mut u8);
        // GC はこの null ポインタをスキップするだけ、クラッシュしない
        willow_gc_collect();
        willow_pop_root();
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 複合シナリオ
    // -------------------------------------------------------------------------

    #[test]
    fn test_gc_multiple_allocs_and_collect() {
        let _guard = gc_test_guard();
        reset_gc();
        for _ in 0..9 {
            willow_alloc_object(1, 16);
        }
        let last_ptr = willow_alloc_object(1, 16);
        let mut slot = last_ptr;
        willow_push_root(&mut slot as *mut *mut u8);

        willow_gc_collect();

        let expected = (header_size() + 16) as i64;
        assert_eq!(
            willow_gc_allocated_bytes(),
            expected,
            "exactly one object must survive"
        );
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    /// 100個確保して1つだけルートを張る → collect後にちょうど1個残る
    #[test]
    fn test_gc_large_population_single_survivor() {
        let _guard = gc_test_guard();
        reset_gc();
        for _ in 0..99 {
            willow_alloc_object(1, 8);
        }
        let survivor = willow_alloc_object(2, 8);
        let mut slot = survivor;
        willow_push_root(&mut slot as *mut *mut u8);

        willow_gc_collect();

        let expected = (header_size() + 8) as i64;
        assert_eq!(
            willow_gc_allocated_bytes(),
            expected,
            "100 objects allocated, only the rooted one should survive"
        );
        assert_eq!(total_frees(), 99, "99 unreachable objects should be freed");
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    /// collect後にまた確保できること (ヒープが壊れていないこと)
    #[test]
    fn test_gc_reallocation_after_collection() {
        let _guard = gc_test_guard();
        reset_gc();
        // 1回目の確保・回収サイクル
        willow_alloc_object(1, 32);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);

        // 2回目: 回収後も新規確保できること
        let ptr = willow_alloc_object(1, 32);
        assert!(!ptr.is_null(), "allocation after collection must succeed");
        assert_eq!(willow_gc_allocated_bytes(), (header_size() + 32) as i64);

        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    /// しきい値を超えた確保が自動でGCをトリガーすること
    #[test]
    fn test_gc_auto_trigger_by_threshold() {
        let _guard = gc_test_guard();
        reset_gc();

        // まず1つ確保してしきい値をそのオブジェクトのサイズより小さく設定
        let obj1 = willow_alloc_object(1, 16);
        assert!(!obj1.is_null());
        let bytes_after_first = willow_gc_allocated_bytes();

        // しきい値を「現在の確保量より小さい値」に設定
        // → 次の willow_alloc_object の冒頭で auto-collect が走る
        set_threshold(1); // 1 バイト → 確実に超えている

        let frees_before = total_frees();

        // obj1 はルートなし → 自動GCで回収されるはず
        let obj2 = willow_alloc_object(1, 16);
        assert!(!obj2.is_null());

        // obj1 (unrooted) が回収されて obj2 だけ残っているはず
        let expected = (header_size() + 16) as i64;
        assert_eq!(
            willow_gc_allocated_bytes(),
            expected,
            "auto-triggered GC should have freed obj1"
        );
        assert!(
            total_frees() > frees_before,
            "auto-triggered GC should have incremented total_frees"
        );
        let _ = bytes_after_first;

        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    /// 複数ルートのうち1つだけ外すと、外したものだけ回収される
    #[test]
    fn test_gc_partial_roots_partial_collection() {
        let _guard = gc_test_guard();
        reset_gc();

        let ptr_a = willow_alloc_object(1, 16);
        let ptr_b = willow_alloc_object(2, 16);
        let mut slot_a: *mut u8 = ptr_a;
        let mut slot_b: *mut u8 = ptr_b;
        willow_push_root(&mut slot_a as *mut *mut u8); // A をルート
        willow_push_root(&mut slot_b as *mut *mut u8); // B をルート

        // B のルートを外す
        willow_pop_root();

        willow_gc_collect();

        // A だけ生き残っているはず
        let expected = (header_size() + 16) as i64;
        assert_eq!(
            willow_gc_allocated_bytes(),
            expected,
            "only A (still rooted) should survive"
        );

        willow_pop_root(); // A のルートを外す
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // =========================================================================
    // 観点1: 大きなペイロード (1MB) の確保が成功する
    // =========================================================================
    #[test]
    fn test_gc_large_payload_alloc_succeeds() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 1024 * 1024);
        assert!(!ptr.is_null(), "1 MiB payload allocation must succeed");
        reset_gc();
    }

    // Consecutive allocations must enter the authoritative region index.
    #[test]
    fn test_gc_consecutive_allocs_update_region_index() {
        let _guard = gc_test_guard();
        reset_gc();
        let first = willow_alloc_object(1, 8);
        let second = willow_alloc_object(2, 8);
        let state = runtime().heap.lock().unwrap();
        let objects: HashSet<_> = old_region_objects(&state)
            .map(|object| object.payload().as_ptr())
            .collect();
        assert_eq!(objects, HashSet::from([first, second]));
        drop(state);
        reset_gc();
    }

    // 観点3: 確保したペイロード領域に読み書きできる
    #[test]
    fn test_gc_payload_is_readable_writable() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 8) as *mut i64;
        unsafe {
            *ptr = 0x0DEADBEEF_i64;
            assert_eq!(*ptr, 0x0DEADBEEF_i64);
        }
        reset_gc();
    }

    // 観点4: 複数の type_id を持つオブジェクトが混在して確保できる
    #[test]
    fn test_gc_multiple_type_ids_work() {
        let _guard = gc_test_guard();
        reset_gc();
        let p1 = willow_alloc_object(1, 8);
        let p2 = willow_alloc_object(2, 8);
        let p3 = willow_alloc_object(99, 8);
        assert!(!p1.is_null() && !p2.is_null() && !p3.is_null());
        unsafe {
            assert_eq!((*payload_to_header(p1)).type_id, 1);
            assert_eq!((*payload_to_header(p2)).type_id, 2);
            assert_eq!((*payload_to_header(p3)).type_id, 99);
        }
        reset_gc();
    }

    // 観点5: 確保直後の GcHeader.marked が false
    #[test]
    fn test_gc_header_marked_false_after_alloc() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 8);
        assert!(!unsafe { (*payload_to_header(ptr)).marked });
        reset_gc();
    }

    // 観点6: 確保直後の GcHeader.type_id が指定値と一致
    #[test]
    fn test_gc_header_type_id_matches() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(42, 8);
        assert_eq!(unsafe { (*payload_to_header(ptr)).type_id }, 42);
        reset_gc();
    }

    // 観点7: GcHeader.size がヘッダ+ペイロードと一致
    #[test]
    fn test_gc_header_size_field_matches_total() {
        let _guard = gc_test_guard();
        reset_gc();
        let payload: i64 = 24;
        let ptr = willow_alloc_object(1, payload);
        let expected = header_size() + payload as usize;
        assert_eq!(unsafe { (*payload_to_header(ptr)).size }, expected);
        reset_gc();
    }

    // The retired linkage word remains zero until the compact-header ABI lands.
    #[test]
    fn test_gc_region_allocations_do_not_publish_heap_links() {
        let _guard = gc_test_guard();
        reset_gc();
        let first = willow_alloc_object(1, 8);
        let second = willow_alloc_object(2, 8);
        assert!(unsafe { (*payload_to_header(first)).next }.is_null());
        assert!(unsafe { (*payload_to_header(second)).next }.is_null());
        reset_gc();
    }

    // =========================================================================
    // 観点9: push_root n回でスタックが n 増える
    // =========================================================================
    #[test]
    fn test_gc_push_root_n_times_increases_stack_by_n() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut slots = [std::ptr::null_mut::<u8>(); 5];
        for s in slots.iter_mut() {
            willow_push_root(s as *mut *mut u8);
        }
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 5));
        willow_pop_roots(5);
        reset_gc();
    }

    // 観点10: pop_roots(n) でちょうど n 個減る
    #[test]
    fn test_gc_pop_roots_n_decreases_by_exactly_n() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut slot = std::ptr::null_mut::<u8>();
        for _ in 0..6 {
            willow_push_root(&mut slot as *mut *mut u8);
        }
        willow_pop_roots(4);
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 2));
        willow_pop_roots(2);
        reset_gc();
    }

    // 観点11: push→pop→push のスロット再利用
    #[test]
    fn test_gc_push_pop_push_slot_reuse() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr1 = willow_alloc_object(1, 8);
        let mut slot: *mut u8 = ptr1;
        willow_push_root(&mut slot as *mut *mut u8);
        willow_gc_collect(); // ptr1 survives
        assert_eq!(willow_gc_allocated_bytes(), obj_size(8));
        willow_pop_root();

        // 新しいオブジェクトを確保してスロットを再利用
        let ptr2 = willow_alloc_object(2, 8);
        slot = ptr2;
        willow_push_root(&mut slot as *mut *mut u8);
        willow_gc_collect(); // ptr1 freed, ptr2 survives
        assert_eq!(willow_gc_allocated_bytes(), obj_size(8));

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // 観点12: 同じスロットを2回 push するとスタックに2エントリ入る
    #[test]
    fn test_gc_same_slot_pushed_twice_creates_two_entries() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut slot = std::ptr::null_mut::<u8>();
        willow_push_root(&mut slot as *mut *mut u8);
        willow_push_root(&mut slot as *mut *mut u8);
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 2));
        willow_pop_roots(2);
        reset_gc();
    }

    // 観点13: 空スタックで pop_root を呼んでもクラッシュしない
    #[test]
    fn test_gc_pop_root_on_empty_stack_no_crash() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_pop_root();
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 0));
        reset_gc();
    }

    // 観点14: 空スタックで pop_roots(n) を呼んでもクラッシュしない
    #[test]
    fn test_gc_pop_roots_on_empty_stack_no_crash() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_pop_roots(5);
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 0));
        reset_gc();
    }

    // 観点15: 4000個近くまで push_root できる
    #[test]
    fn test_gc_push_root_near_max_capacity() {
        let _guard = gc_test_guard();
        reset_gc();
        const N: usize = 4000;
        let mut slots = vec![std::ptr::null_mut::<u8>(); N];
        for s in slots.iter_mut() {
            willow_push_root(s as *mut *mut u8);
        }
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), N));
        willow_pop_roots(N as i32);
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 0));
        reset_gc();
    }

    // =========================================================================
    // 観点16-17: マークフェーズ
    // =========================================================================

    // 観点16: 同じオブジェクトを2つのルートが指しても二重マークでクラッシュしない
    #[test]
    fn test_gc_two_roots_same_object_no_double_mark_crash() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 8);
        let mut s1: *mut u8 = ptr;
        let mut s2: *mut u8 = ptr;
        willow_push_root(&mut s1 as *mut *mut u8);
        willow_push_root(&mut s2 as *mut *mut u8);
        willow_gc_collect(); // must not crash or double-free
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(8),
            "object must survive"
        );
        willow_pop_roots(2);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // =========================================================================
    // 観点21-25: スイープフェーズ
    // =========================================================================

    // 観点21: ヒープ先頭 (heap_head) が unreachable でも正しく回収
    #[test]
    fn test_gc_sweep_head_object_unreachable() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr_a = willow_alloc_object(1, 8); // first alloc → becomes tail
        let _ptr_b = willow_alloc_object(2, 8); // second alloc → becomes head (unreachable)
        let mut slot_a: *mut u8 = ptr_a;
        willow_push_root(&mut slot_a as *mut *mut u8); // only A rooted
        willow_gc_collect();
        // B (head) freed, A (tail→new head) survives
        assert_eq!(willow_gc_allocated_bytes(), obj_size(8));
        assert_eq!(
            old_region_objects(&runtime().heap.lock().unwrap())
                .map(HeapObject::as_ptr)
                .collect::<Vec<_>>(),
            vec![payload_to_header(ptr_a)]
        );
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // 観点22: ヒープ末尾が unreachable でも正しく回収
    #[test]
    fn test_gc_sweep_tail_object_unreachable() {
        let _guard = gc_test_guard();
        reset_gc();
        let _ptr_a = willow_alloc_object(1, 8); // first → tail (unreachable)
        let ptr_b = willow_alloc_object(2, 8); // second → head (rooted)
        let mut slot_b: *mut u8 = ptr_b;
        willow_push_root(&mut slot_b as *mut *mut u8);
        willow_gc_collect();
        // A (tail) freed, B (head) survives
        assert_eq!(willow_gc_allocated_bytes(), obj_size(8));
        assert_eq!(
            old_region_objects(&runtime().heap.lock().unwrap())
                .map(HeapObject::as_ptr)
                .collect::<Vec<_>>(),
            vec![payload_to_header(ptr_b)]
        );
        assert!(unsafe { (*payload_to_header(ptr_b)).next }.is_null());
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // 観点23: ヒープ中間のオブジェクトだけ unreachable でも正しく回収
    #[test]
    fn test_gc_sweep_middle_object_unreachable() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr_a = willow_alloc_object(1, 8); // tail
        let _ptr_b = willow_alloc_object(2, 8); // middle (unreachable)
        let ptr_c = willow_alloc_object(3, 8); // head
        let mut sa: *mut u8 = ptr_a;
        let mut sc: *mut u8 = ptr_c;
        willow_push_root(&mut sa as *mut *mut u8);
        willow_push_root(&mut sc as *mut *mut u8);
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(8) * 2,
            "A and C survive, B freed"
        );
        willow_pop_roots(2);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // 観点24: sweep後にヒープリストが null で終端される
    #[test]
    fn test_gc_heap_null_terminated_after_sweep() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 8);
        let mut slot: *mut u8 = ptr;
        willow_push_root(&mut slot as *mut *mut u8);
        willow_gc_collect();
        assert!(unsafe { (*payload_to_header(ptr)).next }.is_null());
        willow_pop_root();
        willow_gc_collect();
        reset_gc();
    }

    // 観点25: 全員生き残ったときリンクリストが壊れていない
    #[test]
    fn test_gc_survivors_remain_indexed_after_sweep() {
        let _guard = gc_test_guard();
        reset_gc();
        let pa = willow_alloc_object(1, 8);
        let pb = willow_alloc_object(2, 8);
        let pc = willow_alloc_object(3, 8);
        let mut sa: *mut u8 = pa;
        let mut sb: *mut u8 = pb;
        let mut sc: *mut u8 = pc;
        willow_push_root(&mut sa as *mut *mut u8);
        willow_push_root(&mut sb as *mut *mut u8);
        willow_push_root(&mut sc as *mut *mut u8);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), obj_size(8) * 3);
        let objects: HashSet<_> = old_region_objects(&runtime().heap.lock().unwrap())
            .map(|object| object.payload().as_ptr())
            .collect();
        assert_eq!(objects, HashSet::from([pa, pb, pc]));
        willow_pop_roots(3);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // =========================================================================
    // 観点26-29: allocated_bytes の正確性
    // =========================================================================

    // 観点26: 確保するたびに正確に (header+payload) ずつ増える
    #[test]
    fn test_gc_allocated_bytes_grows_precisely_per_alloc() {
        let _guard = gc_test_guard();
        reset_gc();
        let step = obj_size(16);
        willow_alloc_object(1, 16);
        assert_eq!(willow_gc_allocated_bytes(), step);
        willow_alloc_object(1, 16);
        assert_eq!(willow_gc_allocated_bytes(), step * 2);
        willow_alloc_object(1, 16);
        assert_eq!(willow_gc_allocated_bytes(), step * 3);
        reset_gc();
    }

    // 観点28: partial collect で回収分だけ減り、生存分は保持される
    #[test]
    fn test_gc_partial_collect_bytes_accurate() {
        let _guard = gc_test_guard();
        reset_gc();
        let pa = willow_alloc_object(1, 8); // freed
        let pb = willow_alloc_object(2, 8); // survives
        let _pc = willow_alloc_object(3, 8); // freed
        let mut sb: *mut u8 = pb;
        willow_push_root(&mut sb as *mut *mut u8);
        assert_eq!(willow_gc_allocated_bytes(), obj_size(8) * 3);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), obj_size(8), "only B survives");
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        let _ = pa;
        reset_gc();
    }

    // 観点29: 5つルートを張った5個全員の合計バイトが正確
    #[test]
    fn test_gc_five_roots_five_survivors_bytes() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut ptrs: Vec<*mut u8> = (0..5).map(|i| willow_alloc_object(i, 8)).collect();
        for p in ptrs.iter_mut() {
            willow_push_root(p as *mut *mut u8);
        }
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), obj_size(8) * 5);
        willow_pop_roots(5);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // =========================================================================
    // 観点30-34: threshold / 自動トリガー
    // =========================================================================

    // 観点30: threshold 未満では auto-collect が走らない
    #[test]
    fn test_gc_no_auto_trigger_below_threshold() {
        let _guard = gc_test_guard();
        reset_gc();
        set_threshold(usize::MAX);
        let before = total_frees();
        for _ in 0..10 {
            willow_alloc_object(1, 8);
        }
        assert_eq!(total_frees(), before, "no auto-collect should have run");
        reset_gc();
    }

    // 観点31: auto-collect 後に threshold が2倍になる
    #[test]
    fn test_gc_threshold_doubles_after_auto_trigger() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_alloc_object(1, 8); // allocated_bytes = header+8
        set_threshold(1); // 現在の allocated_bytes より小さく設定
        willow_alloc_object(1, 8); // 先頭で auto-collect、その後確保
        let new_threshold = runtime().heap.lock().unwrap().threshold_bytes;
        assert!(
            new_threshold >= 2,
            "threshold must have at least doubled from 1"
        );
        reset_gc();
    }

    // 観点32: auto-collect 後に新規確保が正常にできる
    #[test]
    fn test_gc_realloc_after_auto_trigger_works() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_alloc_object(1, 16);
        set_threshold(1);
        let ptr = willow_alloc_object(1, 16);
        assert!(!ptr.is_null());
        assert!(willow_gc_allocated_bytes() > 0);
        reset_gc();
    }

    // 観点33: threshold=1 で毎回 auto-collect がトリガーされる
    #[test]
    fn test_gc_threshold_one_every_alloc_triggers_collect() {
        let _guard = gc_test_guard();
        reset_gc();
        set_threshold(1);
        let before = total_frees();
        for _ in 0..5 {
            willow_alloc_object(1, 8);
        }
        assert!(
            total_frees() > before,
            "auto-collect must fire at least once"
        );
        reset_gc();
    }

    // 観点34: 100回 auto-collect サイクルを繰り返してもメモリリークなし
    #[test]
    fn test_gc_100_auto_trigger_cycles_no_leak() {
        let _guard = gc_test_guard();
        reset_gc();
        set_threshold(1);
        for _ in 0..100 {
            willow_alloc_object(1, 8);
        }
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // =========================================================================
    // 観点35-39: ライフサイクル
    // =========================================================================

    // 観点35: alloc→root→collect(生存)→unroot→collect(回収) の完全1サイクル
    #[test]
    fn test_gc_full_lifecycle() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 16);
        let mut slot: *mut u8 = ptr;
        willow_push_root(&mut slot as *mut *mut u8);
        willow_gc_collect();
        assert!(
            willow_gc_allocated_bytes() > 0,
            "rooted object must survive"
        );
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            0,
            "unrooted object must be freed"
        );
        reset_gc();
    }

    // 観点36: rooted のまま collect を 10回繰り返しても毎回生き残る
    #[test]
    fn test_gc_repeated_collect_rooted_object_survives() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 8);
        let mut slot: *mut u8 = ptr;
        willow_push_root(&mut slot as *mut *mut u8);
        for i in 0..10 {
            willow_gc_collect();
            assert!(
                willow_gc_allocated_bytes() > 0,
                "object must survive collection #{i}"
            );
        }
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // 観点37: alloc→collect を 100サイクル繰り返してもリークなし
    #[test]
    fn test_gc_100_alloc_collect_cycles_no_leak() {
        let _guard = gc_test_guard();
        reset_gc();
        for _ in 0..100 {
            willow_alloc_object(1, 16);
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), 0);
        }
        reset_gc();
    }

    // 観点38: collect 後も生き残ったオブジェクトのペイロード値が変化していない
    #[test]
    fn test_gc_payload_unchanged_after_collection() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 8) as *mut i64;
        unsafe {
            *ptr = 0xCAFEBABE_i64;
        }
        let mut slot: *mut u8 = ptr as *mut u8;
        willow_push_root(&mut slot as *mut *mut u8);
        willow_gc_collect();
        assert_eq!(
            unsafe { *ptr },
            0xCAFEBABE_i64,
            "GC must not corrupt payload data"
        );
        willow_pop_root();
        reset_gc();
    }

    // 観点39: unroot 直後の collect でオブジェクトが即座に回収される
    #[test]
    fn test_gc_collect_immediately_after_unroot() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 8);
        let mut slot: *mut u8 = ptr;
        willow_push_root(&mut slot as *mut *mut u8);
        willow_gc_collect();
        assert!(willow_gc_allocated_bytes() > 0);
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            0,
            "must be freed immediately after unroot"
        );
        reset_gc();
    }

    // =========================================================================
    // 観点40-42: カウンタの正確性
    // =========================================================================

    // 観点40: reset 後に total_allocs が 0 から始まる
    #[test]
    fn test_gc_total_allocs_starts_at_zero_after_reset() {
        let _guard = gc_test_guard();
        reset_gc();
        assert_eq!(total_allocs(), 0);
        reset_gc();
    }

    // 観点42: partial collect で total_frees が回収個数分だけ増える
    #[test]
    fn test_gc_partial_collect_total_frees_count() {
        let _guard = gc_test_guard();
        reset_gc();
        for _ in 0..5 {
            willow_alloc_object(1, 8);
        }
        let survivor = willow_alloc_object(1, 8);
        let mut slot: *mut u8 = survivor;
        willow_push_root(&mut slot as *mut *mut u8);
        let before = total_frees();
        willow_gc_collect();
        assert_eq!(
            total_frees(),
            before + 5,
            "exactly 5 unrooted objects freed"
        );
        willow_pop_root();
        reset_gc();
    }

    // =========================================================================
    // 観点43-46: エッジケース / 境界値
    // =========================================================================

    // 観点43: ペイロード 1 バイトの確保が成功する
    #[test]
    fn test_gc_alloc_payload_size_one() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 1);
        assert!(!ptr.is_null());
        reset_gc();
    }

    // 観点44: 奇数ペイロードサイズでも正しく動く
    #[test]
    fn test_gc_alloc_odd_payload_sizes() {
        let _guard = gc_test_guard();
        reset_gc();
        for &size in &[1i64, 3, 5, 7, 9, 11] {
            let ptr = willow_alloc_object(1, size);
            assert!(!ptr.is_null(), "alloc of {size} bytes must succeed");
        }
        reset_gc();
    }

    // 観点45: 10,000個確保して全部回収される
    #[test]
    fn test_gc_ten_thousand_allocs_all_freed() {
        let _guard = gc_test_guard();
        reset_gc();
        set_threshold(usize::MAX); // 自動トリガー無効
        for _ in 0..10_000 {
            willow_alloc_object(1, 8);
        }
        assert!(willow_gc_allocated_bytes() > 0);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // 観点46: 確保→回収サイクルを 20回繰り返した後にヒープが空
    #[test]
    fn test_gc_empty_heap_after_many_cycles() {
        let _guard = gc_test_guard();
        reset_gc();
        for _ in 0..20 {
            for _ in 0..10 {
                willow_alloc_object(1, 8);
            }
            willow_gc_collect();
            assert_eq!(
                willow_gc_allocated_bytes(),
                0,
                "heap must be empty after each cycle"
            );
        }
        reset_gc();
    }

    // =========================================================================
    // 観点47-48: payload_to_header ヘルパー
    // =========================================================================

    // 観点47: alloc から返ったポインタを payload_to_header に渡すと元のヘッダが返る
    #[test]
    fn test_gc_payload_to_header_roundtrip() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(7, 16);
        let hdr = payload_to_header(ptr);
        assert_eq!(unsafe { (*hdr).type_id }, 7);
        let expected_payload = unsafe { (hdr as *mut u8).add(header_size()) };
        assert_eq!(
            ptr, expected_payload,
            "payload pointer must be header + header_size"
        );
        reset_gc();
    }

    // 観点48: 2個確保したとき、それぞれ payload_to_header が別のヘッダを返す
    #[test]
    fn test_gc_payload_to_header_two_objects_distinct() {
        let _guard = gc_test_guard();
        reset_gc();
        let p1 = willow_alloc_object(1, 8);
        let p2 = willow_alloc_object(2, 8);
        let h1 = payload_to_header(p1);
        let h2 = payload_to_header(p2);
        assert_ne!(h1, h2, "two allocations must have distinct headers");
        assert_eq!(unsafe { (*h1).type_id }, 1);
        assert_eq!(unsafe { (*h2).type_id }, 2);
        reset_gc();
    }

    // =========================================================================
    // 観点49: WILLOW_GC_LOG 環境変数があってもパニックしない
    // =========================================================================
    #[test]
    fn test_gc_log_env_var_does_not_panic() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_alloc_object(1, 8);
        // WILLOW_GC_LOG が設定されていても collect はクラッシュしない
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // =========================================================================
    // 観点50: 複数スレッドからの同時 alloc でパニックしない (Mutex保護の確認)
    // =========================================================================
    #[test]
    fn test_gc_concurrent_alloc_no_panic() {
        let _guard = gc_test_guard();
        reset_gc();

        let handles: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..100 {
                        let _ptr = willow_alloc_object(1, 16);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().expect("concurrent alloc thread must not panic");
        }

        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // =========================================================================
    // TypeInfo / オブジェクトグラフ トレーステスト
    // =========================================================================
    //
    // テスト用 type_id 定数
    const TYPE_NODE: u32 = 200; // payload = [child: *mut u8]              (8 bytes)
    const TYPE_NODE2: u32 = 201; // payload = [child0: *mut u8, child1: *mut u8] (16 bytes)
    const TYPE_LEAF: u32 = 202; // 内部ポインタなし — TraceFn 未登録
    const TYPE_CLASS: u32 = 203; // payload = [i64_field: 8, gc_ptr: 8]    (16 bytes)
    const TYPE_ARRAY: u32 = 204; // payload = [len: i64, ptr0, ptr1, ...]
    const TYPE_MSG: u32 = 210; // enum: [tag: i64, data: i64|*mut u8]    (16 bytes)

    // テスト用 trace 関数 (naked unsafe fn → TraceFn として使用)

    unsafe fn trace_node(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
        // payload[0..8] = child pointer
        slots.push(payload.cast::<*mut u8>());
    }

    unsafe fn trace_node2(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
        // payload[0..8] = child0, payload[8..16] = child1
        slots.push(payload.cast::<*mut u8>());
        slots.push(unsafe { payload.cast::<*mut u8>().add(1) });
    }

    unsafe fn trace_class(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
        // payload[0..8] = i64 field (not a pointer), payload[8..16] = gc_ptr
        slots.push(unsafe { payload.add(8).cast::<*mut u8>() });
    }

    unsafe fn trace_array(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
        // payload[0..8] = len: i64, payload[8 + i*8] = ptr_i
        let len = unsafe { *(payload as *mut i64) } as usize;
        for i in 0..len {
            slots.push(unsafe { payload.add(8 + i * 8).cast::<*mut u8>() });
        }
    }

    unsafe fn trace_msg(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
        // payload[0..8] = tag: i64
        // tag == 0 (Text)  → payload[8..16] is a GC pointer
        // tag == 1 (Number) → payload[8..16] is an i64, must NOT be traced
        let tag = unsafe { *(payload as *mut i64) };
        if tag == 0 {
            slots.push(unsafe { payload.add(8).cast::<*mut u8>() });
        }
    }

    // -------------------------------------------------------------------------
    // 観点T1: root → child が生き残る
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_root_child_survives() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_NODE, trace_node);

        let child = willow_alloc_object(TYPE_LEAF as i64, 8);
        let parent = willow_alloc_object(TYPE_NODE as i64, 8);
        unsafe {
            *(parent as *mut *mut u8) = child;
        }

        let mut root_slot: *mut u8 = parent;
        willow_push_root(&mut root_slot as *mut *mut u8);

        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(8) * 2,
            "parent and child must both survive"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 観点T2: root → child → grandchild が生き残る
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_root_child_grandchild_survives() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_NODE, trace_node);

        let grandchild = willow_alloc_object(TYPE_LEAF as i64, 8);
        let child = willow_alloc_object(TYPE_NODE as i64, 8);
        unsafe {
            *(child as *mut *mut u8) = grandchild;
        }
        let parent = willow_alloc_object(TYPE_NODE as i64, 8);
        unsafe {
            *(parent as *mut *mut u8) = child;
        }

        let mut root_slot: *mut u8 = parent;
        willow_push_root(&mut root_slot as *mut *mut u8);

        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(8) * 3,
            "parent, child, and grandchild must all survive"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 観点T3: root なしの cycle は回収される
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_rootless_cycle_collected() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_NODE, trace_node);

        let a = willow_alloc_object(TYPE_NODE as i64, 8);
        let b = willow_alloc_object(TYPE_NODE as i64, 8);
        unsafe {
            *(a as *mut *mut u8) = b; // A → B
            *(b as *mut *mut u8) = a; // B → A
        }

        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            0,
            "rootless cycle must be collected"
        );
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 観点T4: root ありの cycle は生き残る
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_rooted_cycle_survives() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_NODE, trace_node);

        let a = willow_alloc_object(TYPE_NODE as i64, 8);
        let b = willow_alloc_object(TYPE_NODE as i64, 8);
        unsafe {
            *(a as *mut *mut u8) = b;
            *(b as *mut *mut u8) = a;
        }

        let mut root_slot: *mut u8 = a;
        willow_push_root(&mut root_slot as *mut *mut u8);

        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(8) * 2,
            "rooted cycle must survive"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 観点T5: class の GC フィールドが trace される
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_class_field_traced() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_CLASS, trace_class);

        let field_obj = willow_alloc_object(TYPE_LEAF as i64, 8);
        // payload: [i64_field: 8, gc_ptr: 8] = 16 bytes
        let instance = willow_alloc_object(TYPE_CLASS as i64, 16);
        unsafe {
            *(instance as *mut i64) = 42i64;
            *((instance.add(8)) as *mut *mut u8) = field_obj;
        }

        let mut root_slot: *mut u8 = instance;
        willow_push_root(&mut root_slot as *mut *mut u8);

        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(16) + obj_size(8),
            "class instance and its GC field must both survive"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 観点T6: 2子 (TYPE_NODE2) が両方 trace される
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_two_children_traced() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_NODE2, trace_node2);

        let child0 = willow_alloc_object(TYPE_LEAF as i64, 8);
        let child1 = willow_alloc_object(TYPE_LEAF as i64, 8);
        // payload: [child0_ptr: 8, child1_ptr: 8] = 16 bytes
        let parent = willow_alloc_object(TYPE_NODE2 as i64, 16);
        unsafe {
            *(parent as *mut *mut u8) = child0;
            *((parent as *mut *mut u8).add(1)) = child1;
        }

        let mut root_slot: *mut u8 = parent;
        willow_push_root(&mut root_slot as *mut *mut u8);

        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(16) + obj_size(8) * 2,
            "parent and both children must survive"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 観点T7: array の全要素が trace される
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_array_elements_traced() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_ARRAY, trace_array);

        const N: usize = 4;
        // Stable slots keep earlier elements alive during later allocations.
        let mut elems = [std::ptr::null_mut(); N];
        for slot in &mut elems {
            *slot = willow_alloc_object(TYPE_LEAF as i64, 8);
            willow_push_root(slot);
        }

        // payload: [len: i64, ptr0, ptr1, ptr2, ptr3] = 8 + 4*8 = 40 bytes
        let array_payload: usize = 8 + N * 8;
        let array = willow_alloc_object(TYPE_ARRAY as i64, array_payload as i64);
        unsafe {
            *(array as *mut i64) = N as i64;
            for (i, &ep) in elems.iter().enumerate() {
                *((array.add(8 + i * 8)) as *mut *mut u8) = ep;
            }
        }

        // Construction is complete. Remove temporary roots before collecting,
        // so survival proves that the array trace visits every element.
        willow_pop_roots(N as i32);
        let mut root_slot: *mut u8 = array;
        willow_push_root(&mut root_slot as *mut *mut u8);

        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(array_payload) + obj_size(8) * (N as i64),
            "array and all elements must survive"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 観点T8: enum Text バリアント — 内部 GC ポインタが trace される
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_enum_text_variant_traced() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_MSG, trace_msg);

        let text_obj = willow_alloc_object(TYPE_LEAF as i64, 8);
        // payload: [tag=0: i64, ptr: *mut u8] = 16 bytes
        let msg = willow_alloc_object(TYPE_MSG as i64, 16);
        unsafe {
            *(msg as *mut i64) = 0i64; // tag = Text
            *((msg.add(8)) as *mut *mut u8) = text_obj;
        }

        let mut root_slot: *mut u8 = msg;
        willow_push_root(&mut root_slot as *mut *mut u8);

        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(16) + obj_size(8),
            "Message::Text and its string payload must both survive"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 観点T9: enum Number バリアント — i64 フィールドをポインタとして trace しない
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_enum_number_variant_not_traced() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_MSG, trace_msg);

        // payload: [tag=1: i64, data=12345: i64] = 16 bytes
        let msg = willow_alloc_object(TYPE_MSG as i64, 16);
        unsafe {
            *(msg as *mut i64) = 1i64; // tag = Number
            *((msg.add(8)) as *mut i64) = 12345i64; // numeric data, NOT a pointer
        }

        let mut root_slot: *mut u8 = msg;
        willow_push_root(&mut root_slot as *mut *mut u8);

        // GC must not crash treating 12345 as a pointer
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(16),
            "only msg survives; no child was traced"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 観点T10: root なしの parent+child は両方回収される
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_unrooted_parent_child_both_collected() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_NODE, trace_node);

        let child = willow_alloc_object(TYPE_LEAF as i64, 8);
        let parent = willow_alloc_object(TYPE_NODE as i64, 8);
        unsafe {
            *(parent as *mut *mut u8) = child;
        }

        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            0,
            "unrooted parent and child must both be collected"
        );
        reset_gc();
    }
}

#[cfg(test)]
#[path = "gc_concurrent_tests.rs"]
mod concurrent_tests;
