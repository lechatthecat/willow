use super::*;

pub(crate) struct BarrierCounts {
    destinations: [AtomicU64; 13],
    deletions: AtomicU64,
}
impl BarrierCounts {
    pub(crate) const fn new() -> Self {
        Self {
            destinations: [const { AtomicU64::new(0) }; 13],
            deletions: AtomicU64::new(0),
        }
    }
    fn snapshot(&self) -> ([u64; 13], u64) {
        (
            std::array::from_fn(|i| self.destinations[i].load(Ordering::Relaxed)),
            self.deletions.load(Ordering::Relaxed),
        )
    }
    fn reset(&self) {
        for counter in &self.destinations {
            counter.store(0, Ordering::Relaxed);
        }
        self.deletions.store(0, Ordering::Relaxed);
    }
}
fn counters() -> &'static BarrierCounts {
    &crate::runtime::current().barrier_counts
}
pub fn barrier_variant_counts() -> ([u64; 13], u64, bool) {
    let owner = crate::runtime::current();
    owner.barrier_counts.variant_counts(&owner.diagnostics)
}
impl BarrierCounts {
    pub(crate) fn variant_counts(
        &self,
        diagnostics: &crate::diagnostic_settings::RuntimeDiagnostics,
    ) -> ([u64; 13], u64, bool) {
        let (destinations, deletions) = self.snapshot();
        (destinations, deletions, *diagnostics.barrier_counting)
    }
}
pub(super) fn reset_barrier_variant_counts() {
    counters().reset();
}
fn count(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
        Some(n.saturating_add(1))
    });
}

pub(super) fn payload_generation(state: &GcState, payload: *mut u8) -> Option<u8> {
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
pub(super) fn tlab_payload_generation(state: &GcState, address: usize) -> Option<u8> {
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

/// Record an old owner with a young child: its card, its remembered-set
/// entry, and the header flag the inline barrier filter reads. Returns
/// whether the entry is new.
pub(super) fn remember_owner(state: &mut GcState, owner_payload: usize) -> bool {
    state.dirty_cards.insert(owner_payload / GC_CARD_SIZE);
    let payload = GcPayload::from_raw(owner_payload as *mut u8).expect("non-null owner");
    HeapObject::from_payload(payload).set_remembered(true);
    state.remembered_set.insert(owner_payload)
}

/// Record one slot card of a card-remembered reference-array buffer
/// (`remembered::card_array_capacity`). Its header flag stays clear, so later
/// young stores into other cards still reach the barrier. Returns whether the
/// owner entry is new.
pub(super) fn remember_card(state: &mut GcState, owner_payload: usize, card: usize) -> bool {
    state.dirty_cards.insert(owner_payload / GC_CARD_SIZE);
    state.remembered_set.insert_card(owner_payload, card)
}

/// Remember an old owner with a young child stored at `slot` (null when the
/// caller has no slot address): per card for a large reference-array buffer
/// whose slot is known, otherwise as a whole owner.
fn remember_store(state: &mut GcState, owner_payload: usize, slot: *mut u8) -> bool {
    if !slot.is_null()
        && let Some(card) = slot_card(owner_payload, slot as usize)
    {
        return remember_card(state, owner_payload, card);
    }
    remember_owner(state, owner_payload)
}

/// Card of `slot` when `owner_payload` is a card-remembered array buffer and
/// `slot` is one of its element slots.
fn slot_card(owner_payload: usize, slot: usize) -> Option<usize> {
    let owner = HeapObject::from_payload(GcPayload::from_raw(owner_payload as *mut u8)?);
    let capacity = super::remembered::card_array_capacity(owner)?;
    let first =
        owner_payload + willow_abi::array_layout::BUFFER_HEADER_WORDS * GC_STORAGE_WORD_BYTES;
    let index = slot.checked_sub(first)?;
    (index % GC_STORAGE_WORD_BYTES == 0 && index / GC_STORAGE_WORD_BYTES < capacity)
        .then(|| super::remembered::array_slot_card(owner_payload, slot))
}

const CARD_CACHE_ENTRIES: usize = 64;
thread_local! {
    /// Direct-mapped `(owner, card, epoch)` entries this thread recorded under
    /// the heap lock. A hit proves the card is still in the remembered set, so
    /// repeated young stores into a card-remembered buffer skip the lock.
    static CARD_CACHE: [std::cell::Cell<(usize, usize, u64)>; CARD_CACHE_ENTRIES] =
        const { [const { std::cell::Cell::new((0, 0, 0)) }; CARD_CACHE_ENTRIES] };
}
fn card_cache_index(owner: usize, card: usize) -> usize {
    let key = (owner >> 3) ^ card.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    (key ^ (key >> 29)) % CARD_CACHE_ENTRIES
}
fn card_cache_hit(owner: usize, card: usize) -> Option<u64> {
    let epoch = super::remembered::card_epoch();
    CARD_CACHE
        .with(|cache| cache[card_cache_index(owner, card)].get() == (owner, card, epoch))
        .then_some(epoch)
}
/// Call under the heap lock after the card was recorded.
fn card_cache_fill(owner: usize, card: usize) {
    let epoch = super::remembered::card_epoch();
    CARD_CACHE.with(|cache| cache[card_cache_index(owner, card)].set((owner, card, epoch)));
    crate::generated_context::set_remembered_card(owner, card, epoch);
}

pub(super) fn barrier_owner_payload(
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

// Acquire observes the epoch published before phase=1. All phase transitions
// run under STW coordination; stopped remark flushes producers before phase=0.
// 0 = inactive, 1 = concurrent mark, 2 = stopped remark.
// The process-default runtime's gate, exported as
// `willow_abi::GC_MARK_PHASE_SYMBOL`. Generated code reads the serving
// runtime's gate through the context's mark-phase pointer (this symbol for the
// default runtime): inline reference-array stores skip the barrier call only
// while it is 0. Phase changes are crossed
// by mutator handshakes/stops, and the inline load/store pair contains no
// safepoint, exactly like a call to `willow_gc_write_barrier` and its store.
#[unsafe(export_name = "willow_gc_mark_phase")]
pub(super) static GC_MARK_PHASE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

thread_local! {
    /// This thread may have entries in the heap-locked `GcState::satb`, so a
    /// safepoint flush must take the heap lock. Stale `true` only costs one
    /// redundant locked flush.
    static LOCKED_SATB_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(super) fn record_satb_locked(state: &mut GcState, old: *mut u8) {
    let GcState {
        concurrent_cycle,
        satb,
        ..
    } = state;
    if let Some(cycle) = concurrent_cycle {
        let _publication = cycle.termination.enter();
        if cycle.closing.load(Ordering::Acquire) {
            cycle.enqueue(old);
            return;
        }
        if !old.is_null() {
            LOCKED_SATB_PENDING.set(true);
        }
        satb.record(std::thread::current().id(), old as usize, |value| {
            cycle.enqueue_satb_batch(value)
        });
    }
}

/// Publish a concurrent-mark deletion (`old`) and new edge (`value`) into the
/// cached cycle's shard buffers without the heap lock.
/// Returns false when the caller must use the heap-locked path instead: no
/// concurrent phase, a stale cache, or shards sealed by closure/remark. The
/// validity checks run under the shard lock, and so does every queue
/// publication, so a seal's flush observes all of them.
fn publish_marking_unlocked_in(
    owner: &crate::runtime::Runtime,
    old: *mut u8,
    value: *mut u8,
    phase: u8,
) -> bool {
    if phase != willow_abi::barrier_buffer::CONCURRENT_PHASE as u8 {
        return false;
    }
    // May take the heap lock on a cache miss: before any shard lock.
    let Some((generation, cycle)) = marking::cached_cycle_in(&owner.gc) else {
        return false;
    };
    let _publication = cycle.termination.enter();
    cycle
        .satb_shards
        .with_current_in(
            &owner.gc,
            || {
                owner.gc.assist_cycle_generation.load(Ordering::Acquire) == generation
                    && !cycle.closing.load(Ordering::Acquire)
            },
            |buffer| {
                buffer.record(old as usize, |values| cycle.enqueue_satb_batch(values));
                if !value.is_null() {
                    cycle.enqueue(value);
                }
                if std::ptr::eq(owner, crate::runtime::current())
                    && safepoint::current_thread_is_registered_in(&owner.gc)
                {
                    crate::generated_context::set_barrier_buffer(
                        buffer.admit_inline(&cycle.closing),
                    );
                }
            },
        )
        .is_some()
}

/// Log a logical reference deletion without a stable physical slot. Does not
/// reach a safepoint or trace payloads, so callers may hold a container lock.
///
/// Kept out of line: inlined into a caller's retry loop, the served-runtime
/// lookup is hoisted ahead of the loop and paid even by non-reference
/// operations that never reach this call.
#[inline(never)]
pub(crate) fn satb_delete(old: *mut u8) {
    satb_delete_in(crate::runtime::current(), old);
}

/// Explicit-owner deletion for scheduler retirement outside mutator TLS.
/// Private cycles must never publish their buffer into the default generated
/// context or leave a pending flag for the default mutator's destructor.
pub(crate) fn satb_delete_in(owner: &crate::runtime::Runtime, old: *mut u8) {
    let phase = owner.mark_phase().load(Ordering::Acquire);
    if old.is_null() || phase == 0 {
        return;
    }
    // Looked up only past the inactive-phase exit.
    let default_owner = std::ptr::eq(owner, crate::runtime::current());
    if *owner.diagnostics.barrier_counting {
        count(&owner.barrier_counts.deletions);
    }
    let gc = &*owner.gc;
    // An enrolled private mutator uses its own buffer and stop handshake.
    // Unregistered service callers retain immediate publication under the
    // heap lock; they must never borrow another runtime's TLS buffer.
    let enrolled = default_owner || safepoint::current_thread_is_registered_in(gc);
    if !enrolled || !publish_marking_unlocked_in(owner, old, std::ptr::null_mut(), phase) {
        let mut heap = gc.heap.lock().unwrap();
        if default_owner {
            record_satb_locked(&mut heap, old);
        } else if let Some(cycle) = &heap.concurrent_cycle {
            // No mutator TLS is enrolled for a private retirement. Publish
            // immediately under the same heap/termination protocol as closing.
            let _publication = cycle.termination.enter();
            cycle.enqueue(old);
        }
    }
}

pub(super) fn flush_satb_current(retire: bool) {
    flush_satb_current_in(crate::runtime::current(), retire);
}

pub(super) fn flush_satb_current_in(owner: &crate::runtime::Runtime, retire: bool) {
    crate::generated_context::set_barrier_buffer(0);
    // `Runtime::mark_phase` keeps the process ABI gate independent of lazy heap
    // initialization. A
    // private owner uses only its own phase; neither gate mirrors the other.
    let phase = owner.mark_phase().load(Ordering::Acquire);
    if !retire && phase == 0 {
        return;
    }
    crate::gc_mark_queue::assert_no_queue_lock_held("SATB buffer flush");
    if phase == 1
        && let Some((generation, cycle)) = marking::cached_cycle_in(&owner.gc)
    {
        let _publication = cycle.termination.enter();
        // Flush only this mutator's buffer; closure seals all registrations.
        cycle.satb_shards.with_current_in(
            &owner.gc,
            || owner.gc.assist_cycle_generation.load(Ordering::Acquire) == generation,
            |buffer| buffer.flush(|values| cycle.enqueue_satb_batch(values)),
        );
        if retire {
            cycle
                .satb_shards
                .retire_current(|values| cycle.enqueue_satb_batch(values));
        }
    }
    if !retire && !LOCKED_SATB_PENDING.get() {
        return;
    }
    LOCKED_SATB_PENDING.set(false);
    let mut state = owner.gc.heap.lock().unwrap();
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

/// Publish every pending deletion and seal the cycle's shard buffers, so all
/// later publications take the heap lock: closure and remark rely on that.
pub(super) fn flush_satb_all_locked(coord: &GcCoord, state: &mut GcState) {
    assert!(coord.stop_requested, "foreign SATB drain requires STW");
    let current = std::thread::current().id();
    assert!(
        coord
            .mutators
            .keys()
            .all(|id| *id == current || coord.parked.contains(id)),
        "foreign SATB owner must park or unregister before drain"
    );
    let GcState {
        concurrent_cycle,
        satb,
        ..
    } = state;
    let cycle = concurrent_cycle.as_ref();
    satb.flush_all(|value| {
        cycle
            .expect("pending SATB entries require an active epoch")
            .enqueue_satb_batch(value);
    });
    if let Some(cycle) = cycle {
        let _publication = cycle.termination.enter();
        cycle
            .satb_shards
            .seal_stopped(coord, |values| cycle.enqueue_satb_batch(values));
    }
}

fn count_barrier_call(gc: &GcRuntime) {
    let _ = gc
        .write_barrier_calls
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |calls| {
            Some(calls.saturating_add(1))
        });
}

/// Fused pre-store barrier: retain the overwritten reference for SATB, publish
/// the new edge for the existing incremental marker, and remember old-to-young
/// edges. The caller must capture `old_value` before overwriting, including
/// removals/null stores. Only proven-null initialization passes null as old.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_write_barrier(
    owner: *mut u8,
    old_value: *mut u8,
    value: *mut u8,
    destination_kind: i64,
) {
    write_barrier(
        owner,
        std::ptr::null_mut(),
        old_value,
        value,
        destination_kind,
    );
}

/// [`willow_gc_write_barrier`] for a store into element `slot` of the
/// reference-array buffer whose payload starts at `buffer`. Large old buffers
/// are remembered per slot card, so a minor scans only the written cards.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_array_write_barrier(
    buffer: *mut u8,
    slot: *mut u8,
    old_value: *mut u8,
    value: *mut u8,
    destination_kind: i64,
) {
    write_barrier(buffer, slot, old_value, value, destination_kind);
}

#[inline]
fn write_barrier(
    owner: *mut u8,
    slot: *mut u8,
    old_value: *mut u8,
    value: *mut u8,
    destination_kind: i64,
) {
    write_barrier_in(
        crate::runtime::current(),
        owner,
        slot,
        old_value,
        value,
        destination_kind,
    );
}

#[inline]
pub(crate) fn write_barrier_in(
    runtime_owner: &crate::runtime::Runtime,
    owner: *mut u8,
    slot: *mut u8,
    old_value: *mut u8,
    value: *mut u8,
    destination_kind: i64,
) {
    // Thread-local mutator caches (card cache, default-path SATB recording)
    // belong to the runtime this thread serves. Asked only off the common
    // inactive-phase/no-young-edge exits, so those pay no runtime lookup.
    let serves_thread = || std::ptr::eq(runtime_owner, crate::runtime::current());
    let value_has_header = destination_kind & willow_abi::GC_STORE_VALUE_HAS_HEADER != 0;
    let owner_has_header = destination_kind & willow_abi::GC_STORE_OWNER_HAS_HEADER != 0;
    let destination_kind = destination_kind
        & !(willow_abi::GC_STORE_VALUE_HAS_HEADER | willow_abi::GC_STORE_OWNER_HAS_HEADER);
    let phase = runtime_owner.mark_phase().load(Ordering::Acquire);
    let marking_active = phase != 0;
    // A null store creates no generational edge; its deletion is relevant
    // only during SATB marking. Like null/null, an inactive deletion is a
    // no-op and is excluded from the processed-barrier telemetry counter.
    if value.is_null() && (old_value.is_null() || !marking_active) {
        return;
    }
    if *runtime_owner.diagnostics.barrier_counting
        && let Some(counter) = destination_kind
            .checked_sub(1)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| runtime_owner.barrier_counts.destinations.get(i))
    {
        count(counter);
    }
    let gc = &*runtime_owner.gc;
    count_barrier_call(gc);
    // With no nursery ever published there are no old-to-young edges. A value
    // flagged as a GC payload start has a readable header; an old one cannot
    // create such an edge either (the inline filter trusts the same byte).
    let young_edge_possible = !value.is_null()
        && gc.tlab_ever_allocated.load(Ordering::Acquire)
        && !(value_has_header
            && GcPayload::from_raw(value).is_some_and(|value| {
                HeapObject::from_payload(value).generation() == GC_GENERATION_OLD
            }))
        && (!owner_has_header
            || GcPayload::from_raw(owner).is_some_and(|owner| {
                let owner = HeapObject::from_payload(owner);
                owner.generation() == GC_GENERATION_OLD && !owner.remembered()
            }));
    // Initial STW crosses pre-activation barrier/store pairs before root
    // snapshots; active epochs publish under a private buffer lock, or
    // under the heap lock once closure or remark sealed the shards.
    let published = marking_active
        && (serves_thread() || safepoint::current_thread_is_registered_in(gc))
        && publish_marking_unlocked_in(runtime_owner, old_value, value, phase);
    if !young_edge_possible && (published || !marking_active) {
        return;
    }
    // A store into an already recorded card of an old card-remembered buffer
    // needs no lock: the cache entry proves the card is recorded for this
    // epoch, so the next minor scans it whatever the value is. Compiled
    // array stores carry no value-header proof, so none is required.
    let cached_card = (young_edge_possible
        && owner_has_header
        && !slot.is_null()
        && (published || !marking_active)
        && serves_thread())
    .then(|| slot_card(owner as usize, slot as usize))
    .flatten();
    if let Some(card) = cached_card
        && let Some(epoch) = card_cache_hit(owner as usize, card)
    {
        // Preserve the exact epoch observed by the TLS lookup, not a later
        // reload that might follow an invalidation on another thread.
        crate::generated_context::set_remembered_card(owner as usize, card, epoch);
        <super::path_observation::RuntimePathObservation as super::path_observation::PathObservation>::card_cache_hit(
            || &gc.path_observation,
        );
        return;
    }
    let _interval = crate::gc_telemetry::observations::Span::new_in(
        &runtime_owner.telemetry.observations,
        crate::gc_telemetry::observations::Kind::BarrierSlow,
    );
    let mut state = gc.heap.lock().unwrap();
    if marking_active && !published {
        if serves_thread() {
            record_satb_locked(&mut state, old_value);
        } else if let Some(cycle) = &state.concurrent_cycle {
            // Private publication must not set the default mutator's pending flag.
            let _publication = cycle.termination.enter();
            cycle.enqueue(old_value);
        }
        if let Some(cycle) = &state.concurrent_cycle {
            cycle.enqueue(value);
        }
    }
    if !young_edge_possible
        || tlab_payload_generation(&state, value as usize) != Some(GC_GENERATION_YOUNG)
    {
        return;
    }
    if let Some(owner_payload) = barrier_owner_payload(&state, owner, destination_kind) {
        if remember_store(&mut state, owner_payload, slot) {
            state.write_barrier_hits = state.write_barrier_hits.saturating_add(1);
        }
        if let Some(card) = cached_card
            && owner_payload == owner as usize
            && state
                .remembered_set
                .owner_cards(owner_payload)
                .is_some_and(|cards| cards.contains(&card))
        {
            card_cache_fill(owner_payload, card);
        }
    }
}

/// Runtime counterpart of the generated inline barrier filter
/// (`emit_filtered_reference_store`, `OldValueSkips`) for stores whose owner
/// is the payload start of a GC object. While marking is inactive, only a
/// non-null young value stored into an old, unremembered owner needs the
/// locked barrier; skipping the rest keeps runtime container stores off the
/// heap lock. Like the generated filter, the phase load and
/// the caller's store contain no safepoint, and a stale unremembered byte
/// only takes the conservative locked path.
pub(crate) fn write_barrier_object_owner(
    owner: *mut u8,
    old_value: *mut u8,
    value: *mut u8,
    destination_kind: i64,
) {
    if !object_store_needs_barrier(owner, value) {
        return;
    }
    willow_gc_write_barrier(
        owner,
        old_value,
        value,
        destination_kind
            | willow_abi::GC_STORE_VALUE_HAS_HEADER
            | willow_abi::GC_STORE_OWNER_HAS_HEADER,
    );
}

/// [`write_barrier_object_owner`] for a store into element `slot` of the
/// reference-array buffer `buffer`, so a large old buffer records only the
/// slot's card.
pub(crate) fn write_barrier_array_slot(
    buffer: *mut u8,
    slot: *mut u8,
    old_value: *mut u8,
    value: *mut u8,
) {
    // A cached card is already guaranteed to be scanned at the next minor.
    // The runtime caller supplies an actual array element slot, so its card
    // can be calculated without reclassifying either object. Observe the mark
    // phase first: active SATB epochs must still publish deletion/insertion.
    // There is no safepoint between this check and the caller's atomic store.
    if crate::runtime::current()
        .mark_phase()
        .load(Ordering::Acquire)
        == 0
        && card_cache_hit(
            buffer as usize,
            super::remembered::array_slot_card(buffer as usize, slot as usize),
        )
        .is_some()
    {
        <super::path_observation::RuntimePathObservation as super::path_observation::PathObservation>::card_cache_hit(
            || &runtime().path_observation,
        );
        return;
    }
    if !object_store_needs_barrier(buffer, value) {
        return;
    }
    write_barrier(
        buffer,
        slot,
        old_value,
        value,
        GcStoreDestination::ArrayElement as i64
            | willow_abi::GC_STORE_VALUE_HAS_HEADER
            | willow_abi::GC_STORE_OWNER_HAS_HEADER,
    );
}

#[inline]
fn object_store_needs_barrier(owner: *mut u8, value: *mut u8) -> bool {
    if crate::runtime::current()
        .mark_phase()
        .load(Ordering::Acquire)
        != 0
    {
        return true;
    }
    // A null or old child cannot create an old-to-young edge. Test the child
    // before reading the owner, so this common case pays no owner-header loads.
    let Some(value) = GcPayload::from_raw(value) else {
        return false;
    };
    if HeapObject::from_payload(value).generation() == GC_GENERATION_OLD {
        return false;
    }
    let Some(owner) = GcPayload::from_raw(owner) else {
        return false;
    };
    let owner = HeapObject::from_payload(owner);
    owner.generation() == GC_GENERATION_OLD && !owner.remembered()
}

// Frozen owner-first generational baseline for the feature-disabled benchmark.
#[cfg(test)]
fn may_create_young_edge(owner: *mut u8, value: *mut u8) -> bool {
    let (Some(owner), Some(value)) = (GcPayload::from_raw(owner), GcPayload::from_raw(value))
    else {
        return false;
    };
    let owner = HeapObject::from_payload(owner);
    owner.generation() == GC_GENERATION_OLD
        && !owner.remembered()
        && HeapObject::from_payload(value).generation() != GC_GENERATION_OLD
}

/// Benchmark reconstruction of the generational-only filtered store and its
/// SATB extension. Both variants use the same store loop. False preserves the
/// frozen owner-first baseline; true uses the production child-first filter,
/// including its phase observation and pre-write deletion logging.
#[cfg(test)]
#[inline(never)]
pub(super) unsafe fn reference_store_batch<const SATB: bool>(
    owner: *mut u8,
    slots: &mut [std::sync::atomic::AtomicPtr<u8>],
    values: [*mut u8; 2],
    passes: usize,
) {
    for pass in 0..passes {
        for (index, slot) in slots.iter_mut().enumerate() {
            let value = values[(index + pass) & 1];
            let needs_barrier = if SATB {
                object_store_needs_barrier(owner, value)
            } else {
                may_create_young_edge(owner, value)
            };
            if needs_barrier {
                let old = if SATB {
                    slot.load(Ordering::Acquire)
                } else {
                    std::ptr::null_mut()
                };
                willow_gc_write_barrier(
                    owner,
                    old,
                    value,
                    GcStoreDestination::ObjectField as i64 | willow_abi::GC_STORE_VALUE_HAS_HEADER,
                );
            }
            slot.store(value, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    #[test]
    fn satb_child_first_filter_preserves_phase_and_generational_truth_table() {
        let _guard = isolated_runtime();
        reset_internal_for_test();
        let owner = willow_alloc(8);
        let value = willow_alloc(8);
        let owner_object = HeapObject::from_payload(GcPayload::from_raw(owner).unwrap());
        let value_object = HeapObject::from_payload(GcPayload::from_raw(value).unwrap());
        for phase in [0, 1, 2] {
            with_stw(crate::gc_telemetry::stops::StopReason::Remark, |_, _| {
                crate::runtime::current()
                    .mark_phase()
                    .store(phase, Ordering::Release);
                for generation in [GC_GENERATION_YOUNG, GC_GENERATION_OLD] {
                    owner_object.set_generation(generation);
                    for remembered in [false, true] {
                        owner_object.set_remembered(remembered);
                        for value_generation in [GC_GENERATION_YOUNG, GC_GENERATION_OLD] {
                            value_object.set_generation(value_generation);
                            for owner in [owner, std::ptr::null_mut()] {
                                for value in [value, std::ptr::null_mut()] {
                                    assert_eq!(
                                        object_store_needs_barrier(owner, value),
                                        phase != 0 || may_create_young_edge(owner, value)
                                    );
                                }
                            }
                        }
                    }
                }
                if phase != 0 {
                    // Active marking bypasses generational header classification.
                    assert!(object_store_needs_barrier(
                        std::ptr::dangling_mut::<u8>(),
                        std::ptr::dangling_mut::<u64>().cast::<u8>()
                    ));
                }
            });
        }
        owner_object.set_generation(GC_GENERATION_OLD);
        value_object.set_generation(GC_GENERATION_OLD);
        with_stw(crate::gc_telemetry::stops::StopReason::Remark, |_, _| {
            crate::runtime::current()
                .mark_phase()
                .store(0, Ordering::Release)
        });
        reset_internal_for_test();
    }
}

#[cfg(test)]
mod count_owner_tests {
    use super::*;

    #[test]
    fn private_write_barriers_remember_only_their_own_young_edges() {
        let first = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        let second = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        let rendezvous = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for runtime_owner in [&first, &second] {
                let rendezvous = &rendezvous;
                scope.spawn(crate::runtime::serving(move || {
                    let mut tlab = GcTlabState::new();
                    let registration = MutatorRegistration::new_in(runtime_owner);
                    let old = allocate_old_in(runtime_owner, 0, 0, 8, 0);
                    let young =
                        tlab_alloc_slow_in(runtime_owner, &raw mut tlab as usize, 0, 0, 8, 0);
                    rendezvous.wait();
                    write_barrier_in(
                        runtime_owner,
                        old,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        young,
                        GcStoreDestination::ObjectField as i64
                            | willow_abi::GC_STORE_VALUE_HAS_HEADER
                            | willow_abi::GC_STORE_OWNER_HAS_HEADER,
                    );
                    let state = runtime_owner.gc.heap.lock().unwrap();
                    assert_eq!(state.remembered_set.len(), 1);
                    assert!(state.remembered_set.contains(&(old as usize)));
                    assert_eq!(state.write_barrier_hits, 1);
                    drop(state);
                    drop(registration);
                }));
            }
        });
    }

    #[test]
    fn private_write_barriers_publish_only_to_selected_phase_and_heap() {
        for phase in [0, 1, 2] {
            let first = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
            let second = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
            let cycle = Arc::new(ConcurrentCycle::new(
                std::iter::empty(),
                Default::default(),
                Default::default(),
                0,
            ));
            cycle.late_published.store(false, Ordering::Release);
            marking::set_concurrent_cycle_in(
                &first.gc,
                &mut first.gc.heap.lock().unwrap(),
                Some(cycle.clone()),
            );
            first.gc.mark_phase().store(phase, Ordering::Release);
            let pending = LOCKED_SATB_PENDING.get();
            let old = std::ptr::without_provenance_mut(0x1000);
            write_barrier_in(
                &first,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                old,
                std::ptr::null_mut(),
                GcStoreDestination::GlobalStatic as i64,
            );
            assert_eq!(
                cycle.unindexed.lock().unwrap().len(),
                usize::from(phase != 0)
            );
            assert_eq!(
                first.gc.write_barrier_calls.load(Ordering::Relaxed),
                u64::from(phase != 0)
            );
            assert_eq!(second.gc.write_barrier_calls.load(Ordering::Relaxed), 0);
            assert_eq!(second.gc.mark_phase().load(Ordering::Acquire), 0);
            assert_eq!(LOCKED_SATB_PENDING.get(), pending);
            first.gc.mark_phase().store(0, Ordering::Release);
            marking::set_concurrent_cycle_in(&first.gc, &mut first.gc.heap.lock().unwrap(), None);
        }
    }

    #[test]
    fn enrolled_private_root_deletion_avoids_coord_and_heap_locks() {
        let owner = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        let cycle = Arc::new(ConcurrentCycle::new(
            std::iter::empty(),
            Default::default(),
            Default::default(),
            0,
        ));
        cycle.late_published.store(false, Ordering::Release);
        marking::set_concurrent_cycle_in(
            &owner.gc,
            &mut owner.gc.heap.lock().unwrap(),
            Some(cycle.clone()),
        );
        owner.gc.mark_phase().store(1, Ordering::Release);
        // This value remains unindexed, so queue publication does not read it.
        let address = 0x1000usize;
        owner
            .gc
            .runtime_roots
            .add(std::ptr::without_provenance_mut(address));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (start_tx, start_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let owner = &owner;
            scope.spawn(crate::runtime::serving(move || {
                let _registration = MutatorRegistration::new_in(owner);
                // A cold cycle lookup is allowed to acquire the heap lock.
                assert!(marking::cached_cycle_in(&owner.gc).is_some());
                ready_tx.send(()).unwrap();
                start_rx.recv().unwrap();
                let pending = LOCKED_SATB_PENDING.get();
                remove_runtime_root_in(owner, std::ptr::without_provenance_mut(address));
                assert_eq!(LOCKED_SATB_PENDING.get(), pending);
                done_tx.send(()).unwrap();
                // Drop flushes the enrolled buffer before unregistering.
            }));
            ready_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            let coord = owner.gc.coord.0.lock().unwrap();
            let heap = owner.gc.heap.lock().unwrap();
            start_tx.send(()).unwrap();
            let completed = done_rx.recv_timeout(std::time::Duration::from_secs(5));
            // Release before asserting so a regression cannot hang scope join.
            drop(heap);
            drop(coord);
            assert!(
                completed.is_ok(),
                "enrolled root deletion acquired a shared GC lock"
            );
        });
        assert!(owner.gc.runtime_roots.snapshot().is_empty());
        assert!(owner.gc.coord.0.lock().unwrap().mutators.is_empty());
        assert_eq!(cycle.unindexed.lock().unwrap().len(), 1);
        owner.gc.mark_phase().store(0, Ordering::Release);
        marking::set_concurrent_cycle_in(&owner.gc, &mut owner.gc.heap.lock().unwrap(), None);
    }

    #[test]
    fn private_runtime_barrier_counts_saturate_and_reset_independently() {
        let first = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        let second = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        std::thread::scope(|scope| {
            for (owner, rounds) in [(&first, 64), (&second, 192)] {
                scope.spawn(crate::runtime::serving(move || {
                    for _ in 0..rounds {
                        for counter in &owner.barrier_counts.destinations {
                            count(counter);
                        }
                        count(&owner.barrier_counts.deletions);
                    }
                }));
            }
        });
        assert_eq!(first.barrier_counts.snapshot(), ([64; 13], 64));
        assert_eq!(second.barrier_counts.snapshot(), ([192; 13], 192));
        first
            .barrier_counts
            .deletions
            .store(u64::MAX, Ordering::Relaxed);
        count(&first.barrier_counts.deletions);
        assert_eq!(first.barrier_counts.snapshot().1, u64::MAX);
        first.barrier_counts.reset();
        assert_eq!(first.barrier_counts.snapshot(), ([0; 13], 0));
        assert_eq!(second.barrier_counts.snapshot(), ([192; 13], 192));
        assert!(std::sync::LazyLock::get(&first.gc).is_none());
        assert!(std::sync::LazyLock::get(&second.gc).is_none());
    }
    #[test]
    fn private_runtime_root_deletions_publish_only_to_their_mark_cycle() {
        for phase in [0, 1, 2] {
            let first = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
            let second = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
            let make_cycle = || {
                Arc::new(ConcurrentCycle::new(
                    std::iter::empty(),
                    Default::default(),
                    Default::default(),
                    0,
                ))
            };
            let a = make_cycle();
            let b = make_cycle();
            for (owner, cycle) in [(&first, &a), (&second, &b)] {
                cycle.late_published.store(false, Ordering::Release);
                marking::set_concurrent_cycle_in(
                    &owner.gc,
                    &mut owner.gc.heap.lock().unwrap(),
                    Some(cycle.clone()),
                );
                owner.gc.mark_phase().store(phase, Ordering::Release);
            }
            // An unindexed address is queued without dereferencing it. Both owners
            // deliberately retain the same address to detect wrong-registry removal.
            let object = std::ptr::without_provenance_mut::<u8>(0x1000);
            first.gc.runtime_roots.add(object);
            second.gc.runtime_roots.add(object);
            let pending_before = LOCKED_SATB_PENDING.get();
            remove_runtime_root_in(&first, object);
            a.satb_shards.seal_without_drain();
            assert_eq!(first.gc.runtime_roots.snapshot().len(), 0);
            assert_eq!(second.gc.runtime_roots.snapshot().len(), 1);
            assert_eq!(a.unindexed.lock().unwrap().len(), usize::from(phase != 0));
            assert_eq!(b.unindexed.lock().unwrap().len(), 0);
            assert_eq!(LOCKED_SATB_PENDING.get(), pending_before);
            remove_runtime_root_in(&second, object);
            b.satb_shards.seal_without_drain();
            assert_eq!(b.unindexed.lock().unwrap().len(), usize::from(phase != 0));
            // Removal after sealing uses direct publication, never default TLS.
            remove_runtime_root_in(&second, object);
            assert_eq!(LOCKED_SATB_PENDING.get(), pending_before);
            for owner in [&first, &second] {
                owner.gc.mark_phase().store(0, Ordering::Release);
                marking::set_concurrent_cycle_in(
                    &owner.gc,
                    &mut owner.gc.heap.lock().unwrap(),
                    None,
                );
            }
        }
    }
}
