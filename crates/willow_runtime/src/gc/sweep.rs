//! Region-at-a-time old sweep. Heap metadata is protected per region; mutators
//! run between visits. Reclaimed spans/regions stay quarantined until all epoch
//! readers are gone and the sweep batch is finalized.
use super::*;

pub(super) struct SweepPlan {
    old: usize,
    chunks: usize,
    marking: Option<Arc<ConcurrentCycle>>,
}

pub(super) fn prepare(state: &mut GcState, marking: Option<Arc<ConcurrentCycle>>) -> SweepPlan {
    assert!(state.sweeping.is_none());
    state.sweeping = Some(std::thread::current().id());
    state.major_collections = state.major_collections.saturating_add(1);
    for region in &mut state.old_regions {
        region.sweep_pending = true;
    }
    SweepPlan {
        old: state.old_regions.len(),
        chunks: state.tlab_chunks.len(),
        marking,
    }
}

pub(super) fn stopped(work: &mut crate::gc_telemetry::stops::StopWorkV2) -> usize {
    let plan = prepare(&mut runtime().heap.lock().unwrap(), None);
    execute(plan, work, false)
}

pub(super) fn concurrent(plan: SweepPlan) -> usize {
    execute(
        plan,
        &mut crate::gc_telemetry::stops::StopWorkV2::default(),
        true,
    )
}

type PendingDrop = (DropFn, *mut u8);

fn run_drops(drops: &mut Vec<PendingDrop>) {
    for (hook, payload) in drops.drain(..) {
        // SAFETY: dead storage is quarantined until finish(), and each hook is
        // queued once before its object leaves the live allocation metadata.
        unsafe { run_drop_hook(hook, payload) };
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
        run_drops(&mut drops);
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
        run_drops(&mut drops);
        if concurrent {
            between_regions(plan.old + index);
        }
    }
    finish(&mut runtime().heap.lock().unwrap(), &dead_chunks);
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

fn sweep_region(
    state: &mut GcState,
    index: usize,
    stop_work: &mut crate::gc_telemetry::stops::StopWorkV2,
    drops: &mut Vec<PendingDrop>,
    marking: Option<&ConcurrentCycle>,
) -> usize {
    let mut freed_bytes = 0;
    {
        let GcState {
            old_regions,
            remembered_set,
            young_allocated_bytes,
            allocated_bytes,
            total_frees,
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
                        drops.push((drop_fn, object.payload().as_ptr()));
                    }
                    remembered_set.remove(&(object.payload().as_ptr() as usize));
                    if object.generation() == GC_GENERATION_YOUNG {
                        *young_allocated_bytes = young_allocated_bytes.saturating_sub(size);
                    }
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
                drops.push((drop_fn, object.payload().as_ptr()));
            }
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
    } else {
        RegionKind::Nursery
    };
    (freed_bytes, !has_live_objects && owner_state.is_none())
}

fn finish(state: &mut GcState, dead_chunks: &[bool]) {
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
    if std::env::var("WILLOW_GC_VERIFY_REGIONS").is_ok()
        && let Err(message) = verify_old_region_metadata(state)
    {
        panic!("willow gc: region verification failed after major collection: {message}");
    }
}
