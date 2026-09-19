//! Opt-in, session-local operation counts. Compiler work is synchronous on the
//! session thread; TLS avoids threading instrumentation through semantic APIs.
//! No counters, allocations, atomics, or per-node hooks run when disabled: each
//! coarse operation only checks the empty slot. Read the environment once per
//! session, and restore an enclosing session on every exit (including unwind).
use std::cell::RefCell;
use std::fmt::Write;
use std::marker::PhantomData;
use std::rc::Rc;

use crate::diagnostics::FileId;

pub(crate) enum Counter {
    TypeChecker,
    NonpreemptibleHelpers,
    ClassLayout,
    ClassVslots,
    EffectSolve,
}

#[derive(Default)]
struct Stats {
    counts: [usize; 5],
    hydrates: usize,
    // File IDs are dense and session-local, including entry file zero. Keeping
    // an indexed vector gives O(1) updates and O(units) ordered reporting.
    hydrates_by_file: Vec<usize>,
    peaks: [usize; 4],
}

impl Stats {
    fn report(&self) -> String {
        let [checkers, helpers, layouts, vslots, solves] = self.counts;
        let [ast, checker, declared, lir] = self.peaks;
        let mut line = format!(
            "[query-stats] hydrates={} type_checkers={checkers} nonpreemptible_helpers={helpers} class_layouts={layouts} class_vslots={vslots} effect_solves={solves} peak_ast={ast} peak_checker={checker} peak_declared={declared} peak_lir={lir} hydrates_by_file=",
            self.hydrates,
        );
        let mut separator = "";
        for (file, count) in self.hydrates_by_file.iter().enumerate() {
            if *count != 0 {
                write!(line, "{separator}{file}:{count}").unwrap();
                separator = ",";
            }
        }
        if separator.is_empty() {
            line.push('-');
        }
        line
    }
}

thread_local! {
    static ACTIVE: RefCell<Option<Stats>> = const { RefCell::new(None) };
}

pub(crate) struct Session {
    previous: Option<Stats>,
    emit: bool,
    _thread_bound: PhantomData<Rc<()>>,
}

impl Session {
    pub(crate) fn enter() -> Self {
        Self::start(
            std::env::var_os("WILLOW_QUERY_STATS").is_some_and(|v| v == "1"),
            true,
        )
    }

    fn start(enabled: bool, emit: bool) -> Self {
        Self {
            previous: ACTIVE.with(|slot| slot.replace(enabled.then(Stats::default))),
            emit,
            _thread_bound: PhantomData,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let finished = ACTIVE.with(|slot| slot.replace(self.previous.take()));
        if self.emit
            && let Some(stats) = finished
        {
            eprintln!("{}", stats.report());
        }
    }
}

fn record(update: impl FnOnce(&mut Stats)) {
    ACTIVE.with(|slot| {
        if let Some(stats) = slot.borrow_mut().as_mut() {
            update(stats);
        }
    });
}

pub(crate) fn add(counter: Counter, count: usize) {
    record(|stats| stats.counts[counter as usize] += count);
}

pub(crate) fn hydrate(file: FileId) {
    record(|stats| {
        stats.hydrates += 1;
        let index = file.0 as usize;
        if stats.hydrates_by_file.len() <= index {
            stats.hydrates_by_file.resize(index + 1, 0);
        }
        stats.hydrates_by_file[index] += 1;
    });
}

pub(crate) fn peaks(peaks: [usize; 4]) {
    record(|stats| {
        for (peak, observed) in stats.peaks.iter_mut().zip(peaks) {
            *peak = (*peak).max(observed);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> String {
        ACTIVE.with(|slot| slot.borrow().as_ref().unwrap().report())
    }

    #[test]
    fn query_stats_line_has_stable_order_and_per_file_totals() {
        let _session = Session::start(true, false);
        hydrate(FileId(2));
        hydrate(FileId::ENTRY);
        hydrate(FileId(2));
        for counter in [
            Counter::TypeChecker,
            Counter::NonpreemptibleHelpers,
            Counter::ClassLayout,
            Counter::ClassVslots,
            Counter::EffectSolve,
        ] {
            add(counter, 3);
        }
        peaks([1, 2, 1, 1]);
        peaks([2, 1, 1, 1]);
        assert_eq!(
            report(),
            "[query-stats] hydrates=3 type_checkers=3 nonpreemptible_helpers=3 class_layouts=3 class_vslots=3 effect_solves=3 peak_ast=2 peak_checker=2 peak_declared=1 peak_lir=1 hydrates_by_file=0:1,2:2"
        );
    }

    #[test]
    fn query_stats_nested_disabled_and_unwinding_sessions_are_isolated() {
        let _outer = Session::start(true, false);
        hydrate(FileId::ENTRY);
        let expected = report();
        {
            let _disabled = Session::start(false, false);
            hydrate(FileId(u32::MAX)); // Must not allocate when disabled.
            add(Counter::TypeChecker, 1);
            peaks([9; 4]);
            ACTIVE.with(|slot| assert!(slot.borrow().is_none()));
        }
        assert_eq!(report(), expected);
        let result = std::panic::catch_unwind(|| {
            let _inner = Session::start(true, false);
            hydrate(FileId(9));
            panic!("test unwind");
        });
        assert!(result.is_err());
        assert_eq!(report(), expected);
        std::thread::spawn(|| {
            let _other = Session::start(true, false);
            assert!(report().contains("hydrates=0 "));
            hydrate(FileId(4));
        })
        .join()
        .unwrap();
        assert_eq!(report(), expected);
    }
}
