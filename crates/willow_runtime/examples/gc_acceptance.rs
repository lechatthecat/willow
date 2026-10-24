//! Isolated graph probe for benches/gc_acceptance. Never a universal GC gate.
#![no_main]
use std::time::Instant;
#[path = "gc_acceptance/transport.rs"]
mod transport;
#[path = "gc_acceptance/workloads.rs"]
mod workloads;
use willow_runtime::{gc, gc_telemetry as t};

fn snapshot() -> t::WillowGcStatsV4 {
    let mut value = t::WillowGcStatsV4::default();
    let size = std::mem::size_of_val(&value) as i64;
    assert_eq!(
        t::willow_gc_stats_snapshot(4, (&mut value as *mut t::WillowGcStatsV4).cast(), size),
        size
    );
    value
}
fn endpoint(label: &str) {
    t::flush_observations();
    let s = snapshot();
    t::flush_observations();
    let o = t::observations::snapshot();
    let b = s.baseline.baseline.baseline;
    let stop = t::stops::snapshot_stops();
    println!(
        "{{\"schema\":1,\"event\":\"endpoint\",\"label\":\"{label}\",\"timestamp_ns\":{},\"observation\":{{\"sequence\":{},\"flags\":{},\"dropped\":{},\"active\":{},\"registered\":{},\"unregistered\":{},\"progress\":{},\"trace_errors\":{},\"reset_generation\":{}}},\"stop\":{{\"version\":2,\"flags\":{},\"reset_generation\":{},\"last_sequence\":{},\"active_sequence\":{},\"events_dropped\":{},\"events_pending\":{},\"requests\":{:?},\"completed\":{:?},\"aborted\":{:?},\"trace_errors\":{},\"completed_cycles\":{}}}}}",
        s.timestamp_ns,
        o.sequence,
        o.flags,
        o.dropped,
        o.active_intervals,
        o.registered,
        o.unregistered,
        o.progress,
        t::trace_errors(),
        b.reset_generation,
        stop.flags,
        stop.reset_generation,
        stop.last_sequence,
        stop.active_sequence,
        stop.events_dropped,
        stop.events_pending,
        stop.by_reason.map(|x| x.requests),
        stop.by_reason.map(|x| x.completed),
        stop.by_reason.map(|x| x.aborted),
        t::trace_errors(),
        b.minor_cycles + b.major_cycles
    );
}
fn sample(index: usize, elapsed: u128, operations: usize, checksum: u64, workload_checksum: u64) {
    let s = snapshot();
    let overload_peak = workloads::overload_peak();
    let (channel_overlap, sweep_overlap) = workloads::overlap_counts();
    let b = s.baseline.baseline.baseline;
    let p = t::pacing::snapshot();
    let sweep = t::sweep::snapshot();
    let (sweep_objects, sweep_bytes, sweep_ns) = (sweep.objects, sweep.bytes, sweep.wall_ns);
    let (peak_committed, wasted_bytes) = t::footprint::snapshot();
    let peak_rss = t::peak_resident_bytes().expect("OS peak RSS unavailable");
    println!(
        "{{\"schema\":1,\"event\":\"sample\",\"index\":{index},\"elapsed_ns\":{elapsed},\"operations\":{operations},\"checksum\":{checksum},\"workload_checksum\":{workload_checksum},\"channel_overlap\":{channel_overlap},\"sweep_overlap\":{sweep_overlap},\"overload_peak\":{overload_peak},\"peak_committed_bytes\":{peak_committed},\"peak_rss_bytes\":{peak_rss},\"wasted_bytes\":{wasted_bytes},\"sweep_objects\":{sweep_objects},\"sweep_bytes\":{sweep_bytes},\"sweep_ns\":{sweep_ns},\"timestamp_ns\":{},\"allocation_bytes\":{},\"committed_bytes\":{},\"reserved_bytes\":{},\"rss_bytes\":{},\"rss_valid\":{},\"live_bytes\":{},\"quarantine_bytes\":{},\"reclaimed_bytes\":{},\"copied_bytes\":{},\"promoted_bytes\":{},\"fragmentation_bytes\":{},\"barrier_ops\":{},\"gc_cpu_ns\":{},\"cpu_missing_cycles\":{},\"goal_bytes\":{},\"cycles\":{}}}",
        s.timestamp_ns,
        b.counters.allocation_bytes,
        b.heap.committed_bytes,
        b.heap.reserved_bytes,
        b.process_resident_bytes,
        b.resident_valid,
        s.baseline.memory.last_major_retained_bytes,
        s.baseline.memory.quarantined_bytes,
        s.baseline.measurements.reclaimed_bytes,
        s.baseline
            .survivors
            .survivor_bytes
            .saturating_add(s.baseline.survivors.tenured_bytes),
        b.counters.promoted_bytes,
        s.baseline.memory.fragmentation_bytes,
        b.counters.barrier_calls,
        p.gc_cpu_ns,
        p.cpu_missing_cycles,
        p.decision.goal_bytes,
        b.major_cycles + b.minor_cycles
    );
    t::flush_observations();
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_user_main() {
    let args: Vec<_> = std::env::args().collect();
    assert_eq!(
        args.len(),
        6,
        "shape live-bytes roots warmup measured-cycles"
    );
    let shape = args[1].as_str();
    assert!(
        [
            "linked",
            "array",
            "fanout",
            "burst",
            "fragmented",
            "tasks",
            "cancellation",
            "pingpong",
            "fanin",
            "producer_consumer",
            "young",
            "young_overlap",
            "sweep_overlap",
            "ffi_pins",
            "catalog",
            "workers",
            "overload",
            "cyclic",
            "churn"
        ]
        .contains(&shape)
    );
    let workers: usize = std::env::var("WILLOW_WORKERS")
        .unwrap_or("1".into())
        .parse()
        .unwrap();
    let work: usize = std::env::var("GC_ACCEPTANCE_OPERATIONS")
        .unwrap_or("1024".into())
        .parse()
        .unwrap();
    assert!(workers > 0 && work > 0);
    let bytes: usize = args[2].parse().unwrap();
    let roots_count: usize = args[3].parse().unwrap();
    let warmup: usize = args[4].parse().unwrap();
    let cycles: usize = args[5].parse().unwrap();
    assert!(roots_count > 0 && roots_count <= i32::MAX as usize && cycles > 0);
    let payload = if shape == "array" {
        65536
    } else if shape == "overload" {
        1024
    } else {
        256
    };
    let id_offset = if shape == "overload" { 1016 } else { 128 };
    let count = bytes.div_ceil(payload).max(roots_count);
    let mut roots = vec![std::ptr::null_mut(); roots_count];
    gc::willow_gc_init();
    endpoint("before");
    // Only the V4 drainer consumes observations; without it the runtime sink
    // keeps flushing even when a spool path is configured.
    let v4 = std::env::var("WILLOW_GC_TRACE_VERSION").as_deref() == Ok("4");
    t::acceptance_external_observation_consumer(
        v4 && std::env::var_os("GC_ACCEPTANCE_EVENT_SPOOL").is_some(),
    );
    let draining = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(v4));
    let drain_flag = draining.clone();
    let drainer = std::thread::spawn(move || transport::drain(drain_flag));
    {
        let _registration = gc::MutatorRegistration::new();
        for root in &mut roots {
            gc::willow_push_root(root);
        }
        // Each fixed root owns an independent chain. Root count and heap bytes
        // vary independently. No unrooted raw pointer survives an allocation.
        let fanout = if shape == "overload" {
            63
        } else if shape == "fanout" {
            8
        } else if shape == "cyclic" {
            2
        } else {
            1
        };
        for i in 0..count {
            let slot = i % roots_count;
            let node = gc::willow_alloc_typed(payload as i64, (1 << fanout) - 1);
            for edge in 0..fanout {
                let child = if shape == "cyclic" && edge == 1 {
                    node
                } else {
                    roots[(slot + edge) % roots_count]
                };
                gc::willow_gc_write_barrier(node, std::ptr::null_mut(), child, 1);
                // SAFETY: pointer-aligned, initialized allocation and mask above.
                unsafe {
                    *node.cast::<*mut u8>().add(edge) = child;
                }
            }
            unsafe {
                *node.add(id_offset).cast::<u64>() = i as u64;
            }
            roots[slot] = node;
            gc::willow_gc_safepoint();
            if i % 64 == 0 {
                t::flush_observations();
            }
        }
        // Establish the retained-heap pacing baseline before allocation traffic;
        // otherwise the initial default goal can hide Assist demand entirely.
        gc::willow_gc_collect();
        // Repeated collection verifies retained payloads using deterministic data,
        // not unstable heap addresses. Allocation traffic is short-lived.
        for cycle in 0..warmup + cycles {
            let start = Instant::now();
            let workload_checksum = match shape {
                "tasks" | "cancellation" | "pingpong" | "fanin" | "producer_consumer" => {
                    workloads::tasks(shape, work, workers)
                }
                "young" | "ffi_pins" | "catalog" => workloads::managed(shape, work),
                "fragmented" => workloads::fragmented(work),
                "workers" => workloads::parallel(work, workers),
                "overload" => workloads::overload(work, workers),
                "young_overlap" => workloads::overlap(work, workers, true),
                "sweep_overlap" => workloads::overlap(work, workers, false),
                _ => 0,
            };
            eprintln!("workload cycle={cycle} checksum={workload_checksum}");
            let allocations = if shape == "churn" {
                work
            } else if shape == "burst" {
                count / 2 + 1
            } else {
                256
            };
            for i in 0..allocations {
                let size = if shape == "fragmented" {
                    4096 + (i % 7) * 1024
                } else {
                    128
                };
                std::hint::black_box(gc::willow_alloc(size as i64));
                gc::willow_gc_safepoint();
                if i % 64 == 0 {
                    t::flush_observations();
                }
            }
            if shape == "burst" {
                for &head in &roots {
                    let mut saved = unsafe { *head.cast::<*mut u8>() };
                    gc::willow_push_root(&mut saved);
                    for _ in 0..64 {
                        gc::willow_gc_write_barrier(head, saved, std::ptr::null_mut(), 1);
                        unsafe {
                            *head.cast::<*mut u8>() = std::ptr::null_mut();
                        }
                        gc::willow_gc_write_barrier(head, std::ptr::null_mut(), saved, 1);
                        unsafe {
                            *head.cast::<*mut u8>() = saved;
                        }
                    }
                    gc::willow_pop_root();
                }
            }
            if let Ok(fault) = std::env::var("GC_ACCEPTANCE_FAULT") {
                gc::willow_gc_collect();
                gc::acceptance_inject(&fault);
            }
            gc::willow_gc_collect();
            let mut checksum = 0u64;
            let mut visited = 0;
            for &head in &roots {
                let mut cursor = head;
                while !cursor.is_null() {
                    // No allocation/safepoint during this read of rooted chains.
                    unsafe {
                        checksum = checksum.wrapping_add(*cursor.add(id_offset).cast::<u64>());
                        let child = *cursor.cast::<*mut u8>();
                        for edge in 1..fanout {
                            let target = *cursor.cast::<*mut u8>().add(edge);
                            if shape == "cyclic" {
                                assert_eq!(target, cursor);
                            } else {
                                let id = *cursor.add(id_offset).cast::<u64>() as usize;
                                let residue = (id % roots_count + edge) % roots_count;
                                if id <= residue {
                                    assert!(target.is_null());
                                } else {
                                    assert_eq!(
                                        *target.add(id_offset).cast::<u64>() as usize,
                                        id - 1 - (id - 1 - residue) % roots_count
                                    );
                                }
                            }
                        }
                        cursor = child;
                    }
                    visited += 1;
                    assert!(visited <= count, "corrupt/cyclic chain");
                }
            }
            assert_eq!(visited, count);
            assert_eq!(checksum, (count as u64).wrapping_mul(count as u64 - 1) / 2);
            let elapsed = start.elapsed().as_nanos();
            sample(
                cycle,
                elapsed,
                allocations
                    + count
                    + if matches!(shape, "workers" | "overload" | "sweep_overlap") {
                        workers * work
                    } else if workload_checksum > 0 {
                        work
                    } else {
                        0
                    },
                checksum,
                workload_checksum,
            );
        }
        gc::willow_pop_roots(roots_count as i32);
        gc::willow_gc_collect();
        gc::willow_gc_safepoint();
    }
    draining.store(false, std::sync::atomic::Ordering::Release);
    let (flushes, observer_cpu_ns) = drainer.join().unwrap();
    t::acceptance_external_observation_consumer(false);
    eprintln!(
        "observer flushes={flushes} cpu_ns={observer_cpu_ns:?} event_capacity={} event_size={} stop_capacity={} stop_size={}",
        t::observations::transport_capacity(),
        std::mem::size_of::<t::observations::Event>(),
        t::stops::transport_capacity(),
        std::mem::size_of::<t::stops::StopEventV2>()
    );
    endpoint("after");
}
#[unsafe(no_mangle)]
pub extern "C" fn __willow_static_init() {}
