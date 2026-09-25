//! Single-evaluator memoization. Neither the state map nor the evaluation stack
//! stays borrowed while running a query, including nested evaluation.
use anyhow::Result;
use std::{cell::RefCell, collections::HashMap, fmt::Debug, hash::Hash, sync::Arc, time::Instant};

enum State<V> {
    Computing,
    Ready(Arc<V>),
    Failed(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryStats {
    pub calls: usize,
    pub hits: usize,
    pub computations: usize,
    pub compute_ns: u128,
    pub max_depth: usize,
    /// Successful [`QueryTable::ready`] reads: reuse of a completed result by
    /// a frozen reader, which records neither a call nor a hit.
    pub frozen_reads: usize,
}

pub struct QueryTable<K, V> {
    states: RefCell<HashMap<K, State<V>>>,
    name: &'static str,
    stats: RefCell<QueryStats>,
}

thread_local! {
    static CHAIN: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

impl<K, V> Default for QueryTable<K, V> {
    fn default() -> Self {
        Self::named("query")
    }
}

impl<K, V> QueryTable<K, V> {
    pub fn named(name: &'static str) -> Self {
        Self {
            states: RefCell::default(),
            name,
            stats: RefCell::default(),
        }
    }
}

impl<K: Clone + Eq + Hash + Debug, V> QueryTable<K, V> {
    pub fn stats(&self) -> QueryStats {
        *self.stats.borrow()
    }

    /// A previously computed result, without evaluating or recording a call.
    /// Readers that must not allocate or depend on query order (backend
    /// emission over frozen layouts) use this instead of [`Self::query`]; a
    /// successful read counts as one `frozen_reads` reuse.
    pub fn ready(&self, key: &K) -> Option<Arc<V>> {
        let value = match self.states.borrow().get(key) {
            Some(State::Ready(value)) => Arc::clone(value),
            _ => return None,
        };
        self.stats.borrow_mut().frozen_reads += 1;
        crate::query_stats::query_frozen_read(self.name);
        Some(value)
    }

    pub fn is_ready(&self, key: &K) -> bool {
        matches!(self.states.borrow().get(key), Some(State::Ready(_)))
    }

    pub fn query(&self, key: K, compute: impl FnOnce() -> Result<V>) -> Result<Arc<V>> {
        self.stats.borrow_mut().calls += 1;
        crate::query_stats::query_call(self.name);
        match self.states.borrow().get(&key) {
            Some(State::Ready(value)) => {
                self.stats.borrow_mut().hits += 1;
                crate::query_stats::query_hit(self.name);
                return Ok(Arc::clone(value));
            }
            Some(State::Failed(error)) => {
                self.stats.borrow_mut().hits += 1;
                crate::query_stats::query_hit(self.name);
                anyhow::bail!("{error}");
            }
            Some(State::Computing) => {
                anyhow::bail!(
                    "E0800: compiler query cycle: {} -> {}({key:?})",
                    CHAIN.with(|chain| chain.borrow().join(" -> ")),
                    self.name
                );
            }
            None => {}
        }
        let frame = format!("{}({key:?})", self.name);
        self.states
            .borrow_mut()
            .insert(key.clone(), State::Computing);
        let depth = CHAIN.with(|chain| {
            let mut chain = chain.borrow_mut();
            chain.push(frame);
            chain.len()
        });
        {
            let mut stats = self.stats.borrow_mut();
            stats.computations += 1;
            stats.max_depth = stats.max_depth.max(depth);
        }
        crate::query_stats::query_enter(self.name, depth, |write| {
            CHAIN.with(|chain| write(chain.borrow().last().unwrap()));
        });
        // A caught panic must not leave a false cycle in a reusable table.
        struct Guard<'a, K: Eq + Hash, V> {
            table: &'a QueryTable<K, V>,
            key: K,
            started: Option<Instant>,
            depth: usize,
        }
        impl<K: Eq + Hash, V> Drop for Guard<'_, K, V> {
            fn drop(&mut self) {
                let elapsed = self
                    .started
                    .map_or(0, |started| started.elapsed().as_nanos());
                self.table.stats.borrow_mut().compute_ns += elapsed;
                crate::query_stats::query_exit(self.table.name, self.depth, elapsed, |write| {
                    CHAIN.with(|chain| write(chain.borrow().last().unwrap()));
                });
                CHAIN.with(|chain| {
                    chain.borrow_mut().pop();
                });
                if matches!(
                    self.table.states.borrow().get(&self.key),
                    Some(State::Computing)
                ) {
                    self.table.states.borrow_mut().remove(&self.key);
                }
            }
        }
        let _guard = Guard {
            table: self,
            key: key.clone(),
            started: crate::query_stats::timing_enabled().then(Instant::now),
            depth,
        };
        match compute() {
            Ok(value) => {
                let value = Arc::new(value);
                self.states
                    .borrow_mut()
                    .insert(key, State::Ready(Arc::clone(&value)));
                Ok(value)
            }
            Err(error) => {
                self.states
                    .borrow_mut()
                    .insert(key, State::Failed(format!("{error:#}")));
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reuses_success_failure_and_reports_cycles() {
        let table = QueryTable::default();
        for _ in 0..10 {
            assert_eq!(*table.query(1, || Ok(42)).unwrap(), 42);
        }
        let error = table
            .query(2, || table.query(2, || Ok(0)).map(|v| *v))
            .unwrap_err();
        assert!(error.to_string().contains("E0800"));
        assert!(table.query(2, || Ok(0)).is_err());
        assert_eq!(table.stats().computations, 2);
    }
    #[test]
    fn cycle_chain_includes_other_query_tables() {
        let first = QueryTable::named("checked_body");
        let second = QueryTable::named("body_effects");
        let error = first
            .query(7, || {
                second
                    .query(9, || first.query(7, || Ok(0)).map(|value| *value))
                    .map(|value| *value)
            })
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("checked_body(7) -> body_effects(9) -> checked_body(7)")
        );
        assert_eq!(first.stats().max_depth, 1);
        assert_eq!(second.stats().max_depth, 2);
        CHAIN.with(|chain| assert!(chain.borrow().is_empty()));
        assert!(first.query(7, || Ok(1)).is_err());
    }

    #[test]
    fn disabled_observation_preserves_query_results_without_timing() {
        let _session = crate::query_stats::Session::start(false, false);
        let table = QueryTable::named("disabled");
        assert_eq!(*table.query(1, || Ok(42)).unwrap(), 42);
        assert_eq!(*table.query(1, || panic!("cached")).unwrap(), 42);
        assert_eq!(
            table.stats(),
            QueryStats {
                calls: 2,
                hits: 1,
                computations: 1,
                compute_ns: 0,
                max_depth: 1,
                frozen_reads: 0,
            }
        );
    }

    #[test]
    fn frozen_reads_count_reuse_without_calls_or_hits() {
        let _session = crate::query_stats::Session::start(false, false);
        let table = QueryTable::named("frozen");
        assert!(table.ready(&1).is_none());
        table.query(1, || Ok(7)).unwrap();
        for _ in 0..5 {
            assert_eq!(*table.ready(&1).unwrap(), 7);
        }
        assert!(table.is_ready(&1));
        assert!(table.ready(&2).is_none());
        let stats = table.stats();
        assert_eq!(
            (
                stats.calls,
                stats.hits,
                stats.computations,
                stats.frozen_reads
            ),
            (1, 0, 1, 5)
        );
    }

    #[test]
    fn nested_queries_and_unwinding_release_borrows() {
        let table = QueryTable::default();
        assert_eq!(
            *table
                .query(1, || table.query(2, || Ok(42)).map(|v| *v))
                .unwrap(),
            42
        );
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            table.query(3, || panic!("test"))
        }));
        assert_eq!(*table.query(3, || Ok(9)).unwrap(), 9);
    }
}
