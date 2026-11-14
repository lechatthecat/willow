use super::*;

fn owner() -> crate::runtime::Runtime {
    crate::runtime::Runtime::new(crate::scheduler::environment_worker_config)
}

#[test]
fn private_phase_cells_do_not_replace_the_process_abi_cell() {
    let first = owner();
    let second = owner();
    assert!(std::ptr::eq(
        crate::runtime::process_default().mark_phase(),
        &GC_MARK_PHASE
    ));
    assert!(!std::ptr::eq(first.gc.mark_phase(), &GC_MARK_PHASE));
    assert!(!std::ptr::eq(first.gc.mark_phase(), second.gc.mark_phase()));
    for phase in [1, 2, 0] {
        first.gc.mark_phase().store(phase, Ordering::Release);
        assert_eq!(first.gc.mark_phase().load(Ordering::Acquire), phase);
        assert_eq!(second.gc.mark_phase().load(Ordering::Acquire), 0);
    }
    // Pointer identity also proves that no copied or aggregated phase cell
    // interposes between the default runtime and the exported ABI symbol.
    assert!(std::ptr::eq(
        crate::runtime::process_default().mark_phase(),
        &GC_MARK_PHASE
    ));
}

#[test]
fn private_mutator_registration_preserves_owner_and_nested_lifetime() {
    let first = owner();
    let second = owner();
    let id = std::thread::current().id();
    {
        let registration = MutatorRegistration::new_in(&first);
        let buffer = current_satb_buffer_in(&first.gc).unwrap();
        assert!(current_thread_is_registered_in(&first.gc));
        assert!(!current_thread_is_registered_in(&second.gc));
        assert!(current_satb_buffer_in(&second.gc).is_none());
        // This thread serves the default runtime, not the private owner.
        assert!(!current_thread_is_registered());
        {
            let _nested = MutatorRegistration::new_in(&first);
            assert!(Arc::ptr_eq(
                &buffer,
                &current_satb_buffer_in(&first.gc).unwrap()
            ));
        }
        assert!(first.gc.coord.0.lock().unwrap().mutators.contains_key(&id));
        assert!(second.gc.coord.0.lock().unwrap().mutators.is_empty());
        drop(registration);
        assert!(first.gc.coord.0.lock().unwrap().mutators.is_empty());
        assert!(current_satb_buffer_in(&first.gc).is_none());
    }
    let _next = MutatorRegistration::new_in(&second);
    assert!(current_thread_is_registered_in(&second.gc));
    assert!(!current_thread_is_registered_in(&first.gc));
}

#[test]
fn private_mutator_unwind_unregisters_only_its_owner() {
    let first = owner();
    let second = owner();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _registration = MutatorRegistration::new_in(&first);
        panic!("exercise private mutator unwind");
    }));
    assert!(result.is_err());
    assert!(first.gc.coord.0.lock().unwrap().mutators.is_empty());
    assert!(second.gc.coord.0.lock().unwrap().mutators.is_empty());
    assert!(current_satb_buffer_in(&first.gc).is_none());
    let _registration = MutatorRegistration::new_in(&second);
    assert!(current_thread_is_registered_in(&second.gc));
}

#[test]
fn private_mutator_retirement_forgets_only_its_tlab_records() {
    let first = owner();
    let second = owner();
    let ready = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        for runtime in [&first, &second] {
            let ready = &ready;
            scope.spawn(crate::runtime::serving(move || {
                // The record must be retired before the stack storage dies.
                let mut tlab = GcTlabState::new();
                let registration = MutatorRegistration::new_in(runtime);
                let address = &raw mut tlab as usize;
                let object = tlab_alloc_slow_in(runtime, address, 0, 0, 8, 0);
                assert!(!object.is_null());
                assert_ne!(tlab.cursor.load(Ordering::Acquire), 0);
                ready.wait();
                drop(registration);
                assert_eq!(tlab.cursor.load(Ordering::Acquire), 0);
                assert_eq!(tlab.limit.load(Ordering::Acquire), 0);
                let state = runtime.gc.heap.lock().unwrap();
                assert!(state.tlab_states.is_empty());
                assert!(state.tlab_owners.is_empty());
                assert_eq!(state.total_allocs, 1);
                assert_eq!(state.tlab_chunks.len(), 1);
                assert!(state.tlab_chunks[0].owner_state.is_none());
            }));
        }
    });
}

#[test]
fn private_old_buffers_and_large_allocations_use_their_own_heap_and_phase() {
    for buffered in [false, true] {
        let first = owner();
        let second = owner();
        first.gc.old_lab_policy.store(buffered, Ordering::Release);
        second.gc.old_lab_policy.store(buffered, Ordering::Release);
        first.gc.mark_phase().store(1, Ordering::Release);
        for runtime in [&first, &second] {
            let _registration = MutatorRegistration::new_in(runtime);
            for size in [8, 24, GC_TLAB_MAX_OBJECT_SIZE + 8] {
                let payload = allocate_old_in(runtime, 0, 0, size as i64, 0);
                assert!(!payload.is_null());
                let state = runtime.gc.heap.lock().unwrap();
                let region_index = state.old_addresses.candidate(payload as usize).unwrap();
                let region = &state.old_regions[region_index];
                let object = HeapObject::from_payload(GcPayload::from_raw(payload).unwrap());
                let granule =
                    (object.as_ptr() as usize - region.base as usize) / GC_REGION_MARK_GRANULE;
                assert_eq!(
                    region.concurrent_marks.contains(granule),
                    std::ptr::eq(runtime, &first)
                );
            }
        }
        first.gc.mark_phase().store(0, Ordering::Release);
        for runtime in [&first, &second] {
            let mut state = runtime.gc.heap.lock().unwrap();
            old_lab::sync_records_in(&runtime.gc, &mut state);
            assert_eq!(state.total_allocs, 3);
            assert!(state.old_labs.is_empty());
            assert_eq!(state.old_lab_indices.len(), state.old_labs.len());
        }
    }
}

#[test]
fn private_stressed_refills_collect_and_allocate_in_the_selected_runtime() {
    let first = owner();
    let second = owner();
    for runtime in [&first, &second] {
        *runtime.diagnostics.stress_override.lock().unwrap() = Some(vec!["alloc".into()]);
        let mut tlab = GcTlabState::new();
        let registration = MutatorRegistration::new_in(runtime);
        let mut retained = std::ptr::null_mut();
        for size in [8, 24, 64] {
            let payload = tlab_alloc_slow_in(runtime, &raw mut tlab as usize, 0, 0, size, 0);
            assert!(!payload.is_null());
            root::add_task_frame_root_in(runtime, payload);
            root::remove_runtime_root_in(runtime, retained);
            retained = payload;
        }
        assert_eq!(runtime.gc.runtime_roots.len(), 1);
        assert_eq!(runtime.gc.heap.lock().unwrap().major_collections, 3);
        root::remove_runtime_root_in(runtime, retained);
        drop(registration);
        assert_eq!(
            try_collect_internal_in(runtime),
            CollectionAttempt::Completed
        );
        assert_eq!(runtime.gc.heap.lock().unwrap().allocated_bytes, 0);
    }
}

#[test]
fn private_runtime_drop_finalizes_indexed_buffered_and_young_native_owners() {
    unsafe fn release(payload: *mut u8) {
        let counter = unsafe { Box::from_raw(payload.cast::<*mut Arc<AtomicUsize>>().read()) };
        counter.fetch_add(1, Ordering::Relaxed);
    }
    let first_count = Arc::new(AtomicUsize::new(0));
    let second_count = Arc::new(AtomicUsize::new(0));
    for count in [&first_count, &second_count] {
        let runtime = owner();
        runtime.gc.drop_registry.lock().unwrap().insert(17, release);
        let mut tlab = GcTlabState::new();
        let young = tlab_alloc_slow_in(&runtime, &raw mut tlab as usize, 0, 17, 8, 0);
        runtime.gc.old_lab_policy.store(false, Ordering::Release);
        let indexed = allocate_old_in(&runtime, 0, 17, 8, 0);
        runtime.gc.old_lab_policy.store(true, Ordering::Release);
        let buffered = allocate_old_in(&runtime, 0, 17, 8, 0);
        for payload in [young, indexed, buffered] {
            assert!(!payload.is_null());
            unsafe {
                payload
                    .cast::<*mut Arc<AtomicUsize>>()
                    .write(Box::into_raw(Box::new(Arc::clone(count))))
            };
        }
        // No collection or registration retirement is needed to destroy the
        // runtime. In particular the buffered object's region index is absent.
        assert_eq!(count.load(Ordering::Relaxed), 0);
        drop(runtime);
        assert_eq!(count.load(Ordering::Relaxed), 3);
        assert_eq!(Arc::strong_count(count), 1);
    }
}

#[test]
fn private_old_buffer_registration_lookups_scale_with_refills_not_peer_count() {
    for peers in [1, 4, 16] {
        for allocations in [1usize, 65, 257] {
            let runtime = owner();
            runtime.gc.old_lab_policy.store(true, Ordering::Release);
            let ready = std::sync::Barrier::new(peers + 1);
            let release = std::sync::Barrier::new(peers + 1);
            std::thread::scope(|scope| {
                for _ in 0..peers {
                    let (runtime, ready, release) = (&runtime, &ready, &release);
                    scope.spawn(crate::runtime::serving(move || {
                        let _registration = MutatorRegistration::new_in(runtime);
                        assert!(!allocate_old_in(runtime, 0, 0, 8, 0).is_null());
                        ready.wait();
                        release.wait();
                    }));
                }
                ready.wait();
                let registration = MutatorRegistration::new_in(&runtime);
                let before = runtime.gc.path_observation.old_lab_lookups();
                for _ in 0..allocations {
                    assert!(
                        !allocate_old_in(
                            &runtime,
                            0,
                            0,
                            (GC_TLAB_MAX_OBJECT_SIZE - GC_HEADER_SIZE) as i64,
                            0,
                        )
                        .is_null()
                    );
                }
                let lookups = runtime.gc.path_observation.old_lab_lookups() - before;
                let expected =
                    allocations.div_ceil(old_lab::OLD_LAB_BYTES / GC_TLAB_MAX_OBJECT_SIZE);
                drop(registration);
                release.wait();
                assert_eq!(
                    lookups, expected,
                    "peers={peers}, allocations={allocations}"
                );
            });
            let state = runtime.gc.heap.lock().unwrap();
            assert!(state.old_labs.is_empty());
            assert!(state.old_lab_indices.is_empty());
        }
    }
}

#[test]
fn private_mutator_epochs_do_not_invalidate_peer_registrations() {
    let first = owner();
    let second = owner();
    let _registration = MutatorRegistration::new_in(&second);
    first.gc.mutator_epoch.fetch_add(1, Ordering::AcqRel);
    assert!(current_thread_is_registered_in(&second.gc));
    assert!(current_satb_buffer_in(&second.gc).is_some());
    second.gc.mutator_epoch.fetch_add(1, Ordering::AcqRel);
    assert!(!current_thread_is_registered_in(&second.gc));
    assert!(current_satb_buffer_in(&second.gc).is_none());
}

#[test]
fn private_mutators_park_and_retire_in_independent_stop_rounds() {
    let first = owner();
    let second = owner();
    let ready = std::sync::Barrier::new(3);
    let finish = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        for runtime in [&first, &second] {
            let ready = &ready;
            let finish = &finish;
            scope.spawn(crate::runtime::serving(move || {
                let _registration = MutatorRegistration::new_in(runtime);
                ready.wait();
                while !finish.load(Ordering::Acquire) {
                    relocatable_safepoint_in(runtime);
                    std::thread::yield_now();
                }
            }));
        }
        ready.wait();
        for _ in 0..8 {
            for runtime in [&first, &second] {
                with_stw_in(
                    runtime,
                    crate::gc_telemetry::stops::StopReason::InitialMark,
                    |coord, _| {
                        assert_eq!(coord.mutators.len(), 1);
                        assert_eq!(coord.parked.len(), 1);
                        assert!(coord.stw_pending.is_empty());
                        assert!(
                            coord
                                .mutators
                                .values()
                                .all(|roots| roots.satb_buffer.is_some())
                        );
                    },
                );
            }
        }
        finish.store(true, Ordering::Release);
    });
    for runtime in [&first, &second] {
        let coord = runtime.gc.coord.0.lock().unwrap();
        assert!(coord.mutators.is_empty());
        assert!(coord.parked.is_empty());
        assert!(!runtime.gc.poll_requested.load(Ordering::Acquire));
    }
}

#[test]
fn private_idle_mutator_publishes_to_selected_registry() {
    let runtime = owner();
    let _registration = MutatorRegistration::new_in(&runtime);
    let id = std::thread::current().id();
    {
        // No managed state is touched while the idle guard is held.
        let _idle = unsafe { IdleMutator::enter_in(&runtime) };
        let coord = runtime.gc.coord.0.lock().unwrap();
        assert!(coord.parked.contains(&id));
        assert!(coord.mutators[&id].deferred.is_some());
        assert!(coord.mutators[&id].satb_buffer.is_some());
    }
    let coord = runtime.gc.coord.0.lock().unwrap();
    assert!(!coord.parked.contains(&id));
    assert!(coord.mutators[&id].deferred.is_none());
}

#[test]
fn selecting_the_registered_runtime_makes_its_registration_served() {
    let owner: &'static crate::runtime::Runtime = Box::leak(Box::new(owner()));
    let previous = crate::runtime::current();
    assert!(!std::ptr::eq(previous, owner));
    let registration = MutatorRegistration::new_in(owner);
    // Registered with a runtime this thread does not serve yet.
    assert!(current_thread_is_registered_in(&owner.gc));
    assert!(!current_thread_is_registered());
    assert!(!registration_permits_selection(previous));
    owner.select_for_thread();
    assert!(current_thread_is_registered());
    drop(registration);
    assert!(!current_thread_is_registered());
    previous.select_for_thread();
    assert!(std::ptr::eq(crate::runtime::current(), previous));
    assert!(!current_thread_is_registered());
}
