use super::*;

/// Expected O(1) per operation: key equality checks per lookup, measured
/// on the exact table types and hashers the maps use (`WordHasher` for
/// scalar keys, the default hashbrown builder for string keys). Hits must
/// average close to one comparison and misses close to zero at every size
/// and key shape, including strided and negative words that defeat weak
/// mixers; the averages must not grow with n.
#[test]
fn hash_lookups_compare_a_constant_number_of_keys() {
    use std::cell::Cell;
    thread_local! { static COMPARISONS: Cell<usize> = const { Cell::new(0) }; }
    #[derive(Clone)]
    struct Counted<K>(K);
    impl<K: PartialEq> PartialEq for Counted<K> {
        fn eq(&self, other: &Self) -> bool {
            COMPARISONS.with(|count| count.set(count.get() + 1));
            self.0 == other.0
        }
    }
    impl<K: Eq> Eq for Counted<K> {}
    impl<K: Hash> Hash for Counted<K> {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.0.hash(state);
        }
    }
    fn measure<K: Hash + Eq + Clone, S: std::hash::BuildHasher + Default>(
        keys: &[K],
        absent: &[K],
    ) -> (f64, f64) {
        let mut table: hashbrown::HashMap<Counted<K>, usize, S> = hashbrown::HashMap::default();
        for (index, key) in keys.iter().enumerate() {
            table.insert(Counted(key.clone()), index);
        }
        COMPARISONS.with(|count| count.set(0));
        for (index, key) in keys.iter().enumerate() {
            assert_eq!(table.get(&Counted(key.clone())), Some(&index));
        }
        let hits = COMPARISONS.with(Cell::get) as f64 / keys.len() as f64;
        COMPARISONS.with(|count| count.set(0));
        for key in absent {
            assert_eq!(table.get(&Counted(key.clone())), None);
        }
        let misses = COMPARISONS.with(Cell::get) as f64 / absent.len() as f64;
        (hits, misses)
    }
    for count in [1_i64 << 10, 1 << 13, 1 << 16, 1 << 18] {
        for stride in [1_i64, 1 << 32, -1, 1024, 2654435761] {
            let keys: Vec<i64> = (0..count).map(|i| i.wrapping_mul(stride)).collect();
            let absent: Vec<i64> = (count..2 * count).map(|i| i.wrapping_mul(stride)).collect();
            let (hits, misses) = measure::<_, BuildHasherDefault<WordHasher>>(&keys, &absent);
            eprintln!("word count={count} stride={stride} hit_cmp={hits:.4} miss_cmp={misses:.4}");
            assert!(
                hits < 1.25,
                "word count={count} stride={stride} hits={hits}"
            );
            assert!(
                misses < 0.25,
                "word count={count} stride={stride} misses={misses}"
            );
        }
        let keys: Vec<String> = (0..count).map(|i| format!("key-{i}")).collect();
        let absent: Vec<String> = (0..count).map(|i| format!("other-{i}")).collect();
        let (hits, misses) = measure::<_, hashbrown::DefaultHashBuilder>(&keys, &absent);
        eprintln!("string count={count} hit_cmp={hits:.4} miss_cmp={misses:.4}");
        assert!(hits < 1.25, "string count={count} hits={hits}");
        assert!(misses < 0.25, "string count={count} misses={misses}");
    }
}

#[test]
fn confined_string_maps_scale_without_lookup_allocation_and_skip_gc_borrows() {
    use crate::scheduler::scaling_measurements::counting_allocator as counter;
    let _guard = crate::gc::isolated_runtime();
    for count in [16, 128, 1024, 8192] {
        for length in [20, 128] {
            // SAFETY: The test controls its isolated runtime at this reset boundary; no
            // other mutator or subsequent access relies on the discarded heap.
            unsafe { willow_gc_init() };
            let baseline = crate::gc_telemetry::external_bytes();
            // SAFETY: Only the creating test task accesses this local map while it is live.
            let mut map = unsafe { willow_map_new_local(3, 0, 0) };
            // SAFETY: The initialized slot stays at this address until its registration
            // is popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut map) };
            // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by
            // the constructor.
            assert!(matches!(unsafe { &storage(map).state }, MapState::Local(_)));
            let mut keys = vec![std::ptr::null_mut(); count];
            STRING_HASH_BYTES.set(0);
            for (i, key) in keys.iter_mut().enumerate() {
                *key = willow_string_from_str(&format!("{i:0length$}"));
                // SAFETY: The slot storage stays live and stationary through the matching
                // pop, including during collection.
                unsafe { willow_push_root(key) };
                // SAFETY: The constructor and reference flags agree. Each pointer word
                // comes from a live fixture object at the call.
                unsafe { willow_map_insert(map, *key as i64, 1, i as i64, 0) };
            }
            let insertion_bytes = STRING_HASH_BYTES.get();
            assert!(insertion_bytes <= 6 * count * length);
            let allocated = counter::thread_allocations();
            let gc_bytes = crate::gc::willow_gc_allocated_bytes();
            let retained = crate::gc_telemetry::external_bytes();
            STRING_HASH_BYTES.set(0);
            for (i, &key) in keys.iter().enumerate() {
                let mut payload = -1;
                assert_eq!(
                    // SAFETY: The fixture supplies the live map and a key of its declared
                    // kind; the output borrows a disjoint local i64.
                    unsafe { willow_map_get_into(map, key as i64, 1, &mut payload) },
                    0
                );
                assert_eq!(payload, i as i64);
                // SAFETY: The constructor and reference flags agree. Each pointer word
                // comes from a live fixture object at the call.
                unsafe { willow_map_insert(map, key as i64, 1, !payload, 0) };
            }
            assert_eq!(STRING_HASH_BYTES.get(), 2 * count * length);
            assert_eq!(counter::thread_allocations(), allocated);
            assert_eq!(crate::gc::willow_gc_allocated_bytes(), gc_bytes);
            assert_eq!(crate::gc_telemetry::external_bytes(), retained);
            // A concurrent tracer must not borrow confined scalar-value state.
            // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by
            // the constructor.
            let borrowed = unsafe { map_data(map) };
            let mut slots = Vec::new();
            let mut children = Vec::new();
            // SAFETY: The callback receives the live map payload selected by its registered
            // layout.
            unsafe {
                trace_map(map, &mut slots);
                snapshot_map(map, &mut children);
            }
            assert!(matches!(
                // SAFETY: The callback receives the live map payload selected by its
                // registered layout.
                unsafe { snapshot_map_slice(map, 0, 1, &mut children) },
                crate::gc::TraceSliceProgress::Done
            ));
            assert!(slots.is_empty() && children.is_empty());
            drop(borrowed);
            eprintln!(
                "strings count={count} length={length} insertion_bytes={insertion_bytes} lookup_update_bytes={} allocations=0",
                2 * count * length
            );
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(count as i32) };
            willow_gc_collect();
            // SAFETY: The source is a live map from the constructor and remains reachable
            // while copied.
            let mut copy = unsafe { willow_map_copy(map) };
            // SAFETY: The initialized slot stays at this address until its registration
            // is popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut copy) };
            let key = willow_string_from_str(&format!("{:0length$}", count - 1));
            let mut payload = 0;
            assert_eq!(
                // SAFETY: The fixture supplies the live map and a key of its declared kind;
                // the output borrows a disjoint local i64.
                unsafe { willow_map_get_into(copy, key as i64, 1, &mut payload) },
                0
            );
            assert_eq!(payload, !((count - 1) as i64));
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(2) };
            willow_gc_collect();
            assert_eq!(crate::gc_telemetry::external_bytes(), baseline);
        }
    }
}

#[test]
fn confined_constructor_keeps_reference_values_synchronized() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    // SAFETY: Only the creating test task accesses this local map while it is live.
    let mut map = unsafe { willow_map_new_local(3, 3, 1) };
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { willow_push_root(&raw mut map) };
    assert!(matches!(
        // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
        // constructor.
        unsafe { &storage(map).state },
        MapState::Traced(_)
    ));
    let mut key = willow_string_from_str("key");
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { willow_push_root(&raw mut key) };
    let value = willow_string_from_str("value");
    // SAFETY: The constructor and reference flags agree. Each pointer word comes from a
    // live fixture object at the call.
    unsafe { willow_map_insert(map, key as i64, 1, value as i64, 1) };
    willow_gc_collect();
    // SAFETY: The key matches the live map layout; any string key is current through the
    // lookup.
    let found = unsafe { willow_map_get(map, key as i64, 1, 1) };
    assert_eq!(
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        unsafe { crate::string::willow_string_as_str(found) },
        "value"
    );
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_roots(2) };
}

#[test]
fn confined_scalar_accesses_never_acquire_mutual_exclusion() {
    let _guard = crate::gc::isolated_runtime();
    for count in [16, 128, 1024, 8192] {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { crate::gc::willow_gc_init() };
        // SAFETY: Only the creating test task accesses this local map while it is live.
        let mut map = unsafe { willow_map_new_local(0, 0, 0) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { crate::gc::willow_push_root(&raw mut map) };
        assert!(matches!(
            // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by
            // the constructor.
            &unsafe { storage(map) }.state,
            MapState::Words(ScalarStorage::Local(_))
        ));
        WORD_SHARED_ACQUIRES.with(|count| count.set(0));
        for key in 0..count {
            // SAFETY: This live map was constructed with scalar key and value slots.
            unsafe { willow_map_insert(map, key, 0, !key, 0) };
            let mut payload = 0;
            // SAFETY: The fixture supplies the live map and a key of its declared kind;
            // the output borrows a disjoint local i64.
            assert_eq!(unsafe { willow_map_get_into(map, key, 0, &mut payload) }, 0);
            assert_eq!(payload, !key);
            assert_eq!(
                // SAFETY: The fixture supplies the live map and a key of its declared kind;
                // the output borrows a disjoint local i64.
                unsafe { willow_map_get_into(map, key + count, 0, &mut payload) },
                1
            );
            // SAFETY: The live map has scalar keys and this lookup borrows no pointer-
            // valued key. The handle is a live map from this fixture or its frozen copy.
            unsafe {
                assert_eq!(willow_map_contains(map, key, 0), 1);
                assert_eq!(willow_map_len(map), key + 1);
            }
        }
        assert_eq!(WORD_SHARED_ACQUIRES.with(|count| count.get()), 0);
        eprintln!(
            "confined operations={} mutual_exclusion_acquisitions=0",
            count * 5
        );
        // SAFETY: The source is a live map from the constructor and remains reachable
        // while copied.
        let mut copy = unsafe { willow_map_copy(map) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { crate::gc::willow_push_root(&raw mut copy) };
        assert!(matches!(
            // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by
            // the constructor.
            &unsafe { storage(copy) }.state,
            MapState::Words(ScalarStorage::Shared(_))
        ));
        // SAFETY: The handle is a live map from this fixture or its frozen copy.
        assert_eq!(unsafe { willow_map_len(copy) }, count);
        // SAFETY: The removed shadow-stack entries belong to this operation and their
        // slot storage is still live.
        unsafe { crate::gc::willow_pop_roots(2) };
        crate::gc::willow_gc_collect();
    }
}

#[test]
fn scalar_storage_uncontended_access_does_not_take_the_parking_mutex() {
    let storage = WordStorage::default();
    // Holding the slow-path mutex must not prevent fast acquire/release.
    // There are no contenders, so neither path should touch this mutex.
    let _parking = storage.waiters.lock().unwrap();
    for _ in 0..32 {
        let access = storage.lock();
        assert!(access.entries.is_empty());
        assert_eq!(storage.state.load(Ordering::Relaxed), 1);
        drop(access);
        assert_eq!(storage.state.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn scalar_storage_gate_serializes_retained_aliases_and_releases_on_unwind() {
    let _guard = crate::gc::isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { crate::gc::willow_gc_init() };
    let storage = WordStorage::default();
    storage.lock().entries.insert(0, 0);
    let start = std::sync::Barrier::new(4);
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let storage = &storage;
            let start = &start;
            scope.spawn(crate::runtime::serving(move || {
                start.wait();
                for _ in 0..1024 {
                    let mut data = storage.lock();
                    let previous = data.entries[&0];
                    // Widen overlap: without the shared storage gate these
                    // distinct aliases would race in the read/modify/write.
                    std::thread::yield_now();
                    data.entries.insert(0, previous + 1);
                }
            }));
        }
    });
    assert_eq!(storage.lock().entries[&0], 4096);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _access = storage.lock();
            panic!("release the storage gate on unwind");
        }))
        .is_err()
    );
    assert_eq!(storage.state.load(Ordering::Relaxed), 0);
    assert_eq!(storage.lock().entries[&0], 4096);
}

#[test]
fn scalar_map_aliases_share_one_gate_across_all_accessors() {
    let _guard = crate::gc::isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { crate::gc::willow_gc_init() };
    let mut map = willow_map_new(0, 0, 0);
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { crate::gc::willow_push_root(&raw mut map) };
    let address = map as usize;
    let start = std::sync::Barrier::new(4);
    std::thread::scope(|scope| {
        for writer in 0..4 {
            let start = &start;
            scope.spawn(crate::runtime::serving(move || {
                // Raw aliases model independent wrappers and an original
                // map alias. No external lock protects these operations.
                let map = address as *mut u8;
                start.wait();
                for i in 0..4096 {
                    let key = writer * 4096 + i;
                    // SAFETY: This live map was constructed with scalar key and value
                    // slots.
                    unsafe { willow_map_insert(map, key, 0, !key, 0) };
                    let mut value = 0;
                    // SAFETY: The fixture supplies the live map and a key of its declared
                    // kind; the output borrows a disjoint local i64.
                    assert_eq!(unsafe { willow_map_get_into(map, key, 0, &mut value) }, 0);
                    assert_eq!(value, !key);
                    // SAFETY: The live map has scalar keys and this lookup borrows no
                    // pointer-valued key. The handle is a live map from this fixture or
                    // its frozen copy.
                    unsafe {
                        assert_eq!(willow_map_contains(map, key, 0), 1);
                        assert!(willow_map_len(map) > i);
                    }
                }
            }));
        }
    });
    // SAFETY: The handle is a live map from this fixture or its frozen copy.
    assert_eq!(unsafe { willow_map_len(map) }, 16384);
    for key in 0..16384 {
        let mut value = 0;
        // SAFETY: The fixture supplies the live map and a key of its declared kind; the
        // output borrows a disjoint local i64.
        assert_eq!(unsafe { willow_map_get_into(map, key, 0, &mut value) }, 0);
        assert_eq!(value, !key);
    }
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { crate::gc::willow_pop_root() };
    crate::gc::willow_gc_collect();
}

#[test]
fn scalar_map_hash_work_and_lookup_allocations_scale_linearly() {
    use crate::scheduler::scaling_measurements::counting_allocator as counter;
    let _guard = crate::gc::isolated_runtime();
    for count in [16_i64, 128, 1024, 8192] {
        for stride in [1_i64, 2654435761, 1 << 32, -1] {
            // SAFETY: The test controls its isolated runtime at this reset boundary; no
            // other mutator or subsequent access relies on the discarded heap.
            unsafe { crate::gc::willow_gc_init() };
            let baseline = crate::gc_telemetry::external_bytes();
            let mut map = willow_map_new(0, 0, 0);
            // SAFETY: The initialized slot stays at this address until its registration
            // is popped; collection may rewrite it there.
            unsafe { crate::gc::willow_push_root(&raw mut map) };
            WORD_HASH_CALLS.with(|calls| calls.set(0));
            for i in 0..count {
                // SAFETY: This live map was constructed with scalar key and value slots.
                unsafe { willow_map_insert(map, i.wrapping_mul(stride), 0, i, 0) };
            }
            let insert_hashes = WORD_HASH_CALLS.with(|calls| calls.get());
            // Geometric table growth rehashes old keys, but total hashes
            // remain linear; this bound includes every growth in the loop.
            assert!(insert_hashes <= 4 * count as usize);
            let allocated = counter::thread_allocations();
            let gc_bytes = crate::gc::willow_gc_allocated_bytes();
            let retained = crate::gc_telemetry::external_bytes();
            WORD_HASH_CALLS.with(|calls| calls.set(0));
            for i in 0..count {
                let key = i.wrapping_mul(stride);
                let mut payload = -1;
                // SAFETY: The fixture supplies the live map and a key of its declared
                // kind; the output borrows a disjoint local i64.
                assert_eq!(unsafe { willow_map_get_into(map, key, 0, &mut payload) }, 0);
                assert_eq!(payload, i);
                // SAFETY: The live map has scalar keys and this lookup borrows no
                // pointer-valued key.
                assert_eq!(unsafe { willow_map_contains(map, key, 0) }, 1);
                assert_eq!(
                    // SAFETY: The fixture supplies the live map and a key of its declared
                    // kind; the output borrows a disjoint local i64.
                    unsafe {
                        willow_map_get_into(map, (i + count).wrapping_mul(stride), 0, &mut payload)
                    },
                    1
                );
                assert_eq!(payload, 0);
                // SAFETY: This live map was constructed with scalar key and value slots.
                unsafe { willow_map_insert(map, key, 0, !i, 0) };
            }
            assert_eq!(
                WORD_HASH_CALLS.with(|calls| calls.get()),
                4 * count as usize
            );
            assert_eq!(counter::thread_allocations(), allocated);
            assert_eq!(crate::gc::willow_gc_allocated_bytes(), gc_bytes);
            assert_eq!(crate::gc_telemetry::external_bytes(), retained);
            // SAFETY: The handle is a live map from this fixture or its frozen copy.
            assert_eq!(unsafe { willow_map_len(map) }, count);
            eprintln!(
                "scalar count={count} stride={stride} insert_hashes={insert_hashes} lookup_update_hashes={} allocations=0",
                4 * count
            );
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { crate::gc::willow_pop_root() };
            crate::gc::willow_gc_collect();
            assert_eq!(crate::gc_telemetry::external_bytes(), baseline);
        }
    }
}

#[test]
fn scalar_gc_hooks_do_not_borrow_mutating_storage() {
    let _guard = crate::gc::isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { crate::gc::willow_gc_init() };
    let mut map = willow_map_new(0, 0, 0);
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { crate::gc::willow_push_root(&raw mut map) };
    let address = map as usize;
    std::thread::scope(|scope| {
        scope.spawn(crate::runtime::serving(move || {
            let map = address as *mut u8;
            let mut slots = Vec::new();
            let mut children = Vec::new();
            for _ in 0..8192 {
                // Exercise all hooks while the mutator may resize its table.
                // SAFETY: The callback receives the live map payload selected by its
                // registered layout.
                unsafe {
                    trace_map(map, &mut slots);
                    snapshot_map(map, &mut children);
                    assert!(matches!(
                        snapshot_map_slice(map, 0, 1, &mut children),
                        crate::gc::TraceSliceProgress::Done
                    ));
                }
            }
            assert!(slots.is_empty());
            assert!(children.is_empty());
        }));
        for key in 0..8192 {
            // SAFETY: This live map was constructed with scalar key and value slots.
            unsafe { willow_map_insert(map, key, 0, key, 0) };
        }
    });
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { crate::gc::willow_pop_root() };
    crate::gc::willow_gc_collect();
}

#[test]
fn scalar_frozen_copy_supports_parallel_reads_and_independent_updates() {
    let _guard = crate::gc::isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { crate::gc::willow_gc_init() };
    let baseline = crate::gc_telemetry::external_bytes();
    let mut map = willow_map_new(0, 0, 0);
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { crate::gc::willow_push_root(&raw mut map) };
    for key in 0..1024 {
        // SAFETY: This live map was constructed with scalar key and value slots.
        unsafe { willow_map_insert(map, key, 0, !key, 0) };
    }
    // SAFETY: The source is a live map from the constructor and remains reachable while
    // copied.
    let mut copy = unsafe { willow_map_copy(map) };
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { crate::gc::willow_push_root(&raw mut copy) };
    // SAFETY: This live map was constructed with scalar key and value slots.
    unsafe { willow_map_insert(map, 0, 0, 42, 0) };
    let address = copy as usize;
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(crate::runtime::serving(move || {
                let map = address as *mut u8;
                for key in 0..1024 {
                    let mut payload = 0;
                    // SAFETY: The fixture supplies the live map and a key of its declared
                    // kind; the output borrows a disjoint local i64.
                    assert_eq!(unsafe { willow_map_get_into(map, key, 0, &mut payload) }, 0);
                    assert_eq!(payload, !key);
                }
            }));
        }
    });
    crate::gc::willow_gc_collect();
    let mut payload = 0;
    // SAFETY: The fixture supplies the live map and a key of its declared kind; the
    // output borrows a disjoint local i64.
    assert_eq!(unsafe { willow_map_get_into(copy, 0, 0, &mut payload) }, 0);
    assert_eq!(payload, -1);
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { crate::gc::willow_pop_roots(2) };
    crate::gc::willow_gc_collect();
    assert_eq!(crate::gc_telemetry::external_bytes(), baseline);
}

#[test]
fn concurrent_slice_retries_a_busy_map_without_waiting_or_partial_children() {
    let _guard = crate::gc::isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { crate::gc::willow_gc_init() };
    let mut map = willow_map_new(0, 0, 1);
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { crate::gc::willow_push_root(&raw mut map) };
    let child = crate::gc::willow_alloc(8);
    // SAFETY: The constructor and reference flags agree. Each pointer word comes from a
    // live fixture object at the call.
    unsafe { willow_map_insert(map, 0, 0, child as i64, 1) };
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    let guard = unsafe { map_data(map) };
    let mut children = Vec::new();
    assert!(matches!(
        // SAFETY: The callback receives the live map payload selected by its registered
        // layout.
        unsafe { snapshot_map_slice(map, 0, 512, &mut children) },
        crate::gc::TraceSliceProgress::Retry
    ));
    assert!(children.is_empty());
    drop(guard);
    assert!(matches!(
        // SAFETY: The callback receives the live map payload selected by its registered
        // layout.
        unsafe { snapshot_map_slice(map, 0, 512, &mut children) },
        crate::gc::TraceSliceProgress::Done
    ));
    assert_eq!(children, [child]);
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { crate::gc::willow_pop_root() };
    crate::gc::willow_gc_collect();
}
use crate::gc::{
    isolated_runtime, willow_gc_collect, willow_gc_init, willow_pop_roots, willow_push_root,
};
use crate::string::{willow_string_as_str, willow_string_from_str};

fn opt_tag(opt: *mut u8) -> i64 {
    // SAFETY: The Option result is a GC word-aligned allocation containing the tag and its
    // payload.
    unsafe { *(opt as *const i64) }
}
fn opt_payload(opt: *mut u8) -> i64 {
    // SAFETY: The Option result is a GC word-aligned allocation containing the tag and its
    // payload.
    unsafe { *((opt as *const i64).add(1)) }
}

#[test]
fn map_unit_01_new_is_empty() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    let m = willow_map_new(0, 0, 0);
    assert!(!m.is_null());
    // SAFETY: The handle is a live map from this fixture or its frozen copy.
    assert_eq!(unsafe { willow_map_len(m) }, 0);
}

#[test]
fn inline_maps_remain_stable_and_are_reclaimed_at_increasing_counts() {
    use crate::gc::{GC_HEADER_SIZE, willow_gc_allocated_bytes};

    let _guard = isolated_runtime();
    for count in [1, 16, 128] {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // Reserve every root slot before registering its address.
        let mut maps = vec![std::ptr::null_mut(); count];
        for slot in &mut maps {
            *slot = willow_map_new(3, 3, 1);
            assert!(!slot.is_null());
            assert!((*slot as usize).is_multiple_of(align_of::<MapStorage>()));
            // SAFETY: The slot storage stays live and stationary through the matching
            // pop, including during collection.
            unsafe { willow_push_root(slot) };
        }
        let payload_bytes = size_of::<MapStorage>();
        assert_eq!(
            willow_gc_allocated_bytes() as usize,
            count * (GC_HEADER_SIZE + payload_bytes)
        );
        let addresses = maps.clone();
        for &map in &maps {
            let mut key = willow_string_from_str("owned-key");
            // SAFETY: The initialized slot stays at this address until its registration
            // is popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut key) };
            let value = willow_string_from_str("traced-value");
            // SAFETY: The constructor and reference flags agree. Each pointer word comes
            // from a live fixture object at the call.
            unsafe { willow_map_insert(map, key as i64, 1, value as i64, 1) };
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(1) };
        }
        willow_gc_collect();
        assert_eq!(maps, addresses, "inline mutexes must never move");
        for &map in &maps {
            let key = willow_string_from_str("owned-key");
            // SAFETY: The key matches the live map layout; any string key is current
            // through the lookup.
            let value = unsafe { willow_map_get(map, key as i64, 1, 1) };
            // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this
            // borrow.
            assert_eq!(unsafe { willow_string_as_str(value) }, "traced-value");
        }
        // SAFETY: The removed shadow-stack entries belong to this operation and their
        // slot storage is still live.
        unsafe { willow_pop_roots(count as i32) };
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
    }
}

#[test]
fn map_unit_02_int_key_insert_get() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    let m = willow_map_new(0, 0, 0);
    let _keep_m = crate::gc::GcRootHandle::new(m);
    // SAFETY: This live map was constructed with scalar key and value slots. The handle is
    // a live map from this fixture or its frozen copy.
    unsafe {
        willow_map_insert(m, 7, 0, 100, 0);
        willow_map_insert(m, 8, 0, 200, 0);
        assert_eq!(willow_map_len(m), 2);
    }
    // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued key.
    let g = unsafe { willow_map_get(m, 7, 0, 0) };
    assert_eq!(opt_tag(g), 0); // Some
    assert_eq!(opt_payload(g), 100);
    // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued key.
    assert_eq!(opt_tag(unsafe { willow_map_get(m, 99, 0, 0) }), 1); // None
}

#[test]
fn map_get_into_preserves_payload_bits_without_allocating_wrappers() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    for kind in [0, 1, 2] {
        let mut map = willow_map_new(0, kind, 0);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut map) };
        let words = [0, 1, -1, i64::MIN, i64::MAX, f64::NAN.to_bits() as i64];
        for (key, word) in words.iter().enumerate() {
            // SAFETY: This live map was constructed with scalar key and value slots.
            unsafe { willow_map_insert(map, key as i64, 0, *word, 0) };
        }
        let bytes = crate::gc::willow_gc_allocated_bytes();
        for count in [1, 16, 128] {
            for _ in 0..count {
                for (key, word) in words.iter().enumerate() {
                    let mut payload = 123;
                    assert_eq!(
                        // SAFETY: The fixture supplies the live map and a key of its declared
                        // kind; the output borrows a disjoint local i64.
                        unsafe { willow_map_get_into(map, key as i64, 0, &mut payload) },
                        0
                    );
                    assert_eq!(payload, *word);
                }
                let mut payload = 123;
                // SAFETY: The fixture supplies the live map and a key of its declared
                // kind; the output borrows a disjoint local i64.
                assert_eq!(unsafe { willow_map_get_into(map, 999, 0, &mut payload) }, 1);
                assert_eq!(payload, 0);
            }
            assert_eq!(crate::gc::willow_gc_allocated_bytes(), bytes);
        }
        // SAFETY: The removed shadow-stack entries belong to this operation and their
        // slot storage is still live.
        unsafe { willow_pop_roots(1) };
    }
    let mut payload = 123;
    assert_eq!(
        // SAFETY: The null-map path writes the local output word and returns a miss.
        unsafe { willow_map_get_into(std::ptr::null_mut(), 0, 0, &mut payload) },
        1
    );
    assert_eq!(payload, 0);
}

#[test]
fn map_unit_03_insert_overwrites() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    let m = willow_map_new(0, 0, 0);
    // SAFETY: This live map was constructed with scalar key and value slots. The handle is
    // a live map from this fixture or its frozen copy. The live map has scalar keys and
    // this lookup borrows no pointer-valued key.
    unsafe {
        willow_map_insert(m, 1, 0, 10, 0);
        willow_map_insert(m, 1, 0, 20, 0);
        assert_eq!(willow_map_len(m), 1);
        assert_eq!(opt_payload(willow_map_get(m, 1, 0, 0)), 20);
    }
}

#[test]
fn map_unit_04_string_keys_compare_by_content() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    let m = willow_map_new(3, 0, 0);
    let _keep_m = crate::gc::GcRootHandle::new(m);
    let alice = willow_string_from_str("Alice");
    // SAFETY: The constructor and reference flags agree. Each pointer word comes from a
    // live fixture object at the call.
    unsafe { willow_map_insert(m, alice as i64, 1, 30, 0) };
    // A *different* string object with the same content must hit.
    let alice2 = willow_string_from_str("Alice");
    // SAFETY: The key matches the live map layout; any string key is current through the
    // lookup.
    let g = unsafe { willow_map_get(m, alice2 as i64, 1, 0) };
    assert_eq!(opt_tag(g), 0);
    assert_eq!(opt_payload(g), 30);
    let bob = willow_string_from_str("Bob");
    // SAFETY: The key matches the live map layout; any string key is current through the
    // lookup.
    assert_eq!(opt_tag(unsafe { willow_map_get(m, bob as i64, 1, 0) }), 1); // None
}

#[test]
fn null_string_key_matches_empty_and_fills_one_output_word() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    let map = crate::gc::GcRootHandle::new(willow_map_new(3, 0, 0));
    let empty = willow_string_from_str("");
    let mut output = [-11_i64, -1, -22];
    // SAFETY: The rooted map uses string keys and scalar values. Zero denotes
    // the empty key. The fresh string stays current through these non-allocating
    // lookups and the middle output word is exclusively borrowed for each call.
    unsafe {
        willow_map_insert(map.load(), 0, 1, 73, 0);
        assert_eq!(willow_map_contains(map.load(), empty as i64, 1), 1);
        assert_eq!(
            willow_map_get_into(map.load(), empty as i64, 1, &mut output[1]),
            0
        );
        assert_eq!(output, [-11, 73, -22]);
        assert_eq!(willow_map_remove_into(map.load(), 0, 1, &mut output[1]), 0);
        assert_eq!(output, [-11, 73, -22]);
        assert_eq!(willow_map_get_into(map.load(), 0, 1, &mut output[1]), 1);
    }
    assert_eq!(output, [-11, 0, -22]);
}

#[test]
fn map_unit_05_contains() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    let m = willow_map_new(0, 0, 0);
    // SAFETY: This live map was constructed with scalar key and value slots. The live map
    // has scalar keys and this lookup borrows no pointer-valued key.
    unsafe {
        willow_map_insert(m, 5, 0, 50, 0);
        assert_eq!(willow_map_contains(m, 5, 0), 1);
        assert_eq!(willow_map_contains(m, 6, 0), 0);
    }
}

#[test]
fn map_unit_06_reference_values_survive_collection() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    let mut m = willow_map_new(0, 3, 1);
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { willow_push_root(&raw mut m) };
    let v = willow_string_from_str("kept-value");
    // SAFETY: The constructor and reference flags agree. Each pointer word comes from a
    // live fixture object at the call.
    unsafe { willow_map_insert(m, 1, 0, v as i64, 1) };
    willow_gc_collect();
    // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued key.
    let g = unsafe { willow_map_get(m, 1, 0, 0) };
    assert_eq!(opt_tag(g), 0);
    let got = opt_payload(g) as *mut u8;
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    assert_eq!(unsafe { willow_string_as_str(got) }, "kept-value");
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_roots(1) };
}

#[test]
fn map_unit_07_reference_option_uses_nullable_pointer_niche() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    let m = willow_map_new(0, 3, 1);
    let _keep_m = crate::gc::GcRootHandle::new(m);
    let value = willow_string_from_str("niche-value");
    // SAFETY: The constructor and reference flags agree. Each pointer word comes from a
    // live fixture object at the call. The live map has scalar keys and this lookup
    // borrows no pointer-valued key.
    unsafe {
        willow_map_insert(m, 1, 0, value as i64, 1);
        assert_eq!(willow_map_get(m, 1, 0, 1), value);
        assert!(willow_map_get(m, 2, 0, 1).is_null());
    }
}
#[test]
fn map_layout_survives_freeze_and_drives_word_display() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    let map = willow_map_new(1, 2, 0);
    let _keep_map = crate::gc::GcRootHandle::new(map);
    // SAFETY: This live map was constructed with scalar key and value slots.
    unsafe { willow_map_insert(map, 1.5f64.to_bits() as i64, 0, 1, 0) };
    // SAFETY: The source is a live map from the constructor and remains reachable while
    // copied.
    let copy = unsafe { willow_map_copy(map) };
    let _keep_copy = crate::gc::GcRootHandle::new(copy);
    // SAFETY: The map copy retains the declared key and value representations.
    let text = unsafe { willow_map_to_string(copy) };
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    assert_eq!(unsafe { willow_string_as_str(text) }, "{1.5: true}");
    // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
    // constructor.
    let layout = unsafe { storage(copy) }.layout;
    assert_eq!(
        (layout.key_kind, layout.value_kind, layout.value_is_ref),
        (1, 2, false)
    );
}
#[test]
fn borrowed_string_keys_survive_copy_update_and_collection() {
    let _guard = isolated_runtime();
    for local in [false, true] {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let mut map = if local {
            // SAFETY: Only the creating test task accesses this local map while it is live.
            unsafe { willow_map_new_local(3, 0, 0) }
        } else {
            willow_map_new(3, 0, 0)
        };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut map) };
        for (i, text) in ["", "日本語🦀", "ordinary", "embedded\0nul"]
            .into_iter()
            .enumerate()
        {
            let key = willow_string_from_str(text);
            // SAFETY: The constructor and reference flags agree. Each pointer word comes
            // from a live fixture object at the call.
            unsafe { willow_map_insert(map, key as i64, 1, i as i64, 0) };
        }
        // SAFETY: The source is a live map from the constructor and remains reachable
        // while copied.
        let mut copy = unsafe { willow_map_copy(map) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut copy) };
        // No source key is rooted: map keys own their bytes independently.
        willow_gc_collect();
        for (i, text) in ["", "日本語🦀", "ordinary", "embedded\0nul"]
            .into_iter()
            .enumerate()
        {
            let mut key = willow_string_from_str(text);
            // SAFETY: The initialized slot stays at this address until its registration
            // is popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut key) };
            // SAFETY: The key matches the live map layout; any string key is current
            // through the lookup.
            assert_eq!(unsafe { willow_map_contains(map, key as i64, 1) }, 1);
            assert_eq!(
                // SAFETY: The key matches the live map layout; any string key is current
                // through the lookup.
                opt_payload(unsafe { willow_map_get(copy, key as i64, 1, 0) }),
                i as i64
            );
            // SAFETY: The constructor and reference flags agree. Each pointer word comes
            // from a live fixture object at the call.
            unsafe { willow_map_insert(map, key as i64, 1, 99, 0) };
            assert_eq!(
                // SAFETY: The key matches the live map layout; any string key is current
                // through the lookup.
                opt_payload(unsafe { willow_map_get(copy, key as i64, 1, 0) }),
                i as i64
            );
            assert_eq!(
                // SAFETY: The key matches the live map layout; any string key is current
                // through the lookup.
                opt_payload(unsafe { willow_map_get(map, key as i64, 1, 0) }),
                99
            );
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(1) };
        }
        // SAFETY: The handle is a live map from this fixture or its frozen copy.
        assert_eq!(unsafe { willow_map_len(map) }, 4);
        // SAFETY: The removed shadow-stack entries belong to this operation and their
        // slot storage is still live.
        unsafe { willow_pop_roots(2) };
    }
}

#[test]
fn borrowed_word_keys_preserve_all_bits() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    let map = willow_map_new(0, 0, 0);
    let _keep_map = crate::gc::GcRootHandle::new(map);
    let words = [
        0,
        i64::MIN,
        0x7ff8_0000_0000_0001,
        0x7ff8_0000_0000_0002,
        -1,
    ];
    for (i, word) in words.into_iter().enumerate() {
        // SAFETY: This live map was constructed with scalar key and value slots.
        unsafe { willow_map_insert(map, word, 0, i as i64, 0) };
    }
    for (i, word) in words.into_iter().enumerate() {
        // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued
        // key.
        assert_eq!(unsafe { willow_map_contains(map, word, 0) }, 1);
        assert_eq!(
            // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued
            // key.
            opt_payload(unsafe { willow_map_get(map, word, 0, 0) }),
            i as i64
        );
    }
    // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued key.
    // The handle is a live map from this fixture or its frozen copy.
    unsafe {
        assert_eq!(willow_map_contains(map, 1, 0), 0);
        assert_eq!(willow_map_len(map), words.len() as i64);
    }
}

#[test]
fn float_keys_normalize_zero_preserve_numbers_and_copy() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    let mut map = willow_map_new(1, 0, 0);
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { willow_push_root(&raw mut map) };
    for zeros in [[0.0_f64, -0.0], [-0.0_f64, 0.0]] {
        for (index, key) in zeros.into_iter().enumerate() {
            // SAFETY: This live map was constructed with scalar key and value slots.
            unsafe { willow_map_insert(map, key.to_bits() as i64, 0, index as i64 + 10, 0) };
        }
        // SAFETY: The handle is a live map from this fixture or its frozen copy.
        assert_eq!(unsafe { willow_map_len(map) }, 1);
        for key in zeros {
            assert_eq!(
                // SAFETY: The live map has scalar keys and this lookup borrows no pointer-
                // valued key.
                unsafe { willow_map_contains(map, key.to_bits() as i64, 0) },
                1
            );
            assert_eq!(
                // SAFETY: The live map has scalar keys and this lookup borrows no
                // pointer-valued key.
                opt_payload(unsafe { willow_map_get(map, key.to_bits() as i64, 0, 0) }),
                11
            );
        }
    }
    let values = [
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::MAX,
        f64::MIN_POSITIVE,
        f64::from_bits(1),
        -f64::from_bits(1),
        1.5,
        -1.5,
    ];
    for (index, value) in values.into_iter().enumerate() {
        // SAFETY: This live map was constructed with scalar key and value slots.
        unsafe { willow_map_insert(map, value.to_bits() as i64, 0, index as i64, 0) };
    }
    // SAFETY: The source is a live map from the constructor and remains reachable while
    // copied.
    let mut copy = unsafe { willow_map_copy(map) };
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { willow_push_root(&raw mut copy) };
    // SAFETY: The handle is a live map from this fixture or its frozen copy.
    assert_eq!(unsafe { willow_map_len(copy) }, 9);
    for (index, value) in values.into_iter().enumerate() {
        assert_eq!(
            // SAFETY: The live map has scalar keys and this lookup borrows no pointer-
            // valued key.
            opt_payload(unsafe { willow_map_get(copy, value.to_bits() as i64, 0, 0) }),
            index as i64
        );
    }
    assert_eq!(
        // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued key.
        opt_payload(unsafe { willow_map_get(copy, i64::MIN, 0, 0) }),
        11
    );
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    assert!(!unsafe { willow_string_as_str(willow_map_to_string(copy)) }.contains("-0:"));
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_roots(2) };
}

#[test]
fn float_keys_reject_all_nan_forms_without_mutating_or_holding_lock() {
    use crate::panic_context::*;
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    let previous = replace_current_context(Some(std::sync::Arc::new(PanicContext::new(903))));
    let mut map = willow_map_new(1, 0, 0);
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { willow_push_root(&raw mut map) };
    // SAFETY: This live map was constructed with scalar key and value slots.
    unsafe { willow_map_insert(map, 0, 0, 42, 0) };
    for bits in [
        0x7ff0_0000_0000_0001u64,
        0x7ff8_0000_0000_0000,
        0x7fff_ffff_ffff_ffff,
    ] {
        for sign in [0, 1u64 << 63] {
            for operation in 0..5 {
                let word = (bits | sign) as i64;
                match operation {
                    // SAFETY: This live map was constructed with scalar key and value
                    // slots.
                    0 => unsafe { willow_map_insert(map, word, 0, 99, 0) },
                    // SAFETY: The live map has scalar keys and this lookup borrows no
                    // pointer-valued key.
                    1 => assert_eq!(unsafe { willow_map_contains(map, word, 0) }, 0),
                    4 => {
                        let mut payload = 123;
                        assert_eq!(
                            // SAFETY: The fixture supplies the live map and a key of its
                            // declared kind; the output borrows a disjoint local i64.
                            unsafe { willow_map_get_into(map, word, 0, &mut payload) },
                            1
                        );
                        assert_eq!(payload, 0);
                    }
                    _ => assert!(
                        // SAFETY: The live map has scalar keys and this lookup borrows no
                        // pointer-valued key.
                        unsafe { willow_map_get(map, word, 0, operation - 2) }.is_null()
                    ),
                }
                assert_eq!(willow_panic_depth(), 1);
                willow_panic_enter_defer();
                let info = willow_panic_recover();
                willow_panic_leave_defer();
                assert_eq!(
                    // SAFETY: The retained PanicInfo keeps its initialized fields live through
                    // this read.
                    unsafe { panic_info_message(info) },
                    "NaN cannot be used as a Map key"
                );
                willow_panic_release_recovered(info);
                // SAFETY: The handle is a live map from this fixture or its frozen copy.
                // The live map has scalar keys and this lookup borrows no pointer-valued
                // key.
                unsafe {
                    assert_eq!(willow_map_len(map), 1);
                    assert_eq!(opt_payload(willow_map_get(map, 0, 0, 0)), 42);
                }
            }
        }
    }
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_roots(1) };
    replace_current_context(previous);
}

#[test]
fn float_key_normalization_scales_without_allocations() {
    use crate::scheduler::scaling_measurements::counting_allocator as counter;
    for count in [16usize, 128, 1024, 8192] {
        let allocations = counter::thread_allocations();
        for index in 0..count {
            let word = (index as f64).to_bits() as i64;
            assert!(
                // SAFETY: The map's declared key kind determines whether the word contains a
                // retained string or a scalar.
                matches!(unsafe { key_from_word(word, 1) }, Some(KeyRef::Word(got)) if got == word)
            );
            // Integer keys that happen to encode NaN or -0.0 remain integers.
            assert!(matches!(
                // SAFETY: The map's declared key kind determines whether the word contains a
                // retained string or a scalar.
                unsafe { key_from_word(i64::MIN, 0) },
                Some(KeyRef::Word(i64::MIN))
            ));
            assert!(matches!(
                // SAFETY: The map's declared key kind determines whether the word contains a
                // retained string or a scalar.
                unsafe { key_from_word(f64::NAN.to_bits() as i64, 0) },
                Some(KeyRef::Word(_))
            ));
        }
        assert_eq!(counter::thread_allocations(), allocations);
        eprintln!("normalization calls={}, allocations=0", 3 * count);
    }
}

/// `freeze()` copies the value words a moving collection
/// leaves in the source, not the ones it read before that collection.
///
/// The scenario is a worker that freezes a map while another thread stops
/// the world for a minor collection. The rooted source map is pinned, but
/// its young values are reachable only through its slots, so they are
/// COPIED to the old generation and the source's slots are rewritten. A
/// native snapshot of those slots taken before `willow_map_copy`'s own
/// allocation would carry the pre-move addresses into the copy.
///
/// Deterministic ordering: this thread registers as a mutator, so the
/// collector cannot proceed until it parks; `alloc` stress makes the
/// destination allocation reach `collect_internal`, whose first act under a
/// pending stop is to park at the safepoint. The collection therefore runs
/// exactly between the copy's allocation and its return.
#[test]
fn copy_stores_values_as_relocated_by_a_collection_inside_its_allocation() {
    use std::time::{Duration, Instant};
    const VALUES: i64 = 8;

    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    crate::gc::willow_gc_register_mutator();
    // Environment stress must not promote the values before the copy: the
    // point is that they are still young when the collection runs.
    crate::gc::set_gc_stress_for_test(Some(""));
    let tls = crate::gc::tlab_state_for_test();
    let mut map = willow_map_new(0, 4, 1);
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { willow_push_root(&raw mut map) };
    let mut young = Vec::new();
    for i in 0..VALUES {
        // SAFETY: The fixture owns the stable TLAB through unregistration. The requested
        // size and tracing metadata describe the object initialized here.
        let value = unsafe { crate::gc::willow_gc_alloc_slow(tls.as_ptr(), 42, 0, 8, 0) };
        assert!(!value.is_null());
        // SAFETY: The test allocates a full i64 in word-aligned GC storage and reads the live
        // value through the map or root.
        unsafe { *(value as *mut i64) = 1000 + i };
        // SAFETY: The constructor and reference flags agree. Each pointer word comes from
        // a live fixture object at the call.
        unsafe { willow_map_insert(map, i, 0, value as i64, 1) };
        young.push(value);
    }
    let moved_before = crate::gc::willow_gc_moved_objects();

    let collector = std::thread::spawn(crate::runtime::serving(crate::gc::minor_collect_internal));
    // Wait for the COORDINATION flag, not the lock-free gate: the gate is
    // published first, and a safepoint reached between the two returns
    // without parking, which would leave the collector waiting forever.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !crate::gc::stop_pending_for_test() {
        assert!(
            Instant::now() < deadline,
            "the collector never requested a stop"
        );
        std::thread::yield_now();
    }
    crate::gc::set_gc_stress_for_test(Some("alloc"));
    // SAFETY: The source is a live map from the constructor and remains reachable while
    // copied.
    let mut copy = unsafe { willow_map_copy(map) };
    crate::gc::set_gc_stress_for_test(None);
    collector.join().unwrap();
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { willow_push_root(&raw mut copy) };

    assert_eq!(
        crate::gc::willow_gc_moved_objects() - moved_before,
        VALUES,
        "every value was evacuated while the copy was in progress"
    );
    for i in 0..VALUES {
        // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued
        // key.
        let source = unsafe { willow_map_get(map, i, 0, 1) };
        // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued
        // key.
        let copied = unsafe { willow_map_get(copy, i, 0, 1) };
        assert_ne!(source, young[i as usize], "value {i} moved");
        assert_eq!(copied, source, "copy holds the relocated value {i}");
        // SAFETY: The test allocates a full i64 in word-aligned GC storage and reads the live
        // value through the map or root.
        assert_eq!(unsafe { *(copied as *const i64) }, 1000 + i);
    }
    // SAFETY: The handle is a live map from this fixture or its frozen copy.
    assert_eq!(unsafe { willow_map_len(copy) }, VALUES);
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_roots(2) };
    crate::gc::willow_gc_unregister_mutator();
}

/// The copy's own slots take the write barrier: a young value copied into
/// the old destination puts the DESTINATION in the remembered set (the
/// source already is), so a later minor collection rewrites the copy as
/// well as the source. The copy is a direct root here, which would get it
/// traced anyway; the remembered-set count is what shows the barrier ran.
#[test]
fn copy_of_young_values_is_updated_by_the_next_minor_collection() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    crate::gc::set_gc_stress_for_test(Some(""));
    let tls = crate::gc::tlab_state_for_test();
    let mut map = willow_map_new(0, 4, 1);
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { willow_push_root(&raw mut map) };
    // SAFETY: The fixture owns the stable TLAB through unregistration. The requested size
    // and tracing metadata describe the object initialized here.
    let young = unsafe { crate::gc::willow_gc_alloc_slow(tls.as_ptr(), 42, 0, 8, 0) };
    // SAFETY: The test allocates a full i64 in word-aligned GC storage and reads the live
    // value through the map or root.
    unsafe { *(young as *mut i64) = 77 };
    // SAFETY: The constructor and reference flags agree. Each pointer word comes from a
    // live fixture object at the call.
    unsafe { willow_map_insert(map, 1, 0, young as i64, 1) };
    let remembered = crate::gc::willow_gc_remembered_set_size();
    // SAFETY: The source is a live map from the constructor and remains reachable while
    // copied.
    let mut copy = unsafe { willow_map_copy(map) };
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { willow_push_root(&raw mut copy) };
    crate::gc::set_gc_stress_for_test(None);
    // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued key.
    assert_eq!(unsafe { willow_map_get(copy, 1, 0, 1) }, young);
    assert_eq!(
        crate::gc::willow_gc_remembered_set_size(),
        remembered + 1,
        "the copy joined the remembered set"
    );

    crate::gc::minor_collect_internal();

    // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued key.
    let moved = unsafe { willow_map_get(map, 1, 0, 1) };
    assert_ne!(moved, young, "the young value was evacuated");
    // SAFETY: The live map has scalar keys and this lookup borrows no pointer-valued key.
    assert_eq!(unsafe { willow_map_get(copy, 1, 0, 1) }, moved);
    // SAFETY: The test allocates a full i64 in word-aligned GC storage and reads the live
    // value through the map or root.
    assert_eq!(unsafe { *(moved as *const i64) }, 77);
    // The copy is independent: dropping the source keeps the value alive
    // through the copy alone.
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_roots(2) };
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { willow_push_root(&raw mut copy) };
    crate::gc::willow_gc_collect();
    assert_eq!(
        // SAFETY: The test allocates a full i64 in word-aligned GC storage and reads the live
        // value through the map or root.
        unsafe { *(willow_map_get(copy, 1, 0, 1) as *const i64) },
        77
    );
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_roots(1) };
    // `tls` lives on this stack and is registered with the heap; drop the
    // registration before it goes out of scope.
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
}

/// A full collection inside the copy's allocation (non-moving) keeps every
/// value: the only thing keeping them alive at that moment is the rooted
/// source map.
#[test]
fn copy_keeps_string_values_across_a_full_collection_in_its_allocation() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    let mut map = willow_map_new(3, 3, 1);
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { willow_push_root(&raw mut map) };
    for (key, value) in [("a", "alpha"), ("b", "beta"), ("c", "gamma")] {
        let mut key = willow_string_from_str(key);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut key) };
        let value = willow_string_from_str(value);
        // SAFETY: The constructor and reference flags agree. Each pointer word comes from
        // a live fixture object at the call.
        unsafe { willow_map_insert(map, key as i64, 1, value as i64, 1) };
        // SAFETY: The removed shadow-stack entries belong to this operation and their
        // slot storage is still live.
        unsafe { willow_pop_roots(1) };
    }
    let majors = crate::gc::willow_gc_major_collections();
    crate::gc::set_gc_stress_for_test(Some("alloc"));
    // SAFETY: The source is a live map from the constructor and remains reachable while
    // copied.
    let mut copy = unsafe { willow_map_copy(map) };
    crate::gc::set_gc_stress_for_test(None);
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { willow_push_root(&raw mut copy) };
    assert!(crate::gc::willow_gc_major_collections() > majors);
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_roots(2) };
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { willow_push_root(&raw mut copy) };
    crate::gc::willow_gc_collect();
    for (key, value) in [("a", "alpha"), ("b", "beta"), ("c", "gamma")] {
        let mut key = willow_string_from_str(key);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut key) };
        // SAFETY: The key matches the live map layout; any string key is current through
        // the lookup.
        let got = unsafe { willow_map_get(copy, key as i64, 1, 1) };
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        assert_eq!(unsafe { willow_string_as_str(got) }, value);
        // SAFETY: The removed shadow-stack entries belong to this operation and their
        // slot storage is still live.
        unsafe { willow_pop_roots(1) };
    }
    // SAFETY: The handle is a live map from this fixture or its frozen copy.
    assert_eq!(unsafe { willow_map_len(copy) }, 3);
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_roots(1) };
}

/// a read-only lookup must not copy the key. Counted per
/// thread through the test binary's counting global allocator, after all
/// setup allocations, so the numbers are this loop's own Rust allocation
/// traffic and nothing else's.
#[test]
fn read_only_lookups_never_copy_the_key() {
    use crate::scheduler::scaling_measurements::counting_allocator as counter;
    const CALLS: usize = 1_000;
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    let mut map = willow_map_new(3, 0, 0);
    // SAFETY: The initialized slot stays at this address until its registration is
    // popped; collection may rewrite it there.
    unsafe { willow_push_root(&raw mut map) };
    let mut get_bytes_by_len: Vec<(usize, usize)> = Vec::new();
    for len in [8usize, 64, 1024] {
        let mut hit = willow_string_from_str(&"k".repeat(len));
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut hit) };
        // SAFETY: The constructor and reference flags agree. Each pointer word comes from
        // a live fixture object at the call.
        unsafe { willow_map_insert(map, hit as i64, 1, len as i64, 0) };
        let mut miss = willow_string_from_str(&"m".repeat(len));
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut miss) };

        // `contains` returns a scalar, so the whole call is the lookup:
        // hits and misses alike must allocate nothing at any key length.
        let allocs = counter::thread_allocations();
        let bytes = counter::thread_bytes();
        for _ in 0..CALLS {
            // SAFETY: The key matches the live map layout; any string key is current
            // through the lookup.
            unsafe {
                assert_eq!(willow_map_contains(map, hit as i64, 1), 1);
                assert_eq!(willow_map_contains(map, miss as i64, 1), 0);
            }
        }
        assert_eq!(
            (
                counter::thread_allocations() - allocs,
                counter::thread_bytes() - bytes
            ),
            (0, 0),
            "contains allocated at key length {len}"
        );

        // The same for `get`'s key lookup with its required `Option`
        // output excluded: borrow the key, probe the table, drop it.
        // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
        // constructor.
        let data = unsafe { map_data(map) };
        let allocs = counter::thread_allocations();
        let bytes = counter::thread_bytes();
        for _ in 0..CALLS {
            // SAFETY: The map's declared key kind determines whether the word contains a
            // retained string or a scalar.
            let key = unsafe { key_from_word(hit as i64, 3) }.unwrap();
            assert_eq!(data.get(&key), Some(len as i64));
            // SAFETY: The map's declared key kind determines whether the word contains a
            // retained string or a scalar.
            let key = unsafe { key_from_word(miss as i64, 3) }.unwrap();
            assert_eq!(data.get(&key), None);
        }
        assert_eq!(
            (
                counter::thread_allocations() - allocs,
                counter::thread_bytes() - bytes
            ),
            (0, 0),
            "get key lookup allocated at key length {len}"
        );
        drop(data);

        // The public `get` still allocates its `Option` in the GC heap,
        // which in turn asks Rust for space. That stays under one Rust
        // allocation per call, so no per-call owned key hides in it.
        let allocs = counter::thread_allocations();
        let bytes = counter::thread_bytes();
        for _ in 0..CALLS {
            assert_eq!(
                // SAFETY: The key matches the live map layout; any string key is current
                // through the lookup.
                opt_payload(unsafe { willow_map_get(map, hit as i64, 1, 0) }),
                len as i64
            );
            assert_eq!(
                // SAFETY: The key matches the live map layout; any string key is current
                // through the lookup.
                opt_tag(unsafe { willow_map_get(map, miss as i64, 1, 0) }),
                1
            );
        }
        let get_allocs = counter::thread_allocations() - allocs;
        let get_bytes = counter::thread_bytes() - bytes;
        assert!(
            get_allocs < 2 * CALLS,
            "get allocated {get_allocs} times for {} calls at key length {len}",
            2 * CALLS
        );
        get_bytes_by_len.push((len, get_bytes));
        // SAFETY: The removed shadow-stack entries belong to this operation and their
        // slot storage is still live.
        unsafe { willow_pop_roots(2) };
    }
    // Copying each looked-up key would put `calls * len` bytes through the
    // allocator; the GC's own bookkeeping grows with the live heap instead,
    // so the totals stay far below that and the growth between the
    // shortest and longest key does too.
    let (long_len, long_bytes) = *get_bytes_by_len.last().expect("one length");
    assert!(
        long_bytes < 2 * CALLS * long_len,
        "{long_bytes} bytes for {} lookups of a {long_len}-byte key",
        2 * CALLS
    );
    let (short_len, short_bytes) = get_bytes_by_len[0];
    // Compare without subtracting: GC bookkeeping can make the short run
    // allocate slightly more than the long one.
    assert!(
        long_bytes < short_bytes + CALLS * (long_len - short_len),
        "get bytes went {short_bytes} -> {long_bytes} from key length {short_len} to {long_len}"
    );
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_roots(1) };
}
