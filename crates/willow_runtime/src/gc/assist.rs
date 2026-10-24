//! Per-OS-thread allocation debt. One work unit is one byte of marked object
//! storage or scanned reference slots. Q16 charges preserve fractional rates;
//! only the thread that actually drains work receives its credit.
use std::cell::Cell;

pub(super) const SCALE: u64 = 1 << 16;

/// Cycle-local pacing estimate. Allocation consumption is published only at
/// slow allocations/refills; completed work includes every queue consumer.
/// Estimates never transfer background credit into a mutator's private debt.
pub(super) struct Pacing {
    pub(super) work: u64,
    pub(super) runway: u64,
    background_rate: u64,
    allocated: std::sync::atomic::AtomicU64,
}

impl Pacing {
    pub(super) fn new(work: u64, runway: u64) -> Self {
        Self {
            work,
            runway,
            background_rate: 0,
            allocated: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub(super) fn with_background(mut self, demand_ppm: u64) -> Self {
        let demand = (u128::from(demand_ppm) * u128::from(SCALE))
            .div_ceil(u128::from(super::pacer::SCALE))
            .min(u64::MAX as u128) as u64;
        self.background_rate = rate(self.work, self.runway).saturating_sub(demand);
        self
    }

    /// Whether allocation has consumed the whole planned runway.
    pub(super) fn exhausted(&self) -> bool {
        self.allocated.load(std::sync::atomic::Ordering::Relaxed) >= self.runway
    }

    /// Consume runway without charging debt; returns the previous total.
    pub(super) fn record(&self, bytes: u64) -> u64 {
        use std::sync::atomic::Ordering;
        self.allocated
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_add(bytes))
            })
            .unwrap()
    }

    pub(super) fn charge(&self, bytes: u64, completed: u64, pending: bool) -> u64 {
        let previous = self.record(bytes);
        let remaining = self.work.saturating_sub(completed);
        let runway = self.runway.saturating_sub(previous.saturating_add(bytes));
        // Past the runway any further allocation overshoots the goal: owe
        // every pending unit, whatever the (possibly stale) work estimate says.
        if pending && runway == 0 {
            return u64::MAX;
        }
        // A historical prediction may underestimate the current graph. Retain
        // a one-to-one rate until outstanding work disappears in that case.
        rate(remaining, runway)
            .saturating_sub(self.background_rate)
            .max(if pending && remaining == 0 { SCALE } else { 0 })
    }
}

// Track only this thread's nested mark CPU, so wrapper-inclusive assist CPU
// can enter the cycle budget without double-counting concurrent consumers.
thread_local! {
    static MARK_CPU: Cell<Option<Option<u64>>> = const { Cell::new(None) };
}

pub(super) fn record_mark_cpu(cpu: Option<u64>) {
    if let Some(total) = MARK_CPU.get() {
        MARK_CPU.set(Some(total.zip(cpu).and_then(|(a, b)| a.checked_add(b))));
    }
}

pub(super) struct CpuScope {
    start: Option<u64>,
    previous: Option<Option<u64>>,
}

impl CpuScope {
    pub(super) fn new() -> Self {
        Self {
            start: crate::gc_telemetry::workers::thread_cpu_ns(),
            previous: MARK_CPU.replace(Some(Some(0))),
        }
    }

    pub(super) fn overhead(&self) -> Option<u64> {
        let elapsed = self
            .start
            .zip(crate::gc_telemetry::workers::thread_cpu_ns())
            .and_then(|(start, end)| end.checked_sub(start));
        elapsed
            .zip(MARK_CPU.get().flatten())
            .and_then(|(all, mark)| all.checked_sub(mark))
    }
}

impl Drop for CpuScope {
    fn drop(&mut self) {
        MARK_CPU.set(self.previous);
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Debt {
    epoch: u64,
    units: i64,
}

impl Debt {
    fn charge(&mut self, epoch: u64, bytes: u64, rate: u64) -> bool {
        if self.epoch != epoch {
            *self = Self { epoch, units: 0 };
        }
        let charge = (u128::from(bytes) * u128::from(rate)).min(i64::MAX as u128) as i64;
        self.units = self.units.saturating_add(charge);
        self.units >= SCALE as i64
    }

    fn credit(&mut self, epoch: u64, work: u64) {
        if self.epoch == epoch {
            let credit = (u128::from(work) * u128::from(SCALE)).min(i64::MAX as u128) as i64;
            self.units = self.units.saturating_sub(credit);
        }
    }
}

thread_local! {
    static DEBT: Cell<Debt> = const { Cell::new(Debt { epoch: 0, units: 0 }) };
}

pub(super) fn rate(work: u64, runway: u64) -> u64 {
    // Round up so small predicted work cannot disappear over a long runway.
    (u128::from(work) * u128::from(SCALE))
        .div_ceil(u128::from(runway.max(1)))
        .min(u64::MAX as u128) as u64
}

pub(super) fn charge(epoch: u64, bytes: u64, rate: u64) -> bool {
    DEBT.with(|local| {
        let mut debt = local.get();
        let due = debt.charge(epoch, bytes, rate);
        local.set(debt);
        due
    })
}

#[cfg(test)]
thread_local! {
    pub(super) static CREDITED: Cell<u64> = const { Cell::new(0) };
}

#[cfg(test)]
pub(super) fn debt_units() -> i64 {
    DEBT.get().units
}

pub(super) fn credit(epoch: u64, work: u64) {
    #[cfg(test)]
    CREDITED.set(CREDITED.get().saturating_add(work));
    DEBT.with(|local| {
        let mut debt = local.get();
        debt.credit(epoch, work);
        local.set(debt);
    });
}

/// Whether this thread still owes at least one work unit in `epoch`.
pub(super) fn due(epoch: u64) -> bool {
    let debt = DEBT.get();
    debt.epoch == epoch && debt.units >= SCALE as i64
}

pub(super) fn reset() {
    DEBT.set(Debt::default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assist_cpu_scope_is_local_checked_and_unwind_safe() {
        assert_eq!(MARK_CPU.get(), None);
        {
            let _scope = CpuScope::new();
            record_mark_cpu(Some(3));
            record_mark_cpu(Some(5));
            assert_eq!(MARK_CPU.get(), Some(Some(8)));
            record_mark_cpu(None);
            assert_eq!(MARK_CPU.get(), Some(None));
        }
        assert_eq!(MARK_CPU.get(), None);
        let result = std::panic::catch_unwind(|| {
            let _scope = CpuScope::new();
            record_mark_cpu(Some(u64::MAX));
            record_mark_cpu(Some(1));
            assert_eq!(MARK_CPU.get(), Some(None));
            panic!("exercise assist CPU unwind");
        });
        assert!(result.is_err());
        assert_eq!(MARK_CPU.get(), None);
    }

    #[test]
    fn predicted_background_reduces_demand_but_exhausted_runway_recovers() {
        let pacing = Pacing::new(100, 1000).with_background(0);
        assert_eq!(pacing.charge(0, 0, true), 0);
        assert!(pacing.charge(500, 0, true) > 0);
        assert!(pacing.charge(500, 0, true) > SCALE);
        // Past the runway every pending unit is owed, even beyond the estimate.
        assert_eq!(pacing.charge(0, 100, true), u64::MAX);
        assert_eq!(pacing.charge(0, 100, false), 0);
        let pacing = Pacing::new(100, 1000);
        assert_eq!(pacing.charge(900, 100, true), SCALE);
        let pacing = Pacing::new(100, 1000).with_background(50_000);
        assert_eq!(pacing.charge(0, 0, true), rate(1, 20));
    }

    #[test]
    fn remaining_work_and_runway_follow_batched_consumption() {
        let pacing = Pacing::new(100, 1000);
        assert_eq!(pacing.charge(100, 0, true), rate(100, 900));
        assert_eq!(pacing.charge(100, 75, true), rate(25, 800));
        assert_eq!(pacing.charge(799, 75, true), rate(25, 1));
        assert!(!pacing.exhausted());
        assert_eq!(pacing.charge(1, 75, true), u64::MAX);
        assert!(pacing.exhausted());
        assert_eq!(pacing.charge(u64::MAX, 100, true), u64::MAX);
        assert_eq!(pacing.charge(u64::MAX, 100, false), 0);
        assert_eq!(
            pacing.allocated.load(std::sync::atomic::Ordering::Relaxed),
            u64::MAX
        );
    }

    #[test]
    fn recorded_closing_allocation_exhausts_runway_and_debt_stays_due() {
        let pacing = Pacing::new(100, 1000);
        assert_eq!(pacing.record(999), 0);
        assert!(!pacing.exhausted());
        assert_eq!(pacing.record(1), 999);
        assert!(pacing.exhausted());
        assert_eq!(pacing.record(u64::MAX), 1000);
        reset();
        assert!(!due(1));
        assert!(charge(1, 1, u64::MAX));
        assert!(due(1));
        assert!(!due(2));
        credit(1, u64::MAX);
        assert!(!due(1));
        assert!(charge(1, 2, SCALE));
        credit(1, 1);
        assert!(due(1));
        credit(1, 1);
        assert!(!due(1));
        reset();
    }

    #[test]
    fn pacing_races_only_consume_allocation_once() {
        for threads in [1, 5, 16] {
            let pacing = Pacing::new(1000, 1000);
            std::thread::scope(|scope| {
                for _ in 0..threads {
                    let pacing = &pacing;
                    scope.spawn(move || {
                        for _ in 0..100 {
                            pacing.charge(64, 0, true);
                        }
                    });
                }
            });
            assert_eq!(
                pacing.allocated.load(std::sync::atomic::Ordering::Relaxed),
                threads * 6400
            );
        }
    }

    #[test]
    fn fractional_batches_credit_and_epochs() {
        let mut debt = Debt::default();
        let fractional = rate(1, 4);
        for _ in 0..3 {
            assert!(!debt.charge(1, 1, fractional));
        }
        assert!(debt.charge(1, 1, fractional));
        debt.credit(1, 2);
        assert_eq!(debt.units, -(SCALE as i64));
        assert!(!debt.charge(1, 4, fractional));
        assert!(debt.charge(2, 4, fractional));
        debt.credit(1, u64::MAX); // old epoch cannot forgive new debt
        assert_eq!(debt.units, SCALE as i64);
    }

    #[test]
    fn signed_saturation_and_zero_work_do_not_invent_credit() {
        let mut debt = Debt::default();
        assert!(debt.charge(1, u64::MAX, u64::MAX));
        assert_eq!(debt.units, i64::MAX);
        debt.credit(1, 0);
        assert_eq!(debt.units, i64::MAX);
        debt.credit(1, u64::MAX);
        debt.credit(1, u64::MAX);
        debt.credit(1, u64::MAX);
        assert_eq!(debt.units, i64::MIN);
        assert_eq!(rate(0, 0), 0);
        assert_eq!(rate(u64::MAX, 0), u64::MAX);
    }

    #[test]
    fn independent_threads_cannot_spend_each_others_credit() {
        for threads in [1, 5, 16] {
            std::thread::scope(|scope| {
                for thread in 0..threads {
                    scope.spawn(move || {
                        reset();
                        assert!(charge(7, 64, SCALE));
                        if thread % 2 == 0 {
                            credit(7, 128);
                            assert!(!charge(7, 32, SCALE));
                        } else {
                            assert!(charge(7, 32, SCALE));
                        }
                        reset();
                        assert!(!charge(7, 0, SCALE));
                    });
                }
            });
        }
    }
}
