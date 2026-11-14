// GC Runtime — non-moving old generation + copying young generation
//
// Object layout in memory:
//   [ GcHeader | payload bytes ... ]
//
// The GcHeader is immediately before the payload.  `willow_alloc_object`
// returns a pointer to the payload start, just like malloc.
//
// Root stack: context-owned storage of *mut *mut u8.  Each entry points to a
// stack slot that holds a GC-managed pointer.  Generated code pushes a slot
// on entry and pops it on exit.  The mark phase reads through each slot to
// reach the live object.
//
// Old objects live in non-moving regular/large regions. Region metadata owns
// allocation enumeration, storage, mark bits, free spans, and liveness accounting.
// Generated young objects live
// in nursery TLAB regions. Survivors reachable through root or heap slots copy
// to collector-owned young chunks once, then tenure into old regions on their
// second minor survival; the collector rewrites every such slot, including
// runtime-owned slots (`willow_gc_add_runtime_root_slot`). Address-registered
// runtime roots name only old objects. A thread parked inside runtime code
// outside a `RelocationSafeScope` defers the minor collection while it
// publishes a young referent; only a hard memory budget pins instead.

mod path_observation;
use path_observation::{PathObservation, RuntimePathObservation};

mod condition;
mod cycle_wait;
mod wait_observation;
pub(crate) use condition::GcCondvar;
use wait_observation::{RuntimeWaitObservation, WaitObservation};

mod safepoint;
pub use safepoint::*;
mod marking;
use marking::*;
mod registry;
mod remembered;
pub use registry::*;
pub(crate) use remembered::ARRAY_CARD_SLOTS;
mod root_arena;
pub(crate) use root_arena::*;
mod tlab;
use tlab::*;
mod root;
pub use root::*;
mod barrier;
pub use barrier::*;
mod progress;
pub use progress::{GcProgressV1, GcSkipReason, gc_progress, willow_gc_progress_v1};
mod heap_snapshot;
pub use heap_snapshot::*;
mod validation;
use validation::*;

pub(crate) use marking::shutdown_mark_workers;

mod storage_allocator;
use storage_allocator::{RuntimeStorageAllocator, StorageAllocation};

mod region;
use region::*;

mod address_index;
mod assist;
mod concurrent_bitmap;
mod coordinator;
mod epoch_index;
mod epoch_observation;
mod free_spans;
mod mark_closure;
mod mark_termination;
mod mark_workers;
pub(crate) use mark_workers::marker_configuration;
mod memory_control;
mod minor;
mod nursery;
mod old_cycle;
mod old_lab;
pub(crate) use old_lab::retire_served_in as retire_served_old_lab_in;
pub mod opportunity;
mod pacer;
mod pending_registry;
mod region_cache;
mod region_candidates;
mod root_handshake;
mod root_tlabs;
mod runtime_roots;
mod satb;
mod sweep;

use free_spans::FreeSpans;
pub(crate) use minor::minor_collect_internal;
use minor::{MinorRoots, PinSource};
#[cfg(test)]
pub(crate) use minor::{deferred_minor_collections_for_test, minor_collect_pinning_for_test};
use runtime_roots::RuntimeRootSet;

use crate::id_hash::{IdMap, IdSet};
use std::alloc::{Layout, dealloc};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, PoisonError};
use std::thread::ThreadId;

pub use willow_abi::{GcObjectKind, GcStoreDestination};

// Inline and bitmap masks index mixed scalar/reference storage words.
// Shadow root arrays and custom trace callbacks retain their native layouts.
const GC_STORAGE_WORD_BYTES: usize =
    willow_abi::storage_word_bytes(std::mem::size_of::<usize>() as u32) as usize;

const GC_GENERATION_YOUNG: u8 = willow_abi::gc_header::GENERATION_YOUNG;
const GC_GENERATION_OLD: u8 = willow_abi::gc_header::GENERATION_OLD;
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
/// for the first word of every slot whose layout marks it as traced.
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
    let mut roots = Vec::new();
    let mut offset = 0;
    for kind in layout.payload.slots {
        if kind.traces_first_word() {
            roots.push((offset, GcRootHandle::new(words[offset] as *mut u8)));
        }
        offset += kind.word_count() as usize;
    }

    let pointer_bytes = std::mem::size_of::<usize>() as u32;
    let payload_bytes = layout.payload_bytes(pointer_bytes) as i64;
    let value = willow_alloc_with_layout(
        GcObjectKind::Enum,
        type_id,
        payload_bytes,
        layout.gc_ref_mask(),
    );
    for (offset, root) in &roots {
        words[*offset] = root.load() as i64;
    }
    if value.is_null() {
        return value;
    }
    // SAFETY: the allocation has exactly the tag plus payload word count
    // described by `layout`; all writes are within that initialized payload.
    unsafe {
        *value.cast::<i64>() = i64::from(layout.tag);
        let mut offset = 0;
        for kind in layout.payload.slots {
            let count = kind.word_count() as usize;
            for index in 0..count {
                let word = words[offset + index];
                let destination = value.cast::<i64>().add(1 + offset + index);
                if index == 0 && kind.traces_first_word() {
                    willow_gc_write_barrier(
                        value,
                        std::ptr::null_mut(),
                        word as *mut u8,
                        GcStoreDestination::EnumPayload as i64,
                    );
                    store_gc_reference(destination.cast(), word as *mut u8);
                } else {
                    destination.write(word);
                }
            }
            offset += count;
        }
    }
    value
}

/// [`willow_alloc_enum_variant`] for runtime entries called directly by
/// generated code. A type-id-0 variant with exactly one payload word
/// bump-allocates young from the runtime TLAB, with no GC point,
/// root registration or old-region work; only a TLAB miss reaches the
/// collecting slow path, rooting a traced word in a shadow-stack slot. Other
/// shapes, and threads without a runtime TLAB, use the general helper.
pub(crate) fn alloc_enum_variant_from_generated(
    type_id: u32,
    layout: willow_abi::EnumVariantLayout<'_>,
    payload_words: &[i64],
) -> *mut u8 {
    let pointer_bytes = std::mem::size_of::<usize>() as u32;
    let mask = layout.gc_ref_mask();
    let young = (type_id == 0
        && payload_words.len() == 1
        && layout.payload.word_count() == 1
        && layout.payload_bytes(pointer_bytes) == 16)
        .then(|| enum_pair_layout(mask))
        .flatten();
    let Some(young) = young else {
        return willow_alloc_enum_variant(type_id, layout, payload_words);
    };
    let mut word = payload_words[0] as *mut u8;
    let value = match try_alloc_young([young]) {
        Some([value]) => value,
        None => {
            let traced = mask != 0;
            if traced {
                willow_push_root(&mut word);
            }
            let value = alloc_young_layout_slow(young);
            if traced {
                willow_pop_roots(1);
            }
            value
        }
    };
    if value.is_null() {
        return value;
    }
    // SAFETY: the object holds the tag word and one payload word; the
    // payload is fresh zeroed storage, so the barrier sees no deleted edge.
    unsafe {
        *value.cast::<i64>() = i64::from(layout.tag);
        let destination = value.cast::<i64>().add(1);
        if mask != 0 {
            willow_gc_write_barrier(
                value,
                std::ptr::null_mut(),
                word,
                GcStoreDestination::EnumPayload as i64,
            );
            store_gc_reference(destination.cast(), word);
        } else {
            destination.write(word as i64);
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
    /// Number of copying minor collections survived while young.
    pub age: u8,
    /// Runtime-interned descriptors are reference counted; generated ones are static.
    pub descriptor_owned: bool,
    /// Nonzero exactly while this payload is in `GcState::remembered_set`.
    /// Generated reference-array stores read it to skip a redundant barrier
    /// call; only the runtime writes it, under the heap lock
    /// or with mutators stopped.
    pub remembered: u8,
    /// Live: immutable descriptor address. During minor STW only, a moved
    /// source holds its destination payload address tagged with low bit 1;
    /// metadata then comes from that live destination. Reclaimed: original
    /// allocation size, so retained TLAB holes remain walkable without metadata.
    pub descriptor: usize,
}

/// Generated-code-facing TLS allocation state.
///
/// The compiler defines one zero-initialized TLS instance of this layout in
/// each Willow executable. The runtime receives its address only on the slow
/// path, registers the owning chunk, and may invalidate cursor/limit while the
/// mutator is stopped for collection. Generated code writes cursor and start
/// words with plain stores and keeps no per-object counters; the runtime
/// derives fast-path bytes from the cursor and counts from the start words.
///
#[repr(C)]
pub struct GcTlabState {
    cursor: AtomicUsize,
    limit: AtomicUsize,
    /// Stable pointer to this chunk's atomic object-start words. Published
    /// before cursor; generated allocation sets its bit after header writes.
    start_bits: AtomicUsize,
}

impl GcTlabState {
    const fn new() -> Self {
        Self {
            cursor: AtomicUsize::new(0),
            limit: AtomicUsize::new(0),
            start_bits: AtomicUsize::new(0),
        }
    }
}

pub const GC_TLAB_STATE_SIZE: usize = std::mem::size_of::<GcTlabState>();
pub const GC_HEADER_SIZE: usize = std::mem::size_of::<GcHeader>();
pub const GC_TLAB_CHUNK_SIZE: usize = willow_abi::tlab::CHUNK_SIZE as usize;
const _: () = assert!(GC_TLAB_CHUNK_SIZE <= u16::MAX as usize + 1);
pub const GC_TLAB_MAX_OBJECT_SIZE: usize = 4 * 1024;

#[path = "gc/layouts.rs"]
mod layouts;
#[path = "gc/runtime_tlab.rs"]
mod runtime_tlab;
use runtime_tlab::enum_pair_layout;
pub use runtime_tlab::willow_gc_current_tlab;
pub(crate) use runtime_tlab::{
    RuntimeTlabOwner, YoungLayout, alloc_string_payload_slow, alloc_young_layout_slow,
    array_buffer_layout, array_handle_layout, array_pair_layout, try_alloc_young,
    try_alloc_young_string_payload,
};

/// Raw allocation and pointer arithmetic boundary for the collector. The rest
/// of the GC works with `Object`/`Payload`/`RootSlot` and cannot directly
/// dereference a header or stack-slot pointer.
mod raw_heap;

use raw_heap::{Object as HeapObject, Payload as GcPayload, RootSlot};

// ---------------------------------------------------------------------------
// GC state
// ---------------------------------------------------------------------------

struct GcState {
    storage_allocator: RuntimeStorageAllocator,
    epoch_views: epoch_observation::ViewObservation,
    concurrent_cycle: Option<Arc<ConcurrentCycle>>,
    sweeping: Option<ThreadId>,
    pending_sweep_chunks: std::ops::Range<usize>,
    satb: satb::SatbBuffers,
    /// Bump-allocation chunks. Active chunks are owned by one TLS state;
    /// collection retires them before walking their object headers. Collector-only
    /// survivor chunks share this index but never belong to a TLS state.
    tlab_chunks: Vec<BumpChunk>,
    /// Empty evacuation chunks admitted before a minor stop; included in TLAB reservations.
    survivor_reserve: Vec<BumpChunk>,
    survivor_reserve_active: bool,
    survivor_reserve_stats: crate::gc_telemetry::SurvivorReserveStats,
    tlab_addresses: address_index::AddressIndex,
    /// Non-moving old-generation storage. Regular regions serve old/runtime
    /// allocations from a bump tail or region-local free spans. Large objects
    /// receive one dedicated region. Object addresses never change.
    old_regions: Vec<OldRegion>,
    old_addresses: address_index::AddressIndex,
    /// (largest available span, region index); excludes dedicated/full regions.
    old_region_candidates: region_candidates::RegionCandidates,
    old_reserved_bytes: usize,
    /// Released region storage kept for reuse; counted by `can_reserve`.
    region_cache: region_cache::RegionStorageCache,
    /// Per-thread old allocation buffers used by runtime-side allocation.
    old_labs: Vec<old_lab::OldLabRecord>,
    old_lab_indices: IdMap<usize, usize>,
    old_lab_pending: pending_registry::Pending<usize>,
    /// Generated TLS states observed on allocation slow paths.
    tlab_states: HashMap<usize, TlabStateRecord>,
    tlab_accounting_observation: tlab::RuntimeAccountingObservation,
    minor_observation: minor::RuntimeMinorObservation,
    tlab_owners: HashMap<ThreadId, HashSet<usize>>,
    /// Total bytes currently allocated (header + payload).
    allocated_bytes: usize,
    /// Trigger a collection when allocated_bytes exceeds this threshold.
    threshold_bytes: usize,
    /// Hard cap on managed reservations plus retained native owner storage.
    memory_limit_bytes: Option<usize>,
    /// Reservation pressure fires once above 75%, rearming at/below 50%.
    hard_pressure_triggered: bool,
    soft_memory: memory_control::Controller,
    last_major_live_bytes: u64,
    /// Allocation clock at sweep entry; bounds floating garbage during sweep.
    sweep_allocation_start: u64,
    /// `last_major_live_bytes` in the major-policy domain, which excludes the
    /// nursery budget ([`major_policy_managed_bytes`]).
    last_major_policy_live_bytes: u64,
    last_major_mark_work: u64,
    pacer: pacer::Sampler,
    pacer_trigger: u64,
    /// Bytes occupied by young objects in TLABs and collector survivor chunks.
    young_allocated_bytes: usize,
    /// Trigger a minor collection at the next TLAB refill after this threshold.
    nursery_threshold_bytes: usize,
    /// Most threads seen allocating young objects at once since the last minor.
    young_allocators: usize,
    scheduler_runway_initialized: bool,
    /// Saturating young-mutator byte clock. Reclamation and copying never alter it.
    young_allocation_clock: u64,
    /// Clock at the first pending request or root deferral; captured once per epoch.
    minor_deferral_start_bytes: Option<u64>,
    minor_deferral_budget_bytes: u64,
    minor_retry_bytes: u64,
    pin_records: HashMap<usize, crate::gc_telemetry::GcPinRecord>,
    nursery_policy: nursery::Policy,
    tenuring: nursery::Tenuring,
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
    /// Legacy combined bump-storage reservation: nursery, pinned, and survivor.
    tlab_reserved_bytes: usize,
    /// Old objects that may contain at least one young reference. Owners are
    /// payload addresses and remain stable because the old generation does not
    /// move.
    remembered_set: remembered::RememberedSet,
    /// Sparse card table keyed by absolute old-heap card. Region bounds make
    /// each dirty card attributable to one old/pinned region without changing
    /// the Stage-4 barrier ABI.
    dirty_cards: HashSet<usize>,
    write_barrier_hits: u64,
    minor_collections: u64,
    promoted_objects: u64,
    promoted_bytes: u64,
    moved_objects: u64,
    survivor_stats: crate::gc_telemetry::GcSurvivorStats,
    old_region_allocations: u64,
    old_region_reuses: u64,
    old_regions_released: u64,
    major_collections: u64,
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

#[derive(Default)]
struct NativeAccounting {
    _reconciliation: crate::native_memory::ReconciliationObservation,
    // Owners can outlive a reset epoch; only their release removes this charge.
    bytes: AtomicUsize,
    allocated_bytes: AtomicU64,
    // A quiescing collector closes headroom before waiting for this to drain.
    in_flight: AtomicUsize,
}

#[cfg(test)]
pub(crate) fn native_reconciliation_observation()
-> &'static crate::native_memory::ReconciliationObservation {
    &runtime().native_accounting._reconciliation
}

pub(crate) fn external_bytes() -> usize {
    runtime().native_accounting.bytes.load(Ordering::Relaxed)
}

/// Coupled native and managed allowances between locked policy checks.
/// Both are invalidated when a policy input can lower a collection threshold.
#[derive(Default)]
struct AllocationHeadroom {
    // Native charges may skip the heap lock only while no trigger can fire.
    // Zero under a soft limit or within a quarter of a hard reservation cap.
    // Managed growth consumes this allowance too.
    external: AtomicUsize,
    // Runtime-allocation policy checks may skip the heap lock below triggers.
    // Zero under hard/soft limits; capped to bound allocation-rate sample lag.
    policy: AtomicU64,
}

impl AllocationHeadroom {
    fn invalidate(&self) {
        self.external.store(0, Ordering::Release);
        self.policy.store(0, Ordering::Release);
    }

    /// Charge occupied growth before publishing it. Exhausted allowances stay
    /// zero until a locked policy check republishes them.
    fn consume(&self, bytes: usize) {
        if self.external.load(Ordering::Acquire) != 0 {
            let _ = self
                .external
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |room| {
                    Some(room.saturating_sub(bytes))
                });
        }
        self.consume_policy(bytes as u64);
    }

    fn consume_policy(&self, bytes: u64) {
        if self.policy.load(Ordering::Acquire) != 0 {
            let _ = self
                .policy
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |room| {
                    Some(room.saturating_sub(bytes))
                });
        }
    }
}

// Bound the interval between locked allocation-rate/policy samples.
const POLICY_HEADROOM_CAP: u64 = 1 << 20;

fn invalidate_external_headroom() {
    runtime().headroom.invalidate();
}

pub(super) fn consume_headrooms(bytes: usize) {
    runtime().headroom.consume(bytes);
}

fn policy_headroom(state: &GcState) -> u64 {
    policy_headroom_in(runtime(), state)
}

fn policy_headroom_in(owner: &GcRuntime, state: &GcState) -> u64 {
    if state.memory_limit_bytes.is_some() || state.soft_memory.enabled() {
        return 0;
    }
    let input = memory_inputs_in(owner, state);
    let mut trigger = input.unlimited_goal.max(input.live);
    if state.pacer.enabled() {
        trigger = trigger.min(state.pacer_trigger);
    }
    trigger
        .saturating_sub(input.occupied)
        .min(POLICY_HEADROOM_CAP)
}

/// Whether `bytes` of runtime allocation fit the lock-free policy headroom.
fn consume_policy_headroom(owner: &GcRuntime, bytes: u64) -> bool {
    owner
        .headroom
        .policy
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |room| {
            room.checked_sub(bytes)
        })
        .is_ok()
}

/// Bytes before the next native charge could cross the unlimited goal, the
/// pacer trigger, or (under a hard cap) the hard-pressure watermark at 3/4 of
/// the cap, below which `can_reserve` cannot fail either. Goal and trigger only
/// move under the heap lock and every write that can lower them invalidates
/// the headroom; reservation growth quiesces it in `can_reserve`. Retained
/// region storage occupies the cap too: it shares the room below the
/// watermark, and sweep only moves reserved bytes into it. Zero at or past the
/// watermark and under a soft limit, whose controller decides on every charge.
fn external_headroom_in(owner: &GcRuntime, state: &GcState) -> usize {
    if state.soft_memory.enabled() {
        return 0;
    }
    let input = memory_inputs_in(owner, state);
    let below = |trigger: u64| trigger.saturating_sub(input.occupied).saturating_sub(1);
    let mut room = below(input.unlimited_goal.max(input.live));
    if let Some(watermark) = hard_pressure_watermark(state) {
        room = room.min(
            watermark
                .saturating_sub(input.pressure_bytes)
                .saturating_sub(state.region_cache.retained_bytes() as u64)
                .saturating_sub(1),
        );
    }
    if state.pacer.enabled() && input.occupied < state.pacer_trigger {
        room = room.min(below(state.pacer_trigger));
    }
    room.min(POLICY_HEADROOM_CAP) as usize
}

/// 3/4 of the hard cap, where native charges stop being lock-free.
fn hard_pressure_watermark(state: &GcState) -> Option<u64> {
    state
        .memory_limit_bytes
        .map(|limit| (limit / 4).saturating_mul(3).max(1) as u64)
}

/// Evict retained region storage that would close the lock-free native
/// headroom below the hard-pressure watermark. Locked charges call this before
/// republishing, so the cache never forces every later charge onto the heap
/// lock. O(1) unless storage is evicted.
fn trim_region_cache_to_watermark(owner: &GcRuntime, state: &mut GcState) {
    if let Some(watermark) = hard_pressure_watermark(state) {
        let keep = watermark
            .saturating_sub(memory_inputs_in(owner, state).pressure_bytes)
            .saturating_sub(1);
        state
            .region_cache
            .shrink_to(usize::try_from(keep).unwrap_or(usize::MAX));
    }
}

/// Close the lock-free native path and wait out charges already inside it.
/// After this, `external_bytes()` holds no transient (rolled-back) bytes and
/// no new lock-free charge can start until a locked charge republishes the
/// headroom. SeqCst pairs with the charger's in-flight increment and headroom
/// load: either the charger sees zero headroom, or this sees it in flight.
/// O(1) plus a spin over at most a few atomics per in-flight charge.
fn quiesce_external_charges() {
    quiesce_external_charges_in(runtime());
}

fn quiesce_external_charges_in(owner: &GcRuntime) {
    owner.headroom.external.store(0, Ordering::SeqCst);
    while owner.native_accounting.in_flight.load(Ordering::SeqCst) != 0 {
        std::hint::spin_loop();
    }
}

/// Lock-free native charge; false (with nothing charged) when the headroom
/// cannot cover `bytes`.
fn charge_external_lock_free_in(owner: &GcRuntime, bytes: usize) -> bool {
    owner
        .native_accounting
        .in_flight
        .fetch_add(1, Ordering::SeqCst);
    let charged = owner.headroom.external.load(Ordering::SeqCst) >= bytes && {
        // Publish the bytes BEFORE claiming headroom: a locked charge that
        // republishes `owner.headroom.external` in between then already counts them
        // in `occupied`. A failed claim rolls the add back.
        owner
            .native_accounting
            .bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(bytes)
            })
            .unwrap_or_else(|_| {
                crate::failure::resource_exhausted(format_args!("native byte accounting overflow"))
            });
        let claimed = owner
            .headroom
            .external
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |room| {
                room.checked_sub(bytes)
            })
            .is_ok();
        if !claimed {
            owner
                .native_accounting
                .bytes
                .fetch_sub(bytes, Ordering::AcqRel);
        }
        claimed
    };
    owner
        .native_accounting
        .in_flight
        .fetch_sub(1, Ordering::SeqCst);
    if !charged {
        return false;
    }
    // Within the headroom no crossing or cap check can fire: account without
    // serializing native-heavy pipelines on the heap mutex. The policy
    // headroom must also see this `occupied` growth.
    owner.headroom.consume_policy(bytes as u64);
    let _ = owner.native_accounting.allocated_bytes.fetch_update(
        Ordering::Relaxed,
        Ordering::Relaxed,
        |current| Some(current.saturating_add(bytes as u64)),
    );
    true
}

pub(crate) fn charge_external(bytes: usize) {
    if charge_external_in(runtime(), bytes) {
        coordinator::request();
    }
}

// Returns whether the owner needs a collection request, after releasing its heap.
fn charge_external_in(owner: &GcRuntime, bytes: usize) -> bool {
    if charge_external_lock_free_in(owner, bytes) {
        return false;
    }
    // Serialize increases with region reservations. Releases need no heap lock,
    // so finalizers also work during reset and stop-the-world collection.
    let mut state = owner.heap.lock().unwrap();
    // Exact inputs for the crossing checks below.
    quiesce_external_charges_in(owner);
    if !can_reserve_in(owner, &mut state, bytes) {
        allocation_failure_in(owner, &state);
    }
    let previous_external = owner
        .native_accounting
        .bytes
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(bytes)
        })
        .unwrap_or_else(|_| {
            crate::failure::resource_exhausted(format_args!("native byte accounting overflow"))
        });
    let _ = owner.native_accounting.allocated_bytes.fetch_update(
        Ordering::Relaxed,
        Ordering::Relaxed,
        |current| Some(current.saturating_add(bytes as u64)),
    );
    let previous_footprint = state
        .old_reserved_bytes
        .saturating_add(state.tlab_reserved_bytes)
        .saturating_add(previous_external);
    // Native-only mutations must not request a whole-heap cycle for every key
    // after crossing the hard-pressure watermark. The cap still applies to
    // every increase; soft limits use their existing progress/relief policy.
    let hard_crossing = state.memory_limit_bytes.is_some_and(|limit| {
        let trigger = (limit / 4).saturating_mul(3).max(1);
        previous_footprint < trigger && previous_footprint.saturating_add(bytes) >= trigger
    });
    let previous_occupied =
        major_policy_managed_bytes(&state).saturating_add(previous_external) as u64;
    let paced_crossing = state.pacer.enabled()
        && previous_occupied < state.pacer_trigger
        && previous_occupied.saturating_add(bytes as u64) >= state.pacer_trigger;
    state.soft_memory.sample_process();
    let pressure = state.concurrent_cycle.is_none()
        && state.sweeping.is_none()
        && (hard_crossing
            || paced_crossing
            || state
                .soft_memory
                .decide(memory_inputs_in(owner, &state))
                .collect);
    trim_region_cache_to_watermark(owner, &mut state);
    owner
        .headroom
        .external
        .store(external_headroom_in(owner, &state), Ordering::Release);
    drop(state);
    pressure
}

pub(crate) fn release_external(bytes: usize) {
    release_external_in(runtime(), bytes);
}

fn release_external_in(owner: &GcRuntime, bytes: usize) {
    owner
        .native_accounting
        .bytes
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_sub(bytes)
        })
        .expect("native byte accounting underflow");
}

/// Retained region storage counts toward the hard cap and the soft limit but
/// never refuses a reservation: it is freed first, so admission ignores it.
#[cfg(test)]
fn can_reserve(state: &mut GcState, additional: usize) -> bool {
    can_reserve_in(runtime(), state, additional)
}

fn can_reserve_in(owner: &GcRuntime, state: &mut GcState, additional: usize) -> bool {
    let Some(limit) = state.memory_limit_bytes else {
        if state.soft_memory.enabled() {
            evict_region_cache_for_in(owner, state, additional);
        }
        return true;
    };
    // Every reservation passes here first: stop lock-free native charges
    // (their headroom predates this reservation) and drop their transient
    // bytes before the exact cap check.
    quiesce_external_charges_in(owner);
    if state
        .old_reserved_bytes
        .checked_add(state.tlab_reserved_bytes)
        .and_then(|total| total.checked_add(owner.native_accounting.bytes.load(Ordering::Relaxed)))
        .and_then(|total| total.checked_add(additional))
        .is_none_or(|total| total > limit)
    {
        return false;
    }
    evict_region_cache_for_in(owner, state, additional);
    true
}

/// Free retained storage that would leave less than `additional` bytes below
/// either limit. O(1) unless storage is freed.
fn evict_region_cache_for(state: &mut GcState, additional: usize) {
    evict_region_cache_for_in(runtime(), state, additional);
}

fn evict_region_cache_for_in(owner: &GcRuntime, state: &mut GcState, additional: usize) {
    let keep = region_cache_budget_in(owner, state).saturating_sub(additional);
    state.region_cache.shrink_to(keep);
}

/// Bytes released storage may occupy without crossing the hard cap or the
/// soft limit. Under a soft limit retained storage is evicted rather than
/// counted as collection pressure: no collection could reclaim it.
fn region_cache_budget(state: &GcState) -> usize {
    region_cache_budget_in(runtime(), state)
}

fn region_cache_budget_in(owner: &GcRuntime, state: &GcState) -> usize {
    let footprint = state
        .old_reserved_bytes
        .saturating_add(state.tlab_reserved_bytes)
        .saturating_add(owner.native_accounting.bytes.load(Ordering::Relaxed));
    let room = |limit: usize| limit.saturating_sub(footprint);
    let soft = state.soft_memory.limit().map_or(usize::MAX, |limit| {
        room(usize::try_from(limit).unwrap_or(usize::MAX))
    });
    state.memory_limit_bytes.map_or(usize::MAX, room).min(soft)
}

/// Admit a new old region, reusing retained storage of the same capacity.
/// The caller accounts the returned region as reserved.
fn reserve_old_region(state: &mut GcState, kind: RegionKind, capacity: usize) -> Option<OldRegion> {
    reserve_old_region_in(runtime(), state, kind, capacity)
}

fn reserve_old_region_in(
    owner: &GcRuntime,
    state: &mut GcState,
    kind: RegionKind,
    capacity: usize,
) -> Option<OldRegion> {
    let retained = state.region_cache.take(capacity);
    if !can_reserve_in(owner, state, capacity) {
        if let Some(base) = retained {
            // SAFETY: the block left the cache and is owned here.
            unsafe { region_cache::free_region_storage(base, capacity) };
        }
        return None;
    }
    match retained {
        // SAFETY: the cache held this released block of exactly `capacity`.
        Some(base) => Some(unsafe { OldRegion::reuse(kind, capacity, base) }),
        None => OldRegion::new(&state.storage_allocator, kind, capacity),
    }
}

/// Collections one failed allocation may wait for while each still releases
/// storage that concurrent allocators claim first. Bounded so a request that
/// never fits reports exhaustion even while other threads keep producing garbage.
const BUDGET_COLLECTION_RETRIES: usize = 8;

/// Give reclamation an opportunity before treating reservation pressure as
/// exhaustion. Callers retry while each collection still releases storage:
/// concurrent allocators may claim it first, so exhaustion is reported after a
/// collection that freed nothing or after [`BUDGET_COLLECTION_RETRIES`].
/// Waiting mutators cooperate with remark and wait through sweep.
/// Allocation-failure path only; explicit `gc()` uses `cycle_wait` directly.
fn collect_for_budget() {
    collect_for_budget_in(crate::runtime::current());
}

fn collect_for_budget_in(owner: &crate::runtime::Runtime) {
    pacer::allocation_failure_in(owner, &owner.gc.heap.lock().unwrap());
    let _interval = crate::gc_telemetry::observations::Span::for_runtime(
        owner,
        crate::gc_telemetry::observations::Kind::AllocationStall,
    );
    cycle_wait::collect_for_budget_in(owner);
}

fn allocation_failure(state: &GcState) -> *mut u8 {
    allocation_failure_in(runtime(), state)
}

fn allocation_failure_in(owner: &GcRuntime, state: &GcState) -> *mut u8 {
    if let Some(limit) = state.memory_limit_bytes {
        let memory = heap_snapshot::accounting_from_state_in(owner, state);
        crate::gc_telemetry::failure_record::emit_with(
            "reservation_threshold",
            "reservation admission failed",
            Some(memory),
            Some(
                memory
                    .old_reserved_bytes
                    .saturating_add(memory.young_reserved_bytes)
                    .saturating_add(memory.external_bytes),
            ),
            Some(limit as u64),
        );
        crate::failure::resource_exhausted(format_args!(
            "GC memory limit exceeded ({limit} bytes of managed and native reservations)"
        ));
    }
    crate::failure::resource_exhausted(format_args!("managed allocation failed"))
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
    pacer_inputs_in(crate::runtime::current(), state)
}

fn pacer_inputs_in(owner: &crate::runtime::Runtime, state: &GcState) -> pacer::Inputs {
    let history = crate::gc_telemetry::pacing_cycle_snapshot_in(&owner.telemetry);
    let cpu = owner.telemetry.pacing.snapshot();
    let memory = memory_inputs_in(&owner.gc, state);
    pacer::Inputs {
        live: memory.live,
        previous_goal: state.threshold_bytes as u64,
        expected_work: state
            .last_major_mark_work
            .max(state.last_major_live_bytes)
            .saturating_sub(history.last_major_cycle.root_scan_bytes),
        allocation_per_second: 0,
        mark_per_cpu_second: None,
        cpu_capacity: pacer::cpu_capacity(),
        max_workers: *owner.marker_count,
        feedback: pacer::Feedback {
            occupied: memory.occupied,
            pressure: state.soft_memory.enabled() && state.soft_memory.decide(memory).collect,
            window_ns: crate::gc_telemetry::timestamp_ns().saturating_sub(cpu.window_start_ns),
            gc_cpu_ns: (cpu.window_end_ns != 0 && cpu.window_cpu_missing == 0)
                .then_some(cpu.window_cpu_ns),
            root_work: history.last_major_cycle.root_scan_bytes,
            resident_bytes: memory.process_resident,
            soft_limit: state.soft_memory.limit(),
            ..pacer::Feedback::default()
        },
        active: state.concurrent_cycle.is_some() || state.sweeping.is_some(),
        fixed_goal: false,
    }
}

// Policy samplers share one cumulative domain. Reclamation reduces occupancy,
// but must not remove native allocation history from a rate sample. Young
// allocation enters the domain only when promoted: minor collections reclaim
// the rest, so a high young allocation rate alone must not shorten the major
// runway.
fn policy_allocated_total(state: &GcState) -> u64 {
    policy_allocated_total_in(runtime(), state)
}

fn policy_allocated_total_in(owner: &GcRuntime, state: &GcState) -> u64 {
    state
        .total_allocated_bytes
        .saturating_sub(state.young_allocation_clock)
        .saturating_add(state.promoted_bytes)
        .saturating_add(
            owner
                .native_accounting
                .allocated_bytes
                .load(Ordering::Relaxed),
        )
}

/// Managed bytes the major policy is responsible for. Minor collections own
/// young bytes up to the nursery budget; counting them would start whole-heap
/// cycles for garbage the next minor reclaims anyway, and once the nursery
/// approaches the goal every allocation sample would request a major. Young growth beyond the
/// budget (deferred minors) still
/// counts. Hard reservation pressure always sees every reserved byte.
fn major_policy_managed_bytes(state: &GcState) -> usize {
    let nursery_owned = state
        .young_allocated_bytes
        .min(state.nursery_threshold_bytes);
    state.allocated_bytes.saturating_sub(nursery_owned)
}

fn memory_inputs(state: &GcState) -> memory_control::Inputs {
    memory_inputs_in(runtime(), state)
}

fn memory_inputs_in(owner: &GcRuntime, state: &GcState) -> memory_control::Inputs {
    // A soft limit budgets every managed byte: excluding the nursery would let
    // the heap exceed the limit by up to one nursery budget.
    // Its relief runway likewise measures all allocation.
    let (live, occupied, allocated_total) = if state.soft_memory.enabled() {
        (
            state.last_major_live_bytes,
            state.allocated_bytes,
            state.total_allocated_bytes.saturating_add(
                owner
                    .native_accounting
                    .allocated_bytes
                    .load(Ordering::Relaxed),
            ),
        )
    } else {
        (
            state.last_major_policy_live_bytes,
            major_policy_managed_bytes(state),
            policy_allocated_total_in(owner, state),
        )
    };
    memory_control::Inputs {
        unlimited_goal: state.threshold_bytes as u64,
        live: live.saturating_add(owner.native_accounting.bytes.load(Ordering::Relaxed) as u64),
        occupied: occupied.saturating_add(owner.native_accounting.bytes.load(Ordering::Relaxed))
            as u64,
        pressure_bytes: state
            .old_reserved_bytes
            .saturating_add(state.tlab_reserved_bytes)
            .saturating_add(owner.native_accounting.bytes.load(Ordering::Relaxed))
            as u64,
        allocated_total,
        // Our process provider reports RSS, not a compatible commit scope.
        non_heap_commit: None,
        process_resident: state.soft_memory.resident(),
    }
}

impl Default for GcState {
    fn default() -> Self {
        let memory_limit_bytes = gc_memory_limit_from_env();
        let nursery_policy = nursery::Policy::from_env();
        Self {
            storage_allocator: RuntimeStorageAllocator::default(),
            epoch_views: Default::default(),
            concurrent_cycle: None,
            sweeping: None,
            pending_sweep_chunks: 0..0,
            satb: satb::SatbBuffers::default(),
            tlab_chunks: Vec::new(),
            survivor_reserve: Vec::new(),
            survivor_reserve_active: false,
            survivor_reserve_stats: Default::default(),
            tlab_addresses: address_index::AddressIndex::default(),
            old_regions: Vec::new(),
            old_addresses: address_index::AddressIndex::default(),
            old_region_candidates: region_candidates::RegionCandidates::default(),
            old_reserved_bytes: 0,
            region_cache: Default::default(),
            old_labs: Vec::new(),
            old_lab_indices: IdMap::default(),
            old_lab_pending: Default::default(),
            tlab_states: HashMap::new(),
            tlab_accounting_observation: tlab::RuntimeAccountingObservation::default(),
            minor_observation: minor::RuntimeMinorObservation::default(),
            tlab_owners: HashMap::new(),
            allocated_bytes: 0,
            threshold_bytes: 1024 * 1024,
            memory_limit_bytes,
            hard_pressure_triggered: false,
            soft_memory: memory_control::Controller::from_env(),
            last_major_live_bytes: 0,
            sweep_allocation_start: 0,
            last_major_policy_live_bytes: 0,
            last_major_mark_work: 0,
            pacer: pacer::Sampler::default(),
            pacer_trigger: 1024 * 1024,
            young_allocated_bytes: 0,
            nursery_threshold_bytes: nursery_policy.initial(memory_limit_bytes),
            young_allocators: 0,
            scheduler_runway_initialized: false,
            young_allocation_clock: 0,
            minor_deferral_budget_bytes: 0,
            pin_records: HashMap::new(),
            minor_deferral_start_bytes: None,
            minor_retry_bytes: 0,
            nursery_policy,
            tenuring: Default::default(),
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
            remembered_set: remembered::RememberedSet::default(),
            dirty_cards: HashSet::new(),
            write_barrier_hits: 0,
            minor_collections: 0,
            promoted_objects: 0,
            promoted_bytes: 0,
            moved_objects: 0,
            survivor_stats: Default::default(),
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

// Compatibility facade: runtime roots and generated roots share context storage.
struct RootStackAccess;
impl RootStackAccess {
    fn with<R>(&self, f: impl FnOnce(&crate::generated_context::ShadowRoots) -> R) -> R {
        crate::generated_context::with_roots(f)
    }
}
static ROOT_STACK: RootStackAccess = RootStackAccess;
// ---------------------------------------------------------------------------
// Multi-mutator coordination: registration + stop-the-world safepoints.
//
//
// The single-mutator runtime reads the current context through ROOT_STACK.
// When more than one mutator thread is registered (for example, a
// `WILLOW_WORKERS=N` worker pool), a collection stops the world: it asks every
// other registered mutator to reach a safepoint, where the mutator publishes a
// SNAPSHOT of its root-slot locations under `COORD`'s lock and parks. The
// collector then scans every registered mutator's published roots. Each thread
// only ever reads its OWN thread-local stack, so there is no cross-thread
// TLS/RefCell aliasing. The collector accesses published slots only while
// their owners are parked; it does not access another thread's RefCell.
//
// Major collection takes its initial snapshot and seals/drains foreign SATB
// buffers inside this parking protocol. Mark work proceeds concurrently. Tracing reads atomic references
// and concurrent container snapshots while SATB/insertion barriers retain edges.
#[derive(Default)]
struct GcCoord {
    /// Registered mutators → writable root-slot addresses, valid only while
    /// the owner is parked. Empty until the first stop-the-world publication.
    mutators: HashMap<ThreadId, PublishedRoots>,
    /// A collector has requested all mutators to reach a safepoint and park.
    stop_requested: bool,
    /// Mutators with published roots at a safepoint or scheduler idle wait.
    parked: HashSet<ThreadId>,
    /// Outstanding registered owners for the current stop. Updated once per
    /// park/unregister, rather than rescanning the registry after every wake.
    stw_pending: HashSet<ThreadId>,
    cycle_collector: Option<ThreadId>,
    cycle_generation: u64,
    recovery_requested_for: Option<u64>,
}

/// Collector-owned GC services. Keeping the heap, roots, registries, and STW
/// coordinator behind one explicit owner makes lock ordering visible and keeps
/// runtime entry points from reaching into unrelated globals.
pub(crate) struct GcRuntime {
    // Drop marker readers before collector storage. Active-cycle shutdown still
    // requires coordinator quiescence before destroying this owner.
    marker_workers: Mutex<Option<mark_workers::Pool>>,
    coordinator: Mutex<Option<coordinator::Coordinator>>,
    active_sweep: sweep::ActiveSweep,
    collection_busy: std::sync::atomic::AtomicBool,
    headroom: AllocationHeadroom,
    policy_identity: u64,
    policy_epoch: AtomicU64,
    mutator_epoch: AtomicU64,
    assist_cycle_generation: AtomicU64,
    mark_phase: MarkPhase,
    card_epoch: remembered::CardEpoch,
    native_accounting: NativeAccounting,
    pub(crate) native_observation: crate::observation::Native,
    progress: Mutex<progress::Progress>,
    stop_diagnostics: Mutex<std::collections::VecDeque<old_cycle::StopDiagnostic>>,
    preparation_diagnostic: Mutex<old_cycle::PreparationDiagnostic>,
    storage_allocator: RuntimeStorageAllocator,
    heap: crate::gc_telemetry::observations::GcMutex<GcState>,
    /// Interned runtime-allocation descriptors referenced by this heap's
    /// headers; declared after `heap` so it outlives every header reference.
    layouts: layouts::Layouts,
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
    root_tlabs: root_tlabs::RootTlabs,
    /// Runtime-owned slots a moving collection rewrites (counted per slot).
    runtime_root_slots: RuntimeRootSet,
    /// Subset of runtime slots whose owners lend a fixed address.
    pin_lease_slots: RuntimeRootSet,
    parked_stack_roots: Mutex<HashMap<u64, PublishedRoots>>,
    next_parked_stack: AtomicU64,
    coord: (
        crate::gc_telemetry::observations::GcMutex<GcCoord>,
        GcCondvar,
    ),
    /// Only cycle/participation transitions wake budget waiters. Mutator
    /// acknowledgements use coord.1, avoiding quadratic wakeup fan-out.
    budget_cv: GcCondvar,
    /// Lock-free fast-path mirror of `GcCoord::stop_requested`.
    minor_pending: std::sync::atomic::AtomicBool,
    stop_requested: std::sync::atomic::AtomicBool,
    poll_requested: std::sync::atomic::AtomicBool,
    trace_registry: Mutex<HashMap<u32, TraceFn>>,
    concurrent_trace_registry: Mutex<IdMap<u32, ConcurrentTraceFn>>,
    concurrent_slice_registry: Mutex<IdMap<u32, ConcurrentTraceSliceFn>>,
    drop_registry: Mutex<HashMap<u32, DropFn>>,
    minor_registry_cache: Mutex<MinorRegistryCache>,
    /// Advances only when registered hooks are invalidated. Runtime container
    /// types use this to cache per-generation registration without taking the
    /// registry mutex on every allocation.
    registry_generation: std::sync::atomic::AtomicU64,
    native_registrations: NativeGcRegistrations,
    path_observation: RuntimePathObservation,
    marker_observation: mark_workers::RuntimeMarkerObservation,
    old_lab_policy: old_lab::RuntimeAllocationPolicy,
    handshake_policy: root_handshake::RuntimeHandshakePolicy,
    sweep_observation: sweep::RuntimeSweepObservation,
    remark_observation: old_cycle::RuntimeBudgetObservation,
    wait_observation: RuntimeWaitObservation,
}

impl Default for GcRuntime {
    fn default() -> Self {
        Self::with_phase(MarkPhase::Owned(std::sync::atomic::AtomicU8::new(0)))
    }
}

impl GcRuntime {
    fn with_phase(mark_phase: MarkPhase) -> Self {
        // The default runtime keeps both exported ABI gates; every other
        // runtime owns both, so its collections affect only its own caches.
        let card_epoch = match mark_phase {
            MarkPhase::DefaultAbi => remembered::CardEpoch::DefaultAbi,
            MarkPhase::Owned(_) => remembered::CardEpoch::default(),
        };
        let heap = GcState {
            remembered_set: remembered::RememberedSet::with_epoch(card_epoch.clone()),
            ..GcState::default()
        };
        let storage_allocator = heap.storage_allocator.share();
        Self {
            marker_workers: Mutex::new(None),
            coordinator: Mutex::new(None),
            active_sweep: sweep::ActiveSweep::default(),
            collection_busy: std::sync::atomic::AtomicBool::new(false),
            headroom: AllocationHeadroom::default(),
            policy_identity: next_policy_identity(),
            policy_epoch: AtomicU64::new(1),
            mutator_epoch: AtomicU64::new(1),
            assist_cycle_generation: AtomicU64::new(0),
            mark_phase,
            card_epoch,
            native_accounting: NativeAccounting::default(),
            native_observation: crate::observation::Native::new(),
            progress: Mutex::new(progress::Progress::default()),
            stop_diagnostics: Mutex::new(std::collections::VecDeque::new()),
            preparation_diagnostic: Mutex::new(old_cycle::PreparationDiagnostic::default()),
            storage_allocator,
            heap: crate::gc_telemetry::observations::GcMutex::new(heap),
            layouts: layouts::Layouts::default(),
            write_barrier_calls: AtomicU64::new(0),
            tlab_ever_allocated: std::sync::atomic::AtomicBool::new(false),
            collect_lock: Mutex::new(()),
            root_stack_owner: Mutex::new(None),
            skipped_foreign_owner_collections: std::sync::atomic::AtomicU64::new(0),
            runtime_roots: RuntimeRootSet::default(),
            root_tlabs: root_tlabs::RootTlabs::default(),
            runtime_root_slots: RuntimeRootSet::default(),
            pin_lease_slots: RuntimeRootSet::default(),
            parked_stack_roots: Mutex::new(HashMap::new()),
            next_parked_stack: AtomicU64::new(1),
            coord: (
                crate::gc_telemetry::observations::GcMutex::coordination(GcCoord::default()),
                GcCondvar::new(),
            ),
            budget_cv: GcCondvar::new(),
            minor_pending: std::sync::atomic::AtomicBool::new(false),
            stop_requested: std::sync::atomic::AtomicBool::new(false),
            poll_requested: std::sync::atomic::AtomicBool::new(false),
            trace_registry: Mutex::new(HashMap::new()),
            concurrent_trace_registry: Mutex::new(IdMap::default()),
            concurrent_slice_registry: Mutex::new(IdMap::default()),
            drop_registry: Mutex::new(HashMap::new()),
            minor_registry_cache: Mutex::new(MinorRegistryCache::default()),
            registry_generation: std::sync::atomic::AtomicU64::new(1),
            native_registrations: NativeGcRegistrations::new(),
            path_observation: RuntimePathObservation::default(),
            marker_observation: mark_workers::RuntimeMarkerObservation::default(),
            old_lab_policy: old_lab::RuntimeAllocationPolicy::default(),
            handshake_policy: root_handshake::RuntimeHandshakePolicy::default(),
            sweep_observation: sweep::RuntimeSweepObservation::default(),
            remark_observation: old_cycle::RuntimeBudgetObservation::default(),
            wait_observation: RuntimeWaitObservation::default(),
        }
    }
}

enum MarkPhase {
    Owned(std::sync::atomic::AtomicU8),
    DefaultAbi,
}

pub(crate) use remembered::default_card_epoch;

/// The process-default runtime's phase gate: the exported ABI data symbol.
#[inline]
pub(crate) const fn default_mark_phase() -> &'static std::sync::atomic::AtomicU8 {
    &GC_MARK_PHASE
}

impl GcRuntime {
    /// Whether this collector belongs to the runtime the calling thread
    /// serves, so its thread-local mutator caches may be used.
    #[inline]
    pub(crate) fn serves_current_thread(&self) -> bool {
        std::ptr::eq(self.mark_phase(), crate::runtime::current().mark_phase())
    }

    pub(crate) fn mark_phase(&self) -> &std::sync::atomic::AtomicU8 {
        match &self.mark_phase {
            MarkPhase::Owned(phase) => phase,
            MarkPhase::DefaultAbi => &GC_MARK_PHASE,
        }
    }

    #[inline]
    pub(crate) fn card_epoch(&self) -> &AtomicU64 {
        self.card_epoch.cell()
    }

    pub(crate) fn initialize_default() -> Self {
        Self::with_phase(MarkPhase::DefaultAbi)
    }

    pub(crate) fn initialize() -> Self {
        Self::default()
    }
}

impl Drop for GcRuntime {
    fn drop(&mut self) {
        // Background jobs retain their runtime until all heap readers leave.
        // At final ownership release only idle workers can remain.
        if let Some(mut pool) = self
            .marker_workers
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            pool.stop(std::time::Duration::from_secs(2));
        }
        self.coordinator
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        old_lab::forget_private_owner(self.policy_identity);
        let mut state = self.heap.lock().unwrap_or_else(|p| p.into_inner());
        let drops = self.drop_registry.lock().unwrap_or_else(|p| p.into_inner());
        let finalize = |object: HeapObject| {
            if !object.allocated() || object.forwarded_to().is_some() {
                return;
            }
            if let Some(drop_fn) = drops.get(&object.type_id()) {
                // Native destructors are leaf operations; this owner is
                // quiescent and still retains the payload and its descriptor.
                unsafe {
                    run_drop_hook(
                        self,
                        *drop_fn,
                        object.payload().as_ptr(),
                        crate::observation::Value::new(|| object.type_id()),
                    )
                };
            }
            object.reclaim_in_place();
        };
        old_lab::finalize_unindexed(&state, finalize);
        for region in &state.old_regions {
            for &offset in region.allocations.keys() {
                finalize(HeapObject::from_raw(unsafe { region.base.add(offset) }.cast()).unwrap());
            }
        }
        for chunk in state.tlab_chunks.drain(..) {
            // Start bits cover active chunks too, without reading a mutator's
            // expired TLS cursor or constructing an index during teardown.
            if chunk.header_offsets.is_empty() {
                chunk.mark_bitmap.bits.for_each_set(|index| {
                    finalize(
                        HeapObject::from_raw(
                            unsafe { chunk.base.add(index * GC_REGION_MARK_GRANULE) }.cast(),
                        )
                        .unwrap(),
                    );
                });
            } else {
                for &offset in &chunk.header_offsets {
                    finalize(
                        HeapObject::from_raw(unsafe { chunk.base.add(usize::from(offset)) }.cast())
                            .unwrap(),
                    );
                }
            }
            let layout =
                Layout::from_size_align(chunk.capacity, std::mem::align_of::<GcHeader>()).unwrap();
            unsafe { dealloc(chunk.base, layout) };
        }
        // Unused survivor reserves contain no initialized payloads.
        for chunk in state.survivor_reserve.drain(..) {
            let layout =
                Layout::from_size_align(chunk.capacity, std::mem::align_of::<GcHeader>()).unwrap();
            unsafe { dealloc(chunk.base, layout) };
        }
    }
}

fn runtime() -> &'static GcRuntime {
    &crate::runtime::current().gc
}

/// Model the generated bump path in tests, including its static descriptor
/// lifetime. Runtime slow-path interning is intentionally a different contract.
#[cfg(test)]
fn initialize_generated_object_at_for_test(
    raw: *mut u8,
    size: usize,
    type_id: u32,
    layout_id: u64,
    gc_ref_mask: u64,
) -> Option<HeapObject> {
    static DESCRIPTORS: std::sync::LazyLock<
        Mutex<HashMap<willow_abi::GcLayoutDescriptor, &'static willow_abi::GcLayoutDescriptor>>,
    > = std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    let shape = willow_abi::GcLayoutDescriptor {
        size: size as u64,
        type_id: u64::from(type_id),
        layout_id,
        gc_ref_mask,
    };
    let descriptor = *DESCRIPTORS
        .lock()
        .unwrap()
        .entry(shape)
        .or_insert_with(|| Box::leak(Box::new(shape)));
    let object = HeapObject::from_raw(raw.cast())?;
    unsafe {
        object.as_ptr().write(GcHeader {
            marked: false,
            allocated: true,
            generation: GC_GENERATION_YOUNG,
            age: 0,
            remembered: 0,
            descriptor_owned: false,
            descriptor: descriptor as *const _ as usize,
        });
    }
    Some(object)
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

/// Reservation occupancy can remain high even when few objects are live. A
/// level-triggered check would repeatedly scan the same rooted region. Keep a
/// pressure latch across cycles; normal occupancy/pacer triggers and reservation
/// admission still operate independently while the latch is set.
fn hard_reservation_pressure(state: &mut GcState) -> bool {
    hard_reservation_pressure_in(runtime(), state)
}

fn hard_reservation_pressure_in(owner: &GcRuntime, state: &mut GcState) -> bool {
    let Some(limit) = state.memory_limit_bytes else {
        state.hard_pressure_triggered = false;
        return false;
    };
    let reserved = state
        .old_reserved_bytes
        .saturating_add(state.tlab_reserved_bytes)
        .saturating_add(owner.native_accounting.bytes.load(Ordering::Relaxed));
    if reserved <= limit / 2 {
        state.hard_pressure_triggered = false;
    }
    if reserved >= (limit / 4).saturating_mul(3).max(1) && !state.hard_pressure_triggered {
        state.hard_pressure_triggered = true;
        return true;
    }
    false
}

/// Mirror of `concurrent_cycle.is_some() || sweeping.is_some()`, written under
/// the heap lock by every change to either, so allocation can skip a policy
/// check that would only report "busy" without contending on the heap lock. A stale read only
/// shifts the check to the next slow path.
fn publish_collection_busy(state: &GcState) {
    publish_collection_busy_in(runtime(), state);
}

fn publish_collection_busy_in(owner: &GcRuntime, state: &GcState) {
    owner.collection_busy.store(
        state.concurrent_cycle.is_some() || state.sweeping.is_some(),
        Ordering::Release,
    );
}

// TLAB refills already publish their own bytes; other live buffers may lag by
// at most their current chunk. Telemetry and non-TLAB callers retain full sync.
fn allocation_should_collect_with_sync_in(owner: &crate::runtime::Runtime, sync: bool) -> bool {
    let stress = owner.diagnostics.stress_enabled("alloc");
    if !stress && owner.gc.collection_busy.load(Ordering::Acquire) {
        return false;
    }
    let mut state = owner.gc.heap.lock().unwrap();
    allocation_policy_due_in(owner, &mut state, sync, stress)
}

fn allocation_policy_due_in(
    owner: &crate::runtime::Runtime,
    state: &mut GcState,
    sync: bool,
    stress: bool,
) -> bool {
    if !stress && (state.concurrent_cycle.is_some() || state.sweeping.is_some()) {
        return false;
    }
    if sync {
        tlab::sync_tlab_bytes_in(&owner.gc, state);
    }
    let bytes = policy_allocated_total_in(&owner.gc, state);
    if state.pacer.sample_due(bytes) {
        state
            .pacer
            .allocation(crate::gc_telemetry::timestamp_ns(), bytes);
        pacer::sample_threshold_in(owner, state);
    }
    let hard_pressure = hard_reservation_pressure_in(&owner.gc, state);
    state.soft_memory.sample_process();
    let memory_input = memory_inputs_in(&owner.gc, state);
    let memory = state.soft_memory.decide(memory_input);
    // The pacer trigger lives in the major-policy domain even when a soft
    // limit makes the memory inputs count every young byte.
    let paced = state.pacer.enabled()
        && major_policy_managed_bytes(state)
            .saturating_add(owner.gc.native_accounting.bytes.load(Ordering::Relaxed))
            as u64
            >= state.pacer_trigger
        && memory.reason != memory_control::Reason::Relief;
    let collect = stress || hard_pressure || paced || memory.collect;
    if !collect {
        owner
            .gc
            .headroom
            .policy
            .store(policy_headroom_in(&owner.gc, state), Ordering::Release);
    }
    collect
}

fn minor_stress_enabled_in(owner: &crate::runtime::Runtime) -> bool {
    owner.diagnostics.stress_enabled("minor") || owner.diagnostics.stress_enabled("relocate")
}

fn allocation_should_minor_collect_in(owner: &crate::runtime::Runtime) -> bool {
    let stress = minor_stress_enabled_in(owner);
    let state = owner.gc.heap.lock().unwrap();
    stress || minor_allocation_due(&state)
}

/// Debt retries remain enabled even when a major sweep lowers occupancy.
fn minor_allocation_due(state: &GcState) -> bool {
    if state.minor_deferral_start_bytes.is_some() {
        state.memory_limit_bytes.is_some()
            || state.young_allocation_clock >= state.minor_retry_bytes
    } else {
        state.young_allocated_bytes >= state.nursery_threshold_bytes
    }
}

fn minor_deferral_age(state: &GcState) -> u64 {
    state.minor_deferral_start_bytes.map_or(0, |start| {
        debug_assert!(
            state.young_allocation_clock >= start,
            "young allocation clock regressed"
        );
        state.young_allocation_clock - start
    })
}

/// A queued minor may use one fixed nursery budget before joining its owner.
/// Start the clock at the request, not at the eventual root scan: otherwise
/// repeated refills during a major sweep would have unbounded, uncharged debt.
fn minor_refill_should_wait(state: &mut GcState) -> bool {
    minor_refill_should_wait_in(runtime(), state)
}

fn minor_refill_should_wait_in(owner: &GcRuntime, state: &mut GcState) -> bool {
    if !minor_allocation_due(state) {
        return false;
    }
    if state.minor_deferral_start_bytes.is_none()
        && minor::requested(owner)
        && state.memory_limit_bytes.is_none()
        // Do not grant an unassessed refill whose full runway exceeds the budget.
        && state.nursery_threshold_bytes >= GC_TLAB_CHUNK_SIZE
        && state.young_allocation_clock != u64::MAX
    {
        state.minor_deferral_start_bytes = Some(state.young_allocation_clock);
        state.minor_deferral_budget_bytes = state.nursery_threshold_bytes as u64;
        state.minor_retry_bytes = state
            .young_allocation_clock
            .saturating_add(GC_TLAB_CHUNK_SIZE as u64);
    }
    state.minor_deferral_start_bytes.is_none()
        || state.young_allocation_clock == u64::MAX
        || minor_deferral_age(state) >= state.minor_deferral_budget_bytes
}

// ---------------------------------------------------------------------------
// Public runtime API
// ---------------------------------------------------------------------------

/// Set the soft memory target in bytes (managed pressure and best-effort
/// process RSS); zero disables it. Returns the previous limit. Collection is considered at the next allocation slow path;
/// this setter never parks mutators or changes the legacy hard reservation cap.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_set_memory_limit(bytes: u64) -> u64 {
    let mut state = runtime().heap.lock().unwrap();
    let previous = state.soft_memory.set_limit(bytes);
    state.soft_memory.sample_process();
    evict_region_cache_for(&mut state, 0);
    drop(state);
    invalidate_runtime_policy_budgets();
    previous
}

/// Initialize the GC runtime.
///
/// Production code calls this once at process startup, before any allocation.
/// Calling it again resets the single process-global heap and invalidates
/// existing GC pointers, so it is not a general-purpose runtime reset API.
/// Unit tests may intentionally reset the heap of the runtime their thread
/// serves; `isolated_runtime()` gives each test thread its own runtime.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_init() {
    reset_internal();
    // Generated array fast paths can allocate a reference buffer before any
    // runtime array call. Re-register after every heap/registry reset.
    crate::array::ensure_trace_registered();
    crate::runtime::current()
        .diagnostic_ready
        .store(true, Ordering::Release);
}

/// Allocate a GC-managed object of `payload_size` bytes with the given
/// `type_id`.  Returns a pointer to the **payload** (past the header), or
/// null on allocation failure.
///
/// This function may trigger a collection if the heap threshold is exceeded.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_alloc_object(type_id: i64, payload_size: i64) -> *mut u8 {
    allocate_object(0, type_id as u32, payload_size, 0)
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_alloc_typed(payload_size: i64, gc_ref_mask: u64) -> *mut u8 {
    allocate_object(0, 0, payload_size, gc_ref_mask)
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
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
/// `layout_id` is the address-independent fingerprint computed by the compiler.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_alloc_bitmap(
    layout_id: i64,
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
    allocate_object(
        layout_id as u64,
        willow_abi::GC_BITMAP_TYPE_ID,
        payload_size,
        descriptor as u64,
    )
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
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
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_alloc_slow(
    tlab_state: *mut GcTlabState,
    layout_id: u64,
    type_id: i64,
    payload_size: i64,
    gc_ref_mask: u64,
) -> *mut u8 {
    let _relocation = RelocationSafeScope::enter();
    if tlab_state.is_null() || payload_size < 0 {
        crate::failure::fatal_invariant("invalid TLAB allocation arguments");
    }
    tlab_alloc_slow_in(
        crate::runtime::current(),
        tlab_state as usize,
        layout_id,
        type_id,
        payload_size,
        gc_ref_mask,
    )
}

/// Shared TLAB miss handling for generated code and the runtime's own TLAB
/// ([`runtime_tlab`]). The caller decides whether this thread's root slots are
/// relocatable for any collection started here.
fn tlab_alloc_slow(
    state_address: usize,
    layout_id: u64,
    type_id: i64,
    payload_size: i64,
    gc_ref_mask: u64,
) -> *mut u8 {
    tlab_alloc_slow_in(
        crate::runtime::current(),
        state_address,
        layout_id,
        type_id,
        payload_size,
        gc_ref_mask,
    )
}

fn tlab_alloc_slow_in(
    owner: &crate::runtime::Runtime,
    state_address: usize,
    layout_id: u64,
    type_id: i64,
    payload_size: i64,
    gc_ref_mask: u64,
) -> *mut u8 {
    let Some(total_size) = (GC_HEADER_SIZE)
        .checked_add(payload_size as usize)
        .and_then(|size| size.checked_next_multiple_of(std::mem::align_of::<GcHeader>()))
    else {
        crate::failure::resource_exhausted(format_args!("TLAB allocation size overflow"));
    };
    let stress = owner.diagnostics.stress_enabled("alloc");
    let small = total_size <= GC_TLAB_MAX_OBJECT_SIZE;
    let (fast_bytes, minor_due, cached, major_due, storage_allocator) = {
        let mut state = owner.gc.heap.lock().unwrap();
        tlab::register_tlab_state_in(&owner.gc, &mut state, state_address);
        let record = state.tlab_states.get_mut(&state_address).unwrap();
        let fast = fast_bytes_total(record);
        let fast_bytes = fast.saturating_sub(record.assist_observed_fast_bytes);
        record.assist_observed_fast_bytes = fast;
        tlab::sync_tlab_record_bytes_in(&owner.gc, &mut state, state_address);
        if small {
            // A small-object miss means the active chunk has insufficient tail
            // space. Seal it before collection/refill.
            tlab::retire_tlab_locked_in(&owner.telemetry.footprint, &mut state, state_address);
        }
        // The refilling record is current; other owners publish at their own
        // refill, bounding unobserved nursery bytes by one chunk per TLAB.
        let cached = if small && !stress {
            state.region_cache.take_survivor()
        } else {
            None
        };
        let minor_due = minor_allocation_due(&state);
        let major_due = if !stress && !minor_due && !minor_stress_enabled_in(owner) {
            Some(allocation_policy_due_in(owner, &mut state, false, false))
        } else {
            None
        };
        (
            fast_bytes,
            minor_due,
            cached,
            major_due,
            state.storage_allocator.share(),
        )
    };
    let cached_storage = cached.map(tlab::ChunkStorage::from_cached);
    sweep::assist_in(owner);
    marking::assist_concurrent_mark_in(owner, fast_bytes.saturating_add(total_size as u64));

    if !stress
        && (minor_due || minor_stress_enabled_in(owner))
        && allocation_should_minor_collect_in(owner)
    {
        minor::minor_collect_in(owner);
        // A busy owner may only have queued the request. Account for its debt
        // immediately; join only when no bounded refill runway remains.
        while small
            && {
                let mut state = owner.gc.heap.lock().unwrap();
                minor_refill_should_wait_in(&owner.gc, &mut state)
            }
            && cycle_wait::wait_for_minor_budget_in(owner)
        {
            // The owner can satisfy the request before publishing completion.
            // Recheck rather than start another minor for every waiting refill.
            if allocation_should_minor_collect_in(owner) {
                minor::minor_collect_in(owner);
            }
        }
    }

    if stress {
        let _ = try_collect_internal_in(owner);
    } else if major_due.unwrap_or_else(|| allocation_should_collect_with_sync_in(owner, false)) {
        automatic_collect_in(owner, false);
    }

    if stress || !small {
        return allocate_old_in(owner, layout_id, type_id as u32, payload_size, gc_ref_mask);
    }

    let mut storage = cached_storage.or_else(|| tlab::ChunkStorage::allocate(&storage_allocator));
    let mut state = owner.gc.heap.lock().unwrap();
    tlab::register_tlab_state_in(&owner.gc, &mut state, state_address);
    let mut base = tlab::install_tlab_chunk(owner, &mut state, state_address, &mut storage);
    for _ in 0..BUDGET_COLLECTION_RETRIES {
        if base.is_some() || (state.memory_limit_bytes.is_none() && !state.soft_memory.enabled()) {
            break;
        }
        let released = state.released_bytes;
        drop(state);
        collect_for_budget_in(owner);
        if storage.is_none() {
            storage = tlab::ChunkStorage::allocate(&storage_allocator);
        }
        state = owner.gc.heap.lock().unwrap();
        tlab::register_tlab_state_in(&owner.gc, &mut state, state_address);
        base = tlab::install_tlab_chunk(owner, &mut state, state_address, &mut storage);
        if state.released_bytes == released {
            break;
        }
    }
    let Some(base) = base else {
        return allocation_failure_in(&owner.gc, &state);
    };
    let Some(header) =
        initialize_object_at(base, total_size, type_id as u32, layout_id, gc_ref_mask)
    else {
        crate::failure::fatal_invariant("fresh TLAB could not initialize a validated object");
    };
    state.tlab_chunks.last_mut().unwrap().mark_bitmap.mark(0);
    // SAFETY: the generated TLS block is aligned and remains alive for this
    // thread. Publish limit before cursor; generated code resumes only after
    // this slow-path call returns.
    let tls = unsafe { tlab_state_at(state_address) };
    // Relocation stress closes the TLAB after one object, so every generated
    // allocation returns here and runs a moving minor collection first.
    let limit = if owner.diagnostics.stress_enabled("relocate") {
        total_size
    } else {
        GC_TLAB_CHUNK_SIZE
    };
    tls.limit.store(base as usize + limit, Ordering::Release);
    tls.cursor
        .store(base as usize + total_size, Ordering::Release);
    // Fast-path bytes in this chunk are measured from here.
    state
        .tlab_states
        .get_mut(&state_address)
        .expect("TLAB state is registered before refill")
        .chunk_fast_start = base as usize + total_size;
    owner.gc.headroom.consume(total_size);
    state.allocated_bytes = state.allocated_bytes.saturating_add(total_size);
    state.young_allocated_bytes = state.young_allocated_bytes.saturating_add(total_size);
    state.young_allocation_clock = state
        .young_allocation_clock
        .saturating_add(total_size as u64);
    state.total_allocs = state.total_allocs.saturating_add(1);
    state.total_allocated_bytes = state
        .total_allocated_bytes
        .saturating_add(total_size as u64);
    state.tlab_slow_allocations = state.tlab_slow_allocations.saturating_add(1);
    header.payload().as_ptr()
}

fn chunk_used_bytes(state: &GcState, chunk: &BumpChunk) -> usize {
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
    chunk: &BumpChunk,
    address: usize,
    interior: bool,
) -> Option<HeapObject> {
    let relative = address.checked_sub(chunk.base as usize + GC_HEADER_SIZE)?;
    let offset = if chunk.needs_index() {
        // Not indexed yet: its start bits name exactly its
        // published headers. O(chunk words) for an interior lookup.
        if interior {
            chunk.mark_bitmap.last_start_in(0, relative)?
        } else {
            (relative < chunk.used
                && relative.is_multiple_of(GC_REGION_MARK_GRANULE)
                && chunk.mark_bitmap.is_marked(relative))
            .then_some(relative)?
        }
    } else {
        indexed_header_offset(chunk, relative, interior)?
    };
    // SAFETY: a validated header index or a published start bit names a
    // header; chunk storage cannot be released while the caller holds the
    // heap mutex.
    let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast())?;
    if !object.allocated() {
        return None;
    }
    let payload = object.payload().as_ptr() as usize;
    ((!interior && payload == address)
        || (interior && address >= payload && address < object.as_ptr() as usize + object.size()))
    .then_some(object)
}

fn indexed_header_offset(chunk: &BumpChunk, relative: usize, interior: bool) -> Option<usize> {
    let index = if interior {
        chunk
            .header_offsets
            .partition_point(|&offset| {
                chunk.lookup_comparisons.observe(|count| {
                    count.fetch_add(1, Ordering::Relaxed);
                });
                usize::from(offset) <= relative
            })
            .checked_sub(1)?
    } else {
        chunk
            .header_offsets
            .binary_search_by(|offset| {
                chunk.lookup_comparisons.observe(|count| {
                    count.fetch_add(1, Ordering::Relaxed);
                });
                usize::from(*offset).cmp(&relative)
            })
            .ok()?
    };
    Some(usize::from(chunk.header_offsets[index]))
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

fn find_tlab_chunk(state: &GcState, address: usize) -> Option<&BumpChunk> {
    let chunk = &state.tlab_chunks[state
        .tlab_addresses
        .candidate(address.checked_sub(GC_HEADER_SIZE)?)?];
    let start = chunk.base as usize;
    let end = start.saturating_add(chunk_used_bytes(state, chunk));
    (address >= start + GC_HEADER_SIZE && address <= end).then_some(chunk)
}

fn allocate_object(layout_id: u64, type_id: u32, payload_size: i64, gc_ref_mask: u64) -> *mut u8 {
    if payload_size < 0 {
        crate::failure::fatal_invariant("negative managed allocation size");
    }
    let total_size = (payload_size as u64).saturating_add(GC_HEADER_SIZE as u64);
    let owner = crate::runtime::current();
    sweep::assist_in(owner);
    assist_concurrent_mark_in(owner, total_size);
    let stress = owner.diagnostics.stress_enabled("alloc");
    // A due check charges the chunk it covers against the lock-free headroom
    // first; only an exhausted headroom consults the policy under the lock.
    if (stress
        || (runtime_policy_check_due_in(&owner.gc, total_size)
            && !consume_policy_headroom(&owner.gc, GC_TLAB_CHUNK_SIZE as u64 + total_size)))
        && allocation_should_collect_with_sync_in(owner, true)
    {
        automatic_collect(stress);
    }
    allocate_old(layout_id, type_id, payload_size, gc_ref_mask)
}

// Process-wide identity allocation prevents a reused owner address or equal
// epochs from accepting another collector's cached allocation budget.
static NEXT_POLICY_IDENTITY: AtomicU64 = AtomicU64::new(1);

fn next_policy_identity() -> u64 {
    NEXT_POLICY_IDENTITY
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .expect("collector policy identities exhausted")
}

std::thread_local! {
    /// Last collector identity, epoch and bytes since the policy check.
    /// This cache contains no owner pointer and cannot prolong its lifetime.
    static RUNTIME_POLICY_DEBT: std::cell::Cell<(u64, u64, u64)> = const { std::cell::Cell::new((0, 0, 0)) };
}

fn invalidate_runtime_policy_budgets() {
    invalidate_runtime_policy_budgets_in(runtime());
}

fn invalidate_runtime_policy_budgets_in(owner: &GcRuntime) {
    owner.policy_epoch.fetch_add(1, Ordering::AcqRel);
    owner.headroom.invalidate();
}

/// Record growth of reserved heap storage. Under a hard reservation cap the
/// pressure trigger depends on reservations rather than allocated bytes, so a
/// reservation re-arms every thread's next policy check and the trigger fires
/// at the same allocation as without the per-thread budget. Reservations grow
/// once per region or chunk, so this costs one check per thread per growth.
#[cfg(test)]
fn note_reservation_growth(state: &GcState) {
    note_reservation_growth_in(crate::runtime::current(), state);
}

fn note_reservation_growth_in(owner: &crate::runtime::Runtime, state: &GcState) {
    owner.telemetry.footprint.reservation(
        (state.old_reserved_bytes as u64).saturating_add(state.tlab_reserved_bytes as u64),
    );
    if state.memory_limit_bytes.is_some() {
        invalidate_runtime_policy_budgets_in(&owner.gc);
    }
}

/// Whether this runtime allocation of `bytes` should consult the collection
/// policy. Generated code decides only at TLAB refills, once per
/// `GC_TLAB_CHUNK_SIZE` bytes per thread; runtime containers use the same
/// per-thread granularity instead of locking the heap for every string or
/// container allocation. A thread's first allocation, and the first after a
/// policy-input change or hard-capped reservation growth, always checks.
/// Reservation limits stay exact because `allocate_old` enforces them under
/// the heap lock; pacing and soft-limit triggers may fire up to one chunk of
/// runtime allocation per thread later, as for generated code.
fn runtime_policy_check_due_in(owner: &GcRuntime, bytes: u64) -> bool {
    let epoch = owner.policy_epoch.load(Ordering::Acquire);
    RUNTIME_POLICY_DEBT.with(|debt| {
        let (identity, seen, owed) = debt.get();
        let owed = if identity == owner.policy_identity && seen == epoch {
            owed.saturating_add(bytes)
        } else {
            u64::MAX
        };
        let due = owed >= GC_TLAB_CHUNK_SIZE as u64;
        debt.set((owner.policy_identity, epoch, if due { 0 } else { owed }));
        due
    })
}

/// Add a newly reserved regular region if it still has usable space.
fn index_old_region(state: &mut GcState, index: usize) {
    let region = &state.old_regions[index];
    if region.kind == RegionKind::Old && !region.sweep_quarantined {
        let available = region.available_span();
        if available >= GC_HEADER_SIZE {
            state.old_region_candidates.insert((available, index));
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
    allocate_old_region_object_locked_in(
        crate::runtime::current(),
        state,
        layout_id,
        type_id,
        payload_size,
        gc_ref_mask,
        count_logical_allocation,
    )
}

fn allocate_old_region_object_locked_in(
    owner: &crate::runtime::Runtime,
    state: &mut GcState,
    layout_id: u64,
    type_id: u32,
    payload_size: usize,
    gc_ref_mask: u64,
    count_logical_allocation: bool,
) -> Option<HeapObject> {
    allocate_old_region_object_for_move_in(
        owner,
        state,
        raw_heap::TraceMetadata {
            layout_id,
            type_id,
            payload_size,
            gc_ref_mask,
        },
        count_logical_allocation,
        None,
    )
}

fn allocate_old_region_object_for_move_in(
    owner: &crate::runtime::Runtime,
    state: &mut GcState,
    metadata: raw_heap::TraceMetadata,
    count_logical_allocation: bool,
    source: Option<HeapObject>,
) -> Option<HeapObject> {
    let payload_size = metadata.payload_size;
    let total_size = GC_HEADER_SIZE.checked_add(payload_size)?;
    let span_size = total_size.checked_next_multiple_of(GC_REGION_MARK_GRANULE)?;
    let large = total_size > GC_LARGE_OBJECT_THRESHOLD;

    while state
        .old_region_candidates
        .last()
        .is_some_and(|&(_, index)| state.old_regions[index].sweep_quarantined)
    {
        // Each stale candidate is discarded at most once during the sweep.
        state.old_region_candidates.pop_last();
    }
    let candidate = state
        .old_region_candidates
        .last()
        .copied()
        .filter(|(available, _)| !large && *available >= span_size);
    let (object, reused) = if let Some((_, index)) = candidate {
        let allocated = state.old_regions[index].allocate_object_for_move_buffered(
            owner.gc.mark_phase(),
            metadata,
            source,
            None,
        )?;
        let available = state.old_regions[index].available_span();
        state.old_region_candidates.pop_last();
        if available >= GC_HEADER_SIZE {
            state.old_region_candidates.insert((available, index));
        }
        allocated
    } else {
        let capacity = if large { span_size } else { GC_OLD_REGION_SIZE };
        let kind = if large {
            RegionKind::LargeObject
        } else {
            RegionKind::Old
        };
        let mut region = reserve_old_region_in(&owner.gc, state, kind, capacity)?;
        let allocated = region.allocate_object_for_move_buffered(
            owner.gc.mark_phase(),
            metadata,
            source,
            None,
        )?;
        state
            .old_addresses
            .insert(region.base as usize, state.old_regions.len());
        state.old_regions.push(region);
        state.old_reserved_bytes += capacity;
        note_reservation_growth_in(owner, state);
        index_old_region(state, state.old_regions.len() - 1);
        allocated
    };

    account_old_region_allocation_in(
        &owner.gc,
        state,
        object.size(),
        reused,
        count_logical_allocation,
    );
    Some(object)
}

#[inline]
fn account_old_region_allocation(
    state: &mut GcState,
    size: usize,
    reused: bool,
    count_logical_allocation: bool,
) {
    account_old_region_allocation_in(runtime(), state, size, reused, count_logical_allocation);
}

#[inline]
fn account_old_region_allocation_in(
    owner: &GcRuntime,
    state: &mut GcState,
    size: usize,
    reused: bool,
    count_logical_allocation: bool,
) {
    owner.headroom.consume(size);
    state.allocated_bytes = state.allocated_bytes.saturating_add(size);
    state.old_region_allocations = state.old_region_allocations.saturating_add(1);
    if reused {
        state.old_region_reuses = state.old_region_reuses.saturating_add(1);
    }
    if count_logical_allocation {
        state.total_allocs = state.total_allocs.saturating_add(1);
        state.total_allocated_bytes = state.total_allocated_bytes.saturating_add(size as u64);
    }
}

fn allocate_old(layout_id: u64, type_id: u32, payload_size: i64, gc_ref_mask: u64) -> *mut u8 {
    allocate_old_in(
        crate::runtime::current(),
        layout_id,
        type_id,
        payload_size,
        gc_ref_mask,
    )
}

fn allocate_old_in(
    owner: &crate::runtime::Runtime,
    layout_id: u64,
    type_id: u32,
    payload_size: i64,
    gc_ref_mask: u64,
) -> *mut u8 {
    RuntimePathObservation::old_allocation(|| &owner.gc.path_observation);
    if payload_size < 0 {
        crate::failure::fatal_invariant("negative managed allocation size");
    }
    let payload_size = payload_size as usize;
    // Large runtime allocations can consume an arbitrary number of bytes
    // between generated loop polls. Service the initial handshake too, when
    // no mark epoch exists yet and mark assists therefore return immediately.
    if payload_size > GC_TLAB_MAX_OBJECT_SIZE.saturating_sub(GC_HEADER_SIZE)
        && !cycle_wait::is_collector()
    {
        safepoint::safepoint_in(owner);
    }
    if old_lab::enabled_in(owner)
        && let Some((total, span)) = old_lab::span_for(payload_size)
        && total <= GC_TLAB_MAX_OBJECT_SIZE
        && old_lab::available_in(owner)
    {
        // Small runtime objects bump-allocate in this thread's old buffer and
        // take the heap mutex only to refill it.
        if let Some(object) =
            old_lab::try_allocate_in(owner, type_id, layout_id, gc_ref_mask, total, span)
        {
            return object.payload().as_ptr();
        }
        let mut state = owner.gc.heap.lock().unwrap();
        let refill = |state: &mut GcState| {
            old_lab::refill_and_allocate_in(
                owner,
                state,
                type_id,
                layout_id,
                gc_ref_mask,
                total,
                span,
            )
        };
        let mut object = refill(&mut state);
        for _ in 0..BUDGET_COLLECTION_RETRIES {
            if object.is_some()
                || (state.memory_limit_bytes.is_none() && !state.soft_memory.enabled())
            {
                break;
            }
            let released = state.released_bytes;
            drop(state);
            collect_for_budget_in(owner);
            state = owner.gc.heap.lock().unwrap();
            object = refill(&mut state);
            if state.released_bytes == released {
                break;
            }
        }
        let Some(object) = object else {
            return allocation_failure_in(&owner.gc, &state);
        };
        return object.payload().as_ptr();
    }
    // TLAB byte merging is O(TLABs); the policy checks and TLAB slow path that
    // read those bytes merge them, so the per-object path does not.
    let mut state = owner.gc.heap.lock().unwrap();
    // A coordinator request can still be queued before collector election.
    // Large allocations must not turn OS scheduling latency into unbounded
    // heap growth. Request/join initial publication once the fixed request
    // runway is spent, then use Retry-aware assists: marking may depend on
    // a lock held by this allocator even if no epoch exists yet.
    if state.concurrent_cycle.is_none()
        && state.sweeping.is_none()
        && major_policy_managed_bytes(&state)
            > state
                .threshold_bytes
                .saturating_add(sweep::ALLOCATION_RUNWAY as usize)
        && !cycle_wait::is_collector()
    {
        drop(state);
        cycle_wait::start_for_allocation_in(owner);
        state = owner.gc.heap.lock().unwrap();
    }
    // New objects are outside the sweep snapshot. Bound this floating garbage
    // rather than feeding unlimited sweep-time growth into the next live goal.
    // Check under the allocation lock, so concurrent allocators share one
    // budget and overshoot by at most one admitted object.
    while sweep::allocation_budget_exhausted(&state) {
        drop(state);
        if !cycle_wait::wait_for_sweep_budget_in(owner) {
            state = owner.gc.heap.lock().unwrap();
            break;
        }
        state = owner.gc.heap.lock().unwrap();
    }
    let mut header = allocate_old_region_object_locked_in(
        owner,
        &mut state,
        layout_id,
        type_id,
        payload_size,
        gc_ref_mask,
        true,
    );
    for _ in 0..BUDGET_COLLECTION_RETRIES {
        if header.is_some() || (state.memory_limit_bytes.is_none() && !state.soft_memory.enabled())
        {
            break;
        }
        let released = state.released_bytes;
        drop(state);
        collect_for_budget_in(owner);
        state = owner.gc.heap.lock().unwrap();
        header = allocate_old_region_object_locked_in(
            owner,
            &mut state,
            layout_id,
            type_id,
            payload_size,
            gc_ref_mask,
            true,
        );
        if state.released_bytes == released {
            break;
        }
    }
    let Some(header) = header else {
        return allocation_failure_in(&owner.gc, &state);
    };
    state.tlab_slow_allocations = state.tlab_slow_allocations.saturating_add(1);
    if header.size() > GC_TLAB_MAX_OBJECT_SIZE {
        state.tlab_large_allocations = state.tlab_large_allocations.saturating_add(1);
    }
    header.payload().as_ptr()
}

/// Trigger a full collection. Normal old marking/closure/sweep are concurrent;
/// legacy trace callbacks or worker failures use the stopped recovery path.
/// If another collector owns a cycle, join its completion instead of returning
/// early. A cycle whose root snapshot predates a root release can retain that
/// object until a subsequent cycle.
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
/// by a collection whose root snapshot is taken **after** the function has
/// returned and the root slots have been popped.
///
/// This is intentional and correct.  The GC cannot distinguish "I'm done with
/// this variable" from "I might use it again later in the same scope".  To
/// reclaim an object eagerly, arrange for it to go out of scope (return from
/// the function, or wrap the allocation in a smaller scope if block-scoped
/// roots are supported) before calling `gc_collect()`.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_collect() {
    let _reentry = safepoint::RegionReentry::enter();
    // An explicit collection must not depend on a later batch of task completions.
    // Active execution units still protect every covered retirement.
    crate::scheduler::release_quiescent_frame_roots(true);
    let _relocation = RelocationSafeScope::enter();
    // A caller that loses collector election must join the active cycle rather
    // than return while its marking/sweep is still in flight. Budget waiters
    // already publish/park at safepoints and handle collector reentrancy.
    cycle_wait::collect_explicit();
}

/// Trigger a stop-the-world minor collection. Young objects reachable from
/// root slots or heap slots copy to young survivor storage at age 1, then to
/// non-moving old storage at age 2, and every such slot is rewritten. The
/// collection is deferred while another thread is parked inside runtime code
/// with a young referent (see `minor_collect_with_roots`).
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_minor_collect() {
    let _reentry = safepoint::RegionReentry::enter();
    let _relocation = RelocationSafeScope::enter();
    minor_collect_internal();
}

/// Whether the GC stress mode `kind` is active via the `WILLOW_GC_STRESS`
/// environment variable. The variable is a comma-separated list of modes; `all`
/// enables every mode.
///
/// Modes (for local test runs / CI):
/// - `alloc`     — collect at every heap allocation boundary.
/// - `minor`     — force a minor collection at every TLAB refill.
/// - `relocate`  — limit each TLAB to one object, so every generated
///   allocation runs a moving minor collection. Moved payloads are poisoned,
///   exposing any holder that kept a pre-move address.
/// - `await`     — collect around await boundaries: before/after the scheduler
///   polls a task (so suspend/resume and task-completion are stressed).
/// - `scheduler` — collect around scheduler operations: spawn, wake, park,
///   completion, and channel-waiter registration.
/// - `all`       — enable all of the above.
///
/// Example: `WILLOW_GC_STRESS=alloc cargo test`, or `WILLOW_GC_STRESS=all`.
///
/// `WILLOW_GC_VERIFY_NO_PIN=1` complements these modes: a minor collection
/// that would pin a young object in place aborts and names the pin source. Generated code pins
/// only the owners of `&`/`&mut`
/// arguments that point into an object or array element.
///
/// The variable is read once: every slow-path allocation asks for it several
/// times, and `std::env::var` costs a `getenv` scan plus, on Windows, a heap
/// allocation for the key on each call.
pub(crate) fn gc_stress_enabled(kind: &str) -> bool {
    crate::runtime::current().diagnostics.stress_enabled(kind)
}

#[cfg(test)]
use crate::diagnostic_settings::parse_gc_stress_modes;

/// Default-runtime stress-mode override for tests that cannot restart the process
/// with a different `WILLOW_GC_STRESS`. `None` restores the environment value.
#[cfg(test)]
fn gc_stress_override() -> &'static Mutex<Option<Vec<String>>> {
    &crate::runtime::current().diagnostics.stress_override
}

#[cfg(test)]
pub(crate) fn set_gc_stress_for_test(modes: Option<&str>) {
    *gc_stress_override()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = modes.map(parse_gc_stress_modes);
}

/// Scoped override; callers must hold `isolated_runtime` until this drops.
#[cfg(test)]
struct GcStressTestScope(Option<Vec<String>>);

#[cfg(test)]
impl GcStressTestScope {
    fn with_modes(value: &str) -> Self {
        let mut modes = gc_stress_override()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        Self(modes.replace(parse_gc_stress_modes(value)))
    }
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
    stress_collect_in(crate::runtime::current(), kind);
}

pub(crate) fn stress_collect_in(owner: &crate::runtime::Runtime, kind: &str) {
    if owner.diagnostics.stress_enabled(kind) {
        let _ = try_collect_internal_in(owner);
    }
}

// ---------------------------------------------------------------------------
// Internal collection
// ---------------------------------------------------------------------------

/// Extend tracing beyond the inline mask. Bitmap descriptors are immutable
/// static data whose lifetime covers every object using them, including moves.
#[inline(always)]
fn bitmap_reference_slots(
    object: HeapObject,
    metadata: raw_heap::TraceMetadata,
) -> impl Iterator<Item = *mut *mut u8> {
    let descriptor = metadata.gc_ref_mask as *const u64;
    // SAFETY: only bitmap objects interpret the mask as an immutable,
    // process-lifetime compiler descriptor. Other types have no extra words.
    let count = if metadata.type_id == willow_abi::GC_BITMAP_TYPE_ID {
        unsafe { *descriptor as usize }
    } else {
        0
    };
    let payload_words = metadata.payload_size / GC_STORAGE_WORD_BYTES;
    (1..count).flat_map(move |word| {
        // SAFETY: word + 1 addresses a descriptor word below its declared count.
        let mut bits = unsafe { *descriptor.add(word + 1) };
        std::iter::from_fn(move || {
            while bits != 0 {
                let index = word * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                if index < payload_words {
                    return Some(object.payload_slot(index));
                }
            }
            None
        })
    })
}

#[inline(always)]
fn append_bitmap_slots(object: HeapObject, slots: &mut Vec<*mut *mut u8>) {
    slots.extend(bitmap_reference_slots(object, object.trace_metadata()));
}

fn object_reference_slots(
    object: HeapObject,
    trace_registry: &HashMap<u32, TraceFn, impl std::hash::BuildHasher>,
) -> Vec<*mut *mut u8> {
    let mut slots = Vec::new();
    append_object_reference_slots(object, trace_registry, &mut slots);
    slots
}

#[inline(always)]
fn inline_reference_slots(
    object: HeapObject,
    metadata: raw_heap::TraceMetadata,
) -> impl Iterator<Item = *mut *mut u8> {
    let words = (metadata.payload_size / GC_STORAGE_WORD_BYTES).min(64);
    let mut mask =
        metadata.inline_ref_mask() & u64::MAX.checked_shr((64 - words) as u32).unwrap_or(0);
    std::iter::from_fn(move || {
        if mask == 0 {
            return None;
        }
        let index = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        Some(object.payload_slot(index))
    })
}

#[inline(always)]
fn append_object_reference_slots(
    object: HeapObject,
    trace_registry: &HashMap<u32, TraceFn, impl std::hash::BuildHasher>,
    slots: &mut Vec<*mut *mut u8>,
) {
    let metadata = object.trace_metadata();
    slots.extend(inline_reference_slots(object, metadata));
    append_bitmap_slots(object, slots);
    if let Some(trace) = trace_registry.get(&metadata.type_id).copied() {
        // SAFETY: trace is registered for this runtime type and exposes mutable
        // reference slots without allocating GC objects.
        unsafe { trace(object.payload().as_ptr(), slots) };
    }
}

fn automatic_collect(stress: bool) {
    automatic_collect_in(crate::runtime::current(), stress);
}

fn automatic_collect_in(owner: &crate::runtime::Runtime, stress: bool) {
    if stress || !coordinator::request_in(owner) {
        let _ = try_collect_internal_in(owner);
    }
}

#[cfg(all(test, debug_assertions))]
pub(crate) fn collect_for_worker_panic_test() {
    collect_internal();
}

fn collect_internal() {
    let _ = try_collect_internal();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CollectionAttempt {
    Completed,
    ElectionLost,
    ForeignOwner,
}

/// Only Completed proves collection; both contention and unsafe foreign roots
/// defer the attempt without satisfying a request.
fn try_collect_internal() -> CollectionAttempt {
    try_collect_internal_in(crate::runtime::current())
}

fn try_collect_internal_in(owner: &crate::runtime::Runtime) -> CollectionAttempt {
    // Collector election: only one thread collects at a time.
    // A thread that cannot become the collector must NOT block on runtime().collect_lock —
    // if a stop-the-world collection is in progress, the holder is waiting for
    // this thread to reach a safepoint, so blocking here would deadlock. Instead
    // reach a safepoint (parking if a STW is pending) and let the active
    // collector proceed.
    if owner
        .gc
        .stop_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        safepoint::safepoint_in(owner);
        return CollectionAttempt::ElectionLost;
    }
    let Some(_serialize) = cycle_wait::CollectorGuard::try_acquire_in(owner) else {
        safepoint::safepoint_in(owner);
        return CollectionAttempt::ElectionLost;
    };
    // A legacy root owner outside the registry cannot publish a snapshot.
    // Other registered workers do not make that foreign stack safe to scan.
    use root_handshake::HandshakePolicy;
    let check_early =
        root_handshake::RuntimeHandshakePolicy::check_early(|| &owner.gc.handshake_policy);
    if check_early && root::foreign_root_stack_owner_active_in(&owner.gc) {
        progress::record_skip(&owner.gc, GcSkipReason::ForeignOwner);
        return CollectionAttempt::ForeignOwner;
    }
    let gc_log = owner.diagnostics.gc_log.unicode;

    let cycle = crate::gc_telemetry::Cycle::begin_in(
        &owner.telemetry,
        crate::gc_telemetry::CycleKind::Major,
    );

    // Initial STW completes all pre-activation stores, retires TLAB metadata,
    // snapshots roots, and publishes the active mark epoch before resuming.
    let started = std::time::Instant::now();
    let coordination_ns = || {
        let stops = owner.telemetry.stops.snapshot_stops();
        [0, 1, 3].into_iter().fold(0u64, |total, index| {
            total
                .saturating_add(stops.by_reason[index].rendezvous.total_ns)
                .saturating_add(stops.by_reason[index].stopped.total_ns)
        })
    };
    let pause_before = coordination_ns();
    let external_before = owner.gc.native_accounting.bytes.load(Ordering::Relaxed) as u64;
    RuntimeWaitObservation::before_initial(|| &owner.gc.wait_observation);
    let Some((heap_before, marking)) = root_handshake::begin_in(owner) else {
        progress::record_skip(&owner.gc, GcSkipReason::ForeignOwnerAtHandshake);
        return CollectionAttempt::ForeignOwner;
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        #[cfg(debug_assertions)]
        if let Some(message) = marking.invalid_root.lock().unwrap().take() {
            crate::gc_telemetry::failure_record::emit_in(owner, "invariant", &message);
            panic!("willow gc: invalid GC pointer in root snapshot: {message}");
        }
        // Bounded chunks allow a producer-heavy workload to reach remark;
        // finishing relies on quiescing producers, never a racy empty sample.
        let budget = marking.objects.len().saturating_mul(4).max(1024);
        {
            // A stack-owned test runtime cannot lend itself to persistent
            // threads. Its elected collector still drains the same bounded
            // mark work, without fabricating a static lifetime or panicking.
            let shareable = coordinator::Lease::for_runtime(owner).is_some();
            let mut workers = owner.gc.marker_workers.lock().unwrap();
            if shareable && workers.is_none() {
                *workers = mark_workers::Pool::new_for_runtime(*owner.marker_count, owner).ok();
            }
            if let Some(pool) = workers
                .as_ref()
                .filter(|pool| shareable && pool.count() != 0 && old_cycle::concurrent_enabled())
            {
                pool.run(owner, &marking, budget);
            } else {
                // The elected collector is a registered, slotless queue reader.
                // Missing background workers must not force unrestricted STW work.
                if old_cycle::concurrent_enabled() {
                    marking.drain_checked_until_in(
                        owner,
                        256,
                        Some(std::time::Instant::now() + std::time::Duration::from_millis(1)),
                    );
                } else {
                    marking.worker_failed.store(true, Ordering::Release);
                }
            }
        }
        minor::service_pending_in(owner);
        let closed = mark_closure::finish_in(owner, &marking, started);
        let concurrent_closed = closed.is_some();
        let result = if let Some((work, plan)) = closed {
            (work, Some(plan), 0)
        } else {
            mark_closure::finish_stopped_in(owner, &marking, started)
        };
        let (mut work, plan, stopped_freed) = result;
        let freed = plan.map_or(stopped_freed, |plan| sweep::concurrent_in(owner, plan));
        {
            let swept = marking.work.lock().unwrap();
            work.sweep_assist_cpu_ns = swept.sweep_assist_cpu_ns;
            work.sweep_cpu_incomplete = swept.sweep_cpu_incomplete;
        }
        minor::service_pending_in(owner);
        let mut state = owner.gc.heap.lock().unwrap();
        tlab::sync_tlab_bytes_in(&owner.gc, &mut state);
        let after = state.allocated_bytes as u64;
        state.last_major_live_bytes = after;
        state.last_major_policy_live_bytes = major_policy_managed_bytes(&state) as u64;
        state.last_major_mark_work = work
            .marked_bytes
            .saturating_add(work.scanned_bytes)
            .saturating_add(work.descriptor_bytes)
            .saturating_add(work.root_scan_bytes);
        let cpu = (!work.cpu_incomplete && concurrent_closed).then_some(work.cpu_ns);
        state.pacer.mark(
            work.marked_bytes
                .saturating_add(work.scanned_bytes)
                .saturating_add(work.descriptor_bytes),
            cpu,
        );
        drop(state);
        // Include the required initial/remark stops in V1 as well as V2.
        let pause_ns = coordination_ns().saturating_sub(pause_before);
        (after, freed, work, pause_ns)
    }));
    let (heap_after, freed, work, pause_ns) = match result {
        Ok(result) => result,
        Err(payload) => {
            if old_cycle::concurrent_enabled() {
                mark_closure::cancel_in(owner, &marking);
                std::panic::resume_unwind(payload);
            }
            // Failure leaves the graph allocated. Remove transient marks under
            // another stop so the next collection can safely retry.
            safepoint::with_stw_in(
                owner,
                crate::gc_telemetry::stops::StopReason::Recovery,
                |coord, stop_work| {
                    let mut state = owner.gc.heap.lock().unwrap();
                    flush_satb_all_locked(coord, &mut state);
                    old_lab::retire_all_in(&owner.gc, &mut state);
                    owner.gc.mark_phase().store(0, Ordering::Release);
                    marking::set_concurrent_cycle_in(&owner.gc, &mut state, None);
                    tlab::retire_tlabs_with_work_in(owner, &mut state, stop_work);
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

    // Completion publishes the entire CPU interval before the next-goal decision.
    // finish_concurrent may emit telemetry: never call it with the heap locked.
    // Collector serialization remains held until all next-cycle policy is ready.
    {
        let mut state = owner.gc.heap.lock().unwrap();
        tlab::sync_tlab_bytes_in(&owner.gc, &mut state);
        let previous_goal = state.threshold_bytes as u64;
        let bytes = policy_allocated_total_in(&owner.gc, &state);
        state
            .pacer
            .resume_allocation(crate::gc_telemetry::timestamp_ns(), bytes);
        pacer::complete_threshold_in(owner, &mut state, previous_goal);
        // A completed full cycle services any high reservation pressure. If
        // reclamation crossed the low watermark, rearm before new allocations
        // can reserve a region again (even within the same allocation call).
        hard_reservation_pressure_in(&owner.gc, &mut state);
        state.soft_memory.sample_process();
        let memory_input = memory_inputs_in(&owner.gc, &state);
        state
            .soft_memory
            .completed(heap_before.saturating_add(external_before), memory_input);
        if gc_log {
            let decision = state.soft_memory.decide(memory_input);
            eprintln!(
                "[gc] soft_memory goal={} trigger={} overshoot={} reason={:?} native_requested_bytes={} native_requested_exact=false native_committed_valid=false process_resident_bytes={} resident_valid={} process_commit_valid=false process_limit_coverage_complete=false",
                decision.goal,
                decision.trigger,
                decision.overshoot,
                decision.reason,
                owner.gc.native_accounting.bytes.load(Ordering::Relaxed),
                memory_input.process_resident.unwrap_or(0),
                memory_input.process_resident.is_some()
            );
        }
        progress::record_success(&owner.gc, state.total_allocated_bytes);
    }

    if gc_log {
        let state = owner.gc.heap.lock().unwrap();
        eprintln!(
            "gc: heap_before={}B freed={}B heap_after={}B total_allocs={} total_frees={}",
            heap_before, freed, state.allocated_bytes, state.total_allocs, state.total_frees,
        );
    }
    drop(_serialize);
    crate::gc_telemetry::emit_cycle_in(owner, event);
    CollectionAttempt::Completed
}

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
fn checked_payload_to_header(
    owner: &crate::runtime::Runtime,
    payload: *mut u8,
    context: &str,
) -> *mut GcHeader {
    let result = validate_payload_pointer_locked(&owner.gc.heap.lock().unwrap(), payload);
    result.unwrap_or_else(|message| {
        crate::gc_telemetry::failure_record::emit_in(owner, "invariant", &message);
        panic!("willow gc: invalid GC pointer in {context}: {message}");
    })
}

#[cfg(not(debug_assertions))]
fn checked_payload_to_header(
    _owner: &crate::runtime::Runtime,
    payload: *mut u8,
    _context: &str,
) -> *mut GcHeader {
    payload_to_header(payload)
}

#[cfg(all(debug_assertions, test))]
fn validate_payload_pointer(payload: *mut u8, _context: &str) -> Result<*mut GcHeader, String> {
    validate_payload_pointer_locked(&runtime().heap.lock().unwrap(), payload)
}

#[cfg(any(debug_assertions, test))]
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

/// Give a cold collector bounded runway when scheduler execution begins.
pub(crate) fn scheduler_start(workers: usize) {
    nursery::scheduler_start(&mut runtime().heap.lock().unwrap(), workers);
}

/// Lock a runtime mutex for `reset_internal`, recovering and clearing poison.
/// Reset overwrites the guarded state, so a panic that poisoned it (a failed
/// unit test asserting under the heap lock) must not turn every later reset
/// into a panic at the `willow_gc_init` FFI boundary, which aborts the whole
/// test process and hides the original failure.
fn reset_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    let guard = mutex.lock().unwrap_or_else(PoisonError::into_inner);
    mutex.clear_poison();
    guard
}

fn reset_internal() {
    // Coordinator shutdown waits on `coord`; recover it before that wait.
    drop(reset_lock(&runtime().coord.0));
    coordinator::shutdown();
    runtime().minor_pending.store(false, Ordering::Release);
    // Exclude a concurrent collection (see runtime().collect_lock).
    let _serialize = runtime()
        .collect_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    crate::runtime::current()
        .mark_phase()
        .store(0, Ordering::Release);
    let mut state = reset_lock(&runtime().heap);
    marking::set_concurrent_cycle(&mut state, None);
    state.sweeping = None;
    state.pending_sweep_chunks = 0..0;
    publish_collection_busy(&state);
    state.satb = satb::SatbBuffers::default();
    {
        let GcState {
            tlab_states,
            tlab_chunks,
            tlab_addresses,
            ..
        } = &mut *state;
        for record in tlab_states.values() {
            // SAFETY: reset excludes mutators and collectors. Capture the live
            // prefix without constructing retirement indexes that reset discards.
            let tls = unsafe { tlab_state_at(record.address) };
            let cursor = tls.cursor.swap(0, Ordering::AcqRel);
            if let Some(base) = record.current_chunk {
                let chunk = &mut tlab_chunks[tlab_addresses.exact(base).expect("active TLAB")];
                chunk.used = cursor.clamp(base, base + chunk.capacity) - base;
            }
            tls.limit.store(0, Ordering::Release);
            tls.start_bits.store(0, Ordering::Release);
        }
    }
    old_lab::reset(&mut state);
    // Reset owns every remaining payload, including rooted objects. Finalize
    // native owners before clearing their registries or freeing region storage.
    // Drain the index so OldRegion::drop does not revisit reclaimed headers.
    for region in &mut state.old_regions {
        for (offset, _) in std::mem::take(&mut region.allocations) {
            let object = HeapObject::from_raw(unsafe { region.base.add(offset) }.cast()).unwrap();
            if let Some(drop_fn) = lookup_drop(object.type_id()) {
                // SAFETY: reset is quiescent and this indexed payload is live.
                unsafe {
                    run_drop_hook(
                        runtime(),
                        drop_fn,
                        object.payload().as_ptr(),
                        crate::observation::Value::new(|| object.type_id()),
                    )
                };
            }
            object.reclaim_in_place();
        }
    }
    state.old_regions.clear();
    state.old_addresses.clear();
    state.tlab_addresses.clear();
    runtime().root_tlabs.clear();
    state.old_region_candidates.clear();
    state.old_reserved_bytes = 0;
    state.region_cache.shrink_to(0);
    minor::release_survivor_reserve(&mut state);
    for chunk in state.tlab_chunks.drain(..) {
        let mut offset = 0;
        while offset < chunk.used {
            let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast()).unwrap();
            offset += object.size();
            // Moved or previously swept objects no longer own their payloads.
            if object.allocated()
                && let Some(drop_fn) = lookup_drop(object.type_id())
            {
                // SAFETY: reset owns the remaining initialized payload.
                unsafe {
                    run_drop_hook(
                        runtime(),
                        drop_fn,
                        object.payload().as_ptr(),
                        crate::observation::Value::new(|| object.type_id()),
                    )
                };
            }
            object.reclaim_in_place();
        }
        let layout = Layout::from_size_align(chunk.capacity, std::mem::align_of::<GcHeader>())
            .expect("TLAB chunk layout remains valid");
        // SAFETY: reset owns and releases each registered chunk exactly once.
        unsafe { dealloc(chunk.base, layout) };
    }
    state.tlab_states.clear();
    state.tlab_owners.clear();
    state.allocated_bytes = 0;
    pacer::reset_threshold(&mut state);
    state.memory_limit_bytes = gc_memory_limit_from_env();
    state.hard_pressure_triggered = false;
    state.soft_memory = memory_control::Controller::from_env();
    state.last_major_live_bytes = 0;
    state.sweep_allocation_start = 0;
    state.last_major_policy_live_bytes = 0;
    state.last_major_mark_work = 0;
    state.pacer = pacer::Sampler::default();
    assist::reset();
    state.young_allocated_bytes = 0;
    state.young_allocation_clock = 0;
    state.minor_deferral_budget_bytes = 0;
    state.pin_records.clear();
    state.nursery_policy = nursery::Policy::from_env();
    state.tenuring = Default::default();
    state.nursery_threshold_bytes = state.nursery_policy.initial(state.memory_limit_bytes);
    state.young_allocators = 0;
    state.minor_deferral_start_bytes = None;
    state.minor_retry_bytes = 0;
    state.total_allocs = 0;
    state.total_allocated_bytes = 0;
    runtime()
        .native_accounting
        .allocated_bytes
        .store(0, Ordering::Relaxed);
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
    reset_barrier_variant_counts();
    runtime()
        .tlab_ever_allocated
        .store(false, Ordering::Release);
    state.write_barrier_hits = 0;
    state.minor_collections = 0;
    state.promoted_objects = 0;
    state.promoted_bytes = 0;
    state.moved_objects = 0;
    state.survivor_stats = Default::default();
    state.survivor_reserve_stats = Default::default();
    state.old_region_allocations = 0;
    state.old_region_reuses = 0;
    state.old_regions_released = 0;
    state.major_collections = 0;
    state.scheduler_runway_initialized = false;
    runtime().runtime_roots.clear();
    runtime().runtime_root_slots.clear();
    runtime().pin_lease_slots.clear();
    reset_lock(&runtime().parked_stack_roots).clear();
    *reset_lock(&runtime().root_stack_owner) = None;
    progress::reset(runtime());
    // Collectors and TLS-block unregistration take coord before heap; never
    // wait for coord while holding heap.
    drop(state);
    {
        let (lock, cv) = &runtime().coord;
        let mut coord = reset_lock(lock);
        *coord = GcCoord::default();
        safepoint::invalidate_mutator_registrations();
        invalidate_runtime_policy_budgets();
        runtime()
            .stop_requested
            .store(false, std::sync::atomic::Ordering::Release);
        cv.notify_all();
    }
    clear_hook_registries();
    runtime()
        .registry_generation
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    ROOT_STACK.with(|rs| rs.borrow_mut().clear());
    runtime().poll_requested.store(false, Ordering::Release);
    HAS_REGISTERED_TLAB.set(false);
    // Clear compiler-owned literal slots: their pointers are into the
    // heap that was just freed above and must not be returned again.
    crate::string::clear_string_literal_slots();
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
    crate::runtime::current()
        .diagnostic_ready
        .store(true, Ordering::Release);
    crate::frame_lifetime::reset();
    runtime()
        .remark_observation
        .work_ns
        .store(0, Ordering::Relaxed);
}

/// A fresh, empty generated-TLS block for tests in sibling modules that need a
/// YOUNG object: `willow_gc_alloc_slow` is the only runtime entry that allocates
/// into the nursery, every `willow_alloc_*` helper goes straight to the old
/// generation. Container tests use it to give a container young children that a
/// minor collection will move.
#[cfg(test)]
pub(crate) fn tlab_state_for_test() -> GcTlabState {
    GcTlabState::new()
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

/// Test fixture re-exported beside the GC test helpers: each test thread
/// serves an independent runtime instead of locking the process default.
#[cfg(test)]
pub(crate) use crate::runtime::isolated_runtime;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod reset_tests;

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
mod tests;

#[cfg(test)]
mod instance_isolation_tests;

#[cfg(test)]
mod m0_reference_tests;

#[cfg(test)]
#[path = "gc_concurrent_tests.rs"]
mod concurrent_tests;

#[cfg(test)]
mod failure_policy_tests;

#[cfg(test)]
mod native_memory_tests;

#[cfg(test)]
mod native_trace_tests;

/// Explicit collection returning reservation high water from after any epoch
/// already in flight finishes through the next major MarkComplete (the cycle
/// this request runs or joins). Arming shares the growth lock; sweep
/// preparation atomically cuts off further updates.
#[cfg(feature = "gc-acceptance-faults")]
pub fn acceptance_collect_scoped() -> u64 {
    crate::scheduler::release_quiescent_frame_roots(true);
    let _relocation = RelocationSafeScope::enter();
    let mut armed = false;
    cycle_wait::collect_explicit_after(|| {
        let clean = {
            let heap = runtime().heap.lock().unwrap();
            crate::gc_telemetry::footprint::window_begin(
                (heap.old_reserved_bytes as u64).saturating_add(heap.tlab_reserved_bytes as u64),
            )
        };
        // Checked after unlock so a failure cannot poison the heap mutex.
        assert!(clean, "footprint window already armed");
        armed = true;
    });
    assert!(
        armed,
        "scoped collection requested by the elected collector"
    );
    // Another election between arming and the request may have been joined
    // instead; request again until a major sweep preparation closes the window.
    while crate::gc_telemetry::footprint::window_open() {
        cycle_wait::collect_for_budget();
    }
    crate::gc_telemetry::footprint::window_end()
}

/// Controlled protocol faults for the opt-in acceptance executable. These enter
/// real stop/heap-lock paths; no trace line or snapshot is forged by the probe.
#[cfg(feature = "gc-acceptance-faults")]
pub fn acceptance_inject(kind: &str) {
    use crate::gc_telemetry::stops::StopReason;
    // Stop initiators must own collector election, like real cycles, so an
    // injected stop never races a background sweep or another stop request.
    fn elected() -> cycle_wait::CollectorGuard<'static> {
        loop {
            if let Some(guard) = cycle_wait::CollectorGuard::try_acquire() {
                break guard;
            }
            willow_gc_safepoint();
            std::thread::yield_now();
        }
    }
    match kind {
        "short_stw" => {
            let _election = elected();
            with_stw(StopReason::InitialMark, |_, _| {
                std::thread::sleep(std::time::Duration::from_millis(5));
            })
        }
        "fallback" => {
            let _election = elected();
            with_stw(StopReason::Recovery, |_, _| {})
        }
        "mark_wait" => {
            let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let signal = ready.clone();
            let owner = std::thread::spawn(crate::runtime::serving(move || {
                let _role = crate::gc_telemetry::observations::MarkerRole::new();
                let _registration = MutatorRegistration::new();
                let _election = elected();
                signal.store(true, Ordering::Release);
                // Hold the election, but stay stoppable by any other initiator.
                let until = std::time::Instant::now() + std::time::Duration::from_millis(50);
                while std::time::Instant::now() < until {
                    willow_gc_safepoint();
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }));
            while !ready.load(Ordering::Acquire) && !owner.is_finished() {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            assert!(ready.load(Ordering::Acquire), "collector injection failed");
            // Exercise the actual explicit-request election join (CycleWait),
            // without inventing a stop-the-world interval around the delay.
            willow_gc_collect();
            owner.join().unwrap();
        }
        "lock_stall" => {
            let heap = runtime().heap.lock().unwrap();
            let worker = std::thread::spawn(crate::runtime::serving(|| {
                let _registration = MutatorRegistration::new();
                std::hint::black_box(willow_alloc(64));
                willow_gc_safepoint();
            }));
            std::thread::sleep(std::time::Duration::from_millis(40));
            drop(heap);
            while !worker.is_finished() {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            worker.join().unwrap();
        }
        _ => panic!("unknown acceptance fault"),
    }
}

#[cfg(test)]
mod sweep_allocation_tests;
