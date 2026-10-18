//! The oracle consumes generated adjacency, never collector layout/tracing code.
use super::*;
static CALLBACK_TRACES: AtomicU64 = AtomicU64::new(0);
unsafe fn reference_callback(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    CALLBACK_TRACES.fetch_add(1, Ordering::Relaxed);
    slots.push(payload.cast());
    slots.push(unsafe { payload.add(GC_STORAGE_WORD_BYTES) }.cast());
}

#[test]
fn m0_all_destination_barriers_publish_deletions_in_open_and_sealed_marking() {
    let _guard = runtime_test_guard();
    let _stress = GcStressTestScope::with_modes("all");
    let destinations = [
        GcStoreDestination::ObjectField,
        GcStoreDestination::ArrayElement,
        GcStoreDestination::MapValue,
        GcStoreDestination::EnumPayload,
        GcStoreDestination::InterfaceObject,
        GcStoreDestination::AsyncFrameSlot,
        GcStoreDestination::IndirectReference,
        GcStoreDestination::GlobalStatic,
        GcStoreDestination::ContainerInternal,
        GcStoreDestination::AsyncMutexCell,
        GcStoreDestination::AsyncRwLockCell,
        GcStoreDestination::BlockingCell,
        GcStoreDestination::BlockingRwCell,
    ];
    for phase in [1, 2] {
        for destination in destinations {
            reset_internal_for_test();
            let mut old = willow_alloc(8);
            willow_push_root(&mut old);
            let mut new = willow_alloc(8);
            willow_push_root(&mut new);
            let mut owner = willow_alloc_typed(24, 2);
            willow_push_root(&mut owner);
            let mut global = old;
            let slot = if destination == GcStoreDestination::GlobalStatic {
                &mut global as *mut *mut u8
            } else {
                unsafe { owner.add(GC_STORAGE_WORD_BYTES) }.cast::<*mut u8>()
            };
            unsafe {
                slot.write(old);
            }
            let owner_or_slot = match destination {
                GcStoreDestination::GlobalStatic | GcStoreDestination::IndirectReference => {
                    slot.cast()
                }
                _ => owner,
            };
            let before = barrier_variant_counts().0;
            super::concurrent_tests::assert_satb_mutation_retains(old, || {
                GC_MARK_PHASE.store(phase, Ordering::Release);
                willow_gc_write_barrier(
                    owner_or_slot,
                    old,
                    new,
                    destination as i64 | willow_abi::GC_STORE_VALUE_HAS_HEADER,
                );
                unsafe {
                    slot.write(new);
                }
            });
            let after = barrier_variant_counts().0;
            assert_eq!(
                after[destination as usize - 1] - before[destination as usize - 1],
                1,
                "{destination:?} phase={phase}"
            );
            assert_eq!(unsafe { slot.read() }, new);
            willow_pop_roots(3);
        }
    }
    reset_internal_for_test();
}

#[test]
fn m0_measurement_pending_reclaimed_reusable_and_phase_samples_are_distinct() {
    let _guard = runtime_test_guard();
    let _normal = GcStressTestScope::normal();
    reset_internal_for_test();
    runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
    let payload = GC_OLD_REGION_SIZE / 16;
    let dead_count = GC_OLD_REGION_SIZE / (payload + GC_HEADER_SIZE) + 2;
    let mut root = willow_alloc(payload as i64);
    willow_push_root(&mut root);
    for _ in 0..dead_count {
        willow_alloc(payload as i64);
    }
    assert!(runtime().heap.lock().unwrap().old_regions.len() >= 2);
    struct RestoreHook;
    impl Drop for RestoreHook {
        fn drop(&mut self) {
            *sweep::SWEEP_TEST_HOOK
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = None;
        }
    }
    let _restore = RestoreHook;
    *sweep::SWEEP_TEST_HOOK.lock().unwrap() = Some(|index| {
        let (m, _) = telemetry_memory_snapshot();
        assert_eq!(m.reusable_bytes, 0, "{m:?}");
        assert!(m.quarantined_bytes > 0, "{m:?}");
        if index == 0 {
            assert!(m.pending_sweep_bytes > 0, "{m:?}");
        }
        assert_eq!(
            crate::gc_telemetry::measurement::snapshot().reclaimed_bytes,
            0
        );
    });
    willow_gc_collect();
    *sweep::SWEEP_TEST_HOOK.lock().unwrap() = None;
    let (memory, _) = telemetry_memory_snapshot();
    assert_eq!(memory.pending_sweep_bytes, 0);
    assert!(memory.quarantined_bytes > 0, "{memory:?}");
    assert_eq!(memory.reusable_bytes, 0);
    // Newly reclaimed spans become reusable only after the next activation.
    willow_gc_collect();
    let (memory, _) = telemetry_memory_snapshot();
    assert_eq!(memory.quarantined_bytes, 0);
    assert!(memory.reusable_bytes > 0, "{memory:?}");
    assert_eq!(memory.managed_bytes as i64, willow_gc_allocated_bytes());
    let stats = crate::gc_telemetry::measurement::snapshot();
    assert_eq!(
        stats.reclaimed_bytes,
        (dead_count * (GC_HEADER_SIZE + payload)) as u64
    );
    assert!(stats.root_scan_samples > 0);
    willow_gc_minor_collect();
    let stats = crate::gc_telemetry::measurement::snapshot();
    assert!(stats.evacuation_samples > 0);
    assert!(stats.resume_samples > 0);
    let size = crate::gc_telemetry::willow_gc_stats_size(3);
    let mut bytes = vec![0; size as usize];
    assert_eq!(
        crate::gc_telemetry::willow_gc_stats_snapshot(3, bytes.as_mut_ptr(), size),
        size
    );
    let stats = unsafe {
        bytes
            .as_ptr()
            .cast::<crate::gc_telemetry::WillowGcStatsV3>()
            .read_unaligned()
    };
    assert_eq!(stats.memory.managed_bytes, memory.managed_bytes);
    assert_eq!(stats.barrier_variants_valid, 1);
    assert_eq!(
        stats.frames.task_frames_running,
        crate::frame_lifetime::snapshot().task_frames_running
    );
    willow_pop_root();
    reset_internal_for_test();
}

#[test]
fn m0_milestones_are_distinct_during_real_sweep_and_election_skip() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    struct RestoreHook;
    impl Drop for RestoreHook {
        fn drop(&mut self) {
            *sweep::SWEEP_TEST_HOOK
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = None;
        }
    }
    let _restore = RestoreHook;
    *sweep::SWEEP_TEST_HOOK.lock().unwrap() = Some(|_| {
        use crate::gc_telemetry::milestones::*;
        let record = snapshot();
        assert_eq!(record.stage, GcStage::MarkComplete as u32);
        for stage in [GcStage::SweepComplete, GcStage::Quiescent] {
            assert_eq!(
                wait_gc_for_test(GcTicket(record.epoch), stage, std::time::Duration::ZERO)
                    .unwrap_err(),
                WaitError::Timeout
            );
        }
    });
    willow_alloc(8);
    let ticket = crate::gc_telemetry::milestones::request_gc_for_test().unwrap();
    assert_eq!(
        crate::gc_telemetry::milestones::record(ticket.0)
            .unwrap()
            .stage,
        3
    );
    let elected = cycle_wait::CollectorGuard::try_acquire().unwrap();
    assert!(crate::gc_telemetry::milestones::request_gc_for_test().is_none());
    drop(elected);
    reset_internal_for_test();
}

fn reachable(edges: &[Vec<usize>], roots: &[usize]) -> (Vec<bool>, usize) {
    let mut seen = vec![false; edges.len()];
    let mut pending = Vec::new();
    for &root in roots {
        if !std::mem::replace(&mut seen[root], true) {
            pending.push(root);
        }
    }
    let mut inspected = 0;
    while let Some(node) = pending.pop() {
        for &child in &edges[node] {
            inspected += 1;
            if !std::mem::replace(&mut seen[child], true) {
                pending.push(child);
            }
        }
    }
    (seen, inspected)
}

#[test]
fn m0_differential_reference_tracer_randomized_heaps() {
    let _guard = runtime_test_guard();
    let _stress = GcStressTestScope::with_modes("all");
    const SEED: u64 = 0x6f_07_01;
    static BITMAP: [u64; 3] = [2, 2, 2];
    let mut random = SEED;
    let mut next = || {
        random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
        (random >> 32) as usize
    };
    eprintln!(
        "m0_reference seed={SEED} gc_mark_threads={} workers={}",
        mark_workers::configured_count(),
        std::env::var("WILLOW_WORKERS").unwrap_or_else(|_| "default".into())
    );
    for shape in 0..4 {
        for count in [1, 8, 32, 128] {
            for round in 0..8 {
                reset_internal_for_test();
                const CALLBACK_TYPE: u32 = 0x70701;
                willow_register_type(CALLBACK_TYPE, reference_callback);
                crate::gc_telemetry::failure_record::reproduction(
                    SEED,
                    ((shape as u64) << 48) | ((count as u64) << 16) | round,
                );
                let mut edges = vec![Vec::new(); count];
                let mut slots = Vec::with_capacity(count);
                let mut nodes = Vec::with_capacity(count);
                for _ in 0..count {
                    let layout = if shape == 2 {
                        [0, 1, 2, 4][next() % 4]
                    } else {
                        next() % 5
                    };
                    let (words, refs) = match layout {
                        0 => (3, vec![0]),
                        1 => (8, vec![1, 3, 7]),
                        2 => (66, vec![1, 65]),
                        4 => (3, vec![0, 1]),
                        _ => (3, vec![]),
                    };
                    let node = if layout == 4 {
                        willow_alloc_object(
                            CALLBACK_TYPE as i64,
                            (words * GC_STORAGE_WORD_BYTES) as i64,
                        )
                    } else if words == 66 {
                        let bytes = words * GC_STORAGE_WORD_BYTES;
                        let id = willow_abi::gc_bitmap_layout_id(
                            bytes as i64,
                            0,
                            willow_abi::gc_bitmap_fingerprint(&BITMAP[1..]),
                        );
                        willow_gc_alloc_bitmap(id as i64, bytes as i64, BITMAP.as_ptr())
                    } else {
                        let mask = refs.iter().fold(0, |mask, &slot| mask | (1u64 << slot));
                        willow_alloc_typed((words * GC_STORAGE_WORD_BYTES) as i64, mask)
                    };
                    unsafe {
                        std::ptr::write_bytes(node, 0, words * GC_STORAGE_WORD_BYTES);
                    }
                    slots.push(refs);
                    nodes.push(node);
                    willow_push_root(nodes.last_mut().unwrap());
                }
                let calls_before = telemetry_heap_snapshot().0.barrier_calls;
                for (index, refs) in slots.iter().enumerate() {
                    for &slot in refs {
                        let child = match shape {
                            0 => (index + 1) % count, // cycles
                            1 if index + 1 < count => index + 1 + next() % (count - index - 1), // DAG
                            2 if index + 1 < count => index + 1, // deep chain
                            3 => next() % count,                 // random fan-out/self edges
                            _ => continue,
                        };
                        edges[index].push(child);
                        willow_gc_write_barrier(
                            nodes[index],
                            std::ptr::null_mut(),
                            nodes[child],
                            GcStoreDestination::ObjectField as i64
                                | willow_abi::GC_STORE_VALUE_HAS_HEADER,
                        );
                        unsafe {
                            nodes[index]
                                .add(slot * GC_STORAGE_WORD_BYTES)
                                .cast::<*mut u8>()
                                .write(nodes[child]);
                        }
                    }
                    // A valid pointer in a scalar slot must not become an edge.
                    unsafe {
                        nodes[index]
                            .add(2 * GC_STORAGE_WORD_BYTES)
                            .cast::<*mut u8>()
                            .write(nodes[next() % count]);
                    }
                }
                assert_eq!(
                    telemetry_heap_snapshot().0.barrier_calls - calls_before,
                    edges.iter().map(Vec::len).sum::<usize>() as u64
                );
                for _ in 0..count {
                    willow_pop_root();
                }
                let root_indices: Vec<_> = if round == 0 {
                    vec![]
                } else {
                    vec![next() % count, next() % count]
                };
                let (expected, inspections) = reachable(&edges, &root_indices);
                assert!(inspections <= edges.iter().map(Vec::len).sum());
                let mut roots: Vec<_> = root_indices.iter().map(|&i| nodes[i]).collect();
                for root in &mut roots {
                    willow_push_root(root);
                }
                let callback_before = CALLBACK_TRACES.load(Ordering::Relaxed);
                let ticket = crate::gc_telemetry::milestones::request_gc_for_test().unwrap();
                if slots
                    .iter()
                    .enumerate()
                    .any(|(i, slots)| slots == &[0, 1] && expected[i])
                {
                    assert!(
                        CALLBACK_TRACES.load(Ordering::Relaxed) > callback_before,
                        "reachable native callback was not exercised"
                    );
                }
                assert_eq!(
                    crate::gc_telemetry::milestones::record(ticket.0)
                        .unwrap()
                        .stage,
                    3
                );
                let actual: HashSet<_> = {
                    let state = runtime().heap.lock().unwrap();
                    old_region_objects(&state)
                        .map(|object| object.payload().as_ptr() as usize)
                        .collect()
                };
                for (index, &node) in nodes.iter().enumerate() {
                    assert_eq!(
                        actual.contains(&(node as usize)),
                        expected[index],
                        "seed={SEED} shape={shape} count={count} round={round} node={index}"
                    );
                }
                for _ in &roots {
                    willow_pop_root();
                }
            }
        }
    }
    reset_internal_for_test();
}

#[test]
fn m0_reference_operation_counts_are_linear_in_reachable_edges() {
    for count in [1, 8, 64, 512, 4096] {
        for fanout in [1, 4, 16] {
            let edges: Vec<_> = (0..count)
                .map(|i| (1..=fanout).map(|j| (i + j) % count).collect())
                .collect();
            let (seen, visits) = reachable(&edges, &[0, 0, 0]);
            assert!(seen.iter().all(|&v| v));
            assert_eq!(visits, count * fanout);
            eprintln!("m0_reference count={count} fanout={fanout} edge_visits={visits}");
        }
    }
}
