use super::*;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

// A loaded host can exhaust the remark budget; production then completes the
// same cycle with a stopped recovery.
fn finish_or_recover(
    cycle: &Arc<ConcurrentCycle>,
) -> (crate::gc_telemetry::MarkWork, Option<sweep::SweepPlan>) {
    match mark_closure::finish(cycle, Instant::now()) {
        Some((work, plan)) => (work, Some(plan)),
        None => {
            let (work, plan, _) = mark_closure::finish_stopped(cycle, Instant::now());
            (work, plan)
        }
    }
}

fn sweep_if_planned(plan: Option<sweep::SweepPlan>) {
    if let Some(plan) = plan {
        sweep::concurrent(plan);
    }
}

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
    // The explicit major cycle adds its initial and remark handshakes while
    // flushing the preceding failed callback's event.
    assert_eq!(stats.by_reason[0].requests, 2);
    assert_eq!(stats.by_reason[0].aborted, 1);
    assert_eq!(stats.by_reason[1].requests, 2);
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
            register_slot_type(CONTROL);
            register_slot_type(SOURCE);
            runtime()
                .concurrent_slice_registry
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
pub(super) fn assert_satb_mutation_retains(old: *mut u8, mutation: impl FnOnce()) {
    let _elected = cycle_wait::CollectorGuard::try_acquire().expect("test owns collector election");
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
        marking::set_concurrent_cycle(&mut state, Some(cycle.clone()));
        GC_MARK_PHASE.store(1, Ordering::Release);
        cycle
    };
    mutation();
    with_stw(
        crate::gc_telemetry::stops::StopReason::Remark,
        |coord, _| {
            flush_satb_all_locked(coord, &mut runtime().heap.lock().unwrap());
        },
    );
    cycle.drain(usize::MAX);
    assert!(cycle.is_marked(old as usize), "deleted edge was not traced");
    assert!(cycle.queue.snapshot().is_drained());
    // Inline descendants and bounded batches can share a queue job. Verify
    // unique tracing by byte accounting rather than requiring one job/object.
    let deferred = cycle.deferred.lock().unwrap();
    let expected_bytes: u64 =
        epoch_objects(&runtime().heap.lock().unwrap(), &mut Default::default())
            .into_iter()
            .filter(|object| {
                let address = object.payload().as_ptr() as usize;
                cycle.is_marked(address) && !deferred.contains(&address)
            })
            .map(|object| object.size() as u64)
            .sum();
    assert_eq!(cycle.work.lock().unwrap().marked_bytes, expected_bytes);
    drop(deferred);
    GC_MARK_PHASE.store(0, Ordering::Release);
    marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
    assert_eq!(cycle.queue.end_epoch(), 0);
}

/// Count only the actual mutation under active marking, excluding setup stores.
fn assert_satb_variant_mutation_retains(
    old: *mut u8,
    destination: Option<GcStoreDestination>,
    mutation: impl FnOnce(),
) {
    assert_satb_mutation_retains(old, || {
        let before = barrier_variant_counts();
        mutation();
        let after = barrier_variant_counts();
        if let Some(destination) = destination {
            let index = destination as usize - 1;
            assert!(
                after.0[index] > before.0[index],
                "destination={destination:?}"
            );
        } else {
            assert!(after.1 > before.1, "logical deletion barrier did not run");
        }
    });
}

/// willow-jz15.53: while concurrently marking, write barriers and logical
/// deletions publish into the cycle's shard buffers without the heap lock.
/// The closure/remark drain seals the shards and enqueues their entries;
/// later publications take the heap-locked path.
#[test]
fn cycle_cache_lookup_survives_thread_local_teardown() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let cycle = {
        let mut state = runtime().heap.lock().unwrap();
        let cycle = Arc::new(ConcurrentCycle::new(
            std::iter::empty(),
            type_registry().lock().unwrap().keys().copied().collect(),
            runtime().concurrent_trace_registry.lock().unwrap().clone(),
            0,
        ));
        marking::set_concurrent_cycle(&mut state, Some(cycle.clone()));
        cycle
    };
    static FOUND: AtomicBool = AtomicBool::new(false);
    FOUND.store(false, Ordering::Relaxed);
    struct LookupOnDrop;
    impl Drop for LookupOnDrop {
        fn drop(&mut self) {
            // Runs while (or after) the cache's own TLS slot is torn down.
            FOUND.store(marking::cached_cycle().is_some(), Ordering::Relaxed);
        }
    }
    thread_local! {
        static LOOKUP: LookupOnDrop = const { LookupOnDrop };
    }
    std::thread::spawn(|| {
        // Register the lookup first so the cache slot, registered second, is
        // destroyed before it on platforms with LIFO TLS destruction.
        LOOKUP.with(|_| ());
        assert!(marking::cached_cycle().is_some());
    })
    .join()
    .expect("lookup during TLS teardown must not panic");
    assert!(FOUND.load(Ordering::Relaxed));
    marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
    drop(cycle);
    reset_internal_for_test();
}

#[test]
fn marking_barriers_publish_without_heap_lock_until_sealed() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let owner = willow_alloc_typed(8, 1);
    let old = willow_alloc(8);
    let value = willow_alloc(8);
    let deleted = willow_alloc(8);
    let late = willow_alloc(8);
    let mut tls = tlab_state_for_test();
    let young = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    let cycle = {
        let mut state = runtime().heap.lock().unwrap();
        retire_all_tlabs_locked(&mut state);
        barrier::remember_owner(&mut state, owner as usize);
        let cycle = Arc::new(ConcurrentCycle::new(
            epoch_objects(&state, &mut Default::default())
                .into_iter()
                .map(|o| (o.payload().as_ptr() as usize, o.trace_metadata())),
            type_registry().lock().unwrap().keys().copied().collect(),
            runtime().concurrent_trace_registry.lock().unwrap().clone(),
            0,
        ));
        marking::set_concurrent_cycle(&mut state, Some(cycle.clone()));
        GC_MARK_PHASE.store(1, Ordering::Release);
        cycle
    };
    let warmed = Arc::new(AtomicBool::new(false));
    let go = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let worker = {
        let (warmed, go, done) = (warmed.clone(), go.clone(), done.clone());
        let young = young as usize;
        let (owner, old, value, deleted) = (
            owner as usize,
            old as usize,
            value as usize,
            deleted as usize,
        );
        std::thread::spawn(move || {
            // A cache miss reads the cycle under the heap lock: warm it first.
            assert!(marking::cached_cycle().is_some());
            warmed.store(true, Ordering::Release);
            while !go.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            willow_gc_write_barrier(
                owner as *mut u8,
                old as *mut u8,
                value as *mut u8,
                GcStoreDestination::ObjectField as i64 | willow_abi::GC_STORE_VALUE_HAS_HEADER,
            );
            barrier::write_barrier_object_owner(
                owner as *mut u8,
                std::ptr::null_mut(),
                young as *mut u8,
                GcStoreDestination::ObjectField as i64,
            );
            satb_delete(deleted as *mut u8);
            done.store(true, Ordering::Release);
        })
    };
    while !warmed.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    {
        let _heap = runtime().heap.lock().unwrap();
        go.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done.load(Ordering::Acquire) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            done.load(Ordering::Acquire),
            "marking barrier blocked on the heap lock"
        );
    }
    worker.join().unwrap();
    // The new edge is enqueued eagerly; deletions wait in the shard buffer.
    assert!(cycle.is_marked(value as usize));
    assert!(cycle.is_marked(young as usize));
    assert!(!cycle.is_marked(old as usize));
    assert!(!cycle.is_marked(deleted as usize));
    with_stw(
        crate::gc_telemetry::stops::StopReason::Remark,
        |coord, _| {
            flush_satb_all_locked(coord, &mut runtime().heap.lock().unwrap());
        },
    );
    assert!(
        cycle.is_marked(old as usize),
        "sealing lost a shard deletion"
    );
    assert!(cycle.is_marked(deleted as usize));
    assert!(cycle.satb_shards.is_empty());
    // Sealed: the deletion goes to the heap-locked buffers instead.
    satb_delete(late);
    assert!(cycle.satb_shards.is_empty());
    assert!(!runtime().heap.lock().unwrap().satb.is_empty());
    with_stw(
        crate::gc_telemetry::stops::StopReason::Remark,
        |coord, _| {
            flush_satb_all_locked(coord, &mut runtime().heap.lock().unwrap());
        },
    );
    assert!(cycle.is_marked(late as usize));
    cycle.drain(usize::MAX);
    GC_MARK_PHASE.store(0, Ordering::Release);
    marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
    assert_eq!(cycle.queue.end_epoch(), 0);
    reset_internal_for_test();
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
    let _stress = GcStressTestScope::with_modes("all");
    for capacity in [1, 256] {
        for case in 0..12 {
            reset_internal_for_test();
            runtime().heap.lock().unwrap().satb = satb::SatbBuffers::new(capacity);
            let mut old = willow_alloc(8);
            willow_push_root(&mut old);
            let destination = match case {
                0 | 1 => Some(GcStoreDestination::ArrayElement),
                2 => Some(GcStoreDestination::MapValue),
                3 => Some(GcStoreDestination::BlockingCell),
                4 => Some(GcStoreDestination::BlockingRwCell),
                6 => Some(GcStoreDestination::GlobalStatic),
                8 => Some(GcStoreDestination::AsyncFrameSlot),
                9 => Some(GcStoreDestination::ContainerInternal),
                10 => Some(GcStoreDestination::AsyncMutexCell),
                11 => Some(GcStoreDestination::AsyncRwLockCell),
                _ => None,
            };
            match case {
                0 | 1 => {
                    let array = crate::array::willow_array_new(1, 1);
                    crate::array::willow_array_set(array, 0, old as i64);
                    assert_satb_variant_mutation_retains(old, destination, || {
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
                    assert_satb_variant_mutation_retains(old, destination, || {
                        crate::map::willow_map_insert(map, 7, 0, 0, 1)
                    });
                }
                3 => {
                    let cell = crate::lock::willow_blocking_cell_new(old as i64, 1);
                    assert_satb_variant_mutation_retains(old, destination, || {
                        crate::lock::willow_blocking_cell_set(cell, 0)
                    });
                }
                4 => {
                    let cell = crate::lock::willow_blocking_rw_cell_new(old as i64, 1);
                    assert_satb_variant_mutation_retains(old, destination, || {
                        crate::lock::willow_blocking_rw_cell_write(cell, 0)
                    });
                }
                5 => {
                    let channel = crate::channel::willow_channel_new(1);
                    assert_eq!(
                        crate::channel::willow_channel_try_send_ptr(channel, old.cast()),
                        1
                    );
                    assert_satb_variant_mutation_retains(old, destination, || {
                        assert_eq!(crate::channel::willow_channel_recv_ptr(channel), old.cast());
                    });
                }
                6 => {
                    let arena = GcRootArena::default();
                    let handle = arena.insert(std::ptr::null_mut(), old);
                    assert_satb_variant_mutation_retains(old, destination, || handle.release());
                }
                7 => {
                    willow_gc_add_runtime_root(old);
                    assert_satb_variant_mutation_retains(old, destination, || {
                        willow_gc_remove_runtime_root(old)
                    });
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
                    assert_satb_variant_mutation_retains(old, destination, || {
                        frame.store_gc(0, std::ptr::null_mut())
                    });
                }
                9 => {
                    let array = crate::array::willow_array_new(1, 1);
                    let old_buffer = crate::array::willow_array_reference_owner(array, 0);
                    assert_satb_variant_mutation_retains(old_buffer, destination, || {
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
                    assert_satb_variant_mutation_retains(old, destination, || {
                        assert!(cell.commit(task, token, 0))
                    });
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
                    assert_satb_variant_mutation_retains(old, destination, || {
                        assert!(cell.commit(task, token, 0))
                    });
                    cell.release(task, token);
                }
            }
            willow_pop_root();
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
            with_stw(
                crate::gc_telemetry::stops::StopReason::Remark,
                |coord, _| {
                    flush_satb_all_locked(coord, &mut runtime().heap.lock().unwrap());
                },
            );
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

unsafe fn snapshot_slot_slice(
    payload: *mut u8,
    _: usize,
    limit: usize,
    children: &mut Vec<*mut u8>,
) -> TraceSliceProgress {
    assert!(limit > 0);
    assert!(!runtime().stop_requested.load(Ordering::Acquire));
    children.push(unsafe { load_gc_reference(payload.cast()) });
    TraceSliceProgress::Done
}

fn register_slot_type(id: u32) {
    willow_register_type_capability(
        id,
        trace_slot,
        NativeTraceCapability::SliceConcurrentSafe(snapshot_slot_slice),
    );
}

unsafe fn snapshot_before_mutation(
    payload: *mut u8,
    cursor: usize,
    _: usize,
    children: &mut Vec<*mut u8>,
) -> TraceSliceProgress {
    if cursor == 0 {
        children.push(unsafe { load_gc_reference(payload.cast()) });
        SNAPSHOT_READY.store(true, Ordering::Release);
        TraceSliceProgress::Continue(1)
    } else if MUTATION_DONE.load(Ordering::Acquire) {
        TraceSliceProgress::Done
    } else {
        TraceSliceProgress::Retry
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
    register_slot_type(SNAPSHOT_TYPE);
    register_slot_type(LEGACY_TYPE);
    runtime()
        .concurrent_slice_registry
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
            let mutator = crate::gc::MutatorRegistration::new();
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
            drop(mutator);
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
    crate::gc::minor_collect_internal();
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
    assert!(can_reserve(&mut runtime().heap.lock().unwrap(), 0));
    willow_pop_roots(1);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    assert!(!willow_alloc(8).is_null());
    reset_internal_for_test();
}

static ASSIST_READY: AtomicBool = AtomicBool::new(false);
static ASSIST_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static ASSIST_SAW_REMARK: AtomicBool = AtomicBool::new(false);
/// Set by whichever participant gives up first, so the others leave their
/// rendezvous instead of each burning its own full budget in turn.
static ASSIST_ABORT: AtomicBool = AtomicBool::new(false);

// Preserve the cause without unwinding out of a GC trace callback or off a
// worker thread. The collector owns live cycle state while a callback runs,
// and a thread that unwinds out of the fixture cannot report anything itself,
// so every failure becomes a flag the main thread asserts on (willow-utqy).
const MARKER_NEVER_TRACED: usize = 1;
const REMARK_NEVER_REQUESTED: usize = 4;
const CYCLE_MISSING: usize = 8;
const CLAIM_REFUSED: usize = 16;
const ASSIST_SLOT_UNAVAILABLE: usize = 32;
const ASSIST_PUSH_FAILED: usize = 64;
static ASSIST_FAILURES: AtomicUsize = AtomicUsize::new(0);

/// Generous enough that an oversubscribed CI runner cannot lose the rendezvous
/// on scheduling alone; the fixture reports rather than hangs when it expires.
const ASSIST_RENDEZVOUS: Duration = Duration::from_secs(30);

fn assist_failed(cause: usize) {
    ASSIST_FAILURES.fetch_or(cause, Ordering::Release);
    ASSIST_ABORT.store(true, Ordering::Release);
}

/// Spin until `done` holds, giving up at `ASSIST_RENDEZVOUS`. Returns false
/// when the wait was abandoned, so the caller can skip the rest of its part.
fn await_assist_step(cause: usize, mut poll: impl FnMut(), done: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + ASSIST_RENDEZVOUS;
    while !done() {
        if ASSIST_ABORT.load(Ordering::Acquire) {
            return false;
        }
        if Instant::now() >= deadline {
            assist_failed(cause);
            return false;
        }
        poll();
        std::thread::yield_now();
    }
    true
}

unsafe fn hold_marker_for_assistant(
    payload: *mut u8,
    _: usize,
    _: usize,
    children: &mut Vec<*mut u8>,
) -> TraceSliceProgress {
    ASSIST_READY.store(true, Ordering::Release);
    if ASSIST_IN_FLIGHT.load(Ordering::Acquire) {
        children.push(unsafe { load_gc_reference(payload.cast()) });
        TraceSliceProgress::Done
    } else {
        TraceSliceProgress::Retry
    }
}

unsafe fn assist_overlaps_remark(
    _: *mut u8,
    _: usize,
    _: usize,
    _: &mut Vec<*mut u8>,
) -> TraceSliceProgress {
    TraceSliceProgress::Done
}

/// Run the mutator-side assist that must overlap the collector's final remark.
///
/// Records a cause and returns instead of asserting: this runs on a worker
/// thread, where a panic would report nothing and would abandon the private
/// assist slot mid-cycle.
fn run_mutator_assist(child: *mut u8) {
    let cycle = {
        let state = runtime().heap.lock().unwrap();
        state.concurrent_cycle.clone()
    };
    let Some(cycle) = cycle else {
        assist_failed(CYCLE_MISSING);
        return;
    };
    // Reserve a private assist job so a background worker cannot steal the
    // fixture whose purpose is to overlap a *mutator* with remark.
    if !cycle.objects.claim(child as usize) {
        assist_failed(CLAIM_REFUSED);
        return;
    }
    let mut consumer = cycle.queue.register_assist();
    if consumer.slot().is_none() {
        assist_failed(ASSIST_SLOT_UNAVAILABLE);
        return;
    }
    let pushed = crate::gc_mark_queue::ObjectRef::from_ptr(child).is_some_and(|object| {
        consumer
            .push(crate::gc_mark_queue::MarkWorkItem::Object(object))
            .is_ok()
    });
    if !pushed {
        assist_failed(ASSIST_PUSH_FAILED);
        return;
    }
    // Explicit test ownership models the reader lifetime outside the callback:
    // native slices remain bounded while final remark must await this owner.
    struct ReaderHold<'a>(&'a ConcurrentCycle);
    impl Drop for ReaderHold<'_> {
        fn drop(&mut self) {
            self.0.active_drains.fetch_sub(1, Ordering::Release);
        }
    }
    cycle.active_drains.fetch_add(1, Ordering::AcqRel);
    let hold = ReaderHold(&cycle);
    ASSIST_IN_FLIGHT.store(true, Ordering::Release);
    cycle.drain_worker(1, &mut Vec::new(), &mut consumer);
    if await_assist_step(
        REMARK_NEVER_REQUESTED,
        || willow_gc_safepoint(),
        || mark_closure::READER_WAIT_OBSERVED.load(Ordering::Acquire),
    ) {
        ASSIST_SAW_REMARK.store(true, Ordering::Release);
    }
    drop(hold);
}

#[test]
fn remark_waits_for_an_in_flight_mutator_assist() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    ASSIST_READY.store(false, Ordering::Relaxed);
    ASSIST_IN_FLIGHT.store(false, Ordering::Relaxed);
    ASSIST_SAW_REMARK.store(false, Ordering::Relaxed);
    ASSIST_ABORT.store(false, Ordering::Relaxed);
    ASSIST_FAILURES.store(0, Ordering::Relaxed);
    mark_closure::READER_WAIT_OBSERVED.store(false, Ordering::Relaxed);
    for (id, callback) in [
        (0xFD01, hold_marker_for_assistant as ConcurrentTraceSliceFn),
        (0xFD02, assist_overlaps_remark as ConcurrentTraceSliceFn),
    ] {
        willow_register_type_capability(
            id,
            trace_slot,
            NativeTraceCapability::SliceConcurrentSafe(callback),
        );
    }
    willow_register_type_capability(
        0xFD03,
        trace_slot,
        NativeTraceCapability::SliceConcurrentSafe(hold_marker_for_assistant),
    );
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
            // The guard, not a trailing call, owns the unregistration: this
            // thread must leave the mutator registry even if it unwinds, or
            // the collector waits for it forever (willow-utqy).
            let _mutator = crate::gc::MutatorRegistration::new();
            ready.store(true, Ordering::Release);
            if await_assist_step(
                MARKER_NEVER_TRACED,
                || willow_gc_safepoint(),
                || ASSIST_READY.load(Ordering::Acquire),
            ) {
                let source = source as *mut u8;
                let child = unsafe { load_gc_reference(source.cast()) };
                run_mutator_assist(child);
            }
            // Stay registered and cooperating until the collection is over, so
            // the drive that follows it observes a quiesced worker.
            await_assist_step(0, || willow_gc_safepoint(), || stop.load(Ordering::Acquire));
        })
    };
    await_assist_step(MARKER_NEVER_TRACED, || {}, || ready.load(Ordering::Acquire));
    willow_gc_collect();
    stop.store(true, Ordering::Release);
    worker.join().unwrap();
    let failures = ASSIST_FAILURES.load(Ordering::Acquire);
    assert_eq!(
        failures, 0,
        "assist rendezvous failures: {failures:#x} (1=marker never traced, \
         2=assist never started, 4=remark never requested, 8=cycle missing, \
         16=claim refused, 32=no assist slot, 64=push failed)"
    );
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
    crate::gc::minor_collect_internal();
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
                .contains("runtime resource exhausted: GC memory limit exceeded")
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
    assert!(can_reserve(&mut runtime().heap.lock().unwrap(), 0));
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
    crate::gc::minor_collect_internal();
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
                    IdSet::default(),
                    IdMap::from_iter([(0xFE01, snapshot_scratch_fixture as ConcurrentTraceFn)]),
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
                epoch_index::EpochIndex::regions(epoch_index::RegionIndex::capture(
                    &runtime().heap.lock().unwrap(),
                )),
                IdSet::default(),
                IdMap::from_iter([(0xFA71, record_marker_thread as ConcurrentTraceFn)]),
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
            // Inline continuations may trace a chain under one queue claim;
            // every object is still traced exactly once (ids assertion below).
            assert!(snapshot.injected > 0 && snapshot.injected <= objects as u64);
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
                "marker workers={workers} objects={objects} worker_traces={worker_traces} trace_jobs={} readers_after=0",
                snapshot.injected
            );
            // No references remain in any worker when storage is reclaimed.
            reset_internal_for_test();
        }
        drop(pool);
    }
}

#[test]
fn concurrent_native_contract_failures_reject_before_reclamation() {
    const KEY: &str = "WILLOW_TEST_NATIVE_CONTRACT_FAILURE";
    let Ok(case) = std::env::var(KEY) else {
        for case in ["legacy", "snapshot", "panic"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "gc::concurrent_tests::concurrent_native_contract_failures_reject_before_reclamation", "--nocapture"])
                .env(KEY, case).output().unwrap();
            assert!(!output.status.success());
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(stderr.contains("runtime fatal:"), "{stderr}");
            assert!(!stderr.contains("UNREACHABLE_DROP"), "{stderr}");
            assert!(!stderr.contains("UNBOUNDED_HOOK"), "{stderr}");
            assert!(!stderr.contains("COLLECTION_RETURNED"), "{stderr}");
        }
        return;
    };
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    unsafe fn failing_slice(
        _: *mut u8,
        _: usize,
        _: usize,
        _: &mut Vec<*mut u8>,
    ) -> TraceSliceProgress {
        panic!("injected bounded callback failure")
    }
    unsafe fn drop_probe(_: *mut u8) {
        eprintln!("UNREACHABLE_DROP");
    }
    unsafe fn rejected_stopped(_: *mut u8, _: &mut Vec<*mut *mut u8>) {
        eprintln!("UNBOUNDED_HOOK");
    }
    unsafe fn rejected_snapshot(_: *mut u8, _: &mut Vec<*mut u8>) {
        eprintln!("UNBOUNDED_HOOK");
    }
    willow_register_type(0xFA72, rejected_stopped);
    if case == "snapshot" {
        willow_register_type_capability(
            0xFA72,
            rejected_stopped,
            NativeTraceCapability::ConcurrentSafe(rejected_snapshot),
        );
    }
    if case == "panic" {
        willow_register_type_capability(
            0xFA72,
            trace_slot,
            NativeTraceCapability::SliceConcurrentSafe(failing_slice),
        );
    }
    willow_register_drop(0xFA73, drop_probe);
    let mut parent = willow_alloc_object(0xFA72, 8);
    willow_push_root(&mut parent);
    willow_alloc_object(0xFA73, 8);
    willow_gc_collect();
    eprintln!("COLLECTION_RETURNED");
}

#[test]
fn disabled_mark_workers_use_collector_queue_tracing() {
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
        crate::gc_telemetry::stops::snapshot_stops().by_reason[3]
            .work
            .swept_objects,
        0
    );
    assert_eq!(
        crate::gc_telemetry::workers::snapshot().fallback_cycles,
        before
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
fn marker_panics_publish_failure_and_retire_worker_collector_and_assist() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    for case in ["worker1", "worker5", "collector", "assist"] {
        let object = willow_alloc(8) as usize;
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
            IdSet::default(),
            IdMap::from_iter([(0xFA73, failing_concurrent_trace as ConcurrentTraceFn)]),
            1,
        ));
        cycle.enqueue(object as *mut u8);
        let before = crate::gc_telemetry::workers::snapshot().marker_panics;
        match case {
            "worker1" | "worker5" => {
                let pool = mark_workers::Pool::new(if case == "worker1" { 1 } else { 5 }).unwrap();
                pool.run(&cycle, 0);
                // The same OS threads remain usable after a failed cycle.
                pool.run(&cycle, 0);
            }
            "collector" => mark_workers::Pool::new(0).unwrap().run(&cycle, 1),
            "assist" => {
                cycle.drain_checked(1);
            }
            _ => unreachable!(),
        }
        assert!(cycle.worker_failed.load(Ordering::Acquire), "{case}");
        assert_eq!(
            crate::gc_telemetry::workers::snapshot().marker_panics,
            before + 1
        );
        let queue = cycle.queue.snapshot();
        assert_eq!(queue.registered_workers, 0);
        assert_eq!(queue.registered_assists, 0);
        assert_eq!(cycle.active_drains.load(Ordering::Acquire), 0);
        assert_eq!(registered_mutator_count(), 0);
        assert!(cycle.is_marked(object));
        cycle.queue.end_epoch();
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

fn m2_await_allocation_with_frame_quarantined(index: usize) {
    if index != 0 {
        return;
    }
    let s = crate::frame_lifetime::snapshot();
    // When the small frame's region precedes the large one, metadata is gone
    // but its storage is not reusable while the sweep is between regions.
    assert_eq!(s.task_frames_reclaimed, 0, "{s:?}");
    assert_eq!(s.quarantined_frames, s.metadata_removed_frames, "{s:?}");
    assert_eq!(s.quarantined_bytes, s.metadata_removed_bytes, "{s:?}");
    await_allocation_between_sweep_regions(index);
}

#[test]
fn concurrent_sweep_retains_new_allocations_and_quarantines_processed_spans() {
    assert_sweep_retains_new_allocations(false);
}

#[test]
fn recovery_sweep_retains_new_allocations_and_quarantines_processed_spans() {
    assert_sweep_retains_new_allocations(true);
}

fn assert_sweep_retains_new_allocations(recovery: bool) {
    let _guard = runtime_test_guard();
    for allocate_in_pending_region in [true, false] {
        reset_internal_for_test();
        if recovery {
            old_cycle::REMARK_WORK_NS_FOR_TEST.store(1, Ordering::Relaxed);
            *MARK_WORKERS.lock().unwrap() = Some(mark_workers::Pool::new(0).unwrap());
        }
        SWEEP_MUTATOR_READY.store(false, Ordering::Relaxed);
        SWEEP_MUTATOR_DONE.store(false, Ordering::Relaxed);
        let large = (GC_LARGE_OBJECT_THRESHOLD + 8) as i64;
        crate::frame_lifetime::capture_events(true);
        let old_small = if allocate_in_pending_region {
            willow_alloc(large);
            crate::async_frame::willow_async_frame_alloc(0, 0).cast::<u8>()
        } else {
            let small = crate::async_frame::willow_async_frame_alloc(0, 0).cast::<u8>();
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
                let mutator = crate::gc::MutatorRegistration::new();
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
                drop(mutator);
            })
        };
        while !ready.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        *sweep::SWEEP_TEST_HOOK.lock().unwrap() = Some(m2_await_allocation_with_frame_quarantined);
        willow_gc_collect();
        *sweep::SWEEP_TEST_HOOK.lock().unwrap() = None;
        stop.store(true, Ordering::Release);
        worker.join().unwrap();
        let address = address.load(Ordering::Acquire);
        assert_ne!(address, 0);
        // Pending extents are conservatively quarantined before resume. A new
        // allocation must use safe storage regardless of visitation order.
        assert!(!(old_base..old_base + GC_OLD_REGION_SIZE).contains(&address));
        assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
        assert_eq!(unsafe { *(address as *const u64) }, 0xAABBCCDD);
        assert!(!unsafe { (*payload_to_header(address as *mut u8)).marked });
        assert!(runtime().heap.lock().unwrap().sweeping.is_none());
        let lifetime = crate::frame_lifetime::snapshot();
        assert_eq!(lifetime.task_frames_reclaimed, 1, "{lifetime:?}");
        assert_eq!(
            lifetime.reclaimed_frame_bytes, lifetime.allocated_frame_bytes,
            "{lifetime:?}"
        );
        assert_eq!(lifetime.quarantined_frames, 0, "{lifetime:?}");
        let events = crate::frame_lifetime::events();
        let dead = events
            .iter()
            .find(|e| e.kind == crate::frame_lifetime::FrameEventKind::MetadataRemoved)
            .unwrap();
        let finished = events
            .iter()
            .find(|e| e.kind == crate::frame_lifetime::FrameEventKind::SweepCompleted)
            .unwrap();
        assert_eq!(dead.sweep_epoch, finished.sweep_epoch);
        assert!(dead.sweep_epoch > 0);
        let stops = crate::gc_telemetry::stops::snapshot_stops();
        assert_eq!(stops.by_reason[1].work.swept_objects, 0);
        assert_eq!(stops.by_reason[3].work.swept_objects, 0);
        if recovery {
            assert_eq!(stops.by_reason[3].requests, 1);
        }
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
            let mutator = crate::gc::MutatorRegistration::new();
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
            drop(mutator);
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
            let mutator = crate::gc::MutatorRegistration::new();
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
            drop(mutator);
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
    let elected = cycle_wait::CollectorGuard::try_acquire().unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let worker = {
        let done = done.clone();
        std::thread::spawn(move || {
            let mutator = crate::gc::MutatorRegistration::new();
            collect_for_budget();
            done.store(true, Ordering::Release);
            drop(mutator);
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

unsafe fn hold_snapshot_for_deleting_mutators(
    _: *mut u8,
    _: usize,
    _: usize,
    _: &mut Vec<*mut u8>,
) -> TraceSliceProgress {
    SATB_MUTATORS_READY.store(true, Ordering::Release);
    if SATB_MUTATORS_DONE.load(Ordering::Acquire) == SATB_MUTATORS_EXPECTED.load(Ordering::Relaxed)
    {
        TraceSliceProgress::Done
    } else {
        TraceSliceProgress::Retry
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
            register_slot_type(CONTROL);
            register_slot_type(SOURCE);
            runtime()
                .concurrent_slice_registry
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
                IdSet::default(),
                IdMap::default(),
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
                IdSet::default(),
                IdMap::default(),
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
            IdSet::default(),
            IdMap::default(),
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
                        layout_id: 1234,
                        gc_ref_mask: descriptor.as_ptr() as u64,
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
            IdSet::default(),
            IdMap::default(),
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
        // The pointer-free child is completed by its discovering slice.
        assert_eq!(jobs, 1 + (bitmap_words - 1).div_ceil(8));
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
        let (_, cycle) = root_handshake::begin().expect("no foreign root owner");
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
        // Pointer-free child is marked directly by its discovering slice.
        assert_eq!(jobs, 1 + slots.div_ceil(512));
        assert_eq!(
            cycle.work.lock().unwrap().scanned_bytes,
            ((slots + 1) * 8) as u64
        );
        assert!(cycle.deferred.lock().unwrap().is_empty());
        drop(consumer);
        assert_eq!(cycle.queue.end_epoch(), 0);
        marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
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
    let (_, cycle) = root_handshake::begin().expect("no foreign root owner");
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
        let (_, cycle) = root_handshake::begin().expect("no foreign root owner");
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
        let (work, plan) = finish_or_recover(&cycle);
        assert_eq!(work.scanned_bytes, count as u64 * 8);
        assert!(cycle.is_marked(deleted as usize));
        sweep_if_planned(plan);
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
    crate::gc::minor_collect_internal();
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
    assert_eq!(
        runtime().heap.lock().unwrap().young_allocated_bytes,
        GC_HEADER_SIZE + 128
    );
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
            let (_, cycle) = root_handshake::begin().expect("no foreign root owner");
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
            let (work, plan) = finish_or_recover(&cycle);
            assert_eq!(work.scanned_bytes, ((2 * count - 1) * 8) as u64);
            // Leaf frames need a mark bit, but no separate trace job.
            let expected_jobs = count + if fan_out { count.div_ceil(512) - 1 } else { 0 };
            assert_eq!(jobs, expected_jobs);
            sweep_if_planned(plan);
            let retained = willow_gc_allocated_bytes();
            willow_pop_roots(count as i32);
            willow_push_root(&mut handles[0]);
            crate::gc::minor_collect_internal();
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
        let (_, cycle) = root_handshake::begin().expect("no foreign root owner");
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
        let (work, plan) = finish_or_recover(&cycle);
        assert_eq!(work.scanned_bytes, (count - 1) as u64 * 8);
        assert!(cycle.is_marked(deleted as usize));
        sweep_if_planned(plan);
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
fn satb_phase_flip_stops_owners_before_activation_and_remark() {
    let _guard = runtime_test_guard();
    for _cycle_number in 0..2 {
        reset_internal_for_test();
        let ready = Arc::new(AtomicBool::new(false));
        let deleted = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = {
            let (ready, deleted, done) = (ready.clone(), deleted.clone(), done.clone());
            std::thread::spawn(move || {
                let _registration = MutatorRegistration::new();
                let mut owner = willow_alloc_typed(8, 1);
                let child = willow_alloc(8);
                unsafe {
                    store_gc_reference(owner.cast(), child);
                }
                willow_push_root(&mut owner);
                tx.send(child as usize).unwrap();
                ready.store(true, Ordering::Release);
                while GC_MARK_PHASE.load(Ordering::Acquire) == 0 {
                    willow_gc_safepoint();
                    std::thread::yield_now();
                }
                assert!(!runtime().stop_requested.load(Ordering::Acquire));
                willow_gc_write_barrier(
                    owner,
                    child,
                    std::ptr::null_mut(),
                    GcStoreDestination::ObjectField as i64,
                );
                unsafe {
                    store_gc_reference(owner.cast(), std::ptr::null_mut());
                }
                deleted.store(true, Ordering::Release);
                while !done.load(Ordering::Acquire) {
                    willow_gc_safepoint();
                    std::thread::yield_now();
                }
                willow_pop_root();
            })
        };
        let child = rx.recv().unwrap();
        while !ready.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        let (_, cycle) = root_handshake::begin().unwrap();
        while !deleted.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        let (_, plan) = mark_closure::finish(&cycle, Instant::now()).unwrap();
        assert_eq!(GC_MARK_PHASE.load(Ordering::Acquire), 0);
        assert!(cycle.is_marked(child));
        assert!(cycle.satb_shards.is_empty());
        let stops = crate::gc_telemetry::stops::snapshot_stops();
        assert_eq!(stops.by_reason[0].completed, 1);
        assert!(stops.by_reason[1].completed >= 1);
        sweep::concurrent(plan);
        done.store(true, Ordering::Release);
        worker.join().unwrap();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
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
                    let mutator = crate::gc::MutatorRegistration::new();
                    let mut tls = tlab_state_for_test();
                    let mut object = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
                    willow_push_root(&mut object);
                    ready.fetch_add(1, Ordering::Release);
                    while !done.load(Ordering::Acquire) {
                        willow_gc_safepoint();
                        std::thread::yield_now();
                    }
                    willow_pop_root();
                    drop(mutator);
                })
            })
            .collect();
        while ready.load(Ordering::Acquire) != count {
            std::thread::yield_now();
        }
        TLAB_ACCOUNTING_RECORD_VISITS.store(0, Ordering::Relaxed);
        root_handshake::INITIAL_PARTICIPANTS.store(0, Ordering::Relaxed);
        willow_gc_collect();
        println!("initial_stw mutators={count} completed_stops=1");
        assert_eq!(
            willow_gc_allocated_bytes(),
            (count * (GC_HEADER_SIZE + 8)) as i64
        );
        // Initial stopped retirement, final accounting and the allocated-byte
        // snapshot remain linear in the mutator/TLAB count. Each remark stop
        // reads every record once; with many mutators a retry-capped remark
        // completes as a recovery stop instead, so both kinds count.
        let stops = crate::gc_telemetry::stops::snapshot_stops();
        let remarks = (stops.by_reason[1].requests + stops.by_reason[3].requests) as usize;
        let expected_reads = (2 + remarks) * count;
        assert_eq!(
            TLAB_ACCOUNTING_RECORD_VISITS.load(Ordering::Relaxed),
            expected_reads
        );
        assert_eq!(
            crate::gc_telemetry::stops::snapshot_stops().by_reason[0].requests,
            1
        );
        assert_eq!(
            root_handshake::INITIAL_PARTICIPANTS.load(Ordering::Relaxed),
            count + 1
        );
        assert_eq!(
            root_handshake::INITIAL_PARTICIPANTS.load(Ordering::Relaxed),
            count + 1
        );
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
            "root_handshake mutators={count} accounting_record_reads={} initial_stops=1",
            expected_reads
        );
    }
    reset_internal_for_test();
}

#[test]
fn legacy_owner_unregistering_during_activation_still_publishes_its_roots() {
    // A legacy owner (explicit root stack claimed before registering) that
    // acknowledges activation and then unregisters must not leave before the
    // publication round: it keeps using its roots, so dropping them from the
    // snapshot freed its live object (chroot_01 hang on ubuntu CI).
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let leaving = Arc::new(AtomicBool::new(false));
    let collected = Arc::new(AtomicBool::new(false));
    let owner = {
        let (leaving, collected) = (leaving.clone(), collected.clone());
        std::thread::spawn(move || {
            let mut tls = tlab_state_for_test();
            let mut object = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
            willow_push_root(&mut object);
            willow_gc_register_mutator();
            while !runtime().poll_requested.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            // Acknowledge the activation round only; roots are not taken yet.
            willow_gc_safepoint();
            leaving.store(true, Ordering::Release);
            willow_gc_unregister_mutator();
            while !collected.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
            willow_pop_root();
        })
    };
    let blocker = {
        let leaving = leaving.clone();
        std::thread::spawn(move || {
            willow_gc_register_mutator();
            while !leaving.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            // Hold the activation round open until the owner has left the
            // registry; with the owner held back, give up after a bound.
            let owner_left = || runtime().coord.0.lock().unwrap().mutators.len() == 1;
            let deadline = Instant::now() + Duration::from_millis(200);
            while !owner_left() && Instant::now() < deadline {
                std::thread::yield_now();
            }
            willow_gc_safepoint();
            willow_gc_unregister_mutator();
        })
    };
    while runtime().coord.0.lock().unwrap().mutators.len() != 2 {
        std::thread::yield_now();
    }
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        (GC_HEADER_SIZE + 8) as i64,
        "the leaving owner's rooted object was reclaimed"
    );
    collected.store(true, Ordering::Release);
    blocker.join().unwrap();
    owner.join().unwrap();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn root_handshake_refuses_an_unregistered_owner_with_a_nonempty_stack() {
    // collect_internal's early skip releases coord before begin(); begin must
    // decide again under its own hold or an owner leaving in between is lost.
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let (held_tx, held_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let owner = std::thread::spawn(move || {
        let mut tls = tlab_state_for_test();
        let mut object = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
        willow_push_root(&mut object);
        willow_gc_register_mutator();
        willow_gc_unregister_mutator();
        held_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        willow_pop_root();
    });
    held_rx.recv().unwrap();
    assert!(root_handshake::begin().is_none());
    assert!(!runtime().coord.0.lock().unwrap().stop_requested);
    assert_eq!(GC_MARK_PHASE.load(Ordering::Acquire), 0);
    release_tx.send(()).unwrap();
    owner.join().unwrap();
    reset_internal_for_test();
}

#[test]
fn native_stack_root_transfers_publish_during_initial_handshake() {
    let _guard = runtime_test_guard();
    for resume_during_mark in [false, true] {
        reset_internal_for_test();
        let mut root = willow_alloc_typed(8, 1);
        // A traced child takes a queue job; a pointer-free leaf would be
        // marked inline and break the one-job-per-mark accounting below.
        let child = willow_alloc_typed(8, 1);
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
            let mutator = crate::gc::MutatorRegistration::new();
            ready.store(true, Ordering::Release);
            while !done.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            drop(mutator);
        })
    };
    while !ready.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    std::thread::spawn(|| {
        let mutator = crate::gc::MutatorRegistration::new();
        assert!(multi_mutator_active());
        willow_gc_collect();
        crate::gc::minor_collect_internal();
        drop(mutator);
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
                epoch_index::EpochIndex::regions(epoch_index::RegionIndex::capture(&state)),
                IdSet::default(),
                IdMap::default(),
                0,
            ));
            assert!(cycle.objects.claim(object));
            marking::set_concurrent_cycle(&mut state, Some(cycle.clone()));
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
                        inline_children: false,
                        branch_children: Vec::new(),
                        discovery_counts: [0; 3],
                        region_hint: None,
                        inline_types: [None; 4],
                        local_child: None,
                        cycle: &cycle,
                        work: Default::default(),
                        deferred: Vec::new(),
                        native_counts: Default::default(),
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
            marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
            GC_MARK_PHASE.store(0, Ordering::Release);
            cycle.queue.end_epoch();
        }
        assert!(
            crate::gc_telemetry::stops::snapshot_stops()
                .by_reason
                .iter()
                .take(2)
                .any(|r| r.requests > 0)
        );
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        println!(
            "closure outcome={outcome} empty_queue_waited_for_batch=true global_stops=required_stw"
        );
    }
    reset_internal_for_test();
}

#[test]
fn m3_real_closure_last_work_latency_and_cpu() {
    let _guard = runtime_test_guard();
    for notification in [false, true] {
        let mut latencies = Vec::new();
        let mut cpu_ns = 0u64;
        let mut cpu_valid = true;
        for sample in 0..100 {
            let (last_tx, last_rx) = std::sync::mpsc::channel();
            reset_internal_for_test();
            mark_closure::READER_WAIT_OBSERVED.store(false, Ordering::Relaxed);
            let object = willow_alloc(8) as usize;
            let cycle = {
                let mut state = runtime().heap.lock().unwrap();
                let cycle = Arc::new(ConcurrentCycle::with_index(
                    epoch_index::EpochIndex::regions(epoch_index::RegionIndex::capture(&state)),
                    IdSet::default(),
                    IdMap::default(),
                    0,
                ));
                assert!(cycle.objects.claim(object));
                marking::set_concurrent_cycle(&mut state, Some(cycle.clone()));
                GC_MARK_PHASE.store(1, Ordering::Release);
                cycle
            };
            let ready = Arc::new(AtomicBool::new(false));
            let assist = {
                let (cycle, ready) = (cycle.clone(), ready.clone());
                std::thread::spawn(move || {
                    {
                        // Model a drain that has completed its queue item but still
                        // owns unpublished accounting or an exception batch.
                        let owner = cycle.termination.enter();
                        cycle.active_drains.fetch_add(1, Ordering::AcqRel);
                        let mut batch = MarkBatch {
                            cpu_start: None,
                            inline_children: false,
                            branch_children: Vec::new(),
                            discovery_counts: [0; 3],
                            region_hint: None,
                            inline_types: [None; 4],
                            local_child: None,
                            cycle: &cycle,
                            work: Default::default(),
                            deferred: Vec::new(),
                            native_counts: Default::default(),
                            unindexed: HashSet::new(),
                        };
                        batch.work.object(GC_HEADER_SIZE + 8, 0);
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
                        // Complete real accounting after closure has observed an
                        // unfinished drain. Vary completion relative to its wait.
                        std::thread::sleep(Duration::from_micros([0, 100, 500, 1500][sample % 4]));
                        drop(batch);
                        let last_work = Instant::now();
                        drop(owner);
                        last_tx.send(last_work).unwrap();
                    }
                })
            };
            while !ready.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            assert!(cycle.queue.snapshot().is_drained());
            let cpu_start = crate::gc_telemetry::workers::thread_cpu_ns();
            let finished = mark_closure::finish_with_policy(
                &cycle,
                Instant::now(),
                std::thread::sleep,
                || {},
                notification,
            );
            let terminated = Instant::now();
            if let Some(elapsed) = cpu_start
                .zip(crate::gc_telemetry::workers::thread_cpu_ns())
                .and_then(|(a, b)| b.checked_sub(a))
            {
                cpu_ns += elapsed;
            } else {
                cpu_valid = false;
            }
            latencies.push(
                terminated
                    .duration_since(last_rx.recv().unwrap())
                    .as_nanos(),
            );
            assist.join().unwrap();
            assert_eq!(cycle.active_drains.load(Ordering::Acquire), 0);
            // A loaded host can exhaust the remark budget after the last owner;
            // production then completes the cycle with a stopped recovery.
            let (work, plan) = match finished {
                Some((work, plan)) => (work, Some(plan)),
                None => {
                    let (work, plan, _) = mark_closure::finish_stopped(&cycle, Instant::now());
                    (work, plan)
                }
            };
            assert_eq!(work.marked_bytes, (GC_HEADER_SIZE + 8) as u64);
            assert_eq!(GC_MARK_PHASE.load(Ordering::Acquire), 0);
            assert!(cycle.queue.snapshot().is_drained());
            sweep_if_planned(plan);
            assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
            assert!(
                crate::gc_telemetry::stops::snapshot_stops()
                    .by_reason
                    .iter()
                    .take(2)
                    .any(|r| r.requests > 0)
            );
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), 0);
        }
        latencies.sort_unstable();
        println!(
            "m3_termination notification={notification} samples=100 p50_ns={} p99_ns={} closure_cpu_ns={cpu_ns} cpu_valid={cpu_valid}",
            latencies[49], latencies[98]
        );
    }
    reset_internal_for_test();
}

#[test]
fn concurrent_closure_and_sweep_preserve_a_new_active_tlab_after_stopped_ownership_transfer() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    SNAPSHOT_READY.store(false, Ordering::Relaxed);
    MUTATION_DONE.store(false, Ordering::Relaxed);
    const CONTROL: u32 = 0xFE12;
    register_slot_type(CONTROL);
    runtime()
        .concurrent_slice_registry
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
            let mutator = crate::gc::MutatorRegistration::new();
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
            drop(mutator);
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
            .take(2)
            .any(|r| r.requests > 0)
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
            epoch_index::EpochIndex::regions(epoch_index::RegionIndex::capture(&state)),
            IdSet::default(),
            IdMap::default(),
            1,
        ));
        assert!(cycle.objects.claim(parent));
        marking::set_concurrent_cycle(&mut state, Some(cycle.clone()));
        GC_MARK_PHASE.store(1, Ordering::Release);
        cycle
    };
    let ready = Arc::new(AtomicBool::new(false));
    let producer = {
        let (cycle, ready) = (cycle.clone(), ready.clone());
        std::thread::spawn(move || {
            let mutator = crate::gc::MutatorRegistration::new();
            cycle.active_drains.fetch_add(1, Ordering::AcqRel);
            let mut batch = MarkBatch {
                cpu_start: None,
                inline_children: false,
                branch_children: Vec::new(),
                discovery_counts: [0; 3],
                region_hint: None,
                inline_types: [None; 4],
                local_child: None,
                cycle: &cycle,
                work: Default::default(),
                deferred: Vec::new(),
                native_counts: Default::default(),
                unindexed: HashSet::new(),
            };
            batch.work.object(GC_HEADER_SIZE + 8, 1);
            ready.store(true, Ordering::Release);
            while !cycle.closing.load(Ordering::Acquire) {
                willow_gc_safepoint();
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
            drop(mutator);
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
            .take(2)
            .any(|r| r.requests > 0)
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
                IdSet::default(),
                IdMap::default(),
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
            // A deadline can return unused claim capacity even with one
            // worker. The shared finite cap and pending-publication assertions
            // still hold; early safe return is part of the bounded drain.
            assert!(calls > 0);
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
fn marker_pool_drains_each_claim_in_one_batch() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let pool = mark_workers::Pool::new(1).unwrap();
    for objects in [256usize, 1024, 4096] {
        BUDGET_RETRY_CALLS.store(0, Ordering::Relaxed);
        BUDGET_RETRY_LIMIT.store(0, Ordering::Relaxed);
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
            IdSet::default(),
            IdMap::default(),
            0,
        );
        cycle.slices.insert(0xFA79, budget_retry_slice);
        let cycle = Arc::new(cycle);
        for address in 1..=objects {
            cycle.enqueue(address as *mut u8);
        }
        mark_workers::BACKGROUND_DRAINS.store(0, Ordering::Relaxed);
        // A zero collector budget leaves every object to the single worker.
        pool.run(&cycle, 0);
        let drains = mark_workers::BACKGROUND_DRAINS.load(Ordering::Relaxed);
        // A minor request or an empty sample on a loaded host ends the worker
        // job early; closure then drains what the worker left.
        cycle.drain(usize::MAX);
        assert_eq!(BUDGET_RETRY_CALLS.load(Ordering::Relaxed), objects);
        assert_eq!(cycle.objects.marked_count(), objects);
        // One drain per 32-object claim plus the final empty probe, not one
        // drain, batch publication and safepoint per object.
        assert!(
            drains <= objects.div_ceil(32) + 1,
            "objects={objects} drains={drains}"
        );
        assert!(!cycle.worker_failed.load(Ordering::Acquire));
        assert_eq!(registered_mutator_count(), 0);
        assert!(cycle.queue.snapshot().is_drained());
        assert_eq!(cycle.queue.end_epoch(), 0);
    }
    reset_internal_for_test();
}

#[test]
fn concurrent_closure_paces_contention_and_retains_work_until_release() {
    let _guard = runtime_test_guard();
    for waits_before_release in [1usize, 4, 16, 64] {
        reset_internal_for_test();
        // The remark stop's 1ms wall-clock budget can expire on a preempted CI
        // runner, which correctly takes the stopped fallback this test excludes.
        old_cycle::REMARK_WORK_NS_FOR_TEST.store(100_000_000, Ordering::Relaxed);
        BUDGET_RETRY_CALLS.store(0, Ordering::Relaxed);
        // Finite on the old spin loop too, so regression fails without hanging.
        BUDGET_RETRY_LIMIT.store(waits_before_release * 1024, Ordering::Relaxed);
        let object = willow_alloc_object(0xFA79, 8) as usize;
        let cycle = {
            let mut state = runtime().heap.lock().unwrap();
            let mut cycle = ConcurrentCycle::with_index(
                epoch_index::EpochIndex::regions(epoch_index::RegionIndex::capture(&state)),
                IdSet::default(),
                IdMap::default(),
                0,
            );
            cycle.slices.insert(0xFA79, budget_retry_slice);
            cycle.enqueue(object as *mut u8);
            let cycle = Arc::new(cycle);
            marking::set_concurrent_cycle(&mut state, Some(cycle.clone()));
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
                .take(2)
                .any(|r| r.requests > 0)
        );
        println!(
            "closure waits={waits} retries={previous_calls} upper_bound={} requested_sleep_ns={} completion_calls=1 global_stops=required_stw",
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
            let mut parent = willow_alloc_typed(8, 1);
            let child = willow_alloc(8) as usize;
            if deletion {
                unsafe {
                    store_gc_reference(parent.cast(), child as *mut u8);
                }
            }
            willow_push_root(&mut parent);
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let done = Arc::new(AtomicBool::new(false));
            let writer = {
                let (done, parent) = (done.clone(), parent as usize);
                std::thread::spawn(move || {
                    let registration = MutatorRegistration::new();
                    let value = if deletion {
                        std::ptr::null_mut()
                    } else {
                        child as *mut u8
                    };
                    willow_gc_write_barrier(
                        parent as *mut u8,
                        if deletion {
                            child as *mut u8
                        } else {
                            std::ptr::null_mut()
                        },
                        value,
                        GcStoreDestination::ObjectField as i64,
                    );
                    ready_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    unsafe {
                        store_gc_reference(parent as *mut *mut u8, value);
                    }
                    if unregister {
                        drop(registration);
                        return;
                    }
                    while !done.load(Ordering::Acquire) {
                        willow_gc_safepoint();
                        std::thread::yield_now();
                    }
                    drop(registration);
                })
            };
            ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let observer = std::thread::spawn(move || {
                while !runtime().stop_requested.load(Ordering::Acquire) {
                    std::thread::yield_now();
                }
                assert_eq!(GC_MARK_PHASE.load(Ordering::Acquire), 0);
                assert!(runtime().heap.lock().unwrap().concurrent_cycle.is_none());
                release_tx.send(()).unwrap();
            });
            let (_, cycle) = root_handshake::begin().unwrap();
            observer.join().unwrap();
            cycle.drain(usize::MAX);
            assert_eq!(cycle.is_marked(child), !deletion);
            let (_, plan) = finish_or_recover(&cycle);
            sweep_if_planned(plan);
            assert_eq!(GC_MARK_PHASE.load(Ordering::Acquire), 0);
            done.store(true, Ordering::Release);
            writer.join().unwrap();
            willow_pop_root();
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), 0);
        }
    }
    reset_internal_for_test();
}

#[test]
fn root_activation_late_registration_waits_for_the_stw_boundary() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let started = Arc::new(AtomicBool::new(false));
    let registered = Arc::new(AtomicBool::new(false));
    let worker = with_stw(
        crate::gc_telemetry::stops::StopReason::InitialMark,
        |coord, _| {
            assert!(coord.stop_requested);
            let (started, registered) = (started.clone(), registered.clone());
            let worker = {
                let (started, registered) = (started.clone(), registered.clone());
                std::thread::spawn(move || {
                    started.store(true, Ordering::Release);
                    let _registration = MutatorRegistration::new();
                    registered.store(true, Ordering::Release);
                })
            };
            while !started.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            assert!(
                !registered.load(Ordering::Acquire),
                "new registration cannot pass COORD while stopped"
            );
            worker
        },
    );
    worker.join().unwrap();
    assert!(registered.load(Ordering::Acquire));
    reset_internal_for_test();
}

#[test]
fn a_registered_mutator_that_panics_still_unregisters() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    willow_gc_register_mutator();
    let registered = registered_mutator_count();
    let worker = std::thread::spawn(|| {
        let _mutator = crate::gc::MutatorRegistration::new();
        panic!("worker unwinds while registered as a mutator");
    });
    assert!(worker.join().is_err());
    assert_eq!(registered_mutator_count(), registered);
    // Both handshake rounds must find every participant, so this returns
    // rather than blocking on the thread that is already gone.
    willow_gc_collect();
    willow_gc_unregister_mutator();
    reset_internal_for_test();
}

/// Contain the wedge in a child process: the collector cannot recover from a
/// mutator that stopped without unregistering, so it reports and aborts.
fn run_wedge_child(test: &str) -> std::process::Output {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env("WILLOW_HANDSHAKE_WEDGE_CHILD", "1")
        .env("WILLOW_GC_HANDSHAKE_TIMEOUT_SECS", "2")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("wedged collector never reported: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

/// Without the bound, this is the shape that spent a whole 90-minute Windows
/// CI job inside one `cv.wait` and printed nothing at all (willow-utqy).
#[test]
fn a_mutator_that_dies_registered_reports_instead_of_hanging_the_collector() {
    let test = "gc::concurrent_tests::a_mutator_that_dies_registered_reports_instead_of_hanging_the_collector";
    if std::env::var_os("WILLOW_HANDSHAKE_WEDGE_CHILD").is_none() {
        let output = run_wedge_child(test);
        assert!(!output.status.success(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("stop-the-world park did not complete within 2s"),
            "{output:?}"
        );
        assert!(
            stderr.contains("registered mutator(s) never acknowledged"),
            "{output:?}"
        );
        assert!(stderr.contains("gc::MutatorRegistration"), "{output:?}");
        return;
    }
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    willow_gc_register_mutator();
    // Deliberately violate the contract the guard exists to keep: register
    // through the raw entry point and let the thread end still registered.
    std::thread::spawn(|| willow_gc_register_mutator())
        .join()
        .unwrap();
    willow_gc_collect();
    unreachable!("the collector returned from a wedged handshake");
}

/// Explicitly selected same-host throughput benchmark. No timing assertion:
/// the median comparison is retained as review evidence, including failures.
#[test]
#[ignore = "reference-store throughput measurement"]
fn satb_reference_store_throughput_baseline_and_current() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let owner = willow_alloc(8);
    let values = [willow_alloc(8), willow_alloc(8)];
    let mutator = MutatorRegistration::new();
    willow_gc_add_runtime_root(owner);
    for value in values {
        willow_gc_add_runtime_root(value);
    }
    for entries in [1024, 65536, 4194304] {
        let passes = (16_777_216 / entries).max(1);
        let stores = entries * passes;
        let mut slots: Vec<_> = (0..entries)
            .map(|_| std::sync::atomic::AtomicPtr::new(values[0]))
            .collect();
        let mut baseline = Vec::new();
        let mut current = Vec::new();
        // Warm both compiled variants before the three measured pairs.
        unsafe {
            barrier::reference_store_batch::<false>(owner, &mut slots, values, 1);
            barrier::reference_store_batch::<true>(owner, &mut slots, values, 1);
        }
        for run in 0..3 {
            for satb in if run & 1 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let start = Instant::now();
                unsafe {
                    if satb {
                        barrier::reference_store_batch::<true>(
                            std::hint::black_box(owner),
                            &mut slots,
                            std::hint::black_box(values),
                            passes,
                        );
                    } else {
                        barrier::reference_store_batch::<false>(
                            std::hint::black_box(owner),
                            &mut slots,
                            std::hint::black_box(values),
                            passes,
                        );
                    }
                }
                let ns = start.elapsed().as_nanos();
                println!(
                    "reference_store target={}-{} entries={entries} stores={stores} phase=0 satb={satb} run={run} elapsed_ns={ns}",
                    std::env::consts::ARCH,
                    std::env::consts::OS
                );
                if satb {
                    current.push(ns);
                } else {
                    baseline.push(ns);
                }
                assert_eq!(
                    slots.last().unwrap().load(Ordering::Acquire),
                    values[(entries - 1 + passes - 1) & 1]
                );
            }
        }
        baseline.sort_unstable();
        current.sort_unstable();
        let regression = (current[1] as f64 / baseline[1] as f64 - 1.0) * 100.0;
        let throughput_loss = (1.0 - baseline[1] as f64 / current[1] as f64) * 100.0;
        println!(
            "reference_store_median entries={entries} stores={stores} baseline_ns={} current_ns={} regression_percent={regression:.3} throughput_loss_percent={throughput_loss:.3} g8_pass={}",
            baseline[1],
            current[1],
            throughput_loss <= 5.0
        );
    }
    for capacity in [1, 256] {
        let cycle = with_stw(
            crate::gc_telemetry::stops::StopReason::InitialMark,
            |_, _| {
                let mut state = runtime().heap.lock().unwrap();
                let mut cycle = ConcurrentCycle::with_index(
                    epoch_index::EpochIndex::regions(epoch_index::RegionIndex::capture(&state)),
                    IdSet::default(),
                    IdMap::default(),
                    1,
                );
                cycle.satb_shards = satb::SatbShards::new(capacity);
                let cycle = Arc::new(cycle);
                marking::set_concurrent_cycle(&mut state, Some(cycle.clone()));
                GC_MARK_PHASE.store(1, Ordering::Release);
                cycle
            },
        );
        let mut slots: Vec<_> = (0..1024)
            .map(|_| std::sync::atomic::AtomicPtr::new(values[0]))
            .collect();
        let mut timings = Vec::new();
        for run in 0..3 {
            let start = Instant::now();
            unsafe {
                barrier::reference_store_batch::<true>(owner, &mut slots, values, 100);
            }
            let ns = start.elapsed().as_nanos();
            timings.push(ns);
            println!(
                "reference_store_active capacity={capacity} run={run} stores=102400 elapsed_ns={ns}"
            );
        }
        timings.sort_unstable();
        println!(
            "reference_store_active_median capacity={capacity} stores=102400 elapsed_ns={}",
            timings[1]
        );
        let (_, plan) = finish_or_recover(&cycle);
        sweep_if_planned(plan);
        assert!(values.iter().all(|value| cycle.is_marked(*value as usize)));
    }
    willow_gc_remove_runtime_root(owner);
    for value in values {
        willow_gc_remove_runtime_root(value);
    }
    drop(mutator);
    reset_internal_for_test();
}

#[test]
fn satb_registration_owns_buffer_and_unwind_unregister_drains_it() {
    let _guard = runtime_test_guard();
    for capacity in [1, 256] {
        for unwind in [false, true] {
            reset_internal_for_test();
            let old = willow_alloc(8) as usize;
            let cycle = {
                let mut state = runtime().heap.lock().unwrap();
                let mut cycle = ConcurrentCycle::with_index(
                    epoch_index::EpochIndex::regions(epoch_index::RegionIndex::capture(&state)),
                    IdSet::default(),
                    IdMap::default(),
                    1,
                );
                cycle.satb_shards = satb::SatbShards::new(capacity);
                let cycle = Arc::new(cycle);
                marking::set_concurrent_cycle(&mut state, Some(cycle.clone()));
                GC_MARK_PHASE.store(1, Ordering::Release);
                cycle
            };
            std::thread::spawn(move || {
                let result = std::panic::catch_unwind(|| {
                    let _registration = MutatorRegistration::new();
                    let local = safepoint::current_satb_buffer().unwrap();
                    {
                        let coord = runtime().coord.0.lock().unwrap();
                        let record = &coord.mutators[&std::thread::current().id()];
                        assert!(Arc::ptr_eq(&local, record.satb_buffer.as_ref().unwrap()));
                    }
                    satb_delete(old as *mut u8);
                    if unwind {
                        panic!("exercise mutator unwind drain");
                    }
                });
                assert_eq!(result.is_err(), unwind);
                assert!(safepoint::current_satb_buffer().is_none());
                assert!(
                    !runtime()
                        .coord
                        .0
                        .lock()
                        .unwrap()
                        .mutators
                        .contains_key(&std::thread::current().id())
                );
            })
            .join()
            .unwrap();
            assert!(
                cycle.is_marked(old),
                "unregister must publish without a collector drain"
            );
            assert!(cycle.satb_shards.is_empty());
            cycle.drain(usize::MAX);
            assert!(cycle.queue.snapshot().is_drained());
            GC_MARK_PHASE.store(0, Ordering::Release);
            marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
            reset_internal_for_test();
        }
    }
}

#[test]
fn satb_remark_cap_preserves_rendezvous_deletions_through_stopped_completion() {
    let _guard = runtime_test_guard();
    for count in [1, 16, 257] {
        for unregister in [false, true] {
            reset_internal_for_test();
            let mut owners = Vec::new();
            for _ in 0..count {
                let owner = willow_alloc_typed(8, 1);
                let child = willow_alloc_typed(8, 1);
                let leaf = willow_alloc(8);
                unsafe {
                    store_gc_reference(owner.cast(), child);
                    store_gc_reference(child.cast(), leaf);
                }
                owners.push(owner as usize);
            }
            willow_alloc(8); // white garbage proves the fallback also sweeps.
            let cycle = {
                let mut state = runtime().heap.lock().unwrap();
                let cycle = Arc::new(ConcurrentCycle::with_index(
                    epoch_index::EpochIndex::regions(epoch_index::RegionIndex::capture(&state)),
                    IdSet::default(),
                    IdMap::default(),
                    0,
                ));
                for owner in &owners {
                    assert!(cycle.objects.claim(*owner));
                    // Model an already completed owner scan. Recovery retains
                    // epoch accounting instead of retracing claimed objects.
                    cycle.work.lock().unwrap().object(GC_HEADER_SIZE + 8, 1);
                }
                marking::set_concurrent_cycle(&mut state, Some(cycle.clone()));
                GC_MARK_PHASE.store(1, Ordering::Release);
                cycle
            };
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (arm_tx, arm_rx) = std::sync::mpsc::channel();
            let (armed_tx, armed_rx) = std::sync::mpsc::channel();
            let done = Arc::new(AtomicBool::new(false));
            let worker = {
                let done = done.clone();
                std::thread::spawn(move || {
                    let registration = MutatorRegistration::new();
                    let mut owners: Vec<_> =
                        owners.into_iter().map(|value| value as *mut u8).collect();
                    for owner in &mut owners {
                        willow_push_root(owner);
                    }
                    ready_tx.send(()).unwrap();
                    loop {
                        if arm_rx.try_recv().is_ok() {
                            break;
                        }
                        willow_gc_safepoint();
                        std::thread::yield_now();
                    }
                    armed_tx.send(()).unwrap();
                    // Keep the rendezvous open until all deletions publish.
                    while !runtime().stop_requested.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                    for &owner in &owners {
                        let old = unsafe { load_gc_reference(owner.cast()) };
                        willow_gc_write_barrier(
                            owner,
                            old,
                            std::ptr::null_mut(),
                            GcStoreDestination::ObjectField as i64,
                        );
                        unsafe {
                            store_gc_reference(owner.cast(), std::ptr::null_mut());
                        }
                    }
                    if unregister {
                        willow_pop_roots(count as i32);
                        drop(registration);
                        while !done.load(Ordering::Acquire) {
                            std::thread::yield_now();
                        }
                    } else {
                        while !done.load(Ordering::Acquire) {
                            willow_gc_safepoint();
                            std::thread::yield_now();
                        }
                        willow_pop_roots(count as i32);
                        drop(registration);
                    }
                })
            };
            ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let mut rechecks = 0;
            let finished = mark_closure::finish_with_hooks(
                &cycle,
                Instant::now(),
                |_| std::thread::yield_now(),
                || {
                    rechecks += 1;
                    arm_tx.send(()).unwrap();
                    armed_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                },
            );
            assert!(finished.is_none());
            assert_eq!(rechecks, 1);
            assert!(cycle.stopped_completion_required.load(Ordering::Acquire));
            assert!(!cycle.worker_failed.load(Ordering::Acquire));
            assert!(
                !cycle.queue.snapshot().is_drained(),
                "late snapshot edges must remain queued"
            );
            assert_eq!(
                crate::gc_telemetry::stops::snapshot_stops().by_reason[1].requests,
                2
            );
            let (work, plan, freed) = mark_closure::finish_stopped(&cycle, Instant::now());
            assert_eq!(freed, 0);
            assert_eq!(sweep::concurrent(plan.unwrap()), GC_HEADER_SIZE + 8);
            assert_eq!(work.marked_bytes, (3 * count * (GC_HEADER_SIZE + 8)) as u64);
            assert_eq!(
                willow_gc_allocated_bytes(),
                (3 * count * (GC_HEADER_SIZE + 8)) as i64
            );
            assert_eq!(GC_MARK_PHASE.load(Ordering::Acquire), 0);
            assert_eq!(
                crate::gc_telemetry::stops::snapshot_stops().by_reason[1].requests as usize,
                mark_closure::MAX_REMARK_ROUNDS - 1
            );
            done.store(true, Ordering::Release);
            worker.join().unwrap();
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), 0);
            println!(
                "remark_cap nodes={} unregister={unregister} optimistic_rechecks={rechecks} total_remark_rounds=3 retained_then_reclaimed=true",
                3 * count
            );
        }
    }
    reset_internal_for_test();
}

#[test]
fn satb_closure_wave_cap_resumes_retrying_work_with_bounded_recovery() {
    unsafe fn retry_slot_slice(
        object: *mut u8,
        cursor: usize,
        budget: usize,
        output: &mut Vec<*mut u8>,
    ) -> TraceSliceProgress {
        if matches!(
            unsafe { budget_retry_slice(object, cursor, budget, output) },
            TraceSliceProgress::Retry
        ) {
            TraceSliceProgress::Retry
        } else {
            unsafe { snapshot_slot_slice(object, cursor, budget, output) }
        }
    }
    let _guard = runtime_test_guard();
    for count in [1, 16, 257] {
        reset_internal_for_test();
        BUDGET_RETRY_CALLS.store(0, Ordering::Relaxed);
        BUDGET_RETRY_LIMIT.store(usize::MAX, Ordering::Relaxed);
        const TYPE: u32 = 0xFA79;
        register_slot_type(TYPE);
        let mut parents = Vec::new();
        for _ in 0..count {
            let parent = willow_alloc_object(TYPE as i64, 8);
            let child = willow_alloc(8);
            unsafe {
                store_gc_reference(parent.cast(), child);
            }
            parents.push(parent);
        }
        let cycle = {
            let mut state = runtime().heap.lock().unwrap();
            let mut cycle = ConcurrentCycle::with_index(
                epoch_index::EpochIndex::regions(epoch_index::RegionIndex::capture(&state)),
                IdSet::default(),
                IdMap::default(),
                0,
            );
            cycle.slices.insert(TYPE, retry_slot_slice);
            let cycle = Arc::new(cycle);
            for parent in parents {
                cycle.enqueue(parent);
            }
            marking::set_concurrent_cycle(&mut state, Some(cycle.clone()));
            GC_MARK_PHASE.store(1, Ordering::Release);
            cycle
        };
        let mut waits = 0;
        assert!(
            mark_closure::finish_with_wait(&cycle, Instant::now(), |_| {
                waits += 1;
            })
            .is_none()
        );
        assert_eq!(waits, mark_closure::MAX_DRAIN_WAVES - 1);
        let calls = BUDGET_RETRY_CALLS.load(Ordering::Relaxed);
        assert!(
            (mark_closure::MAX_DRAIN_WAVES..=mark_closure::MAX_DRAIN_WAVES * 256).contains(&calls)
        );
        assert!(cycle.stopped_completion_required.load(Ordering::Acquire));
        assert!(!cycle.queue.snapshot().is_drained());
        let mut recovery_waits = 0;
        // Still Retry on entry: release contention only after recovery actually
        // yields twice with mutators resumed, without requesting another stop.
        let (work, plan, freed) =
            mark_closure::finish_recovery_with_wait(&cycle, Instant::now(), |delay| {
                assert_eq!(delay, Duration::from_millis(1));
                assert!(!runtime().stop_requested.load(Ordering::Acquire));
                assert_eq!(
                    crate::gc_telemetry::stops::snapshot_stops().by_reason[3].requests,
                    1
                );
                recovery_waits += 1;
                if recovery_waits == 2 {
                    BUDGET_RETRY_LIMIT.store(0, Ordering::Relaxed);
                }
            });
        assert_eq!(recovery_waits, 2);
        assert_eq!(freed, 0);
        sweep::concurrent(plan.expect("recovery publishes a sweep plan"));
        assert_eq!(
            BUDGET_RETRY_CALLS.load(Ordering::Relaxed),
            calls + count + 2
        );
        assert_eq!(work.marked_bytes, (2 * count * (GC_HEADER_SIZE + 8)) as u64);
        assert_eq!(
            willow_gc_allocated_bytes(),
            (2 * count * (GC_HEADER_SIZE + 8)) as i64
        );
        assert_eq!(
            crate::gc_telemetry::stops::snapshot_stops().by_reason[1].requests,
            1
        );
        let recovery = crate::gc_telemetry::stops::snapshot_stops().by_reason[3];
        assert!(recovery.requests >= 1);
        assert_eq!(recovery.work.swept_objects, 0);
        assert_eq!(GC_MARK_PHASE.load(Ordering::Acquire), 0);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        println!(
            "closure_cap objects={count} waves=128 concurrent_retry_calls={calls} bounded_recovery_completed=true"
        );
    }
    BUDGET_RETRY_LIMIT.store(0, Ordering::Relaxed);
    reset_internal_for_test();
}

#[test]
fn satb_bounded_native_and_retry_slice_complete_with_snapshot_retention() {
    unsafe fn retry_slot_slice(
        object: *mut u8,
        cursor: usize,
        budget: usize,
        output: &mut Vec<*mut u8>,
    ) -> TraceSliceProgress {
        if matches!(
            unsafe { budget_retry_slice(object, cursor, budget, output) },
            TraceSliceProgress::Retry
        ) {
            TraceSliceProgress::Retry
        } else {
            unsafe { snapshot_slot_slice(object, cursor, budget, output) }
        }
    }
    let _guard = runtime_test_guard();
    for (count, predrain) in [
        (1, true),
        (16, true),
        (257, true),
        (1, false),
        (16, false),
        (257, false),
    ] {
        reset_internal_for_test();
        BUDGET_RETRY_CALLS.store(0, Ordering::Relaxed);
        BUDGET_RETRY_LIMIT.store(usize::MAX, Ordering::Relaxed);
        const LEGACY: u32 = 0xFA7A;
        const RETRY: u32 = 0xFA79;
        willow_register_type(LEGACY, trace_slot);
        willow_register_type(RETRY, trace_slot);
        let mut parents = Vec::new();
        let mut deleted = Vec::new();
        for _ in 0..count {
            for kind in [LEGACY, RETRY] {
                let parent = willow_alloc_object(kind as i64, 8);
                let child = willow_alloc_typed(8, 1);
                let leaf = willow_alloc(8);
                unsafe {
                    store_gc_reference(parent.cast(), child);
                    store_gc_reference(child.cast(), leaf);
                }
                parents.push(parent);
                if kind == LEGACY {
                    deleted.push((parent, child));
                }
            }
        }
        let cycle = {
            let mut state = runtime().heap.lock().unwrap();
            let mut legacy = IdSet::default();
            legacy.insert(LEGACY);
            let mut cycle = ConcurrentCycle::with_index(
                epoch_index::EpochIndex::regions(epoch_index::RegionIndex::capture(&state)),
                legacy,
                IdMap::default(),
                0,
            );
            cycle.slices.insert(RETRY, retry_slot_slice);
            cycle.slices.insert(LEGACY, snapshot_slot_slice);
            let cycle = Arc::new(cycle);
            for parent in parents {
                cycle.enqueue(parent);
            }
            marking::set_concurrent_cycle(&mut state, Some(cycle.clone()));
            GC_MARK_PHASE.store(1, Ordering::Release);
            cycle
        };
        if predrain {
            cycle.drain_checked(4 * count);
        }
        assert!(cycle.deferred.lock().unwrap().is_empty());
        assert!(!cycle.queue.snapshot().is_drained());
        for (parent, child) in deleted {
            willow_gc_write_barrier(
                parent,
                child,
                std::ptr::null_mut(),
                GcStoreDestination::ObjectField as i64,
            );
            unsafe {
                store_gc_reference(parent.cast(), std::ptr::null_mut());
            }
        }
        let calls = BUDGET_RETRY_CALLS.load(Ordering::Relaxed);
        assert_eq!(calls == 0, !predrain);
        let mut waits = 0;
        if predrain {
            assert!(
                mark_closure::finish_with_wait(&cycle, Instant::now(), |_| waits += 1).is_none()
            );
        }
        assert_eq!(
            waits,
            if predrain {
                mark_closure::MAX_DRAIN_WAVES - 1
            } else {
                0
            }
        );
        assert_eq!(
            cycle.stopped_completion_required.load(Ordering::Acquire),
            predrain
        );
        assert!(!cycle.worker_failed.load(Ordering::Acquire));
        BUDGET_RETRY_LIMIT.store(0, Ordering::Relaxed);
        let (work, plan, freed) = mark_closure::finish_stopped(&cycle, Instant::now());
        assert_eq!(freed, 0);
        sweep::concurrent(plan.expect("recovery publishes a sweep plan"));
        let stopped_calls = BUDGET_RETRY_CALLS.load(Ordering::Relaxed);
        assert!(stopped_calls >= calls + count);
        assert_eq!(work.marked_bytes, (6 * count * (GC_HEADER_SIZE + 8)) as u64);
        assert_eq!(
            willow_gc_allocated_bytes(),
            (6 * count * (GC_HEADER_SIZE + 8)) as i64
        );
        assert_eq!(
            crate::gc_telemetry::stops::snapshot_stops().by_reason[1].requests,
            if predrain { 1 } else { 0 }
        );
        let recovery = crate::gc_telemetry::stops::snapshot_stops().by_reason[3];
        assert!(recovery.requests >= 1);
        assert_eq!(recovery.work.swept_objects, 0);
        assert_eq!(GC_MARK_PHASE.load(Ordering::Acquire), 0);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        println!(
            "mixed_bounded_retry graphs={count} predrain={predrain} retry_calls={calls} stopped_calls={stopped_calls} snapshot_retained_then_reclaimed=true"
        );
    }
    BUDGET_RETRY_LIMIT.store(0, Ordering::Relaxed);
    reset_internal_for_test();
}

#[test]
fn marker_discovery_diagnostics_count_duplicate_and_unindexed_candidates() {
    let _guard = runtime_test_guard();
    for count in [16usize, 128, 1024] {
        let before = crate::gc_telemetry::workers::discovery_snapshot();
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
            IdSet::default(),
            IdMap::default(),
            0,
        );
        for address in 1..=count {
            for _ in 0..8 {
                cycle.enqueue(address as *mut u8);
            }
        }
        cycle.enqueue((count + 1) as *mut u8);
        cycle.enqueue(std::ptr::null_mut());
        assert_eq!(cycle.queue.snapshot().injected, count as u64);
        cycle.drain(usize::MAX);
        assert_eq!(cycle.objects.marked_count(), count);
        assert_eq!(cycle.queue.end_epoch(), 0);
        drop(cycle);
        let after = crate::gc_telemetry::workers::discovery_snapshot();
        assert_eq!(
            after.pointer_checks - before.pointer_checks,
            (8 * count + 1) as u64
        );
        assert_eq!(after.duplicates - before.duplicates, (7 * count) as u64);
        assert_eq!(after.unindexed - before.unindexed, 1);
        println!(
            "objects={count} discoveries={} unique_jobs={count} duplicates={} unindexed=1",
            8 * count + 1,
            7 * count
        );
    }
}

static ENGINE_READER_ENTERED: AtomicBool = AtomicBool::new(false);
static ENGINE_MUTATORS_DONE: AtomicUsize = AtomicUsize::new(0);
static ENGINE_MUTATORS_EXPECTED: AtomicUsize = AtomicUsize::new(0);

unsafe fn await_engine_mutators(
    payload: *mut u8,
    _: usize,
    _: usize,
    children: &mut Vec<*mut u8>,
) -> TraceSliceProgress {
    if !ENGINE_READER_ENTERED.load(Ordering::Acquire) {
        assert!(
            std::thread::current()
                .name()
                .unwrap()
                .starts_with("willow-gc-mark-")
        );
        ENGINE_READER_ENTERED.store(true, Ordering::Release);
    }
    if ENGINE_MUTATORS_DONE.load(Ordering::Acquire)
        != ENGINE_MUTATORS_EXPECTED.load(Ordering::Relaxed)
    {
        return TraceSliceProgress::Retry;
    }
    children.push(unsafe { load_gc_reference(payload.cast()) });
    TraceSliceProgress::Done
}

#[test]
fn background_marker_snapshot_survives_registry_changes_and_mutator_allocations() {
    let _guard = runtime_test_guard();
    for count in [1, 5, 16] {
        reset_internal_for_test();
        ENGINE_READER_ENTERED.store(false, Ordering::Relaxed);
        ENGINE_MUTATORS_DONE.store(0, Ordering::Relaxed);
        ENGINE_MUTATORS_EXPECTED.store(count, Ordering::Relaxed);
        const TYPE: u32 = 0xFA99;
        register_slot_type(TYPE);
        runtime()
            .concurrent_slice_registry
            .lock()
            .unwrap()
            .insert(TYPE, await_engine_mutators);
        let parent = willow_alloc_object(TYPE as i64, 8);
        let child = willow_alloc(8);
        unsafe {
            store_gc_reference(parent.cast(), child);
        }
        let cycle = {
            let mut state = runtime().heap.lock().unwrap();
            let mut cycle = ConcurrentCycle::with_index(
                epoch_index::EpochIndex::regions(epoch_index::RegionIndex::capture(&state)),
                IdSet::from_iter([TYPE]),
                runtime().concurrent_trace_registry.lock().unwrap().clone(),
                1,
            );
            cycle.slices = runtime().concurrent_slice_registry.lock().unwrap().clone();
            let cycle = Arc::new(cycle);
            marking::set_concurrent_cycle(&mut state, Some(cycle.clone()));
            GC_MARK_PHASE.store(1, Ordering::Release);
            cycle
        };
        cycle.enqueue(parent);
        let mut mutators = Vec::new();
        for _ in 0..count {
            let cycle = cycle.clone();
            mutators.push(std::thread::spawn(move || {
                willow_gc_register_mutator();
                let deadline = Instant::now() + Duration::from_secs(10);
                while !ENGINE_READER_ENTERED.load(Ordering::Acquire) {
                    assert!(Instant::now() < deadline);
                    willow_gc_safepoint();
                    std::thread::yield_now();
                }
                // Mutation cannot invalidate the captured callback registry.
                register_slot_type(TYPE);
                let new = willow_alloc(8) as usize;
                assert!(
                    cycle.objects.retains_allocated(new),
                    "allocation published white"
                );
                willow_gc_unregister_mutator();
                ENGINE_MUTATORS_DONE.fetch_add(1, Ordering::Release);
            }));
        }
        let pool = mark_workers::Pool::new(1).unwrap();
        pool.run(&cycle, 0);
        for mutator in mutators {
            mutator.join().unwrap();
        }
        cycle.drain(usize::MAX);
        assert!(cycle.is_marked(parent as usize));
        assert!(cycle.is_marked(child as usize));
        assert!(!cycle.worker_failed.load(Ordering::Acquire));
        assert_eq!(cycle.queue.end_epoch(), 0);
        assert_eq!(registered_mutator_count(), 0);
        GC_MARK_PHASE.store(0, Ordering::Release);
        marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
        drop(pool);
        drop(cycle);
        println!(
            "background_workers=1 progressing_mutators={count} registry_replacements={count} black_allocations={count}"
        );
    }
    reset_internal_for_test();
}

static RETRY_UNTIL_ALLOCATED: AtomicBool = AtomicBool::new(false);

unsafe fn retry_until_mutator_allocated(
    payload: *mut u8,
    _: usize,
    _: usize,
    children: &mut Vec<*mut u8>,
) -> TraceSliceProgress {
    if !RETRY_UNTIL_ALLOCATED.load(Ordering::Acquire) {
        return TraceSliceProgress::Retry;
    }
    children.push(unsafe { load_gc_reference(payload.cast()) });
    TraceSliceProgress::Done
}

/// Regression (CI hang on every OS): a mutator allocating past an exhausted
/// runway while it owns the only remaining (retrying) slice must not livelock
/// in assist repayment or the allocation stall; the cycle then completes.
#[test]
fn exhausted_runway_allocation_returns_while_only_its_own_slice_retries() {
    let _guard = runtime_test_guard();
    for threads in [1, 4] {
        reset_internal_for_test();
        RETRY_UNTIL_ALLOCATED.store(false, Ordering::Relaxed);
        const TYPE: u32 = 0xFA9A;
        register_slot_type(TYPE);
        runtime()
            .concurrent_slice_registry
            .lock()
            .unwrap()
            .insert(TYPE, retry_until_mutator_allocated);
        let parent = willow_alloc_object(TYPE as i64, 8);
        let child = willow_alloc(8);
        unsafe {
            store_gc_reference(parent.cast(), child);
        }
        let cycle = {
            let mut state = runtime().heap.lock().unwrap();
            let mut cycle = ConcurrentCycle::with_index(
                epoch_index::EpochIndex::regions(epoch_index::RegionIndex::capture(&state)),
                IdSet::from_iter([TYPE]),
                runtime().concurrent_trace_registry.lock().unwrap().clone(),
                1,
            );
            cycle.slices = runtime().concurrent_slice_registry.lock().unwrap().clone();
            let cycle = Arc::new(cycle);
            marking::set_concurrent_cycle(&mut state, Some(cycle.clone()));
            GC_MARK_PHASE.store(1, Ordering::Release);
            cycle
        };
        cycle.enqueue(parent);
        let (done, allocated) = std::sync::mpsc::channel();
        let mutators: Vec<_> = (0..threads)
            .map(|_| {
                let done = done.clone();
                std::thread::spawn(move || {
                    willow_gc_register_mutator();
                    // Fresh TLAB: the slow path charges and assists.
                    for _ in 0..4 {
                        willow_alloc(8);
                    }
                    willow_gc_unregister_mutator();
                    done.send(()).unwrap();
                })
            })
            .collect();
        drop(done);
        let marker = {
            let cycle = cycle.clone();
            std::thread::spawn(move || {
                let pool = mark_workers::Pool::new(1).unwrap();
                pool.run(&cycle, 0);
            })
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        for _ in 0..threads {
            let left = deadline.saturating_duration_since(Instant::now());
            allocated
                .recv_timeout(left)
                .expect("allocation livelocked on a retrying slice");
        }
        assert!(cycle.assist_pacing.as_ref().unwrap().exhausted());
        assert!(cycle.native_retries.load(Ordering::Acquire) > 0);
        assert!(!cycle.is_marked(child as usize));
        RETRY_UNTIL_ALLOCATED.store(true, Ordering::Release);
        for mutator in mutators {
            mutator.join().unwrap();
        }
        marker.join().unwrap();
        cycle.drain(usize::MAX);
        assert!(cycle.is_marked(parent as usize));
        assert!(cycle.is_marked(child as usize));
        assert!(!cycle.worker_failed.load(Ordering::Acquire));
        assert_eq!(registered_mutator_count(), 0);
        GC_MARK_PHASE.store(0, Ordering::Release);
        marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
        println!(
            "retrying_slice_owner_threads={threads} retries={}",
            cycle.native_retries.load(Ordering::Relaxed)
        );
        drop(cycle);
    }
    reset_internal_for_test();
}

#[test]
fn pointer_free_leaf_fanout_uses_only_bounded_parent_jobs() {
    let _guard = runtime_test_guard();
    for count in [1_000usize, 10_000, 100_000] {
        reset_internal_for_test();
        let mut array = crate::array::willow_array_new(count as i64, 1);
        willow_push_root(&mut array);
        let mut leaves = Vec::with_capacity(count);
        for index in 0..count {
            let child = willow_alloc(8);
            leaves.push(child);
            crate::array::willow_array_set(array, index as i64, child as i64);
        }
        let (_, cycle) = root_handshake::begin().unwrap();
        let mut consumer = cycle.queue.register_assist();
        let mut jobs = 0;
        while !cycle.queue.snapshot().is_drained() {
            let (processed, _) = cycle.drain_worker(1, &mut Vec::new(), &mut consumer);
            assert_eq!(processed, 1);
            jobs += 1;
        }
        assert_eq!(jobs, 1 + count.div_ceil(512));
        for leaf in leaves {
            assert!(cycle.is_marked(leaf as usize));
        }
        assert_eq!(
            cycle.work.lock().unwrap().scanned_bytes,
            ((count + 1) * 8) as u64
        );
        println!(
            "leaf_fanout={count} queue_jobs={jobs} reference_slots={}",
            count + 1
        );
        drop(consumer);
        assert_eq!(cycle.queue.end_epoch(), 0);
        marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
        GC_MARK_PHASE.store(0, Ordering::Release);
        willow_pop_root();
    }
    reset_internal_for_test();
}

#[test]
fn concurrent_chain_continuations_bound_queue_publication_and_preserve_tails() {
    let _guard = runtime_test_guard();
    for count in [100usize, 1_000, 10_000] {
        reset_internal_for_test();
        let mut head = std::ptr::null_mut();
        willow_push_root(&mut head);
        for _ in 0..count {
            let node = willow_alloc_typed(8, 1);
            unsafe {
                store_gc_reference(node.cast(), head);
            }
            head = node;
        }
        let (_, cycle) = root_handshake::begin().unwrap();
        let mut consumer = cycle.queue.register_assist();
        let mut children = Vec::new();
        let mut scanned = 0;
        let before = cycle.queue.snapshot().injected;
        epoch_index::EPOCH_READS.set(0);
        while !cycle.queue.snapshot().is_drained() {
            let (processed, _) = cycle.drain_worker(64, &mut children, &mut consumer);
            assert!(processed > 0 && processed <= 64);
            scanned += processed;
            assert_eq!(cycle.active_drains.load(Ordering::Acquire), 0);
        }
        let readers = epoch_index::EPOCH_READS.get();
        assert_eq!(readers, count.div_ceil(64));
        eprintln!("inline-chain nodes={count} epoch_readers={readers}");
        assert_eq!(scanned, count);
        assert_eq!(
            cycle.queue.snapshot().injected - before,
            (count.div_ceil(64) - 1) as u64
        );
        assert_eq!(cycle.queue.snapshot().counter_underflows, 0);
        let mut node = head;
        for _ in 0..count {
            assert!(cycle.is_marked(node as usize));
            node = unsafe { load_gc_reference(node.cast()) };
        }
        assert!(node.is_null());
        drop(consumer);
        assert_eq!(cycle.queue.end_epoch(), 0);
        marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
        GC_MARK_PHASE.store(0, Ordering::Release);
        willow_pop_root();
    }
    reset_internal_for_test();
}

#[test]
fn concurrent_nonleaf_fanout_batches_claims_without_losing_shared_edges() {
    let _guard = runtime_test_guard();
    for count in [1_000usize, 10_000, 100_000] {
        reset_internal_for_test();
        let mut array = crate::array::willow_array_new(count as i64, 1);
        willow_push_root(&mut array);
        let mut shared = willow_alloc(8);
        willow_push_root(&mut shared);
        for index in 0..count {
            let child = willow_alloc_typed(8, 1);
            unsafe {
                store_gc_reference(child.cast(), shared);
            }
            crate::array::willow_array_set(array, index as i64, child as i64);
        }
        let (_, cycle) = root_handshake::begin().unwrap();
        let before = cycle.queue.snapshot().injected;
        let mut consumer = cycle.queue.register_assist();
        let mut children = Vec::new();
        epoch_index::EPOCH_READS.set(0);
        while !cycle.queue.snapshot().is_drained() {
            cycle.drain_worker(64, &mut children, &mut consumer);
        }
        let readers = epoch_index::EPOCH_READS.get();
        assert!(
            readers <= count.div_ceil(32) + 4 * count.div_ceil(512) + 8,
            "fanout={count} epoch_readers={readers}"
        );
        eprintln!("fanout={count} epoch_readers={readers}");
        // At most one slice job and one bounded local tail per 512 references.
        eprintln!(
            "fanout={count} before={before} after={:?}",
            cycle.queue.snapshot()
        );
        assert!(
            cycle.queue.snapshot().injected - before
                <= (count.div_ceil(32) + 2 * count.div_ceil(512) + 2) as u64
        );
        assert_eq!(cycle.queue.snapshot().counter_underflows, 0);
        assert!(cycle.is_marked(shared as usize));
        for index in 0..count {
            let child = crate::array::willow_array_get(array, index as i64) as usize;
            assert!(cycle.is_marked(child));
        }
        drop(consumer);
        assert_eq!(cycle.queue.end_epoch(), 0);
        marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
        GC_MARK_PHASE.store(0, Ordering::Release);
        willow_pop_root();
        willow_pop_root();
    }
    reset_internal_for_test();
}

#[test]
fn owned_pair_growth_bulk_barrier_retains_all_snapshot_edges() {
    use crate::array::{willow_array_new, willow_array_push_owned_pair};
    let _guard = runtime_test_guard();
    for count in [4usize, 1024, 16384] {
        reset_internal_for_test();
        {
            let mut state = runtime().heap.lock().unwrap();
            state.threshold_bytes = usize::MAX;
            state.pacer_trigger = u64::MAX;
            state.pacer.disable_for_test();
            state.nursery_threshold_bytes = usize::MAX;
        }
        let mut array = willow_array_new(0, 1);
        willow_push_root(&mut array);
        let mut pairs = Vec::new();
        let mut children = Vec::new();
        for _ in 0..count {
            let child = willow_alloc(8);
            let pair = willow_alloc_typed(16, 1);
            unsafe {
                store_gc_reference(pair.cast(), child);
            }
            willow_array_push_owned_pair(array, pair as i64);
            pairs.push(pair);
            children.push(child);
        }
        let (_, cycle) = root_handshake::begin().unwrap();
        assert!(pairs.iter().all(|p| !cycle.is_marked(*p as usize)));
        let incoming = willow_alloc_typed(16, 1);
        let before = runtime().write_barrier_calls.load(Ordering::Relaxed);
        willow_array_push_owned_pair(array, incoming as i64);
        // Deletion barriers buffer SATB roots until the owner's safepoint;
        // closure flushes these producers before draining their publications.
        barrier::flush_satb_current(false);
        let barriers = runtime().write_barrier_calls.load(Ordering::Relaxed) - before;
        // Allocation may assist queued slice work; count the growth's actual
        // barriers, not queue jobs also generated by those marker assists.
        assert!(barriers <= 4, "count={count} barriers={barriers}");
        cycle.drain(usize::MAX);
        for pointer in pairs.iter().chain(&children) {
            assert!(cycle.is_marked(*pointer as usize));
        }
        assert!(cycle.queue.snapshot().is_drained());
        assert_eq!(cycle.queue.snapshot().counter_underflows, 0);
        assert_eq!(cycle.queue.end_epoch(), 0);
        marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
        GC_MARK_PHASE.store(0, Ordering::Release);
        willow_gc_collect();
        assert_eq!(crate::array::willow_array_len(array), count as i64 + 1);
        willow_pop_root();
        eprintln!("bulk_growth edges={count} barriers={barriers}");
    }
    reset_internal_for_test();
}

#[test]
fn owned_pair_growth_bulk_barrier_remembers_young_destination_edges() {
    use crate::array::{willow_array_new, willow_array_push_owned_pair};
    let _guard = runtime_test_guard();
    for count in [1024usize, 16384] {
        reset_internal_for_test();
        {
            let mut state = runtime().heap.lock().unwrap();
            state.threshold_bytes = usize::MAX;
            state.pacer_trigger = u64::MAX;
            state.pacer.disable_for_test();
            state.nursery_threshold_bytes = usize::MAX;
        }
        let mut array = willow_array_new(0, 1);
        willow_push_root(&mut array);
        let mut tls = tlab_state_for_test();
        let child = willow_alloc(8);
        for index in 0..=count {
            let pair = willow_gc_alloc_slow(&mut tls, 1, 0, 16, 1);
            unsafe {
                store_gc_reference(pair.cast(), child);
                *pair.cast::<usize>().add(1) = index;
            }
            willow_array_push_owned_pair(array, pair as i64);
        }
        let buffer = unsafe { *array.cast::<usize>().add(willow_abi::array_layout::H_BUF) };
        assert_eq!(
            unsafe { (*payload_to_header(buffer as *mut u8)).generation },
            GC_GENERATION_OLD
        );
        assert!(
            runtime()
                .heap
                .lock()
                .unwrap()
                .remembered_set
                .contains(&buffer)
        );
        minor_collect_internal();
        for index in 0..=count {
            let pair = crate::array::willow_array_get(array, index as i64) as *mut u8;
            assert_eq!(unsafe { *pair.cast::<usize>().add(1) }, index);
            assert_eq!(unsafe { load_gc_reference(pair.cast()) }, child);
        }
        willow_pop_root();
        eprintln!("bulk_growth_young edges={}", count + 1);
    }
    reset_internal_for_test();
}

#[test]
fn m0_marker_slice_recovery_emits_without_panic_hook() {
    const CHILD: &str = "WILLOW_TEST_MARKER_DIAGNOSTIC";
    if std::env::var_os(CHILD).is_none() {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "gc::concurrent_tests::m0_marker_slice_recovery_emits_without_panic_hook",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stderr = String::from_utf8(child.stderr).unwrap();
        assert!(child.status.success(), "{stderr}");
        let records: Vec<_> = stderr.lines().filter(|s| s.starts_with('{')).collect();
        assert_eq!(records.len(), 3, "{stderr}");
        for (record, reason) in records.iter().zip([
            "native trace exceeded its slot budget",
            "native trace did not advance its cursor",
            "retry must not publish a partial native slice",
        ]) {
            assert!(record.len() < crate::gc_telemetry::failure_record::FAILURE_RECORD_LIMIT);
            assert!(record.contains("\"kind\":\"marker_panic\""), "{record}");
            assert!(record.contains(reason), "{record}");
            println!("{record}");
        }
        return;
    }
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    std::panic::set_hook(Box::new(|_| {}));
    unsafe fn invalid_slice(
        _: *mut u8,
        offset: usize,
        max: usize,
        children: &mut Vec<*mut u8>,
    ) -> TraceSliceProgress {
        match offset {
            0 => {
                children.resize(max + 1, std::ptr::null_mut());
                TraceSliceProgress::Done
            }
            1 => TraceSliceProgress::Continue(offset),
            _ => {
                children.push(std::ptr::null_mut());
                TraceSliceProgress::Retry
            }
        }
    }
    for offset in 0..3 {
        let mut cycle = ConcurrentCycle::new([], IdSet::default(), IdMap::default(), 1);
        cycle.slices.insert(0xFA79, invalid_slice);
        let before = crate::gc_telemetry::workers::snapshot().marker_panics;
        let result = cycle
            .drain_recovering(|| cycle.snapshot_native_slice(0, 0xFA79, offset, &mut Vec::new()));
        assert_eq!(result, 0);
        assert!(cycle.worker_failed.load(Ordering::Acquire));
        assert_eq!(
            crate::gc_telemetry::workers::snapshot().marker_panics,
            before + 1
        );
        // A failed cycle never attempts another partial scan or reports again.
        cycle.drain_recovering::<()>(|| panic!("must not drain a failed cycle"));
        cycle.queue.end_epoch();
    }
    reset_internal_for_test();
}

/// Recovery bounds each stop independently of graph and heap size. Graph
/// callbacks and exactly-once native cleanup execute only after resume.
#[test]
fn stage6_recovery_sweep_is_outside_stop() {
    if std::env::var("WILLOW_STAGE6_RECOVERY_CHILD").as_deref() != Ok("1") {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "gc::concurrent_tests::stage6_recovery_sweep_is_outside_stop",
                "--nocapture",
            ])
            .env("WILLOW_STAGE6_RECOVERY_CHILD", "1")
            .env("WILLOW_GC_REMARK_MAX_SLOTS", "1")
            .env("WILLOW_GC_REMARK_MAX_BYTES", "1")
            .env("WILLOW_GC_REMARK_MAX_WORK_NS", "1")
            .output()
            .unwrap();
        print!("{}", String::from_utf8_lossy(&output.stdout));
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("stage6_recovery nodes=4096"));
        return;
    }
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    static DROPPED: std::sync::LazyLock<Mutex<HashSet<usize>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));
    unsafe fn dropped(payload: *mut u8) {
        assert!(!runtime().stop_requested.load(Ordering::Acquire));
        assert!(DROPPED.lock().unwrap().insert(payload as usize));
        DROPS.fetch_add(1, Ordering::Relaxed);
    }
    let _guard = runtime_test_guard();
    for count in [16, 256, 4096] {
        reset_internal_for_test();
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        old_lab::OLD_LABS_FOR_TEST.store(true, Ordering::Release);
        mark_closure::RECOVERY_SAMPLES.lock().unwrap().clear();
        DROPS.store(0, Ordering::Relaxed);
        DROPPED.lock().unwrap().clear();
        const TYPE: u32 = 0xFA90;
        register_slot_type(TYPE);
        willow_register_drop(TYPE, dropped);
        let mut root = std::ptr::null_mut();
        willow_push_root(&mut root);
        for _ in 0..count {
            let next = willow_alloc_object(TYPE as i64, 8);
            unsafe { store_gc_reference(next.cast(), root) };
            root = next;
        }
        let before = willow_gc_allocated_bytes();
        for index in 0..count {
            willow_alloc_object(TYPE as i64, 8);
            if index % 16 == 0 {
                willow_alloc((GC_LARGE_OBJECT_THRESHOLD + 8) as i64);
            }
        }
        // Start with an entirely undrained graph, so every chain edge is
        // processed by recovery between stopped ownership/publication slices.
        let (_, cycle) = root_handshake::begin().unwrap();
        let (work, plan, _) = mark_closure::finish_stopped(&cycle, Instant::now());
        sweep::concurrent(plan.unwrap());
        assert_eq!(work.objects, count as u64);
        assert_eq!(DROPS.load(Ordering::Relaxed), count);
        let stops = crate::gc_telemetry::stops::snapshot_stops();
        let recovery = &stops.by_reason[3];
        assert_eq!(willow_gc_allocated_bytes(), before);
        assert!(recovery.requests >= 1);
        let samples = mark_closure::RECOVERY_SAMPLES.lock().unwrap();
        assert_eq!(samples.len() as u64, recovery.requests);
        for &[slots, bytes, regions, metadata] in samples.iter() {
            assert!(slots <= mark_closure::RECOVERY_SLOTS);
            assert!(bytes <= mark_closure::RECOVERY_BYTES);
            assert!(regions <= mark_closure::RECOVERY_SLOTS);
            assert!(metadata <= mark_closure::RECOVERY_SLOTS / 2);
        }
        assert!(samples.iter().map(|sample| sample[2]).sum::<u64>() >= (count / 16) as u64);
        let max_slots = samples.iter().map(|s| s[0]).max().unwrap();
        let max_bytes = samples.iter().map(|s| s[1]).max().unwrap();
        let max_metadata = samples.iter().map(|s| s[3]).max().unwrap();
        let published = samples.iter().map(|s| s[2]).sum::<u64>();
        println!(
            "stage6_bounds nodes={count} max_slots={max_slots} max_bytes={max_bytes} max_metadata={max_metadata} published_regions={published} stopped_callbacks=0 stopped_trace=0"
        );
        drop(samples);
        assert_eq!(recovery.work.swept_objects, 0);
        println!(
            "stage6_recovery nodes={count} roots=1 configured_slots=1 configured_bytes=1 configured_work_ns=1 recovery_stops={} recovery_metadata_objects={} recovery_swept_objects={} recovery_stopped_ns={}",
            recovery.requests,
            recovery.work.metadata_objects,
            recovery.work.swept_objects,
            recovery.stopped.total_ns,
        );
        willow_pop_roots(1);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        assert_eq!(DROPS.load(Ordering::Relaxed), 2 * count);
        assert_eq!(
            crate::gc_telemetry::stops::snapshot_stops().by_reason[3]
                .work
                .swept_objects,
            0,
        );
    }
    old_lab::OLD_LABS_FOR_TEST.store(false, Ordering::Release);
    reset_internal_for_test();
}

#[test]
fn bounded_cancellation_clears_partial_publication_without_touching_active_tlabs() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    {
        let mut state = runtime().heap.lock().unwrap();
        state.threshold_bytes = usize::MAX;
        state.pacer_trigger = u64::MAX;
        state.pacer.disable_for_test();
        state.nursery_threshold_bytes = usize::MAX;
    }
    let mut root = willow_alloc_typed(8, 1);
    let child = willow_alloc(8);
    unsafe { store_gc_reference(root.cast(), child) };
    willow_push_root(&mut root);
    let (_, cycle) = root_handshake::begin().unwrap();
    {
        let mut state = runtime().heap.lock().unwrap();
        for region in &mut state.old_regions {
            region.sweep_pending = true;
            region.sweep_quarantined = true;
        }
    }
    let black = willow_alloc(8);
    // Model a black header published from a cached pending-region LAB.
    HeapObject::from_payload(GcPayload::from_raw(black).unwrap()).begin_trace();
    assert!(HeapObject::from_payload(GcPayload::from_raw(black).unwrap()).marked());
    let mut tls = tlab_state_for_test();
    let generated = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
    assert!(!generated.is_null());
    mark_closure::cancel(&cycle);
    assert_eq!(GC_MARK_PHASE.load(Ordering::Acquire), 0);
    assert!(!HeapObject::from_payload(GcPayload::from_raw(black).unwrap()).marked());
    {
        let state = runtime().heap.lock().unwrap();
        assert!(
            state
                .old_regions
                .iter()
                .all(|r| !r.sweep_pending && !r.sweep_quarantined)
        );
        assert!(
            state
                .tlab_chunks
                .iter()
                .any(|chunk| chunk.owner_state.is_some())
        );
    }
    willow_gc_collect();
    assert_eq!(unsafe { load_gc_reference(root.cast()) }, child);
    willow_pop_root();
    willow_gc_collect();
    reset_internal_for_test();
}

#[test]
fn native_retry_exhaustion_resolves_recovery_without_reclamation() {
    const KEY: &str = "WILLOW_TEST_NATIVE_RETRY_EXHAUSTION";
    if std::env::var_os(KEY).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "gc::concurrent_tests::native_retry_exhaustion_resolves_recovery_without_reclamation", "--nocapture"])
            .env(KEY, "1").output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(
            stderr.contains(
                "native trace Retry exhausted bounded recovery allowance; graph not reclaimed"
            ),
            "{stderr}"
        );
        assert!(
            stderr.contains("RETRY_LIMIT_REACHED attempts=128 waits=127 recovery_stops=1"),
            "{stderr}"
        );
        assert!(!stderr.contains("UNSAFE_RECLAMATION"), "{stderr}");
        assert!(!stderr.contains("RECOVERY_RETURNED"), "{stderr}");
        return;
    }
    static RECOVERY: AtomicBool = AtomicBool::new(false);
    static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
    static WAITS: AtomicUsize = AtomicUsize::new(0);
    unsafe fn retry(_: *mut u8, _: usize, _: usize, _: &mut Vec<*mut u8>) -> TraceSliceProgress {
        assert!(!runtime().stop_requested.load(Ordering::Acquire));
        if RECOVERY.load(Ordering::Acquire) {
            let attempts = ATTEMPTS.fetch_add(1, Ordering::Relaxed) + 1;
            assert!(attempts <= mark_closure::MAX_NATIVE_RECOVERY_RETRIES);
            if attempts == mark_closure::MAX_NATIVE_RECOVERY_RETRIES {
                let waits = WAITS.load(Ordering::Relaxed);
                let stops = crate::gc_telemetry::stops::snapshot_stops().by_reason[3].requests;
                assert_eq!(waits, attempts - 1);
                assert_eq!(stops, 1);
                eprintln!(
                    "RETRY_LIMIT_REACHED attempts={attempts} waits={waits} recovery_stops={stops}"
                );
            }
        }
        TraceSliceProgress::Retry
    }
    unsafe fn drop_probe(_: *mut u8) {
        eprintln!("UNSAFE_RECLAMATION");
    }
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
    const TYPE: u32 = 0xFA98;
    willow_register_type_capability(
        TYPE,
        trace_slot,
        NativeTraceCapability::SliceConcurrentSafe(retry),
    );
    willow_register_drop(TYPE, drop_probe);
    let mut parent = willow_alloc_object(TYPE as i64, 8);
    let child = willow_alloc_object(TYPE as i64, 8);
    unsafe { store_gc_reference(parent.cast(), child) };
    willow_push_root(&mut parent);
    willow_alloc_object(TYPE as i64, 8); // unreachable drop probe must not run either
    let (_, cycle) = root_handshake::begin().unwrap();
    assert!(mark_closure::finish_with_wait(&cycle, Instant::now(), |_| {}).is_none());
    assert!(cycle.stopped_completion_required.load(Ordering::Acquire));
    RECOVERY.store(true, Ordering::Release);
    mark_closure::finish_recovery_with_wait(&cycle, Instant::now(), |_| {
        assert!(!runtime().stop_requested.load(Ordering::Acquire));
        WAITS.fetch_add(1, Ordering::Relaxed);
    });
    eprintln!("RECOVERY_RETURNED");
}

#[test]
fn allocation_assist_progress_latency_and_thread_fairness() {
    let _guard = runtime_test_guard();
    for threads in [1usize, 5, 16] {
        if std::env::var("WILLOW_ASSIST_THREADS")
            .ok()
            .is_some_and(|value| value != threads.to_string())
        {
            continue;
        }
        reset_internal_for_test();
        {
            let mut state = runtime().heap.lock().unwrap();
            state.threshold_bytes = usize::MAX;
            state.pacer_trigger = u64::MAX;
            state.nursery_threshold_bytes = usize::MAX;
        }
        let objects = 8192;
        let mut cycle = ConcurrentCycle::new(
            (1..=objects).map(|address| {
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
            IdSet::default(),
            IdMap::default(),
            0,
        );
        let runway = 512 * 1024;
        cycle.assist_pacing = Some(assist::Pacing::new(
            (objects * GC_HEADER_SIZE) as u64,
            runway,
        ));
        let cycle = Arc::new(cycle);
        let allocated = AtomicU64::new(0);
        let completion_bytes = AtomicU64::new(0);
        cycle.enqueue_satb_batch(&(1..=objects).collect::<Vec<_>>());
        marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), Some(cycle.clone()));
        GC_MARK_PHASE.store(1, Ordering::Release);
        let start = Instant::now();
        let barrier = std::sync::Barrier::new(threads);
        let reports = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for _ in 0..threads {
                let barrier = &barrier;
                let allocated = &allocated;
                let completion_bytes = &completion_bytes;
                let cycle = &cycle;
                handles.push(scope.spawn(move || {
                    willow_gc_register_mutator();
                    assist::CREDITED.set(0);
                    barrier.wait();
                    let cpu = crate::gc_telemetry::workers::thread_cpu_ns();
                    let mut samples = Vec::new();
                    let mut peak_debt = 0;
                    for _ in 0..1024 {
                        let started = Instant::now();
                        // Exercise the real runtime allocation/assist path.
                        allocated.fetch_add((1024 + GC_HEADER_SIZE) as u64, Ordering::Relaxed);
                        assert!(!allocate_object(0, 0, 1024, 0).is_null());
                        if cycle.queue.snapshot().is_drained() {
                            let _ = completion_bytes.compare_exchange(
                                0,
                                allocated.load(Ordering::Relaxed),
                                Ordering::Relaxed,
                                Ordering::Relaxed,
                            );
                        }
                        samples.push(started.elapsed().as_nanos() as u64);
                        peak_debt = peak_debt.max(assist::debt_units());
                    }
                    let cpu = cpu
                        .zip(crate::gc_telemetry::workers::thread_cpu_ns())
                        .map(|(a, b)| b - a);
                    let work = assist::CREDITED.get();
                    let debt = assist::debt_units();
                    willow_gc_unregister_mutator();
                    assert_eq!(assist::debt_units(), 0);
                    samples.sort_unstable();
                    (
                        work,
                        debt,
                        peak_debt,
                        cpu,
                        samples[512],
                        samples[1013],
                        samples[1023],
                    )
                }));
            }
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(
            cycle.queue.snapshot().is_drained(),
            "assist-only marking must finish"
        );
        assert_eq!(
            reports.iter().map(|r| r.0).sum::<u64>(),
            (objects * GC_HEADER_SIZE) as u64
        );
        for (thread, report) in reports.iter().enumerate() {
            println!(
                "allocation_assist threads={threads} thread={thread} work_bytes={} debt_q16={} peak_debt_q16={} allocation_cpu_ns={:?} allocation_p50_ns={} allocation_p99_ns={} allocation_max_ns={}",
                report.0, report.1, report.2, report.3, report.4, report.5, report.6
            );
        }
        let hist = crate::gc_telemetry::observations::snapshot().intervals
            [crate::gc_telemetry::observations::Kind::Assist as usize];
        let quantile = |percent: u64| {
            let rank = (hist.count * percent).div_ceil(100);
            let mut count = 0;
            hist.buckets
                .iter()
                .position(|n| {
                    count += n;
                    count >= rank
                })
                .map_or(0, |bucket| {
                    1u64.checked_shl(bucket as u32).unwrap_or(u64::MAX)
                })
        };
        println!(
            "assist_latency threads={threads} samples={} p50_upper_ns={} p99_upper_ns={} max_ns={} total_ns={}",
            hist.count,
            quantile(50),
            quantile(99),
            hist.max_ns,
            hist.total_ns
        );
        let completion = completion_bytes.load(Ordering::Relaxed);
        let overshoot = completion.saturating_sub(runway);
        // Advisory pacing cannot bound an owner descheduled while holding the
        // last work item. Report the isolated controller's overshoot; the real
        // reservation safety bound is exercised by allocation_assist_pressure.
        assert!(completion > 0);
        println!(
            "assist_runway threads={threads} runway={runway} completion_bytes={completion} overshoot_bytes={overshoot}"
        );
        let measurement = crate::gc_telemetry::measurement::snapshot();
        println!(
            "allocation_assist threads={threads} allocations={} elapsed_ns={} assist_samples={} assist_cpu_ns={}",
            threads * 1024,
            start.elapsed().as_nanos(),
            measurement.assist_samples,
            measurement.assist_cpu_ns
        );
        assert_eq!(cycle.active_drains.load(Ordering::Acquire), 0);
        assert_eq!(cycle.queue.end_epoch(), 0);
        GC_MARK_PHASE.store(0, Ordering::Release);
        marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
        drop(cycle);
    }
    reset_internal_for_test();
}

#[test]
fn assist_remark_boundary_republishes_satb_tail() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    for stop in [false, true] {
        let cycle = Arc::new(ConcurrentCycle::new(
            (1..=32).map(|address| {
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
            IdSet::default(),
            IdMap::default(),
            0,
        ));
        cycle.enqueue_satb_batch(&(1..=32).collect::<Vec<_>>());
        cycle.closing.store(!stop, Ordering::Release);
        runtime().poll_requested.store(stop, Ordering::Release);
        let other = cycle.clone();
        let work = std::thread::spawn(move || other.drain_checked(8))
            .join()
            .unwrap();
        assert_eq!(work, GC_HEADER_SIZE as u64);
        assert_eq!(cycle.active_drains.load(Ordering::Acquire), 0);
        assert!(!cycle.queue.snapshot().is_drained());
        runtime().poll_requested.store(false, Ordering::Release);
        // Collector may finish the published tail across a stopped boundary.
        assert_eq!(cycle.drain_checked(64), (31 * GC_HEADER_SIZE) as u64);
        assert!(cycle.queue.snapshot().is_drained());
        assert_eq!(cycle.queue.end_epoch(), 0);
    }
    reset_internal_for_test();
}

#[test]
fn allocation_assist_pressure_preserves_hard_reservation_bound() {
    let _guard = runtime_test_guard();
    for threads in [1usize, 5, 16] {
        reset_internal_for_test();
        let limit = 16 * 1024 * 1024;
        runtime().heap.lock().unwrap().memory_limit_bytes = Some(limit);
        let peak = AtomicUsize::new(0);
        let started = Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..threads {
                let peak = &peak;
                scope.spawn(move || {
                    let _registration = safepoint::MutatorRegistration::new();
                    for i in 0..1024 {
                        assert!(!willow_alloc(4096).is_null());
                        if i % 16 == 0 {
                            let state = runtime().heap.lock().unwrap();
                            let reserved = state.old_reserved_bytes + state.tlab_reserved_bytes;
                            peak.fetch_max(reserved, Ordering::Relaxed);
                            assert!(reserved <= limit);
                        }
                    }
                });
            }
        });
        marking::shutdown_mark_workers();
        assert_eq!(registered_mutator_count(), 0);
        willow_gc_collect();
        willow_gc_unregister_mutator();
        println!(
            "assist_pressure threads={threads} allocations={} elapsed_ns={} peak_reserved={} limit={limit} assist_enabled={}",
            threads * 1024,
            started.elapsed().as_nanos(),
            peak.load(Ordering::Relaxed),
            std::env::var("WILLOW_GC_ASSIST").unwrap_or_else(|_| "1".into())
        );
    }
    reset_internal_for_test();
}

#[test]
fn inline_satb_roots_survive_minor_overlap_and_unregister_without_runtime_calls() {
    let _guard = runtime_test_guard();
    for count in [1, 16, 64] {
        reset_internal_for_test();
        willow_gc_register_mutator();
        let mut tls = tlab_state_for_test();
        let children: Vec<_> = (0..2 * count)
            .map(|_| willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0))
            .collect();
        let (_, cycle) = root_handshake::begin().unwrap();
        // A real runtime barrier admits storage, without publishing our graph.
        let owner = willow_alloc(8);
        willow_gc_write_barrier(
            owner,
            std::ptr::null_mut(),
            owner,
            GcStoreDestination::ObjectField as i64
                | willow_abi::GC_STORE_OWNER_HAS_HEADER
                | willow_abi::GC_STORE_VALUE_HAS_HEADER,
        );
        let calls = runtime().write_barrier_calls.load(Ordering::Relaxed);
        let context = crate::generated_context::active() as usize;
        let address = unsafe {
            *((context + willow_abi::generated_context::BARRIER_BUFFER_OFFSET as usize)
                as *const usize)
        };
        assert_ne!(address, 0);
        use willow_abi::barrier_buffer as abi;
        for pair in children.chunks_exact(2) {
            unsafe {
                let length = (address + abi::LENGTH_OFFSET as usize) as *mut usize;
                let limit = *((address + abi::LIMIT_OFFSET as usize) as *const usize);
                let n = *length;
                assert!(n + 2 <= limit);
                let base = *(address as *const *mut usize);
                base.add(n).write(pair[0] as usize);
                base.add(n + 1).write(pair[1] as usize);
                *length = n + 2;
            }
        }
        assert_eq!(runtime().write_barrier_calls.load(Ordering::Relaxed), calls);
        // No stack/root slot points to these children. Only the inline buffer
        // retains both deletion and insertion references across nursery moves.
        minor::collect_owned();
        cycle.drain(usize::MAX);
        assert!(cycle.objects.marked_count() >= 2 * count);
        assert_eq!(
            runtime().heap.lock().unwrap().moved_objects,
            2 * count as u64
        );
        willow_gc_unregister_mutator();
        assert_eq!(
            unsafe {
                *((context + willow_abi::generated_context::BARRIER_BUFFER_OFFSET as usize)
                    as *const usize)
            },
            0
        );
        GC_MARK_PHASE.store(0, Ordering::Release);
        marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
        cycle.queue.end_epoch();
        reset_internal_for_test();
    }
}
