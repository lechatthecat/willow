//! Soft memory pressure policy. This is separate from the legacy hard
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
    /// Managed reservations plus native requested-capacity estimates, in bytes.
    /// A pressure proxy, NOT measured OS commit or RSS.
    pub pressure_bytes: u64,
    pub allocated_total: u64,
    /// Only supply a value with compatible process/managed commit scopes.
    /// Process RSS minus managed reservations is NOT such a measurement.
    pub non_heap_commit: Option<u64>,
    /// Independent whole-process RSS sample; never subtract reservations from RSS.
    pub process_resident: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reason {
    Unlimited,
    BelowLimit,
    HeapGoal,
    StoragePressure,
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
    last_completed_resident: Option<u64>,
    ineffective: u8,
    effective: u8,
    relief: bool,
    resident: Option<u64>,
    sampled_at: Option<std::time::Instant>,
}

impl Controller {
    pub(super) fn enabled(&self) -> bool {
        self.limit != 0
    }
    pub(super) fn limit(&self) -> Option<u64> {
        self.enabled().then_some(self.limit)
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
        // Updates (including disable/re-enable) must not let an application
        // repeatedly erase evidence of ineffective collections. Two effective
        // cycles, not a configuration write, leave relief.
        self.limit = limit;
        previous
    }

    pub(super) fn sample_process(&mut self) {
        if self.enabled()
            && self
                .sampled_at
                .is_none_or(|at| at.elapsed().as_millis() >= 10)
        {
            self.resident = crate::gc_telemetry::resident_bytes();
            self.sampled_at = Some(std::time::Instant::now());
        }
    }

    pub(super) fn resident(&self) -> Option<u64> {
        self.enabled().then_some(self.resident).flatten()
    }

    fn pressure(input: Inputs) -> u64 {
        // These scopes overlap: compare each with the target independently.
        // Taking their maximum does not pretend either is OS commitment.
        input
            .pressure_bytes
            .saturating_add(input.non_heap_commit.unwrap_or(0))
            .max(input.process_resident.unwrap_or(0))
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
        let pressure = Self::pressure(input) >= self.limit;
        let overshoot = Self::pressure(input).saturating_sub(self.limit);
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
            Reason::StoragePressure
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
            self.last_completed_resident = None;
            return;
        }
        let pressure = Self::pressure(input) >= self.limit.saturating_sub(self.limit / 8);
        let reclaimed = before.saturating_sub(input.occupied);
        // Heap reclamation is not evidence that independent process pressure
        // improved. A reused 10ms RSS sample also supplies no such evidence:
        // conservatively grant relief rather than collect repeatedly while
        // waiting for a refresh. This may temporarily overshoot the soft target.
        let resident_stalled = input
            .process_resident
            .zip(self.last_completed_resident)
            .is_some_and(|(current, previous)| {
                current >= self.limit.saturating_sub(self.limit / 8)
                    && previous.saturating_sub(current) <= previous / 32
            });
        self.last_completed_resident = input.process_resident;
        let irreducible = input
            .live
            .saturating_add(input.non_heap_commit.unwrap_or(0))
            >= self.limit;
        let ineffective = pressure && (reclaimed <= before / 32 || irreducible || resident_stalled);
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
            pressure_bytes: 1000,
            allocated_total: 800,
            non_heap_commit: None,
            process_resident: None,
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
        assert_eq!(process.reason, Reason::StoragePressure);
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
            pressure_bytes: 1024,
            allocated_total: 1000,
            non_heap_commit: None,
            process_resident: None,
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
        input.pressure_bytes = 0;
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
                                pressure_bytes: commit,
                                allocated_total: occupied,
                                non_heap_commit: non_heap,
                                process_resident: None,
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
    fn updates_cannot_erase_relief_or_allocation_history() {
        for rounds in [2, 8, 64, 1024] {
            let mut controller = Controller::default();
            controller.set_limit(1);
            let mut input = Inputs {
                unlimited_goal: 1024,
                live: 1024,
                occupied: 1024,
                pressure_bytes: 1024,
                allocated_total: 1024,
                ..Inputs::default()
            };
            let mut collections = 0;
            for n in 0..rounds {
                controller.set_limit(0);
                controller.set_limit(1 + n % 2);
                input.allocated_total += 1;
                if controller.decide(input).collect {
                    collections += 1;
                    controller.completed(input.occupied, input);
                }
            }
            assert_eq!(collections, 2, "rounds={rounds}");
            assert_eq!(controller.decide(input).reason, Reason::Relief);
        }
    }

    #[test]
    fn resident_pressure_is_independent_and_invalid_is_not_zero_commit() {
        let mut controller = Controller::default();
        controller.set_limit(1000);
        let input = Inputs {
            unlimited_goal: 2000,
            live: 100,
            occupied: 200,
            pressure_bytes: 600,
            allocated_total: 200,
            ..Inputs::default()
        };
        for (resident, collect, overshoot) in [
            (None, false, 0),
            (Some(600), false, 0),
            (Some(1000), true, 0),
            (Some(1500), true, 500),
            (Some(u64::MAX), true, u64::MAX - 1000),
        ] {
            let decision = controller.decide(Inputs {
                process_resident: resident,
                ..input
            });
            assert_eq!(decision.collect, collect);
            assert_eq!(decision.overshoot, overshoot);
            assert_eq!(decision.goal, 1000); // no RSS-reservation subtraction
        }
        let input = Inputs {
            process_resident: Some(1500),
            ..input
        };
        controller.completed(200, input);
        controller.completed(200, input);
        assert_eq!(controller.decide(input).reason, Reason::Relief);
        assert!(!controller.decide(input).collect);
        assert!(
            controller
                .decide(Inputs {
                    allocated_total: 200 + MIN_RUNWAY,
                    ..input
                })
                .collect
        );
        let recovered = Inputs {
            process_resident: None,
            pressure_bytes: 0,
            ..input
        };
        controller.completed(200, recovered);
        assert!(controller.relief);
        controller.completed(200, recovered);
        assert!(!controller.relief);
    }

    #[test]
    fn process_sampling_is_disabled_or_cached_between_slow_paths() {
        let mut controller = Controller::default();
        controller.sample_process();
        assert!(controller.sampled_at.is_none());
        controller.set_limit(1);
        controller.sample_process();
        let sampled = controller.sampled_at;
        // A future instant makes this deterministic even on a slow test host.
        controller.sampled_at = Some(std::time::Instant::now() + std::time::Duration::from_secs(1));
        let future = controller.sampled_at;
        controller.set_limit(2);
        controller.sample_process();
        assert_eq!(controller.sampled_at, future);
        controller.sampled_at = None;
        controller.sample_process();
        assert_ne!(controller.sampled_at, future);
        assert!(sampled.is_some());
        controller.set_limit(0);
        assert_eq!(controller.resident(), None);
    }

    #[test]
    fn process_pressure_reaches_pacer_but_relief_does_not() {
        use super::super::*;
        let _guard = runtime_test_guard();
        reset_internal();
        assert!(!willow_alloc(8).is_null());
        let mut state = runtime().heap.lock().unwrap();
        state.soft_memory.set_limit(1 << 30);
        state.soft_memory.resident = Some(2 << 30);
        assert!(pacer_inputs(&state).feedback.pressure);
        let input = memory_inputs(&state);
        state.soft_memory.completed(input.occupied, input);
        state.soft_memory.completed(input.occupied, input);
        assert!(!pacer_inputs(&state).feedback.pressure);
        drop(state);
        reset_internal();
    }

    #[test]
    fn stagnant_resident_with_heap_reclamation_has_bounded_collection_counts() {
        for rounds in [8u64, 64, 1024] {
            let mut controller = Controller::default();
            controller.set_limit(1 << 20);
            let mut input = Inputs {
                unlimited_goal: 4 << 20,
                live: 4096,
                occupied: 8192,
                pressure_bytes: 256 << 10,
                allocated_total: 8192,
                process_resident: Some(2 << 20),
                ..Inputs::default()
            };
            let mut collections = 0;
            for _ in 0..rounds {
                if controller.decide(input).collect {
                    collections += 1;
                    controller.completed(
                        8192,
                        Inputs {
                            occupied: 4096,
                            ..input
                        },
                    );
                }
                input.allocated_total += 4096;
            }
            assert_eq!(collections, 3 + (rounds - 3) / 64);
            assert_eq!(controller.decide(input).reason, Reason::Relief);
        }
    }

    #[test]
    fn resident_history_handles_recovery_invalidity_and_irreducible_live() {
        let mut controller = Controller::default();
        controller.set_limit(1 << 20);
        let input = Inputs {
            unlimited_goal: 4 << 20,
            live: 4096,
            occupied: 4096,
            pressure_bytes: 256 << 10,
            allocated_total: 8192,
            process_resident: Some(2 << 20),
            ..Inputs::default()
        };
        for _ in 0..3 {
            controller.completed(8192, input);
        }
        assert!(controller.relief);
        // Two effective cycle observations recover even when RSS is cached.
        let recovered = Inputs {
            process_resident: Some(512 << 10),
            ..input
        };
        controller.completed(8192, recovered);
        assert!(controller.relief);
        controller.completed(8192, recovered);
        assert!(!controller.relief);
        // Significant resident reduction above target is progress too.
        controller.completed(
            8192,
            Inputs {
                process_resident: Some(4 << 20),
                ..input
            },
        );
        controller.completed(8192, input);
        assert_eq!(controller.ineffective, 0);
        // Missing measurements break comparison history, never invent a zero.
        controller.completed(
            8192,
            Inputs {
                process_resident: None,
                ..input
            },
        );
        assert_eq!(controller.last_completed_resident, None);
        controller.completed(8192, input);
        assert_eq!(controller.ineffective, 0);
        controller.set_limit(0);
        controller.completed(8192, input);
        assert_eq!(controller.last_completed_resident, None);
        controller.set_limit(1);
        for _ in 0..2 {
            controller.completed(
                8192,
                Inputs {
                    process_resident: None,
                    ..input
                },
            );
        }
        assert!(controller.relief); // live alone is irreducible
    }

    #[test]
    fn cached_process_pressure_relief_preserves_live_runtime_root() {
        use super::super::*;
        let _guard = runtime_test_guard();
        reset_internal();
        let root = willow_alloc((4096 - GC_HEADER_SIZE) as i64);
        willow_gc_add_runtime_root(root);
        unsafe {
            root.cast::<u64>().write(42);
        }
        willow_gc_set_memory_limit(1 << 20);
        {
            let mut state = runtime().heap.lock().unwrap();
            state.soft_memory.resident = Some(2 << 20);
            state.soft_memory.sampled_at =
                Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
        }
        for _ in 0..3 {
            {
                let mut state = runtime().heap.lock().unwrap();
                allocate_old_region_object_locked(&mut state, 0, 0, 4096 - GC_HEADER_SIZE, 0, true)
                    .unwrap();
                assert_eq!(state.allocated_bytes, 8192);
            }
            collect_internal();
            assert_eq!(runtime().heap.lock().unwrap().allocated_bytes, 4096);
        }
        let before = runtime().heap.lock().unwrap().major_collections;
        assert!(runtime().heap.lock().unwrap().soft_memory.relief);
        for _ in 0..128 {
            assert!(!willow_alloc(8).is_null());
        }
        assert_eq!(runtime().heap.lock().unwrap().major_collections, before);
        assert_eq!(unsafe { root.cast::<u64>().read() }, 42);
        runtime().heap.lock().unwrap().soft_memory.resident = Some(512 << 10);
        collect_internal();
        collect_internal();
        assert!(!runtime().heap.lock().unwrap().soft_memory.relief);
        willow_gc_remove_runtime_root(root);
        collect_internal();
        assert_eq!(runtime().heap.lock().unwrap().allocated_bytes, 0);
        reset_internal();
    }

    #[test]
    fn environment_limit_parsing_in_fresh_process() {
        const CHILD: &str = "WILLOW_SOFT_LIMIT_ENV_TEST";
        if let Ok(expected) = std::env::var(CHILD) {
            assert_eq!(
                Controller::from_env().limit().unwrap_or(0),
                expected.parse::<u64>().unwrap()
            );
            return;
        }
        for value in [None, Some("0"), Some("1"), Some("18446744073709551615")] {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "gc::memory_control::tests::environment_limit_parsing_in_fresh_process",
                ])
                .env(CHILD, value.unwrap_or("0"))
                .env_remove("WILLOW_GC_MEMORY_LIMIT_BYTES");
            if let Some(value) = value {
                command.env("WILLOW_GC_MEMORY_LIMIT_BYTES", value);
            }
            let result = command.output().unwrap();
            assert!(result.status.success(), "{result:?}");
        }
    }

    #[test]
    fn concurrent_updates_preserve_roots_and_relief() {
        use super::super::*;
        let _guard = runtime_test_guard();
        reset_internal();
        let root = willow_alloc(8);
        willow_gc_add_runtime_root(root);
        unsafe {
            root.cast::<u64>().write(0x1234);
        }
        willow_gc_set_memory_limit(1);
        collect_internal();
        collect_internal();
        let before = runtime().heap.lock().unwrap().major_collections;
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for n in 0..100 {
                        willow_gc_set_memory_limit(1 + n % 2);
                    }
                });
            }
            for _ in 0..100 {
                assert!(!willow_alloc(8).is_null());
            }
        });
        willow_gc_set_memory_limit(1);
        assert_eq!(runtime().heap.lock().unwrap().major_collections, before);
        assert_eq!(unsafe { root.cast::<u64>().read() }, 0x1234);
        willow_gc_remove_runtime_root(root);
        collect_internal();
        assert_eq!(runtime().heap.lock().unwrap().allocated_bytes, 0);
        reset_internal();
    }

    #[test]
    fn allocator_failure_gets_exactly_one_recovery_and_one_retry() {
        use super::super::*;
        let _guard = runtime_test_guard();
        for nursery in [false, true] {
            reset_internal();
            willow_gc_set_memory_limit(u64::MAX);
            STORAGE_FAILURES.set(1);
            STORAGE_ATTEMPTS.set(0);
            let mut tls = tlab_state_for_test();
            let result = if nursery {
                willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0)
            } else {
                willow_alloc(GC_LARGE_OBJECT_THRESHOLD as i64)
            };
            assert!(!result.is_null());
            assert_eq!(STORAGE_ATTEMPTS.get(), 2);
            assert_eq!(STORAGE_FAILURES.get(), 0);
            assert_eq!(runtime().heap.lock().unwrap().major_collections, 1);
            reset_internal(); // TLS is still alive here.
        }
    }
}
