use super::*;
use crate::gc::*;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

fn assert_consistent(s: &WillowGcStatsV1) {
    assert_eq!(s.version, 1);
    assert!(s.phase <= 2);
    assert!(s.last_cycle.epoch <= s.epoch);
    if s.phase == 0 && s.last_cycle.epoch != 0 {
        assert_eq!(s.epoch, s.last_cycle.epoch);
    }
    assert_eq!(s.pauses.count, s.minor_cycles + s.major_cycles);
    assert_eq!(s.pauses.buckets.iter().sum::<u64>(), s.pauses.count);
    assert_eq!(s.minor_pauses.count, s.minor_cycles);
    assert_eq!(s.major_pauses.count, s.major_cycles);
    assert!(s.heap.young_occupied_bytes <= s.heap.occupied_bytes);
    assert!(s.heap.occupied_bytes <= s.heap.committed_bytes);
    assert!(s.heap.committed_bytes <= s.heap.reserved_bytes);
    assert_eq!(
        s.heap.reserved_bytes,
        s.heap.old_reserved_bytes + s.heap.nursery_reserved_bytes
    );
    assert!(s.last_cycle.mark_ns <= s.last_cycle.pause_ns);
    assert_eq!(
        s.last_cycle
            .heap_before_bytes
            .saturating_sub(s.last_cycle.heap_after_bytes),
        s.last_cycle.reclaimed_bytes
    );
}

#[test]
fn histogram_covers_zero_every_power_of_two_and_maximum() {
    let mut h = GcLatencyV1::default();
    h.record(0);
    assert_eq!(h.buckets[0], 1);
    for bit in 0..64 {
        let mut sample = GcLatencyV1::default();
        sample.record(1u64 << bit);
        assert_eq!(sample.buckets[bit + 1], 1);
        if bit > 0 {
            sample.record((1u64 << bit) - 1);
            assert_eq!(sample.buckets[bit], 1);
        }
    }
    h.record(u64::MAX);
    assert_eq!(h.buckets[64], 1);
    assert_eq!(h.max_ns, u64::MAX);
}

#[test]
fn histogram_and_cycle_work_saturate_without_wrapping() {
    let mut t = GcTelemetry::default();
    t.stats.major_cycles = u64::MAX;
    t.stats.marked_bytes = u64::MAX;
    t.stats.pauses.count = u64::MAX;
    t.stats.pauses.total_ns = u64::MAX;
    t.stats.pauses.buckets[1] = u64::MAX;
    t.complete(GcCycleV1 {
        kind: 2,
        pause_ns: 1,
        marked_bytes: 9,
        ..Default::default()
    });
    assert_eq!(t.stats.major_cycles, u64::MAX);
    assert_eq!(t.stats.marked_bytes, u64::MAX);
    assert_eq!(t.stats.pauses.count, u64::MAX);
    assert_eq!(t.stats.pauses.total_ns, u64::MAX);
    assert_eq!(t.stats.pauses.buckets[1], u64::MAX);
}

#[test]
fn fixed_v1_header_and_symbol_write_one_struct() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    assert_eq!(std::mem::offset_of!(WillowGcStatsV1, version), 0);
    assert_eq!(std::mem::offset_of!(WillowGcStatsV1, struct_size), 4);
    assert_eq!(std::mem::offset_of!(WillowGcStatsV1, timestamp_ns), 8);
    assert_eq!(std::mem::size_of::<GcLatencyV1>(), 544);
    assert_eq!(std::mem::size_of::<GcCycleV1>(), 96);
    // Lock V1's complete layout. Any extension needs a new versioned symbol.
    assert_eq!(std::mem::size_of::<WillowGcStatsV1>(), 2112);
    #[repr(C)]
    struct Guarded {
        before: u64,
        stats: WillowGcStatsV1,
        after: u64,
    }
    let mut guarded = Guarded {
        before: 0x1234,
        stats: Default::default(),
        after: 0x5678,
    };
    assert_eq!(willow_gc_stats_snapshot_v1(std::ptr::null_mut()), -1);
    assert_eq!(willow_gc_stats_snapshot_v1(&mut guarded.stats), 0);
    assert_eq!(guarded.before, 0x1234);
    assert_eq!(guarded.after, 0x5678);
    assert_eq!(
        guarded.stats.struct_size as usize,
        std::mem::size_of::<WillowGcStatsV1>()
    );
    assert_consistent(&guarded.stats);
}

#[test]
fn major_cycle_reports_live_graph_null_slots_and_duplicate_roots() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let mut parent = willow_alloc_typed(16, 3);
    willow_push_root(&mut parent);
    let child = willow_alloc(8);
    // SAFETY: parent owns two initialized reference slots; second stays null.
    unsafe {
        *parent.cast::<*mut u8>() = child;
    }
    willow_gc_add_runtime_root(parent); // duplicate root is traced once
    willow_alloc(24); // unreachable
    let before = snapshot();
    willow_gc_collect();
    let after = snapshot();
    assert_consistent(&after);
    assert_eq!(after.major_cycles, 1);
    assert_eq!(
        after.last_cycle.marked_bytes,
        2 * GC_HEADER_SIZE as u64 + 24
    );
    assert_eq!(after.last_cycle.scanned_bytes, 16);
    assert_eq!(after.last_cycle.root_scan_bytes, 16);
    assert_eq!(
        after.last_cycle.heap_after_bytes,
        after.last_cycle.marked_bytes
    );
    assert_eq!(after.last_cycle.reclaimed_bytes, GC_HEADER_SIZE as u64 + 24);
    assert_eq!(
        after.counters.allocation_bytes,
        before.counters.allocation_bytes
    );
    assert_eq!(after.counters.freed_objects, 1);
    willow_gc_remove_runtime_root(parent);
    willow_pop_root();
    willow_gc_collect();
    let empty = snapshot();
    assert_eq!(empty.heap.occupied_bytes, 0);
    assert!(empty.counters.released_bytes > 0);
    assert_eq!(empty.last_cycle.heap_after_bytes, 0);
}

#[test]
fn registered_trace_callbacks_contribute_reference_scan_bytes() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    unsafe fn trace(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
        slots.push(payload.cast());
    }
    willow_register_type(901, trace);
    let mut parent = willow_alloc_object(901, 8);
    willow_push_root(&mut parent);
    let child = willow_alloc(8);
    unsafe {
        *parent.cast::<*mut u8>() = child;
    }
    willow_gc_collect();
    let stats = snapshot();
    assert_eq!(stats.last_cycle.scanned_bytes, 8);
    assert_eq!(
        stats.last_cycle.marked_bytes,
        2 * (GC_HEADER_SIZE as u64 + 8)
    );
    willow_pop_root();
}

#[test]
fn minor_cycles_do_not_overwrite_the_last_major_live_measurement() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let mut live = willow_alloc(8);
    willow_push_root(&mut live);
    willow_gc_collect();
    let major = snapshot().last_major_cycle;
    willow_gc_minor_collect();
    let stats = snapshot();
    assert_consistent(&stats);
    assert_eq!(stats.last_cycle.kind, 1);
    assert_eq!(stats.minor_cycles, 1);
    assert_eq!(stats.major_cycles, 1);
    assert_eq!(stats.last_major_cycle.epoch, major.epoch);
    assert_eq!(
        stats.last_major_cycle.heap_after_bytes,
        major.heap_after_bytes
    );
    assert!(stats.last_cycle.epoch > major.epoch);
    willow_pop_root();
}

#[test]
fn reset_clears_cumulative_counts_without_reusing_cycle_identity() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    willow_gc_collect();
    let epoch = snapshot().epoch;
    reset_internal_for_test();
    assert_eq!(snapshot().pauses.count, 0);
    willow_gc_collect();
    assert!(snapshot().epoch > epoch);
}

#[test]
fn rates_use_explicit_windows_and_reject_counter_reset() {
    let mut before = WillowGcStatsV1 {
        version: 1,
        timestamp_ns: 1,
        ..Default::default()
    };
    let mut after = before;
    after.timestamp_ns += 1_000_000_000;
    after.counters.allocation_bytes = 100;
    after.marked_bytes = 50;
    after.mark_ns = 500_000_000;
    let rates = GcRates::between(&before, &after).unwrap();
    assert_eq!(rates.allocation_bytes_per_second, 100.0);
    assert_eq!(rates.mark_bytes_per_second, 50.0);
    assert_eq!(rates.mark_bytes_per_mark_second, 100.0);
    assert!(GcRates::between(&after, &before).is_none());
    assert!(GcRates::between(&before, &before).is_none());
    after.reset_generation = 1;
    assert!(GcRates::between(&before, &after).is_none());
    after.reset_generation = 0;
    before.counters.allocation_bytes = 101;
    assert!(GcRates::between(&before, &after).is_none());
}

#[test]
fn trace_pair_and_histogram_describe_the_same_cycle() {
    let c = GcCycleV1 {
        epoch: 8,
        kind: 2,
        start_ns: 10,
        end_ns: 42,
        pause_ns: 32,
        heap_before_bytes: 9,
        heap_after_bytes: 3,
        reclaimed_bytes: 6,
        ..Default::default()
    };
    let mut bytes = Vec::new();
    write_cycle(&mut bytes, c).unwrap();
    let output = String::from_utf8(bytes).unwrap();
    let lines: Vec<_> = output.lines().collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(
        lines[0],
        r#"{"version":1,"event":"gc_start","epoch":8,"kind":2,"timestamp_ns":10,"heap_bytes":9}"#
    );
    assert_eq!(
        lines[1],
        r#"{"version":1,"event":"gc_end","epoch":8,"kind":2,"timestamp_ns":42,"pause_ns":32,"mark_ns":0,"marked_bytes":0,"scanned_bytes":0,"root_scan_bytes":0,"heap_bytes":3,"reclaimed_bytes":6}"#
    );
    let mut t = GcTelemetry::default();
    t.complete(c);
    assert_eq!(t.stats.pauses.total_ns, 32);
    assert_eq!(t.stats.pauses.buckets[6], 1);
}

#[test]
fn broken_trace_writer_is_disabled_and_gc_still_completes() {
    let _guard = runtime_test_guard();
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("broken sink"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let before = TRACE_ERRORS.load(Ordering::Relaxed);
    let mut sink = TraceSink {
        writer: Some(Box::new(Broken)),
    };
    sink.cycle(GcCycleV1::default());
    assert!(sink.writer.is_none());
    assert_eq!(TRACE_ERRORS.load(Ordering::Relaxed), before + 1);
    sink.cycle(GcCycleV1::default());
    assert_eq!(TRACE_ERRORS.load(Ordering::Relaxed), before + 1);
    reset_internal_for_test();
    willow_alloc(8);
    willow_gc_collect();
    assert_eq!(snapshot().heap.occupied_bytes, 0);
}

#[test]
fn concurrent_allocation_registration_collection_and_100k_snapshots() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    willow_gc_register_mutator();
    let start = Arc::new(std::sync::Barrier::new(41));
    let done = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let readers: Vec<_> = (0..8)
        .map(|_| {
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                let mut previous = snapshot();
                for _ in 0..12_500 {
                    let s = snapshot();
                    assert_consistent(&s);
                    assert!(s.counters.allocation_bytes >= previous.counters.allocation_bytes);
                    assert!(s.pauses.count >= previous.pauses.count);
                    assert!(s.timestamp_ns >= previous.timestamp_ns);
                    previous = s;
                }
            })
        })
        .collect();
    let workers: Vec<_> = (0..32)
        .map(|_| {
            let start = start.clone();
            let done = done.clone();
            let ready = ready.clone();
            std::thread::spawn(move || {
                // Wait before registering: a barrier wait is not a GC safepoint.
                start.wait();
                willow_gc_register_mutator();
                ready.fetch_add(1, Ordering::Release);
                for _ in 0..64 {
                    std::hint::black_box(willow_alloc(8));
                    willow_gc_safepoint();
                }
                while !done.load(Ordering::Acquire) {
                    willow_gc_safepoint();
                    std::thread::yield_now();
                }
                willow_gc_unregister_mutator();
            })
        })
        .collect();
    start.wait();
    while ready.load(Ordering::Acquire) != 32 {
        willow_gc_safepoint();
        std::thread::yield_now();
    }
    for _ in 0..16 {
        willow_gc_collect();
    }
    done.store(true, Ordering::Release);
    while workers.iter().any(|h| !h.is_finished()) {
        willow_gc_safepoint();
        std::thread::yield_now();
    }
    for h in workers {
        h.join().unwrap();
    }
    willow_gc_unregister_mutator();
    for h in readers {
        h.join().unwrap();
    }
    willow_gc_collect();
    let stats = snapshot();
    assert_consistent(&stats);
    assert_eq!(stats.counters.allocation_count, 32 * 64);
    assert_eq!(
        stats.counters.allocation_bytes,
        32 * 64 * (GC_HEADER_SIZE as u64 + 8)
    );
    assert_eq!(stats.heap.occupied_bytes, 0);
}

#[test]
fn resident_sampling_is_best_effort_and_does_not_claim_heap_rss() {
    if let Some(bytes) = resident_bytes() {
        assert!(bytes > 0);
    }
    let _guard = runtime_test_guard();
    let s = snapshot();
    assert_eq!(s.resident_valid, 0); // frequent Rust API never performs OS I/O
    assert_eq!(s.process_resident_bytes, 0);
}

// Subprocesses isolate the process-global trace configuration from parallel
// tests. They exercise real open/buffered-write/flush failures, not just a mock.
#[test]
fn trace_subprocess_child() {
    let Ok(expected_errors) = std::env::var("WILLOW_GC_TELEMETRY_TEST_CHILD") else {
        return;
    };
    reset_internal_for_test();
    willow_alloc(8);
    willow_gc_collect();
    willow_gc_collect();
    let s = snapshot();
    assert_eq!(s.major_cycles, 2);
    assert_eq!(s.heap.occupied_bytes, 0);
    assert_eq!(s.trace_errors, expected_errors.parse::<u64>().unwrap());
}

fn run_trace_child(path: &std::path::Path, expected_errors: u64) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "gc_telemetry::tests::trace_subprocess_child",
            "--nocapture",
        ])
        .env(
            "WILLOW_GC_TELEMETRY_TEST_CHILD",
            expected_errors.to_string(),
        )
        .env("WILLOW_GC_TRACE", path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn trace_file_flushes_complete_pairs_and_open_failure_preserves_gc() {
    let dir = std::env::temp_dir().join(format!(
        "willow-gc-telemetry-{}-{}",
        std::process::id(),
        timestamp_ns()
    ));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("cycles.ndjson");
    run_trace_child(&path, 0);
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 4);
    for (index, pair) in lines.chunks(2).enumerate() {
        let epoch = format!("\"epoch\":{},", index + 1);
        assert!(pair[0].contains("\"event\":\"gc_start\""));
        assert!(pair[1].contains("\"event\":\"gc_end\""));
        assert!(pair.iter().all(|line| line.contains(&epoch)));
    }
    run_trace_child(&dir.join("missing").join("cycles.ndjson"), 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn trace_file_write_failure_preserves_collection() {
    run_trace_child(std::path::Path::new("/dev/full"), 1);
}
