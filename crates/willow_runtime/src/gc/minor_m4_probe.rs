//! Opt-in reproducible scaling probe; never part of an ordinary test run.
use crate::gc::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};

fn setting(name: &str, default: usize) -> usize {
    std::env::var(name).map_or(default, |v| v.parse().unwrap())
}

#[test]
#[ignore = "explicit M4 release measurement, independent dimensions"]
fn m4_pause_scaling_probe() {
    let _guard = runtime_test_guard();
    let _stress = GcStressTestScope::normal();
    let chunks = setting("M4_CHUNKS", 16);
    let roots = setting("M4_ROOTS", 256);
    let pins = setting("M4_PINS", 0);
    let workers = setting("M4_WORKERS", 0);
    let remembered = setting("M4_REMEMBERED", 32);
    let samples = setting("M4_SAMPLES", 101);
    assert!(chunks > 0 && roots > 0 && remembered <= 256);
    for sample in 0..samples {
        reset_internal();
        let mut tls = tlab_state_for_test();
        {
            let mut state = runtime().heap.lock().unwrap();
            state.threshold_bytes = usize::MAX;
            state.nursery_threshold_bytes = usize::MAX;
        }
        // Fixed 16 MiB old payload and 256 old owners. Only remembered density
        // changes between density cases; the young graph always stays live.
        let background = willow_alloc(16 * 1024 * 1024);
        let owners: Vec<_> = (0..256).map(|_| willow_alloc_typed(8, 1)).collect();
        let done = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(Barrier::new(workers + 1));
        let threads: Vec<_> = (0..workers)
            .map(|_| {
                let done = done.clone();
                let ready = ready.clone();
                std::thread::spawn(move || {
                    willow_gc_register_mutator();
                    ready.wait();
                    while !done.load(Ordering::Acquire) {
                        willow_gc_safepoint();
                        std::thread::yield_now();
                    }
                    willow_gc_unregister_mutator();
                })
            })
            .collect();
        ready.wait();
        let start = std::time::Instant::now();
        let size = GC_HEADER_SIZE + 8;
        let per_chunk = GC_TLAB_CHUNK_SIZE / size;
        let mut nodes = Vec::with_capacity(chunks * per_chunk);
        let mut head = std::ptr::null_mut();
        for _ in 0..chunks {
            for index in 0..per_chunk {
                let node = if index == 0 {
                    willow_gc_alloc_slow(&mut tls, 1, 0, 8, 1)
                } else {
                    let cursor = tls.cursor.load(Ordering::Acquire);
                    initialize_object_at(cursor as *mut u8, size, 0, 1, 1).unwrap();
                    publish_tlab_start_for_test(&tls, cursor as *mut u8);
                    tls.cursor.store(cursor + size, Ordering::Release);
                    (cursor + GC_HEADER_SIZE) as *mut u8
                };
                unsafe {
                    *node.cast::<*mut u8>() = head;
                }
                head = node;
                nodes.push(node);
            }
        }
        let mut root_slots = vec![head; roots];
        for slot in &mut root_slots {
            willow_gc_add_runtime_root_slot(slot);
        }
        let mut pin_slots: Vec<_> = nodes.iter().copied().take(pins).collect();
        assert_eq!(pin_slots.len(), pins);
        for slot in &mut pin_slots {
            runtime().pin_lease_slots.add((slot as *mut *mut u8).cast());
        }
        for &owner in owners.iter().take(remembered) {
            willow_gc_write_barrier(owner, std::ptr::null_mut(), head, 1);
            unsafe {
                *owner.cast::<*mut u8>() = head;
            }
        }
        let gc_cpu_start = crate::gc_telemetry::workers::thread_cpu_ns().unwrap();
        minor_collect_internal();
        let gc_cpu_ns = crate::gc_telemetry::workers::thread_cpu_ns()
            .unwrap()
            .checked_sub(gc_cpu_start)
            .unwrap();
        let elapsed = start.elapsed().as_nanos();
        done.store(true, Ordering::Release);
        for thread in threads {
            thread.join().unwrap();
        }
        let stop = crate::gc_telemetry::stops::snapshot_stops().last_event;
        let state = runtime().heap.lock().unwrap();
        assert_eq!(
            state.moved_objects + state.survivor_stats.pinned_promotions,
            nodes.len() as u64
        );
        let mut cursor = root_slots[0];
        for _ in 0..nodes.len() {
            assert!(!cursor.is_null());
            cursor = unsafe { *cursor.cast::<*mut u8>() };
        }
        assert!(cursor.is_null());
        println!(
            "M4 {{\"sample\":{sample},\"live_bytes\":{},\"roots\":{roots},\"pins\":{pins},\"workers\":{workers},\"remembered\":{remembered},\"pause_ns\":{},\"elapsed_ns\":{elapsed},\"gc_cpu_ns\":{gc_cpu_ns},\"reserved_after\":{},\"metadata_objects\":{},\"root_values\":{},\"objects\":{}}}",
            nodes.len() * size,
            stop.released_ns - stop.requested_ns,
            state.tlab_reserved_bytes + state.old_reserved_bytes,
            stop.work.metadata_objects,
            stop.work.root_values,
            nodes.len()
        );
        // M4_COMPONENTS_BEGIN: omitted when injecting this probe into baseline.
        let components = crate::gc_telemetry::pause::snapshot().last;
        assert_eq!(components.sequence, stop.sequence);
        assert_eq!(
            components.ns.iter().sum::<u64>(),
            stop.released_ns - stop.requested_ns
        );
        println!("M4_COMPONENTS {:?}", components.ns);
        // M4_COMPONENTS_END
        drop(state);
        for slot in &mut root_slots {
            willow_gc_remove_runtime_root_slot(slot);
        }
        for slot in &mut pin_slots {
            runtime()
                .pin_lease_slots
                .remove((slot as *mut *mut u8).cast());
        }
        std::hint::black_box(background);
        reset_internal();
    }
}
