use super::*;

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

pub(crate) fn survivor_snapshot() -> crate::gc_telemetry::GcSurvivorStats {
    runtime().heap.lock().unwrap().survivor_stats
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_survivor_copies() -> i64 {
    runtime()
        .heap
        .lock()
        .unwrap()
        .survivor_stats
        .survivor_copies as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_survivor_bytes() -> i64 {
    runtime().heap.lock().unwrap().survivor_stats.survivor_bytes as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_tenured_objects() -> i64 {
    runtime()
        .heap
        .lock()
        .unwrap()
        .survivor_stats
        .tenured_objects as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_tenured_bytes() -> i64 {
    runtime().heap.lock().unwrap().survivor_stats.tenured_bytes as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_pinned_promotions() -> i64 {
    runtime()
        .heap
        .lock()
        .unwrap()
        .survivor_stats
        .pinned_promotions as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_survivor_space_reserved() -> i64 {
    survivor_snapshot().survivor_space_reserved as i64
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_survivor_space_live() -> i64 {
    survivor_snapshot().survivor_space_live as i64
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
