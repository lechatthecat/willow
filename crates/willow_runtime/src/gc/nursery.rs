//! Constant-work minor-trigger policy; consumes counters from the completed cycle.

const FLOOR: usize = 256 * 1024;
const CAP: usize = 64 * 1024 * 1024;

/// Preserve the compact synchronous startup nursery.
pub(super) fn initial_budget() -> usize {
    FLOOR
}

/// Called once when an actual scheduler drive starts, not merely because a
/// synchronous program configured worker capacity. Do not overwrite learned
/// survival/pacing feedback when sync work already warmed the collector.
pub(super) fn scheduler_start(state: &mut super::GcState, workers: usize) {
    if state.scheduler_runway_initialized {
        return;
    }
    state.scheduler_runway_initialized = true;
    if state.major_collections != 0
        || state.minor_collections != 0
        || state.concurrent_cycle.is_some()
        || state.sweeping.is_some()
    {
        return;
    }
    let budget = major_startup_budget_for(workers);
    // Worker capacity is not allocation pressure: an async main may be the
    // only allocator. Keep the compact nursery (including its configured
    // override/limit) until measured survival justifies growing it. Widen only
    // the major-cycle runway; otherwise the first minor retains up to 32 MiB
    // of dead young objects solely because a scheduler was entered.
    super::pacer::scheduler_start(state, budget as u64);
}

/// Bounded cold major-cycle runway for scheduler capacity, without reserving
/// storage. Minor survival feedback independently controls the nursery.
fn major_startup_budget_for(workers: usize) -> usize {
    workers.clamp(1, 8) * (4 * 1024 * 1024)
}

#[derive(Clone, Copy, Default)]
pub(super) struct Policy {
    fixed: Option<usize>,
}

impl Policy {
    pub(super) fn from_env() -> Self {
        Self {
            fixed: std::env::var("WILLOW_GC_NURSERY_BYTES").ok().map(|value| {
                value
                    .parse::<usize>()
                    .ok()
                    .filter(|bytes| *bytes > 0)
                    .expect("WILLOW_GC_NURSERY_BYTES must be a positive byte count")
            }),
        }
    }

    fn bound(&self, bytes: usize, memory_limit: Option<usize>) -> usize {
        // A hard reservation limit takes precedence even below the usual floor.
        // The allocator independently enforces the combined old + TLAB budget.
        bytes.min(CAP).min(memory_limit.unwrap_or(usize::MAX))
    }

    pub(super) fn initial(&self, memory_limit: Option<usize>) -> usize {
        self.bound(self.fixed.unwrap_or_else(initial_budget), memory_limit)
    }

    pub(super) fn next(
        &self,
        current: usize,
        young_before: usize,
        promoted: u64,
        memory_limit: Option<usize>,
    ) -> usize {
        if self.fixed.is_some() {
            return self.initial(memory_limit);
        }
        // A minor's cost follows its survivor bytes, not the dead bytes it
        // skips, and its stop is serial: keep survivors between 1/64 and 1/16
        // of the nursery so the copy stays amortized over the allocation
        // between minors. Above the band, jump straight to 32 times the
        // survivors instead of doubling once per cycle: ramp-up cycles
        // otherwise copy (and tenure) nearly every in-flight object, and
        // concurrent allocators each keep a live working set in flight
        // (willow-jz15.120). Empty cycles carry no survival evidence; the dead
        // band avoids oscillation, and division keeps the comparisons safe
        // even for extreme accounting values.
        let target = if young_before == 0 {
            current
        } else if promoted >= (young_before / 16) as u64 {
            let goal = usize::try_from(promoted.saturating_mul(32)).unwrap_or(usize::MAX);
            current.saturating_mul(2).max(goal)
        } else if promoted <= (young_before / 64) as u64 {
            current / 2
        } else {
            current
        };
        self.bound(target.max(FLOOR), memory_limit)
    }
}

/// Three consistently high-survival cycles justify skipping the intermediate
/// survivor copy. Low survival resets immediately; empty cycles are no evidence.
/// This changes only destination generation, never liveness or reservation limits.
#[derive(Default)]
pub(super) struct Tenuring {
    high_survival: u8,
}
impl Tenuring {
    pub(super) fn age(&self) -> u8 {
        if self.high_survival == 3 { 1 } else { 2 }
    }
    pub(super) fn complete(&mut self, young: usize, survived: u64) {
        if young == 0 {
            return;
        }
        if survived >= (young - young / 8) as u64 {
            self.high_survival = self.high_survival.saturating_add(1).min(3);
        } else if survived < (young - young / 4) as u64 {
            self.high_survival = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenuring_feedback_requires_persistent_survival_and_recovers() {
        for young in [8usize, 1024, 1 << 30, usize::MAX] {
            let mut policy = Tenuring::default();
            assert_eq!(policy.age(), 2);
            for cycle in 0..3 {
                policy.complete(young, (young - young / 8) as u64);
                assert_eq!(policy.age(), if cycle == 2 { 1 } else { 2 });
            }
            policy.complete(0, 0);
            assert_eq!(policy.age(), 1);
            policy.complete(young, (young - young / 4) as u64);
            assert_eq!(policy.age(), 1);
            policy.complete(young, 0);
            assert_eq!(policy.age(), 2);
        }
    }

    #[test]
    fn nursery_runtime_survival_updates_live_telemetry() {
        use crate::gc::*;
        let _guard = runtime_test_guard();
        let initial = initial_budget();
        for n in [32, 128, 512] {
            reset_internal();
            let mut tls = tlab_state_for_test();
            // Rewritable runtime-root slots; full capacity keeps them in place.
            let mut roots = Vec::<*mut u8>::with_capacity(n);
            assert_eq!(
                telemetry_heap_snapshot().1.minor_trigger_bytes,
                initial as u64
            );
            for _ in 0..n {
                let root = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
                assert!(!root.is_null());
                roots.push(root);
                willow_gc_add_runtime_root_slot(roots.last_mut().unwrap());
            }
            crate::gc::minor_collect_internal();
            let (counters, heap) = telemetry_heap_snapshot();
            // Every survivor is evacuated: copied to survivor space, not pinned.
            assert_eq!(counters.moved_objects, n as u64);
            assert_eq!(counters.promoted_objects, 0);
            assert_eq!(heap.minor_trigger_bytes, (2 * initial).min(CAP) as u64);
            for root in &mut roots {
                willow_gc_remove_runtime_root_slot(root);
            }
            for _ in 0..n {
                assert!(!willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0).is_null());
            }
            crate::gc::minor_collect_internal();
            assert_eq!(
                telemetry_heap_snapshot().1.minor_trigger_bytes,
                ((2 * initial).min(CAP) / 2).max(FLOOR) as u64
            );
            // Explicit empty cycles must not repeatedly inflate the threshold.
            crate::gc::minor_collect_internal();
            assert_eq!(
                telemetry_heap_snapshot().1.minor_trigger_bytes,
                ((2 * initial).min(CAP) / 2).max(FLOOR) as u64
            );
            assert_eq!(runtime().heap.lock().unwrap().minor_collections, 3);
            eprintln!(
                "nursery objects_per_batch={n} minor_cycles=3 policy_updates=3 initial={initial}"
            );
            reset_internal();
        }
    }

    #[test]
    fn nursery_environment_override_and_alloc_stress() {
        use crate::gc::*;
        const CHILD: &str = "WILLOW_TEST_NURSERY_CHILD";
        if std::env::var_os(CHILD).is_none() {
            for stress in ["", "alloc"] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "gc::nursery::tests::nursery_environment_override_and_alloc_stress",
                        "--nocapture",
                    ])
                    .env(CHILD, "1")
                    .env("WILLOW_GC_NURSERY_BYTES", "4194304")
                    .env("WILLOW_GC_STRESS", stress)
                    .env_remove("WILLOW_GC_MEMORY_LIMIT")
                    .env_remove("WILLOW_GC_MEMORY_LIMIT_BYTES")
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            return;
        }
        let _guard = runtime_test_guard();
        reset_internal();
        let mut tls = tlab_state_for_test();
        assert_eq!(telemetry_heap_snapshot().1.minor_trigger_bytes, 4194304);
        let before = runtime().heap.lock().unwrap().major_collections;
        for _ in 0..8 {
            assert!(!willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0).is_null());
        }
        if gc_stress_enabled("alloc") {
            assert_eq!(runtime().heap.lock().unwrap().major_collections - before, 8);
        }
        crate::gc::minor_collect_internal();
        assert_eq!(telemetry_heap_snapshot().1.minor_trigger_bytes, 4194304);
        reset_internal();
        assert_eq!(telemetry_heap_snapshot().1.minor_trigger_bytes, 4194304);
    }

    #[test]
    fn nursery_policy_growth_shrink_dead_band_and_empty_cycles() {
        let policy = Policy::default();
        assert_eq!(policy.initial(None), initial_budget());
        let mut threshold = FLOOR;
        for _ in 0..20 {
            // Survival at the 1/16 boundary grows; 32x of it stays below 2x.
            let next = policy.next(threshold, 1024, 64, None);
            assert_eq!(next, threshold.saturating_mul(2).min(CAP));
            threshold = next;
        }
        assert_eq!(threshold, CAP);
        // Both band edges are exclusive: neither grows nor shrinks.
        assert_eq!(policy.next(threshold, 1024, 63, None), threshold);
        assert_eq!(policy.next(threshold, 1024, 17, None), threshold);
        assert_eq!(policy.next(threshold, 0, 0, None), threshold);
        for _ in 0..20 {
            let next = policy.next(threshold, 1024, 16, None);
            assert_eq!(next, (threshold / 2).max(FLOOR));
            threshold = next;
        }
        assert_eq!(threshold, FLOOR);
    }

    #[test]
    fn nursery_policy_jumps_to_32x_survivors_within_cap_and_limit() {
        // willow-jz15.120: high survival jumps straight to 32x the survivors
        // instead of doubling once per cycle, bounded by the cap and limit.
        let policy = Policy::default();
        let mib = 1024 * 1024;
        assert_eq!(policy.next(FLOOR, mib, mib as u64, None), 32 * mib);
        // Not rounded: 32 x 300 KiB is below the next power of two.
        assert_eq!(policy.next(FLOOR, mib, 300 * 1024, None), 300 * 32 * 1024);
        // Doubling still wins when it exceeds the survivor goal.
        assert_eq!(policy.next(16 * mib, mib, 64 * 1024, None), 32 * mib);
        assert_eq!(policy.next(FLOOR, mib, 4 * mib as u64, None), CAP);
        assert_eq!(policy.next(FLOOR, mib, mib as u64, Some(3 * mib)), 3 * mib);
    }

    #[test]
    fn nursery_policy_override_and_extreme_budgets() {
        for fixed in [None, Some(1), Some(4 * 1024 * 1024), Some(usize::MAX)] {
            let policy = Policy { fixed };
            for limit in [None, Some(1), Some(FLOOR / 2), Some(CAP), Some(usize::MAX)] {
                let initial = policy.initial(limit);
                assert!(initial <= CAP);
                assert!(initial <= limit.unwrap_or(usize::MAX));
                for current in [1, FLOOR, CAP, usize::MAX] {
                    for young in [0, 1, 1024, usize::MAX] {
                        for promoted in [0, 1, u64::MAX] {
                            let next = policy.next(current, young, promoted, limit);
                            assert!(next > 0 && next <= CAP);
                            assert!(next <= limit.unwrap_or(usize::MAX));
                            if fixed.is_some() {
                                assert_eq!(next, initial);
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[test]
fn startup_runway_is_worker_bounded_and_respects_limits() {
    for workers in [0, 1, 2, 4, 8, 64, usize::MAX] {
        let bytes = major_startup_budget_for(workers);
        assert_eq!(bytes, workers.clamp(1, 8) * 4 * 1024 * 1024);
        assert!(bytes <= CAP);
    }
    let policy = Policy::default();
    assert_eq!(policy.initial(Some(1024)), 1024);
    let fixed = Policy { fixed: Some(8192) };
    assert_eq!(fixed.initial(None), 8192);
    assert_eq!(fixed.initial(Some(1024)), 1024);
    assert_eq!(policy.next(8 * FLOOR, 1024, 0, None), 4 * FLOOR);
    assert_eq!(policy.next(FLOOR, 1024, 0, None), FLOOR);
}

#[cfg(test)]
#[test]
fn scheduler_runway_only_initializes_a_cold_heap_once() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    reset_internal();
    {
        let mut state = runtime().heap.lock().unwrap();
        assert_eq!(state.nursery_threshold_bytes, FLOOR);
        self::scheduler_start(&mut state, 8);
        assert_eq!(state.nursery_threshold_bytes, FLOOR);
        assert_eq!(state.threshold_bytes, major_startup_budget_for(8));
        state.nursery_threshold_bytes = FLOOR;
        self::scheduler_start(&mut state, 8);
        assert_eq!(state.nursery_threshold_bytes, FLOOR);
    }
    for major in [false, true] {
        reset_internal();
        let mut state = runtime().heap.lock().unwrap();
        if major {
            state.major_collections = 1;
        } else {
            state.minor_collections = 1;
        }
        self::scheduler_start(&mut state, 8);
        assert_eq!(state.nursery_threshold_bytes, FLOOR);
        assert_eq!(state.threshold_bytes, 1024 * 1024);
    }
    reset_internal();
    let (_, cycle) = root_handshake::begin().unwrap();
    {
        let mut state = runtime().heap.lock().unwrap();
        self::scheduler_start(&mut state, 8);
        assert_eq!(state.nursery_threshold_bytes, FLOOR);
        assert_eq!(state.threshold_bytes, 1024 * 1024);
        marking::set_concurrent_cycle(&mut state, None);
    }
    GC_MARK_PHASE.store(0, Ordering::Release);
    cycle.queue.end_epoch();
    reset_internal();
    {
        let mut state = runtime().heap.lock().unwrap();
        state.nursery_policy = Policy { fixed: Some(8192) };
        state.memory_limit_bytes = Some(4096);
        state.nursery_threshold_bytes = state.nursery_policy.initial(state.memory_limit_bytes);
        self::scheduler_start(&mut state, 8);
        assert_eq!(state.nursery_threshold_bytes, 4096);
    }
    reset_internal();
}

#[cfg(test)]
#[test]
fn scheduler_capacity_preserves_compact_nursery_and_overrides() {
    use crate::gc::*;
    let _guard = runtime_test_guard();
    for workers in [0, 1, 2, 8, 64, usize::MAX] {
        for fixed in [None, Some(8192), Some(4 * 1024 * 1024)] {
            for limit in [None, Some(4096)] {
                reset_internal();
                let mut state = runtime().heap.lock().unwrap();
                state.nursery_policy = Policy { fixed };
                state.memory_limit_bytes = limit;
                let initial = state.nursery_policy.initial(limit);
                state.nursery_threshold_bytes = initial;
                self::scheduler_start(&mut state, workers);
                assert_eq!(state.nursery_threshold_bytes, initial);
                // Startup must not disable the measured-survival feedback.
                assert_eq!(
                    state.nursery_policy.next(initial, 1024, 0, limit),
                    state
                        .nursery_policy
                        .bound(fixed.unwrap_or((initial / 2).max(FLOOR)), limit)
                );
            }
        }
    }
    reset_internal();
}
