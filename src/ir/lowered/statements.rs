use super::*;

#[willow_continuations::methods(
    lower_assign_operands,
    lower_condition,
    lower_conditional_suspend,
    lower_for,
    lower_lock,
    lower_lock_walk,
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
    pub(super) fn lower_stmts(&mut self, stmts: &[HirStmt]) {
        for stmt in stmts {
            self.lower_stmt(stmt);
        }
    }

    /// Lower a statement list as a lexical scope: if it registers any `defer`,
    /// bracket it with [`SourceInst::EnterDeferScope`]/[`SourceInst::LeaveDeferScope`].
    ///
    ///
    /// Scopes without a `defer` get no markers at all because nothing would
    /// run at their exit.
    pub(super) fn lower_scope(&mut self, stmts: &[HirStmt]) {
        self.lower_scope_inner(stmts, None, None);
    }

    /// [`Self::lower_scope`], but `lock` names the critical section this scope
    /// IS the body of, and opens the scope even when the body registers no
    /// `defer` at all.
    ///
    /// A `lock` body needs one regardless: the scope is what
    /// gives the critical section a cleanup block, and a panic inside the
    /// section has to release the lock on its way out even when the section
    /// defers nothing.
    pub(super) fn lower_scope_inner(
        &mut self,
        stmts: &[HirStmt],
        lock: Option<LirLockSlots>,
        walk: Option<(&HirExpr, Span)>,
    ) {
        // Record direct ownership for ordinary exits and the full local range
        // for recovery, which can bypass nested scopes' normal closes.
        let lexical_scope = self.scope_starts.len();
        self.scope_starts.push(LirScopeMark::opening_at(
            self.locals.len(),
            self.scope_roots.len(),
        ));
        let mut scope = std::collections::HashMap::new();
        // Traversal's outer scope owns the changing lock; user defers belong
        // to the inner per-node scope and run before attempting the next node.
        let scope_stmts = if walk.is_some() { &[][..] } else { stmts };
        let sites: Vec<(LirDeferId, Span)> = scope_stmts
            .iter()
            .filter_map(|s| match s {
                HirStmt::Defer {
                    id: hir_id, span, ..
                } => {
                    let id = LirDeferId(self.defer_counter);
                    self.defer_counter += 1;
                    assert!(
                        scope.insert(*hir_id, id).is_none(),
                        "duplicate HIR defer identity"
                    );
                    Some((id, *span))
                }
                // Only defer declarations reserve cleanup sites at scope entry.
                _ => None,
            })
            .collect();
        if sites.is_empty() && lock.is_none() {
            self.lower_stmts(stmts);
            self.push_scope_root_clears(lexical_scope);
            self.pop_root_scope();
            return;
        }
        let enter_block = self.current;
        let enter_index = self.blocks[enter_block].0.len();
        // Only a scope that can actually swallow a panic continues at its
        // resume block; a scope whose defers merely run cleanup lets the panic
        // through, so it adds no edge.
        let recovers = scope_stmts.iter().any(|stmt| match stmt {
            HirStmt::Defer { id, body, .. } => self
                .nested_defer_access
                .as_ref()
                .and_then(|sites| sites.get(id))
                .map_or_else(
                    || defer_body_contains_recover(body),
                    |access| access.recovers,
                ),
            _ => false,
        });
        self.push(SourceInst::EnterDeferScope {
            sites: sites.clone(),
            resume: None,
            lock: lock.clone(),
        });
        self.defer_scopes.push(scope);
        self.defer_depth += 1;
        let body_start = self.blocks.len();
        if let Some((target, span)) = walk {
            self.lower_lock_walk(stmts, lock.as_ref().expect("walk lock slots"), target, span);
        } else {
            self.lower_stmts(stmts);
        }
        self.defer_depth -= 1;
        let mut sites: Vec<_> = self
            .defer_scopes
            .pop()
            .expect("LIR defer scope stack")
            .into_values()
            .collect();
        sites.sort_unstable();
        // The fallthrough close. If the scope ended in a `return`, this lands
        // in the dead block `terminate` switched to and is pruned.
        self.push(SourceInst::LeaveDeferScope { sites });
        if self.is_async || recovers {
            // Recovery must branch to a real LIR continuation, not a backend-
            // invented block after the whole poll body has been emitted.
            let resume = self.new_block();
            self.terminate(SourceTerminator::Jump(resume));
            let SourceInst::EnterDeferScope {
                resume: entry_resume,
                ..
            } = &mut self.blocks[enter_block].0[enter_index]
            else {
                unreachable!("recorded defer entry instruction changed kind");
            };
            *entry_resume = Some(resume);
            if recovers {
                // Every block the scope's body occupies can raise the panic
                // this scope recovers from. `new_block` only ever appends, so
                // the body's blocks are exactly the ones created between the
                // entry and the resume, plus the entry block itself.
                //
                // Innermost first falls out of the nesting: an inner scope
                // records its edge while lowering the outer scope's body, so
                // the outer scope's edge is appended after it.
                self.block_recovery[enter_block].push(resume);
                for id in body_start..resume.0 {
                    self.block_recovery[id].push(resume);
                }
            }
            self.switch_to(resume);
            // In the resume block, not before the jump: a recovered panic
            // branches straight here from wherever it was raised, so a clear on
            // the fallthrough path alone would leave the scope's roots standing
            // on the recovery path. Both paths have run the
            // scope's `defer`s by the time they arrive, which is what the clear
            // has to come after — a deferred body may read the bindings.
            if recovers {
                self.push_recovery_root_clears(lexical_scope);
            } else {
                self.push_scope_root_clears(lexical_scope);
            }
        } else {
            // After the scope's own `defer`s have run: they may read the
            // bindings this drops the roots of.
            self.push_scope_root_clears(lexical_scope);
        }
        self.pop_root_scope();
    }

    pub(super) fn pop_root_scope(&mut self) {
        let mark = self.scope_starts.pop().expect("LIR lexical scope stack");
        self.scope_roots.truncate(mark.first_root);
    }

    /// Ordinary exits only leave still-open scopes. Closed children have
    /// already cleared their roots on every ordinary path reaching this point.
    /// Ownership is recorded once at declaration, avoiding nested range scans.
    pub(super) fn push_scope_root_clears(&mut self, scope: usize) {
        let locals = self.scope_roots[self.scope_starts[scope].first_root..].to_vec();
        if !locals.is_empty() {
            self.push(SourceInst::ClearScopeRoots { locals });
        }
    }

    /// Recovery can bypass any child's close, so its continuation must still
    /// clear descendants. Unlike ordinary exits, none are definitely cleared.
    pub(super) fn push_recovery_root_clears(&mut self, scope: usize) {
        let mark = &self.scope_starts[scope];
        let mut locals: Vec<LirLocalId> = self.locals[mark.first_local..]
            .iter()
            .filter(|local| !local.synthetic && !local.parameter)
            .map(|local| local.id)
            .collect();
        // Explicitly adopted synthetic roots (for iterable/element and match
        // scrutinee) are disjoint from the source-local range above.
        locals.extend(
            self.scope_roots[mark.first_root..]
                .iter()
                .copied()
                .filter(|id| self.locals[id.0 as usize].synthetic),
        );
        if !locals.is_empty() {
            self.push(SourceInst::ClearScopeRoots { locals });
        }
    }

    /// Drop the GC roots of every lexical scope an early exit is about to leave.
    ///
    ///
    /// `break` and `continue` jump out without passing the fallthrough close,
    /// so the boundary that close marks has to be re-stated here — the same
    /// reason [`SourceInst::FlushDefers`] exists beside
    /// [`SourceInst::LeaveDeferScope`]. `return` needs nothing, because the emitter
    /// pops every root there.
    pub(super) fn clear_scope_roots_down_to(&mut self, depth: usize) {
        self.push_scope_root_clears(depth);
    }

    /// Flush every defer scope an early exit is about to leave, releasing the
    /// critical section it leaves on the way out.
    ///
    /// The release sits BETWEEN the two flushes rather than before or after
    /// both: the section's own `defer`s are part of the critical section and
    /// run while the lock is still held, and only then does the lock go back,
    /// before the enclosing scopes' `defer`s run outside it. That is the same
    /// order the AST unwinder produces by releasing as each defer frame
    /// finishes.
    pub(super) fn flush_defers_down_to(&mut self, depth: usize) {
        let mut scope_end = self.defer_scopes.len();
        for index in (0..self.active_locks.len()).rev() {
            let lock = self.active_locks[index].clone();
            if lock.defer_depth < depth {
                break;
            }
            self.flush_defer_scopes(lock.defer_depth, scope_end);
            self.push(SourceInst::ReleaseLock(lock.slots));
            scope_end = lock.defer_depth;
        }
        self.flush_defer_scopes(depth, scope_end);
    }

    /// Register one `FlushDefers` for the scopes in `from..to`, newest last.
    pub(super) fn flush_defer_scopes(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let mut sites = Vec::new();
        for scope in &self.defer_scopes[from..to] {
            let mut scope_sites: Vec<_> = scope.values().copied().collect();
            scope_sites.sort_unstable();
            sites.extend(scope_sites);
        }
        self.push(SourceInst::FlushDefers {
            sites,
            scopes: to - from,
        });
    }

    pub(super) fn lower_stmt(&mut self, stmt: &HirStmt) {
        match stmt {
            HirStmt::Let {
                name,
                mutable,
                ty,
                value,
                span,
            } => {
                let local = self.declare_local(
                    name.clone(),
                    ty.clone(),
                    Some(*span),
                    LocalOptions {
                        synthetic: false,
                        parameter: false,
                    },
                );
                if self.lower_root_suspend(value, Some(local)).is_none() {
                    let value = self
                        .lower_nested_suspend(value)
                        .unwrap_or_else(|| value.clone());
                    self.push_existing_let(
                        local,
                        name.clone(),
                        *mutable,
                        ty.clone(),
                        value,
                        Some(*span),
                    );
                }
            }
            HirStmt::Assign { name, value, span } => {
                let local = self.local_by_name.get(name).copied().unwrap_or_else(|| {
                    // Every assignable name is declared before it is assigned:
                    // parameters in `Builder::new`, locals by `HirStmt::Let`,
                    // and a lambda cannot reach an enclosing function's binding
                    // (the checker rejects captures with E1002). Declaring one
                    // here keeps a release build emitting a well-formed graph,
                    // but the write would go to a local nothing else reads, so
                    // a debug build reports it instead of losing it silently.
                    debug_assert!(
                        false,
                        "assignment to `{name}` at line {} has no LIR local; \
                         a binding form is missing from lowering",
                        span.line,
                    );
                    self.declare_local(
                        name.clone(),
                        value.ty.clone(),
                        Some(*span),
                        LocalOptions {
                            synthetic: false,
                            parameter: false,
                        },
                    )
                });
                if self.lower_root_suspend(value, Some(local)).is_none() {
                    let value = self
                        .lower_nested_suspend(value)
                        .unwrap_or_else(|| value.clone());
                    self.push(SourceInst::Assign {
                        local,
                        name: name.clone(),
                        value,
                    });
                }
            }
            HirStmt::FieldAssign {
                object,
                field,
                value,
                ..
            } => {
                let operands = self
                    .lower_assign_operands(&[object, value])
                    .unwrap_or_else(|| vec![object.clone(), value.clone()]);
                let [object, value] = operands
                    .try_into()
                    .unwrap_or_else(|_| unreachable!("two operands in, two operands out"));
                self.push(SourceInst::FieldAssign {
                    object,
                    field: field.clone(),
                    value,
                });
            }
            HirStmt::IndexAssign {
                array,
                index,
                value,
                ..
            } => {
                let operands = self
                    .lower_assign_operands(&[array, index, value])
                    .unwrap_or_else(|| vec![array.clone(), index.clone(), value.clone()]);
                let [array, index, value] = operands
                    .try_into()
                    .unwrap_or_else(|_| unreachable!("three operands in, three operands out"));
                self.push(SourceInst::IndexAssign {
                    array,
                    index,
                    value,
                });
            }
            HirStmt::StaticFieldAssign {
                class,
                field,
                value,
                ..
            } => {
                let operands = self
                    .lower_assign_operands(&[value])
                    .unwrap_or_else(|| vec![value.clone()]);
                let [value] = operands
                    .try_into()
                    .unwrap_or_else(|_| unreachable!("one operand in, one operand out"));
                self.push(SourceInst::StaticFieldAssign {
                    class: *class,
                    field: field.clone(),
                    value,
                });
            }
            HirStmt::SuperInit { args, span } => {
                if !self.lower_super_init_values(args, *span) {
                    self.push(SourceInst::SuperInit {
                        args: args.clone(),
                        span: *span,
                    });
                }
            }
            HirStmt::Expr(e) => {
                if let HirExprKind::Select { cases } = &e.kind {
                    self.lower_select(cases, e.span);
                } else if self.lower_root_suspend(e, None).is_none() {
                    let value = self.lower_nested_suspend(e).unwrap_or_else(|| e.clone());
                    self.push(SourceInst::Expr(value));
                }
            }
            HirStmt::Return { value, .. } => {
                // The returned value is computed BEFORE the defers run: a
                // deferred body can mutate what the expression reads.
                let value = if self.defer_depth == 0 {
                    if let Some(value) = value.as_ref() {
                        {
                            let destination = if value.ty == Type::Void {
                                None
                            } else {
                                let name = self.synthetic_name("return");
                                Some(self.declare_local(
                                    name,
                                    value.ty.clone(),
                                    None,
                                    LocalOptions {
                                        synthetic: true,
                                        parameter: false,
                                    },
                                ))
                            };
                            match self.lower_root_suspend(value, destination) {
                                Some(Some(value)) => Some(value),
                                // A `void` operand that lowering already turned
                                // into blocks — `return await sleep(1);`, a `void`
                                // `match` — has nothing left to return. Re-reading
                                // the operand here would put the suspension back
                                // into the terminator.
                                Some(None) => None,
                                None => Some(
                                    self.lower_nested_suspend(value)
                                        .unwrap_or_else(|| value.clone()),
                                ),
                            }
                        }
                    } else {
                        None
                    }
                } else {
                    if let Some(value) = value.as_ref() {
                        'returned: {
                            if value.ty == Type::Void {
                                if self.lower_root_suspend(value, None).is_none() {
                                    let value = self
                                        .lower_nested_suspend(value)
                                        .unwrap_or_else(|| value.clone());
                                    self.push(SourceInst::Expr(value));
                                }
                                break 'returned None;
                            }

                            let name = self.synthetic_name("return");
                            let destination = self.declare_local(
                                name.clone(),
                                value.ty.clone(),
                                None,
                                LocalOptions {
                                    synthetic: true,
                                    parameter: false,
                                },
                            );
                            if self.lower_root_suspend(value, Some(destination)).is_none() {
                                let evaluated = self
                                    .lower_nested_suspend(value)
                                    .unwrap_or_else(|| value.clone());
                                self.push_existing_let(
                                    destination,
                                    name,
                                    false,
                                    value.ty.clone(),
                                    evaluated,
                                    None,
                                );
                            }
                            Some(self.local_expr(destination, value.span))
                        }
                    } else {
                        None
                    }
                };
                self.flush_defers_down_to(0);
                self.terminate(SourceTerminator::Return(value));
                // Anything after a return is unreachable; give it a fresh
                // predecessor-less block rather than corrupting this one.
                let dead = self.new_block();
                self.switch_to(dead);
            }
            HirStmt::Break { .. } => {
                let frame = *self.loop_stack.last().expect("break outside loop");
                self.flush_defers_down_to(frame.defer_depth);
                self.clear_scope_roots_down_to(frame.scope_depth);
                self.terminate(SourceTerminator::Jump(frame.exit));
                let dead = self.new_block();
                self.switch_to(dead);
            }
            HirStmt::Defer {
                id: hir_id,
                body,
                span,
            } => {
                let id = self
                    .defer_scopes
                    .last()
                    .and_then(|scope| scope.get(hir_id))
                    .copied()
                    .expect("defer outside its LIR scope");
                let expression;
                let (stmts, frozen) = match body {
                    HirDeferBody::Expr(expr) => {
                        let (call, inputs) = self.capture_defer_expr(id, expr);
                        expression = HirStmt::Expr(call);
                        (std::slice::from_ref(&expression), inputs)
                    }
                    HirDeferBody::Block(stmts) => (stmts.as_slice(), None),
                };
                let plan = self
                    .nested_defer_access
                    .as_ref()
                    .and_then(|sites| sites.get(hir_id));
                let mut nested = self.nested_defer_access.clone();
                let (inputs, recovery_capable) = if let Some(inputs) = frozen {
                    (
                        inputs,
                        plan.map_or_else(|| defer_body_contains_recover(body), |p| p.recovers),
                    )
                } else {
                    let root;
                    let access = if let Some(access) = plan {
                        access
                    } else {
                        let mut sites = HashMap::new();
                        root = defer_captures::analyze(body, &mut sites);
                        nested = (!sites.is_empty()).then(|| std::rc::Rc::new(sites));
                        &root
                    };
                    let mut inputs: Vec<_> = access
                        .names
                        .iter()
                        .filter_map(|name| self.local_by_name.get(name.as_ref()).copied())
                        .collect();
                    inputs.sort_unstable_by_key(|id| id.0);
                    (inputs, access.recovers)
                };
                let body = self.lower_defer_region(stmts, recovery_capable, &inputs, nested);
                self.push(SourceInst::Defer {
                    id,
                    body,
                    span: *span,
                });
            }
            HirStmt::Continue { .. } => {
                let frame = *self.loop_stack.last().expect("continue outside loop");
                self.flush_defers_down_to(frame.defer_depth);
                self.clear_scope_roots_down_to(frame.scope_depth);
                self.terminate(SourceTerminator::Jump(frame.next));
                let dead = self.new_block();
                self.switch_to(dead);
            }
            HirStmt::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                if let Some(branches) =
                    constant_if_branches(cond, then_branch, else_branch.as_deref())
                {
                    if let Some(taken) = branches.taken {
                        self.lower_scope(taken);
                    }
                    return;
                }
                let cond = self.lower_condition(cond);
                let then_block = self.new_block();
                let merge_block = self.new_block();
                let else_block = match else_branch {
                    Some(_) => self.new_block(),
                    None => merge_block,
                };
                self.terminate(SourceTerminator::Branch {
                    cond,
                    then_block,
                    else_block,
                });

                self.switch_to(then_block);
                self.lower_scope(then_branch);
                self.terminate(SourceTerminator::Jump(merge_block));

                if let Some(else_branch) = else_branch {
                    self.switch_to(else_block);
                    self.lower_scope(else_branch);
                    self.terminate(SourceTerminator::Jump(merge_block));
                }

                self.switch_to(merge_block);
            }
            HirStmt::While { cond, body, .. } => {
                // Source flow sees the parser's ordinary loop/loan scopes;
                // the ownership transfer itself is a dedicated CFG edge.
                if let [lock @ HirStmt::Lock { walk: true, .. }] = body.as_slice() {
                    self.lower_stmt(lock);
                    return;
                }
                let header = self.new_block();
                self.terminate(SourceTerminator::Jump(header));
                self.switch_to(header);
                if self.cleanup_name.is_some() && !while_body_is_reachable(cond) {
                    // Keep the reachable header and exit from CFG pruning. The skipped
                    // body can mention bindings this cleanup does not import.
                    let exit = self.new_block();
                    self.terminate(SourceTerminator::Jump(exit));
                    self.switch_to(exit);
                    return;
                }
                let body_block = self.new_block();
                let backedge = if self.is_async {
                    self.new_block()
                } else {
                    header
                };
                let exit = self.new_block();
                let cond = self.lower_condition(cond);
                self.terminate(SourceTerminator::Branch {
                    cond,
                    then_block: body_block,
                    else_block: exit,
                });

                self.switch_to(body_block);
                self.loop_stack.push(LirLoopFrame {
                    exit,
                    next: backedge,
                    defer_depth: self.defer_depth,
                    scope_depth: self.scope_starts.len(),
                });
                self.lower_scope(body);
                self.loop_stack.pop();
                self.terminate(SourceTerminator::Jump(backedge));

                if self.is_async {
                    self.switch_to(backedge);
                    self.terminate(SourceTerminator::Suspend {
                        operation: SuspendOp::Preempt,
                        resume: header,
                    });
                }

                self.switch_to(exit);
            }
            HirStmt::For {
                name,
                iterable,
                body,
                span,
            } => self.lower_for(name, iterable, body, *span),
            HirStmt::Lock {
                walk,
                additional,
                mode,
                target,
                binding,
                body,
                span,
                ..
            } => self.lower_lock(*mode, target, binding, additional, body, (*span, *walk)),
        }
    }

    /// `lock <target> as [mut] <binding> { .. }`.
    ///
    /// The critical section becomes a suspension edge plus a forced defer
    /// scope:
    ///
    /// ```text
    ///   <handle> = target                  // evaluated exactly ONCE
    ///   suspend LockAcquire -> body        // parks and re-polls until owned
    ///   body: EnterDeferScope .. LeaveDeferScope
    ///   ReleaseLock                        // fallthrough exit
    /// ```
    ///
    /// The target is hoisted into its own local because a suspension's operands
    /// must be locals: a resumed poll re-enters at the acquisition and reloads
    /// the handle from the frame, so a side-effecting target expression must
    /// never be re-evaluated. The token and phase locals are compiler-owned
    /// state with no initialiser — the runtime writes the token through a
    /// pointer to its slot — and exist here only so that liveness gives each a
    /// frame slot of its own.
    ///
    /// The scope is forced open even for a section that defers nothing, because
    /// the section's cleanup block is what releases the lock when a panic
    /// leaves it. Every other exit carries an explicit
    /// [`SourceInst::ReleaseLock`]: the fallthrough one is emitted here, and
    /// `return`/`break`/`continue` get theirs from
    /// [`Self::flush_defers_down_to`].
    pub(super) fn prepare_lock(
        &mut self,
        mode: LockMode,
        target: &HirExpr,
        binding: &str,
        span: Span,
    ) -> LirLockSlots {
        let value_ty = match &target.ty {
            Type::Generic(_, args) if args.len() == 1 => args[0].clone(),
            // The checker rejects any other lock target, so this only has to
            // keep lowering total.
            other => other.clone(),
        };
        let binding_local = self.declare_local(
            binding.to_string(),
            value_ty.clone(),
            Some(span),
            LocalOptions {
                synthetic: false,
                parameter: false,
            },
        );

        let handle_name = self.synthetic_name("lock_handle");
        let handle = self.declare_local(
            handle_name.clone(),
            target.ty.clone(),
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        if self.lower_root_suspend(target, Some(handle)).is_none() {
            let evaluated = self
                .lower_nested_suspend(target)
                .unwrap_or_else(|| target.clone());
            self.push_existing_let(
                handle,
                handle_name,
                false,
                target.ty.clone(),
                evaluated,
                None,
            );
        }
        let token_name = self.synthetic_name("lock_token");
        let token = self.declare_local(
            token_name,
            Type::I64,
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );
        let phase_name = self.synthetic_name("lock_phase");
        let phase = self.declare_local(
            phase_name,
            Type::I64,
            None,
            LocalOptions {
                synthetic: true,
                parameter: false,
            },
        );

        LirLockSlots {
            members: Vec::new(),
            mode,
            handle,
            token,
            phase,
            binding: binding_local,
            value_ty,
        }
    }

    pub(super) fn lower_lock(
        &mut self,
        mode: LockMode,
        target: &HirExpr,
        binding: &str,
        additional: &[crate::ir::typed_ast::HirLockTarget],
        body: &[HirStmt],
        location: (Span, bool),
    ) {
        let (span, walk) = location;
        if !self.is_async {
            self.lower_scope(body);
            return;
        }
        let first = self.prepare_lock(mode, target, binding, span);
        let slots = if additional.is_empty() {
            first
        } else {
            let mut members = vec![first];
            for member in additional {
                members.push(self.prepare_lock(
                    member.mode,
                    &member.target,
                    &member.binding,
                    member.target.span,
                ));
            }
            let mut local = |suffix: &str| {
                let name = self.synthetic_name(suffix);
                self.declare_local(
                    name,
                    Type::I64,
                    None,
                    LocalOptions {
                        synthetic: true,
                        parameter: false,
                    },
                )
            };
            LirLockSlots {
                members,
                mode: LockMode::Group,
                handle: local("lock_group"),
                token: local("lock_group_token"),
                phase: local("lock_group_phase"),
                binding: local("lock_group_value"),
                value_ty: Type::I64,
            }
        };

        let entry = self.new_block();
        self.terminate(SourceTerminator::Suspend {
            operation: SuspendOp::LockAcquire {
                slots: slots.clone(),
                span,
            },
            resume: entry,
        });
        self.switch_to(entry);

        self.active_locks.push(ActiveLock {
            slots: slots.clone(),
            defer_depth: self.defer_depth,
        });
        self.lower_scope_inner(body, Some(slots.clone()), walk.then_some((target, span)));
        self.active_locks.pop();
        self.push(SourceInst::ReleaseLock(slots));
    }

    /// One iteration has a fresh binding scope and defers. Both `continue`
    /// and fallthrough take the same transfer edge; `break` reaches the outer
    /// lock scope's release. Return/panic/cancel use its ordinary cleanup.
    fn lower_lock_walk(
        &mut self,
        body: &[HirStmt],
        slots: &LirLockSlots,
        target: &HirExpr,
        span: Span,
    ) {
        let entry = self.new_block();
        let handoff = self.new_block();
        let exit = self.new_block();
        self.terminate(SourceTerminator::Jump(entry));
        self.switch_to(entry);
        self.loop_stack.push(LirLoopFrame {
            exit,
            next: handoff,
            defer_depth: self.defer_depth,
            scope_depth: self.scope_starts.len(),
        });
        self.lower_scope(body);
        self.loop_stack.pop();
        self.terminate(SourceTerminator::Jump(handoff));
        self.switch_to(handoff);
        // A source variable may read through a mutable-capture cell in HIR.
        // Evaluate that storage after the iteration's defers, on every handoff,
        // and root the loaded handle across value boxing and preemption. Plain
        // locals already have the required slot and need no additional copy.
        let next = if let HirExprKind::Var(name) = &target.kind {
            self.local_by_name[name]
        } else {
            let name = self.synthetic_name("lock_next");
            let next = self.push_synth_let(&name, false, target.clone());
            self.scope_roots.push(next);
            next
        };
        self.terminate(SourceTerminator::Suspend {
            operation: SuspendOp::LockHandoff {
                slots: slots.clone(),
                next,
                span,
            },
            resume: entry,
        });
        self.switch_to(exit);
    }

    /// Desugar `for` into a while-shaped header/body/exit with an induction
    /// variable: bound-based for ranges, index-based for arrays.
    pub(super) fn lower_for(
        &mut self,
        name: &str,
        iterable: &HirExpr,
        body: &[HirStmt],
        span: crate::diagnostics::Span,
    ) {
        let n = self.for_counter;
        self.for_counter += 1;
        let i_name = format!("__for{n}_i$");
        // The whole `for` construct is a scope of its own.
        // The desugaring hoists the iterable into a synthetic `let` that lives
        // as long as the loop does; nothing in the source scope holds it, so
        // without a close of its own that temp keeps the array — and everything
        // in it — reachable until the function returns.
        let construct = self.scope_starts.len();
        self.scope_starts.push(LirScopeMark::opening_at(
            self.locals.len(),
            self.scope_roots.len(),
        ));
        let i64_var = |name: &str| HirExpr {
            kind: HirExprKind::Var(name.to_string()),
            ty: Type::I64,
            span,
        };
        let lt = |lhs: HirExpr, rhs: HirExpr| HirExpr {
            kind: HirExprKind::Binary {
                op: crate::parser::ast::BinOp::Lt,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
            ty: Type::Bool,
            span,
        };
        let plus_one = |var: HirExpr| HirExpr {
            kind: HirExprKind::Binary {
                op: crate::parser::ast::BinOp::Add,
                lhs: Box::new(var),
                rhs: Box::new(HirExpr {
                    kind: HirExprKind::Int(1),
                    ty: Type::I64,
                    span,
                }),
            },
            ty: Type::I64,
            span,
        };

        // Entry instructions, the loop-variable binding for the body, and the
        // header's upper bound. A range's bound is evaluated once up front; an
        // array's length is re-read on every header entry, so a `push`/`pop`
        // inside the body changes how far the walk goes.
        let bound_expr: HirExpr;
        let element_binding: HirExpr;
        let literal_bounds = match &iterable.kind {
            HirExprKind::Range { start, end } => Some((start, end)),
            ir_variants!(
                HirExprKind;
                Int, Float, Bool, Str, Var, FnRef, Binary, Unary, Call, Invoke, Print, Array,
                Index, Ternary, New, FieldAccess, MethodCall, ObjectLiteral, StaticField,
                StaticCall, ReferenceArg, Await, TryPropagate, Lambda, Match, Select
            ) => None,
        };
        match (literal_bounds, &iterable.ty) {
            // for x in start..end  →  i = start; while i < end { x = i; .. }
            (Some((start, end)), _) => {
                let bound_name = format!("__for{n}_end$");
                self.push_synth_let(&i_name, true, (**start).clone());
                self.push_synth_let(&bound_name, false, (**end).clone());
                bound_expr = HirExpr {
                    kind: HirExprKind::Var(bound_name),
                    ty: Type::I64,
                    span,
                };
                element_binding = i64_var(&i_name);
            }
            // for x in arr  →  a = arr; i = 0; while i < a.len() { x = a[i]; .. }
            (None, ty)
                if matches!(ty, Type::Array(_))
                    || builtin_types::unary_arg(ty, B::FrozenArray).is_some() =>
            {
                let elem = match ty {
                    Type::Array(elem) => elem.as_ref(),
                    // The guard above admits FrozenArray as the other representation.
                    _ => builtin_types::unary_arg(ty, B::FrozenArray).unwrap(),
                };
                let arr_name = format!("__for{n}_arr$");
                let arr_var = HirExpr {
                    kind: HirExprKind::Var(arr_name.clone()),
                    ty: iterable.ty.clone(),
                    span,
                };
                let arr_local = self.push_synth_let(&arr_name, false, iterable.clone());
                self.scope_roots.push(arr_local);
                self.push_synth_let(
                    &i_name,
                    true,
                    HirExpr {
                        kind: HirExprKind::Int(0),
                        ty: Type::I64,
                        span,
                    },
                );
                let (length, spelling) = if matches!(arr_var.ty, Type::Array(_)) {
                    (
                        Intrinsic::ArrayLen,
                        crate::semantic::intrinsics::ARRAY_LENGTH,
                    )
                } else {
                    (
                        Intrinsic::FrozenArrayLen,
                        crate::semantic::intrinsics::LENGTH,
                    )
                };
                // Not hoisted into a `let`: the header re-evaluates it, so a
                // body that grows or shrinks the array is observed.
                bound_expr = HirExpr {
                    kind: HirExprKind::MethodCall {
                        object: Box::new(arr_var.clone()),
                        builtin: Some(BuiltinMethod::Intrinsic(length)),
                        method: spelling.into(),
                        args: vec![],
                        super_class: None,
                    },
                    ty: Type::I64,
                    span,
                };
                element_binding = HirExpr {
                    kind: HirExprKind::Index {
                        array: Box::new(arr_var),
                        index: Box::new(i64_var(&i_name)),
                    },
                    ty: elem.clone(),
                    span,
                };
            }
            // A range held as a VALUE (`let r = 0..3; for x in r`, or a call
            // that returns one) — for x in r  →  rng = r; i = rng.start;
            // end = rng.end; while i < end { x = i; .. }.
            //
            // Both bounds are read once before the loop. A `Range<i64>` is
            // immutable, so re-reading them per iteration could only cost
            // loads, and hoisting the value itself keeps a call in the iterable
            // position from running twice.
            (None, Type::Generic(g, args))
                if g.namespace().is_none()
                    && B::from_name(g.name().as_ref()) == Some(B::Range)
                    && args.first() == Some(&Type::I64) =>
            {
                let range_name = format!("__for{n}_range$");
                let bound_name = format!("__for{n}_end$");
                let range_local = self.push_synth_let(&range_name, false, iterable.clone());
                self.scope_roots.push(range_local);
                let bound = |field: &str| HirExpr {
                    kind: HirExprKind::FieldAccess {
                        object: Box::new(HirExpr {
                            kind: HirExprKind::Var(range_name.clone()),
                            ty: iterable.ty.clone(),
                            span,
                        }),
                        field: field.to_string(),
                    },
                    ty: Type::I64,
                    span,
                };
                self.push_synth_let(&i_name, true, bound("start"));
                self.push_synth_let(&bound_name, false, bound("end"));
                bound_expr = HirExpr {
                    kind: HirExprKind::Var(bound_name),
                    ty: Type::I64,
                    span,
                };
                element_binding = i64_var(&i_name);
            }
            (None, _) => {
                // The HIR lowering only produces array/range iterables.
                unreachable!("for over unsupported iterable reached LIR lowering")
            }
        }

        let header = self.new_block();
        let body_block = self.new_block();
        // Dedicated increment block: `continue` jumps HERE so the induction
        // variable still advances.
        let inc_block = self.new_block();
        let exit = self.new_block();

        self.terminate(SourceTerminator::Jump(header));
        self.switch_to(header);
        self.terminate(SourceTerminator::Branch {
            cond: lt(i64_var(&i_name), bound_expr),
            then_block: body_block,
            else_block: exit,
        });

        self.switch_to(body_block);
        // One iteration is a scope of its own, opened around the element
        // binding: lowering emits that `let` ahead of the body's own scope, and
        // flags it synthetic because it synthesized it from the iteration
        // protocol, but the binding is the source loop variable and its root
        // ends with the iteration like any other.
        let iteration = self.scope_starts.len();
        self.scope_starts.push(LirScopeMark::opening_at(
            self.locals.len(),
            self.scope_roots.len(),
        ));
        let element = self.push_synth_let(name, false, element_binding);
        self.scope_roots.push(element);
        self.loop_stack.push(LirLoopFrame {
            exit,
            next: inc_block,
            defer_depth: self.defer_depth,
            scope_depth: iteration,
        });
        self.lower_scope(body);
        self.loop_stack.pop();
        self.push_scope_root_clears(iteration);
        self.pop_root_scope();
        self.terminate(SourceTerminator::Jump(inc_block));

        self.switch_to(inc_block);
        let local = self.local_by_name[&i_name];
        self.push(SourceInst::Assign {
            local,
            name: i_name.clone(),
            value: plus_one(i64_var(&i_name)),
        });
        if self.is_async {
            self.terminate(SourceTerminator::Suspend {
                operation: SuspendOp::Preempt,
                resume: header,
            });
        } else {
            self.terminate(SourceTerminator::Jump(header));
        }

        self.switch_to(exit);
        // Both ways out of the loop land here — fallthrough from the header and
        // every `break` — so the iterable temp's root is dropped once, on the
        // one path that leaves the construct.
        self.push_scope_root_clears(construct);
        self.pop_root_scope();
    }
}
