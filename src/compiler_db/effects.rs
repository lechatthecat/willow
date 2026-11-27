//! One session-local helper/lock/panic fixpoint over typed callable identities.
//! Imported facts use semantic names; linker spelling belongs to the backend.

mod delta;
use delta::*;

use super::{incremental::SourceOwner, query::QueryTable};
use crate::driver::OverflowChecks;
use crate::qualified_name::QualifiedName;
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
/// Checked operations that the resolved policy wraps. Every mask that transmits
/// [`PANIC`] also transmits this bit, so `PANIC | CHECKED_PANIC` of any
/// solution equals the fully overflow-checked solution's `PANIC` exactly.
const CHECKED_PANIC: RuntimeEffects = RuntimeEffects::MAY_CHECKED_PANIC;
/// Profile-independent panic summary read by checker verdicts.
pub(crate) const ANY_PANIC: RuntimeEffects = PANIC.union(CHECKED_PANIC);

use crate::semantic::concurrency::{NonpreemptibleHelper, NonpreemptibleReason};
const NO_PREEMPT: RuntimeEffects = RuntimeEffects::NO_PREEMPT_REGION;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
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
    Allocation {
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

#[derive(Debug, Clone)]
pub(crate) struct EffectEvidence {
    pub(crate) summary: Arc<EffectSummary<EffectWitness>>,
}
impl PartialEq for EffectEvidence {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.summary, &other.summary) || same_seed(&self.summary, &other.summary)
    }
}
impl Eq for EffectEvidence {}
#[cfg(test)]
impl EffectEvidence {
    fn new(summary: EffectSummary<EffectWitness>) -> Self {
        Self {
            summary: Arc::new(summary),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapturedEffect {
    pub(crate) capabilities: EffectCapabilities,
    pub(crate) evidence: EffectEvidence,
    pub(crate) helper: Option<(Span, bool)>,
    pub(crate) loop_bodies: Vec<BodyId>,
}

type EffectRootInventory = im::HashMap<BodyId, Arc<Vec<FunctionId>>>;
/// One unit's lock-order facts and the import map that resolves its external calls.
type LockOrderFacts = (
    crate::semantic::type_checker::lock_order::Inputs,
    Arc<HashMap<String, String>>,
);

#[derive(Clone)]
pub(crate) struct UnitEffects {
    pub(crate) facts: EffectFacts<EffectWitness>,
    pub(crate) helpers: Arc<super::HelperSummary>,
    loop_bodies: im::HashSet<BodyId>,
    callables: im::HashMap<FunctionId, bool>,
    may_io: im::HashSet<FunctionId>,
    pub(crate) preemption_bodies: im::HashSet<FunctionId>,
    pub(crate) preemption_loops: im::HashSet<FunctionId>,
    pub(crate) preemption_copies: im::OrdSet<(FunctionId, UnitId, FunctionId)>,
    plan: EffectPlan,
}

impl UnitEffects {
    /// Returns the number of retained parts and spans inspected.
    fn relocate(&mut self, correspondence: &super::syntax::Correspondence) -> usize {
        let span = |old: Span| correspondence.span(old).filter(|new| *new != old);
        let plan = &mut self.plan;
        let mut visits = 0;
        let mut parts = Vec::new();
        for (&id, owned) in &plan.direct {
            for (&root, part) in owned {
                visits += 1;
                let moved = |old: Option<Span>| old.and_then(span);
                let checked = moved(part.checked_span);
                let panic = moved(part.panic_span);
                let lock = moved(part.lock_span);
                let looped = moved(part.loop_span);
                let target = moved(part.lock_target.as_ref().map(|t| t.target));
                if checked.is_none()
                    && panic.is_none()
                    && lock.is_none()
                    && looped.is_none()
                    && target.is_none()
                {
                    continue;
                }
                let mut relocated = (**part).clone();
                relocated.checked_span = checked.or(relocated.checked_span);
                relocated.panic_span = panic.or(relocated.panic_span);
                relocated.lock_span = lock.or(relocated.lock_span);
                relocated.loop_span = looped.or(relocated.loop_span);
                if let (Some(lock_target), Some(new)) = (&mut relocated.lock_target, target) {
                    lock_target.target = new;
                }
                parts.push((id, root, Arc::new(relocated)));
            }
        }
        for (id, root, part) in parts {
            plan.direct.entry(id).or_default().insert(root, part);
        }
        for spans in [&mut plan.helpers, &mut plan.loops] {
            visits += spans.len();
            let moved: Vec<_> = spans
                .iter()
                .filter_map(|(&id, &old)| Some((id, span(old)?)))
                .collect();
            spans.extend(moved);
        }
        visits += self.helpers.len();
        let moved: Vec<_> = self
            .helpers
            .iter()
            .filter_map(|(&id, helper)| Some((id, span(helper.span)?)))
            .collect();
        if !moved.is_empty() {
            let helpers = Arc::make_mut(&mut self.helpers);
            for (id, new) in moved {
                if let Some(helper) = helpers.get_mut(&id) {
                    helper.span = new;
                }
            }
        }
        visits
    }
}

#[derive(Clone, Default)]
struct EffectPlan {
    roots: EffectRootInventory,
    source_roots: im::HashMap<String, im::HashSet<BodyId>>,
    helpers: im::HashMap<FunctionId, crate::diagnostics::Span>,
    loops: im::HashMap<FunctionId, crate::diagnostics::Span>,
    bodies: im::HashMap<FunctionId, BodyId>,
    direct: im::HashMap<FunctionId, im::HashMap<BodyId, Arc<DirectBodyEffects>>>,
    external: im::HashMap<(UnitId, FunctionId), im::HashSet<FunctionId>>,
    providers: im::HashSet<UnitId>,
    copy_sources: im::HashMap<UnitId, im::HashSet<FunctionId>>,
    copies: im::HashMap<FunctionId, CopyContribution>,
    copy_targets: im::HashMap<(UnitId, FunctionId), im::HashSet<FunctionId>>,
    inherited: im::HashMap<BodyId, CopyContribution>,
    inherited_targets: im::HashMap<(UnitId, FunctionId), im::HashSet<FunctionId>>,
    ambiguous: bool,
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
struct CopyContribution {
    body: BodyId,
    source_body: BodyId,
    provider: UnitId,
    source: FunctionId,
}

/// Source inventory locator supplied while declarations are registered.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub(crate) enum EffectRootLocation {
    Function(usize),
    Method { item: usize, member: usize },
    Constructor { item: usize, member: usize },
    InterfaceDefault { item: usize, member: usize },
}
impl EffectRootLocation {
    fn span(self, program: &Program) -> Option<crate::diagnostics::Span> {
        match self {
            Self::Function(item) => match program.items.get(item)? {
                Item::Function(f) => Some(f.span),
                _ => None,
            },
            Self::Method { item, member } => match program.items.get(item)? {
                Item::Class(c) => Some(c.methods.get(member)?.span),
                _ => None,
            },
            Self::Constructor { item, member } => match program.items.get(item)? {
                Item::Class(c) => Some(c.constructors.get(member)?.span),
                _ => None,
            },
            Self::InterfaceDefault { item, member } => match program.items.get(item)? {
                Item::Interface(i) => Some(i.methods.get(member)?.span),
                _ => None,
            },
        }
    }
    fn block(self, program: &Program) -> Option<&Block> {
        match self {
            Self::Function(item) => match program.items.get(item)? {
                // The parser gives a foreign declaration a placeholder block.
                // Its effects are seeded from the declaration by the full unit solver.
                Item::Function(f) if f.rust_bridge.is_none() => Some(&f.body),
                _ => None,
            },
            Self::Method { item, member } => match program.items.get(item)? {
                Item::Class(c) => Some(&c.methods.get(member)?.body),
                _ => None,
            },
            Self::Constructor { item, member } => match program.items.get(item)? {
                Item::Class(c) => Some(&c.constructors.get(member)?.body),
                _ => None,
            },
            Self::InterfaceDefault { item, member } => match program.items.get(item)? {
                Item::Interface(i) => i.methods.get(member)?.default_body.as_ref(),
                _ => None,
            },
        }
    }
}

pub(crate) struct EffectQueries {
    /// Body-derived purity for this checking session; external declarations and
    /// persistent runtime-effect summaries cannot supply these proofs.
    pub(crate) purity_facts: std::cell::RefCell<
        HashMap<UnitId, HashMap<FunctionId, crate::semantic::type_checker::purity::PuritySummary>>,
    >,
    /// Verified contract proofs for this checking session only. Never restored
    /// from persistent runtime summaries or copied across source revisions.
    pub(crate) contract_facts: std::cell::RefCell<HashMap<UnitId, Arc<EffectFacts<EffectWitness>>>>,
    pub(crate) static_initializers: super::static_init::StaticInitQueries,
    pub(crate) lock_orders: std::cell::RefCell<HashMap<UnitId, LockOrderFacts>>,
    root_locations: std::cell::RefCell<HashMap<(UnitId, BodyId), EffectRootLocation>>,
    unseen_roots: std::cell::RefCell<HashMap<UnitId, EffectRootInventory>>,
    active_unit: std::cell::Cell<Option<UnitId>>,
    affected: std::cell::RefCell<HashMap<UnitId, HashSet<FunctionId>>>,
    relocated: std::cell::RefCell<HashMap<UnitId, HashSet<FunctionId>>>,
    full_units: std::cell::RefCell<HashSet<UnitId>>,
    solutions: std::cell::RefCell<
        HashMap<UnitId, Arc<crate::semantic::effects::EffectSolution<EffectWitness>>>,
    >,
    io_solutions:
        std::cell::RefCell<HashMap<UnitId, Arc<crate::semantic::effects::EffectSolution<u8>>>>,
    scanned: std::cell::Cell<usize>,
    recomputed: std::cell::Cell<usize>,
    compared: std::cell::Cell<usize>,
    /// Retained direct parts and helper/loop spans inspected by relocation.
    relocation_visits: std::cell::Cell<usize>,
    body_queries: std::cell::RefCell<std::rc::Weak<super::body::BodyQueries>>,
    pub(crate) analysis: std::cell::RefCell<Option<HashMap<UnitId, crate::ai::CapturedUnit>>>,
    // Frozen unit assembly for backend reads. Direct-body queries and callable
    // equations own recomputation; this table does not invalidate their caches.
    units: QueryTable<UnitId, UnitEffects>,
    completed: std::cell::RefCell<HashMap<UnitId, Arc<UnitEffects>>>,
    previous: std::cell::RefCell<HashMap<UnitId, Arc<UnitEffects>>>,
    tracking:
        std::cell::RefCell<std::rc::Weak<std::cell::RefCell<super::incremental::SyntaxQueries>>>,
    imported_reads: std::cell::RefCell<HashMap<UnitId, HashSet<(UnitId, FunctionId)>>>,
    source_reads: std::cell::RefCell<HashMap<UnitId, HashSet<UnitId>>>,
    roots: std::cell::RefCell<HashMap<UnitId, Vec<BodyId>>>,
    names: HashMap<String, UnitId>,
    dependencies: Option<std::rc::Rc<super::dependencies::ModuleDependencies>>,
    /// Resolved arithmetic/shift policy. Sessions without options conservatively
    /// retain all overflow fault paths.
    overflow_checks: OverflowChecks,
    /// Function-value facts of the units checked so far in this session, for
    /// E2604 through indirect calls.
    pub(crate) indirect_lock: std::cell::RefCell<IndirectLockSession>,
}
impl Default for EffectQueries {
    fn default() -> Self {
        Self {
            purity_facts: Default::default(),
            contract_facts: Default::default(),
            static_initializers: Default::default(),
            lock_orders: Default::default(),
            root_locations: Default::default(),
            unseen_roots: Default::default(),
            active_unit: Default::default(),
            affected: Default::default(),
            relocated: Default::default(),
            full_units: Default::default(),
            solutions: Default::default(),
            io_solutions: Default::default(),
            scanned: Default::default(),
            recomputed: Default::default(),
            compared: Default::default(),
            relocation_visits: Default::default(),
            body_queries: Default::default(),
            analysis: Default::default(),
            units: QueryTable::named("unit_effects"),
            completed: Default::default(),
            previous: Default::default(),
            tracking: Default::default(),
            imported_reads: Default::default(),
            source_reads: Default::default(),
            roots: Default::default(),
            names: HashMap::new(),
            dependencies: None,
            overflow_checks: true.into(),
            indirect_lock: Default::default(),
        }
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct EffectSnapshot {
    solutions: Vec<(
        UnitId,
        crate::semantic::effects::StoredEffectSolution<EffectWitness>,
    )>,
    io: Vec<(UnitId, crate::semantic::effects::StoredEffectSolution<u8>)>,
    #[serde(default)]
    units: Vec<(UnitId, StoredEffectUnit)>,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredEffectUnit {
    #[serde(default)]
    copy_contributions: Vec<(FunctionId, CopyContribution)>,
    #[serde(default)]
    inherited_contributions: Vec<(FunctionId, CopyContribution)>,
    #[serde(default)]
    providers: Vec<UnitId>,
    #[serde(default)]
    ambiguous: bool,
    helpers: Vec<(FunctionId, crate::diagnostics::Span, NonpreemptibleReason)>,
    loop_bodies: Vec<BodyId>,
    callables: Vec<(FunctionId, bool)>,
    may_io: Vec<FunctionId>,
    own: Vec<FunctionId>,
    preemption_loops: Vec<FunctionId>,
    copies: Vec<(FunctionId, UnitId, FunctionId)>,
    roots: Vec<(BodyId, Vec<FunctionId>)>,
    source_roots: Vec<(String, Vec<BodyId>)>,
    helper_spans: Vec<(FunctionId, crate::diagnostics::Span)>,
    loops: Vec<(FunctionId, crate::diagnostics::Span)>,
    bodies: Vec<(FunctionId, BodyId)>,
    direct: Vec<(FunctionId, Vec<(BodyId, DirectBodyEffects)>)>,
    external: Vec<((UnitId, FunctionId), Vec<FunctionId>)>,
}
impl StoredEffectUnit {
    fn store(unit: &UnitEffects) -> Self {
        Self {
            inherited_contributions: unit
                .plan
                .inherited
                .iter()
                .filter_map(|(root, copy)| {
                    unit.plan
                        .roots
                        .get(root)
                        .and_then(|ids| ids.first())
                        .map(|&id| (id, *copy))
                })
                .collect(),
            copy_contributions: unit
                .plan
                .copies
                .iter()
                .map(|(&id, &copy)| (id, copy))
                .collect(),
            providers: unit.plan.providers.iter().copied().collect(),
            ambiguous: unit.plan.ambiguous,
            helpers: unit
                .helpers
                .iter()
                .map(|(&id, h)| (id, h.span, h.reason))
                .collect(),
            loop_bodies: unit.loop_bodies.iter().copied().collect(),
            callables: unit.callables.iter().map(|(&id, &a)| (id, a)).collect(),
            may_io: unit.may_io.iter().copied().collect(),
            own: unit.preemption_bodies.iter().copied().collect(),
            preemption_loops: unit.preemption_loops.iter().copied().collect(),
            copies: unit.preemption_copies.iter().copied().collect(),
            source_roots: unit
                .plan
                .source_roots
                .iter()
                .map(|(key, roots)| (key.clone(), roots.iter().copied().collect()))
                .collect(),
            roots: unit
                .plan
                .roots
                .iter()
                .map(|(&id, f)| (id, (**f).clone()))
                .collect(),
            helper_spans: unit.plan.helpers.iter().map(|(&id, &s)| (id, s)).collect(),
            loops: unit.plan.loops.iter().map(|(&id, &s)| (id, s)).collect(),
            bodies: unit.plan.bodies.iter().map(|(&id, &b)| (id, b)).collect(),
            direct: unit
                .plan
                .direct
                .iter()
                .map(|(&id, parts)| {
                    (
                        id,
                        parts
                            .iter()
                            .map(|(&root, p)| (root, (**p).clone()))
                            .collect(),
                    )
                })
                .collect(),
            external: unit
                .plan
                .external
                .iter()
                .map(|(&origin, targets)| (origin, targets.iter().copied().collect()))
                .collect(),
        }
    }
    fn restore(self, facts: EffectFacts<EffectWitness>) -> anyhow::Result<UnitEffects> {
        let direct: im::HashMap<_, im::HashMap<_, _>> = self
            .direct
            .into_iter()
            .map(|(id, parts)| {
                (
                    id,
                    parts
                        .into_iter()
                        .map(|(root, part)| (root, Arc::new(part)))
                        .collect(),
                )
            })
            .collect();
        for (&id, parts) in &direct {
            anyhow::ensure!(
                facts.get(&id).is_some() && parts.values().all(|p| p.id == id),
                "invalid direct effect inventory"
            );
        }
        for (_, ids) in &self.roots {
            anyhow::ensure!(
                ids.iter().all(|id| facts.get(id).is_some()),
                "missing direct effect root"
            );
        }
        let mut copy_sources: im::HashMap<UnitId, im::HashSet<FunctionId>> = Default::default();
        for &(_, provider, source) in &self.copies {
            copy_sources.entry(provider).or_default().insert(source);
        }
        let contribution_ids: HashSet<_> =
            self.copy_contributions.iter().map(|(id, _)| *id).collect();
        anyhow::ensure!(
            self.copies
                .iter()
                .all(|(id, _, _)| contribution_ids.contains(id)),
            "missing canonical copy contribution index"
        );
        let mut copy_targets: im::HashMap<_, im::HashSet<FunctionId>> = Default::default();
        for &(id, copy) in &self.copy_contributions {
            copy_targets
                .entry((copy.provider, copy.source))
                .or_default()
                .insert(id);
        }
        let mut inherited_targets: im::HashMap<_, im::HashSet<FunctionId>> = Default::default();
        let mut inherited = im::HashMap::new();
        for (id, copy) in self.inherited_contributions {
            inherited_targets
                .entry((copy.provider, copy.source))
                .or_default()
                .insert(id);
            inherited.insert(copy.body, copy);
        }
        Ok(UnitEffects {
            facts,
            helpers: Arc::new(
                self.helpers
                    .into_iter()
                    .map(|(id, span, reason)| (id, NonpreemptibleHelper { span, reason }))
                    .collect(),
            ),
            loop_bodies: self.loop_bodies.into_iter().collect(),
            callables: self.callables.into_iter().collect(),
            may_io: self.may_io.into_iter().collect(),
            preemption_bodies: self.own.into_iter().collect(),
            preemption_loops: self.preemption_loops.into_iter().collect(),
            preemption_copies: self.copies.into_iter().collect(),
            plan: EffectPlan {
                copy_sources,
                copies: self.copy_contributions.into_iter().collect(),
                copy_targets,
                inherited,
                inherited_targets,
                providers: self.providers.into_iter().collect(),
                ambiguous: self.ambiguous,
                roots: self
                    .roots
                    .into_iter()
                    .map(|(root, ids)| (root, Arc::new(ids)))
                    .collect(),
                source_roots: self
                    .source_roots
                    .into_iter()
                    .map(|(key, roots)| (key, roots.into_iter().collect()))
                    .collect(),
                helpers: self.helper_spans.into_iter().collect(),
                loops: self.loops.into_iter().collect(),
                bodies: self.bodies.into_iter().collect(),
                direct,
                external: self
                    .external
                    .into_iter()
                    .map(|(origin, targets)| (origin, targets.into_iter().collect()))
                    .collect(),
            },
        })
    }
}

impl EffectQueries {
    pub(crate) fn register_root(&self, unit: UnitId, body: BodyId, location: EffectRootLocation) {
        self.unseen_roots
            .borrow_mut()
            .entry(unit)
            .or_insert_with(|| {
                self.previous
                    .borrow()
                    .get(&unit)
                    .map(|state| state.plan.roots.clone())
                    .unwrap_or_default()
            })
            .remove(&body);
        self.root_locations
            .borrow_mut()
            .insert((unit, body), location);
    }
    pub(crate) fn removed_roots(&self, unit: UnitId) -> im::HashSet<BodyId> {
        let remaining = self
            .unseen_roots
            .borrow()
            .get(&unit)
            .cloned()
            .unwrap_or_else(|| {
                self.previous
                    .borrow()
                    .get(&unit)
                    .map(|state| state.plan.roots.clone())
                    .unwrap_or_default()
            });
        remaining.keys().copied().collect()
    }
    pub(crate) fn affected_functions(&self, unit: UnitId) -> HashSet<FunctionId> {
        self.affected
            .borrow()
            .get(&unit)
            .cloned()
            .unwrap_or_default()
    }
    pub(crate) fn disk_snapshot(&self) -> EffectSnapshot {
        EffectSnapshot {
            units: self
                .completed
                .borrow()
                .iter()
                .filter(|(unit, _)| {
                    self.solutions.borrow().contains_key(unit)
                        && self.io_solutions.borrow().contains_key(unit)
                })
                .map(|(&unit, state)| (unit, StoredEffectUnit::store(state)))
                .collect(),
            solutions: self
                .solutions
                .borrow()
                .iter()
                .map(|(&unit, s)| (unit, s.store()))
                .collect(),
            io: self
                .io_solutions
                .borrow()
                .iter()
                .map(|(&unit, s)| (unit, s.store()))
                .collect(),
        }
    }
    pub(crate) fn reuse_solutions(&self, previous: &Self) {
        self.previous
            .borrow_mut()
            .clone_from(&previous.previous.borrow());
        self.solutions
            .borrow_mut()
            .clone_from(&previous.solutions.borrow());
        self.io_solutions
            .borrow_mut()
            .clone_from(&previous.io_solutions.borrow());
    }
    pub(crate) fn retain_units(&self, units: &HashSet<UnitId>) {
        self.previous
            .borrow_mut()
            .retain(|unit, _| units.contains(unit));
        self.solutions
            .borrow_mut()
            .retain(|unit, _| units.contains(unit));
        self.io_solutions
            .borrow_mut()
            .retain(|unit, _| units.contains(unit));
    }

    pub(crate) fn relocate_solutions(&self, correspondence: &super::syntax::Correspondence) {
        let files = correspondence.moved_files();
        if files.is_empty() {
            return;
        }
        // Retained spans belong to their unit's file, or to a provider file for
        // copied/inherited bodies. Units outside both keep every coordinate.
        let moved: HashSet<UnitId> = files
            .iter()
            .map(|file| crate::module::ModuleId(file.0))
            .collect();
        let previous = self.previous.borrow();
        let affected: HashSet<UnitId> = previous
            .iter()
            .filter(|(unit, state)| {
                moved.contains(unit)
                    || state
                        .plan
                        .providers
                        .iter()
                        .any(|provider| moved.contains(provider))
            })
            .map(|(&unit, _)| unit)
            .chain(moved.iter().copied())
            .collect();
        drop(previous);
        let locations: HashMap<_, _> = correspondence
            .spans
            .iter()
            .map(|(old, new)| {
                (
                    (old.file_id.0, old.start, old.end),
                    (new.file_id.0, new.start, new.end),
                )
            })
            .collect();
        for (&unit, solution) in self
            .solutions
            .borrow_mut()
            .iter_mut()
            .filter(|(unit, _)| affected.contains(unit))
        {
            let relocated = Arc::make_mut(solution).map_witnesses(|witness| {
                let mut changed = false;
                match witness {
                    EffectWitness::Lock(lock) => {
                        if let Some(span) = correspondence.spans.get(&lock.cause.span) {
                            changed |= lock.cause.span != *span;
                            lock.cause.span = *span;
                        }
                        if let Some(target) = &mut lock.cause.lock
                            && let Some(span) = correspondence.spans.get(&target.target)
                        {
                            changed |= target.target != *span;
                            target.target = *span;
                        }
                    }
                    EffectWitness::Helper { source, .. }
                    | EffectWitness::Panic { source, .. }
                    | EffectWitness::Allocation { source, .. } => {
                        if let Some(location) = locations.get(source) {
                            changed |= *source != *location;
                            *source = *location;
                        }
                    }
                    EffectWitness::External(_) => {}
                }
                changed
            });
            self.relocated
                .borrow_mut()
                .entry(unit)
                .or_default()
                .extend(relocated);
        }
        // Retained direct parts reseed equations after later edits; keep their
        // coordinates current so a reseed never sees a span several edits old.
        let mut previous = self.previous.borrow_mut();
        for unit in &affected {
            if let Some(state) = previous.get_mut(unit) {
                let visits = Arc::make_mut(state).relocate(correspondence);
                self.relocation_visits
                    .set(self.relocation_visits.get() + visits);
            }
        }
    }

    pub(crate) fn restore_solutions(&self, snapshot: EffectSnapshot) -> anyhow::Result<()> {
        let mut solutions = HashMap::new();
        for (unit, stored) in snapshot.solutions {
            anyhow::ensure!(
                solutions
                    .insert(unit, Arc::new(stored.restore()?))
                    .is_none(),
                "duplicate effect unit"
            );
        }
        let mut io = HashMap::new();
        for (unit, stored) in snapshot.io {
            anyhow::ensure!(
                io.insert(unit, Arc::new(stored.restore()?)).is_none(),
                "duplicate IO effect unit"
            );
        }
        let mut units = HashMap::new();
        for (unit, stored) in snapshot.units {
            let facts = solutions
                .get(&unit)
                .ok_or_else(|| anyhow::anyhow!("missing unit effect solution"))?
                .facts
                .clone();
            anyhow::ensure!(io.contains_key(&unit), "missing unit IO solution");
            anyhow::ensure!(
                units
                    .insert(unit, Arc::new(stored.restore(facts)?))
                    .is_none(),
                "duplicate retained effect unit"
            );
        }
        *self.previous.borrow_mut() = units;
        *self.solutions.borrow_mut() = solutions;
        *self.io_solutions.borrow_mut() = io;
        Ok(())
    }

    pub(crate) fn equation_comparisons(&self) -> usize {
        self.compared.get()
    }
    pub(crate) fn work(&self) -> (usize, usize) {
        (self.scanned.get(), self.recomputed.get())
    }
    #[cfg(test)]
    pub(crate) fn relocation_visits(&self) -> usize {
        self.relocation_visits.get()
    }

    pub(crate) fn new(
        modules: &[crate::module::ResolvedModule],
        dependencies: std::rc::Rc<super::dependencies::ModuleDependencies>,
        overflow_checks: OverflowChecks,
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
            let roots = Vec::new();
            let green = match &tracking {
                Some(tracking) => tracking
                    .borrow_mut()
                    .validate_effects(unit, roots.clone())?,
                None => false,
            };
            let changed_bodies = tracking
                .as_ref()
                .is_some_and(|tracking| !tracking.borrow().changed_bodies_in(unit).is_empty());
            let previous = self
                .previous
                .borrow()
                .get(&unit)
                .cloned()
                .filter(|_| green && !changed_bodies);
            let reused = previous.is_some();
            let mut result = match previous {
                Some(previous) => (*previous).clone(),
                None => {
                    self.active_unit.set(Some(unit));
                    let result = compute();
                    self.active_unit.set(None);
                    let mut affected = self
                        .solutions
                        .borrow()
                        .get(&unit)
                        .map(|s| s.affected.clone())
                        .unwrap_or_default();
                    if let Some(io) = self.io_solutions.borrow().get(&unit) {
                        affected.extend(io.affected.iter().copied());
                    }
                    self.affected.borrow_mut().insert(unit, affected);
                    result
                }
            };
            let relocated = self
                .relocated
                .borrow_mut()
                .remove(&unit)
                .unwrap_or_default();
            if let Some(solution) = self.solutions.borrow().get(&unit) {
                result.facts = solution.facts.clone();
            }
            if let Some(tracking) = tracking.filter(|_| !reused || !relocated.is_empty()) {
                let full = self.full_units.borrow_mut().remove(&unit)
                    || !self.solutions.borrow().contains_key(&unit)
                    || !tracking.borrow().has_effect_inventory(unit);
                let mut owners: HashSet<_> = if full {
                    result
                        .facts
                        .iter()
                        .map(|(&id, _)| id)
                        .chain(tracking.borrow().effect_inventory_ids(unit))
                        .collect()
                } else {
                    self.affected
                        .borrow()
                        .get(&unit)
                        .cloned()
                        .unwrap_or_default()
                };
                owners.extend(relocated);
                let inventory = owners
                    .into_iter()
                    .map(|id| {
                        let effect = result.facts.shared(&id).map(|summary| {
                            let mut loop_bodies: Vec<_> = result
                                .plan
                                .direct
                                .get(&id)
                                .into_iter()
                                .flat_map(|parts| parts.values())
                                .filter(|part| part.loop_span.is_some())
                                .map(|part| part.body)
                                .collect();
                            if let Some(copy) = result.plan.copies.get(&id)
                                && result.loop_bodies.contains(&copy.body)
                            {
                                loop_bodies.push(copy.body);
                            }
                            loop_bodies.sort();
                            loop_bodies.dedup();
                            CapturedEffect {
                                capabilities: EffectCapabilities {
                                    runtime: summary.effects(),
                                    may_io: result.may_io.contains(&id),
                                },
                                evidence: EffectEvidence {
                                    summary: Arc::clone(summary),
                                },
                                helper: result.helpers.get(&id).map(|helper| {
                                    (
                                        helper.span,
                                        matches!(helper.reason, NonpreemptibleReason::Loop),
                                    )
                                }),
                                loop_bodies,
                            }
                        });
                        (id, effect)
                    })
                    .collect();
                self.imported_reads.borrow_mut().remove(&unit);
                if let Some(reads) = self.source_reads.borrow_mut().remove(&unit) {
                    result.plan.providers.extend(reads);
                }
                let source_units = result.plan.providers.iter().copied().collect();
                tracking.borrow_mut().publish_effect_delta(
                    unit,
                    roots,
                    Vec::new(),
                    inventory,
                    source_units,
                )?;
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
        let completed = self.completed.borrow();
        let result = completed.get(&unit)?;
        Some(Arc::new(EffectCapabilities {
            runtime: result.facts.get(&id)?.effects(),
            may_io: result.may_io.contains(&id),
        }))
    }

    /// Evidence includes locations; capability consumers never read this table.
    pub(crate) fn effect_evidence(
        &self,
        unit: UnitId,
        id: FunctionId,
    ) -> Option<Arc<EffectSummary<EffectWitness>>> {
        let frozen = self
            .completed
            .borrow()
            .get(&unit)?
            .facts
            .shared(&id)?
            .clone();
        if let Some(tracking) = self.tracking.borrow().upgrade() {
            let evidence = tracking
                .borrow()
                .effect_evidence(unit, id)
                .expect("completed effect evidence query");
            return Some(evidence.summary);
        }
        Some(frozen)
    }

    fn source_effects(
        &self,
        consumer: UnitId,
        unit: UnitId,
        body: BodyId,
        id: FunctionId,
    ) -> Option<(Arc<EffectSummary<EffectWitness>>, bool, bool)> {
        let effects = self.units.ready(&unit)?;
        if consumer != unit && self.tracking.borrow().upgrade().is_some() {
            self.source_reads
                .borrow_mut()
                .entry(consumer)
                .or_default()
                .insert(unit);
        }
        Some((
            self.effect_evidence(unit, id)?,
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
    pub(crate) fn external_waiting(
        &self,
        consumer: UnitId,
        target: &FunctionId,
        imports: &HashMap<String, String>,
    ) -> RuntimeEffects {
        self.lookup_external_capabilities(consumer, target, imports)
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
        // Module-qualified free calls use StaticCall's owner slot. Resolve
        // the full module path before interpreting that slot as a class.
        if let (Some(namespace), Some(owner)) = (&namespace, &owner) {
            let path = QualifiedName::join(namespace, owner);
            if let Some(unit) = self.resolve_module(consumer, &path) {
                return Some((
                    unit,
                    FunctionId::synthetic_free(target.name(), target.synthetic_kind()),
                ));
            }
        }
        let (path, id) = if let Some(namespace) = namespace.as_deref() {
            (
                imports
                    .get(namespace)
                    .map(String::as_str)
                    .unwrap_or(namespace),
                match owner {
                    Some(owner) => FunctionId::synthetic_method(
                        TypeId::local(owner),
                        target.name(),
                        target.synthetic_kind(),
                    ),
                    None => FunctionId::synthetic_free(target.name(), target.synthetic_kind()),
                },
            )
        } else if let Some(owner) = owner.as_deref() {
            let path = imports.get(owner).map(String::as_str).unwrap_or(owner);
            if self.resolve_module(consumer, path).is_some() {
                (
                    path,
                    FunctionId::synthetic_free(target.name(), target.synthetic_kind()),
                )
            } else if let Some((module, item)) = QualifiedName::new(path).split_last() {
                (
                    module,
                    FunctionId::synthetic_method(
                        TypeId::local(item),
                        target.name(),
                        target.synthetic_kind(),
                    ),
                )
            } else {
                return None;
            }
        } else if let Some(path) = imports.get(target.name().as_ref()) {
            let (module, item) = QualifiedName::new(path).split_last()?;
            (module, FunctionId::free(item))
        } else {
            return None;
        };
        let unit = self.resolve_module(consumer, path)?;
        Some((unit, id))
    }

    /// Resolve a declared callable before reading its asyncness or evidence.
    /// Source dependencies also invalidate asyncness and witness-only edits.
    /// Arity mask of the function values `target`, a callable of an earlier
    /// checked unit, can call indirectly before returning.
    pub(crate) fn indirect_reach(
        &self,
        consumer: UnitId,
        target: &FunctionId,
        imports: &HashMap<String, String>,
    ) -> u64 {
        let Some((unit, id)) = self.external_target(consumer, target, imports) else {
            return 0;
        };
        self.indirect_lock
            .borrow()
            .reach
            .get(&unit)
            .and_then(|reach| reach.get(&id))
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn external_lock_callable(
        &self,
        consumer: UnitId,
        target: &FunctionId,
        imports: &HashMap<String, String>,
    ) -> Option<(UnitId, FunctionId, bool)> {
        let (unit, id) = self.external_target(consumer, target, imports)?;
        if consumer != unit && self.tracking.borrow().upgrade().is_some() {
            self.source_reads
                .borrow_mut()
                .entry(consumer)
                .or_default()
                .insert(unit);
        }
        let is_async = self
            .units
            .ready(&unit)
            .and_then(|effects| effects.callables.get(&id).copied())
            .unwrap_or(false);
        Some((unit, id, is_async))
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
                may_io: intrinsic_may_io(target),
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

/// Panic classification only: a known runtime mask is not evidence of IO absence.
pub(crate) fn intrinsic_effects(target: &FunctionId) -> Option<RuntimeEffects> {
    crate::semantic::intrinsics::builtin_target_effects(target)
        .or_else(|| crate::semantic::intrinsics::builtin_static_effects(target, &HashMap::new()))
        .map(|effects| effects.intersection(PANIC))
}
/// Preserve the IO-free leaf set independently of the panic catalog. Static
/// builtins include filesystem/network operations and callback dispatch; keep
/// them IO-capable unless a separate IO proof is available.
fn intrinsic_may_io(target: &FunctionId) -> bool {
    crate::semantic::intrinsics::builtin_target_effects(target).is_none()
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
pub(crate) fn solve_unit<N: std::fmt::Display>(
    program: &Program,
    graph: &CallGraph,
    types: &HashMap<ExprId, Type<N>>,
    origins: Option<(&super::ids::BodyIndex, &EffectQueries)>,
    callables: &HashMap<FunctionId, bool>,
    direct: &HashMap<FunctionId, DirectLockEffects>,
    external: impl Fn(&FunctionId) -> RuntimeEffects,
) -> UnitEffects {
    crate::query_stats::add(crate::query_stats::Counter::NonpreemptibleHelpers, 1);
    if let Some((index, queries)) = origins
        && let Some(unit) = queries.active_unit.get()
        && let Some(result) = solve_unit_delta(
            program,
            graph,
            types,
            DeltaOrigin {
                index,
                queries,
                unit,
            },
            callables,
            direct,
            &external,
        )
    {
        return result;
    }
    if let Some((_, queries)) = origins
        && let Some(unit) = queries.active_unit.get()
    {
        queries.full_units.borrow_mut().insert(unit);
    }
    let UnitInventory {
        contract_spans,
        pending,
        helpers,
        foreign,
        value_constructors,
        reused,
        ambiguous,
    } = UnitInventory::collect(program, callables);
    let mut solve = ColdUnitSolve::new(origins, ambiguous, &pending);
    let visits = solve.seed_declarations(value_constructors, foreign)
        + solve.scan_bodies(pending, types, &contract_spans, &reused);
    solve.inherit_copy_loops();
    crate::query_stats::add(crate::query_stats::Counter::EffectInventory, visits);
    solve.seed_helper_preemption(graph, &helpers);
    solve.seed_direct_locks(direct);
    solve.set_transmit_masks(graph, callables, &helpers);
    let mut graphs = solve.connect_copies(graph);
    solve.classify_external_targets(
        graph,
        callables,
        &import_aliases(program),
        &external,
        &mut graphs.io_callers,
    );
    solve.propagate_io(&graphs.io_graph, &graphs.io_callers);
    crate::query_stats::add(crate::query_stats::Counter::EffectEdges, solve.edge_visits);
    crate::query_stats::add(crate::query_stats::Counter::EffectSolve, 1);
    let facts = solve.solve_facts(&graphs.lock_graph);
    solve.finish(facts, helpers, callables)
}

/// Import alias table that resolves a unit's external call targets.
fn import_aliases(program: &Program) -> HashMap<String, String> {
    program
        .imports
        .iter()
        .map(|import| {
            (
                import
                    .alias
                    .clone()
                    .unwrap_or_else(|| QualifiedName::new(&import.path).last_segment().to_owned()),
                import.path.clone(),
            )
        })
        .collect()
}

/// Call sites carrying only statically resolved targets.
fn target_sites(
    targets: std::collections::BTreeSet<FunctionId>,
) -> crate::semantic::call_graph::CallSites {
    crate::semantic::call_graph::CallSites {
        targets,
        ..Default::default()
    }
}

/// The precise witness of a body's first lock acquisition.
fn lock_acquisition_witness(
    owner: FunctionId,
    span: crate::diagnostics::Span,
    target: Option<&LockTarget>,
) -> EffectWitness {
    EffectWitness::Lock(LockEffectWitness {
        owner,
        cause: LockEffectCause {
            is_conservative: false,
            span,
            operation: "lock".into(),
            kind: LockEffectKind::Suspend,
            lock: target.cloned().map(Box::new),
        },
    })
}

/// Panic bits a scanned body contributes on its own, with their witness.
fn direct_panic(
    owner: FunctionId,
    direct: &DirectBodyEffects,
) -> Option<(RuntimeEffects, Option<EffectWitness>)> {
    let (bits, span) = if direct.panics {
        (PANIC, direct.panic_span)
    } else if direct.checked_panics {
        (CHECKED_PANIC, direct.checked_span)
    } else {
        return None;
    };
    let witness = span.map(|span| EffectWitness::Panic {
        owner,
        source: (span.file_id.0, span.start, span.end),
    });
    Some((bits, witness))
}

/// The checker's direct lock causes. Conservative causes carry no witness.
fn lock_cause_witness(owner: FunctionId, cause: &LockEffectCause) -> Option<EffectWitness> {
    (!cause.is_conservative).then(|| {
        EffectWitness::Lock(LockEffectWitness {
            owner,
            cause: cause.clone(),
        })
    })
}

/// Declarations of one unit, collected by a single pass over its items.
struct UnitInventory<'a> {
    contract_spans: HashMap<BodyId, ContractRuntimeEffects>,
    pending: Vec<(FunctionId, BodyId, &'a Block)>,
    helpers: HashMap<FunctionId, crate::diagnostics::Span>,
    foreign: Vec<(FunctionId, crate::diagnostics::Span, bool, bool, bool)>,
    value_constructors: Vec<FunctionId>,
    /// Injected default copies the checker skipped: they have no typed body in
    /// this unit and reuse their declaring interface's check.
    reused: HashSet<BodyId>,
    ambiguous: bool,
}

impl<'a> UnitInventory<'a> {
    fn collect(program: &'a Program, callables: &HashMap<FunctionId, bool>) -> Self {
        let mut inventory = Self {
            contract_spans: HashMap::new(),
            pending: Vec::new(),
            helpers: HashMap::new(),
            foreign: Vec::new(),
            value_constructors: Vec::new(),
            reused: HashSet::new(),
            ambiguous: false,
        };
        for item in &program.items {
            match item {
                Item::Function(f) => {
                    let id = FunctionId::free(&f.name);
                    if let Some(contract) = contract_runtime_effects(f) {
                        inventory.contract_spans.insert(f.body.id, contract);
                    }
                    if let Some(bridge) = &f.rust_bridge {
                        inventory.foreign.push((
                            FunctionId::free_from_source_name(&f.name),
                            f.span,
                            bridge.assumes_no_block(),
                            bridge.is_async,
                            bridge.assumes_no_io(),
                        ));
                        continue;
                    }
                    inventory.pending.push((id, f.body.id, &f.body));
                    if !f.is_async() {
                        inventory.helpers.insert(id, f.span);
                    }
                }
                Item::Class(c) => {
                    let owner = TypeId::local(&c.name);
                    if c.constructors.is_empty() {
                        inventory
                            .value_constructors
                            .push(FunctionId::method(owner, "init"));
                    }
                    for m in &c.methods {
                        let id = FunctionId::method(owner, &m.name);
                        if m.is_default_injected {
                            inventory.reused.insert(m.body.id);
                        }
                        inventory.pending.push((id, m.body.id, &m.body));
                        if !m.is_async() {
                            inventory.helpers.insert(id, m.span);
                        }
                    }
                    for init in &c.constructors {
                        inventory.pending.push((
                            FunctionId::method(owner, "init"),
                            init.body.id,
                            &init.body,
                        ));
                    }
                }
                Item::Enum(e) => {
                    let owner = TypeId::local(&e.name);
                    for variant in &e.variants {
                        inventory
                            .value_constructors
                            .push(FunctionId::method(owner, &variant.name));
                    }
                }
                Item::Interface(i) => {
                    for m in &i.methods {
                        let id = FunctionId::synthetic_method(
                            TypeId::local(&i.name),
                            format!("$default${}", m.name),
                            crate::qualified_name::SyntheticKind::InterfaceDefault,
                        );
                        if let Some(body) = &m.default_body
                            && (i.type_params.is_empty() || callables.contains_key(&id))
                        {
                            // Generic default bodies are included only when the abstract
                            // checker explicitly registered their typed evidence.
                            inventory.pending.push((id, body.id, body));
                        }
                    }
                }
            }
        }
        let mut free_names = HashSet::new();
        inventory.ambiguous = program.items.iter().any(|item| match item {
            Item::Function(function) => !free_names.insert(function.name.as_str()),
            _ => false,
        });
        inventory
    }
}

/// Call graphs the cold producer solves after connecting copied bodies.
struct ColdGraphs<'g> {
    lock_graph: std::borrow::Cow<'g, CallGraph>,
    io_graph: std::borrow::Cow<'g, CallGraph>,
    /// Reverse IO edges for the non-incremental worklist.
    io_callers: HashMap<FunctionId, Vec<FunctionId>>,
}

/// State the cold producer accumulates across its stages, in inventory order.
struct ColdUnitSolve<'a> {
    origins: Option<(&'a super::ids::BodyIndex, &'a EffectQueries)>,
    index: Option<&'a super::ids::BodyIndex>,
    consumer: Option<UnitId>,
    overflow_checks: OverflowChecks,
    plan: EffectPlan,
    problem: EffectProblem<EffectWitness>,
    own: HashSet<FunctionId>,
    loops: HashSet<FunctionId>,
    preemption_loops: HashSet<FunctionId>,
    loop_spans: HashMap<FunctionId, crate::diagnostics::Span>,
    loop_body_spans: HashMap<BodyId, crate::diagnostics::Span>,
    loop_bodies: HashSet<BodyId>,
    copies: Vec<(FunctionId, BodyId)>,
    preemption_copies: Vec<(FunctionId, UnitId, FunctionId)>,
    may_io: HashSet<FunctionId>,
    edge_visits: usize,
}

impl<'a> ColdUnitSolve<'a> {
    fn new(
        origins: Option<(&'a super::ids::BodyIndex, &'a EffectQueries)>,
        ambiguous: bool,
        pending: &[(FunctionId, BodyId, &Block)],
    ) -> Self {
        let index = origins.map(|(index, _)| index);
        let consumer = index.and_then(|index| {
            pending
                .iter()
                .find_map(|(_, body, _)| index.owner(*body).map(|(unit, _)| unit))
        });
        Self {
            origins,
            index,
            consumer,
            overflow_checks: origins.map_or(true.into(), |(_, queries)| queries.overflow_checks),
            plan: EffectPlan {
                ambiguous,
                ..EffectPlan::default()
            },
            problem: EffectProblem::new()
                .external_callee(PANIC)
                .unknown_callee(PANIC)
                .missing_body(PANIC)
                .default_transmit(
                    ANY_PANIC
                        .union(LOCK_EFFECT_WAIT)
                        .union(RuntimeEffects::MAY_ALLOCATE),
                ),
            own: HashSet::new(),
            loops: HashSet::new(),
            preemption_loops: HashSet::new(),
            loop_spans: HashMap::new(),
            loop_body_spans: HashMap::new(),
            loop_bodies: HashSet::new(),
            copies: Vec::new(),
            preemption_copies: Vec::new(),
            may_io: HashSet::new(),
            edge_visits: 0,
        }
    }

    fn seed(&mut self, id: FunctionId, effects: RuntimeEffects, witness: Option<EffectWitness>) {
        self.problem = std::mem::take(&mut self.problem).seed(id, effects, witness);
    }

    fn body(&mut self, id: FunctionId) {
        self.own.insert(id);
        self.problem = std::mem::take(&mut self.problem).body(id);
    }

    /// Seeds bodiless value constructors and foreign bridges. Returns the
    /// inventory visits they cost.
    fn seed_declarations(
        &mut self,
        value_constructors: Vec<FunctionId>,
        foreign: Vec<(FunctionId, crate::diagnostics::Span, bool, bool, bool)>,
    ) -> usize {
        // Memberwise and enum initialization only store already-evaluated arguments.
        // Publish its empty summary even when no body in this unit constructs the
        // class: imported callers must not mistake an unused constructor for an
        // unavailable implementation. Allocation belongs to the construction site.
        let visits = value_constructors.len();
        for id in value_constructors {
            self.own.insert(id);
            self.seed(id, RuntimeEffects::NONE, None);
        }
        for (id, span, nonblocking, _is_async, no_io) in foreign {
            self.seed(
                id,
                PANIC.union(RuntimeEffects::MAY_ALLOCATE),
                Some(EffectWitness::External(id)),
            );
            if !nonblocking {
                self.seed(
                    id,
                    RuntimeEffects::MAY_SUSPEND,
                    Some(EffectWitness::Lock(LockEffectWitness {
                        owner: id,
                        cause: LockEffectCause {
                            is_conservative: true,
                            span,
                            operation: "extern rust".into(),
                            kind: LockEffectKind::Suspend,
                            lock: None,
                        },
                    })),
                );
            }
            if !no_io {
                self.seed(
                    id,
                    RuntimeEffects::MAY_IO,
                    Some(EffectWitness::External(id)),
                );
                self.may_io.insert(id);
            }
        }
        visits
    }

    /// Scans every pending body once, in reverse source order. Returns the
    /// number of bodies visited.
    fn scan_bodies<N: std::fmt::Display>(
        &mut self,
        mut pending: Vec<(FunctionId, BodyId, &Block)>,
        types: &HashMap<ExprId, Type<N>>,
        contract_spans: &HashMap<BodyId, ContractRuntimeEffects>,
        reused: &HashSet<BodyId>,
    ) -> usize {
        let canonical_bodies: HashSet<_> = pending.iter().map(|(_, body, _)| *body).collect();
        let mut visits = 0;
        while let Some((id, body_id, body)) = pending.pop() {
            visits += 1;
            self.body(id);
            if self.inherit_source_facts(id, body_id, &canonical_bodies, reused) {
                continue;
            }
            self.scan_direct_body(
                id,
                body_id,
                body,
                types,
                contract_spans.get(&body_id).copied(),
            );
        }
        visits
    }

    /// A copied body inherits canonical facts. Independently checked
    /// cross-unit copies must also retain destination-scope hazards. Returns
    /// whether the copy is complete without a destination body scan.
    fn inherit_source_facts(
        &mut self,
        id: FunctionId,
        body_id: BodyId,
        canonical_bodies: &HashSet<BodyId>,
        reused: &HashSet<BodyId>,
    ) -> bool {
        let Some((index, queries)) = self.origins else {
            return false;
        };
        let source = index.source_body(body_id);
        if source == body_id {
            return false;
        }
        let known = index.owner(source).and_then(|(unit, _)| {
            self.consumer.and_then(|consumer| {
                queries.source_effects(consumer, unit, source, source_callable(index, source))
            })
        });
        if let Some((summary, has_loop, source_io)) = &known {
            if *source_io {
                self.may_io.insert(id);
            }
            self.seed(id, summary.effects().intersection(ANY_PANIC), None);
            // Reused defaults execute the declaring body's waits too.
            self.seed(
                id,
                summary.effects().intersection(LOCK_EFFECT_WAIT),
                summary.witness(LOCK_EFFECT_WAIT).cloned(),
            );
            if *has_loop {
                self.loops.insert(id);
                self.loop_bodies.insert(body_id);
            }
        }
        if canonical_bodies.contains(&source) || reused.contains(&body_id) {
            if let Some((unit, _)) = index.owner(source) {
                let source_id = source_callable(index, source);
                self.preemption_copies.push((id, unit, source_id));
                self.plan.copies.insert(
                    id,
                    CopyContribution {
                        body: body_id,
                        source_body: source,
                        provider: unit,
                        source: source_id,
                    },
                );
                self.plan
                    .copy_targets
                    .entry((unit, source_id))
                    .or_default()
                    .insert(id);
                self.plan.bodies.insert(id, body_id);
                self.plan.roots.insert(body_id, Arc::new(vec![id]));
            }
            // Defaults are checked only under the canonical interface
            // identity, in the declaring unit. Do not repeat their
            // source walk; a copy has no typed body of its own.
            if known.is_none() && !canonical_bodies.contains(&source) {
                // Without the declaring unit's facts, assume the
                // unscanned body may panic, perform I/O and loop.
                self.seed(id, PANIC.union(LOCK_EFFECT_WAIT), None);
                self.may_io.insert(id);
                self.loops.insert(id);
            }
            self.copies.push((id, source));
            return true;
        }
        if let Some((provider, _)) = index.owner(source) {
            let source_id = source_callable(index, source);
            self.plan.inherited.insert(
                body_id,
                CopyContribution {
                    body: body_id,
                    source_body: source,
                    provider,
                    source: source_id,
                },
            );
            self.plan
                .inherited_targets
                .entry((provider, source_id))
                .or_default()
                .insert(id);
        }
        if known.is_none() {
            // Generic instantiations have no canonical checked proof.
            self.seed(id, PANIC, None);
        }
        // Cross-unit copies are checked in their destination scope.
        // Union the canonical fact with this copy's direct guards and
        // resolved edges: rebinding a free name can change both a call
        // effect and the inferred type of a subsequent addition.
        false
    }

    /// Scans one declaration body and its block lambdas, then records and
    /// seeds each scanned part.
    fn scan_direct_body<N: std::fmt::Display>(
        &mut self,
        id: FunctionId,
        body_id: BodyId,
        body: &Block,
        types: &HashMap<ExprId, Type<N>>,
        contract_span: Option<ContractRuntimeEffects>,
    ) {
        let (origins, index, overflow_checks) = (self.origins, self.index, self.overflow_checks);
        let scan = || {
            if let Some((_, queries)) = origins {
                queries.scanned.set(queries.scanned.get() + 1);
            }
            Ok(with_contract_effects(
                scan_direct_effects(id, body_id, body, types, index, overflow_checks),
                contract_span,
            ))
        };
        let queries = origins.and_then(|(_, effects)| effects.body_queries.borrow().upgrade());
        let direct_bodies = match queries {
            Some(queries) if !self.plan.ambiguous => queries
                .derived(
                    body_id,
                    super::tracked::QueryNode::DirectEffects(body_id),
                    None,
                    scan,
                )
                .expect("direct effect dependencies have completed"),
            _ => scan().expect("direct effect scan is infallible"),
        };
        self.plan.roots.insert(
            body_id,
            Arc::new(
                direct_bodies
                    .iter()
                    .map(|d| d.id)
                    .chain(graph_lambdas(body_id, body, index))
                    .collect(),
            ),
        );
        if let Some((_, queries)) = origins
            && let Some(tracking) = queries.tracking.borrow().upgrade()
            && let Some(owner) = tracking.borrow().body_source_owner(body_id)
        {
            self.plan
                .source_roots
                .entry(owner)
                .or_default()
                .insert(body_id);
        }
        for direct in direct_bodies {
            self.record_direct_part(body_id, direct);
        }
    }

    fn record_direct_part(&mut self, root: BodyId, direct: DirectBodyEffects) {
        self.plan
            .direct
            .entry(direct.id)
            .or_default()
            .insert(root, Arc::new(direct.clone()));
        self.plan.bodies.insert(direct.id, direct.body);
        if direct.preemption_loop {
            self.preemption_loops.insert(direct.id);
        }
        self.body(direct.id);
        if let Some(span) = direct.loop_span {
            self.loop_spans.insert(direct.id, span);
            self.loop_body_spans.insert(direct.body, span);
            self.loops.insert(direct.id);
            self.loop_bodies.insert(direct.body);
        }
        if direct.may_io {
            self.may_io.insert(direct.id);
        }
        if let Some(span) = direct.lock_span {
            // Acquisition parks a contended task before entering the
            // critical section. Seed it before the checker's other waits
            // so this precise acquisition witness survives same-owner ties.
            self.seed(
                direct.id,
                RuntimeEffects::MAY_SUSPEND,
                Some(lock_acquisition_witness(
                    direct.id,
                    span,
                    direct.lock_target.as_ref(),
                )),
            );
        }
        if let Some((panic, witness)) = direct_panic(direct.id, &direct) {
            self.seed(direct.id, panic, witness);
        }
    }

    /// A canonical copy loops wherever its source body loops.
    fn inherit_copy_loops(&mut self) {
        for &(id, source) in &self.copies {
            if self.loop_bodies.contains(&source) {
                self.loops.insert(id);
                if let Some(&span) = self.loop_body_spans.get(&source) {
                    self.loop_spans.insert(id, span);
                }
            }
        }
    }

    /// Seeds helpers that loop or recurse through other helpers.
    fn seed_helper_preemption(
        &mut self,
        graph: &CallGraph,
        helpers: &HashMap<FunctionId, crate::diagnostics::Span>,
    ) {
        let mut helper_graph = CallGraph::default();
        for &id in helpers.keys() {
            let targets = graph
                .get(&id)
                .map(|sites| {
                    sites
                        .targets
                        .iter()
                        .filter(|id| {
                            self.edge_visits += 1;
                            helpers.contains_key(id)
                        })
                        .copied()
                        .collect()
                })
                .unwrap_or_default();
            helper_graph.merge(id, target_sites(targets));
        }
        let cycles = crate::semantic::effects::cycle_members(&helper_graph, std::iter::empty());
        for &id in helpers.keys() {
            let reason = if self.loops.contains(&id) {
                Some(NonpreemptibleReason::Loop)
            } else if cycles.contains(&id) {
                Some(NonpreemptibleReason::Recursion)
            } else {
                None
            };
            if let Some(reason) = reason {
                let span = self.loop_spans.get(&id).unwrap_or(&helpers[&id]);
                let witness = EffectWitness::Helper {
                    reason,
                    owner: id,
                    source: (span.file_id.0, span.start, span.end),
                };
                self.seed(id, NO_PREEMPT, Some(witness));
            }
        }
    }

    fn seed_direct_locks(&mut self, direct: &HashMap<FunctionId, DirectLockEffects>) {
        for (&id, causes) in direct {
            for cause in &causes.causes {
                // Each wait bit retains its first deterministic source witness.
                self.seed(id, cause.kind.effects(), lock_cause_witness(id, cause));
            }
        }
    }

    fn set_transmit_masks(
        &mut self,
        graph: &CallGraph,
        callables: &HashMap<FunctionId, bool>,
        helpers: &HashMap<FunctionId, crate::diagnostics::Span>,
    ) {
        for id in graph.ids().chain(callables.keys()).chain(helpers.keys()) {
            let mut mask = ANY_PANIC
                .union(LOCK_EFFECT_WAIT)
                .union(RuntimeEffects::MAY_ALLOCATE);
            if callables.get(id) == Some(&true) {
                mask = ANY_PANIC.union(RuntimeEffects::MAY_ALLOCATE);
            } else if helpers.contains_key(id) {
                mask = mask.union(NO_PREEMPT);
            }
            self.problem = std::mem::take(&mut self.problem).transmit(*id, mask);
        }
    }

    fn incremental(&self) -> bool {
        self.origins.is_some() && self.consumer.is_some()
    }

    /// Connects each canonical copy to its source callable.
    fn connect_copies<'g>(&mut self, graph: &'g CallGraph) -> ColdGraphs<'g> {
        let mut graphs = ColdGraphs {
            lock_graph: std::borrow::Cow::Borrowed(graph),
            io_graph: std::borrow::Cow::Borrowed(graph),
            io_callers: HashMap::new(),
        };
        let Some(index) = self.index else {
            return graphs;
        };
        let incremental = self.incremental();
        for (id, source) in std::mem::take(&mut self.copies) {
            if index.owner(source).map(|(unit, _)| unit) == self.consumer {
                self.edge_visits += 1;
                graphs.lock_graph.to_mut().merge(
                    id,
                    target_sites(std::iter::once(source_callable(index, source)).collect()),
                );
            }
            let source = source_callable(index, source);
            if incremental {
                graphs
                    .io_graph
                    .to_mut()
                    .merge(id, target_sites(std::iter::once(source).collect()));
            } else {
                graphs.io_callers.entry(source).or_default().push(id);
            }
            self.edge_visits += 1;
        }
        graphs
    }

    /// Classifies every call target outside this unit's bodies once.
    fn classify_external_targets(
        &mut self,
        graph: &CallGraph,
        callables: &HashMap<FunctionId, bool>,
        imports: &HashMap<String, String>,
        external: &impl Fn(&FunctionId) -> RuntimeEffects,
        io_callers: &mut HashMap<FunctionId, Vec<FunctionId>>,
    ) {
        let incremental = self.incremental();
        let resolved = self
            .origins
            .zip(self.consumer)
            .map(|((_, queries), unit)| (queries, unit));
        let mut classified = HashSet::new();
        for (&caller, sites) in graph.iter() {
            if sites.has_unknown {
                self.may_io.insert(caller);
            }
            for target in &sites.targets {
                self.edge_visits += 1;
                if !incremental {
                    io_callers.entry(*target).or_default().push(caller);
                }
                if self.own.contains(target) || !classified.insert(*target) {
                    continue;
                }
                if complete_dispatch_target(graph, callables, target) {
                    self.body(*target);
                    continue;
                }
                if let Some((queries, unit)) = resolved
                    && let Some(origin) = queries.external_target(unit, target, imports)
                {
                    self.plan
                        .external
                        .entry(origin)
                        .or_default()
                        .insert(*target);
                    // Qualified enum constructors can resolve to this unit.
                    // Their conservative external facts do not make the unit
                    // inventory depend on itself.
                    if origin.0 != unit {
                        self.plan.providers.insert(origin.0);
                    }
                }
                // Every unproved external call is conservatively IO-capable.
                // Known runtime/stat/math intrinsics are the only pure leaves;
                // filesystem, networking, printing and foreign calls fail closed.
                let external_io = match resolved {
                    Some((queries, unit)) => {
                        queries.external_capabilities(unit, target, imports).may_io
                    }
                    None => intrinsic_may_io(target),
                };
                if external_io {
                    self.may_io.insert(*target);
                }
                if let Some((queries, unit)) = resolved
                    && let Some((declaring, id, is_async)) =
                        queries.external_lock_callable(unit, target, imports)
                    && !is_async
                {
                    let evidence = queries.effect_evidence(declaring, id);
                    let waiting = evidence.as_ref().map_or(LOCK_EFFECT_WAIT, |summary| {
                        summary.effects().intersection(LOCK_EFFECT_WAIT)
                    });
                    let witness = evidence
                        .as_ref()
                        .and_then(|summary| summary.witness(LOCK_EFFECT_WAIT))
                        .cloned();
                    self.seed(*target, waiting, witness);
                }
                self.seed(
                    *target,
                    external(target).intersection(ANY_PANIC),
                    Some(EffectWitness::External(*target)),
                );
            }
        }
    }

    /// The IO lattice has one bit: each vertex enters the queue once and each
    /// existing call edge is visited once, including mutually recursive bodies.
    fn propagate_io(
        &mut self,
        io_graph: &CallGraph,
        io_callers: &HashMap<FunctionId, Vec<FunctionId>>,
    ) {
        if let (Some((_, queries)), Some(unit)) = (self.origins, self.consumer) {
            // The IO lattice uses a private carrier bit; it is never exposed as an
            // ABI capability. Reuse the same masked fixed-point implementation.
            let io_bit = RuntimeEffects::MAY_ALLOCATE;
            let mut io_problem = EffectProblem::<u8>::new()
                .external_callee(RuntimeEffects::NONE)
                .unknown_callee(RuntimeEffects::NONE)
                .missing_body(RuntimeEffects::NONE)
                .default_transmit(io_bit);
            for &id in &self.own {
                io_problem = io_problem.body(id);
            }
            for &id in &self.may_io {
                io_problem = io_problem.seed(id, io_bit, None);
            }
            let mut previous = queries.io_solutions.borrow_mut();
            crate::query_stats::add(crate::query_stats::Counter::EffectIoSolve, 1);
            let solution =
                io_problem.solve_incremental(io_graph, previous.get(&unit).map(Arc::as_ref));
            queries
                .compared
                .set(queries.compared.get() + solution.compared);
            queries
                .recomputed
                .set(queries.recomputed.get() + solution.recomputed);
            self.may_io = solution
                .facts
                .iter()
                .filter(|(_, fact)| fact.intersects(io_bit))
                .map(|(&id, _)| id)
                .collect();
            previous.insert(unit, Arc::new(solution));
        } else {
            let mut pending_io: VecDeque<_> = self.may_io.iter().copied().collect();
            while let Some(callee) = pending_io.pop_front() {
                for &caller in io_callers.get(&callee).into_iter().flatten() {
                    self.edge_visits += 1;
                    if self.may_io.insert(caller) {
                        pending_io.push_back(caller);
                    }
                }
            }
        }
    }

    /// Solves the helper/lock/panic fixpoint, retaining it for later deltas.
    fn solve_facts(&mut self, lock_graph: &CallGraph) -> EffectFacts<EffectWitness> {
        let problem = std::mem::take(&mut self.problem);
        match (self.origins, self.consumer) {
            (Some((_, queries)), Some(unit)) => {
                let mut solutions = queries.solutions.borrow_mut();
                let solution = problem.solve_incremental_by(
                    lock_graph,
                    solutions.get(&unit).map(Arc::as_ref),
                    same_seed,
                );
                queries
                    .compared
                    .set(queries.compared.get() + solution.compared);
                queries
                    .recomputed
                    .set(queries.recomputed.get() + solution.recomputed);
                let facts = solution.facts.clone();
                solutions.insert(unit, Arc::new(solution));
                facts
            }
            _ => {
                // A unit without bodies (for example only value constructors) has
                // no retained fixpoint; drop an earlier one so it cannot override
                // these facts when the unit completes.
                if let Some((_, queries)) = self.origins
                    && let Some(unit) = queries.active_unit.get()
                {
                    queries.solutions.borrow_mut().remove(&unit);
                    queries.io_solutions.borrow_mut().remove(&unit);
                }
                problem.solve(lock_graph)
            }
        }
    }

    fn finish(
        mut self,
        facts: EffectFacts<EffectWitness>,
        helpers: HashMap<FunctionId, crate::diagnostics::Span>,
        callables: &HashMap<FunctionId, bool>,
    ) -> UnitEffects {
        for &(_, provider, source) in &self.preemption_copies {
            self.plan
                .copy_sources
                .entry(provider)
                .or_default()
                .insert(source);
        }
        self.plan.helpers = helpers.iter().map(|(&id, &span)| (id, span)).collect();
        self.plan.loops = self.loop_spans.into_iter().collect();
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
            loop_bodies: self.loop_bodies.into_iter().collect(),
            callables: callables.iter().map(|(&id, &a)| (id, a)).collect(),
            may_io: self.may_io.into_iter().collect(),
            preemption_bodies: self.own.into_iter().collect(),
            preemption_loops: self.preemption_loops.into_iter().collect(),
            preemption_copies: self.preemption_copies.into_iter().collect(),
            plan: self.plan,
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
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
    #[serde(default)]
    lock_target: Option<LockTarget>,
    loop_span: Option<crate::diagnostics::Span>,
    #[serde(default)]
    preemption_loop: bool,
    may_io: bool,
}

// Graph-only expression lambdas have no block contribution but still own an
// equation, including when they are not called directly from their enclosing body.
fn graph_lambdas(
    root: BodyId,
    body: &Block,
    index: Option<&super::ids::BodyIndex>,
) -> Vec<FunctionId> {
    let mut result = Vec::new();
    let mut pending = vec![(root, AstEvent::Block(body))];
    while let Some((parent, body)) = pending.pop() {
        let mut walk = AstWalk::new(body);
        while let Some(event) = walk.next() {
            if let AstEvent::Lambda(lambda) = event {
                if let Some(id) = index.and_then(|i| i.lambda_in(parent, lambda.id)) {
                    result.push(FunctionId::lambda(id));
                    match &lambda.body {
                        LambdaBody::Block(block) => pending.push((id, AstEvent::Block(block))),
                        LambdaBody::Expr(expr) => pending.push((id, AstEvent::Expr(expr))),
                    }
                }
                walk.skip_children();
            }
        }
    }
    result
}

fn scan_direct_effects<N: std::fmt::Display>(
    id: FunctionId,
    body_id: BodyId,
    body: &Block,
    types: &HashMap<ExprId, Type<N>>,
    index: Option<&super::ids::BodyIndex>,
    overflow_checks: OverflowChecks,
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
        let mut preemption_loop = false;
        let mut may_io = false;
        let mut lock_span = None;
        let mut lock_target = None;
        let mut defer_depth = 0;
        // Loop invariants and variants are verifier-only: no backend evaluates
        // them, so their operations are not runtime effects of the body.
        let mut ghosts = HashSet::new();
        let mut walk = AstWalk::new(AstEvent::Block(body));
        while let Some(event) = walk.next() {
            visits += 1;
            match event {
                AstEvent::Expr(expr) if ghosts.contains(&expr.id()) => walk.skip_children(),
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
                    preemption_loop |= matches!(stmt, Stmt::While(_) | Stmt::For(_));
                    match stmt {
                        Stmt::While(s) => {
                            ghosts.extend(s.invariants.iter().chain(&s.variant).map(Expr::id))
                        }
                        Stmt::For(s) => {
                            ghosts.extend(s.invariants.iter().chain(&s.variant).map(Expr::id))
                        }
                        _ => {}
                    }
                    hazards.visit_stmt(stmt);
                    match stmt {
                        Stmt::Lock(lock) if lock_span.is_none() => {
                            // Keep an AST-native span: coordinate-only edits
                            // remap cached evidence by exact syntax spans.
                            lock_span = Some(lock.span);
                            lock_target = Some(LockTarget {
                                mode: lock.mode.keyword().into(),
                                target: lock.target_span,
                                r#type: types
                                    .get(&lock.target.id())
                                    .map(|ty| ty.map_names(ToString::to_string)),
                            });
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
                    if matches!(expr, Expr::Call(c) if matches!(c.callee.as_str(), "forall" | "exists"))
                    {
                        preemption_loop = true;
                        if defer_depth == 0 {
                            loop_span.get_or_insert(expr.span());
                        }
                    }
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
            lock_target,
            loop_span,
            preemption_loop,
            may_io,
        });
    }
    crate::query_stats::add(crate::query_stats::Counter::EffectInventory, visits);
    result
}

fn same_seed(a: &EffectSummary<EffectWitness>, b: &EffectSummary<EffectWitness>) -> bool {
    a.effects() == b.effects()
        && crate::semantic::effects::witnessed_effects().all(|bit| {
            match (a.witness(bit), b.witness(bit)) {
                (Some(EffectWitness::Lock(a)), Some(EffectWitness::Lock(b))) => {
                    a.owner == b.owner
                        && a.cause.span == b.cause.span
                        && a.cause.kind == b.cause.kind
                        && a.cause.operation == b.cause.operation
                        && a.cause.is_conservative == b.cause.is_conservative
                        && match (&a.cause.lock, &b.cause.lock) {
                            (Some(a), Some(b)) => {
                                a.mode == b.mode && a.target == b.target && a.r#type == b.r#type
                            }
                            (None, None) => true,
                            _ => false,
                        }
                }
                (a, b) => a == b,
            }
        })
}

/// A checked dispatch union has no AST body; its edges are its definition.
/// Empty or incomplete slots still require conservative external evidence.
fn complete_dispatch_target(
    graph: &CallGraph,
    callables: &HashMap<FunctionId, bool>,
    target: &FunctionId,
) -> bool {
    callables.contains_key(target)
        && graph
            .get(target)
            .is_some_and(|sites| !sites.has_unknown && !sites.targets.is_empty())
}

fn source_callable(index: &super::ids::BodyIndex, body: BodyId) -> FunctionId {
    use super::ids::BodyOwner;
    match index.owner(body).map(|(_, owner)| owner) {
        Some(BodyOwner::Function(id)) => match id.owner() {
            Some(owner) => {
                FunctionId::synthetic_method(TypeId::local(owner), id.name(), id.synthetic_kind())
            }
            None => FunctionId::synthetic_free(id.name(), id.synthetic_kind()),
        },
        Some(BodyOwner::InterfaceDefault(id)) => FunctionId::synthetic_method(
            TypeId::local(id.owner().expect("interface owner")),
            format!("$default${}", id.name()),
            crate::qualified_name::SyntheticKind::InterfaceDefault,
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
    /// Overflow fault paths disabled by the resolved policy.
    checked_panics: bool,
    checked_span: Option<crate::diagnostics::Span>,
    panic_span: Option<crate::diagnostics::Span>,
    expr_types: &'a HashMap<ExprId, Type<N>>,
    overflow_checks: OverflowChecks,
}

impl<N> HazardVisitor<'_, N> {
    fn mark_direct(&mut self, span: crate::diagnostics::Span) {
        self.panics = true;
        earliest(&mut self.panic_span, span);
    }
    /// Checked arithmetic panics only with overflow checks. Without them it
    /// still feeds the profile-independent [`CHECKED_PANIC`] summary.
    fn mark_checked(&mut self, span: crate::diagnostics::Span, enabled: bool) {
        if enabled {
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
                let concat = expr.op == BinOp::Add
                    && !matches!(
                        ty,
                        Some(
                            Type::I8
                                | Type::I16
                                | Type::I32
                                | Type::U16
                                | Type::U32
                                | Type::U64
                                | Type::Char
                                | Type::U8
                                | Type::I64
                                | Type::F64
                        )
                    );
                // Only integer division keeps a zero/overflow guard, and a
                // literal divisor other than 0 and -1 can trip neither.
                let guarded_division = matches!(expr.op, BinOp::Div | BinOp::Rem)
                    && !matches!(ty, Some(Type::F64))
                    && !matches!(expr.rhs, Expr::Integer(divisor, ..) if divisor != 0 && divisor != -1);
                // `f64 **` lowers to a plain float power with no guard. Integer
                // `**` guards a runtime exponent's sign; a literal exponent
                // unrolls into a multiplication chain, which only debug builds
                // check, and `0`/`1` multiply nothing (see `emit_pow`).
                let float_pow = matches!(ty, Some(Type::F64));
                let pow_literal = match expr.rhs {
                    Expr::Integer(exponent, ..) if exponent >= 0 => Some(exponent),
                    _ => None,
                };
                let integer = ty.and_then(Type::integer_layout);
                let guarded_pow = expr.op == BinOp::Pow
                    && !float_pow
                    && integer.is_none_or(|layout| layout.signed)
                    && pow_literal.is_none();
                // Debug builds check integer `+ - *` for overflow and a shift
                // amount outside `0..64` unless it is an in-range literal.
                let checked = matches!(expr.op, BinOp::Add | BinOp::Sub | BinOp::Mul)
                    && !matches!(ty, Some(Type::F64))
                    || matches!(expr.op, BinOp::Shl | BinOp::Shr)
                        && !matches!(expr.rhs, Expr::Integer(amount, ..) if (0..i128::from(integer.map_or(64, |layout| layout.bits))).contains(&amount))
                    || expr.op == BinOp::Pow
                        && !float_pow
                        && (pow_literal.is_some_and(|exponent| exponent >= 2)
                            || integer.is_some_and(|layout| !layout.signed)
                                && pow_literal.is_none());
                if concat || guarded_division || guarded_pow {
                    self.mark_direct(expression.span());
                } else if checked {
                    self.mark_checked(
                        expression.span(),
                        if matches!(expr.op, BinOp::Shl | BinOp::Shr) {
                            self.overflow_checks.other
                        } else {
                            integer.map_or(self.overflow_checks.i64_arithmetic, |layout| {
                                self.overflow_checks.arithmetic(layout)
                            })
                        },
                    );
                }
            }
            // Debug builds check integer negation of `i64::MIN`; a negated
            // literal is at least `-i64::MAX` and cannot overflow.
            Expr::Unary(unary)
                if unary.op == UnaryOp::Neg
                    && !matches!(unary.expr, Expr::Integer(..))
                    && !matches!(self.expr_types.get(&unary.id), Some(Type::F64)) =>
            {
                let enabled = self
                    .expr_types
                    .get(&unary.id)
                    .and_then(Type::integer_layout)
                    .map_or(self.overflow_checks.i64_arithmetic, |layout| {
                        self.overflow_checks.arithmetic(layout)
                    });
                self.mark_checked(expression.span(), enabled);
            }
            // Strict await can turn cancellation into a language panic.
            // TaskResult awaits are intentionally not distinguished here:
            // retaining a check is conservative.
            Expr::Await(_) => self.mark_direct(expression.span()),
            Expr::Select(_) => self.mark_direct(expression.span()),
            // Object/interface display may invoke user `to_string` code.
            Expr::Print(..) => self.mark_direct(expression.span()),
            // Bounds guard.
            Expr::Index(..) => self.mark_direct(expression.span()),
            // Every call form is an edge in the shared graph, classified by
            // `classify_edge` rather than here. A field access lowers to a
            // plain load of a checked layout offset with no language-level
            // guard; its receiver is visited on its own.
            Expr::FieldAccess(..)
            | Expr::Call(_)
            | Expr::MethodCall(_)
            | Expr::StaticCall(_)
            | Expr::New(_)
            | Expr::Integer(..)
            | Expr::Char(..)
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
    Io,
    Suspend,
    Block,
    /// A separately compiled/imported synchronous implementation whose body
    /// is not available to this checker. Treat it conservatively as capable of
    /// either kind of wait instead of silently weakening E2604.
    SuspendOrBlock,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct LockEffectCause {
    /// A fail-closed bound without a source-level waiting witness.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) is_conservative: bool,
    pub(crate) span: Span,
    pub(crate) operation: std::borrow::Cow<'static, str>,
    pub(crate) kind: LockEffectKind,
    /// The value a `lock` statement acquires, when the wait is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) lock: Option<Box<LockTarget>>,
}

/// Bounded direct waiting evidence: retain the first cause for each newly
/// discovered wait bit, in source traversal order. At most two causes survive.
/// Keeping the first entry preserves the existing E2604 witness preference.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct DirectLockEffects {
    causes: Vec<LockEffectCause>,
}

impl DirectLockEffects {
    pub(crate) fn insert(&mut self, cause: LockEffectCause) {
        let known = self
            .causes
            .iter()
            .fold(RuntimeEffects::NONE, |bits, c| bits.union(c.kind.effects()));
        if !known.contains(cause.kind.effects()) {
            self.causes.push(cause);
        }
    }

    pub(crate) fn merge(&mut self, other: Self) {
        for cause in other.causes {
            self.insert(cause);
        }
    }

    pub(crate) fn first(&self) -> Option<&LockEffectCause> {
        self.causes.first()
    }
}

/// The locked operand of a `lock` statement: its source spelling of the mode
/// (`lock`, `lock read`, `lock write`), operand span and checked type, so
/// effect evidence can name what is acquired.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct LockTarget {
    pub(crate) mode: std::borrow::Cow<'static, str>,
    pub(crate) target: Span,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) r#type: Option<Type>,
}

impl LockEffectKind {
    /// The lattice bits this wait contributes to the shared effect fixpoint.
    ///
    pub(crate) fn effects(self) -> RuntimeEffects {
        match self {
            Self::Io => RuntimeEffects::MAY_IO,
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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

/// Session aggregate of function values and indirect lock-held calls, keyed
/// by arity bit (`1 << min(arity, 63)`). Units add their facts
/// when they finish, so each cross-unit pair is checked by the later unit.
#[derive(Default)]
pub(crate) struct IndirectLockSession {
    /// Per unit and callable: the arity bits of the indirect calls it can
    /// make synchronously, through any non-async call path.
    pub(crate) reach: HashMap<UnitId, HashMap<FunctionId, u64>>,
    pub(crate) values: crate::semantic::indirect_lock::ValueMasks,
    /// Arity bits of the indirect calls made while holding a lock.
    pub(crate) locks: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct LockEffectCallsite {
    pub(crate) callee: FunctionId,
    pub(crate) span: Span,
}

#[cfg(test)]
mod tests;

/// Immutable callable declarations shared by body evaluators.
#[derive(Default, Clone)]
pub(crate) struct CallableIndex {
    pub(crate) callables: HashMap<FunctionId, bool>,
    /// Canonical declaration IDs mapped to this unit's body graph IDs.
    pub(crate) local_methods: HashMap<FunctionId, FunctionId>,
    pub(crate) hierarchy: Arc<ClassHierarchy>,
    pub(crate) imports: Arc<HashMap<String, String>>,
}

impl CallableIndex {
    pub(crate) fn local_target(&self, id: FunctionId) -> FunctionId {
        self.local_methods.get(&id).copied().unwrap_or(id)
    }
}

/// Typed effect contributions and lock-held diagnostic sites for one unit/body.
#[derive(Default)]
pub(crate) struct EffectInputs {
    pub(crate) edges: HashMap<FunctionId, HashSet<FunctionId>>,
    pub(crate) direct: HashMap<FunctionId, DirectLockEffects>,
    pub(crate) direct_sites: Vec<LockEffectCause>,
    pub(crate) sites: Vec<LockEffectCallsite>,
}

impl super::retained::Retained for EffectCapabilities {
    fn heap_bytes(&self) -> usize {
        0
    }
}
fn witness_type_bytes(ty: &Type) -> usize {
    match ty {
        Type::Named(name) => name.capacity(),
        Type::Array(child) | Type::FixedArray(child, _) => {
            std::mem::size_of::<Type>() + witness_type_bytes(child)
        }
        Type::Generic(name, args) => {
            name.capacity()
                + args.capacity() * std::mem::size_of::<Type>()
                + args.iter().map(witness_type_bytes).sum::<usize>()
        }
        Type::Fn(args, result) | Type::Closure(args, result) => {
            args.capacity() * std::mem::size_of::<Type>()
                + args.iter().map(witness_type_bytes).sum::<usize>()
                + std::mem::size_of::<Type>()
                + witness_type_bytes(result)
        }
        _ => 0,
    }
}
impl super::retained::Retained for EffectEvidence {
    fn heap_bytes(&self) -> usize {
        // At most one witness per witnessed bit. Include dynamic witness storage.
        let mut slots = 0usize;
        let dynamic: usize = crate::semantic::effects::witnessed_effects()
            .inspect(|_| slots += 1)
            .filter_map(|bit| match self.summary.witness(bit) {
                Some(EffectWitness::Lock(w)) => Some(
                    w.cause.operation.len()
                        + w.cause.lock.as_ref().map_or(0, |lock| {
                            lock.mode.len() + lock.r#type.as_ref().map_or(0, witness_type_bytes)
                        }),
                ),
                _ => None,
            })
            .sum();
        dynamic + slots * (std::mem::size_of::<EffectWitness>() + 64)
    }
}
impl super::retained::Retained for CapturedEffect {
    fn heap_bytes(&self) -> usize {
        self.evidence.heap_bytes() + self.loop_bodies.capacity() * std::mem::size_of::<BodyId>()
    }
}

#[cfg(test)]
mod local_callable_identity_tests;

#[cfg(test)]
mod direct_wait_tests;

#[derive(Clone, Copy)]
struct ContractRuntimeEffects {
    span: crate::diagnostics::Span,
    loop_span: Option<crate::diagnostics::Span>,
}
fn contract_runtime_effects(
    f: &crate::parser::ast::FunctionDecl,
) -> Option<ContractRuntimeEffects> {
    let span = f.requires.first().or(f.ensures.first())?.span();
    let loop_span = f
        .requires
        .iter()
        .chain(&f.ensures)
        .flat_map(|e| AstWalk::new(AstEvent::Expr(e)))
        .find_map(|event| {
            if let AstEvent::Expr(Expr::Call(c)) = event
                && matches!(c.callee.as_str(), "forall" | "exists")
            {
                Some(c.span)
            } else {
                None
            }
        });
    Some(ContractRuntimeEffects { span, loop_span })
}

/// Include generated guards and quantified loops in the persistent direct
/// contribution, including incremental provider/caller updates.
fn with_contract_effects(
    mut direct: Vec<DirectBodyEffects>,
    contract: Option<ContractRuntimeEffects>,
) -> Vec<DirectBodyEffects> {
    if let (Some(root), Some(contract)) = (direct.first_mut(), contract) {
        root.panics = true;
        root.panic_span.get_or_insert(contract.span);
        if let Some(span) = contract.loop_span {
            root.preemption_loop = true;
            root.loop_span.get_or_insert(span);
        }
    }
    direct
}

#[cfg(test)]
#[test]
fn quantified_runtime_effect_inventory() {
    let source = "fn pre(n:i64) requires forall(0,n,|i:i64|i>=0) {} fn post(n:i64) ensures exists(0,n,|i:i64|i==0) {} fn body(n:i64)->bool {return forall(0,n,|i:i64|i>=0);} fn scalar(n:i64) requires n>=0 {}";
    let (program, errors) =
        crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap()).parse();
    assert!(errors.is_empty());
    for item in &program.items {
        let Item::Function(f) = item else { panic!() };
        let effects = with_contract_effects(
            scan_direct_effects(
                FunctionId::free(&f.name),
                f.body.id,
                &f.body,
                &HashMap::<ExprId, Type>::new(),
                None,
                true.into(),
            ),
            contract_runtime_effects(f),
        );
        let loops = f.name != "scalar";
        assert_eq!(effects[0].preemption_loop, loops, "{}", f.name);
        assert_eq!(effects[0].loop_span.is_some(), loops, "{}", f.name);
        assert_eq!(effects.len(), 1, "binders must not become callable bodies");
    }
}
