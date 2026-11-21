use super::*;
use crate::gc_telemetry::native_trace;

unsafe fn stopped(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    assert_ne!(
        crate::runtime::current()
            .mark_phase()
            .load(Ordering::Acquire),
        1,
        "legacy callback raced marking"
    );
    slots.push(payload.cast());
}
unsafe fn snapshot(payload: *mut u8, children: &mut Vec<*mut u8>) {
    // SAFETY: The live object's trace layout reserves these aligned pointer slots.
    children.push(unsafe { load_gc_reference(payload.cast()) });
}
unsafe fn slice(
    payload: *mut u8,
    _: usize,
    _: usize,
    children: &mut Vec<*mut u8>,
) -> TraceSliceProgress {
    // SAFETY: The registered test callback forwards the live payload and cursor within the
    // selected trace layout.
    unsafe { snapshot(payload, children) };
    TraceSliceProgress::Done
}

#[test]
fn native_capability_registration_replacement_and_reset() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
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
        // SAFETY: The fixture controls every instance of this ID and installs the matching
        // stopped tracer and synchronized snapshot capability, if present.
        unsafe { willow_register_type_capability(ID, stopped, capability) };
        assert_eq!(native_trace_modes(), [(ID, mode)]);
        let (legacy, traces, slices) = trace_registry_snapshot();
        assert!(legacy.contains(&ID));
        assert_eq!(
            trace_mode(traces.contains_key(&ID), slices.contains_key(&ID)),
            mode
        );
        // SAFETY: The fixture controls every instance of this ID and installs the matching
        // stopped tracer and synchronized snapshot capability, if present.
        unsafe { willow_register_type(ID, stopped) };
        assert_eq!(native_trace_modes(), [(ID, TraceMode::StopOnly)]);
        // Snapshot capability remains unchanged by replacement.
        assert_eq!(
            trace_mode(traces.contains_key(&ID), slices.contains_key(&ID)),
            mode
        );
    }
    // SAFETY: The fixture controls this ID and its remaining objects and collector epochs
    // across the change.
    unsafe { willow_unregister_type(ID) };
    assert!(native_trace_modes().is_empty());
    assert_eq!(trace_mode(true, true), TraceMode::SliceConcurrentSafe);
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
    assert!(native_trace::snapshot().is_empty());
}

#[test]
fn native_capability_sliced_graph_deduplicates_and_avoids_stopped_hooks() {
    let _guard = isolated_runtime();
    for count in [1, 16, 257] {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        for id in [0xFA91, 0xFA92, 0xFA93] {
            // SAFETY: The fixture controls every instance of this ID and installs the
            // matching stopped tracer and synchronized snapshot capability, if present.
            unsafe {
                willow_register_type_capability(
                    id,
                    stopped,
                    NativeTraceCapability::SliceConcurrentSafe(slice),
                )
            };
        }
        let mut roots = Vec::with_capacity(count * 3);
        for id in [0xFA91, 0xFA92, 0xFA93] {
            for _ in 0..count {
                // SAFETY: The requested layout describes the initialized words of this object
                // and its type’s tracing or finalization contract.
                let object = unsafe { willow_alloc_object(id, 8) };
                // SAFETY: The live object's trace layout reserves these aligned pointer slots.
                unsafe { store_gc_reference(object.cast(), object) };
                roots.push(object);
            }
        }
        for root in &mut roots {
            // SAFETY: The slot storage stays live and stationary through the matching pop,
            // including during collection.
            unsafe { willow_push_root(root) };
        }
        willow_gc_register_mutator();
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            (count * 3 * (GC_HEADER_SIZE + 8)) as i64
        );
        assert!(
            native_trace::snapshot().is_empty(),
            "sliced graph never invokes stopped hooks"
        );
        for _ in &roots {
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_root() };
        }
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        willow_gc_unregister_mutator();
    }
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn native_capability_transitive_sliced_types_preserve_reachability() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
    for id in [0xFA94, 0xFA95] {
        // SAFETY: The fixture controls every instance of this ID and installs the matching
        // stopped tracer and synchronized snapshot capability, if present.
        unsafe {
            willow_register_type_capability(
                id,
                stopped,
                NativeTraceCapability::SliceConcurrentSafe(slice),
            )
        };
    }
    // SAFETY: The requested layout describes the initialized words of this object and its
    // type’s tracing or finalization contract.
    let mut parent = unsafe { willow_alloc_object(0xFA94, 8) };
    // SAFETY: The requested layout describes the initialized words of this object and its
    // type’s tracing or finalization contract.
    let child = unsafe { willow_alloc_object(0xFA95, 8) };
    // SAFETY: The live object's trace layout reserves these aligned pointer slots.
    unsafe {
        store_gc_reference(parent.cast(), child);
    }
    // SAFETY: The initialized slot stays at this address until its registration is popped;
    // collection may rewrite it there.
    unsafe { willow_push_root(&raw mut parent) };
    willow_gc_register_mutator();
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        (2 * (GC_HEADER_SIZE + 8)) as i64
    );
    assert!(native_trace::snapshot().is_empty());
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_root() };
    willow_gc_unregister_mutator();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn native_capability_telemetry_unwind_and_reset() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
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
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
    drop(stale);
    assert!(native_trace::snapshot().is_empty());
}

/// Native array scaling never invokes an unrestricted stopped callback.
#[test]
fn native_capability_array_remark_measurement() {
    let _guard = isolated_runtime();
    for size in [4096, 32768, 262144] {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        // SAFETY: Flag 1 allocates full-width reference slots.
        let mut array = unsafe { crate::array::willow_array_new(size, 1) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut array) };
        let child = willow_alloc(8);
        for index in 0..size {
            // SAFETY: The destination is the live fixture array; the element word matches its
            // declared storage and tracing layout.
            unsafe { crate::array::willow_array_set(array, index, child as i64) };
        }
        willow_gc_register_mutator();
        willow_gc_collect();
        assert!(native_trace::snapshot().is_empty());
        assert_eq!(
            // SAFETY: The fixture retains the array allocated for these accesses.
            unsafe { crate::array::willow_array_get(array, size - 1) },
            child as i64
        );
        assert!(willow_gc_allocated_bytes() > 0);
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_root() };
        willow_gc_unregister_mutator();
    }
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

static SNAPSHOT_READY: crate::runtime::PerRuntime<std::sync::atomic::AtomicBool> =
    crate::runtime::PerRuntime::new(|| std::sync::atomic::AtomicBool::new(false));
static MUTATION_DONE: crate::runtime::PerRuntime<std::sync::atomic::AtomicBool> =
    crate::runtime::PerRuntime::new(|| std::sync::atomic::AtomicBool::new(false));

unsafe fn wait_for_mutation(
    _: *mut u8,
    _: usize,
    _: usize,
    _: &mut Vec<*mut u8>,
) -> TraceSliceProgress {
    SNAPSHOT_READY.store(true, Ordering::Release);
    if MUTATION_DONE.load(Ordering::Acquire) {
        TraceSliceProgress::Done
    } else {
        TraceSliceProgress::Retry
    }
}

#[test]
fn native_capability_sliced_slot_mutation_during_marking() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
    SNAPSHOT_READY.store(false, Ordering::Relaxed);
    MUTATION_DONE.store(false, Ordering::Relaxed);
    // SAFETY: The fixture controls every instance of this ID and installs the matching
    // stopped tracer and synchronized snapshot capability, if present.
    unsafe {
        willow_register_type_capability(
            0xFA96,
            stopped,
            NativeTraceCapability::SliceConcurrentSafe(wait_for_mutation),
        )
    };
    // SAFETY: The fixture controls every instance of this ID and installs the matching
    // stopped tracer and synchronized snapshot capability, if present.
    unsafe {
        willow_register_type_capability(
            0xFA97,
            stopped,
            NativeTraceCapability::SliceConcurrentSafe(slice),
        )
    };
    // SAFETY: The requested layout describes the initialized words of this object and its
    // type’s tracing or finalization contract.
    let mut control = unsafe { willow_alloc_object(0xFA96, 8) };
    // SAFETY: The requested layout describes the initialized words of this object and its
    // type’s tracing or finalization contract.
    let mut source = unsafe { willow_alloc_object(0xFA97, 8) };
    let child = willow_alloc(8);
    // SAFETY: The live object's trace layout reserves these aligned pointer slots.
    unsafe {
        store_gc_reference(source.cast(), child);
    }
    // SAFETY: The initialized slot stays at this address until its registration is popped;
    // collection may rewrite it there.
    unsafe {
        willow_push_root(&raw mut control);
        willow_push_root(&raw mut source);
    }
    willow_gc_register_mutator();
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let finish = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = {
        let (source, child) = (source as usize, child as usize);
        let (ready, finish) = (ready.clone(), finish.clone());
        std::thread::spawn(crate::runtime::serving(move || {
            let _mutator = MutatorRegistration::new();
            ready.store(true, Ordering::Release);
            while !SNAPSHOT_READY.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            let source = source as *mut u8;
            // SAFETY: The destination describes this reference update. Any asserted header
            // belongs to its live GC payload.
            unsafe {
                willow_gc_write_barrier(
                    source,
                    child as *mut u8,
                    std::ptr::null_mut(),
                    GcStoreDestination::ObjectField as i64,
                )
            };
            // Atomic slot publication permits bounded concurrent snapshots.
            // SAFETY: The live object's trace layout reserves these aligned pointer slots.
            unsafe {
                store_gc_reference(source.cast(), std::ptr::null_mut());
            }
            MUTATION_DONE.store(true, Ordering::Release);
            while !finish.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
        }))
    };
    while !ready.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    willow_gc_collect();
    finish.store(true, Ordering::Release);
    worker.join().unwrap();
    assert!(MUTATION_DONE.load(Ordering::Acquire));
    // SATB retains the deleted edge across bounded retries.
    assert_eq!(
        willow_gc_allocated_bytes(),
        (3 * (GC_HEADER_SIZE + 8)) as i64
    );
    assert!(native_trace::snapshot().is_empty());
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe {
        willow_pop_root();
        willow_pop_root();
    }
    willow_gc_unregister_mutator();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn native_capability_parallel_telemetry_aggregates_per_type() {
    let _guard = isolated_runtime();
    for types in [1, 16, 257] {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(crate::runtime::serving(|| {
                    let mut counts = native_trace::Counts::default();
                    for id in 0..types {
                        for _ in 0..8 {
                            counts.deferred(id);
                        }
                    }
                }));
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
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn native_capability_standard_container_inventory_survives_reset() {
    use willow_abi::runtime_type_ids::*;
    let _guard = isolated_runtime();
    for _ in 0..2 {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        let mut roots = [
            // SAFETY: Flag 1 allocates full-width reference slots.
            unsafe { crate::array::willow_array_new(1, 1) },
            crate::map::willow_map_new(0, 0, 1),
            crate::channel::willow_channel_new(1).cast(),
            // SAFETY: The initial word matches the constructor’s reference flag.
            unsafe { crate::async_mutex::willow_async_mutex_new(0, 1) }.cast(),
            // SAFETY: The initial word matches the constructor’s reference flag.
            unsafe { crate::async_rwlock::willow_async_rwlock_new(0, 1) }.cast(),
            // SAFETY: Zero initializes an empty reference slot.
            unsafe { crate::lock::willow_blocking_cell_new(0, 1) }.cast(),
            // SAFETY: Zero initializes an empty reference slot.
            unsafe { crate::lock::willow_blocking_rw_cell_new(0, 1) }.cast(),
            crate::cancellation::willow_task_scope_new(),
        ];
        for root in &mut roots {
            // SAFETY: The slot storage stays live and stationary through the matching pop,
            // including during collection.
            unsafe { willow_push_root(root) };
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
            assert_eq!(modes[&id], TraceMode::SliceConcurrentSafe);
        }
        assert!(!modes.contains_key(&CANCELLATION_TOKEN_TYPE_ID));
        willow_gc_register_mutator();
        willow_gc_collect();
        assert!(
            native_trace::snapshot().is_empty(),
            "standard containers must not fall back"
        );
        for _ in &roots {
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_root() };
        }
        willow_gc_unregister_mutator();
    }
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn native_capability_registration_snapshot_is_coherent() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
    const ID: u32 = 0xFA98;
    // SAFETY: The fixture controls every instance of this ID and installs the matching
    // stopped tracer and synchronized snapshot capability, if present.
    unsafe { willow_register_type(ID, stopped) };
    std::thread::scope(|scope| {
        scope.spawn(crate::runtime::serving(|| {
            for _ in 0..1024 {
                // SAFETY: The fixture controls every instance of this ID and installs the
                // matching stopped tracer and synchronized snapshot capability, if present.
                unsafe {
                    willow_register_type_capability(
                        ID,
                        stopped,
                        NativeTraceCapability::ConcurrentSafe(snapshot),
                    )
                };
                // SAFETY: The fixture controls every instance of this ID and installs the
                // matching stopped tracer and synchronized snapshot capability, if present.
                unsafe {
                    willow_register_type_capability(
                        ID,
                        stopped,
                        NativeTraceCapability::SliceConcurrentSafe(slice),
                    )
                };
                // SAFETY: The fixture controls every instance of this ID and installs the
                // matching stopped tracer and synchronized snapshot capability, if present.
                unsafe { willow_register_type(ID, stopped) };
            }
        }));
        for _ in 0..1024 {
            let (legacy, traces, slices) = trace_registry_snapshot();
            assert!(legacy.contains(&ID));
            assert!(!(traces.contains_key(&ID) && slices.contains_key(&ID)));
        }
    });
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}
