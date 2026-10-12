use super::*;
use crate::gc_telemetry::native_trace;

unsafe fn stopped(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    assert_ne!(
        GC_MARK_PHASE.load(Ordering::Acquire),
        1,
        "legacy callback raced marking"
    );
    slots.push(payload.cast());
}
unsafe fn snapshot(payload: *mut u8, children: &mut Vec<*mut u8>) {
    children.push(unsafe { load_gc_reference(payload.cast()) });
}
unsafe fn slice(
    payload: *mut u8,
    _: usize,
    _: usize,
    children: &mut Vec<*mut u8>,
) -> TraceSliceProgress {
    unsafe { snapshot(payload, children) };
    TraceSliceProgress::Done
}

#[test]
fn native_capability_registration_replacement_and_reset() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    const ID: u32 = 0xFA90;
    assert!(native_trace_modes().is_empty());
    for (capability, mode) in [
        (NativeTraceCapability::StopOnly, TraceMode::StopOnly),
        (
            NativeTraceCapability::ConcurrentSafe(snapshot),
            TraceMode::ConcurrentSafe,
        ),
        (
            NativeTraceCapability::SliceConcurrentSafe(slice),
            TraceMode::SliceConcurrentSafe,
        ),
    ] {
        willow_register_type_capability(ID, stopped, capability);
        assert_eq!(native_trace_modes(), [(ID, mode)]);
        let (legacy, traces, slices) = trace_registry_snapshot();
        assert!(legacy.contains(&ID));
        assert_eq!(
            trace_mode(traces.contains_key(&ID), slices.contains_key(&ID)),
            mode
        );
        willow_register_type(ID, stopped);
        assert_eq!(native_trace_modes(), [(ID, TraceMode::StopOnly)]);
        // Snapshot capability remains unchanged by replacement.
        assert_eq!(
            trace_mode(traces.contains_key(&ID), slices.contains_key(&ID)),
            mode
        );
    }
    willow_unregister_type(ID);
    assert!(native_trace_modes().is_empty());
    assert_eq!(trace_mode(true, true), TraceMode::SliceConcurrentSafe);
    reset_internal_for_test();
    assert!(native_trace::snapshot().is_empty());
}

#[test]
fn native_capability_counts_deduplicate_and_keep_legacy_stopped() {
    let _guard = runtime_test_guard();
    for count in [1, 16, 257] {
        reset_internal_for_test();
        willow_register_type_capability(0xFA91, stopped, NativeTraceCapability::StopOnly);
        willow_register_type_capability(
            0xFA92,
            stopped,
            NativeTraceCapability::ConcurrentSafe(snapshot),
        );
        willow_register_type_capability(
            0xFA93,
            stopped,
            NativeTraceCapability::SliceConcurrentSafe(slice),
        );
        let mut roots = Vec::with_capacity(count * 3);
        for id in [0xFA91, 0xFA92, 0xFA93] {
            for _ in 0..count {
                let object = willow_alloc_object(id, 8);
                unsafe { store_gc_reference(object.cast(), object) };
                roots.push(object);
            }
        }
        for root in &mut roots {
            willow_push_root(root);
        }
        let (_, cycle) = root_handshake::begin().unwrap();
        for root in &roots {
            for _ in 0..8 {
                cycle.enqueue(*root);
            }
        }
        cycle.drain_checked_until(usize::MAX, None);
        assert_eq!(cycle.deferred.lock().unwrap().len(), count);
        // Counts are visible before epoch Drop; cached epoch owners cannot delay them.
        let stats = native_trace::snapshot();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].deferred_objects, count as u64);
        assert_eq!(stats[0].stopped_calls, 0);
        assert!(
            mark_closure::finish_with_wait(&cycle, std::time::Instant::now(), |_| {}).is_none()
        );
        mark_closure::finish_stopped(&cycle, std::time::Instant::now());
        let stats = native_trace::snapshot();
        assert_eq!(stats.len(), 3);
        for row in &stats {
            assert_eq!(
                row.deferred_objects,
                if row.type_id == 0xFA91 {
                    count as u64
                } else {
                    0
                }
            );
            assert_eq!(row.stopped_calls, count as u64);
            assert_eq!(
                row.stop_only_calls,
                if row.type_id == 0xFA91 {
                    count as u64
                } else {
                    0
                }
            );
        }
        drop(cycle);
        for _ in &roots {
            willow_pop_root();
        }
        println!(
            "native counts objects_per_type={count} legacy_deferrals={count} repeated_edges=8"
        );
    }
    reset_internal_for_test();
}

#[test]
fn native_capability_transitive_stopped_types_are_reported() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    willow_register_type(0xFA94, stopped);
    willow_register_type(0xFA95, stopped);
    let mut parent = willow_alloc_object(0xFA94, 8);
    let child = willow_alloc_object(0xFA95, 8);
    unsafe {
        store_gc_reference(parent.cast(), child);
    }
    willow_push_root(&mut parent);
    willow_gc_register_mutator();
    willow_gc_collect();
    let rows = native_trace::snapshot();
    let parent_stats = rows.iter().find(|r| r.type_id == 0xFA94).unwrap();
    let child_stats = rows.iter().find(|r| r.type_id == 0xFA95).unwrap();
    assert_eq!(parent_stats.deferred_objects, 1);
    assert_eq!(parent_stats.stop_only_calls, 1);
    assert_eq!(child_stats.deferred_objects, 0);
    assert_eq!(child_stats.stop_only_calls, 1);
    willow_pop_root();
    willow_gc_unregister_mutator();
    reset_internal_for_test();
}

#[test]
fn native_capability_telemetry_unwind_and_reset() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let _ = std::panic::catch_unwind(|| {
        let mut counts = native_trace::Counts::default();
        counts.deferred(123);
        counts.stopped(123, true);
        panic!("callback failure");
    });
    assert_eq!(
        native_trace::snapshot(),
        [native_trace::NativeTraceStats {
            type_id: 123,
            deferred_objects: 1,
            stopped_calls: 1,
            stop_only_calls: 1,
        }]
    );
    let mut stale = native_trace::Counts::default();
    stale.deferred(123);
    reset_internal_for_test();
    drop(stale);
    assert!(native_trace::snapshot().is_empty());
}

/// Controlled A/B with real native reference arrays; only hook capability differs.
/// Times are observations, deterministic fallback/call counts are assertions.
#[test]
fn native_capability_array_remark_measurement() {
    let _guard = runtime_test_guard();
    use willow_abi::runtime_type_ids::ARRAY_REF_TYPE_ID;
    for size in [4096, 32768, 262144] {
        for legacy in [true, false] {
            let mut times = Vec::new();
            for _ in 0..5 {
                reset_internal_for_test();
                runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
                let mut array = crate::array::willow_array_new(size, 1);
                willow_push_root(&mut array);
                let child = willow_alloc(8);
                for index in 0..size {
                    crate::array::willow_array_set(array, index, child as i64);
                }
                if legacy {
                    let hook = type_registry().lock().unwrap()[&ARRAY_REF_TYPE_ID];
                    willow_register_type(ARRAY_REF_TYPE_ID, hook);
                }
                willow_gc_register_mutator();
                willow_gc_collect();
                let stats = native_trace::snapshot();
                assert_eq!(
                    stats.iter().map(|r| r.deferred_objects).sum::<u64>(),
                    u64::from(legacy)
                );
                assert_eq!(
                    stats.iter().map(|r| r.stopped_calls).sum::<u64>(),
                    u64::from(legacy)
                );
                let stops = crate::gc_telemetry::stops::snapshot_stops();
                assert!(stops.by_reason[1].completed > 0);
                assert_eq!(stops.by_reason[3].completed, u64::from(legacy));
                times.push(
                    stops.by_reason[1].stopped.total_ns + stops.by_reason[3].stopped.total_ns,
                );
                assert!(willow_gc_allocated_bytes() > 0);
                willow_pop_root();
                willow_gc_unregister_mutator();
            }
            times.sort_unstable();
            println!(
                "native array slots={size} legacy={legacy} remark_and_recovery_ns={times:?} median_ns={}",
                times[2]
            );
        }
    }
    reset_internal_for_test();
}

static SNAPSHOT_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static MUTATION_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

unsafe fn wait_for_mutation(_: *mut u8, _: &mut Vec<*mut u8>) {
    SNAPSHOT_READY.store(true, Ordering::Release);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !MUTATION_DONE.load(Ordering::Acquire) {
        assert!(
            std::time::Instant::now() < deadline,
            "mutator did not overlap concurrent snapshot"
        );
        std::thread::yield_now();
    }
}

#[test]
fn native_capability_legacy_slot_mutation_during_marking() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    SNAPSHOT_READY.store(false, Ordering::Relaxed);
    MUTATION_DONE.store(false, Ordering::Relaxed);
    willow_register_type_capability(
        0xFA96,
        stopped,
        NativeTraceCapability::ConcurrentSafe(wait_for_mutation),
    );
    willow_register_type(0xFA97, stopped);
    let mut control = willow_alloc_object(0xFA96, 8);
    let mut source = willow_alloc_object(0xFA97, 8);
    let child = willow_alloc(8);
    unsafe {
        *source.cast::<*mut u8>() = child;
    }
    willow_push_root(&mut control);
    willow_push_root(&mut source);
    willow_gc_register_mutator();
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let finish = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = {
        let (source, child) = (source as usize, child as usize);
        let (ready, finish) = (ready.clone(), finish.clone());
        std::thread::spawn(move || {
            let _mutator = MutatorRegistration::new();
            ready.store(true, Ordering::Release);
            while !SNAPSHOT_READY.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            let source = source as *mut u8;
            willow_gc_write_barrier(
                source,
                child as *mut u8,
                std::ptr::null_mut(),
                GcStoreDestination::ObjectField as i64,
            );
            // Intentionally non-atomic legacy storage: collector must defer its hook.
            unsafe {
                *source.cast::<*mut u8>() = std::ptr::null_mut();
            }
            MUTATION_DONE.store(true, Ordering::Release);
            while !finish.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
        })
    };
    while !ready.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    willow_gc_collect();
    finish.store(true, Ordering::Release);
    worker.join().unwrap();
    assert!(MUTATION_DONE.load(Ordering::Acquire));
    // SATB retains the deleted edge; legacy slot callbacks ran only after stop.
    assert_eq!(
        willow_gc_allocated_bytes(),
        (3 * (GC_HEADER_SIZE + 8)) as i64
    );
    let rows = native_trace::snapshot();
    let legacy = rows.iter().find(|row| row.type_id == 0xFA97).unwrap();
    assert_eq!(legacy.deferred_objects, 1);
    assert_eq!(legacy.stop_only_calls, 1);
    willow_pop_root();
    willow_pop_root();
    willow_gc_unregister_mutator();
    reset_internal_for_test();
}

#[test]
fn native_capability_parallel_telemetry_aggregates_per_type() {
    let _guard = runtime_test_guard();
    for types in [1, 16, 257] {
        reset_internal_for_test();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    let mut counts = native_trace::Counts::default();
                    for id in 0..types {
                        for _ in 0..8 {
                            counts.deferred(id);
                        }
                    }
                });
            }
        });
        let rows = native_trace::snapshot();
        assert_eq!(rows.len(), types as usize);
        assert!(rows.iter().all(|row| row.deferred_objects == 32));
        println!(
            "native telemetry types={types} workers=4 discoveries={} published_rows={}",
            types * 32,
            rows.len()
        );
    }
    reset_internal_for_test();
}

#[test]
fn native_capability_standard_container_inventory_survives_reset() {
    use willow_abi::runtime_type_ids::*;
    let _guard = runtime_test_guard();
    for _ in 0..2 {
        reset_internal_for_test();
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        let mut roots = [
            crate::array::willow_array_new(1, 1),
            crate::map::willow_map_new(0, 0, 1),
            crate::channel::willow_channel_new(1).cast(),
            crate::async_mutex::willow_async_mutex_new(0, 1).cast(),
            crate::async_rwlock::willow_async_rwlock_new(0, 1).cast(),
            crate::lock::willow_blocking_cell_new(0, 1).cast(),
            crate::lock::willow_blocking_rw_cell_new(0, 1).cast(),
            crate::cancellation::willow_task_scope_new(),
        ];
        for root in &mut roots {
            willow_push_root(root);
        }
        let modes: IdMap<_, _> = native_trace_modes().into_iter().collect();
        assert_eq!(modes.len(), 8);
        for id in [
            ARRAY_REF_TYPE_ID,
            MAP_TYPE_ID,
            CHANNEL_TYPE_ID,
            TASK_SCOPE_TYPE_ID,
        ] {
            assert_eq!(modes[&id], TraceMode::SliceConcurrentSafe);
        }
        for id in [
            ASYNC_MUTEX_TYPE_ID,
            ASYNC_RWLOCK_TYPE_ID,
            BLOCKING_CELL_TYPE_ID,
            BLOCKING_RW_CELL_TYPE_ID,
        ] {
            assert_eq!(modes[&id], TraceMode::ConcurrentSafe);
        }
        assert!(!modes.contains_key(&CANCELLATION_TOKEN_TYPE_ID));
        willow_gc_register_mutator();
        willow_gc_collect();
        assert!(
            native_trace::snapshot().is_empty(),
            "standard containers must not fall back"
        );
        for _ in &roots {
            willow_pop_root();
        }
        willow_gc_unregister_mutator();
    }
    reset_internal_for_test();
}

#[test]
fn native_capability_registration_snapshot_is_coherent() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    const ID: u32 = 0xFA98;
    willow_register_type(ID, stopped);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for _ in 0..1024 {
                willow_register_type_capability(
                    ID,
                    stopped,
                    NativeTraceCapability::ConcurrentSafe(snapshot),
                );
                willow_register_type_capability(
                    ID,
                    stopped,
                    NativeTraceCapability::SliceConcurrentSafe(slice),
                );
                willow_register_type(ID, stopped);
            }
        });
        for _ in 0..1024 {
            let (legacy, traces, slices) = trace_registry_snapshot();
            assert!(legacy.contains(&ID));
            assert!(!(traces.contains_key(&ID) && slices.contains_key(&ID)));
        }
    });
    reset_internal_for_test();
}
