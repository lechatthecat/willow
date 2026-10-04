use super::*;

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

// Acquire observes the epoch published before phase=1. Initial root handshakes
// run with SATB active; stopped remark flushes every producer before phase=0.
// 0 = inactive, 1 = concurrent mark, 2 = stopped remark.
// Exported as `willow_abi::GC_MARK_PHASE_SYMBOL`: inline reference-array
// stores skip the barrier call only while it is 0. Phase changes are crossed
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
/// cached cycle's shard buffers without the heap lock (willow-jz15.53).
/// Returns false when the caller must use the heap-locked path instead: no
/// concurrent phase, a stale cache, or shards sealed by closure/remark. The
/// validity checks run under the shard lock, and so does every queue
/// publication, so a seal's flush observes all of them.
fn publish_marking_unlocked(old: *mut u8, value: *mut u8) -> bool {
    if GC_MARK_PHASE.load(Ordering::Acquire) != 1 {
        return false;
    }
    // May take the heap lock on a cache miss: before any shard lock.
    let Some((generation, cycle)) = marking::cached_cycle() else {
        return false;
    };
    cycle
        .satb_shards
        .with_current(
            || {
                GC_MARK_PHASE.load(Ordering::Acquire) == 1
                    && marking::ASSIST_CYCLE_GENERATION.load(Ordering::Acquire) == generation
                    && !cycle.closing.load(Ordering::Acquire)
            },
            |buffer| {
                buffer.record(old as usize, |values| cycle.enqueue_satb_batch(values));
                if !value.is_null() {
                    cycle.enqueue(value);
                }
            },
        )
        .is_some()
}

/// Log a logical reference deletion without a stable physical slot. Does not
/// reach a safepoint or trace payloads, so callers may hold a container lock.
pub(crate) fn satb_delete(old: *mut u8) {
    if !old.is_null()
        && GC_MARK_PHASE.load(Ordering::Acquire) != 0
        && !publish_marking_unlocked(old, std::ptr::null_mut())
    {
        record_satb_locked(&mut runtime().heap.lock().unwrap(), old);
    }
}

pub(super) fn flush_satb_current(retire: bool) {
    if !retire && GC_MARK_PHASE.load(Ordering::Acquire) == 0 {
        return;
    }
    crate::gc_mark_queue::assert_no_queue_lock_held("SATB buffer flush");
    if GC_MARK_PHASE.load(Ordering::Acquire) == 1
        && let Some((generation, cycle)) = marking::cached_cycle()
    {
        // Shards are shared, so this publishes neighbours' entries too; the
        // closure seal publishes whatever no safepoint flushed.
        cycle.satb_shards.with_current(
            || marking::ASSIST_CYCLE_GENERATION.load(Ordering::Acquire) == generation,
            |buffer| buffer.flush(|values| cycle.enqueue_satb_batch(values)),
        );
    }
    if !retire && !LOCKED_SATB_PENDING.get() {
        return;
    }
    LOCKED_SATB_PENDING.set(false);
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

/// Publish every pending deletion and seal the cycle's shard buffers, so all
/// later publications take the heap lock: closure and remark rely on that.
pub(super) fn flush_satb_all_locked(state: &mut GcState) {
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
        cycle
            .satb_shards
            .seal(|values| cycle.enqueue_satb_batch(values));
    }
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
    let value_has_header = destination_kind & willow_abi::GC_STORE_VALUE_HAS_HEADER != 0;
    let destination_kind = destination_kind & !willow_abi::GC_STORE_VALUE_HAS_HEADER;
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
    // With no nursery ever published there are no old-to-young edges. A value
    // flagged as a GC payload start has a readable header; an old one cannot
    // create such an edge either (the inline filter trusts the same byte).
    let young_edge_possible = !value.is_null()
        && runtime().tlab_ever_allocated.load(Ordering::Acquire)
        && !(value_has_header
            && GcPayload::from_raw(value).is_some_and(|value| {
                HeapObject::from_payload(value).generation() == GC_GENERATION_OLD
            }));
    // The activation handshake crosses pre-activation barrier/store pairs
    // before root snapshots; active epochs publish under a shard lock, or
    // under the heap lock once closure or remark sealed the shards.
    let published = marking_active && publish_marking_unlocked(old_value, value);
    if !young_edge_possible && (published || !marking_active) {
        return;
    }
    let mut state = runtime().heap.lock().unwrap();
    if marking_active && !published {
        record_satb_locked(&mut state, old_value);
        if let Some(cycle) = &state.concurrent_cycle {
            cycle.enqueue(value);
        }
    }
    if !young_edge_possible
        || tlab_payload_generation(&state, value as usize) != Some(GC_GENERATION_YOUNG)
    {
        return;
    }
    if let Some(owner_payload) = barrier_owner_payload(&state, owner, destination_kind)
        && remember_owner(&mut state, owner_payload)
    {
        state.write_barrier_hits = state.write_barrier_hits.saturating_add(1);
    }
}

/// Runtime counterpart of the generated inline barrier filter
/// (`emit_filtered_reference_store`, `OldValueSkips`) for stores whose owner
/// is the payload start of a GC object. While marking is inactive, only a
/// non-null young value stored into an old, unremembered owner needs the
/// locked barrier; skipping the rest keeps runtime container stores off the
/// heap lock (willow-jz15.51). Like the generated filter, the phase load and
/// the caller's store contain no safepoint, and a stale unremembered byte
/// only takes the conservative locked path.
pub(crate) fn write_barrier_object_owner(
    owner: *mut u8,
    old_value: *mut u8,
    value: *mut u8,
    destination_kind: i64,
) {
    if GC_MARK_PHASE.load(Ordering::Acquire) == 0 && !may_create_young_edge(owner, value) {
        return;
    }
    willow_gc_write_barrier(
        owner,
        old_value,
        value,
        destination_kind | willow_abi::GC_STORE_VALUE_HAS_HEADER,
    );
}

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
