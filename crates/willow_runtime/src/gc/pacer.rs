//! Integer-only feedback for the heap runway. Sampling and policy are separate;
//! unavailable CPU measurements never masquerade as zero-cost marking.
pub(super) const SCALE: u64 = 1_000_000;
const NORMAL_CPU: u64 = 250_000;
const HARD_CPU: u64 = 500_000;
const MIN_GOAL: u64 = 1024 * 1024;
// Coalesce sub-millisecond allocation bursts instead of feeding refill-scale
// clock/scheduling noise into the immediate-rise runway estimator.
const ALLOCATION_SAMPLE_NS: u64 = 1_000_000;

/// Available CPU capacity, sampled once outside the allocation hot path.
pub(super) fn cpu_capacity() -> usize {
    static CPUS: std::sync::LazyLock<usize> =
        std::sync::LazyLock::new(|| std::thread::available_parallelism().map_or(1, usize::from));
    *CPUS
}

fn publish_decision(decision: Decision) {
    crate::gc_telemetry::pacing::publish(crate::gc_telemetry::pacing::Decision {
        goal_bytes: decision.goal,
        trigger_bytes: decision.trigger,
        runway_bytes: decision.runway,
        expected_work_bytes: decision.expected_work,
        dedicated_workers: decision.workers,
        fractional_duty_ppm: decision.fractional_duty_ppm,
        assist_work_per_byte_ppm: decision.assist_work_per_byte_ppm,
        overshoot_budget_bytes: decision.overshoot_budget,
        reasons: decision.reasons | UNCONTROLLED_PHASES,
        start_now: decision.start_now,
    });
}

/// Sole writer of goals and triggers; reservation budgets remain separate.
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
    let input = Inputs {
        fixed_goal: true,
        ..super::pacer_inputs(state)
    };
    let decision = state.pacer.decision(input);
    publish_decision(decision);
    publish(state, decision.goal, decision.trigger, "allocation_sample");
}

pub(super) fn allocation_failure(state: &super::GcState) {
    let mut input = super::pacer_inputs(state);
    input.feedback.allocator_failed = true;
    let mut decision = state.pacer.decision(input);
    decision.reasons |= ALLOCATOR_FAILED;
    decision.start_now = true;
    publish_decision(decision);
}

pub(super) fn complete_threshold(state: &mut super::GcState, previous_goal: u64) {
    let input = Inputs {
        previous_goal,
        ..super::pacer_inputs(state)
    };
    let (goal, trigger, reason) = if state.pacer.enabled() {
        let decision = state.pacer.decision(input);
        publish_decision(decision);
        (decision.goal, decision.trigger, "cycle_paced")
    } else {
        let decision = state.pacer.decision(input);
        publish_decision(decision);
        (decision.goal, decision.trigger, "cycle_fixed")
    };
    state.pacer.barrier_baseline = super::runtime()
        .write_barrier_calls
        .load(std::sync::atomic::Ordering::Relaxed);
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
    pub max_workers: usize,
    pub active: bool,
    pub fixed_goal: bool,
    pub feedback: Feedback,
}

/// Optional observations retain validity instead of interpreting missing CPU/RSS
/// as zero. Work and allocation are bytes; CPU and wall windows are nanoseconds.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Feedback {
    pub occupied: u64,
    pub root_work: u64,
    pub barrier_work: u64,
    pub pause_ns: u64,
    pub window_ns: u64,
    pub gc_cpu_ns: Option<u64>,
    pub previous_peak: u64,
    pub pressure: bool,
    pub allocator_failed: bool,
    pub committed_bytes: Option<u64>,
    pub resident_bytes: Option<u64>,
    /// Soft managed goal ceiling; the live floor always wins.
    pub soft_limit: Option<u64>,
}

pub(super) const COLD: u64 = 1;
pub(super) const CPU_MISSING: u64 = 2;
pub(super) const CPU_NORMAL: u64 = 4;
pub(super) const CPU_HARD: u64 = 8;
pub(super) const PREDICTED_LATE: u64 = 16;
pub(super) const PRESSURE: u64 = 32;
pub(super) const ALLOCATOR_FAILED: u64 = 64;
pub(super) const FIXED: u64 = 128;
// Root coordination, sweep and mutator assistance currently cannot be throttled
// by the background worker controller. Never label its duty as a global cap.
pub(super) const UNCONTROLLED_PHASES: u64 = 256;
pub(super) const MEMORY_UNAVAILABLE: u64 = 512;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Decision {
    pub goal: u64,
    pub trigger: u64,
    pub runway: u64,
    pub expected_work: u64,
    pub workers: u64,
    pub fractional_duty_ppm: u64,
    pub assist_work_per_byte_ppm: u64,
    pub overshoot_budget: u64,
    pub reasons: u64,
    pub start_now: bool,
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
    let feedback = input.feedback;
    let cpus = input.cpu_capacity.clamp(1, 65536) as u64;
    let worker_capacity_ppm =
        (cpus * NORMAL_CPU).min((input.max_workers.min(65536) as u64) * SCALE);
    let expected_work = input
        .expected_work
        .saturating_add(feedback.root_work)
        .saturating_add(feedback.barrier_work);
    let mut reasons = 0;
    // Unlimited goals never clamp to RSS or commitment: the separate memory
    // controller owns that policy. Preserve missing observations diagnostically.
    if feedback.committed_bytes.is_none() || feedback.resident_bytes.is_none() {
        reasons |= MEMORY_UNAVAILABLE;
    }
    if input.mark_per_cpu_second.is_none_or(|rate| rate == 0) {
        reasons |= COLD;
    }
    match feedback.gc_cpu_ns.filter(|_| feedback.window_ns != 0) {
        Some(cpu) => {
            let ratio = mul_div_ceil(
                cpu,
                SCALE,
                u128::from(feedback.window_ns) * u128::from(cpus),
            );
            if ratio > NORMAL_CPU {
                reasons |= CPU_NORMAL;
            }
            if ratio > HARD_CPU {
                reasons |= CPU_HARD;
            }
        }
        None => reasons |= CPU_MISSING,
    }
    // Conservative cold/invalid-rate runway: half the fixed fallback goal.
    let runway = match input.mark_per_cpu_second.filter(|&rate| rate != 0) {
        Some(rate) => {
            // Preserve the full denominator: saturating it at u64::MAX
            // overestimates required runway on multi-CPU inputs.
            let capacity =
                (u128::from(rate) * u128::from(worker_capacity_ppm) / u128::from(SCALE)).max(1);
            mul_div_ceil(expected_work, input.allocation_per_second, capacity).saturating_mul(2)
        }
        None => floor / 2,
    };
    let runway = runway.saturating_add(mul_div_ceil(
        input.allocation_per_second,
        feedback.pause_ns,
        1_000_000_000,
    ));
    let floor = if reasons & CPU_NORMAL != 0 {
        floor.saturating_mul(2)
    } else {
        floor
    };
    let desired = floor
        .max(input.live.saturating_add(runway.saturating_mul(2)))
        .max(feedback.previous_peak);
    // Limit each inter-cycle adjustment to a factor of two. Live is always a
    // lower bound. Never shrink a goal while that cycle is active.
    let previous = input.previous_goal.max(MIN_GOAL);
    let lower = if input.active { previous } else { previous / 2 };
    let goal = if input.fixed_goal {
        input.previous_goal.max(input.live)
    } else {
        desired
            .clamp(lower, previous.saturating_mul(2))
            .max(input.live)
    };
    // Apply before deriving trigger, background work and Assist demand so
    // all published decisions describe the same soft-bounded goal.
    let goal = feedback
        .soft_limit
        .map_or(goal, |limit| goal.min(limit).max(input.live));
    let trigger = goal
        .saturating_sub(runway)
        .max(minimum_trigger(input.live).min(goal));
    if feedback.pressure {
        reasons |= PRESSURE;
    }
    if feedback.allocator_failed {
        reasons |= ALLOCATOR_FAILED;
    }
    let predicted_late = feedback.occupied >= minimum_trigger(input.live)
        && feedback.occupied >= goal.saturating_sub(runway);
    if predicted_late {
        reasons |= PREDICTED_LATE;
    }
    let start_now = feedback.occupied >= goal
        || predicted_late
        || feedback.pressure
        || feedback.allocator_failed;
    let headroom = goal.saturating_sub(input.live).max(1);
    let background_rate = input.mark_per_cpu_second.map_or(0, |rate| {
        ((u128::from(rate) * u128::from(worker_capacity_ppm)) / u128::from(SCALE))
            .min(u128::from(u64::MAX)) as u64
    });
    let background_work = if input.allocation_per_second == 0 {
        0
    } else {
        ((u128::from(background_rate) * u128::from(headroom))
            / u128::from(input.allocation_per_second))
        .min(u128::from(u64::MAX)) as u64
    };
    let assist_work = expected_work.saturating_sub(background_work);
    Decision {
        goal,
        trigger,
        runway,
        expected_work,
        workers: worker_capacity_ppm / SCALE,
        fractional_duty_ppm: worker_capacity_ppm % SCALE,
        assist_work_per_byte_ppm: mul_div_ceil(assist_work, SCALE, u128::from(headroom)),
        overshoot_budget: goal / 8,
        reasons,
        start_now,
    }
}

/// Full workers plus at most one fractional worker. No idle CPU is borrowed.
pub(super) fn worker_duty(cpus: usize, index: usize) -> u64 {
    let total = (cpus.clamp(1, 65536) as u64) * NORMAL_CPU;
    total
        .saturating_sub((index as u64).saturating_mul(SCALE))
        .min(SCALE)
}

/// Repay CPU after a bounded drain, crediting wall time already elapsed.
/// Missing CPU conservatively charges the whole interval.
pub(super) fn duty_delay(cpu: Option<u64>, elapsed: u64, duty: u64) -> u64 {
    mul_div_ceil(cpu.unwrap_or(elapsed), SCALE, u128::from(duty.max(1))).saturating_sub(elapsed)
}

#[derive(Debug)]
pub(super) struct Sampler {
    enabled: bool,
    last: Option<(u64, u64)>,
    allocation_rate: u64,
    mark_rate: Option<u64>,
    pub(super) barrier_baseline: u64,
}

impl Default for Sampler {
    fn default() -> Self {
        Self {
            enabled: std::env::var("WILLOW_GC_PACER").as_deref() != Ok("0"),
            last: None,
            allocation_rate: 0,
            mark_rate: None,
            barrier_baseline: 0,
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
        if !self.enabled {
            let goal = if input.fixed_goal {
                input.previous_goal.max(input.live)
            } else {
                input.live.saturating_mul(2).max(MIN_GOAL)
            };
            return Decision {
                goal,
                trigger: goal,
                reasons: FIXED,
                assist_work_per_byte_ppm: SCALE,
                start_now: input.feedback.occupied >= goal,
                ..Decision::default()
            };
        }
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
            max_workers: 64,
            active: false,
            fixed_goal: false,
            feedback: Feedback::default(),
        }
    }

    #[test]
    fn threshold_policy_completion_reset_and_samples() {
        use super::super::*;
        let _guard = runtime_test_guard();
        reset_internal();
        let mut state = runtime().heap.lock().unwrap();
        state.pacer.disable_for_test();
        state.last_major_policy_live_bytes = 3 * MIN_GOAL;
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
                    max_workers: 64,
                    active: false,
                    fixed_goal: false,
                    feedback: Feedback::default(),
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
                mul_div_ceil(4 * MIN_GOAL, MIN_GOAL, u128::from(MIN_GOAL) * cpus as u128) * 2
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
            assert_eq!(decision.runway, if cpus == 2 { 6 } else { 2 });
            let decision = decide(Inputs {
                expected_work: u64::MAX,
                allocation_per_second: cpus as u64,
                mark_per_cpu_second: Some(u64::MAX),
                cpu_capacity: cpus,
                ..input()
            });
            assert_eq!(decision.runway, if cpus < 64 { 10 } else { 8 });
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
    fn soft_goal_bounds_derived_assist_and_preserves_live_floor() {
        for limit in [0, 1, 1024, u64::MAX] {
            for live in [0, 512, 2048, u64::MAX] {
                let mut input = input();
                input.live = live;
                input.feedback.soft_limit = Some(limit);
                input.allocation_per_second = 1_000_000;
                input.mark_per_cpu_second = Some(1);
                let bounded = decide(input);
                assert!(bounded.goal >= live);
                assert!(bounded.goal <= limit.max(live));
                assert!(bounded.trigger <= bounded.goal);
                assert_eq!(bounded.overshoot_budget, bounded.goal / 8);
                input.fixed_goal = true;
                input.previous_goal = bounded.goal;
                input.feedback.soft_limit = None;
                let equivalent = decide(input);
                assert_eq!(
                    bounded.assist_work_per_byte_ppm,
                    equivalent.assist_work_per_byte_ppm
                );
            }
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
    #[test]
    fn runtime_completed_cpu_excess_adjusts_next_goal() {
        use super::super::*;
        use crate::gc_telemetry::{pacing, with_completed_cpu_for_test};
        let _guard = runtime_test_guard();
        for (cpu, expected_flags, expected_goal) in [
            (Some(0), 0, MIN_GOAL),
            (Some(u64::MAX / 64), CPU_NORMAL | CPU_HARD, 2 * MIN_GOAL),
            (None, CPU_MISSING, MIN_GOAL),
        ] {
            reset_internal();
            runtime()
                .heap
                .lock()
                .unwrap()
                .pacer
                .set_enabled_for_test(true);
            with_completed_cpu_for_test(cpu, || {
                // More than a full history window: verify sustained feedback,
                // not just a synthetic call to the deterministic controller.
                for cycle in 1..=10 {
                    collect_internal();
                    let snapshot = pacing::snapshot();
                    assert_eq!(crate::gc_telemetry::snapshot().major_cycles, cycle);
                    assert_eq!(
                        snapshot.decision.reasons & (CPU_NORMAL | CPU_HARD | CPU_MISSING),
                        expected_flags
                    );
                    assert_eq!(snapshot.decision.goal_bytes, expected_goal);
                    let state = runtime().heap.lock().unwrap();
                    assert_eq!(state.threshold_bytes as u64, expected_goal);
                }
            });
        }
        reset_internal();
    }

    #[test]
    fn cpu_budget_thresholds_and_missing_windows() {
        for (cpu, flags) in [
            (0, 0),
            (250, 0),
            (251, CPU_NORMAL),
            (500, CPU_NORMAL),
            (501, CPU_NORMAL | CPU_HARD),
        ] {
            let d = decide(Inputs {
                feedback: Feedback {
                    window_ns: 1000,
                    gc_cpu_ns: Some(cpu),
                    ..Feedback::default()
                },
                ..input()
            });
            assert_eq!(d.reasons & (CPU_NORMAL | CPU_HARD | CPU_MISSING), flags);
        }
        for cpu in [None, Some(0), Some(u64::MAX)] {
            let d = decide(Inputs {
                feedback: Feedback {
                    gc_cpu_ns: cpu,
                    ..Feedback::default()
                },
                ..input()
            });
            assert_ne!(d.reasons & CPU_MISSING, 0);
        }
    }

    #[test]
    fn fractional_workers_and_duty_repayment() {
        for cpus in [1, 2, 5, 64] {
            let sum: u64 = (0..65).map(|i| worker_duty(cpus, i)).sum();
            assert_eq!(sum, cpus as u64 * NORMAL_CPU);
            let d = decide(Inputs {
                cpu_capacity: cpus,
                ..input()
            });
            assert_eq!(d.workers * SCALE + d.fractional_duty_ppm, sum);
        }
        assert_eq!(duty_delay(Some(100), 100, 250_000), 300);
        assert_eq!(duty_delay(Some(100), 500, 250_000), 0);
        assert_eq!(duty_delay(None, 100, 500_000), 100);
        assert_eq!(duty_delay(Some(u64::MAX), 1, 1), u64::MAX - 1);
        assert_eq!(
            decide(Inputs {
                cpu_capacity: 64,
                max_workers: 2,
                ..input()
            })
            .workers,
            2
        );
        assert_eq!(
            decide(Inputs {
                max_workers: 0,
                ..input()
            })
            .workers,
            0
        );
    }

    #[test]
    fn work_pause_pressure_and_exhaustion_inputs() {
        let base = decide(input());
        for feedback in [
            Feedback {
                root_work: MIN_GOAL,
                ..Feedback::default()
            },
            Feedback {
                barrier_work: MIN_GOAL,
                ..Feedback::default()
            },
            Feedback {
                pause_ns: 1_000_000_000,
                ..Feedback::default()
            },
        ] {
            assert!(
                decide(Inputs {
                    feedback,
                    ..input()
                })
                .runway
                    > base.runway
            );
        }
        for feedback in [
            Feedback {
                occupied: u64::MAX,
                ..Feedback::default()
            },
            Feedback {
                pressure: true,
                ..Feedback::default()
            },
            Feedback {
                allocator_failed: true,
                ..Feedback::default()
            },
        ] {
            assert!(
                decide(Inputs {
                    feedback,
                    ..input()
                })
                .start_now
            );
        }
        let d = decide(Inputs {
            feedback: Feedback {
                previous_peak: 8 * MIN_GOAL,
                ..Feedback::default()
            },
            ..input()
        });
        assert_eq!(d.goal, 8 * MIN_GOAL);
    }

    #[test]
    fn smoothed_improvements_converge_and_spikes_are_immediate() {
        let mut s = Sampler::default();
        s.mark(1000, Some(1_000_000_000));
        for _ in 0..20 {
            s.mark(2000, Some(1_000_000_000));
        }
        assert!(s.mark_rate.unwrap() >= 1990);
        s.mark(100, Some(1_000_000_000));
        assert_eq!(s.mark_rate, Some(100));
        s.allocation(0, 0);
        s.allocation(1_000_000_000, 1000);
        for i in 2..=21 {
            s.allocation(i * 1_000_000_000, 1000);
        }
        assert!(s.allocation_rate <= 4);
        s.allocation(22_000_000_000, 1_001_000);
        assert_eq!(s.allocation_rate, 1_000_000);
    }

    #[test]
    fn decision_publication_is_a_coherent_group() {
        use super::super::*;
        let _guard = runtime_test_guard();
        reset_internal();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for n in 1..=10_000 {
                    publish_decision(Decision {
                        goal: n * 2,
                        trigger: n,
                        ..Decision::default()
                    });
                }
            });
            scope.spawn(|| {
                for _ in 0..10_000 {
                    let s = crate::gc_telemetry::pacing::snapshot();
                    assert_eq!(s.decision.goal_bytes, s.decision.trigger_bytes * 2);
                }
            });
        });
        reset_internal();
    }

    #[test]
    fn nested_cpu_window_preserves_hard_budget_and_missing_reasons() {
        use super::super::*;
        use crate::gc_telemetry::pacing;
        let _guard = runtime_test_guard();
        for missing in [None, Some(0), Some(8)] {
            reset_internal();
            pacing::begin_major(100);
            for i in 0..9 {
                pacing::cycle(
                    100 + i * 10,
                    106 + i * 10,
                    (missing != Some(i)).then_some(6),
                    true,
                );
            }
            pacing::cycle(100, 200, Some(1), false);
            let snapshot = pacing::snapshot();
            let d = decide(Inputs {
                feedback: Feedback {
                    window_ns: snapshot.window_end_ns - snapshot.window_start_ns,
                    gc_cpu_ns: (snapshot.window_cpu_missing == 0).then_some(snapshot.window_cpu_ns),
                    ..Feedback::default()
                },
                ..input()
            });
            if missing.is_none() {
                assert_eq!(snapshot.window_cpu_ns, 55);
                assert_eq!(
                    d.reasons & (CPU_NORMAL | CPU_HARD | CPU_MISSING),
                    CPU_NORMAL | CPU_HARD
                );
            } else {
                assert_eq!(
                    d.reasons & (CPU_NORMAL | CPU_HARD | CPU_MISSING),
                    CPU_MISSING
                );
            }
        }
        reset_internal();
    }

    #[test]
    fn fixed_goal_keeps_derived_outputs_consistent() {
        let i = Inputs {
            fixed_goal: true,
            ..input()
        };
        let d = decide(i);
        assert_eq!(d.goal, i.previous_goal);
        assert_eq!(d.overshoot_budget, d.goal / 8);
        assert!(d.trigger <= d.goal);
        let cold = decide(Inputs {
            mark_per_cpu_second: None,
            ..i
        });
        assert_eq!(
            cold.assist_work_per_byte_ppm,
            mul_div_ceil(i.expected_work, SCALE, u128::from(i.previous_goal - i.live))
        );
        assert!(d.assist_work_per_byte_ppm < cold.assist_work_per_byte_ppm);
        let d = decide(Inputs {
            live: i.previous_goal * 2,
            ..i
        });
        assert_eq!(d.goal, i.previous_goal * 2);
    }

    #[test]
    fn output_ranges_cover_independent_extremes() {
        for live in [0, 1, MIN_GOAL, u64::MAX] {
            for work in [0, 1, MIN_GOAL, u64::MAX] {
                for allocation in [0, 1, MIN_GOAL, u64::MAX] {
                    for cpus in [0, 1, 2, 5, usize::MAX] {
                        let d = decide(Inputs {
                            live,
                            expected_work: work,
                            allocation_per_second: allocation,
                            cpu_capacity: cpus,
                            ..input()
                        });
                        assert!(d.goal >= live);
                        assert!(d.trigger >= live && d.trigger <= d.goal);
                        assert!(d.fractional_duty_ppm < SCALE);
                        assert!(d.workers <= 64);
                        assert!(d.overshoot_budget <= d.goal);
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "reference-host measurement; run explicitly with --ignored --nocapture"]
    fn pacer_reference_measurement() {
        use super::super::*;
        let _guard = runtime_test_guard();
        for n in [10_000, 100_000, 1_000_000] {
            let start = std::time::Instant::now();
            for _ in 0..n {
                std::hint::black_box(decide(std::hint::black_box(input())));
            }
            println!(
                "PACER_MODEL iterations={n} elapsed_ns={}",
                start.elapsed().as_nanos()
            );
        }
        for paced in [false, true] {
            for payload in [64, 512, 4096] {
                reset_internal();
                runtime()
                    .heap
                    .lock()
                    .unwrap()
                    .pacer
                    .set_enabled_for_test(paced);
                let mut roots = [std::ptr::null_mut(); 128];
                for root in &mut roots {
                    willow_push_root(root);
                    *root = willow_alloc(payload);
                }
                let start = std::time::Instant::now();
                let mut overshoot = 0u64;
                let mut peak = 0u64;
                for _ in 0..128 {
                    for _ in 0..64 {
                        std::hint::black_box(willow_alloc(payload));
                    }
                    let state = runtime().heap.lock().unwrap();
                    let occupied = memory_inputs(&state).occupied;
                    peak = peak.max(occupied);
                    overshoot =
                        overshoot.max(occupied.saturating_sub(state.threshold_bytes as u64));
                }
                willow_gc_collect();
                let elapsed = crate::gc_telemetry::elapsed_ns(start);
                let stats = crate::gc_telemetry::snapshot();
                let p = crate::gc_telemetry::pacing::snapshot();
                let m = crate::gc_telemetry::measurement::snapshot();
                println!(
                    "PACER_RUNTIME enabled={paced} payload={payload} allocations=8192 wall_ns={elapsed} peak_bytes={peak} overshoot_bytes={overshoot} gc_cpu_ns={} missing_cpu_cycles={} pause_max_ns={} assist_samples={} assist_cpu_ns={} assist_demand_ppm={} reasons={}",
                    p.gc_cpu_ns,
                    p.cpu_missing_cycles,
                    stats.major_pauses.max_ns,
                    m.assist_samples,
                    m.assist_cpu_ns,
                    p.decision.assist_work_per_byte_ppm,
                    p.decision.reasons
                );
                for _ in &roots {
                    willow_pop_root();
                }
                reset_internal();
            }
        }
    }
}
