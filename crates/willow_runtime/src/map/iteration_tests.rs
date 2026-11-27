use super::*;
use crate::gc::*;

#[test]
fn remove_reuses_slots_and_snapshots_sort_all_storage_paths() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    for confined in [false, true] {
        for string_key in [false, true] {
            let map = GcRootHandle::new(map_new(if string_key { 3 } else { 0 }, 0, 0, confined));
            for count in [8, 32, 128, 512] {
                for round in 0..4 {
                    for key in (0..count).rev() {
                        let word = if string_key {
                            crate::string::willow_string_from_str(&format!("{key:04}")) as i64
                        } else {
                            key
                        };
                        // SAFETY: The constructor and reference flags agree. Each pointer
                        // word comes from a live fixture object at the call.
                        unsafe {
                            willow_map_insert(map.load(), word, i64::from(string_key), key, 0)
                        };
                    }
                    // SAFETY: The root supplies the current map; this snapshot selects
                    // ordinary single-word values.
                    let snapshot =
                        GcRootHandle::new(unsafe { willow_map_snapshot(map.load(), 1, 0) });
                    for key in 0..count {
                        assert_eq!(
                            // SAFETY: The root supplies the current live array for this
                            // access.
                            unsafe { crate::array::willow_array_get(snapshot.load(), key) },
                            key
                        );
                        let word = if string_key {
                            crate::string::willow_string_from_str(&format!("{key:04}")) as i64
                        } else {
                            key
                        };
                        let mut out = -1;
                        assert_eq!(
                            // SAFETY: The fixture supplies the live map and a key of its
                            // declared kind; the output borrows a disjoint local i64.
                            unsafe {
                                willow_map_remove_into(
                                    map.load(),
                                    word,
                                    i64::from(string_key),
                                    &mut out,
                                )
                            },
                            0
                        );
                        assert_eq!(out, key);
                        assert_eq!(
                            // SAFETY: The fixture supplies the live map and a key of its
                            // declared kind; the output borrows a disjoint local i64.
                            unsafe {
                                willow_map_remove_into(
                                    map.load(),
                                    word,
                                    i64::from(string_key),
                                    &mut out,
                                )
                            },
                            1
                        );
                        assert_eq!(out, 0);
                    }
                    // SAFETY: The handle is a live map from this fixture or its frozen
                    // copy.
                    assert_eq!(unsafe { willow_map_len(map.load()) }, 0);
                    if string_key {
                        // SAFETY: The handle identifies a live, GC word-aligned MapStorage
                        // initialized by the constructor.
                        let data = unsafe { map_data(map.load()) };
                        assert!(data.values.is_empty(), "round {round}");
                        assert_eq!(data.key_bytes, 0);
                        assert!(data.values.iter().all(|v| *v == 0));
                    }
                }
            }
        }
    }
}

#[test]
fn removed_holes_copy_and_reinsert_are_independent() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    let map = GcRootHandle::new(willow_map_new(3, 0, 0));
    for text in ["a", "b", "c"] {
        let key = crate::string::willow_string_from_str(text);
        // SAFETY: The constructor and reference flags agree. Each pointer word comes from
        // a live fixture object at the call.
        unsafe { willow_map_insert(map.load(), key as i64, 1, 7, 0) };
    }
    let key = GcRootHandle::new(crate::string::willow_string_from_str("b"));
    let mut out = 0;
    // SAFETY: The fixture supplies the live map and a key of its declared kind; the
    // output borrows a disjoint local i64.
    unsafe { willow_map_remove_into(map.load(), key.load() as i64, 1, &mut out) };
    // SAFETY: The source is a live map from the constructor and remains reachable while
    // copied.
    let copy = GcRootHandle::new(unsafe { willow_map_copy(map.load()) });
    // SAFETY: The constructor and reference flags agree. Each pointer word comes from a
    // live fixture object at the call.
    unsafe { willow_map_insert(copy.load(), key.load() as i64, 1, 9, 0) };
    assert_eq!(
        // SAFETY: The key matches the live map layout; any string key is current through the
        // lookup.
        unsafe { willow_map_contains(map.load(), key.load() as i64, 1) },
        0
    );
    assert_eq!(
        // SAFETY: The fixture supplies the live map and a key of its declared kind; the
        // output borrows a disjoint local i64.
        unsafe { willow_map_get_into(copy.load(), key.load() as i64, 1, &mut out) },
        0
    );
    assert_eq!(out, 9);
}

#[test]
fn snapshot_comparison_and_compaction_counts_scale() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    for count in [64_i64, 256, 1024, 4096] {
        let map = GcRootHandle::new(willow_map_new(0, 3, 1));
        let value = GcRootHandle::new(crate::string::willow_string_from_str("live"));
        for key in 0..count {
            // SAFETY: The constructor and reference flags agree. Each pointer word comes
            // from a live fixture object at the call.
            unsafe { willow_map_insert(map.load(), key, 0, value.load() as i64, 1) };
        }
        SNAPSHOT_COMPARISONS.with(|n| n.set(0));
        // SAFETY: The root supplies the current map; this snapshot selects ordinary
        // single-word values.
        let snapshot = GcRootHandle::new(unsafe { willow_map_snapshot(map.load(), 0, 0) });
        assert_eq!(
            // SAFETY: The root supplies the current live array for this access.
            unsafe { crate::array::array_len(snapshot.load()) },
            count
        );
        let comparisons = SNAPSHOT_COMPARISONS.with(|n| n.get());
        assert!(comparisons < 4 * count as usize * count.ilog2() as usize);
        COMPACT_MOVES.with(|n| n.set(0));
        for key in 0..count {
            let mut out = 0;
            // SAFETY: The fixture supplies the live map and a key of its declared kind;
            // the output borrows a disjoint local i64.
            unsafe { willow_map_remove_into(map.load(), key, 0, &mut out) };
        }
        let moved = COMPACT_MOVES.with(|n| n.get());
        assert!(moved < count as usize);
        // SAFETY: The handle identifies a live, GC word-aligned MapStorage initialized by the
        // constructor.
        assert!(unsafe { map_data(map.load()) }.values.is_empty());
        println!("entries={count} comparisons={comparisons} compact_moves={moved}");
    }
}

#[test]
fn remove_barrier_clears_reference_slots_and_snapshot_roots_survive_gc() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    let map = GcRootHandle::new(willow_map_new(0, 3, 1));
    for key in 0..128 {
        let value = crate::string::willow_string_from_str(&format!("value-{key}"));
        // SAFETY: The constructor and reference flags agree. Each pointer word comes from
        // a live fixture object at the call.
        unsafe { willow_map_insert(map.load(), key, 0, value as i64, 1) };
    }
    // SAFETY: The root supplies the current map; this snapshot selects ordinary single-
    // word values.
    let array = GcRootHandle::new(unsafe { willow_map_snapshot(map.load(), 1, 0) });
    for key in 0..128 {
        let mut out = 0;
        assert_eq!(
            // SAFETY: The fixture supplies the live map and a key of its declared kind; the
            // output borrows a disjoint local i64.
            unsafe { willow_map_remove_into(map.load(), key, 0, &mut out) },
            0
        );
    }
    let mut children = Vec::new();
    // SAFETY: The callback receives the live map payload selected by its registered layout.
    unsafe {
        snapshot_map(map.load(), &mut children);
    }
    assert!(children.iter().all(|child| child.is_null()));
    willow_gc_collect();
    for key in 0..128 {
        // SAFETY: The root supplies the current live array for this access.
        let value = unsafe { crate::array::willow_array_get(array.load(), key) };
        assert_eq!(
            // SAFETY: The operand is null or a live string, and no safepoint intervenes in
            // this borrow.
            unsafe { willow_string_as_bytes(value as *const u8) },
            format!("value-{key}").as_bytes()
        );
    }
}
