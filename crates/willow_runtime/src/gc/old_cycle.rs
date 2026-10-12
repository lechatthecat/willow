//! Checked old-cycle transitions. Collector serialization spans sweep's final
//! free-span publication; epoch identities are process-monotonic, never reused.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Idle,
    InitialMarkSTW,
    ConcurrentMark,
    RemarkRequested,
    RemarkSTW,
    Sweep,
}

pub(super) struct State {
    phase: Phase,
    epoch: u64,
    sweep_completed_epoch: u64,
}

impl State {
    pub(super) fn initial(epoch: u64) -> Self {
        assert!(epoch > 0);
        let mut state = Self {
            phase: Phase::Idle,
            epoch,
            sweep_completed_epoch: epoch - 1,
        };
        state.transition(Phase::InitialMarkSTW);
        state
    }

    pub(super) fn transition(&mut self, next: Phase) {
        use Phase::*;
        assert!(
            matches!(
                (self.phase, next),
                (Idle, InitialMarkSTW)
                    | (InitialMarkSTW, ConcurrentMark)
                    | (ConcurrentMark, RemarkRequested)
                    | (RemarkRequested, RemarkSTW)
                    | (RemarkSTW, ConcurrentMark)
                    | (RemarkSTW, Sweep)
                    | (Sweep, Idle)
            ),
            "illegal old GC transition {:?} -> {:?} epoch {}",
            self.phase,
            next,
            self.epoch
        );
        if next == Idle {
            self.sweep_completed_epoch = self.epoch;
        }
        self.phase = next;
    }
}

pub(super) fn concurrent_enabled() -> bool {
    std::env::var("WILLOW_GC_CONCURRENT_MARK").as_deref() != Ok("0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_cycle_checks_every_phase_pair_and_sweep_completion() {
        use Phase::*;
        let phases = [
            Idle,
            InitialMarkSTW,
            ConcurrentMark,
            RemarkRequested,
            RemarkSTW,
            Sweep,
        ];
        for from in phases {
            for to in phases {
                let legal = matches!(
                    (from, to),
                    (Idle, InitialMarkSTW)
                        | (InitialMarkSTW, ConcurrentMark)
                        | (ConcurrentMark, RemarkRequested)
                        | (RemarkRequested, RemarkSTW)
                        | (RemarkSTW, ConcurrentMark)
                        | (RemarkSTW, Sweep)
                        | (Sweep, Idle)
                );
                let mut state = State {
                    phase: from,
                    epoch: 9,
                    sweep_completed_epoch: 8,
                };
                assert_eq!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| state.transition(to)))
                        .is_ok(),
                    legal
                );
                assert_eq!(
                    state.sweep_completed_epoch,
                    if legal && to == Idle { 9 } else { 8 }
                );
            }
        }
    }

    #[test]
    fn old_cycle_restarts_preserve_epoch_until_sweep_publication() {
        use Phase::*;
        let mut state = State::initial(17);
        state.transition(ConcurrentMark);
        for _ in 0..128 {
            state.transition(RemarkRequested);
            state.transition(RemarkSTW);
            state.transition(ConcurrentMark);
            assert_eq!(state.epoch, 17);
            assert_eq!(state.sweep_completed_epoch, 16);
        }
        state.transition(RemarkRequested);
        state.transition(RemarkSTW);
        state.transition(Sweep);
        assert_eq!(state.sweep_completed_epoch, 16);
        state.transition(Idle);
        assert_eq!(state.sweep_completed_epoch, 17);
    }
}

/// All normal remark chunks must reserve both work dimensions before running,
/// and sample the clock between chunks. The clock is injectable for tests.
pub(super) struct RemarkBudget {
    max_slots: u64,
    max_bytes: u64,
    max_work_ns: u64,
    slots: u64,
    bytes: u64,
    start: u64,
    last_ns: u64,
    checks: u64,
    yields: u64,
    regions: u64,
}

fn positive_setting(setting: Option<&str>, default: u64, maximum: u64) -> u64 {
    setting
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0 && n <= maximum)
        .unwrap_or(default)
}

impl RemarkBudget {
    pub(super) fn configured(now: u64) -> Self {
        fn positive(name: &str, default: u64, maximum: u64) -> u64 {
            positive_setting(std::env::var(name).ok().as_deref(), default, maximum)
        }
        Self::new(
            positive("WILLOW_GC_REMARK_MAX_SLOTS", 4096, 1_000_000),
            positive("WILLOW_GC_REMARK_MAX_BYTES", 1024 * 1024, 16 * 1024 * 1024),
            positive("WILLOW_GC_REMARK_MAX_WORK_NS", 1_000_000, 100_000_000),
            now,
        )
    }

    pub(super) fn new(max_slots: u64, max_bytes: u64, max_work_ns: u64, start: u64) -> Self {
        assert!(max_slots > 0 && max_bytes > 0 && max_work_ns > 0);
        Self {
            max_slots,
            max_bytes,
            max_work_ns,
            slots: 0,
            bytes: 0,
            start,
            last_ns: start,
            checks: 0,
            yields: 0,
            regions: 0,
        }
    }

    pub(super) fn record_region_publication(&mut self) {
        self.regions += 1;
    }

    pub(super) fn reserve(&mut self, slots: u64, bytes: u64, now: u64) -> bool {
        self.last_ns = now;
        self.checks += 1;
        #[cfg(test)]
        BUDGET_CHECKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some(total_slots) = self.slots.checked_add(slots) else {
            self.yields += 1;
            return false;
        };
        let Some(total_bytes) = self.bytes.checked_add(bytes) else {
            self.yields += 1;
            return false;
        };
        if total_slots > self.max_slots
            || total_bytes > self.max_bytes
            || now.saturating_sub(self.start) >= self.max_work_ns
        {
            self.yields += 1;
            #[cfg(test)]
            BUDGET_YIELDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return false;
        }
        self.slots = total_slots;
        self.bytes = total_bytes;
        true
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;

    #[test]
    fn old_cycle_remark_settings_reject_zero_overflow_and_effectively_unlimited_limits() {
        for value in [
            None,
            Some(""),
            Some("0"),
            Some("-1"),
            Some("1001"),
            Some("18446744073709551615"),
            Some("18446744073709551616"),
        ] {
            assert_eq!(positive_setting(value, 10, 1000), 10);
        }
        for value in [1, 10, 1000] {
            assert_eq!(positive_setting(Some(&value.to_string()), 10, 1000), value);
        }
    }

    #[test]
    fn old_cycle_remark_fake_clock_limits_preserve_unreserved_work() {
        for (slots, bytes, clock) in [(2, 1, 0), (1, 9, 0), (1, 1, 7)] {
            let mut budget = RemarkBudget::new(1, 8, 7, 100);
            assert!(!budget.reserve(slots, bytes, 100 + clock));
            assert_eq!((budget.slots, budget.bytes), (0, 0));
            assert!(budget.reserve(1, 8, 100));
            assert!(!budget.reserve(1, 1, 101));
            assert_eq!((budget.slots, budget.bytes), (1, 8));
        }
        let mut budget = RemarkBudget::new(u64::MAX, u64::MAX, 1, 0);
        assert!(budget.reserve(u64::MAX, u64::MAX, 0));
        assert!(!budget.reserve(1, 1, 0));
    }
}

#[cfg(test)]
pub(super) static REGION_VIEWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
pub(super) static BUDGET_CHECKS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
pub(super) static BUDGET_YIELDS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

#[cfg(test)]
mod integration_tests {
    use super::super::*;
    use super::*;

    #[test]
    fn old_cycle_fixed_root_live_heap_scaling_and_bitmap_ordering() {
        if std::env::var("WILLOW_OLD_CYCLE_SCALING_CHILD").as_deref() != Ok("1") {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "gc::old_cycle::integration_tests::old_cycle_fixed_root_live_heap_scaling_and_bitmap_ordering", "--nocapture"])
                .env("WILLOW_OLD_CYCLE_SCALING_CHILD", "1")
                .env_remove("WILLOW_GC_REMARK_MAX_SLOTS")
                .env_remove("WILLOW_GC_REMARK_MAX_BYTES")
                .env_remove("WILLOW_GC_REMARK_MAX_WORK_NS")
                .output().unwrap();
            print!("{}", String::from_utf8_lossy(&output.stdout));
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let _guard = runtime_test_guard();
        for mib in [64, 256, 1024] {
            reset_internal_for_test();
            runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
            let payload = 64 * 1024;
            let count = mib * 1024 * 1024 / payload;
            let mut root = std::ptr::null_mut();
            willow_push_root(&mut root);
            for _ in 0..count {
                let next = willow_gc_alloc_layout(0, 0, payload as i64, 1);
                unsafe {
                    store_gc_reference(next.cast(), root);
                }
                root = next;
            }
            if mib == 1024 {
                assert!(runtime().heap.lock().unwrap().old_regions.len() > 4096);
            }
            let before = willow_gc_allocated_bytes();
            take_stop_diagnostics();
            let views = REGION_VIEWS.load(Ordering::Relaxed);
            let checks = BUDGET_CHECKS.load(Ordering::Relaxed);
            willow_gc_collect();
            let stops = crate::gc_telemetry::stops::snapshot_stops();
            assert_eq!(stops.by_reason[0].work.metadata_objects, 0);
            assert_eq!(stops.by_reason[0].work.metadata_bytes, 0);
            assert_eq!(stops.by_reason[1].work.metadata_objects, 0);
            assert_eq!(stops.by_reason[1].work.swept_objects, 0);
            assert!(stops.by_reason[1].requests <= 2);
            if mib == 1024 {
                assert_eq!(
                    stops.by_reason[3].requests, 1,
                    "default slots require classified Recovery within the third round"
                );
            } else {
                assert_eq!(
                    stops.by_reason[3].requests, 0,
                    "smaller fixtures fit normal remark"
                );
            }
            assert_eq!(willow_gc_allocated_bytes(), before);
            let diagnostics = take_stop_diagnostics();
            let mut published_regions = 0;
            for diagnostic in diagnostics {
                println!(
                    "live_mib={mib} stop_reason={} collector_ns={} resume_gate_ns={} coordination_hold_ns={}",
                    diagnostic.reason,
                    diagnostic.collector_ns,
                    diagnostic.resume_ns,
                    diagnostic.coordination_hold_ns
                );
                if let Some(budget) = diagnostic.budget {
                    published_regions += budget.regions;
                    println!(
                        "live_mib={mib} stop_reason={} published_regions={} region_slots={} region_bytes={} used_slots={} used_bytes={} max_slots={} max_bytes={} max_work_ns={} checked_work_ns={} checks={} yields={}",
                        diagnostic.reason,
                        budget.regions,
                        budget.regions,
                        budget.regions,
                        budget.used[0],
                        budget.used[1],
                        budget.limits[0],
                        budget.limits[1],
                        budget.limits[2],
                        budget.used[2],
                        budget.checks,
                        budget.yields
                    );
                }
            }
            if mib == 1024 {
                assert!(published_regions > 0 && published_regions <= 4096);
            } else {
                assert_eq!(
                    published_regions as usize,
                    runtime().heap.lock().unwrap().old_regions.len()
                );
            }
            let preparation = *PREPARATION.lock().unwrap();
            println!(
                "live_mib={mib} concurrent_prepare_regions={} concurrent_index_entries={} concurrent_bitmap_words={} concurrent_bitmap_bytes={} concurrent_heap_hold_ns={}",
                preparation.regions,
                preparation.index_entries,
                preparation.bitmap_words,
                preparation.bitmap_words * 8,
                preparation.heap_hold_ns
            );
            let initial = &stops.by_reason[0];
            let remark = &stops.by_reason[1];
            println!(
                "live_mib={mib} heap_objects={count} roots=1 region_views={} initial_metadata_objects=0 remark_metadata_objects=0 stopped_sweep=0 recovery_stops={} budget_checks={} initial_wait_ns={} initial_work_resume_ns={} remark_wait_ns={} remark_work_resume_ns={}",
                REGION_VIEWS.load(Ordering::Relaxed) - views,
                stops.by_reason[3].requests,
                BUDGET_CHECKS.load(Ordering::Relaxed) - checks,
                initial.rendezvous.total_ns,
                initial.stopped.total_ns,
                remark.rendezvous.total_ns,
                remark.stopped.total_ns
            );
            // A second cycle must reset stale colors before publication, then
            // reclaim all objects after deleting the sole root.
            willow_pop_roots(1);
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), 0);
        }
        reset_internal_for_test();
    }

    #[test]
    fn old_cycle_shared_identity_is_monotonic_and_rejects_prior_cycle_work() {
        use crate::gc_mark_queue::{MarkEpoch, MarkWork, RejectReason};
        let mut previous = MarkEpoch::IDLE;
        for _ in 0..64 {
            let cycle = ConcurrentCycle::new([], IdSet::default(), IdMap::default(), 0);
            assert!(cycle.mark_epoch > previous);
            assert_eq!(cycle.mark_epoch.0, cycle.assist_epoch);
            assert_eq!(cycle.mark_epoch.0, cycle.phase.lock().unwrap().epoch);
            assert_eq!(cycle.queue.current_epoch(), cycle.mark_epoch);
            if !previous.is_idle() {
                assert_eq!(
                    cycle.queue.inject(MarkWork::object(
                        previous,
                        crate::gc_mark_queue::ObjectRef::from_addr(16).unwrap()
                    )),
                    Err(RejectReason::StaleEpoch)
                );
                assert_eq!(cycle.queue.snapshot().outstanding, 0);
            }
            previous = cycle.mark_epoch;
            assert_eq!(cycle.queue.end_epoch(), 0);
        }
    }

    #[test]
    fn old_cycle_root_count_scaling_is_separate_from_heap_scaling() {
        let _guard = runtime_test_guard();
        for count in [1, 16, 257, 4096] {
            reset_internal_for_test();
            runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
            let mut roots: Vec<_> = (0..count).map(|_| willow_alloc(8)).collect();
            for root in &mut roots {
                willow_push_root(root);
            }
            willow_gc_collect();
            let stop = crate::gc_telemetry::stops::snapshot_stops();
            assert_eq!(stop.by_reason[0].work.root_values, count as u64);
            assert_eq!(stop.by_reason[0].work.metadata_objects, 0);
            println!(
                "root_count={count} initial_root_values={} initial_metadata_objects=0 initial_wait_ns={} initial_work_resume_ns={}",
                stop.by_reason[0].work.root_values,
                stop.by_reason[0].rendezvous.total_ns,
                stop.by_reason[0].stopped.total_ns
            );
            willow_pop_roots(count as i32);
        }
        reset_internal_for_test();
    }

    #[test]
    fn old_cycle_tiny_remark_budget_restarts_then_recovers_without_loss() {
        if std::env::var("WILLOW_OLD_CYCLE_TINY_CHILD").as_deref() != Ok("1") {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "gc::old_cycle::integration_tests::old_cycle_tiny_remark_budget_restarts_then_recovers_without_loss", "--nocapture"])
                .env("WILLOW_OLD_CYCLE_TINY_CHILD", "1")
                .env("WILLOW_GC_REMARK_MAX_SLOTS", "1")
                .env("WILLOW_GC_REMARK_MAX_BYTES", "1")
                .env("WILLOW_GC_REMARK_MAX_WORK_NS", "1")
                .output().unwrap();
            print!("{}", String::from_utf8_lossy(&output.stdout));
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let mut root = willow_alloc(8);
        willow_push_root(&mut root);
        let before = willow_gc_allocated_bytes();
        willow_gc_collect();
        let stops = crate::gc_telemetry::stops::snapshot_stops();
        assert_eq!(stops.by_reason[1].requests, 1);
        assert_eq!(stops.by_reason[1].work.swept_objects, 0);
        assert_eq!(stops.by_reason[3].requests, 1);
        assert!(BUDGET_YIELDS.load(Ordering::Relaxed) > 0);
        assert_eq!(willow_gc_allocated_bytes(), before);
        willow_pop_roots(1);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_internal_for_test();
    }

    #[test]
    fn old_cycle_feature_off_uses_stopped_tracing() {
        if std::env::var("WILLOW_OLD_CYCLE_OFF_CHILD").as_deref() != Ok("1") {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "gc::old_cycle::integration_tests::old_cycle_feature_off_uses_stopped_tracing",
                    "--nocapture",
                ])
                .env("WILLOW_OLD_CYCLE_OFF_CHILD", "1")
                .env("WILLOW_GC_CONCURRENT_MARK", "0")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let mut root = willow_alloc_typed(8, 1);
        let child = willow_alloc(8);
        unsafe {
            store_gc_reference(root.cast(), child);
        }
        willow_push_root(&mut root);
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            (2 * (GC_HEADER_SIZE + 8)) as i64
        );
        let stops = crate::gc_telemetry::stops::snapshot_stops();
        assert_eq!(stops.by_reason[3].requests, 1);
        assert_eq!(stops.by_reason[1].work.swept_objects, 0);
        assert_eq!(GC_MARK_PHASE.load(Ordering::Acquire), 0);
        willow_pop_roots(1);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_internal_for_test();
    }

    #[test]
    fn old_cycle_tlab_initial_snapshot_does_not_index_headers() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        let mut tls = tlab_state_for_test();
        let mut root = std::ptr::null_mut();
        willow_push_root(&mut root);
        for _ in 0..128 {
            root = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
        }
        assert!(!root.is_null());
        let (_, cycle) = root_handshake::begin().unwrap();
        let stops = crate::gc_telemetry::stops::snapshot_stops();
        assert_eq!(stops.by_reason[0].work.metadata_objects, 0);
        assert!(
            runtime()
                .heap
                .lock()
                .unwrap()
                .tlab_chunks
                .iter()
                .any(BumpChunk::needs_index)
        );
        mark_closure::finish_stopped(&cycle, std::time::Instant::now());
        willow_pop_roots(1);
        reset_internal_for_test();
    }
}

/// Separate diagnostic group: the published V2 C layouts remain unchanged.
/// Times are measured at the stop boundary, never inferred from heap timings.
#[derive(Clone, Copy, Debug)]
pub(super) struct StopDiagnostic {
    pub(super) reason: u32,
    pub(super) collector_ns: u64,
    pub(super) resume_ns: u64,
    pub(super) coordination_hold_ns: u64,
    budget: Option<BudgetDiagnostic>,
}
static STOP_DIAGNOSTICS: std::sync::Mutex<std::collections::VecDeque<StopDiagnostic>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

pub(super) fn record_stop(reason: u32, start: u64, work_end: u64, unlocked: u64) {
    let diagnostic = StopDiagnostic {
        reason,
        collector_ns: work_end.saturating_sub(start),
        resume_ns: unlocked.saturating_sub(work_end),
        coordination_hold_ns: unlocked.saturating_sub(start),
        budget: (reason == 2).then(|| LAST_BUDGET.get()),
    };
    if std::env::var_os("WILLOW_GC_LOG").is_some() {
        if reason == 2 {
            let budget = diagnostic.budget.expect("remark owns its budget");
            eprintln!(
                "[gc] remark_budget max_slots={} max_bytes={} max_work_ns={} used_slots={} used_bytes={} checked_work_ns={} checks={} yields={} published_regions={} region_slots={} region_bytes={}",
                budget.limits[0],
                budget.limits[1],
                budget.limits[2],
                budget.used[0],
                budget.used[1],
                budget.used[2],
                budget.checks,
                budget.yields,
                budget.regions,
                budget.regions,
                budget.regions * std::mem::size_of::<bool>() as u64
            );
        }
        eprintln!(
            "[gc] stop reason={} collector_ns={} resume_gate_ns={} coordination_hold_ns={}",
            diagnostic.reason,
            diagnostic.collector_ns,
            diagnostic.resume_ns,
            diagnostic.coordination_hold_ns
        );
    }
    let mut events = STOP_DIAGNOSTICS.lock().unwrap_or_else(|p| p.into_inner());
    if events.len() == 64 {
        events.pop_front();
    }
    events.push_back(diagnostic);
}

#[cfg(test)]
fn take_stop_diagnostics() -> Vec<StopDiagnostic> {
    STOP_DIAGNOSTICS.lock().unwrap().drain(..).collect()
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct PreparationDiagnostic {
    pub(super) bitmap_words: usize,
    pub(super) regions: usize,
    pub(super) index_entries: usize,
    pub(super) heap_hold_ns: u64,
}
static PREPARATION: std::sync::Mutex<PreparationDiagnostic> =
    std::sync::Mutex::new(PreparationDiagnostic {
        bitmap_words: 0,
        regions: 0,
        index_entries: 0,
        heap_hold_ns: 0,
    });
pub(super) fn record_preparation(
    bitmap_words: usize,
    regions: usize,
    index_entries: usize,
    heap_hold_ns: u64,
) {
    let diagnostic = PreparationDiagnostic {
        bitmap_words,
        regions,
        index_entries,
        heap_hold_ns,
    };
    if std::env::var_os("WILLOW_GC_LOG").is_some() {
        eprintln!(
            "[gc] concurrent_prepare regions={} index_entries={} bitmap_words={} bitmap_bytes={} heap_hold_ns={}",
            diagnostic.regions,
            diagnostic.index_entries,
            diagnostic.bitmap_words,
            diagnostic.bitmap_words * 8,
            diagnostic.heap_hold_ns
        );
    }
    *PREPARATION.lock().unwrap_or_else(|p| p.into_inner()) = diagnostic;
}

#[derive(Clone, Copy, Debug)]
struct BudgetDiagnostic {
    limits: [u64; 3],
    used: [u64; 3],
    checks: u64,
    yields: u64,
    regions: u64,
}
std::thread_local! {
    static LAST_BUDGET: std::cell::Cell<BudgetDiagnostic> = const { std::cell::Cell::new(BudgetDiagnostic {
        limits: [0; 3], used: [0; 3], checks: 0, yields: 0, regions: 0,
    }) };
}
impl Drop for RemarkBudget {
    fn drop(&mut self) {
        LAST_BUDGET.set(BudgetDiagnostic {
            limits: [self.max_slots, self.max_bytes, self.max_work_ns],
            used: [
                self.slots,
                self.bytes,
                self.last_ns.saturating_sub(self.start),
            ],
            checks: self.checks,
            yields: self.yields,
            regions: self.regions,
        });
    }
}
