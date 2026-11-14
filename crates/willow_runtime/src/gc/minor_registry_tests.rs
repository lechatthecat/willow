use super::*;
use std::sync::{Arc, Barrier};

unsafe fn trace_a(_: *mut u8, _: &mut Vec<*mut *mut u8>) {}
unsafe fn trace_b(_: *mut u8, _: &mut Vec<*mut *mut u8>) {}
unsafe fn drop_a(_: *mut u8) {}
unsafe fn drop_b(_: *mut u8) {}

#[test]
fn native_registration_uses_owning_gc_generation_and_hooks() {
    let barrier = Barrier::new(2);
    std::thread::scope(|scope| {
        let handles: Vec<_> = [drop_a as DropFn, drop_b as DropFn]
            .into_iter()
            .map(|hook| {
                let barrier = &barrier;
                scope.spawn(crate::runtime::serving(move || {
                    let gc = GcRuntime::default();
                    let registration = NativeGcRegistration::Network;
                    let type_id = willow_abi::runtime_type_ids::NETWORK_HANDLE_TYPE_ID;
                    let types = [NativeGcType::new(type_id, None, Some(hook))];
                    let mut isolated = true;
                    for round in 0..32 {
                        // Both collectors have identical generations and type IDs,
                        // but intentionally different hooks. No global test guard.
                        barrier.wait();
                        isolated &= registration.ensure_in(&gc, &types);
                        isolated &= !registration.ensure_in(&gc, &types);
                        let snapshot = minor_registry_snapshot_in(&gc);
                        barrier.wait();
                        isolated &= snapshot.drops.len() == 1;
                        isolated &= snapshot
                            .drops
                            .get(&type_id)
                            .is_some_and(|&found| std::ptr::fn_addr_eq(found, hook));
                        if round % 2 == 0 {
                            clear_hook_registries_in(&gc);
                            // As with heap reset, invalidation follows clearing.
                            gc.registry_generation.fetch_add(1, Ordering::AcqRel);
                        } else {
                            unregister_type_in(&gc, type_id);
                        }
                        // Captured epochs survive subsequent registry mutations.
                        isolated &= snapshot
                            .drops
                            .get(&type_id)
                            .is_some_and(|&found| std::ptr::fn_addr_eq(found, hook));
                    }
                    isolated
                }))
            })
            .collect();
        for handle in handles {
            assert!(handle.join().unwrap());
        }
    });
}

#[test]
fn native_registration_groups_publish_once_under_contention() {
    let groups = [
        NativeGcRegistration::Array,
        NativeGcRegistration::Map,
        NativeGcRegistration::Channel,
        NativeGcRegistration::BlockingCell,
        NativeGcRegistration::AsyncMutex,
        NativeGcRegistration::AsyncRwLock,
        NativeGcRegistration::Cancellation,
        NativeGcRegistration::Network,
    ];
    for threads in [2, 8, 32] {
        let gc = GcRuntime::default();
        let barrier = Barrier::new(threads);
        let types = [NativeGcType::new(
            willow_abi::runtime_type_ids::NETWORK_HANDLE_TYPE_ID,
            None,
            Some(drop_a),
        )];
        let publications = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    let (gc, barrier, types) = (&gc, &barrier, &types);
                    scope.spawn(crate::runtime::serving(move || {
                        barrier.wait();
                        groups.map(|group| usize::from(group.ensure_in(gc, types)))
                    }))
                })
                .collect();
            let mut publications = [0; NATIVE_REGISTRATION_COUNT];
            for handle in handles {
                for (count, added) in publications.iter_mut().zip(handle.join().unwrap()) {
                    *count += added;
                }
            }
            publications
        });
        assert_eq!(publications, [1; NATIVE_REGISTRATION_COUNT]);
    }
}

#[test]
fn gc_registry_snapshots_are_independent_at_increasing_type_counts() {
    for count in [0, 1, 8, 64, 1024] {
        let first = GcRuntime::default();
        let second = GcRuntime::default();
        for id in 0..count {
            register_hooks_in(&first, id, trace_a, None, None);
            register_hooks_in(&second, id, trace_b, None, None);
        }
        let snapshot = minor_registry_snapshot_in(&first);
        let other = minor_registry_snapshot_in(&second);
        assert_eq!(snapshot.trace.len(), count as usize);
        assert_eq!(other.trace.len(), count as usize);
        for id in 0..count {
            assert!(std::ptr::fn_addr_eq(
                snapshot.trace[&id],
                trace_a as TraceFn
            ));
            assert!(std::ptr::fn_addr_eq(other.trace[&id], trace_b as TraceFn));
            let (hook, mode) = lookup_trace_contract_in(&first, id).unwrap();
            assert!(std::ptr::fn_addr_eq(hook, trace_a as TraceFn));
            assert_eq!(mode, TraceMode::StopOnly);
        }
        assert_eq!(native_trace_modes_in(&first).len(), count as usize);
        let (legacy, concurrent, sliced) = trace_registry_snapshot_in(&second);
        assert_eq!(legacy.len(), count as usize);
        assert!(concurrent.is_empty());
        assert!(sliced.is_empty());
        assert!(lookup_drop_in(&first, u32::MAX).is_none());
        clear_hook_registries_in(&first);
        assert!(minor_registry_snapshot_in(&first).trace.is_empty());
        // Mutating one owner must not invalidate or rebuild the other's cache.
        for _ in 0..32 {
            assert!(Arc::ptr_eq(&other, &minor_registry_snapshot_in(&second)));
        }
        assert_eq!(snapshot.trace.len(), count as usize);
    }
}

#[test]
fn minor_registry_sparse_capacity_is_not_rescanned_on_cache_hits() {
    let _guard = isolated_runtime();
    for count in [100, 10_000] {
        reset_internal();
        for id in 0..count {
            willow_register_type(id, trace_a);
        }
        for id in 1..count {
            willow_unregister_type(id);
        }
        let sparse = minor_registry_snapshot();
        assert_eq!(sparse.trace.len(), 1);
        assert!(sparse.trace.capacity() > 1);
        for _ in 0..100 {
            assert!(Arc::ptr_eq(&sparse, &minor_registry_snapshot()));
        }
    }
    reset_internal();
}

#[test]
fn minor_registry_reuses_one_snapshot_at_increasing_type_counts() {
    let _guard = isolated_runtime();
    for count in [0, 1, 100, 10_000] {
        reset_internal();
        for id in 0..count {
            willow_register_type(id, trace_a);
            willow_register_drop(id, drop_a);
        }
        // Startup registration batches do not eagerly clone growing maps.
        assert!(
            runtime()
                .minor_registry_cache
                .lock()
                .unwrap()
                .snapshot
                .is_none()
        );
        let snapshot = minor_registry_snapshot();
        assert_eq!(snapshot.trace.len(), count as usize);
        assert_eq!(snapshot.drops.len(), count as usize);
        for _ in 0..100 {
            let repeated = minor_registry_snapshot();
            assert!(Arc::ptr_eq(&snapshot, &repeated));
            assert_eq!(Arc::strong_count(&snapshot), 3);
        }
    }
    reset_internal();
}

#[test]
fn minor_registry_replacement_unregister_and_reset_preserve_captured_hooks() {
    let _guard = isolated_runtime();
    reset_internal();
    willow_register_type(1, trace_a);
    willow_register_drop(1, drop_a);
    let original = minor_registry_snapshot();
    willow_register_type(1, trace_b);
    let trace_changed = minor_registry_snapshot();
    assert_ne!(original.generation, trace_changed.generation);
    assert!(std::ptr::fn_addr_eq(
        trace_changed.trace[&1],
        trace_b as TraceFn
    ));
    willow_register_drop(1, drop_b);
    let drop_changed = minor_registry_snapshot();
    assert_ne!(trace_changed.generation, drop_changed.generation);
    assert!(std::ptr::fn_addr_eq(
        drop_changed.drops[&1],
        drop_b as DropFn
    ));
    willow_unregister_type(1);
    let removed = minor_registry_snapshot();
    assert!(!removed.trace.contains_key(&1));
    // Existing API unregisters tracing only; a finalizer still belongs to type 1.
    assert!(removed.drops.contains_key(&1));
    reset_internal();
    let empty = minor_registry_snapshot();
    assert!(empty.trace.is_empty());
    assert!(empty.drops.is_empty());
    assert!(!empty.generated_drop);
    assert!(std::ptr::fn_addr_eq(original.trace[&1], trace_a as TraceFn));
    assert!(std::ptr::fn_addr_eq(original.drops[&1], drop_a as DropFn));
    reset_internal();
}

#[test]
fn minor_registry_capture_is_consistent_during_registration() {
    let _guard = isolated_runtime();
    reset_internal();
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let worker = std::thread::spawn(crate::runtime::serving(move || {
        worker_barrier.wait();
        for id in 0..1_000 {
            willow_register_type(id, trace_a);
            willow_register_drop(id, drop_a);
        }
    }));
    barrier.wait();
    for _ in 0..1_000 {
        let snapshot = minor_registry_snapshot();
        // The writer always publishes trace before drop. A pair of captures at
        // different times could contain a drop whose trace was absent earlier.
        assert!(
            snapshot
                .drops
                .keys()
                .all(|id| snapshot.trace.contains_key(id))
        );
        assert!(snapshot.trace.len() <= snapshot.drops.len() + 1);
    }
    worker.join().unwrap();
    let final_snapshot = minor_registry_snapshot();
    assert_eq!(final_snapshot.trace.len(), 1_000);
    assert_eq!(final_snapshot.drops.len(), 1_000);
    reset_internal();
}

#[test]
fn minor_registry_derived_drop_metadata_refreshes_and_generation_wraps() {
    let _guard = isolated_runtime();
    reset_internal();
    let native_id = willow_abi::runtime_type_ids::NETWORK_HANDLE_TYPE_ID;
    willow_register_drop(native_id, drop_a);
    let native = minor_registry_snapshot();
    assert!(!native.generated_drop);
    // Unknown/generated IDs require the general finalizer path.
    willow_register_drop(u32::MAX, drop_b);
    assert!(minor_registry_snapshot().generated_drop);
    {
        let mut cache = runtime().minor_registry_cache.lock().unwrap();
        cache.generation = u64::MAX;
        cache.changed();
        assert_eq!(cache.generation, 0);
        assert!(cache.snapshot.is_none());
    }
    let rebuilt = minor_registry_snapshot();
    assert_eq!(rebuilt.generation, 0);
    assert_eq!(rebuilt.drops.len(), 2);
    assert!(!Arc::ptr_eq(&native, &rebuilt));
    reset_internal();
}
