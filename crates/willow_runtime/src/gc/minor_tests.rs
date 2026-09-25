//! Deterministic tracing-work checks for the minor-collector extraction.

use super::MinorCollector;
use crate::gc::{
    GC_HEADER_SIZE, drop_registry, reset_internal, retire_all_tlabs_locked, runtime,
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
            let trace = type_registry().lock().unwrap().clone();
            let drops = drop_registry().lock().unwrap().clone();
            let mut state = runtime().heap.lock().unwrap();
            retire_all_tlabs_locked(&mut state);
            let mut stop = crate::gc_telemetry::stops::StopWorkV2::default();
            let (_, work) = MinorCollector::new(&mut state, trace, drops, &mut stop)
                .run(vec![nodes[0]; root_count], Default::default());
            // Existing production telemetry counts objects and reference slots,
            // not elapsed time. Duplicate edges/roots must not rescan objects.
            assert_eq!(work.marked_bytes, (n * (GC_HEADER_SIZE + 16)) as u64);
            assert_eq!(work.scanned_bytes, (2 * n * size_of::<usize>()) as u64);
            assert_eq!(
                work.root_scan_bytes,
                (root_count * size_of::<usize>()) as u64
            );
            // One sweep walk over 2n source headers plus n - 1 copies; the
            // young set is not pre-indexed (willow-8hq4.16).
            assert_eq!(stop.metadata_objects, (3 * n - 1) as u64);
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
    let trace = type_registry().lock().unwrap().clone();
    let drops = drop_registry().lock().unwrap().clone();
    let mut state = runtime().heap.lock().unwrap();
    retire_all_tlabs_locked(&mut state);
    let remembered = std::mem::take(&mut state.remembered_set);
    state.dirty_cards.clear();
    let (_, work) = MinorCollector::new(&mut state, trace, drops, &mut Default::default())
        .run(roots, remembered);
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
                retire_all_tlabs_locked(&mut state);
                let remembered = std::mem::take(&mut state.remembered_set);
                state.dirty_cards.clear();
                let mut stop = crate::gc_telemetry::stops::StopWorkV2::default();
                let roots = if young == 0 { vec![] } else { vec![owner] };
                let (_, work) = MinorCollector::new(
                    &mut state,
                    Default::default(),
                    Default::default(),
                    &mut stop,
                )
                .run(roots, remembered);
                // One sweep walk: sources plus this cycle's survivor copies.
                let expected = if cycle % 2 == 0 { 3 * young } else { young };
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
    retire_all_tlabs_locked(&mut state);
    let mut stop = crate::gc_telemetry::stops::StopWorkV2::default();
    let mut collector = MinorCollector::new(
        &mut state,
        Default::default(),
        Default::default(),
        &mut stop,
    );
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
