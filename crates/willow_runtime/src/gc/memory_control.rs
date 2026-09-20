//! Soft managed-memory pressure policy. This is separate from the legacy hard
//! reservation cap. All arithmetic and hysteresis are deterministic; callers
//! supply accounting and execute collection outside the heap mutex.

const MIN_RUNWAY: u64 = 256 * 1024;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Inputs {
    pub unlimited_goal: u64,
    /// Conservative post-major occupied bytes, including allocations that
    /// survived the snapshot. Never infer this from a minor cycle.
    pub live: u64,
    pub occupied: u64,
    pub committed: u64,
    pub allocated_total: u64,
    /// Only supply a value with compatible process/managed commit scopes.
    /// Process RSS minus managed reservations is NOT such a measurement.
    pub non_heap_commit: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reason {
    Unlimited,
    BelowLimit,
    HeapGoal,
    CommitPressure,
    Relief,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Decision {
    pub goal: u64,
    pub trigger: u64,
    pub overshoot: u64,
    pub collect: bool,
    pub reason: Reason,
}

#[derive(Debug, Default)]
pub(super) struct Controller {
    limit: u64,
    last_collection_allocation: u64,
    ineffective: u8,
    effective: u8,
    relief: bool,
}

impl Controller {
    pub(super) fn enabled(&self) -> bool {
        self.limit != 0
    }
    pub(super) fn from_env() -> Self {
        let limit = std::env::var("WILLOW_GC_MEMORY_LIMIT_BYTES")
            .ok()
            .map(|value| {
                value
                    .parse::<u64>()
                    .expect("WILLOW_GC_MEMORY_LIMIT_BYTES must be a nonnegative byte count")
            })
            .unwrap_or(0);
        Self {
            limit,
            ..Self::default()
        }
    }

    pub(super) fn set_limit(&mut self, limit: u64) -> u64 {
        let previous = self.limit;
        // An idempotent setter must not continually defeat relief hysteresis.
        if previous != limit {
            self.limit = limit;
            self.ineffective = 0;
            self.effective = 0;
            self.relief = false;
        }
        previous
    }

    pub(super) fn decide(&self, input: Inputs) -> Decision {
        let unlimited = input.unlimited_goal.max(input.live);
        if self.limit == 0 {
            return Decision {
                goal: unlimited,
                trigger: unlimited,
                overshoot: 0,
                collect: input.occupied >= unlimited,
                reason: Reason::Unlimited,
            };
        }
        let non_heap = input.non_heap_commit.unwrap_or(0);
        let available = self.limit.saturating_sub(non_heap);
        let goal = unlimited.min(available).max(input.live);
        // Reserve an eighth of the remaining runway for concurrent work.
        let trigger = goal.saturating_sub(goal.saturating_sub(input.live) / 8);
        let pressure = input.committed.saturating_add(non_heap) >= self.limit;
        let overshoot = input
            .committed
            .saturating_add(non_heap)
            .saturating_sub(self.limit);
        let allocated = input
            .allocated_total
            .saturating_sub(self.last_collection_allocation);
        // An ineffective cycle must allow allocation progress. This is an
        // explicit soft-limit overshoot, never a hidden allocation stop.
        let runway = if self.relief {
            MIN_RUNWAY.max(input.live / 8)
        } else {
            1
        };
        let due = input.occupied >= trigger || pressure;
        let collect = due && allocated >= runway;
        let reason = if self.relief && due {
            Reason::Relief
        } else if pressure {
            Reason::CommitPressure
        } else if input.occupied >= trigger {
            Reason::HeapGoal
        } else {
            Reason::BelowLimit
        };
        Decision {
            goal,
            trigger,
            overshoot,
            collect,
            reason,
        }
    }

    pub(super) fn completed(&mut self, before: u64, input: Inputs) {
        self.last_collection_allocation = input.allocated_total;
        if self.limit == 0 {
            return;
        }
        let pressure = input
            .committed
            .saturating_add(input.non_heap_commit.unwrap_or(0))
            >= self.limit.saturating_sub(self.limit / 8);
        let reclaimed = before.saturating_sub(input.occupied);
        let ineffective = pressure && reclaimed <= before / 32;
        if ineffective {
            self.effective = 0;
            self.ineffective = self.ineffective.saturating_add(1);
            if self.ineffective >= 2 {
                self.relief = true;
            }
        } else {
            self.ineffective = 0;
            self.effective = self.effective.saturating_add(1);
            // Two good observations prevent alternating workloads from
            // repeatedly switching relief on and off.
            if self.effective >= 2 {
                self.relief = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_and_compatible_commit_scopes() {
        let mut controller = Controller::default();
        let input = Inputs {
            unlimited_goal: 2000,
            live: 500,
            occupied: 800,
            committed: 1000,
            allocated_total: 800,
            non_heap_commit: None,
        };
        let unlimited = controller.decide(input);
        assert_eq!(unlimited.goal, 2000);
        assert!(!unlimited.collect);
        assert_eq!(controller.set_limit(1800), 0);
        let managed = controller.decide(input);
        assert_eq!(managed.goal, 1800);
        assert!(!managed.collect);
        let process = controller.decide(Inputs {
            non_heap_commit: Some(1000),
            ..input
        });
        assert_eq!(process.goal, 800);
        assert_eq!(process.overshoot, 200);
        assert_eq!(process.reason, Reason::CommitPressure);
        assert!(process.collect);
    }

    #[test]
    fn live_floor_saturates_and_relief_allows_progress() {
        let mut controller = Controller::default();
        controller.set_limit(1);
        let mut input = Inputs {
            unlimited_goal: 100,
            live: 1000,
            occupied: 1000,
            committed: 1024,
            allocated_total: 1000,
            non_heap_commit: None,
        };
        assert_eq!(controller.decide(input).goal, input.live);
        for _ in 0..2 {
            controller.completed(1000, input);
            assert!(!controller.decide(input).collect);
            input.allocated_total += 1;
        }
        assert_eq!(controller.decide(input).reason, Reason::Relief);
        assert!(!controller.decide(input).collect);
        controller.set_limit(1); // idempotent does not clear relief
        input.allocated_total += MIN_RUNWAY;
        assert!(controller.decide(input).collect);
        input.occupied = 0;
        input.committed = 0;
        controller.completed(1000, input);
        assert!(controller.relief);
        controller.completed(1000, input);
        assert!(!controller.relief);
        assert_eq!(controller.set_limit(0), 1);
        assert_eq!(controller.decide(input).reason, Reason::Unlimited);
    }

    #[test]
    fn extreme_accounting_never_wraps_or_lowers_goal_below_live() {
        let values = [0, 1, 7, 1024, u64::MAX / 2, u64::MAX];
        let mut cases = 0;
        for limit in values {
            let controller = Controller {
                limit,
                ..Controller::default()
            };
            for live in values {
                for occupied in values {
                    for commit in values {
                        for non_heap in [None, Some(0), Some(u64::MAX)] {
                            let input = Inputs {
                                unlimited_goal: occupied,
                                live,
                                occupied,
                                committed: commit,
                                allocated_total: occupied,
                                non_heap_commit: non_heap,
                            };
                            let decision = controller.decide(input);
                            assert!(decision.goal >= live);
                            assert!(decision.trigger >= live);
                            assert!(decision.trigger <= decision.goal);
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 3888);
    }

    #[test]
    fn runtime_soft_pressure_collects_without_changing_hard_cap() {
        use super::super::*;
        let _guard = runtime_test_guard();
        reset_internal();
        runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
        let root = willow_alloc(1024);
        assert!(!root.is_null());
        willow_gc_add_runtime_root(root);
        assert_eq!(willow_gc_set_memory_limit(1), 0);
        assert!(allocation_should_collect());
        collect_internal();
        let state = runtime().heap.lock().unwrap();
        assert!(state.memory_limit_bytes.is_none());
        let decision = state.soft_memory.decide(memory_inputs(&state));
        assert!(decision.goal >= 1024);
        assert!(decision.overshoot > 0);
        assert!(!decision.collect);
        drop(state);
        // Repeated ineffective collections enter relief. Allocations may
        // exceed a soft limit smaller than the live set and still progress.
        let extra = willow_alloc(8);
        assert!(!extra.is_null());
        willow_gc_add_runtime_root(extra);
        collect_internal();
        let before = runtime().heap.lock().unwrap().major_collections;
        for _ in 0..128 {
            assert!(!willow_alloc(8).is_null());
        }
        assert_eq!(runtime().heap.lock().unwrap().major_collections, before);
        assert_eq!(willow_gc_set_memory_limit(0), 1);
        willow_gc_remove_runtime_root(extra);
        willow_gc_remove_runtime_root(root);
        reset_internal();
    }

    #[test]
    fn allocator_failure_gets_exactly_one_recovery_and_one_retry() {
        use super::super::*;
        let _guard = runtime_test_guard();
        for nursery in [false, true] {
            for failures in [1, 2] {
                reset_internal();
                willow_gc_set_memory_limit(u64::MAX);
                STORAGE_FAILURES.set(failures);
                STORAGE_ATTEMPTS.set(0);
                let mut tls = tlab_state_for_test();
                let result = if nursery {
                    willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0)
                } else {
                    willow_alloc(GC_LARGE_OBJECT_THRESHOLD as i64)
                };
                assert_eq!(result.is_null(), failures == 2);
                assert_eq!(STORAGE_ATTEMPTS.get(), 2);
                assert_eq!(STORAGE_FAILURES.get(), 0);
                assert_eq!(runtime().heap.lock().unwrap().major_collections, 1);
                reset_internal(); // TLS is still alive here.
            }
        }
    }
}
