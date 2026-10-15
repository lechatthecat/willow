//! E2604 for waits reached through function values (willow-h4t7); see
//! [`crate::semantic::indirect_lock`] for the address-taken model.

use std::collections::HashMap;

use crate::compiler_db::effects::EffectWitness;
use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::{CallArg, Expr, Type};
use crate::semantic::call_graph::CallGraph;
use crate::semantic::effects::EffectFacts;
use crate::semantic::ids::FunctionId;
use crate::semantic::indirect_lock::{FunctionValue, ValueMasks, arity_bit, close_reach};

use super::TypeChecker;

impl TypeChecker {
    /// A named function or a lambda became a value that indirect calls can
    /// reach.
    pub(super) fn record_function_value(
        &mut self,
        target: Option<FunctionId>,
        arity: usize,
        span: Span,
    ) {
        self.indirect_lock.values.push(FunctionValue {
            target,
            bit: arity_bit(arity),
            span,
        });
    }

    /// A call through a function value with `arity` arguments.
    fn record_indirect_call(&mut self, arity: usize, span: Span) {
        let bit = arity_bit(arity);
        if let Some(caller) = self.local.current_effect_callable {
            self.indirect_lock.calls.push((caller, bit));
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
        let args: &[CallArg] = match expr {
            Expr::Call(call) => {
                if self.symbols.lookup_var(&call.callee).is_some() {
                    self.record_indirect_call(call.args.len(), call.span);
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
            if let Some(Type::Fn(params, _) | Type::Closure(params, _)) =
                self.expr_types.get(&arg.expr.id())
            {
                self.record_indirect_call(params.len(), arg.span);
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
    pub(super) fn report_indirect_lock_effects(
        &mut self,
        graph: &CallGraph,
        facts: &EffectFacts<EffectWitness>,
    ) {
        let inputs = std::mem::take(&mut self.indirect_lock);
        let session = self.effect_queries.clone();
        let imports = std::sync::Arc::clone(&self.effect_index.imports);
        let external_reach = |target: &FunctionId| match &session {
            Some((queries, unit)) => queries.indirect_reach(*unit, target, &imports),
            None => 0,
        };

        // Each callable's own indirect calls, plus those its synchronous
        // callees in earlier units can make.
        let mut seed: HashMap<FunctionId, u64> = HashMap::new();
        for (caller, bit) in &inputs.calls {
            *seed.entry(*caller).or_default() |= bit;
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
                    }
                }
            }
        }
        let reach = close_reach(seed, edges.into_iter());
        let reach_of = |target: &FunctionId| {
            reach
                .get(target)
                .copied()
                .unwrap_or_else(|| external_reach(target))
        };

        // This unit's function values: waiting ones and conditional ones.
        let mut own = ValueMasks::default();
        let mut witnesses: HashMap<u64, Span> = HashMap::new();
        let mut waiting_values = Vec::new();
        for value in &inputs.values {
            match self.value_waits(value, facts) {
                Some(true) => {
                    own.waiting |= value.bit;
                    witnesses.entry(value.bit).or_insert(value.span);
                    waiting_values.push((value, u64::MAX));
                }
                Some(false) => {}
                None => {
                    let target_reach = value.target.as_ref().map_or(0, &reach_of);
                    own.add_conditional(value.bit, target_reach);
                    if target_reach != 0 {
                        waiting_values.push((value, target_reach));
                    }
                }
            }
        }

        // Lock-held indirect calls: direct ones, and lock-held calls to
        // helpers that reach indirect calls.
        let mut lock_sites = inputs.lock_sites.clone();
        for site in &self.effect_inputs.sites {
            let bits = reach_of(&site.callee);
            if bits != 0 {
                lock_sites.push((site.span, bits));
            }
        }
        let own_locks = lock_sites.iter().fold(0, |locks, (_, bits)| locks | bits);

        let (before, earlier_locks) = match &session {
            Some((queries, _)) => {
                let session = queries.indirect_lock.borrow();
                (session.values.clone(), session.locks)
            }
            None => (ValueMasks::default(), 0),
        };
        let waiting_before = before.closure();
        let mut after = before;
        after.merge(&own);
        let waiting = after.closure();

        let mut reported: std::collections::HashSet<Span> = self
            .errors
            .iter()
            .filter(|diagnostic| diagnostic.code == ErrorCode::E2604)
            .flat_map(|diagnostic| diagnostic.labels.first().map(|label| label.span))
            .collect();
        lock_sites.sort_by_key(|(span, _)| (span.file_id, span.start, span.end));
        for (span, bits) in lock_sites {
            let hit = bits & waiting;
            if hit == 0 || !reported.insert(span) {
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
            if let Some(witness) = (0..64)
                .map(|bit| 1u64 << bit)
                .filter(|bit| hit & bit != 0)
                .find_map(|bit| witnesses.get(&bit))
            {
                diagnostic = diagnostic.with_label(Label::secondary(
                    *witness,
                    "this function value may wait and has a matching parameter count",
                ));
            }
            self.push(diagnostic);
        }

        // A critical section of an earlier unit can call the function values
        // this unit makes waiting: report each such value where it is made.
        let newly_waiting = waiting & !waiting_before;
        if newly_waiting & earlier_locks != 0 {
            for (value, reaches) in waiting_values {
                if value.bit & newly_waiting == 0
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
            session.values.merge(&own);
            session.locks |= own_locks;
            session.reach.insert(*unit, reach);
        }
    }
}
