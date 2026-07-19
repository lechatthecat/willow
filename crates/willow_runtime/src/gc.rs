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
// Old objects live in non-moving regular/large regions. GcHeader::next keeps a
// live-object index for marking/validation while region metadata owns storage,
// mark bits, free spans, and liveness accounting. Generated young objects live
// in nursery TLAB regions; directly rooted survivors retain that storage as
// pinned old regions.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, LazyLock, Mutex};
use std::thread::ThreadId;

const GC_GENERATION_YOUNG: u8 = 0;
const GC_GENERATION_OLD: u8 = 1;
const GC_NURSERY_THRESHOLD_BYTES: usize = 256 * 1024;
const GC_CARD_SIZE: usize = 512;
const GC_OLD_REGION_SIZE: usize = 256 * 1024;
const GC_LARGE_OBJECT_THRESHOLD: usize = GC_OLD_REGION_SIZE / 2;
const GC_REGION_MARK_GRANULE: usize = std::mem::align_of::<GcHeader>();

// ---------------------------------------------------------------------------
// Object header
// ---------------------------------------------------------------------------

/// Cross-ABI object-shape categories used to derive opaque layout ids.
///
/// Values 1–4 are shared with the compiler's generated layouts. Runtime-owned
/// containers use the remaining values. These are metadata, not user-visible
/// type ids and do not affect dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub enum GcObjectKind {
    Class = 1,
    Enum = 2,
    InterfaceBox = 3,
    Range = 4,
    AsyncFrame = 5,
    ArrayHandle = 6,
    ArrayBuffer = 7,
    Map = 8,
    String = 9,
    Channel = 10,
    AtomicCell = 11,
}

/// Destination category supplied to the structural write-barrier hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum GcStoreDestination {
    ObjectField = 1,
    ArrayElement = 2,
    MapValue = 3,
    EnumPayload = 4,
    InterfaceObject = 5,
    AsyncFrameSlot = 6,
    IndirectReference = 7,
    GlobalStatic = 8,
    ContainerInternal = 9,
}

/// Derive the current opaque layout fingerprint. The compiler uses the same
/// Stage-2 algorithm for generated allocations.
pub fn gc_layout_id(
    kind: GcObjectKind,
    payload_size: i64,
    runtime_type_id: i64,
    gc_ref_mask: u64,
) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for word in [
        kind as u64,
        payload_size as u64,
        runtime_type_id as u64,
        gc_ref_mask,
    ] {
        hash ^= word;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    if hash == 0 { 1 } else { hash }
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
    /// Next object in the heap linked list.
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
}

pub const GC_TLAB_STATE_SIZE: usize = std::mem::size_of::<GcTlabState>();
pub const GC_HEADER_SIZE: usize = std::mem::size_of::<GcHeader>();
pub const GC_TLAB_CHUNK_SIZE: usize = 32 * 1024;
pub const GC_TLAB_MAX_OBJECT_SIZE: usize = 4 * 1024;

/// Raw allocation and pointer arithmetic boundary for the collector. The rest
/// of the GC works with `Object`/`Payload`/`RootSlot` and cannot directly
/// dereference a header or stack-slot pointer.
mod raw_heap {
    use std::ptr::NonNull;

    use super::GcHeader;

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

        pub(super) fn next(self) -> Option<Self> {
            // SAFETY: `Object` is created only for a live heap allocation.
            Self::from_raw(unsafe { self.0.as_ref().next })
        }

        pub(super) fn set_next(self, next: Option<Self>) {
            // SAFETY: collection/allocation holds the heap lock while linking.
            unsafe {
                (*self.as_ptr()).next = next.map(Self::as_ptr).unwrap_or(std::ptr::null_mut());
            }
        }

        pub(super) fn begin_trace(self) -> Option<TraceMetadata> {
            // SAFETY: the heap keeps this object allocated throughout marking.
            let header = unsafe { &mut *self.as_ptr() };
            if !header.allocated || header.marked {
                return None;
            }
            header.marked = true;
            Some(self.trace_metadata())
        }

        pub(super) fn trace_metadata(self) -> TraceMetadata {
            // SAFETY: `Object` refers to a live heap allocation.
            let header = unsafe { self.0.as_ref() };
            TraceMetadata {
                type_id: header.type_id,
                layout_id: header.layout_id,
                gc_ref_mask: header.gc_ref_mask,
                payload_size: header.size - std::mem::size_of::<GcHeader>(),
            }
        }

        pub(super) fn payload_word(self, index: usize) -> Option<Payload> {
            // SAFETY: the caller bounds `index` by the payload size.
            let child = unsafe { *self.payload().as_ptr().cast::<*mut u8>().add(index) };
            Payload::from_raw(child)
        }

        pub(super) fn payload_slot(self, index: usize) -> *mut *mut u8 {
            // SAFETY: callers bound `index` by the payload word count.
            unsafe { self.payload().as_ptr().cast::<*mut u8>().add(index) }
        }

        pub(super) fn marked(self) -> bool {
            // SAFETY: `Object` refers to a live heap allocation.
            unsafe { self.0.as_ref().marked }
        }

        pub(super) fn allocated(self) -> bool {
            // SAFETY: `Object` refers to storage containing a valid header.
            unsafe { self.0.as_ref().allocated }
        }

        pub(super) fn reclaim_in_place(self) {
            // SAFETY: sweep has exclusive access while all mutators are stopped.
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
            unsafe { self.0.as_ref().size }
        }

        pub(super) fn type_id(self) -> u32 {
            // SAFETY: `Object` refers to a live heap allocation.
            unsafe { self.0.as_ref().type_id }
        }

        pub(super) fn generation(self) -> u8 {
            // SAFETY: `Object` refers to a live heap allocation.
            unsafe { self.0.as_ref().generation }
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
    /// Head of the live region-backed old-object index.
    heap_head: *mut GcHeader,
    /// Bump-allocation chunks. Active chunks are owned by one TLS state;
    /// collection retires them before walking their object headers.
    tlab_chunks: Vec<TlabChunk>,
    /// Non-moving old-generation storage. Regular regions serve old/runtime
    /// allocations from a bump tail or region-local free spans. Large objects
    /// receive one dedicated region. Object addresses never change.
    old_regions: Vec<OldRegion>,
    /// Generated TLS states observed on allocation slow paths.
    tlab_states: HashMap<usize, TlabStateRecord>,
    /// Total bytes currently allocated (header + payload).
    allocated_bytes: usize,
    /// Trigger a collection when allocated_bytes exceeds this threshold.
    threshold_bytes: usize,
    /// Bytes occupied by allocated young objects in retired or active TLABs.
    young_allocated_bytes: usize,
    /// Trigger a minor collection at the next TLAB refill after this threshold.
    nursery_threshold_bytes: usize,
    /// Total objects allocated lifetime.
    total_allocs: u64,
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
    write_barrier_calls: u64,
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
    bits: Vec<u64>,
}

impl RegionMarkBitmap {
    fn new(capacity: usize) -> Self {
        let granules = capacity.div_ceil(GC_REGION_MARK_GRANULE);
        Self {
            bits: vec![0; granules.div_ceil(64)],
        }
    }

    fn clear(&mut self) {
        self.bits.fill(0);
    }

    fn mark(&mut self, offset: usize) {
        let granule = offset / GC_REGION_MARK_GRANULE;
        let word = granule / 64;
        let bit = granule % 64;
        if let Some(bits) = self.bits.get_mut(word) {
            *bits |= 1u64 << bit;
        }
    }

    fn is_marked(&self, offset: usize) -> bool {
        let granule = offset / GC_REGION_MARK_GRANULE;
        self.bits
            .get(granule / 64)
            .is_some_and(|bits| bits & (1u64 << (granule % 64)) != 0)
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
    free_spans: Vec<RegionFreeSpan>,
    mark_bitmap: RegionMarkBitmap,
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
}

struct TlabStateRecord {
    address: usize,
    owner: ThreadId,
    current_chunk: Option<usize>,
    observed_fast_allocations: u64,
    observed_fast_allocated_bytes: u64,
}

impl RegionMarkBitmap {
    fn unmark(&mut self, offset: usize) {
        let granule = offset / GC_REGION_MARK_GRANULE;
        let word = granule / 64;
        let bit = granule % 64;
        if let Some(bits) = self.bits.get_mut(word) {
            *bits &= !(1u64 << bit);
        }
    }
}

impl OldRegion {
    fn new(kind: RegionKind, capacity: usize) -> Option<Self> {
        debug_assert!(matches!(kind, RegionKind::Old | RegionKind::LargeObject));
        let layout = Layout::from_size_align(capacity, std::mem::align_of::<GcHeader>()).ok()?;
        // SAFETY: `layout` is nonzero, aligned, and owned by the returned region.
        let base = unsafe { alloc_zeroed(layout) };
        if base.is_null() {
            return None;
        }
        Some(Self {
            base,
            capacity,
            used: 0,
            kind,
            live_bytes: 0,
            allocations: BTreeMap::new(),
            free_spans: Vec::new(),
            mark_bitmap: RegionMarkBitmap::new(capacity),
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
        let total_size = GC_HEADER_SIZE.checked_add(payload_size)?;
        let span_size = total_size.checked_next_multiple_of(GC_REGION_MARK_GRANULE)?;
        let mut reused = false;
        let offset = if let Some(index) = self
            .free_spans
            .iter()
            .position(|span| span.size >= span_size)
        {
            reused = true;
            let span = self.free_spans.remove(index);
            if span.size > span_size {
                self.free_spans.insert(
                    index,
                    RegionFreeSpan {
                        offset: span.offset + span_size,
                        size: span.size - span_size,
                    },
                );
            }
            span.offset
        } else {
            let end = self.used.checked_add(span_size)?;
            if end > self.capacity {
                return None;
            }
            let offset = self.used;
            self.used = end;
            offset
        };

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
        self.mark_bitmap.mark(offset);
        Some((object, reused))
    }

    fn object_for_address(&self, address: usize, interior: bool) -> Option<HeapObject> {
        if !self.contains(address) {
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

    fn record_marked_object(&mut self, object: HeapObject) {
        let offset = object.as_ptr() as usize - self.start();
        self.mark_bitmap.mark(offset);
    }

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

    fn coalesce_free_spans(&mut self) {
        self.free_spans.sort_unstable_by_key(|span| span.offset);
        let mut merged: Vec<RegionFreeSpan> = Vec::with_capacity(self.free_spans.len());
        for span in self.free_spans.drain(..) {
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
        self.free_spans = merged;
    }

    fn fragmentation_bytes(&self) -> usize {
        self.used.saturating_sub(self.live_bytes)
    }
}

impl Drop for OldRegion {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.capacity, std::mem::align_of::<GcHeader>())
            .expect("old-region allocation layout remains valid");
        // SAFETY: each region owns one block and `Drop` runs exactly once.
        unsafe { dealloc(self.base, layout) };
    }
}

impl Default for GcState {
    fn default() -> Self {
        Self {
            heap_head: std::ptr::null_mut(),
            tlab_chunks: Vec::new(),
            old_regions: Vec::new(),
            tlab_states: HashMap::new(),
            allocated_bytes: 0,
            threshold_bytes: 1024 * 1024,
            young_allocated_bytes: 0,
            nursery_threshold_bytes: GC_NURSERY_THRESHOLD_BYTES,
            total_allocs: 0,
            total_frees: 0,
            tlab_fast_allocations: 0,
            tlab_slow_allocations: 0,
            tlab_refills: 0,
            tlab_large_allocations: 0,
            tlab_fast_allocated_bytes: 0,
            tlab_reserved_bytes: 0,
            remembered_set: HashSet::new(),
            dirty_cards: HashSet::new(),
            write_barrier_calls: 0,
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

// SAFETY: the raw list head is owned by `GcRuntime::heap`. Allocation/sweep/reset
// hold that mutex; marking is serialized by `collect_lock` and either runs on
// the sole mutator or while all registered mutators are parked.
unsafe impl Send for GcState {}

#[cfg(test)]
static RUNTIME_TEST_LOCK: Mutex<()> = Mutex::new(());

// Root stack — per-thread explicit shadow stack.
std::thread_local! {
    static ROOT_STACK: std::cell::RefCell<Vec<*mut *mut u8>> =
        const { std::cell::RefCell::new(Vec::new()) };
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
// Concurrent marking (tracing while mutators run, with write barriers) is NOT
// part of this slice; this is the stop-the-world coordination layer it builds on.
#[derive(Default)]
struct GcCoord {
    /// Registered mutator threads → their most recently published root snapshot
    /// (object payload addresses). Empty vec until the thread parks at a safepoint.
    mutators: HashMap<ThreadId, Vec<usize>>,
    /// A collector has requested all mutators to reach a safepoint and park.
    stop_requested: bool,
    /// Mutators currently parked at a safepoint.
    parked: HashSet<ThreadId>,
}

/// Process-wide GC services. Keeping the heap, roots, registries, and STW
/// coordinator behind one explicit owner makes lock ordering visible and keeps
/// runtime entry points from reaching into unrelated globals.
struct GcRuntime {
    heap: Mutex<GcState>,
    /// Always acquired before `heap`; marking temporarily releases the heap
    /// lock while registered trace callbacks run.
    collect_lock: Mutex<()>,
    root_stack_owner: Mutex<Option<ThreadId>>,
    skipped_foreign_owner_collections: std::sync::atomic::AtomicU64,
    runtime_roots: Mutex<HashMap<usize, usize>>,
    coord: (Mutex<GcCoord>, Condvar),
    /// Lock-free fast-path mirror of `GcCoord::stop_requested`.
    stop_requested: std::sync::atomic::AtomicBool,
    trace_registry: Mutex<HashMap<u32, TraceFn>>,
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
            collect_lock: Mutex::new(()),
            root_stack_owner: Mutex::new(None),
            skipped_foreign_owner_collections: std::sync::atomic::AtomicU64::new(0),
            runtime_roots: Mutex::new(HashMap::new()),
            coord: (Mutex::new(GcCoord::default()), Condvar::new()),
            stop_requested: std::sync::atomic::AtomicBool::new(false),
            trace_registry: Mutex::new(HashMap::new()),
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
    lock.lock()
        .unwrap()
        .mutators
        .entry(std::thread::current().id())
        .or_default();
    // Registration can race with a collection that has already requested a
    // stop. Join that stop before executing any mutator work so the collector
    // never waits on a newly registered thread that has not published roots.
    willow_gc_safepoint();
}

/// Unregister the current thread as a GC mutator. Must be called before the
/// thread stops allocating/holding GC references (e.g. at worker shutdown).
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_unregister_mutator() {
    let id = std::thread::current().id();
    let (lock, cv) = &runtime().coord;
    let mut coord = lock.lock().unwrap();
    coord.mutators.remove(&id);
    coord.parked.remove(&id);
    // Keep the coordination lock while retiring TLS state: collectors acquire
    // coord then heap, so this preserves lock ordering and prevents a collector
    // from missing this thread while it still mutates its chunk metadata.
    retire_tlabs_for_thread(id);
    // A collector may be waiting for this thread to park; it no longer needs to.
    cv.notify_all();
}

/// A cooperative GC safepoint (willow-6fv.5.6). Cheap when no collection is
/// pending. When a stop-the-world collection is in progress, the calling mutator
/// publishes a snapshot of its roots and parks here until the collector resumes
/// it. The scheduler polls this between task polls; future compiler-inserted
/// safepoints can add loop-backedge coverage.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_safepoint() {
    // Hot-path: a single relaxed atomic load. No collection pending → return
    // immediately without touching the coordination lock.
    if !runtime()
        .stop_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return;
    }
    let (lock, cv) = &runtime().coord;
    let mut coord = lock.lock().unwrap();
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
fn with_stw<R>(collect: impl FnOnce(&GcCoord) -> R) -> R {
    let (lock, cv) = &runtime().coord;
    let me = std::thread::current().id();
    // Publish the stop request on the lock-free gate first so mutators on the
    // hot path observe it at their next safepoint.
    runtime()
        .stop_requested
        .store(true, std::sync::atomic::Ordering::Release);
    let mut coord = lock.lock().unwrap();
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
        coord = cv.wait(coord).unwrap();
    }
    let result = collect(&coord);
    coord.stop_requested = false;
    runtime()
        .stop_requested
        .store(false, std::sync::atomic::Ordering::Release);
    cv.notify_all();
    result
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
fn mark_worklist(mut worklist: Vec<*mut u8>) {
    while let Some(obj_ptr) = worklist.pop() {
        let header = checked_payload_to_header(obj_ptr, "GC root graph");
        let object = HeapObject::from_raw(header).expect("validated payload has a header");
        let Some(metadata) = object.begin_trace() else {
            continue; // already visited — handles cycles
        };
        let payload_words = metadata.payload_size / std::mem::size_of::<usize>();
        for i in 0..payload_words.min(64) {
            if (metadata.gc_ref_mask & (1u64 << i)) != 0
                && let Some(child) = object.payload_word(i)
            {
                worklist.push(child.as_ptr());
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
                // SAFETY: registered trace callbacks expose live GC-reference
                // slots owned by this object or its runtime payload.
                let child = unsafe { *slot };
                if !child.is_null() {
                    worklist.push(child);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TypeInfo registry
// ---------------------------------------------------------------------------

/// Trace function: given a payload pointer, expose the addresses of all mutable
/// GC-reference slots it owns. Full marking loads the slots; minor collection
/// can additionally replace a moved young pointer in place.
pub type TraceFn = unsafe fn(payload: *mut u8, slots: &mut Vec<*mut *mut u8>);

fn type_registry() -> &'static Mutex<HashMap<u32, TraceFn>> {
    &runtime().trace_registry
}

/// Register a trace function for `type_id`.  Call once per class at startup.
pub fn willow_register_type(type_id: u32, trace: TraceFn) {
    type_registry().lock().unwrap().insert(type_id, trace);
}

/// Unregister the trace function for `type_id`.
pub fn willow_unregister_type(type_id: u32) {
    type_registry().lock().unwrap().remove(&type_id);
    runtime()
        .registry_generation
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
}

/// Finalizer: given a payload pointer, release any non-GC resources the object
/// owns (e.g. a boxed Rust collection) just before the object is freed by the
/// sweep phase.  Must not allocate GC memory or touch GC state.
pub type DropFn = unsafe fn(payload: *mut u8);

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

fn sync_tlab_accounting(state: &mut GcState) {
    for record in state.tlab_states.values_mut() {
        // SAFETY: records are removed before their owning thread unregisters
        // and its generated TLS storage can disappear.
        let tls = unsafe { tlab_state_at(record.address) };
        let fast_allocations = tls.fast_allocations.load(Ordering::Acquire);
        let fast_bytes = tls.fast_allocated_bytes.load(Ordering::Acquire);
        let allocation_delta = fast_allocations.saturating_sub(record.observed_fast_allocations);
        let byte_delta = fast_bytes.saturating_sub(record.observed_fast_allocated_bytes);
        record.observed_fast_allocations = fast_allocations;
        record.observed_fast_allocated_bytes = fast_bytes;
        state.total_allocs = state.total_allocs.saturating_add(allocation_delta);
        state.tlab_fast_allocations = state.tlab_fast_allocations.saturating_add(allocation_delta);
        state.tlab_fast_allocated_bytes =
            state.tlab_fast_allocated_bytes.saturating_add(byte_delta);
        state.allocated_bytes = state.allocated_bytes.saturating_add(byte_delta as usize);
        state.young_allocated_bytes = state
            .young_allocated_bytes
            .saturating_add(byte_delta as usize);
    }
}

fn register_tlab_state(state: &mut GcState, address: usize) {
    state
        .tlab_states
        .entry(address)
        .or_insert_with(|| TlabStateRecord {
            address,
            owner: std::thread::current().id(),
            current_chunk: None,
            observed_fast_allocations: 0,
            observed_fast_allocated_bytes: 0,
        });
}

fn retire_tlab_locked(state: &mut GcState, address: usize) {
    let Some(record) = state.tlab_states.get_mut(&address) else {
        return;
    };
    // SAFETY: the record owns this generated TLS state until unregister/reset.
    let tls = unsafe { tlab_state_at(record.address) };
    let cursor = tls.cursor.swap(0, Ordering::AcqRel);
    tls.limit.store(0, Ordering::Release);
    let current_chunk = record.current_chunk.take();
    if let Some(base) = current_chunk
        && let Some(chunk) = state
            .tlab_chunks
            .iter_mut()
            .find(|chunk| chunk.base as usize == base)
    {
        let start = chunk.base as usize;
        let end = start.saturating_add(chunk.capacity);
        chunk.used = cursor.clamp(start, end).saturating_sub(start);
        chunk.owner_state = None;
    }
}

fn retire_all_tlabs_locked(state: &mut GcState) {
    sync_tlab_accounting(state);
    let addresses: Vec<usize> = state.tlab_states.keys().copied().collect();
    for address in addresses {
        retire_tlab_locked(state, address);
    }
}

fn retire_tlabs_for_thread(owner: ThreadId) {
    let mut state = runtime().heap.lock().unwrap();
    sync_tlab_accounting(&mut state);
    let addresses: Vec<usize> = state
        .tlab_states
        .iter()
        .filter_map(|(&address, record)| (record.owner == owner).then_some(address))
        .collect();
    for address in &addresses {
        retire_tlab_locked(&mut state, *address);
    }
    for address in addresses {
        state.tlab_states.remove(&address);
    }
}

fn allocate_tlab_chunk(state: &mut GcState, owner_state: usize) -> Option<*mut u8> {
    let layout =
        Layout::from_size_align(GC_TLAB_CHUNK_SIZE, std::mem::align_of::<GcHeader>()).ok()?;
    // SAFETY: the layout is nonzero and valid. Fresh zeroing makes every
    // unallocated payload byte safe before generated code publishes a header.
    let base = unsafe { alloc_zeroed(layout) };
    if base.is_null() {
        return None;
    }
    state.tlab_chunks.push(TlabChunk {
        base,
        capacity: GC_TLAB_CHUNK_SIZE,
        used: 0,
        owner_state: Some(owner_state),
        kind: RegionKind::Nursery,
        live_bytes: 0,
        mark_bitmap: RegionMarkBitmap::new(GC_TLAB_CHUNK_SIZE),
    });
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
    sync_tlab_accounting(&mut state);
    stress || state.allocated_bytes >= state.threshold_bytes
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
    ROOT_STACK.with(|rs| rs.borrow_mut().push(slot));
}

/// Unregister the most recently pushed root slot.
#[unsafe(no_mangle)]
pub extern "C" fn willow_pop_root() {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    ROOT_STACK.with(|rs| {
        rs.borrow_mut().pop();
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
    });
    release_root_stack_owner_if_empty();
}

/// Keep a GC-managed object alive through a runtime-owned structure such as a
/// scheduler task, future frame, join handle, or wait queue.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_add_runtime_root(object: *mut u8) {
    if object.is_null() {
        return;
    }

    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let mut roots = runtime().runtime_roots.lock().unwrap();
    let root = object as usize;
    *roots.entry(root).or_insert(0) += 1;
}

/// Remove a persistent runtime root when the owning runtime structure no
/// longer needs to retain the object.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_remove_runtime_root(object: *mut u8) {
    if object.is_null() {
        return;
    }

    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let root = object as usize;
    let mut roots = runtime().runtime_roots.lock().unwrap();
    if let Some(count) = roots.get_mut(&root) {
        if *count > 1 {
            *count -= 1;
        } else {
            roots.remove(&root);
        }
    }
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

    {
        let mut state = runtime().heap.lock().unwrap();
        register_tlab_state(&mut state, state_address);
        sync_tlab_accounting(&mut state);
        if small {
            // A small-object miss means the active chunk has insufficient tail
            // space. Seal it before collection/refill.
            retire_tlab_locked(&mut state, state_address);
        }
    }

    if !stress && allocation_should_minor_collect() {
        minor_collect_internal();
    }

    if stress || allocation_should_collect() {
        collect_internal();
    }

    if stress || !small {
        return allocate_old(layout_id, type_id as u32, payload_size, gc_ref_mask);
    }

    let mut state = runtime().heap.lock().unwrap();
    register_tlab_state(&mut state, state_address);
    let Some(base) = allocate_tlab_chunk(&mut state, state_address) else {
        return std::ptr::null_mut();
    };
    let Some(header) =
        initialize_object_at(base, total_size, type_id as u32, layout_id, gc_ref_mask)
    else {
        return std::ptr::null_mut();
    };
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

fn object_in_retired_chunk(
    chunk: &TlabChunk,
    address: usize,
    interior: bool,
) -> Option<HeapObject> {
    let mut offset = 0usize;
    while offset < chunk.used {
        // SAFETY: retired chunks contain a stable sequential header prefix.
        let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast())?;
        let size = object.size();
        if size < GC_HEADER_SIZE || size > chunk.used - offset {
            return None;
        }
        if object.allocated() {
            let payload = object.payload().as_ptr() as usize;
            let payload_end = object.as_ptr() as usize + size;
            if (!interior && payload == address)
                || (interior && address >= payload && address < payload_end)
            {
                return Some(object);
            }
        }
        offset += size;
    }
    None
}

fn find_old_region_object(state: &GcState, address: usize, interior: bool) -> Option<HeapObject> {
    state
        .old_regions
        .iter()
        .find_map(|region| region.object_for_address(address, interior))
}

fn payload_generation(state: &GcState, payload: *mut u8) -> Option<u8> {
    if payload.is_null() {
        return None;
    }
    let address = payload as usize;
    if let Some(object) = find_old_region_object(state, address, false) {
        return Some(object.generation());
    }
    for chunk in &state.tlab_chunks {
        let start = chunk.base as usize;
        let end = start.saturating_add(chunk_used_bytes(state, chunk));
        if address < start + GC_HEADER_SIZE || address >= end {
            continue;
        }
        // Every object in an active generated TLAB is young. Avoid parsing the
        // concurrently advancing header prefix on this barrier hot path.
        if chunk.owner_state.is_some() {
            return Some(GC_GENERATION_YOUNG);
        }
        if let Some(object) = object_in_retired_chunk(chunk, address, false) {
            return Some(object.generation());
        }
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
    for chunk in &state.tlab_chunks {
        let start = chunk.base as usize;
        let end = start.saturating_add(chunk_used_bytes(state, chunk));
        if address < start + GC_HEADER_SIZE || address >= end {
            continue;
        }
        // Active TLAB owners are young and therefore never need remembering.
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

/// Generational write barrier. The initial implementation deliberately uses an
/// unconditional runtime call from compiler-generated reference stores. It
/// records only old-to-young edges; scalar stores and young owners remain cheap
/// compiler-side skips.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_write_barrier(owner: *mut u8, value: *mut u8, destination_kind: i64) {
    if value.is_null() {
        return;
    }
    let mut state = runtime().heap.lock().unwrap();
    state.write_barrier_calls = state.write_barrier_calls.saturating_add(1);
    if payload_generation(&state, value) != Some(GC_GENERATION_YOUNG) {
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

fn forget_remembered_owner(state: &mut GcState, owner_payload: usize) {
    if !state.remembered_set.remove(&owner_payload) {
        return;
    }
    let card = owner_payload / GC_CARD_SIZE;
    if !state
        .remembered_set
        .iter()
        .any(|owner| owner / GC_CARD_SIZE == card)
    {
        state.dirty_cards.remove(&card);
    }
}

fn allocate_object(layout_id: u64, type_id: u32, payload_size: i64, gc_ref_mask: u64) -> *mut u8 {
    if payload_size < 0 {
        return std::ptr::null_mut();
    }
    if allocation_should_collect() {
        collect_internal();
    }
    allocate_old(layout_id, type_id, payload_size, gc_ref_mask)
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

    let (object, reused) = if large {
        let mut region = OldRegion::new(RegionKind::LargeObject, span_size)?;
        let allocated = region.allocate_object(type_id, layout_id, gc_ref_mask, payload_size)?;
        state.old_regions.push(region);
        allocated
    } else if let Some(allocated) = state
        .old_regions
        .iter_mut()
        .filter(|region| region.kind == RegionKind::Old)
        .find_map(|region| region.allocate_object(type_id, layout_id, gc_ref_mask, payload_size))
    {
        allocated
    } else {
        let mut region = OldRegion::new(RegionKind::Old, GC_OLD_REGION_SIZE)?;
        let allocated = region.allocate_object(type_id, layout_id, gc_ref_mask, payload_size)?;
        state.old_regions.push(region);
        allocated
    };

    object.set_next(HeapObject::from_raw(state.heap_head));
    state.heap_head = object.as_ptr();
    state.allocated_bytes = state.allocated_bytes.saturating_add(object.size());
    state.old_region_allocations = state.old_region_allocations.saturating_add(1);
    if reused {
        state.old_region_reuses = state.old_region_reuses.saturating_add(1);
    }
    if count_logical_allocation {
        state.total_allocs = state.total_allocs.saturating_add(1);
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
    let Some(header) = allocate_old_region_object_locked(
        &mut state,
        layout_id,
        type_id,
        payload_size,
        gc_ref_mask,
        true,
    ) else {
        return std::ptr::null_mut();
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

/// Trigger a full stop-the-world mark-and-sweep collection.
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
pub(crate) fn gc_stress_enabled(kind: &str) -> bool {
    std::env::var("WILLOW_GC_STRESS")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .any(|mode| mode == "all" || mode == kind)
        })
        .unwrap_or(false)
}

pub(crate) fn stress_collect(kind: &str) {
    if gc_stress_enabled(kind) {
        collect_internal();
    }
}

/// Return the total bytes currently on the GC heap (header + payload).
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

struct MinorCollector<'a> {
    state: &'a mut GcState,
    young_objects: HashMap<usize, HeapObject>,
    forwarding: HashMap<usize, *mut u8>,
    worklist: Vec<HeapObject>,
    scanned: HashSet<usize>,
    trace_registry: HashMap<u32, TraceFn>,
    drop_registry: HashMap<u32, DropFn>,
}

fn object_reference_slots(
    object: HeapObject,
    trace_registry: &HashMap<u32, TraceFn>,
) -> Vec<*mut *mut u8> {
    let metadata = object.trace_metadata();
    let payload_words = metadata.payload_size / std::mem::size_of::<usize>();
    let mut slots = Vec::new();
    for index in 0..payload_words.min(64) {
        if (metadata.gc_ref_mask & (1u64 << index)) != 0 {
            slots.push(object.payload_slot(index));
        }
    }
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
    let mut current = HeapObject::from_raw(state.heap_head);
    while let Some(object) = current {
        if object.allocated() && object.generation() == GC_GENERATION_OLD {
            old_objects.push(object);
        }
        current = object.next();
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
    let mut linked = HashSet::new();
    let mut current = HeapObject::from_raw(state.heap_head);
    while let Some(object) = current {
        let address = object.as_ptr() as usize;
        if !linked.insert(address) {
            return Err(format!(
                "old-object linked list contains a cycle at 0x{address:x}"
            ));
        }
        current = object.next();
    }

    let mut region_objects = HashSet::new();
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
            region_objects.insert(object.as_ptr() as usize);
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

    if linked != region_objects {
        let missing_from_regions = linked.difference(&region_objects).next().copied();
        let missing_from_list = region_objects.difference(&linked).next().copied();
        return Err(format!(
            "old-object list/region map mismatch: list-only={missing_from_regions:?}, region-only={missing_from_list:?}"
        ));
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
    Ok(())
}

impl<'a> MinorCollector<'a> {
    fn new(
        state: &'a mut GcState,
        trace_registry: HashMap<u32, TraceFn>,
        drop_registry: HashMap<u32, DropFn>,
    ) -> Self {
        let mut young_objects = HashMap::new();
        for chunk in &state.tlab_chunks {
            debug_assert!(
                chunk.owner_state.is_none(),
                "minor collection requires retired TLABs"
            );
            let mut offset = 0usize;
            while offset < chunk.used {
                // SAFETY: retired chunks contain a stable sequential header prefix.
                let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast())
                    .expect("TLAB header address is non-null");
                let size = object.size();
                if size < GC_HEADER_SIZE || size > chunk.used - offset {
                    panic!(
                        "willow gc: corrupt nursery header at 0x{:x}: size={size}, remaining={}",
                        object.as_ptr() as usize,
                        chunk.used - offset
                    );
                }
                if object.allocated() && object.generation() == GC_GENERATION_YOUNG {
                    young_objects.insert(object.payload().as_ptr() as usize, object);
                }
                offset += size;
            }
        }
        Self {
            state,
            young_objects,
            forwarding: HashMap::new(),
            worklist: Vec::new(),
            scanned: HashSet::new(),
            trace_registry,
            drop_registry,
        }
    }

    /// Current generated code can retain an SSA alias after registering a root
    /// slot. Until precise relocation-aware reloads exist, directly rooted
    /// young objects are promoted in place. Their children can still move and
    /// are updated through object/container slots below.
    fn pin_root(&mut self, payload: *mut u8) {
        if payload.is_null() {
            return;
        }
        let address = payload as usize;
        if let Some(&object) = self.young_objects.get(&address)
            && object.generation() == GC_GENERATION_YOUNG
        {
            object.set_generation(GC_GENERATION_OLD);
            let size = object.size();
            self.state.young_allocated_bytes =
                self.state.young_allocated_bytes.saturating_sub(size);
            self.state.promoted_objects = self.state.promoted_objects.saturating_add(1);
            self.state.promoted_bytes = self.state.promoted_bytes.saturating_add(size as u64);
            self.worklist.push(object);
            return;
        }
        if let Some(payload) = GcPayload::from_raw(payload) {
            let object = HeapObject::from_payload(payload);
            if object.allocated() && object.generation() == GC_GENERATION_OLD {
                self.worklist.push(object);
                return;
            }
        }
        #[cfg(debug_assertions)]
        panic!("willow gc: invalid root 0x{address:x} during minor collection");
    }

    fn evacuate(&mut self, payload: *mut u8) -> *mut u8 {
        if payload.is_null() {
            return payload;
        }
        let address = payload as usize;
        let Some(&source) = self.young_objects.get(&address) else {
            return payload;
        };
        // Direct roots were already promoted in place.
        if source.generation() != GC_GENERATION_YOUNG {
            return payload;
        }
        if let Some(&forwarded) = self.forwarding.get(&address) {
            return forwarded;
        }

        let metadata = source.trace_metadata();
        let size = source.size();
        let Some(target) = allocate_old_region_object_locked(
            self.state,
            metadata.layout_id,
            metadata.type_id,
            metadata.payload_size,
            metadata.gc_ref_mask,
            false,
        ) else {
            std::process::abort();
        };
        // SAFETY: source and target are distinct allocations with identical
        // payload sizes. Header/list metadata remains owned by the collector.
        unsafe {
            std::ptr::copy_nonoverlapping(
                source.payload().as_ptr(),
                target.payload().as_ptr(),
                metadata.payload_size,
            );
        }
        self.state.promoted_objects = self.state.promoted_objects.saturating_add(1);
        self.state.promoted_bytes = self.state.promoted_bytes.saturating_add(size as u64);
        self.state.moved_objects = self.state.moved_objects.saturating_add(1);
        let forwarded = target.payload().as_ptr();
        self.forwarding.insert(address, forwarded);
        self.worklist.push(target);
        forwarded
    }

    fn scan_slot(&mut self, slot: *mut *mut u8) {
        if slot.is_null() {
            return;
        }
        // SAFETY: slots come from layout masks or registered mutable trace hooks.
        let old = unsafe { *slot };
        let new = self.evacuate(old);
        if new != old {
            // SAFETY: minor collection owns all heap mutation under STW.
            unsafe { *slot = new };
        }
    }

    fn scan_object(&mut self, object: HeapObject) {
        let address = object.payload().as_ptr() as usize;
        if !object.allocated() || !self.scanned.insert(address) {
            return;
        }
        for slot in object_reference_slots(object, &self.trace_registry) {
            self.scan_slot(slot);
        }
    }

    fn run(mut self, roots: Vec<*mut u8>, remembered: HashSet<usize>) -> usize {
        // Pin every direct root before scanning any interior edge so a duplicate
        // stack/runtime root can never observe a moved stale SSA pointer.
        for root in roots {
            self.pin_root(root);
        }
        for owner in remembered {
            self.pin_root(owner as *mut u8);
        }
        while let Some(object) = self.worklist.pop() {
            self.scan_object(object);
        }

        let mut reclaimed_bytes = 0usize;
        for (&payload, &object) in &self.young_objects {
            if !object.allocated() || object.generation() != GC_GENERATION_YOUNG {
                continue;
            }
            let size = object.size();
            if !self.forwarding.contains_key(&payload) {
                if let Some(drop_fn) = self.drop_registry.get(&object.type_id()).copied() {
                    // SAFETY: unreachable young objects still own their runtime payload.
                    unsafe { drop_fn(object.payload().as_ptr()) };
                }
                self.state.total_frees = self.state.total_frees.saturating_add(1);
            }
            object.reclaim_in_place();
            self.state.allocated_bytes = self.state.allocated_bytes.saturating_sub(size);
            self.state.young_allocated_bytes =
                self.state.young_allocated_bytes.saturating_sub(size);
            reclaimed_bytes = reclaimed_bytes.saturating_add(size);
        }

        let mut chunk_index = 0usize;
        while chunk_index < self.state.tlab_chunks.len() {
            let base = self.state.tlab_chunks[chunk_index].base;
            let used = self.state.tlab_chunks[chunk_index].used;
            let mut offset = 0usize;
            let mut has_allocated = false;
            let mut live_bytes = 0usize;
            self.state.tlab_chunks[chunk_index].mark_bitmap.clear();
            while offset < used {
                // SAFETY: minor collection has already validated this retired prefix.
                let object = HeapObject::from_raw(unsafe { base.add(offset) }.cast())
                    .expect("TLAB header address is non-null");
                if object.allocated() {
                    debug_assert_eq!(
                        object.generation(),
                        GC_GENERATION_OLD,
                        "every minor survivor is promoted"
                    );
                    has_allocated = true;
                    live_bytes = live_bytes.saturating_add(object.size());
                    self.state.tlab_chunks[chunk_index].mark_bitmap.mark(offset);
                }
                offset += object.size();
            }
            if !has_allocated {
                let chunk = self.state.tlab_chunks.swap_remove(chunk_index);
                let layout =
                    Layout::from_size_align(chunk.capacity, std::mem::align_of::<GcHeader>())
                        .expect("TLAB chunk layout remains valid");
                // SAFETY: every object in this retired chunk was reclaimed or moved.
                unsafe { dealloc(chunk.base, layout) };
                self.state.tlab_reserved_bytes = self
                    .state
                    .tlab_reserved_bytes
                    .saturating_sub(chunk.capacity);
            } else {
                self.state.tlab_chunks[chunk_index].kind = RegionKind::Pinned;
                self.state.tlab_chunks[chunk_index].live_bytes = live_bytes;
                chunk_index += 1;
            }
        }
        reclaimed_bytes
    }
}

fn minor_collect_with_roots(mut roots: Vec<*mut u8>) -> usize {
    roots.extend(runtime_roots_snapshot());
    roots.extend(crate::lock::lock_gc_roots());
    let trace_registry = type_registry().lock().unwrap().clone();
    let drop_registry = drop_registry().lock().unwrap().clone();
    let mut state = runtime().heap.lock().unwrap();
    if std::env::var("WILLOW_GC_VERIFY_BARRIER").is_ok()
        && let Err(message) = verify_remembered_set(&state, &trace_registry)
    {
        panic!("willow gc: write barrier verification failed: {message}");
    }
    let remembered = std::mem::take(&mut state.remembered_set);
    state.dirty_cards.clear();
    state.minor_collections = state.minor_collections.saturating_add(1);
    let reclaimed =
        MinorCollector::new(&mut state, trace_registry, drop_registry).run(roots, remembered);
    if std::env::var("WILLOW_GC_VERIFY_REGIONS").is_ok()
        && let Err(message) = verify_old_region_metadata(&state)
    {
        panic!("willow gc: region verification failed after minor collection: {message}");
    }
    reclaimed
}

fn minor_collect_internal() {
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
            willow_gc_safepoint();
            return;
        }
    };
    if !multi_mutator_active() && foreign_root_stack_owner_active() {
        runtime()
            .skipped_foreign_owner_collections
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    }

    if multi_mutator_active() {
        with_stw(|coord| {
            {
                let mut state = runtime().heap.lock().unwrap();
                retire_all_tlabs_locked(&mut state);
            }
            let roots = all_registered_stack_roots(coord);
            minor_collect_with_roots(roots)
        });
    } else {
        {
            let mut state = runtime().heap.lock().unwrap();
            retire_all_tlabs_locked(&mut state);
        }
        let roots = snapshot_local_roots()
            .into_iter()
            .map(|address| address as *mut u8)
            .collect();
        minor_collect_with_roots(roots);
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
    // When other mutator threads are registered, a stop-the-world collection
    // (below) scans all of their roots, so the single-mutator skip does not
    // apply. Only the legacy single-mutator runtime falls back to skipping when
    // a foreign thread owns the (unregistered) root stack (willow-6fv.2 / .5.6).
    if !multi_mutator_active() && foreign_root_stack_owner_active() {
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

    let heap_before = willow_gc_allocated_bytes() as usize;

    // ---- Mark phase --------------------------------------------------------
    // Gather the root set, then trace. When other mutator threads are registered
    // (willow-6fv.5.6), stop the world and scan EVERY registered mutator's root
    // stack; otherwise scan only this thread's stack (the unchanged single-
    // mutator path). Either way, runtime roots and channel buffers are included.
    // Mark AND sweep must both run with the world stopped in the multi-mutator
    // case. If sweep ran after the world resumed, another mutator could, in the
    // gap, allocate an object (prepended to the heap, hence unmarked) and even
    // install a runtime root on it before sweep walked the heap — sweep would
    // then free that live, already-rooted object, leaving a dangling runtime
    // root that the next collection traces and aborts on (willow-w5e2).
    let freed = if multi_mutator_active() {
        with_stw(|coord| {
            {
                let mut state = runtime().heap.lock().unwrap();
                retire_all_tlabs_locked(&mut state);
            }
            let mut worklist = all_registered_stack_roots(coord);
            worklist.extend(runtime_roots_snapshot());
            worklist.extend(crate::lock::lock_gc_roots());
            mark_worklist(worklist);
            sweep()
        })
    } else {
        {
            let mut state = runtime().heap.lock().unwrap();
            retire_all_tlabs_locked(&mut state);
        }
        ROOT_STACK.with(|rs| {
            let mut worklist: Vec<*mut u8> = {
                let stack = rs.borrow();
                stack
                    .iter()
                    .filter(|&&slot| !slot.is_null())
                    .filter_map(|&slot| {
                        RootSlot::from_raw(slot)
                            .and_then(RootSlot::load)
                            .map(GcPayload::as_ptr)
                    })
                    .collect()
            };
            worklist.extend(runtime_roots_snapshot());
            // GC-element channel buffers hold live references (willow-dsw).
            worklist.extend(crate::lock::lock_gc_roots());
            mark_worklist(worklist);
        });
        // Single-mutator: no other thread can allocate during the sweep.
        sweep()
    };

    if gc_log {
        let state = runtime().heap.lock().unwrap();
        eprintln!(
            "gc: heap_before={}B freed={}B heap_after={}B total_allocs={} total_frees={}",
            heap_before, freed, state.allocated_bytes, state.total_allocs, state.total_frees,
        );
    }
}

/// Sweep the region-backed old-object index and nursery/pinned regions without
/// moving survivors. Dead old spans return to their owning region's free list;
/// completely empty regions are released. Returns total logical bytes freed.
fn sweep() -> usize {
    let mut freed_bytes = 0usize;
    let mut freed_count = 0u64;

    let mut state = runtime().heap.lock().unwrap();
    state.major_collections = state.major_collections.saturating_add(1);
    for region in &mut state.old_regions {
        region.mark_bitmap.clear();
    }
    let mut previous: Option<HeapObject> = None;
    let mut current = HeapObject::from_raw(state.heap_head);
    while let Some(object) = current {
        let next = object.next();
        let size = object.size();

        if object.marked() {
            // Survivor: retain the stable address and rebuild region-local
            // liveness/mark metadata for future partial-region selection.
            let region = state
                .old_regions
                .iter_mut()
                .find(|region| region.contains(object.as_ptr() as usize))
                .expect("linked old object belongs to an old region");
            region.record_marked_object(object);
            object.clear_mark();
            previous = Some(object);
            current = next;
        } else {
            // Unreachable: unlink and free.
            if let Some(previous) = previous {
                previous.set_next(next);
            } else {
                state.heap_head = next.map(HeapObject::as_ptr).unwrap_or(std::ptr::null_mut());
            }
            // Run a finalizer (if any) before releasing the payload so the
            // object can free non-GC resources it owns (e.g. a boxed Map).
            if let Some(drop_fn) = lookup_drop(object.type_id()) {
                // SAFETY: drop_fn is the registered finalizer for this type_id;
                // it releases the payload's owned resources and does not touch GC state.
                unsafe { drop_fn(object.payload().as_ptr()) };
            }
            let payload = object.payload().as_ptr() as usize;
            forget_remembered_owner(&mut state, payload);
            if object.generation() == GC_GENERATION_YOUNG {
                state.young_allocated_bytes = state.young_allocated_bytes.saturating_sub(size);
            }
            object.reclaim_in_place();
            let region = state
                .old_regions
                .iter_mut()
                .find(|region| region.contains(object.as_ptr() as usize))
                .expect("linked old object belongs to an old region");
            region.release_object(object);
            freed_bytes += size;
            freed_count += 1;
            state.allocated_bytes = state.allocated_bytes.saturating_sub(size);
            state.total_frees += 1;
            current = next;
        }
    }

    let regions_before = state.old_regions.len();
    state
        .old_regions
        .retain(|region| !region.allocations.is_empty());
    state.old_regions_released = state
        .old_regions_released
        .saturating_add((regions_before - state.old_regions.len()) as u64);

    let mut chunk_index = 0;
    while chunk_index < state.tlab_chunks.len() {
        let base = state.tlab_chunks[chunk_index].base;
        let used = state.tlab_chunks[chunk_index].used;
        let owner_state = state.tlab_chunks[chunk_index].owner_state;
        state.tlab_chunks[chunk_index].live_bytes = 0;
        state.tlab_chunks[chunk_index].mark_bitmap.clear();
        let mut offset = 0usize;
        let mut has_live_objects = false;
        let mut has_old_objects = false;
        while offset < used {
            // SAFETY: `offset` is advanced only by validated aligned header
            // sizes within this registered chunk.
            let raw = unsafe { base.add(offset) };
            let object = HeapObject::from_raw(raw.cast::<GcHeader>())
                .expect("TLAB object header address is non-null");
            let size = object.size();
            if size < GC_HEADER_SIZE
                || !size.is_multiple_of(std::mem::align_of::<GcHeader>())
                || size > used - offset
            {
                panic!(
                    "willow gc: corrupt TLAB header at 0x{:x}: size={size}, remaining={}",
                    raw as usize,
                    used - offset
                );
            }
            if !object.allocated() {
                offset += size;
                continue;
            }
            if object.marked() {
                object.clear_mark();
                has_live_objects = true;
                has_old_objects |= object.generation() == GC_GENERATION_OLD;
                state.tlab_chunks[chunk_index].mark_bitmap.mark(offset);
                state.tlab_chunks[chunk_index].live_bytes = state.tlab_chunks[chunk_index]
                    .live_bytes
                    .saturating_add(size);
            } else {
                let payload = object.payload().as_ptr() as usize;
                forget_remembered_owner(&mut state, payload);
                if let Some(drop_fn) = lookup_drop(object.type_id()) {
                    // SAFETY: same finalizer contract as old-region objects.
                    unsafe { drop_fn(object.payload().as_ptr()) };
                }
                object.reclaim_in_place();
                if object.generation() == GC_GENERATION_YOUNG {
                    state.young_allocated_bytes = state.young_allocated_bytes.saturating_sub(size);
                }
                freed_bytes += size;
                freed_count += 1;
                state.allocated_bytes = state.allocated_bytes.saturating_sub(size);
                state.total_frees += 1;
            }
            offset += size;
        }

        if !has_live_objects && owner_state.is_none() {
            let chunk = state.tlab_chunks.swap_remove(chunk_index);
            let layout = Layout::from_size_align(chunk.capacity, std::mem::align_of::<GcHeader>())
                .expect("TLAB chunk layout remains valid");
            // SAFETY: the retired chunk has no live objects and is removed once.
            unsafe { dealloc(chunk.base, layout) };
            state.tlab_reserved_bytes = state.tlab_reserved_bytes.saturating_sub(chunk.capacity);
        } else {
            state.tlab_chunks[chunk_index].kind = if has_old_objects {
                RegionKind::Pinned
            } else {
                RegionKind::Nursery
            };
            chunk_index += 1;
        }
    }

    if std::env::var("WILLOW_GC_VERIFY_REGIONS").is_ok()
        && let Err(message) = verify_old_region_metadata(&state)
    {
        panic!("willow gc: region verification failed after major collection: {message}");
    }
    let _ = freed_count;
    freed_bytes
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
    if payload.is_null() {
        return Err("null is not a traceable GC payload pointer".to_string());
    }
    let payload_addr = payload as usize;
    let header_size = std::mem::size_of::<GcHeader>();
    let header_align = std::mem::align_of::<GcHeader>();
    let state = runtime().heap.lock().unwrap();
    let mut current = HeapObject::from_raw(state.heap_head);
    while let Some(object) = current {
        let header_addr = object.as_ptr() as usize;
        let expected_payload = header_addr.saturating_add(header_size);
        if expected_payload == payload_addr {
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
        current = object.next();
    }
    for chunk in &state.tlab_chunks {
        let start = chunk.base as usize;
        let used = chunk
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
            .unwrap_or(chunk.used);
        let mut offset = 0usize;
        while offset < used {
            // SAFETY: the allocated chunk prefix contains sequential headers.
            let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast())
                .expect("TLAB header address is non-null");
            let size = object.size();
            if size < header_size || size > used - offset {
                return Err(format!(
                    "TLAB header at 0x{:x} has invalid size {size}",
                    object.as_ptr() as usize
                ));
            }
            let expected_payload = object.as_ptr() as usize + header_size;
            if expected_payload == payload_addr {
                if !object.allocated() {
                    return Err(format!(
                        "0x{payload_addr:x} refers to a reclaimed TLAB object"
                    ));
                }
                return Ok(object.as_ptr());
            }
            offset += size;
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
    runtime()
        .root_stack_owner
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|owner| *owner != current)
}

fn runtime_roots_snapshot() -> Vec<*mut u8> {
    runtime()
        .runtime_roots
        .lock()
        .unwrap()
        .keys()
        .map(|&root| root as *mut u8)
        .filter(|root| !root.is_null())
        .collect()
}

fn reset_internal() {
    // Exclude a concurrent collection (see runtime().collect_lock).
    let _serialize = runtime()
        .collect_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut state = runtime().heap.lock().unwrap();
    for record in state.tlab_states.values() {
        // SAFETY: reset is serialized against mutator activity in production
        // and tests hold the runtime test guard.
        let tls = unsafe { tlab_state_at(record.address) };
        tls.cursor.store(0, Ordering::Release);
        tls.limit.store(0, Ordering::Release);
        tls.fast_allocations.store(0, Ordering::Release);
        tls.fast_allocated_bytes.store(0, Ordering::Release);
    }
    state.heap_head = std::ptr::null_mut();
    state.old_regions.clear();
    for chunk in state.tlab_chunks.drain(..) {
        let layout = Layout::from_size_align(chunk.capacity, std::mem::align_of::<GcHeader>())
            .expect("TLAB chunk layout remains valid");
        // SAFETY: reset owns and releases each registered chunk exactly once.
        unsafe { dealloc(chunk.base, layout) };
    }
    state.tlab_states.clear();
    state.allocated_bytes = 0;
    state.threshold_bytes = 1024 * 1024;
    state.young_allocated_bytes = 0;
    state.nursery_threshold_bytes = GC_NURSERY_THRESHOLD_BYTES;
    state.total_allocs = 0;
    state.total_frees = 0;
    state.tlab_fast_allocations = 0;
    state.tlab_slow_allocations = 0;
    state.tlab_refills = 0;
    state.tlab_large_allocations = 0;
    state.tlab_fast_allocated_bytes = 0;
    state.tlab_reserved_bytes = 0;
    state.remembered_set.clear();
    state.dirty_cards.clear();
    state.write_barrier_calls = 0;
    state.write_barrier_hits = 0;
    state.minor_collections = 0;
    state.promoted_objects = 0;
    state.promoted_bytes = 0;
    state.moved_objects = 0;
    state.old_region_allocations = 0;
    state.old_region_reuses = 0;
    state.old_regions_released = 0;
    state.major_collections = 0;
    runtime().runtime_roots.lock().unwrap().clear();
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
    drop_registry().lock().unwrap().clear();
    runtime()
        .registry_generation
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    ROOT_STACK.with(|rs| rs.borrow_mut().clear());
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
mod tests {
    use super::*;

    fn gc_test_guard() -> std::sync::MutexGuard<'static, ()> {
        runtime_test_guard()
    }

    #[test]
    fn runtime_state_starts_owned_and_empty() {
        let runtime = GcRuntime::default();
        let heap = runtime.heap.lock().unwrap();
        assert!(heap.heap_head.is_null());
        assert_eq!(heap.allocated_bytes, 0);
        assert!(runtime.runtime_roots.lock().unwrap().is_empty());
        assert!(runtime.trace_registry.lock().unwrap().is_empty());
        assert!(runtime.drop_registry.lock().unwrap().is_empty());
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
    fn multi_mutator_stw_keeps_other_thread_roots_alive() {
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
            r2.store(true, Ordering::SeqCst);
            while !d2.load(Ordering::SeqCst) {
                willow_gc_safepoint();
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
        GcTlabState {
            cursor: AtomicUsize::new(0),
            limit: AtomicUsize::new(0),
            fast_allocations: AtomicU64::new(0),
            fast_allocated_bytes: AtomicU64::new(0),
        }
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
        assert_eq!(GC_TLAB_STATE_SIZE, 32);
        assert_eq!(std::mem::offset_of!(GcTlabState, cursor), 0);
        assert_eq!(std::mem::offset_of!(GcTlabState, limit), 8);
        assert_eq!(std::mem::offset_of!(GcTlabState, fast_allocations), 16);
        assert_eq!(std::mem::offset_of!(GcTlabState, fast_allocated_bytes), 24);
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
        willow_gc_write_barrier(parent, young, GcStoreDestination::ObjectField as i64);
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
        let mut map = crate::map::willow_map_new();
        willow_push_root(&mut map as *mut *mut u8);
        let mut tls = new_tlab_state();
        let young = willow_gc_alloc_slow(&mut tls, 42, 0, 8, 0);
        unsafe { *(young as *mut i64) = 902 };
        crate::map::willow_map_insert(map, 7, 0, young as i64, 1);
        assert!(willow_gc_remembered_set_size() > 0);

        willow_gc_minor_collect();

        let option = crate::map::willow_map_get(map, 7, 0);
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
        willow_gc_write_barrier(parent, child, 1);
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
        willow_gc_write_barrier(parent, young, GcStoreDestination::ObjectField as i64);
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
    fn test_gc_runtime_root_ignores_null_and_ref_counts_retentions() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(2, 32);
        let root = ptr as usize;

        willow_gc_add_runtime_root(std::ptr::null_mut());
        willow_gc_add_runtime_root(ptr);
        willow_gc_add_runtime_root(ptr);
        assert_eq!(runtime().runtime_roots.lock().unwrap().len(), 1);
        assert_eq!(
            runtime().runtime_roots.lock().unwrap().get(&root).copied(),
            Some(2)
        );

        willow_gc_remove_runtime_root(std::ptr::null_mut());
        assert_eq!(runtime().runtime_roots.lock().unwrap().len(), 1);
        assert_eq!(
            runtime().runtime_roots.lock().unwrap().get(&root).copied(),
            Some(2)
        );

        willow_gc_remove_runtime_root(ptr);
        assert_eq!(runtime().runtime_roots.lock().unwrap().len(), 1);
        assert_eq!(
            runtime().runtime_roots.lock().unwrap().get(&root).copied(),
            Some(1)
        );
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(32),
            "one remaining runtime retention must keep the object alive"
        );

        willow_gc_remove_runtime_root(ptr);
        assert_eq!(runtime().runtime_roots.lock().unwrap().len(), 0);
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

    // 観点2: 連続確保でヒープリストが更新される
    #[test]
    fn test_gc_consecutive_allocs_update_heap_head() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_alloc_object(1, 8);
        let head1 = runtime().heap.lock().unwrap().heap_head;
        willow_alloc_object(2, 8);
        let head2 = runtime().heap.lock().unwrap().heap_head;
        assert_ne!(head1, head2, "heap_head must change after each alloc");
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

    // 観点8: ヒープリストのリンク順序が正しい
    #[test]
    fn test_gc_heap_list_link_order() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr1 = willow_alloc_object(1, 8);
        let hdr1 = payload_to_header(ptr1);
        let ptr2 = willow_alloc_object(2, 8);
        let hdr2 = payload_to_header(ptr2);
        let head = runtime().heap.lock().unwrap().heap_head;
        assert_eq!(head, hdr2, "most recent alloc must be heap_head");
        assert_eq!(
            unsafe { (*hdr2).next },
            hdr1,
            "hdr2.next must point to hdr1"
        );
        assert!(unsafe { (*hdr1).next }.is_null(), "hdr1.next must be null");
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
            runtime().heap.lock().unwrap().heap_head,
            payload_to_header(ptr_a)
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
            runtime().heap.lock().unwrap().heap_head,
            payload_to_header(ptr_b)
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
    fn test_gc_survivors_remain_linked_after_sweep() {
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
        // ヒープリストをたどって3個確認
        let mut count = 0;
        let mut cur = runtime().heap.lock().unwrap().heap_head;
        while !cur.is_null() {
            count += 1;
            cur = unsafe { (*cur).next };
        }
        assert_eq!(count, 3, "all 3 survivors must be linked");
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
        let elems: Vec<*mut u8> = (0..N)
            .map(|_| willow_alloc_object(TYPE_LEAF as i64, 8))
            .collect();

        // payload: [len: i64, ptr0, ptr1, ptr2, ptr3] = 8 + 4*8 = 40 bytes
        let array_payload: usize = 8 + N * 8;
        let array = willow_alloc_object(TYPE_ARRAY as i64, array_payload as i64);
        unsafe {
            *(array as *mut i64) = N as i64;
            for (i, &ep) in elems.iter().enumerate() {
                *((array.add(8 + i * 8)) as *mut *mut u8) = ep;
            }
        }

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
