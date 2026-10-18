use super::*;
fn e(op: u64, kind: u64, ns: u64) -> Event {
    Event {
        event: op,
        kind,
        timestamp_ns: ns,
        ..Default::default()
    }
}
#[test]
fn interval_kinds_histograms_and_work_are_separate() {
    let mut s = State::default();
    for kind in 0..KINDS as u64 {
        let id = s.begin(e(1, kind, 10));
        let mut end = e(2, kind, 10 + (1 << kind));
        end.identity = id;
        s.end(end, 10);
        assert_eq!(s.stats.intervals[kind as usize].count, 1);
        assert_eq!(s.stats.intervals[kind as usize].total_ns, 1 << kind);
    }
    assert_eq!(s.stats.active_intervals, 0);
    for kind in 0..6 {
        let mut event = e(6, kind, 0);
        event.work = Work {
            objects: 3,
            slots: 5,
            bytes: 7,
            metadata_bytes: 11,
            budget_checks: 13,
            yields: 17,
            restarts: 19,
        };
        s.work(event);
    }
    for work in s.stats.work {
        assert_eq!(
            (
                work.objects,
                work.slots,
                work.bytes,
                work.metadata_bytes,
                work.budget_checks,
                work.yields,
                work.restarts
            ),
            (3, 5, 7, 11, 13, 17, 19)
        );
    }
}
#[test]
fn bounded_history_reports_exact_loss_at_increasing_sizes() {
    for n in [0, 1, 64, CAPACITY, CAPACITY + 1, CAPACITY * 4] {
        let mut s = State::default();
        for _ in 0..n {
            s.event(e(5, 0, 0));
        }
        assert_eq!(s.events.len(), n.min(CAPACITY));
        assert_eq!(s.stats.sequence, n as u64);
        assert_eq!(s.stats.dropped, n.saturating_sub(CAPACITY) as u64);
        assert_eq!(s.stats.flags & LOST != 0, n > CAPACITY);
    }
}
#[test]
fn saturation_and_backwards_time_cannot_be_valid_zero() {
    let mut s = State::default();
    s.stats.sequence = u64::MAX;
    s.begin(e(1, 0, 10));
    s.end(e(2, 0, 9), 10);
    assert_eq!(s.stats.sequence, u64::MAX);
    assert_eq!(s.stats.flags & (SATURATED | INVALID), SATURATED | INVALID);
    s.stats.intervals[0].total_ns = u64::MAX;
    s.begin(e(1, 0, 0));
    s.end(e(2, 0, 1), 0);
    assert_eq!(s.stats.intervals[0].total_ns, u64::MAX);
    let mut event = e(6, 0, 0);
    event.work.bytes = u64::MAX;
    s.work(event);
    s.work(event);
    assert_eq!(s.stats.work[0].bytes, u64::MAX);
}
#[test]
fn overlapping_intervals_are_not_summed_as_wall_time() {
    let mut s = State::default();
    s.begin(e(1, 0, 10));
    s.begin(e(1, 1, 11));
    assert_eq!(s.stats.active_intervals, 2);
    s.end(e(2, 1, 20), 11);
    s.end(e(2, 0, 30), 10);
    assert_eq!(s.stats.intervals[0].total_ns, 20);
    assert_eq!(s.stats.intervals[1].total_ns, 9);
}
#[test]
fn trace_writer_reports_io_errors_and_checkpoint_schema() {
    let mut bytes = Vec::new();
    let mut s = State::default();
    s.event(e(5, 0, 1));
    write(&mut bytes, s.events.make_contiguous(), s.stats).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("gc_observation_checkpoint"));
    assert_eq!(text.lines().count(), 2);
    struct Broken;
    impl std::io::Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("injected"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    assert!(write(&mut Broken, &[], s.stats).is_err());
}
#[test]
fn concurrent_recording_and_snapshots_preserve_group_invariants() {
    let s = Mutex::new(State::default());
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let s = &s;
            scope.spawn(move || {
                for _ in 0..100 {
                    let mut s = s.lock().unwrap();
                    s.begin(e(1, 0, 1));
                    s.end(e(2, 0, 2), 1);
                }
            });
        }
        for _ in 0..100 {
            let s = s.lock().unwrap();
            assert_eq!(s.stats.sequence, 2 * s.stats.intervals[0].count);
            assert_eq!(s.stats.active_intervals, 0);
        }
    });
    assert_eq!(s.lock().unwrap().stats.intervals[0].count, 400);
}
#[test]
fn v4_abi_negotiation_preserves_v1_v2_v3() {
    use crate::gc_telemetry::*;
    let _guard = crate::gc::runtime_test_guard();
    assert_eq!(std::mem::offset_of!(WillowGcStatsV4, struct_size), 4);
    assert_eq!(std::mem::offset_of!(WillowGcStatsV4, baseline), 16);
    assert_eq!(std::mem::size_of::<Work>(), 56);
    assert_eq!(std::mem::size_of::<Stats>(), 56 + 9 * 544 + 6 * 56);
    let size = willow_gc_stats_size(4) as usize;
    let mut bytes = vec![0xa5; size + 2];
    assert_eq!(
        willow_gc_stats_snapshot(4, bytes[1..].as_mut_ptr(), size as i64 - 1),
        0
    );
    assert!(bytes.iter().all(|b| *b == 0xa5));
    assert_eq!(
        willow_gc_stats_snapshot(4, bytes[1..].as_mut_ptr(), size as i64),
        size as i64
    );
    assert_eq!((bytes[0], bytes[size + 1]), (0xa5, 0xa5));
    let v = unsafe {
        bytes[1..]
            .as_ptr()
            .cast::<WillowGcStatsV4>()
            .read_unaligned()
    };
    assert_eq!(
        (
            v.version,
            v.baseline.version,
            v.baseline.baseline.version,
            v.baseline.baseline.baseline.version
        ),
        (4, 3, 2, 1)
    );
    assert_eq!(
        v.baseline.struct_size as usize,
        std::mem::size_of::<WillowGcStatsV3>()
    );
}
#[test]
fn discarded_history_keeps_stats_without_loss() {
    let mut s = State {
        discard: true,
        ..Default::default()
    };
    for _ in 0..CAPACITY * 2 {
        s.begin(e(1, 0, 1));
        s.end(e(2, 0, 2), 1);
    }
    assert!(s.events.is_empty());
    assert_eq!(
        (s.stats.sequence, s.stats.dropped, s.stats.flags & LOST),
        (4 * CAPACITY as u64, 0, 0)
    );
    assert_eq!(s.stats.intervals[0].count, 2 * CAPACITY as u64);
}
const CHILD: &str = "WILLOW_OBSERVATION_TEST_CHILD";
const NAME: &str =
    "gc_telemetry::observations::tests::enabled_runtime_lifecycle_assist_unwind_and_lock_capture";
/// Runs the lifecycle body in a child process per trace configuration, since
/// the env gates are process-lifetime `LazyLock`s.
#[test]
fn enabled_runtime_lifecycle_assist_unwind_and_lock_capture() {
    let Some(mode) = std::env::var_os(CHILD) else {
        let dir = std::env::temp_dir().join(format!("willow-observations-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (mode, version) in [("measure", "3"), ("trace", "4")] {
            let trace = dir.join(format!("{mode}.jsonl"));
            let _ = std::fs::remove_file(&trace);
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture"])
                .env(CHILD, mode)
                .env("WILLOW_GC_MEASURE", "1")
                .env("WILLOW_GC_TRACE_VERSION", version)
                .env("WILLOW_GC_TRACE", &trace)
                .status()
                .unwrap();
            assert!(status.success(), "{mode} child failed");
            let text = std::fs::read_to_string(&trace).unwrap_or_default();
            // V1-V3 streams never receive V4 lines; V4 ends with a checkpoint.
            assert_eq!(
                text.contains("\"version\":4"),
                mode == "trace",
                "{mode}: {text}"
            );
            if mode == "trace" {
                assert!(
                    text.lines()
                        .last()
                        .unwrap()
                        .contains("gc_observation_checkpoint")
                );
                assert!(text.contains("\"aborted\":1"));
                assert!(
                    !text.contains("\"op\":5"),
                    "safepoint polls must not be events"
                );
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
        return;
    };
    let _guard = crate::gc::runtime_test_guard();
    let before = snapshot();
    std::thread::spawn(|| {
        let _registration = crate::gc::MutatorRegistration::new();
        for _ in 0..CAPACITY * 2 {
            crate::gc::willow_gc_safepoint();
        }
        let _ = std::panic::catch_unwind(|| {
            let _s = Span::new(Kind::Assist);
            panic!("injected unwind");
        });
        let mutex = HeapMutex::new(());
        drop(mutex.lock().unwrap());
    })
    .join()
    .unwrap();
    let after = snapshot();
    assert_eq!(after.registered - before.registered, 1);
    assert_eq!(after.unregistered - before.unregistered, 1);
    assert!(after.progress - before.progress >= 2 * CAPACITY as u64);
    assert!(
        after.intervals[Kind::Assist as usize].count
            > before.intervals[Kind::Assist as usize].count
    );
    assert_eq!(after.active_intervals, 0);
    // Safepoint polls neither enter the history nor force loss.
    assert_eq!((after.dropped, after.flags & LOST), (0, 0));
    // Explicit collection joins through the shared cycle wait, which is not an
    // allocation stall. Checked after lifecycle counts: it starts marker threads.
    let stalls = snapshot().intervals[Kind::AllocationStall as usize].count;
    std::thread::spawn(|| {
        let _registration = crate::gc::MutatorRegistration::new();
        crate::gc::willow_gc_collect();
    })
    .join()
    .unwrap();
    assert_eq!(
        snapshot().intervals[Kind::AllocationStall as usize].count,
        stalls
    );
    if mode == "measure" {
        assert!(
            take_events().0.is_empty(),
            "no V4 sink, so no retained history"
        );
    }
    crate::gc_telemetry::flush_observations();
}
