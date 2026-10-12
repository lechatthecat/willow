//! Focused old-LAB invariants (willow-jz15.53): unindexed lookups, flushing,
//! allocation across mark closure and a pending sweep, and pruning of buffers
//! owned by exited threads.
use super::*;
use std::time::Instant;

/// Enables buffers for one test and restores the default even on panic.
struct LabsOn;

impl LabsOn {
    fn new() -> Self {
        OLD_LABS_FOR_TEST.store(true, Ordering::Release);
        LabsOn
    }
}

impl Drop for LabsOn {
    fn drop(&mut self) {
        OLD_LABS_FOR_TEST.store(false, Ordering::Release);
    }
}

fn indexed(state: &GcState, payload: *mut u8) -> bool {
    let object = find_old_region_object(state, payload as usize, false).expect("live object");
    let address = object.as_ptr() as usize;
    state.old_regions.iter().any(|region| {
        region.contains(address) && region.allocations.contains_key(&(address - region.start()))
    })
}

#[test]
fn pending_objects_are_found_by_exact_and_interior_lookup_until_flushed() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let _labs = LabsOn::new();
    let objects: Vec<_> = [8i64, 40, 200]
        .iter()
        .map(|&size| willow_alloc(size))
        .collect();
    assert_eq!(local_pending_for_test(), objects.len());
    {
        let state = runtime().heap.lock().unwrap();
        for &payload in &objects {
            assert!(!indexed(&state, payload), "still buffered");
            let exact = find_old_region_object(&state, payload as usize, false).unwrap();
            assert_eq!(exact.payload().as_ptr(), payload);
        }
        // The last object lies past every indexed one: the interior lookup
        // must fall back to the start bits.
        let last = *objects.last().unwrap();
        let interior = find_old_region_object(&state, last as usize + 100, true).unwrap();
        assert_eq!(interior.payload().as_ptr(), last);
        let first = find_old_region_object(&state, objects[0] as usize + 4, true).unwrap();
        assert_eq!(first.payload().as_ptr(), objects[0]);
    }
    {
        let mut state = runtime().heap.lock().unwrap();
        old_lab::flush_all(&mut state);
        for &payload in &objects {
            assert!(indexed(&state, payload));
        }
        let region = state
            .old_regions
            .iter()
            .find(|region| region.contains(objects[0] as usize))
            .unwrap();
        assert_eq!(region.active_labs, 1, "flush keeps the buffer active");
    }
    assert_eq!(local_pending_for_test(), 0);
    let expected: usize = [8usize, 40, 200]
        .iter()
        .map(|size| size + GC_HEADER_SIZE)
        .sum();
    assert_eq!(willow_gc_allocated_bytes() as usize, expected);
    // A following allocation still bumps in the same buffer.
    let next = willow_alloc(8);
    assert_eq!(local_pending_for_test(), 1);
    let (_, last_span) = span_for(200).unwrap();
    assert_eq!(next as usize, objects[2] as usize + last_span);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn buffer_allocations_survive_mark_closure_and_a_pending_sweep() {
    let _guard = runtime_test_guard();
    for paced in [false, true] {
        for limit_kind in [0, 1, 2] {
            reset_internal_for_test();
            let _labs = LabsOn::new();
            {
                let mut state = runtime().heap.lock().unwrap();
                state.pacer.set_enabled_for_test(paced);
                state.soft_memory = memory_control::Controller::default();
                state.memory_limit_bytes = None;
                if limit_kind == 1 {
                    state.soft_memory.set_limit(64 * 1024 * 1024);
                }
                if limit_kind == 2 {
                    state.memory_limit_bytes = Some(64 * 1024 * 1024);
                }
            }
            let mut rooted = willow_alloc(16);
            willow_push_root(&mut rooted);
            let (_, cycle) = root_handshake::begin().expect("no foreign root owner");
            // Allocated black while marking, from the buffer carved before the cycle.
            let during_mark = willow_alloc(24);
            assert!(local_pending_for_test() > 0);
            let mut consumer = cycle.queue.register_assist();
            while cycle.drain_worker(64, &mut Vec::new(), &mut consumer).0 > 0 {}
            drop(consumer);
            let (_, plan) =
                mark_closure::finish(&cycle, Instant::now()).expect("closes concurrently");
            assert_eq!(local_pending_for_test(), 0, "closure retires every buffer");
            assert!(cycle.is_marked(during_mark as usize));
            // The sweep is pending: a refilled buffer copies `sweep_pending`, and the
            // region visit must index this unmarked, unrooted object before freeing.
            // Cross the current goal while collection is already active: the pending
            // sweep, rather than allocation accounting, must determine policy timing.
            {
                let mut state = runtime().heap.lock().unwrap();
                state.threshold_bytes = 1;
            }
            let sweep_goal = 1;
            let during_sweep = willow_alloc(32);
            assert_eq!(local_pending_for_test(), 1);
            {
                let state = runtime().heap.lock().unwrap();
                assert!(state.sweeping.is_some());
                assert!(!indexed(&state, during_sweep));
            }
            assert_eq!(runtime().heap.lock().unwrap().threshold_bytes, sweep_goal);
            sweep::concurrent(plan);
            assert_eq!(runtime().heap.lock().unwrap().threshold_bytes, sweep_goal);
            {
                let state = runtime().heap.lock().unwrap();
                for payload in [rooted, during_mark] {
                    assert!(indexed(&state, payload), "captured objects remain indexed");
                }
                assert!(
                    find_old_region_object(&state, during_sweep as usize, false).is_some(),
                    "new extent remains visible through its published start bit"
                );
                assert!(
                    state
                        .old_regions
                        .iter()
                        .filter(|region| region.sweep_state.epoch != 0)
                        .all(|region| region.active_labs == 0)
                );
            }
            let expected = [16usize, 24, 32]
                .iter()
                .map(|size| size + GC_HEADER_SIZE)
                .sum::<usize>();
            assert_eq!(willow_gc_allocated_bytes() as usize, expected);
            // The next cycle frees the two unrooted objects without accounting drift.
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes() as usize, 16 + GC_HEADER_SIZE);
            willow_pop_root();
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), 0);
            reset_internal_for_test();
        }
    }
}

#[test]
fn exited_thread_buffers_are_indexed_and_pruned() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let _labs = LabsOn::new();
    let mine = willow_alloc(8);
    let theirs = std::thread::spawn(|| willow_alloc(48) as usize)
        .join()
        .unwrap() as *mut u8;
    {
        let mut state = runtime().heap.lock().unwrap();
        assert_eq!(state.old_labs.len(), 2);
        assert!(!indexed(&state, theirs));
        let active: usize = state.old_regions.iter().map(|r| r.active_labs).sum();
        assert_eq!(active, 2);
        old_lab::flush_all(&mut state);
        assert_eq!(state.old_labs.len(), 1, "exited owner's record is dropped");
        assert!(indexed(&state, theirs));
        assert!(indexed(&state, mine));
        let active: usize = state.old_regions.iter().map(|r| r.active_labs).sum();
        assert_eq!(active, 1, "only the live owner's buffer stays active");
    }
    assert_eq!(
        willow_gc_allocated_bytes() as usize,
        8 + 48 + 2 * GC_HEADER_SIZE
    );
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

/// Clear environment overrides in a child without mutating process-wide env
/// while Rust tests may be running concurrently.
fn default_budgets_child() -> bool {
    const NAME: &str = "gc::old_lab::tests::default_budgets_complete_small_allocations_and_cap_full_buffer_recovery";
    if std::env::var("WILLOW_LAB_DEFAULT_CHILD").as_deref() == Ok("1") {
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture"])
        .env("WILLOW_LAB_DEFAULT_CHILD", "1")
        .env_remove("WILLOW_GC_REMARK_MAX_SLOTS")
        .env_remove("WILLOW_GC_REMARK_MAX_BYTES")
        .env_remove("WILLOW_GC_REMARK_MAX_WORK_NS")
        .output()
        .unwrap();
    print!("{}", String::from_utf8_lossy(&output.stdout));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

#[test]
fn default_budgets_complete_small_allocations_and_cap_full_buffer_recovery() {
    if !default_budgets_child() {
        return;
    }
    let _guard = runtime_test_guard();
    for count in [1, 128, 4096, OLD_LAB_BYTES / GC_HEADER_SIZE] {
        reset_internal_for_test();
        let _labs = LabsOn::new();
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        let payload = if count == OLD_LAB_BYTES / GC_HEADER_SIZE {
            0
        } else {
            8
        };
        let mut root = std::ptr::null_mut();
        for index in 0..count {
            if payload == 0 {
                let object = willow_alloc(0);
                if index == 0 {
                    root = object;
                }
            } else {
                let object = willow_alloc_typed(payload, 1);
                unsafe {
                    store_gc_reference(object.cast(), root);
                }
                root = object;
            }
        }
        willow_push_root(&mut root);
        assert_eq!(local_pending_for_test(), count);
        willow_gc_collect();
        let stops = crate::gc_telemetry::stops::snapshot_stops();
        assert_eq!(stops.by_reason[0].work.metadata_objects, 0);
        if count <= 128 {
            assert_eq!(stops.by_reason[1].work.metadata_objects, count as u64);
        } else {
            assert!(stops.by_reason[1].work.metadata_objects < count as u64);
        }
        assert_eq!(stops.by_reason[1].work.swept_objects, 0);
        assert_eq!(
            stops.by_reason[3].requests,
            u64::from(count > 128),
            "small buffers complete normally; overflowing buffers use capped Recovery"
        );
        assert_eq!(local_pending_for_test(), 0);
        let expected_live = if payload == 0 {
            GC_HEADER_SIZE as i64
        } else {
            count as i64 * (GC_HEADER_SIZE as i64 + payload)
        };
        assert_eq!(willow_gc_allocated_bytes(), expected_live);
        assert!(stops.by_reason[1].requests <= (mark_closure::MAX_REMARK_ROUNDS - 1) as u64);
        assert!(stops.by_reason[1].requests + stops.by_reason[3].requests <= 3);
        println!(
            "default_lab pending_entries={count} root_count=1 initial_metadata_objects=0 remark_metadata_objects={} remark_metadata_bytes={} remark_stops={} recovery_stops={} normal_stopped_sweep=0 max_slots=4096 max_bytes=1048576 max_work_ns=1000000",
            stops.by_reason[1].work.metadata_objects,
            stops.by_reason[1].work.metadata_bytes,
            stops.by_reason[1].requests,
            stops.by_reason[3].requests
        );
        willow_pop_roots(1);
        willow_gc_collect(); // Includes empty retired owner records.
        assert_eq!(willow_gc_allocated_bytes(), 0);
    }
    reset_internal_for_test();
}

#[test]
fn bounded_retirement_fake_clock_keeps_prefix_and_indexes_each_entry_once() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let _labs = LabsOn::new();
    runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
    let objects: Vec<_> = [1, 8, 17, 40, 200]
        .into_iter()
        .map(|size| willow_alloc(size))
        .collect();
    let total: usize = objects
        .iter()
        .map(|&value| {
            HeapObject::from_raw(unsafe { value.sub(GC_HEADER_SIZE) }.cast())
                .unwrap()
                .size()
        })
        .sum();
    let mut state = runtime().heap.lock().unwrap();
    let mut work = crate::gc_telemetry::stops::StopWorkV2::default();
    let now = std::cell::Cell::new(0);
    let mut budget = old_cycle::RemarkBudget::new(100, 10000, 4, 0);
    assert!(!retire_bounded_with_clock(
        &mut state,
        &mut budget,
        &mut work,
        || {
            let time = now.get();
            now.set(time + 1);
            time
        }
    ));
    assert_eq!(work.metadata_objects, 2);
    assert!(indexed(&state, objects[0]) && indexed(&state, objects[1]));
    assert!(!indexed(&state, objects[2]));
    assert_eq!(
        state
            .old_regions
            .iter()
            .map(|r| r.active_labs)
            .sum::<usize>(),
        0
    );
    LOCAL.with(|lab| assert_eq!(lab.inner.lock().unwrap().pending_index, 2));
    for completed in 3..=5 {
        let mut budget = old_cycle::RemarkBudget::new(3, 1000, 100, 0);
        let finished = retire_bounded_with_clock(&mut state, &mut budget, &mut work, || 0);
        assert_eq!(finished, completed == 5);
        assert_eq!(work.metadata_objects, completed);
        assert_eq!(
            state
                .old_regions
                .iter()
                .map(|r| r.allocations.len())
                .sum::<usize>(),
            completed as usize
        );
    }
    assert_eq!(
        state
            .old_regions
            .iter()
            .map(|r| r.live_bytes)
            .sum::<usize>(),
        total
    );
    assert_eq!(state.allocated_bytes, total);
    assert_eq!(state.total_allocs, 5);
    let mut budget = old_cycle::RemarkBudget::new(1, 1, 1, 0);
    assert!(
        retire_bounded_with_clock(&mut state, &mut budget, &mut work, || 0),
        "empty retired record has no carve charge"
    );
    assert_eq!(work.metadata_objects, 5);
    println!(
        "partial_retirement entries=5 fake_time_yield_after=2 slot_chunk_entries=1 indexed_visits=5 prefix_moves=0 duplicates=0"
    );
    drop(state);
    reset_internal_for_test();
}

#[test]
fn partial_retirement_refill_flushes_only_remaining_suffix_without_double_accounting() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let _labs = LabsOn::new();
    runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
    let objects: Vec<_> = [1, 8, 17, 40, 200]
        .into_iter()
        .map(|size| willow_alloc(size))
        .collect();
    let mut state = runtime().heap.lock().unwrap();
    let mut work = crate::gc_telemetry::stops::StopWorkV2::default();
    let mut budget = old_cycle::RemarkBudget::new(3, 1000, 100, 0);
    assert!(!retire_bounded_with_clock(
        &mut state,
        &mut budget,
        &mut work,
        || 0
    ));
    assert_eq!(work.metadata_objects, 1);
    let first_live = state
        .old_regions
        .iter()
        .map(|r| r.live_bytes)
        .sum::<usize>();
    assert_eq!(first_live, GC_HEADER_SIZE + 1);
    drop(state);
    let next = willow_alloc(8); // Forces suffix flush and a fresh carve.
    let mut state = runtime().heap.lock().unwrap();
    flush_all(&mut state);
    for object in objects.iter().copied().chain([next]) {
        assert!(indexed(&state, object));
    }
    assert_eq!(state.total_allocs, 6);
    let expected = GC_HEADER_SIZE
        + 8
        + [1usize, 8, 17, 40, 200]
            .iter()
            .map(|n| GC_HEADER_SIZE + n)
            .sum::<usize>();
    assert_eq!(state.allocated_bytes, expected);
    assert_eq!(
        state
            .old_regions
            .iter()
            .map(|r| r.live_bytes)
            .sum::<usize>(),
        expected
    );
    assert_eq!(
        state
            .old_regions
            .iter()
            .map(|r| r.allocations.len())
            .sum::<usize>(),
        6
    );
    drop(state);
    reset_internal_for_test();
}

#[test]
fn threshold_policy_old_lab_pressure_matrix() {
    let _guard = runtime_test_guard();
    for paced in [false, true] {
        for limit_kind in [0, 1, 2] {
            for count in [1usize, 128, 4096] {
                reset_internal_for_test();
                let _labs = LabsOn::new();
                {
                    let mut state = runtime().heap.lock().unwrap();
                    state.pacer.set_enabled_for_test(paced);
                    state.threshold_bytes = usize::MAX;
                    state.pacer_trigger = usize::MAX as u64;
                    state.soft_memory = memory_control::Controller::default();
                    state.memory_limit_bytes = None;
                }
                for _ in 0..count {
                    assert!(!willow_alloc(8).is_null());
                }
                let goal = {
                    let mut state = runtime().heap.lock().unwrap();
                    // Merge real pending LAB counters after setting a crossed
                    // goal: neither bytes-only nor full publication may grow it.
                    state.threshold_bytes = 1;
                    state.pacer_trigger = 1;
                    if limit_kind == 1 {
                        state.soft_memory.set_limit(1);
                    } else if limit_kind == 2 {
                        state.memory_limit_bytes = Some(1);
                    }
                    sync_bytes(&mut state);
                    assert_eq!(state.threshold_bytes, 1);
                    flush_all(&mut state);
                    assert_eq!(state.threshold_bytes, 1);
                    assert_eq!(state.total_allocs, count as u64);
                    assert_eq!(state.allocated_bytes, count * (GC_HEADER_SIZE + 8));
                    state.threshold_bytes
                };
                assert!(
                    allocation_should_collect(),
                    "paced={paced} limit={limit_kind} count={count}"
                );
                assert_eq!(runtime().heap.lock().unwrap().threshold_bytes, goal);
                println!(
                    "threshold_matrix paced={paced} limit={limit_kind} objects={count} goal_writes_during_publication=0"
                );
            }
        }
    }
    reset_internal_for_test();
}
