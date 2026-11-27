use super::*;

#[willow_continuations::methods(
    lower_assign_operands,
    lower_condition,
    lower_conditional_suspend,
    lower_for,
    lower_lock,
    lower_defer_region,
    lower_match_arm_body,
    lower_match_arms_cfg,
    lower_nested_suspend,
    lower_nested_cfg,
    lower_operand_before_suspend,
    lower_root_suspend,
    lower_scope,
    lower_scope_inner,
    lower_select,
    lower_super_init_values,
    lower_stmt,
    lower_stmts,
    lower_suspending_operand,
    lower_value_into
)]
impl<'r> Builder<'r> {
    /// Lower root expression control flow or a scheduler suspension.
    /// Suspension operands are evaluated once before parking; an optional
    /// destination receives the selected branch value or resumed result.
    pub(super) fn lower_root_suspend(
        &mut self,
        value: &HirExpr,
        destination: Option<LirLocalId>,
    ) -> Option<Option<HirExpr>> {
        if let Some(lowered) = self.lower_try_propagate(value, destination) {
            return Some(lowered);
        }
        if let Some(lowered) = self.lower_match_arms_cfg(value, destination) {
            return Some(lowered);
        }
        if let Some(lowered) = self.lower_conditional_suspend(value) {
            if let Some(destination) = destination {
                let local = &self.locals[destination.0 as usize];
                self.push(SourceInst::Assign {
                    local: destination,
                    name: local.name.clone(),
                    value: lowered,
                });
                return Some(Some(self.local_expr(destination, value.span)));
            }
            return Some(None);
        }
        if !self.is_async {
            return None;
        }
        let operation = match &value.kind {
            // Awaiting a `Future<T>` VALUE is a BLOCKING runtime call
            // (`willow_future_await_*`), not a scheduler suspension: there is
            // no async frame behind that pointer to park on. Splitting it into
            // a `Suspend` would hand `SuspendOp::AwaitTask` a raw future
            // pointer to read frame slots out of, so this refuses the SPLIT and
            // leaves the await as an ordinary expression for the walker to emit
            // as that call — the function itself still compiles
            // from LIR. `await sleep(..)` / `await yield()` are not this case:
            // their type is `Future<void>` too, but they are recognised by
            // their CALL shape below and become real suspensions.
            HirExprKind::Await { inner }
                if awaitable_task_type(&inner.ty).is_none()
                    && builtin_types::unary_arg(&inner.ty, B::Future).is_some()
                    && !matches!(&inner.kind, HirExprKind::Call { callee, binding: HirCallBinding::Function, args }
                        if (callee.is_free_named("sleep") && args.len() == 1)
                            || (callee.is_free_named("yield") && args.is_empty())) =>
            {
                return None;
            }
            HirExprKind::Await { inner } => {
                // Only the runtime builtins return `Future<void>`; a user async
                // fn named `sleep`/`yield` returns a `Task` and is awaited as one.
                let builtin = builtin_types::unary_arg(&inner.ty, B::Future).is_some();
                if let HirExprKind::Call {
                    callee,
                    binding: HirCallBinding::Function,
                    args,
                } = &inner.kind
                {
                    match (callee.name().as_ref(), args.as_slice()) {
                        ("sleep", [millis]) if builtin && value.ty == Type::Void => {
                            let millis = self.lower_operand_before_suspend(millis)?;
                            let name = self.synthetic_name("sleep_millis");
                            let millis = self.push_synth_let(&name, false, millis);
                            SuspendOp::Sleep { millis }
                        }
                        ("yield", []) if builtin && value.ty == Type::Void => SuspendOp::Yield,
                        // Other calls produce a task that must be evaluated before parking.
                        _ => {
                            let awaited = self.lower_operand_before_suspend(inner)?;
                            let name = self.synthetic_name("task");
                            let task = self.push_synth_let(&name, false, awaited);
                            let (result_ty, cancel_aware) = awaitable_task_type(&inner.ty)
                                .unwrap_or_else(|| (value.ty.clone(), false));
                            SuspendOp::AwaitTask {
                                task,
                                result: destination,
                                result_ty,
                                cancel_aware,
                            }
                        }
                    }
                } else {
                    let awaited = self.lower_operand_before_suspend(inner)?;
                    let name = self.synthetic_name("task");
                    let task = self.push_synth_let(&name, false, awaited);
                    let (result_ty, cancel_aware) =
                        awaitable_task_type(&inner.ty).unwrap_or_else(|| (value.ty.clone(), false));
                    SuspendOp::AwaitTask {
                        task,
                        result: destination,
                        result_ty,
                        cancel_aware,
                    }
                }
            }
            HirExprKind::MethodCall {
                object,
                builtin: Some(BuiltinMethod::Intrinsic(id)),
                args,
                ..
            } if id.is_suspension_point() => {
                let receiver = self.lower_operand_before_suspend(object)?;
                let channel_name = self.synthetic_name("channel");
                let channel = self.push_synth_let(&channel_name, false, receiver);
                match (id, args.as_slice()) {
                    (Intrinsic::ChannelRecv | Intrinsic::ChannelRecvOpt, []) => {
                        SuspendOp::ChannelRecv {
                            channel,
                            result: destination,
                            elem_ty: builtin_types::unary_arg(&object.ty, B::Channel)
                                .expect("channel receiver has an element type")
                                .clone(),
                            closed_aware: *id == Intrinsic::ChannelRecvOpt,
                        }
                    }
                    (Intrinsic::ChannelSend, [sent]) => {
                        let Type::Generic(_, type_args) = &object.ty else {
                            unreachable!("channel method receiver has a generic channel type");
                        };
                        let elem_ty = type_args
                            .first()
                            .expect("Channel has an element type")
                            .clone();
                        let sent = self.lower_operand_before_suspend(sent)?;
                        let value_name = self.synthetic_name("send_value");
                        let sent = self.push_synth_let(&value_name, false, sent);
                        SuspendOp::ChannelSend {
                            channel,
                            value: sent,
                            elem_ty,
                        }
                    }
                    // Only these channel method and arity pairs park a task.
                    _ => return None,
                }
            }
            ir_variants!(
                HirExprKind;
                Int, Float, Bool, Str, Var, FnRef, Binary, Unary, Call, Invoke, Print, Array,
                Index, Ternary, New, FieldAccess, MethodCall, ObjectLiteral, StaticField,
                StaticCall, ReferenceArg, Range, TryPropagate, Lambda, Match, Select
            ) => return None,
        };
        let resume = self.new_block();
        self.terminate(SourceTerminator::Suspend { operation, resume });
        self.switch_to(resume);
        Some(destination.map(|local| self.local_expr(local, value.span)))
    }

    /// Split every root `match` into a dispatch chain with scoped arm bodies.
    /// The scrutinee is evaluated once and an optional destination receives
    /// the selected arm's value. Suspensions use the ordinary statement path.
    pub(super) fn lower_match_arms_cfg(
        &mut self,
        value: &HirExpr,
        destination: Option<LirLocalId>,
    ) -> Option<Option<HirExpr>> {
        let HirExprKind::Match { scrutinee, arms } = &value.kind else {
            return None;
        };

        // Evaluated once, before any test. A suspension in the scrutinee itself
        // splits here, in front of the whole dispatch.
        // The whole `match` is a scope of its own. The
        // scrutinee is held in a temp that outlives every arm and that no source
        // scope declared, so — like a `for` loop's hoisted iterable — the
        // construct has to drop its root itself, at the merge every arm reaches.
        let construct = self.scope_starts.len();
        self.scope_starts.push(LirScopeMark::opening_at(
            self.locals.len(),
            self.scope_roots.len(),
        ));
        let scrutinee_name = self.synthetic_name("match_scrutinee");
        let scrutinee_local = self.declare_local(
            scrutinee_name,
            scrutinee.ty.clone(),
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.scope_roots.push(scrutinee_local);
        self.lower_value_into(scrutinee_local, scrutinee);

        let merge = self.new_block();
        for (index, arm) in arms.iter().enumerate() {
            // A wildcard or a whole-value binding always applies, so it needs
            // no test, and without a guard nothing after it is reachable.
            let irrefutable = matches!(
                arm.pattern,
                HirPattern::Wildcard | HirPattern::Binding { .. }
            );
            let always_matches = irrefutable && arm.guard.is_empty();
            let is_last = index + 1 == arms.len();
            let arm_block = self.new_block();
            let next = (!always_matches && !is_last).then(|| self.new_block());
            if irrefutable {
                self.terminate(SourceTerminator::Jump(arm_block));
            } else {
                let test_name = self.synthetic_name("match_test");
                let test = self.declare_local(
                    test_name,
                    Type::Bool,
                    None,
                    LocalOptions {
                        synthetic: true,
                        parameter: false,
                    },
                );
                self.push(SourceInst::MatchTest {
                    scrutinee: scrutinee_local,
                    pattern: arm.pattern.clone(),
                    result: test,
                    span: arm.span,
                });
                // The last arm falling through means the scrutinee matched
                // nothing; the merge keeps the result at its seeded value,
                // exactly as the tree-shaped `match` does.
                self.terminate(SourceTerminator::Branch {
                    cond: self.local_expr(test, arm.span),
                    then_block: arm_block,
                    else_block: next.unwrap_or(merge),
                });
            }

            self.switch_to(arm_block);
            // The arm is a scope, and its pattern bindings belong to it: they
            // are declared HERE, ahead of the body's own scope, so the body's
            // close does not reach them.
            let arm_scope = self.scope_starts.len();
            self.scope_starts.push(LirScopeMark::opening_at(
                self.locals.len(),
                self.scope_roots.len(),
            ));
            self.declare_match_bindings(scrutinee_local, &arm.pattern, arm.span);
            // The guard chain sees the bindings of the pattern and of earlier
            // steps. A rejected step leaves the arm scope, dropping the roots
            // declared so far exactly as the body's fallthrough does, and
            // tries the next arm. Rejections share one clear ladder: rung `k`
            // clears only the roots declared since rung `k - 1`'s point and
            // falls into it, so the clears stay linear in the chain length.
            let mut rejections: Vec<(BlockId, usize)> = Vec::new();
            for step in &arm.guard {
                match step {
                    HirGuard::If(cond) => {
                        let accepted = self.lower_condition(cond);
                        let rejected = self.new_block();
                        let accepted_block = self.new_block();
                        rejections.push((rejected, self.scope_roots.len()));
                        self.terminate(SourceTerminator::Branch {
                            cond: accepted,
                            then_block: accepted_block,
                            else_block: rejected,
                        });
                        self.switch_to(accepted_block);
                    }
                    HirGuard::Let { scrutinee, pattern } => {
                        let local = self.guard_scrutinee_local(scrutinee);
                        if !matches!(pattern, HirPattern::Wildcard | HirPattern::Binding { .. }) {
                            let test_name = self.synthetic_name("match_test");
                            let test = self.declare_local(
                                test_name,
                                Type::Bool,
                                None,
                                LocalOptions {
                                    synthetic: true,
                                    parameter: false,
                                },
                            );
                            self.push(SourceInst::MatchTest {
                                scrutinee: local,
                                pattern: pattern.clone(),
                                result: test,
                                span: scrutinee.span,
                            });
                            let rejected = self.new_block();
                            let accepted_block = self.new_block();
                            rejections.push((rejected, self.scope_roots.len()));
                            self.terminate(SourceTerminator::Branch {
                                cond: self.local_expr(test, scrutinee.span),
                                then_block: accepted_block,
                                else_block: rejected,
                            });
                            self.switch_to(accepted_block);
                        }
                        self.declare_match_bindings(local, pattern, scrutinee.span);
                    }
                }
            }
            if !rejections.is_empty() {
                let body_block = BlockId(self.current);
                let first_root = self.scope_starts[arm_scope].first_root;
                let mut fall_to = next.unwrap_or(merge);
                let mut cleared_to = first_root;
                for (rejected, roots) in &rejections {
                    self.switch_to(*rejected);
                    let locals = self.scope_roots[cleared_to..*roots].to_vec();
                    if !locals.is_empty() {
                        self.push(SourceInst::ClearScopeRoots { locals });
                    }
                    self.terminate(SourceTerminator::Jump(fall_to));
                    fall_to = *rejected;
                    cleared_to = *roots;
                }
                self.switch_to(body_block);
            }
            self.lower_match_arm_body(&arm.body, destination);
            // After the body: an arm that produces a value has already copied it
            // into `destination`, which has a rooted slot of its own.
            self.push_scope_root_clears(arm_scope);
            self.pop_root_scope();
            self.terminate(SourceTerminator::Jump(merge));

            if let Some(next) = next {
                self.switch_to(next);
            }
            if always_matches {
                break;
            }
        }

        self.switch_to(merge);
        self.push_scope_root_clears(construct);
        self.pop_root_scope();
        Some(destination.map(|local| self.local_expr(local, value.span)))
    }

    /// Declare `pattern`'s bindings in the current scope and bind them from
    /// `scrutinee`.
    pub(super) fn declare_match_bindings(
        &mut self,
        scrutinee: LirLocalId,
        pattern: &HirPattern,
        span: Span,
    ) {
        let bindings: Vec<_> = pattern_bindings(pattern)
            .into_iter()
            .map(|(name, ty)| {
                // Discarded payloads still occupy binding positions but
                // must not alias another arm's discarded payload local.
                let name = if name == "_" {
                    self.synthetic_name("discard")
                } else {
                    name
                };
                self.declare_local(
                    name,
                    ty,
                    Some(span),
                    LocalOptions {
                        synthetic: false,
                        parameter: false,
                    },
                )
            })
            .collect();
        if !bindings.is_empty() {
            self.push(SourceInst::MatchBind {
                scrutinee,
                pattern: pattern.clone(),
                bindings,
                span,
            });
        }
    }

    /// The local a guard `let` step tests. A plain local (every nested payload
    /// local is one) is tested in place; any other value is evaluated once
    /// into a rooted temp of the arm scope.
    pub(super) fn guard_scrutinee_local(&mut self, scrutinee: &HirExpr) -> LirLocalId {
        if let HirExprKind::Var(name) = &scrutinee.kind
            && let Some(&local) = self.local_by_name.get(name)
            && self.locals[local.0 as usize].storage_kind != LirStorageKind::ReferenceCell
        {
            return local;
        }
        let name = self.synthetic_name("guard_scrutinee");
        let local = self.declare_local(
            name,
            scrutinee.ty.clone(),
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.scope_roots.push(local);
        self.lower_value_into(local, scrutinee);
        local
    }

    pub(super) fn push_compute(
        &mut self,
        value: LirRvalue,
        ty: Type,
        span: Span,
        label: &str,
    ) -> LirLocalId {
        let name = self.synthetic_name(label);
        let local = self.declare_local(
            name,
            ty,
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.push(SourceInst::Compute { local, value, span });
        local
    }

    pub(super) fn lower_try_propagate(
        &mut self,
        expression: &HirExpr,
        destination: Option<LirLocalId>,
    ) -> Option<Option<HirExpr>> {
        let HirExprKind::TryPropagate { inner } = &expression.kind else {
            return None;
        };
        let resolved = builtin_types::resolve(&inner.ty)?;
        let returning = builtin_types::resolve(&self.return_type)?;
        if !matches!(resolved.id, B::Option | B::Result) || resolved.id != returning.id {
            return None;
        }
        let is_option = resolved.id == B::Option;
        let source_error = (!is_option)
            .then(|| resolved.args.get(1).cloned())
            .flatten();
        let target_error = (!is_option)
            .then(|| returning.args.get(1).cloned())
            .flatten();
        let name = self.synthetic_name("try_operand");
        let operand = self.declare_local(
            name,
            inner.ty.clone(),
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.lower_value_into(operand, inner);
        let receiver = LirOperand::Local(operand);
        let condition = self.push_compute(
            LirRvalue::EnumMethod {
                receiver: receiver.clone(),
                receiver_ty: inner.ty.clone(),
                method: if is_option {
                    AdtMethod::IsSome
                } else {
                    AdtMethod::IsOk
                },
                args: Vec::new(),
                arg_types: Vec::new(),
                result: Type::Bool,
            },
            Type::Bool,
            expression.span,
            "try_success",
        );
        let success = self.new_block();
        let failure = self.new_block();
        self.terminate(SourceTerminator::Branch {
            cond: self.local_expr(condition, expression.span),
            then_block: success,
            else_block: failure,
        });
        self.switch_to(failure);
        let return_type = self.return_type.clone();
        let failure_value = if is_option {
            self.push_compute(
                LirRvalue::EnumAlloc {
                    class: TypeId::local(B::Option.source_name()),
                    variant: "None".into(),
                    enum_ty: return_type.clone(),
                },
                return_type.clone(),
                expression.span,
                "try_none",
            )
        } else if let (Some(source_error @ Type::Named(_)), Some(target_error)) =
            (&source_error, &target_error)
        {
            if source_error != target_error && *target_error != Type::Void {
                let error = self.push_compute(
                    LirRvalue::EnumMethod {
                        receiver: receiver.clone(),
                        receiver_ty: inner.ty.clone(),
                        method: AdtMethod::UnwrapErr,
                        args: Vec::new(),
                        arg_types: Vec::new(),
                        result: source_error.clone(),
                    },
                    source_error.clone(),
                    expression.span,
                    "try_error",
                );
                let converted = self.push_compute(
                    LirRvalue::IntoError {
                        value: LirOperand::Local(error),
                        source: source_error.clone(),
                        target: target_error.clone(),
                    },
                    target_error.clone(),
                    expression.span,
                    "try_converted_error",
                );
                let result = self.push_compute(
                    LirRvalue::EnumAlloc {
                        class: TypeId::local(B::Result.source_name()),
                        variant: "Err".into(),
                        enum_ty: return_type.clone(),
                    },
                    return_type.clone(),
                    expression.span,
                    "try_failure",
                );
                self.push(SourceInst::Compute {
                    local: result,
                    value: LirRvalue::EnumPayloadStore {
                        object: LirOperand::Local(result),
                        class: TypeId::local(B::Result.source_name()),
                        variant: "Err".into(),
                        index: 0,
                        value: LirOperand::Local(converted),
                        source: target_error.clone(),
                        enum_ty: return_type.clone(),
                    },
                    span: expression.span,
                });
                result
            } else {
                self.push_compute(
                    LirRvalue::RebindResultError {
                        value: receiver.clone(),
                        source: inner.ty.clone(),
                        target: return_type.clone(),
                    },
                    return_type.clone(),
                    expression.span,
                    "try_failure",
                )
            }
        } else {
            self.push_compute(
                LirRvalue::RebindResultError {
                    value: receiver.clone(),
                    source: inner.ty.clone(),
                    target: return_type.clone(),
                },
                return_type.clone(),
                expression.span,
                "try_failure",
            )
        };
        self.lower_stmt(&HirStmt::Return {
            value: Some(self.local_expr(failure_value, expression.span)),
            span: expression.span,
        });
        self.switch_to(success);
        if let Some(local) = destination
            && expression.ty != Type::Void
        {
            self.push(SourceInst::Compute {
                local,
                value: LirRvalue::EnumMethod {
                    receiver,
                    receiver_ty: inner.ty.clone(),
                    method: AdtMethod::Unwrap,
                    args: Vec::new(),
                    arg_types: Vec::new(),
                    result: expression.ty.clone(),
                },
                span: expression.span,
            });
            return Some(Some(self.local_expr(local, expression.span)));
        }
        Some(None)
    }

    /// Lower one arm's body into the block already switched to.
    ///
    /// An arm that produces a value ends in an expression statement, and that
    /// statement is what writes the match's result; everything before it is an
    /// effect. An arm that produces nothing — a block arm, or one that
    /// `return`s or panics — is lowered as an ordinary scope, and the result
    /// keeps whatever the merge was seeded with.
    pub(super) fn lower_match_arm_body(
        &mut self,
        body: &[HirStmt],
        destination: Option<LirLocalId>,
    ) {
        let value_tail = destination.and_then(|destination| match body.split_last() {
            // A `defer` anywhere in the body needs the scope brackets
            // `lower_scope` puts around it, so those arms take the plain path.
            Some((HirStmt::Expr(value), rest))
                if value.ty != Type::Never
                    && !body.iter().any(|s| matches!(s, HirStmt::Defer { .. })) =>
            {
                Some((destination, value, rest))
            }
            // Arms without a value-producing tail use ordinary scope lowering.
            _ => None,
        });
        match value_tail {
            Some((destination, value, rest)) => {
                self.lower_stmts(rest);
                self.lower_value_into(destination, value);
            }
            None => self.lower_scope(body),
        }
    }

    /// Evaluate one operand into a local of its own, splitting the suspension it
    /// holds out of the expression that reads it.
    ///
    /// `None` when the suspension sits somewhere no split reaches; the operand
    /// then keeps its suspension and the enclosing statement is refused as a
    /// whole.
    pub(super) fn lower_suspending_operand(&mut self, operand: &HirExpr) -> Option<HirExpr> {
        let name = self.synthetic_name("operand");
        let local = self.declare_local(
            name.clone(),
            operand.ty.clone(),
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        if self.lower_root_suspend(operand, Some(local)).is_none() {
            let value = self.lower_nested_suspend(operand)?;
            self.push_existing_let(local, name, false, operand.ty.clone(), value, None);
        }
        Some(self.local_expr(local, operand.span))
    }

    /// An operand of a statement that is ITSELF about to park: anything it
    /// suspends on runs first, in its own split, so the park that follows holds
    /// no nested suspension. `done.send(work.recv())` parks on
    /// the recv and sends the value it resumed with.
    pub(super) fn lower_operand_before_suspend(&mut self, operand: &HirExpr) -> Option<HirExpr> {
        if expression_needs_cfg(operand) {
            return self.lower_nested_cfg(operand);
        }
        if !suspends_anywhere(operand) {
            return Some(operand.clone());
        }
        self.lower_suspending_operand(operand)
    }

    /// Split a suspension out of an assignment's operands.
    ///
    /// `obj.field = await task;` and `xs[i] = await task;` park in the middle of
    /// a store, so their operands are ANF'd here the way a `let`'s initialiser
    /// is: the suspension becomes a [`SourceTerminator::Suspend`] of its own and the
    /// store reads the local the resume filled. Operands are evaluated left to
    /// right, so every operand before the parking one is frozen into a local
    /// first — a resume must not re-run `obj` or `i`.
    ///
    /// `None` when no operand needs control-flow splitting or async suspension
    /// hoisting; the caller then emits the original operands. Otherwise operands through
    /// the last splitting operand are materialized left to right via
    /// `lower_value_into`, except scalar literals and function references.
    /// Any suspension left in an unsupported position is rejected by
    /// `lir_async_rejection_reason`; `compile_function_named` reports invalid lowered
    /// IR as a compile error. There is no alternative function-body emitter.
    pub(super) fn lower_assign_operands(&mut self, operands: &[&HirExpr]) -> Option<Vec<HirExpr>> {
        let last = operands.iter().rposition(|operand| {
            expression_needs_cfg(operand) || (self.is_async && suspends_anywhere(operand))
        })?;
        let mut out = Vec::with_capacity(operands.len());
        for (index, operand) in operands.iter().enumerate() {
            let lowered = if index <= last
                && !matches!(
                    operand.kind,
                    HirExprKind::Int(_)
                        | HirExprKind::Float(_)
                        | HirExprKind::Bool(_)
                        | HirExprKind::Str(_)
                        | HirExprKind::FnRef(_)
                ) {
                let name = self.synthetic_name("operand");
                let local = self.declare_local(
                    name,
                    operand.ty.clone(),
                    None,
                    LocalOptions {
                        synthetic: true,
                        parameter: false,
                    },
                );
                self.lower_value_into(local, operand);
                self.local_expr(local, operand.span)
            } else {
                (*operand).clone()
            };
            out.push(lowered);
        }
        Some(out)
    }

    /// Evaluate eager operands in order around nested source control flow.
    /// The worklist keeps eager expression depth off the native stack. Place
    /// operands retain their address identity; only their receiver/index values
    /// are materialized, never the referenced slot's contents.
    pub(super) fn lower_nested_cfg(&mut self, value: &HirExpr) -> Option<HirExpr> {
        nested_cfg::lower(self, value)
    }

    pub(super) fn append_format_piece(
        &mut self,
        local: LirLocalId,
        piece: LirOperand,
        first: bool,
        span: Span,
    ) {
        let value = if first {
            LirRvalue::Use(piece)
        } else {
            LirRvalue::Binary {
                op: crate::parser::ast::BinOp::Add,
                lhs: LirOperand::Local(local),
                rhs: piece,
                operand_ty: Type::String,
            }
        };
        self.push(SourceInst::Compute { local, value, span });
    }

    pub(super) fn flat_operand(&mut self, value: &HirExpr) -> LirOperand {
        if let Some(operand) =
            LirOperand::from_flat_expr(value, |name| self.local_by_name.get(name).copied())
        {
            return operand;
        }
        let name = self.synthetic_name("operand");
        let local = self.declare_local(
            name,
            value.ty.clone(),
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.lower_value_into(local, value);
        LirOperand::Local(local)
    }

    pub(super) fn lower_nested_suspend(&mut self, value: &HirExpr) -> Option<HirExpr> {
        if expression_needs_cfg(value)
            || (self.is_async && suspends_anywhere(value) && !expr_suspends_here(value))
        {
            return self.lower_nested_cfg(value);
        }
        if !self.is_async || expr_suspends_here(value) {
            return None;
        }
        if let Some(value) = self.lower_conditional_suspend(value) {
            return Some(value);
        }
        let mut lowered = value.clone();
        let mut changed = false;
        loop {
            let mut suspensions = Vec::new();
            collect_suspensions(&lowered, &mut suspensions);
            let Some(target) = suspensions.first() else {
                return changed.then_some(lowered);
            };
            if target.ty == Type::Void {
                return None;
            }
            let mut seen = false;
            let initially_hoistable = hoistable_around(&lowered, target, &mut seen);
            let target = (*target).clone();
            if !initially_hoistable {
                lowered = self.freeze_binary_prefix(&lowered, target.span)?;
                let mut prepared_suspensions = Vec::new();
                collect_suspensions(&lowered, &mut prepared_suspensions);
                let prepared_target = prepared_suspensions.first()?;
                let mut seen = false;
                if !hoistable_around(&lowered, prepared_target, &mut seen) {
                    return None;
                }
            }
            let name = self.synthetic_name("result");
            let result = self.declare_local(
                name,
                target.ty.clone(),
                None,
                LocalOptions {
                    synthetic: true,
                    parameter: false,
                },
            );
            self.lower_root_suspend(&target, Some(result))?;
            let replacement = self.local_expr(result, target.span);
            lowered = replace_suspension(&lowered, target.span, &replacement);
            changed = true;
        }
    }

    /// Preserve a non-repeatable left operand before parking on a suspension
    /// in the right operand. This is the minimal ANF step needed for expressions
    /// such as `count() + await task`: `count()` runs once, before the park.
    pub(super) fn freeze_binary_prefix(
        &mut self,
        value: &HirExpr,
        target_span: Span,
    ) -> Option<HirExpr> {
        let target = value
            .walk_postorder(true)
            .find(|node| node.span == target_span && expr_suspends_here(node))?;
        let ancestors: std::collections::HashSet<*const HirExpr> = value
            .path_to(target)?
            .into_iter()
            .map(std::ptr::from_ref)
            .collect();
        let mut out = value.clone();
        let mut source = value;
        let mut current = &mut out;
        let mut can_fall_back = false;
        loop {
            match &source.kind {
                HirExprKind::Print { value: child, .. } => {
                    let HirExprKind::Print { value, .. } = &mut current.kind else {
                        unreachable!()
                    };
                    source = child;
                    current = value;
                }
                HirExprKind::Binary { lhs, rhs, .. } => {
                    let HirExprKind::Binary {
                        lhs: target_lhs,
                        rhs: target_rhs,
                        ..
                    } = &mut current.kind
                    else {
                        unreachable!()
                    };
                    if ancestors.contains(&std::ptr::from_ref(&**lhs)) {
                        source = lhs;
                        current = target_lhs;
                        continue;
                    }
                    if !ancestors.contains(&std::ptr::from_ref(&**rhs)) {
                        if can_fall_back {
                            break;
                        } else {
                            return None;
                        }
                    }
                    if !rematerializable(lhs) {
                        let name = self.synthetic_name("prefix");
                        let old = std::mem::replace(
                            &mut **target_lhs,
                            HirExpr {
                                kind: HirExprKind::Int(0),
                                ty: Type::Void,
                                span: lhs.span,
                            },
                        );
                        let local = self.push_synth_let(&name, false, old);
                        **target_lhs = self.local_expr(local, lhs.span);
                    }
                    can_fall_back = true;
                    if rhs.span == target_span && expr_suspends_here(rhs) {
                        break;
                    }
                    source = rhs;
                    current = target_rhs;
                }
                ir_variants!(
                    HirExprKind;
                    Int, Float, Bool, Str, Var, FnRef, Unary, Call, Invoke, Array, Index, Ternary,
                    New, FieldAccess, MethodCall, ObjectLiteral, StaticField, StaticCall,
                    ReferenceArg, Range, Await, TryPropagate, Lambda, Match, Select
                ) => {
                    if can_fall_back {
                        break;
                    }
                    return None;
                }
            }
        }
        Some(out)
    }

    pub(super) fn lower_conditional_suspend(&mut self, value: &HirExpr) -> Option<HirExpr> {
        let branches = ConditionalOperands::from_kind(&value.kind)?;
        let selected = self.cleanup_name.and_then(|_| branches.literal_condition());
        let condition = self.lower_condition(branches.condition);
        let result_name = self.synthetic_name("conditional");
        let result = self.declare_local(
            result_name,
            value.ty.clone(),
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        let first = self.new_block();
        let (first_truth, other) = if let Some(truth) = selected {
            self.terminate(SourceTerminator::Jump(first));
            (truth, None)
        } else {
            let otherwise = self.new_block();
            self.terminate(SourceTerminator::Branch {
                cond: condition,
                then_block: first,
                else_block: otherwise,
            });
            (true, Some((false, otherwise)))
        };
        let merge = self.new_block();
        for (truth, block) in std::iter::once((first_truth, first)).chain(other) {
            self.switch_to(block);
            if let Some(operand) = branches.operand(truth) {
                self.lower_value_into(result, operand);
            } else {
                let constant = HirExpr {
                    kind: HirExprKind::Bool(truth),
                    ty: Type::Bool,
                    span: value.span,
                };
                self.lower_value_into(result, &constant);
            }
            self.terminate(SourceTerminator::Jump(merge));
        }
        self.switch_to(merge);
        Some(self.local_expr(result, value.span))
    }

    pub(super) fn lower_value_into(&mut self, destination: LirLocalId, value: &HirExpr) {
        if self.lower_root_suspend(value, Some(destination)).is_some() {
            return;
        }
        let value = self
            .lower_nested_suspend(value)
            .unwrap_or_else(|| value.clone());
        let local = &self.locals[destination.0 as usize];
        self.push(SourceInst::Assign {
            local: destination,
            name: local.name.clone(),
            value,
        });
    }

    pub(super) fn lower_condition(&mut self, condition: &HirExpr) -> HirExpr {
        let name = self.synthetic_name("condition");
        let local = self.declare_local(
            name,
            Type::Bool,
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        match self.lower_root_suspend(condition, Some(local)) {
            Some(Some(value)) => value,
            Some(None) | None => self
                .lower_nested_suspend(condition)
                .unwrap_or_else(|| condition.clone()),
        }
    }

    /// Capture a reference at its source evaluation point. Array references retain
    /// the backing buffer, so later arguments may resize the array safely.
    pub(super) fn lower_reference_argument(
        &mut self,
        expr: &HirExpr,
        callee: FunctionId,
        index: usize,
    ) -> Option<LirOperand> {
        let HirExprKind::ReferenceArg { place } = &expr.kind else {
            return None;
        };
        let captured = match &place.kind {
            HirExprKind::Var(name) => LirPlace::Local(*self.local_by_name.get(name)?),
            HirExprKind::FieldAccess { object, field } => {
                let name = self.synthetic_name("reference_object");
                LirPlace::Field {
                    object: self.declare_local(
                        name,
                        object.ty.clone(),
                        None,
                        LocalOptions {
                            synthetic: true,
                            parameter: false,
                        },
                    ),
                    object_ty: object.ty.clone(),
                    field: field.clone(),
                    ty: place.ty.clone(),
                }
            }
            HirExprKind::Index { .. } => {
                let name = self.synthetic_name("reference_owner");
                let owner = self.declare_local(
                    name,
                    Type::Void,
                    None,
                    LocalOptions {
                        synthetic: true,
                        parameter: false,
                    },
                );
                self.locals[owner.0 as usize].storage_kind = LirStorageKind::GcOwner;
                let name = self.synthetic_name("reference_index");
                LirPlace::ArrayElement {
                    owner,
                    index: self.declare_local(
                        name,
                        Type::I64,
                        None,
                        LocalOptions {
                            synthetic: true,
                            parameter: false,
                        },
                    ),
                    element: place.ty.clone(),
                }
            }
            ir_variants!(
                HirExprKind;
                Int, Float, Bool, Str, FnRef, Binary, Unary, Call, Invoke, Print, Array, Ternary,
                New, MethodCall, ObjectLiteral, StaticField, StaticCall, ReferenceArg, Range,
                Await, TryPropagate, Lambda, Match, Select
            ) => return None,
        };
        let argument = LirOperand::Reference {
            place: captured.clone(),
            span: expr.span,
            display: reference_place_name(place),
        };
        self.push_compute(
            LirRvalue::ReferenceDebug {
                argument: argument.clone(),
                callee,
                index,
            },
            Type::Void,
            expr.span,
            "reference_debug",
        );
        match (&place.kind, captured) {
            (
                HirExprKind::FieldAccess { object, .. },
                LirPlace::Field {
                    object: destination,
                    ..
                },
            ) => self.lower_value_into(destination, object),
            (
                HirExprKind::Index { array, index },
                LirPlace::ArrayElement {
                    owner,
                    index: saved_index,
                    ..
                },
            ) => {
                let name = self.synthetic_name("reference_array");
                let array = self.push_synth_let(&name, false, (**array).clone());
                self.lower_value_into(saved_index, index);
                self.push(SourceInst::Compute {
                    local: owner,
                    value: LirRvalue::CaptureArrayOwner {
                        array: LirOperand::Local(array),
                        index: LirOperand::Local(saved_index),
                    },
                    span: place.span,
                });
            }
            (HirExprKind::Var(_), LirPlace::Local(_)) => {}
            // A reserved place must pair the source form with the matching storage.
            (
                ir_variants!(
                    HirExprKind;
                    Int, Float, Bool, Str, Var, FnRef, Binary, Unary, Call, Invoke, Print, Array,
                    Index, Ternary, New, FieldAccess, MethodCall, ObjectLiteral, StaticField,
                    StaticCall, ReferenceArg, Range, Await, TryPropagate, Lambda, Match, Select
                ),
                _,
            ) => unreachable!(),
        }
        Some(argument)
    }

    pub(super) fn lower_super_init_values(&mut self, args: &[HirExpr], span: Span) -> bool {
        let Some(&receiver) = self.local_by_name.get("self") else {
            return false;
        };
        let receiver_ty = self.locals[receiver.0 as usize].ty.clone();
        let Type::Named(owner) = &receiver_ty else {
            return false;
        };
        let Some(base) = self
            .resolution
            .classes
            .get(owner)
            .and_then(|info| info.base)
        else {
            return false;
        };
        let resolution = self.resolution;
        let Some(info) = resolution.classes.get(&base) else {
            return false;
        };
        let fields = if let Some(signature) = &info.constructor {
            if !arguments_compatible(resolution, &signature.params, args) {
                return false;
            }
            Vec::new()
        } else {
            let Some(fields) = resolution.fields_rev(&base) else {
                return false;
            };
            if fields.len() != args.len() {
                return false;
            }
            // Argument lowering can itself resolve another layout. Keep only
            // the borrowed declarations while evaluating in source order.
            let mut fields: Vec<_> = fields.collect();
            fields.reverse();
            if !arguments_compatible(resolution, fields.iter().map(|(_, ty)| ty), args) {
                return false;
            }
            fields
        };
        let name = self.synthetic_name("base_receiver");
        let base_ty = Type::Named(base);
        let base_local = self.declare_local(
            name,
            base_ty.clone(),
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.push(SourceInst::Compute {
            local: base_local,
            value: LirRvalue::Coerce {
                value: LirOperand::Local(receiver),
                source: receiver_ty,
                target: base_ty.clone(),
            },
            span,
        });
        let mut operands = Vec::new();
        if args
            .iter()
            .any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
        {
            self.push_compute(
                LirRvalue::BeginReferenceCall,
                Type::Void,
                span,
                "reference_call",
            );
        }
        for (index, arg) in args.iter().enumerate() {
            let target = match &info.constructor {
                Some(signature) => &signature.params[index],
                None => &fields[index].1,
            };
            if matches!(arg.kind, HirExprKind::ReferenceArg { .. }) {
                let Some(reference) =
                    self.lower_reference_argument(arg, FunctionId::method(base, "init"), index)
                else {
                    return false;
                };
                operands.push(reference);
                continue;
            }
            let value = self
                .lower_nested_suspend(arg)
                .unwrap_or_else(|| arg.clone());
            let name = self.synthetic_name("base_argument");
            let source = self.push_synth_let(&name, false, value);
            let name = self.synthetic_name("base_coerced");
            let coerced = self.declare_local(
                name,
                target.clone(),
                None,
                LocalOptions {
                    synthetic: true,
                    parameter: false,
                },
            );
            self.push(SourceInst::Compute {
                local: coerced,
                value: LirRvalue::Coerce {
                    value: LirOperand::Local(source),
                    source: arg.ty.clone(),
                    target: target.clone(),
                },
                span: arg.span,
            });
            if info.constructor.is_some() {
                operands.push(LirOperand::Local(coerced));
            } else {
                let name = self.synthetic_name("base_store");
                let local = self.declare_local(
                    name,
                    Type::Void,
                    None,
                    LocalOptions {
                        synthetic: true,
                        parameter: false,
                    },
                );
                self.push(SourceInst::Compute {
                    local,
                    value: LirRvalue::FieldStore {
                        object: LirOperand::Local(base_local),
                        object_ty: base_ty.clone(),
                        field: fields[index].0.clone(),
                        value: LirOperand::Local(coerced),
                    },
                    span: arg.span,
                });
            }
        }
        if let Some(signature) = &info.constructor {
            let name = self.synthetic_name("base_init");
            let local = self.declare_local(
                name,
                Type::Void,
                None,
                LocalOptions {
                    synthetic: true,
                    parameter: false,
                },
            );
            self.push(SourceInst::Compute {
                local,
                value: LirRvalue::ConstructorCall {
                    object: LirOperand::Local(base_local),
                    class: base,
                    args: operands,
                    arg_types: signature.params.clone(),
                },
                span,
            });
        }
        true
    }

    pub(super) fn lower_select(
        &mut self,
        cases: &[crate::ir::typed_ast::HirSelectCase],
        span: Span,
    ) {
        use crate::ir::typed_ast::HirSelectCaseKind;

        let mut operations = Vec::with_capacity(cases.len());
        for case in cases {
            let operation = match &case.kind {
                HirSelectCaseKind::Recv {
                    binding,
                    channel,
                    closed_aware,
                } => {
                    let name = self.synthetic_name("select_channel");
                    let channel_local = self.push_synth_let(&name, false, channel.clone());
                    let elem_ty = builtin_types::unary_arg(&channel.ty, B::Channel)
                        .expect("select recv channel was type checked")
                        .clone();
                    let binding = (binding != "_").then(|| {
                        let binding_ty = if *closed_aware {
                            B::Option.apply(vec![elem_ty.clone()])
                        } else {
                            elem_ty.clone()
                        };
                        self.declare_local(
                            binding.clone(),
                            binding_ty,
                            Some(case.span),
                            LocalOptions {
                                synthetic: false,
                                parameter: false,
                            },
                        )
                    });
                    LirSelectOp::Recv {
                        channel: channel_local,
                        binding,
                        elem_ty,
                        closed_aware: *closed_aware,
                    }
                }
                HirSelectCaseKind::Send { channel, value } => {
                    let channel_name = self.synthetic_name("select_channel");
                    let channel_local = self.push_synth_let(&channel_name, false, channel.clone());
                    let value_name = self.synthetic_name("select_value");
                    let value_local = self.push_synth_let(&value_name, false, value.clone());
                    let elem_ty = builtin_types::unary_arg(&channel.ty, B::Channel)
                        .expect("select send channel was type checked")
                        .clone();
                    let value_local = if self.is_async {
                        value_local
                    } else {
                        let name = self.synthetic_name("select_stored_value");
                        let stored = self.declare_local(
                            name,
                            elem_ty.clone(),
                            None,
                            LocalOptions {
                                synthetic: true,
                                parameter: false,
                            },
                        );
                        self.push(SourceInst::Compute {
                            local: stored,
                            value: LirRvalue::Coerce {
                                value: LirOperand::Local(value_local),
                                source: value.ty.clone(),
                                target: elem_ty.clone(),
                            },
                            span: value.span,
                        });
                        stored
                    };
                    LirSelectOp::Send {
                        channel: channel_local,
                        value: value_local,
                        elem_ty,
                    }
                }
                HirSelectCaseKind::Timeout { millis } => {
                    let millis_name = self.synthetic_name("select_millis");
                    let millis_local = self.push_synth_let(&millis_name, false, millis.clone());
                    let deadline_name = self.synthetic_name("select_deadline");
                    let deadline = self.declare_local(
                        deadline_name,
                        Type::I64,
                        None,
                        LocalOptions {
                            synthetic: true,
                            parameter: false,
                        },
                    );
                    LirSelectOp::Timeout {
                        millis: millis_local,
                        deadline,
                    }
                }
                HirSelectCaseKind::Join { binding, task } => {
                    let task_name = self.synthetic_name("select_task");
                    let task_local = self.push_synth_let(&task_name, false, task.clone());
                    // `result_ty` is the PAYLOAD the backend loads out of the
                    // terminal frame; the `Result<T, Cancelled>` wrapper that
                    // `await t.result()` binds is built by the emitter from it,
                    // so only the BINDING carries the wrapped type.
                    let (result_ty, cancel_aware) = awaitable_task_type(&task.ty)
                        .expect("checked select join must have a task type");
                    let binding_ty = await_output_type(&task.ty)
                        .expect("checked select join must have an output type");
                    let binding = (binding != "_").then(|| {
                        self.declare_local(
                            binding.clone(),
                            binding_ty,
                            Some(case.span),
                            LocalOptions {
                                synthetic: false,
                                parameter: false,
                            },
                        )
                    });
                    LirSelectOp::Join {
                        task: task_local,
                        binding,
                        result_ty,
                        cancel_aware,
                    }
                }
                HirSelectCaseKind::Default => LirSelectOp::Default,
            };
            if !self.is_async && matches!(operation, LirSelectOp::Timeout { .. }) {
                self.push(SourceInst::SelectInit {
                    operations: vec![operation.clone()],
                });
            }
            operations.push(operation);
        }

        if self.is_async {
            self.push(SourceInst::SelectInit {
                operations: operations.clone(),
            });
        }
        // A slow-path quiescence observation must precede the final probe.
        // A peer can publish a ready case while the local drive completes zero tasks.
        let quiescent = if !self.is_async
            && !operations
                .iter()
                .any(|op| matches!(op, LirSelectOp::Default))
        {
            let name = self.synthetic_name("select_quiescent");
            Some(self.push_synth_let(
                &name,
                true,
                HirExpr {
                    kind: HirExprKind::Bool(false),
                    ty: Type::Bool,
                    span,
                },
            ))
        } else {
            None
        };
        let probe = self.new_block();
        let idle = self.new_block();
        let dispatch = self.new_block();
        let done = self.new_block();
        let case_blocks: Vec<_> = cases.iter().map(|_| self.new_block()).collect();
        self.terminate(SourceTerminator::Jump(probe));

        self.switch_to(probe);
        let ready: Vec<_> = operations
            .iter()
            .map(|operation| {
                (!matches!(operation, LirSelectOp::Default)).then(|| {
                    let name = self.synthetic_name("select_ready");
                    self.declare_local(
                        name,
                        Type::Bool,
                        None,
                        LocalOptions {
                            synthetic: true,
                            parameter: false,
                        },
                    )
                })
            })
            .collect();
        let chosen_name = self.synthetic_name("select_chosen");
        let chosen = self.declare_local(
            chosen_name,
            Type::I64,
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        self.push(SourceInst::SelectProbe {
            operations: operations.clone(),
            ready: ready.clone(),
        });
        self.push(SourceInst::SelectPick { ready, chosen });
        let chosen_expr = self.local_expr(chosen, span);
        self.terminate(SourceTerminator::Branch {
            cond: HirExpr {
                kind: HirExprKind::Binary {
                    op: crate::parser::ast::BinOp::Ge,
                    lhs: Box::new(chosen_expr.clone()),
                    rhs: Box::new(HirExpr {
                        kind: HirExprKind::Int(0),
                        ty: Type::I64,
                        span,
                    }),
                },
                ty: Type::Bool,
                span,
            },
            then_block: dispatch,
            else_block: idle,
        });

        self.switch_to(idle);
        let default = operations
            .iter()
            .position(|operation| matches!(operation, LirSelectOp::Default));
        if default.is_some() {
            self.terminate(SourceTerminator::Jump(dispatch));
        } else if self.is_async {
            self.terminate(SourceTerminator::Suspend {
                operation: SuspendOp::SelectWait {
                    operations: operations.iter().filter_map(LirSelectOp::wait_op).collect(),
                },
                resume: probe,
            });
        } else {
            let local = quiescent.expect("synchronous blocking select has a phase local");
            let deadlines = operations
                .iter()
                .filter_map(|operation| match operation {
                    LirSelectOp::Timeout { deadline, .. } => Some(LirOperand::Local(*deadline)),
                    LirSelectOp::Recv { .. }
                    | LirSelectOp::Send { .. }
                    | LirSelectOp::Join { .. }
                    | LirSelectOp::Default => None,
                })
                .collect();
            self.push(SourceInst::Compute {
                local,
                value: LirRvalue::SelectIdleWait {
                    deadlines,
                    quiescent: LirOperand::Local(local),
                },
                span,
            });
            self.terminate(SourceTerminator::Jump(probe));
        }

        self.switch_to(dispatch);
        // All winners share one alias-aware cleanup graph. `chosen == -1`
        // denotes default, which retains no channel ownership.
        self.push(SourceInst::SelectUnregister {
            operations: operations.clone(),
            winner: chosen,
        });
        let selectable: Vec<_> = operations
            .iter()
            .enumerate()
            .filter(|(_, operation)| !matches!(operation, LirSelectOp::Default))
            .map(|(index, _)| index)
            .collect();
        if selectable.is_empty() {
            self.terminate(SourceTerminator::Jump(
                default.map_or(done, |index| case_blocks[index]),
            ));
        }
        for (position, index) in selectable.iter().enumerate() {
            let fallback = if position + 1 == selectable.len() {
                default.map_or(probe, |index| case_blocks[index])
            } else {
                self.new_block()
            };
            self.terminate(SourceTerminator::Branch {
                cond: HirExpr {
                    kind: HirExprKind::Binary {
                        op: crate::parser::ast::BinOp::Eq,
                        lhs: Box::new(chosen_expr.clone()),
                        rhs: Box::new(HirExpr {
                            kind: HirExprKind::Int(*index as i64),
                            ty: Type::I64,
                            span,
                        }),
                    },
                    ty: Type::Bool,
                    span,
                },
                then_block: case_blocks[*index],
                else_block: fallback,
            });
            if fallback != probe {
                self.switch_to(fallback);
            }
        }

        for (index, case) in cases.iter().enumerate() {
            self.switch_to(case_blocks[index]);
            let success_name = self.synthetic_name("select_success");
            let success = self.declare_local(
                success_name,
                Type::Bool,
                None,
                LocalOptions {
                    synthetic: true,
                    parameter: false,
                },
            );
            self.push(SourceInst::SelectCommit {
                operation: operations[index].clone(),
                success,
            });
            if matches!(operations[index], LirSelectOp::Send { .. }) {
                let body = self.new_block();
                self.terminate(SourceTerminator::Branch {
                    cond: self.local_expr(success, case.span),
                    then_block: body,
                    else_block: probe,
                });
                self.switch_to(body);
            }
            self.lower_scope(&case.body);
            self.terminate(SourceTerminator::Jump(done));
        }
        self.switch_to(done);
    }
}
