//! Fixed byte-debt regressions, with explicit synchronization and no sleeps.
use super::*;
use crate::gc::*;

fn roots(value: *mut u8) -> MinorRoots {
    MinorRoots {
        slots: vec![],
        values: vec![(value, PinSource::RuntimeCodeStack)],
    }
}

#[test]
fn minor_deferral_clock_boundaries_and_fixed_budget() {
    let _guard = runtime_test_guard();
    for start in [0, u32::MAX as u64 - 8, u32::MAX as u64 + 8, u64::MAX - 64] {
        reset_internal();
        let mut tls = tlab_state_for_test();
        let root = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
        let mut state = runtime().heap.lock().unwrap();
        retire_all_tlabs_unindexed(&mut state);
        state.young_allocation_clock = start;
        state.nursery_threshold_bytes = 32;
        assert!(should_defer_minor(&mut state, &roots(root)));
        state.nursery_threshold_bytes = usize::MAX;
        state.young_allocated_bytes = 0; // independent occupancy correction
        state.total_allocated_bytes = u64::MAX; // Old allocation is irrelevant
        assert!(should_defer_minor(&mut state, &roots(root)));
        state.young_allocation_clock = start + 31;
        assert!(should_defer_minor(&mut state, &roots(root)));
        state.young_allocation_clock += 1;
        assert!(!should_defer_minor(&mut state, &roots(root)));
        assert_eq!(minor_deferral_age(&state), 32);
        drop(state);
    }
    reset_internal();
}

#[test]
fn minor_deferral_saturation_and_monotonicity() {
    let _guard = runtime_test_guard();
    reset_internal();
    let mut tls = tlab_state_for_test();
    let root = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    let mut state = runtime().heap.lock().unwrap();
    retire_all_tlabs_unindexed(&mut state);
    state.young_allocation_clock = u64::MAX - 1;
    assert!(should_defer_minor(&mut state, &roots(root)));
    tlab::add_tlab_accounting(&mut state, 0, 10);
    assert_eq!(state.young_allocation_clock, u64::MAX);
    assert!(!should_defer_minor(&mut state, &roots(root)));
    state.young_allocation_clock = 1;
    state.minor_deferral_start_bytes = Some(2);
    #[cfg(debug_assertions)]
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| minor_deferral_age(&state)))
            .is_err()
    );
    drop(state);
    reset_internal();
}

#[test]
fn minor_deferral_concurrent_reclamation_and_pin_release() {
    let _guard = runtime_test_guard();
    reset_internal();
    let mut tls = tlab_state_for_test();
    let root = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    unsafe {
        *root.cast::<u64>() = 123;
    }
    let dead = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    assert!(!dead.is_null());
    assert!(!willow_alloc_typed(64, 0).is_null());
    {
        let mut state = runtime().heap.lock().unwrap();
        retire_all_tlabs_unindexed(&mut state);
        state.nursery_threshold_bytes = 256;
    }
    assert!(minor_collect_with_roots(roots(root), &mut Default::default()).is_none());
    let (clock, occupied, plan) = {
        let mut state = runtime().heap.lock().unwrap();
        // Only the permanently parked root survives the production major sweep.
        unsafe { (*payload_to_header(root)).marked = true };
        (
            state.young_allocation_clock,
            state.young_allocated_bytes,
            sweep::prepare(&mut state, None),
        )
    };
    let freed = std::thread::spawn(move || sweep::concurrent(plan))
        .join()
        .unwrap();
    assert!(freed > 0);
    {
        let mut state = runtime().heap.lock().unwrap();
        assert!(state.young_allocated_bytes < occupied);
        assert_eq!(state.young_allocation_clock, clock);
        assert_eq!(state.minor_retry_bytes, clock + 256); // budget smaller than chunk
        assert!(!minor_allocation_due(&state));
        state.nursery_threshold_bytes = usize::MAX;
    }
    // Production Old allocation must not advance young debt.
    for _ in 0..16 {
        assert!(!willow_alloc_typed(64, 0).is_null());
    }
    assert_eq!(runtime().heap.lock().unwrap().young_allocation_clock, clock);
    let attempts = 256usize.div_ceil(GC_HEADER_SIZE + 8);
    for step in 1..=attempts {
        assert!(!willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0).is_null());
        let plan = {
            let mut state = runtime().heap.lock().unwrap();
            retire_all_tlabs_unindexed(&mut state);
            unsafe { (*payload_to_header(root)).marked = true };
            sweep::prepare(&mut state, None)
        };
        std::thread::spawn(move || sweep::concurrent(plan))
            .join()
            .unwrap();
        let state = runtime().heap.lock().unwrap();
        assert_eq!(state.young_allocated_bytes, GC_HEADER_SIZE + 8);
        assert_eq!(
            minor_deferral_age(&state),
            (step * (GC_HEADER_SIZE + 8)) as u64
        );
        assert_eq!(minor_allocation_due(&state), step == attempts);
    }
    eprintln!(
        "reclaim_rounds={attempts} fixed_budget=256 young_debt={}",
        minor_deferral_age(&runtime().heap.lock().unwrap())
    );
    assert!(minor_collect_with_roots(roots(root), &mut Default::default()).is_some());
    let stats = crate::gc_telemetry::survivor_snapshot();
    assert_eq!(stats.pin_fallback_count, 1);
    assert_eq!(stats.active_pins, 1);
    assert_eq!(stats.pin_reasons[PinSource::RuntimeCodeStack as usize], 1);
    assert_eq!(stats.minor_deferral_age_bytes, 0);
    assert_eq!(stats.minor_retry_target_bytes, 0);
    assert_eq!(stats.minor_blockers, 0);
    assert_eq!(unsafe { *root.cast::<u64>() }, 123);
    let records = crate::gc_telemetry::pin_snapshot();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].owner_allocation, root as usize);
    assert_eq!(records[0].bytes, stats.active_pin_bytes);
    assert_eq!(records[0].minor_epoch, 1);
    assert!(records[0].major_epoch > 0);
    // Root is now released: the next major reclaims its promotion and record.
    let plan = sweep::prepare(&mut runtime().heap.lock().unwrap(), None);
    std::thread::spawn(move || sweep::concurrent(plan))
        .join()
        .unwrap();
    let stats = crate::gc_telemetry::survivor_snapshot();
    assert_eq!(stats.active_pins, 0);
    assert_eq!(stats.active_pin_bytes, 0);
    assert_eq!(stats.released_pins, 1);
    assert!(crate::gc_telemetry::pin_snapshot().is_empty());
    reset_internal();
}

#[test]
fn minor_deferral_parallel_tlab_refills_count_once() {
    let _guard = runtime_test_guard();
    for workers in [1, 4, 8] {
        reset_internal();
        runtime().heap.lock().unwrap().nursery_threshold_bytes = usize::MAX;
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    let mut tls = tlab_state_for_test();
                    for _ in 0..16 {
                        assert!(!willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0).is_null());
                        let cursor = tls.cursor.load(Ordering::Acquire);
                        initialize_object_at(cursor as *mut u8, GC_HEADER_SIZE + 8, 0, 1, 0)
                            .unwrap();
                        publish_tlab_start_for_test(&tls, cursor as *mut u8);
                        tls.cursor
                            .store(cursor + GC_HEADER_SIZE + 8, Ordering::Release);
                    }
                    tlab::unregister_tlab_state(&raw mut tls as usize);
                });
            }
        });
        let mut state = runtime().heap.lock().unwrap();
        sync_tlab_bytes(&mut state);
        assert_eq!(
            state.young_allocation_clock,
            (workers * 32 * (GC_HEADER_SIZE + 8)) as u64
        );
        let clock = state.young_allocation_clock;
        sync_tlab_bytes(&mut state);
        assert_eq!(state.young_allocation_clock, clock);
        eprintln!(
            "refill_workers={workers} objects={} bytes={clock}",
            workers * 32
        );
        drop(state);
    }
    reset_internal();
}

#[test]
fn minor_deferral_hard_cap_and_saturation_clear_debt() {
    let _guard = runtime_test_guard();
    for saturated in [false, true] {
        reset_internal();
        let mut tls = tlab_state_for_test();
        let root = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
        {
            let mut state = runtime().heap.lock().unwrap();
            retire_all_tlabs_unindexed(&mut state);
            if saturated {
                state.young_allocation_clock = u64::MAX - 1;
            }
        }
        assert!(minor_collect_with_roots(roots(root), &mut Default::default()).is_none());
        let reserved = {
            let mut state = runtime().heap.lock().unwrap();
            let reserved = state.tlab_reserved_bytes + state.old_reserved_bytes;
            if saturated {
                assert_eq!(state.minor_retry_bytes, u64::MAX);
                state.young_allocation_clock = u64::MAX;
            } else {
                state.memory_limit_bytes = Some(reserved);
            }
            assert!(minor_allocation_due(&state));
            reserved
        };
        assert!(minor_collect_with_roots(roots(root), &mut Default::default()).is_some());
        let state = runtime().heap.lock().unwrap();
        assert!(state.tlab_reserved_bytes + state.old_reserved_bytes <= reserved);
        assert_eq!(state.minor_deferral_start_bytes, None);
        assert_eq!(state.minor_deferral_budget_bytes, 0);
        assert_eq!(state.minor_retry_bytes, 0);
        assert_eq!(state.survivor_stats.active_pins, 1);
        drop(state);
    }
    reset_internal();
}

#[test]
fn minor_deferral_exhausted_refill_waits_for_collector() {
    let _guard = runtime_test_guard();
    reset_internal();
    {
        let mut state = runtime().heap.lock().unwrap();
        state.young_allocation_clock = 32;
        state.minor_deferral_start_bytes = Some(0);
        state.minor_deferral_budget_bytes = 32;
        state.minor_retry_bytes = 32;
    }
    cycle_wait::SLEEPS.store(0, Ordering::Release);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let owner = scope.spawn(move || {
            let _owner = cycle_wait::CollectorGuard::try_acquire().unwrap();
            // Same owner/wait protocol used while a concurrent sweep runs.
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        ready_rx.recv().unwrap();
        let allocator = scope.spawn(|| {
            let mut tls = tlab_state_for_test();
            assert!(!willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0).is_null());
            tlab::unregister_tlab_state(&raw mut tls as usize);
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while cycle_wait::SLEEPS.load(Ordering::Acquire) == 0 {
            if std::time::Instant::now() >= deadline {
                release_tx.send(()).unwrap();
                panic!("expired refill did not join collector");
            }
            std::thread::yield_now();
        }
        assert_eq!(runtime().heap.lock().unwrap().young_allocation_clock, 32);
        release_tx.send(()).unwrap();
        owner.join().unwrap();
        allocator.join().unwrap();
    });
    let state = runtime().heap.lock().unwrap();
    assert_eq!(state.minor_deferral_start_bytes, None);
    assert_eq!(
        state.young_allocation_clock,
        32 + (GC_HEADER_SIZE + 8) as u64
    );
    drop(state);
    reset_internal();
}

#[test]
fn minor_deferral_first_request_during_sweep_waits_before_refill() {
    let _guard = runtime_test_guard();
    reset_internal();
    let mut tls = tlab_state_for_test();
    let root = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    unsafe {
        *root.cast::<u64>() = 91;
    }
    let mut slot = root;
    let depth = gc_thread_root_depth();
    willow_push_root(&mut slot);
    let token = unsafe { park_current_roots(depth) };
    let clock = {
        let mut state = runtime().heap.lock().unwrap();
        retire_all_tlabs_unindexed(&mut state);
        state.nursery_threshold_bytes = GC_HEADER_SIZE + 8;
        assert_eq!(state.minor_deferral_start_bytes, None);
        state.young_allocation_clock
    };
    cycle_wait::SLEEPS.store(0, Ordering::Release);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let root_address = root as usize;
        let owner = scope.spawn(move || {
            let root = root_address as *mut u8;
            let _owner = cycle_wait::CollectorGuard::try_acquire().unwrap();
            let plan = {
                let mut state = runtime().heap.lock().unwrap();
                // Model the major mark result for the permanently parked root.
                unsafe {
                    (*payload_to_header(root)).marked = true;
                }
                sweep::prepare(&mut state, None)
            };
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            sweep::concurrent(plan);
            service_pending();
        });
        ready_rx.recv().unwrap();
        let allocator = scope.spawn(|| {
            let mut tls = tlab_state_for_test();
            assert!(!willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0).is_null());
            tlab::unregister_tlab_state(&raw mut tls as usize);
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while cycle_wait::SLEEPS.load(Ordering::Acquire) == 0 {
            if allocator.is_finished() || std::time::Instant::now() >= deadline {
                release_tx.send(()).unwrap();
                owner.join().unwrap();
                allocator.join().unwrap();
                panic!("first pending minor granted a refill before sweep completed");
            }
            std::thread::yield_now();
        }
        {
            let state = runtime().heap.lock().unwrap();
            assert!(state.sweeping.is_some());
            assert_eq!(state.minor_deferral_start_bytes, None);
            assert_eq!(state.young_allocation_clock, clock);
            assert_eq!(state.tlab_reserved_bytes, GC_TLAB_CHUNK_SIZE);
        }
        release_tx.send(()).unwrap();
        owner.join().unwrap();
        allocator.join().unwrap();
    });
    // The same parked root stays in place through the bounded fallback.
    minor_collect_internal();
    assert_eq!(slot, root);
    assert_eq!(unsafe { *root.cast::<u64>() }, 91);
    assert_eq!(
        unsafe { (*payload_to_header(root)).generation },
        GC_GENERATION_OLD
    );
    assert_eq!(
        runtime().heap.lock().unwrap().young_allocation_clock,
        clock + (GC_HEADER_SIZE + 8) as u64
    );
    unsafe { resume_parked_roots(token) };
    willow_pop_root();
    willow_gc_collect();
    reset_internal();
}
