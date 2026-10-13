use super::*;
use std::sync::{Arc, Barrier};

unsafe fn trace_a(_: *mut u8, _: &mut Vec<*mut *mut u8>) {}
unsafe fn trace_b(_: *mut u8, _: &mut Vec<*mut *mut u8>) {}
unsafe fn drop_a(_: *mut u8) {}
unsafe fn drop_b(_: *mut u8) {}

#[test]
fn minor_registry_sparse_capacity_is_not_rescanned_on_cache_hits() {
    let _guard = runtime_test_guard();
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
    let _guard = runtime_test_guard();
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
    let _guard = runtime_test_guard();
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
    let _guard = runtime_test_guard();
    reset_internal();
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let worker = std::thread::spawn(move || {
        worker_barrier.wait();
        for id in 0..1_000 {
            willow_register_type(id, trace_a);
            willow_register_drop(id, drop_a);
        }
    });
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
    let _guard = runtime_test_guard();
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
