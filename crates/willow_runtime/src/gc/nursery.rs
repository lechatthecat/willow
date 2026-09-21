//! Constant-work minor-trigger policy; consumes counters from the completed cycle.

const FLOOR: usize = 256 * 1024;
const CAP: usize = 32 * 1024 * 1024;

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
        self.bound(self.fixed.unwrap_or(FLOOR), memory_limit)
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
        // Empty cycles carry no survival evidence. A dead band avoids
        // oscillation around a single survival-ratio boundary. Division keeps
        // the comparisons safe even for extreme accounting values.
        let target = if young_before == 0 {
            current
        } else if promoted >= (young_before - young_before / 2) as u64 {
            current.saturating_mul(2)
        } else if promoted <= (young_before / 4) as u64 {
            current / 2
        } else {
            current
        };
        self.bound(target.max(FLOOR), memory_limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nursery_runtime_survival_updates_live_telemetry() {
        use crate::gc::*;
        let _guard = runtime_test_guard();
        for n in [32, 128, 512] {
            reset_internal();
            let mut tls = tlab_state_for_test();
            let mut roots = Vec::new();
            assert_eq!(
                telemetry_heap_snapshot().1.minor_trigger_bytes,
                FLOOR as u64
            );
            for _ in 0..n {
                let root = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
                assert!(!root.is_null());
                willow_gc_add_runtime_root(root);
                roots.push(root);
            }
            willow_gc_minor_collect();
            let (counters, heap) = telemetry_heap_snapshot();
            assert_eq!(counters.promoted_objects, n as u64);
            assert_eq!(heap.minor_trigger_bytes, (2 * FLOOR) as u64);
            for root in roots {
                willow_gc_remove_runtime_root(root);
            }
            for _ in 0..n {
                assert!(!willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0).is_null());
            }
            willow_gc_minor_collect();
            assert_eq!(
                telemetry_heap_snapshot().1.minor_trigger_bytes,
                FLOOR as u64
            );
            // Explicit empty cycles must not repeatedly inflate the threshold.
            willow_gc_minor_collect();
            assert_eq!(
                telemetry_heap_snapshot().1.minor_trigger_bytes,
                FLOOR as u64
            );
            assert_eq!(runtime().heap.lock().unwrap().minor_collections, 3);
            eprintln!(
                "nursery objects_per_batch={n} minor_cycles=3 policy_updates=3 thresholds=262144,524288,262144,262144"
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
        willow_gc_minor_collect();
        assert_eq!(telemetry_heap_snapshot().1.minor_trigger_bytes, 4194304);
        reset_internal();
        assert_eq!(telemetry_heap_snapshot().1.minor_trigger_bytes, 4194304);
    }

    #[test]
    fn nursery_policy_growth_shrink_dead_band_and_empty_cycles() {
        let policy = Policy::default();
        assert_eq!(policy.initial(None), FLOOR);
        let mut threshold = FLOOR;
        for _ in 0..20 {
            let next = policy.next(threshold, 1024, 512, None);
            assert_eq!(next, threshold.saturating_mul(2).min(CAP));
            threshold = next;
        }
        assert_eq!(threshold, CAP);
        assert_eq!(policy.next(threshold, 1024, 400, None), threshold);
        assert_eq!(policy.next(threshold, 0, 0, None), threshold);
        for _ in 0..20 {
            let next = policy.next(threshold, 1024, 256, None);
            assert_eq!(next, (threshold / 2).max(FLOOR));
            threshold = next;
        }
        assert_eq!(threshold, FLOOR);
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
