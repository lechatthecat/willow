//! One monotone effect fixpoint over the shared call graph (willow-uqzx.1.3).
//!
//! Three analyses used to own three hand-rolled propagation loops: the
//! backend's recoverable-panic pass, the type checker's lock-effect pass behind
//! E2604, and the concurrency pass behind E0810. They agreed on nothing —
//! neither the key space, nor the lattice, nor what a missing fact means — so a
//! rule fixed in one kept surviving in the others.
//!
//! This module owns the propagation. A consumer supplies:
//!
//! * the bodies it claims to have walked,
//! * seed effects for what a body does *itself*, each with a witness it can put
//!   in a diagnostic,
//! * per-callee transmission masks, and
//! * what an edge it cannot see is worth.
//!
//! The lattice is [`RuntimeEffects`], the same one the runtime ABI rows carry,
//! so an edge into a runtime symbol is seeded from
//! [`crate::backend::abi::runtime_symbol`] rather than from a hand-maintained
//! list per analysis.
//!
//! # Fail-closed
//!
//! Every "I cannot see this" knob defaults to [`RuntimeEffects::ALL`]. A missing
//! fact is never read as safe; a consumer that can prove better must say so.
//!
//! # Why no SCC condensation
//!
//! Collapsing each strongly connected component to one summary would be wrong
//! here: transmission masks are per-edge, so two mutually recursive bodies
//! joined by a masked edge must not share a summary. `async` is exactly that
//! case — calling an `async fn` creates a Task without suspending the caller, so
//! the edge drops [`RuntimeEffects::MAY_SUSPEND`] in one direction only. The
//! worklist below keeps each edge's mask. [`cycle_members`] still exposes the
//! SCC pass on its own, because "is in a call cycle" is a seed for E0810.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use crate::semantic::call_graph::CallGraph;
use crate::semantic::ids::FunctionId;

pub use willow_abi::RuntimeEffects;

/// What one body can do, plus the witness that proves each effect.
///
/// A witness is whatever the consumer needs to point at in a diagnostic: the
/// owning [`FunctionId`], a [`crate::diagnostics::span::Span`], a reason code.
/// Witnesses join by `min`, which makes the result independent of hash
/// iteration order — the property the lock analysis previously hand-rolled as
/// "the lexicographically smallest reachable owner".
///
/// An effect can be set with no witness: `has_unknown` and an unanalyzed callee
/// are real effects with no source location to blame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectSummary<W> {
    effects: RuntimeEffects,
    /// Keyed by effect bit index, never by the bit mask, so one entry is one
    /// effect. Only bits present in `effects` appear.
    witnesses: BTreeMap<u32, W>,
}

impl<W> Default for EffectSummary<W> {
    fn default() -> Self {
        Self {
            effects: RuntimeEffects::NONE,
            witnesses: BTreeMap::new(),
        }
    }
}

impl<W: Clone + Ord> EffectSummary<W> {
    /// A leaf fact: these effects, proven by this witness.
    pub fn new(effects: RuntimeEffects, witness: Option<W>) -> Self {
        let mut summary = Self::default();
        summary.add(effects, witness.as_ref());
        summary
    }

    pub fn effects(&self) -> RuntimeEffects {
        self.effects
    }

    /// True when every effect in `effects` is present.
    pub fn contains(&self, effects: RuntimeEffects) -> bool {
        self.effects.contains(effects)
    }

    /// True when any effect in `effects` is present. This is the predicate a
    /// diagnostic wants: E2604 fires on block *or* suspend, not on both.
    pub fn intersects(&self, effects: RuntimeEffects) -> bool {
        self.effects.intersects(effects)
    }

    pub fn is_empty(&self) -> bool {
        self.effects.is_empty()
    }

    /// The witness for the lowest-numbered effect this summary has in common
    /// with `effects`, if that effect has one.
    pub fn witness(&self, effects: RuntimeEffects) -> Option<&W> {
        let matching = self.effects.intersection(effects);
        (0..RuntimeEffects::BIT_COUNT)
            .filter(|bit| matching.contains(RuntimeEffects::from_bit(*bit)))
            .find_map(|bit| self.witnesses.get(&bit))
    }

    /// Union in a leaf fact. Returns whether anything changed, which is what
    /// drives the worklist.
    fn add(&mut self, effects: RuntimeEffects, witness: Option<&W>) -> bool {
        if effects.is_empty() {
            return false;
        }
        let mut changed = !self.effects.contains(effects);
        self.effects = self.effects.union(effects);
        let Some(witness) = witness else {
            return changed;
        };
        for bit in 0..RuntimeEffects::BIT_COUNT {
            if !effects.contains(RuntimeEffects::from_bit(bit)) {
                continue;
            }
            changed |= self.record_witness(bit, witness);
        }
        changed
    }

    /// Union in a callee's summary through an edge that transmits `mask`.
    fn join(&mut self, other: &Self, mask: RuntimeEffects) -> bool {
        let effects = other.effects.intersection(mask);
        if effects.is_empty() {
            return false;
        }
        let mut changed = !self.effects.contains(effects);
        self.effects = self.effects.union(effects);
        for (bit, witness) in &other.witnesses {
            if !effects.contains(RuntimeEffects::from_bit(*bit)) {
                continue;
            }
            changed |= self.record_witness(*bit, witness);
        }
        changed
    }

    /// Keep the smaller witness. Monotone: a witness only ever decreases, and
    /// the candidate set is finite, so the fixpoint terminates.
    fn record_witness(&mut self, bit: u32, witness: &W) -> bool {
        match self.witnesses.get(&bit) {
            Some(current) if *current <= *witness => false,
            _ => {
                self.witnesses.insert(bit, witness.clone());
                true
            }
        }
    }
}

/// One effect-propagation problem, ready to [`EffectProblem::solve`].
///
/// Construct with [`EffectProblem::new`], which starts fail-closed, then relax
/// only the knobs the consumer can actually prove.
#[derive(Debug, Clone)]
pub struct EffectProblem<W> {
    bodies: BTreeSet<FunctionId>,
    seeds: BTreeMap<FunctionId, EffectSummary<W>>,
    transmit: BTreeMap<FunctionId, RuntimeEffects>,
    default_transmit: RuntimeEffects,
    external_callee: RuntimeEffects,
    unknown_callee: RuntimeEffects,
    missing_body: RuntimeEffects,
}

impl<W: Clone + Ord> Default for EffectProblem<W> {
    fn default() -> Self {
        Self::new()
    }
}

impl<W: Clone + Ord> EffectProblem<W> {
    /// A problem whose every unknown is worth every effect.
    pub fn new() -> Self {
        Self {
            bodies: BTreeSet::new(),
            seeds: BTreeMap::new(),
            transmit: BTreeMap::new(),
            default_transmit: RuntimeEffects::ALL,
            external_callee: RuntimeEffects::ALL,
            unknown_callee: RuntimeEffects::ALL,
            missing_body: RuntimeEffects::ALL,
        }
    }

    /// Declare that `id` is a body this consumer walked. A declared body with
    /// no node in the call graph was not actually walked, and picks up
    /// [`EffectProblem::missing_body`].
    pub fn body(mut self, id: FunctionId) -> Self {
        self.bodies.insert(id);
        self
    }

    /// Record what `id` does on its own, independent of what it calls.
    /// Repeated seeds for one id union, so a consumer can report a panic
    /// witness and a suspend witness separately.
    pub fn seed(mut self, id: FunctionId, effects: RuntimeEffects, witness: Option<W>) -> Self {
        self.seeds
            .entry(id)
            .or_default()
            .add(effects, witness.as_ref());
        self
    }

    /// Restrict what an edge *into* `callee` carries back to its callers.
    ///
    /// The masked effects still belong to the callee's own summary; they just do
    /// not reach the caller through this call. Calling an `async fn` is the
    /// motivating case: it allocates a Task in the caller, but the callee's
    /// suspension happens on the Task, not on the caller's stack.
    pub fn transmit(mut self, callee: FunctionId, mask: RuntimeEffects) -> Self {
        self.transmit.insert(callee, mask);
        self
    }

    /// The mask for a callee with no explicit entry. Defaults to
    /// [`RuntimeEffects::ALL`].
    pub fn default_transmit(mut self, mask: RuntimeEffects) -> Self {
        self.default_transmit = mask;
        self
    }

    /// What an edge to an id outside the problem is worth. A consumer with
    /// facts about separately compiled callees should instead
    /// [`EffectProblem::seed`] them as leaves, which is both precise and
    /// self-documenting.
    pub fn external_callee(mut self, effects: RuntimeEffects) -> Self {
        self.external_callee = effects;
        self
    }

    /// What [`crate::semantic::call_graph::CallSites::has_unknown`] is worth:
    /// the body calls something the graph could not name at all.
    pub fn unknown_callee(mut self, effects: RuntimeEffects) -> Self {
        self.unknown_callee = effects;
        self
    }

    /// What a declared body missing from the call graph is worth.
    pub fn missing_body(mut self, effects: RuntimeEffects) -> Self {
        self.missing_body = effects;
        self
    }

    /// Run the fixpoint over `graph`.
    ///
    /// The result covers every declared body, every graph node, and every
    /// seeded id. Iteration is a worklist over reverse edges, so each edge is
    /// revisited only when its callee's summary actually grows.
    pub fn solve(self, graph: &CallGraph) -> EffectFacts<W> {
        let mut universe = self.bodies.clone();
        universe.extend(graph.ids().cloned());
        universe.extend(self.seeds.keys().cloned());
        let nodes: Vec<FunctionId> = universe.into_iter().collect();
        let index: HashMap<&FunctionId, usize> = nodes
            .iter()
            .enumerate()
            .map(|(position, id)| (id, position))
            .collect();

        let mut summaries: Vec<EffectSummary<W>> = vec![EffectSummary::default(); nodes.len()];
        // (callee, mask) per caller, and the reverse index the worklist walks.
        let mut edges: Vec<Vec<(usize, RuntimeEffects)>> = vec![Vec::new(); nodes.len()];
        let mut callers: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];

        for (position, id) in nodes.iter().enumerate() {
            if let Some(seed) = self.seeds.get(id) {
                summaries[position].join(seed, RuntimeEffects::ALL);
            }
            let Some(sites) = graph.get(id) else {
                if self.bodies.contains(id) {
                    summaries[position].add(self.missing_body, None);
                }
                continue;
            };
            if sites.has_unknown {
                summaries[position].add(self.unknown_callee, None);
            }
            for target in &sites.targets {
                let mask = self
                    .transmit
                    .get(target)
                    .copied()
                    .unwrap_or(self.default_transmit);
                match index.get(target) {
                    Some(&callee) => {
                        edges[position].push((callee, mask));
                        callers[callee].push(position);
                    }
                    None => {
                        summaries[position].add(self.external_callee.intersection(mask), None);
                    }
                }
            }
        }

        // Seed the worklist with every node: a leaf's summary has to reach its
        // callers even though nothing ever "changes" about the leaf.
        let mut queued: HashSet<usize> = (0..nodes.len()).collect();
        let mut work: VecDeque<usize> = (0..nodes.len()).collect();
        while let Some(position) = work.pop_front() {
            queued.remove(&position);
            let mut changed = false;
            // Lift the caller's summary out so the callees can be read in
            // place. A self-edge is skipped rather than handled: joining a
            // summary with itself under any mask can only re-add effects and
            // witnesses it already has.
            let mut summary = std::mem::take(&mut summaries[position]);
            for &(callee, mask) in &edges[position] {
                if callee == position {
                    continue;
                }
                changed |= summary.join(&summaries[callee], mask);
            }
            summaries[position] = summary;
            if !changed {
                continue;
            }
            for &caller in &callers[position] {
                if caller != position && queued.insert(caller) {
                    work.push_back(caller);
                }
            }
        }

        EffectFacts {
            summaries: nodes.into_iter().zip(summaries).collect(),
        }
    }
}

/// The solved summaries, keyed by [`FunctionId`].
#[derive(Debug, Clone)]
pub struct EffectFacts<W> {
    summaries: BTreeMap<FunctionId, EffectSummary<W>>,
}

impl<W: Clone + Ord> EffectFacts<W> {
    pub fn get(&self, id: &FunctionId) -> Option<&EffectSummary<W>> {
        self.summaries.get(id)
    }

    /// Fail-closed lookup: an id with no summary is assumed to do `effects`.
    pub fn intersects(&self, id: &FunctionId, effects: RuntimeEffects) -> bool {
        self.summaries
            .get(id)
            .is_none_or(|summary| summary.intersects(effects))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&FunctionId, &EffectSummary<W>)> {
        self.summaries.iter()
    }

    pub fn len(&self) -> usize {
        self.summaries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.summaries.is_empty()
    }
}

/// Every id in `graph` that belongs to a call cycle: a strongly connected
/// component with more than one member, or a node that calls itself.
///
/// This is a graph property rather than an effect, which is why it is a seed
/// helper and not part of the fixpoint. E0810 needs it because unbounded work
/// needs no `while`: `fib(40)` monopolizes a worker with nothing but recursion.
///
/// `extra` contributes nodes that have no call sites of their own but must
/// still be numbered, so an edge to a leaf declared elsewhere is not mistaken
/// for an edge out of the graph.
pub fn cycle_members<'a>(
    graph: &CallGraph,
    extra: impl IntoIterator<Item = &'a FunctionId>,
) -> BTreeSet<FunctionId> {
    let mut universe: BTreeSet<FunctionId> = graph.ids().cloned().collect();
    universe.extend(extra.into_iter().cloned());
    let nodes: Vec<FunctionId> = universe.into_iter().collect();
    let index: HashMap<&FunctionId, usize> = nodes
        .iter()
        .enumerate()
        .map(|(position, id)| (id, position))
        .collect();

    let adjacency: Vec<Vec<usize>> = nodes
        .iter()
        .map(|id| {
            let Some(sites) = graph.get(id) else {
                return Vec::new();
            };
            let mut edges: Vec<usize> = sites
                .targets
                .iter()
                .filter_map(|target| index.get(target).copied())
                .collect();
            edges.sort_unstable();
            edges.dedup();
            edges
        })
        .collect();

    let component = strongly_connected_components(&adjacency);
    let mut component_size = vec![0usize; nodes.len()];
    for &id in &component {
        component_size[id] += 1;
    }

    nodes
        .into_iter()
        .enumerate()
        .filter(|(position, _)| {
            component_size[component[*position]] > 1 || adjacency[*position].contains(position)
        })
        .map(|(_, id)| id)
        .collect()
}

/// Tarjan's strongly-connected components over `adjacency`, returning each
/// node's component index.
///
/// Iterative on purpose: one caller of this exists to catch unbounded
/// recursion, so it must not itself recurse once per call-graph edge and
/// overflow the compiler's own stack on a deeply chained program.
pub fn strongly_connected_components(adjacency: &[Vec<usize>]) -> Vec<usize> {
    const UNVISITED: usize = usize::MAX;
    let node_count = adjacency.len();
    let mut visit_index = vec![UNVISITED; node_count];
    let mut lowlink = vec![0usize; node_count];
    let mut on_stack = vec![false; node_count];
    let mut component = vec![UNVISITED; node_count];
    let mut component_stack: Vec<usize> = Vec::new();
    // (node, index of the next outgoing edge to explore)
    let mut work: Vec<(usize, usize)> = Vec::new();
    let mut next_index = 0usize;
    let mut next_component = 0usize;

    for root in 0..node_count {
        if visit_index[root] != UNVISITED {
            continue;
        }
        visit_index[root] = next_index;
        lowlink[root] = next_index;
        next_index += 1;
        component_stack.push(root);
        on_stack[root] = true;
        work.push((root, 0));

        while let Some((node, edge)) = work.pop() {
            if let Some(&callee) = adjacency[node].get(edge) {
                work.push((node, edge + 1));
                if visit_index[callee] == UNVISITED {
                    visit_index[callee] = next_index;
                    lowlink[callee] = next_index;
                    next_index += 1;
                    component_stack.push(callee);
                    on_stack[callee] = true;
                    work.push((callee, 0));
                } else if on_stack[callee] {
                    lowlink[node] = lowlink[node].min(visit_index[callee]);
                }
                continue;
            }
            // Every edge explored: close `node`, then fold its lowlink into the
            // caller frame that is now on top of the work stack.
            if lowlink[node] == visit_index[node] {
                while let Some(member) = component_stack.pop() {
                    on_stack[member] = false;
                    component[member] = next_component;
                    if member == node {
                        break;
                    }
                }
                next_component += 1;
            }
            if let Some(&(caller, _)) = work.last() {
                lowlink[caller] = lowlink[caller].min(lowlink[node]);
            }
        }
    }

    component
}

#[cfg(test)]
mod tests {
    //! Test perspectives for the shared effect fixpoint (willow-uqzx.1.3).
    //!
    //! e01 an empty problem solves to no facts
    //! e02 a seed with no graph node is its own summary
    //! e03 a direct edge carries the callee's effect to the caller
    //! e04 the effect travels the whole length of a chain
    //! e05 a chain with no seed anywhere stays provably pure
    //! e06 a self-recursive body with no seed stays provably pure
    //! e07 a mutually recursive pair with no seed stays provably pure
    //! e08 one seed anywhere in a cycle reaches every member
    //! e09 `has_unknown` contributes the unknown-callee effects
    //! e10 a consumer that can prove unknown callees harmless says so
    //! e11 a declared body with no graph node picks up `missing_body`
    //! e12 an edge leaving the universe picks up `external_callee`
    //! e13 the transmission mask also applies to an external callee
    //! e14 a mask can pass allocation while blocking suspension (the async rule)
    //! e15 a masked effect still belongs to the callee's own summary
    //! e16 `default_transmit` applies where no per-callee mask exists
    //! e17 witnesses join by `min`, so the smaller one wins
    //! e18 each effect bit carries its own witness
    //! e19 a witness survives transitive propagation
    //! e20 an effect with no witness is still reported
    //! e21 insertion order does not change the result
    //! e22 `EffectFacts::intersects` is fail-closed for an unknown id
    //! e23 `witness` answers for the lowest queried bit that is present
    //! e24 a diamond yields one summary whose witness is the minimum
    //! e25 repeated seeds for one id union rather than replace
    //! e26 `cycle_members` finds self, mutual, and longer cycles
    //! e27 `cycle_members` does not mistake a diamond for a cycle
    //! e28 a deep chain does not overflow the compiler's own stack

    use super::*;
    use crate::semantic::call_graph::CallSites;
    use crate::semantic::ids::TypeId;

    const PANIC: RuntimeEffects = RuntimeEffects::MAY_PANIC;
    const BLOCK: RuntimeEffects = RuntimeEffects::MAY_BLOCK;
    const SUSPEND: RuntimeEffects = RuntimeEffects::MAY_SUSPEND;
    const ALLOC: RuntimeEffects = RuntimeEffects::MAY_ALLOCATE;
    const NONE: RuntimeEffects = RuntimeEffects::NONE;

    fn id(name: &str) -> FunctionId {
        FunctionId::free(name)
    }

    /// A graph node whose body calls `targets` and resolved every call site.
    fn calls(graph: &mut CallGraph, caller: &str, targets: &[&str]) {
        graph.merge(
            id(caller),
            CallSites {
                targets: targets.iter().map(|name| id(name)).collect(),
                has_unknown: false,
            },
        );
    }

    fn unresolved(graph: &mut CallGraph, caller: &str) {
        graph.merge(
            id(caller),
            CallSites {
                targets: BTreeSet::new(),
                has_unknown: true,
            },
        );
    }

    /// A problem that trusts what it cannot see, so a test isolates one knob.
    fn permissive<W: Clone + Ord>() -> EffectProblem<W> {
        EffectProblem::new()
            .external_callee(NONE)
            .unknown_callee(NONE)
            .missing_body(NONE)
    }

    fn effects_of<W: Clone + Ord>(facts: &EffectFacts<W>, name: &str) -> RuntimeEffects {
        facts
            .get(&id(name))
            .map(EffectSummary::effects)
            .unwrap_or(NONE)
    }

    #[test]
    fn e01_empty_problem_solves_to_no_facts() {
        let facts = permissive::<&'static str>().solve(&CallGraph::default());
        assert!(facts.is_empty());
        assert_eq!(facts.len(), 0);
    }

    #[test]
    fn e02_a_seed_without_a_graph_node_is_its_own_summary() {
        let facts = permissive()
            .seed(id("leaf"), PANIC, Some("boom"))
            .solve(&CallGraph::default());
        let summary = facts.get(&id("leaf")).expect("seeded id is in the facts");
        assert_eq!(summary.effects(), PANIC);
        assert_eq!(summary.witness(PANIC), Some(&"boom"));
    }

    #[test]
    fn e03_a_direct_edge_carries_the_callee_effect() {
        let mut graph = CallGraph::default();
        calls(&mut graph, "caller", &["callee"]);
        calls(&mut graph, "callee", &[]);
        let facts = permissive::<&'static str>()
            .seed(id("callee"), PANIC, None)
            .solve(&graph);
        assert_eq!(effects_of(&facts, "caller"), PANIC);
    }

    #[test]
    fn e04_the_effect_travels_the_whole_chain() {
        let mut graph = CallGraph::default();
        calls(&mut graph, "a", &["b"]);
        calls(&mut graph, "b", &["c"]);
        calls(&mut graph, "c", &["d"]);
        calls(&mut graph, "d", &[]);
        let facts = permissive::<&'static str>()
            .seed(id("d"), BLOCK, None)
            .solve(&graph);
        for name in ["a", "b", "c", "d"] {
            assert_eq!(effects_of(&facts, name), BLOCK, "{name}");
        }
    }

    #[test]
    fn e05_a_chain_with_no_seed_stays_pure() {
        let mut graph = CallGraph::default();
        calls(&mut graph, "a", &["b"]);
        calls(&mut graph, "b", &["c"]);
        calls(&mut graph, "c", &[]);
        let facts = permissive::<&'static str>().solve(&graph);
        for name in ["a", "b", "c"] {
            assert!(effects_of(&facts, name).is_empty(), "{name}");
        }
    }

    #[test]
    fn e06_self_recursion_without_a_seed_stays_pure() {
        let mut graph = CallGraph::default();
        calls(&mut graph, "spin", &["spin"]);
        let facts = permissive::<&'static str>().solve(&graph);
        assert!(effects_of(&facts, "spin").is_empty());
    }

    #[test]
    fn e07_mutual_recursion_without_a_seed_stays_pure() {
        let mut graph = CallGraph::default();
        calls(&mut graph, "ping", &["pong"]);
        calls(&mut graph, "pong", &["ping"]);
        let facts = permissive::<&'static str>().solve(&graph);
        assert!(effects_of(&facts, "ping").is_empty());
        assert!(effects_of(&facts, "pong").is_empty());
    }

    #[test]
    fn e08_one_seed_in_a_cycle_reaches_every_member() {
        let mut graph = CallGraph::default();
        calls(&mut graph, "ping", &["pong"]);
        calls(&mut graph, "pong", &["ping", "hazard"]);
        calls(&mut graph, "hazard", &[]);
        let facts = permissive::<&'static str>()
            .seed(id("hazard"), SUSPEND, Some("await"))
            .solve(&graph);
        assert_eq!(effects_of(&facts, "ping"), SUSPEND);
        assert_eq!(effects_of(&facts, "pong"), SUSPEND);
        assert_eq!(
            facts.get(&id("ping")).unwrap().witness(SUSPEND),
            Some(&"await")
        );
    }

    #[test]
    fn e09_has_unknown_contributes_the_unknown_callee_effects() {
        let mut graph = CallGraph::default();
        unresolved(&mut graph, "indirect");
        calls(&mut graph, "caller", &["indirect"]);
        let facts = permissive::<&'static str>()
            .unknown_callee(PANIC)
            .solve(&graph);
        assert_eq!(effects_of(&facts, "indirect"), PANIC);
        assert_eq!(effects_of(&facts, "caller"), PANIC);
    }

    #[test]
    fn e10_a_consumer_may_prove_unknown_callees_harmless() {
        let mut graph = CallGraph::default();
        unresolved(&mut graph, "indirect");
        let facts = permissive::<&'static str>().solve(&graph);
        assert!(effects_of(&facts, "indirect").is_empty());
    }

    #[test]
    fn e11_a_declared_body_missing_from_the_graph_is_conservative() {
        let facts = permissive::<&'static str>()
            .missing_body(RuntimeEffects::ALL)
            .body(id("never_walked"))
            .solve(&CallGraph::default());
        assert_eq!(effects_of(&facts, "never_walked"), RuntimeEffects::ALL);
    }

    #[test]
    fn e12_an_edge_leaving_the_universe_is_conservative() {
        let mut graph = CallGraph::default();
        calls(&mut graph, "caller", &["separately_compiled"]);
        let facts = permissive::<&'static str>()
            .external_callee(PANIC.union(BLOCK))
            .solve(&graph);
        assert_eq!(effects_of(&facts, "caller"), PANIC.union(BLOCK));
        assert!(facts.get(&id("separately_compiled")).is_none());
    }

    #[test]
    fn e13_the_mask_also_applies_to_an_external_callee() {
        let mut graph = CallGraph::default();
        calls(&mut graph, "caller", &["separately_compiled"]);
        let facts = permissive::<&'static str>()
            .external_callee(PANIC.union(BLOCK))
            .transmit(id("separately_compiled"), BLOCK)
            .solve(&graph);
        assert_eq!(effects_of(&facts, "caller"), BLOCK);
    }

    #[test]
    fn e14_a_mask_passes_allocation_while_blocking_suspension() {
        // The eager-async rule: calling an `async fn` allocates a Task in the
        // caller, but the callee suspends on the Task rather than on the
        // caller's stack.
        let mut graph = CallGraph::default();
        calls(&mut graph, "caller", &["spawned"]);
        calls(&mut graph, "spawned", &[]);
        let facts = permissive::<&'static str>()
            .seed(id("spawned"), SUSPEND.union(ALLOC), None)
            .transmit(id("spawned"), RuntimeEffects::ALL.difference(SUSPEND))
            .solve(&graph);
        assert_eq!(effects_of(&facts, "caller"), ALLOC);
    }

    #[test]
    fn e15_a_masked_effect_still_belongs_to_the_callee() {
        let mut graph = CallGraph::default();
        calls(&mut graph, "caller", &["spawned"]);
        calls(&mut graph, "spawned", &[]);
        let facts = permissive::<&'static str>()
            .seed(id("spawned"), SUSPEND, None)
            .transmit(id("spawned"), NONE)
            .solve(&graph);
        assert_eq!(effects_of(&facts, "spawned"), SUSPEND);
        assert!(effects_of(&facts, "caller").is_empty());
    }

    #[test]
    fn e16_default_transmit_applies_without_a_per_callee_mask() {
        let mut graph = CallGraph::default();
        calls(&mut graph, "caller", &["callee"]);
        calls(&mut graph, "callee", &[]);
        let facts = permissive::<&'static str>()
            .seed(id("callee"), SUSPEND.union(ALLOC), None)
            .default_transmit(ALLOC)
            .solve(&graph);
        assert_eq!(effects_of(&facts, "caller"), ALLOC);
    }

    #[test]
    fn e17_witnesses_join_by_min() {
        let mut graph = CallGraph::default();
        calls(&mut graph, "caller", &["left", "right"]);
        calls(&mut graph, "left", &[]);
        calls(&mut graph, "right", &[]);
        let facts = permissive()
            .seed(id("left"), PANIC, Some("zulu"))
            .seed(id("right"), PANIC, Some("alpha"))
            .solve(&graph);
        assert_eq!(
            facts.get(&id("caller")).unwrap().witness(PANIC),
            Some(&"alpha")
        );
    }

    #[test]
    fn e18_each_effect_bit_carries_its_own_witness() {
        let facts = permissive()
            .seed(id("body"), PANIC, Some("divide"))
            .seed(id("body"), BLOCK, Some("recv"))
            .solve(&CallGraph::default());
        let summary = facts.get(&id("body")).unwrap();
        assert_eq!(summary.witness(PANIC), Some(&"divide"));
        assert_eq!(summary.witness(BLOCK), Some(&"recv"));
    }

    #[test]
    fn e19_a_witness_survives_transitive_propagation() {
        let mut graph = CallGraph::default();
        calls(&mut graph, "a", &["b"]);
        calls(&mut graph, "b", &["c"]);
        calls(&mut graph, "c", &[]);
        let facts = permissive()
            .seed(id("c"), BLOCK, Some("lock_acquire"))
            .solve(&graph);
        assert_eq!(
            facts.get(&id("a")).unwrap().witness(BLOCK),
            Some(&"lock_acquire")
        );
    }

    #[test]
    fn e20_an_effect_with_no_witness_is_still_reported() {
        let mut graph = CallGraph::default();
        unresolved(&mut graph, "indirect");
        let facts = permissive::<&'static str>()
            .unknown_callee(PANIC)
            .solve(&graph);
        let summary = facts.get(&id("indirect")).unwrap();
        assert!(summary.contains(PANIC));
        assert_eq!(summary.witness(PANIC), None);
    }

    #[test]
    fn e21_insertion_order_does_not_change_the_result() {
        let mut graph = CallGraph::default();
        calls(&mut graph, "root", &["left", "right"]);
        calls(&mut graph, "left", &["shared"]);
        calls(&mut graph, "right", &["shared"]);
        calls(&mut graph, "shared", &[]);
        let forward = permissive()
            .seed(id("left"), PANIC, Some("m"))
            .seed(id("right"), PANIC, Some("b"))
            .seed(id("shared"), PANIC, Some("y"))
            .solve(&graph);
        let reverse = permissive()
            .seed(id("shared"), PANIC, Some("y"))
            .seed(id("right"), PANIC, Some("b"))
            .seed(id("left"), PANIC, Some("m"))
            .solve(&graph);
        let collect = |facts: &EffectFacts<&'static str>| {
            facts
                .iter()
                .map(|(id, summary)| (id.clone(), summary.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(collect(&forward), collect(&reverse));
        assert_eq!(forward.get(&id("root")).unwrap().witness(PANIC), Some(&"b"));
    }

    #[test]
    fn e22_lookup_of_an_unknown_id_is_fail_closed() {
        let facts = permissive::<&'static str>().solve(&CallGraph::default());
        assert!(facts.intersects(&id("never_seen"), PANIC));
        assert!(facts.intersects(&FunctionId::method(TypeId::local("C"), "m"), BLOCK));
    }

    #[test]
    fn e23_witness_answers_for_the_lowest_queried_bit_present() {
        // MAY_ALLOCATE is bit 0 and MAY_PANIC is bit 5, so a query covering
        // both resolves to the allocation witness.
        let facts = permissive()
            .seed(id("body"), ALLOC, Some("alloc_site"))
            .seed(id("body"), PANIC, Some("panic_site"))
            .solve(&CallGraph::default());
        let summary = facts.get(&id("body")).unwrap();
        assert_eq!(summary.witness(ALLOC.union(PANIC)), Some(&"alloc_site"));
        assert_eq!(summary.witness(PANIC), Some(&"panic_site"));
        assert_eq!(summary.witness(BLOCK), None);
    }

    #[test]
    fn e24_a_diamond_yields_one_summary_with_the_minimum_witness() {
        let mut graph = CallGraph::default();
        calls(&mut graph, "root", &["left", "right"]);
        calls(&mut graph, "left", &["sink"]);
        calls(&mut graph, "right", &["sink"]);
        calls(&mut graph, "sink", &[]);
        let facts = permissive()
            .seed(id("sink"), PANIC, Some("sink_site"))
            .seed(id("right"), PANIC, Some("early_site"))
            .solve(&graph);
        let summary = facts.get(&id("root")).unwrap();
        assert_eq!(summary.effects(), PANIC);
        assert_eq!(summary.witness(PANIC), Some(&"early_site"));
    }

    #[test]
    fn e25_repeated_seeds_for_one_id_union() {
        let facts = permissive::<&'static str>()
            .seed(id("body"), PANIC, None)
            .seed(id("body"), BLOCK, None)
            .solve(&CallGraph::default());
        assert_eq!(effects_of(&facts, "body"), PANIC.union(BLOCK));
    }

    #[test]
    fn e26_cycle_members_finds_self_mutual_and_longer_cycles() {
        let mut graph = CallGraph::default();
        calls(&mut graph, "self_call", &["self_call"]);
        calls(&mut graph, "ping", &["pong"]);
        calls(&mut graph, "pong", &["ping"]);
        calls(&mut graph, "one", &["two"]);
        calls(&mut graph, "two", &["three"]);
        calls(&mut graph, "three", &["one"]);
        calls(&mut graph, "straight", &["leaf"]);
        calls(&mut graph, "leaf", &[]);
        let members = cycle_members(&graph, std::iter::empty());
        for name in ["self_call", "ping", "pong", "one", "two", "three"] {
            assert!(members.contains(&id(name)), "{name}");
        }
        assert!(!members.contains(&id("straight")));
        assert!(!members.contains(&id("leaf")));
    }

    #[test]
    fn e27_cycle_members_does_not_mistake_a_diamond_for_a_cycle() {
        let mut graph = CallGraph::default();
        calls(&mut graph, "root", &["left", "right"]);
        calls(&mut graph, "left", &["sink"]);
        calls(&mut graph, "right", &["sink"]);
        calls(&mut graph, "sink", &[]);
        assert!(cycle_members(&graph, std::iter::empty()).is_empty());
    }

    #[test]
    fn e28_a_deep_chain_does_not_overflow_the_compiler_stack() {
        const DEPTH: usize = 20_000;
        let mut graph = CallGraph::default();
        let names: Vec<String> = (0..DEPTH).map(|step| format!("f{step:06}")).collect();
        for step in 0..DEPTH {
            let targets: Vec<&str> = names
                .get(step + 1)
                .map(|next| vec![next.as_str()])
                .unwrap_or_default();
            calls(&mut graph, &names[step], &targets);
        }
        assert!(cycle_members(&graph, std::iter::empty()).is_empty());
        let facts = permissive::<&'static str>()
            .seed(id(&names[DEPTH - 1]), PANIC, None)
            .solve(&graph);
        assert_eq!(effects_of(&facts, &names[0]), PANIC);
    }
}
