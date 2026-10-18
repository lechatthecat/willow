//! Bounded normal old sweep with a shared resumable cursor; the classified
//! stopped Recovery path retains its compatibility sweep. New reclaimed spans
//! wait for completion and the next mark epoch before allocator reuse.
use super::*;
mod batches;
pub(super) fn assist() {
    batches::assist();
}

/// Region flag/state writes plus one candidate-index key. Tree-node traffic
/// is accounted by lock/work time rather than pretending to be payload bytes.
pub(super) const REGION_PUBLICATION_BYTES: u64 = (2 * std::mem::size_of::<bool>()
    + std::mem::size_of::<region::SweepState>()
    + std::mem::size_of::<(usize, usize)>())
    as u64;

pub(super) struct SweepPlan {
    old: usize,
    chunks: usize,
    epoch: u64,
    marking: Option<Arc<ConcurrentCycle>>,
}

pub(super) fn prepare(state: &mut GcState, marking: Option<Arc<ConcurrentCycle>>) -> SweepPlan {
    assert!(state.sweeping.is_none());
    crate::gc_telemetry::milestones::reached(crate::gc_telemetry::GcStage::MarkComplete);
    state.sweeping = Some(std::thread::current().id());
    state.pending_sweep_chunks = 0..state.tlab_chunks.len();
    publish_collection_busy(state);
    state.major_collections = state.major_collections.saturating_add(1);
    // Active buffers copied a clear sweep_pending flag.
    old_lab::retire_all(state);
    for (index, region) in state.old_regions.iter_mut().enumerate() {
        region.sweep_pending = true;
        region.sweep_quarantined = true;
        region.sweep_state = region::SweepState {
            epoch: marking
                .as_ref()
                .map_or(state.major_collections, |cycle| cycle.assist_epoch),
            phase: region::SweepPhase::Unswept,
        };
        state
            .old_region_candidates
            .remove(&(region.available_span(), index));
    }
    SweepPlan {
        old: state.old_regions.len(),
        chunks: state.tlab_chunks.len(),
        epoch: state.major_collections,
        marking,
    }
}

/// Only region flags are published under normal remark. Object reclamation
/// starts in execute(), after with_stw has released the stop gate.
pub(super) fn prepare_bounded(
    state: &mut GcState,
    marking: Arc<ConcurrentCycle>,
    budget: &mut old_cycle::RemarkBudget,
    stop_work: &mut crate::gc_telemetry::stops::StopWorkV2,
) -> Option<SweepPlan> {
    if !old_lab::retire_bounded(state, budget, stop_work) {
        return None;
    }
    if !publish_regions_bounded(state, &marking, budget, crate::gc_telemetry::timestamp_ns) {
        return None;
    }
    assert!(state.sweeping.is_none());
    state.sweeping = Some(std::thread::current().id());
    state.pending_sweep_chunks = 0..state.tlab_chunks.len();
    publish_collection_busy(state);
    state.major_collections = state.major_collections.saturating_add(1);
    Some(SweepPlan {
        old: state.old_regions.len(),
        chunks: state.tlab_chunks.len(),
        epoch: state.major_collections,
        marking: Some(marking),
    })
}

/// The heap lock serializes this cursor with append-only old-region growth.
/// Minor collection does not remove/reorder old regions; only the subsequent
/// major sweep does. Flags already published remain set across remark restarts,
/// and new buffers copy them, so their allocations remain black. Retiring all
/// current buffers before each call also freezes buffers with older clear flags.
fn publish_regions_bounded(
    state: &mut GcState,
    marking: &ConcurrentCycle,
    budget: &mut old_cycle::RemarkBudget,
    mut now: impl FnMut() -> u64,
) -> bool {
    let mut cursor = marking.sweep_publication_cursor.load(Ordering::Relaxed);
    assert!(cursor <= state.old_regions.len());
    while cursor < state.old_regions.len() {
        if !budget.reserve(1, REGION_PUBLICATION_BYTES, now()) {
            return false;
        }
        let region = &mut state.old_regions[cursor];
        region.sweep_pending = true;
        region.sweep_quarantined = true;
        region.sweep_state = region::SweepState {
            epoch: marking.assist_epoch,
            phase: region::SweepPhase::Unswept,
        };
        state
            .old_region_candidates
            .remove(&(region.available_span(), cursor));
        budget.record_region_publication();
        cursor += 1;
        // Preserve even the last admitted flag if the deadline check fails.
        marking
            .sweep_publication_cursor
            .store(cursor, Ordering::Relaxed);
    }
    budget.reserve(0, 0, now())
}

/// Publish eligible swept spans only after the next mark epoch has activated.
/// Each visit is region-local and runs after initial stop release; no heap-wide
/// index rebuild or sweep work is hidden in the initial pause.
pub(super) fn release_quarantine(epoch: u64) {
    let count = runtime().heap.lock().unwrap().old_regions.len();
    for index in 0..count {
        let mut state = runtime().heap.lock().unwrap();
        let region = &mut state.old_regions[index];
        if region.sweep_state.phase == region::SweepPhase::Swept
            && region.sweep_quarantined
            && region.reusable_after_epoch <= epoch
        {
            region.sweep_quarantined = false;
            index_old_region(&mut state, index);
        }
    }
}

pub(super) fn stopped(work: &mut crate::gc_telemetry::stops::StopWorkV2) -> usize {
    let plan = prepare(&mut runtime().heap.lock().unwrap(), None);
    execute(plan, work, false)
}

pub(super) fn concurrent(plan: SweepPlan) -> usize {
    assert!(
        !runtime().stop_requested.load(Ordering::Acquire),
        "normal sweep began before mutator resume"
    );
    batches::execute(plan)
}

type PendingDrop = (DropFn, *mut u8);

fn run_drops(drops: &mut Vec<PendingDrop>, epoch: u64) {
    for (hook, payload) in drops.drain(..) {
        // SAFETY: dead storage is quarantined until finish(), and each hook is
        // queued once before its object leaves the live allocation metadata.
        unsafe { run_drop_hook(hook, payload) };
        crate::frame_lifetime::record_sweep(
            crate::frame_lifetime::FrameEventKind::DropCompleted,
            payload as usize,
            epoch,
            0,
        );
    }
}

fn execute(
    plan: SweepPlan,
    work: &mut crate::gc_telemetry::stops::StopWorkV2,
    concurrent: bool,
) -> usize {
    let mut freed = 0;
    let mut drops = Vec::new();
    for index in 0..plan.old {
        freed += sweep_region(
            &mut runtime().heap.lock().unwrap(),
            index,
            work,
            &mut drops,
            plan.marking.as_deref(),
        );
        run_drops(&mut drops, plan.epoch);
        if concurrent {
            between_regions(index);
        }
    }
    let mut dead_chunks = Vec::with_capacity(plan.chunks);
    for index in 0..plan.chunks {
        let (bytes, dead) = sweep_chunk(
            &mut runtime().heap.lock().unwrap(),
            index,
            work,
            &mut drops,
            plan.marking.as_deref(),
        );
        freed += bytes;
        dead_chunks.push(dead);
        run_drops(&mut drops, plan.epoch);
        if concurrent {
            between_regions(plan.old + index);
        }
    }
    let mut state = runtime().heap.lock().unwrap();
    finish(&mut state, &dead_chunks);
    if let Some(marking) = &plan.marking {
        marking
            .phase
            .lock()
            .unwrap()
            .transition(old_cycle::Phase::Idle);
    }
    crate::gc_telemetry::milestones::reached(crate::gc_telemetry::GcStage::SweepComplete);
    // Account reusable storage before another sweep can start.
    crate::frame_lifetime::record_sweep(
        crate::frame_lifetime::FrameEventKind::SweepCompleted,
        0,
        plan.epoch,
        freed as u64,
    );
    freed
}

fn between_regions(_index: usize) {
    #[cfg(test)]
    {
        let hook = *SWEEP_TEST_HOOK.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(hook) = hook {
            hook(_index);
        }
    }
    std::thread::yield_now();
}

#[cfg(test)]
pub(super) static SWEEP_TEST_HOOK: Mutex<Option<fn(usize)>> = Mutex::new(None);

/// Shared by stopped and bounded concurrent sweep, before storage reuse.
fn release_pin(
    records: &mut HashMap<usize, crate::gc_telemetry::GcPinRecord>,
    stats: &mut crate::gc_telemetry::GcSurvivorStats,
    payload: usize,
) {
    if let Some(record) = records.remove(&payload) {
        stats.active_pins -= 1;
        stats.active_pin_bytes -= record.bytes;
        stats.released_pins += 1;
    }
}

fn sweep_region(
    state: &mut GcState,
    index: usize,
    stop_work: &mut crate::gc_telemetry::stops::StopWorkV2,
    drops: &mut Vec<PendingDrop>,
    marking: Option<&ConcurrentCycle>,
) -> usize {
    let mut freed_bytes = 0;
    let sweep_epoch = state.major_collections;
    // Index buffer objects carved during this sweep before visiting them.
    old_lab::retire_region(state, index);
    {
        let GcState {
            old_regions,
            remembered_set,
            young_allocated_bytes,
            allocated_bytes,
            total_frees,
            pin_records,
            survivor_stats,
            ..
        } = &mut *state;
        let region = &mut old_regions[index];
        {
            #[cfg(test)]
            SWEEP_REGION_VISITS.fetch_add(1, Ordering::Relaxed);
            region.mark_bitmap.clear();
            let mut free_spans = Vec::new();
            region.largest_free_span = 0;
            let mut live_end = 0;
            let base = region.base;
            region.allocations.retain(|&offset, span_size| {
                stop_work.swept_objects += 1;
                #[cfg(test)]
                SWEEP_OBJECT_VISITS.fetch_add(1, Ordering::Relaxed);
                // SAFETY: allocation metadata owns a valid header at offset.
                let object = HeapObject::from_raw(unsafe { base.add(offset) }.cast())
                    .expect("region allocation has a non-null header");
                let size = object.size();
                if object.marked()
                    || marking.is_some_and(|cycle| {
                        cycle
                            .objects
                            .retains_allocated(object.payload().as_ptr() as usize)
                    })
                {
                    if offset > live_end {
                        region.largest_free_span = region.largest_free_span.max(offset - live_end);
                        free_spans.push(RegionFreeSpan {
                            offset: live_end,
                            size: offset - live_end,
                        });
                    }
                    live_end = offset + *span_size;
                    region.mark_bitmap.mark(offset);
                    object.clear_mark();
                    true
                } else {
                    if let Some(drop_fn) = lookup_drop(object.type_id()) {
                        crate::frame_lifetime::pending_drop(
                            object.payload().as_ptr() as usize,
                            sweep_epoch,
                        );
                        drops.push((drop_fn, object.payload().as_ptr()));
                    }
                    remembered_set.remove(&(object.payload().as_ptr() as usize));
                    if object.generation() == GC_GENERATION_YOUNG {
                        *young_allocated_bytes = young_allocated_bytes.saturating_sub(size);
                    }
                    release_pin(
                        pin_records,
                        survivor_stats,
                        object.payload().as_ptr() as usize,
                    );
                    note_dead_frame(object, sweep_epoch);
                    object.reclaim_in_place();
                    region.live_bytes = region.live_bytes.saturating_sub(size);
                    freed_bytes += size;
                    *allocated_bytes = allocated_bytes.saturating_sub(size);
                    *total_frees = total_frees.saturating_add(1);
                    false
                }
            });
            region.free_spans = FreeSpans::from(free_spans);
            region.used = live_end;
            region.sweep_pending = false;
            region.sweep_quarantined = true;
        }
    }

    freed_bytes
}

fn sweep_chunk(
    state: &mut GcState,
    chunk_index: usize,
    stop_work: &mut crate::gc_telemetry::stops::StopWorkV2,
    drops: &mut Vec<PendingDrop>,
    marking: Option<&ConcurrentCycle>,
) -> (usize, bool) {
    state.pending_sweep_chunks.start = chunk_index + 1;
    let mut freed_bytes = 0;
    let base = state.tlab_chunks[chunk_index].base;
    let used = state.tlab_chunks[chunk_index].used;
    let owner_state = state.tlab_chunks[chunk_index].owner_state;
    if owner_state.is_some() {
        // Every captured chunk was retired by its initial root handshake.
        // New active chunks belong entirely to a later cycle; do not clear
        // start bits concurrently published by generated allocation.
        return (0, false);
    }
    // A chunk retired after the remark stop is validated before its start
    // bits are rebuilt from the live objects alone (willow-8hq4.21).
    let indexed = index_retired_chunk(&mut state.tlab_chunks[chunk_index]) as u64;
    stop_work.metadata_objects += indexed;
    stop_work.metadata_bytes += indexed * GC_HEADER_SIZE as u64;
    let was_survivor = state.tlab_chunks[chunk_index].kind == RegionKind::Survivor;
    if was_survivor {
        state.survivor_stats.survivor_space_live -=
            state.tlab_chunks[chunk_index].live_bytes as u64;
    }
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
        stop_work.swept_objects += 1;
        if object.marked()
            || marking.is_some_and(|cycle| {
                cycle
                    .objects
                    .retains_allocated(object.payload().as_ptr() as usize)
            })
        {
            object.clear_mark();
            has_live_objects = true;
            has_old_objects |= object.generation() == GC_GENERATION_OLD;
            state.tlab_chunks[chunk_index].mark_bitmap.mark(offset);
            state.tlab_chunks[chunk_index].live_bytes = state.tlab_chunks[chunk_index]
                .live_bytes
                .saturating_add(size);
        } else {
            let payload = object.payload().as_ptr() as usize;
            state.remembered_set.remove(&payload);
            if let Some(drop_fn) = lookup_drop(object.type_id()) {
                crate::frame_lifetime::pending_drop(
                    object.payload().as_ptr() as usize,
                    state.major_collections,
                );
                drops.push((drop_fn, object.payload().as_ptr()));
            }
            release_pin(&mut state.pin_records, &mut state.survivor_stats, payload);
            note_dead_frame(object, state.major_collections);
            object.reclaim_in_place();
            if object.generation() == GC_GENERATION_YOUNG {
                state.young_allocated_bytes = state.young_allocated_bytes.saturating_sub(size);
            }
            freed_bytes += size;
            state.allocated_bytes = state.allocated_bytes.saturating_sub(size);
            state.total_frees = state.total_frees.saturating_add(1);
        }
        offset += size;
    }

    state.tlab_chunks[chunk_index].kind = if has_old_objects {
        RegionKind::Pinned
    } else if state.tlab_chunks[chunk_index].kind == RegionKind::Survivor {
        RegionKind::Survivor
    } else {
        RegionKind::Nursery
    };
    if was_survivor {
        if state.tlab_chunks[chunk_index].kind == RegionKind::Survivor {
            state.survivor_stats.survivor_space_live +=
                state.tlab_chunks[chunk_index].live_bytes as u64;
        } else {
            state.survivor_stats.survivor_space_reserved -=
                state.tlab_chunks[chunk_index].capacity as u64;
        }
    }
    (freed_bytes, !has_live_objects && owner_state.is_none())
}

fn finish(state: &mut GcState, dead_chunks: &[bool]) {
    // Release decisions read `allocations`, and remapping invalidates the
    // region indexes of active buffers.
    old_lab::retire_all(state);
    let regions_before = state.old_regions.len();
    let mut released = 0usize;
    let mut positions = Vec::with_capacity(regions_before);
    let mut next_index = 0;
    state.old_regions.retain_mut(|region| {
        if region.allocations.is_empty() {
            positions.push(None);
            released += region.capacity;
            false
        } else {
            region.sweep_pending = false;
            region.sweep_quarantined = false;
            positions.push(Some(next_index));
            next_index += 1;
            true
        }
    });
    state.old_addresses.remap(&positions);
    state.old_reserved_bytes -= released;
    state.released_bytes = state.released_bytes.saturating_add(released as u64);
    state.old_regions_released = state
        .old_regions_released
        .saturating_add((regions_before - state.old_regions.len()) as u64);
    state.old_region_candidates = state
        .old_regions
        .iter()
        .enumerate()
        .filter(|(_, region)| region.kind == RegionKind::Old)
        .map(|(index, region)| (region.available_span(), index))
        .filter(|(available, _)| *available >= GC_HEADER_SIZE)
        .collect();

    let mut positions = Vec::with_capacity(state.tlab_chunks.len());
    let mut old_index = 0;
    let mut new_index = 0;
    let mut released = 0usize;
    state.tlab_chunks.retain(|chunk| {
        let dead = dead_chunks.get(old_index).copied().unwrap_or(false);
        old_index += 1;
        if dead {
            if chunk.kind == RegionKind::Survivor {
                state.survivor_stats.survivor_space_reserved -= chunk.capacity as u64;
            }
            assert!(chunk.owner_state.is_none());
            positions.push(None);
            let layout =
                Layout::from_size_align(chunk.capacity, std::mem::align_of::<GcHeader>()).unwrap();
            // SAFETY: only initial-plan chunks proven entirely dead are removed;
            // new active/retired chunks are outside dead_chunks and survive.
            unsafe { dealloc(chunk.base, layout) };
            released += chunk.capacity;
            false
        } else {
            positions.push(Some(new_index));
            new_index += 1;
            true
        }
    });
    state.tlab_addresses.remap(&positions);
    state.tlab_reserved_bytes -= released;
    state.released_bytes = state.released_bytes.saturating_add(released as u64);
    state.dirty_cards.clear();
    state.dirty_cards.extend(
        state
            .remembered_set
            .iter()
            .map(|owner| owner / GC_CARD_SIZE),
    );
    state.sweeping = None;
    state.pending_sweep_chunks = 0..0;
    publish_collection_busy(state);
    if std::env::var("WILLOW_GC_VERIFY_REGIONS").is_ok()
        && let Err(message) = verify_old_region_metadata(state)
    {
        panic!("willow gc: region verification failed after major collection: {message}");
    }
}

fn note_dead_frame(object: HeapObject, sweep_epoch: u64) {
    if object.is_async_frame() {
        crate::frame_lifetime::metadata_removed(
            object.payload().as_ptr() as usize,
            object.size(),
            crate::async_frame::frame_is_terminal(object.payload().as_ptr().cast()),
            sweep_epoch,
        );
    }
}

#[cfg(test)]
mod publication_tests {
    use super::*;

    fn cycle() -> ConcurrentCycle {
        ConcurrentCycle::new([], IdSet::default(), IdMap::default(), 0)
    }

    fn append(state: &mut GcState, count: usize) {
        for _ in 0..count {
            state
                .old_regions
                .push(OldRegion::new(RegionKind::Old, 16).unwrap());
        }
    }

    #[test]
    fn region_publication_default_slots_preserve_progress_and_appended_regions() {
        let mut state = GcState::default();
        append(&mut state, 5462);
        let marking = cycle();
        let mut budget = old_cycle::RemarkBudget::new(4096, 1048576, 1000000, 0);
        assert!(!publish_regions_bounded(
            &mut state,
            &marking,
            &mut budget,
            || 0
        ));
        assert_eq!(
            marking.sweep_publication_cursor.load(Ordering::Relaxed),
            4096
        );
        assert!(state.old_regions[..4096].iter().all(|r| r.sweep_pending));
        assert!(state.old_regions[4096..].iter().all(|r| !r.sweep_pending));
        append(&mut state, 17);
        // Exactly the remaining suffix fits: charging the prefix again fails.
        let mut budget = old_cycle::RemarkBudget::new(5462 + 17 - 4096, 1048576, 1000000, 0);
        assert!(publish_regions_bounded(
            &mut state,
            &marking,
            &mut budget,
            || 0
        ));
        assert_eq!(
            marking.sweep_publication_cursor.load(Ordering::Relaxed),
            5479
        );
        assert!(state.old_regions.iter().all(|r| r.sweep_pending));
        assert_eq!(cycle().sweep_publication_cursor.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn region_publication_fake_clock_preserves_every_chunk_and_final_deadline_progress() {
        let mut state = GcState::default();
        append(&mut state, 7);
        let marking = cycle();
        for expected in [2, 4, 6, 7] {
            let mut ticks = 0;
            let mut budget = old_cycle::RemarkBudget::new(4096, 1048576, 3, 0);
            assert!(!publish_regions_bounded(
                &mut state,
                &marking,
                &mut budget,
                || {
                    ticks += 1;
                    // The last round publishes its final region, then times out.
                    if expected == 7 && ticks == 2 {
                        3
                    } else {
                        ticks
                    }
                }
            ));
            assert_eq!(
                marking.sweep_publication_cursor.load(Ordering::Relaxed),
                expected
            );
            assert!(
                state.old_regions[..expected]
                    .iter()
                    .all(|r| r.sweep_pending)
            );
            assert!(
                state.old_regions[expected..]
                    .iter()
                    .all(|r| !r.sweep_pending)
            );
        }
        append(&mut state, 1);
        let mut budget = old_cycle::RemarkBudget::new(1, REGION_PUBLICATION_BYTES, 3, 0);
        assert!(publish_regions_bounded(
            &mut state,
            &marking,
            &mut budget,
            || 1
        ));
        assert_eq!(marking.sweep_publication_cursor.load(Ordering::Relaxed), 8);
        assert!(state.old_regions.iter().all(|r| r.sweep_pending));
    }
}
