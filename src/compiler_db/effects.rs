//! One session-local helper/lock/panic fixpoint over typed callable identities.
//! Imported facts use semantic names; linker spelling belongs to the backend.
use super::query::QueryTable;
use crate::{
    module::UnitId,
    parser::{
        ast::*,
        iter::{AstEvent, AstWalk},
    },
    semantic::{
        call_graph::{CallGraph, ClassHierarchy},
        effects::{EffectFacts, EffectProblem, EffectSummary, RuntimeEffects},
        ids::{FunctionId, TypeId},
    },
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

const PANIC: RuntimeEffects = RuntimeEffects::MAY_PANIC;
/// Checked arithmetic that this build mode wraps. Every mask that transmits
/// [`PANIC`] also transmits this bit, so `PANIC | CHECKED_PANIC` of a release
/// solution equals the overflow-checked solution's `PANIC` exactly.
const CHECKED_PANIC: RuntimeEffects = RuntimeEffects::MAY_CHECKED_PANIC;
/// Profile-independent panic summary read by checker verdicts.
pub(crate) const ANY_PANIC: RuntimeEffects = PANIC.union(CHECKED_PANIC);

use crate::semantic::concurrency::{NonpreemptibleHelper, NonpreemptibleReason};
const NO_PREEMPT: RuntimeEffects = RuntimeEffects::NO_PREEMPT_REGION;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum EffectWitness {
    Lock(LockEffectWitness),
    Helper {
        reason: NonpreemptibleReason,
        owner: FunctionId,
        source: (u32, usize, usize),
    },
    Panic {
        owner: FunctionId,
        source: (u32, usize, usize),
    },
    External(FunctionId),
}
impl EffectWitness {
    pub(crate) fn lock(&self) -> Option<&LockEffectCause> {
        match self {
            Self::Lock(w) => Some(&w.cause),
            _ => None,
        }
    }
}

/// Semantic capabilities extend the runtime ABI without changing ABI bit values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EffectCapabilities {
    pub(crate) runtime: RuntimeEffects,
    pub(crate) may_io: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectEvidence {
    pub(crate) summary: EffectSummary<EffectWitness>,
    // LockEffectWitness's in-solve ordering deliberately compares owners only.
    // Across revisions, evidence equality must also compare locations/causes.
    pub(crate) canonical: serde_json::Value,
}
impl EffectEvidence {
    fn new(summary: EffectSummary<EffectWitness>) -> Self {
        let witnesses: Vec<_> = (0..RuntimeEffects::BIT_COUNT)
            .map(|bit| match summary.witness(RuntimeEffects::from_bit(bit)) {
                Some(EffectWitness::Lock(witness)) => {
                    serde_json::json!(["lock", witness.owner, witness.cause])
                }
                Some(EffectWitness::Helper {
                    reason,
                    owner,
                    source,
                }) => serde_json::json!([
                    "helper",
                    owner,
                    source,
                    match reason {
                        NonpreemptibleReason::Loop => "loop",
                        NonpreemptibleReason::Recursion => "recursion",
                    }
                ]),
                Some(EffectWitness::Panic { owner, source }) => {
                    serde_json::json!(["panic", owner, source])
                }
                Some(EffectWitness::External(owner)) => serde_json::json!(["external", owner]),
                None => serde_json::Value::Null,
            })
            .collect();
        let canonical = serde_json::json!([summary.effects().bits(), witnesses]);
        Self { summary, canonical }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapturedEffect {
    pub(crate) capabilities: EffectCapabilities,
    pub(crate) evidence: EffectEvidence,
}

#[derive(Clone)]
pub(crate) struct UnitEffects {
    pub(crate) facts: EffectFacts<EffectWitness>,
    pub(crate) helpers: Arc<super::HelperSummary>,
    loop_bodies: HashSet<BodyId>,
    may_io: HashSet<FunctionId>,
}

pub(crate) struct EffectQueries {
    pub(crate) static_initializers: super::static_init::StaticInitQueries,
    solutions: std::cell::RefCell<
        HashMap<UnitId, Arc<crate::semantic::effects::EffectSolution<EffectWitness>>>,
    >,
    io_solutions:
        std::cell::RefCell<HashMap<UnitId, Arc<crate::semantic::effects::EffectSolution<u8>>>>,
    scanned: std::cell::Cell<usize>,
    recomputed: std::cell::Cell<usize>,
    body_queries: std::cell::RefCell<std::rc::Weak<super::body::BodyQueries>>,
    pub(crate) analysis: std::cell::RefCell<Option<HashMap<UnitId, crate::ai::CapturedUnit>>>,
    // Frozen unit assembly for backend reads. Direct-body queries and callable
    // equations own recomputation; this table does not invalidate their caches.
    units: QueryTable<UnitId, UnitEffects>,
    completed: std::cell::RefCell<HashMap<UnitId, Arc<UnitEffects>>>,
    previous: std::cell::RefCell<HashMap<UnitId, Arc<UnitEffects>>>,
    capabilities: QueryTable<(UnitId, FunctionId), EffectCapabilities>,
    evidence: QueryTable<(UnitId, FunctionId), EffectSummary<EffectWitness>>,
    tracking:
        std::cell::RefCell<std::rc::Weak<std::cell::RefCell<super::incremental::SyntaxQueries>>>,
    imported_reads: std::cell::RefCell<HashMap<UnitId, HashSet<(UnitId, FunctionId)>>>,
    source_reads: std::cell::RefCell<HashMap<UnitId, HashSet<UnitId>>>,
    roots: std::cell::RefCell<HashMap<UnitId, Vec<BodyId>>>,
    names: HashMap<String, UnitId>,
    dependencies: Option<std::rc::Rc<super::dependencies::ModuleDependencies>>,
    /// Debug builds check `+ - * **` and unary `-` on `i64` for overflow
    /// (willow-jz15.14), which makes those operators recoverable faults.
    /// Release builds wrap. Sessions without a mode stay conservative.
    overflow_checks: bool,
}
impl Default for EffectQueries {
    fn default() -> Self {
        Self {
            static_initializers: Default::default(),
            solutions: Default::default(),
            io_solutions: Default::default(),
            scanned: Default::default(),
            recomputed: Default::default(),
            body_queries: Default::default(),
            analysis: Default::default(),
            units: QueryTable::named("unit_effects"),
            completed: Default::default(),
            previous: Default::default(),
            capabilities: QueryTable::named("effect_capabilities"),
            evidence: QueryTable::named("effect_evidence"),
            tracking: Default::default(),
            imported_reads: Default::default(),
            source_reads: Default::default(),
            roots: Default::default(),
            names: HashMap::new(),
            dependencies: None,
            overflow_checks: true,
        }
    }
}
impl EffectQueries {
    #[cfg(test)]
    pub(crate) fn work(&self) -> (usize, usize) {
        (self.scanned.get(), self.recomputed.get())
    }

    pub(crate) fn new(
        modules: &[crate::module::ResolvedModule],
        dependencies: std::rc::Rc<super::dependencies::ModuleDependencies>,
        overflow_checks: bool,
    ) -> Self {
        let mut result = Self {
            dependencies: Some(dependencies),
            overflow_checks,
            ..Self::default()
        };
        for module in modules {
            result
                .names
                .insert(module.identity_path().to_string(), module.id);
            result
                .names
                .insert(module.registration_name().to_string(), module.id);
        }
        result
    }
    pub(crate) fn reuse_from(&self, previous: &Self) {
        let completed = previous.completed.borrow();
        *self.previous.borrow_mut() = completed.clone();
        *self.solutions.borrow_mut() = previous
            .solutions
            .borrow()
            .iter()
            .filter(|(unit, _)| completed.contains_key(unit))
            .map(|(&unit, solution)| (unit, solution.clone()))
            .collect();
        *self.io_solutions.borrow_mut() = previous
            .io_solutions
            .borrow()
            .iter()
            .filter(|(unit, _)| completed.contains_key(unit))
            .map(|(&unit, solution)| (unit, solution.clone()))
            .collect();
    }

    pub(crate) fn set_tracking(
        &self,
        syntax: std::rc::Rc<std::cell::RefCell<super::incremental::SyntaxQueries>>,
        bodies: std::rc::Rc<super::ids::BodyIndex>,
        body_queries: std::rc::Rc<super::body::BodyQueries>,
    ) {
        *self.body_queries.borrow_mut() = std::rc::Rc::downgrade(&body_queries);
        let mut roots = self.roots.borrow_mut();
        for (body, unit) in bodies.entries() {
            if !matches!(
                bodies.owner(body),
                Some((_, super::ids::BodyOwner::Lambda { .. }))
            ) {
                roots.entry(unit).or_default().push(body);
            }
        }
        for roots in roots.values_mut() {
            roots.sort();
        }
        *self.tracking.borrow_mut() = std::rc::Rc::downgrade(&syntax);
    }

    pub(crate) fn complete(
        &self,
        unit: UnitId,
        compute: impl FnOnce() -> UnitEffects,
    ) -> anyhow::Result<Arc<UnitEffects>> {
        let completed = self.units.query(unit, || {
            let tracking = self.tracking.borrow().upgrade();
            let roots = self.roots.borrow().get(&unit).cloned().unwrap_or_default();
            let green = match &tracking {
                Some(tracking) => tracking
                    .borrow_mut()
                    .validate_effects(unit, roots.clone())?,
                None => false,
            };
            let previous = self.previous.borrow_mut().remove(&unit).filter(|_| green);
            let reused = previous.is_some();
            let result = match previous {
                Some(previous) => (*previous).clone(),
                None => compute(),
            };
            if let Some(tracking) = tracking.filter(|_| !reused) {
                let inventory = result
                    .facts
                    .iter()
                    .map(|(&id, summary)| {
                        (
                            id,
                            CapturedEffect {
                                capabilities: EffectCapabilities {
                                    runtime: summary.effects(),
                                    may_io: result.may_io.contains(&id),
                                },
                                evidence: EffectEvidence::new(summary.clone()),
                            },
                        )
                    })
                    .collect();
                let imported = self
                    .imported_reads
                    .borrow_mut()
                    .remove(&unit)
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                let source_units = self
                    .source_reads
                    .borrow_mut()
                    .remove(&unit)
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
                let mut loop_bodies: Vec<_> = result.loop_bodies.iter().copied().collect();
                loop_bodies.sort();
                let mut helpers: Vec<_> = result
                    .helpers
                    .iter()
                    .map(|(id, helper)| {
                        (
                            *id,
                            serde_json::json!([
                                id,
                                helper.span,
                                match helper.reason {
                                    NonpreemptibleReason::Loop => "loop",
                                    NonpreemptibleReason::Recursion => "recursion",
                                }
                            ]),
                        )
                    })
                    .collect();
                helpers.sort_by_key(|(id, _)| *id);
                let metadata = serde_json::json!([
                    loop_bodies,
                    helpers
                        .into_iter()
                        .map(|(_, value)| value)
                        .collect::<Vec<_>>()
                ]);
                tracking.borrow_mut().publish_effects(
                    unit,
                    roots,
                    imported,
                    inventory,
                    source_units,
                    metadata,
                )?;
            }
            for (&id, summary) in result.facts.iter() {
                self.capabilities.seed(
                    (unit, id),
                    EffectCapabilities {
                        runtime: summary.effects(),
                        may_io: result.may_io.contains(&id),
                    },
                );
                self.evidence.seed((unit, id), summary.clone());
            }
            Ok(result)
        })?;
        self.completed
            .borrow_mut()
            .insert(unit, Arc::clone(&completed));
        Ok(completed)
    }
    pub(crate) fn completed_helpers(&self, unit: UnitId) -> Option<Arc<super::HelperSummary>> {
        Some(Arc::clone(&self.units.ready(&unit)?.helpers))
    }
    pub(crate) fn nonpreemptible_helpers(
        &self,
        unit: UnitId,
        _program: &Program,
    ) -> anyhow::Result<Arc<super::HelperSummary>> {
        self.completed_helpers(unit)
            .ok_or_else(|| anyhow::anyhow!("unit effects not completed"))
    }
    pub(crate) fn panic(&self, unit: UnitId, id: FunctionId) -> bool {
        self.effect_capabilities(unit, id)
            .is_none_or(|effects| effects.runtime.intersects(PANIC))
    }
    pub(crate) fn has_fact(&self, unit: UnitId, id: FunctionId) -> bool {
        self.effect_capabilities(unit, id).is_some()
    }
    /// Frozen callable projection used by backend and cross-unit analysis.
    pub(crate) fn effect_capabilities(
        &self,
        unit: UnitId,
        id: FunctionId,
    ) -> Option<Arc<EffectCapabilities>> {
        self.capabilities.ready(&(unit, id))
    }

    /// Evidence includes locations; capability consumers never read this table.
    pub(crate) fn effect_evidence(
        &self,
        unit: UnitId,
        id: FunctionId,
    ) -> Option<Arc<EffectSummary<EffectWitness>>> {
        let frozen = self.evidence.ready(&(unit, id))?;
        if let Some(tracking) = self.tracking.borrow().upgrade() {
            let evidence = tracking
                .borrow()
                .effect_evidence(unit, id)
                .expect("completed effect evidence query");
            return Some(Arc::new(evidence.summary));
        }
        Some(frozen)
    }

    fn source_effects(
        &self,
        consumer: UnitId,
        unit: UnitId,
        body: BodyId,
        id: FunctionId,
    ) -> Option<(RuntimeEffects, bool, bool)> {
        let effects = self.units.ready(&unit)?;
        if consumer != unit && self.tracking.borrow().upgrade().is_some() {
            self.source_reads
                .borrow_mut()
                .entry(consumer)
                .or_default()
                .insert(unit);
        }
        Some((
            self.effect_capabilities(unit, id)?
                .runtime
                .intersection(ANY_PANIC),
            effects.loop_bodies.contains(&body),
            self.effect_capabilities(unit, id)?.may_io,
        ))
    }
    fn resolve_module(&self, consumer: UnitId, path: &str) -> Option<UnitId> {
        // Package modules register only their unspellable canonical namespace.
        // Legacy sessions retain their historical canonical/access-name adapter.
        self.names
            .get(path)
            .copied()
            .or_else(|| self.dependencies.as_ref()?.unit_for_path(consumer, path))
    }

    pub(crate) fn external(
        &self,
        consumer: UnitId,
        target: &FunctionId,
        imports: &HashMap<String, String>,
    ) -> RuntimeEffects {
        self.external_capabilities(consumer, target, imports)
            .runtime
            .intersection(ANY_PANIC)
    }

    /// Imported dispatch uses completed capability queries; unavailable units
    /// still fail closed. These reads participate in revision invalidation.
    pub(crate) fn external_waiting(&self, consumer: UnitId, target: &FunctionId) -> RuntimeEffects {
        self.lookup_external_capabilities(consumer, target, &HashMap::new())
            .map_or(LOCK_EFFECT_WAIT, |fact| {
                fact.runtime.intersection(LOCK_EFFECT_WAIT)
            })
    }

    fn external_capabilities(
        &self,
        consumer: UnitId,
        target: &FunctionId,
        imports: &HashMap<String, String>,
    ) -> EffectCapabilities {
        self.lookup_external_capabilities(consumer, target, imports)
            .unwrap_or_else(|| unknown_external_capabilities(target))
    }

    pub(crate) fn external_target(
        &self,
        consumer: UnitId,
        target: &FunctionId,
        imports: &HashMap<String, String>,
    ) -> Option<(UnitId, FunctionId)> {
        let namespace = target.namespace();
        let owner = target.owner();
        let (path, id) = if let Some(namespace) = namespace.as_deref() {
            (
                imports
                    .get(namespace)
                    .map(String::as_str)
                    .unwrap_or(namespace),
                match owner {
                    Some(owner) => FunctionId::method(TypeId::local(owner), target.name()),
                    None => FunctionId::free(target.name()),
                },
            )
        } else if let Some(owner) = owner.as_deref() {
            let path = imports.get(owner).map(String::as_str).unwrap_or(owner);
            if self.resolve_module(consumer, path).is_some() {
                (path, FunctionId::free(target.name()))
            } else if let Some((module, item)) = path.rsplit_once("::") {
                (
                    module,
                    FunctionId::method(TypeId::local(item), target.name()),
                )
            } else {
                return None;
            }
        } else if let Some(path) = imports.get(target.name().as_ref()) {
            let (module, item) = path.rsplit_once("::")?;
            (module, FunctionId::free(item))
        } else {
            return None;
        };
        let unit = self.resolve_module(consumer, path)?;
        Some((unit, id))
    }

    fn lookup_external_capabilities(
        &self,
        consumer: UnitId,
        target: &FunctionId,
        imports: &HashMap<String, String>,
    ) -> Option<EffectCapabilities> {
        if let Some(effect) = intrinsic_effects(target) {
            return Some(EffectCapabilities {
                runtime: effect,
                may_io: false,
            });
        }
        let (unit, id) = self.external_target(consumer, target, imports)?;
        if !self.units.is_ready(&unit) {
            return Some(EffectCapabilities {
                runtime: PANIC.union(LOCK_EFFECT_WAIT),
                may_io: true,
            });
        }
        let fact = self.effect_capabilities(unit, id)?;
        Some(if let Some(tracking) = self.tracking.borrow().upgrade() {
            let capabilities = tracking
                .borrow()
                .effect_capabilities(unit, id)
                .expect("completed effect capability query");
            self.imported_reads
                .borrow_mut()
                .entry(consumer)
                .or_default()
                .insert((unit, id));
            capabilities
        } else {
            *fact
        })
    }
}

pub(crate) fn intrinsic_effects(target: &FunctionId) -> Option<RuntimeEffects> {
    crate::semantic::intrinsics::builtin_target_effects(target)
        .map(|effects| effects.intersection(PANIC))
}
fn unknown_external_capabilities(target: &FunctionId) -> EffectCapabilities {
    EffectCapabilities {
        runtime: default_external(target),
        may_io: true,
    }
}

fn default_external(target: &FunctionId) -> RuntimeEffects {
    if target.owner().is_some() && target.name().as_ref() == "init" {
        RuntimeEffects::NONE
    } else {
        PANIC
    }
}

/// One body inventory and one fixpoint. Helper membership is intentionally
/// narrower than lock membership; masks prevent a constructor, lambda, dispatch
/// union, or eager async call from broadening the historical helper contract.
pub(crate) fn solve_unit<N>(
    program: &Program,
    graph: &CallGraph,
    types: &HashMap<ExprId, Type<N>>,
    origins: Option<(&super::ids::BodyIndex, &EffectQueries)>,
    callables: &HashMap<FunctionId, bool>,
    direct: &HashMap<FunctionId, LockEffectCause>,
    external: impl Fn(&FunctionId) -> RuntimeEffects,
) -> UnitEffects {
    crate::query_stats::add(crate::query_stats::Counter::NonpreemptibleHelpers, 1);
    let index = origins.map(|(index, _)| index);
    let overflow_checks = origins.is_none_or(|(_, queries)| queries.overflow_checks);
    let mut pending = Vec::new();
    let mut helpers = HashMap::new();
    for item in &program.items {
        match item {
            Item::Function(f) => {
                let id = FunctionId::free(&f.name);
                pending.push((id, f.body.id, &f.body));
                if !f.is_async {
                    helpers.insert(id, f.span);
                }
            }
            Item::Class(c) => {
                let owner = TypeId::local(&c.name);
                for m in &c.methods {
                    let id = FunctionId::method(owner, &m.name);
                    pending.push((id, m.body.id, &m.body));
                    if !m.is_async {
                        helpers.insert(id, m.span);
                    }
                }
                for init in &c.constructors {
                    pending.push((FunctionId::method(owner, "init"), init.body.id, &init.body));
                }
            }
            Item::Interface(i) if i.type_params.is_empty() => {
                for m in &i.methods {
                    if let Some(body) = &m.default_body {
                        pending.push((
                            FunctionId::method(
                                TypeId::local(&i.name),
                                format!("$default${}", m.name),
                            ),
                            body.id,
                            body,
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    let canonical_bodies: HashSet<_> = pending.iter().map(|(_, body, _)| *body).collect();
    let consumer = index.and_then(|index| {
        pending
            .iter()
            .find_map(|(_, body, _)| index.owner(*body).map(|(unit, _)| unit))
    });
    let imports: HashMap<_, _> = program
        .imports
        .iter()
        .map(|import| {
            (
                import.alias.clone().unwrap_or_else(|| {
                    import
                        .path
                        .rsplit("::")
                        .next()
                        .unwrap_or(&import.path)
                        .to_owned()
                }),
                import.path.clone(),
            )
        })
        .collect();
    let mut problem = EffectProblem::new()
        .external_callee(PANIC)
        .unknown_callee(PANIC)
        .missing_body(PANIC)
        .default_transmit(ANY_PANIC.union(LOCK_EFFECT_WAIT));
    let mut own = HashSet::new();
    let mut loops = HashSet::new();
    let mut loop_spans = HashMap::new();
    let mut loop_body_spans = HashMap::new();
    let mut loop_bodies = HashSet::new();
    let mut copies = Vec::new();
    let mut may_io = HashSet::new();
    let mut visits = 0;
    while let Some((id, body_id, body)) = pending.pop() {
        visits += 1;
        own.insert(id);
        problem = problem.body(id);
        // A copied body inherits canonical facts. Independently checked
        // cross-unit copies must also retain destination-scope hazards.
        if let Some(index) = index {
            let source = index.source_body(body_id);
            if source != body_id {
                let known = index.owner(source).and_then(|(unit, _)| {
                    consumer.and_then(|consumer| {
                        origins.expect("source index").1.source_effects(
                            consumer,
                            unit,
                            source,
                            source_callable(index, source),
                        )
                    })
                });
                if let Some((panic, has_loop, source_io)) = known {
                    if source_io {
                        may_io.insert(id);
                    }
                    problem = problem.seed(id, panic, None);
                    if has_loop {
                        loops.insert(id);
                        loop_bodies.insert(body_id);
                    }
                }
                if canonical_bodies.contains(&source) {
                    // Same-unit defaults are checked only under the canonical
                    // interface identity. Do not repeat their source walk.
                    copies.push((id, source));
                    continue;
                }
                if known.is_none() {
                    // Generic instantiations have no canonical checked proof.
                    problem = problem.seed(id, PANIC, None);
                }
                // Cross-unit copies are checked in their destination scope.
                // Union the canonical fact with this copy's direct guards and
                // resolved edges: rebinding a free name can change both a call
                // effect and the inferred type of a subsequent addition.
            }
        }

        let scan = || {
            if let Some((_, queries)) = origins {
                queries.scanned.set(queries.scanned.get() + 1);
            }
            Ok(scan_direct_effects(
                id,
                body_id,
                body,
                types,
                index,
                overflow_checks,
            ))
        };
        let queries = origins.and_then(|(_, effects)| effects.body_queries.borrow().upgrade());
        let direct_bodies = match queries {
            Some(queries) => queries
                .derived(
                    body_id,
                    super::tracked::QueryNode::DirectEffects(body_id),
                    None,
                    scan,
                )
                .expect("direct effect dependencies have completed"),
            None => scan().expect("direct effect scan is infallible"),
        };
        for direct in direct_bodies {
            own.insert(direct.id);
            problem = problem.body(direct.id);
            if let Some(span) = direct.loop_span {
                loop_spans.insert(direct.id, span);
                loop_body_spans.insert(direct.body, span);
                loops.insert(direct.id);
                loop_bodies.insert(direct.body);
            }
            if direct.may_io {
                may_io.insert(direct.id);
            }
            if let Some(span) = direct.lock_span {
                // Acquisition parks a contended task before entering the
                // critical section. Seed it before the checker's other waits
                // so this precise acquisition witness survives same-owner ties.
                problem = problem.seed(
                    direct.id,
                    RuntimeEffects::MAY_SUSPEND,
                    Some(EffectWitness::Lock(LockEffectWitness {
                        owner: direct.id,
                        cause: LockEffectCause {
                            span,
                            operation: "lock".into(),
                            kind: LockEffectKind::Suspend,
                        },
                    })),
                );
            }
            let seed = if direct.panics {
                Some((PANIC, direct.panic_span))
            } else if direct.checked_panics {
                Some((CHECKED_PANIC, direct.checked_span))
            } else {
                None
            };
            if let Some((panic, span)) = seed {
                let witness = span.map(|span| EffectWitness::Panic {
                    owner: direct.id,
                    source: (span.file_id.0, span.start, span.end),
                });
                problem = problem.seed(direct.id, panic, witness);
            }
        }
    }
    for &(id, source) in &copies {
        if loop_bodies.contains(&source) {
            loops.insert(id);
            if let Some(&span) = loop_body_spans.get(&source) {
                loop_spans.insert(id, span);
            }
        }
    }
    crate::query_stats::add(crate::query_stats::Counter::EffectInventory, visits);
    let mut edge_visits = 0;
    let mut helper_graph = CallGraph::default();
    for &id in helpers.keys() {
        let targets = graph
            .get(&id)
            .map(|sites| {
                sites
                    .targets
                    .iter()
                    .filter(|id| {
                        edge_visits += 1;
                        helpers.contains_key(id)
                    })
                    .copied()
                    .collect()
            })
            .unwrap_or_default();
        helper_graph.merge(
            id,
            crate::semantic::call_graph::CallSites {
                unsupported_initialization: Default::default(),
                virtual_calls: Default::default(),
                targets,
                has_unknown: false,
            },
        );
    }
    let cycles = crate::semantic::effects::cycle_members(&helper_graph, std::iter::empty());
    for &id in helpers.keys() {
        let reason = if loops.contains(&id) {
            Some(NonpreemptibleReason::Loop)
        } else if cycles.contains(&id) {
            Some(NonpreemptibleReason::Recursion)
        } else {
            None
        };
        if let Some(reason) = reason {
            let span = loop_spans.get(&id).unwrap_or(&helpers[&id]);
            problem = problem.seed(
                id,
                NO_PREEMPT,
                Some(EffectWitness::Helper {
                    reason,
                    owner: id,
                    source: (span.file_id.0, span.start, span.end),
                }),
            );
        }
    }
    for (&id, cause) in direct {
        problem = problem.seed(
            id,
            cause.kind.effects(),
            Some(EffectWitness::Lock(LockEffectWitness {
                owner: id,
                cause: cause.clone(),
            })),
        );
    }
    for id in graph.ids().chain(callables.keys()).chain(helpers.keys()) {
        let mut mask = ANY_PANIC.union(LOCK_EFFECT_WAIT);
        if callables.get(id) == Some(&true) {
            mask = ANY_PANIC;
        } else if helpers.contains_key(id) {
            mask = mask.union(NO_PREEMPT);
        }
        problem = problem.transmit(*id, mask);
    }
    let mut classified = HashSet::new();
    let incremental = origins.is_some() && consumer.is_some();
    let mut io_graph = std::borrow::Cow::Borrowed(graph);
    let mut io_callers: HashMap<FunctionId, Vec<FunctionId>> = HashMap::new();
    if let Some(index) = index {
        for (id, source) in copies {
            let source = source_callable(index, source);
            if incremental {
                io_graph.to_mut().merge(
                    id,
                    crate::semantic::call_graph::CallSites {
                        unsupported_initialization: Default::default(),
                        virtual_calls: Default::default(),
                        targets: std::iter::once(source).collect(),
                        has_unknown: false,
                    },
                );
            } else {
                io_callers.entry(source).or_default().push(id);
            }
            edge_visits += 1;
        }
    }
    for (&caller, sites) in graph.iter() {
        if sites.has_unknown {
            may_io.insert(caller);
        }
        for target in &sites.targets {
            edge_visits += 1;
            if !incremental {
                io_callers.entry(*target).or_default().push(caller);
            }
            if !own.contains(target) && classified.insert(*target) {
                // Every unproved external call is conservatively IO-capable.
                // Known runtime/stat/math intrinsics are the only pure leaves;
                // filesystem, networking, printing and foreign calls fail closed.
                let external_io = match (origins, consumer) {
                    (Some((_, queries)), Some(unit)) => {
                        queries.external_capabilities(unit, target, &imports).may_io
                    }
                    _ => intrinsic_effects(target).is_none(),
                };
                if external_io {
                    may_io.insert(*target);
                }
                problem = problem.seed(
                    *target,
                    external(target).intersection(ANY_PANIC),
                    Some(EffectWitness::External(*target)),
                );
            }
        }
    }
    // The IO lattice has one bit: each vertex enters the queue once and each
    // existing call edge is visited once, including mutually recursive bodies.
    if let (Some((_, queries)), Some(unit)) = (origins, consumer) {
        // The IO lattice uses a private carrier bit; it is never exposed as an
        // ABI capability. Reuse the same masked fixed-point implementation.
        let io_bit = RuntimeEffects::MAY_ALLOCATE;
        let mut io_problem = EffectProblem::<u8>::new()
            .external_callee(RuntimeEffects::NONE)
            .unknown_callee(RuntimeEffects::NONE)
            .missing_body(RuntimeEffects::NONE)
            .default_transmit(io_bit);
        for &id in &own {
            io_problem = io_problem.body(id);
        }
        for &id in &may_io {
            io_problem = io_problem.seed(id, io_bit, None);
        }
        let mut previous = queries.io_solutions.borrow_mut();
        crate::query_stats::add(crate::query_stats::Counter::EffectIoSolve, 1);
        let solution =
            io_problem.solve_incremental(&io_graph, previous.get(&unit).map(Arc::as_ref));
        queries
            .recomputed
            .set(queries.recomputed.get() + solution.recomputed);
        may_io = solution
            .facts
            .iter()
            .filter(|(_, fact)| fact.intersects(io_bit))
            .map(|(&id, _)| id)
            .collect();
        previous.insert(unit, Arc::new(solution));
    } else {
        let mut pending_io: VecDeque<_> = may_io.iter().copied().collect();
        while let Some(callee) = pending_io.pop_front() {
            for &caller in io_callers.get(&callee).into_iter().flatten() {
                edge_visits += 1;
                if may_io.insert(caller) {
                    pending_io.push_back(caller);
                }
            }
        }
    }
    crate::query_stats::add(crate::query_stats::Counter::EffectEdges, edge_visits);
    crate::query_stats::add(crate::query_stats::Counter::EffectSolve, 1);
    let facts = match (origins, consumer) {
        (Some((_, queries)), Some(unit)) => {
            let mut solutions = queries.solutions.borrow_mut();
            let solution = problem.solve_incremental_by(
                graph,
                solutions.get(&unit).map(Arc::as_ref),
                same_seed,
            );
            queries
                .recomputed
                .set(queries.recomputed.get() + solution.recomputed);
            let facts = solution.facts.clone();
            solutions.insert(unit, Arc::new(solution));
            facts
        }
        _ => problem.solve(graph),
    };
    let helpers = helpers
        .into_iter()
        .filter_map(|(id, span)| match facts.get(&id)?.witness(NO_PREEMPT)? {
            EffectWitness::Helper { reason, .. } => Some((
                id,
                NonpreemptibleHelper {
                    span,
                    reason: *reason,
                },
            )),
            _ => None,
        })
        .collect();
    UnitEffects {
        facts,
        helpers: Arc::new(helpers),
        loop_bodies,
        may_io,
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DirectBodyEffects {
    id: FunctionId,
    body: BodyId,
    panics: bool,
    /// Panics only through checked arithmetic this build mode wraps.
    #[serde(default)]
    checked_panics: bool,
    #[serde(default)]
    checked_span: Option<crate::diagnostics::Span>,
    panic_span: Option<crate::diagnostics::Span>,
    lock_span: Option<crate::diagnostics::Span>,
    loop_span: Option<crate::diagnostics::Span>,
    may_io: bool,
}

fn scan_direct_effects<N>(
    id: FunctionId,
    body_id: BodyId,
    body: &Block,
    types: &HashMap<ExprId, Type<N>>,
    index: Option<&super::ids::BodyIndex>,
    overflow_checks: bool,
) -> Vec<DirectBodyEffects> {
    let mut pending = vec![(id, body_id, body)];
    let mut result = Vec::new();
    let mut visits = 0;
    while let Some((id, body_id, body)) = pending.pop() {
        let mut hazards = HazardVisitor {
            panics: false,
            checked_panics: false,
            checked_span: None,
            panic_span: None,
            expr_types: types,
            overflow_checks,
        };
        let mut loop_span = None;
        let mut may_io = false;
        let mut lock_span = None;
        let mut defer_depth = 0;
        let mut walk = AstWalk::new(AstEvent::Block(body));
        while let Some(event) = walk.next() {
            visits += 1;
            match event {
                AstEvent::Lambda(lambda) => {
                    if let LambdaBody::Block(child) = &lambda.body {
                        let child_id = index
                            .and_then(|i| i.lambda_in(body_id, lambda.id))
                            .unwrap_or(child.id);
                        pending.push((FunctionId::lambda(child_id), child_id, child));
                    }
                    walk.skip_children();
                }
                AstEvent::Stmt(stmt) => {
                    hazards.visit_stmt(stmt);
                    match stmt {
                        Stmt::Lock(lock) if lock_span.is_none() => {
                            // Keep an AST-native span: coordinate-only edits
                            // remap cached evidence by exact syntax spans.
                            lock_span = Some(lock.span);
                        }
                        Stmt::Defer(_) => defer_depth += 1,
                        Stmt::While(_) | Stmt::For(_) if defer_depth == 0 => {
                            loop_span.get_or_insert(stmt.span());
                        }
                        _ => {}
                    }
                }
                AstEvent::ExitStmt(Stmt::Defer(_)) => defer_depth -= 1,
                AstEvent::Expr(expr) => {
                    may_io |= matches!(expr, Expr::Print(..));
                    hazards.visit_expr(expr);
                }
                _ => {}
            }
        }
        result.push(DirectBodyEffects {
            id,
            body: body_id,
            panics: hazards.panics,
            checked_panics: hazards.checked_panics,
            checked_span: hazards.checked_span,
            panic_span: hazards.panic_span,
            lock_span,
            loop_span,
            may_io,
        });
    }
    crate::query_stats::add(crate::query_stats::Counter::EffectInventory, visits);
    result
}

fn same_seed(a: &EffectSummary<EffectWitness>, b: &EffectSummary<EffectWitness>) -> bool {
    a.effects() == b.effects()
        && (0..RuntimeEffects::BIT_COUNT).all(|bit| {
            let bit = RuntimeEffects::from_bit(bit);
            match (a.witness(bit), b.witness(bit)) {
                (Some(EffectWitness::Lock(a)), Some(EffectWitness::Lock(b))) => {
                    a.owner == b.owner
                        && a.cause.span == b.cause.span
                        && a.cause.kind == b.cause.kind
                        && a.cause.operation == b.cause.operation
                }
                (a, b) => a == b,
            }
        })
}

fn source_callable(index: &super::ids::BodyIndex, body: BodyId) -> FunctionId {
    use super::ids::BodyOwner;
    match index.owner(body).map(|(_, owner)| owner) {
        Some(BodyOwner::Function(id)) => match id.owner() {
            Some(owner) => FunctionId::method(TypeId::local(owner), id.name()),
            None => FunctionId::free(id.name()),
        },
        Some(BodyOwner::InterfaceDefault(id)) => FunctionId::method(
            TypeId::local(id.owner().expect("interface owner")),
            format!("$default${}", id.name()),
        ),
        Some(BodyOwner::Constructor { owner, .. }) => {
            FunctionId::method(TypeId::local(owner.name()), "init")
        }
        Some(BodyOwner::Lambda { .. }) => FunctionId::lambda(body),
        _ => FunctionId::lambda(body),
    }
}

/// Standalone evaluator for callers without a compilation session. The caller
/// adapts any already-known linker facts; typed edge collection is shared.
pub(crate) fn analyze(
    program: &Program,
    lambdas: &[(FunctionId, &LambdaExpr)],
    expr_types: &HashMap<ExprId, Type<TypeId>>,
    external: impl Fn(&FunctionId) -> RuntimeEffects,
) -> HashMap<FunctionId, bool> {
    let graph = crate::semantic::TypeChecker::resolved_effect_graph(program);
    let result = solve_unit(
        program,
        &graph,
        expr_types,
        None,
        &HashMap::new(),
        &HashMap::new(),
        external,
    );
    let mut facts: HashMap<_, _> = result
        .facts
        .iter()
        .map(|(id, f)| (*id, f.intersects(PANIC)))
        .collect();
    for (id, lambda) in lambdas {
        if let LambdaBody::Block(body) = &lambda.body {
            facts.insert(
                *id,
                result.facts.intersects(&FunctionId::lambda(body.id), PANIC),
            );
        }
    }
    facts
}

/// Direct hazards a body performs itself, independent of what it calls.
///
/// Direct hazards accumulate independently of traversal order. The explicit
/// worklist keeps this read-only analysis independent of expression depth.
struct HazardVisitor<'a, N> {
    panics: bool,
    /// Checked arithmetic that `overflow_checks == false` wraps.
    checked_panics: bool,
    checked_span: Option<crate::diagnostics::Span>,
    panic_span: Option<crate::diagnostics::Span>,
    expr_types: &'a HashMap<ExprId, Type<N>>,
    overflow_checks: bool,
}

impl<N> HazardVisitor<'_, N> {
    fn mark_direct(&mut self, span: crate::diagnostics::Span) {
        self.panics = true;
        earliest(&mut self.panic_span, span);
    }
    /// Checked arithmetic panics only with overflow checks. Without them it
    /// still feeds the profile-independent [`CHECKED_PANIC`] summary.
    fn mark_checked(&mut self, span: crate::diagnostics::Span) {
        if self.overflow_checks {
            self.mark_direct(span);
        } else {
            self.checked_panics = true;
            earliest(&mut self.checked_span, span);
        }
    }
}

/// Keep the first hazard in source order, independent of traversal order.
fn earliest(slot: &mut Option<crate::diagnostics::Span>, span: crate::diagnostics::Span) {
    if slot.is_none_or(|old| {
        (span.file_id.0, span.start, span.end) < (old.file_id.0, old.start, old.end)
    }) {
        *slot = Some(span);
    }
}

impl<N> HazardVisitor<'_, N> {
    #[cfg(test)]
    fn visit_block(&mut self, block: &Block) {
        let mut walk = AstWalk::new(AstEvent::Block(block));
        while let Some(event) = walk.next() {
            match event {
                // A lambda is analyzed as its own callable.
                AstEvent::Lambda(_) => walk.skip_children(),
                AstEvent::Stmt(stmt) => self.visit_stmt(stmt),
                AstEvent::Expr(expr) => self.visit_expr(expr),
                _ => {}
            }
        }
    }

    fn visit_stmt(&mut self, statement: &Stmt) {
        match statement {
            // Bounds guard.
            Stmt::IndexAssign(_) => self.mark_direct(statement.span()),
            // Recursive acquisition and a lost ownership token are recoverable
            // language faults.
            Stmt::Lock(_) => self.mark_direct(statement.span()),
            // `super.init` is an unresolved edge in the shared graph, which
            // already makes the body conservative.
            Stmt::Let(_)
            | Stmt::SuperInit(_)
            | Stmt::Assign(_)
            | Stmt::FieldAssign(_)
            | Stmt::StaticFieldAssign(_)
            | Stmt::If(_)
            | Stmt::While(_)
            | Stmt::Break(_)
            | Stmt::Continue(_)
            | Stmt::Defer(_)
            | Stmt::For(_)
            | Stmt::Return(_)
            | Stmt::Expr(_) => {}
        }
    }

    fn visit_expr(&mut self, expression: &Expr) {
        match expression {
            Expr::Binary(expr) => {
                // Reuse checked expression types; unknown additions remain
                // conservative for direct backend users without artifacts.
                let ty = self.expr_types.get(&expr.id);
                let concat = expr.op == BinOp::Add && !matches!(ty, Some(Type::I64 | Type::F64));
                // Only integer division keeps a zero/overflow guard, and a
                // literal divisor other than 0 and -1 can trip neither.
                let guarded_division = matches!(expr.op, BinOp::Div | BinOp::Rem)
                    && !matches!(ty, Some(Type::F64))
                    && !matches!(expr.rhs, Expr::Integer(divisor, ..) if divisor != 0 && divisor != -1);
                // Debug builds check integer `+ - *` for overflow and a shift
                // amount outside `0..64` unless it is an in-range literal.
                let checked = matches!(expr.op, BinOp::Add | BinOp::Sub | BinOp::Mul)
                    && !matches!(ty, Some(Type::F64))
                    || matches!(expr.op, BinOp::Shl | BinOp::Shr)
                        && !matches!(expr.rhs, Expr::Integer(amount, ..) if (0..64).contains(&amount));
                if concat || guarded_division || expr.op == BinOp::Pow {
                    self.mark_direct(expression.span());
                } else if checked {
                    self.mark_checked(expression.span());
                }
            }
            // Debug builds check integer negation of `i64::MIN`; a negated
            // literal is at least `-i64::MAX` and cannot overflow.
            Expr::Unary(unary)
                if unary.op == UnaryOp::Neg
                    && !matches!(unary.expr, Expr::Integer(..))
                    && !matches!(self.expr_types.get(&unary.id), Some(Type::F64)) =>
            {
                self.mark_checked(expression.span());
            }
            // Strict await can turn cancellation into a language panic.
            // TaskResult awaits are intentionally not distinguished here:
            // retaining a check is conservative.
            Expr::Await(_) => self.mark_direct(expression.span()),
            Expr::Select(_) => self.mark_direct(expression.span()),
            // Object/interface display may invoke user `toString` code.
            Expr::Print(..) => self.mark_direct(expression.span()),
            // Bounds guard.
            Expr::Index(..) => self.mark_direct(expression.span()),
            // Every call form is an edge in the shared graph, classified by
            // `classify_edge` rather than here. A field access lowers to a
            // plain load of a checked layout offset with no language-level
            // guard (willow-8hq4.14); its receiver is visited on its own.
            Expr::FieldAccess(..)
            | Expr::Call(_)
            | Expr::MethodCall(_)
            | Expr::StaticCall(_)
            | Expr::New(_)
            | Expr::Integer(..)
            | Expr::Float(..)
            | Expr::Bool(..)
            | Expr::String(..)
            | Expr::Var(..)
            | Expr::StaticField(_)
            | Expr::Unary(_)
            | Expr::ObjectLiteral(_)
            | Expr::Ternary(_)
            | Expr::Range(_)
            | Expr::Lambda(_)
            | Expr::Match(_)
            | Expr::TryPropagate(..)
            | Expr::ArrayLiteral(..) => {}
        }
    }
}

use crate::diagnostics::Span;

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct TaskMethodCall {
    pub(crate) callee: FunctionId,
    pub(crate) span: Span,
    pub(crate) diagnostic_index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum LockEffectKind {
    Suspend,
    Block,
    /// A separately compiled/imported synchronous implementation whose body
    /// is not available to this checker. Treat it conservatively as capable of
    /// either kind of wait instead of silently weakening E2604.
    SuspendOrBlock,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct LockEffectCause {
    pub(crate) span: Span,
    pub(crate) operation: std::borrow::Cow<'static, str>,
    pub(crate) kind: LockEffectKind,
}

impl LockEffectKind {
    /// The lattice bits this wait contributes to the shared effect fixpoint
    /// (willow-uqzx.1.3).
    pub(crate) fn effects(self) -> RuntimeEffects {
        match self {
            Self::Suspend => RuntimeEffects::MAY_SUSPEND,
            Self::Block => RuntimeEffects::MAY_BLOCK,
            Self::SuspendOrBlock => RuntimeEffects::MAY_SUSPEND.union(RuntimeEffects::MAY_BLOCK),
        }
    }
}

/// Either kind of wait. E2604 fires on one or the other, never on both at once,
/// so the query is an intersection rather than a containment.
pub(crate) const LOCK_EFFECT_WAIT: RuntimeEffects =
    RuntimeEffects::MAY_SUSPEND.union(RuntimeEffects::MAY_BLOCK);

/// Which callable's direct wait explains a propagated lock effect.
///
/// This is the witness carried through [`crate::semantic::effects`]. Witnesses
/// there join by `min`, and ordering by owner alone reproduces the rule this
/// analysis has always used: report the lexicographically smallest reachable
/// owner, so diagnostics do not depend on hash iteration order.
///
/// `Eq` is deliberately owner-only too, keeping `Ord` consistent with it. That
/// preserves the first seeded cause per effect and owner. Acquisition seeds
/// precede other direct waits, making their precise lock location canonical.
#[derive(Debug, Clone)]
pub(crate) struct LockEffectWitness {
    pub(crate) owner: FunctionId,
    pub(crate) cause: LockEffectCause,
}

impl PartialEq for LockEffectWitness {
    fn eq(&self, other: &Self) -> bool {
        self.owner == other.owner
    }
}

impl Eq for LockEffectWitness {}

impl PartialOrd for LockEffectWitness {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LockEffectWitness {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.owner.cmp(&other.owner)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct LockEffectCallsite {
    pub(crate) callee: FunctionId,
    pub(crate) span: Span,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_read_only_scans_use_a_one_megabyte_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let tokens = crate::lexer::Lexer::new("async fn f() { await sleep(1); }")
                    .tokenize()
                    .unwrap();
                let (mut program, errors) = crate::parser::Parser::new(tokens).parse();
                assert!(errors.is_empty());
                let Item::Function(function) = &mut program.items[0] else {
                    unreachable!()
                };
                let Stmt::Expr(expr) = &mut function.body.stmts[0] else {
                    unreachable!()
                };
                let span = crate::diagnostics::Span::new(0, 0, 1, 1);
                let mut expr =
                    std::mem::replace(&mut expr.expr, crate::parser::ownership::placeholder_expr());
                for _ in 0..50_000 {
                    expr = Expr::TryPropagate(
                        Box::new(expr),
                        span,
                        crate::parser::ast::ExprId::fresh(),
                    );
                }
                function
                    .body
                    .stmts
                    .push(Stmt::Expr(crate::parser::ast::ExprStmt { expr, span }));
                let mut hazards = HazardVisitor {
                    panics: false,
                    checked_panics: false,
                    checked_span: None,
                    panic_span: None,
                    expr_types: &HashMap::<ExprId, Type<TypeId>>::new(),
                    overflow_checks: true,
                };
                hazards.visit_block(&function.body);
                assert!(hazards.panics);
                drop(program);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    fn program(source: &str) -> Program {
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        program
    }

    #[test]
    fn integer_arithmetic_may_panic_only_with_overflow_checks() {
        // Without expression types `+` stays a possible string concatenation,
        // which may panic regardless of the mode, so it is not listed here.
        for (body, checked_panics) in [
            ("return a - 1;", true),
            ("return a * 2;", true),
            ("return -a;", true),
            ("return -5;", false),
            ("if a < 1 { return 0; } return a;", false),
            // Shifts check a runtime amount; an in-range literal cannot fault.
            ("return 1 << a;", true),
            ("return a >> a;", true),
            ("return a << 64;", true),
            ("return a >> -1;", true),
            ("return a << 63;", false),
            ("return a >> 0;", false),
            ("return a & 3 | a ^ 5;", false),
            ("return !a;", false),
        ] {
            let program = program(&format!("fn f(a: i64) -> i64 {{ {body} }}"));
            let Item::Function(function) = &program.items[0] else {
                unreachable!()
            };
            for overflow_checks in [true, false] {
                let mut hazards = HazardVisitor {
                    panics: false,
                    checked_panics: false,
                    checked_span: None,
                    panic_span: None,
                    expr_types: &HashMap::<ExprId, Type<TypeId>>::new(),
                    overflow_checks,
                };
                hazards.visit_block(&function.body);
                assert_eq!(
                    hazards.panics,
                    checked_panics && overflow_checks,
                    "{body} overflow_checks={overflow_checks}"
                );
                // Release keeps the same answer in the profile-independent bit.
                assert_eq!(
                    hazards.checked_panics,
                    checked_panics && !overflow_checks,
                    "{body} overflow_checks={overflow_checks}"
                );
            }
        }
    }

    #[test]
    fn lock_acquisition_seeds_suspend_and_preserves_its_witness() {
        use crate::semantic::call_graph::CallSites;
        for mode in ["", "read ", "write "] {
            for prefix in ["", "await sleep(0); "] {
                let source = format!(
                    "async fn acquire() {{ {prefix}lock {mode}cell as value {{ }} }} fn relay() {{ }}"
                );
                let program = program(&source);
                let acquire = FunctionId::free("acquire");
                let relay = FunctionId::free("relay");
                let mut graph = CallGraph::default();
                graph.merge(
                    relay,
                    CallSites {
                        unsupported_initialization: Default::default(),
                        virtual_calls: Default::default(),
                        targets: [acquire].into(),
                        has_unknown: false,
                    },
                );
                let direct = HashMap::from([(
                    acquire,
                    LockEffectCause {
                        span: Span::dummy(),
                        operation: "earlier wait".into(),
                        kind: LockEffectKind::SuspendOrBlock,
                    },
                )]);
                let lock_only = solve_unit(
                    &program,
                    &graph,
                    &HashMap::<ExprId, Type>::new(),
                    None,
                    &HashMap::new(),
                    &HashMap::new(),
                    |_| PANIC,
                );
                for id in [acquire, relay] {
                    assert!(lock_only.facts.intersects(&id, RuntimeEffects::MAY_SUSPEND));
                    assert!(!lock_only.facts.intersects(&id, RuntimeEffects::MAY_BLOCK));
                }
                let effects = solve_unit(
                    &program,
                    &graph,
                    &HashMap::<ExprId, Type>::new(),
                    None,
                    &HashMap::new(),
                    &direct,
                    |_| PANIC,
                );
                for id in [acquire, relay] {
                    let summary = effects.facts.get(&id).unwrap();
                    assert!(summary.contains(LOCK_EFFECT_WAIT));
                    let cause = summary
                        .witness(RuntimeEffects::MAY_SUSPEND)
                        .unwrap()
                        .lock()
                        .unwrap();
                    assert_eq!(cause.operation, "lock");
                    assert_eq!(cause.span.start, source.find("lock ").unwrap());
                    assert_eq!(
                        summary
                            .witness(RuntimeEffects::MAY_BLOCK)
                            .unwrap()
                            .lock()
                            .unwrap()
                            .operation,
                        "earlier wait"
                    );
                }
            }
        }
    }

    #[test]
    fn lock_scan_isolates_lambda_and_roundtrips_cached_evidence() {
        let program = program("async fn outer() { let f = || { lock cell as value {} }; }");
        let Item::Function(function) = &program.items[0] else {
            panic!("function")
        };
        let direct = scan_direct_effects(
            FunctionId::free("outer"),
            function.body.id,
            &function.body,
            &HashMap::<ExprId, Type>::new(),
            None,
            true,
        );
        assert_eq!(direct.len(), 2);
        assert!(direct[0].lock_span.is_none());
        assert!(direct[1].lock_span.is_some());
        let encoded = serde_json::to_vec(&direct).unwrap();
        let restored: Vec<DirectBodyEffects> = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(restored[1].lock_span, direct[1].lock_span);
    }

    #[test]
    fn cached_lock_evidence_relocates_after_coordinate_only_edit() {
        let before = program("async fn acquire() { lock cell as value {} }");
        let mut after =
            program("\n// shifted declaration\nasync fn acquire() { lock cell as value {} }");
        let Item::Function(old) = &before.items[0] else {
            panic!("function");
        };
        let cached = scan_direct_effects(
            FunctionId::free("acquire"),
            old.body.id,
            &old.body,
            &HashMap::<ExprId, Type>::new(),
            None,
            true,
        );
        let mut correspondence = super::super::syntax::Correspondence::default();
        correspondence.reconcile(&before, &mut after).unwrap();
        let Item::Function(new) = &after.items[0] else {
            panic!("function");
        };
        assert_eq!(new.body.id, old.body.id, "unchanged body is reused");
        // BodyQueries::derived uses this same remapping for cached direct facts.
        let relocated: Vec<DirectBodyEffects> = correspondence.remap(&cached).unwrap();
        let Stmt::Lock(lock) = &new.body.stmts[0] else {
            panic!("lock");
        };
        assert_eq!(relocated[0].lock_span, Some(lock.span));
        assert_ne!(relocated[0].lock_span, cached[0].lock_span);
        assert_eq!(relocated[0].lock_span.unwrap().line, 3);
    }

    #[test]
    fn buildgraph_cached_loop_evidence_relocates() {
        let before = program("fn work() { while true {} }");
        let mut after = program("\n// move loop\nfn work() { while true {} }");
        let Item::Function(old) = &before.items[0] else {
            panic!("function");
        };
        let cached = scan_direct_effects(
            FunctionId::free("work"),
            old.body.id,
            &old.body,
            &HashMap::<ExprId, Type>::new(),
            None,
            true,
        );
        let mut correspondence = super::super::syntax::Correspondence::default();
        correspondence.reconcile(&before, &mut after).unwrap();
        let Item::Function(new) = &after.items[0] else {
            panic!("function");
        };
        let relocated: Vec<DirectBodyEffects> = correspondence.remap(&cached).unwrap();
        assert_eq!(relocated[0].loop_span, Some(new.body.stmts[0].span()));
        assert_ne!(relocated[0].loop_span, cached[0].loop_span);
    }

    #[test]
    fn combined_masks_keep_lock_helper_and_panic_contracts_distinct() {
        use crate::semantic::call_graph::CallSites;
        let program = program(
            "fn leaf() { while true {} let n = 1 / 0; } async fn task() {} fn async_only() {} fn constructor_only() {} fn lambda_only() {} fn union_only() {} fn helper_only() {} class C { init(self) {} }",
        );
        let leaf = FunctionId::free("leaf");
        let task = FunctionId::free("task");
        let constructor = FunctionId::method(TypeId::local("C"), "init");
        let lambda = FunctionId::lambda(BodyId::fresh());
        let dispatch = FunctionId::method(TypeId::local("I"), "run");
        let mut graph = CallGraph::default();
        graph.merge(leaf, CallSites::default());
        for (caller, target) in [
            (task, leaf),
            (constructor, leaf),
            (lambda, leaf),
            (dispatch, leaf),
            (FunctionId::free("async_only"), task),
            (FunctionId::free("constructor_only"), constructor),
            (FunctionId::free("lambda_only"), lambda),
            (FunctionId::free("union_only"), dispatch),
            (FunctionId::free("helper_only"), leaf),
        ] {
            graph.merge(
                caller,
                CallSites {
                    unsupported_initialization: Default::default(),
                    virtual_calls: Default::default(),
                    targets: [target].into(),
                    has_unknown: false,
                },
            );
        }
        let direct = HashMap::from([(
            leaf,
            LockEffectCause {
                span: Span::dummy(),
                operation: "wait".into(),
                kind: LockEffectKind::Block,
            },
        )]);
        let effects = solve_unit(
            &program,
            &graph,
            &HashMap::<ExprId, Type>::new(),
            None,
            &HashMap::from([(task, true)]),
            &direct,
            |_| PANIC,
        );
        for name in [
            "async_only",
            "constructor_only",
            "lambda_only",
            "union_only",
            "helper_only",
        ] {
            let id = FunctionId::free(name);
            assert!(effects.facts.intersects(&id, PANIC), "{name}");
            assert_eq!(
                effects.facts.intersects(&id, LOCK_EFFECT_WAIT),
                name != "async_only",
                "{name}"
            );
            assert_eq!(
                effects.helpers.contains_key(&id),
                name == "helper_only",
                "{name}"
            );
        }
        assert!(effects.facts.intersects(&task, LOCK_EFFECT_WAIT));
        assert!(
            effects
                .facts
                .get(&FunctionId::free("constructor_only"))
                .unwrap()
                .witness(LOCK_EFFECT_WAIT)
                .unwrap()
                .lock()
                .is_some()
        );
    }

    #[test]
    fn inventory_counts_scale_with_syntax_and_edges_and_classify_shared_targets_once() {
        use crate::query_stats::{Counter, Session, count};
        for shape in ["chain", "fanout", "repeated", "cycle", "locks"] {
            let mut previous = None;
            for size in [16usize, 64, 256, 1024] {
                let mut source = String::new();
                for i in 0..size {
                    let target = match shape {
                        "chain" if i + 1 < size => format!("node_{}", i + 1),
                        "cycle" => format!("node_{}", (i + 1) % size),
                        _ => "external".to_string(),
                    };
                    let lock = if shape == "locks" {
                        "lock cell as value {}"
                    } else {
                        ""
                    };
                    source.push_str(&format!("fn node_{i}() {{ {target}(); {lock} }}\n"));
                }
                if shape == "fanout" || shape == "repeated" {
                    source.push_str("fn root() { ");
                    for i in 0..size {
                        source.push_str(&format!(
                            "node_{}();",
                            if shape == "repeated" { 0 } else { i }
                        ));
                    }
                    source.push('}');
                }
                let program = program(&source);
                let graph = crate::semantic::TypeChecker::resolved_effect_graph(&program);
                let calls = std::cell::Cell::new(0);
                let _session = Session::start(true, false);
                solve_unit(
                    &program,
                    &graph,
                    &HashMap::<ExprId, Type>::new(),
                    None,
                    &HashMap::new(),
                    &HashMap::new(),
                    |_| {
                        calls.set(calls.get() + 1);
                        PANIC
                    },
                );
                assert_eq!(count(Counter::EffectSolve), 1);
                assert_eq!(count(Counter::EffectIoSolve), 0);
                assert_eq!(calls.get(), usize::from(shape != "cycle"));
                let work = count(Counter::EffectInventory) + count(Counter::EffectEdges);
                if let Some((previous_size, previous_work)) = previous {
                    // Root scaffolding is constant; all variable work is linear.
                    assert!(work <= previous_work * (size / previous_size));
                }
                previous = Some((size, work));
                eprintln!(
                    "effect-inventory shape={shape} size={size} ast={} edges={} solves=1",
                    count(Counter::EffectInventory),
                    count(Counter::EffectEdges)
                );
            }
        }
    }

    #[test]
    fn tracked_capabilities_backdate_and_gate_real_solver() {
        use super::super::incremental::SyntaxQueries;
        use std::{cell::RefCell, path::Path, rc::Rc};
        let path = Path::new("tracked_effect.wi");
        let root = BodyId::fresh();
        let id = FunctionId::free("get");
        let consumer = crate::module::ModuleId(123);
        let consumer_id = FunctionId::free("consumer");
        let mut accepted = SyntaxQueries::default();
        let mut previous = None;
        let solves = std::cell::Cell::new(0);
        for (step, source) in [
            "fn get() -> i64 { return 1; }",
            "fn get() -> i64 { return 2; }",
            "fn get() -> i64 { println(2); return 2; }",
            "fn get() -> i64 { println(2); return 2; }",
        ]
        .into_iter()
        .enumerate()
        {
            let tracking = Rc::new(RefCell::new(accepted.candidate().unwrap()));
            let (mut source, _, _) = tracking
                .borrow_mut()
                .parse(path, crate::diagnostics::FileId::ENTRY, source)
                .unwrap();
            let Item::Function(function) = &mut source.items[0] else {
                panic!()
            };
            function.body.id = root;
            if !tracking
                .borrow_mut()
                .validate_body(UnitId::ENTRY, root, path, "Function:get", vec![])
                .unwrap()
            {
                tracking
                    .borrow_mut()
                    .publish_body(UnitId::ENTRY, root, serde_json::Value::Null, vec![], vec![])
                    .unwrap();
            }
            let queries = EffectQueries::default();
            *queries.tracking.borrow_mut() = Rc::downgrade(&tracking);
            queries.roots.borrow_mut().insert(UnitId::ENTRY, vec![root]);
            if let Some(previous) = &previous {
                queries.reuse_from(previous);
            }
            let graph = crate::semantic::TypeChecker::resolved_effect_graph(&source);
            queries
                .complete(UnitId::ENTRY, || {
                    solves.set(solves.get() + 1);
                    solve_unit(
                        &source,
                        &graph,
                        &HashMap::<ExprId, Type>::new(),
                        None,
                        &HashMap::new(),
                        &HashMap::new(),
                        |_| PANIC,
                    )
                })
                .unwrap();
            let capabilities = tracking
                .borrow()
                .effect_capabilities(UnitId::ENTRY, id)
                .unwrap();
            assert_eq!(capabilities.may_io, step >= 2);
            let consumer_green = tracking
                .borrow_mut()
                .validate_effects(consumer, vec![])
                .unwrap();
            assert_eq!(consumer_green, step == 1 || step == 3);
            if !consumer_green {
                tracking
                    .borrow_mut()
                    .publish_effects(
                        consumer,
                        vec![],
                        vec![(UnitId::ENTRY, id)],
                        HashMap::from([(
                            consumer_id,
                            CapturedEffect {
                                capabilities,
                                evidence: EffectEvidence::new(EffectSummary::default()),
                            },
                        )]),
                        vec![],
                        serde_json::Value::Null,
                    )
                    .unwrap();
            }
            assert_eq!(solves.get(), (step + 1).min(3));
            previous = Some(queries);
            accepted = Rc::try_unwrap(tracking).unwrap().into_inner();
        }
    }

    #[test]
    fn tracked_lock_evidence_equality_includes_location_and_cause() {
        let summary = |offset| {
            EffectSummary::new(
                LOCK_EFFECT_WAIT,
                Some(EffectWitness::Lock(LockEffectWitness {
                    owner: FunctionId::free("wait"),
                    cause: LockEffectCause {
                        span: Span::new(offset, offset + 2, 1, offset + 1),
                        operation: "sleep".into(),
                        kind: LockEffectKind::Suspend,
                    },
                })),
            )
        };
        // In-solve witness ordering intentionally identifies a cause by owner.
        assert_eq!(summary(1), summary(7));
        assert!(!same_seed(&summary(1), &summary(7)));
        assert_ne!(
            EffectEvidence::new(summary(1)),
            EffectEvidence::new(summary(7))
        );
    }

    #[test]
    fn callable_capabilities_separate_io_and_evidence() {
        fn evaluate(source: &str) -> EffectQueries {
            let queries = EffectQueries::default();
            let source = program(source);
            let graph = crate::semantic::TypeChecker::resolved_effect_graph(&source);
            queries
                .complete(UnitId::ENTRY, || {
                    solve_unit(
                        &source,
                        &graph,
                        &HashMap::<ExprId, Type>::new(),
                        None,
                        &HashMap::new(),
                        &HashMap::new(),
                        |_| PANIC,
                    )
                })
                .unwrap();
            queries
        }
        let first = evaluate("fn get() -> i64 { return 1; } fn caller() -> i64 { return get(); }");
        let second = evaluate("fn get() -> i64 { return 2; } fn caller() -> i64 { return get(); }");
        let io = evaluate(
            "fn get() -> i64 { println(2); return 2; } fn caller() -> i64 { return get(); }",
        );
        for name in ["get", "caller"] {
            let id = FunctionId::free(name);
            assert_eq!(
                first.effect_capabilities(UnitId::ENTRY, id),
                second.effect_capabilities(UnitId::ENTRY, id)
            );
            assert!(!first.effect_capabilities(UnitId::ENTRY, id).unwrap().may_io);
            assert!(io.effect_capabilities(UnitId::ENTRY, id).unwrap().may_io);
        }
        let first = evaluate("fn danger(n: i64) -> i64 { return 1 / n; }");
        let shifted = evaluate("  fn danger(n: i64) -> i64 { return 1 / n; }");
        let id = FunctionId::free("danger");
        assert_eq!(
            first.effect_capabilities(UnitId::ENTRY, id),
            shifted.effect_capabilities(UnitId::ENTRY, id)
        );
        assert_ne!(
            first.effect_evidence(UnitId::ENTRY, id),
            shifted.effect_evidence(UnitId::ENTRY, id)
        );
    }

    #[test]
    fn imported_capabilities_resolve_aliases_and_keep_unknown_units_conservative() {
        let mut queries = EffectQueries::default();
        let dependency = crate::module::ModuleId(7);
        queries.names.insert("dependency".to_owned(), dependency);
        let source = program("fn pure() -> i64 { return 1; } fn output() { println(1); }");
        let graph = crate::semantic::TypeChecker::resolved_effect_graph(&source);
        let imports = HashMap::from([("alias".to_owned(), "dependency".to_owned())]);
        let target = |name| FunctionId::method(TypeId::local("alias"), name);
        let missing = queries.external_capabilities(UnitId::ENTRY, &target("init"), &imports);
        assert!(missing.may_io);
        assert!(missing.runtime.intersects(PANIC));
        queries
            .complete(dependency, || {
                solve_unit(
                    &source,
                    &graph,
                    &HashMap::<ExprId, Type>::new(),
                    None,
                    &HashMap::new(),
                    &HashMap::new(),
                    |_| PANIC,
                )
            })
            .unwrap();
        assert!(
            !queries
                .external_capabilities(UnitId::ENTRY, &target("pure"), &imports)
                .may_io
        );
        assert!(
            queries
                .external_capabilities(UnitId::ENTRY, &target("output"), &imports)
                .may_io
        );
        assert!(
            queries
                .external_capabilities(UnitId::ENTRY, &target("missing"), &imports)
                .may_io
        );
    }

    #[test]
    fn io_propagates_through_cycles_and_unknown_calls() {
        let queries = EffectQueries::default();
        let source = program(
            "fn a() { b(); } fn b() { a(); println(1); } fn indirect(f: fn() -> i64) -> i64 { return f(); } fn pure() { pow(2, 3); }",
        );
        let graph = crate::semantic::TypeChecker::resolved_effect_graph(&source);
        queries
            .complete(UnitId::ENTRY, || {
                solve_unit(
                    &source,
                    &graph,
                    &HashMap::<ExprId, Type>::new(),
                    None,
                    &HashMap::new(),
                    &HashMap::new(),
                    |_| PANIC,
                )
            })
            .unwrap();
        for name in ["a", "b", "indirect"] {
            assert!(
                queries
                    .effect_capabilities(UnitId::ENTRY, FunctionId::free(name))
                    .unwrap()
                    .may_io
            );
        }
        assert!(
            !queries
                .effect_capabilities(UnitId::ENTRY, FunctionId::free("pure"))
                .unwrap()
                .may_io
        );
    }

    #[test]
    fn unified_query_reuses_all_facts_and_isolates_units() {
        let queries = EffectQueries::default();
        let source = program("fn first() { first(); }");
        let graph = crate::semantic::TypeChecker::resolved_effect_graph(&source);
        for unit in [UnitId::ENTRY, crate::module::ModuleId(1)] {
            let facts = queries
                .complete(unit, || {
                    solve_unit(
                        &source,
                        &graph,
                        &HashMap::<ExprId, Type<TypeId>>::new(),
                        None,
                        &HashMap::new(),
                        &HashMap::new(),
                        |_| PANIC,
                    )
                })
                .unwrap();
            assert!(facts.helpers.contains_key(&FunctionId::free("first")));
            assert!(!facts.facts.intersects(&FunctionId::free("first"), PANIC));
            for _ in 0..8 {
                let hit = queries.complete(unit, || panic!("recomputed")).unwrap();
                assert!(Arc::ptr_eq(&facts, &hit));
            }
        }
        assert_eq!(queries.units.stats().computations, 2);
        assert_eq!(queries.units.stats().hits, 16);
    }

    #[test]
    fn recursive_components_guards_and_unknown_calls_keep_their_effects() {
        let program = program(
            "fn a() { b(); } fn b() { a(); } fn danger(n: i64) -> i64 { return 10 / n; } fn caller(n: i64) -> i64 { return danger(n); } fn indirect(f: fn() -> i64) -> i64 { return f(); }",
        );
        let facts = analyze(&program, &[], &HashMap::new(), |_| PANIC);
        assert!(!facts[&FunctionId::free("a")]);
        assert!(!facts[&FunctionId::free("b")]);
        assert!(facts[&FunctionId::free("danger")]);
        assert!(facts[&FunctionId::free("caller")]);
        assert!(facts[&FunctionId::free("indirect")]);
    }
    #[test]
    fn nested_lambda_hazards_belong_to_the_lambda_and_constructors_union() {
        let program = program(
            "fn outer() { let f = || { return 10 / 0; }; } class C { pub init(self) { let n = 10 / 0; } pub init(self, n: i64) {} }",
        );
        let Item::Function(outer) = &program.items[0] else {
            panic!("function");
        };
        let Stmt::Let(binding) = &outer.body.stmts[0] else {
            panic!("binding");
        };
        let Expr::Lambda(lambda) = &binding.init else {
            panic!("lambda");
        };
        let lambda_id = FunctionId::free("lifted_lambda");
        let facts = analyze(&program, &[(lambda_id, lambda)], &HashMap::new(), |_| PANIC);
        assert!(!facts[&FunctionId::free("outer")]);
        assert!(facts[&lambda_id]);
        assert!(facts[&FunctionId::method(TypeId::local("C"), "init")]);
    }
}

/// Immutable callable declarations shared by body evaluators.
#[derive(Default, Clone)]
pub(crate) struct CallableIndex {
    pub(crate) callables: HashMap<FunctionId, bool>,
    pub(crate) hierarchy: Arc<ClassHierarchy>,
}

/// Typed effect contributions and lock-held diagnostic sites for one unit/body.
#[derive(Default)]
pub(crate) struct EffectInputs {
    pub(crate) edges: HashMap<FunctionId, HashSet<FunctionId>>,
    pub(crate) direct: HashMap<FunctionId, LockEffectCause>,
    pub(crate) direct_sites: Vec<LockEffectCause>,
    pub(crate) sites: Vec<LockEffectCallsite>,
}

impl super::retained::Retained for EffectCapabilities {
    fn heap_bytes(&self) -> usize {
        0
    }
}
impl super::retained::Retained for EffectEvidence {
    fn heap_bytes(&self) -> usize {
        // Canonical evidence contains the same owned witness strings. Charge
        // it twice plus the maximum B-tree storage for all runtime effect bits.
        2 * self.canonical.heap_bytes()
            + RuntimeEffects::BIT_COUNT as usize * (std::mem::size_of::<EffectWitness>() + 64) * 2
    }
}
impl super::retained::Retained for CapturedEffect {
    fn heap_bytes(&self) -> usize {
        self.evidence.heap_bytes()
    }
}
