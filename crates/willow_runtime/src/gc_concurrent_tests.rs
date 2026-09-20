use super::*;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

#[test]
fn failed_stopped_callback_resumes_world_and_remains_in_stop_telemetry() {
    use crate::gc_telemetry::stops::{STOP_COUNTERS_VALID, StopReason, snapshot_stops};
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let result = std::panic::catch_unwind(|| {
        with_stw(StopReason::InitialMark, |_, _| {
            let active = snapshot_stops();
            assert_ne!(active.active_sequence, 0);
            assert_eq!(active.by_reason[0].requests, 1);
            assert_eq!(active.by_reason[0].completed, 0);
            panic!("injected stopped callback failure");
        });
    });
    assert!(result.is_err());
    assert!(!runtime().stop_requested.load(Ordering::Acquire));
    let stats = snapshot_stops();
    assert_eq!(stats.flags, STOP_COUNTERS_VALID);
    assert_eq!(stats.active_sequence, 0);
    assert_eq!(stats.by_reason[0].completed, 1);
    assert_eq!(stats.by_reason[0].aborted, 1);
    assert_eq!(stats.events_pending, 1);
    willow_gc_collect();
    let stats = snapshot_stops();
    assert_eq!(stats.by_reason[0].requests, 1);
    assert_eq!(stats.by_reason[1].requests, 0);
    assert_eq!(stats.events_pending, 0);
    reset_internal_for_test();
}

static SNAPSHOT_READY: AtomicBool = AtomicBool::new(false);
static MUTATION_DONE: AtomicBool = AtomicBool::new(false);

#[test]
fn satb_deleted_subgraph_survives_then_dies_in_the_next_epoch() {
    let _guard = runtime_test_guard();
    for capacity in [1, 256] {
        for unregister in [false, true] {
            reset_internal_for_test();
            runtime().heap.lock().unwrap().satb = satb::SatbBuffers::new(capacity);
            SNAPSHOT_READY.store(false, Ordering::Relaxed);
            MUTATION_DONE.store(false, Ordering::Relaxed);
            const CONTROL: u32 = 0xFC11;
            const SOURCE: u32 = 0xFC12;
            willow_register_type(CONTROL, trace_slot);
            willow_register_type(SOURCE, trace_slot);
            runtime()
                .concurrent_trace_registry
                .lock()
                .unwrap()
                .insert(CONTROL, snapshot_before_mutation);
            let mut control = willow_alloc_object(CONTROL as i64, 8);
            let mut source = willow_alloc_object(SOURCE as i64, 8);
            let child = willow_alloc_typed(8, 1);
            let grandchild = willow_alloc(8);
            unsafe {
                store_gc_reference(source.cast(), child);
                store_gc_reference(child.cast(), grandchild);
            }
            willow_push_root(&mut control);
            willow_push_root(&mut source);
            willow_gc_register_mutator();
            let ready = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let worker = {
                let source = source as usize;
                let (ready, stop) = (ready.clone(), stop.clone());
                std::thread::spawn(move || {
                    willow_gc_register_mutator();
                    ready.store(true, Ordering::Release);
                    while !SNAPSHOT_READY.load(Ordering::Acquire) {
                        willow_gc_safepoint();
                        std::thread::yield_now();
                    }
                    let source = source as *mut u8;
                    let old = unsafe { load_gc_reference(source.cast()) };
                    willow_gc_write_barrier(
                        source,
                        old,
                        std::ptr::null_mut(),
                        GcStoreDestination::ObjectField as i64,
                    );
                    unsafe { store_gc_reference(source.cast(), std::ptr::null_mut()) };
                    if unregister {
                        willow_gc_unregister_mutator();
                    }
                    MUTATION_DONE.store(true, Ordering::Release);
                    if !unregister {
                        while !stop.load(Ordering::Acquire) {
                            willow_gc_safepoint();
                            std::thread::yield_now();
                        }
                        willow_gc_unregister_mutator();
                    }
                })
            };
            while !ready.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            willow_gc_collect();
            stop.store(true, Ordering::Release);
            worker.join().unwrap();
            assert_eq!(
                willow_gc_allocated_bytes(),
                4 * (GC_HEADER_SIZE + 8) as i64,
                "snapshot subgraph lost: capacity={capacity} unregister={unregister}"
            );
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), 2 * (GC_HEADER_SIZE + 8) as i64);
            willow_pop_roots(2);
            willow_gc_unregister_mutator();
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), 0);
        }
    }
    reset_internal_for_test();
}

/// Exercise real container mutations before any snapshot graph is scanned.
/// No root/new-edge publication can keep the overwritten object alive here.
fn assert_satb_mutation_retains(old: *mut u8, mutation: impl FnOnce()) {
    let cycle = {
        let mut state = runtime().heap.lock().unwrap();
        retire_all_tlabs_locked(&mut state);
        let cycle = Arc::new(ConcurrentCycle::new(
            epoch_objects(&state, &mut Default::default())
                .into_iter()
                .map(|o| (o.payload().as_ptr() as usize, o.trace_metadata())),
            type_registry().lock().unwrap().keys().copied().collect(),
            runtime().concurrent_trace_registry.lock().unwrap().clone(),
            0,
        ));
        state.concurrent_cycle = Some(cycle.clone());
        GC_MARK_PHASE.store(1, Ordering::Release);
        cycle
    };
    mutation();
    flush_satb_all_locked(&mut runtime().heap.lock().unwrap());
    cycle.drain(usize::MAX);
    assert!(cycle.is_marked(old as usize), "deleted edge was not traced");
    assert!(cycle.queue.snapshot().is_drained());
    assert_eq!(
        cycle.queue.snapshot().injected,
        cycle.objects.marked_count() as u64,
        "equivalent publications must not create duplicate queue jobs"
    );
    GC_MARK_PHASE.store(0, Ordering::Release);
    runtime().heap.lock().unwrap().concurrent_cycle = None;
    assert_eq!(cycle.queue.end_epoch(), 0);
}

#[test]
fn old_only_barriers_still_publish_during_marking_and_closure() {
    let _guard = runtime_test_guard();
    for phase in [1, 2] {
        for deletion in [false, true] {
            for runtime_root in [false, true] {
                reset_internal_for_test();
                let object = willow_alloc(8);
                assert!(!runtime().tlab_ever_allocated.load(Ordering::Acquire));
                if runtime_root && deletion {
                    willow_gc_add_runtime_root(object);
                }
                assert_satb_mutation_retains(object, || {
                    GC_MARK_PHASE.store(phase, Ordering::Release);
                    if runtime_root {
                        if deletion {
                            willow_gc_remove_runtime_root(object);
                        } else {
                            willow_gc_add_runtime_root(object);
                        }
                        return;
                    }
                    let (old, value) = if deletion {
                        (object, std::ptr::null_mut())
                    } else {
                        (std::ptr::null_mut(), object)
                    };
                    willow_gc_write_barrier(
                        std::ptr::null_mut(),
                        old,
                        value,
                        GcStoreDestination::GlobalStatic as i64,
                    );
                });
                assert_eq!(
                    telemetry_heap_snapshot().0.barrier_calls,
                    u64::from(!runtime_root)
                );
            }
        }
    }
    reset_internal_for_test();
}

#[test]
fn satb_native_container_overwrites_and_removals_publish_old_references() {
    let _guard = runtime_test_guard();
    for capacity in [1, 256] {
        for case in 0..12 {
            reset_internal_for_test();
            runtime().heap.lock().unwrap().satb = satb::SatbBuffers::new(capacity);
            let old = willow_alloc(8);
            match case {
                0 | 1 => {
                    let array = crate::array::willow_array_new(1, 1);
                    crate::array::willow_array_set(array, 0, old as i64);
                    assert_satb_mutation_retains(old, || {
                        if case == 0 {
                            crate::array::willow_array_set(array, 0, 0);
                        } else {
                            assert_eq!(crate::array::willow_array_pop(array), old as i64);
                        }
                    });
                }
                2 => {
                    let map = crate::map::willow_map_new(0, 4, 1);
                    crate::map::willow_map_insert(map, 7, 0, old as i64, 1);
                    assert_satb_mutation_retains(old, || {
                        crate::map::willow_map_insert(map, 7, 0, 0, 1)
                    });
                }
                3 => {
                    let cell = crate::lock::willow_blocking_cell_new(old as i64, 1);
                    assert_satb_mutation_retains(old, || {
                        crate::lock::willow_blocking_cell_set(cell, 0)
                    });
                }
                4 => {
                    let cell = crate::lock::willow_blocking_rw_cell_new(old as i64, 1);
                    assert_satb_mutation_retains(old, || {
                        crate::lock::willow_blocking_rw_cell_write(cell, 0)
                    });
                }
                5 => {
                    let channel = crate::channel::willow_channel_new(1);
                    assert_eq!(
                        crate::channel::willow_channel_try_send_ptr(channel, old.cast()),
                        1
                    );
                    assert_satb_mutation_retains(old, || {
                        assert_eq!(crate::channel::willow_channel_recv_ptr(channel), old.cast());
                    });
                }
                6 => {
                    let arena = GcRootArena::default();
                    let handle = arena.insert(std::ptr::null_mut(), old);
                    assert_satb_mutation_retains(old, || handle.release());
                }
                7 => {
                    willow_gc_add_runtime_root(old);
                    assert_satb_mutation_retains(old, || willow_gc_remove_runtime_root(old));
                }
                8 => {
                    use crate::native_frame::{NativeFrameSpec, NativeTaskFrame};
                    struct Frame;
                    impl NativeFrameSpec for Frame {
                        const LAYOUT: willow_abi::NativeFrameLayout<'static> =
                            willow_abi::NativeFrameLayout::new(&[willow_abi::SlotKind::GcRef]);
                        const NAME: &'static str = "SATB test";
                    }
                    let frame = NativeTaskFrame::<Frame>::allocate().unwrap();
                    frame.store_gc(0, old);
                    assert_satb_mutation_retains(old, || frame.store_gc(0, std::ptr::null_mut()));
                }
                9 => {
                    let array = crate::array::willow_array_new(1, 1);
                    let old_buffer = crate::array::willow_array_reference_owner(array, 0);
                    assert_satb_mutation_retains(old_buffer, || {
                        crate::array::willow_array_push(array, 0)
                    });
                }
                10 => {
                    use crate::async_mutex::{AsyncMutex, MutexAcquire};
                    let task =
                        crate::scheduler::with_global_for_test(|s| s.spawn_parked_placeholder());
                    let cell = AsyncMutex::new(old as i64, true);
                    let MutexAcquire::Acquired(token) = cell.acquire(task) else {
                        panic!("acquire");
                    };
                    assert_satb_mutation_retains(old, || assert!(cell.commit(task, token, 0)));
                    cell.release(task, token);
                }
                _ => {
                    use crate::async_rwlock::{AsyncRwLock, RwAcquire};
                    let task =
                        crate::scheduler::with_global_for_test(|s| s.spawn_parked_placeholder());
                    let cell = AsyncRwLock::new(old as i64, true);
                    let RwAcquire::Acquired(token) =
                        cell.acquire(task, crate::lock_wait::LockAccess::Write)
                    else {
                        panic!("acquire");
                    };
                    assert_satb_mutation_retains(old, || assert!(cell.commit(task, token, 0)));
                    cell.release(task, token);
                }
            }
            willow_gc_collect();
        }
    }
    reset_internal_for_test();
}

#[test]
fn satb_repeated_equivalent_deletions_do_not_expand_mark_work() {
    let _guard = runtime_test_guard();
    for n in [16, 256, 4096] {
        reset_internal_for_test();
        let old = willow_alloc(8);
        assert_satb_mutation_retains(old, || {
            for _ in 0..n {
                satb_delete(old);
            }
            flush_satb_all_locked(&mut runtime().heap.lock().unwrap());
            let state = runtime().heap.lock().unwrap();
            assert_eq!(
                state
                    .concurrent_cycle
                    .as_ref()
                    .unwrap()
                    .queue
                    .snapshot()
                    .injected,
                1
            );
        });
        println!("satb repeated_deletions={n} unique_queue_jobs=1");
    }
    reset_internal_for_test();
}

unsafe fn trace_slot(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    slots.push(payload.cast());
}

unsafe fn snapshot_before_mutation(payload: *mut u8, children: &mut Vec<*mut u8>) {
    children.push(unsafe { load_gc_reference(payload.cast()) });
    SNAPSHOT_READY.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !MUTATION_DONE.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "mutator could not execute during marking"
        );
        std::thread::yield_now();
    }
}

#[test]
fn concurrent_edge_publication_and_new_allocation_survive_remark() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    SNAPSHOT_READY.store(false, Ordering::Relaxed);
    MUTATION_DONE.store(false, Ordering::Relaxed);
    const SNAPSHOT_TYPE: u32 = 0xFC01;
    const LEGACY_TYPE: u32 = 0xFC02;
    willow_register_type(SNAPSHOT_TYPE, trace_slot);
    willow_register_type(LEGACY_TYPE, trace_slot);
    runtime()
        .concurrent_trace_registry
        .lock()
        .unwrap()
        .insert(SNAPSHOT_TYPE, snapshot_before_mutation);
    let mut target = willow_alloc_object(SNAPSHOT_TYPE as i64, 8);
    let mut source = willow_alloc_object(LEGACY_TYPE as i64, 8);
    let child = willow_alloc(8);
    unsafe {
        store_gc_reference(source.cast(), child);
    }
    willow_push_root(&mut target);
    willow_push_root(&mut source);
    willow_gc_register_mutator();
    let ready = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let newborn = Arc::new(AtomicUsize::new(0));
    let worker = {
        let (target, source) = (target as usize, source as usize);
        let (ready, stop, newborn) = (Arc::clone(&ready), Arc::clone(&stop), Arc::clone(&newborn));
        std::thread::spawn(move || {
            willow_gc_register_mutator();
            ready.store(true, Ordering::Release);
            while !SNAPSHOT_READY.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            let target = target as *mut u8;
            let source = source as *mut u8;
            let child = unsafe { load_gc_reference(source.cast()) };
            // The marker already snapshotted target without this edge. The
            // source uses a legacy trace deferred until remark, and its edge
            // disappears now. Only insertion publication can retain child.
            willow_gc_write_barrier(
                target,
                std::ptr::null_mut(),
                child,
                GcStoreDestination::ObjectField as i64,
            );
            unsafe {
                store_gc_reference(target.cast(), child);
                store_gc_reference(source.cast(), std::ptr::null_mut());
            }
            let allocation = willow_alloc(8) as usize;
            {
                let state = runtime().heap.lock().unwrap();
                let epoch = state.concurrent_cycle.as_ref().unwrap();
                assert!(
                    epoch.objects.contains(allocation),
                    "new object reused a captured region"
                );
                assert!(
                    epoch.is_marked(allocation),
                    "allocation must publish black before its start bit"
                );
            }
            newborn.store(allocation, Ordering::Release);
            MUTATION_DONE.store(true, Ordering::Release);
            while !stop.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            willow_gc_unregister_mutator();
        })
    };
    while !ready.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    willow_gc_collect();
    stop.store(true, Ordering::Release);
    worker.join().unwrap();
    assert!(MUTATION_DONE.load(Ordering::Acquire));
    assert_eq!(unsafe { load_gc_reference(target.cast()) }, child);
    assert_eq!(willow_gc_allocated_bytes(), 4 * (GC_HEADER_SIZE + 8) as i64);
    assert_ne!(newborn.load(Ordering::Acquire), 0);
    assert!(runtime().heap.lock().unwrap().concurrent_cycle.is_none());
    willow_pop_roots(2);
    willow_gc_unregister_mutator();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn parked_native_stack_roots_survive_major_and_minor_then_resume() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let mut root = willow_alloc(8);
    let depth = gc_thread_root_depth();
    willow_push_root(&mut root);
    let token = unsafe { park_current_roots(depth) };
    assert_eq!(gc_thread_root_depth(), depth);
    willow_gc_collect();
    willow_gc_minor_collect();
    assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
    unsafe {
        resume_parked_roots(token);
    }
    assert_eq!(gc_thread_root_depth(), depth + 1);
    willow_pop_roots(1);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn memory_limit_bounds_region_reservations_and_recovers_after_sweep() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    runtime().heap.lock().unwrap().memory_limit_bytes = Some(GC_OLD_REGION_SIZE);
    let mut root = willow_alloc(8);
    assert!(!root.is_null());
    willow_push_root(&mut root);
    // One ordinary region consumes the budget. A large-object region cannot
    // exceed it, and failed reservations must not increment heap accounting.
    let before = willow_gc_allocated_bytes();
    assert!(
        allocate_old_region_object_locked(
            &mut runtime().heap.lock().unwrap(),
            0,
            0,
            GC_OLD_REGION_SIZE,
            0,
            true
        )
        .is_none()
    );
    assert_eq!(willow_gc_allocated_bytes(), before);
    assert!(can_reserve(&runtime().heap.lock().unwrap(), 0));
    willow_pop_roots(1);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    assert!(!willow_alloc(8).is_null());
    reset_internal_for_test();
}

static ASSIST_READY: AtomicBool = AtomicBool::new(false);
static ASSIST_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static ASSIST_SAW_REMARK: AtomicBool = AtomicBool::new(false);

unsafe fn hold_marker_for_assistant(_: *mut u8, _: &mut Vec<*mut u8>) {
    ASSIST_READY.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ASSIST_IN_FLIGHT.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "allocation assist never started");
        std::thread::yield_now();
    }
}

unsafe fn assist_overlaps_remark(_: *mut u8, _: &mut Vec<*mut u8>) {
    ASSIST_IN_FLIGHT.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !runtime().stop_requested.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "collector never requested final remark"
        );
        std::thread::yield_now();
    }
    ASSIST_SAW_REMARK.store(true, Ordering::Release);
}

#[test]
fn remark_waits_for_an_in_flight_mutator_assist() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    ASSIST_READY.store(false, Ordering::Relaxed);
    ASSIST_IN_FLIGHT.store(false, Ordering::Relaxed);
    ASSIST_SAW_REMARK.store(false, Ordering::Relaxed);
    for (id, callback) in [
        (0xFD01, hold_marker_for_assistant as ConcurrentTraceFn),
        (0xFD02, assist_overlaps_remark as ConcurrentTraceFn),
    ] {
        willow_register_type(id, trace_slot);
        runtime()
            .concurrent_trace_registry
            .lock()
            .unwrap()
            .insert(id, callback);
    }
    willow_register_type(0xFD03, trace_slot);
    let mut marker = willow_alloc_object(0xFD01, 8);
    let mut source = willow_alloc_object(0xFD03, 8);
    let assisted = willow_alloc_object(0xFD02, 8);
    unsafe {
        store_gc_reference(source.cast(), assisted);
    }
    willow_push_root(&mut marker);
    willow_push_root(&mut source);
    willow_gc_register_mutator();
    let ready = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let (ready, stop) = (Arc::clone(&ready), Arc::clone(&stop));
        let source = source as usize;
        std::thread::spawn(move || {
            willow_gc_register_mutator();
            ready.store(true, Ordering::Release);
            while !ASSIST_READY.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            let source = source as *mut u8;
            let child = unsafe { load_gc_reference(source.cast()) };
            let cycle = runtime()
                .heap
                .lock()
                .unwrap()
                .concurrent_cycle
                .clone()
                .unwrap();
            // Reserve a private assist job so a background worker cannot steal
            // the fixture whose purpose is to overlap a *mutator* with remark.
            assert!(cycle.objects.claim(child as usize));
            let mut consumer = cycle.queue.register_assist();
            assert!(consumer.slot().is_some());
            consumer
                .push(crate::gc_mark_queue::MarkWorkItem::Object(
                    crate::gc_mark_queue::ObjectRef::from_ptr(child).unwrap(),
                ))
                .unwrap();
            cycle.drain_worker(1, &mut Vec::new(), &mut consumer);
            drop(consumer);
            while !stop.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            willow_gc_unregister_mutator();
        })
    };
    while !ready.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    willow_gc_collect();
    stop.store(true, Ordering::Release);
    worker.join().unwrap();
    assert!(ASSIST_SAW_REMARK.load(Ordering::Acquire));
    assert!(runtime().heap.lock().unwrap().concurrent_cycle.is_none());
    willow_pop_roots(2);
    willow_gc_unregister_mutator();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn memory_limit_also_bounds_generated_tlab_reservations() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    runtime().heap.lock().unwrap().memory_limit_bytes = Some(GC_TLAB_CHUNK_SIZE);
    let mut tls = tlab_state_for_test();
    let mut root = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
    assert!(!root.is_null());
    willow_push_root(&mut root);
    let address = &mut tls as *mut GcTlabState as usize;
    assert!(allocate_tlab_chunk(&mut runtime().heap.lock().unwrap(), address).is_none());
    assert_eq!(
        runtime().heap.lock().unwrap().tlab_reserved_bytes,
        GC_TLAB_CHUNK_SIZE
    );
    willow_pop_roots(1);
    reset_internal_for_test();
}

#[test]
fn minor_collection_pins_children_when_region_budget_prevents_copying() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let mut tls = tlab_state_for_test();
    let mut parent = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 1);
    willow_push_root(&mut parent);
    let child = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
    willow_gc_write_barrier(
        parent,
        std::ptr::null_mut(),
        child,
        GcStoreDestination::ObjectField as i64,
    );
    unsafe {
        store_gc_reference(parent.cast(), child);
    }
    runtime().heap.lock().unwrap().memory_limit_bytes = Some(2 * GC_TLAB_CHUNK_SIZE);
    willow_gc_minor_collect();
    assert_eq!(unsafe { load_gc_reference(parent.cast()) }, child);
    let state = runtime().heap.lock().unwrap();
    assert_eq!(state.promoted_objects, 2);
    assert_eq!(state.moved_objects, 0);
    assert!(state.old_regions.is_empty());
    drop(state);
    willow_pop_roots(1);
    reset_internal_for_test();
}

#[test]
fn budget_exhaustion_reports_error_before_callers_can_dereference_null() {
    if let Ok(path) = std::env::var("WILLOW_TEST_GC_OOM_PATH") {
        willow_gc_init();
        if path == "old" {
            willow_alloc(8);
        } else {
            let mut tls = tlab_state_for_test();
            willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
        }
        panic!("budget exhaustion returned to an unchecked allocation caller");
    }
    for path in ["old", "tlab"] {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "gc::concurrent_tests::budget_exhaustion_reports_error_before_callers_can_dereference_null", "--nocapture"])
            .env("WILLOW_TEST_GC_OOM_PATH", path).env("WILLOW_GC_MEMORY_LIMIT", "1")
            .output().unwrap();
        assert_eq!(
            result.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            String::from_utf8_lossy(&result.stderr)
                .contains("runtime fatal: GC memory limit exceeded")
        );
    }
}

#[test]
fn reservation_pressure_collects_garbage_before_rejecting_an_allocation() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    runtime().heap.lock().unwrap().memory_limit_bytes = Some(2 * GC_OLD_REGION_SIZE);
    // A tiny garbage object occupies a regular region, leaving less reserved
    // capacity than the large request despite being below the growth trigger.
    assert!(!willow_alloc(8).is_null());
    let large = willow_alloc((GC_OLD_REGION_SIZE + GC_OLD_REGION_SIZE / 2) as i64);
    assert!(!large.is_null());
    assert_eq!(runtime().heap.lock().unwrap().total_frees, 1);
    assert!(can_reserve(&runtime().heap.lock().unwrap(), 0));
    reset_internal_for_test();
}

#[test]
fn captured_array_owner_survives_resize_and_moving_collection() {
    let _guard = runtime_test_guard();
    // Alloc stress allocates old objects; this fixture requires evacuation.
    let _stress = GcStressTestScope::normal();
    reset_internal_for_test();
    let mut tls = tlab_state_for_test();
    let mut array = crate::array::willow_array_new(1, 0);
    willow_push_root(&mut array);
    crate::array::willow_array_set(array, 0, 17);
    // Runtime arrays currently allocate old buffers. Install an equivalent
    // young buffer to prove the capture ABI remains relocation-safe when
    // allocation policy changes. Array handle word 3 is its traced buffer.
    let young = willow_gc_alloc_slow(&mut tls, 0, 0, 16, 0);
    unsafe {
        *(young.cast::<i64>()) = 1;
        *(young.cast::<i64>().add(1)) = 17;
        willow_gc_write_barrier(
            array,
            std::ptr::null_mut(),
            young,
            GcStoreDestination::ContainerInternal as i64,
        );
        store_gc_reference(array.cast::<*mut u8>().add(3), young);
    }
    let captured = crate::array::willow_array_reference_owner(array, 0);
    assert_eq!(captured, young);
    // A captured owner in a parked async frame is an interior GC edge, not a
    // pinned stack root. The collector must update it after evacuation.
    let mut frame = willow_alloc_typed(8, 1);
    willow_push_root(&mut frame);
    unsafe {
        willow_gc_write_barrier(
            frame,
            std::ptr::null_mut(),
            captured,
            GcStoreDestination::AsyncFrameSlot as i64,
        );
        store_gc_reference(frame.cast(), captured);
    }
    crate::array::willow_array_push(array, 23);
    assert_ne!(
        crate::array::willow_array_reference_owner(array, 0),
        captured
    );
    willow_gc_minor_collect();
    let relocated = unsafe { load_gc_reference(frame.cast()) };
    assert_ne!(relocated, captured, "captured owner should have evacuated");
    assert_eq!(unsafe { *relocated.cast::<i64>().add(1) }, 17);
    unsafe {
        *relocated.cast::<i64>().add(1) = 99;
    }
    assert_eq!(
        crate::array::willow_array_get(array, 0),
        17,
        "reference still names the pre-resize buffer"
    );
    willow_gc_collect();
    assert_eq!(
        unsafe { *load_gc_reference(frame.cast()).cast::<i64>().add(1) },
        99
    );
    willow_pop_roots(2);
    reset_internal_for_test();
}

// Count capacity changes at the native hook boundary, independently of the
// queue/visited-set allocations. Each case runs on a fresh marking thread.
thread_local! {
    static SCRATCH_COUNTS: std::cell::Cell<(usize, usize, usize)> = const {
        std::cell::Cell::new((0, 0, 0))
    };
}

unsafe fn snapshot_scratch_fixture(payload: *mut u8, children: &mut Vec<*mut u8>) {
    let words = payload.cast::<usize>();
    let count = unsafe { *words };
    assert!(children.is_empty(), "previous object's children leaked");
    let before = children.capacity();
    children.extend((0..count).map(|i| unsafe { *words.add(i + 1) as *mut u8 }));
    SCRATCH_COUNTS.with(|stats| {
        let (objects, slots, growths) = stats.get();
        stats.set((
            objects + 1,
            slots + count,
            growths + usize::from(children.capacity() != before),
        ));
    });
}

#[test]
fn concurrent_trace_reuses_scratch_across_bounded_drains() {
    for n in [16, 64, 256] {
        for fanout in [1, 8, 64] {
            std::thread::spawn(move || {
                // A chain closed into a cycle, with repeated equivalent edges.
                let mut payloads: Vec<_> = (0..n).map(|_| vec![0usize; fanout + 1]).collect();
                let addresses: Vec<_> = payloads
                    .iter_mut()
                    .map(|p| p.as_mut_ptr() as usize)
                    .collect();
                for (i, payload) in payloads.iter_mut().enumerate() {
                    payload[0] = fanout;
                    payload[1..].fill(addresses[(i + 1) % n]);
                }
                let cycle = ConcurrentCycle::new(
                    addresses.iter().map(|&address| {
                        (
                            address,
                            raw_heap::TraceMetadata {
                                type_id: 0xFE01,
                                layout_id: 0,
                                gc_ref_mask: 0,
                                payload_size: (fanout + 1) * GC_STORAGE_WORD_BYTES,
                            },
                        )
                    }),
                    HashSet::new(),
                    HashMap::from([(0xFE01, snapshot_scratch_fixture as ConcurrentTraceFn)]),
                    0,
                );
                cycle.enqueue(addresses[0] as *mut u8);
                while !cycle.queue.snapshot().is_drained() {
                    cycle.drain(1);
                }
                assert_eq!(cycle.objects.marked_count(), n);
                assert_eq!(cycle.queue.snapshot().injected, n as u64);
                assert_eq!(
                    cycle.work.lock().unwrap().scanned_bytes,
                    (n * fanout * GC_STORAGE_WORD_BYTES) as u64
                );
                SCRATCH_COUNTS.with(|stats| assert_eq!(stats.get(), (n, n * fanout, 1)));
                println!(
                    "objects={n} fanout={fanout} slots={} scratch_growths=1",
                    n * fanout
                );
            })
            .join()
            .unwrap();
        }
    }
}

static MARKER_THREAD_IDS: Mutex<Vec<std::thread::ThreadId>> = Mutex::new(Vec::new());

unsafe fn record_marker_thread(payload: *mut u8, children: &mut Vec<*mut u8>) {
    MARKER_THREAD_IDS
        .lock()
        .unwrap()
        .push(std::thread::current().id());
    children.push(unsafe { load_gc_reference(payload.cast()) });
}

unsafe fn failing_concurrent_trace(_: *mut u8, _: &mut Vec<*mut u8>) {
    panic!("injected concurrent trace failure");
}

#[test]
fn dedicated_marker_pool_reuses_threads_and_quiesces_before_reclamation() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    for workers in [1, 5, 8] {
        let pool = mark_workers::Pool::new(workers).unwrap();
        let mut all_threads = HashSet::new();
        for objects in [16, 128, 1024] {
            MARKER_THREAD_IDS.lock().unwrap().clear();
            let addresses: Vec<_> = (0..objects)
                .map(|_| willow_alloc_object(0xFA71, 8) as usize)
                .collect();
            for (index, &address) in addresses.iter().enumerate() {
                unsafe {
                    store_gc_reference(
                        address as *mut *mut u8,
                        addresses[(index + 1) % objects] as *mut u8,
                    )
                };
            }
            let cycle = Arc::new(ConcurrentCycle::with_index(
                epoch_index::EpochIndex::Regions(epoch_index::RegionIndex::capture(
                    &runtime().heap.lock().unwrap(),
                )),
                HashSet::new(),
                HashMap::from([(0xFA71, record_marker_thread as ConcurrentTraceFn)]),
                1,
            ));
            cycle.enqueue(addresses[0] as *mut u8);
            // No collector assistance during the bounded pool job.
            let before = crate::gc_telemetry::workers::snapshot();
            pool.run(&cycle, 0);
            let after = crate::gc_telemetry::workers::snapshot();
            assert_eq!(after.completed_jobs - before.completed_jobs, workers as u64);
            assert_eq!(
                after.cpu_samples + after.cpu_unavailable
                    - before.cpu_samples
                    - before.cpu_unavailable,
                workers as u64,
            );
            assert!(after.wall_ns >= before.wall_ns);
            assert!(!cycle.worker_failed.load(Ordering::Acquire));
            let snapshot = cycle.queue.snapshot();
            assert_eq!(snapshot.registered_workers, 0);
            assert_eq!(snapshot.registered_assists, 0);
            assert_eq!(snapshot.abandoned, 0);
            assert_eq!(cycle.active_drains.load(Ordering::Acquire), 0);
            assert_eq!(registered_mutator_count(), 0);
            let ids = MARKER_THREAD_IDS.lock().unwrap();
            let worker_traces = ids.len();
            assert!(worker_traces > 0 && worker_traces <= objects);
            assert!(ids.iter().all(|&id| id != std::thread::current().id()));
            all_threads.extend(ids.iter().copied());
            assert!(
                all_threads.len() <= workers,
                "pool recreated threads between epochs"
            );
            drop(ids);
            // Pool jobs have a shared finite budget; unused parts of claims
            // are not refunded. A chain handed between workers can exhaust
            // that budget before it is fully traced. As in collector closure,
            // drain the retained work only after every pool reader has left.
            cycle.drain(usize::MAX);
            assert_eq!(cycle.objects.marked_count(), objects);
            let snapshot = cycle.queue.snapshot();
            assert!(snapshot.is_drained());
            assert_eq!(snapshot.injected, objects as u64);
            assert_eq!(snapshot.registered_workers, 0);
            assert_eq!(snapshot.registered_assists, 0);
            assert_eq!(cycle.active_drains.load(Ordering::Acquire), 0);
            let ids = MARKER_THREAD_IDS.lock().unwrap();
            assert_eq!(ids.len(), objects);
            assert!(
                ids[worker_traces..]
                    .iter()
                    .all(|&id| id == std::thread::current().id())
            );
            drop(ids);
            assert_eq!(cycle.queue.end_epoch(), 0);
            println!(
                "marker workers={workers} objects={objects} worker_traces={worker_traces} trace_jobs={objects} readers_after=0"
            );
            // No references remain in any worker when storage is reclaimed.
            reset_internal_for_test();
        }
        drop(pool);
    }
}

#[test]
fn concurrent_trace_failure_falls_back_and_retains_the_full_graph() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    willow_register_type(0xFA72, trace_slot);
    runtime()
        .concurrent_trace_registry
        .lock()
        .unwrap()
        .insert(0xFA72, failing_concurrent_trace);
    let mut parent = willow_alloc_object(0xFA72, 8);
    let child = willow_alloc_typed(8, 1);
    let leaf = willow_alloc(8);
    unsafe {
        store_gc_reference(parent.cast(), child);
        store_gc_reference(child.cast(), leaf);
    }
    willow_push_root(&mut parent);
    let fallbacks = crate::gc_telemetry::workers::snapshot().fallback_cycles;
    let panics = crate::gc_telemetry::workers::snapshot().marker_panics;
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 3 * (GC_HEADER_SIZE + 8) as i64);
    assert_eq!(
        crate::gc_telemetry::workers::snapshot().fallback_cycles,
        fallbacks + 1
    );
    assert_eq!(
        crate::gc_telemetry::workers::snapshot().marker_panics,
        panics + 1
    );
    assert_eq!(registered_mutator_count(), 0);
    willow_pop_roots(1);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    shutdown_mark_workers();
    assert!(MARK_WORKERS.lock().unwrap().is_none());
    reset_internal_for_test();
}

#[test]
fn disabled_mark_workers_use_stopped_graph_tracing() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    shutdown_mark_workers();
    *MARK_WORKERS.lock().unwrap() = Some(mark_workers::Pool::new(0).unwrap());
    let mut root = willow_alloc_typed(8, 1);
    let child = willow_alloc_typed(8, 1);
    unsafe {
        store_gc_reference(root.cast(), child);
        store_gc_reference(child.cast(), root);
    }
    willow_push_root(&mut root);
    let before = crate::gc_telemetry::workers::snapshot().fallback_cycles;
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 2 * (GC_HEADER_SIZE + 8) as i64);
    assert_eq!(
        crate::gc_telemetry::stops::snapshot_stops().by_reason[1]
            .work
            .swept_objects,
        2
    );
    assert_eq!(
        crate::gc_telemetry::workers::snapshot().fallback_cycles,
        before + 1
    );
    willow_pop_roots(1);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    shutdown_mark_workers();
    reset_internal_for_test();
}

#[test]
fn marker_unregister_without_tlab_does_not_scan_mutator_allocation_records() {
    let _guard = runtime_test_guard();
    for count in [16usize, 64, 256] {
        reset_internal_for_test();
        let mut states: Vec<_> = (0..count).map(|_| tlab_state_for_test()).collect();
        for state in &mut states {
            assert!(!willow_gc_alloc_slow(state, 0, 0, 8, 0).is_null());
        }
        TLAB_ACCOUNTING_RECORD_VISITS.store(0, Ordering::Relaxed);
        std::thread::spawn(|| {
            willow_gc_register_mutator();
            willow_gc_unregister_mutator();
        })
        .join()
        .unwrap();
        assert_eq!(TLAB_ACCOUNTING_RECORD_VISITS.load(Ordering::Relaxed), 0);
        assert_eq!(runtime().heap.lock().unwrap().tlab_states.len(), count);
        willow_gc_unregister_mutator();
        assert_eq!(TLAB_ACCOUNTING_RECORD_VISITS.load(Ordering::Relaxed), count);
        assert!(runtime().heap.lock().unwrap().tlab_states.is_empty());
        reset_internal_for_test();
    }
}

#[test]
fn background_marker_panic_retires_consumer_and_pool_accepts_next_epoch() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    for workers in [1, 5] {
        let pool = mark_workers::Pool::new(workers).unwrap();
        let object = willow_alloc(8) as usize;
        for fail in [true, false] {
            let trace = if fail {
                failing_concurrent_trace
            } else {
                record_marker_thread
            };
            let cycle = Arc::new(ConcurrentCycle::new(
                [(
                    object,
                    raw_heap::TraceMetadata {
                        type_id: 0xFA73,
                        layout_id: 0,
                        gc_ref_mask: 0,
                        payload_size: 8,
                    },
                )],
                HashSet::new(),
                HashMap::from([(0xFA73, trace as ConcurrentTraceFn)]),
                1,
            ));
            cycle.enqueue(object as *mut u8);
            let before = crate::gc_telemetry::workers::snapshot().marker_panics;
            pool.run(&cycle, 0);
            assert_eq!(cycle.worker_failed.load(Ordering::Acquire), fail);
            assert_eq!(
                crate::gc_telemetry::workers::snapshot().marker_panics,
                before + u64::from(fail)
            );
            assert_eq!(cycle.queue.snapshot().registered_workers, 0);
            assert_eq!(registered_mutator_count(), 0);
            assert_eq!(cycle.queue.end_epoch(), 0);
        }
        drop(pool);
    }
    reset_internal_for_test();
}

static SWEEP_MUTATOR_READY: AtomicBool = AtomicBool::new(false);
static SWEEP_MUTATOR_DONE: AtomicBool = AtomicBool::new(false);

fn await_allocation_between_sweep_regions(index: usize) {
    if index != 0 {
        return;
    }
    assert!(!runtime().stop_requested.load(Ordering::Acquire));
    SWEEP_MUTATOR_READY.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !SWEEP_MUTATOR_DONE.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "mutator could not allocate between sweep regions"
        );
        std::thread::yield_now();
    }
}

#[test]
fn concurrent_sweep_retains_new_allocations_and_quarantines_processed_spans() {
    let _guard = runtime_test_guard();
    for allocate_in_pending_region in [true, false] {
        reset_internal_for_test();
        SWEEP_MUTATOR_READY.store(false, Ordering::Relaxed);
        SWEEP_MUTATOR_DONE.store(false, Ordering::Relaxed);
        let large = (GC_LARGE_OBJECT_THRESHOLD + 8) as i64;
        let old_small = if allocate_in_pending_region {
            willow_alloc(large);
            willow_alloc(8)
        } else {
            let small = willow_alloc(8);
            willow_alloc(large);
            small
        };
        let old_base = old_small as usize - GC_HEADER_SIZE;
        assert_eq!(runtime().heap.lock().unwrap().old_regions.len(), 2);
        let ready = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let address = Arc::new(AtomicUsize::new(0));
        let worker = {
            let (ready, stop, address) = (ready.clone(), stop.clone(), address.clone());
            std::thread::spawn(move || {
                willow_gc_register_mutator();
                ready.store(true, Ordering::Release);
                while !SWEEP_MUTATOR_READY.load(Ordering::Acquire) {
                    willow_gc_safepoint();
                    std::thread::yield_now();
                }
                let mut object = willow_alloc(8);
                unsafe {
                    *(object as *mut u64) = 0xAABBCCDD;
                }
                willow_push_root(&mut object);
                address.store(object as usize, Ordering::Release);
                SWEEP_MUTATOR_DONE.store(true, Ordering::Release);
                while !stop.load(Ordering::Acquire) {
                    willow_gc_safepoint();
                    std::thread::yield_now();
                }
                willow_pop_root();
                willow_gc_unregister_mutator();
            })
        };
        while !ready.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        *sweep::SWEEP_TEST_HOOK.lock().unwrap() = Some(await_allocation_between_sweep_regions);
        willow_gc_collect();
        *sweep::SWEEP_TEST_HOOK.lock().unwrap() = None;
        stop.store(true, Ordering::Release);
        worker.join().unwrap();
        let address = address.load(Ordering::Acquire);
        assert_ne!(address, 0);
        assert_eq!(
            (old_base..old_base + GC_OLD_REGION_SIZE).contains(&address),
            allocate_in_pending_region
        );
        assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
        assert_eq!(unsafe { *(address as *const u64) }, 0xAABBCCDD);
        assert!(!unsafe { (*payload_to_header(address as *mut u8)).marked });
        assert!(runtime().heap.lock().unwrap().sweeping.is_none());
        let stops = crate::gc_telemetry::stops::snapshot_stops();
        assert_eq!(stops.by_reason[1].work.swept_objects, 0);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
    }
    reset_internal_for_test();
}

fn await_reservation_wait_between_regions(index: usize) {
    if index != 0 {
        return;
    }
    assert!(!runtime().stop_requested.load(Ordering::Acquire));
    SWEEP_MUTATOR_READY.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !SWEEP_BUDGET_WAITING.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "reservation retry did not wait for pending sweep"
        );
        std::thread::yield_now();
    }
}

#[test]
fn reservation_pressure_waits_until_concurrent_sweep_releases_quarantine() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    SWEEP_MUTATOR_READY.store(false, Ordering::Relaxed);
    SWEEP_BUDGET_WAITING.store(false, Ordering::Relaxed);
    willow_alloc(8);
    willow_alloc((GC_LARGE_OBJECT_THRESHOLD + 8) as i64);
    {
        let mut state = runtime().heap.lock().unwrap();
        state.memory_limit_bytes = Some(state.old_reserved_bytes);
    }
    let ready = Arc::new(AtomicBool::new(false));
    let worker = {
        let ready = ready.clone();
        std::thread::spawn(move || {
            willow_gc_register_mutator();
            ready.store(true, Ordering::Release);
            while !SWEEP_MUTATOR_READY.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            assert!(
                allocate_old_region_object_locked(
                    &mut runtime().heap.lock().unwrap(),
                    0,
                    0,
                    8,
                    0,
                    true
                )
                .is_none()
            );
            collect_for_budget();
            let object = allocate_old_region_object_locked(
                &mut runtime().heap.lock().unwrap(),
                0,
                0,
                8,
                0,
                true,
            )
            .expect("sweep must free reservations before retry");
            let address = object.payload().as_ptr() as usize;
            willow_gc_unregister_mutator();
            address
        })
    };
    while !ready.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    *sweep::SWEEP_TEST_HOOK.lock().unwrap() = Some(await_reservation_wait_between_regions);
    willow_gc_collect();
    *sweep::SWEEP_TEST_HOOK.lock().unwrap() = None;
    let address = worker.join().unwrap();
    assert!(SWEEP_BUDGET_WAITING.load(Ordering::Acquire));
    assert_eq!(
        payload_generation(&runtime().heap.lock().unwrap(), address as *mut u8),
        Some(GC_GENERATION_OLD)
    );
    assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn concurrent_sweep_excludes_new_active_and_retired_tlab_chunks() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    SWEEP_MUTATOR_READY.store(false, Ordering::Relaxed);
    SWEEP_MUTATOR_DONE.store(false, Ordering::Relaxed);
    willow_alloc((GC_LARGE_OBJECT_THRESHOLD + 8) as i64);
    let ready = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let (ready, stop) = (ready.clone(), stop.clone());
        std::thread::spawn(move || {
            willow_gc_register_mutator();
            let mut tls = tlab_state_for_test();
            ready.store(true, Ordering::Release);
            while !SWEEP_MUTATOR_READY.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            let first = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0) as usize;
            let second = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0) as usize;
            SWEEP_MUTATOR_DONE.store(true, Ordering::Release);
            while !stop.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            willow_gc_unregister_mutator();
            [first, second]
        })
    };
    while !ready.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    *sweep::SWEEP_TEST_HOOK.lock().unwrap() = Some(await_allocation_between_sweep_regions);
    willow_gc_collect();
    *sweep::SWEEP_TEST_HOOK.lock().unwrap() = None;
    stop.store(true, Ordering::Release);
    let addresses = worker.join().unwrap();
    let state = runtime().heap.lock().unwrap();
    assert_eq!(state.tlab_chunks.len(), 2);
    for address in addresses {
        assert_eq!(
            payload_generation(&state, address as *mut u8),
            Some(GC_GENERATION_YOUNG)
        );
    }
    drop(state);
    assert_eq!(willow_gc_allocated_bytes(), 2 * (GC_HEADER_SIZE + 8) as i64);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn reservation_retry_waits_for_election_before_phase_publication() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    ELECTION_BUDGET_WAITING.store(false, Ordering::Relaxed);
    let elected = runtime().collect_lock.lock().unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let worker = {
        let done = done.clone();
        std::thread::spawn(move || {
            willow_gc_register_mutator();
            collect_for_budget();
            done.store(true, Ordering::Release);
            willow_gc_unregister_mutator();
        })
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ELECTION_BUDGET_WAITING.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "retry returned before observing collector election"
        );
        std::thread::yield_now();
    }
    assert!(!done.load(Ordering::Acquire));
    drop(elected);
    worker.join().unwrap();
    assert!(done.load(Ordering::Acquire));
    reset_internal_for_test();
}

static SATB_MUTATORS_READY: AtomicBool = AtomicBool::new(false);
static SATB_MUTATORS_DONE: AtomicUsize = AtomicUsize::new(0);
static SATB_MUTATORS_EXPECTED: AtomicUsize = AtomicUsize::new(0);

unsafe fn hold_snapshot_for_deleting_mutators(_: *mut u8, _: &mut Vec<*mut u8>) {
    SATB_MUTATORS_READY.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(10);
    while SATB_MUTATORS_DONE.load(Ordering::Acquire)
        != SATB_MUTATORS_EXPECTED.load(Ordering::Relaxed)
    {
        assert!(
            Instant::now() < deadline,
            "registered mutators could not delete during marking"
        );
        std::thread::yield_now();
    }
}

#[test]
fn satb_parallel_mutators_retain_deleted_subgraphs_across_phase_flips() {
    let _guard = runtime_test_guard();
    for count in [1usize, 5, 16] {
        for capacity in [1, 256] {
            reset_internal_for_test();
            runtime().heap.lock().unwrap().satb = satb::SatbBuffers::new(capacity);
            SATB_MUTATORS_READY.store(false, Ordering::Relaxed);
            SATB_MUTATORS_DONE.store(0, Ordering::Relaxed);
            SATB_MUTATORS_EXPECTED.store(count, Ordering::Relaxed);
            const CONTROL: u32 = 0xFA81;
            const SOURCE: u32 = 0xFA82;
            willow_register_type(CONTROL, trace_slot);
            willow_register_type(SOURCE, trace_slot);
            runtime()
                .concurrent_trace_registry
                .lock()
                .unwrap()
                .insert(CONTROL, hold_snapshot_for_deleting_mutators);
            let mut roots = Vec::with_capacity(count + 1);
            roots.push(willow_alloc_object(CONTROL as i64, 8));
            for _ in 0..count {
                let source = willow_alloc_object(SOURCE as i64, 8);
                let child = willow_alloc_typed(8, 1);
                let leaf = willow_alloc(8);
                unsafe {
                    store_gc_reference(source.cast(), child);
                    store_gc_reference(child.cast(), leaf);
                }
                roots.push(source);
            }
            for root in &mut roots {
                willow_push_root(root);
            }
            willow_gc_register_mutator();
            let ready = Arc::new(AtomicUsize::new(0));
            let mut workers = Vec::with_capacity(count);
            for (index, &source) in roots[1..].iter().enumerate() {
                let source = source as usize;
                let ready = ready.clone();
                workers.push(std::thread::spawn(move || {
                    willow_gc_register_mutator();
                    ready.fetch_add(1, Ordering::Release);
                    while !SATB_MUTATORS_READY.load(Ordering::Acquire) {
                        willow_gc_safepoint();
                        std::thread::yield_now();
                    }
                    let source = source as *mut u8;
                    let old = unsafe { load_gc_reference(source.cast()) };
                    willow_gc_write_barrier(
                        source,
                        old,
                        std::ptr::null_mut(),
                        GcStoreDestination::ObjectField as i64,
                    );
                    unsafe { store_gc_reference(source.cast(), std::ptr::null_mut()) };
                    if index % 2 == 0 {
                        willow_gc_safepoint();
                    }
                    willow_gc_unregister_mutator();
                    SATB_MUTATORS_DONE.fetch_add(1, Ordering::Release);
                }));
            }
            while ready.load(Ordering::Acquire) != count {
                std::thread::yield_now();
            }
            willow_gc_collect();
            for worker in workers {
                worker.join().unwrap();
            }
            assert_eq!(
                willow_gc_allocated_bytes(),
                ((1 + 3 * count) * (GC_HEADER_SIZE + 8)) as i64
            );
            willow_gc_collect();
            assert_eq!(
                willow_gc_allocated_bytes(),
                ((1 + count) * (GC_HEADER_SIZE + 8)) as i64
            );
            willow_pop_roots(roots.len() as i32);
            willow_gc_unregister_mutator();
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), 0);
            println!(
                "satb mutators={count} capacity={capacity} retained_deleted={} subsequent_reclaimed={}",
                2 * count,
                2 * count
            );
        }
    }
    reset_internal_for_test();
}

#[test]
fn satb_owned_batches_bound_queue_items_and_assist_overshoot() {
    for capacity in [1, 256, 65536] {
        for count in [1usize, 16, 257, 4096] {
            let cycle = ConcurrentCycle::new(
                (1..=count).map(|address| {
                    (
                        address,
                        raw_heap::TraceMetadata {
                            type_id: 0,
                            layout_id: 0,
                            gc_ref_mask: 0,
                            payload_size: 0,
                        },
                    )
                }),
                HashSet::new(),
                HashMap::new(),
                0,
            );
            let mut buffers = satb::SatbBuffers::new(capacity);
            let id = std::thread::current().id();
            for address in 1..=count {
                buffers.record(id, address, |values| cycle.enqueue_satb_batch(values));
            }
            buffers.flush_thread(id, true, |values| cycle.enqueue_satb_batch(values));
            let expected =
                (count / capacity) * capacity.div_ceil(32) + (count % capacity).div_ceil(32);
            assert_eq!(cycle.queue.snapshot().injected, expected as u64);
            // Equivalent deletions cannot allocate another trace item.
            cycle.enqueue_satb_batch(&(1..=count).collect::<Vec<_>>());
            assert_eq!(cycle.queue.snapshot().injected, expected as u64);
            assert_eq!(cycle.objects.marked_count(), count);
            let mut consumer = cycle.queue.register_assist();
            let mut traced = 0;
            while traced < count {
                let n = cycle.drain_worker(1, &mut Vec::new(), &mut consumer).0;
                assert!(
                    (1..=32).contains(&n),
                    "one-object assist exceeded one bounded SATB batch"
                );
                traced += n;
            }
            drop(consumer);
            assert_eq!(traced, count);
            assert_eq!(
                cycle.work.lock().unwrap().marked_bytes,
                (count * GC_HEADER_SIZE) as u64
            );
            assert!(cycle.queue.snapshot().is_drained());
            assert_eq!(cycle.queue.end_epoch(), 0);
            println!(
                "satb capacity={capacity} unique={count} items={expected} max_assist_objects=32"
            );
        }
    }
}

#[test]
fn assist_work_credit_counts_only_each_consumers_actual_work() {
    for threads in [1, 5, 16] {
        for count in [16usize, 256, 4096] {
            let cycle = Arc::new(ConcurrentCycle::new(
                (1..=count).map(|address| {
                    (
                        address,
                        raw_heap::TraceMetadata {
                            type_id: 0,
                            layout_id: 0,
                            gc_ref_mask: 0,
                            payload_size: 0,
                        },
                    )
                }),
                HashSet::new(),
                HashMap::new(),
                0,
            ));
            cycle.enqueue_satb_batch(&(1..=count).collect::<Vec<_>>());
            let credited = std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for _ in 0..threads {
                    let cycle = cycle.clone();
                    handles.push(scope.spawn(move || {
                        let mut total = 0;
                        loop {
                            let work = cycle.drain_checked(8);
                            if work == 0 {
                                break;
                            }
                            total += work;
                        }
                        total
                    }));
                }
                handles
                    .into_iter()
                    .map(|handle| handle.join().unwrap())
                    .sum::<u64>()
            });
            assert_eq!(credited, (count * GC_HEADER_SIZE) as u64);
            assert_eq!(cycle.drain_checked(8), 0);
            assert_eq!(cycle.active_drains.load(Ordering::Acquire), 0);
            assert!(cycle.queue.snapshot().is_drained());
            assert_eq!(cycle.queue.end_epoch(), 0);
            println!("assist threads={threads} objects={count} credited_bytes={credited}");
        }
    }
}

#[test]
fn expired_assist_budget_publishes_unfinished_satb_tails_before_returning() {
    for count in [1usize, 16, 32, 256] {
        let cycle = ConcurrentCycle::new(
            (1..=count).map(|address| {
                (
                    address,
                    raw_heap::TraceMetadata {
                        type_id: 0,
                        layout_id: 0,
                        gc_ref_mask: 0,
                        payload_size: 0,
                    },
                )
            }),
            HashSet::new(),
            HashMap::new(),
            0,
        );
        cycle.enqueue_satb_batch(&(1..=count).collect::<Vec<_>>());
        let mut consumer = cycle.queue.register_assist();
        let mut credited = 0;
        for completed in 1..=count {
            let (objects, work) =
                cycle.drain_worker_until(8, &mut Vec::new(), &mut consumer, Some(Instant::now()));
            assert_eq!(objects, 1);
            credited += work;
            assert_eq!(cycle.queue.snapshot().is_drained(), completed == count);
            assert_eq!(cycle.active_drains.load(Ordering::Acquire), 0);
        }
        assert_eq!(credited, (count * GC_HEADER_SIZE) as u64);
        assert_eq!(cycle.queue.snapshot().injected, count as u64);
        drop(consumer);
        assert_eq!(cycle.queue.end_epoch(), 0);
    }
}

#[test]
fn large_bitmap_continuations_bound_scan_work_without_rescanning_prefixes() {
    for slots in [65usize, 513, 4097, 65_537] {
        let payload: Vec<AtomicUsize> = (0..slots).map(|_| AtomicUsize::new(1)).collect();
        let bitmap_words = slots.div_ceil(64);
        let mut descriptor = vec![u64::MAX; bitmap_words + 1];
        descriptor[0] = bitmap_words as u64;
        let root = payload.as_ptr() as usize;
        let cycle = ConcurrentCycle::new(
            [
                (
                    root,
                    raw_heap::TraceMetadata {
                        type_id: willow_abi::GC_BITMAP_TYPE_ID,
                        layout_id: descriptor.as_ptr() as u64,
                        gc_ref_mask: u64::MAX,
                        payload_size: slots * GC_STORAGE_WORD_BYTES,
                    },
                ),
                (
                    1,
                    raw_heap::TraceMetadata {
                        type_id: 0,
                        layout_id: 0,
                        gc_ref_mask: 0,
                        payload_size: 0,
                    },
                ),
            ],
            HashSet::new(),
            HashMap::new(),
            0,
        );
        cycle.enqueue(root as *mut u8);
        let mut consumer = cycle.queue.register_assist();
        let mut jobs = 0;
        while !cycle.queue.snapshot().is_drained() {
            let before = *cycle.work.lock().unwrap();
            let (processed, _) = cycle.drain_worker(1, &mut Vec::new(), &mut consumer);
            assert_eq!(processed, 1);
            let after = *cycle.work.lock().unwrap();
            assert!(after.scanned_bytes - before.scanned_bytes <= 512 * 8);
            assert!(after.descriptor_bytes - before.descriptor_bytes <= 8 * 8);
            jobs += 1;
        }
        assert_eq!(jobs, 2 + (bitmap_words - 1).div_ceil(8));
        let work = *cycle.work.lock().unwrap();
        assert_eq!(work.scanned_bytes, (slots * 8) as u64);
        assert_eq!(work.descriptor_bytes, ((bitmap_words - 1) * 8) as u64);
        assert_eq!(work.marked_bytes, (2 * GC_HEADER_SIZE + slots * 8) as u64);
        assert_eq!(cycle.queue.end_epoch(), 0);
        println!(
            "bitmap slots={slots} descriptor_words={bitmap_words} jobs={jobs} max_slots_per_job=512"
        );
    }
    assert!(
        std::mem::size_of::<crate::gc_mark_queue::MarkWorkItem>()
            <= 3 * std::mem::size_of::<usize>()
    );
}

#[test]
fn native_reference_arrays_use_bounded_continuations_in_the_real_engine() {
    let _guard = runtime_test_guard();
    for slots in [65usize, 513, 4097, 65_537] {
        reset_internal_for_test();
        let mut array = crate::array::willow_array_new(slots as i64, 1);
        willow_push_root(&mut array);
        let child = willow_alloc(0);
        for slot in 0..slots {
            crate::array::willow_array_set(array, slot as i64, child as i64);
        }
        let (_, cycle) = root_handshake::begin();
        let mut consumer = cycle.queue.register_assist();
        let mut jobs = 0;
        while !cycle.queue.snapshot().is_drained() {
            let before = cycle.work.lock().unwrap().scanned_bytes;
            let (processed, _) = cycle.drain_worker(1, &mut Vec::new(), &mut consumer);
            assert_eq!(processed, 1);
            let after = cycle.work.lock().unwrap().scanned_bytes;
            assert!(after - before <= 512 * 8);
            jobs += 1;
        }
        assert_eq!(jobs, 2 + slots.div_ceil(512));
        assert_eq!(
            cycle.work.lock().unwrap().scanned_bytes,
            ((slots + 1) * 8) as u64
        );
        assert!(cycle.deferred.lock().unwrap().is_empty());
        drop(consumer);
        assert_eq!(cycle.queue.end_epoch(), 0);
        runtime().heap.lock().unwrap().concurrent_cycle = None;
        GC_MARK_PHASE.store(0, Ordering::Release);
        willow_pop_root();
        println!("native array slots={slots} jobs={jobs} max_slots_per_job=512");
    }
    reset_internal_for_test();
}

#[test]
fn array_shrink_between_slices_preserves_unscanned_deleted_references() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let mut array = crate::array::willow_array_new(1024, 1);
    willow_push_root(&mut array);
    let mut first = willow_alloc(8);
    willow_push_root(&mut first);
    let mut deleted = willow_alloc(8);
    willow_push_root(&mut deleted);
    for slot in 0..1024 {
        crate::array::willow_array_set(
            array,
            slot,
            if slot < 512 { first } else { deleted } as i64,
        );
    }
    willow_pop_roots(2);
    let (_, cycle) = root_handshake::begin();
    let mut consumer = cycle.queue.register_assist();
    assert_eq!(cycle.drain_worker(2, &mut Vec::new(), &mut consumer).0, 2);
    assert!(!cycle.is_marked(deleted as usize));
    for _ in 0..512 {
        assert_eq!(crate::array::willow_array_pop(array), deleted as i64);
    }
    drop(consumer);
    let (_, plan) =
        mark_closure::finish(&cycle, Instant::now()).expect("native slices close concurrently");
    assert!(cycle.is_marked(deleted as usize));
    sweep::concurrent(plan);
    let retained = willow_gc_allocated_bytes();
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        retained - (GC_HEADER_SIZE + 8) as i64
    );
    assert_eq!(crate::array::willow_array_get(array, 511), first as i64);
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn map_growth_during_slices_has_a_finite_cursor_and_preserves_deleted_values() {
    let _guard = runtime_test_guard();
    for count in [513usize, 4097, 65_537] {
        reset_internal_for_test();
        let mut map = crate::map::willow_map_new(0, 0, 1);
        willow_push_root(&mut map);
        let mut first = willow_alloc(8);
        willow_push_root(&mut first);
        let mut deleted = willow_alloc(8);
        willow_push_root(&mut deleted);
        for key in 0..count {
            crate::map::willow_map_insert(
                map,
                key as i64,
                0,
                if key == 512 { deleted } else { first } as i64,
                1,
            );
        }
        willow_pop_roots(2);
        let (_, cycle) = root_handshake::begin();
        let mut consumer = cycle.queue.register_assist();
        assert_eq!(cycle.drain_worker(1, &mut Vec::new(), &mut consumer).0, 1);
        assert_eq!(cycle.work.lock().unwrap().scanned_bytes, 512 * 8);
        assert!(!cycle.is_marked(deleted as usize));
        let replacement = willow_alloc(8);
        crate::map::willow_map_insert(map, 512, 0, replacement as i64, 1);
        for key in count..2 * count {
            crate::map::willow_map_insert(map, key as i64, 0, first as i64, 1);
        }
        drop(consumer);
        let (work, plan) = mark_closure::finish(&cycle, Instant::now()).unwrap();
        assert_eq!(work.scanned_bytes, count as u64 * 8);
        assert!(cycle.is_marked(deleted as usize));
        sweep::concurrent(plan);
        let retained = willow_gc_allocated_bytes();
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            retained - (GC_HEADER_SIZE + 8) as i64
        );
        assert_eq!(crate::map::willow_map_get(map, 512, 0, 1), replacement);
        assert_eq!(crate::map::willow_map_len(map), (2 * count) as i64);
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        println!("map original={count} appended={count} scanned_slots={count}");
    }
    reset_internal_for_test();
}

#[test]
fn nested_scope_remembers_young_frames_without_an_independent_child_root() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let mut parent = crate::cancellation::willow_task_scope_new();
    willow_push_root(&mut parent);
    let child = crate::cancellation::willow_task_scope_child(parent);
    let mut tls = tlab_state_for_test();
    let frame = willow_gc_alloc_slow(&mut tls, 1, 0, 128, 0);
    unsafe {
        *frame.cast::<u64>() = 1234;
        *frame
            .add(crate::async_frame::async_frame_slot_offset(1))
            .cast::<u64>() = 42;
    }
    crate::cancellation::willow_task_scope_add(child, frame);
    assert!(
        runtime()
            .heap
            .lock()
            .unwrap()
            .remembered_set
            .contains(&(child as usize))
    );
    willow_gc_minor_collect();
    let trace = type_registry().lock().unwrap()[&willow_abi::runtime_type_ids::TASK_SCOPE_TYPE_ID];
    let mut slots = Vec::new();
    unsafe { trace(child, &mut slots) };
    assert_eq!(slots.len(), 1);
    let retained = unsafe { *slots[0] };
    assert_ne!(
        retained, frame,
        "unrooted young frame is evacuated through its stable cell"
    );
    assert_eq!(unsafe { *retained.cast::<u64>() }, 1234);
    assert_eq!(runtime().heap.lock().unwrap().young_allocated_bytes, 0);
    willow_gc_collect();
    assert_eq!(unsafe { *retained.cast::<u64>() }, 1234);
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn task_scope_graph_traces_each_scope_and_frame_once() {
    let _guard = runtime_test_guard();
    for fan_out in [false, true] {
        for count in [16usize, 256, 4096] {
            reset_internal_for_test();
            let mut handles = vec![std::ptr::null_mut(); count];
            for index in 0..count {
                handles[index] = if index == 0 {
                    crate::cancellation::willow_task_scope_new()
                } else {
                    crate::cancellation::willow_task_scope_child(
                        handles[if fan_out { 0 } else { index - 1 }],
                    )
                };
                willow_push_root(&mut handles[index]);
                let frame = crate::async_frame::willow_async_frame_alloc(2, 0).cast::<u8>();
                unsafe {
                    *frame
                        .add(crate::async_frame::async_frame_slot_offset(1))
                        .cast::<u64>() = (index + 1) as u64;
                }
                crate::cancellation::willow_task_scope_add(handles[index], frame);
            }
            // Every handle is independently rooted: a transitive hook would
            // visit N(N+1)/2 scopes on the chain, instead of N scopes here.
            let (_, cycle) = root_handshake::begin();
            let mut consumer = cycle.queue.register_assist();
            let mut jobs = 0;
            while !cycle.queue.snapshot().is_drained() {
                let before = cycle.work.lock().unwrap().scanned_bytes;
                assert_eq!(cycle.drain_worker(1, &mut Vec::new(), &mut consumer).0, 1);
                let after = cycle.work.lock().unwrap().scanned_bytes;
                assert!(after - before <= 512 * 8);
                jobs += 1;
            }
            drop(consumer);
            let (work, plan) = mark_closure::finish(&cycle, Instant::now()).unwrap();
            assert_eq!(work.scanned_bytes, ((2 * count - 1) * 8) as u64);
            let expected_jobs = 2 * count + if fan_out { count.div_ceil(512) - 1 } else { 0 };
            assert_eq!(jobs, expected_jobs);
            sweep::concurrent(plan);
            let retained = willow_gc_allocated_bytes();
            willow_pop_roots(count as i32);
            willow_push_root(&mut handles[0]);
            willow_gc_minor_collect();
            willow_gc_collect();
            assert_eq!(
                willow_gc_allocated_bytes(),
                retained,
                "parent retains descendants"
            );
            willow_pop_root();
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), 0);
            println!(
                "scope fan_out={fan_out} scopes={count} frames={count} scanned_slots={} jobs={jobs}",
                2 * count - 1
            );
        }
    }
    reset_internal_for_test();
}

#[test]
fn channel_pops_and_appends_do_not_shift_or_extend_the_snapshot() {
    let _guard = runtime_test_guard();
    for count in [513usize, 4097, 65_537] {
        reset_internal_for_test();
        let mut channel = crate::channel::willow_channel_new(1).cast::<u8>();
        willow_push_root(&mut channel);
        let mut first = willow_alloc(8);
        willow_push_root(&mut first);
        let mut deleted = willow_alloc(8);
        willow_push_root(&mut deleted);
        for index in 0..count {
            crate::channel::willow_channel_send_ptr(
                channel.cast(),
                if index == 512 { deleted } else { first }.cast(),
            );
        }
        willow_pop_roots(2);
        let (_, cycle) = root_handshake::begin();
        let mut consumer = cycle.queue.register_assist();
        assert_eq!(cycle.drain_worker(1, &mut Vec::new(), &mut consumer).0, 1);
        assert_eq!(cycle.work.lock().unwrap().scanned_bytes, 512 * 8);
        assert!(!cycle.is_marked(deleted as usize));
        for index in 0..513 {
            assert_eq!(
                crate::channel::willow_channel_recv_ptr(channel.cast()).cast::<u8>(),
                if index == 512 { deleted } else { first }
            );
        }
        for _ in 0..count {
            crate::channel::willow_channel_send_ptr(channel.cast(), first.cast());
        }
        drop(consumer);
        let (work, plan) = mark_closure::finish(&cycle, Instant::now()).unwrap();
        assert_eq!(work.scanned_bytes, (count - 1) as u64 * 8);
        assert!(cycle.is_marked(deleted as usize));
        sweep::concurrent(plan);
        let retained = willow_gc_allocated_bytes();
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            retained - (GC_HEADER_SIZE + 8) as i64
        );
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        println!(
            "channel original={count} popped=513 appended={count} scanned_slots={}",
            count - 1
        );
    }
    reset_internal_for_test();
}

#[test]
fn root_handshake_resumes_early_mutator_and_replays_unretired_tlab_deletion() {
    let _guard = runtime_test_guard();
    for unregister in [false, true] {
        reset_internal_for_test();
        runtime().heap.lock().unwrap().satb = satb::SatbBuffers::new(1);
        willow_gc_register_mutator();
        let mut parent = willow_alloc_typed(8, 1);
        let grandchild = willow_alloc(8) as usize;
        willow_push_root(&mut parent);
        let deleted = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let slow = {
            let (deleted, finished) = (deleted.clone(), finished.clone());
            std::thread::spawn(move || {
                willow_gc_register_mutator();
                let mut tls = tlab_state_for_test();
                let child = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 1);
                unsafe {
                    store_gc_reference(child.cast(), grandchild as *mut u8);
                }
                tx.send(child as usize).unwrap();
                // No safepoint until the fast peer has resumed and deleted its
                // edge. An all-mutator parking protocol would deadlock here.
                let deadline = Instant::now() + Duration::from_secs(10);
                while !deleted.load(Ordering::Acquire) {
                    assert!(
                        Instant::now() < deadline,
                        "early root publisher did not resume"
                    );
                    std::thread::yield_now();
                }
                if unregister {
                    willow_gc_unregister_mutator();
                } else {
                    willow_gc_safepoint();
                    while !finished.load(Ordering::Acquire) {
                        willow_gc_safepoint();
                        std::thread::yield_now();
                    }
                    willow_gc_unregister_mutator();
                }
            })
        };
        let child = rx.recv().unwrap();
        unsafe {
            store_gc_reference(parent.cast(), child as *mut u8);
        }
        let ready = Arc::new(AtomicBool::new(false));
        let fast = {
            let parent = parent as usize;
            let (ready, deleted, finished) = (ready.clone(), deleted.clone(), finished.clone());
            std::thread::spawn(move || {
                willow_gc_register_mutator();
                ready.store(true, Ordering::Release);
                while !runtime().poll_requested.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                willow_gc_safepoint();
                assert!(!runtime().stop_requested.load(Ordering::Acquire));
                let epoch = runtime()
                    .heap
                    .lock()
                    .unwrap()
                    .concurrent_cycle
                    .clone()
                    .unwrap();
                assert!(
                    epoch.objects.contains(child),
                    "slow allocation must publish its start bit"
                );
                assert!(!epoch.is_marked(child));
                assert!(
                    runtime()
                        .heap
                        .lock()
                        .unwrap()
                        .tlab_chunks
                        .iter()
                        .any(|chunk| chunk.owner_state.is_some()),
                    "slow owner was not yet allowed to retire"
                );
                willow_gc_write_barrier(
                    parent as *mut u8,
                    child as *mut u8,
                    std::ptr::null_mut(),
                    GcStoreDestination::ObjectField as i64,
                );
                unsafe {
                    store_gc_reference(parent as *mut *mut u8, std::ptr::null_mut());
                }
                assert!(epoch.is_marked(child), "pre-retirement deletion was lost");
                deleted.store(true, Ordering::Release);
                while !finished.load(Ordering::Acquire) {
                    willow_gc_safepoint();
                    std::thread::yield_now();
                }
                willow_gc_unregister_mutator();
            })
        };
        while !ready.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        willow_gc_collect();
        finished.store(true, Ordering::Release);
        fast.join().unwrap();
        slow.join().unwrap();
        assert_eq!(willow_gc_allocated_bytes(), 3 * (GC_HEADER_SIZE + 8) as i64);
        let stops = crate::gc_telemetry::stops::snapshot_stops();
        assert_eq!(stops.by_reason[0].requests, 0);
        assert_eq!(stops.by_reason[1].requests, 0);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
        willow_pop_root();
        willow_gc_unregister_mutator();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        println!(
            "root_handshake unregister={unregister} early_mutator_progress=true late_tlab_satb_retained=2 initial_stops=0"
        );
    }
    reset_internal_for_test();
}

#[test]
fn root_handshake_tlab_owner_index_avoids_mutator_squared_accounting() {
    let _guard = runtime_test_guard();
    for count in [1, 5, 16] {
        reset_internal_for_test();
        willow_gc_register_mutator();
        let ready = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let workers: Vec<_> = (0..count)
            .map(|_| {
                let (ready, done) = (ready.clone(), done.clone());
                std::thread::spawn(move || {
                    willow_gc_register_mutator();
                    let mut tls = tlab_state_for_test();
                    let mut object = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
                    willow_push_root(&mut object);
                    ready.fetch_add(1, Ordering::Release);
                    while !done.load(Ordering::Acquire) {
                        willow_gc_safepoint();
                        std::thread::yield_now();
                    }
                    willow_pop_root();
                    willow_gc_unregister_mutator();
                })
            })
            .collect();
        while ready.load(Ordering::Acquire) != count {
            std::thread::yield_now();
        }
        TLAB_ACCOUNTING_RECORD_VISITS.store(0, Ordering::Relaxed);
        root_handshake::ACTIVATION_ACKS.store(0, Ordering::Relaxed);
        root_handshake::ROOT_ACKS.store(0, Ordering::Relaxed);
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            (count * (GC_HEADER_SIZE + 8)) as i64
        );
        // Capture accounting, per-owner publication, final accounting and the
        // allocated-bytes snapshot immediately above. No remark scan remains.
        assert_eq!(
            TLAB_ACCOUNTING_RECORD_VISITS.load(Ordering::Relaxed),
            4 * count
        );
        assert_eq!(
            crate::gc_telemetry::stops::snapshot_stops().by_reason[0].requests,
            0
        );
        assert_eq!(
            root_handshake::ACTIVATION_ACKS.load(Ordering::Relaxed),
            count + 1
        );
        assert_eq!(root_handshake::ROOT_ACKS.load(Ordering::Relaxed), count + 1);
        println!(
            "activation participants={} acknowledgements={} root_snapshots={}",
            count + 1,
            count + 1,
            count + 1
        );
        done.store(true, Ordering::Release);
        for worker in workers {
            worker.join().unwrap();
        }
        willow_gc_unregister_mutator();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        println!(
            "root_handshake mutators={count} accounting_record_reads={} initial_stops=0",
            4 * count
        );
    }
    reset_internal_for_test();
}

#[test]
fn native_stack_root_transfers_publish_during_initial_handshake() {
    let _guard = runtime_test_guard();
    for resume_during_mark in [false, true] {
        reset_internal_for_test();
        let mut root = willow_alloc_typed(8, 1);
        let child = willow_alloc(8);
        unsafe {
            store_gc_reference(root.cast(), child);
        }
        let depth = gc_thread_root_depth();
        willow_push_root(&mut root);
        if resume_during_mark {
            let token = unsafe { park_current_roots(depth) };
            assert_satb_mutation_retains(root, || unsafe { resume_parked_roots(token) });
        } else {
            let mut token = 0;
            assert_satb_mutation_retains(root, || {
                token = unsafe { park_current_roots(depth) };
            });
            unsafe {
                resume_parked_roots(token);
            }
        }
        assert_eq!(gc_thread_root_depth(), depth + 1);
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
    }
    reset_internal_for_test();
}

#[test]
fn registered_peers_do_not_hide_an_unregistered_foreign_root_owner() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let mut root = willow_alloc(8);
    willow_push_root(&mut root); // Deliberately legacy/unregistered owner.
    let ready = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let peer = {
        let (ready, done) = (ready.clone(), done.clone());
        std::thread::spawn(move || {
            willow_gc_register_mutator();
            ready.store(true, Ordering::Release);
            while !done.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            willow_gc_unregister_mutator();
        })
    };
    while !ready.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    std::thread::spawn(|| {
        willow_gc_register_mutator();
        assert!(multi_mutator_active());
        willow_gc_collect();
        willow_gc_minor_collect();
        willow_gc_unregister_mutator();
    })
    .join()
    .unwrap();
    done.store(true, Ordering::Release);
    peer.join().unwrap();
    assert_eq!(
        runtime()
            .skipped_foreign_owner_collections
            .load(Ordering::Relaxed),
        2
    );
    assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
    // Registration of that same owner makes its root available to handshakes.
    willow_gc_register_mutator();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
    willow_gc_unregister_mutator();
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn concurrent_closure_waits_for_batched_work_exceptions_and_unwind_publication() {
    let _guard = runtime_test_guard();
    for outcome in [0, 1, 2] {
        reset_internal_for_test();
        mark_closure::READER_WAIT_OBSERVED.store(false, Ordering::Relaxed);
        let object = willow_alloc(8) as usize;
        let cycle = {
            let mut state = runtime().heap.lock().unwrap();
            let cycle = Arc::new(ConcurrentCycle::with_index(
                epoch_index::EpochIndex::Regions(epoch_index::RegionIndex::capture(&state)),
                HashSet::new(),
                HashMap::new(),
                0,
            ));
            assert!(cycle.objects.claim(object));
            state.concurrent_cycle = Some(cycle.clone());
            GC_MARK_PHASE.store(1, Ordering::Release);
            cycle
        };
        let ready = Arc::new(AtomicBool::new(false));
        let assist = {
            let (cycle, ready) = (cycle.clone(), ready.clone());
            std::thread::spawn(move || {
                let result = std::panic::catch_unwind(|| {
                    // Model a drain that has completed its queue item but still
                    // owns unpublished accounting or an exception batch.
                    cycle.active_drains.fetch_add(1, Ordering::AcqRel);
                    let mut batch = MarkBatch {
                        cpu_start: None,
                        cycle: &cycle,
                        work: Default::default(),
                        deferred: Vec::new(),
                        unindexed: HashSet::new(),
                    };
                    batch.work.object(GC_HEADER_SIZE + 8, 0);
                    if outcome == 1 {
                        batch.deferred.push(object);
                    }
                    ready.store(true, Ordering::Release);
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !mark_closure::READER_WAIT_OBSERVED.load(Ordering::Acquire) {
                        assert!(
                            Instant::now() < deadline,
                            "closure ignored an unpublished engine batch"
                        );
                        std::thread::yield_now();
                    }
                    assert!(!runtime().stop_requested.load(Ordering::Acquire));
                    assert!(runtime().heap.lock().unwrap().concurrent_cycle.is_some());
                    if outcome == 2 {
                        panic!("assist failed after queue completion");
                    }
                    drop(batch);
                });
                assert_eq!(result.is_err(), outcome == 2);
            })
        };
        while !ready.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        assert!(cycle.queue.snapshot().is_drained());
        let finished = mark_closure::finish(&cycle, Instant::now());
        assist.join().unwrap();
        assert_eq!(cycle.active_drains.load(Ordering::Acquire), 0);
        assert_eq!(finished.is_some(), outcome == 0);
        if let Some((work, plan)) = finished {
            assert_eq!(work.marked_bytes, (GC_HEADER_SIZE + 8) as u64);
            sweep::concurrent(plan);
            assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
        } else {
            assert_eq!(cycle.worker_failed.load(Ordering::Acquire), outcome == 2);
            assert_eq!(
                cycle.deferred.lock().unwrap().len(),
                usize::from(outcome == 1)
            );
            runtime().heap.lock().unwrap().concurrent_cycle = None;
            GC_MARK_PHASE.store(0, Ordering::Release);
            cycle.queue.end_epoch();
        }
        assert!(
            crate::gc_telemetry::stops::snapshot_stops()
                .by_reason
                .iter()
                .all(|r| r.requests == 0)
        );
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        println!("closure outcome={outcome} empty_queue_waited_for_batch=true global_stops=0");
    }
    reset_internal_for_test();
}

#[test]
fn concurrent_closure_and_sweep_preserve_a_new_active_tlab_without_stopping_its_owner() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    SNAPSHOT_READY.store(false, Ordering::Relaxed);
    MUTATION_DONE.store(false, Ordering::Relaxed);
    const CONTROL: u32 = 0xFE12;
    willow_register_type(CONTROL, trace_slot);
    runtime()
        .concurrent_trace_registry
        .lock()
        .unwrap()
        .insert(CONTROL, snapshot_before_mutation);
    let mut control = willow_alloc_object(CONTROL as i64, 8);
    let mut leaf = willow_alloc(8);
    willow_push_root(&mut control);
    willow_push_root(&mut leaf);
    willow_gc_register_mutator();
    let ready = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let published = Arc::new(AtomicUsize::new(0));
    let worker = {
        let (ready, done, published) = (ready.clone(), done.clone(), published.clone());
        let (control, leaf) = (control as usize, leaf as usize);
        std::thread::spawn(move || {
            willow_gc_register_mutator();
            let mut tls = tlab_state_for_test();
            ready.store(true, Ordering::Release);
            while !SNAPSHOT_READY.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            let child = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 1);
            willow_gc_write_barrier(
                child,
                std::ptr::null_mut(),
                leaf as *mut u8,
                GcStoreDestination::ObjectField as i64,
            );
            unsafe {
                store_gc_reference(child.cast(), leaf as *mut u8);
            }
            willow_gc_write_barrier(
                control as *mut u8,
                std::ptr::null_mut(),
                child,
                GcStoreDestination::ObjectField as i64,
            );
            unsafe {
                store_gc_reference(control as *mut *mut u8, child);
            }
            published.store(child as usize, Ordering::Release);
            MUTATION_DONE.store(true, Ordering::Release);
            while !done.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            assert_ne!(
                tls.start_bits.load(Ordering::Acquire),
                0,
                "normal closure retired a post-snapshot TLAB"
            );
            willow_gc_unregister_mutator();
        })
    };
    while !ready.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    willow_gc_collect();
    assert_ne!(published.load(Ordering::Acquire), 0);
    assert_eq!(willow_gc_allocated_bytes(), 3 * (GC_HEADER_SIZE + 8) as i64);
    assert!(
        crate::gc_telemetry::stops::snapshot_stops()
            .by_reason
            .iter()
            .all(|r| r.requests == 0)
    );
    {
        let state = runtime().heap.lock().unwrap();
        let chunk = find_tlab_chunk(&state, published.load(Ordering::Acquire)).unwrap();
        assert!(chunk.owner_state.is_some());
        assert!(
            chunk.mark_bitmap.is_marked(0),
            "sweep cleared an active start map"
        );
    }
    done.store(true, Ordering::Release);
    worker.join().unwrap();
    willow_pop_roots(2);
    willow_gc_unregister_mutator();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn closing_phase_publishes_deletions_directly_instead_of_hiding_new_buffer_work() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let parent = willow_alloc_typed(8, 1) as usize;
    let child = willow_alloc_typed(8, 1) as usize;
    let leaf = willow_alloc(8) as usize;
    unsafe {
        store_gc_reference(parent as *mut *mut u8, child as *mut u8);
        store_gc_reference(child as *mut *mut u8, leaf as *mut u8);
    }
    let cycle = {
        let mut state = runtime().heap.lock().unwrap();
        state.satb = satb::SatbBuffers::new(256);
        let cycle = Arc::new(ConcurrentCycle::with_index(
            epoch_index::EpochIndex::Regions(epoch_index::RegionIndex::capture(&state)),
            HashSet::new(),
            HashMap::new(),
            1,
        ));
        assert!(cycle.objects.claim(parent));
        state.concurrent_cycle = Some(cycle.clone());
        GC_MARK_PHASE.store(1, Ordering::Release);
        cycle
    };
    let ready = Arc::new(AtomicBool::new(false));
    let producer = {
        let (cycle, ready) = (cycle.clone(), ready.clone());
        std::thread::spawn(move || {
            willow_gc_register_mutator();
            cycle.active_drains.fetch_add(1, Ordering::AcqRel);
            let mut batch = MarkBatch {
                cpu_start: None,
                cycle: &cycle,
                work: Default::default(),
                deferred: Vec::new(),
                unindexed: HashSet::new(),
            };
            batch.work.object(GC_HEADER_SIZE + 8, 1);
            ready.store(true, Ordering::Release);
            while !cycle.closing.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            willow_gc_write_barrier(
                parent as *mut u8,
                child as *mut u8,
                std::ptr::null_mut(),
                GcStoreDestination::ObjectField as i64,
            );
            unsafe {
                store_gc_reference(parent as *mut *mut u8, std::ptr::null_mut());
            }
            assert!(runtime().heap.lock().unwrap().satb.is_empty());
            assert!(cycle.is_marked(child));
            drop(batch);
            willow_gc_unregister_mutator();
        })
    };
    while !ready.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    let (work, plan) = mark_closure::finish(&cycle, Instant::now()).expect("normal closure");
    producer.join().unwrap();
    assert_eq!(work.marked_bytes, 3 * (GC_HEADER_SIZE + 8) as u64);
    sweep::concurrent(plan);
    assert_eq!(willow_gc_allocated_bytes(), 3 * (GC_HEADER_SIZE + 8) as i64);
    assert!(
        crate::gc_telemetry::stops::snapshot_stops()
            .by_reason
            .iter()
            .all(|r| r.requests == 0)
    );
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

static BUDGET_RETRY_CALLS: AtomicUsize = AtomicUsize::new(0);
static BUDGET_RETRY_LIMIT: AtomicUsize = AtomicUsize::new(0);

unsafe fn budget_retry_slice(
    _: *mut u8,
    _: usize,
    _: usize,
    _: &mut Vec<*mut u8>,
) -> TraceSliceProgress {
    let call = BUDGET_RETRY_CALLS.fetch_add(1, Ordering::Relaxed);
    if call < BUDGET_RETRY_LIMIT.load(Ordering::Relaxed) {
        TraceSliceProgress::Retry
    } else {
        TraceSliceProgress::Done
    }
}

#[test]
fn marker_pool_shared_budget_bounds_retries_and_preserves_pending_work() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    for workers in [1, 5, 16] {
        let pool = mark_workers::Pool::new(workers).unwrap();
        for objects in [256usize, 1024, 4096] {
            let budget = objects * 4;
            BUDGET_RETRY_CALLS.store(0, Ordering::Relaxed);
            // Finite even on the old implementation: regression must fail an
            // assertion instead of hanging the test process indefinitely.
            BUDGET_RETRY_LIMIT.store(budget * 4, Ordering::Relaxed);
            let mut cycle = ConcurrentCycle::new(
                (1..=objects).map(|address| {
                    (
                        address,
                        raw_heap::TraceMetadata {
                            type_id: 0xFA79,
                            layout_id: 0,
                            gc_ref_mask: 0,
                            payload_size: 0,
                        },
                    )
                }),
                HashSet::new(),
                HashMap::new(),
                0,
            );
            cycle.slices.insert(0xFA79, budget_retry_slice);
            let cycle = Arc::new(cycle);
            for address in 1..=objects {
                cycle.enqueue(address as *mut u8);
            }
            pool.run(&cycle, 0);
            let calls = BUDGET_RETRY_CALLS.load(Ordering::Relaxed);
            assert!(
                calls <= budget,
                "workers={workers} calls={calls} budget={budget}"
            );
            if workers == 1 {
                assert_eq!(calls, budget);
            }
            assert!(!cycle.worker_failed.load(Ordering::Acquire));
            assert_eq!(cycle.active_drains.load(Ordering::Acquire), 0);
            assert_eq!(registered_mutator_count(), 0);
            let pending = cycle.queue.snapshot();
            assert!(!pending.is_drained());
            assert_eq!(pending.registered_workers, 0);
            assert_eq!(pending.abandoned, 0);
            // Once the native lock becomes available, closure can consume all
            // retained continuations without revisiting or losing an object.
            BUDGET_RETRY_LIMIT.store(0, Ordering::Relaxed);
            cycle.drain(usize::MAX);
            assert_eq!(BUDGET_RETRY_CALLS.load(Ordering::Relaxed) - calls, objects);
            assert_eq!(cycle.objects.marked_count(), objects);
            assert!(cycle.queue.snapshot().is_drained());
            assert_eq!(cycle.queue.end_epoch(), 0);
            println!(
                "workers={workers} objects={objects} budget={budget} retries={calls} completion_calls={objects}"
            );
        }
    }
    reset_internal_for_test();
}

#[test]
fn concurrent_closure_paces_contention_and_retains_work_until_release() {
    let _guard = runtime_test_guard();
    for waits_before_release in [1usize, 4, 16, 64] {
        reset_internal_for_test();
        BUDGET_RETRY_CALLS.store(0, Ordering::Relaxed);
        // Finite on the old spin loop too, so regression fails without hanging.
        BUDGET_RETRY_LIMIT.store(waits_before_release * 1024, Ordering::Relaxed);
        let object = willow_alloc_object(0xFA79, 8) as usize;
        let cycle = {
            let mut state = runtime().heap.lock().unwrap();
            let mut cycle = ConcurrentCycle::with_index(
                epoch_index::EpochIndex::Regions(epoch_index::RegionIndex::capture(&state)),
                HashSet::new(),
                HashMap::new(),
                0,
            );
            cycle.slices.insert(0xFA79, budget_retry_slice);
            cycle.enqueue(object as *mut u8);
            let cycle = Arc::new(cycle);
            state.concurrent_cycle = Some(cycle.clone());
            GC_MARK_PHASE.store(1, Ordering::Release);
            cycle
        };
        let mut waits = 0;
        let mut previous_calls = 0;
        let mut requested_sleep = Duration::ZERO;
        let (work, plan) = mark_closure::finish_with_wait(&cycle, Instant::now(), |delay| {
            waits += 1;
            assert!(delay >= Duration::from_millis(1));
            requested_sleep += delay;
            let calls = BUDGET_RETRY_CALLS.load(Ordering::Relaxed);
            assert!(calls > previous_calls && calls - previous_calls <= 256);
            previous_calls = calls;
            assert!(!cycle.queue.snapshot().is_drained());
            assert_eq!(cycle.active_drains.load(Ordering::Acquire), 0);
            assert!(!cycle.worker_failed.load(Ordering::Acquire));
            // The wait owns no heap lock and leaves the cycle and storage live.
            assert!(
                runtime()
                    .heap
                    .try_lock()
                    .unwrap()
                    .concurrent_cycle
                    .is_some()
            );
            assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
            assert!(!runtime().stop_requested.load(Ordering::Acquire));
            if waits == waits_before_release {
                BUDGET_RETRY_LIMIT.store(0, Ordering::Relaxed);
            }
        })
        .expect("contention must complete without stopped fallback");
        assert_eq!(waits, waits_before_release);
        assert_eq!(
            BUDGET_RETRY_CALLS.load(Ordering::Relaxed),
            previous_calls + 1
        );
        assert_eq!(work.marked_bytes, (GC_HEADER_SIZE + 8) as u64);
        assert!(cycle.queue.snapshot().is_drained());
        sweep::concurrent(plan);
        assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
        assert!(
            crate::gc_telemetry::stops::snapshot_stops()
                .by_reason
                .iter()
                .all(|r| r.requests == 0)
        );
        println!(
            "closure waits={waits} retries={previous_calls} upper_bound={} requested_sleep_ns={} completion_calls=1 global_stops=0",
            waits * 256,
            requested_sleep.as_nanos()
        );
    }
    reset_internal_for_test();
}

#[test]
fn root_activation_crosses_pending_stores_before_snapshots_or_assist_scans() {
    let _guard = runtime_test_guard();
    for deletion in [true, false] {
        for unregister in [false, true] {
            reset_internal_for_test();
            willow_gc_register_mutator();
            let parent = willow_alloc_typed(8, 1) as usize;
            let child = willow_alloc(8) as usize;
            if deletion {
                unsafe { store_gc_reference(parent as *mut *mut u8, child as *mut u8) };
            }
            let done = Arc::new(AtomicBool::new(false));
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            let (observed_tx, observed_rx) = std::sync::mpsc::channel();
            let writer = {
                let (done, ready_tx) = (done.clone(), ready_tx.clone());
                std::thread::spawn(move || {
                    willow_gc_register_mutator();
                    assert_eq!(GC_MARK_PHASE.load(Ordering::Acquire), 0);
                    willow_gc_write_barrier(
                        parent as *mut u8,
                        if deletion {
                            child as *mut u8
                        } else {
                            std::ptr::null_mut()
                        },
                        if deletion {
                            std::ptr::null_mut()
                        } else {
                            child as *mut u8
                        },
                        GcStoreDestination::ObjectField as i64,
                    );
                    ready_tx.send(()).unwrap();
                    resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    unsafe {
                        store_gc_reference(
                            parent as *mut *mut u8,
                            if deletion {
                                std::ptr::null_mut()
                            } else {
                                child as *mut u8
                            },
                        )
                    };
                    if !unregister {
                        while !done.load(Ordering::Acquire) {
                            willow_gc_safepoint();
                            std::thread::yield_now();
                        }
                    }
                    willow_gc_unregister_mutator();
                })
            };
            let reader = {
                let done = done.clone();
                std::thread::spawn(move || {
                    willow_gc_register_mutator();
                    let mut parent_root = parent as *mut u8;
                    let mut local = std::ptr::null_mut();
                    willow_push_root(&mut parent_root);
                    willow_push_root(&mut local);
                    ready_tx.send(()).unwrap();
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !runtime().poll_requested.load(Ordering::Acquire) {
                        assert!(Instant::now() < deadline);
                        std::thread::yield_now();
                    }
                    willow_gc_safepoint();
                    // This first acknowledgement must return without waiting
                    // for the writer, whose barrier/store interval is open.
                    local = unsafe { load_gc_reference(parent as *mut *mut u8) };
                    let cycle = runtime()
                        .heap
                        .lock()
                        .unwrap()
                        .concurrent_cycle
                        .clone()
                        .unwrap();
                    cycle.enqueue(parent as *mut u8);
                    let assist_work = cycle.drain_checked(8);
                    let work = *cycle.work.lock().unwrap();
                    observed_tx
                        .send((
                            local as usize,
                            assist_work,
                            work.root_scan_bytes,
                            work.marked_bytes,
                        ))
                        .unwrap();
                    resume_tx.send(()).unwrap();
                    while !done.load(Ordering::Acquire) {
                        willow_gc_safepoint();
                        std::thread::yield_now();
                    }
                    std::hint::black_box(local);
                    willow_pop_roots(2);
                    willow_gc_unregister_mutator();
                })
            };
            ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let before = willow_gc_allocated_bytes();
            willow_gc_collect();
            let after = willow_gc_allocated_bytes();
            done.store(true, Ordering::Release);
            writer.join().unwrap();
            reader.join().unwrap();
            willow_gc_unregister_mutator();
            let (local, assist_work, root_bytes, marked_bytes) = observed_rx.recv().unwrap();
            assert_eq!(local, if deletion { child } else { 0 });
            assert_eq!((assist_work, root_bytes, marked_bytes), (0, 0, 0));
            assert_eq!(after, before, "deletion={deletion} unregister={unregister}");
            assert!(
                crate::gc_telemetry::stops::snapshot_stops()
                    .by_reason
                    .iter()
                    .all(|r| r.requests == 0)
            );
            println!(
                "activation deletion={deletion} unregister={unregister} before={before} after={after} early_progress=true early_roots=0 early_trace=0 stops=0"
            );
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), 0);
        }
    }
    reset_internal_for_test();
}

#[test]
fn root_activation_late_registration_joins_the_current_round() {
    let _guard = runtime_test_guard();
    for during_snapshots in [false, true] {
        reset_internal_for_test();
        willow_gc_register_mutator();
        let mut root = willow_alloc(8);
        willow_push_root(&mut root);
        root_handshake::ACTIVATION_ACKS.store(0, Ordering::Relaxed);
        root_handshake::ROOT_ACKS.store(0, Ordering::Relaxed);
        let done = Arc::new(AtomicBool::new(false));
        let collector = {
            let done = done.clone();
            std::thread::spawn(move || {
                willow_gc_register_mutator();
                willow_gc_collect();
                willow_gc_unregister_mutator();
                done.store(true, Ordering::Release);
            })
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !runtime().poll_requested.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        if during_snapshots {
            willow_gc_safepoint(); // Release the activation round only.
            loop {
                let cycle = runtime()
                    .heap
                    .lock()
                    .unwrap()
                    .concurrent_cycle
                    .clone()
                    .unwrap();
                if cycle.tracing_enabled.load(Ordering::Acquire) {
                    break;
                }
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
        }
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let late = {
            let (done, address) = (done.clone(), root as usize);
            std::thread::spawn(move || {
                willow_gc_register_mutator();
                let mut root = address as *mut u8;
                willow_push_root(&mut root);
                ready_tx.send(()).unwrap();
                while !done.load(Ordering::Acquire) {
                    willow_gc_safepoint();
                    std::thread::yield_now();
                }
                std::hint::black_box(root);
                willow_pop_root();
                willow_gc_unregister_mutator();
            })
        };
        ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        while !done.load(Ordering::Acquire) {
            willow_gc_safepoint();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        collector.join().unwrap();
        late.join().unwrap();
        assert_eq!(
            root_handshake::ACTIVATION_ACKS.load(Ordering::Relaxed),
            if during_snapshots { 2 } else { 3 }
        );
        assert_eq!(root_handshake::ROOT_ACKS.load(Ordering::Relaxed), 3);
        assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
        assert!(
            crate::gc_telemetry::stops::snapshot_stops()
                .by_reason
                .iter()
                .all(|r| r.requests == 0)
        );
        println!("late_registration during_snapshots={during_snapshots} root_snapshots=3 stops=0");
        willow_pop_root();
        willow_gc_unregister_mutator();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
    }
    reset_internal_for_test();
}
