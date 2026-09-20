//! Integer-only feedback for the heap runway. Sampling and policy are separate;
//! unavailable CPU measurements never masquerade as zero-cost marking.
const MIN_GOAL: u64 = 1024 * 1024;

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
        .max(input.live.saturating_add(MIN_GOAL / 4).min(goal));
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

    pub(super) fn sample_due(&self, bytes: u64) -> bool {
        self.enabled
            && self
                .last
                .is_none_or(|(_, previous)| bytes.saturating_sub(previous) >= 65536)
    }

    pub(super) fn allocation(&mut self, ns: u64, bytes: u64) {
        let previous = self.last.replace((ns, bytes));
        let Some((old_ns, old_bytes)) = previous else {
            return;
        };
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
