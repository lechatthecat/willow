use super::*;

pub(super) fn verify_remembered_set(
    state: &GcState,
    trace_registry: &HashMap<u32, TraceFn, impl std::hash::BuildHasher>,
) -> Result<(), String> {
    let retired_objects = state.tlab_chunks.iter().flat_map(|chunk| {
        let mut offset = 0usize;
        std::iter::from_fn(move || {
            if offset >= chunk.used {
                return None;
            }
            // SAFETY: barrier verification runs after every TLAB is retired.
            let object = HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast())
                .expect("TLAB header address is non-null");
            offset += object.size();
            Some(object)
        })
    });
    let (young_start, young_end) = state
        .tlab_chunks
        .iter()
        .filter(|chunk| chunk.kind != RegionKind::Pinned)
        .fold((usize::MAX, 0usize), |(low, high), chunk| {
            (
                low.min(chunk.base as usize),
                high.max((chunk.base as usize).saturating_add(chunk_used_bytes(state, chunk))),
            )
        });
    type Scratch = (
        Vec<*mut *mut u8>,
        Option<(usize, raw_heap::TraceMetadata, Option<TraceFn>, u64)>,
        usize,
    );
    let mut scratch: Scratch = (Vec::new(), None, 0);
    #[inline(always)]
    fn verify_object(
        state: &GcState,
        trace_registry: &HashMap<u32, TraceFn, impl std::hash::BuildHasher>,
        (young_start, young_end): (usize, usize),
        scratch: &mut Scratch,
        object: HeapObject,
    ) -> Result<(), String> {
        let (slots, last_layout, flagged) = scratch;
        if !object.allocated() || object.generation() != GC_GENERATION_OLD {
            return Ok(());
        }
        let owner = object.payload().as_ptr() as usize;
        // Card-remembered buffers are members with a clear header flag.
        if object.remembered()
            && (!state.remembered_set.contains(&owner)
                || state.remembered_set.owner_cards(owner).is_some())
        {
            return Err(format!(
                "old object 0x{owner:x} remembered flag disagrees with the remembered set"
            ));
        }
        // Descriptors and the registry are immutable for this stopped scan.
        // Key by descriptor identity, not type: one type may have many layouts.
        *flagged += usize::from(object.remembered());
        let identity = object.descriptor_address();
        let (metadata, trace, inline_mask) = match *last_layout {
            Some((last, metadata, trace, mask)) if identity == last => (metadata, trace, mask),
            _ => {
                let metadata = object.trace_metadata();
                let trace = trace_registry.get(&metadata.type_id).copied();
                let words = (metadata.payload_size / GC_STORAGE_WORD_BYTES).min(64);
                let mask = metadata.inline_ref_mask()
                    & u64::MAX.checked_shr((64 - words) as u32).unwrap_or(0);
                *last_layout = Some((identity, metadata, trace, mask));
                (metadata, trace, mask)
            }
        };
        slots.clear();
        if let Some(trace) = trace {
            // SAFETY: verification runs stopped over allocated objects.
            unsafe { trace(object.payload().as_ptr(), slots) };
        }
        #[inline(always)]
        fn verify_slot(
            state: &GcState,
            owner: usize,
            slot: *mut *mut u8,
            young_start: usize,
            young_end: usize,
        ) -> Result<(), String> {
            if slot.is_null() {
                return Ok(());
            }
            // SAFETY: trace/layout slots are readable under stop-the-world.
            let child = unsafe { *slot };
            // Only TLAB storage can contain young objects; avoid searching
            // old-region allocation trees for every verified old-to-old edge.
            if (child as usize) < young_start
                || (child as usize) > young_end
                || tlab_payload_generation(state, child as usize) != Some(GC_GENERATION_YOUNG)
            {
                return Ok(());
            }
            if let Some(cards) = state.remembered_set.owner_cards(owner) {
                let card = super::remembered::array_slot_card(owner, slot as usize);
                if !cards.contains(&card) {
                    return Err(format!(
                        "old buffer 0x{owner:x} contains young reference 0x{:x} in unrecorded card {card}",
                        child as usize
                    ));
                }
                return Ok(());
            }
            if !state.remembered_set.contains(&owner) {
                return Err(format!(
                    "old object 0x{owner:x} contains young reference 0x{:x} without a remembered-set entry",
                    child as usize
                ));
            }
            Ok(())
        }
        if trace.is_none()
            && metadata.type_id != willow_abi::GC_BITMAP_TYPE_ID
            && inline_mask.is_power_of_two()
        {
            return verify_slot(
                state,
                owner,
                object.payload_slot(inline_mask.trailing_zeros() as usize),
                young_start,
                young_end,
            );
        }
        // Keep each streaming source separate: chaining the three stateful
        // iterators adds state dispatch to every ordinary inline reference.
        let mut mask = inline_mask;
        while mask != 0 {
            let slot = object.payload_slot(mask.trailing_zeros() as usize);
            mask &= mask - 1;
            verify_slot(state, owner, slot, young_start, young_end)?;
        }
        for slot in bitmap_reference_slots(object, metadata) {
            verify_slot(state, owner, slot, young_start, young_end)?;
        }
        for &slot in slots.iter() {
            verify_slot(state, owner, slot, young_start, young_end)?;
        }
        Ok(())
    }
    for region in &state.old_regions {
        // Dense regions use the authoritative object-start bitmap. Fall back
        // to allocation entries for sparse regions so unused bitmap capacity
        // cannot make verifier work proportional to reserved heap size.
        if region.mark_bitmap.bits.word_count() <= region.allocations.len() {
            region.mark_bitmap.bits.try_for_each_set(|index| {
                // SAFETY: stopped, retired region start bits identify live
                // allocated headers; sweep holds the same heap lock.
                let raw = unsafe { region.base.add(index * GC_REGION_MARK_GRANULE) };
                verify_object(
                    state,
                    trace_registry,
                    (young_start, young_end),
                    &mut scratch,
                    HeapObject::from_raw(raw.cast()).unwrap(),
                )
            })?;
        } else {
            for &offset in region.allocations.keys() {
                // SAFETY: the allocation map owns this stopped header.
                let raw = unsafe { region.base.add(offset) };
                verify_object(
                    state,
                    trace_registry,
                    (young_start, young_end),
                    &mut scratch,
                    HeapObject::from_raw(raw.cast()).unwrap(),
                )?;
            }
        }
    }
    for object in retired_objects {
        verify_object(
            state,
            trace_registry,
            (young_start, young_end),
            &mut scratch,
            object,
        )?;
    }
    // Every flagged object was checked for unique whole-owner membership.
    // Equality proves the reverse implication without scanning hash capacity
    // or looking up each remembered address in the heap indexes.
    if scratch.2 != state.remembered_set.whole_owner_count() {
        return Err("remembered flags disagree with whole-owner membership".into());
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
        .filter(|(_, region)| region.kind == RegionKind::Old && !region.sweep_quarantined)
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
