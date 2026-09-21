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
pub(super) static GC_MARK_PHASE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

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

pub(super) fn flush_satb_current(retire: bool) {
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

pub(super) fn flush_satb_all_locked(state: &mut GcState) {
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
