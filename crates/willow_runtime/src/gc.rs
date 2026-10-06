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
// in nursery TLAB regions. Survivors reachable through root or heap slots copy
// to collector-owned young chunks once, then tenure into old regions on their
// second minor survival; the collector rewrites every such slot, including
// runtime-owned slots (`willow_gc_add_runtime_root_slot`). Address-registered
// runtime roots name only old objects. A thread parked inside runtime code
// outside a `RelocationSafeScope` defers the minor collection while it
// publishes a young referent; only a hard memory budget pins instead.

mod safepoint;
pub use safepoint::*;
mod marking;
use marking::*;
mod registry;
pub use registry::*;
mod root_arena;
pub(crate) use root_arena::*;
mod tlab;
use tlab::*;
mod root;
pub use root::*;
mod barrier;
pub use barrier::*;
mod telemetry;
pub use telemetry::*;
mod validation;
use validation::*;

pub(crate) use marking::shutdown_mark_workers;

mod region;
use region::*;

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
mod nursery;
mod old_lab;
mod pacer;
mod root_handshake;
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
    let mut rooted = 0usize;
    let mut offset = 0;
    for kind in layout.payload.slots {
        if kind.traces_first_word() {
            // SAFETY: `words` is not resized while the root is registered and
            // each i64 word is one pointer-sized Willow slot on supported
            // targets.
            willow_push_root(unsafe { words.as_mut_ptr().add(offset).cast::<*mut u8>() });
            rooted += 1;
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
    willow_pop_roots(rooted as i32);
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
    /// call (willow-8hq4.15); only the runtime writes it, under the heap lock
    /// or with mutators stopped.
    pub remembered: u8,
    /// Live: immutable descriptor address. Reclaimed: original allocation size,
    /// so retained TLAB holes remain walkable without retaining dead metadata.
    pub descriptor: usize,
}

/// Generated-code-facing TLS allocation state.
///
/// The compiler defines one zero-initialized TLS instance of this layout in
/// each Willow executable. The runtime receives its address only on the slow
/// path, registers the owning chunk, and may invalidate cursor/limit while the
/// mutator is stopped for collection. Generated code writes cursor and start
/// words with plain stores and keeps no per-object counters; the runtime
/// derives fast-path bytes from the cursor and counts from the start words
/// (willow-8hq4.16).
#[repr(C)]
pub struct GcTlabState {
    cursor: AtomicUsize,
    limit: AtomicUsize,
    /// Stable pointer to this chunk's atomic object-start words. Published
    /// before cursor; generated allocation sets its bit after header writes.
    start_bits: AtomicUsize,
}

pub const GC_TLAB_STATE_SIZE: usize = std::mem::size_of::<GcTlabState>();
pub const GC_HEADER_SIZE: usize = std::mem::size_of::<GcHeader>();
pub const GC_TLAB_CHUNK_SIZE: usize = willow_abi::tlab::CHUNK_SIZE as usize;
const _: () = assert!(GC_TLAB_CHUNK_SIZE <= u16::MAX as usize + 1);
pub const GC_TLAB_MAX_OBJECT_SIZE: usize = 4 * 1024;

#[path = "gc/layouts.rs"]
mod layouts;

/// Raw allocation and pointer arithmetic boundary for the collector. The rest
/// of the GC works with `Object`/`Payload`/`RootSlot` and cannot directly
/// dereference a header or stack-slot pointer.
mod raw_heap;

use raw_heap::{Object as HeapObject, Payload as GcPayload, RootSlot};

// ---------------------------------------------------------------------------
// GC state
// ---------------------------------------------------------------------------

struct GcState {
    concurrent_cycle: Option<Arc<ConcurrentCycle>>,
    sweeping: Option<ThreadId>,
    satb: satb::SatbBuffers,
    /// Bump-allocation chunks. Active chunks are owned by one TLS state;
    /// collection retires them before walking their object headers. Collector-only
    /// survivor chunks share this index but never belong to a TLS state.
    tlab_chunks: Vec<BumpChunk>,
    tlab_addresses: address_index::AddressIndex,
    /// Non-moving old-generation storage. Regular regions serve old/runtime
    /// allocations from a bump tail or region-local free spans. Large objects
    /// receive one dedicated region. Object addresses never change.
    old_regions: Vec<OldRegion>,
    old_addresses: address_index::AddressIndex,
    /// (largest available span, region index); excludes dedicated/full regions.
    old_region_candidates: BinaryHeap<(usize, usize)>,
    old_reserved_bytes: usize,
    /// Per-thread old allocation buffers used by runtime-side allocation.
    old_labs: Vec<old_lab::OldLabRecord>,
    /// Generated TLS states observed on allocation slow paths.
    tlab_states: HashMap<usize, TlabStateRecord>,
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
    last_major_mark_work: u64,
    pacer: pacer::Sampler,
    pacer_trigger: u64,
    /// Bytes occupied by young objects in TLABs and collector survivor chunks.
    young_allocated_bytes: usize,
    /// Trigger a minor collection at the next TLAB refill after this threshold.
    nursery_threshold_bytes: usize,
    /// Young bytes when the current run of deferred minor collections began
    /// (willow-9tls.9); `None` when the last minor attempt collected.
    minor_deferral_start_bytes: Option<usize>,
    /// The allocation path retries a deferred minor collection only once the
    /// young bytes reach this mark, one TLAB chunk past the deferral.
    minor_retry_bytes: usize,
    nursery_policy: nursery::Policy,
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

// Native owners can outlive a test/reset epoch. Never zero this counter at reset:
// each owner releases its own charge, including non-GC lock state.
static EXTERNAL_BYTES: AtomicUsize = AtomicUsize::new(0);
static EXTERNAL_ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn external_bytes() -> usize {
    EXTERNAL_BYTES.load(Ordering::Relaxed)
}

/// Native bytes that may still be charged without the heap lock because no
/// collection-request decision can change before they are used up. Zero under
/// a soft memory limit and within a quarter of a hard reservation cap, so those
/// charges stay exact; recomputed under the heap lock by every slow-path charge and
/// capped like `POLICY_HEADROOM`. Managed growth consumes it too
/// (`consume_headrooms`), so native and managed bytes since publication never
/// cross a trigger unobserved.
static EXTERNAL_HEADROOM: AtomicUsize = AtomicUsize::new(0);

/// Runtime-allocation bytes whose collection-policy check may skip the heap
/// lock: no trigger can fire before they are used up. Zero under a hard
/// reservation cap or a soft memory limit, at or past the pacer trigger, and
/// after any policy-input change; recomputed by every locked check that finds
/// nothing to do (willow-jz15.53). Capped so the pacer's allocation-rate
/// samples, taken by locked checks, stay at most this many bytes apart.
static POLICY_HEADROOM: AtomicU64 = AtomicU64::new(0);
const POLICY_HEADROOM_CAP: u64 = 1 << 20;

/// Invalidate both lock-free headrooms; every write that can lower a goal or
/// trigger calls this.
fn invalidate_external_headroom() {
    EXTERNAL_HEADROOM.store(0, Ordering::Release);
    POLICY_HEADROOM.store(0, Ordering::Release);
}

/// Charge `bytes` of `occupied` growth against both lock-free headrooms.
/// Called before the growth becomes visible, by every heap-locked site that
/// raises `allocated_bytes`, so one headroom's room never ignores growth
/// through the other path. O(1); a no-op once both are exhausted.
pub(super) fn consume_headrooms(bytes: usize) {
    if EXTERNAL_HEADROOM.load(Ordering::Acquire) != 0 {
        let _ = EXTERNAL_HEADROOM.fetch_update(Ordering::AcqRel, Ordering::Acquire, |room| {
            Some(room.saturating_sub(bytes))
        });
    }
    consume_policy_room(bytes as u64);
}

fn consume_policy_room(bytes: u64) {
    if POLICY_HEADROOM.load(Ordering::Acquire) != 0 {
        let _ = POLICY_HEADROOM.fetch_update(Ordering::AcqRel, Ordering::Acquire, |room| {
            Some(room.saturating_sub(bytes))
        });
    }
}

fn policy_headroom(state: &GcState) -> u64 {
    if state.memory_limit_bytes.is_some() || state.soft_memory.enabled() {
        return 0;
    }
    let input = memory_inputs(state);
    let mut trigger = input.unlimited_goal.max(input.live);
    if state.pacer.enabled() {
        trigger = trigger.min(state.pacer_trigger);
    }
    trigger
        .saturating_sub(input.occupied)
        .min(POLICY_HEADROOM_CAP)
}

/// Whether `bytes` of runtime allocation fit the lock-free policy headroom.
fn consume_policy_headroom(bytes: u64) -> bool {
    POLICY_HEADROOM
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |room| {
            room.checked_sub(bytes)
        })
        .is_ok()
}

/// Bytes before the next native charge could cross the unlimited goal, the
/// pacer trigger, or (under a hard cap) the hard-pressure watermark at 3/4 of
/// the cap, below which `can_reserve` cannot fail either. Goal and trigger only
/// move under the heap lock and every write that can lower them invalidates
/// the headroom; reservation growth quiesces it in `can_reserve`. Zero at or
/// past the watermark and under a soft limit, whose controller decides on
/// every charge.
fn external_headroom(state: &GcState) -> usize {
    if state.soft_memory.enabled() {
        return 0;
    }
    let input = memory_inputs(state);
    let below = |trigger: u64| trigger.saturating_sub(input.occupied).saturating_sub(1);
    let mut room = below(input.unlimited_goal.max(input.live));
    if let Some(limit) = state.memory_limit_bytes {
        let watermark = (limit / 4).saturating_mul(3).max(1) as u64;
        room = room.min(watermark.saturating_sub(input.committed).saturating_sub(1));
    }
    if state.pacer.enabled() && input.occupied < state.pacer_trigger {
        room = room.min(below(state.pacer_trigger));
    }
    room.min(POLICY_HEADROOM_CAP) as usize
}

/// Lock-free native charges between their headroom check and completion.
/// [`quiesce_external_charges`] waits for zero so `external_bytes()` is exact.
static EXTERNAL_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Close the lock-free native path and wait out charges already inside it.
/// After this, `external_bytes()` holds no transient (rolled-back) bytes and
/// no new lock-free charge can start until a locked charge republishes the
/// headroom. SeqCst pairs with the charger's in-flight increment and headroom
/// load: either the charger sees zero headroom, or this sees it in flight.
/// O(1) plus a spin over at most a few atomics per in-flight charge.
fn quiesce_external_charges() {
    EXTERNAL_HEADROOM.store(0, Ordering::SeqCst);
    while EXTERNAL_IN_FLIGHT.load(Ordering::SeqCst) != 0 {
        std::hint::spin_loop();
    }
}

/// Lock-free native charge; false (with nothing charged) when the headroom
/// cannot cover `bytes`.
fn charge_external_lock_free(bytes: usize) -> bool {
    EXTERNAL_IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
    let charged = EXTERNAL_HEADROOM.load(Ordering::SeqCst) >= bytes && {
        // Publish the bytes BEFORE claiming headroom: a locked charge that
        // republishes `EXTERNAL_HEADROOM` in between then already counts them
        // in `occupied`. A failed claim rolls the add back.
        EXTERNAL_BYTES
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(bytes)
            })
            .unwrap_or_else(|_| {
                crate::failure::resource_exhausted(format_args!("native byte accounting overflow"))
            });
        let claimed = EXTERNAL_HEADROOM
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |room| {
                room.checked_sub(bytes)
            })
            .is_ok();
        if !claimed {
            EXTERNAL_BYTES.fetch_sub(bytes, Ordering::AcqRel);
        }
        claimed
    };
    EXTERNAL_IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
    if !charged {
        return false;
    }
    // Within the headroom no crossing or cap check can fire: account without
    // serializing native-heavy pipelines on the heap mutex. The policy
    // headroom must also see this `occupied` growth.
    consume_policy_room(bytes as u64);
    let _ =
        EXTERNAL_ALLOCATED_BYTES.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_add(bytes as u64))
        });
    true
}

pub(crate) fn charge_external(bytes: usize) {
    if charge_external_lock_free(bytes) {
        return;
    }
    // Serialize increases with region reservations. Releases need no heap lock,
    // so finalizers also work during reset and stop-the-world collection.
    let state = runtime().heap.lock().unwrap();
    // Exact inputs for the crossing checks below.
    quiesce_external_charges();
    if !can_reserve(&state, bytes) {
        allocation_failure(&state);
    }
    let previous_external = EXTERNAL_BYTES
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(bytes)
        })
        .unwrap_or_else(|_| {
            crate::failure::resource_exhausted(format_args!("native byte accounting overflow"))
        });
    let _ =
        EXTERNAL_ALLOCATED_BYTES.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            Some(current.saturating_add(bytes as u64))
        });
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
    let previous_occupied = state.allocated_bytes.saturating_add(previous_external) as u64;
    let paced_crossing = state.pacer.enabled()
        && previous_occupied < state.pacer_trigger
        && previous_occupied.saturating_add(bytes as u64) >= state.pacer_trigger;
    let pressure = state.concurrent_cycle.is_none()
        && state.sweeping.is_none()
        && (hard_crossing
            || paced_crossing
            || state.soft_memory.decide(memory_inputs(&state)).collect);
    EXTERNAL_HEADROOM.store(external_headroom(&state), Ordering::Release);
    drop(state);
    if pressure {
        // Request only; never collect or park while a native guard is held.
        // Unregistered compatibility callers collect on their next GC allocation.
        coordinator::request();
    }
}

pub(crate) fn release_external(bytes: usize) {
    EXTERNAL_BYTES.fetch_sub(bytes, Ordering::Relaxed);
}

fn can_reserve(state: &GcState, additional: usize) -> bool {
    state.memory_limit_bytes.is_none_or(|limit| {
        // Every reservation passes here first: stop lock-free native charges
        // (their headroom predates this reservation) and drop their transient
        // bytes before the exact cap check.
        quiesce_external_charges();
        state
            .old_reserved_bytes
            .checked_add(state.tlab_reserved_bytes)
            .and_then(|total| total.checked_add(external_bytes()))
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
    pacer::Inputs {
        live: state
            .last_major_live_bytes
            .saturating_add(external_bytes() as u64),
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

// Policy samplers share one cumulative domain. Reclamation reduces occupancy,
// but must not remove native allocation history from a rate sample.
fn policy_allocated_total(state: &GcState) -> u64 {
    state
        .total_allocated_bytes
        .saturating_add(EXTERNAL_ALLOCATED_BYTES.load(Ordering::Relaxed))
}

fn memory_inputs(state: &GcState) -> memory_control::Inputs {
    memory_control::Inputs {
        unlimited_goal: state.threshold_bytes as u64,
        live: state
            .last_major_live_bytes
            .saturating_add(external_bytes() as u64),
        occupied: state.allocated_bytes.saturating_add(external_bytes()) as u64,
        committed: state
            .old_reserved_bytes
            .saturating_add(state.tlab_reserved_bytes)
            .saturating_add(external_bytes()) as u64,
        allocated_total: policy_allocated_total(state),
        // Our process provider reports RSS, not a compatible commit scope.
        non_heap_commit: None,
    }
}

impl Default for GcState {
    fn default() -> Self {
        let memory_limit_bytes = gc_memory_limit_from_env();
        let nursery_policy = nursery::Policy::from_env();
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
            old_labs: Vec::new(),
            tlab_states: HashMap::new(),
            tlab_owners: HashMap::new(),
            allocated_bytes: 0,
            threshold_bytes: 1024 * 1024,
            memory_limit_bytes,
            hard_pressure_triggered: false,
            soft_memory: memory_control::Controller::from_env(),
            last_major_live_bytes: 0,
            last_major_mark_work: 0,
            pacer: pacer::Sampler::default(),
            pacer_trigger: 1024 * 1024,
            young_allocated_bytes: 0,
            nursery_threshold_bytes: nursery_policy.initial(memory_limit_bytes),
            minor_deferral_start_bytes: None,
            minor_retry_bytes: 0,
            nursery_policy,
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
// SNAPSHOT of its root-slot locations under `COORD`'s lock and parks. The
// collector then scans every registered mutator's published roots. Each thread
// only ever reads its OWN thread-local stack, so there is no cross-thread
// TLS/RefCell aliasing. The collector accesses published slots only while
// their owners are parked; it does not access another thread's RefCell.
//
// Major collection uses an independent root-publication handshake initially
// and this parking protocol for final remark. Tracing reads atomic references
// and concurrent container snapshots while SATB/insertion barriers retain edges.
#[derive(Default)]
struct GcCoord {
    /// Registered mutators → writable root-slot addresses, valid only while
    /// the owner is parked. Empty until the first stop-the-world publication.
    mutators: HashMap<ThreadId, PublishedRoots>,
    /// A collector has requested all mutators to reach a safepoint and park.
    stop_requested: bool,
    /// Mutators currently parked at a safepoint.
    parked: HashSet<ThreadId>,
    handshake: Option<root_handshake::Handshake>,
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
    /// Runtime-owned slots a moving collection rewrites (counted per slot).
    runtime_root_slots: RuntimeRootSet,
    parked_stack_roots: Mutex<HashMap<u64, PublishedRoots>>,
    next_parked_stack: AtomicU64,
    coord: (Mutex<GcCoord>, Condvar),
    /// Lock-free fast-path mirror of `GcCoord::stop_requested`.
    stop_requested: std::sync::atomic::AtomicBool,
    poll_requested: std::sync::atomic::AtomicBool,
    trace_registry: Mutex<HashMap<u32, TraceFn>>,
    concurrent_trace_registry: Mutex<IdMap<u32, ConcurrentTraceFn>>,
    concurrent_slice_registry: Mutex<IdMap<u32, ConcurrentTraceSliceFn>>,
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
            runtime_root_slots: RuntimeRootSet::default(),
            parked_stack_roots: Mutex::new(HashMap::new()),
            next_parked_stack: AtomicU64::new(1),
            coord: (Mutex::new(GcCoord::default()), Condvar::new()),
            stop_requested: std::sync::atomic::AtomicBool::new(false),
            poll_requested: std::sync::atomic::AtomicBool::new(false),
            trace_registry: Mutex::new(HashMap::new()),
            concurrent_trace_registry: Mutex::new(IdMap::default()),
            concurrent_slice_registry: Mutex::new(IdMap::default()),
            drop_registry: Mutex::new(HashMap::new()),
            registry_generation: std::sync::atomic::AtomicU64::new(1),
        }
    }
}

static GC_RUNTIME: LazyLock<GcRuntime> = LazyLock::new(GcRuntime::default);

fn runtime() -> &'static GcRuntime {
    &GC_RUNTIME
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
    let Some(limit) = state.memory_limit_bytes else {
        state.hard_pressure_triggered = false;
        return false;
    };
    let reserved = state
        .old_reserved_bytes
        .saturating_add(state.tlab_reserved_bytes)
        .saturating_add(external_bytes());
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
/// check that would only report "busy" without contending on the heap lock
/// (willow-jz15.53). A stale read only shifts the check to the next slow path.
static COLLECTION_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn publish_collection_busy(state: &GcState) {
    COLLECTION_BUSY.store(
        state.concurrent_cycle.is_some() || state.sweeping.is_some(),
        Ordering::Release,
    );
}

fn allocation_should_collect() -> bool {
    let stress = gc_stress_enabled("alloc");
    if !stress && COLLECTION_BUSY.load(Ordering::Acquire) {
        return false;
    }
    let mut state = runtime().heap.lock().unwrap();
    if !stress && (state.concurrent_cycle.is_some() || state.sweeping.is_some()) {
        return false;
    }
    sync_tlab_bytes(&mut state);
    let bytes = policy_allocated_total(&state);
    if state.pacer.sample_due(bytes) {
        state
            .pacer
            .allocation(crate::gc_telemetry::timestamp_ns(), bytes);
        // Sampling may advance the trigger, but cannot move the current goal.
        let input = pacer_inputs(&state);
        let decision = state.pacer.decision(input);
        state.pacer_trigger = (state.threshold_bytes as u64)
            .saturating_sub(decision.runway)
            .max(input.live.saturating_add(256 * 1024))
            .min(state.threshold_bytes as u64);
        invalidate_external_headroom();
    }
    let hard_pressure = hard_reservation_pressure(&mut state);
    let memory_input = memory_inputs(&state);
    let memory = state.soft_memory.decide(memory_input);
    let paced = state.pacer.enabled()
        && memory_input.occupied >= state.pacer_trigger
        && memory.reason != memory_control::Reason::Relief;
    let collect = stress || hard_pressure || paced || memory.collect;
    if !collect {
        POLICY_HEADROOM.store(policy_headroom(&state), Ordering::Release);
    }
    collect
}

fn allocation_should_minor_collect() -> bool {
    let stress = gc_stress_enabled("minor") || gc_stress_enabled("relocate");
    let mut state = runtime().heap.lock().unwrap();
    sync_tlab_bytes(&mut state);
    stress
        || (state.young_allocated_bytes >= state.nursery_threshold_bytes
            && state.young_allocated_bytes >= state.minor_retry_bytes)
}

// ---------------------------------------------------------------------------
// Public runtime API
// ---------------------------------------------------------------------------

/// Set the soft managed-memory limit in bytes; zero disables it. Returns the
/// previous limit. Collection is considered at the next allocation slow path;
/// this setter never parks mutators or changes the legacy hard reservation cap.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_set_memory_limit(bytes: u64) -> u64 {
    let previous = runtime().heap.lock().unwrap().soft_memory.set_limit(bytes);
    invalidate_runtime_policy_budgets();
    previous
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
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_init() {
    reset_internal();
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
    let Some(total_size) = (GC_HEADER_SIZE)
        .checked_add(payload_size as usize)
        .and_then(|size| size.checked_next_multiple_of(std::mem::align_of::<GcHeader>()))
    else {
        crate::failure::resource_exhausted(format_args!("TLAB allocation size overflow"));
    };
    let state_address = tlab_state as usize;
    let stress = gc_stress_enabled("alloc");
    let small = total_size <= GC_TLAB_MAX_OBJECT_SIZE;
    let fast_bytes = {
        let mut state = runtime().heap.lock().unwrap();
        register_tlab_state(&mut state, state_address);
        let record = state.tlab_states.get_mut(&state_address).unwrap();
        let fast = fast_bytes_total(record);
        let fast_bytes = fast.saturating_sub(record.assist_observed_fast_bytes);
        record.assist_observed_fast_bytes = fast;
        sync_tlab_bytes(&mut state);
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
        crate::failure::fatal_invariant("fresh TLAB could not initialize a validated object");
    };
    state.tlab_chunks.last_mut().unwrap().mark_bitmap.mark(0);
    // SAFETY: the generated TLS block is aligned and remains alive for this
    // thread. Publish limit before cursor; generated code resumes only after
    // this slow-path call returns.
    let tls = unsafe { tlab_state_at(state_address) };
    // Relocation stress closes the TLAB after one object, so every generated
    // allocation returns here and runs a moving minor collection first.
    let limit = if gc_stress_enabled("relocate") {
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
    consume_headrooms(total_size);
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

#[cfg(test)]
thread_local! { static RETIRED_LOOKUP_COMPARISONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

fn object_in_retired_chunk(
    chunk: &BumpChunk,
    address: usize,
    interior: bool,
) -> Option<HeapObject> {
    let relative = address.checked_sub(chunk.base as usize + GC_HEADER_SIZE)?;
    let offset = if chunk.needs_index() {
        // Not indexed yet (willow-8hq4.21): its start bits name exactly its
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
    assist_concurrent_mark(total_size);
    let stress = gc_stress_enabled("alloc");
    // A due check charges the chunk it covers against the lock-free headroom
    // first; only an exhausted headroom consults the policy under the lock.
    if (stress
        || (runtime_policy_check_due(total_size)
            && !consume_policy_headroom(GC_TLAB_CHUNK_SIZE as u64 + total_size)))
        && allocation_should_collect()
    {
        automatic_collect(stress);
    }
    allocate_old(layout_id, type_id, payload_size, gc_ref_mask)
}

/// Bumped when collection-policy inputs change outside allocation, so every
/// thread's [`RUNTIME_POLICY_DEBT`] budget expires and its next runtime
/// allocation consults the policy immediately.
static RUNTIME_POLICY_EPOCH: AtomicU64 = AtomicU64::new(1);

std::thread_local! {
    /// `(epoch, bytes)` this thread's runtime allocations have made since its
    /// last collection-policy check (willow-jz15.51).
    static RUNTIME_POLICY_DEBT: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

fn invalidate_runtime_policy_budgets() {
    RUNTIME_POLICY_EPOCH.fetch_add(1, Ordering::AcqRel);
    invalidate_external_headroom();
}

/// Record growth of reserved heap storage. Under a hard reservation cap the
/// pressure trigger depends on reservations rather than allocated bytes, so a
/// reservation re-arms every thread's next policy check and the trigger fires
/// at the same allocation as without the per-thread budget. Reservations grow
/// once per region or chunk, so this costs one check per thread per growth.
fn note_reservation_growth(state: &GcState) {
    if state.memory_limit_bytes.is_some() {
        invalidate_runtime_policy_budgets();
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
fn runtime_policy_check_due(bytes: u64) -> bool {
    let epoch = RUNTIME_POLICY_EPOCH.load(Ordering::Acquire);
    RUNTIME_POLICY_DEBT.with(|debt| {
        let (seen, owed) = debt.get();
        let owed = if seen == epoch {
            owed.saturating_add(bytes)
        } else {
            u64::MAX
        };
        let due = owed >= GC_TLAB_CHUNK_SIZE as u64;
        debt.set((epoch, if due { 0 } else { owed }));
        due
    })
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
        note_reservation_growth(state);
        index_old_region(state, state.old_regions.len() - 1);
        allocated
    };

    consume_headrooms(object.size());
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
        crate::failure::fatal_invariant("negative managed allocation size");
    }
    let payload_size = payload_size as usize;
    if old_lab::enabled()
        && let Some((total, span)) = old_lab::span_for(payload_size)
        && total <= GC_TLAB_MAX_OBJECT_SIZE
        && old_lab::available()
    {
        // Small runtime objects bump-allocate in this thread's old buffer and
        // take the heap mutex only to refill it (willow-jz15.53).
        if let Some(object) = old_lab::try_allocate(type_id, layout_id, gc_ref_mask, total, span) {
            return object.payload().as_ptr();
        }
        let mut state = runtime().heap.lock().unwrap();
        let refill = |state: &mut GcState| {
            old_lab::refill_and_allocate(state, type_id, layout_id, gc_ref_mask, total, span)
        };
        let mut object = refill(&mut state);
        if object.is_none() && (state.memory_limit_bytes.is_some() || state.soft_memory.enabled()) {
            drop(state);
            collect_for_budget();
            state = runtime().heap.lock().unwrap();
            object = refill(&mut state);
        }
        let Some(object) = object else {
            return allocation_failure(&state);
        };
        return object.payload().as_ptr();
    }
    // TLAB byte merging is O(TLABs); the policy checks and TLAB slow path that
    // read those bytes merge them, so the per-object path does not.
    let mut state = runtime().heap.lock().unwrap();
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
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_collect() {
    let _relocation = RelocationSafeScope::enter();
    collect_internal();
}

/// Trigger a stop-the-world minor collection. Young objects reachable from
/// root slots or heap slots copy to young survivor storage at age 1, then to
/// non-moving old storage at age 2, and every such slot is rewritten. The
/// collection is deferred while another thread is parked inside runtime code
/// with a young referent (see `minor_collect_with_roots`).
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_minor_collect() {
    let _relocation = RelocationSafeScope::enter();
    minor_collect_internal();
}

/// Whether the GC stress mode `kind` is active via the `WILLOW_GC_STRESS`
/// environment variable. The variable is a comma-separated list of modes; `all`
/// enables every mode (willow-lpn.8).
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
/// that would pin a young object in place aborts and names the pin source
/// (willow-9tls.9). Generated code pins only the owners of `&`/`&mut`
/// arguments that point into an object or array element.
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
    let descriptor = metadata.gc_ref_mask as *const u64;
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
    let inline_mask = metadata.inline_ref_mask();
    for index in 0..payload_words.min(64) {
        if (inline_mask & (1u64 << index)) != 0 {
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

fn automatic_collect(stress: bool) {
    if stress || !coordinator::request() {
        collect_internal();
    }
}

#[cfg(all(test, debug_assertions))]
pub(crate) fn collect_for_worker_panic_test() {
    collect_internal();
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
    let external_before = external_bytes() as u64;
    let Some((heap_before, marking)) = root_handshake::begin() else {
        runtime()
            .skipped_foreign_owner_collections
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    };
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
                    let mut state = runtime().heap.lock().unwrap();
                    // No buffer may allocate white between phase 0 and sweep_pending.
                    old_lab::retire_all(&mut state);
                    GC_MARK_PHASE.store(0, Ordering::Release);
                    marking::set_concurrent_cycle(&mut state, None);
                    drop(state);
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
        sync_tlab_bytes(&mut state);
        let after = state.allocated_bytes as u64;
        let previous_goal = state.threshold_bytes as u64;
        // The goal and pacer trigger may move down below.
        invalidate_external_headroom();
        state.threshold_bytes = state
            .allocated_bytes
            .saturating_add(external_bytes())
            .saturating_mul(2)
            .max(1024 * 1024);
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
        let bytes = policy_allocated_total(&state);
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
        // A completed full cycle services any high reservation pressure. If
        // reclamation crossed the low watermark, rearm before new allocations
        // can reserve a region again (even within the same allocation call).
        hard_reservation_pressure(&mut state);
        let memory_input = memory_inputs(&state);
        state
            .soft_memory
            .completed(heap_before.saturating_add(external_before), memory_input);
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
                    old_lab::retire_all(&mut state);
                    GC_MARK_PHASE.store(0, Ordering::Release);
                    marking::set_concurrent_cycle(&mut state, None);
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
fn reset_internal() {
    coordinator::shutdown();
    // Exclude a concurrent collection (see runtime().collect_lock).
    let _serialize = runtime()
        .collect_lock
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    GC_MARK_PHASE.store(0, Ordering::Release);
    let mut state = runtime().heap.lock().unwrap();
    marking::set_concurrent_cycle(&mut state, None);
    state.sweeping = None;
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
                unsafe { run_drop_hook(drop_fn, object.payload().as_ptr()) };
            }
            object.reclaim_in_place();
        }
    }
    state.old_regions.clear();
    state.old_addresses.clear();
    state.tlab_addresses.clear();
    state.old_region_candidates.clear();
    state.old_reserved_bytes = 0;
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
                unsafe { run_drop_hook(drop_fn, object.payload().as_ptr()) };
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
    state.threshold_bytes = 1024 * 1024;
    state.memory_limit_bytes = gc_memory_limit_from_env();
    state.hard_pressure_triggered = false;
    state.soft_memory = memory_control::Controller::from_env();
    state.last_major_live_bytes = 0;
    state.last_major_mark_work = 0;
    state.pacer = pacer::Sampler::default();
    state.pacer_trigger = 1024 * 1024;
    assist::reset();
    state.young_allocated_bytes = 0;
    state.nursery_policy = nursery::Policy::from_env();
    state.nursery_threshold_bytes = state.nursery_policy.initial(state.memory_limit_bytes);
    state.minor_deferral_start_bytes = None;
    state.minor_retry_bytes = 0;
    state.total_allocs = 0;
    state.total_allocated_bytes = 0;
    EXTERNAL_ALLOCATED_BYTES.store(0, Ordering::Relaxed);
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
    state.survivor_stats = Default::default();
    state.old_region_allocations = 0;
    state.old_region_reuses = 0;
    state.old_regions_released = 0;
    state.major_collections = 0;
    runtime().runtime_roots.clear();
    runtime().runtime_root_slots.clear();
    runtime().parked_stack_roots.lock().unwrap().clear();
    *runtime().root_stack_owner.lock().unwrap() = None;
    {
        let (lock, cv) = &runtime().coord;
        let mut coord = lock.lock().unwrap();
        *coord = GcCoord::default();
        safepoint::invalidate_mutator_registrations();
        invalidate_runtime_policy_budgets();
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
#[path = "gc_concurrent_tests.rs"]
mod concurrent_tests;

#[cfg(test)]
mod failure_policy_tests;

#[cfg(test)]
mod native_memory_tests;
