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
    EffectInventory,
    EffectEdges,
}

#[derive(Default)]
struct Stats {
    counts: [usize; 7],
    hydrates: usize,
    // File IDs are dense and session-local, including entry file zero. Keeping
    // an indexed vector gives O(1) updates and O(units) ordered reporting.
    hydrates_by_file: Vec<usize>,
    peaks: [usize; 4],
    queries: std::collections::BTreeMap<&'static str, crate::compiler_db::query::QueryStats>,
}

impl Stats {
    fn report(&self) -> String {
        let [
            checkers,
            helpers,
            layouts,
            vslots,
            solves,
            effect_inventory,
            effect_edges,
        ] = self.counts;
        let [ast, checker, declared, lir] = self.peaks;
        let mut line = format!(
            "[query-stats] hydrates={} type_checkers={checkers} nonpreemptible_helpers={helpers} class_layouts={layouts} class_vslots={vslots} effect_solves={solves} effect_inventory={effect_inventory} effect_edges={effect_edges} peak_ast={ast} peak_checker={checker} peak_declared={declared} peak_lir={lir} hydrates_by_file=",
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
        for (name, query) in &self.queries {
            write!(
                line,
                " {name}[calls={},hits={},computations={},compute_ns={},max_depth={},frozen_reads={}]",
                query.calls,
                query.hits,
                query.computations,
                query.compute_ns,
                query.max_depth,
                query.frozen_reads
            )
            .unwrap();
        }
        line
    }
}

thread_local! {
    static LOG: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static ACTIVE: RefCell<Option<Stats>> = const { RefCell::new(None) };
}

pub(crate) struct Session {
    previous: Option<Stats>,
    emit: bool,
    previous_log: bool,
    _thread_bound: PhantomData<Rc<()>>,
}

impl Session {
    pub(crate) fn enter() -> Self {
        let session = Self::start(
            std::env::var_os("WILLOW_QUERY_STATS").is_some_and(|v| v == "1"),
            true,
        );
        LOG.with(|log| log.set(std::env::var_os("WILLOW_QUERY_LOG").is_some_and(|v| v == "1")));
        session
    }

    pub(crate) fn start(enabled: bool, emit: bool) -> Self {
        Self {
            previous: ACTIVE.with(|slot| slot.replace(enabled.then(Stats::default))),
            emit,
            previous_log: LOG.with(|log| log.replace(false)),
            _thread_bound: PhantomData,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        LOG.with(|log| log.set(self.previous_log));
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

#[cfg(test)]
pub(crate) fn count(counter: Counter) -> usize {
    ACTIVE.with(|active| {
        active
            .borrow()
            .as_ref()
            .map_or(0, |stats| stats.counts[counter as usize])
    })
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

pub(crate) fn timing_enabled() -> bool {
    ACTIVE.with(|slot| slot.borrow().is_some())
}

pub(crate) fn query_call(name: &'static str) {
    record(|stats| stats.queries.entry(name).or_default().calls += 1);
}

pub(crate) fn query_hit(name: &'static str) {
    record(|stats| stats.queries.entry(name).or_default().hits += 1);
}

pub(crate) fn query_frozen_read(name: &'static str) {
    record(|stats| stats.queries.entry(name).or_default().frozen_reads += 1);
}

pub(crate) fn query_enter(
    name: &'static str,
    depth: usize,
    frame: impl FnOnce(&mut dyn FnMut(&str)),
) {
    record(|stats| {
        let query = stats.queries.entry(name).or_default();
        query.computations += 1;
        query.max_depth = query.max_depth.max(depth);
    });
    if LOG.with(|log| log.get()) {
        frame(&mut |frame| trace(format_args!("enter depth={depth} {frame}")));
    }
}

pub(crate) fn query_exit(
    name: &'static str,
    depth: usize,
    elapsed: u128,
    frame: impl FnOnce(&mut dyn FnMut(&str)),
) {
    record(|stats| stats.queries.entry(name).or_default().compute_ns += elapsed);
    if LOG.with(|log| log.get()) {
        frame(&mut |frame| {
            trace(format_args!(
                "exit depth={depth} {frame} compute_ns={elapsed}"
            ))
        });
    }
}

fn trace(message: std::fmt::Arguments<'_>) {
    // A closed stderr must not panic while an evaluator is unwinding.
    use std::io::Write as _;
    let _ = writeln!(std::io::stderr().lock(), "[query-log] {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> String {
        ACTIVE.with(|slot| slot.borrow().as_ref().unwrap().report())
    }

    #[test]
    fn named_query_counts_and_timing_are_session_local() {
        let _session = Session::start(true, false);
        let table = crate::compiler_db::query::QueryTable::named("checked_body");
        table.query(1, || Ok(42)).unwrap();
        table.query(1, || Ok(0)).unwrap();
        let expected = table.stats();
        ACTIVE.with(|slot| {
            assert_eq!(
                slot.borrow().as_ref().unwrap().queries["checked_body"],
                expected
            )
        });
        assert!(report().contains("checked_body[calls=2,hits=1,computations=1,compute_ns="));
    }

    #[test]
    fn disabled_logging_does_not_evaluate_trace_formatter() {
        let _session = Session::start(false, false);
        query_enter("unused", 1, |_| panic!("disabled trace formatter"));
        query_exit("unused", 1, 0, |_| panic!("disabled trace formatter"));
        assert!(!timing_enabled());
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
            "[query-stats] hydrates=3 type_checkers=3 nonpreemptible_helpers=3 class_layouts=3 class_vslots=3 effect_solves=3 effect_inventory=0 effect_edges=0 peak_ast=2 peak_checker=2 peak_declared=1 peak_lir=1 hydrates_by_file=0:1,2:2"
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
