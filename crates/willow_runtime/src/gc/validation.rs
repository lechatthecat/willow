use super::*;

pub(super) fn verify_remembered_set(
    state: &GcState,
    trace_registry: &HashMap<u32, TraceFn>,
) -> Result<(), String> {
    let mut old_objects = Vec::new();
    for object in old_region_objects(state) {
        if object.allocated() && object.generation() == GC_GENERATION_OLD {
            old_objects.push(object);
        }
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

pub(super) fn verify_old_region_metadata(state: &GcState) -> Result<(), String> {
    if !state
        .old_addresses
        .matches(state.old_regions.len(), |index| {
            state.old_regions.get(index).map(OldRegion::start)
        })
    {
        return Err("old-region address index mismatch".into());
    }
    if !state
        .tlab_addresses
        .matches(state.tlab_chunks.len(), |index| {
            state
                .tlab_chunks
                .get(index)
                .map(|chunk| chunk.base as usize)
        })
    {
        return Err("TLAB address index mismatch".into());
    }
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

    for chunk in &state.tlab_chunks {
        if chunk.used > chunk.capacity {
            return Err(format!(
                "{:?} region 0x{:x} used {} bytes beyond capacity {}",
                chunk.kind, chunk.base as usize, chunk.used, chunk.capacity
            ));
        }
        if matches!(chunk.kind, RegionKind::Pinned | RegionKind::Survivor) {
            if chunk.owner_state.is_some() {
                return Err("collector/pinned chunk has a mutator owner".into());
            }
            let expected_generation = if chunk.kind == RegionKind::Survivor {
                GC_GENERATION_YOUNG
            } else {
                GC_GENERATION_OLD
            };
            let mut offset = 0usize;
            let mut live = 0usize;
            while offset < chunk.used {
                // SAFETY: pinned regions retain the sequential TLAB layout.
                let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast())
                    .expect("pinned-region object address is non-null");
                if object.allocated() {
                    if object.generation() != expected_generation
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
    for region in &state.old_regions {
        if !region.free_spans.is_consistent() {
            return Err("old-region free span index mismatch".into());
        }
        if region.largest_free_span
            != region
                .free_spans
                .iter()
                .map(|span| span.size)
                .max()
                .unwrap_or(0)
        {
            return Err("old-region largest free span mismatch".into());
        }
    }
    let expected: HashSet<_> = state
        .old_regions
        .iter()
        .enumerate()
        .filter(|(_, region)| region.kind == RegionKind::Old)
        .map(|(index, region)| (region.available_span(), index))
        .filter(|(available, _)| *available >= GC_HEADER_SIZE)
        .collect();
    if expected.len() != state.old_region_candidates.len()
        || expected != state.old_region_candidates.iter().copied().collect()
    {
        return Err("old-region allocation candidate index mismatch".into());
    }
    if state
        .old_regions
        .iter()
        .map(|region| region.capacity)
        .sum::<usize>()
        != state.old_reserved_bytes
    {
        return Err("old-region reservation accounting mismatch".into());
    }
    Ok(())
}
