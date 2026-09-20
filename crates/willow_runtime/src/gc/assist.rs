//! Per-OS-thread allocation debt. One work unit is one byte of marked object
//! storage or scanned reference slots. Q16 charges preserve fractional rates;
//! only the thread that actually drains work receives its credit.
use std::cell::Cell;

pub(super) const SCALE: u64 = 1 << 16;

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

pub(super) fn credit(epoch: u64, work: u64) {
    DEBT.with(|local| {
        let mut debt = local.get();
        debt.credit(epoch, work);
        local.set(debt);
    });
}

pub(super) fn reset() {
    DEBT.set(Debt::default());
}

#[cfg(test)]
mod tests {
    use super::*;

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
