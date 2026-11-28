//! E2604 for waits reached through function values; see
//! [`crate::semantic::indirect_lock`] for the address-taken model.

use std::collections::HashMap;

use crate::compiler_db::effects::EffectWitness;
use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::{CallArg, Expr, Type};
use crate::semantic::call_graph::CallGraph;
use crate::semantic::effects::EffectFacts;
use crate::semantic::ids::FunctionId;
use crate::semantic::indirect_lock::{
    FunctionValue, IndirectCall, ValueMasks, arity_bit, close_reach,
};

use super::TypeChecker;

impl TypeChecker {
    pub(super) fn refine_function_flow(
        &mut self,
        program: &crate::parser::ast::Program,
        graph: &mut CallGraph,
    ) {
        let cross_owner = crate::semantic::function_flow::resolve(
            program,
            &self.expr_types,
            &self.analysis_calls,
            &mut self.indirect_lock,
        );
        let mut complete: HashMap<FunctionId, bool> = HashMap::new();
        for call in &self.indirect_lock.calls {
            *complete.entry(call.caller).or_insert(true) &= call.targets.is_some();
        }
        let mut refined = CallGraph::default();
        refined.flow_sensitive = cross_owner;
        for (&id, sites) in graph.iter() {
            let mut sites = sites.clone();
            if complete.get(&id) == Some(&true) {
                sites.indirect_spans.clear();
                sites
                    .unsupported_initialization
                    .remove(&crate::semantic::call_graph::UnsupportedInitialization::IndirectCall);
                sites.has_unknown =
                    sites.unknown_span.is_some() || !sites.unsupported_initialization.is_empty();
            }
            refined.merge(id, sites);
        }
        for call in &self.indirect_lock.calls {
            refined.value_calls.insert(
                call.span,
                call.targets.clone().unwrap_or_else(|| vec![call.bound()]),
            );
            if let Some(targets) = &call.targets {
                refined.merge(
                    call.caller,
                    crate::semantic::call_graph::CallSites {
                        targets: targets.iter().copied().collect(),
                        call_spans: targets.iter().map(|&t| (t, call.span)).collect(),
                        ..Default::default()
                    },
                );
            }
        }
        for (id, span) in crate::semantic::function_flow::attach(&self.indirect_lock, &mut refined)
        {
            self.effect_inputs.direct.entry(id).or_default().insert(
                crate::compiler_db::effects::LockEffectCause {
                    is_conservative: true,
                    span,
                    operation: "opaque function value".into(),
                    kind: crate::compiler_db::effects::LockEffectKind::SuspendOrBlock,
                    lock: None,
                },
            );
        }
        *graph = refined;
    }
    /// A named function or a lambda of type `ty` became a value that
    /// indirect calls can reach.
    pub(super) fn record_function_value(
        &mut self,
        target: Option<FunctionId>,
        arity: usize,
        span: Span,
        ty: &Type,
    ) {
        let key = self.symbols.function_type_key(ty);
        self.indirect_lock.values.push(FunctionValue {
            target,
            bit: arity_bit(arity),
            span,
            key,
        });
    }

    /// A call through a function value of type `ty` with `arity` arguments.
    fn record_indirect_call(&mut self, arity: usize, span: Span, ty: Option<&Type>) {
        let bit = arity_bit(arity);
        if let Some(caller) = self.local.current_effect_callable {
            let key = ty.and_then(|ty| self.symbols.function_type_key(ty));
            self.indirect_lock.calls.push(IndirectCall {
                caller,
                bit,
                key,
                span,
                targets: None,
                unknown_reasons: Vec::new(),
            });
            self.resolved_calls
                .entry(caller)
                .or_default()
                .indirect_spans
                .entry(bit)
                .or_insert(span);
        }
        if self.local.lock_depth > 0 {
            self.indirect_lock.lock_sites.push((span, bit));
        }
    }

    /// Record the indirect calls of one call expression whose resolved
    /// targets are `targets`: a call through a local function value, and each
    /// function value handed to a callee without a Willow body (a builtin such
    /// as `map`), which may call it before returning. A Willow callee's own
    /// indirect calls are reached through the call graph instead.
    pub(super) fn record_indirect_uses(&mut self, expr: &Expr, targets: &[FunctionId]) {
        // Constructing a container or object stores its callback; it does not
        // invoke it. Explicit constructors still reach their checked bodies.
        if self.enum_variant_resolutions.contains_key(&expr.id()) {
            return;
        }
        if matches!(expr, Expr::New(_))
            && self
                .expr_types
                .get(&expr.id())
                .and_then(|ty| match ty {
                    Type::Named(name) => self.symbols.lookup_class(name),
                    _ => None,
                })
                .is_some_and(|class| class.constructor.is_none())
        {
            return;
        }
        let args: &[CallArg] = match expr {
            Expr::Call(call) => {
                if let Some(var) = self.symbols.lookup_var(&call.callee) {
                    let ty = var.ty.clone();
                    self.record_indirect_call(call.args.len(), call.span, Some(&ty));
                }
                &call.args
            }
            Expr::MethodCall(call) => &call.args,
            Expr::StaticCall(call) => &call.args,
            Expr::New(call) => &call.args,
            _ => return,
        };
        let opaque = targets
            .iter()
            .all(|target| self.lock_callee_is_async(target).is_none());
        if !opaque {
            return;
        }
        for arg in args {
            if let Some(ty @ (Type::Fn(params, _) | Type::Closure(params, _))) =
                self.expr_types.get(&arg.expr.id())
            {
                let (arity, ty) = (params.len(), ty.clone());
                self.record_indirect_call(arity, arg.span, Some(&ty));
            }
        }
    }

    /// Whether the function value `value` may wait before returning, given
    /// the unit's facts. `None` means it waits exactly when one of the arities
    /// it reaches indirectly does, which the caller decides with the session
    /// closure. An `async` function value never waits in its caller: calling
    /// it only creates a Task.
    fn value_waits(
        &self,
        value: &FunctionValue,
        facts: &EffectFacts<EffectWitness>,
    ) -> Option<bool> {
        let Some(target) = &value.target else {
            return Some(true);
        };
        let local_lambda =
            !self.effect_index.callables.contains_key(target) && facts.get(target).is_some();
        if !local_lambda && self.lock_callee_is_async(target) != Some(false) {
            // An async callable, or a name without a Willow body whose waits
            // are diagnosed at its direct call sites only.
            return Some(false);
        }
        self.lock_wait_cause(target, value.span, facts)
            .is_some()
            .then_some(true)
    }

    /// Report lock-held calls that may run a waiting function value, after the
    /// unit's effects are solved. Runs in O(values + calls + lock sites +
    /// 64 * graph edges): reach is a bitmask fixpoint and the waiting set is
    /// closed over at most 64 arity bits.
    #[allow(
        clippy::too_many_lines,
        reason = "one session snapshot joins bounded arity/type reach, local witnesses and earlier-unit locks before publishing the result"
    )]
    pub(super) fn report_indirect_lock_effects(
        &mut self,
        graph: &CallGraph,
        facts: &EffectFacts<EffectWitness>,
    ) {
        let mut inputs = std::mem::take(&mut self.indirect_lock);
        let locks: std::collections::HashSet<_> = inputs.lock_sites.iter().map(|s| s.0).collect();
        let mut exact = std::collections::HashSet::new();
        let witnesses: HashMap<_, _> = inputs
            .values
            .iter()
            .filter_map(|v| v.target.map(|t| (t, v.span)))
            .collect();
        for call in &inputs.calls {
            let fallback;
            let targets = match &call.targets {
                Some(targets) => targets,
                None => {
                    fallback = vec![call.bound()];
                    &fallback
                }
            };
            if call.targets.is_some() {
                exact.insert(call.span);
            }
            if !locks.contains(&call.span) {
                continue;
            }
            for target in targets {
                let Some(cause) = self.lock_wait_cause(target, call.span, facts) else {
                    continue;
                };
                let witness = witnesses.get(target).copied().unwrap_or(cause.span);
                self.push(Diagnostic::new(Severity::Error,ErrorCode::E2604,
                    "cannot call a waiting function value while holding a Willow lock")
                    .with_label(Label::primary(call.span,"this function value can suspend the task or block the scheduler worker before returning"))
                    .with_label(Label::secondary(witness,"this function value may wait and has a matching parameter count"))
                    .with_help("call the function value outside the critical section"));
                break;
            }
        }
        inputs.calls.retain(|call| call.targets.is_none());
        let unresolved_sites: std::collections::HashSet<_> = inputs
            .calls
            .iter()
            .filter(|call| call.targets.is_none())
            .map(|call| call.span)
            .collect();
        let deferred_locks: Vec<_> = inputs
            .lock_sites
            .iter()
            .filter(|(span, _)| unresolved_sites.contains(span))
            .copied()
            .collect();
        inputs.lock_sites.retain(|(span, _)| !exact.contains(span));
        let session = self.effect_queries.clone();
        let imports = std::sync::Arc::clone(&self.effect_index.imports);
        let external_reach = |target: &FunctionId| match &session {
            Some((queries, unit)) => queries.indirect_reach(*unit, target, &imports),
            None => 0,
        };

        use crate::semantic::indirect_lock::{TypeReach, TypeValues, close_types};
        let external_types = |target: &FunctionId| {
            session
                .as_ref()
                .and_then(|(queries, consumer)| {
                    let (unit, id) = queries.external_target(*consumer, target, &imports)?;
                    queries
                        .indirect_lock
                        .borrow()
                        .types
                        .get(&unit)?
                        .get(&id)
                        .cloned()
                })
                .unwrap_or_default()
        };
        let mut type_seed: HashMap<FunctionId, TypeReach> = HashMap::new();
        for call in &inputs.calls {
            type_seed
                .entry(call.caller)
                .or_default()
                .add(call.bit, call.key.as_deref());
        }
        // Each callable's own indirect calls, plus those its synchronous
        // callees in earlier units can make.
        let mut seed: HashMap<FunctionId, u64> = HashMap::new();
        for call in &inputs.calls {
            *seed.entry(call.caller).or_default() |= call.bit;
        }
        let mut edges = Vec::new();
        for (caller, sites) in graph.iter() {
            for target in &sites.targets {
                if graph.get(target).is_some() {
                    if self.effect_index.callables.get(target) != Some(&true) {
                        edges.push((*caller, *target));
                    }
                } else if self.lock_callee_is_async(target) == Some(false) {
                    let bits = external_reach(target);
                    if bits != 0 {
                        *seed.entry(*caller).or_default() |= bits;
                        type_seed
                            .entry(*caller)
                            .or_default()
                            .merge(&external_types(target));
                    }
                }
            }
        }
        let types = close_types(type_seed, edges.iter().copied());
        let types_of = |target: &FunctionId| {
            types
                .get(target)
                .cloned()
                .unwrap_or_else(|| external_types(target))
        };
        let reach = close_reach(seed, edges.into_iter());
        let reach_of = |target: &FunctionId| {
            reach
                .get(target)
                .copied()
                .unwrap_or_else(|| external_reach(target))
        };

        let mut lock_types: HashMap<Span, TypeReach> = HashMap::new();
        for call in &inputs.calls {
            if locks.contains(&call.span) {
                lock_types
                    .entry(call.span)
                    .or_default()
                    .add(call.bit, call.key.as_deref());
            }
        }
        for site in &self.effect_inputs.sites {
            lock_types
                .entry(site.span)
                .or_default()
                .merge(&types_of(&site.callee));
        }
        let mut own_type_locks = TypeReach::default();
        for keys in lock_types.values() {
            own_type_locks.merge(keys);
        }

        // This unit's function values: waiting ones and conditional ones.
        let mut own = ValueMasks::default();
        let mut own_types = TypeValues::default();
        let mut waiting_values = Vec::new();
        for value in &inputs.values {
            match self.value_waits(value, facts) {
                Some(true) => {
                    own.waiting |= value.bit;
                    own_types.waiting.add(value.bit, value.key.as_deref());
                    waiting_values.push((value, u64::MAX));
                }
                Some(false) => {}
                None => {
                    let target_reach = value.target.as_ref().map_or(0, &reach_of);
                    own.add_conditional(value.bit, target_reach);
                    if let Some(target) = &value.target {
                        own_types.conditional(value.bit, value.key.as_deref(), &types_of(target));
                    }
                    if target_reach != 0 {
                        waiting_values.push((value, target_reach));
                    }
                }
            }
        }

        // Lock-held indirect calls: direct ones, and lock-held calls to
        // helpers that reach indirect calls.
        let mut lock_sites = inputs.lock_sites.clone();
        let mut deferred_locks = deferred_locks;
        for site in &self.effect_inputs.sites {
            deferred_locks.push((site.span, reach_of(&site.callee)));
            // A local helper can itself reach an unresolved storage read whose
            // waiting candidates originate in an earlier module.
            let bits = reach_of(&site.callee);
            if bits != 0 {
                lock_sites.push((site.span, bits));
            }
        }
        let own_locks = lock_sites
            .iter()
            .chain(&deferred_locks)
            .fold(0, |locks, (_, bits)| locks | bits);

        let (before, earlier_locks, earlier_types, mut value_types) = match &session {
            Some((queries, _)) => {
                let session = queries.indirect_lock.borrow();
                (
                    session.values.clone(),
                    session.locks,
                    session.type_locks.clone(),
                    session.type_values.clone(),
                )
            }
            None => (
                ValueMasks::default(),
                0,
                TypeReach::default(),
                TypeValues::default(),
            ),
        };
        value_types.merge(&own_types);
        let value_types = value_types.closure();
        let mut after = before;
        after.merge(&own);
        let waiting = after.closure();
        let mut own_witnesses = crate::semantic::indirect_lock::TypeWitnesses::default();
        for (value, reaches) in &waiting_values {
            if *reaches == u64::MAX
                || value.target.is_some_and(|target| {
                    types_of(&target).intersects(&value_types, reaches & waiting)
                })
            {
                own_witnesses.add(value.bit, value.key.as_deref(), value.span);
            }
        }

        let mut reported: std::collections::HashSet<Span> = self
            .errors
            .iter()
            .filter(|diagnostic| diagnostic.code == ErrorCode::E2604)
            .flat_map(|diagnostic| diagnostic.labels.first().map(|label| label.span))
            .collect();
        lock_sites.sort_by_key(|(span, _)| (span.file_id, span.start, span.end));
        for (span, bits) in lock_sites {
            let hit = bits & waiting;
            if hit == 0
                || !lock_types
                    .get(&span)
                    .is_some_and(|keys| keys.intersects(&value_types, hit))
                || !reported.insert(span)
            {
                continue;
            }
            let mut diagnostic = Diagnostic::new(
                Severity::Error,
                ErrorCode::E2604,
                "cannot call a waiting function value while holding a Willow lock",
            )
            .with_label(Label::primary(
                span,
                "this may call a function value that can suspend the task or block the scheduler worker before returning",
            ))
            .with_help("call the function value outside the critical section");
            let witness = lock_types.get(&span).and_then(|keys| {
                own_witnesses.matching(keys, hit).or_else(|| {
                    session.as_ref().and_then(|(queries, _)| {
                        queries
                            .indirect_lock
                            .borrow()
                            .value_witnesses
                            .matching(keys, hit)
                    })
                })
            });
            if let Some(witness) = witness {
                diagnostic = diagnostic.with_label(Label::secondary(
                    witness,
                    "this function value may wait and has a matching parameter count",
                ));
            }
            self.push(diagnostic);
        }

        // A critical section of an earlier unit can call the function values
        // this unit makes waiting: report each such value where it is made.
        if waiting & earlier_locks != 0 {
            for (value, reaches) in waiting_values {
                let mut keys = TypeReach::default();
                keys.add(value.bit, value.key.as_deref());
                if !keys.intersects(&earlier_types, value.bit & waiting)
                    || !keys.intersects(&value_types, value.bit)
                    || reaches & waiting == 0
                    || !reported.insert(value.span)
                {
                    continue;
                }
                let name = value
                    .target
                    .map_or_else(|| "lambda".to_string(), |target| target.to_string());
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E2604,
                        "a waiting function value can be called while holding a Willow lock",
                    )
                    .with_label(Label::primary(
                        value.span,
                        format!(
                            "`{name}` may suspend the task or block the scheduler worker, and a critical section in an imported module calls function values with this parameter count"
                        ),
                    ))
                    .with_help("pass a function value that does not wait, or move the call outside the critical section"),
                );
            }
        }

        if let Some((queries, unit)) = &session {
            let mut session = queries.indirect_lock.borrow_mut();
            session.types.insert(*unit, types);
            session.type_locks.merge(&own_type_locks);
            session.type_values.merge(&own_types);
            session.value_witnesses.merge(own_witnesses);
            session.values.merge(&own);
            session.locks |= own_locks;
            session.reach.insert(*unit, reach);
        }
    }
}
