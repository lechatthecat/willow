use super::*;

const SMALL_OBJECT_SIZE: usize = GC_HEADER_SIZE + 8;

fn stress_guard() -> std::sync::MutexGuard<'static, ()> {
    runtime_test_guard()
}

fn reset_gc() {
    reset_internal();
}

fn assert_global_regions_valid() {
    let state = runtime().heap.lock().unwrap();
    if let Err(message) = verify_old_region_metadata(&state) {
        panic!("stress test found invalid region metadata: {message}");
    }
}

fn new_tlab_state() -> GcTlabState {
    GcTlabState {
        cursor: AtomicUsize::new(0),
        limit: AtomicUsize::new(0),
        fast_allocations: AtomicU64::new(0),
        fast_allocated_bytes: AtomicU64::new(0),
    }
}

fn tlab_fast_alloc(
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
    tls.cursor.store(cursor + total_size, Ordering::Release);
    tls.fast_allocations.fetch_add(1, Ordering::Relaxed);
    tls.fast_allocated_bytes
        .fetch_add(total_size as u64, Ordering::Relaxed);
    object.payload().as_ptr()
}

fn assert_local_region_valid(region: &OldRegion) {
    assert!(region.used <= region.capacity);
    let mut intervals = Vec::new();
    let mut live_bytes = 0usize;
    for (&offset, &size) in &region.allocations {
        assert!(offset.is_multiple_of(GC_REGION_MARK_GRANULE));
        assert!(size.is_multiple_of(GC_REGION_MARK_GRANULE));
        assert!(offset + size <= region.used);
        let object =
            HeapObject::from_raw(unsafe { region.base.add(offset) }.cast()).expect("live header");
        assert!(object.allocated());
        assert_eq!(object.generation(), GC_GENERATION_OLD);
        assert!(object.size() <= size);
        assert!(region.mark_bitmap.is_marked(offset));
        live_bytes += object.size();
        intervals.push((offset, offset + size));
    }
    for span in &region.free_spans {
        assert!(span.size > 0);
        assert!(span.offset + span.size <= region.used);
        intervals.push((span.offset, span.offset + span.size));
    }
    intervals.sort_unstable();
    for pair in intervals.windows(2) {
        assert!(pair[0].1 <= pair[1].0);
    }
    assert_eq!(region.live_bytes, live_bytes);
}

#[test]
#[ignore = "explicit GC stress suite"]
fn stress_region_01_middle_hole_churn_reuses_one_region() {
    let _guard = stress_guard();
    reset_gc();
    let mut left = willow_alloc_object(1, 8);
    for _ in 0..1000 {
        let _garbage = willow_alloc_object(2, 8);
    }
    let mut right = willow_alloc_object(3, 8);
    willow_push_root(&mut left);
    willow_push_root(&mut right);
    willow_gc_collect();
    assert_eq!(willow_gc_old_region_count(), 1);

    let initial_reuses = willow_gc_old_region_reuses();
    for round in 0..200 {
        for value in 0..1000i64 {
            let object = willow_alloc_object(4, 8);
            unsafe { *(object as *mut i64) = value ^ round };
        }
        willow_gc_collect();
        assert_eq!(willow_gc_old_region_count(), 1);
        assert_global_regions_valid();
    }
    assert!(willow_gc_old_region_reuses() >= initial_reuses + 200_000);

    willow_pop_roots(2);
    willow_gc_collect();
    assert_eq!(willow_gc_old_region_count(), 0);
    reset_gc();
}

#[test]
#[ignore = "explicit GC stress suite"]
fn stress_region_02_randomized_free_span_allocator_preserves_invariants() {
    let capacity = 64 * 1024;
    let mut region = OldRegion::new(RegionKind::Old, capacity).unwrap();
    let payload_sizes = [0usize, 1, 7, 8, 9, 17, 31, 64, 127, 255];
    let mut live = Vec::<HeapObject>::new();
    let mut seed = 0x4d59_5df4_d0f3_3173u64;
    let mut reuse_count = 0usize;

    for step in 0..100_000 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let allocate = live.is_empty() || seed & 3 != 0;
        if allocate {
            let size = payload_sizes[(seed as usize >> 8) % payload_sizes.len()];
            if let Some((object, reused)) = region.allocate_object(step as u32, seed, 0, size) {
                reuse_count += usize::from(reused);
                live.push(object);
            } else {
                let index = (seed as usize >> 16) % live.len();
                let object = live.swap_remove(index);
                object.reclaim_in_place();
                region.release_object(object);
            }
        } else {
            let index = (seed as usize >> 16) % live.len();
            let object = live.swap_remove(index);
            object.reclaim_in_place();
            region.release_object(object);
        }
        if step % 257 == 0 {
            assert_local_region_valid(&region);
        }
    }

    for object in live {
        object.reclaim_in_place();
        region.release_object(object);
    }
    assert_local_region_valid(&region);
    assert_eq!(region.used, 0);
    assert_eq!(region.live_bytes, 0);
    assert!(reuse_count > 1000);
}

#[test]
#[ignore = "explicit GC stress suite"]
fn stress_region_03_sparse_survivors_across_many_regions_stay_stable() {
    let _guard = stress_guard();
    reset_gc();
    const OBJECTS: usize = 50_000;
    const ROOT_STRIDE: usize = 5_000;
    let mut objects = Vec::with_capacity(OBJECTS);
    for value in 0..OBJECTS {
        let object = willow_alloc_object(value as i64 + 1, 8);
        unsafe { *(object as *mut i64) = value as i64 };
        objects.push(object);
    }
    let mut roots: Vec<*mut u8> = objects.iter().step_by(ROOT_STRIDE).copied().collect();
    let original = roots.clone();
    for root in &mut roots {
        willow_push_root(root);
    }

    willow_gc_collect();

    assert!(willow_gc_old_region_count() > 1);
    assert_eq!(
        willow_gc_allocated_bytes(),
        (roots.len() * SMALL_OBJECT_SIZE) as i64
    );
    for (index, (&root, &address)) in roots.iter().zip(&original).enumerate() {
        assert_eq!(root, address);
        assert_eq!(unsafe { *(root as *mut i64) }, (index * ROOT_STRIDE) as i64);
    }
    assert_global_regions_valid();

    willow_pop_roots(roots.len() as i32);
    willow_gc_collect();
    assert_eq!(willow_gc_old_region_count(), 0);
    reset_gc();
}

#[test]
#[ignore = "explicit GC stress suite"]
fn stress_region_04_large_and_regular_cycles_release_every_region() {
    let _guard = stress_guard();
    reset_gc();
    for round in 0..100 {
        for _ in 0..64 {
            let _garbage = willow_alloc_object(1, 8);
        }
        for _ in 0..3 {
            let _garbage = willow_alloc_object(2, GC_LARGE_OBJECT_THRESHOLD as i64);
        }
        let large = willow_alloc_object(3, GC_LARGE_OBJECT_THRESHOLD as i64);
        let mut parent = willow_alloc_typed(8, 0b1);
        unsafe { *(parent as *mut *mut u8) = large };
        willow_push_root(&mut parent);

        willow_gc_collect();

        assert_eq!(willow_gc_large_object_region_count(), 1);
        assert_eq!(unsafe { *(parent as *mut *mut u8) }, large);
        assert_global_regions_valid();

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(
            willow_gc_old_region_count(),
            0,
            "round {round} leaked a region"
        );
    }
    assert!(willow_gc_old_regions_released() >= 500);
    reset_gc();
}

#[test]
#[ignore = "explicit GC stress suite"]
fn stress_region_05_minor_major_and_remembered_set_interleave() {
    let _guard = stress_guard();
    reset_gc();
    let mut parent = willow_alloc_typed(8, 0b1);
    willow_push_root(&mut parent);
    let mut tls = new_tlab_state();

    for round in 0..1000i64 {
        let young = willow_gc_alloc_slow(&mut tls, 2, 2, 8, 0);
        unsafe { *(young as *mut i64) = round };
        willow_gc_write_barrier(parent, young, GcStoreDestination::ObjectField as i64);
        unsafe { *(parent as *mut *mut u8) = young };
        assert_eq!(willow_gc_remembered_set_size(), 1);

        willow_gc_minor_collect();

        let promoted = unsafe { *(parent as *mut *mut u8) };
        assert_ne!(promoted, young);
        assert_eq!(unsafe { *(promoted as *mut i64) }, round);
        assert_eq!(willow_gc_remembered_set_size(), 0);
        if round % 10 == 0 {
            willow_gc_collect();
            assert_global_regions_valid();
        }
    }

    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), (SMALL_OBJECT_SIZE * 2) as i64);
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

#[test]
#[ignore = "explicit GC stress suite"]
fn stress_region_06_many_sparse_pinned_chunks_are_eventually_released() {
    let _guard = stress_guard();
    reset_gc();
    const CHUNKS: usize = 64;
    const OBJECTS_PER_CHUNK: usize = 64;
    let mut states: Vec<Box<GcTlabState>> =
        (0..CHUNKS).map(|_| Box::new(new_tlab_state())).collect();
    let mut survivors = Vec::with_capacity(CHUNKS);

    for (index, tls) in states.iter_mut().enumerate() {
        let survivor = willow_gc_alloc_slow(&mut **tls, 1, index as i64 + 1, 8, 0);
        unsafe { *(survivor as *mut i64) = index as i64 };
        survivors.push(survivor);
        for object_index in 1..OBJECTS_PER_CHUNK {
            let dead = tlab_fast_alloc(tls, 2, object_index as u32, 8, 0);
            unsafe { *(dead as *mut i64) = object_index as i64 };
        }
    }
    for survivor in &mut survivors {
        willow_push_root(survivor);
    }

    willow_gc_minor_collect();
    willow_gc_collect();

    assert_eq!(willow_gc_pinned_region_count(), CHUNKS as i64);
    assert_eq!(
        willow_gc_old_region_reserved_bytes(),
        (CHUNKS * GC_TLAB_CHUNK_SIZE) as i64
    );
    assert_eq!(
        willow_gc_old_region_live_bytes(),
        (CHUNKS * SMALL_OBJECT_SIZE) as i64
    );
    assert_eq!(
        willow_gc_old_region_fragmentation_bytes(),
        (CHUNKS * (OBJECTS_PER_CHUNK - 1) * SMALL_OBJECT_SIZE) as i64
    );
    for (index, survivor) in survivors.iter().enumerate() {
        assert_eq!(unsafe { *(*survivor as *mut i64) }, index as i64);
    }
    assert_global_regions_valid();

    willow_pop_roots(CHUNKS as i32);
    willow_gc_collect();
    assert_eq!(willow_gc_pinned_region_count(), 0);
    assert_eq!(willow_gc_old_region_reserved_bytes(), 0);
    reset_gc();
}

#[test]
#[ignore = "explicit GC stress suite"]
fn stress_region_07_deterministic_random_graph_matches_reachability_model() {
    let _guard = stress_guard();
    reset_gc();
    const OBJECTS: usize = 18_000;
    let mut objects = Vec::with_capacity(OBJECTS);
    for index in 0..OBJECTS {
        objects.push(willow_gc_alloc_layout(
            index as u64 + 1,
            index as i64 + 1,
            8,
            0b1,
        ));
    }
    for index in 0..OBJECTS {
        let target = (index.wrapping_mul(1103515245).wrapping_add(12345)) % OBJECTS;
        unsafe { *(objects[index] as *mut *mut u8) = objects[target] };
    }
    let mut roots: Vec<*mut u8> = objects.iter().step_by(997).copied().collect();
    for root in &mut roots {
        willow_push_root(root);
    }

    let mut expected = HashSet::new();
    let mut worklist: Vec<usize> = (0..OBJECTS).step_by(997).collect();
    while let Some(index) = worklist.pop() {
        if expected.insert(index as u32 + 1) {
            let target = (index.wrapping_mul(1103515245).wrapping_add(12345)) % OBJECTS;
            worklist.push(target);
        }
    }

    willow_gc_collect();

    let state = runtime().heap.lock().unwrap();
    let mut actual = HashSet::new();
    let mut current = HeapObject::from_raw(state.heap_head);
    while let Some(object) = current {
        actual.insert(object.type_id());
        current = object.next();
    }
    assert_eq!(actual, expected);
    assert_eq!(state.allocated_bytes, expected.len() * SMALL_OBJECT_SIZE);
    assert_eq!(verify_old_region_metadata(&state), Ok(()));
    drop(state);

    willow_pop_roots(roots.len() as i32);
    willow_gc_collect();
    assert_eq!(willow_gc_old_region_count(), 0);
    reset_gc();
}

#[test]
#[ignore = "explicit GC stress suite"]
fn stress_region_08_five_mutators_allocate_and_collect_concurrently() {
    let _guard = stress_guard();
    reset_gc();
    let handles: Vec<_> = (0..5)
        .map(|worker| {
            std::thread::spawn(move || {
                willow_gc_register_mutator();
                for iteration in 0..500 {
                    let object = willow_alloc_object(worker + 1, 8);
                    unsafe { *(object as *mut i64) = iteration };
                    if iteration % 7 == 0 {
                        willow_gc_collect();
                    } else {
                        willow_gc_safepoint();
                    }
                }
                willow_gc_unregister_mutator();
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    assert_global_regions_valid();
    reset_gc();
}

#[test]
#[ignore = "explicit GC stress suite"]
fn stress_region_09_sparse_pinned_waves_quantify_retained_capacity() {
    let _guard = stress_guard();
    reset_gc();
    const WAVES: usize = 8;
    const CHUNKS_PER_WAVE: usize = 64;
    const SURVIVOR_PAYLOAD_SIZE: usize = 24;
    const SURVIVOR_SIZE: usize = GC_HEADER_SIZE + SURVIVOR_PAYLOAD_SIZE;
    const OBJECTS_PER_CHUNK: usize = GC_TLAB_CHUNK_SIZE / SURVIVOR_SIZE;
    const TOTAL_CHUNKS: usize = WAVES * CHUNKS_PER_WAVE;
    const EXPECTED_RESERVED: usize = TOTAL_CHUNKS * GC_TLAB_CHUNK_SIZE;
    const EXPECTED_LIVE: usize = TOTAL_CHUNKS * SURVIVOR_SIZE;

    assert_eq!(SURVIVOR_SIZE, 64);
    assert_eq!(GC_TLAB_CHUNK_SIZE % SURVIVOR_SIZE, 0);

    let mut states = Vec::<Box<GcTlabState>>::with_capacity(TOTAL_CHUNKS);
    let mut survivors = Vec::<*mut u8>::with_capacity(TOTAL_CHUNKS);
    for wave in 1..=WAVES {
        for chunk_index in 0..CHUNKS_PER_WAVE {
            let mut tls = Box::new(new_tlab_state());
            let survivor = willow_gc_alloc_slow(&mut *tls, 1, chunk_index as i64 + 1, 24, 0);
            unsafe { *(survivor as *mut i64) = (wave * CHUNKS_PER_WAVE + chunk_index) as i64 };
            survivors.push(survivor);
            willow_push_root(
                survivors
                    .last_mut()
                    .expect("the newly pushed survivor has a stable root slot"),
            );

            for object_index in 1..OBJECTS_PER_CHUNK {
                let dead = tlab_fast_alloc(&tls, 2, object_index as u32, 24, 0);
                unsafe { *(dead as *mut i64) = object_index as i64 };
            }
            states.push(tls);
        }

        willow_gc_minor_collect();
        willow_gc_collect();

        let expected_chunks = wave * CHUNKS_PER_WAVE;
        let expected_reserved = expected_chunks * GC_TLAB_CHUNK_SIZE;
        let expected_live = expected_chunks * SURVIVOR_SIZE;
        assert_eq!(willow_gc_pinned_region_count(), expected_chunks as i64);
        assert_eq!(
            willow_gc_old_region_reserved_bytes(),
            expected_reserved as i64,
            "each wave must reserve fresh chunks while earlier sparse chunks stay pinned"
        );
        assert_eq!(willow_gc_old_region_live_bytes(), expected_live as i64);
        assert_eq!(
            willow_gc_old_region_fragmentation_bytes(),
            (expected_reserved - expected_live) as i64
        );
        assert_eq!(expected_reserved / expected_live, 512);
        assert_global_regions_valid();
    }

    eprintln!(
        "sparse pinned retention: chunks={TOTAL_CHUNKS}, reserved={EXPECTED_RESERVED}, \
         live={EXPECTED_LIVE}, amplification={}x",
        EXPECTED_RESERVED / EXPECTED_LIVE
    );
    assert_eq!(willow_gc_old_region_reserved_bytes(), (16 * 1024 * 1024) as i64);
    assert_eq!(willow_gc_old_region_live_bytes(), (32 * 1024) as i64);

    for remaining_waves in (0..WAVES).rev() {
        willow_pop_roots(CHUNKS_PER_WAVE as i32);
        willow_gc_collect();
        let remaining_chunks = remaining_waves * CHUNKS_PER_WAVE;
        let remaining_reserved = remaining_chunks * GC_TLAB_CHUNK_SIZE;
        let remaining_live = remaining_chunks * SURVIVOR_SIZE;
        assert_eq!(willow_gc_pinned_region_count(), remaining_chunks as i64);
        assert_eq!(
            willow_gc_old_region_reserved_bytes(),
            remaining_reserved as i64
        );
        assert_eq!(
            willow_gc_old_region_live_bytes(),
            remaining_live as i64
        );
        assert_eq!(
            willow_gc_old_region_fragmentation_bytes(),
            (remaining_reserved - remaining_live) as i64
        );
    }
    reset_gc();
}

#[test]
#[ignore = "explicit GC stress suite"]
fn stress_region_10_bounded_runtime_root_lifetimes_bound_pinned_retention() {
    let _guard = stress_guard();
    reset_gc();
    const WAVES: usize = 32;
    const LIVE_WINDOW: usize = 4;
    const CHUNKS_PER_WAVE: usize = 16;
    const OBJECTS_PER_CHUNK: usize = 64;
    const SURVIVOR_PAYLOAD_SIZE: usize = 24;
    const SURVIVOR_SIZE: usize = GC_HEADER_SIZE + SURVIVOR_PAYLOAD_SIZE;
    const WAVE_RESERVED: usize = CHUNKS_PER_WAVE * GC_TLAB_CHUNK_SIZE;
    const WAVE_LIVE: usize = CHUNKS_PER_WAVE * SURVIVOR_SIZE;
    const WAVE_FRAGMENTATION: usize =
        CHUNKS_PER_WAVE * (OBJECTS_PER_CHUNK - 1) * SURVIVOR_SIZE;

    assert_eq!(SURVIVOR_SIZE, 64);
    let mut states = Vec::<Box<GcTlabState>>::with_capacity(WAVES * CHUNKS_PER_WAVE);
    let mut live_batches = std::collections::VecDeque::<Vec<*mut u8>>::new();

    eprintln!("phase,wave,pinned_regions,reserved_bytes,live_bytes,fragmentation_bytes,ratio");
    for wave in 1..=WAVES {
        let mut batch = Vec::with_capacity(CHUNKS_PER_WAVE);
        for chunk_index in 0..CHUNKS_PER_WAVE {
            let mut tls = Box::new(new_tlab_state());
            let survivor =
                willow_gc_alloc_slow(&mut *tls, 1, chunk_index as i64 + 1, 24, 0);
            unsafe { *(survivor as *mut i64) = (wave * CHUNKS_PER_WAVE + chunk_index) as i64 };
            willow_gc_add_runtime_root(survivor);
            batch.push(survivor);
            for object_index in 1..OBJECTS_PER_CHUNK {
                let dead = tlab_fast_alloc(&tls, 2, object_index as u32, 24, 0);
                unsafe { *(dead as *mut i64) = object_index as i64 };
            }
            states.push(tls);
        }
        live_batches.push_back(batch);

        willow_gc_minor_collect();
        willow_gc_collect();
        assert_eq!(
            willow_gc_old_region_reserved_bytes(),
            (live_batches.len() * WAVE_RESERVED) as i64
        );

        if live_batches.len() > LIVE_WINDOW {
            let expired = live_batches
                .pop_front()
                .expect("the oldest runtime-root batch exists");
            for survivor in expired {
                willow_gc_remove_runtime_root(survivor);
            }
            willow_gc_collect();
        }

        let live_waves = wave.min(LIVE_WINDOW);
        let expected_regions = live_waves * CHUNKS_PER_WAVE;
        let expected_reserved = live_waves * WAVE_RESERVED;
        let expected_live = live_waves * WAVE_LIVE;
        let expected_fragmentation = live_waves * WAVE_FRAGMENTATION;
        assert_eq!(willow_gc_pinned_region_count(), expected_regions as i64);
        assert_eq!(
            willow_gc_old_region_reserved_bytes(),
            expected_reserved as i64
        );
        assert_eq!(willow_gc_old_region_live_bytes(), expected_live as i64);
        assert_eq!(
            willow_gc_old_region_fragmentation_bytes(),
            expected_fragmentation as i64
        );
        assert_eq!(expected_reserved / expected_live, 512);
        assert_global_regions_valid();
        eprintln!(
            "steady,{wave},{expected_regions},{expected_reserved},{expected_live},\
             {expected_fragmentation},{}",
            expected_reserved / expected_live
        );
    }

    while let Some(expired) = live_batches.pop_front() {
        for survivor in expired {
            willow_gc_remove_runtime_root(survivor);
        }
        willow_gc_collect();
        assert_eq!(
            willow_gc_pinned_region_count(),
            (live_batches.len() * CHUNKS_PER_WAVE) as i64
        );
        assert_eq!(
            willow_gc_old_region_reserved_bytes(),
            (live_batches.len() * WAVE_RESERVED) as i64
        );
    }
    assert_eq!(willow_gc_old_region_live_bytes(), 0);
    assert_eq!(willow_gc_old_region_fragmentation_bytes(), 0);
    reset_gc();
}
