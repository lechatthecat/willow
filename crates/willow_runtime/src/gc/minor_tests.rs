//! Deterministic tracing-work checks for the minor-collector extraction.

use super::{MinorCollector, MinorRoots, PinSource, verify_no_pin_enabled};
use crate::gc::{
    GC_HEADER_SIZE, drop_registry, reset_internal, retire_all_tlabs_unindexed, runtime,
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
            retire_all_tlabs_unindexed(&mut state);
            let mut stop = crate::gc_telemetry::stops::StopWorkV2::default();
            let (_, work) = MinorCollector::new(&mut state, trace, drops, &mut stop).run(
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
            // The n chunks holding a node are indexed once and swept once,
            // plus n - 1 copies; the n dead chunks are released without a
            // header walk (willow-8hq4.21), and the young set is not
            // pre-indexed (willow-8hq4.16).
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
    retire_all_tlabs_unindexed(&mut state);
    let remembered = std::mem::take(&mut state.remembered_set);
    state.dirty_cards.clear();
    let (_, work) = MinorCollector::new(&mut state, trace, drops, &mut Default::default()).run(
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
                let (_, work) = MinorCollector::new(
                    &mut state,
                    Default::default(),
                    Default::default(),
                    &mut stop,
                )
                .run(
                    MinorRoots {
                        slots: Vec::new(),
                        values: roots
                            .into_iter()
                            .map(|root| (root, PinSource::RuntimeCodeStack))
                            .collect(),
                    },
                    remembered,
                );
                // Child chunks are indexed and swept, survivor copies swept;
                // dead chunks are released unwalked (willow-8hq4.21).
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
    retire_all_tlabs_unindexed(&mut state);
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
    let trace = type_registry().lock().unwrap().clone();
    let drops = drop_registry().lock().unwrap().clone();
    let mut state = runtime().heap.lock().unwrap();
    retire_all_tlabs_unindexed(&mut state);
    let mut stop = crate::gc_telemetry::stops::StopWorkV2::default();
    MinorCollector::new(&mut state, trace, drops, &mut stop).run(
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
                // The rooted chunk: index + sweep walk; plus the one copy.
                assert_eq!(stop.metadata_objects, 2 * per_chunk as u64 + 1);
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
            MinorCollector::new(
                &mut state,
                Default::default(),
                Default::default(),
                &mut stop,
            )
            .run(
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
