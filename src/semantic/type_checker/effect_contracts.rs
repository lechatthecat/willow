//! Declaration contracts consume the same solved effects as lock checking.
use super::*;
use crate::compiler_db::effects::EffectWitness;
use crate::semantic::call_graph::CallGraph;
use crate::semantic::effects::{EffectFacts, RuntimeEffects};
use std::collections::VecDeque;

const CONTRACT_BITS: [RuntimeEffects; 6] = [
    RuntimeEffects::MAY_BLOCK,
    RuntimeEffects::MAY_SUSPEND,
    RuntimeEffects::MAY_IO,
    RuntimeEffects::MAY_PANIC,
    RuntimeEffects::MAY_CHECKED_PANIC,
    RuntimeEffects::MAY_ALLOCATE,
];

#[cfg(test)]
thread_local! {
    static OBLIGATION_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static CONTRACT_GRAPH_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ENUM_REPR_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ALLOCATION_SCAN_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PATH_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A shared next-hop forest per forbidden bit. Each edge is examined once;
/// rendering a diagnostic then costs only the length of its emitted chain.
fn witness_paths(
    graph: &CallGraph,
    facts: &EffectFacts<EffectWitness>,
    asynchronous: &HashMap<FunctionId, bool>,
    bit: RuntimeEffects,
) -> HashMap<FunctionId, (FunctionId, Option<Span>)> {
    let mut owners = HashMap::new();
    let mut queue = VecDeque::new();
    let mut visited = HashSet::new();
    for (&id, summary) in facts.iter() {
        #[cfg(test)]
        PATH_WORK.with(|n| n.set(n.get() + 1));
        let owner = match summary.witness(bit) {
            Some(EffectWitness::Lock(w)) => w.owner,
            Some(
                EffectWitness::External(owner)
                | EffectWitness::Panic { owner, .. }
                | EffectWitness::Allocation { owner, .. },
            ) => *owner,
            _ => continue,
        };
        owners.insert(id, owner);
        // Imported summaries can name a root inside another module. Their
        // exported callable has no local graph body, but is still a terminal
        // witness boundary for the consumer's call chain.
        if id == owner || graph.get(&id).is_none() {
            visited.insert(id);
            queue.push_back(id);
        }
    }
    let mut reverse: HashMap<FunctionId, Vec<(FunctionId, Option<Span>)>> = HashMap::new();
    for (caller, sites) in graph.iter() {
        #[cfg(test)]
        PATH_WORK.with(|n| n.set(n.get() + 1));
        for callee in &sites.targets {
            #[cfg(test)]
            PATH_WORK.with(|n| n.set(n.get() + 1));
            if (!bit.intersects(RuntimeEffects::MAY_BLOCK.union(RuntimeEffects::MAY_SUSPEND))
                || asynchronous.get(callee) != Some(&true))
                && owners.contains_key(caller)
                && owners.get(caller) == owners.get(callee)
            {
                reverse
                    .entry(*callee)
                    .or_default()
                    .push((*caller, sites.call_spans.get(callee).copied()));
            }
        }
    }
    let mut next = HashMap::new();
    while let Some(callee) = queue.pop_front() {
        if let Some(callers) = reverse.get(&callee) {
            for &(caller, span) in callers {
                #[cfg(test)]
                PATH_WORK.with(|n| n.set(n.get() + 1));
                if visited.insert(caller) {
                    next.insert(caller, (callee, span));
                    queue.push_back(caller);
                }
            }
        }
    }
    next
}

fn contract_cause(witness: &EffectWitness) -> Option<(Span, &str)> {
    match witness {
        EffectWitness::Lock(w) if !w.cause.is_conservative => {
            Some((w.cause.span, &w.cause.operation))
        }
        EffectWitness::Panic { source, .. } | EffectWitness::Allocation { source, .. } => Some((
            Span::in_file(
                crate::diagnostics::FileId(source.0),
                source.1,
                source.2,
                0,
                0,
            ),
            if matches!(witness, EffectWitness::Allocation { .. }) {
                "allocation"
            } else {
                "panic"
            },
        )),
        _ => None,
    }
}

static VOID: Type = Type::Void;

/// Every source body the contract checker scans: functions with their
/// requires/ensures, constructors, methods and interface defaults.
fn contract_body_roots(
    program: &Program,
) -> Vec<(
    FunctionId,
    crate::parser::ast::BodyId,
    &Type,
    crate::parser::iter::AstEvent<'_>,
)> {
    use crate::parser::iter::AstEvent;
    let mut pending = Vec::new();
    for item in &program.items {
        match item {
            Item::Function(f) => {
                let id = FunctionId::free(&f.name);
                pending.push((id, f.body.id, &f.return_type, AstEvent::Block(&f.body)));
                pending.extend(
                    f.requires
                        .iter()
                        .chain(&f.ensures)
                        .map(|e| (id, f.body.id, &f.return_type, AstEvent::Expr(e))),
                );
            }
            Item::Class(c) => {
                for constructor in &c.constructors {
                    pending.push((
                        FunctionId::method(TypeId::local(&c.name), "init"),
                        constructor.body.id,
                        &VOID,
                        AstEvent::Block(&constructor.body),
                    ));
                }
                // Injected interface defaults are checked under the interface
                // identity; their class copy is an unproven (has_unknown) node.
                for m in c.methods.iter().filter(|m| !m.is_default_injected) {
                    pending.push((
                        FunctionId::method(TypeId::local(&c.name), &m.name),
                        m.body.id,
                        &m.return_type,
                        AstEvent::Block(&m.body),
                    ));
                }
            }
            Item::Interface(i) => {
                for m in &i.methods {
                    if let Some(body) = &m.default_body {
                        pending.push((
                            interface_default_effect_id(&i.name, &m.name),
                            body.id,
                            &m.return_type,
                            AstEvent::Block(body),
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    pending
}

/// Bodies and contract clauses are scanned in stack order; keep the first
/// allocation in source order so the reported site does not depend on it.
fn earliest_allocation(allocations: &mut HashMap<FunctionId, Span>, owner: FunctionId, span: Span) {
    let key = |s: &Span| (s.file_id.0, s.start, s.end);
    allocations
        .entry(owner)
        .and_modify(|old| {
            if key(&span) < key(old) {
                *old = span;
            }
        })
        .or_insert(span);
}

/// Witnesses persist only byte offsets. Map them back to the AST span with
/// its line/column, built once per check and only when a diagnostic needs it.
fn source_span_index(program: &Program) -> HashMap<(u32, usize, usize), Span> {
    use crate::parser::iter::{AstEvent, AstWalk};
    let mut index = HashMap::new();
    for (_, _, _, root) in contract_body_roots(program) {
        for event in AstWalk::new(root) {
            let span = match event {
                AstEvent::Expr(expr) => expr.span(),
                AstEvent::Stmt(stmt) => stmt.span(),
                _ => continue,
            };
            index
                .entry((span.file_id.0, span.start, span.end))
                .or_insert(span);
        }
    }
    index
}

impl TypeChecker {
    /// Match the inline representations boxed by emit_to_storage_word. The
    /// destination type matters: a concrete object may be coerced to an interface.
    fn contract_storage_type_allocates(&self, ty: &Type) -> bool {
        crate::semantic::builtin_types::is_scalar_pair(ty)
            || matches!(ty, Type::Named(name) | Type::Generic(name, _)
                if self.symbols.lookup_interface(name).is_some())
    }

    fn contract_unary_storage_allocates(&self, receiver: &Expr) -> bool {
        self.expr_types
            .get(&receiver.id())
            .and_then(crate::semantic::builtin_types::resolve)
            .and_then(|receiver| receiver.args.first())
            .is_some_and(|ty| self.contract_storage_type_allocates(ty))
    }

    /// ABI rows describe the runtime entry point, not the typed storage-word
    /// conversions emitted before calling it. Preserve scalar/non-pair calls.
    fn contract_call_storage_allocates(&self, call: &MethodCallExpr) -> bool {
        use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
        use crate::semantic::intrinsics::{self, Intrinsic};
        let Some(receiver) = self.expr_types.get(&call.object.id()) else {
            return false;
        };
        let Some(storage) = builtin_types::resolve(receiver) else {
            return false;
        };
        // Map operations and constructors already carry MAY_ALLOCATE in their
        // ABI rows. Restrict resolution to these one-word writers; resolving a
        // read merely to inspect its identity would clone its result type.
        if call.args.len() != 1
            || !matches!(storage.id, B::BlockingCell | B::BlockingRwCell | B::Channel)
        {
            return false;
        }
        let Some(resolved) = intrinsics::resolve(receiver, &call.method, call.args.len()) else {
            return false;
        };
        matches!(
            resolved.intrinsic,
            Intrinsic::CellSet | Intrinsic::RwCellWrite | Intrinsic::ChannelSend
        ) && storage
            .args
            .first()
            .is_some_and(|ty| self.contract_storage_type_allocates(ty))
    }

    /// Match enum lowering without rescanning every variant at every use.
    fn contract_enum_heap(&self, name: &str, heaps: &mut HashMap<String, bool>) -> bool {
        if let Some(&heap) = heaps.get(name) {
            return heap;
        }
        let heap = self.symbols.lookup_enum(name).is_none_or(|info| {
            info.variants.iter().any(|variant| {
                #[cfg(test)]
                ENUM_REPR_WORK.with(|n| n.set(n.get() + 1));
                !variant.payload_types.is_empty()
            })
        });
        heaps.insert(name.to_owned(), heap);
        heap
    }

    fn contract_enum_allocates(&self, ty: &Type, heaps: &mut HashMap<String, bool>) -> bool {
        use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
        if builtin_types::is_scalar_pair(ty) {
            return false;
        }
        if let Some(inner) = builtin_types::unary_arg(ty, B::Option) {
            // Interfaces are inline data/vtable pairs, not nullable heap
            // pointers. Match the backend's interface-specific Option override.
            if matches!(inner, Type::Named(name) | Type::Generic(name, _)
                if self.symbols.lookup_interface(name).is_some())
            {
                return true;
            }
            // Same non-null managed-pointer niche as option_repr. Nested Options
            // have no spare null value, and scalar pairs are not heap pointers.
            let managed = !builtin_types::is_scalar_pair(inner)
                && match inner {
                    // A type parameter's representation depends on the
                    // instantiation; only known declarations take the niche.
                    Type::Named(name)
                        if self.symbols.lookup_class(name).is_some()
                            || self.symbols.lookup_enum(name).is_some()
                            || builtin_types::resolve_name(name).is_some() =>
                    {
                        self.contract_enum_heap(name, heaps)
                    }
                    Type::Named(_) => false,
                    Type::Generic(name, _) => builtin_types::resolve_name(name) != Some(B::Future),
                    Type::FixedArray(..) | Type::Array(_) | Type::String | Type::Closure(..) => {
                        true
                    }
                    _ => false,
                };
            return builtin_types::unary_arg(inner, B::Option).is_some() || !managed;
        }
        match ty {
            Type::Named(name) | Type::Generic(name, _) => self.contract_enum_heap(name, heaps),
            _ => true,
        }
    }

    /// Failure lowering either constructs None, reuses a Result, or constructs
    /// a converted Err. An Into call is a separate virtual effect obligation.
    fn contract_propagation_allocates(
        &self,
        source: &Type,
        target: &Type,
        heaps: &mut HashMap<String, bool>,
    ) -> (bool, Option<FunctionId>) {
        use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
        if builtin_types::unary_arg(source, B::Option).is_some() {
            return (self.contract_enum_allocates(target, heaps), None);
        }
        let (Some((_, source_error)), Some((_, target_error))) = (
            builtin_types::binary_args(source, B::Result),
            builtin_types::binary_args(target, B::Result),
        ) else {
            return (true, None);
        };
        // A generic user class (`Wrap<i64>`) lowers to the same Into call as
        // a named one once monomorphized; builtin generics never convert.
        let source_class = match source_error {
            Type::Named(name) => Some(name),
            Type::Generic(name, _) if self.symbols.lookup_class(name).is_some() => Some(name),
            _ => None,
        };
        if let Some(name) = source_class
            && !self.same_type_identity(source_error, target_error)
            && *target_error != Type::Void
        {
            let owner = self
                .symbols
                .lookup_class(name)
                .map_or(name.as_str(), |c| c.name.as_str());
            return (
                self.contract_enum_allocates(target, heaps),
                Some(FunctionId::method(TypeId::from_source_name(owner), "into")),
            );
        }
        // RebindResultError only reconstructs on a representation change;
        // reconstruction into a scalar pair remains allocation-free.
        (
            builtin_types::is_scalar_pair(source) && !builtin_types::is_scalar_pair(target),
            None,
        )
    }

    /// Literal construction has no ABI call edge. Scan each body once, keeping
    /// one allocation witness per callable; lambda bodies belong to their own nodes.
    fn contract_allocations(
        &self,
        program: &Program,
        graph: &mut CallGraph,
    ) -> HashMap<FunctionId, Span> {
        use crate::parser::iter::{AstEvent, AstWalk};
        let mut pending = contract_body_roots(program);
        let mut allocations = HashMap::new();
        let mut inline_ranges = HashSet::new();
        let mut heaps = HashMap::new();
        let mut conversions = HashSet::new();
        let mut conversion_hubs = HashSet::new();
        // Loop invariants/variants are proof-only; lowering drops them.
        let mut ghosts = HashSet::new();
        while let Some((owner, body_id, return_type, root)) = pending.pop() {
            let mut walk = AstWalk::new(root);
            while let Some(event) = walk.next() {
                #[cfg(test)]
                ALLOCATION_SCAN_WORK.with(|n| n.set(n.get() + 1));
                match event {
                    AstEvent::Stmt(Stmt::While(stmt)) => {
                        ghosts.extend(stmt.invariants.iter().chain(&stmt.variant).map(Expr::id))
                    }
                    AstEvent::Stmt(Stmt::For(stmt)) => {
                        ghosts.extend(stmt.invariants.iter().chain(&stmt.variant).map(Expr::id))
                    }
                    AstEvent::Expr(expr) if ghosts.contains(&expr.id()) => {
                        walk.skip_children();
                        continue;
                    }
                    _ => {}
                }
                // lower_for consumes a syntactic range's bounds directly;
                // only that range is unmaterialized, not ranges in its body.
                if let AstEvent::Stmt(Stmt::For(stmt)) = event
                    && let Expr::Range(range) = &stmt.iterable
                {
                    inline_ranges.insert(range.id);
                }
                let stored = match event {
                    // HIR lowering snapshots Map iteration with entries();
                    // the implicit managed array/tuple allocation has no AST call.
                    AstEvent::Stmt(Stmt::For(stmt))
                        if self.expr_types.get(&stmt.iterable.id()).is_some_and(|ty| {
                            crate::semantic::builtin_types::binary_args(
                                ty,
                                crate::semantic::builtin_types::BuiltinTypeId::Map,
                            )
                            .is_some()
                        }) =>
                    {
                        Some(stmt.span)
                    }
                    AstEvent::Stmt(Stmt::IndexAssign(stmt))
                        if self.contract_unary_storage_allocates(&stmt.array) =>
                    {
                        Some(stmt.span)
                    }
                    AstEvent::Stmt(Stmt::Lock(stmt))
                        if (stmt.mode != LockMode::Read
                            && self.contract_unary_storage_allocates(&stmt.target))
                            || stmt.additional.iter().any(|member| {
                                member.mode != LockMode::Read
                                    && self.contract_unary_storage_allocates(&member.target)
                            }) =>
                    {
                        Some(stmt.span)
                    }
                    _ => None,
                };
                if let Some(span) = stored {
                    earliest_allocation(&mut allocations, owner, span);
                }
                let AstEvent::Expr(expr) = event else {
                    continue;
                };
                let enum_type = self.expr_types.get(&expr.id());
                let allocates = match expr {
                    Expr::String(..)
                    | Expr::ArrayLiteral(..)
                    | Expr::ObjectLiteral(_)
                    | Expr::New(_) => true,
                    Expr::TryPropagate(inner, span, _) => {
                        let (allocates, conversion) = self.expr_types.get(&inner.id())
                            .map(|source| self.contract_propagation_allocates(source, return_type, &mut heaps))
                            .unwrap_or((true, None));
                        if let Some(conversion) = conversion
                            && conversions.insert((owner, conversion))
                        {
                            use crate::semantic::call_graph::CallSites;
                            let hub = FunctionId::free(format!("$contract$into${conversion}"));
                            // Share the virtual target union across callers and repeated ? sites.
                            if conversion_hubs.insert(conversion) {
                                let targets = self.method_effect_ids(&Type::Named(conversion.owner_type().unwrap().to_string()), "into");
                                graph.merge(hub, CallSites {
                                    has_unknown: targets.is_empty(),
                                    targets: targets.into_iter().map(|id| self.effect_index.local_target(id)).collect(),
                                    virtual_calls: [conversion].into(),
                                    virtual_spans: [(conversion, *span)].into(),
                                    ..Default::default()
                                });
                            }
                            graph.merge(owner, CallSites {
                                targets: [hub].into(),
                                call_spans: [(hub, *span)].into(),
                                ..Default::default()
                            });
                        }
                        allocates
                    },
                    Expr::Range(range) => !inline_ranges.remove(&range.id),
                    Expr::MethodCall(call) => self.contract_call_storage_allocates(call),
                    Expr::Select(select) => select.cases.iter().any(|case| matches!(&case.kind,
                        SelectCaseKind::Send { channel, .. } if self.contract_unary_storage_allocates(channel))),
                    Expr::Binary(b) => {
                        b.op == BinOp::Add && self.expr_types.get(&expr.id()) == Some(&Type::String)
                    }
                    Expr::Call(_) => self.enum_variant_resolutions.contains_key(&expr.id())
                        && enum_type.is_none_or(|ty| self.contract_enum_allocates(ty, &mut heaps)),
                    Expr::Var(name, ..) => {
                        (self.enum_variant_resolutions.contains_key(&expr.id())
                                && enum_type.is_none_or(|ty| self.contract_enum_allocates(ty, &mut heaps)))
                            || (self
                                .analysis_calls
                                .get(&expr.id())
                                .is_some_and(Option::is_some)
                                && self.symbols.lookup_func(name).is_some_and(|f| {
                                    matches!(f.constant, Some(ConstValue::String(_)))
                                }))
                    }
                    Expr::StaticCall(c) => {
                        (self.is_static_enum_variant(&c.class, &c.method)
                                && enum_type.is_none_or(|ty| self.contract_enum_allocates(ty, &mut heaps)))
                            || self
                                .symbols
                                .lookup_module_func(&c.class, &c.method)
                                .is_some_and(|f| matches!(f.constant, Some(ConstValue::String(_))))
                    }
                    Expr::StaticField(c) => {
                        (self.is_static_enum_variant(&c.class, &c.field)
                                && enum_type.is_none_or(|ty| self.contract_enum_allocates(ty, &mut heaps)))
                            || self
                                .symbols
                                .lookup_module_func(&c.class, &c.field)
                                .is_some_and(|f| matches!(f.constant, Some(ConstValue::String(_))))
                    }
                    Expr::Lambda(lambda) => {
                        walk.skip_children();
                        let id = self
                            .body_queries
                            .as_ref()
                            .and_then(|q| q.index().lambda_in(body_id, lambda.id))
                            .or(match &lambda.body {
                                LambdaBody::Block(b) => Some(b.id),
                                _ => None,
                            });
                        if let Some(id) = id {
                            pending.push((
                                FunctionId::lambda(id),
                                id,
                                match self.expr_types.get(&lambda.id) {
                                    Some(Type::Fn(_, ret) | Type::Closure(_, ret)) => ret.as_ref(),
                                    _ => lambda.return_type.as_ref().unwrap_or(&VOID),
                                },
                                match &lambda.body {
                                    LambdaBody::Block(b) => AstEvent::Block(b),
                                    LambdaBody::Expr(e) => AstEvent::Expr(e),
                                },
                            ));
                        }
                        matches!(self.expr_types.get(&expr.id()), Some(Type::Closure(..)))
                    }
                    _ => false,
                };
                if allocates {
                    earliest_allocation(&mut allocations, owner, expr.span());
                }
            }
        }
        allocations
    }

    /// Contract proofs are deliberately separate from the runtime/lock lattice:
    /// an unknown callback must fail a declaration proof even outside a lock.
    /// Arity hub nodes reuse the existing address-taken analysis without a
    /// call-sites × candidate-values expansion.
    fn solve_contracts(
        &self,
        program: &Program,
        graph: &CallGraph,
        facts: &EffectFacts<EffectWitness>,
        roots: impl Iterator<Item = FunctionId>,
    ) -> (CallGraph, EffectFacts<EffectWitness>) {
        const CONTRACT_EFFECTS: RuntimeEffects = RuntimeEffects::MAY_BLOCK
            .union(RuntimeEffects::MAY_SUSPEND)
            .union(RuntimeEffects::MAY_IO)
            .union(RuntimeEffects::MAY_PANIC)
            .union(RuntimeEffects::MAY_CHECKED_PANIC)
            .union(RuntimeEffects::MAY_ALLOCATE);
        use crate::compiler_db::effects::LOCK_EFFECT_WAIT;
        use crate::semantic::{call_graph::CallSites, effects::EffectProblem};
        let mut graph = graph.clone();
        let allocations = self.contract_allocations(program, &mut graph);
        for call in &self.indirect_lock.calls {
            #[cfg(test)]
            CONTRACT_GRAPH_WORK.with(|n| n.set(n.get() + 1));
            graph.merge(
                call.caller,
                CallSites {
                    targets: [FunctionId::free(format!(
                        "$contract$arity{}",
                        call.bit.trailing_zeros()
                    ))]
                    .into(),
                    // The arity set is conservative, not a proof that an arbitrary
                    // parameter cannot receive an unseen value.
                    has_unknown: true,
                    ..Default::default()
                },
            );
        }
        for value in &self.indirect_lock.values {
            #[cfg(test)]
            CONTRACT_GRAPH_WORK.with(|n| n.set(n.get() + 1));
            graph.merge(
                FunctionId::free(format!("$contract$arity{}", value.bit.trailing_zeros())),
                CallSites {
                    has_unknown: value.target.is_none(),
                    ..Default::default()
                },
            );
        }
        let mut problem = EffectProblem::new()
            .external_callee(CONTRACT_EFFECTS)
            .unknown_callee(CONTRACT_EFFECTS)
            .missing_body(CONTRACT_EFFECTS)
            .default_transmit(CONTRACT_EFFECTS);
        // A matching arity identifies a possible target, not an exact dynamic
        // target. External witnesses sort after proven Lock witnesses, so an
        // uncertain candidate cannot hide an independently proven violation.
        for value in &self.indirect_lock.values {
            let Some(target) = value.target else {
                continue;
            };
            let target = self.effect_index.local_target(target);
            if self.lock_callee_is_async(&target) == Some(true) {
                continue;
            }
            let Some(summary) = facts.get(&target) else {
                continue;
            };
            let hub = FunctionId::free(format!("$contract$arity{}", value.bit.trailing_zeros()));
            for bit in CONTRACT_BITS {
                if summary.intersects(bit) {
                    problem = problem.seed(hub, bit, Some(EffectWitness::External(hub)));
                }
            }
        }
        let mut nodes: HashSet<_> = graph.ids().copied().chain(roots).collect();
        for (_, sites) in graph.iter() {
            nodes.extend(&sites.targets);
        }
        // Lambdas and implicit constructors can have empty bodies without a
        // call-graph entry. Their checked summaries still establish purity.
        nodes.extend(
            facts
                .iter()
                .filter_map(|(&id, _)| (id.is_lambda()).then_some(id)),
        );
        let mut normalized = CallGraph::default();
        let mut abi_effects = HashMap::new();
        let builtin_aliases = self
            .imported_std_modules
            .iter()
            .map(|(alias, module)| (alias.clone(), module.module.clone()))
            .collect();
        for (&id, sites) in graph.iter() {
            let mut sites = sites.clone();
            if sites.has_unknown
                && !sites.targets.is_empty()
                && sites.unsupported_initialization.len() == 1
                && sites.unsupported_initialization.contains(
                    &crate::semantic::call_graph::UnsupportedInitialization::InjectedDefault,
                )
            {
                sites.has_unknown = false;
            }
            // A dependency is checked before its consumers. Open dispatch in
            // that dependency is not a closed set: a consumer may introduce a
            // new override. Do not export a proof based on today's subset.
            if self
                .effect_queries
                .as_ref()
                .is_some_and(|(_, unit)| *unit != crate::module::UnitId::ENTRY)
                && sites.virtual_calls.iter().any(|target| {
                    let Some(owner) = target.owner_type().map(|ty| ty.to_string()) else {
                        return true;
                    };
                    if self.symbols.lookup_interface(&owner).is_some() {
                        return true;
                    }
                    self.symbols.lookup_class(&owner).is_none_or(|class| {
                        class.is_open
                            && self
                                .resolved_method_class(&owner, &target.name())
                                .and_then(|declaring| self.symbols.lookup_class(&declaring))
                                .and_then(|class| class.methods.get(target.name().as_ref()))
                                .is_none_or(|method| method.is_open)
                    })
                })
            {
                sites.has_unknown = true;
            }
            normalized.merge(id, sites);
        }
        #[cfg(test)]
        CONTRACT_GRAPH_WORK.with(|n| {
            n.set(
                n.get()
                    + nodes.len()
                    + normalized
                        .iter()
                        .map(|(_, sites)| sites.targets.len())
                        .sum::<usize>(),
            )
        });
        let foreign: HashMap<_, _> = program
            .items
            .iter()
            .filter_map(|item| {
                let Item::Function(f) = item else {
                    return None;
                };
                let bridge = f.rust_bridge.as_ref()?;
                // A reviewed `nonblocking` declaration does not wait; an
                // `@effects(no_io)` declaration performs no I/O.
                let mut effects = if bridge.nonblocking.is_some() {
                    RuntimeEffects::NONE
                } else {
                    LOCK_EFFECT_WAIT
                };
                if !f
                    .effect_contracts
                    .iter()
                    .any(|c| c.kind == EffectContractKind::NoIo)
                {
                    effects = effects.union(RuntimeEffects::MAY_IO);
                }
                // Same identity as the effect summaries: a namespaced
                // declaration `ns::f` is `f` in namespace `ns`.
                Some((FunctionId::free_from_source_name(&f.name), effects))
            })
            .collect();
        for id in nodes {
            if let Some(&effects) = foreign.get(&id) {
                // Rust code may panic or allocate; no declaration can rule it out.
                let effects = effects
                    .union(RuntimeEffects::MAY_PANIC)
                    .union(RuntimeEffects::MAY_CHECKED_PANIC)
                    .union(RuntimeEffects::MAY_ALLOCATE);
                problem = problem.seed(id, effects.intersection(CONTRACT_EFFECTS), None);
                continue;
            }
            let local = graph.get(&id).is_some()
                || id.is_lambda()
                || (id.name().as_ref() == "init"
                    && id.owner().is_some_and(|owner| {
                        self.symbols
                            .lookup_class(&owner)
                            .is_some_and(|_| id.namespace().is_none())
                    }));
            let summary = if local {
                facts.get(&id).cloned()
            } else {
                self.effect_queries.as_ref().and_then(|(queries, unit)| {
                    let (owner, target) =
                        queries.external_target(*unit, &id, &self.effect_index.imports)?;
                    queries
                        .contract_facts
                        .borrow()
                        .get(&owner)?
                        .get(&target)
                        .cloned()
                })
            };
            if !local
                && let Some(effects) =
                    crate::semantic::intrinsics::builtin_contract_effects(&id, &builtin_aliases)
            {
                abi_effects.insert(id, effects);
                problem = problem.seed(id, effects.intersection(CONTRACT_EFFECTS), None);
                continue;
            }
            if let Some(summary) = summary {
                problem = problem.seed(id, RuntimeEffects::NONE, None);
                for bit in CONTRACT_BITS {
                    if summary.intersects(bit) {
                        let witness = summary.witness(bit).cloned().filter(|w| {
                            contract_cause(w).is_some()
                                && w.lock().is_none_or(|c| c.operation != "extern rust")
                        });
                        // A solved runtime bound may include unavailable imported
                        // evidence. Re-resolve those edges against current proofs;
                        // only proven local effects are valid additional seeds.
                        if !local || witness.is_some() {
                            problem = problem.seed(id, bit, witness);
                        }
                    }
                }
                if summary
                    .witness(LOCK_EFFECT_WAIT)
                    .and_then(EffectWitness::lock)
                    .is_some_and(|c| c.operation == "extern rust")
                {
                    problem = problem.seed(id, LOCK_EFFECT_WAIT, None);
                }
            } else if id.name().starts_with("$contract$arity")
                || id.name().starts_with("$contract$into$")
            {
                problem = problem.body(id);
            } else {
                problem = problem.seed(id, CONTRACT_EFFECTS, None);
            }
            // Interface union nodes with no implementation do not prove a
            // signature merely because an empty runtime fact was allocated.
            if let Some(owner) = id.owner()
                && self.symbols.lookup_interface(&owner).is_some()
                && !id.name().starts_with("$default$")
                && graph.get(&id).is_none_or(|sites| sites.targets.is_empty())
            {
                problem = problem.seed(id, CONTRACT_EFFECTS, None);
            }
            if self.lock_callee_is_async(&id) == Some(true) {
                problem = problem.transmit(
                    id,
                    RuntimeEffects::MAY_IO
                        .union(RuntimeEffects::MAY_PANIC)
                        .union(RuntimeEffects::MAY_CHECKED_PANIC)
                        .union(RuntimeEffects::MAY_ALLOCATE),
                );
            }
        }
        // ABI effects are proved at each actual source call, not at a shared
        // synthetic leaf whose representative span might belong to another caller.
        for (&caller, sites) in normalized.iter() {
            // Calling an async callable creates its task/future at the call
            // site, whether the target is bound directly or by dispatch.
            // Pick the earliest site so the witness is independent of map order.
            if let Some(span) = sites
                .call_spans
                .iter()
                .chain(&sites.virtual_spans)
                .filter(|(target, _)| self.lock_callee_is_async(target) == Some(true))
                .map(|(_, span)| span)
                .min_by_key(|span| (span.file_id.0, span.start, span.end))
            {
                problem = problem.seed(
                    caller,
                    RuntimeEffects::MAY_ALLOCATE,
                    Some(EffectWitness::Allocation {
                        owner: caller,
                        source: (span.file_id.0, span.start, span.end),
                    }),
                );
            }
            for (target, span) in &sites.call_spans {
                let Some(effects) = abi_effects
                    .get(target)
                    .filter(|e| **e != RuntimeEffects::ALL)
                else {
                    continue;
                };
                let source = (span.file_id.0, span.start, span.end);
                for bit in [
                    RuntimeEffects::MAY_BLOCK,
                    RuntimeEffects::MAY_SUSPEND,
                    RuntimeEffects::MAY_IO,
                    RuntimeEffects::MAY_PANIC,
                    RuntimeEffects::MAY_CHECKED_PANIC,
                    RuntimeEffects::MAY_ALLOCATE,
                ] {
                    if !effects.intersects(bit) {
                        continue;
                    }
                    // Only wait/IO bits carry a lock-style cause; panic and
                    // allocation witnesses record the call-site offsets.
                    let lock_kind = if bit == RuntimeEffects::MAY_BLOCK {
                        Some(LockEffectKind::Block)
                    } else if bit == RuntimeEffects::MAY_SUSPEND {
                        Some(LockEffectKind::Suspend)
                    } else if bit == RuntimeEffects::MAY_IO {
                        Some(LockEffectKind::Io)
                    } else {
                        None
                    };
                    let witness = match lock_kind {
                        Some(kind) => {
                            EffectWitness::Lock(crate::compiler_db::effects::LockEffectWitness {
                                owner: caller,
                                cause: LockEffectCause {
                                    is_conservative: false,
                                    span: *span,
                                    operation: target.to_string().into(),
                                    kind,
                                    lock: None,
                                },
                            })
                        }
                        None if bit == RuntimeEffects::MAY_ALLOCATE => EffectWitness::Allocation {
                            owner: caller,
                            source,
                        },
                        None => EffectWitness::Panic {
                            owner: caller,
                            source,
                        },
                    };
                    problem = problem.seed(caller, bit, Some(witness));
                }
            }
        }
        for (owner, span) in allocations {
            problem = problem.seed(
                owner,
                RuntimeEffects::MAY_ALLOCATE,
                Some(EffectWitness::Allocation {
                    owner,
                    source: (span.file_id.0, span.start, span.end),
                }),
            );
        }
        let facts = problem.solve_reachable(&normalized);
        (normalized, facts)
    }

    /// Propagate each declaration contract kind once per (type, method).
    /// In particular, a deep chain never rescans all its ancestors per method.
    fn contract_obligations(
        &mut self,
        program: &Program,
        templates: bool,
    ) -> Vec<(FunctionId, EffectContract)> {
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        let mut obligations: HashMap<String, HashMap<String, [Option<EffectContract>; 8]>> =
            HashMap::new();
        let mut queue = VecDeque::new();
        let mut types = HashSet::new();
        let canonical = |name: &str| {
            self.symbols
                .lookup_class(name)
                .map(|c| c.name.clone())
                .or_else(|| self.symbols.lookup_interface(name).map(|i| i.name.clone()))
                .unwrap_or_else(|| name.to_string())
        };
        // Only local declarations and their ancestors can impose an obligation
        // in this unit. Do not rescan unrelated imported type families.
        let mut pending: Vec<_> = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Class(c) if c.type_params.is_empty() != templates => Some(canonical(&c.name)),
                Item::Interface(i) if i.type_params.is_empty() != templates => {
                    Some(canonical(&i.name))
                }
                _ => None,
            })
            .collect();
        while let Some(name) = pending.pop() {
            if !types.insert(name.clone()) {
                continue;
            }
            if let Some(class) = self.symbols.lookup_class(&name) {
                for parent in class.base_class.iter().map(String::as_str).chain(
                    class.implements.iter().filter_map(|ty| match ty {
                        Type::Named(name) | Type::Generic(name, _) => Some(name.as_str()),
                        _ => None,
                    }),
                ) {
                    let parent = canonical(parent);
                    children
                        .entry(parent.clone())
                        .or_default()
                        .push(name.clone());
                    pending.push(parent);
                }
                for (method, info) in &class.methods {
                    if !info.is_static {
                        for contract in &info.effect_contracts {
                            queue.push_back((name.clone(), method.clone(), contract.clone()));
                        }
                    }
                }
            } else if let Some(interface) = self.symbols.lookup_interface(&name) {
                for parent in &interface.extends {
                    let parent = canonical(parent);
                    children
                        .entry(parent.clone())
                        .or_default()
                        .push(name.clone());
                    pending.push(parent);
                }
                for (method, info) in &interface.methods {
                    for contract in &info.effect_contracts {
                        queue.push_back((name.clone(), method.clone(), contract.clone()));
                    }
                }
            }
        }
        // Stable declaration locations make inherited diagnostic labels stable
        // even when several ancestors impose the same contract.
        queue.make_contiguous().sort_by_key(|(owner, method, c)| {
            (c.span.file_id, c.span.start, owner.clone(), method.clone())
        });
        while let Some((owner, method, contract)) = queue.pop_front() {
            #[cfg(test)]
            OBLIGATION_WORK.with(|n| n.set(n.get() + 1));
            let index = match contract.kind {
                EffectContractKind::Pure => 3,
                EffectContractKind::Verified => 4,
                EffectContractKind::NoWait => 0,
                EffectContractKind::NoBlock => 1,
                EffectContractKind::NoSuspend => 2,
                EffectContractKind::NoIo => 5,
                EffectContractKind::NoPanic => 6,
                EffectContractKind::NoAlloc => 7,
            };
            let entry = obligations
                .entry(owner.clone())
                .or_default()
                .entry(method.clone())
                .or_default();
            if entry[index].is_some() {
                continue;
            }
            entry[index] = Some(contract.clone());
            for child in children.get(&owner).into_iter().flatten() {
                queue.push_back((child.clone(), method.clone(), contract.clone()));
            }
        }
        let mut declarations = Vec::new();
        let mut diagnostics = Vec::new();
        for item in &program.items {
            match item {
                Item::Function(f) if f.type_params.is_empty() != templates => {
                    declarations.extend(
                        f.effect_contracts
                            .iter()
                            .cloned()
                            .map(|c| (FunctionId::free(&f.name), c)),
                    );
                }
                Item::Class(class) if class.type_params.is_empty() != templates => {
                    let owner = canonical(&class.name);
                    for method in &class.methods {
                        let id = FunctionId::method(TypeId::local(&class.name), &method.name);
                        declarations
                            .extend(method.effect_contracts.iter().cloned().map(|c| (id, c)));
                    }
                    // Include inherited implementations, even if the class adds
                    // an interface without declaring a new method body.
                    let explicit_methods: HashMap<_, _> =
                        class.methods.iter().map(|m| (m.name.as_str(), m)).collect();
                    let mut required: Vec<_> = obligations
                        .get(&owner)
                        .into_iter()
                        .flat_map(|m| m.iter())
                        .collect();
                    required.sort_by_key(|(method, _)| *method);
                    for (method, contracts) in required {
                        let Some(declaring) = self.resolved_method_class(&class.name, method)
                        else {
                            continue;
                        };
                        let id = self.effect_index.local_target(FunctionId::method(
                            TypeId::from_source_name(&declaring),
                            method,
                        ));
                        let explicit = explicit_methods.get(method.as_str()).copied();
                        for contract in contracts.iter().flatten() {
                            if let Some(m) = explicit
                                && !m.effect_contracts.is_empty()
                                && !m
                                    .effect_contracts
                                    .iter()
                                    .fold(RuntimeEffects::NONE, |bits, c| {
                                        bits.union(c.kind.forbidden())
                                    })
                                    .contains(contract.kind.forbidden())
                            {
                                diagnostics.push(Diagnostic::new(Severity::Error, ErrorCode::E2611, format!("override `{id}` weakens inherited effect contract `{}`", contract.kind.name()))
                                        .with_label(Label::primary(m.span, "override must preserve all inherited prohibitions"))
                                        .with_label(Label::secondary(contract.span, "inherited contract declared here")));
                            }
                            declarations.push((id, contract.clone()));
                        }
                    }
                }
                Item::Interface(interface) if interface.type_params.is_empty() != templates => {
                    let owner = canonical(&interface.name);
                    for method in &interface.methods {
                        let inherited = obligations.get(&owner).and_then(|m| m.get(&method.name));
                        for contract in inherited.into_iter().flatten().flatten() {
                            if !method.effect_contracts.is_empty()
                                && !method
                                    .effect_contracts
                                    .iter()
                                    .fold(RuntimeEffects::NONE, |bits, c| {
                                        bits.union(c.kind.forbidden())
                                    })
                                    .contains(contract.kind.forbidden())
                            {
                                diagnostics.push(Diagnostic::new(Severity::Error, ErrorCode::E2611, format!("interface method `{}::{}` weakens an inherited effect contract", interface.name, method.name))
                                    .with_label(Label::primary(method.span, "weakened contract"))
                                    .with_label(Label::secondary(contract.span, "inherited contract declared here")));
                            }
                            if method.default_body.is_some() {
                                declarations.push((
                                    interface_default_effect_id(&interface.name, &method.name),
                                    contract.clone(),
                                ));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        for diagnostic in diagnostics {
            self.push(diagnostic);
        }
        declarations.sort_by_key(|(id, c)| (*id, c.kind.name(), c.span.file_id, c.span.start));
        declarations.dedup_by_key(|(id, c)| (*id, c.kind.name()));
        declarations
    }

    pub(super) fn report_effect_contracts(
        &mut self,
        program: &Program,
        graph: &CallGraph,
        facts: &EffectFacts<EffectWitness>,
        templates: bool,
    ) {
        let mut declarations = self.contract_obligations(program, templates);
        self.report_purity(program, graph, &declarations, templates);
        declarations.retain(|(_, c)| {
            !matches!(
                c.kind,
                EffectContractKind::Pure | EffectContractKind::Verified
            )
        });
        let (graph, contract_facts) = self.solve_contracts(
            program,
            graph,
            facts,
            declarations.iter().map(|(id, _)| *id),
        );
        let contract_facts = std::sync::Arc::new(contract_facts);
        let facts = &contract_facts;
        let graph = &graph;
        if !templates && let Some((queries, unit)) = &self.effect_queries {
            queries
                .contract_facts
                .borrow_mut()
                .insert(*unit, std::sync::Arc::clone(&contract_facts));
        }
        if declarations.is_empty() {
            return;
        }
        let summaries: HashMap<_, _> = facts.iter().map(|(&id, summary)| (id, summary)).collect();
        let forbidden = declarations
            .iter()
            .fold(RuntimeEffects::NONE, |bits, (_, c)| {
                bits.union(c.kind.forbidden())
            });
        let bits: Vec<_> = CONTRACT_BITS
            .into_iter()
            .filter(|bit| forbidden.intersects(*bit))
            .collect();
        let paths: Vec<_> = bits
            .iter()
            .map(|&bit| witness_paths(graph, facts, &self.effect_index.callables, bit))
            .collect();
        let mut span_index = None;
        for (id, contract) in declarations {
            let Some(summary) = summaries.get(&id) else {
                self.push(Diagnostic::new(Severity::Error, ErrorCode::E2612,
                        format!("effect contract `{}` cannot be proven for `{id}`: missing verified summary", contract.kind.name()))
                        .with_label(Label::primary(contract.span, "effect contract declared here")));
                continue;
            };
            // Prefer a proven violation over uncertainty in another bit.
            let matching = |bit: &RuntimeEffects| {
                contract.kind.forbidden().intersects(*bit) && summary.intersects(*bit)
            };
            let Some(index) = bits
                .iter()
                .position(|bit| {
                    matching(bit) && summary.witness(*bit).and_then(contract_cause).is_some()
                })
                .or_else(|| bits.iter().position(matching))
            else {
                continue;
            };
            let bit = bits[index];
            let cause = summary.witness(bit).and_then(contract_cause);
            let proven = cause.is_some();
            let mut diagnostic = Diagnostic::new(
                Severity::Error,
                if proven {
                    ErrorCode::E2610
                } else {
                    ErrorCode::E2612
                },
                format!(
                    "effect contract `{}` {} `{id}`",
                    contract.kind.name(),
                    if proven {
                        "violated by"
                    } else {
                        "cannot be proven for"
                    }
                ),
            )
            .with_label(Label::secondary(
                contract.span,
                "effect contract declared here",
            ));
            let mut chain = vec![id.to_string()];
            let mut cursor = id;
            let mut conversion_span = None;
            while let Some(&(callee, span)) = paths[index].get(&cursor) {
                if callee.name().starts_with("$contract$into$") {
                    conversion_span = span;
                } else {
                    if let Some(span) = conversion_span.take().or(span) {
                        diagnostic = diagnostic.with_label(Label::secondary(
                            span,
                            format!("`{}` calls `{callee}`", chain.last().unwrap()),
                        ));
                    }
                    chain.push(callee.to_string());
                }
                cursor = callee;
            }
            let certainty = match cause {
                Some((span, operation)) => {
                    let span = if span.line == 0 {
                        span_index
                            .get_or_insert_with(|| source_span_index(program))
                            .get(&(span.file_id.0, span.start, span.end))
                            .copied()
                            .unwrap_or(span)
                    } else {
                        span
                    };
                    diagnostic = diagnostic.with_label(Label::primary(
                        span,
                        format!("root operation `{operation}`"),
                    ));
                    chain.push(operation.to_owned());
                    "compiler-proven"
                }
                None => {
                    // A conservative wait/IO bound still locates the operation
                    // that may exhibit the effect.
                    if let Some(cause) = summary.witness(bit).and_then(EffectWitness::lock) {
                        diagnostic = diagnostic.with_label(Label::primary(
                            cause.span,
                            format!("possible operation `{}`", cause.operation),
                        ));
                        chain.push(cause.operation.to_string());
                    } else {
                        diagnostic = diagnostic.with_label(Label::primary(
                            contract.span,
                            "effect has no available source witness",
                        ));
                    }
                    "conservative-bound"
                }
            };
            diagnostic = diagnostic.with_note(format!(
                "certainty: {certainty}; call chain: {}",
                chain.join(" -> ")
            ));
            self.push(diagnostic);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::check::check_source;
    use super::*;

    fn contract_errors(source: &str) -> Vec<Diagnostic> {
        check_source(source)
            .into_iter()
            .filter(|d| d.severity == Severity::Error)
            .collect()
    }

    /// Without body queries an expression-bodied lambda has no stable
    /// BodyId to scan, but every lambda is reached only through an unknown
    /// indirect or callback edge, so the contract stays fail-closed.
    #[test]
    fn no_alloc_expression_lambda_without_body_queries_fails_closed() {
        for lambda in [
            "|x: i64| x + 1",
            "|x: i64| [x].len()",
            "|x: i64| { return [x].len(); }",
        ] {
            for body in [
                "return h(1);",
                "let o: Option<i64> = Some(1); return o.map(h).unwrap();",
            ] {
                let source = format!(
                    "import std::collections::Array; enum Option<T> {{ Some(T), None }} @effects(no_alloc) fn f(h: fn(i64) -> i64) -> i64 {{ {body} }} fn main() {{ let g = {lambda}; f(g); }}"
                );
                let errors = contract_errors(&source);
                assert!(
                    !errors.is_empty()
                        && errors
                            .iter()
                            .all(|d| matches!(d.code, ErrorCode::E2610 | ErrorCode::E2612)),
                    "{lambda} {body}: {errors:?}"
                );
            }
        }
    }

    /// Calling an async method creates its task at the call site, whether the
    /// method is bound directly or through virtual dispatch.
    #[test]
    fn no_alloc_async_call_through_dispatch() {
        for (receiver, open) in [("Base", true), ("D", true), ("Base", false)] {
            let modifier = if open { "open" } else { "" };
            let source = format!(
                "open class Base {{ pub {modifier} async fn run(self) -> i64 {{ return 1; }} }} class D extends Base {{ {} }} @effects(no_alloc) fn f(x: {receiver}) {{ let _ = x.run(); }} fn main() {{}}",
                if open {
                    "pub override async fn run(self) -> i64 { return 2; }"
                } else {
                    ""
                }
            );
            let errors = contract_errors(&source);
            assert!(
                errors.len() == 1 && errors[0].code == ErrorCode::E2610,
                "{receiver} {open}: {errors:?}"
            );
        }
    }

    #[test]
    fn no_alloc_conversion_dispatch_work_bounds() {
        for n in [8, 32, 128, 512] {
            let mut source = String::from(
                "interface Into<T> { fn into(self)->T; } enum Result<T,E> { Ok(T), Err(E) } open class Source implements Into<i64> { pub open fn into(self)->i64 { return 0; } }",
            );
            for i in 0..n {
                source.push_str(&format!("class C{i} extends Source {{ pub override fn into(self)->i64 {{ return {i}; }} }} @effects(no_alloc) fn guard{i}(x:Result<i64,Source>,y:Result<i64,i64>)->Result<i64,i64> {{ x?; x?; return y; }}"));
            }
            CONTRACT_GRAPH_WORK.with(|v| v.set(0));
            let errors: Vec<_> = check_source(&source)
                .into_iter()
                .filter(|d| d.severity == Severity::Error)
                .collect();
            assert!(errors.is_empty(), "{n}: {errors:?}");
            let work = CONTRACT_GRAPH_WORK.with(|v| v.get());
            assert!(work <= 20 * n + 100, "{n} {work}");
            println!("conversion-dispatch callers={n} overrides={n} graph={work}");
        }
    }

    #[test]
    fn no_alloc_iteration_work_bounds() {
        for n in [8, 32, 128, 512] {
            for (ty, body, allocates) in [
                ("Map<i64,i64>", "for item in value {}", true),
                (
                    "Map<i64,i64>",
                    "for outer in value { for inner in value {} }",
                    true,
                ),
                ("Array<i64>", "for item in value {}", false),
                ("Range<i64>", "for item in value {}", false),
                ("i64", "for item in 0..value {}", false),
            ] {
                let source = format!(
                    "import std::collections::Map; import std::collections::Array; @effects(no_alloc) fn f(value:{ty}) {{ {} }}",
                    body.repeat(n)
                );
                ALLOCATION_SCAN_WORK.with(|v| v.set(0));
                let errors: Vec<_> = check_source(&source)
                    .into_iter()
                    .filter(|d| d.severity == Severity::Error)
                    .collect();
                assert_eq!(
                    errors.len(),
                    usize::from(allocates),
                    "{ty} {body} {n}: {errors:?}"
                );
                assert!(
                    errors.iter().all(|d| d.code == ErrorCode::E2610),
                    "{errors:?}"
                );
                let visits = ALLOCATION_SCAN_WORK.with(|v| v.get());
                assert!(visits <= 25 * n + 10, "{ty} {n} {visits}");
                println!(
                    "iteration ty={ty} nested={} n={n} ast={visits}",
                    body.contains("outer")
                );
            }
        }
    }

    #[test]
    fn no_alloc_interface_option_work_bounds() {
        for n in [8, 32, 128, 512] {
            for (declaration, payload) in [("interface I {}", "I"), ("interface I<T> {}", "I<i64>")]
            {
                for (shape, expression) in
                    [("some", "Some(y)"), ("none", "None"), ("propagate", "x?")]
                {
                    let body = if shape == "propagate" {
                        "x?;".to_owned()
                    } else {
                        format!("if true {{ let v:Option<{payload}> = {expression}; }}")
                    };
                    let source = format!(
                        "{declaration} enum Option<T> {{ Some(T), None }} @effects(no_alloc) fn f(x:Option<{payload}>,y:{payload})->Option<{payload}> {{ {} return x; }}",
                        body.repeat(n)
                    );
                    ALLOCATION_SCAN_WORK.with(|v| v.set(0));
                    let errors: Vec<_> = check_source(&source)
                        .into_iter()
                        .filter(|d| d.severity == Severity::Error)
                        .collect();
                    assert_eq!(errors.len(), 1, "{payload} {shape} {n}: {errors:?}");
                    assert_eq!(errors[0].code, ErrorCode::E2610, "{errors:?}");
                    let ast = ALLOCATION_SCAN_WORK.with(|v| v.get());
                    assert!(ast <= 20 * n + 20, "{payload} {shape} {n} {ast}");
                    println!("interface-option payload={payload} shape={shape} n={n} ast={ast}");
                }
            }
        }
    }

    #[test]
    fn no_alloc_enum_representation_work_bounds() {
        for n in [8, 32, 128, 512] {
            let variants = (0..n)
                .map(|i| format!("V{i}"))
                .collect::<Vec<_>>()
                .join(",");
            for (shape, body) in [("tags", "Color::V0;"), ("propagation", "x?;")] {
                let source = format!(
                    "enum Color {{ {variants} }} enum Result<T,E> {{ Ok(T), Err(E) }} @effects(no_alloc) fn f(x:Result<i64,bool>)->Result<i64,bool> {{ {} return x; }}",
                    body.repeat(n)
                );
                ALLOCATION_SCAN_WORK.with(|v| v.set(0));
                ENUM_REPR_WORK.with(|v| v.set(0));
                let errors: Vec<_> = check_source(&source)
                    .into_iter()
                    .filter(|d| d.severity == Severity::Error)
                    .collect();
                assert!(errors.is_empty(), "{shape} {n}: {errors:?}");
                let ast = ALLOCATION_SCAN_WORK.with(|v| v.get());
                let variants = ENUM_REPR_WORK.with(|v| v.get());
                assert!(ast <= 10 * n + 10, "{shape} {n} {ast}");
                assert!(variants <= n, "{shape} {n} {variants}");
                println!("enum-repr shape={shape} n={n} ast={ast} variants={variants}");
            }
        }
    }

    #[test]
    fn no_alloc_storage_lowering_paths() {
        for (source, allocation) in [
            ("fn f(a:Array<I>,v:I) { a[0]=v; }", true),
            ("fn f(a:Array<Option<i64>>,v:Option<i64>) { a[0]=v; }", true),
            ("fn f(a:Array<i64>,v:i64) { a[0]=v; }", false),
            (
                "fn f(a:Map<Option<i64>,i64>,v:Option<i64>) { a.contains(v); }",
                true,
            ),
            ("fn f(a:Map<i64,I>,v:I) { a.insert(0,v); }", true),
            ("fn f(a:Map<i64,i64>,v:i64) { a.contains(v); }", true),
            ("async fn f(c:Channel<I>,v:I) { c.send(v); }", true),
            ("async fn f(c:Mutex<I>) { lock c as v {} }", true),
            ("async fn f(c:RwLock<I>) { lock read c as v {} }", false),
            ("async fn f(c:Mutex<i64>) { lock c as v {} }", false),
            (
                "async fn f(c:Channel<I>,v:I) { select { c.send(v) => {} } }",
                true,
            ),
        ] {
            let source = format!(
                "import std::collections::Array; import std::collections::Map; interface Send {{}} interface Sync extends Send {{}} interface I extends Sync {{}} enum Option<T> {{ Some(T), None }} enum Result<T,E> {{ Ok(T), Err(E) }} @effects(no_alloc) {source}"
            );
            let errors: Vec<_> = check_source(&source)
                .into_iter()
                .filter(|d| d.severity == Severity::Error)
                .collect();
            if allocation {
                assert!(
                    errors.iter().any(|d| d.code == ErrorCode::E2610),
                    "{source}: {errors:?}"
                );
                assert!(
                    errors.iter().all(|d| d.code == ErrorCode::E2610),
                    "{source}: {errors:?}"
                );
            } else {
                assert!(errors.is_empty(), "{source}: {errors:?}");
            }
        }
    }

    #[test]
    fn no_alloc_range_storage_work_bounds() {
        for n in [8, 32, 128, 512] {
            for (shape, body, allocation) in [
                ("materialized", "0..10;", true),
                ("inline", "for i in 0..10 {}", false),
                ("pair-store", "c.set(v);", true),
            ] {
                let source = format!(
                    "interface Send {{}} interface Sync extends Send {{}} interface I extends Sync {{}} enum Option<T> {{ Some(T), None }} enum Result<T,E> {{ Ok(T), Err(E) }} @effects(no_alloc) fn f(c:BlockingCell<I>,v:I) {{ {} }}",
                    body.repeat(n)
                );
                ALLOCATION_SCAN_WORK.with(|v| v.set(0));
                let errors = check_source(&source);
                assert_eq!(
                    errors.iter().filter(|d| d.code == ErrorCode::E2610).count(),
                    usize::from(allocation),
                    "{shape} {n}: {errors:?}"
                );
                assert!(
                    errors
                        .iter()
                        .all(|d| d.severity != Severity::Error || d.code == ErrorCode::E2610),
                    "{errors:?}"
                );
                let visits = ALLOCATION_SCAN_WORK.with(|v| v.get());
                assert!(visits <= 20 * n + 10, "{shape} {n} {visits}");
                println!("allocation-scan shape={shape} n={n} ast={visits}");
            }
        }
    }

    #[test]
    fn no_alloc_materialized_ranges_and_typed_storage() {
        for (source, allocation) in [
            ("fn f() { let r=0..10; }", true),
            ("fn f() { helper(); } fn helper() { let r=0..10; }", true),
            ("fn f() { for i in 0..10 {} }", false),
            ("fn f(n:i64) { for i in 0..n { for j in 0..i {} } }", false),
            ("fn f() { for i in 0..10 { let r=0..i; } }", true),
            ("fn f(c:BlockingCell<I>,v:I) { c.set(v); }", true),
            ("fn f(c:BlockingRwCell<I>,v:I) { c.write(v); }", true),
            (
                "fn f(c:BlockingCell<Option<i64>>,v:Option<i64>) { c.set(v); }",
                true,
            ),
            (
                "fn f(c:BlockingCell<Option<bool>>,v:Option<bool>) { c.set(v); }",
                true,
            ),
            (
                "fn f(c:BlockingCell<Option<f64>>,v:Option<f64>) { c.set(v); }",
                true,
            ),
            (
                "fn f(c:BlockingRwCell<Result<i64,bool>>,v:Result<i64,bool>) { c.write(v); }",
                true,
            ),
            (
                "fn f(c:BlockingCell<I>,v:I) { helper(c,v); } fn helper(c:BlockingCell<I>,v:I) { c.set(v); }",
                true,
            ),
            ("fn f(c:BlockingCell<i64>,v:i64) { c.set(v); }", false),
            ("fn f(c:BlockingCell<f64>,v:f64) { c.set(v); }", false),
            ("fn f(c:BlockingCell<bool>,v:bool) { c.set(v); }", false),
            ("fn f(c:BlockingRwCell<i64>,v:i64) { c.write(v); }", false),
            ("fn f(c:BlockingCell<String>,v:String) { c.set(v); }", false),
            (
                "fn f(c:BlockingCell<Option<String>>,v:Option<String>) { c.set(v); }",
                false,
            ),
            ("fn f(c:BlockingCell<I>)->I { return c.get(); }", false),
            (
                "fn f(c:BlockingRwCell<Option<i64>>)->Option<i64> { return c.read(); }",
                false,
            ),
            (
                "fn f(c:C,v:I) { c.set(v); } class C { pub fn set(self,v:I) {} }",
                false,
            ),
        ] {
            let source = format!(
                "interface Send {{}} interface Sync extends Send {{}} interface I extends Sync {{}} enum Option<T> {{ Some(T), None }} enum Result<T,E> {{ Ok(T), Err(E) }} @effects(no_alloc) {source}"
            );
            let errors: Vec<_> = check_source(&source)
                .into_iter()
                .filter(|d| d.severity == Severity::Error)
                .collect();
            if allocation {
                assert!(
                    errors.iter().any(|d| d.code == ErrorCode::E2610),
                    "{source}: {errors:?}"
                );
                assert!(
                    errors.iter().all(|d| d.code == ErrorCode::E2610),
                    "{source}: {errors:?}"
                );
            } else {
                assert!(errors.is_empty(), "{source}: {errors:?}");
            }
        }
    }

    #[test]
    fn panic_allocation_contract_work_bounds() {
        for (contract, leaf) in [("no_panic", "panic(\"bad\");"), ("no_alloc", "let a=[1];")] {
            for shape in ["chain", "fanout", "cycle"] {
                for n in [8, 32, 128, 512] {
                    let mut source = format!("fn leaf() {{ {leaf} }} ");
                    for i in 0..n {
                        let next = if shape == "fanout" || i == 0 {
                            "leaf".to_owned()
                        } else {
                            format!("node_{}", i - 1)
                        };
                        let back = if shape == "cycle" && i == 0 {
                            format!("node_{}();", n - 1)
                        } else {
                            String::new()
                        };
                        source
                            .push_str(&format!("fn node_{i}() {{ {next}(); {next}(); {back} }} "));
                    }
                    source.push_str(&format!(
                        "@effects({contract}) fn guard() {{ node_{}(); }}",
                        n - 1
                    ));
                    CONTRACT_GRAPH_WORK.with(|v| v.set(0));
                    PATH_WORK.with(|v| v.set(0));
                    ALLOCATION_SCAN_WORK.with(|v| v.set(0));
                    crate::semantic::effects::REACHABLE_EDGE_VISITS.with(|v| v.set(0));
                    let errors = check_source(&source);
                    assert_eq!(
                        errors.iter().filter(|e| e.code == ErrorCode::E2610).count(),
                        1,
                        "{errors:?}"
                    );
                    let graph = CONTRACT_GRAPH_WORK.with(|v| v.get());
                    let paths = PATH_WORK.with(|v| v.get());
                    let ast = ALLOCATION_SCAN_WORK.with(|v| v.get());
                    let edges = crate::semantic::effects::REACHABLE_EDGE_VISITS.with(|v| v.get());
                    assert!(
                        graph <= 12 * (n + 4)
                            && paths <= 60 * (n + 4)
                            && ast <= 40 * (n + 4)
                            && edges <= 24 * (n + 4),
                        "{contract} {shape} {n}: {graph} {paths} {ast} {edges}"
                    );
                    println!(
                        "{contract} {shape} n={n} graph={graph} paths={paths} ast={ast} edges={edges}"
                    );
                }
            }
        }
    }

    #[test]
    fn panic_allocation_contract_matrix() {
        for (label, source, expected) in [
            (
                "string constant",
                "const MSG:String=\"hi\"; @effects(no_alloc) fn f()->String { return MSG; }",
                Some(ErrorCode::E2610),
            ),
            (
                "shadowed constant",
                "const MSG:String=\"hi\"; @effects(no_alloc) fn f(MSG:String)->String { return MSG; }",
                None,
            ),
            (
                "requires",
                "@effects(no_panic) fn f(x:i64)->i64 requires x>0 { return x; }",
                Some(ErrorCode::E2610),
            ),
            (
                "ensures",
                "@effects(no_panic) fn f(x:i64)->i64 ensures result>0 { return x; }",
                Some(ErrorCode::E2610),
            ),
            (
                "literal string",
                "@effects(no_alloc) fn f()->String { return \"hello\"; }",
                Some(ErrorCode::E2610),
            ),
            (
                "closure creation",
                "@effects(no_alloc) fn f(x:i64) { let g:closure()->i64 = || x; }",
                Some(ErrorCode::E2610),
            ),
            (
                "pure dispatch",
                "interface I { fn f(self); } class C implements I { pub fn f(self) {} } @effects(no_panic,no_alloc) fn f(i:I) { i.f(); }",
                None,
            ),
            (
                "pure integer",
                "@effects(no_panic, no_alloc) fn f()->i64 { return 7; }",
                None,
            ),
            (
                "explicit panic",
                "@effects(no_panic) fn f() { panic(\"bad\"); }",
                Some(ErrorCode::E2610),
            ),
            (
                "checked arithmetic",
                "@effects(no_panic) fn f(x:i64)->i64 { return x+1; }",
                Some(ErrorCode::E2610),
            ),
            (
                "division",
                "@effects(no_panic) fn f(x:i64)->i64 { return 10/x; }",
                Some(ErrorCode::E2610),
            ),
            (
                "index",
                "import std::collections::Array; @effects(no_panic) fn f(a:Array<i64>)->i64 { return a[0]; }",
                Some(ErrorCode::E2610),
            ),
            (
                "array literal",
                "@effects(no_alloc) fn f() { let a=[1,2]; }",
                Some(ErrorCode::E2610),
            ),
            (
                "object",
                "class C { pub x:i64; } @effects(no_alloc) fn f() { let c=new C(1); }",
                Some(ErrorCode::E2610),
            ),
            (
                "concat",
                "@effects(no_alloc) fn f(a:String,b:String)->String { return a+b; }",
                Some(ErrorCode::E2610),
            ),
            (
                "helper panic",
                "fn leaf() { panic(\"bad\"); } fn middle() { leaf(); } @effects(no_panic) fn f() { middle(); }",
                Some(ErrorCode::E2610),
            ),
            (
                "helper allocation",
                "fn leaf() { let a=[1]; } fn middle() { leaf(); } @effects(no_alloc) fn f() { middle(); }",
                Some(ErrorCode::E2610),
            ),
            (
                "unknown panic",
                "@effects(no_panic) fn f(g:fn()) { g(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "unknown allocation",
                "@effects(no_alloc) fn f(g:fn()) { g(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "foreign panic",
                "extern rust { fn foreign(); } @effects(no_panic) fn f() { foreign(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "foreign allocation",
                "extern rust { fn foreign(); } @effects(no_alloc) fn f() { foreign(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "foreign io promise",
                "extern rust { @effects(no_io) fn foreign(); } @effects(no_alloc) fn f() { foreign(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "panic weakening",
                "open class B { @effects(no_panic,no_alloc) pub open fn f(self) {} } class C extends B { @effects(no_alloc) pub override fn f(self) {} }",
                Some(ErrorCode::E2611),
            ),
            (
                "allocation weakening",
                "open class B { @effects(no_panic,no_alloc) pub open fn f(self) {} } class C extends B { @effects(no_panic) pub override fn f(self) {} }",
                Some(ErrorCode::E2611),
            ),
            (
                "strengthening",
                "open class B { @effects(no_panic) pub open fn f(self) {} } class C extends B { @effects(no_panic,no_alloc) pub override fn f(self) {} }",
                None,
            ),
            (
                "inherited allocation",
                "interface I { @effects(no_alloc) fn f(self); } class C implements I { pub fn f(self) { let a=[1]; } }",
                Some(ErrorCode::E2610),
            ),
            (
                "dispatch allocation",
                "interface I { fn f(self); } class C implements I { pub fn f(self) { let a=[1]; } } @effects(no_alloc) fn f(i:I) { i.f(); }",
                Some(ErrorCode::E2610),
            ),
            (
                "dispatch panic",
                "interface I { fn f(self); } class C implements I { pub fn f(self) { panic(\"bad\"); } } @effects(no_panic) fn f(i:I) { i.f(); }",
                Some(ErrorCode::E2610),
            ),
            (
                "unresolved dispatch",
                "interface I { fn f(self); } @effects(no_alloc) fn f(i:I) { i.f(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "generic pure",
                "@effects(no_panic,no_alloc) fn f<T>(x:T)->T { return x; }",
                None,
            ),
            (
                "generic allocation",
                "@effects(no_alloc) fn f<T>(x:T) { let a=[x]; }",
                Some(ErrorCode::E2610),
            ),
            (
                "recursive pure",
                "fn a() { b(); } fn b() { a(); } @effects(no_panic,no_alloc) fn f() { a(); }",
                None,
            ),
            (
                "task allocation",
                "async fn child() {} @effects(no_alloc) fn f() { child(); }",
                Some(ErrorCode::E2610),
            ),
            (
                "arithmetic allocation free",
                "@effects(no_alloc) fn f(x:i64)->i64 { return x+1; }",
                None,
            ),
        ] {
            let errors: Vec<_> = check_source(source)
                .into_iter()
                .filter(|d| d.severity == Severity::Error)
                .collect();
            if let Some(code) = expected {
                assert!(errors.iter().any(|d| d.code == code), "{label}: {errors:?}");
                assert!(
                    errors.iter().all(|d| matches!(
                        d.code,
                        ErrorCode::E2610 | ErrorCode::E2611 | ErrorCode::E2612
                    )),
                    "{label}: {errors:?}"
                );
                if label.starts_with("helper") {
                    assert!(
                        errors
                            .iter()
                            .any(|d| d.notes.iter().any(|n| n.contains("f -> middle -> leaf"))),
                        "{label}: {errors:?}"
                    );
                }
            } else {
                assert!(errors.is_empty(), "{label}: {errors:?}");
            }
        }
    }

    #[test]
    fn io_contract_graph_work_is_bounded() {
        for shape in ["chain", "fanout", "cycle"] {
            for n in [8, 32, 128, 512] {
                let mut source = String::from("fn leaf() { println(1); } ");
                for i in 0..n {
                    let next = if shape == "fanout" || i == 0 {
                        "leaf".to_owned()
                    } else {
                        format!("node_{}", i - 1)
                    };
                    source.push_str(&format!(
                        "fn node_{i}() {{ {next}(); {} }} ",
                        if shape == "cycle" && i == 0 {
                            format!("node_{}();", n - 1)
                        } else {
                            String::new()
                        }
                    ));
                }
                source.push_str(&format!(
                    "@effects(no_io) fn guard() {{ node_{}(); }}",
                    n - 1
                ));
                CONTRACT_GRAPH_WORK.with(|v| v.set(0));
                PATH_WORK.with(|v| v.set(0));
                crate::semantic::effects::REACHABLE_EDGE_VISITS.with(|v| v.set(0));
                let errors = check_source(&source);
                assert_eq!(
                    errors.iter().filter(|d| d.code == ErrorCode::E2610).count(),
                    1,
                    "{shape} {n}: {errors:?}"
                );
                let graph = CONTRACT_GRAPH_WORK.with(|v| v.get());
                let paths = PATH_WORK.with(|v| v.get());
                let visits = crate::semantic::effects::REACHABLE_EDGE_VISITS.with(|v| v.get());
                assert!(graph <= 12 * (n + 4), "{shape} {n} graph={graph}");
                assert!(paths <= 30 * (n + 4), "{shape} {n} paths={paths}");
                // The shared contract solve now carries panic/allocation too.
                // Bound visits per supported bit, rather than assuming three bits.
                assert!(
                    visits <= 4 * CONTRACT_BITS.len() * (n + 4),
                    "{shape} {n} edges={visits}"
                );
                println!(
                    "io-contract shape={shape} nodes={n} graph={graph} paths={paths} edge_visits={visits}"
                );
            }
        }
    }

    #[test]
    fn io_contract_std_resource_catalog_is_complete() {
        for module in crate::stdlib_schema::STDLIB_SCHEMA {
            match module.name {
                "fs" | "net" | "env" | "time" => {}
                // Console output is syntax, tested through its overload ABI inventory.
                "io" => {
                    assert_eq!(
                        module
                            .items
                            .iter()
                            .map(|item| item.name)
                            .collect::<Vec<_>>(),
                        ["println", "print", "eprintln"]
                    );
                    continue;
                }
                // Pure value modules, or callback-driven effects classified at call sites.
                "text" | "collections" | "option" | "result" | "parallel" => continue,
                other => panic!("classify external access for new standard module {other}"),
            }
            for item in module.items {
                if !matches!(
                    item.kind,
                    crate::stdlib_schema::StdItemKind::Function { .. }
                ) {
                    continue;
                }
                let builtin =
                    crate::semantic::intrinsics::namespace_builtin(module.name, item.name)
                        .unwrap_or_else(|| panic!("unclassified {}::{}", module.name, item.name));
                assert!(
                    willow_abi::runtime_symbol(builtin.runtime)
                        .unwrap()
                        .effects()
                        .contains(RuntimeEffects::MAY_IO),
                    "{}::{}",
                    module.name,
                    item.name
                );
            }
        }
    }

    #[test]
    fn io_contract_permission_matrix() {
        let cases = [
            (
                "defer",
                "@effects(no_io) fn f() { defer println(1); }",
                Some(ErrorCode::E2610),
            ),
            (
                "synchronization",
                "@effects(no_io) fn f(c:BlockingCell<i64>) { c.get(); }",
                None,
            ),
            ("empty", "@effects(no_io) fn f() {}", None),
            (
                "arithmetic",
                "@effects(no_io) fn f(x:i64)->i64 { return x+1; }",
                None,
            ),
            (
                "allocation",
                "@effects(no_io) fn f()->String { return 3.toString(); }",
                None,
            ),
            (
                "print",
                "@effects(no_io) fn f() { print(1); }",
                Some(ErrorCode::E2610),
            ),
            (
                "println",
                "@effects(no_io) fn f() { println(1); }",
                Some(ErrorCode::E2610),
            ),
            (
                "transitive",
                "fn leaf() { println(1); } fn relay() { leaf(); } @effects(no_io) fn f() { relay(); }",
                Some(ErrorCode::E2610),
            ),
            (
                "recursive",
                "fn leaf(x:bool) { if x { leaf(false); } else { println(1); } } @effects(no_io) fn f() { leaf(true); }",
                Some(ErrorCode::E2610),
            ),
            (
                "pure helper",
                "fn leaf() {} @effects(no_io) fn f() { leaf(); }",
                None,
            ),
            (
                "unknown foreign",
                "extern rust { fn foreign(); } @effects(no_io) fn f() { foreign(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "declared foreign",
                "extern rust { @effects(no_io) fn foreign(); } @effects(no_io) fn f() { foreign(); }",
                None,
            ),
            (
                "foreign relay",
                "extern rust { fn foreign(); } fn relay() { foreign(); } @effects(no_io) fn f() { relay(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "declared relay",
                "extern rust { @effects(no_io) fn foreign(); } fn relay() { foreign(); } @effects(no_io) fn f() { relay(); }",
                None,
            ),
            (
                "unused foreign",
                "extern rust { fn foreign(); } @effects(no_io) fn f() {}",
                None,
            ),
            (
                "callback unknown",
                "@effects(no_io) fn f(cb:fn()) { cb(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "constructor",
                "class C { pub init(self) { println(1); } } @effects(no_io) fn f() { let c = new C(); }",
                Some(ErrorCode::E2610),
            ),
            (
                "method",
                "class C { pub fn out(self) { println(1); } } @effects(no_io) fn f(c:C) { c.out(); }",
                Some(ErrorCode::E2610),
            ),
            (
                "override",
                "open class B { pub open fn out(self) {} } class C extends B { pub override fn out(self) { println(1); } } @effects(no_io) fn f(b:B) { b.out(); }",
                Some(ErrorCode::E2610),
            ),
            (
                "interface",
                "interface I { fn out(self); } class C implements I { pub fn out(self) { println(1); } } @effects(no_io) fn f(i:I) { i.out(); }",
                Some(ErrorCode::E2610),
            ),
            (
                "inherited obligation",
                "open class B { @effects(no_io) pub open fn out(self) {} } class C extends B { pub override fn out(self) { println(1); } }",
                Some(ErrorCode::E2610),
            ),
            (
                "combined",
                "@effects(no_io,no_block) fn f() { println(1); }",
                Some(ErrorCode::E2610),
            ),
            (
                "async body",
                "@effects(no_io) async fn f() { println(1); }",
                Some(ErrorCode::E2610),
            ),
            (
                "async callee",
                "async fn leaf() { println(1); } @effects(no_io) async fn f() { await leaf(); }",
                Some(ErrorCode::E2610),
            ),
        ];
        for (label, source, expected) in cases {
            let errors: Vec<_> = check_source(source)
                .into_iter()
                .filter(|d| d.severity == Severity::Error)
                .collect();
            if let Some(code) = expected {
                assert!(errors.iter().any(|d| d.code == code), "{label}: {errors:?}");
                if code == ErrorCode::E2610 {
                    assert!(
                        errors.iter().any(|d| d.code == code
                            && d.labels
                                .iter()
                                .any(|label| label.message.starts_with("root operation"))),
                        "{label}: missing source witness: {errors:?}"
                    );
                }
                assert!(
                    errors.iter().all(|d| matches!(
                        d.code,
                        ErrorCode::E2610 | ErrorCode::E2611 | ErrorCode::E2612
                    )),
                    "{label}: {errors:?}"
                );
            } else {
                assert!(errors.is_empty(), "{label}: {errors:?}");
            }
        }
    }

    #[test]
    fn effect_contract_proven_wait_dominates_uncertain_value() {
        let errors = check_source(
            "fn wait() { println(1); } @effects(no_wait) fn guard() { let f = wait; f(); println(2); }",
        );
        assert!(
            errors.iter().any(|d| d.code == ErrorCode::E2610),
            "{errors:?}"
        );
        assert!(
            !errors.iter().any(|d| d.code == ErrorCode::E2612),
            "{errors:?}"
        );
        let errors = check_source(
            "async fn guard() { let m = Mutex::new(0); lock m as value { println(1); } }",
        );
        assert!(
            errors.iter().all(|d| d.severity != Severity::Error),
            "{errors:?}"
        );
    }

    #[test]
    fn effect_contract_obligation_and_arity_work_bounds() {
        for n in [8, 32, 128] {
            for shape in ["chain", "fanout"] {
                let mut source =
                    String::from("open class C0 { @effects(no_wait) pub open fn f(self) {} }\n");
                for i in 1..n {
                    let parent = if shape == "chain" { i - 1 } else { 0 };
                    source.push_str(&format!("open class C{i} extends C{parent} {{ @effects(no_wait) pub open override fn f(self) {{}} }}\n"));
                }
                OBLIGATION_WORK.with(|v| v.set(0));
                let errors = check_source(&source);
                assert!(
                    errors.iter().all(|d| d.severity != Severity::Error),
                    "{errors:?}"
                );
                let work = OBLIGATION_WORK.with(|v| v.get());
                assert_eq!(work, 2 * n - 1, "{shape} {n}");
                println!("contract-obligations shape={shape} classes={n} visits={work}");
            }
            let mut source = String::from("fn wait(c: BlockingCell<i64>) { c.get(); }\n");
            for i in 0..n {
                source.push_str(&format!(
                    "fn relay{i}(c: BlockingCell<i64>) {{ let f = wait; f(c); }}\n"
                ));
            }
            CONTRACT_GRAPH_WORK.with(|v| v.set(0));
            let errors = check_source(&source);
            assert!(
                errors.iter().all(|d| d.severity != Severity::Error),
                "{errors:?}"
            );
            let work = CONTRACT_GRAPH_WORK.with(|v| v.get());
            assert!(work <= 5 * n + 10, "{n}: {work}");
            println!("contract-arity callers={n} values={n} visits={work}");
        }
    }

    #[test]
    fn effect_contract_polymorphic_perspectives() {
        let cases = [
            (
                "trusted blocking ABI",
                "@effects(no_wait) fn guard() { println(1); }",
                Some(ErrorCode::E2610),
            ),
            (
                "printing does not suspend",
                "@effects(no_suspend) fn guard() { print(1); }",
                None,
            ),
            (
                "blocking ABI through helper",
                "fn helper() { print(1); } @effects(no_block) fn guard() { helper(); }",
                Some(ErrorCode::E2610),
            ),
            (
                "unannotated interface implementation waits",
                "interface I { @effects(no_wait) fn f(self, c: BlockingCell<i64>); } class C implements I { pub fn f(self, c: BlockingCell<i64>) { c.get(); } }",
                Some(ErrorCode::E2610),
            ),
            (
                "unannotated interface implementation pure",
                "interface I { @effects(no_wait) fn f(self); } class C implements I { pub fn f(self) {} }",
                None,
            ),
            (
                "override weakens",
                "open class B { @effects(no_wait) pub open fn f(self) {} } class C extends B { @effects(no_suspend) pub override fn f(self) {} }",
                Some(ErrorCode::E2611),
            ),
            (
                "override strengthens",
                "open class B { @effects(no_suspend) pub open fn f(self) {} } class C extends B { @effects(no_wait) pub override fn f(self) {} }",
                None,
            ),
            (
                "override preserves",
                "open class B { @effects(no_wait) pub open fn f(self) {} } class C extends B { @effects(no_wait) pub override fn f(self) {} }",
                None,
            ),
            (
                "omitted annotation inherits",
                "open class B { @effects(no_wait) pub open fn f(self, c: BlockingCell<i64>) {} } class C extends B { pub override fn f(self, c: BlockingCell<i64>) { c.get(); } }",
                Some(ErrorCode::E2610),
            ),
            (
                "omitted annotation pure",
                "open class B { @effects(no_wait) pub open fn f(self) {} } class C extends B { pub override fn f(self) {} }",
                None,
            ),
            (
                "combined prohibitions preserve",
                "open class B { @effects(no_wait) pub open fn f(self) {} } class C extends B { @effects(no_suspend, no_block) pub override fn f(self) {} }",
                None,
            ),
            (
                "interface weakening",
                "interface I { @effects(no_wait) fn f(self); } class C implements I { @effects(no_block) pub fn f(self) {} }",
                Some(ErrorCode::E2611),
            ),
            (
                "deep inherited implementation",
                "interface I { @effects(no_wait) fn f(self, c: BlockingCell<i64>); } open class B { pub fn f(self, c: BlockingCell<i64>) { c.get(); } } class C extends B implements I {}",
                Some(ErrorCode::E2610),
            ),
            (
                "virtual mixture",
                "open class B { pub open fn f(self, c: BlockingCell<i64>) {} } class C extends B { pub override fn f(self, c: BlockingCell<i64>) { c.get(); } } @effects(no_wait) fn guard(b: B, c: BlockingCell<i64>) { b.f(c); }",
                Some(ErrorCode::E2610),
            ),
            (
                "virtual all pure",
                "open class B { pub open fn f(self) {} } class C extends B { pub override fn f(self) {} } @effects(no_wait) fn guard(b: B) { b.f(); }",
                None,
            ),
            (
                "interface mixture",
                "interface I { fn f(self, c: BlockingCell<i64>); } class A implements I { pub fn f(self, c: BlockingCell<i64>) {} } class B implements I { pub fn f(self, c: BlockingCell<i64>) { c.get(); } } @effects(no_wait) fn guard(i: I, c: BlockingCell<i64>) { i.f(c); }",
                Some(ErrorCode::E2610),
            ),
            (
                "unknown interface target",
                "interface I { fn f(self); } @effects(no_wait) fn guard(i: I) { i.f(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "unknown function parameter",
                "@effects(no_wait) fn guard(f: fn()) { f(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "unknown closure parameter",
                "@effects(no_wait) fn guard(f: closure()) { f(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "unknown helper callback",
                "fn relay(f: fn()) { f(); } @effects(no_wait) fn guard(f: fn()) { relay(f); }",
                Some(ErrorCode::E2612),
            ),
            (
                "waiting named function value",
                "fn wait(c: BlockingCell<i64>) { c.get(); } @effects(no_wait) fn guard(c: BlockingCell<i64>) { let f = wait; f(c); }",
                Some(ErrorCode::E2612),
            ),
            (
                "waiting value through argument",
                "fn wait(c: BlockingCell<i64>) { c.get(); } fn relay(f: fn(BlockingCell<i64>), c: BlockingCell<i64>) { f(c); } @effects(no_wait) fn guard(c: BlockingCell<i64>) { relay(wait, c); }",
                Some(ErrorCode::E2612),
            ),
            (
                "waiting captured closure",
                "@effects(no_wait) fn guard(c: BlockingCell<i64>) { let f = || { c.get(); }; f(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "closure captures waiting value",
                "fn wait(c: BlockingCell<i64>) { c.get(); } @effects(no_wait) fn guard(c: BlockingCell<i64>) { let f = wait; let g = || { f(c); }; g(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "unverified ffi",
                "extern rust { fn foreign(); } @effects(no_wait) fn guard() { foreign(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "ffi cannot prove no suspend",
                "extern rust { fn foreign(); } @effects(no_suspend) fn guard() { foreign(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "ffi transitive",
                "extern rust { fn foreign(); } fn relay() { foreign(); } @effects(no_block) fn guard() { relay(); }",
                Some(ErrorCode::E2612),
            ),
            (
                "trusted arithmetic",
                "@effects(no_wait) fn guard(n: i64) -> i64 { return n + 1; }",
                None,
            ),
            (
                "async creation",
                "async fn wait() { await sleep(1); } @effects(no_wait) fn guard() { wait(); }",
                None,
            ),
            (
                "unannotated ffi unaffected",
                "extern rust { fn foreign(); } fn guard() { foreign(); }",
                None,
            ),
        ];
        for (label, source, expected) in cases {
            let errors = check_source(source);
            let codes: Vec<_> = errors
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .map(|d| d.code)
                .collect();
            if let Some(code) = expected {
                assert!(codes.contains(&code), "{label}: {errors:?}");
                assert!(codes.iter().all(|c| [ErrorCode::E2610, ErrorCode::E2611, ErrorCode::E2612].contains(c)), "{label}: {errors:?}");
                if code == ErrorCode::E2612 {
                    assert!(
                        !codes.contains(&ErrorCode::E2610),
                        "{label}: uncertainty was reported as proof: {errors:?}"
                    );
                }
            } else {
                assert!(codes.is_empty(), "{label}: {errors:?}");
            }
        }
    }

    #[test]
    fn effect_contract_generic_default_check_count_is_linear() {
        use super::super::check_decls::DEFAULT_BODY_CHECKS;
        for count in [8, 32, 128] {
            let source: String = (0..count).map(|i| format!(
                "interface I{i}<T> {{ fn leaf(self, c: BlockingCell<i64>) {{ c.get(); }} fn middle(self, c: BlockingCell<i64>) {{ self.leaf(c); }} @effects(no_wait) fn guarded(self, c: BlockingCell<i64>) {{ self.middle(c); }} }}\n"
            )).collect();
            DEFAULT_BODY_CHECKS.with(|checks| checks.set(0));
            let errors = check_source(&source);
            assert_eq!(
                errors.iter().filter(|e| e.code == ErrorCode::E2610).count(),
                count,
                "{errors:?}"
            );
            assert!(
                errors
                    .iter()
                    .all(|e| e.severity != Severity::Error || e.code == ErrorCode::E2610),
                "{errors:?}"
            );
            let checks = DEFAULT_BODY_CHECKS.with(|checks| checks.get());
            assert_eq!(checks, 3 * count);
            println!(
                "generic-default-checks interfaces={count} bodies={} checks={checks}",
                3 * count
            );
        }
    }

    #[test]
    fn effect_contract_unused_generic_interface_defaults() {
        let cases = [
            (
                "interface I<T> { @effects(no_wait) fn f(self, c: BlockingCell<i64>) { c.get(); } }",
                Some("BlockingCell.get"),
            ),
            (
                "interface I<T> { @effects(no_block) fn f(self, c: BlockingCell<i64>) { c.get(); } }",
                Some("BlockingCell.get"),
            ),
            (
                "interface I<T> { @effects(no_suspend) fn f(self, c: Channel<i64>) { c.recv(); } }",
                Some("Channel.recv"),
            ),
            (
                "fn leaf(c: BlockingCell<i64>) { c.get(); } fn middle(c: BlockingCell<i64>) { leaf(c); } interface I<T> { @effects(no_wait) fn f(self, c: BlockingCell<i64>) { middle(c); } }",
                Some("BlockingCell.get"),
            ),
            (
                "interface I<T> { fn helper(self, c: BlockingCell<i64>) { c.get(); } @effects(no_wait) fn f(self, c: BlockingCell<i64>) { self.helper(c); } }",
                Some("BlockingCell.get"),
            ),
            (
                "interface I<T> { @effects(no_wait) fn f(self, value: T) -> T { return value; } }",
                None,
            ),
            (
                "interface I<T> { @effects(no_wait) fn f(self, value: T) -> T; }",
                None,
            ),
            (
                "interface I<T> { fn f(self, c: BlockingCell<i64>) { c.get(); } }",
                None,
            ),
        ];
        for (source, root) in cases {
            let errors = check_source(source);
            assert!(
                errors
                    .iter()
                    .all(|d| d.severity != Severity::Error || d.code == ErrorCode::E2610),
                "{source}: {errors:?}"
            );
            let contract_errors: Vec<_> = errors
                .iter()
                .filter(|d| d.code == ErrorCode::E2610)
                .collect();
            assert_eq!(
                contract_errors.len(),
                usize::from(root.is_some()),
                "{source}: {errors:?}"
            );
            if let Some(root) = root {
                assert!(
                    contract_errors[0]
                        .labels
                        .iter()
                        .any(|l| l.message == format!("root operation `{root}`")),
                    "{errors:?}"
                );
            }
        }
    }

    #[test]
    fn effect_contract_mixed_direct_and_transitive_wait_bits() {
        for reverse in [false, true] {
            for (contract, root) in [("no_suspend", "await"), ("no_block", "BlockingCell.get")] {
                let body = if reverse {
                    "await sleep(1); c.get();"
                } else {
                    "c.get(); await sleep(1);"
                };
                let source =
                    format!("@effects({contract}) async fn f(c: BlockingCell<i64>) {{ {body} }}");
                let errors = check_source(&source);
                let error = errors
                    .iter()
                    .find(|d| d.code == ErrorCode::E2610)
                    .expect("mixed async body must violate either contract");
                assert!(
                    error
                        .labels
                        .iter()
                        .any(|l| l.message == format!("root operation `{root}`")),
                    "{error:?}"
                );
                let body = if reverse {
                    "ch.recv(); c.get();"
                } else {
                    "c.get(); ch.recv();"
                };
                let root = if contract == "no_suspend" {
                    "Channel.recv"
                } else {
                    root
                };
                for transitive in [false, true] {
                    let source = if transitive {
                        format!(
                            "fn leaf(c: BlockingCell<i64>, ch: Channel<i64>) {{ {body} }} fn middle(c: BlockingCell<i64>, ch: Channel<i64>) {{ leaf(c, ch); }} @effects({contract}) fn f(c: BlockingCell<i64>, ch: Channel<i64>) {{ middle(c, ch); }}"
                        )
                    } else {
                        format!(
                            "@effects({contract}) fn f(c: BlockingCell<i64>, ch: Channel<i64>) {{ {body} }}"
                        )
                    };
                    let errors = check_source(&source);
                    let error = errors
                        .iter()
                        .find(|d| d.code == ErrorCode::E2610)
                        .expect("mixed sync body must violate either contract");
                    assert!(
                        error
                            .labels
                            .iter()
                            .any(|l| l.message == format!("root operation `{root}`")),
                        "{error:?}"
                    );
                    if transitive {
                        assert!(
                            error
                                .notes
                                .iter()
                                .any(|n| n.contains("f -> middle -> leaf")),
                            "{error:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn effect_contract_perspectives() {
        let cases = [
            (
                "pure method",
                "class C { @effects(no_wait) pub fn f(self) -> i64 { return 1; } }",
                false,
            ),
            (
                "async method body",
                "class C { @effects(no_wait) pub async fn f(self) { await sleep(1); } }",
                true,
            ),
            (
                "async method creation",
                "class C { pub async fn f(self) { await sleep(1); } } @effects(no_wait) fn task(c: C) { c.f(); }",
                false,
            ),
            (
                "pure interface default",
                "interface I { @effects(no_wait) fn f(self) -> i64 { return 1; } }",
                false,
            ),
            (
                "generic helper wait",
                "fn a<T>(x: T, c: BlockingCell<i64>) { c.get(); } @effects(no_wait) fn f<T>(x: T, c: BlockingCell<i64>) { a(x, c); }",
                true,
            ),
            (
                "pure",
                "@effects(no_wait) fn f() -> i64 { return 1; }",
                false,
            ),
            (
                "allocation",
                "@effects(no_wait) fn f() { let a = [1, 2]; }",
                false,
            ),
            (
                "await",
                "@effects(no_wait) async fn f() { await sleep(1); }",
                true,
            ),
            (
                "suspend",
                "@effects(no_suspend) async fn f() { await sleep(1); }",
                true,
            ),
            (
                "block allows await",
                "@effects(no_block) async fn f() { await sleep(1); }",
                false,
            ),
            (
                "channel",
                "@effects(no_wait) fn f(c: Channel<i64>) { c.recv(); }",
                true,
            ),
            (
                "channel suspend",
                "@effects(no_suspend) fn f(c: Channel<i64>) { c.recv(); }",
                true,
            ),
            (
                "cell",
                "@effects(no_wait) fn f(c: BlockingCell<i64>) { c.get(); }",
                true,
            ),
            (
                "block cell",
                "@effects(no_block) fn f(c: BlockingCell<i64>) { c.get(); }",
                true,
            ),
            (
                "suspend allows cell",
                "@effects(no_suspend) fn f(c: BlockingCell<i64>) { c.get(); }",
                false,
            ),
            (
                "task creation",
                "async fn child() { await sleep(1); } @effects(no_wait) fn f() { child(); }",
                false,
            ),
            (
                "async body",
                "@effects(no_wait) async fn child() { await sleep(1); } fn f() { child(); }",
                true,
            ),
            (
                "two helpers",
                "fn a(c: BlockingCell<i64>) { c.get(); } fn b(c: BlockingCell<i64>) { a(c); } @effects(no_wait) fn f(c: BlockingCell<i64>) { b(c); }",
                true,
            ),
            (
                "recursive wait",
                "fn a(c: BlockingCell<i64>) { b(c); c.get(); } fn b(c: BlockingCell<i64>) { a(c); } @effects(no_wait) fn f(c: BlockingCell<i64>) { b(c); }",
                true,
            ),
            (
                "recursive pure",
                "fn a() { b(); } fn b() { a(); } @effects(no_wait) fn f() { a(); }",
                false,
            ),
            (
                "class method",
                "class C { @effects(no_wait) pub fn f(self, c: BlockingCell<i64>) { c.get(); } }",
                true,
            ),
            (
                "static method",
                "class C { @effects(no_wait) pub static fn f(c: BlockingCell<i64>) { c.get(); } }",
                true,
            ),
            (
                "interface signature",
                "interface I { @effects(no_wait) fn f(self); }",
                false,
            ),
            (
                "interface default",
                "interface I { @effects(no_wait) fn f(self, c: BlockingCell<i64>) { c.get(); } }",
                true,
            ),
            (
                "multiple contracts",
                "@effects(no_block, no_suspend) fn f(c: BlockingCell<i64>) { c.get(); }",
                true,
            ),
            (
                "repeated attributes",
                "@effects(no_suspend) @effects(no_block) fn f(c: BlockingCell<i64>) { c.get(); }",
                true,
            ),
            (
                "unannotated",
                "fn f(c: BlockingCell<i64>) { c.get(); }",
                false,
            ),
            (
                "generic pure",
                "@effects(no_wait) fn f<T>(x: T) -> T { return x; }",
                false,
            ),
            (
                "generic wait",
                "@effects(no_wait) fn f<T>(x: T, c: BlockingCell<i64>) { c.get(); }",
                true,
            ),
        ];
        for (label, source, violation) in cases {
            let diagnostics = check_source(source);
            assert_eq!(
                diagnostics.iter().any(|d| d.code == ErrorCode::E2610),
                violation,
                "{label}: {diagnostics:?}"
            );
            assert!(
                diagnostics
                    .iter()
                    .all(|d| d.severity != Severity::Error || d.code == ErrorCode::E2610),
                "{label}: {diagnostics:?}"
            );
        }
    }

    #[test]
    fn effect_contract_chain_is_deterministic_and_located() {
        let source = "fn a(c: BlockingCell<i64>) { c.get(); } fn b(c: BlockingCell<i64>) { a(c); } @effects(no_wait) fn f(c: BlockingCell<i64>) { b(c); }";
        let first = check_source(source);
        let diagnostic = first.iter().find(|d| d.code == ErrorCode::E2610).unwrap();
        assert!(
            diagnostic
                .notes
                .iter()
                .any(|n| n.contains("compiler-proven")
                    && n.contains("f -> b -> a -> BlockingCell.get")),
            "{diagnostic:?}"
        );
        assert_eq!(diagnostic.labels.len(), 4);
        for _ in 0..8 {
            assert_eq!(format!("{first:?}"), format!("{:?}", check_source(source)));
        }
    }

    #[test]
    fn effect_contract_declaration_storage_roundtrips() {
        let source = "@effects(no_wait) pub fn f<T>(x: T) -> T { return x; } class C { @effects(no_block) pub fn m(self) {} } interface I { @effects(no_suspend) fn m(self); }";
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let decoded: Program =
            serde_json::from_slice(&serde_json::to_vec(&program).unwrap()).unwrap();
        let contracts: Vec<_> = decoded
            .items
            .iter()
            .map(|item| match item {
                Item::Function(f) => &f.effect_contracts,
                Item::Class(c) => &c.methods[0].effect_contracts,
                Item::Interface(i) => &i.methods[0].effect_contracts,
                _ => unreachable!(),
            })
            .collect();
        for (contracts, expected) in contracts.iter().zip([
            EffectContractKind::NoWait,
            EffectContractKind::NoBlock,
            EffectContractKind::NoSuspend,
        ]) {
            assert_eq!(contracts.len(), 1);
            assert_eq!(contracts[0].kind, expected);
            assert!(
                source[contracts[0].span.start..contracts[0].span.end].contains(expected.name())
            );
        }
    }

    #[test]
    fn effect_contract_invalid_syntax_is_rejected() {
        for source in [
            "@effects(unknown) fn f() {}",
            "@other(no_wait) fn f() {}",
            "@effects() fn f() {}",
            "@effects(no_wait) class C {}",
            "class C { @effects(no_wait) x: i64; }",
            "fn f(x: @effects(no_wait) fn()) {}",
        ] {
            let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
            let (_, errors) = crate::parser::Parser::new(tokens).parse();
            assert!(!errors.is_empty(), "{source}");
        }
    }
    #[test]
    fn effect_contract_path_work_scales_with_nodes_and_edges() {
        use crate::compiler_db::effects::{LockEffectCause, LockEffectKind, LockEffectWitness};
        use crate::semantic::{call_graph::CallSites, effects::EffectProblem};
        for shape in ["chain", "fanout", "cycle", "many-roots"] {
            for n in [32, 128, 512, 2048] {
                let ids: Vec<_> = (0..n)
                    .map(|i| FunctionId::free(format!("f{i:05}")))
                    .collect();
                let mut graph = CallGraph::default();
                let mut problem = EffectProblem::new();
                for i in 0..n {
                    problem = problem.body(ids[i]);
                    let target = match shape {
                        "fanout" => (i > 0).then_some(ids[0]),
                        "cycle" => Some(ids[(i + 1) % n]),
                        _ => (i > 0).then(|| ids[i - 1]),
                    };
                    graph.merge(
                        ids[i],
                        CallSites {
                            targets: target.into_iter().collect(),
                            ..Default::default()
                        },
                    );
                    if i == 0 || shape == "many-roots" {
                        problem = problem.seed(
                            ids[i],
                            RuntimeEffects::MAY_BLOCK,
                            Some(EffectWitness::Lock(LockEffectWitness {
                                owner: ids[i],
                                cause: LockEffectCause {
                                    is_conservative: false,
                                    span: Span::dummy(),
                                    operation: "BlockingCell.get".into(),
                                    kind: LockEffectKind::Block,
                                    lock: None,
                                },
                            })),
                        );
                    }
                }
                let facts = problem.solve(&graph);
                PATH_WORK.with(|count| count.set(0));
                let paths =
                    witness_paths(&graph, &facts, &HashMap::new(), RuntimeEffects::MAY_BLOCK);
                let work = PATH_WORK.with(|count| count.get());
                let edges = graph.iter().map(|(_, s)| s.targets.len()).sum::<usize>();
                assert!(work <= 2 * n + 2 * edges, "{shape}: {n} {edges} {work}");
                assert_eq!(paths.len(), n - 1);
                println!(
                    "effect-path-work shape={shape} nodes={n} edges={edges} visits={work} next_hops={}",
                    paths.len()
                );
            }
        }
    }
}
