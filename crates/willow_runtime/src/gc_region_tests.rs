use super::*;

fn regular_region(capacity: usize) -> OldRegion {
    OldRegion::new(RegionKind::Old, capacity).expect("test region allocation must succeed")
}

fn region_alloc(region: &mut OldRegion, payload_size: usize) -> HeapObject {
    region
        .allocate_object(7, 11, 0, payload_size)
        .expect("test object allocation must succeed")
        .0
}

fn span_size(payload_size: usize) -> usize {
    (GC_HEADER_SIZE + payload_size)
        .checked_next_multiple_of(GC_REGION_MARK_GRANULE)
        .unwrap()
}

fn reset_gc() {
    reset_internal();
}

fn global_gc_guard() -> std::sync::MutexGuard<'static, ()> {
    runtime_test_guard()
}

fn state_with_regular_object(payload_size: usize) -> (GcState, HeapObject) {
    let mut state = GcState::default();
    let object =
        allocate_old_region_object_locked(&mut state, 11, 7, payload_size, 0, true).unwrap();
    (state, object)
}

fn tlab_fast_alloc_for_test(
    tls: &GcTlabState,
    layout_id: u64,
    type_id: u32,
    payload_size: usize,
    gc_ref_mask: u64,
) -> *mut u8 {
    let total_size = (GC_HEADER_SIZE + payload_size)
        .checked_next_multiple_of(std::mem::align_of::<GcHeader>())
        .unwrap();
    let cursor = tls.cursor.load(Ordering::Acquire);
    let limit = tls.limit.load(Ordering::Acquire);
    assert!(cursor + total_size <= limit);
    let object = initialize_object_at(
        cursor as *mut u8,
        total_size,
        type_id,
        layout_id,
        gc_ref_mask,
    )
    .unwrap();
    publish_tlab_start_for_test(tls, cursor as *mut u8);
    tls.cursor.store(cursor + total_size, Ordering::Release);
    tls.fast_allocations.fetch_add(1, Ordering::Relaxed);
    tls.fast_allocated_bytes
        .fetch_add(total_size as u64, Ordering::Relaxed);
    object.payload().as_ptr()
}

#[test]
fn region_bitmap_01_new_bitmap_starts_clear() {
    let bitmap = RegionMarkBitmap::new(GC_REGION_MARK_GRANULE * 130);
    assert!(!bitmap.is_marked(0));
    assert!(!bitmap.is_marked(GC_REGION_MARK_GRANULE * 64));
    assert!(!bitmap.is_marked(GC_REGION_MARK_GRANULE * 129));
}

#[test]
fn region_bitmap_02_marks_across_machine_words() {
    let mut bitmap = RegionMarkBitmap::new(GC_REGION_MARK_GRANULE * 130);
    for granule in [0, 63, 64, 65, 129] {
        bitmap.mark(GC_REGION_MARK_GRANULE * granule);
    }
    for granule in [0, 63, 64, 65, 129] {
        assert!(bitmap.is_marked(GC_REGION_MARK_GRANULE * granule));
    }
}

#[test]
fn region_bitmap_03_clear_removes_every_mark() {
    let mut bitmap = RegionMarkBitmap::new(GC_REGION_MARK_GRANULE * 130);
    for granule in [0, 63, 64, 129] {
        bitmap.mark(GC_REGION_MARK_GRANULE * granule);
    }
    bitmap.clear();
    for granule in [0, 63, 64, 129] {
        assert!(!bitmap.is_marked(GC_REGION_MARK_GRANULE * granule));
    }
}

#[test]
fn region_bitmap_04_unmark_preserves_neighboring_marks() {
    let mut bitmap = RegionMarkBitmap::new(GC_REGION_MARK_GRANULE * 3);
    bitmap.mark(0);
    bitmap.mark(GC_REGION_MARK_GRANULE);
    bitmap.unmark(0);
    assert!(!bitmap.is_marked(0));
    assert!(bitmap.is_marked(GC_REGION_MARK_GRANULE));
}

#[test]
fn region_bitmap_05_out_of_range_operations_are_safe() {
    let mut bitmap = RegionMarkBitmap::new(GC_REGION_MARK_GRANULE);
    let outside = GC_REGION_MARK_GRANULE * 100;
    bitmap.mark(outside);
    bitmap.unmark(outside);
    assert!(!bitmap.is_marked(outside));
}

#[test]
fn old_region_01_regular_region_starts_empty() {
    let region = regular_region(256);
    assert_eq!(region.kind, RegionKind::Old);
    assert_eq!(region.capacity, 256);
    assert_eq!(region.used, 0);
    assert_eq!(region.live_bytes, 0);
    assert!(region.allocations.is_empty());
    assert!(region.free_spans.is_empty());
}

#[test]
fn old_region_02_large_region_starts_empty() {
    let region = OldRegion::new(RegionKind::LargeObject, 256).unwrap();
    assert_eq!(region.kind, RegionKind::LargeObject);
    assert_eq!(region.capacity, 256);
    assert_eq!(region.used, 0);
    assert_eq!(region.live_bytes, 0);
}

#[test]
fn old_region_03_base_is_header_aligned() {
    let region = regular_region(256);
    assert!(
        region
            .start()
            .is_multiple_of(std::mem::align_of::<GcHeader>())
    );
}

#[test]
fn old_region_04_bounds_are_half_open() {
    let region = regular_region(256);
    assert!(region.contains(region.start()));
    assert!(region.contains(region.end() - 1));
    assert!(!region.contains(region.end()));
    assert!(!region.contains(region.start() - 1));
}

#[test]
fn old_region_alloc_01_zero_payload_uses_one_aligned_span() {
    let mut region = regular_region(256);
    let object = region_alloc(&mut region, 0);
    assert_eq!(object.size(), GC_HEADER_SIZE);
    assert_eq!(region.used, span_size(0));
    assert_eq!(region.live_bytes, GC_HEADER_SIZE);
}

#[test]
fn old_region_alloc_02_odd_payload_keeps_logical_size() {
    let mut region = regular_region(256);
    let object = region_alloc(&mut region, 3);
    assert_eq!(object.size(), GC_HEADER_SIZE + 3);
    assert_eq!(region.used, span_size(3));
    assert_eq!(region.live_bytes, GC_HEADER_SIZE + 3);
}

#[test]
fn old_region_alloc_03_consecutive_spans_do_not_overlap() {
    let mut region = regular_region(512);
    let first = region_alloc(&mut region, 1);
    let second = region_alloc(&mut region, 17);
    let first_offset = first.as_ptr() as usize - region.start();
    let second_offset = second.as_ptr() as usize - region.start();
    assert_eq!(first_offset, 0);
    assert_eq!(second_offset, span_size(1));
    assert!(first_offset + span_size(1) <= second_offset);
}

#[test]
fn old_region_alloc_04_exact_capacity_can_be_filled() {
    let span = span_size(8);
    let mut region = regular_region(span * 2);
    assert!(region.allocate_object(1, 1, 0, 8).is_some());
    assert!(region.allocate_object(2, 2, 0, 8).is_some());
    assert_eq!(region.used, region.capacity);
}

#[test]
fn old_region_alloc_05_capacity_failure_preserves_metadata() {
    let span = span_size(8);
    let mut region = regular_region(span);
    region_alloc(&mut region, 8);
    let used = region.used;
    let live = region.live_bytes;
    let allocations = region.allocations.clone();
    assert!(region.allocate_object(2, 2, 0, 8).is_none());
    assert_eq!(region.used, used);
    assert_eq!(region.live_bytes, live);
    assert_eq!(region.allocations, allocations);
}

#[test]
fn old_region_alloc_06_payload_is_zero_initialized() {
    let mut region = regular_region(256);
    let object = region_alloc(&mut region, 31);
    let payload = object.payload().as_ptr();
    for index in 0..31 {
        assert_eq!(unsafe { *payload.add(index) }, 0);
    }
}

#[test]
fn old_region_alloc_07_header_metadata_is_recorded() {
    let mut region = regular_region(256);
    let (object, reused) = region.allocate_object(91, 1234, 0b101, 24).unwrap();
    let metadata = object.trace_metadata();
    assert!(!reused);
    assert_eq!(object.type_id(), 91);
    assert_eq!(object.generation(), GC_GENERATION_OLD);
    assert_eq!(metadata.layout_id, 1234);
    assert_eq!(metadata.gc_ref_mask, 0b101);
    assert_eq!(metadata.payload_size, 24);
}

#[test]
fn old_region_alloc_08_exact_payload_lookup_finds_object() {
    let mut region = regular_region(256);
    let object = region_alloc(&mut region, 16);
    assert_eq!(
        region
            .object_for_address(object.payload().as_ptr() as usize, false)
            .map(HeapObject::as_ptr),
        Some(object.as_ptr())
    );
}

#[test]
fn old_region_alloc_09_exact_lookup_rejects_interior_address() {
    let mut region = regular_region(256);
    let object = region_alloc(&mut region, 16);
    let interior = object.payload().as_ptr() as usize + 1;
    assert!(region.object_for_address(interior, false).is_none());
}

#[test]
fn old_region_alloc_10_interior_lookup_accepts_payload_range() {
    let mut region = regular_region(256);
    let object = region_alloc(&mut region, 16);
    let start = object.payload().as_ptr() as usize;
    for address in [start, start + 1, start + 15] {
        assert_eq!(
            region
                .object_for_address(address, true)
                .map(HeapObject::as_ptr),
            Some(object.as_ptr())
        );
    }
}

#[test]
fn old_region_alloc_11_interior_lookup_rejects_header_and_payload_end() {
    let mut region = regular_region(256);
    let object = region_alloc(&mut region, 16);
    assert!(
        region
            .object_for_address(object.as_ptr() as usize, true)
            .is_none()
    );
    let payload_end = object.payload().as_ptr() as usize + 16;
    assert!(region.object_for_address(payload_end, true).is_none());
}

#[test]
fn old_region_alloc_12_lookup_rejects_alignment_padding() {
    let mut region = regular_region(256);
    let object = region_alloc(&mut region, 1);
    let padding = object.as_ptr() as usize + object.size();
    assert!(padding < region.start() + region.used);
    assert!(region.object_for_address(padding, true).is_none());
}

#[test]
fn old_region_alloc_13_allocation_marks_object_start() {
    let mut region = regular_region(256);
    let object = region_alloc(&mut region, 8);
    let offset = object.as_ptr() as usize - region.start();
    assert!(region.mark_bitmap.is_marked(offset));
}

#[test]
fn old_region_reuse_01_middle_release_creates_free_span() {
    let mut region = regular_region(512);
    let _first = region_alloc(&mut region, 8);
    let middle = region_alloc(&mut region, 8);
    let _last = region_alloc(&mut region, 8);
    let offset = middle.as_ptr() as usize - region.start();
    region.release_object(middle);
    assert_eq!(
        region.free_spans,
        vec![RegionFreeSpan {
            offset,
            size: span_size(8)
        }]
    );
}

#[test]
fn old_region_reuse_02_allocator_uses_first_suitable_hole() {
    let mut region = regular_region(1024);
    let _a = region_alloc(&mut region, 8);
    let first_hole = region_alloc(&mut region, 24);
    let _c = region_alloc(&mut region, 8);
    let second_hole = region_alloc(&mut region, 40);
    let _e = region_alloc(&mut region, 8);
    region.release_object(first_hole);
    region.release_object(second_hole);
    let (replacement, reused) = region.allocate_object(8, 12, 0, 16).unwrap();
    assert!(reused);
    assert_eq!(replacement.as_ptr(), first_hole.as_ptr());
}

#[test]
fn old_region_reuse_03_smaller_replacement_splits_hole() {
    let mut region = regular_region(512);
    let _a = region_alloc(&mut region, 8);
    let hole = region_alloc(&mut region, 40);
    let _c = region_alloc(&mut region, 8);
    let hole_offset = hole.as_ptr() as usize - region.start();
    let original_span = span_size(40);
    region.release_object(hole);
    let (replacement, reused) = region.allocate_object(9, 13, 0, 8).unwrap();
    assert!(reused);
    assert_eq!(replacement.as_ptr(), hole.as_ptr());
    assert_eq!(
        region.free_spans,
        vec![RegionFreeSpan {
            offset: hole_offset + span_size(8),
            size: original_span - span_size(8),
        }]
    );
}

#[test]
fn old_region_reuse_04_two_adjacent_holes_coalesce() {
    let mut region = regular_region(512);
    let _a = region_alloc(&mut region, 8);
    let b = region_alloc(&mut region, 8);
    let c = region_alloc(&mut region, 8);
    let _d = region_alloc(&mut region, 8);
    let offset = b.as_ptr() as usize - region.start();
    region.release_object(b);
    region.release_object(c);
    assert_eq!(
        region.free_spans,
        vec![RegionFreeSpan {
            offset,
            size: span_size(8) * 2
        }]
    );
}

#[test]
fn old_region_reuse_05_three_adjacent_holes_coalesce() {
    let mut region = regular_region(512);
    let _a = region_alloc(&mut region, 8);
    let b = region_alloc(&mut region, 8);
    let c = region_alloc(&mut region, 8);
    let d = region_alloc(&mut region, 8);
    let _e = region_alloc(&mut region, 8);
    let offset = b.as_ptr() as usize - region.start();
    region.release_object(c);
    region.release_object(b);
    region.release_object(d);
    assert_eq!(
        region.free_spans,
        vec![RegionFreeSpan {
            offset,
            size: span_size(8) * 3
        }]
    );
}

#[test]
fn old_region_reuse_06_nonadjacent_holes_stay_separate() {
    let mut region = regular_region(512);
    let _a = region_alloc(&mut region, 8);
    let b = region_alloc(&mut region, 8);
    let _c = region_alloc(&mut region, 8);
    let d = region_alloc(&mut region, 8);
    let _e = region_alloc(&mut region, 8);
    region.release_object(b);
    region.release_object(d);
    assert_eq!(region.free_spans.len(), 2);
}

#[test]
fn old_region_reuse_07_releasing_tail_shrinks_used() {
    let mut region = regular_region(256);
    let _first = region_alloc(&mut region, 8);
    let tail = region_alloc(&mut region, 8);
    region.release_object(tail);
    assert_eq!(region.used, span_size(8));
    assert!(region.free_spans.is_empty());
}

#[test]
fn old_region_reuse_08_releasing_everything_resets_used() {
    let mut region = regular_region(256);
    let first = region_alloc(&mut region, 8);
    let second = region_alloc(&mut region, 8);
    region.release_object(second);
    region.release_object(first);
    assert_eq!(region.used, 0);
    assert_eq!(region.live_bytes, 0);
    assert!(region.allocations.is_empty());
    assert!(region.free_spans.is_empty());
}

#[test]
fn old_region_reuse_09_fragmentation_includes_alignment_padding() {
    let mut region = regular_region(256);
    region_alloc(&mut region, 1);
    assert_eq!(
        region.fragmentation_bytes(),
        span_size(1) - (GC_HEADER_SIZE + 1)
    );
}

#[test]
fn old_region_reuse_10_reused_payload_is_zeroed() {
    let mut region = regular_region(512);
    let _prefix = region_alloc(&mut region, 8);
    let object = region_alloc(&mut region, 8);
    let _suffix = region_alloc(&mut region, 8);
    unsafe { *(object.payload().as_ptr() as *mut u64) = u64::MAX };
    let address = object.payload().as_ptr();
    region.release_object(object);
    let (replacement, reused) = region.allocate_object(8, 12, 0, 8).unwrap();
    assert!(reused);
    assert_eq!(replacement.payload().as_ptr(), address);
    assert_eq!(
        unsafe { *(replacement.payload().as_ptr() as *const u64) },
        0
    );
}

#[test]
fn old_region_reuse_11_reused_header_is_reinitialized() {
    let mut region = regular_region(512);
    let _prefix = region_alloc(&mut region, 8);
    let (object, _) = region.allocate_object(1, 2, 0b1, 8).unwrap();
    let _suffix = region_alloc(&mut region, 8);
    object.set_generation(GC_GENERATION_YOUNG);
    let address = object.as_ptr();
    region.release_object(object);
    let (replacement, reused) = region.allocate_object(9, 10, 0, 8).unwrap();
    assert!(reused);
    assert_eq!(replacement.as_ptr(), address);
    assert_eq!(replacement.type_id(), 9);
    assert_eq!(replacement.trace_metadata().layout_id, 10);
    assert_eq!(replacement.trace_metadata().gc_ref_mask, 0);
    assert_eq!(replacement.generation(), GC_GENERATION_OLD);
    assert!(!replacement.marked());
    assert!(replacement.allocated());
}

#[test]
fn old_region_reuse_12_release_removes_mark_bit() {
    let mut region = regular_region(256);
    let object = region_alloc(&mut region, 8);
    let offset = object.as_ptr() as usize - region.start();
    region.release_object(object);
    assert!(!region.mark_bitmap.is_marked(offset));
}

#[test]
#[should_panic(expected = "missing region allocation metadata")]
fn old_region_reuse_13_double_release_is_rejected() {
    let mut region = regular_region(256);
    let object = region_alloc(&mut region, 8);
    region.release_object(object);
    region.release_object(object);
}

#[test]
fn region_verify_01_accepts_valid_state() {
    let (state, _) = state_with_regular_object(8);
    assert_eq!(verify_old_region_metadata(&state), Ok(()));
}

#[test]
fn region_verify_02_rejects_used_beyond_capacity() {
    let (mut state, _) = state_with_regular_object(8);
    state.old_regions[0].used = state.old_regions[0].capacity + 1;
    let error = verify_old_region_metadata(&state).unwrap_err();
    assert!(error.contains("beyond capacity"));
}

#[test]
fn region_verify_03_rejects_missing_mark_bit() {
    let (mut state, object) = state_with_regular_object(8);
    let offset = object.as_ptr() as usize - state.old_regions[0].start();
    state.old_regions[0].mark_bitmap.unmark(offset);
    let error = verify_old_region_metadata(&state).unwrap_err();
    assert!(error.contains("absent from its mark bitmap"));
}

#[test]
fn region_verify_04_rejects_live_byte_mismatch() {
    let (mut state, _) = state_with_regular_object(8);
    state.old_regions[0].live_bytes += 1;
    let error = verify_old_region_metadata(&state).unwrap_err();
    assert!(error.contains("live-byte mismatch"));
}

#[test]
fn region_verify_05_rejects_misaligned_allocation_offset() {
    let (mut state, _) = state_with_regular_object(8);
    let span = state.old_regions[0].allocations.remove(&0).unwrap();
    state.old_regions[0].allocations.insert(1, span);
    let error = verify_old_region_metadata(&state).unwrap_err();
    assert!(error.contains("invalid allocation span"));
}

#[test]
fn region_verify_06_rejects_allocation_past_used_prefix() {
    let (mut state, _) = state_with_regular_object(8);
    *state.old_regions[0].allocations.get_mut(&0).unwrap() += GC_REGION_MARK_GRANULE;
    let error = verify_old_region_metadata(&state).unwrap_err();
    assert!(error.contains("invalid allocation span"));
}

#[test]
fn region_verify_07_rejects_zero_size_free_span() {
    let (mut state, _) = state_with_regular_object(8);
    state.old_regions[0]
        .free_spans
        .push(RegionFreeSpan { offset: 0, size: 0 });
    let error = verify_old_region_metadata(&state).unwrap_err();
    assert!(error.contains("invalid free span"));
}

#[test]
fn region_verify_08_rejects_overlapping_free_and_allocated_spans() {
    let (mut state, _) = state_with_regular_object(8);
    state.old_regions[0].free_spans.push(RegionFreeSpan {
        offset: 0,
        size: GC_REGION_MARK_GRANULE,
    });
    let error = verify_old_region_metadata(&state).unwrap_err();
    assert!(error.contains("overlapping"));
}

#[test]
fn region_verify_09_rejects_missing_address_index() {
    let (mut state, _) = state_with_regular_object(8);
    state.old_addresses.clear();
    let error = verify_old_region_metadata(&state).unwrap_err();
    assert!(error.contains("address index mismatch"));
}

#[test]
fn region_verify_10_rejects_invalid_address_index_position() {
    let (mut state, _) = state_with_regular_object(8);
    state.old_addresses.clear();
    state
        .old_addresses
        .insert(state.old_regions[0].start(), usize::MAX);
    let error = verify_old_region_metadata(&state).unwrap_err();
    assert!(error.contains("address index mismatch"));
}

#[test]
fn region_verify_11_rejects_non_old_region_object() {
    let (state, object) = state_with_regular_object(8);
    object.set_generation(GC_GENERATION_YOUNG);
    let error = verify_old_region_metadata(&state).unwrap_err();
    assert!(error.contains("inconsistent header metadata"));
}

#[test]
fn region_verify_12_requires_one_allocation_in_large_region() {
    let mut state = GcState::default();
    let _object =
        allocate_old_region_object_locked(&mut state, 11, 7, GC_LARGE_OBJECT_THRESHOLD, 0, true)
            .unwrap();
    state.old_regions[0].allocations.clear();
    let error = verify_old_region_metadata(&state).unwrap_err();
    assert!(error.contains("owns 0 allocations instead of one"));
}

#[test]
fn region_metrics_01_empty_heap_reports_zeroes() {
    let _guard = global_gc_guard();
    reset_gc();
    assert_eq!(willow_gc_old_region_count(), 0);
    assert_eq!(willow_gc_old_region_reserved_bytes(), 0);
    assert_eq!(willow_gc_old_region_live_bytes(), 0);
    assert_eq!(willow_gc_old_region_fragmentation_bytes(), 0);
    assert_eq!(willow_gc_large_object_region_count(), 0);
    assert_eq!(willow_gc_pinned_region_count(), 0);
    assert_eq!(willow_gc_old_region_allocations(), 0);
    assert_eq!(willow_gc_old_region_reuses(), 0);
    assert_eq!(willow_gc_old_regions_released(), 0);
    assert_eq!(willow_gc_major_collections(), 0);
}

#[test]
fn region_metrics_02_regular_reserved_and_live_bytes_are_distinct() {
    let _guard = global_gc_guard();
    reset_gc();
    let _object = willow_alloc_object(1, 8);
    assert_eq!(
        willow_gc_old_region_reserved_bytes(),
        GC_OLD_REGION_SIZE as i64
    );
    assert_eq!(
        willow_gc_old_region_live_bytes(),
        (GC_HEADER_SIZE + 8) as i64
    );
    reset_gc();
}

#[test]
fn region_metrics_03_odd_payload_reports_alignment_fragmentation() {
    let _guard = global_gc_guard();
    reset_gc();
    let _object = willow_alloc_object(1, 1);
    assert_eq!(
        willow_gc_old_region_fragmentation_bytes(),
        (span_size(1) - (GC_HEADER_SIZE + 1)) as i64
    );
    reset_gc();
}

#[test]
fn region_metrics_04_empty_major_collection_is_counted() {
    let _guard = global_gc_guard();
    reset_gc();
    willow_gc_collect();
    willow_gc_collect();
    assert_eq!(willow_gc_major_collections(), 2);
    reset_gc();
}

#[test]
fn large_region_01_threshold_is_based_on_total_object_size() {
    let _guard = global_gc_guard();
    reset_gc();
    let at_threshold = GC_LARGE_OBJECT_THRESHOLD - GC_HEADER_SIZE;
    let _regular = willow_alloc_object(1, at_threshold as i64);
    assert_eq!(willow_gc_large_object_region_count(), 0);
    let _large = willow_alloc_object(2, (at_threshold + 1) as i64);
    assert_eq!(willow_gc_large_object_region_count(), 1);
    reset_gc();
}

#[test]
fn large_region_02_multiple_large_objects_get_dedicated_regions() {
    let _guard = global_gc_guard();
    reset_gc();
    for type_id in 1..=3 {
        let object = willow_alloc_object(type_id, GC_LARGE_OBJECT_THRESHOLD as i64);
        assert!(!object.is_null());
    }
    assert_eq!(willow_gc_large_object_region_count(), 3);
    assert_eq!(willow_gc_old_region_count(), 3);
    reset_gc();
}

#[test]
fn large_region_03_reserved_bytes_include_regular_and_large_regions() {
    let _guard = global_gc_guard();
    reset_gc();
    let _regular = willow_alloc_object(1, 8);
    let _large = willow_alloc_object(2, GC_LARGE_OBJECT_THRESHOLD as i64);
    let state = runtime().heap.lock().unwrap();
    let expected: usize = state.old_regions.iter().map(|region| region.capacity).sum();
    drop(state);
    assert_eq!(willow_gc_old_region_reserved_bytes(), expected as i64);
    reset_gc();
}

#[test]
fn major_region_01_releases_only_completely_dead_regions() {
    let _guard = global_gc_guard();
    reset_gc();
    let mut objects = Vec::with_capacity(6000);
    for value in 0..6000i64 {
        let object = willow_alloc_object(1, 8);
        unsafe { *(object as *mut i64) = value };
        objects.push(object);
    }
    assert!(willow_gc_old_region_count() >= 2);
    let mut survivor = *objects.last().unwrap();
    let survivor_address = survivor;
    willow_push_root(&mut survivor);
    let before = willow_gc_old_region_count();
    willow_gc_collect();
    assert_eq!(survivor, survivor_address);
    assert_eq!(unsafe { *(survivor as *mut i64) }, 5999);
    assert!(willow_gc_old_region_count() < before);
    assert_eq!(willow_gc_old_region_count(), 1);
    willow_pop_root();
    willow_gc_collect();
    reset_gc();
}

#[test]
fn major_region_02_cross_region_cycle_survives_from_one_root() {
    let _guard = global_gc_guard();
    reset_gc();
    let mut first = willow_alloc_typed(8, 0b1);
    let mut second = first;
    while willow_gc_old_region_count() == 1 {
        second = willow_alloc_typed(8, 0b1);
    }
    assert_ne!(first, second);
    unsafe {
        *(first as *mut *mut u8) = second;
        *(second as *mut *mut u8) = first;
    }
    willow_push_root(&mut first);
    willow_gc_collect();
    assert_eq!(willow_gc_old_region_count(), 2);
    assert_eq!(unsafe { *(first as *mut *mut u8) }, second);
    assert_eq!(unsafe { *(second as *mut *mut u8) }, first);
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_old_region_count(), 0);
    reset_gc();
}

#[test]
fn major_region_03_rootless_cross_region_cycle_is_reclaimed() {
    let _guard = global_gc_guard();
    reset_gc();
    let first = willow_alloc_typed(8, 0b1);
    let mut second = first;
    while willow_gc_old_region_count() == 1 {
        second = willow_alloc_typed(8, 0b1);
    }
    unsafe {
        *(first as *mut *mut u8) = second;
        *(second as *mut *mut u8) = first;
    }
    willow_gc_collect();
    assert_eq!(willow_gc_old_region_count(), 0);
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

#[test]
fn major_region_04_regular_object_keeps_large_child_alive() {
    let _guard = global_gc_guard();
    reset_gc();
    let large = willow_alloc_object(2, GC_LARGE_OBJECT_THRESHOLD as i64);
    let mut parent = willow_alloc_typed(8, 0b1);
    unsafe { *(parent as *mut *mut u8) = large };
    willow_push_root(&mut parent);
    willow_gc_collect();
    assert_eq!(willow_gc_large_object_region_count(), 1);
    assert_eq!(unsafe { *(parent as *mut *mut u8) }, large);
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_old_region_count(), 0);
    reset_gc();
}

#[test]
fn major_region_05_large_object_keeps_regular_child_alive() {
    let _guard = global_gc_guard();
    reset_gc();
    let child = willow_alloc_object(1, 8);
    let mut large = willow_alloc_typed(GC_LARGE_OBJECT_THRESHOLD as i64, 0b1);
    unsafe { *(large as *mut *mut u8) = child };
    willow_push_root(&mut large);
    willow_gc_collect();
    assert_eq!(willow_gc_large_object_region_count(), 1);
    assert_eq!(
        willow_gc_old_region_live_bytes(),
        (GC_HEADER_SIZE * 2 + GC_LARGE_OBJECT_THRESHOLD + 8) as i64
    );
    assert_eq!(unsafe { *(large as *mut *mut u8) }, child);
    willow_pop_root();
    willow_gc_collect();
    reset_gc();
}

#[test]
fn pinned_region_01_metrics_include_in_place_survivor() {
    let _guard = global_gc_guard();
    reset_gc();
    let mut tls = GcTlabState {
        cursor: AtomicUsize::new(0),
        limit: AtomicUsize::new(0),
        fast_allocations: AtomicU64::new(0),
        fast_allocated_bytes: AtomicU64::new(0),
        start_bits: AtomicUsize::new(0),
    };
    let mut young = willow_gc_alloc_slow(&mut tls, 1, 1, 8, 0);
    willow_push_root(&mut young);
    willow_gc_minor_collect();
    assert_eq!(willow_gc_pinned_region_count(), 1);
    assert_eq!(willow_gc_old_region_count(), 1);
    assert_eq!(
        willow_gc_old_region_reserved_bytes(),
        GC_TLAB_CHUNK_SIZE as i64
    );
    assert_eq!(
        willow_gc_old_region_live_bytes(),
        (GC_HEADER_SIZE + 8) as i64
    );
    willow_pop_root();
    willow_gc_collect();
    reset_gc();
}

#[test]
fn pinned_region_02_major_collection_releases_unrooted_chunk() {
    let _guard = global_gc_guard();
    reset_gc();
    let mut tls = GcTlabState {
        cursor: AtomicUsize::new(0),
        limit: AtomicUsize::new(0),
        fast_allocations: AtomicU64::new(0),
        fast_allocated_bytes: AtomicU64::new(0),
        start_bits: AtomicUsize::new(0),
    };
    let mut young = willow_gc_alloc_slow(&mut tls, 1, 1, 8, 0);
    willow_push_root(&mut young);
    willow_gc_minor_collect();
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_pinned_region_count(), 0);
    assert_eq!(willow_gc_old_region_reserved_bytes(), 0);
    reset_gc();
}

#[test]
fn pinned_region_03_verifier_rejects_missing_mark() {
    let _guard = global_gc_guard();
    reset_gc();
    let mut tls = GcTlabState {
        cursor: AtomicUsize::new(0),
        limit: AtomicUsize::new(0),
        fast_allocations: AtomicU64::new(0),
        fast_allocated_bytes: AtomicU64::new(0),
        start_bits: AtomicUsize::new(0),
    };
    let mut young = willow_gc_alloc_slow(&mut tls, 1, 1, 8, 0);
    willow_push_root(&mut young);
    willow_gc_minor_collect();
    let mut state = runtime().heap.lock().unwrap();
    let chunk_index = state
        .tlab_chunks
        .iter()
        .position(|chunk| chunk.kind == RegionKind::Pinned)
        .unwrap();
    state.tlab_chunks[chunk_index].mark_bitmap.unmark(0);
    let error = verify_old_region_metadata(&state).unwrap_err();
    state.tlab_chunks[chunk_index].mark_bitmap.mark(0);
    drop(state);
    assert!(error.contains("inconsistent generation/mark metadata"));
    willow_pop_root();
    reset_gc();
}

#[test]
fn pinned_region_04_verifier_rejects_live_byte_mismatch() {
    let _guard = global_gc_guard();
    reset_gc();
    let mut tls = GcTlabState {
        cursor: AtomicUsize::new(0),
        limit: AtomicUsize::new(0),
        fast_allocations: AtomicU64::new(0),
        fast_allocated_bytes: AtomicU64::new(0),
        start_bits: AtomicUsize::new(0),
    };
    let mut young = willow_gc_alloc_slow(&mut tls, 1, 1, 8, 0);
    willow_push_root(&mut young);
    willow_gc_minor_collect();
    let mut state = runtime().heap.lock().unwrap();
    let chunk_index = state
        .tlab_chunks
        .iter()
        .position(|chunk| chunk.kind == RegionKind::Pinned)
        .unwrap();
    state.tlab_chunks[chunk_index].live_bytes += 1;
    let error = verify_old_region_metadata(&state).unwrap_err();
    state.tlab_chunks[chunk_index].live_bytes -= 1;
    drop(state);
    assert!(error.contains("live-byte mismatch"));
    willow_pop_root();
    reset_gc();
}

#[test]
fn pinned_region_05_one_survivor_retains_chunk_and_reports_sparse_liveness() {
    let _guard = global_gc_guard();
    reset_gc();
    let mut tls = GcTlabState {
        cursor: AtomicUsize::new(0),
        limit: AtomicUsize::new(0),
        fast_allocations: AtomicU64::new(0),
        fast_allocated_bytes: AtomicU64::new(0),
        start_bits: AtomicUsize::new(0),
    };
    let mut survivor = willow_gc_alloc_slow(&mut tls, 1, 1, 8, 0);
    let dead_a = tlab_fast_alloc_for_test(&tls, 2, 2, 8, 0);
    let dead_b = tlab_fast_alloc_for_test(&tls, 3, 3, 8, 0);
    unsafe {
        *(survivor as *mut i64) = 10;
        *(dead_a as *mut i64) = 20;
        *(dead_b as *mut i64) = 30;
    }
    willow_push_root(&mut survivor);

    willow_gc_minor_collect();
    willow_gc_collect();

    let object_size = GC_HEADER_SIZE + 8;
    assert_eq!(unsafe { *(survivor as *mut i64) }, 10);
    assert_eq!(willow_gc_pinned_region_count(), 1);
    assert_eq!(
        willow_gc_old_region_reserved_bytes(),
        GC_TLAB_CHUNK_SIZE as i64,
        "one live object pins the complete 32 KiB TLAB chunk"
    );
    assert_eq!(willow_gc_old_region_live_bytes(), object_size as i64);
    assert_eq!(
        willow_gc_old_region_fragmentation_bytes(),
        (object_size * 2) as i64,
        "dead allocated-prefix holes are visible as fragmentation"
    );
    assert!(
        willow_gc_old_region_reserved_bytes() - willow_gc_old_region_live_bytes()
            > willow_gc_old_region_fragmentation_bytes(),
        "unused TLAB tail is reserved capacity, not an allocated-prefix hole"
    );

    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_pinned_region_count(), 0);
    assert_eq!(willow_gc_old_region_reserved_bytes(), 0);
    reset_gc();
}

#[test]
fn pinned_region_06_dead_holes_are_not_reused_for_new_tlab_allocations() {
    let _guard = global_gc_guard();
    reset_gc();
    let mut tls = GcTlabState {
        cursor: AtomicUsize::new(0),
        limit: AtomicUsize::new(0),
        fast_allocations: AtomicU64::new(0),
        fast_allocated_bytes: AtomicU64::new(0),
        start_bits: AtomicUsize::new(0),
    };
    let mut survivor = willow_gc_alloc_slow(&mut tls, 1, 1, 8, 0);
    let dead = tlab_fast_alloc_for_test(&tls, 2, 2, 8, 0);
    willow_push_root(&mut survivor);
    willow_gc_minor_collect();

    let fresh = willow_gc_alloc_slow(&mut tls, 3, 3, 8, 0);
    assert_ne!(fresh, dead);
    let state = runtime().heap.lock().unwrap();
    let pinned = state
        .tlab_chunks
        .iter()
        .find(|chunk| chunk.kind == RegionKind::Pinned)
        .unwrap();
    let pinned_start = pinned.base as usize;
    let pinned_end = pinned_start + pinned.capacity;
    assert!((fresh as usize) < pinned_start || (fresh as usize) >= pinned_end);
    assert!(state.tlab_chunks.iter().any(|chunk| {
        let start = chunk.base as usize;
        let end = start + chunk.capacity;
        chunk.kind == RegionKind::Nursery && fresh as usize >= start && (fresh as usize) < end
    }));
    drop(state);
    assert_eq!(
        willow_gc_tlab_reserved_bytes(),
        (GC_TLAB_CHUNK_SIZE * 2) as i64
    );

    willow_pop_root();
    willow_gc_collect();
    reset_gc();
}

#[test]
fn remembered_region_01_duplicate_barrier_hits_only_once() {
    let _guard = global_gc_guard();
    reset_gc();
    let mut tls = GcTlabState {
        cursor: AtomicUsize::new(0),
        limit: AtomicUsize::new(0),
        fast_allocations: AtomicU64::new(0),
        fast_allocated_bytes: AtomicU64::new(0),
        start_bits: AtomicUsize::new(0),
    };
    let young = willow_gc_alloc_slow(&mut tls, 1, 1, 8, 0);
    let owner = willow_alloc_typed(8, 0b1);
    willow_gc_write_barrier(
        owner,
        std::ptr::null_mut(),
        young,
        GcStoreDestination::ObjectField as i64,
    );
    willow_gc_write_barrier(
        owner,
        std::ptr::null_mut(),
        young,
        GcStoreDestination::ObjectField as i64,
    );
    assert_eq!(willow_gc_remembered_set_size(), 1);
    assert_eq!(willow_gc_write_barrier_hits(), 1);
    reset_gc();
}

#[test]
fn remembered_region_02_two_owners_can_share_one_card() {
    let _guard = global_gc_guard();
    reset_gc();
    let owners: Vec<*mut u8> = (0..16).map(|_| willow_alloc_typed(8, 0b1)).collect();
    let pair = owners
        .windows(2)
        .find(|pair| pair[0] as usize / GC_CARD_SIZE == pair[1] as usize / GC_CARD_SIZE)
        .unwrap();
    let mut tls = GcTlabState {
        cursor: AtomicUsize::new(0),
        limit: AtomicUsize::new(0),
        fast_allocations: AtomicU64::new(0),
        fast_allocated_bytes: AtomicU64::new(0),
        start_bits: AtomicUsize::new(0),
    };
    let young = willow_gc_alloc_slow(&mut tls, 1, 1, 8, 0);
    for &owner in pair {
        willow_gc_write_barrier(
            owner,
            std::ptr::null_mut(),
            young,
            GcStoreDestination::ObjectField as i64,
        );
    }
    assert_eq!(willow_gc_remembered_set_size(), 2);
    assert_eq!(willow_gc_dirty_card_count(), 1);
    reset_gc();
}

#[test]
fn remembered_region_03_sweeping_one_shared_card_owner_keeps_card_dirty() {
    let _guard = global_gc_guard();
    reset_gc();
    let owners: Vec<*mut u8> = (0..16).map(|_| willow_alloc_typed(8, 0b1)).collect();
    let pair = owners
        .windows(2)
        .find(|pair| pair[0] as usize / GC_CARD_SIZE == pair[1] as usize / GC_CARD_SIZE)
        .unwrap();
    let mut live_owner = pair[0];
    let dead_owner = pair[1];
    let mut tls = GcTlabState {
        cursor: AtomicUsize::new(0),
        limit: AtomicUsize::new(0),
        fast_allocations: AtomicU64::new(0),
        fast_allocated_bytes: AtomicU64::new(0),
        start_bits: AtomicUsize::new(0),
    };
    let young = willow_gc_alloc_slow(&mut tls, 1, 1, 8, 0);
    unsafe {
        *(live_owner as *mut *mut u8) = young;
        *(dead_owner as *mut *mut u8) = young;
    }
    willow_gc_write_barrier(
        live_owner,
        std::ptr::null_mut(),
        young,
        GcStoreDestination::ObjectField as i64,
    );
    willow_gc_write_barrier(
        dead_owner,
        std::ptr::null_mut(),
        young,
        GcStoreDestination::ObjectField as i64,
    );
    willow_push_root(&mut live_owner);
    willow_gc_collect();
    assert_eq!(willow_gc_remembered_set_size(), 1);
    assert_eq!(willow_gc_dirty_card_count(), 1);
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_remembered_set_size(), 0);
    assert_eq!(willow_gc_dirty_card_count(), 0);
    reset_gc();
}

#[test]
fn remembered_region_04_global_destination_is_not_remembered() {
    let _guard = global_gc_guard();
    reset_gc();
    let mut tls = GcTlabState {
        cursor: AtomicUsize::new(0),
        limit: AtomicUsize::new(0),
        fast_allocations: AtomicU64::new(0),
        fast_allocated_bytes: AtomicU64::new(0),
        start_bits: AtomicUsize::new(0),
    };
    let young = willow_gc_alloc_slow(&mut tls, 1, 1, 8, 0);
    let owner = willow_alloc_typed(8, 0b1);
    willow_gc_write_barrier(
        owner,
        std::ptr::null_mut(),
        young,
        GcStoreDestination::GlobalStatic as i64,
    );
    assert_eq!(willow_gc_remembered_set_size(), 0);
    assert_eq!(willow_gc_dirty_card_count(), 0);
    reset_gc();
}

#[test]
fn remembered_region_05_pinned_owner_tracks_new_young_child() {
    let _guard = global_gc_guard();
    reset_gc();
    let mut tls = GcTlabState {
        cursor: AtomicUsize::new(0),
        limit: AtomicUsize::new(0),
        fast_allocations: AtomicU64::new(0),
        fast_allocated_bytes: AtomicU64::new(0),
        start_bits: AtomicUsize::new(0),
    };
    let mut owner = willow_gc_alloc_slow(&mut tls, 1, 1, 8, 0b1);
    willow_push_root(&mut owner);
    willow_gc_minor_collect();
    let child = willow_gc_alloc_slow(&mut tls, 2, 2, 8, 0);
    unsafe { *(child as *mut i64) = 77 };
    willow_gc_write_barrier(
        owner,
        std::ptr::null_mut(),
        child,
        GcStoreDestination::ObjectField as i64,
    );
    unsafe { *(owner as *mut *mut u8) = child };
    assert_eq!(willow_gc_remembered_set_size(), 1);
    willow_gc_minor_collect();
    let moved = unsafe { *(owner as *mut *mut u8) };
    assert_ne!(moved, child);
    assert_eq!(unsafe { *(moved as *mut i64) }, 77);
    assert_eq!(willow_gc_remembered_set_size(), 1);
    willow_gc_minor_collect();
    assert_eq!(willow_gc_remembered_set_size(), 0);
    willow_pop_root();
    willow_gc_collect();
    reset_gc();
}

#[test]
fn old_region_index_allocation_attempts_scale_linearly() {
    for n in [4096, 8192, 16384, 32768] {
        let mut state = GcState {
            memory_limit_bytes: None,
            ..GcState::default()
        };
        for _ in 0..n {
            allocate_old_region_object_locked(&mut state, 11, 7, 8, 0, true).unwrap();
        }
        assert_eq!(
            state
                .old_regions
                .iter()
                .map(|region| region.allocation_attempts)
                .sum::<usize>(),
            n
        );
        assert!(state.old_region_candidates.len() <= 1);
        verify_old_region_metadata(&state).unwrap();
        eprintln!(
            "old-region index objects={n} regions={} attempts={}",
            state.old_regions.len(),
            state
                .old_regions
                .iter()
                .map(|region| region.allocation_attempts)
                .sum::<usize>()
        );
    }
}

#[test]
fn old_region_index_mixed_sizes_reuse_small_tails_at_memory_limit() {
    let mut state = GcState {
        memory_limit_bytes: Some(4 * GC_OLD_REGION_SIZE),
        ..GcState::default()
    };
    let large_regular_payload = GC_LARGE_OBJECT_THRESHOLD - GC_HEADER_SIZE - span_size(8);
    for _ in 0..8 {
        allocate_old_region_object_locked(&mut state, 11, 7, large_regular_payload, 0, true)
            .unwrap();
    }
    assert_eq!(state.old_regions.len(), 4);
    assert!(
        allocate_old_region_object_locked(&mut state, 11, 7, large_regular_payload, 0, true)
            .is_none()
    );
    for _ in 0..8 {
        allocate_old_region_object_locked(&mut state, 11, 7, 8, 0, true).unwrap();
    }
    assert!(state.old_region_candidates.is_empty());
    assert!(allocate_old_region_object_locked(&mut state, 11, 7, 0, 0, true).is_none());
    assert_eq!(
        state
            .old_regions
            .iter()
            .map(|region| region.allocation_attempts)
            .sum::<usize>(),
        16
    );
    verify_old_region_metadata(&state).unwrap();
}

#[test]
fn old_region_index_sweep_rebuilds_holes_and_compacted_indices() {
    let _guard = global_gc_guard();
    reset_gc();
    let payload_size = GC_OLD_REGION_SIZE / 4 - GC_HEADER_SIZE;
    let mut objects = Vec::new();
    {
        let mut state = runtime().heap.lock().unwrap();
        for _ in 0..12 {
            objects.push(
                allocate_old_region_object_locked(&mut state, 11, 7, payload_size, 0, true)
                    .unwrap(),
            );
        }
        assert_eq!(state.old_regions.len(), 3);
        assert!(state.old_region_candidates.is_empty());
    }
    // Drop the first region entirely and leave an interior hole in each survivor.
    for i in [4, 6, 7, 8, 10, 11] {
        unsafe {
            (*objects[i].as_ptr()).marked = true;
        }
    }
    sweep();
    {
        let mut state = runtime().heap.lock().unwrap();
        assert_eq!(state.old_regions.len(), 2);
        assert_eq!(state.old_reserved_bytes, 2 * GC_OLD_REGION_SIZE);
        state.memory_limit_bytes = Some(state.old_reserved_bytes);
        for i in [9, 5] {
            let replacement =
                allocate_old_region_object_locked(&mut state, 11, 7, payload_size, 0, true)
                    .unwrap();
            assert_eq!(replacement.as_ptr(), objects[i].as_ptr());
        }
        assert_eq!(state.old_region_reuses, 2);
        assert_eq!(state.old_region_allocations, 14);
        assert!(state.old_region_candidates.is_empty());
        verify_old_region_metadata(&state).unwrap();
    }
    reset_gc();
}

#[test]
fn old_region_index_large_objects_and_overflow_do_not_enter_index() {
    let mut state = GcState {
        memory_limit_bytes: None,
        ..GcState::default()
    };
    for _ in 0..8 {
        allocate_old_region_object_locked(&mut state, 11, 7, GC_LARGE_OBJECT_THRESHOLD, 0, true)
            .unwrap();
    }
    assert_eq!(state.old_regions.len(), 8);
    assert!(state.old_region_candidates.is_empty());
    let reserved = state.old_reserved_bytes;
    assert!(allocate_old_region_object_locked(&mut state, 11, 7, usize::MAX, 0, true).is_none());
    assert_eq!(state.old_reserved_bytes, reserved);
    verify_old_region_metadata(&state).unwrap();
}

#[test]
fn old_region_index_fragmented_regions_remain_candidates() {
    let _guard = global_gc_guard();
    for n in [32, 64, 128, 256] {
        reset_gc();
        {
            let mut state = runtime().heap.lock().unwrap();
            // Four objects fill a region; retain alternating objects.
            for i in 0..4 * n {
                let object = allocate_old_region_object_locked(
                    &mut state,
                    11,
                    7,
                    GC_OLD_REGION_SIZE / 4 - GC_HEADER_SIZE,
                    0,
                    true,
                )
                .unwrap();
                if i % 2 == 0 {
                    unsafe {
                        (*object.as_ptr()).marked = true;
                    }
                }
            }
        }
        sweep();
        {
            let mut state = runtime().heap.lock().unwrap();
            assert_eq!(state.old_region_candidates.len(), n);
            state.memory_limit_bytes = Some(state.old_reserved_bytes);
            for _ in 0..2 * n {
                allocate_old_region_object_locked(
                    &mut state,
                    11,
                    7,
                    GC_OLD_REGION_SIZE / 4 - GC_HEADER_SIZE,
                    0,
                    true,
                )
                .unwrap();
            }
            assert_eq!(state.old_region_reuses, n as u64);
            assert_eq!(
                state
                    .old_regions
                    .iter()
                    .map(|r| r.allocation_attempts)
                    .sum::<usize>(),
                6 * n
            );
            assert!(state.old_region_candidates.is_empty());
            verify_old_region_metadata(&state).unwrap();
            eprintln!(
                "fragmented index regions={n} allocations={} attempts={}",
                6 * n,
                6 * n
            );
        }
    }
    reset_gc();
}

#[test]
fn barrier_region_index_survives_fragmented_region_compaction() {
    let _guard = runtime_test_guard();
    for count in [16usize, 64, 256] {
        reset_internal_for_test();
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        let payload_size = GC_LARGE_OBJECT_THRESHOLD + 8;
        let addresses: Vec<_> = (0..count)
            .map(|_| willow_alloc(payload_size as i64))
            .collect();
        let mut roots: Vec<_> = addresses.iter().step_by(2).copied().collect();
        for root in &mut roots {
            willow_push_root(root);
        }
        for round in 0..2 {
            let state = runtime().heap.lock().unwrap();
            address_index::take_comparisons();
            for (index, &address) in addresses.iter().enumerate() {
                let alive = round == 0 || index % 2 == 0;
                assert_eq!(
                    payload_generation(&state, address),
                    alive.then_some(GC_GENERATION_OLD)
                );
                assert_eq!(
                    barrier_owner_payload(
                        &state,
                        (address as usize + 1) as *mut u8,
                        GcStoreDestination::IndirectReference as i64
                    ),
                    alive.then_some(address as usize)
                );
                assert_eq!(
                    payload_generation(&state, (address as usize + 1) as *mut u8),
                    None
                );
            }
            assert_eq!(
                payload_generation(&state, std::ptr::dangling_mut::<u8>()),
                None
            );
            assert_eq!(payload_generation(&state, usize::MAX as *mut u8), None);
            let comparisons = address_index::take_comparisons();
            assert!(comparisons < count * 36 * (count.ilog2() as usize + 1));
            println!("barrier-index regions={count} round={round} comparisons={comparisons}");
            drop(state);
            if round == 0 {
                willow_gc_collect();
            }
        }
        willow_pop_roots(roots.len() as i32);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
    }
    reset_internal_for_test();
}

#[test]
fn retired_tlab_header_index_bounds_lookup_work_and_rejects_dead_headers() {
    let _guard = runtime_test_guard();
    for count in [16usize, 64, 256] {
        reset_internal_for_test();
        let mut tls = tlab_state_for_test();
        let first = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
        let base = payload_to_header(first).cast::<u8>();
        let size = GC_HEADER_SIZE + 8;
        // Simulate generated fast allocation using the actual ABI header helper.
        for index in 1..count {
            initialize_object_at(unsafe { base.add(index * size) }, size, 0, 0, 0).unwrap();
            publish_tlab_start_for_test(&tls, unsafe { base.add(index * size) });
        }
        tls.cursor
            .store(base as usize + count * size, Ordering::Release);
        tls.fast_allocations
            .store((count - 1) as u64, Ordering::Release);
        tls.fast_allocated_bytes
            .store(((count - 1) * size) as u64, Ordering::Release);
        let mut state = runtime().heap.lock().unwrap();
        assert_eq!(retire_all_tlabs_locked(&mut state), count);
        let chunk = &state.tlab_chunks[0];
        RETIRED_LOOKUP_COMPARISONS.set(0);
        for index in 0..count {
            let address = unsafe { base.add(index * size + GC_HEADER_SIZE) } as usize;
            assert!(object_in_retired_chunk(chunk, address, false).is_some());
            assert!(object_in_retired_chunk(chunk, address + 1, true).is_some());
            assert!(object_in_retired_chunk(chunk, address + 1, false).is_none());
        }
        let comparisons = RETIRED_LOOKUP_COMPARISONS.get();
        assert!(comparisons <= count * 3 * (count.ilog2() as usize + 2));
        println!(
            "retired-lookup headers={count} queries={} comparisons={comparisons}",
            count * 3
        );
        let dead = HeapObject::from_raw(base.cast()).unwrap();
        dead.reclaim_in_place();
        assert!(object_in_retired_chunk(chunk, first as usize, false).is_none());
        assert!(object_in_retired_chunk(chunk, first as usize + 1, true).is_none());
        drop(state);
        reset_internal_for_test();
    }
}

#[test]
fn barrier_without_nursery_needs_no_heap_lock_and_keeps_counters() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let value = willow_alloc(8) as usize;
    let state = runtime().heap.lock().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        for _ in 0..1024 {
            willow_gc_add_runtime_root(value as *mut u8);
            willow_gc_write_barrier(
                value as *mut u8,
                std::ptr::null_mut(),
                value as *mut u8,
                GcStoreDestination::AsyncFrameSlot as i64,
            );
            willow_gc_remove_runtime_root(value as *mut u8);
        }
        tx.send(()).unwrap();
    });
    let completed = rx.recv_timeout(std::time::Duration::from_secs(2));
    // Release before asserting/joining so a regression reports a failure,
    // rather than leaving the worker permanently blocked behind this test.
    drop(state);
    worker.join().unwrap();
    assert!(
        completed.is_ok(),
        "inactive old-only barrier took the heap lock"
    );
    assert_eq!(telemetry_heap_snapshot().0.barrier_calls, 1024);
    assert_eq!(runtime().runtime_roots.len(), 0);
    willow_gc_write_barrier(
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        0,
    );
    willow_gc_write_barrier(
        std::ptr::null_mut(),
        value as *mut u8,
        std::ptr::null_mut(),
        0,
    );
    assert_eq!(telemetry_heap_snapshot().0.barrier_calls, 1024);
    runtime()
        .write_barrier_calls
        .store(u64::MAX, Ordering::Relaxed);
    willow_gc_write_barrier(
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        value as *mut u8,
        0,
    );
    assert_eq!(telemetry_heap_snapshot().0.barrier_calls, u64::MAX);
    reset_internal_for_test();
    assert_eq!(runtime().write_barrier_calls.load(Ordering::Relaxed), 0);
    assert!(!runtime().tlab_ever_allocated.load(Ordering::Acquire));
}

#[test]
fn barrier_young_test_skips_old_indexes_and_preserves_tlab_classification() {
    let _guard = runtime_test_guard();
    for regions in [4usize, 16, 64, 256] {
        reset_internal_for_test();
        let old: Vec<_> = {
            let mut state = runtime().heap.lock().unwrap();
            state.threshold_bytes = usize::MAX;
            state.nursery_threshold_bytes = usize::MAX;
            (0..regions)
                .map(|_| {
                    allocate_old_region_object_locked(
                        &mut state,
                        0,
                        0,
                        GC_LARGE_OBJECT_THRESHOLD + 8,
                        0,
                        true,
                    )
                    .unwrap()
                    .payload()
                    .as_ptr()
                })
                .collect()
        };
        for repetitions in [1usize, 4] {
            address_index::take_comparisons();
            {
                let state = runtime().heap.lock().unwrap();
                for _ in 0..repetitions {
                    for &value in &old {
                        for _ in 0..2 {
                            assert_eq!(payload_generation(&state, value), Some(GC_GENERATION_OLD));
                        }
                    }
                }
            }
            let previous_comparisons = address_index::take_comparisons();
            assert!(previous_comparisons > 0);
            for _ in 0..repetitions {
                for &value in &old {
                    // Two non-null reference stores per idle_worker spawn/first poll.
                    for _ in 0..2 {
                        willow_gc_write_barrier(
                            old[0],
                            std::ptr::null_mut(),
                            value,
                            GcStoreDestination::AsyncFrameSlot as i64,
                        );
                    }
                }
            }
            let comparisons = address_index::take_comparisons();
            assert_eq!(
                comparisons, 0,
                "old-only barriers must not search old indexes"
            );
            println!(
                "young-barrier regions={regions} stores={} previous_index_comparisons={previous_comparisons} index_comparisons={comparisons}",
                regions * repetitions * 2
            );
        }
        let mut tls = tlab_state_for_test();
        let mut young = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
        willow_push_root(&mut young);
        assert!(runtime().tlab_ever_allocated.load(Ordering::Acquire));
        address_index::take_comparisons();
        for &value in &old {
            willow_gc_write_barrier(
                old[0],
                std::ptr::null_mut(),
                value,
                GcStoreDestination::AsyncFrameSlot as i64,
            );
        }
        let comparisons = address_index::take_comparisons();
        assert!(
            comparisons <= regions,
            "only the single TLAB index may be searched"
        );
        println!(
            "young-barrier-with-tlab regions={regions} stores={regions} index_comparisons={comparisons}"
        );
        for phase in 0..3 {
            if phase == 1 {
                retire_all_tlabs_locked(&mut runtime().heap.lock().unwrap());
            } else if phase == 2 {
                willow_gc_minor_collect();
            }
            let state = runtime().heap.lock().unwrap();
            for address in [
                0,
                1,
                usize::MAX,
                young as usize,
                young as usize + 1,
                young as usize - GC_HEADER_SIZE,
                old[0] as usize,
                old[0] as usize + 1,
            ] {
                assert_eq!(
                    tlab_payload_generation(&state, address) == Some(GC_GENERATION_YOUNG),
                    payload_generation(&state, address as *mut u8) == Some(GC_GENERATION_YOUNG),
                    "regions={regions} phase={phase} address={address}"
                );
            }
            assert_eq!(
                tlab_payload_generation(&state, young as usize),
                Some(if phase == 2 {
                    GC_GENERATION_OLD
                } else {
                    GC_GENERATION_YOUNG
                })
            );
        }
        willow_pop_root();
        reset_internal_for_test();
    }
}

#[test]
fn old_region_lookup_bounds_work_for_hits_misses_nursery_and_pinned_pointers() {
    let _guard = runtime_test_guard();
    for count in [16usize, 64, 256, 1024] {
        reset_internal_for_test();
        {
            let mut state = runtime().heap.lock().unwrap();
            state.threshold_bytes = usize::MAX;
            state.nursery_threshold_bytes = usize::MAX;
        }
        let old: Vec<_> = (0..count)
            .map(|_| willow_alloc((GC_LARGE_OBJECT_THRESHOLD + 8) as i64))
            .collect();
        let mut tls = tlab_state_for_test();
        let mut young = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
        willow_push_root(&mut young);
        for pinned in [false, true] {
            if pinned {
                willow_gc_minor_collect();
            }
            let state = runtime().heap.lock().unwrap();
            assert_eq!(state.old_regions.len(), count);
            assert_eq!(
                payload_generation(&state, young),
                Some(if pinned {
                    GC_GENERATION_OLD
                } else {
                    GC_GENERATION_YOUNG
                })
            );
            let mut single_pass_comparisons = 0;
            for repetitions in [1, 4] {
                address_index::take_comparisons();
                for query in 0..count * repetitions {
                    let address = old[query % count] as usize;
                    assert_eq!(
                        find_old_region_object(&state, address, false)
                            .map(|object| object.payload().as_ptr() as usize),
                        Some(address)
                    );
                    assert!(find_old_region_object(&state, address + 1, true).is_some());
                    assert!(find_old_region_object(&state, address + 1, false).is_none());
                    assert!(find_old_region_object(&state, young as usize, false).is_none());
                    assert!(find_old_region_object(&state, young as usize + 1, true).is_none());
                    assert!(find_old_region_object(&state, usize::MAX, true).is_none());
                }
                let queries = count * repetitions * 6;
                let comparisons = address_index::take_comparisons();
                assert!(comparisons > 0, "lookup must exercise the address index");
                assert!(comparisons <= queries * 12 * (count.ilog2() as usize + 1));
                if repetitions == 1 {
                    single_pass_comparisons = comparisons;
                } else {
                    assert_eq!(comparisons, single_pass_comparisons * repetitions);
                }
                println!(
                    "old-lookup regions={count} queries={queries} pinned={pinned} comparisons={comparisons}"
                );
            }
        }
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
    }
    reset_internal_for_test();
}

#[test]
fn barrier_tlab_range_index_survives_minor_swap_removal() {
    let _guard = runtime_test_guard();
    for count in [16usize, 64, 256] {
        reset_internal_for_test();
        let mut tls = tlab_state_for_test();
        {
            let mut state = runtime().heap.lock().unwrap();
            state.threshold_bytes = usize::MAX;
            state.nursery_threshold_bytes = usize::MAX;
        }
        let addresses: Vec<_> = (0..count)
            .map(|_| willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0))
            .collect();
        let mut roots: Vec<_> = addresses.iter().step_by(2).copied().collect();
        for root in &mut roots {
            willow_push_root(root);
        }
        willow_gc_minor_collect();
        let state = runtime().heap.lock().unwrap();
        assert_eq!(state.tlab_chunks.len(), count / 2);
        for (index, &address) in addresses.iter().enumerate() {
            let alive = index % 2 == 0;
            assert_eq!(
                payload_generation(&state, address),
                alive.then_some(GC_GENERATION_OLD)
            );
            assert_eq!(
                barrier_owner_payload(
                    &state,
                    (address as usize + 1) as *mut u8,
                    GcStoreDestination::IndirectReference as i64
                ),
                alive.then_some(address as usize)
            );
        }
        drop(state);
        willow_pop_roots(roots.len() as i32);
        willow_gc_collect();
        assert!(runtime().heap.lock().unwrap().tlab_chunks.is_empty());
    }
    reset_internal_for_test();
}

#[test]
fn zero_payload_at_region_end_remains_a_valid_exact_root() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    willow_alloc((GC_OLD_REGION_SIZE / 2 - GC_HEADER_SIZE) as i64);
    willow_alloc((GC_OLD_REGION_SIZE / 2 - 2 * GC_HEADER_SIZE) as i64);
    let mut empty = willow_alloc(0);
    {
        let state = runtime().heap.lock().unwrap();
        assert_eq!(state.old_regions.len(), 1);
        assert_eq!(empty as usize, state.old_regions[0].end());
        assert_eq!(payload_generation(&state, empty), Some(GC_GENERATION_OLD));
        assert!(find_old_region_object(&state, empty as usize, false).is_some());
        assert!(find_old_region_object(&state, empty as usize, true).is_none());
    }
    willow_push_root(&mut empty);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), GC_HEADER_SIZE as i64);
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn pending_sweep_colors_reused_holes_before_publication() {
    let mut region = regular_region(1024);
    let hole = region_alloc(&mut region, 8);
    let _tail = region_alloc(&mut region, 8);
    region.release_object(hole);
    region.sweep_pending = true;
    let (replacement, reused) = region.allocate_object(7, 11, 0, 8).unwrap();
    assert!(reused);
    assert_eq!(replacement.as_ptr(), hole.as_ptr());
    assert!(replacement.marked());
    assert!(region.mark_bitmap.is_marked(0));
}

#[test]
#[cfg(debug_assertions)]
fn active_tlab_validation_requires_publication_not_just_an_advanced_cursor() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let mut tls = tlab_state_for_test();
    let first = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
    assert!(validate_payload_pointer(first, "published slow allocation").is_ok());
    let header = tls.cursor.load(Ordering::Acquire) as *mut u8;
    let bytes = GC_HEADER_SIZE + 8;
    let payload = unsafe { header.add(GC_HEADER_SIZE) };
    // Generated code can advance the cursor before header initialization. An
    // unpublished tail must never be parsed, even if its flag bytes are invalid.
    unsafe {
        std::ptr::write_bytes(header, 0xff, bytes);
    }
    tls.cursor.store(header as usize + bytes, Ordering::Release);
    let address = payload as usize;
    assert!(
        std::thread::spawn(move || validate_payload_pointer(
            address as *mut u8,
            "unpublished tail"
        )
        .is_err())
        .join()
        .unwrap()
    );
    initialize_object_at(header, bytes, 0, 0, 0).unwrap();
    assert!(validate_payload_pointer(payload, "initialized but unpublished").is_err());
    publish_tlab_start_for_test(&tls, header);
    tls.fast_allocations.store(1, Ordering::Release);
    tls.fast_allocated_bytes
        .store(bytes as u64, Ordering::Release);
    assert!(
        std::thread::spawn(
            move || validate_payload_pointer(address as *mut u8, "published tail").is_ok()
        )
        .join()
        .unwrap()
    );
    assert!(validate_payload_pointer(unsafe { payload.add(1) }, "interior").is_err());
    let mut state = runtime().heap.lock().unwrap();
    assert_eq!(retire_all_tlabs_locked(&mut state), 2);
    assert_eq!(tls.start_bits.load(Ordering::Acquire), 0);
    drop(state);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}
