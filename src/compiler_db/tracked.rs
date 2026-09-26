//! Single-evaluator revision validation. Values remain shared; dependency
//! metadata belongs to this engine and is never serialized with an artifact.
// Query-family migration follows the engine phase. Keep the typed dispatcher
// internal until those families have passed their purity/equality audits.
#![allow(dead_code)]
use crate::{module::UnitId, parser::ast::BodyId, semantic::ids::FunctionId};
use anyhow::{Context, Result};
use std::{
    any::Any,
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Revision(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ResultFingerprint(u128);
impl ResultFingerprint {
    /// Callers supply canonical bytes, never Debug output or numeric intern IDs.
    pub(crate) fn bytes(bytes: &[u8]) -> Self {
        Self(
            bytes
                .iter()
                .fold(0x6c62272e07bb014262b821756295c58d, |h, b| {
                    (h ^ u128::from(*b)).wrapping_mul(0x1000000000000000000013b)
                }),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum InputNode {
    Source(PathBuf),
    Manifest(PathBuf),
    Lock(PathBuf),
    PackageGraph,
    Options,
    Target,
    RuntimeCapabilities,
    Features,
    CompilerStamp,
    StdlibStamp,
    ManifestMode,
    Entry,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum QueryNode {
    Parse(PathBuf),
    Declarations(UnitId),
    Signature(UnitId, FunctionId),
    TypedBody(BodyId),
    Effects(FunctionId),
    References(UnitId),
    Layout(super::layout::TargetLayoutKey),
    LirBody(BodyId),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum DependencyNode {
    Input(InputNode),
    Query(QueryNode),
}
#[derive(Clone, Debug)]
struct Dependency {
    node: DependencyNode,
    observed_changed_at: Revision,
}

/// Equality is mandatory even when fingerprints match: collisions cannot cause
/// stale reuse. Each family chooses its semantic, diagnostics-inclusive value.
#[derive(Clone)]
pub(crate) struct QueryValue {
    value: Arc<dyn Any>,
    fingerprint: ResultFingerprint,
    equivalent: fn(&dyn Any, &dyn Any) -> bool,
}
impl QueryValue {
    pub(crate) fn new<V: Eq + 'static>(value: V, fingerprint: ResultFingerprint) -> Self {
        Self {
            value: Arc::new(value),
            fingerprint,
            equivalent: |a, b| {
                a.downcast_ref::<V>()
                    .zip(b.downcast_ref::<V>())
                    .is_some_and(|(a, b)| a == b)
            },
        }
    }
    fn equivalent(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint && (self.equivalent)(&*self.value, &*other.value)
    }
    pub(crate) fn get<V: 'static>(&self) -> &V {
        self.value
            .downcast_ref()
            .expect("tracked query result type mismatch")
    }
}

#[derive(Clone)]
struct Input {
    value: QueryValue,
    changed_at: Revision,
    _durability: (),
}
#[derive(Clone)]
struct Memo {
    result: std::result::Result<QueryValue, Arc<str>>,
    verified_at: Revision,
    changed_at: Revision,
    dependencies: Arc<[Dependency]>,
}
enum State {
    Computing { revision: Revision },
    Ready(Memo),
}

#[derive(Default)]
struct Edges {
    ordered: Vec<Dependency>,
    index: Option<HashSet<DependencyNode>>,
}
impl Edges {
    fn insert(&mut self, edge: Dependency) {
        const SMALL: usize = 8;
        if let Some(index) = &mut self.index {
            if !index.insert(edge.node.clone()) {
                return;
            }
        } else {
            if self.ordered.iter().any(|old| old.node == edge.node) {
                return;
            }
            if self.ordered.len() == SMALL {
                let mut index: HashSet<_> = self.ordered.iter().map(|d| d.node.clone()).collect();
                index.insert(edge.node.clone());
                self.index = Some(index);
            }
        }
        self.ordered.push(edge);
    }
}
struct EvalFrame {
    node: QueryNode,
    dependencies: Edges,
    recording: bool,
}
thread_local! {
    static ACTIVE: Cell<(usize, *const TrackedQueryTable)> = const { Cell::new((0, std::ptr::null())) };
}
pub(crate) fn assert_frozen_read() {
    debug_assert!(
        ACTIVE.with(|n| n.get().0 == 0),
        "frozen read bypasses tracked dependencies"
    );
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct TrackedStats {
    pub validated: usize,
    pub recomputed: usize,
    pub green_after_recompute: usize,
    pub changed: usize,
    pub dependency_edges_visited: usize,
}

/// Dispatcher implementations may only read captured inputs and other queries.
/// Source capture, resolution and artifact I/O remain outside this boundary.
pub(crate) trait QueryProvider {
    fn compute(&self, table: &TrackedQueryTable, node: &QueryNode) -> Result<QueryValue>;
    fn contains_body(&self, _body: BodyId) -> bool {
        false
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct QueryCycle(Arc<str>);

#[derive(Default)]
pub(crate) struct TrackedQueryTable {
    revision: Revision,
    inputs: HashMap<InputNode, Input>,
    states: RefCell<HashMap<QueryNode, State>>,
    stack: RefCell<Vec<EvalFrame>>,
    stats: Cell<TrackedStats>,
    // A cycle leaves an unrecorded dependency. Poison the active evaluation,
    // even if a provider catches or reformats the error, until its root unwinds.
    cycle_error: RefCell<Option<Arc<str>>>,
}
impl TrackedQueryTable {
    pub(crate) fn revision(&self) -> Revision {
        self.revision
    }
    pub(crate) fn stats(&self) -> TrackedStats {
        self.stats.get()
    }
    pub(crate) fn matches_inputs(&self, values: &[(InputNode, QueryValue)]) -> bool {
        assert_frozen_read();
        values.iter().all(|(node, value)| {
            self.inputs
                .get(node)
                .is_some_and(|old| old.value.equivalent(value))
        })
    }

    /// Commit a fully accepted input batch. Validation happens before this API;
    /// rejected refreshes must never mutate the accepted table.
    pub(crate) fn accept(&mut self, values: Vec<(InputNode, QueryValue)>) -> Result<bool> {
        self.accept_batch(values, false)
    }
    pub(crate) fn replace_inputs(&mut self, values: Vec<(InputNode, QueryValue)>) -> Result<bool> {
        self.accept_batch(values, true)
    }
    fn accept_batch(
        &mut self,
        values: Vec<(InputNode, QueryValue)>,
        replace: bool,
    ) -> Result<bool> {
        assert_frozen_read();
        assert!(
            self.stack.get_mut().is_empty(),
            "input mutation during evaluation"
        );
        let mut unique = HashSet::with_capacity(values.len());
        anyhow::ensure!(
            values.iter().all(|(n, _)| unique.insert(n.clone())),
            "duplicate tracked input"
        );
        let removed = replace && self.inputs.keys().any(|node| !unique.contains(node));
        // Compare each payload once, including large captured source values.
        let changes: Vec<_> = values
            .into_iter()
            .filter(|(node, value)| {
                self.inputs
                    .get(node)
                    .is_none_or(|old| !old.value.equivalent(value))
            })
            .collect();
        if !removed && changes.is_empty() {
            return Ok(false);
        }
        let next = Revision(
            self.revision
                .0
                .checked_add(1)
                .context("revision overflow")?,
        );
        for (node, value) in changes {
            self.inputs.insert(
                node,
                Input {
                    value,
                    changed_at: next,
                    _durability: (),
                },
            );
        }
        if replace {
            self.inputs.retain(|node, _| unique.contains(node));
        }
        self.revision = next;
        Ok(true)
    }

    pub(crate) fn input(&self, node: &InputNode) -> Result<QueryValue> {
        self.assert_active_owner();
        // Missing inputs are programming errors, not memoizable query failures:
        // a failure without its dependency could otherwise remain green forever.
        let input = self.inputs.get(node).unwrap_or_else(|| {
            panic!(
                "missing captured dependency {node:?} at {:?}",
                self.revision
            )
        });
        self.record(DependencyNode::Input(node.clone()), input.changed_at);
        Ok(input.value.clone())
    }
    fn record(&self, node: DependencyNode, changed_at: Revision) {
        if let Some(frame) = self.stack.borrow_mut().last_mut().filter(|f| f.recording) {
            frame.dependencies.insert(Dependency {
                node,
                observed_changed_at: changed_at,
            });
        }
    }
    pub(crate) fn read(
        &self,
        provider: &impl QueryProvider,
        node: QueryNode,
    ) -> Result<QueryValue> {
        let memo = self.validate(provider, &node)?;
        self.record(DependencyNode::Query(node), memo.changed_at);
        memo.result.map_err(|error| anyhow::anyhow!("{error}"))
    }
    fn validate(&self, provider: &impl QueryProvider, node: &QueryNode) -> Result<Memo> {
        self.assert_active_owner();
        self.check_cycle()?;
        if let QueryNode::TypedBody(body) | QueryNode::LirBody(body) = node {
            debug_assert!(
                provider.contains_body(*body),
                "dangling BodyId in {node:?} at {:?}",
                self.revision
            );
        }
        let previous = match self.states.borrow().get(node) {
            Some(State::Computing { revision }) => {
                let error: Arc<str> = format!(
                    "E0800: compiler query cycle: {} -> {node:?} (revision {})",
                    self.stack
                        .borrow()
                        .iter()
                        .map(|f| format!("{:?}", f.node))
                        .collect::<Vec<_>>()
                        .join(" -> "),
                    revision.0
                )
                .into();
                *self.cycle_error.borrow_mut() = Some(Arc::clone(&error));
                return Err(QueryCycle(error).into());
            }
            Some(State::Ready(memo)) => {
                debug_assert!(
                    memo.changed_at <= memo.verified_at,
                    "invalid revision in {node:?}"
                );
                if memo.verified_at == self.revision {
                    return Ok(memo.clone());
                }
                Some(memo.clone())
            }
            None => None,
        };
        self.states.borrow_mut().insert(
            node.clone(),
            State::Computing {
                revision: self.revision,
            },
        );
        let depth = self.stack.borrow().len();
        self.stack.borrow_mut().push(EvalFrame {
            node: node.clone(),
            dependencies: Edges::default(),
            recording: false,
        });
        ACTIVE.with(|n| n.set((n.get().0 + 1, self)));
        struct Guard<'a> {
            table: &'a TrackedQueryTable,
            node: QueryNode,
            depth: usize,
        }
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                let mut stack = self.table.stack.borrow_mut();
                debug_assert_eq!(stack.len(), self.depth + 1, "eval-stack leak");
                debug_assert_eq!(stack.last().map(|f| &f.node), Some(&self.node));
                stack.pop();
                if self.depth == 0 {
                    self.table.cycle_error.borrow_mut().take();
                }
                ACTIVE.with(|n| {
                    let (depth, owner) = n.get();
                    n.set((depth - 1, if depth == 1 { std::ptr::null() } else { owner }));
                });
                let mut states = self.table.states.borrow_mut();
                if matches!(states.get(&self.node), Some(State::Computing { .. })) {
                    states.remove(&self.node);
                }
            }
        }
        let _guard = Guard {
            table: self,
            node: node.clone(),
            depth,
        };
        if let Some(old) = &previous {
            self.bump(|s| s.validated += 1);
            let mut green = true;
            for dependency in old.dependencies.iter() {
                self.bump(|s| s.dependency_edges_visited += 1);
                let changed_at = match &dependency.node {
                    DependencyNode::Input(input) => self.inputs.get(input).map(|v| v.changed_at),
                    DependencyNode::Query(query) => {
                        Some(self.validate(provider, query)?.changed_at)
                    }
                };
                if changed_at != Some(dependency.observed_changed_at) {
                    green = false;
                    break;
                }
            }
            if green {
                let mut memo = old.clone();
                memo.verified_at = self.revision;
                self.states
                    .borrow_mut()
                    .insert(node.clone(), State::Ready(memo.clone()));
                return Ok(memo);
            }
        }
        self.stack.borrow_mut().last_mut().unwrap().recording = true;
        self.bump(|s| s.recomputed += 1);
        let result = provider.compute(self, node);
        // Do not publish a failed memo (or a caught-error fallback) whose
        // dependency set is incomplete. Guard removes the Computing entry.
        self.check_cycle()?;
        let result = result.map_err(|e| Arc::<str>::from(format!("{e:#}")));
        let dependencies =
            std::mem::take(&mut self.stack.borrow_mut().last_mut().unwrap().dependencies).ordered;
        let equal = previous
            .as_ref()
            .is_some_and(|old| match (&old.result, &result) {
                (Ok(a), Ok(b)) => a.equivalent(b),
                (Err(a), Err(b)) => a == b,
                _ => false,
            });
        self.bump(|s| {
            if equal {
                s.green_after_recompute += 1;
            } else {
                s.changed += 1;
            }
        });
        let memo = Memo {
            changed_at: if equal {
                previous.as_ref().unwrap().changed_at
            } else {
                self.revision
            },
            verified_at: self.revision,
            // Keep the old Arc if the semantic result is identical.
            result: if equal {
                previous.unwrap().result
            } else {
                result
            },
            dependencies: dependencies.into(),
        };
        self.states
            .borrow_mut()
            .insert(node.clone(), State::Ready(memo.clone()));
        Ok(memo)
    }
    fn bump(&self, f: impl FnOnce(&mut TrackedStats)) {
        let mut s = self.stats.get();
        f(&mut s);
        self.stats.set(s);
    }
    fn check_cycle(&self) -> Result<()> {
        if let Some(error) = self.cycle_error.borrow().as_ref() {
            return Err(QueryCycle(Arc::clone(error)).into());
        }
        Ok(())
    }
    fn assert_active_owner(&self) {
        assert!(
            ACTIVE.with(|n| {
                let (depth, owner) = n.get();
                depth == 0 || std::ptr::eq(owner, self)
            }),
            "cross-database read would lose a dependency at {:?}",
            self.revision
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct RecoveringCycle {
        length: usize,
        catch: bool,
    }
    impl QueryProvider for RecoveringCycle {
        fn compute(&self, table: &TrackedQueryTable, node: &QueryNode) -> Result<QueryValue> {
            if node == &query(0) && *table.input(&input(0))?.get::<i64>() != 0 {
                return Ok(value(42));
            }
            let QueryNode::Parse(path) = node else {
                unreachable!()
            };
            let index: usize = path.file_stem().unwrap().to_str().unwrap().parse().unwrap();
            let result = table.read(self, query((index + 1) % self.length));
            if self.catch {
                Ok(result.unwrap_or_else(|_| value(99)))
            } else {
                result.context("dependent query failed")
            }
        }
    }
    #[test]
    fn cycle_failures_recover_after_input_change_from_either_entry() {
        for length in [2, 16, 64] {
            for entry in [0, 1] {
                let mut table = TrackedQueryTable::default();
                let provider = RecoveringCycle {
                    length,
                    catch: false,
                };
                table.accept(vec![(input(0), value(0))]).unwrap();
                assert!(
                    table
                        .read(&provider, query(0))
                        .err()
                        .unwrap()
                        .to_string()
                        .contains("E0800")
                );
                assert!(table.states.borrow().is_empty());
                assert_eq!(table.stats().recomputed, length);
                table.accept(vec![(input(0), value(1))]).unwrap();
                assert_eq!(
                    *table.read(&provider, query(entry)).unwrap().get::<i64>(),
                    42
                );
                assert_eq!(
                    *table
                        .read(&provider, query(1 - entry))
                        .unwrap()
                        .get::<i64>(),
                    42
                );
                let stats = table.stats();
                assert_eq!(stats.recomputed, 2 * length);
                table.read(&provider, query(entry)).unwrap();
                assert_eq!(table.stats(), stats);
                // Now introduce a cycle while validating previously green memos.
                table.accept(vec![(input(0), value(0))]).unwrap();
                assert!(table.read(&provider, query(0)).is_err());
                assert!(table.states.borrow().is_empty());
                table.accept(vec![(input(0), value(1))]).unwrap();
                assert_eq!(*table.read(&provider, query(1)).unwrap().get::<i64>(), 42);
                assert!(table.stack.borrow().is_empty());
                assert_frozen_read();
            }
        }
    }
    #[test]
    fn caught_cycle_cannot_publish_success_with_missing_dependencies() {
        let mut table = TrackedQueryTable::default();
        let provider = RecoveringCycle {
            length: 2,
            catch: true,
        };
        table.accept(vec![(input(0), value(0))]).unwrap();
        for _ in 0..2 {
            assert!(table.read(&provider, query(0)).is_err());
            assert!(table.states.borrow().is_empty());
        }
        table.accept(vec![(input(0), value(1))]).unwrap();
        assert_eq!(*table.read(&provider, query(1)).unwrap().get::<i64>(), 42);
    }
    fn value(n: i64) -> QueryValue {
        QueryValue::new(n, ResultFingerprint::bytes(&n.to_le_bytes()))
    }
    fn input(n: usize) -> InputNode {
        InputNode::Source(format!("{n}.wi").into())
    }
    fn query(n: usize) -> QueryNode {
        QueryNode::Parse(format!("{n}.wi").into())
    }
    struct Graph {
        edges: HashMap<QueryNode, Vec<DependencyNode>>,
        parity: bool,
        fail: bool,
    }
    impl QueryProvider for Graph {
        fn compute(&self, table: &TrackedQueryTable, node: &QueryNode) -> Result<QueryValue> {
            let mut total = 0;
            for dep in &self.edges[node] {
                let v = match dep {
                    DependencyNode::Input(i) => table.input(i)?,
                    DependencyNode::Query(q) => table.read(self, q.clone())?,
                };
                total += *v.get::<i64>();
            }
            anyhow::ensure!(!self.fail || total >= 0, "negative input");
            Ok(value(if self.parity { total % 2 } else { total }))
        }
    }
    fn graph(edges: Vec<(usize, Vec<DependencyNode>)>) -> Graph {
        Graph {
            edges: edges.into_iter().map(|(n, e)| (query(n), e)).collect(),
            parity: false,
            fail: false,
        }
    }
    fn q(n: usize) -> DependencyNode {
        DependencyNode::Query(query(n))
    }
    fn i(n: usize) -> DependencyNode {
        DependencyNode::Input(input(n))
    }
    #[test]
    fn nested_backdating_and_same_revision_fast_path() {
        let mut table = TrackedQueryTable::default();
        let mut graph = graph(vec![(0, vec![q(1)]), (1, vec![i(0), i(0), i(0)])]);
        graph.parity = true;
        table.accept(vec![(input(0), value(1))]).unwrap();
        let first = table.read(&graph, query(0)).unwrap();
        let initial = table.stats();
        assert_eq!(initial.recomputed, 2);
        table.read(&graph, query(0)).unwrap();
        assert_eq!(table.stats(), initial);
        assert!(!table.accept(vec![(input(0), value(1))]).unwrap());
        assert_eq!(table.revision(), Revision(1));
        table.accept(vec![(input(0), value(3))]).unwrap();
        let second = table.read(&graph, query(0)).unwrap();
        assert!(Arc::ptr_eq(&first.value, &second.value));
        assert_eq!(table.stats().recomputed, 3); // Only B recomputed.
        assert_eq!(table.stats().green_after_recompute, 1);
        assert_eq!(table.stats().dependency_edges_visited, 2); // Duplicate X reads deduped.
        table.accept(vec![(input(0), value(4))]).unwrap();
        assert_eq!(*table.read(&graph, query(0)).unwrap().get::<i64>(), 0);
        assert_eq!(table.stats().recomputed, 5);
    }
    #[test]
    fn failures_track_dependencies_and_recover() {
        let mut table = TrackedQueryTable::default();
        let mut graph = graph(vec![(0, vec![q(1)]), (1, vec![i(0)])]);
        graph.fail = true;
        table.accept(vec![(input(0), value(-1))]).unwrap();
        assert!(table.read(&graph, query(0)).is_err());
        let stats = table.stats();
        assert!(table.read(&graph, query(0)).is_err());
        assert_eq!(table.stats(), stats);
        table.accept(vec![(input(1), value(99))]).unwrap();
        assert!(table.read(&graph, query(0)).is_err());
        assert_eq!(table.stats().recomputed, 2);
        table.accept(vec![(input(0), value(1))]).unwrap();
        assert_eq!(*table.read(&graph, query(0)).unwrap().get::<i64>(), 1);
        assert_eq!(table.stats().recomputed, 4);
    }
    #[test]
    fn collisions_do_not_backdate_unequal_values() {
        let mut table = TrackedQueryTable::default();
        let fingerprint = ResultFingerprint(0);
        table
            .accept(vec![(input(0), QueryValue::new(1_i64, fingerprint))])
            .unwrap();
        let graph = graph(vec![(0, vec![i(0)])]);
        table.read(&graph, query(0)).unwrap();
        table
            .accept(vec![(input(0), QueryValue::new(2_i64, fingerprint))])
            .unwrap();
        assert_eq!(*table.read(&graph, query(0)).unwrap().get::<i64>(), 2);
        assert_eq!(table.revision(), Revision(2));
    }
    #[test]
    fn result_collision_and_diagnostic_changes_propagate() {
        struct Diagnostics;
        impl QueryProvider for Diagnostics {
            fn compute(&self, table: &TrackedQueryTable, node: &QueryNode) -> Result<QueryValue> {
                let result = if node == &query(0) {
                    table.read(self, query(1))?
                } else {
                    let input = table.input(&input(0))?;
                    QueryValue::new(
                        serde_json::json!({ "result": 42, "diagnostics": [{ "code": "E0800", "message": input.get::<i64>().to_string(), "severity": "error", "labels": [], "fixes": [], "notes": [] }] }),
                        ResultFingerprint(0),
                    )
                };
                Ok(result)
            }
        }
        let mut table = TrackedQueryTable::default();
        table.accept(vec![(input(0), value(1))]).unwrap();
        let first = table.read(&Diagnostics, query(0)).unwrap();
        table.accept(vec![(input(0), value(2))]).unwrap();
        let second = table.read(&Diagnostics, query(0)).unwrap();
        assert_ne!(
            first.get::<serde_json::Value>(),
            second.get::<serde_json::Value>()
        );
        assert_eq!(table.stats().recomputed, 4);
        assert_eq!(table.stats().green_after_recompute, 0);
    }
    #[test]
    fn cross_database_reads_cannot_escape_dependency_recording() {
        struct CrossDatabase(TrackedQueryTable);
        impl QueryProvider for CrossDatabase {
            fn compute(&self, _: &TrackedQueryTable, _: &QueryNode) -> Result<QueryValue> {
                self.0.input(&input(0))
            }
        }
        let mut other = TrackedQueryTable::default();
        other.accept(vec![(input(0), value(1))]).unwrap();
        let table = TrackedQueryTable::default();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || table.read(&CrossDatabase(other), query(0))
            ))
            .is_err()
        );
        assert_frozen_read();
        assert!(table.stack.borrow().is_empty());
    }
    #[test]
    fn reachable_diamond_and_large_dedup_have_linear_counts() {
        for n in [16, 64, 256] {
            let mut table = TrackedQueryTable::default();
            table
                .accept(vec![(input(0), value(1)), (input(1), value(1))])
                .unwrap();
            let mut edges = vec![(0, (1..=n).flat_map(|j| [q(j), q(j)]).collect())];
            for j in 1..=n {
                edges.push((j, vec![i(0)]));
            }
            for j in n + 1..=n * 4 {
                edges.push((j, vec![i(1)]));
            }
            let graph = graph(edges);
            for j in 0..=n * 4 {
                table.read(&graph, query(j)).unwrap();
            }
            let before = table.stats();
            table.accept(vec![(input(1), value(2))]).unwrap();
            table.read(&graph, query(0)).unwrap();
            let after = table.stats();
            assert_eq!(after.recomputed, before.recomputed);
            assert_eq!(after.validated - before.validated, n + 1);
            assert_eq!(
                after.dependency_edges_visited - before.dependency_edges_visited,
                2 * n
            );
            eprintln!(
                "tracked fanout={n} unrelated={} validated={} edges={}",
                3 * n,
                n + 1,
                2 * n
            );
        }
    }
    #[test]
    fn cycle_and_panics_release_frames_and_states() {
        let table = TrackedQueryTable::default();
        let graph = graph(vec![(0, vec![q(1)]), (1, vec![q(0)])]);
        let error = table.read(&graph, query(0)).err().unwrap();
        assert!(error.to_string().contains("E0800: compiler query cycle:"));
        assert!(table.stack.borrow().is_empty());
        assert_frozen_read();
        struct Panics;
        impl QueryProvider for Panics {
            fn compute(&self, _: &TrackedQueryTable, _: &QueryNode) -> Result<QueryValue> {
                panic!("test")
            }
        }
        for _ in 0..2 {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                    || table.read(&Panics, query(8))
                ))
                .is_err()
            );
            assert!(table.stack.borrow().is_empty());
            assert!(!table.states.borrow().contains_key(&query(8)));
            assert_frozen_read();
        }
    }
    #[test]
    fn duplicate_batch_is_atomic() {
        let mut table = TrackedQueryTable::default();
        assert!(
            table
                .accept(vec![(input(0), value(1)), (input(0), value(2))])
                .is_err()
        );
        assert_eq!(table.revision(), Revision(0));
        assert!(table.inputs.is_empty());
    }
    #[test]
    fn replacing_inputs_prunes_removed_files_and_advances_once() {
        let mut table = TrackedQueryTable::default();
        table
            .replace_inputs(vec![(input(0), value(1)), (input(1), value(2))])
            .unwrap();
        table.replace_inputs(vec![(input(0), value(1))]).unwrap();
        assert_eq!(table.revision(), Revision(2));
        assert_eq!(table.inputs.len(), 1);
        assert_eq!(table.inputs[&input(0)].changed_at, Revision(1));
        assert!(!table.replace_inputs(vec![(input(0), value(1))]).unwrap());
    }
    #[test]
    fn deep_chain_validation_visits_each_reachable_edge_once() {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                for n in [16, 64, 256] {
                    let mut table = TrackedQueryTable::default();
                    table
                        .accept(vec![(input(0), value(1)), (input(1), value(9))])
                        .unwrap();
                    let mut edges: Vec<_> = (0..n).map(|j| (j, vec![q(j + 1)])).collect();
                    edges.push((n, vec![i(0)]));
                    let mut graph = graph(edges);
                    graph.parity = true;
                    table.read(&graph, query(0)).unwrap();
                    let before = table.stats();
                    table.accept(vec![(input(0), value(3))]).unwrap();
                    table.read(&graph, query(0)).unwrap();
                    let after = table.stats();
                    assert_eq!(after.recomputed - before.recomputed, 1);
                    assert_eq!(
                        after.dependency_edges_visited - before.dependency_edges_visited,
                        n + 1
                    );
                    assert_eq!(after.validated - before.validated, n + 1);
                    eprintln!(
                        "tracked depth={n} validated={} edges={} recomputed=1",
                        n + 1,
                        n + 1
                    );
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }
    #[test]
    fn branch_changes_replace_dependencies_even_when_result_backdates() {
        struct Branch;
        impl QueryProvider for Branch {
            fn compute(&self, table: &TrackedQueryTable, _: &QueryNode) -> Result<QueryValue> {
                let selector = *table.input(&input(0))?.get::<i64>();
                table.input(&input(if selector == 0 { 1 } else { 2 }))
            }
        }
        let mut table = TrackedQueryTable::default();
        table
            .accept(vec![
                (input(0), value(0)),
                (input(1), value(7)),
                (input(2), value(7)),
            ])
            .unwrap();
        let first = table.read(&Branch, query(0)).unwrap();
        table.accept(vec![(input(0), value(1))]).unwrap();
        let second = table.read(&Branch, query(0)).unwrap();
        assert!(Arc::ptr_eq(&first.value, &second.value));
        table.accept(vec![(input(1), value(8))]).unwrap();
        table.read(&Branch, query(0)).unwrap();
        assert_eq!(table.stats().recomputed, 2);
        table.accept(vec![(input(2), value(9))]).unwrap();
        assert_eq!(*table.read(&Branch, query(0)).unwrap().get::<i64>(), 9);
        assert_eq!(table.stats().recomputed, 3);
    }
    #[test]
    #[cfg(debug_assertions)]
    fn debug_guards_detect_read_bypass_missing_inputs_and_dangling_bodies() {
        struct Bypass;
        impl QueryProvider for Bypass {
            fn compute(&self, _: &TrackedQueryTable, _: &QueryNode) -> Result<QueryValue> {
                let old = super::super::query::QueryTable::<i32, i32>::default();
                old.ready(&0);
                Ok(value(0))
            }
        }
        let table = TrackedQueryTable::default();
        let catches = |f: &dyn Fn()| {
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err())
        };
        catches(&|| {
            let _ = table.read(&Bypass, query(0));
        });
        catches(&|| {
            let _ = table.input(&input(0));
        });
        catches(&|| {
            let _ = table.read(&Bypass, QueryNode::TypedBody(BodyId::fresh()));
        });
        assert!(table.stack.borrow().is_empty());
        assert_frozen_read();
    }
}
