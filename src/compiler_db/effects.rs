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
        effects::{EffectFacts, EffectProblem, RuntimeEffects},
        ids::{FunctionId, TypeId},
    },
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

const PANIC: RuntimeEffects = RuntimeEffects::MAY_PANIC;

use crate::semantic::concurrency::{NonpreemptibleHelper, NonpreemptibleReason};
const NO_PREEMPT: RuntimeEffects = RuntimeEffects::NO_PREEMPT_REGION;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum EffectWitness {
    Lock(LockEffectWitness),
    Helper(NonpreemptibleReason),
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

pub(crate) struct UnitEffects {
    pub(crate) facts: EffectFacts<EffectWitness>,
    pub(crate) helpers: Arc<super::HelperSummary>,
    loop_bodies: HashSet<BodyId>,
}

pub(crate) struct EffectQueries {
    pub(crate) analysis: std::cell::RefCell<Option<HashMap<UnitId, crate::ai::CapturedUnit>>>,
    units: QueryTable<UnitId, UnitEffects>,
    names: HashMap<String, UnitId>,
    dependencies: Option<std::rc::Rc<super::dependencies::ModuleDependencies>>,
}
impl Default for EffectQueries {
    fn default() -> Self {
        Self {
            analysis: Default::default(),
            units: QueryTable::named("unit_effects"),
            names: HashMap::new(),
            dependencies: None,
        }
    }
}
impl EffectQueries {
    pub(crate) fn new(
        modules: &[crate::module::ResolvedModule],
        dependencies: std::rc::Rc<super::dependencies::ModuleDependencies>,
    ) -> Self {
        let mut result = Self {
            dependencies: Some(dependencies),
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
    pub(crate) fn complete(
        &self,
        unit: UnitId,
        compute: impl FnOnce() -> UnitEffects,
    ) -> anyhow::Result<Arc<UnitEffects>> {
        self.units.query(unit, || Ok(compute()))
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
        self.units
            .ready(&unit)
            .is_none_or(|effects| effects.facts.intersects(&id, PANIC))
    }
    pub(crate) fn has_fact(&self, unit: UnitId, id: FunctionId) -> bool {
        self.units
            .ready(&unit)
            .is_some_and(|effects| effects.facts.get(&id).is_some())
    }
    fn source_effects(&self, unit: UnitId, body: BodyId, id: FunctionId) -> Option<(bool, bool)> {
        let effects = self.units.ready(&unit)?;
        Some((
            effects.facts.get(&id)?.intersects(PANIC),
            effects.loop_bodies.contains(&body),
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
        if let Some(effect) = intrinsic_effects(target) {
            return effect;
        }
        let namespace = target.namespace();
        let owner = target.owner();
        let (path, id) = if let Some(namespace) = namespace {
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
        } else if let Some(owner) = owner {
            let path = imports.get(owner).map(String::as_str).unwrap_or(owner);
            if self.resolve_module(consumer, path).is_some() {
                (path, FunctionId::free(target.name()))
            } else if let Some((module, item)) = path.rsplit_once("::") {
                (
                    module,
                    FunctionId::method(TypeId::local(item), target.name()),
                )
            } else {
                return default_external(target);
            }
        } else if let Some(path) = imports.get(target.name()) {
            let Some((module, item)) = path.rsplit_once("::") else {
                return PANIC;
            };
            (module, FunctionId::free(item))
        } else {
            return default_external(target);
        };
        self.resolve_module(consumer, path).map_or_else(
            || default_external(target),
            |unit| {
                let Some(effects) = self.units.ready(&unit) else {
                    return PANIC;
                };
                effects.facts.get(&id).map_or_else(
                    || default_external(target),
                    |fact| fact.effects().intersection(PANIC),
                )
            },
        )
    }
}

pub(crate) fn intrinsic_effects(target: &FunctionId) -> Option<RuntimeEffects> {
    if target.owner().is_some() || target.namespace().is_some() {
        return None;
    }
    match target.name() {
        "panic" | "format" => Some(PANIC),
        "recover" | "pow" | "powf" => Some(RuntimeEffects::NONE),
        name => crate::semantic::intrinsics::builtin_call_runtime_name(name).map(|name| {
            willow_abi::runtime_symbol(name)
                .map(|symbol| symbol.effects().intersection(PANIC))
                .unwrap_or(PANIC)
        }),
    }
}
fn default_external(target: &FunctionId) -> RuntimeEffects {
    if target.owner().is_some() && target.name() == "init" {
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
    let mut problem = EffectProblem::new()
        .external_callee(PANIC)
        .unknown_callee(PANIC)
        .missing_body(PANIC)
        .default_transmit(PANIC.union(LOCK_EFFECT_WAIT));
    let mut own = HashSet::new();
    let mut loops = HashSet::new();
    let mut loop_bodies = HashSet::new();
    let mut copies = Vec::new();
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
                    origins.expect("source index").1.source_effects(
                        unit,
                        source,
                        source_callable(index, source),
                    )
                });
                if let Some((panic, has_loop)) = known {
                    if panic {
                        problem = problem.seed(id, PANIC, None);
                    }
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

        let mut hazards = HazardVisitor {
            panics: false,
            panic_span: None,
            expr_types: types,
        };
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
                        Stmt::Defer(_) => defer_depth += 1,
                        Stmt::While(_) | Stmt::For(_) if defer_depth == 0 => {
                            loops.insert(id);
                            loop_bodies.insert(body_id);
                        }
                        _ => {}
                    }
                }
                AstEvent::ExitStmt(Stmt::Defer(_)) => defer_depth -= 1,
                AstEvent::Expr(expr) => hazards.visit_expr(expr),
                _ => {}
            }
        }
        if hazards.panics {
            let witness = hazards.panic_span.map(|span| EffectWitness::Panic {
                owner: id,
                source: (span.file_id.0, span.start, span.end),
            });
            problem = problem.seed(id, PANIC, witness);
        }
    }
    for (id, source) in copies {
        if loop_bodies.contains(&source) {
            loops.insert(id);
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
            problem = problem.seed(id, NO_PREEMPT, Some(EffectWitness::Helper(reason)));
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
        let mut mask = PANIC.union(LOCK_EFFECT_WAIT);
        if callables.get(id) == Some(&true) {
            mask = PANIC;
        } else if helpers.contains_key(id) {
            mask = mask.union(NO_PREEMPT);
        }
        problem = problem.transmit(*id, mask);
    }
    let mut classified = HashSet::new();
    for (_, sites) in graph.iter() {
        for target in &sites.targets {
            edge_visits += 1;
            if !own.contains(target) && classified.insert(*target) {
                problem = problem.seed(
                    *target,
                    external(target).intersection(PANIC),
                    Some(EffectWitness::External(*target)),
                );
            }
        }
    }
    crate::query_stats::add(crate::query_stats::Counter::EffectEdges, edge_visits);
    let facts = problem.solve(graph);
    let helpers = helpers
        .into_iter()
        .filter_map(|(id, span)| match facts.get(&id)?.witness(NO_PREEMPT)? {
            EffectWitness::Helper(reason) => Some((
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
    }
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
    panic_span: Option<crate::diagnostics::Span>,
    expr_types: &'a HashMap<ExprId, Type<N>>,
}

impl<N> HazardVisitor<'_, N> {
    fn mark_direct(&mut self, span: crate::diagnostics::Span) {
        self.panics = true;
        if self.panic_span.is_none_or(|old| {
            (span.file_id.0, span.start, span.end) < (old.file_id.0, old.start, old.end)
        }) {
            self.panic_span = Some(span);
        }
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
                let concat = expr.op == BinOp::Add
                    && !matches!(self.expr_types.get(&expr.id), Some(Type::I64 | Type::F64));
                if concat || matches!(expr.op, BinOp::Div | BinOp::Rem | BinOp::Pow) {
                    self.mark_direct(expression.span());
                }
            }
            Expr::FieldAccess(..) => self.mark_direct(expression.span()),
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
            // `classify_edge` rather than here.
            Expr::Call(_)
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
/// is sound because `EffectInputs::direct` holds exactly one cause per owner, so
/// two witnesses naming the same owner carry the same cause.
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
                    panic_span: None,
                    expr_types: &HashMap::<ExprId, Type<TypeId>>::new(),
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
        for shape in ["chain", "fanout", "repeated", "cycle"] {
            let mut previous = None;
            for size in [16usize, 64, 256, 1024] {
                let mut source = String::new();
                for i in 0..size {
                    let target = match shape {
                        "chain" if i + 1 < size => format!("node_{}", i + 1),
                        "cycle" => format!("node_{}", (i + 1) % size),
                        _ => "external".to_string(),
                    };
                    source.push_str(&format!("fn node_{i}() {{ {target}(); }}\n"));
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
    pub(crate) hierarchy: ClassHierarchy,
}

/// Typed effect contributions and lock-held diagnostic sites for one unit/body.
#[derive(Default)]
pub(crate) struct EffectInputs {
    pub(crate) edges: HashMap<FunctionId, HashSet<FunctionId>>,
    pub(crate) direct: HashMap<FunctionId, LockEffectCause>,
    pub(crate) direct_sites: Vec<LockEffectCause>,
    pub(crate) sites: Vec<LockEffectCallsite>,
}
