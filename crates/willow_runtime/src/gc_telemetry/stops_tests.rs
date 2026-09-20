use super::*;

fn complete(t: &mut StopTelemetry, reason: StopReason, start: u64, aborted: bool) {
    let mut e = t.request(reason, start);
    e.stopped_ns = start + 10;
    e.released_ns = start + 30;
    e.outcome = u32::from(aborted);
    e.work.metadata_objects = 7;
    e.work.metadata_bytes = 7 * crate::gc::GC_HEADER_SIZE as u64;
    t.complete(e);
}

#[test]
fn stop_reasons_separate_rendezvous_work_and_aborts() {
    let mut t = StopTelemetry::default();
    for reason in [
        StopReason::InitialMark,
        StopReason::Remark,
        StopReason::Minor,
        StopReason::Recovery,
    ] {
        complete(&mut t, reason, 100 * reason as u64, reason as u32 == 4);
    }
    assert_eq!(t.stats.last_sequence, 4);
    assert_eq!(t.stats.active_sequence, 0);
    assert_eq!(t.stats.flags, STOP_COUNTERS_VALID);
    for (i, totals) in t.stats.by_reason.iter().enumerate() {
        assert_eq!(totals.requests, 1);
        assert_eq!(totals.completed, 1);
        assert_eq!(totals.aborted, u64::from(i == 3));
        assert_eq!(totals.rendezvous.total_ns, 10);
        assert_eq!(totals.stopped.total_ns, 20);
        assert_eq!(totals.work.metadata_objects, 7);
    }
}

#[test]
fn active_overlapping_and_malformed_intervals_cannot_look_complete() {
    let mut t = StopTelemetry::default();
    let first = t.request(StopReason::Minor, 5);
    assert_eq!(t.stats.active_sequence, first.sequence);
    assert_eq!(t.stats.by_reason[2].completed, 0);
    let second = t.request(StopReason::Remark, 6);
    assert_ne!(t.stats.flags & STOP_PROTOCOL_ERROR, 0);
    t.complete(first);
    assert_eq!(t.stats.active_sequence, second.sequence);
    t.complete(second);
    assert_ne!(t.stats.flags & STOP_PROTOCOL_ERROR, 0);
}

#[test]
fn event_storage_is_bounded_and_reports_every_lost_interval() {
    for n in [64, 256, 1024, 4096] {
        let mut t = StopTelemetry::default();
        for i in 0..n {
            complete(&mut t, StopReason::Minor, i as u64 * 40, false);
        }
        assert_eq!(t.pending.len(), EVENT_CAPACITY);
        assert_eq!(t.stats.events_dropped, (n - EVENT_CAPACITY) as u64);
        assert_eq!(t.stats.by_reason[2].requests, n as u64);
        assert_eq!(t.stats.by_reason[2].completed, n as u64);
        assert_eq!(t.stats.by_reason[2].work.metadata_objects, 7 * n as u64);
        assert_eq!(t.stats.flags & STOP_EVENTS_LOST != 0, n > EVENT_CAPACITY);
        println!(
            "requests={n} retained={} dropped={}",
            t.pending.len(),
            t.stats.events_dropped
        );
    }
}

#[test]
fn saturated_counters_are_explicitly_invalid_for_acceptance() {
    let mut t = StopTelemetry::default();
    t.stats.by_reason[2].requests = u64::MAX;
    t.stats.by_reason[2].stopped.total_ns = u64::MAX;
    complete(&mut t, StopReason::Minor, 0, false);
    assert_eq!(t.stats.by_reason[2].requests, u64::MAX);
    assert_eq!(t.stats.by_reason[2].stopped.total_ns, u64::MAX);
    assert_ne!(t.stats.flags & STOP_COUNTERS_SATURATED, 0);
}

#[test]
fn v2_negotiation_preserves_v1_and_guards_unaligned_output() {
    use crate::gc_telemetry::*;
    let _guard = crate::gc::runtime_test_guard();
    crate::gc::reset_internal_for_test();
    assert_eq!(std::mem::size_of::<WillowGcStatsV1>(), 2112);
    assert_eq!(std::mem::offset_of!(WillowGcStatsV2, baseline), 16);
    assert_eq!(std::mem::offset_of!(WillowGcStatsV2, stops), 2128);
    assert_eq!(std::mem::size_of::<StopEventV2>(), 80);
    assert_eq!(std::mem::size_of::<StopTotalsV2>(), 1144);
    assert_eq!(std::mem::size_of::<StopStatsV2>(), 4704);
    assert_eq!(std::mem::offset_of!(WillowGcStatsV2, workers), 6832);
    assert_eq!(std::mem::size_of::<WillowGcStatsV2>(), 6920);
    let size = willow_gc_stats_size(2) as usize;
    let mut buffer = vec![0xa5; size + 2];
    let out = unsafe { buffer.as_mut_ptr().add(1) };
    assert_eq!(willow_gc_stats_snapshot(2, out, size as i64 - 1), 0);
    assert!(buffer.iter().all(|&byte| byte == 0xa5));
    assert_eq!(
        willow_gc_stats_snapshot(2, std::ptr::null_mut(), size as i64),
        -1
    );
    assert_eq!(willow_gc_stats_snapshot(2, out, size as i64), size as i64);
    let snapshot = unsafe { out.cast::<WillowGcStatsV2>().read_unaligned() };
    assert_eq!(snapshot.version, 2);
    assert_eq!(snapshot.struct_size as usize, size);
    assert_eq!(snapshot.baseline.version, 1);
    assert_eq!(snapshot.stops.flags, STOP_COUNTERS_VALID);
    assert_eq!(buffer[0], 0xa5);
    assert_eq!(buffer[size + 1], 0xa5);
}

#[test]
fn actual_baseline_stops_are_visible_even_for_empty_heaps() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    willow_gc_collect();
    willow_gc_minor_collect();
    let stats = snapshot_stops();
    assert_eq!(stats.flags, STOP_COUNTERS_VALID);
    assert_eq!(stats.active_sequence, 0);
    assert_eq!(stats.events_pending, 0);
    assert_eq!(stats.by_reason.map(|t| t.requests), [0, 0, 1, 0]);
    assert_eq!(stats.by_reason.map(|t| t.completed), [0, 0, 1, 0]);
    let sequence = stats.last_sequence;
    let generation = stats.reset_generation;
    reset_internal_for_test();
    let reset = snapshot_stops();
    assert_eq!(reset.last_sequence, sequence);
    assert_eq!(reset.reset_generation, generation + 1);
    assert_eq!(reset.by_reason.map(|t| t.requests), [0; 4]);
}

#[test]
fn normal_old_cycles_do_not_scan_headers_or_sweep_under_stops() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    for n in [16, 64, 256, 1024] {
        reset_internal_for_test();
        for _ in 0..n {
            willow_alloc(8);
        }
        willow_gc_collect();
        let stats = snapshot_stops();
        assert!(stats.by_reason.iter().all(|r| r.requests == 0));
        assert_eq!(stats.by_reason[0].work.root_values, 0);
        assert_eq!(stats.by_reason[0].work.metadata_objects, 0);
        assert_eq!(stats.by_reason[1].work.metadata_objects, 0);
        assert_eq!(stats.by_reason[1].work.swept_objects, 0);
        assert_eq!(willow_gc_allocated_bytes(), 0);
        println!("heap_objects={n} roots=0 initial_headers=0 remark_headers=0 stopped_sweep=0");
    }
    reset_internal_for_test();
}

#[test]
fn unfinished_measurement_records_failure_instead_of_zero_stops() {
    let _guard = crate::gc::runtime_test_guard();
    crate::gc::reset_internal_for_test();
    let result = std::panic::catch_unwind(|| {
        let _stop = StopMeasurement::begin(StopReason::Recovery);
        panic!("injected measurement unwind");
    });
    assert!(result.is_err());
    let stats = snapshot_stops();
    assert_ne!(stats.flags & STOP_PROTOCOL_ERROR, 0);
    assert_eq!(stats.by_reason[3].requests, 1);
    assert_eq!(stats.by_reason[3].aborted, 1);
    crate::gc::reset_internal_for_test();
}
