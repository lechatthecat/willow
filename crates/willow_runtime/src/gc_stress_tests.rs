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
        start_bits: AtomicUsize::new(0),
    }
}

// Allocation stress runs a full collection inside every allocation. Fixtures
// that count allocator reuse, or root tens of thousands of objects while
// allocating, measure something else under it (or become quadratic).
fn skip_under_alloc_stress(test_name: &str, reason: &str) -> bool {
    if gc_stress_enabled("alloc") {
        eprintln!("SKIP {test_name}: WILLOW_GC_STRESS=alloc (including all) {reason}");
        return true;
    }
    false
}

// These nursery-chunk fixtures need real TLABs: allocation stress routes
// the slow allocation directly to old regions and leaves no TLAB to fill.
// Keep stress enabled for the process; skip only these incompatible fixtures.
fn skip_nursery_tlab_fixture(test_name: &str) -> bool {
    if gc_stress_enabled("alloc") {
        eprintln!(
            "SKIP {test_name}: WILLOW_GC_STRESS=alloc (including all) bypasses TLABs; \
             rerun without alloc/all to exercise nursery-chunk assertions"
        );
        return true;
    }
    false
}

// Fixtures that fill a TLAB chunk by hand also need the chunk's full limit:
// relocation stress caps every TLAB at one object.
fn skip_filled_tlab_fixture(test_name: &str) -> bool {
    if skip_nursery_tlab_fixture(test_name) {
        return true;
    }
    if gc_stress_enabled("relocate") {
        eprintln!(
            "SKIP {test_name}: WILLOW_GC_STRESS=relocate limits TLABs to one object; \
             rerun without relocate/all to exercise filled-chunk assertions"
        );
        return true;
    }
    false
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
    publish_tlab_start_for_test(tls, cursor as *mut u8);
    tls.cursor.store(cursor + total_size, Ordering::Release);
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
    if skip_under_alloc_stress(
        "stress_region_01",
        "reclaims each garbage object before the next allocation, so the reuse count differs",
    ) {
        return;
    }
    let mut left = willow_alloc_object(1, 8);
    willow_push_root(&mut left);
    for _ in 0..1000 {
        let _garbage = willow_alloc_object(2, 8);
    }
    let mut right = willow_alloc_object(3, 8);
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
    if skip_under_alloc_stress(
        "stress_region_03",
        "marks up to 50,000 rooted objects at each of 50,000 allocations",
    ) {
        return;
    }
    const OBJECTS: usize = 50_000;
    const ROOT_STRIDE: usize = 5_000;
    let mut objects = Vec::with_capacity(OBJECTS);
    for value in 0..OBJECTS {
        let object = willow_alloc_object(value as i64 + 1, 8);
        unsafe { *(object as *mut i64) = value as i64 };
        objects.push(object);
        // Capacity is fixed before any root slot is registered.
        willow_push_root(objects.last_mut().unwrap());
    }
    let mut roots: Vec<*mut u8> = objects.iter().step_by(ROOT_STRIDE).copied().collect();
    let original = roots.clone();
    willow_pop_roots(OBJECTS as i32);
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
        let mut large = willow_alloc_object(3, GC_LARGE_OBJECT_THRESHOLD as i64);
        // The parent allocation can now trigger paced collection. Native test
        // locals need the same explicit rooting as generated live references.
        willow_push_root(&mut large);
        let mut parent = willow_alloc_typed(8, 0b1);
        unsafe { *(parent as *mut *mut u8) = large };
        willow_push_root(&mut parent);

        willow_gc_collect();

        assert_eq!(willow_gc_large_object_region_count(), 1);
        assert_eq!(unsafe { *(parent as *mut *mut u8) }, large);
        assert_global_regions_valid();

        willow_pop_roots(2);
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
    if skip_nursery_tlab_fixture("stress_region_05") {
        return;
    }
    let mut parent = willow_alloc_typed(8, 0b1);
    willow_push_root(&mut parent);
    let mut tls = new_tlab_state();

    for round in 0..1000i64 {
        let young = willow_gc_alloc_slow(&mut tls, 2, 2, 8, 0);
        unsafe { *(young as *mut i64) = round };
        willow_gc_write_barrier(
            parent,
            std::ptr::null_mut(),
            young,
            GcStoreDestination::ObjectField as i64,
        );
        unsafe { *(parent as *mut *mut u8) = young };
        assert_eq!(willow_gc_remembered_set_size(), 1);

        crate::gc::minor_collect_internal();

        let survivor = unsafe { *(parent as *mut *mut u8) };
        assert_ne!(survivor, young);
        assert_eq!(unsafe { *(survivor as *mut i64) }, round);
        assert_eq!(
            unsafe { (*payload_to_header(survivor)).generation },
            GC_GENERATION_YOUNG
        );
        assert_eq!(willow_gc_remembered_set_size(), 1);

        // No intervening store: the remembered owner must retain the survivor
        // until the second minor collection tenures it.
        crate::gc::minor_collect_internal();
        let promoted = unsafe { *(parent as *mut *mut u8) };
        assert_ne!(promoted, survivor);
        assert_eq!(unsafe { *(promoted as *mut i64) }, round);
        assert_eq!(
            unsafe { (*payload_to_header(promoted)).generation },
            GC_GENERATION_OLD
        );
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

/// Reserved nursery plus old-region bytes. Without pinning, sparse survivor
/// chunks are evacuated, so this stays near the live size instead of growing
/// with the number of source chunks (willow-9tls.9).
fn reserved_managed_bytes() -> usize {
    (willow_gc_old_region_reserved_bytes() + willow_gc_tlab_reserved_bytes()) as usize
}

/// Upper bound for compact survivor storage: one partly filled old region and
/// one partly filled survivor chunk beyond the live bytes themselves.
fn compact_reserved_bound(live: usize) -> usize {
    live.next_multiple_of(GC_TLAB_CHUNK_SIZE) + GC_OLD_REGION_SIZE + GC_TLAB_CHUNK_SIZE
}

fn assert_no_pinned_retention(context: &str) {
    assert_eq!(willow_gc_pinned_region_count(), 0, "{context}");
    assert_eq!(willow_gc_pinned_promotions(), 0, "{context}");
}

#[test]
#[ignore = "explicit GC stress suite"]
fn stress_region_06_many_sparse_survivor_chunks_relocate_and_release() {
    let _guard = stress_guard();
    reset_gc();
    if skip_filled_tlab_fixture("stress_region_06") {
        return;
    }
    const CHUNKS: usize = 64;
    const OBJECTS_PER_CHUNK: usize = 64;
    let mut states: Vec<Box<GcTlabState>> =
        (0..CHUNKS).map(|_| Box::new(new_tlab_state())).collect();
    let mut survivors = Vec::with_capacity(CHUNKS);
    let mut originals = Vec::with_capacity(CHUNKS);

    for (index, tls) in states.iter_mut().enumerate() {
        let survivor = willow_gc_alloc_slow(&mut **tls, 1, index as i64 + 1, 8, 0);
        unsafe { *(survivor as *mut i64) = index as i64 };
        survivors.push(survivor);
        // Relocatable slots: a collection inside the slow path may already
        // move earlier survivors, and every later read reloads the slot.
        willow_push_root(survivors.last_mut().unwrap());
        originals.push(survivor);
        for object_index in 1..OBJECTS_PER_CHUNK {
            let dead = tlab_fast_alloc(tls, 2, object_index as u32, 8, 0);
            unsafe { *(dead as *mut i64) = object_index as i64 };
        }
    }
    // The generated-code entry point runs in a relocation-safe scope.
    willow_gc_minor_collect();
    willow_gc_collect();

    assert_no_pinned_retention("after evacuating sparse chunks");
    let live = CHUNKS * SMALL_OBJECT_SIZE;
    let reserved = reserved_managed_bytes();
    eprintln!(
        "sparse survivors relocated: chunks={CHUNKS}, live={live}, reserved={reserved}, \
         pinned_baseline={}",
        CHUNKS * GC_TLAB_CHUNK_SIZE
    );
    assert!(
        reserved <= compact_reserved_bound(live),
        "reserved {reserved} bytes for {live} live bytes"
    );
    for (index, (survivor, original)) in survivors.iter().zip(&originals).enumerate() {
        assert_ne!(survivor, original, "survivor {index} was evacuated");
        assert_eq!(unsafe { *(*survivor as *mut i64) }, index as i64);
    }
    assert_global_regions_valid();

    willow_pop_roots(CHUNKS as i32);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    assert_eq!(willow_gc_old_region_reserved_bytes(), 0);
    reset_gc();
}

#[test]
#[ignore = "explicit GC stress suite"]
fn stress_region_07_deterministic_random_graph_matches_reachability_model() {
    let _guard = stress_guard();
    reset_gc();
    // Allocation stress collects before every allocation: retaining N roots
    // necessarily visits 1 + ... + N objects. Bound that mode's fixture size.
    let objects_count = if gc_stress_enabled("alloc") {
        512
    } else {
        18_000
    };
    let mut objects = Vec::with_capacity(objects_count);
    for index in 0..objects_count {
        objects.push(willow_gc_alloc_layout(
            index as u64 + 1,
            index as i64 + 1,
            8,
            0b1,
        ));
        willow_push_root(objects.last_mut().unwrap());
    }
    for index in 0..objects_count {
        let target = (index.wrapping_mul(1103515245).wrapping_add(12345)) % objects_count;
        unsafe { *(objects[index] as *mut *mut u8) = objects[target] };
    }
    let mut roots: Vec<*mut u8> = objects.iter().step_by(997).copied().collect();
    willow_pop_roots(objects_count as i32);
    for root in &mut roots {
        willow_push_root(root);
    }

    let mut expected = HashSet::new();
    let mut worklist: Vec<usize> = (0..objects_count).step_by(997).collect();
    while let Some(index) = worklist.pop() {
        if expected.insert(index as u32 + 1) {
            let target = (index.wrapping_mul(1103515245).wrapping_add(12345)) % objects_count;
            worklist.push(target);
        }
    }

    willow_gc_collect();

    let state = runtime().heap.lock().unwrap();
    let mut actual = HashSet::new();
    for object in old_region_objects(&state) {
        actual.insert(object.type_id());
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
fn stress_region_11_generated_graph_preserves_edges_through_moving_collection() {
    let _guard = stress_guard();
    // Allocation stress deliberately bypasses the nursery. The runner also
    // runs this test without that mode so relocation cannot pass vacuously.
    if skip_filled_tlab_fixture("stress_region_11") {
        return;
    }
    for count in [8usize, 64, 512] {
        for initial_seed in [1u64, 0x5eed, 0xdead_beef] {
            reset_gc();
            let live = count * 3 / 4;
            let mut seed = initial_seed;
            let mut edges = Vec::with_capacity(count);
            for index in 0..count {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                // A ring guarantees live-node coverage; the other edge adds
                // aliases, self-edges, and fan-in. The last quarter is garbage.
                edges.push(if index < live {
                    [(index + 1) % live, (seed >> 32) as usize % live]
                } else {
                    [index, index]
                });
            }
            let mut parent = willow_gc_alloc_layout(1, 0, 8, 1);
            willow_push_root(&mut parent);
            let mut tls = new_tlab_state();
            let mut objects = Vec::with_capacity(count);
            for index in 0..count {
                let object = if index == 0 {
                    willow_gc_alloc_slow(&mut tls, 2, 0, 24, 0b11)
                } else {
                    tlab_fast_alloc(&tls, 2, 0, 24, 0b11)
                };
                assert!(!object.is_null());
                unsafe { *object.add(16).cast::<u64>() = index as u64 };
                objects.push(object);
            }
            for (index, targets) in edges.iter().enumerate() {
                for (slot, &target) in targets.iter().enumerate() {
                    unsafe { *objects[index].cast::<*mut u8>().add(slot) = objects[target] };
                }
            }
            willow_gc_write_barrier(
                parent,
                std::ptr::null_mut(),
                objects[0],
                GcStoreDestination::ObjectField as i64,
            );
            unsafe { *parent.cast::<*mut u8>() = objects[0] };

            let mut relocated = Vec::with_capacity(live);
            for round in 0..3 {
                crate::gc::minor_collect_internal();
                let mut current = unsafe { *parent.cast::<*mut u8>() };
                relocated.clear();
                for (index, &original) in objects.iter().take(live).enumerate() {
                    assert_eq!(unsafe { *current.add(16).cast::<u64>() }, index as u64);
                    if round == 0 {
                        assert_ne!(current, original, "seed={initial_seed} count={count}");
                    }
                    relocated.push(current);
                    current = unsafe { *current.cast::<*mut u8>() };
                }
                assert_eq!(current, unsafe { *parent.cast::<*mut u8>() });
                for (index, targets) in edges.iter().take(live).enumerate() {
                    for (slot, &target) in targets.iter().enumerate() {
                        let child = unsafe { *relocated[index].cast::<*mut u8>().add(slot) };
                        assert_eq!(child, relocated[target]);
                    }
                }
                assert_eq!(
                    willow_gc_allocated_bytes(),
                    (live * (GC_HEADER_SIZE + 24) + GC_HEADER_SIZE + 8) as i64,
                    "disconnected nursery garbage must be reclaimed"
                );
            }
            assert!(willow_gc_moved_objects() >= live as i64);
            willow_pop_root();
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), 0);
            assert_global_regions_valid();
            eprintln!(
                "moving graph: seed={initial_seed} nodes={count} live={live} checked_edges={}",
                3 * 2 * live
            );
            reset_gc();
        }
    }
}

#[test]
#[ignore = "explicit GC stress suite"]
fn stress_region_08_five_mutators_allocate_and_collect_concurrently() {
    let _guard = stress_guard();
    reset_gc();
    let handles: Vec<_> = (0..5)
        .map(|worker| {
            std::thread::spawn(move || {
                let mutator = crate::gc::MutatorRegistration::new();
                for iteration in 0..500 {
                    let object = willow_alloc_object(worker + 1, 8);
                    unsafe { *(object as *mut i64) = iteration };
                    if iteration % 7 == 0 {
                        willow_gc_collect();
                    } else {
                        willow_gc_safepoint();
                    }
                }
                drop(mutator);
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
fn stress_region_09_sparse_survivor_waves_do_not_retain_source_capacity() {
    let _guard = stress_guard();
    if skip_filled_tlab_fixture("stress_region_09") {
        return;
    }
    reset_gc();
    const WAVES: usize = 8;
    const CHUNKS_PER_WAVE: usize = 64;
    // Keep the fixture at 64 bytes regardless of the runtime header size.
    const SURVIVOR_SIZE: usize = 64;
    const SURVIVOR_PAYLOAD_SIZE: usize = SURVIVOR_SIZE - GC_HEADER_SIZE;
    const OBJECTS_PER_CHUNK: usize = GC_TLAB_CHUNK_SIZE / SURVIVOR_SIZE;
    const TOTAL_CHUNKS: usize = WAVES * CHUNKS_PER_WAVE;

    assert_eq!(SURVIVOR_SIZE, 64);
    assert_eq!(GC_TLAB_CHUNK_SIZE % SURVIVOR_SIZE, 0);

    let mut states = Vec::<Box<GcTlabState>>::with_capacity(TOTAL_CHUNKS);
    let mut survivors = Vec::<*mut u8>::with_capacity(TOTAL_CHUNKS);
    let mut max_reserved = 0;
    for wave in 1..=WAVES {
        for chunk_index in 0..CHUNKS_PER_WAVE {
            let mut tls = Box::new(new_tlab_state());
            let survivor = willow_gc_alloc_slow(
                &mut *tls,
                1,
                chunk_index as i64 + 1,
                SURVIVOR_PAYLOAD_SIZE as i64,
                0,
            );
            unsafe { *(survivor as *mut i64) = (wave * CHUNKS_PER_WAVE + chunk_index) as i64 };
            survivors.push(survivor);
            willow_push_root(
                survivors
                    .last_mut()
                    .expect("the newly pushed survivor has a stable root slot"),
            );

            for object_index in 1..OBJECTS_PER_CHUNK {
                let dead = tlab_fast_alloc(&tls, 2, object_index as u32, SURVIVOR_PAYLOAD_SIZE, 0);
                unsafe { *(dead as *mut i64) = object_index as i64 };
            }
            states.push(tls);
        }

        willow_gc_minor_collect();
        willow_gc_collect();

        assert_no_pinned_retention(&format!("wave {wave}"));
        let live = wave * CHUNKS_PER_WAVE * SURVIVOR_SIZE;
        let reserved = reserved_managed_bytes();
        max_reserved = max_reserved.max(reserved);
        assert!(
            reserved <= compact_reserved_bound(live),
            "wave {wave}: reserved {reserved} bytes for {live} live bytes"
        );
        for (index, survivor) in survivors.iter().enumerate() {
            let expected = CHUNKS_PER_WAVE + index;
            assert_eq!(unsafe { *(*survivor as *mut i64) }, expected as i64);
        }
        assert_global_regions_valid();
    }

    let live = TOTAL_CHUNKS * SURVIVOR_SIZE;
    eprintln!(
        "sparse survivor waves: chunks={TOTAL_CHUNKS}, live={live}, max_reserved={max_reserved}, \
         pinned_baseline={}",
        TOTAL_CHUNKS * GC_TLAB_CHUNK_SIZE
    );

    for remaining_waves in (0..WAVES).rev() {
        willow_pop_roots(CHUNKS_PER_WAVE as i32);
        willow_gc_collect();
        assert_no_pinned_retention(&format!("{remaining_waves} waves remain"));
        let remaining_live = remaining_waves * CHUNKS_PER_WAVE * SURVIVOR_SIZE;
        assert!(reserved_managed_bytes() <= compact_reserved_bound(remaining_live));
    }
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

#[test]
#[ignore = "explicit GC stress suite"]
fn stress_region_10_runtime_root_slots_relocate_bounded_lifetimes() {
    let _guard = stress_guard();
    if skip_filled_tlab_fixture("stress_region_10") {
        return;
    }
    reset_gc();
    const WAVES: usize = 32;
    const LIVE_WINDOW: usize = 4;
    const CHUNKS_PER_WAVE: usize = 16;
    const OBJECTS_PER_CHUNK: usize = 64;
    // Keep the fixture at 64 bytes regardless of the runtime header size.
    const SURVIVOR_SIZE: usize = 64;
    const SURVIVOR_PAYLOAD_SIZE: usize = SURVIVOR_SIZE - GC_HEADER_SIZE;

    assert_eq!(SURVIVOR_SIZE, 64);
    let mut states = Vec::<Box<GcTlabState>>::with_capacity(WAVES * CHUNKS_PER_WAVE);
    // Each batch's buffer is allocated once at full capacity, so its slots stay
    // put while the batch moves through the queue.
    let mut live_batches = std::collections::VecDeque::<Vec<*mut u8>>::new();

    eprintln!("phase,wave,live_slots,reserved_bytes,live_bytes,moved_objects");
    for wave in 1..=WAVES {
        let mut batch = Vec::with_capacity(CHUNKS_PER_WAVE);
        for chunk_index in 0..CHUNKS_PER_WAVE {
            let mut tls = Box::new(new_tlab_state());
            let survivor = willow_gc_alloc_slow(
                &mut *tls,
                1,
                chunk_index as i64 + 1,
                SURVIVOR_PAYLOAD_SIZE as i64,
                0,
            );
            unsafe { *(survivor as *mut i64) = (wave * CHUNKS_PER_WAVE + chunk_index) as i64 };
            batch.push(survivor);
            willow_gc_add_runtime_root_slot(batch.last_mut().unwrap());
            for object_index in 1..OBJECTS_PER_CHUNK {
                let dead = tlab_fast_alloc(&tls, 2, object_index as u32, SURVIVOR_PAYLOAD_SIZE, 0);
                unsafe { *(dead as *mut i64) = object_index as i64 };
            }
            states.push(tls);
        }
        live_batches.push_back(batch);

        willow_gc_minor_collect();
        willow_gc_collect();

        if live_batches.len() > LIVE_WINDOW {
            let mut expired = live_batches
                .pop_front()
                .expect("the oldest runtime-root batch exists");
            for slot in &mut expired {
                willow_gc_remove_runtime_root_slot(slot);
            }
            willow_gc_collect();
        }

        assert_no_pinned_retention(&format!("wave {wave}"));
        for (offset, batch) in live_batches.iter().enumerate() {
            let batch_wave = wave + 1 + offset - live_batches.len();
            for (chunk_index, survivor) in batch.iter().enumerate() {
                let expected = batch_wave * CHUNKS_PER_WAVE + chunk_index;
                assert_eq!(unsafe { *(*survivor as *mut i64) }, expected as i64);
            }
        }
        let live = live_batches.len() * CHUNKS_PER_WAVE * SURVIVOR_SIZE;
        let reserved = reserved_managed_bytes();
        assert!(
            reserved <= compact_reserved_bound(live),
            "wave {wave}: reserved {reserved} bytes for {live} live bytes"
        );
        assert_global_regions_valid();
        eprintln!(
            "steady,{wave},{},{reserved},{live},{}",
            live_batches.len() * CHUNKS_PER_WAVE,
            willow_gc_moved_objects()
        );
    }
    assert!(willow_gc_moved_objects() > 0, "runtime root slots relocate");

    while let Some(mut expired) = live_batches.pop_front() {
        for slot in &mut expired {
            willow_gc_remove_runtime_root_slot(slot);
        }
        willow_gc_collect();
        assert_no_pinned_retention("draining runtime root slots");
    }
    assert_eq!(crate::gc::runtime_root_count(), 0);
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}
