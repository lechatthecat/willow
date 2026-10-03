use super::*;

#[test]
fn native_bytes_participate_in_reservation_and_trigger_inputs() {
    let _guard = runtime_test_guard();
    willow_gc_init();
    let mut map = crate::map::willow_map_new(0, 0, 0);
    willow_push_root(&mut map);
    for key in 0..1024 {
        crate::map::willow_map_insert(map, key, 0, key, 0);
    }
    let external = external_bytes();
    assert!(external > 0);
    {
        let mut state = runtime().heap.lock().unwrap();
        let managed = state.old_reserved_bytes + state.tlab_reserved_bytes;
        state.memory_limit_bytes = Some(managed + external);
        assert!(can_reserve(&state, 0));
        assert!(!can_reserve(&state, 1));
        let inputs = memory_inputs(&state);
        assert_eq!(inputs.committed, (managed + external) as u64);
        assert_eq!(inputs.occupied, (state.allocated_bytes + external) as u64);
        assert_eq!(inputs.live, state.last_major_live_bytes + external as u64);
    }
    assert!(allocation_should_collect());
    runtime().heap.lock().unwrap().memory_limit_bytes = None;
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(external_bytes(), 0);
}

#[test]
fn native_growth_requests_collection_without_a_managed_allocation() {
    let _guard = runtime_test_guard();
    willow_gc_init();
    let mut map = crate::map::willow_map_new(0, 0, 0);
    willow_push_root(&mut map);
    willow_gc_register_mutator();
    runtime().heap.lock().unwrap().threshold_bytes = 1;
    let allocations = runtime().heap.lock().unwrap().total_allocs;
    crate::map::willow_map_insert(map, 42, 0, 7, 0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while runtime().heap.lock().unwrap().major_collections == 0 {
        willow_gc_safepoint();
        assert!(
            std::time::Instant::now() < deadline,
            "native growth failed to schedule GC"
        );
        std::thread::yield_now();
    }
    coordinator::shutdown();
    assert_eq!(runtime().heap.lock().unwrap().total_allocs, allocations);
    assert_eq!(crate::map::willow_map_len(map), 1);
    assert!(external_bytes() > 0);
    willow_gc_unregister_mutator();
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(external_bytes(), 0);
}

#[test]
fn native_allocation_progress_reaches_soft_controller_without_managed_allocations() {
    let _guard = runtime_test_guard();
    willow_gc_init();
    let map = crate::map::willow_map_new(0, 0, 0);
    let before = {
        let state = runtime().heap.lock().unwrap();
        memory_inputs(&state).allocated_total
    };
    let managed = runtime().heap.lock().unwrap().total_allocated_bytes;
    for key in 0..128 {
        crate::map::willow_map_insert(map, key, 0, key, 0);
    }
    let state = runtime().heap.lock().unwrap();
    assert_eq!(state.total_allocated_bytes, managed);
    assert!(memory_inputs(&state).allocated_total > before);
    assert_eq!(
        memory_inputs(&state).allocated_total - before,
        external_bytes() as u64
    );
    drop(state);
    willow_gc_collect();
    assert_eq!(external_bytes(), 0);
}

#[test]
fn gc_owned_async_lock_finalizers_release_the_boxed_states() {
    let _guard = runtime_test_guard();
    willow_gc_init();
    for reset in [false, true] {
        crate::async_mutex::willow_async_mutex_new(42, 0);
        crate::async_rwlock::willow_async_rwlock_new(42, 0);
        assert!(external_bytes() > 0);
        if reset {
            willow_gc_init();
        } else {
            willow_gc_collect();
        }
        assert_eq!(external_bytes(), 0);
    }
}

#[test]
fn managed_growth_crosses_pacer_trigger_with_constant_native_storage() {
    let _guard = runtime_test_guard();
    willow_gc_init();
    let mut map = crate::map::willow_map_new(0, 0, 0);
    willow_push_root(&mut map);
    for key in 0..128 {
        crate::map::willow_map_insert(map, key, 0, key, 0);
    }
    let native = external_bytes();
    assert!(native > 256);
    {
        let mut state = runtime().heap.lock().unwrap();
        assert!(state.pacer.enabled());
        let total = memory_inputs(&state).allocated_total;
        state.pacer.allocation(0, total);
        state.pacer_trigger = (state.allocated_bytes + native + 128) as u64;
    }
    assert!(!allocation_should_collect());
    assert!(!willow_alloc(128).is_null());
    assert_eq!(external_bytes(), native, "native growth has stopped");
    {
        let state = runtime().heap.lock().unwrap();
        assert!((state.allocated_bytes as u64) < state.pacer_trigger);
        assert!((state.allocated_bytes + native) as u64 >= state.pacer_trigger);
        assert!(!state.soft_memory.decide(memory_inputs(&state)).collect);
    }
    assert!(
        allocation_should_collect(),
        "combined occupancy crossed the pacer trigger"
    );
    willow_pop_root();
    willow_gc_collect();
}

#[test]
fn native_growth_is_included_in_allocation_path_pacer_sample() {
    let _guard = runtime_test_guard();
    willow_gc_init();
    let mut map = crate::map::willow_map_new(0, 0, 0);
    willow_push_root(&mut map);
    {
        let mut state = runtime().heap.lock().unwrap();
        let total = memory_inputs(&state).allocated_total;
        state.pacer.allocation(0, total);
    }
    for key in 0..1024 {
        crate::map::willow_map_insert(map, key, 0, key, 0);
    }
    let total = {
        let state = runtime().heap.lock().unwrap();
        let total = memory_inputs(&state).allocated_total;
        assert!(state.pacer.sample_due(total));
        assert!(!state.pacer.sample_due(state.total_allocated_bytes));
        total
    };
    allocation_should_collect();
    {
        let state = runtime().heap.lock().unwrap();
        assert!(
            !state.pacer.sample_due(total + 65535),
            "sample must include native growth"
        );
        assert!(state.pacer.sample_due(total + 65536));
    }
    willow_pop_root();
    willow_gc_collect();
}

#[test]
fn cycle_completion_pacer_sample_keeps_cumulative_native_growth_after_drop() {
    let _guard = runtime_test_guard();
    willow_gc_init();
    let map = crate::map::willow_map_new(0, 0, 0);
    for key in 0..128 {
        crate::map::willow_map_insert(map, key, 0, key, 0);
    }
    assert!(external_bytes() > 0);
    let total = {
        let state = runtime().heap.lock().unwrap();
        memory_inputs(&state).allocated_total
    };
    willow_gc_collect();
    assert_eq!(external_bytes(), 0);
    let state = runtime().heap.lock().unwrap();
    assert_eq!(memory_inputs(&state).allocated_total, total);
    assert!(
        !state.pacer.sample_due(total + 65535),
        "cycle sample must retain native allocation history"
    );
    assert!(state.pacer.sample_due(total + 65536));
}

#[test]
fn unchanged_high_reservations_do_not_collect_per_managed_allocation() {
    let _guard = runtime_test_guard();
    for count in [16, 128, 1024] {
        willow_gc_init();
        runtime().heap.lock().unwrap().memory_limit_bytes = Some(300 * 1024);
        let mut roots = vec![std::ptr::null_mut(); count];
        for root in &mut roots {
            *root = willow_alloc(8);
            willow_push_root(root);
        }
        let state = runtime().heap.lock().unwrap();
        assert_eq!(state.old_reserved_bytes, 256 * 1024);
        assert_eq!(state.tlab_reserved_bytes, 0);
        assert!(state.allocated_bytes < 225 * 1024);
        assert_eq!(
            state.major_collections, 1,
            "unchanged reservations caused repeated full collections"
        );
        assert!(can_reserve(&state, 44 * 1024));
        assert!(!can_reserve(&state, 44 * 1024 + 1));
        println!(
            "reservation pressure objects={count} reserved={} occupied={} major_collections={}",
            state.old_reserved_bytes, state.allocated_bytes, state.major_collections
        );
        drop(state);
        willow_pop_roots(count as i32);
        willow_gc_collect();
    }
}

#[test]
fn reservation_pressure_hysteresis_rearms_after_reclamation_and_reset() {
    let _guard = runtime_test_guard();
    willow_gc_init();
    let mut state = GcState {
        memory_limit_bytes: Some(400),
        ..GcState::default()
    };
    for reserved in [0, 299] {
        state.old_reserved_bytes = reserved;
        assert!(!hard_reservation_pressure(&mut state));
    }
    state.old_reserved_bytes = 300;
    assert!(hard_reservation_pressure(&mut state));
    for reserved in [300, 301, 399, 299, 201, 300] {
        state.old_reserved_bytes = reserved;
        assert!(
            !hard_reservation_pressure(&mut state),
            "must remain latched at {reserved}"
        );
    }
    state.old_reserved_bytes = 200;
    assert!(!hard_reservation_pressure(&mut state));
    state.old_reserved_bytes = 300;
    assert!(hard_reservation_pressure(&mut state));
    state.memory_limit_bytes = None;
    assert!(!hard_reservation_pressure(&mut state));
    assert!(!state.hard_pressure_triggered);

    for _ in 0..2 {
        runtime().heap.lock().unwrap().memory_limit_bytes = Some(300 * 1024);
        let mut root = willow_alloc(8);
        willow_push_root(&mut root);
        assert!(allocation_should_collect());
        assert!(!allocation_should_collect());
        willow_pop_root();
        willow_gc_collect();
        let state = runtime().heap.lock().unwrap();
        assert_eq!(state.old_reserved_bytes, 0);
        assert!(
            !state.hard_pressure_triggered,
            "full reclamation must rearm pressure"
        );
    }
    runtime().heap.lock().unwrap().hard_pressure_triggered = true;
    willow_gc_init();
    assert!(!runtime().heap.lock().unwrap().hard_pressure_triggered);
}

#[test]
fn reservation_latch_does_not_suppress_occupied_pacer_pressure() {
    let _guard = runtime_test_guard();
    willow_gc_init();
    let mut root = willow_alloc(8);
    willow_push_root(&mut root);
    let mut state = runtime().heap.lock().unwrap();
    state.memory_limit_bytes = Some(300 * 1024);
    assert!(hard_reservation_pressure(&mut state));
    assert!(!hard_reservation_pressure(&mut state));
    let total = policy_allocated_total(&state);
    state.pacer.allocation(0, total);
    state.pacer_trigger = state.allocated_bytes as u64;
    drop(state);
    assert!(
        allocation_should_collect(),
        "occupancy must still trigger while reservation pressure is latched"
    );
    willow_pop_root();
    willow_gc_collect();
}
