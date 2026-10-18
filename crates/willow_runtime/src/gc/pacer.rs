//! Integer-only feedback for the heap runway. Sampling and policy are separate;
//! unavailable CPU measurements never masquerade as zero-cost marking.
const MIN_GOAL: u64 = 1024 * 1024;
// Coalesce sub-millisecond allocation bursts instead of feeding refill-scale
// clock/scheduling noise into the immediate-rise runway estimator.
const ALLOCATION_SAMPLE_NS: u64 = 1_000_000;

/// The sole mutable writer of the heap goal and pacing trigger. Allocation
/// accounting never grants runway; reservation budgets are enforced separately.
fn publish(state: &mut super::GcState, goal: u64, trigger: u64, reason: &str) {
    let goal = goal.min(usize::MAX as u64) as usize;
    let trigger = trigger.min(goal as u64);
    // Inputs (including live bytes) may change even if the thresholds do not.
    super::invalidate_external_headroom();
    if state.threshold_bytes == goal && state.pacer_trigger == trigger {
        return;
    }
    if std::env::var("WILLOW_GC_LOG").is_ok() {
        eprintln!(
            "[gc] threshold goal={} trigger={} previous_goal={} previous_trigger={} reason={}",
            goal, trigger, state.threshold_bytes, state.pacer_trigger, reason
        );
    }
    state.threshold_bytes = goal;
    state.pacer_trigger = trigger;
}

pub(super) fn reset_threshold(state: &mut super::GcState) {
    publish(state, MIN_GOAL, MIN_GOAL, "reset");
}

pub(super) fn scheduler_start(state: &mut super::GcState, budget: u64) {
    publish(state, budget, budget, "scheduler_start");
}

pub(super) fn sample_threshold(state: &mut super::GcState) {
    let input = super::pacer_inputs(state);
    let decision = state.pacer.decision(input);
    let goal = state.threshold_bytes as u64;
    let trigger = goal
        .saturating_sub(decision.runway)
        .max(minimum_trigger(input.live))
        .min(goal);
    publish(state, goal, trigger, "allocation_sample");
}

pub(super) fn complete_threshold(state: &mut super::GcState, previous_goal: u64) {
    let input = Inputs {
        previous_goal,
        ..super::pacer_inputs(state)
    };
    let (goal, trigger, reason) = if state.pacer.enabled() {
        let decision = state.pacer.decision(input);
        (decision.goal, decision.trigger, "cycle_paced")
    } else {
        let goal = input.live.saturating_mul(2).max(MIN_GOAL);
        (goal, goal, "cycle_fixed")
    };
    publish(state, goal, trigger, reason);
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Inputs {
    pub live: u64,
    pub previous_goal: u64,
    pub expected_work: u64,
    pub allocation_per_second: u64,
    pub mark_per_cpu_second: Option<u64>,
    pub cpu_capacity: usize,
    pub active: bool,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Decision {
    pub goal: u64,
    pub trigger: u64,
    pub runway: u64,
}

// A completed full-heap scan must buy allocation progress proportional to
// its live work. A fixed 256 KiB floor could repeatedly scan an arbitrarily
// large unchanged live set when a noisy runway estimate exceeded the bounded
// goal. The hard/soft reservation controllers still enforce their own limits.
fn minimum_trigger(live: u64) -> u64 {
    live.saturating_add((live / 4).max(MIN_GOAL / 4))
}

fn mul_div_ceil(a: u64, b: u64, divisor: u128) -> u64 {
    (u128::from(a) * u128::from(b))
        .div_ceil(divisor.max(1))
        .min(u64::MAX as u128) as u64
}

pub(super) fn decide(input: Inputs) -> Decision {
    let floor = input.live.saturating_mul(2).max(MIN_GOAL);
    // Conservative cold/invalid-rate runway: half the fixed fallback goal.
    let runway = match input.mark_per_cpu_second.filter(|&rate| rate != 0) {
        Some(rate) => {
            // Preserve the full denominator: saturating it at u64::MAX
            // overestimates required runway on multi-CPU inputs.
            let capacity = u128::from(rate) * input.cpu_capacity.max(1) as u128;
            mul_div_ceil(input.expected_work, input.allocation_per_second, capacity)
                .saturating_mul(2)
        }
        None => floor / 2,
    };
    let desired = floor.max(input.live.saturating_add(runway.saturating_mul(2)));
    // Limit each inter-cycle adjustment to a factor of two. Live is always a
    // lower bound. Never shrink a goal while that cycle is active.
    let previous = input.previous_goal.max(MIN_GOAL);
    let lower = if input.active { previous } else { previous / 2 };
    let goal = desired
        .clamp(lower, previous.saturating_mul(2))
        .max(input.live);
    let trigger = goal
        .saturating_sub(runway)
        .max(minimum_trigger(input.live).min(goal));
    Decision {
        goal,
        trigger,
        runway,
    }
}

#[derive(Debug)]
pub(super) struct Sampler {
    enabled: bool,
    last: Option<(u64, u64)>,
    allocation_rate: u64,
    mark_rate: Option<u64>,
}

impl Default for Sampler {
    fn default() -> Self {
        Self {
            enabled: std::env::var("WILLOW_GC_PACER").as_deref() != Ok("0"),
            last: None,
            allocation_rate: 0,
            mark_rate: None,
        }
    }
}

fn smooth(old: u64, new: u64) -> u64 {
    ((u128::from(old) * 3 + u128::from(new)) / 4) as u64
}

impl Sampler {
    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Isolate a minor-GC policy test from concurrent major-GC pacing.
    #[cfg(test)]
    pub(super) fn disable_for_test(&mut self) {
        self.set_enabled_for_test(false);
    }

    #[cfg(test)]
    pub(super) fn set_enabled_for_test(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub(super) fn sample_due(&self, bytes: u64) -> bool {
        self.enabled
            && self
                .last
                .is_none_or(|(_, previous)| bytes.saturating_sub(previous) >= 65536)
    }

    /// Collection time is not mutator allocation time. Resume the sampling
    /// window without treating a long GC pause as a fall in allocation demand.
    /// Occupancy and hard reservation accounting remain independent.
    pub(super) fn resume_allocation(&mut self, ns: u64, bytes: u64) {
        self.last = Some((ns, bytes));
    }

    pub(super) fn allocation(&mut self, ns: u64, bytes: u64) {
        let Some((old_ns, old_bytes)) = self.last else {
            self.last = Some((ns, bytes));
            return;
        };
        if ns > old_ns && ns - old_ns < ALLOCATION_SAMPLE_NS && bytes >= old_bytes {
            return;
        }
        self.last = Some((ns, bytes));
        let Some(elapsed) = ns.checked_sub(old_ns).filter(|&elapsed| elapsed != 0) else {
            self.allocation_rate = 0;
            self.mark_rate = None;
            return;
        };
        let Some(allocated) = bytes.checked_sub(old_bytes) else {
            self.allocation_rate = 0;
            self.mark_rate = None;
            return;
        };
        let rate = mul_div_ceil(allocated, 1_000_000_000, u128::from(elapsed));
        if elapsed > 5_000_000_000 {
            self.mark_rate = None;
        }
        self.allocation_rate = if self.allocation_rate == 0 || elapsed > 5_000_000_000 {
            rate
        } else {
            // Rising pressure reacts immediately; decay is smoothed.
            rate.max(smooth(self.allocation_rate, rate))
        };
    }

    pub(super) fn mark(&mut self, work: u64, cpu_ns: Option<u64>) {
        self.mark_rate = match cpu_ns.filter(|&ns| ns != 0).filter(|_| work != 0) {
            Some(ns) => {
                let rate = mul_div_ceil(work, 1_000_000_000, u128::from(ns));
                // Slowdowns are immediate; improvement must persist.
                Some(
                    self.mark_rate
                        .map_or(rate, |old| rate.min(smooth(old, rate))),
                )
            }
            None => None,
        };
    }

    pub(super) fn decision(&self, mut input: Inputs) -> Decision {
        input.allocation_per_second = self.allocation_rate;
        input.mark_per_cpu_second = self.mark_rate;
        decide(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn collection_pauses_do_not_decay_mutator_allocation_rate() {
        let mut sampler = Sampler::default();
        sampler.allocation(0, 0);
        sampler.allocation(1_000_000, 1_000_000);
        assert_eq!(sampler.allocation_rate, 1_000_000_000);
        for pause in [1_000_000u64, 1_000_000_000, 10_000_000_000] {
            let (time, bytes) = sampler.last.unwrap();
            sampler.resume_allocation(time + pause, bytes);
            assert_eq!(sampler.allocation_rate, 1_000_000_000);
            sampler.allocation(time + pause + 1_000_000, bytes + 1_000_000);
            assert_eq!(sampler.allocation_rate, 1_000_000_000);
        }
    }

    fn input() -> Inputs {
        Inputs {
            live: MIN_GOAL,
            previous_goal: 4 * MIN_GOAL,
            expected_work: 4 * MIN_GOAL,
            allocation_per_second: MIN_GOAL,
            mark_per_cpu_second: Some(4 * MIN_GOAL),
            cpu_capacity: 1,
            active: false,
        }
    }

    #[test]
    fn threshold_policy_completion_reset_and_samples() {
        use super::super::*;
        let _guard = runtime_test_guard();
        reset_internal();
        let mut state = runtime().heap.lock().unwrap();
        state.pacer.disable_for_test();
        state.last_major_live_bytes = 3 * MIN_GOAL;
        complete_threshold(&mut state, MIN_GOAL);
        assert_eq!(state.threshold_bytes as u64, 6 * MIN_GOAL);
        assert_eq!(state.pacer_trigger, 6 * MIN_GOAL);
        sample_threshold(&mut state);
        assert_eq!(state.threshold_bytes as u64, 6 * MIN_GOAL);
        reset_threshold(&mut state);
        assert_eq!(state.threshold_bytes, MIN_GOAL as usize);
        assert_eq!(state.pacer_trigger, MIN_GOAL);
        drop(state);
        reset_internal();
    }

    #[test]
    fn saturated_runway_requires_proportional_allocation_progress() {
        for live in [MIN_GOAL, 16 * MIN_GOAL, 256 * MIN_GOAL, 4096 * MIN_GOAL] {
            for rate in [1, MIN_GOAL, u64::MAX] {
                let decision = decide(Inputs {
                    live,
                    previous_goal: live * 2,
                    expected_work: live,
                    allocation_per_second: u64::MAX,
                    mark_per_cpu_second: Some(rate),
                    cpu_capacity: 1,
                    active: false,
                });
                assert!(decision.trigger >= live + live / 4);
                assert!(decision.trigger <= decision.goal);
                // At most four full scans per live-size worth of allocation,
                // independent of the absolute live-set size.
                assert!(live.div_ceil(decision.trigger - live) <= 4);
            }
        }
        assert_eq!(minimum_trigger(u64::MAX), u64::MAX);
        assert_eq!(minimum_trigger(0), MIN_GOAL / 4);
    }

    #[test]
    fn formula_capacity_and_active_goal() {
        for cpus in [1, 2, 5, 64] {
            let decision = decide(Inputs {
                cpu_capacity: cpus,
                ..input()
            });
            assert_eq!(
                decision.runway,
                mul_div_ceil(
                    4 * MIN_GOAL,
                    MIN_GOAL,
                    u128::from(4 * MIN_GOAL) * cpus as u128
                ) * 2
            );
            assert!(decision.trigger >= MIN_GOAL);
            assert!(decision.trigger <= decision.goal);
        }
        assert!(
            decide(Inputs {
                active: true,
                allocation_per_second: 0,
                ..input()
            })
            .goal
                >= 4 * MIN_GOAL
        );
    }

    #[test]
    fn rate_windows_spikes_slowdowns_and_invalid_clocks() {
        let mut sampler = Sampler::default();
        sampler.allocation(0, 0);
        sampler.allocation(1_000_000_000, 100);
        assert_eq!(sampler.allocation_rate, 100);
        sampler.allocation(2_000_000_000, 1100);
        assert_eq!(sampler.allocation_rate, 1000);
        sampler.allocation(3_000_000_000, 1100);
        assert_eq!(sampler.allocation_rate, 750);
        sampler.mark(1000, Some(1_000_000_000));
        sampler.mark(1000, Some(2_000_000_000));
        assert_eq!(sampler.mark_rate, Some(500));
        sampler.mark(0, Some(0));
        assert_eq!(sampler.mark_rate, None);
        sampler.allocation(2, 3);
        assert_eq!(sampler.allocation_rate, 0);
        sampler.allocation(2, 4);
        assert_eq!(sampler.allocation_rate, 0);
        sampler.mark(100, Some(100));
        sampler.allocation(6_000_000_002, 1000);
        assert_eq!(sampler.mark_rate, None);
    }

    #[test]
    fn allocation_bursts_share_a_stable_sampling_window() {
        for pieces in [1u64, 10, 1000] {
            let mut sampler = Sampler::default();
            sampler.allocation(0, 0);
            for i in 1..pieces {
                sampler.allocation(i * ALLOCATION_SAMPLE_NS / pieces, i * 1_000_000 / pieces);
                assert_eq!(sampler.allocation_rate, 0);
            }
            sampler.allocation(ALLOCATION_SAMPLE_NS, 1_000_000);
            assert_eq!(sampler.allocation_rate, 1_000_000_000);
            sampler.allocation(2 * ALLOCATION_SAMPLE_NS, 3_000_000);
            assert_eq!(sampler.allocation_rate, 2_000_000_000);
        }
    }

    #[test]
    fn extreme_rates_and_sizes_preserve_bounds() {
        for live in [0, 1, MIN_GOAL, u64::MAX / 2, u64::MAX] {
            for rate in [0, 1, u64::MAX] {
                for cpus in [0, 1, 5, 64] {
                    let result = decide(Inputs {
                        live,
                        previous_goal: live,
                        expected_work: live,
                        allocation_per_second: rate,
                        mark_per_cpu_second: Some(rate),
                        cpu_capacity: cpus,
                        ..input()
                    });
                    assert!(result.goal >= live);
                    assert!(result.trigger >= live && result.trigger <= result.goal);
                }
            }
        }
    }

    #[test]
    fn full_width_capacity_preserves_runway() {
        for cpus in [2, 5, 64] {
            let decision = decide(Inputs {
                expected_work: u64::MAX,
                allocation_per_second: 1,
                mark_per_cpu_second: Some(u64::MAX),
                cpu_capacity: cpus,
                ..input()
            });
            assert_eq!(decision.runway, 2);
            let decision = decide(Inputs {
                expected_work: u64::MAX,
                allocation_per_second: cpus as u64,
                mark_per_cpu_second: Some(u64::MAX),
                cpu_capacity: cpus,
                ..input()
            });
            assert_eq!(decision.runway, 2);
        }
    }

    #[test]
    fn invalid_allocation_samples_discard_stale_mark_rate() {
        for (ns, bytes) in [(1, 101), (0, 101), (2, 99)] {
            let mut sampler = Sampler::default();
            sampler.allocation(1, 100);
            sampler.mark(1000, Some(1));
            sampler.allocation(ns, bytes);
            assert_eq!(sampler.mark_rate, None);
            assert_eq!(sampler.allocation_rate, 0);
            assert_eq!(sampler.decision(input()).runway, MIN_GOAL);
            sampler.allocation(ns + 1_000_000_000, bytes + 100);
            assert_eq!(sampler.allocation_rate, 100);
            assert_eq!(sampler.mark_rate, None);
            sampler.mark(100, Some(1_000_000_000));
            assert_eq!(sampler.mark_rate, Some(100));
        }
    }

    #[test]
    fn disabled_pacer_preserves_soft_memory_trigger() {
        use super::super::*;
        let _guard = runtime_test_guard();
        reset_internal();
        {
            let mut state = runtime().heap.lock().unwrap();
            state.pacer.enabled = false;
            state.threshold_bytes = usize::MAX;
            state.pacer_trigger = 1;
        }
        let mut root = willow_alloc(1024);
        willow_push_root(&mut root);
        assert!(!allocation_should_collect());
        willow_gc_set_memory_limit(1);
        assert!(allocation_should_collect());
        collect_internal();
        assert!(!allocation_should_collect());
        willow_pop_root();
        reset_internal();
    }
}
