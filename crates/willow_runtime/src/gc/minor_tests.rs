//! Deterministic tracing-work checks for the minor-collector extraction.

use super::{MinorCollector, MinorRoots, PinSource, verify_no_pin_enabled};

#[test]
fn minor_registry_active_collection_retains_trace_during_registration() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static OLD_TRACES: AtomicUsize = AtomicUsize::new(0);
    static NEW_TRACES: AtomicUsize = AtomicUsize::new(0);
    unsafe fn old_trace(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
        OLD_TRACES.fetch_add(1, Ordering::Relaxed);
        slots.push(payload.cast());
    }
    unsafe fn new_trace(_: *mut u8, _: &mut Vec<*mut *mut u8>) {
        NEW_TRACES.fetch_add(1, Ordering::Relaxed);
    }
    let _guard = runtime_test_guard();
    reset_internal();
    OLD_TRACES.store(0, Ordering::Relaxed);
    NEW_TRACES.store(0, Ordering::Relaxed);
    crate::gc::willow_register_type(123_456, old_trace);
    let mut tls = tlab_state_for_test();
    let child = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    let mut root = willow_gc_alloc_slow(&mut tls, 1, 123_456, 8, 0);
    assert!(!child.is_null() && !root.is_null());
    // SAFETY: root owns one initialized pointer slot, exposed by old_trace.
    unsafe { *root.cast::<*mut u8>() = child };
    let registry = minor_registry_snapshot();
    let mut state = runtime().heap.lock().unwrap();
    retire_all_tlabs_unindexed(&mut state);
    let mut stop = Default::default();
    let collector = MinorCollector::new(&mut state, registry, &mut stop);
    std::thread::spawn(|| crate::gc::willow_register_type(123_456, new_trace))
        .join()
        .unwrap();
    let (_, work) = collector.run(
        MinorRoots {
            slots: vec![&raw mut root],
            values: Vec::new(),
        },
        Default::default(),
    );
    assert_eq!(OLD_TRACES.load(Ordering::Relaxed), 1);
    assert_eq!(NEW_TRACES.load(Ordering::Relaxed), 0);
    assert_eq!(work.marked_bytes, (2 * (GC_HEADER_SIZE + 8)) as u64);
    drop(state);
    reset_internal();
}

#[test]
fn minor_registry_active_collection_retains_drop_during_registration() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static OLD_DROPS: AtomicUsize = AtomicUsize::new(0);
    static NEW_DROPS: AtomicUsize = AtomicUsize::new(0);
    unsafe fn old_drop(_: *mut u8) {
        OLD_DROPS.fetch_add(1, Ordering::Relaxed);
    }
    unsafe fn new_drop(_: *mut u8) {
        NEW_DROPS.fetch_add(1, Ordering::Relaxed);
    }
    let _guard = runtime_test_guard();
    reset_internal();
    OLD_DROPS.store(0, Ordering::Relaxed);
    NEW_DROPS.store(0, Ordering::Relaxed);
    crate::gc::willow_register_drop(123_456, old_drop);
    let mut tls = tlab_state_for_test();
    assert!(!willow_gc_alloc_slow(&mut tls, 1, 123_456, 8, 0).is_null());
    let registry = minor_registry_snapshot();
    let mut state = runtime().heap.lock().unwrap();
    retire_all_tlabs_unindexed(&mut state);
    let mut stop = Default::default();
    let collector = MinorCollector::new(&mut state, registry, &mut stop);
    // Deterministically pause the collection after capture while a mutator
    // replaces the callback. Its finalization must still use the old snapshot.
    std::thread::spawn(|| crate::gc::willow_register_drop(123_456, new_drop))
        .join()
        .unwrap();
    collector.run(MinorRoots::default(), Default::default());
    assert_eq!(OLD_DROPS.load(Ordering::Relaxed), 1);
    assert_eq!(NEW_DROPS.load(Ordering::Relaxed), 0);
    assert!(std::ptr::fn_addr_eq(
        minor_registry_snapshot().drops[&123_456],
        new_drop as crate::gc::DropFn
    ));
    drop(state);
    reset_internal();
}

#[test]
fn scoped_lease_policy_visits_scale_with_roots_plus_loans() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    reset_internal();
    for leases in [1, 16, 256, 4096] {
        for fanout in [1, 8] {
            let mut roots = MinorRoots::default();
            for index in 1..=leases {
                // Policy never dereferences these opaque addresses: every
                // unsafe alias is covered by an explicit loan before the
                // young-generation query. Large spacing models fragmentation.
                let value = (index * 32768) as *mut u8;
                roots.values.extend(std::iter::repeat_n(
                    (value, PinSource::RuntimeCodeStack),
                    fanout,
                ));
                roots.values.push((value, PinSource::ScopedLoan));
            }
            let mut state = runtime().heap.lock().unwrap();
            assert!(!super::should_defer_minor(&mut state, &roots));
            let visits = super::LEASE_POLICY_VISITS.get();
            assert_eq!(visits, 2 * roots.values.len());
            eprintln!(
                "leases={leases} aliases={} policy_visits={visits}",
                leases * fanout
            );
        }
    }
    reset_internal();
}
use crate::gc::{
    GC_HEADER_SIZE, minor_registry_snapshot, reset_internal, retire_all_tlabs_unindexed, runtime,
    runtime_test_guard, tlab_state_for_test, type_registry, verify_old_region_metadata,
    willow_gc_alloc_slow,
};

#[test]
fn minor_tracing_work_scales_with_graph_and_roots() {
    let _guard = runtime_test_guard();
    for shape in ["chain", "fanout", "shared", "duplicate_roots"] {
        for n in [32usize, 128, 512, 1024] {
            reset_internal();
            let mut tls = tlab_state_for_test();
            let mut nodes = Vec::with_capacity(n);
            for _ in 0..n {
                let node = willow_gc_alloc_slow(&mut tls, 1, 0, 16, 0b11);
                assert!(!node.is_null());
                nodes.push(node);
                // Explicit slow-path calls retire the previous chunk. Mix
                // unreachable chunks with survivors to exercise swap_remove
                // cleanup while the root's sparse chunk remains pinned.
                assert!(!willow_gc_alloc_slow(&mut tls, 1, 0, 16, 0).is_null());
            }
            for (i, &node) in nodes.iter().enumerate() {
                let children = match shape {
                    "fanout" => [2 * i + 1, 2 * i + 2],
                    "shared" => [i + 1, i + 1],
                    _ => [i + 1, n],
                };
                for (slot, child) in children.into_iter().enumerate() {
                    // SAFETY: each live fixture payload owns two pointer slots;
                    // all graph construction precedes the sole collection.
                    unsafe {
                        *node.cast::<*mut u8>().add(slot) =
                            nodes.get(child).copied().unwrap_or(std::ptr::null_mut());
                    }
                }
            }

            let root_count = if shape == "duplicate_roots" { n } else { 1 };
            let registry = minor_registry_snapshot();
            let mut state = runtime().heap.lock().unwrap();
            retire_all_tlabs_unindexed(&mut state);
            let mut stop = crate::gc_telemetry::stops::StopWorkV2::default();
            let (_, work) = MinorCollector::new(&mut state, registry, &mut stop).run(
                MinorRoots {
                    slots: Vec::new(),
                    values: vec![(nodes[0], PinSource::RuntimeCodeStack); root_count],
                },
                Default::default(),
            );
            // Existing production telemetry counts objects and reference slots,
            // not elapsed time. Duplicate edges/roots must not rescan objects.
            assert_eq!(work.marked_bytes, (n * (GC_HEADER_SIZE + 16)) as u64);
            assert_eq!(work.scanned_bytes, (2 * n * size_of::<usize>()) as u64);
            assert_eq!(
                work.root_scan_bytes,
                (root_count * size_of::<usize>()) as u64
            );
            // The n chunks holding a node are indexed once and swept once;
            // survivor copies land in fresh destination chunks that the sweep
            // skips (willow-jz15.120). The n dead chunks are released without
            // a header walk (willow-8hq4.21), and the young set is not
            // pre-indexed (willow-8hq4.16).
            assert_eq!(stop.metadata_objects, (2 * n) as u64);
            assert_eq!(state.promoted_objects, 1);
            assert_eq!(state.survivor_stats.survivor_copies, (n - 1) as u64);
            assert_eq!(state.moved_objects, (n - 1) as u64);
            assert_eq!(state.total_frees, n as u64);
            verify_old_region_metadata(&state).unwrap();
            eprintln!(
                "minor shape={shape} n={n} roots={root_count} metadata={} marked_bytes={} scanned_bytes={} promoted={} moved={} freed={}",
                stop.metadata_objects,
                work.marked_bytes,
                work.scanned_bytes,
                state.promoted_objects,
                state.moved_objects,
                state.total_frees,
            );
            drop(state);
            let second = collect(vec![nodes[0]; root_count]);
            let state = runtime().heap.lock().unwrap();
            assert_eq!(second.marked_bytes, (n * (GC_HEADER_SIZE + 16)) as u64);
            assert_eq!(state.survivor_stats.tenured_objects, (n - 1) as u64);
            assert_eq!(state.promoted_objects, n as u64);
            assert_eq!(state.moved_objects, (2 * (n - 1)) as u64);
            assert_eq!(state.young_allocated_bytes, 0);
            assert!(state.remembered_set.is_empty());
            eprintln!(
                "tenure shape={shape} n={n} marked_bytes={} scanned_bytes={} copies={} tenured={} moved={}",
                second.marked_bytes,
                second.scanned_bytes,
                state.survivor_stats.survivor_copies,
                state.survivor_stats.tenured_objects,
                state.moved_objects
            );
            drop(state);
            // Reset while the registered TLS storage is still alive.
            reset_internal();
        }
    }
}

fn collect(roots: Vec<*mut u8>) -> crate::gc_telemetry::MarkWork {
    let registry = minor_registry_snapshot();
    let mut state = runtime().heap.lock().unwrap();
    retire_all_tlabs_unindexed(&mut state);
    let remembered = std::mem::take(&mut state.remembered_set);
    state.dirty_cards.clear();
    let (_, work) = MinorCollector::new(&mut state, registry, &mut Default::default()).run(
        MinorRoots {
            slots: Vec::new(),
            values: roots
                .into_iter()
                .map(|root| (root, PinSource::RuntimeCodeStack))
                .collect(),
        },
        remembered,
    );
    verify_old_region_metadata(&state).unwrap();
    crate::gc::verify_remembered_set(&state, &type_registry().lock().unwrap()).unwrap();
    work
}

#[test]
fn survivor_remembered_edge_tenures_without_root_or_mutator_store() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    reset_internal();
    let mut tls = tlab_state_for_test();
    let child = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    let owner = willow_alloc_typed(8, 1);
    unsafe {
        *owner.cast::<*mut u8>() = child;
        *child.cast::<u64>() = 42;
    }
    collect(vec![owner]);
    let survivor = unsafe { *owner.cast::<*mut u8>() };
    assert_ne!(child, survivor);
    assert_eq!(unsafe { (*payload_to_header(survivor)).age }, 1);
    assert_eq!(
        unsafe { (*payload_to_header(survivor)).generation },
        GC_GENERATION_YOUNG
    );
    assert_eq!(willow_gc_survivor_copies(), 1);
    assert_eq!(willow_gc_promoted_objects(), 0);
    assert_eq!(willow_gc_remembered_set_size(), 1);
    assert_eq!(willow_gc_dirty_card_count(), 1);
    // No root and no new barrier: only the remembered owner keeps child alive.
    collect(vec![]);
    let tenured = unsafe { *owner.cast::<*mut u8>() };
    assert_ne!(survivor, tenured);
    assert_eq!(unsafe { *tenured.cast::<u64>() }, 42);
    assert_eq!(
        unsafe { (*payload_to_header(tenured)).generation },
        GC_GENERATION_OLD
    );
    assert_eq!(unsafe { (*payload_to_header(tenured)).age }, 0);
    assert_eq!(willow_gc_tenured_objects(), 1);
    assert_eq!(willow_gc_promoted_objects(), 1);
    assert_eq!(willow_gc_moved_objects(), 2);
    assert_eq!(willow_gc_survivor_space_reserved(), 0);
    assert_eq!(willow_gc_remembered_set_size(), 0);
    assert_eq!(willow_gc_dirty_card_count(), 0);
    reset_internal();
}

#[test]
fn survivor_dies_young_and_drop_hook_runs_exactly_once() {
    use crate::gc::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    unsafe fn drop_child(_: *mut u8) {
        DROPS.fetch_add(1, Ordering::Relaxed);
    }
    let _guard = runtime_test_guard();
    reset_internal();
    DROPS.store(0, Ordering::Relaxed);
    willow_register_drop(98761, drop_child);
    let mut tls = tlab_state_for_test();
    let child = willow_gc_alloc_slow(&mut tls, 1, 98761, 8, 0);
    let owner = willow_alloc_typed(8, 1);
    unsafe {
        *owner.cast::<*mut u8>() = child;
    }
    collect(vec![owner]);
    assert_eq!(DROPS.load(Ordering::Relaxed), 0);
    unsafe {
        *owner.cast::<*mut u8>() = std::ptr::null_mut();
    }
    collect(vec![]);
    collect(vec![]);
    assert_eq!(DROPS.load(Ordering::Relaxed), 1);
    assert_eq!(willow_gc_promoted_objects(), 0);
    assert_eq!(willow_gc_survivor_space_live(), 0);
    assert_eq!(runtime().heap.lock().unwrap().young_allocated_bytes, 0);
    reset_internal();
}

#[test]
fn survivor_direct_root_and_budget_fallback_promote_in_place() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    for limit in [false, true] {
        reset_internal();
        let mut tls = tlab_state_for_test();
        let child = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
        let owner = willow_alloc_typed(8, 1);
        unsafe {
            *owner.cast::<*mut u8>() = child;
        }
        if limit {
            let mut state = runtime().heap.lock().unwrap();
            state.memory_limit_bytes = Some(state.tlab_reserved_bytes + state.old_reserved_bytes);
        }
        collect(vec![owner]);
        let survivor = unsafe { *owner.cast::<*mut u8>() };
        if limit {
            assert_eq!(survivor, child);
            assert_eq!(willow_gc_survivor_copies(), 0);
            let state = runtime().heap.lock().unwrap();
            assert_eq!(
                state.survivor_stats.pin_reasons[PinSource::MemoryBudget as usize],
                1
            );
            assert_eq!(
                state.survivor_stats.active_pin_bytes,
                (GC_HEADER_SIZE + 8) as u64
            );
            assert!(
                state.tlab_reserved_bytes + state.old_reserved_bytes
                    <= state.memory_limit_bytes.unwrap()
            );
        } else {
            collect(vec![survivor]);
            assert_eq!(unsafe { *owner.cast::<*mut u8>() }, survivor);
        }
        assert_eq!(
            unsafe { (*payload_to_header(survivor)).generation },
            GC_GENERATION_OLD
        );
        assert_eq!(willow_gc_pinned_promotions(), 1);
        assert_eq!(willow_gc_tenured_objects(), 0);
        reset_internal();
    }
}

#[test]
fn major_collection_traces_and_reclaims_survivor_storage() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    reset_internal();
    let mut tls = tlab_state_for_test();
    let child = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    let mut owner = willow_alloc_typed(8, 1);
    unsafe {
        *owner.cast::<*mut u8>() = child;
        *child.cast::<u64>() = 73;
    }
    collect(vec![owner]);
    willow_push_root(&mut owner);
    willow_gc_collect();
    let survivor = unsafe { *owner.cast::<*mut u8>() };
    assert_eq!(unsafe { *survivor.cast::<u64>() }, 73);
    assert_eq!(unsafe { (*payload_to_header(survivor)).age }, 1);
    assert!(willow_gc_survivor_space_live() > 0);
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    assert_eq!(willow_gc_survivor_space_reserved(), 0);
    reset_internal();
}

#[test]
fn minor_metadata_skips_pinned_chunks_across_repeated_collections() {
    use crate::gc::{RegionKind, willow_alloc_typed};
    let _guard = runtime_test_guard();
    for n in [32usize, 128, 512] {
        for young in [0usize, 4] {
            reset_internal();
            let mut tls = tlab_state_for_test();
            let roots = (0..n)
                .map(|_| willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0))
                .collect();
            collect(roots);
            // An old owner keeps a fixed young population alive through copying
            // and tenuring. Pinned chunks themselves are deliberately unrooted:
            // only a major collection may reclaim them.
            let owner = willow_alloc_typed((young * size_of::<usize>()) as i64, (1 << young) - 1);
            for cycle in 0..6 {
                if cycle % 2 == 0 {
                    for slot in 0..young {
                        // Interleave dead source chunks to exercise swap removal
                        // and address-index remapping around retained chunks.
                        assert!(!willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0).is_null());
                        let child = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
                        unsafe { *owner.cast::<*mut u8>().add(slot) = child };
                    }
                }
                let mut state = runtime().heap.lock().unwrap();
                retire_all_tlabs_unindexed(&mut state);
                let remembered = std::mem::take(&mut state.remembered_set);
                state.dirty_cards.clear();
                let mut stop = crate::gc_telemetry::stops::StopWorkV2::default();
                let roots = if young == 0 { vec![] } else { vec![owner] };
                let (_, work) = MinorCollector::new(&mut state, Default::default(), &mut stop).run(
                    MinorRoots {
                        slots: Vec::new(),
                        values: roots
                            .into_iter()
                            .map(|root| (root, PinSource::RuntimeCodeStack))
                            .collect(),
                    },
                    remembered,
                );
                // Child chunks are indexed and swept; survivor copies sit in
                // destination chunks the sweep skips (willow-jz15.120); dead
                // chunks are released unwalked (willow-8hq4.21).
                let expected = if cycle % 2 == 0 { 2 * young } else { young };
                assert_eq!(stop.metadata_objects, expected as u64);
                if young == 0 {
                    assert_eq!(work.marked_bytes, 0);
                }
                assert_eq!(
                    state
                        .tlab_chunks
                        .iter()
                        .filter(|c| c.kind == RegionKind::Pinned)
                        .count(),
                    n
                );
                for (index, chunk) in state.tlab_chunks.iter().enumerate() {
                    assert_eq!(state.tlab_addresses.exact(chunk.base as usize), Some(index));
                }
                assert_eq!(
                    state.survivor_stats.survivor_space_live,
                    if cycle % 2 == 0 {
                        (young * (GC_HEADER_SIZE + 8)) as u64
                    } else {
                        0
                    }
                );
                verify_old_region_metadata(&state).unwrap();
                eprintln!(
                    "pinned-metadata n={n} young={young} cycle={cycle} chunks={} metadata={}",
                    state.tlab_chunks.len(),
                    stop.metadata_objects
                );
            }
            reset_internal();
        }
    }
}

#[test]
fn tenured_parent_remembers_younger_cycle_and_shared_child() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    reset_internal();
    let mut tls = tlab_state_for_test();
    let parent = willow_gc_alloc_slow(&mut tls, 1, 0, 16, 0b11);
    let owner = willow_alloc_typed(8, 1);
    unsafe {
        *owner.cast::<*mut u8>() = parent;
    }
    collect(vec![owner]);
    let parent = unsafe { *owner.cast::<*mut u8>() };
    let child = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 1);
    // A cycle with two equivalent edges, and two different age cohorts.
    unsafe {
        *parent.cast::<*mut u8>() = child;
        *parent.cast::<*mut u8>().add(1) = child;
        *child.cast::<*mut u8>() = parent;
    }
    collect(vec![]);
    let parent = unsafe { *owner.cast::<*mut u8>() };
    let child = unsafe { *parent.cast::<*mut u8>() };
    assert_eq!(unsafe { *parent.cast::<*mut u8>().add(1) }, child);
    assert_eq!(unsafe { *child.cast::<*mut u8>() }, parent);
    assert_eq!(
        unsafe { (*payload_to_header(parent)).generation },
        GC_GENERATION_OLD
    );
    assert_eq!(
        unsafe { (*payload_to_header(child)).generation },
        GC_GENERATION_YOUNG
    );
    assert_eq!(willow_gc_tenured_objects(), 1);
    assert_eq!(willow_gc_survivor_copies(), 2);
    collect(vec![]);
    let child = unsafe { *parent.cast::<*mut u8>() };
    assert_eq!(unsafe { *parent.cast::<*mut u8>().add(1) }, child);
    assert_eq!(unsafe { *child.cast::<*mut u8>() }, parent);
    assert_eq!(willow_gc_tenured_objects(), 2);
    assert_eq!(willow_gc_remembered_set_size(), 0);
    reset_internal();
}

#[test]
fn tenure_budget_failure_retains_survivor_chunk_as_pinned() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    reset_internal();
    let mut tls = tlab_state_for_test();
    let child = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    let owner = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 1);
    unsafe {
        *owner.cast::<*mut u8>() = child;
    }
    collect(vec![owner]);
    let survivor = unsafe { *owner.cast::<*mut u8>() };
    {
        let mut state = runtime().heap.lock().unwrap();
        assert!(state.old_regions.is_empty());
        state.memory_limit_bytes = Some(state.tlab_reserved_bytes);
    }
    collect(vec![]);
    assert_eq!(unsafe { *owner.cast::<*mut u8>() }, survivor);
    assert_eq!(
        unsafe { (*payload_to_header(survivor)).generation },
        GC_GENERATION_OLD
    );
    assert_eq!(willow_gc_survivor_copies(), 1);
    assert_eq!(willow_gc_pinned_promotions(), 2);
    assert_eq!(willow_gc_tenured_objects(), 0);
    assert_eq!(willow_gc_survivor_space_reserved(), 0);
    assert_eq!(willow_gc_pinned_region_count(), 2);
    reset_internal();
}

/// Membership comes from the chunk index and start bitmap (willow-8hq4.16):
/// only exact payload starts of young objects in pre-cycle chunks qualify.
#[test]
fn young_source_accepts_only_exact_pre_cycle_young_payloads() {
    let _guard = runtime_test_guard();
    reset_internal();
    let mut tls = tlab_state_for_test();
    let young = willow_gc_alloc_slow(&mut tls, 1, 0, 16, 0);
    assert!(!young.is_null());
    let mut state = runtime().heap.lock().unwrap();
    retire_all_tlabs_unindexed(&mut state);
    let mut stop = crate::gc_telemetry::stops::StopWorkV2::default();
    let mut collector = MinorCollector::new(&mut state, Default::default(), &mut stop);
    let address = young as usize;
    assert!(collector.young_source(address).is_some());
    assert!(collector.young_source(address + 8).is_none(), "interior");
    assert!(
        collector.young_source(address - GC_HEADER_SIZE).is_none(),
        "header"
    );
    assert!(
        collector.young_source(GC_HEADER_SIZE).is_none(),
        "outside heap"
    );
    let copy = collector.evacuate(young);
    assert_ne!(copy, young);
    assert!(
        collector.young_source(copy as usize).is_none(),
        "destinations are never sources"
    );
    assert_eq!(collector.evacuate(young), copy, "forwarded exactly once");
    drop(collector);
    drop(state);
    reset_internal();
}

#[test]
fn remembered_header_flag_mirrors_remembered_set_membership() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    reset_internal();
    let remembered = |payload: *mut u8| unsafe { (*payload_to_header(payload)).remembered };
    let mut tls = tlab_state_for_test();
    let child = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    let other_old = willow_alloc_typed(8, 0);
    let owner = willow_alloc_typed(8, 1);
    assert_eq!(remembered(owner), 0, "fresh old objects start unremembered");
    // An old-to-old edge does not remember the owner.
    willow_gc_write_barrier(
        owner,
        std::ptr::null_mut(),
        other_old,
        GcStoreDestination::ObjectField as i64,
    );
    assert_eq!(remembered(owner), 0);
    assert_eq!(willow_gc_remembered_set_size(), 0);
    // An old-to-young edge sets the flag together with the set entry.
    willow_gc_write_barrier(
        owner,
        other_old,
        child,
        GcStoreDestination::ObjectField as i64,
    );
    unsafe { *owner.cast::<*mut u8>() = child };
    assert_eq!(remembered(owner), 1);
    assert_eq!(willow_gc_remembered_set_size(), 1);
    // A repeated store keeps one entry and the flag.
    willow_gc_write_barrier(owner, child, child, GcStoreDestination::ObjectField as i64);
    assert_eq!(willow_gc_remembered_set_size(), 1);
    // The survivor copy is still young: the collector re-remembers the owner.
    collect(vec![]);
    assert_eq!(remembered(owner), 1);
    assert_eq!(willow_gc_remembered_set_size(), 1);
    // Tenuring the child removes the entry and clears the flag.
    collect(vec![]);
    assert_eq!(
        unsafe { (*payload_to_header(*owner.cast::<*mut u8>())).generation },
        GC_GENERATION_OLD
    );
    assert_eq!(remembered(owner), 0);
    assert_eq!(willow_gc_remembered_set_size(), 0);
    reset_internal();
}

#[test]
fn runtime_object_owner_filter_calls_barrier_only_for_new_young_edges() {
    // willow-jz15.51: runtime container stores skip the locked barrier
    // exactly when the generated inline filter would.
    use crate::gc::*;
    let _guard = runtime_test_guard();
    reset_internal();
    let calls = || {
        runtime()
            .write_barrier_calls
            .load(std::sync::atomic::Ordering::Relaxed)
    };
    let store = |owner: *mut u8, value: *mut u8| {
        write_barrier_object_owner(
            owner,
            std::ptr::null_mut(),
            value,
            GcStoreDestination::MapValue as i64,
        )
    };
    let mut tls = tlab_state_for_test();
    let young = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    let young_owner = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    let old_value = willow_alloc_typed(8, 0);
    let owner = willow_alloc_typed(8, 1);
    let before = calls();
    store(owner, old_value);
    store(owner, std::ptr::null_mut());
    store(std::ptr::null_mut(), young);
    store(young_owner, young);
    assert_eq!(calls(), before, "old, null and young-owner stores skip");
    assert_eq!(willow_gc_remembered_set_size(), 0);
    store(owner, young);
    assert_eq!(
        calls(),
        before + 1,
        "a new old-to-young edge takes the barrier"
    );
    assert_eq!(willow_gc_remembered_set_size(), 1);
    store(owner, young);
    assert_eq!(calls(), before + 1, "a remembered owner skips");
    // Active marking always reaches the barrier, even for old values.
    crate::gc::barrier::GC_MARK_PHASE.store(1, std::sync::atomic::Ordering::Release);
    store(owner, old_value);
    crate::gc::barrier::GC_MARK_PHASE.store(0, std::sync::atomic::Ordering::Release);
    assert_eq!(calls(), before + 2);
    reset_internal();
}

#[test]
fn in_place_promotion_clears_stale_remembered_byte() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    reset_internal();
    let mut tls = tlab_state_for_test();
    let pinned = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    // Generated TLAB allocation does not write the remembered byte, so a
    // recycled chunk can leave a stale nonzero value in a young header.
    unsafe { (*payload_to_header(pinned)).remembered = 1 };
    // A direct root is promoted in place; the verifier in `collect` checks
    // that the old object's flag matches remembered-set membership.
    collect(vec![pinned]);
    assert_eq!(
        unsafe { (*payload_to_header(pinned)).generation },
        GC_GENERATION_OLD
    );
    assert_eq!(unsafe { (*payload_to_header(pinned)).remembered }, 0);
    assert_eq!(willow_gc_remembered_set_size(), 0);
    reset_internal();
}

#[test]
fn verify_no_pin_accepts_only_non_empty_non_zero_values() {
    use std::ffi::OsStr;
    assert!(!verify_no_pin_enabled(None));
    assert!(!verify_no_pin_enabled(Some(OsStr::new(""))));
    assert!(!verify_no_pin_enabled(Some(OsStr::new("0"))));
    assert!(verify_no_pin_enabled(Some(OsStr::new("1"))));
    assert!(verify_no_pin_enabled(Some(OsStr::new("yes"))));
}

/// Fill `chunks` TLAB chunks with `per_chunk` objects each: the slow-path
/// first object of `first_type`, then generated-style fast allocations of
/// `fast_type`. Returns every payload; the last chunk stays active.
fn fill_chunks(
    tls: &mut crate::gc::GcTlabState,
    chunks: usize,
    per_chunk: usize,
    first_type: impl Fn(usize) -> u32,
    fast_type: u32,
) -> Vec<*mut u8> {
    use crate::gc::*;
    use std::sync::atomic::Ordering;
    // Keep the slow path from collecting the fixture before it is complete.
    runtime().heap.lock().unwrap().nursery_threshold_bytes = usize::MAX;
    let size = GC_HEADER_SIZE + 8;
    let mut payloads = Vec::with_capacity(chunks * per_chunk);
    for chunk in 0..chunks {
        payloads.push(willow_gc_alloc_slow(
            tls,
            1,
            i64::from(first_type(chunk)),
            8,
            0,
        ));
        for _ in 1..per_chunk {
            let cursor = tls.cursor.load(Ordering::Acquire);
            initialize_object_at(cursor as *mut u8, size, fast_type, 1, 0).unwrap();
            publish_tlab_start_for_test(tls, cursor as *mut u8);
            tls.cursor.store(cursor + size, Ordering::Release);
            payloads.push((cursor + GC_HEADER_SIZE) as *mut u8);
        }
    }
    payloads
}

/// Run one minor collection over the production (unindexed) retirement and
/// return its stop counters.
fn collect_with_stop(slots: Vec<*mut *mut u8>) -> crate::gc_telemetry::stops::StopWorkV2 {
    let registry = minor_registry_snapshot();
    let mut state = runtime().heap.lock().unwrap();
    retire_all_tlabs_unindexed(&mut state);
    let mut stop = crate::gc_telemetry::stops::StopWorkV2::default();
    MinorCollector::new(&mut state, registry, &mut stop).run(
        MinorRoots {
            slots,
            values: Vec::new(),
        },
        Default::default(),
    );
    verify_old_region_metadata(&state).unwrap();
    stop
}

/// willow-8hq4.21: a nursery chunk without survivors or droppable objects is
/// released without visiting a header, so minor metadata work is independent
/// of its object count; a chunk with one survivor is indexed and walked once.
#[test]
fn fully_dead_nursery_chunks_are_released_without_header_walks() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    let size = GC_HEADER_SIZE + 8;
    let chunks = 4;
    for per_chunk in [1, 16, 256, GC_TLAB_CHUNK_SIZE / size] {
        for rooted in [false, true] {
            reset_internal();
            let mut tls = tlab_state_for_test();
            let payloads = fill_chunks(&mut tls, chunks, per_chunk, |_| 0, 0);
            let mut root = payloads[per_chunk / 2];
            let slots = if rooted {
                vec![&raw mut root]
            } else {
                Vec::new()
            };
            let objects = (chunks * per_chunk) as u64;
            let stop = collect_with_stop(slots);
            let state = runtime().heap.lock().unwrap();
            if rooted {
                // The rooted chunk: index + sweep walk. The copy's destination
                // chunk is not swept (willow-jz15.120).
                assert_eq!(stop.metadata_objects, 2 * per_chunk as u64);
                assert_ne!(root, payloads[per_chunk / 2]);
                assert_eq!(state.survivor_stats.survivor_copies, 1);
                assert_eq!(state.young_allocated_bytes, size);
                assert_eq!(state.tlab_chunks.len(), 1);
            } else {
                assert_eq!(stop.metadata_objects, 0, "per_chunk={per_chunk}");
                assert_eq!(state.young_allocated_bytes, 0);
                assert_eq!(state.allocated_bytes, 0);
                assert!(state.tlab_chunks.is_empty());
                assert_eq!(state.tlab_reserved_bytes, 0);
            }
            assert_eq!(stop.swept_objects, objects);
            assert_eq!(state.total_frees, objects - u64::from(rooted));
            assert_eq!(state.released_bytes, (chunks * GC_TLAB_CHUNK_SIZE) as u64);
            eprintln!(
                "dead-chunk per_chunk={per_chunk} rooted={rooted} metadata={} swept={}",
                stop.metadata_objects, stop.swept_objects
            );
            drop(state);
            reset_internal();
        }
    }
}

/// Drop hooks still run exactly once when the dead-chunk shortcut cannot be
/// taken: a droppable slow-path first object, or a drop hook registered for
/// a type that generated code can allocate on the fast path.
#[test]
fn droppable_nursery_chunks_are_walked_and_drop_exactly_once() {
    use crate::gc::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    unsafe fn count_drop(_: *mut u8) {
        DROPS.fetch_add(1, Ordering::Relaxed);
    }
    const GENERATED: u32 = 98_771;
    let native = willow_abi::runtime_type_ids::NETWORK_HANDLE_TYPE_ID;
    let _guard = runtime_test_guard();
    let chunks = 4;
    let per_chunk = 32;
    for generated in [false, true] {
        reset_internal();
        DROPS.store(0, Ordering::Relaxed);
        let mut tls = tlab_state_for_test();
        if generated {
            willow_register_drop(GENERATED, count_drop);
            fill_chunks(&mut tls, chunks, per_chunk, |_| 0, GENERATED);
        } else {
            // Only even chunks start with an object owning a native resource.
            willow_register_drop(native, count_drop);
            fill_chunks(
                &mut tls,
                chunks,
                per_chunk,
                |chunk| if chunk % 2 == 0 { native } else { 0 },
                0,
            );
        }
        let stop = collect_with_stop(Vec::new());
        let (expected_drops, walked_chunks) = if generated {
            (chunks * (per_chunk - 1), chunks)
        } else {
            (chunks / 2, chunks / 2)
        };
        assert_eq!(DROPS.load(Ordering::Relaxed), expected_drops);
        // Each walked chunk is validated once and swept once.
        assert_eq!(
            stop.metadata_objects,
            (2 * walked_chunks * per_chunk) as u64
        );
        assert_eq!(stop.swept_objects, (chunks * per_chunk) as u64);
        {
            let state = runtime().heap.lock().unwrap();
            assert!(state.tlab_chunks.is_empty());
            assert_eq!(state.young_allocated_bytes, 0);
        }
        collect_with_stop(Vec::new());
        assert_eq!(DROPS.load(Ordering::Relaxed), expected_drops);
        reset_internal();
    }
}

/// A chunk retired without its header index is validated when the minor
/// collector first finds a reachable object in it: gaps and overlaps in its
/// start bits are still rejected.
#[test]
fn minor_rejects_unpublished_and_overlapping_start_bits_in_unindexed_chunk() {
    use crate::gc::*;
    use std::sync::atomic::Ordering;
    let _guard = runtime_test_guard();
    for extra_bit in [false, true] {
        reset_internal();
        let mut tls = tlab_state_for_test();
        let mut root = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
        let bytes = GC_HEADER_SIZE + 8;
        let cursor = tls.cursor.load(Ordering::Acquire);
        initialize_object_at(cursor as *mut u8, bytes, 0, 1, 0).unwrap();
        if extra_bit {
            publish_tlab_start_for_test(&tls, cursor as *mut u8);
            publish_tlab_start_for_test(&tls, (cursor + GC_REGION_MARK_GRANULE) as *mut u8);
        }
        tls.cursor.store(cursor + bytes, Ordering::Release);
        let slot = &raw mut root as usize;
        let result = std::panic::catch_unwind(|| {
            let mut state = runtime().heap.lock().unwrap_or_else(|p| p.into_inner());
            retire_all_tlabs_unindexed(&mut state);
            assert!(state.tlab_chunks[0].needs_index());
            let mut stop = crate::gc_telemetry::stops::StopWorkV2::default();
            MinorCollector::new(&mut state, Default::default(), &mut stop).run(
                MinorRoots {
                    slots: vec![slot as *mut *mut u8],
                    values: Vec::new(),
                },
                Default::default(),
            );
        });
        runtime().heap.clear_poison();
        let payload = result.expect_err("inconsistent start bits must be rejected");
        let message = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or_default();
        assert!(message.contains("was not published"), "{message}");
    }
    reset_internal();
}

#[test]
fn persistent_old_roots_only_scan_changed_young_edges() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    for count in [1_000, 10_000, 100_000] {
        reset_internal();
        {
            let mut state = runtime().heap.lock().unwrap();
            state.threshold_bytes = usize::MAX;
            state.pacer.disable_for_test();
        }
        let mut frames = Vec::with_capacity(count);
        for _ in 0..count {
            let frame = willow_alloc_typed(8, 1);
            willow_gc_add_runtime_root(frame);
            frames.push(frame);
        }
        let mut tls = tlab_state_for_test();
        let first = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
        unsafe { *first.cast::<u64>() = 73 };
        let frame = frames[count / 2];
        willow_gc_write_barrier(
            frame,
            std::ptr::null_mut(),
            first,
            GcStoreDestination::ObjectField as i64,
        );
        unsafe { *frame.cast::<*mut u8>() = first };
        let run = || {
            let result = with_stw(
                crate::gc_telemetry::stops::StopReason::Minor,
                super::collect_stopped,
            );
            result.unwrap().2
        };
        // The changed old frame and one leaf survive; every other frame is
        // retained by the major root registry without a per-minor scan.
        for cycle in 1..=2 {
            let work = run();
            assert_eq!(work.root_scan_bytes, 0);
            assert_eq!(work.marked_bytes, 2 * (GC_HEADER_SIZE + 8) as u64);
            assert_eq!(work.scanned_bytes, 8);
            let child = unsafe { *frame.cast::<*mut u8>() };
            assert_eq!(unsafe { *child.cast::<u64>() }, 73);
            eprintln!(
                "persistent_frames={count} cycle={cycle} root_slots=0 scanned_owners=1 scanned_slots=1"
            );
        }
        for cycle in 3..=5 {
            let work = run();
            assert_eq!(work.marked_bytes, 0);
            assert_eq!(work.scanned_bytes, 0);
            eprintln!(
                "persistent_frames={count} cycle={cycle} root_slots=0 scanned_owners=0 scanned_slots=0"
            );
        }
        // Roots registered after nursery activation are conservatively dirty
        // once. Registration work is not repeated on unchanged later cycles.
        let late: Vec<_> = (0..count)
            .map(|_| {
                let frame = willow_alloc_typed(8, 1);
                willow_gc_add_runtime_root(frame);
                frame
            })
            .collect();
        let first_scan = run();
        assert_eq!(
            first_scan.marked_bytes,
            (count * (GC_HEADER_SIZE + 8)) as u64
        );
        assert_eq!(first_scan.remembered_owners, count as u64);
        for _ in 0..3 {
            assert_eq!(run().marked_bytes, 0);
        }
        for frame in late {
            willow_gc_remove_runtime_root(frame);
        }
        // Registration itself retains a freshly initialized old owner's
        // young child, even before the first subsequent barriered mutation.
        let second = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
        unsafe { *second.cast::<u64>() = 91 };
        let initial = willow_alloc_typed(8, 1);
        unsafe { *initial.cast::<*mut u8>() = second };
        willow_gc_add_runtime_root(initial);
        run();
        assert_eq!(unsafe { **initial.cast::<*mut u64>() }, 91);
        willow_gc_remove_runtime_root(initial);
        for frame in frames {
            willow_gc_remove_runtime_root(frame);
        }
        reset_internal();
    }
}

#[test]
fn minor_reference_slot_scratch_reuses_capacity_across_shapes() {
    use crate::gc::*;
    use crate::scheduler::scaling_measurements::counting_allocator as counter;
    let _guard = runtime_test_guard();
    reset_internal();
    let _stress = GcStressTestScope::normal();
    unsafe fn native_trace(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
        slots.push(payload.cast());
        slots.push(std::ptr::null_mut());
    }
    let mut registry = std::collections::HashMap::new();
    registry.insert(123456, native_trace as TraceFn);
    let bitmap = [3_u64, 1, 1_u64 << 63, 1];
    let shapes = [
        (willow_alloc_typed(0, 0), vec![]),
        (willow_alloc_typed(8, 0), vec![]),
        (willow_alloc_typed(8, 1), vec![0]),
        (willow_alloc_typed(512, 1_u64 << 63), vec![63]),
        (willow_alloc_typed(32, 15), vec![0, 1, 2, 3]),
        (willow_alloc_object(123456, 8), vec![0, usize::MAX]),
        (
            willow_gc_alloc_bitmap(0, 129 * 8, bitmap.as_ptr()),
            vec![0, 127, 128],
        ),
    ];
    let mut slots = Vec::with_capacity(8);
    for count in [1, 100, 10000] {
        let before = counter::thread_allocations();
        let mut visited = 0;
        for _ in 0..count {
            for (payload, indices) in &shapes {
                slots.clear();
                let object = HeapObject::from_payload(GcPayload::from_raw(*payload).unwrap());
                append_object_reference_slots(object, &registry, &mut slots);
                assert_eq!(slots.len(), indices.len());
                for (&slot, &index) in slots.iter().zip(indices) {
                    let expected = if index == usize::MAX {
                        std::ptr::null_mut()
                    } else {
                        object.payload_slot(index)
                    };
                    assert_eq!(slot, expected);
                    visited += 1;
                }
            }
        }
        assert_eq!(counter::thread_allocations() - before, 0);
        assert_eq!(visited, count * 11);
        eprintln!(
            "slot_scratch scans={} slots={visited} allocations=0",
            count * shapes.len()
        );
    }
    reset_internal();
}

#[test]
fn minor_collector_keeps_reference_scratch_between_objects() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    reset_internal();
    let _stress = GcStressTestScope::normal();
    unsafe fn native_slots(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
        for index in 0..4 {
            slots.push(unsafe { payload.cast::<*mut u8>().add(index) });
        }
    }
    willow_register_type(123_459, native_slots);
    let first = crate::gc::allocate_old(0, 123_459, 32, 0);
    let second = crate::gc::allocate_old(0, 123_459, 32, 0);
    let registry = minor_registry_snapshot();
    let mut state = runtime().heap.lock().unwrap();
    retire_all_tlabs_unindexed(&mut state);
    let mut stop = Default::default();
    let mut collector = MinorCollector::new(&mut state, registry, &mut stop);
    collector.scan_object(HeapObject::from_payload(
        GcPayload::from_raw(first).unwrap(),
    ));
    let buffer = collector.reference_slots.as_ptr();
    assert!(collector.reference_slots.capacity() >= 4);
    assert!(collector.reference_slots.is_empty());
    collector.scan_object(HeapObject::from_payload(
        GcPayload::from_raw(second).unwrap(),
    ));
    assert_eq!(collector.reference_slots.as_ptr(), buffer);
    assert!(collector.reference_slots.is_empty());
    drop(collector);
    drop(state);
    reset_internal();
}

#[test]
fn promotion_cursor_rekeys_regions_instead_of_each_object() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    for count in [1usize, 1_000, 100_000] {
        reset_internal();
        let _stress = GcStressTestScope::normal();
        let mut source = HeapObject::from_payload(GcPayload::from_raw(willow_alloc(8)).unwrap());
        let registry = minor_registry_snapshot();
        let mut state = runtime().heap.lock().unwrap();
        retire_all_tlabs_unindexed(&mut state);
        let mut stop = Default::default();
        let mut collector = MinorCollector::new(&mut state, registry, &mut stop);
        let mut switches = 0;
        let mut previous = None;
        for _ in 0..count {
            // Repeatedly transfer one descriptor through fresh destinations;
            // payload contents are immaterial to allocation/index ownership.
            source = collector
                .allocate_promotion(source, source.trace_metadata())
                .unwrap();
            let index = collector.promotion_destination.unwrap();
            if previous != Some(index) {
                switches += 1;
                previous = Some(index);
            }
            assert!(
                !collector
                    .state
                    .old_region_candidates
                    .iter()
                    .any(|&(_, id)| id == index)
            );
        }
        assert!(switches <= 1 + count * (GC_HEADER_SIZE + 8) / GC_OLD_REGION_SIZE);
        eprintln!("promotions={count} candidate_regions={switches}");
        // Drop restores a partially filled candidate, including during unwind.
        drop(collector);
        assert!(verify_old_region_metadata(&state).is_ok());
        drop(state);
        reset_internal();
    }
}

#[test]
fn persistent_survival_skips_intermediate_copy_and_churn_restores_it() {
    use crate::gc::{
        GC_GENERATION_OLD, GC_GENERATION_YOUNG, minor_collect_internal, payload_to_header,
        reset_internal_for_test,
    };
    let _guard = runtime_test_guard();
    for count in [16usize, 1_000, 10_000] {
        reset_internal_for_test();
        {
            let mut state = runtime().heap.lock().unwrap();
            state.threshold_bytes = usize::MAX;
            state.pacer_trigger = u64::MAX;
            state.pacer.disable_for_test();
            state.nursery_threshold_bytes = usize::MAX;
        }
        let mut tls = tlab_state_for_test();
        let mut head = std::ptr::null_mut();
        let slot = (&mut head as *mut *mut u8).cast::<u8>();
        runtime().runtime_root_slots.add(slot);
        for round in 0..5 {
            let before = runtime()
                .heap
                .lock()
                .unwrap()
                .survivor_stats
                .survivor_copies;
            for _ in 0..count {
                let node = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 1);
                unsafe {
                    *node.cast::<*mut u8>() = head;
                }
                head = node;
            }
            minor_collect_internal();
            let state = runtime().heap.lock().unwrap();
            assert_eq!(
                state.survivor_stats.survivor_copies - before,
                if round >= 3 { 0 } else { count as u64 }
            );
            assert_eq!(
                unsafe { (*payload_to_header(head)).generation },
                if round >= 3 {
                    GC_GENERATION_OLD
                } else {
                    GC_GENERATION_YOUNG
                }
            );
            verify_old_region_metadata(&state).unwrap();
            drop(state);
            let mut cursor = head;
            for _ in 0..((round + 1) * count) {
                assert!(!cursor.is_null());
                cursor = unsafe { *cursor.cast::<*mut u8>() };
            }
            assert!(cursor.is_null());
        }
        // An allocation wave that dies young resets the policy. No roots or
        // thresholds are weakened to retain those dead objects.
        for _ in 0..count {
            willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
        }
        minor_collect_internal();
        assert_eq!(runtime().heap.lock().unwrap().tenuring.age(), 2);
        runtime().runtime_root_slots.remove(slot);
        reset_internal_for_test();
    }
}

#[test]
fn old_reference_slots_do_not_probe_the_young_page_index() {
    let _guard = runtime_test_guard();
    for count in [1, 1000, 100000] {
        reset_internal();
        let old = crate::gc::allocate_old(0, 0, 8, 0);
        let registry = minor_registry_snapshot();
        let mut state = runtime().heap.lock().unwrap();
        let mut stop = Default::default();
        let mut collector = MinorCollector::new(&mut state, registry, &mut stop);
        super::PAGE_QUERIES.set(0);
        for _ in 0..count {
            let mut slot = old;
            collector.scan_slot(&raw mut slot);
            assert_eq!(slot, old);
        }
        assert_eq!(super::PAGE_QUERIES.get(), 0);
        assert!(collector.worklist.is_empty());
        drop(collector);
        drop(state);
    }
    reset_internal();
}

#[test]
fn fully_forwarded_chunks_release_without_a_second_header_walk() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    let _stress = GcStressTestScope::normal();
    for chunks in [1usize, 16, 256] {
        reset_internal();
        let mut tls = tlab_state_for_test();
        let mut payloads = fill_chunks(&mut tls, chunks, 32, |_| 0, 0);
        for (index, payload) in payloads.iter().enumerate() {
            unsafe {
                *(*payload).cast::<usize>() = index;
            }
        }
        {
            let mut state = runtime().heap.lock().unwrap();
            for _ in 0..3 {
                state.tenuring.complete(1024, 1024);
            }
        }
        let stop = collect_with_stop(payloads.iter_mut().map(|p| p as *mut *mut u8).collect());
        let count = payloads.len() as u64;
        assert_eq!(stop.swept_objects, count);
        // Debug retains the source poison walk; release discards the fully
        // forwarded chunk after its one validation/indexing pass.
        assert_eq!(
            stop.metadata_objects,
            count * if cfg!(debug_assertions) { 2 } else { 1 }
        );
        for (index, payload) in payloads.iter().enumerate() {
            assert_eq!(unsafe { *(*payload).cast::<usize>() }, index);
            assert_eq!(
                unsafe { (*payload_to_header(*payload)).generation },
                GC_GENERATION_OLD
            );
        }
        let state = runtime().heap.lock().unwrap();
        assert_eq!(state.moved_objects, count);
        assert!(state.tlab_chunks.is_empty());
        verify_old_region_metadata(&state).unwrap();
        eprintln!(
            "forwarded_chunks={chunks} objects={count} metadata_objects={}",
            stop.metadata_objects
        );
    }
    reset_internal();
}

#[test]
fn minor_survivor_cost_and_page_index_scale() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    let _stress = GcStressTestScope::normal();
    for chunks in [1usize, 4, 16, 64] {
        reset_internal();
        let mut tls = tlab_state_for_test();
        let mut payloads = fill_chunks(&mut tls, chunks, 32, |_| 0, 0);
        for (i, payload) in payloads.iter().enumerate() {
            unsafe { *(*payload).cast::<usize>() = i };
        }
        let registry = minor_registry_snapshot();
        let mut state = runtime().heap.lock().unwrap();
        retire_all_tlabs_unindexed(&mut state);
        let mut stop = Default::default();
        let mut collector = MinorCollector::new(&mut state, registry, &mut stop);
        // A fixed-size, potentially unaligned chunk intersects at most two pages.
        assert!(collector.chunk_pages.capacity() >= 2 * chunks);
        let capacity = collector.chunk_pages.capacity();
        let pages = collector.chunk_pages.len();
        // Reconstruct the source index without permitting any hash-table growth.
        collector.chunk_pages.clear();
        for index in 0..chunks {
            collector.index_chunk(index);
            assert_eq!(collector.chunk_pages.capacity(), capacity);
        }
        assert_eq!(collector.chunk_pages.len(), pages);
        let roots = MinorRoots {
            slots: payloads.iter_mut().map(|p| p as *mut *mut u8).collect(),
            values: Vec::new(),
        };
        let started = std::time::Instant::now();
        collector.run(roots, Default::default());
        let elapsed = started.elapsed().as_nanos();
        let count = payloads.len();
        assert_eq!(state.moved_objects, count as u64);
        assert_eq!(state.survivor_stats.survivor_copies, count as u64);
        assert_eq!(stop.root_values, count as u64);
        assert_eq!(stop.swept_objects, count as u64);
        for (i, payload) in payloads.iter().enumerate() {
            assert_eq!(unsafe { *(*payload).cast::<usize>() }, i);
        }
        eprintln!(
            "survivors={count} source_chunks={chunks} pages={pages} capacity={capacity} index_growths=0 moved={} elapsed_ns={elapsed} ns_per_move={:.2}",
            state.moved_objects,
            elapsed as f64 / count as f64
        );
        drop(state);
        reset_internal();
    }
}

#[test]
fn m4_survivor_admission_accounts_before_stop_and_transfers_without_storage_allocation() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    let _stress = GcStressTestScope::normal();
    for count in [1, 16, 128] {
        reset_internal();
        let mut tls = tlab_state_for_test();
        let mut payloads = fill_chunks(&mut tls, count, 8, |_| 0, 0);
        let mut state = runtime().heap.lock().unwrap();
        retire_all_tlabs_unindexed(&mut state);
        let before = state.tlab_reserved_bytes;
        super::prepare_survivor_reserve(&mut state);
        assert_eq!(state.survivor_reserve.len(), count);
        assert_eq!(
            state.survivor_reserve_stats.admitted_bytes,
            (count * GC_TLAB_CHUNK_SIZE) as u64
        );
        assert_eq!(
            state.survivor_reserve_stats.available_bytes,
            state.survivor_reserve_stats.admitted_bytes
        );
        assert_eq!(
            state.tlab_reserved_bytes,
            before + count * GC_TLAB_CHUNK_SIZE
        );
        assert!(
            state.survivor_reserve.iter().all(|c| c.used == 0
                && c.header_offsets.capacity() >= GC_TLAB_CHUNK_SIZE / GC_HEADER_SIZE)
        );
        STORAGE_ATTEMPTS.set(0);
        let mut stop = Default::default();
        MinorCollector::new(&mut state, minor_registry_snapshot(), &mut stop).run(
            MinorRoots {
                slots: payloads.iter_mut().map(|p| p as *mut *mut u8).collect(),
                values: Vec::new(),
            },
            Default::default(),
        );
        assert_eq!(STORAGE_ATTEMPTS.get(), 0);
        assert_eq!(state.survivor_stats.survivor_copies, (count * 8) as u64);
        // Production admission releases unused storage only after with_stw.
        assert!(state.survivor_reserve_active);
        super::release_survivor_reserve(&mut state);
        assert!(state.survivor_reserve.is_empty());
        assert_eq!(state.survivor_reserve_stats.available_bytes, 0);
        assert_eq!(
            state.survivor_reserve_stats.consumed_bytes,
            state.survivor_stats.survivor_space_reserved
        );
        assert!(!state.survivor_reserve_active);
        assert_eq!(
            state.tlab_reserved_bytes,
            state.tlab_chunks.iter().map(|c| c.capacity).sum::<usize>()
        );
        drop(state);
        reset_internal();
    }
}

#[test]
fn m4_empty_admission_fallback_preserves_graph_and_releases_reserve() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    for hard_cap in [false, true] {
        reset_internal();
        let mut tls = tlab_state_for_test();
        let child = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
        let mut root = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 1);
        unsafe {
            *root.cast::<*mut u8>() = child;
            *child.cast::<u64>() = 43;
        }
        let original = root;
        let mut state = runtime().heap.lock().unwrap();
        retire_all_tlabs_unindexed(&mut state);
        if hard_cap {
            state.memory_limit_bytes = Some(state.tlab_reserved_bytes);
        }
        // Empty admission also models refills racing a finite pre-stop reserve.
        state.survivor_reserve_active = true;
        let mut stop = Default::default();
        MinorCollector::new(&mut state, minor_registry_snapshot(), &mut stop).run(
            MinorRoots {
                slots: vec![&raw mut root],
                values: Vec::new(),
            },
            Default::default(),
        );
        assert_eq!(root, original);
        assert_eq!(unsafe { **root.cast::<*mut u64>() }, 43);
        assert_eq!(
            state.survivor_stats.pin_reasons[PinSource::MemoryBudget as usize],
            2
        );
        assert_eq!(state.young_allocated_bytes, 0);
        super::release_survivor_reserve(&mut state);
        assert!(!state.survivor_reserve_active);
        drop(state);
        reset_internal();
    }
}

#[test]
fn m4_admission_guard_releases_unused_capacity_on_unwind() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    reset_internal();
    let mut tls = tlab_state_for_test();
    willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    let before = runtime().heap.lock().unwrap().tlab_reserved_bytes;
    let result = std::panic::catch_unwind(|| {
        let _admission = super::SurvivorAdmission::new();
        assert!(runtime().heap.lock().unwrap().tlab_reserved_bytes > before);
        panic!("admission unwind fixture");
    });
    assert!(result.is_err());
    let state = runtime().heap.lock().unwrap();
    assert_eq!(state.tlab_reserved_bytes, before);
    assert!(!state.survivor_reserve_active);
    drop(state);
    reset_internal();
}

#[test]
fn m4_actual_stop_components_sum_to_complete_request_and_release() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    reset_internal();
    let mut tls = tlab_state_for_test();
    let mut root = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    willow_gc_add_runtime_root_slot(&raw mut root);
    minor_collect_internal();
    let event = crate::gc_telemetry::stops::snapshot_stops().last_event;
    let stats = crate::gc_telemetry::pause::snapshot();
    assert_eq!(stats.last.sequence, event.sequence);
    assert_eq!(
        stats.last.ns.iter().sum::<u64>(),
        event.released_ns - event.requested_ns
    );
    assert_eq!(stats.last.generation, 1);
    assert!(stats.by_reason[2].iter().all(|h| h.count == 1));
    assert!(!stats.last.aborted);
    willow_gc_remove_runtime_root_slot(&raw mut root);
    reset_internal();
}
