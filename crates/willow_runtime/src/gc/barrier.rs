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
    tlab_chunk_generation(state, address).flatten()
}

/// [`tlab_payload_generation`] of an address inside a TLAB chunk, or `None`
/// when `address` lies outside every TLAB chunk (so it is not young until a
/// chunk joins the heap). One chunk lookup.
pub(super) fn tlab_chunk_generation(state: &GcState, address: usize) -> Option<Option<u8>> {
    let chunk = find_tlab_chunk(state, address)?;
    // Active generated chunks are exclusively young. Their header prefix is
    // still advancing, so do not inspect unpublished headers here.
    if chunk.owner_state.is_some() {
        return Some(Some(GC_GENERATION_YOUNG));
    }
    Some(object_in_retired_chunk(chunk, address, false).map(|object| object.generation()))
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
pub(super) fn remember_store(state: &mut GcState, owner_payload: usize, slot: *mut u8) -> bool {
    if !slot.is_null()
        && let Some(card) = slot_card(owner_payload, slot as usize)
    {
        return remember_card(state, owner_payload, card);
    }
    remember_owner(state, owner_payload)
}

/// Card of `slot` when `owner_payload` is a card-remembered array buffer and
/// `slot` is one of its element slots.
pub(super) fn slot_card(owner_payload: usize, slot: usize) -> Option<usize> {
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
pub(super) fn card_cache_hit(owner: usize, card: usize) -> Option<u64> {
    let epoch = super::remembered::card_epoch();
    CARD_CACHE
        .with(|cache| cache[card_cache_index(owner, card)].get() == (owner, card, epoch))
        .then_some(epoch)
}

/// A hit proving only that the card is recorded for this epoch: private
/// entries and shared ones (`CARD_EPOCH_SHARED_TAG`). For callers whose
/// value is already published. Returns the cached epoch word.
fn recorded_card_cache_hit(owner: usize, card: usize) -> Option<u64> {
    let epoch = super::remembered::card_epoch();
    let (cached_owner, cached_card, cached_epoch) =
        CARD_CACHE.with(|cache| cache[card_cache_index(owner, card)].get());
    (cached_owner == owner
        && cached_card == card
        && cached_epoch & !willow_abi::generated_context::CARD_EPOCH_SHARED_TAG == epoch)
        .then_some(cached_epoch)
}
/// Call under the heap lock after the card was recorded.
///
/// Both entries prove the card is recorded. An untagged entry also proves
/// the buffer private: the generated array store skips everything,
/// publication included, on such a hit, and publishing a cached private
/// buffer advances the card epoch, which invalidates every entry. A shared
/// buffer (which stays shared) is cached under `CARD_EPOCH_SHARED_TAG`:
/// only stores past their publication filter accept that entry.
pub(super) fn card_cache_fill(owner: usize, card: usize) {
    let mut epoch = super::remembered::card_epoch();
    if owner_shared(owner) {
        epoch |= willow_abi::generated_context::CARD_EPOCH_SHARED_TAG;
    }
    CARD_CACHE.with(|cache| cache[card_cache_index(owner, card)].set((owner, card, epoch)));
    crate::generated_context::set_remembered_card(owner, card, epoch);
}

fn owner_shared(owner: usize) -> bool {
    GcPayload::from_raw(owner as *mut u8)
        .is_some_and(|payload| HeapObject::from_payload(payload).shared())
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
        // Revoked private publication (closure before its first stopped seal,
        // or recovery) publishes directly; a reopened closing epoch buffers
        // here like concurrent marking and its recheck stop flushes these.
        if cycle.buffers_revoked.load(Ordering::Acquire) {
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
/// calling mutator's shard buffer without the heap lock. Both are batched;
/// the shared mark queue sees one publication per full buffer.
/// Returns false when the caller must use the heap-locked path instead: no
/// concurrent phase, a stale cache, private publication revoked, or shards
/// sealed by closure/remark. Closing stores after the first stopped seal
/// reopened the shards publish here too; every recheck stop reseals and
/// drains these buffers before electing termination. The validity checks run
/// under the buffer lock, and so does every queue publication, so a seal's
/// flush observes all of them.
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
    // No termination ownership per store: a buffered entry is not mark work
    // yet. Election requires the stopped seal's drain, which either takes
    // this entry or makes the sealed check below reject it; a full-buffer
    // publication owns termination inside `enqueue_satb_batch`.
    cycle
        .satb_shards
        .with_current_in(
            &owner.gc,
            || {
                owner.gc.assist_cycle_generation.load(Ordering::Acquire) == generation
                    && !cycle.buffers_revoked.load(Ordering::Acquire)
            },
            |buffer| {
                // The new edge shares the deletion's private buffer: both are
                // claimed in batches, and every seal/flush that drains
                // deletions before closure or minor relocation drains them
                // too. A store touches the shared mark queue once per full
                // buffer instead of once per value.
                buffer.record(old as usize, |values| cycle.enqueue_satb_batch(values));
                buffer.record(value as usize, |values| cycle.enqueue_satb_batch(values));
                if std::ptr::eq(owner, crate::runtime::current())
                    && safepoint::current_thread_is_registered_in(&owner.gc)
                {
                    crate::generated_context::set_barrier_buffer(
                        buffer.admit_inline(&cycle.buffers_revoked),
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
        // Flush only this mutator's buffer; closure seals all registrations.
        // Entries stay in the locked buffer until `enqueue_satb_batch`
        // returns, and it owns termination for claimable values itself. An
        // empty or fully marked buffer therefore takes no ownership and does
        // not advance the termination generation at every safepoint.
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
    gc.write_barrier_calls.increment();
}

/// Shards of the processed-barrier telemetry counter. Every runtime barrier
/// call counts, from every worker at once: one shared atomic made each call a
/// contended read-modify-write on one cache line. Each thread adds to its own
/// line; telemetry reads pay for the sum. Only telemetry and tests read it.
const BARRIER_CALL_SHARDS: usize = 16;

#[repr(align(128))]
struct BarrierCallShard(AtomicU64);

/// Processed runtime write-barrier calls of one runtime (telemetry). Relaxed,
/// like the single counter it replaces: a read concurrent with calls may miss
/// in-flight increments. The total saturates at `u64::MAX`.
pub(crate) struct BarrierCallCounter([BarrierCallShard; BARRIER_CALL_SHARDS]);

// Shard choice is a per-thread spreading index, not runtime state.
static NEXT_BARRIER_CALL_SHARD: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
thread_local! {
    static BARRIER_CALL_SHARD: usize =
        NEXT_BARRIER_CALL_SHARD.fetch_add(1, Ordering::Relaxed) % BARRIER_CALL_SHARDS;
}

impl BarrierCallCounter {
    pub(crate) const fn new() -> Self {
        Self([const { BarrierCallShard(AtomicU64::new(0)) }; BARRIER_CALL_SHARDS])
    }

    fn increment(&self) {
        let shard = BARRIER_CALL_SHARD.with(|index| *index);
        // Uncontended per thread; wraps only after 2^64 calls on one shard.
        self.0[shard].0.fetch_add(1, Ordering::Relaxed);
    }

    /// Total processed calls. Same signature as `AtomicU64::load`.
    pub(crate) fn load(&self, order: Ordering) -> u64 {
        self.0
            .iter()
            .fold(0u64, |sum, shard| sum.saturating_add(shard.0.load(order)))
    }

    /// Replace the total (reset, or a test fixture value).
    pub(crate) fn store(&self, value: u64, order: Ordering) {
        for shard in &self.0[1..] {
            shard.0.store(0, order);
        }
        self.0[0].0.store(value, order);
    }
}

/// Publish `value` before generated code stores it into a shared object.
/// The caller proved `value` a non-null GC payload whose shared byte read
/// zero. No store and no fence: the caller then runs its generational filter
/// on the now shared value. Never parks (generated store sites are not GC
/// points), so nothing moves between this call and the store.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_publish_stored_value(value: *mut u8) {
    super::publish::publish_in(
        crate::runtime::current(),
        value,
        super::publish::PublicationPoint::SharedStore,
        false,
    );
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
    // A store into a shared object (or a global, a lock cell) makes the value
    // reachable by other tasks: publish it first. A published young value
    // stays young until the next minor, so the generational record below
    // still applies. Barrier call sites are not GC points (generated code may
    // hold unrooted temporaries), so the publication never parks here.
    if !value.is_null()
        && let Some(point) = publication_point(owner, slot, destination_kind, owner_has_header)
    {
        super::publish::publish_in(runtime_owner, value, point, false);
    }
    // With no nursery ever published there are no old-to-young edges. A value
    // flagged as a GC payload start has a readable header; an old one cannot
    // create such an edge either (the inline filter trusts the same byte).
    let young_edge_possible = !value.is_null()
        && !owner.is_null()
        && destination_kind != GcStoreDestination::GlobalStatic as i64
        && gc.tlab_ever_allocated.load(Ordering::Acquire)
        && (!value_has_header
            || GcPayload::from_raw(value).is_none_or(|value| {
                HeapObject::from_payload(value).generation() != GC_GENERATION_OLD
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
    // array stores carry no value-header proof, so none is required. The
    // value was published above, so a shared buffer's entry serves too.
    let cached_card = (young_edge_possible
        && owner_has_header
        && !slot.is_null()
        && (published || !marking_active)
        && serves_thread())
    .then(|| slot_card(owner as usize, slot as usize))
    .flatten();
    if let Some(card) = cached_card
        && let Some(epoch) = recorded_card_cache_hit(owner as usize, card)
    {
        // Preserve the exact epoch observed by the TLS lookup, not a later
        // reload that might follow an invalidation on another thread.
        crate::generated_context::set_remembered_card(owner as usize, card, epoch);
        <super::path_observation::RuntimePathObservation as super::path_observation::PathObservation>::card_cache_hit(
            || &gc.path_observation,
        );
        return;
    }
    // A registered mutator logs the generational record privately; the next
    // stop applies it before any collector reads the remembered set. Only an
    // unpublished marking store (which must enqueue under the heap lock) or an
    // unregistered thread takes the shared lock below.
    if young_edge_possible
        && (published || !marking_active)
        && super::mutator_log::record_edge_in(
            runtime_owner,
            owner,
            slot,
            value,
            destination_kind,
            owner_has_header && destination_kind != GcStoreDestination::IndirectReference as i64,
            // `young_edge_possible` already rejected a header-proven old value.
            value_has_header,
            serves_thread(),
        )
    {
        return;
    }
    let _interval = crate::gc_telemetry::observations::Span::new_in(
        &runtime_owner.telemetry.observations,
        crate::gc_telemetry::observations::Kind::BarrierSlow,
    );
    let mut state = gc.heap.lock().unwrap();
    super::mutator_log::count_shared_lock();
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

/// Publication point of a store into `owner`, when the destination is
/// reachable by other tasks: a global, a lock-protected cell, or a field,
/// element or entry of a shared object. `owner` is read only when it is a
/// proven payload start (header flag, or an `IndirectReference` whose caller
/// passes the referenced storage's base with the slot).
#[inline]
fn publication_point(
    owner: *mut u8,
    slot: *mut u8,
    destination_kind: i64,
    owner_has_header: bool,
) -> Option<super::publish::PublicationPoint> {
    use super::publish::PublicationPoint;
    const GLOBAL: i64 = GcStoreDestination::GlobalStatic as i64;
    const ASYNC_MUTEX: i64 = GcStoreDestination::AsyncMutexCell as i64;
    const ASYNC_RWLOCK: i64 = GcStoreDestination::AsyncRwLockCell as i64;
    const BLOCKING: i64 = GcStoreDestination::BlockingCell as i64;
    const BLOCKING_RW: i64 = GcStoreDestination::BlockingRwCell as i64;
    const INDIRECT: i64 = GcStoreDestination::IndirectReference as i64;
    match destination_kind {
        GLOBAL => (!owner.is_null()).then_some(PublicationPoint::Global),
        ASYNC_MUTEX | ASYNC_RWLOCK | BLOCKING | BLOCKING_RW => Some(PublicationPoint::LockedState),
        _ => {
            let payload_start =
                owner_has_header || (destination_kind == INDIRECT && !slot.is_null());
            (payload_start
                && GcPayload::from_raw(owner)
                    .is_some_and(|owner| HeapObject::from_payload(owner).shared()))
            .then_some(PublicationPoint::SharedStore)
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
    // A hit also proves the buffer private (shared buffers are never cached),
    // so no publication is skipped. There is no safepoint between this check
    // and the caller's atomic store.
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
    // A null child needs nothing. An old child that is shared, or stored into
    // a private owner, neither needs publishing nor creates an old-to-young
    // edge. Test the child before reading the owner, so the common old-child
    // case pays one owner-header load at most.
    let Some(value) = GcPayload::from_raw(value) else {
        return false;
    };
    let value = HeapObject::from_payload(value);
    let value_old = value.generation() == GC_GENERATION_OLD;
    if value_old && value.shared() {
        return false;
    }
    let Some(owner) = GcPayload::from_raw(owner) else {
        return false;
    };
    let owner = HeapObject::from_payload(owner);
    // A private child stored into a shared owner must be published.
    if owner.shared() && !value.shared() {
        return true;
    }
    // A young child (published or not) stored into an old owner is an
    // old-to-young edge.
    !value_old && owner.generation() == GC_GENERATION_OLD && !owner.remembered()
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

    /// The generated object store (`gc_codegen`, `OldValueSkips`) outside
    /// marking, for a non-null value: whether it calls the store-free
    /// publication entry first, and whether it then calls the barrier.
    fn generated_calls(
        owner_old: bool,
        remembered: bool,
        owner_shared: bool,
        value_old: bool,
        value_shared: bool,
    ) -> (bool, bool) {
        let publishes = owner_shared && !value_shared;
        let young_edge = owner_old && !remembered;
        (publishes, young_edge && !value_old)
    }

    #[test]
    fn generated_and_runtime_filters_agree_on_every_shared_and_generation_state() {
        let _guard = isolated_runtime();
        reset_internal_for_test();
        let owner = willow_alloc(8);
        let value = willow_alloc(8);
        let owner_object = HeapObject::from_payload(GcPayload::from_raw(owner).unwrap());
        let value_object = HeapObject::from_payload(GcPayload::from_raw(value).unwrap());
        let mut cases = 0;
        with_stw(crate::gc_telemetry::stops::StopReason::Remark, |_, _| {
            crate::runtime::current()
                .mark_phase()
                .store(0, Ordering::Release);
            for bits in 0..32u32 {
                let bit = |index: u32| bits & (1 << index) != 0;
                let (owner_old, remembered, owner_shared, value_old, value_shared) =
                    (bit(0), bit(1), bit(2), bit(3), bit(4));
                let generation = |old: bool| {
                    if old {
                        GC_GENERATION_OLD
                    } else {
                        GC_GENERATION_YOUNG
                    }
                };
                owner_object.set_generation(generation(owner_old));
                owner_object.set_remembered(remembered);
                value_object.set_generation(generation(value_old));
                // SAFETY: test-owned headers; the byte is never cleared by
                // the runtime, so restore it through the raw header.
                unsafe {
                    (*owner_object.as_ptr()).shared = u8::from(owner_shared);
                    (*value_object.as_ptr()).shared = u8::from(value_shared);
                }
                let case = format!(
                    "owner old {owner_old} remembered {remembered} shared {owner_shared}, \
                     value old {value_old} shared {value_shared}"
                );
                let (publishes, calls) =
                    generated_calls(owner_old, remembered, owner_shared, value_old, value_shared);
                // The runtime filter does both jobs in its one barrier call.
                assert_eq!(
                    object_store_needs_barrier(owner, value),
                    publishes || calls,
                    "{case}"
                );
                if publishes {
                    willow_gc_publish_stored_value(value);
                    assert!(value_object.shared(), "{case}");
                    // Publication settled, only the generational edge is left.
                    assert_eq!(object_store_needs_barrier(owner, value), calls, "{case}");
                }
                cases += 1;
            }
        });
        assert_eq!(cases, 32);
        // SAFETY: as above.
        unsafe {
            (*owner_object.as_ptr()).shared = 0;
            (*value_object.as_ptr()).shared = 0;
        }
        owner_object.set_remembered(false);
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
                    // The registered mutator logs the edge privately.
                    assert!(
                        runtime_owner
                            .gc
                            .heap
                            .lock()
                            .unwrap()
                            .remembered_set
                            .is_empty()
                    );
                    super::super::mutator_log::flush_current_in(runtime_owner);
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

#[cfg(test)]
mod barrier_call_counter_tests {
    use super::*;

    #[test]
    fn striped_counter_sums_every_thread_and_store_replaces_the_total() {
        let counter = BarrierCallCounter::new();
        const THREADS: u64 = 24;
        const CALLS: u64 = 1000;
        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|| {
                    for _ in 0..CALLS {
                        counter.increment();
                    }
                });
            }
        });
        assert_eq!(counter.load(Ordering::Relaxed), THREADS * CALLS);
        counter.store(7, Ordering::Relaxed);
        assert_eq!(counter.load(Ordering::Relaxed), 7);
        counter.increment();
        assert_eq!(counter.load(Ordering::Relaxed), 8);
        counter.store(u64::MAX, Ordering::Relaxed);
        counter.increment();
        counter.increment();
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX, "saturates");
    }

    #[test]
    fn a_shared_card_entry_proves_only_the_recorded_card() {
        let _runtime = crate::runtime::isolated_runtime();
        let owner = allocate_old_in(crate::runtime::current(), 0, 0, 8, 0) as usize;
        card_cache_fill(owner, 9);
        assert!(card_cache_hit(owner, 9).is_some(), "private: full proof");
        assert!(recorded_card_cache_hit(owner, 9).is_some());
        HeapObject::from_payload(GcPayload::from_raw(owner as *mut u8).unwrap()).set_shared(false);
        // As publication of a cached buffer does.
        crate::runtime::current()
            .card_epoch()
            .fetch_add(1, Ordering::AcqRel);
        card_cache_fill(owner, 9);
        let tagged = recorded_card_cache_hit(owner, 9).expect("shared entry records the card");
        assert_ne!(
            tagged & willow_abi::generated_context::CARD_EPOCH_SHARED_TAG,
            0
        );
        assert!(card_cache_hit(owner, 9).is_none(), "never proves privacy");
        assert!(recorded_card_cache_hit(owner, 10).is_none());
    }
}
