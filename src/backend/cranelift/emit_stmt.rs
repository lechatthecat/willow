use cranelift_codegen::ir::{
    InstBuilder, MemFlagsData, StackSlotData, StackSlotKind, condcodes::IntCC, types,
};
use cranelift_module::Module;

use super::*;

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Emit `expr`, then coerce the result to `target_ty` (class→interface box).
    pub(super) fn emit_expr_coerced(
        &mut self,
        expr: &Expr,
        target_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        // An array literal with a known target element type emits its elements
        // boxed to that type (so `let a: Array<Animal> = [Dog {}]` stores boxes,
        // and `[]` becomes a reference-element array).
        if let Expr::ArrayLiteral(elements, _) = expr {
            let target_inner = match target_ty {
                Type::Nullable(inner) => inner.as_ref(),
                other => other,
            };
            if let Type::Array(elem) = target_inner {
                let elem = (**elem).clone();
                return self.emit_array_literal(elements, &elem);
            }
        }
        let value = self.emit_expr(expr);
        let value_ty = self.ast_type_of(expr);
        self.coerce_to_target(value, &value_ty, target_ty)
    }

    pub(super) fn emit_block(&mut self, block: &Block) {
        let saved_vars = self.vars.clone();
        let saved_sync_defer_flags = self.sync_defer_flags.clone();
        let gc_roots_before = self.gc_root_count;
        // `emit_block` is also used inside cooperative match arms and deferred
        // blocks, outside `emit_coop_stmts`' scope wrapper. Preserve the
        // compile-time binding-root depth here as well; otherwise the runtime
        // roots and `gc_root_count` are popped below while `active` retains a
        // stale slot and the next poll boundary trips the p42j invariant.
        let coop_roots_before = self
            .coop_shadow_roots
            .as_ref()
            .map(|roots| roots.active.len());
        self.defer_stack.push(Vec::new());
        let defer_depth = self.defer_stack.len() - 1;
        let owns_defer = block
            .stmts
            .iter()
            .any(|stmt| matches!(stmt, Stmt::Defer(_)));
        let owns_sync_defer = !self.is_async && self.coop_frame.is_none() && owns_defer;
        if owns_sync_defer {
            // A panic can branch here before a later defer statement executes,
            // so every source-site flag must be initialized at scope entry.
            for stmt in &block.stmts {
                let Stmt::Defer(defer) = stmt else {
                    continue;
                };
                let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    0,
                ));
                let zero = self.builder.ins().iconst(types::I64, 0);
                self.stack_store(zero, slot);
                self.sync_defer_flags.insert(defer.span, slot);
            }
        }

        // Only defer-owning synchronous scopes need a cleanup/continuation
        // pair. A panic in intervening scopes jumps directly to the nearest
        // such owner and abandons the whole lexical region.
        let panic_scope =
            (owns_sync_defer || (self.coop_frame.is_some() && owns_defer)).then(|| {
                let root_depth_at_entry = if self.coop_frame.is_some() {
                    self.panic_function_root_depth
                        .expect("poll root depth snapshot")
                } else {
                    self.emit_value_runtime_call("willow_root_depth", &[])
                };
                super::PanicScope {
                    cleanup: self.builder.create_block(),
                    resume: self.builder.create_block(),
                    root_depth_at_entry,
                    defer_depth,
                    vars_before: saved_vars.clone(),
                    coop_root_depth_at_entry: coop_roots_before,
                }
            });
        if let Some(scope) = panic_scope.clone() {
            self.panic_scopes.push(scope);
        }

        for stmt in &block.stmts {
            if self.terminated {
                break;
            }
            self.emit_stmt(stmt);
        }

        // Scope fallthrough: run this scope's defers (LIFO), THEN pop the
        // block's GC roots (the defers read their operands from rooted slots).
        // On terminated paths the return/break/`?` handler already flushed.
        let mut normal_reaches_resume = false;
        if !self.terminated {
            self.emit_flush_defers_from(defer_depth);
            if !self.terminated {
                let block_roots = self.gc_root_count - gc_roots_before;
                if block_roots > 0 {
                    self.emit_pop_roots_n(block_roots);
                }
                if let Some(scope) = &panic_scope {
                    self.builder.ins().jump(scope.resume, &[]);
                }
                normal_reaches_resume = true;
            }
        }

        // Emit exactly one abnormal cleanup path for this lexical defer owner.
        // All panic sites in the scope (including nested cleanup panics) jump
        // to this block and consult the pre-zeroed registration flags.
        if let Some(scope) = &panic_scope {
            self.emit_shared_panic_cleanup(scope);
            self.builder.seal_block(scope.cleanup);
        }
        if panic_scope.is_some() {
            self.panic_scopes.pop();
        }
        self.defer_stack.pop();
        // Restore scope: gc_root_count goes back to what it was before the block
        // (in the terminated path the return handler already popped all roots).
        self.gc_root_count = gc_roots_before;
        if let (Some(depth), Some(roots)) = (coop_roots_before, self.coop_shadow_roots.as_mut()) {
            assert!(
                roots.active.len() >= depth,
                "cooperative block root scope underflow"
            );
            roots.active.truncate(depth);
        }
        self.vars = saved_vars;
        self.sync_defer_flags = saved_sync_defer_flags;
        if let Some(scope) = panic_scope {
            let recovery_reaches_resume = self.panic_recovery_targets.remove(&scope.resume);
            if normal_reaches_resume || recovery_reaches_resume {
                self.builder.switch_to_block(scope.resume);
                self.builder.seal_block(scope.resume);
                self.terminated = false;
            } else {
                self.terminated = true;
            }
        }
    }

    /// Run every registered defer from frame `depth` outward, innermost frame
    /// first, newest registration first (LIFO). Frames are left in place —
    /// scope bookkeeping pops them (willow-vynv.2).
    pub(super) fn emit_flush_defers_from(&mut self, depth: usize) {
        let frames: Vec<Vec<super::DeferEntry>> = self.defer_stack[depth..].to_vec();
        let unavailable_before = self.unavailable_defer_ids.clone();
        'frames: for frame in frames.iter().rev() {
            for entry in frame.iter().rev() {
                if self.unavailable_defer_ids.contains(&entry.id) {
                    continue;
                }
                // Consume before entering user code. If this action panics, its
                // nested unwind sees the registration as unavailable and cannot
                // execute it a second time.
                self.unavailable_defer_ids.insert(entry.id);
                self.vars = entry.vars_at_registration.clone();
                // Rebind the hidden frame operands: coop loop bodies restore
                // `vars`, wiping the names between registration and a
                // function-exit flush (willow-vynv.3).
                for (name, offset, ty) in &entry.bindings {
                    self.vars.insert(
                        name.clone(),
                        VarStorage::Frame {
                            offset: *offset,
                            ty: ty.clone(),
                        },
                    );
                }
                // Async: leave REGISTERED before entering user cleanup code.
                // A future recoverable panic may unwind out of that code; if
                // the flag stayed set until afterwards, cancellation/recovery
                // could run this same registration twice (willow-s9ej.1).
                if let (Some(off), Some(frame_ptr)) = (entry.flag_offset, self.coop_frame) {
                    let zero = self.builder.ins().iconst(types::I64, 0);
                    self.builder
                        .ins()
                        .store(MemFlagsData::new(), zero, frame_ptr, off);
                }
                if let Some(slot) = entry.sync_flag_slot {
                    let zero = self.builder.ins().iconst(types::I64, 0);
                    self.stack_store(zero, slot);
                }
                // Normal-exit cleanup is NOT a recovery capability. The runtime
                // panic-defer depth is per execution context, so a defer that
                // runs while an OUTER frame is unwinding would otherwise be able
                // to consume that frame's panic — a helper called from panic
                // cleanup could steal its caller's panic (willow-s9ej.7). Only
                // the cleanup emitted for the unwinding scope itself may
                // recover; here `recover()` lowers to a constant `None`.
                let eligible_before = self.recover_eligible_depth;
                self.recover_eligible_depth = 0;
                self.emit_deferred_action(&entry.action);
                self.recover_eligible_depth = eligible_before;
                if self.terminated {
                    break 'frames;
                }
            }
        }
        self.unavailable_defer_ids = unavailable_before;
    }

    pub(super) fn emit_deferred_action(&mut self, action: &super::DeferredAction) {
        match action {
            super::DeferredAction::Stmt(stmt) => self.emit_stmt(stmt),
            super::DeferredAction::Block(block) => self.emit_block(block),
        }
    }

    /// Branch an already-raised synchronous language panic to the nearest
    /// shared lexical cleanup. The cleanup flags, rather than duplicated AST,
    /// decide which registrations were active at this exact panic site.
    pub(super) fn emit_sync_panic_unwind(&mut self) {
        let codegen_depth_before = self.panic_defer_codegen_depth;
        let eligible_depth_before = self.recover_eligible_depth;
        let callstack_depth_before = self.callstack_frame_depth;
        for _ in 0..codegen_depth_before {
            self.emit_void_runtime_call("willow_panic_leave_defer", &[]);
        }
        // The raise-time diagnostic already owns a snapshot. Balance every
        // caller-owned debug frame before recovery can resume normal code.
        for _ in 0..callstack_depth_before {
            self.emit_callstack_pop();
        }
        if self.build_mode == BuildMode::Debug {
            self.emit_debug_reference_call_clear();
        }
        self.panic_defer_codegen_depth = 0;
        self.recover_eligible_depth = 0;
        if let Some(scope) = self.panic_scopes.last() {
            self.builder.ins().jump(scope.cleanup, &[]);
        } else if let Some(panic_return) = self.panic_return_block {
            self.builder.ins().jump(panic_return, &[]);
        } else {
            if self.coop_frame.is_some() {
                self.emit_unhandled_panic_exit();
            } else {
                if self.gc_root_count > 0 {
                    self.emit_pop_roots_n(self.gc_root_count);
                }
                self.emit_unhandled_panic_exit();
            }
        }
        self.panic_defer_codegen_depth = codegen_depth_before;
        self.recover_eligible_depth = eligible_depth_before;
        self.callstack_frame_depth = callstack_depth_before;
        self.terminated = true;
    }

    /// Emit one shared panic cleanup block for a defer-owning lexical scope.
    /// Synchronous scopes use stack flags; cooperative async scopes use frame
    /// flags. Both consume a registration before user cleanup, so a nested
    /// panic cannot run the same action twice.
    pub(super) fn emit_shared_panic_cleanup(&mut self, scope: &super::PanicScope) {
        let vars_before = self.vars.clone();
        let roots_before = self.gc_root_count;
        let coop_active_before = self
            .coop_shadow_roots
            .as_ref()
            .map(|roots| roots.active.clone());
        let unavailable_before = self.unavailable_defer_ids.clone();
        let codegen_depth_before = self.panic_defer_codegen_depth;
        let eligible_depth_before = self.recover_eligible_depth;

        self.builder.switch_to_block(scope.cleanup);
        self.terminated = false;
        if let Some(depth) = scope.coop_root_depth_at_entry {
            self.gc_root_count = depth;
            self.coop_shadow_roots
                .as_mut()
                .expect("cooperative panic scope requires a root tracker")
                .active
                .truncate(depth);
        }
        let entries = self.defer_stack[scope.defer_depth].clone();
        let scope_can_recover = entries.iter().any(|entry| entry.recovery_capable);

        for entry in entries.iter().rev() {
            let run = self.builder.create_block();
            let next = self.builder.create_block();
            let flag = if let Some(slot) = entry.sync_flag_slot {
                self.stack_load(types::I64, slot)
            } else {
                let frame = self
                    .coop_frame
                    .expect("async panic cleanup requires a cooperative frame");
                let offset = entry
                    .flag_offset
                    .expect("async panic cleanup requires a frame flag");
                self.builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), frame, offset)
            };
            let registered = self.builder.ins().icmp_imm_u(IntCC::NotEqual, flag, 0);
            self.builder.ins().brif(registered, run, &[], next, &[]);

            self.builder.switch_to_block(run);
            self.builder.seal_block(run);
            let zero = self.builder.ins().iconst(types::I64, 0);
            if let Some(slot) = entry.sync_flag_slot {
                self.stack_store(zero, slot);
            } else {
                let frame = self
                    .coop_frame
                    .expect("async panic cleanup requires a cooperative frame");
                let offset = entry
                    .flag_offset
                    .expect("async panic cleanup requires a frame flag");
                self.builder
                    .ins()
                    .store(MemFlagsData::new(), zero, frame, offset);
            }
            self.vars = entry.vars_at_registration.clone();
            for (name, offset, ty) in &entry.bindings {
                self.vars.insert(
                    name.clone(),
                    VarStorage::Frame {
                        offset: *offset,
                        ty: ty.clone(),
                    },
                );
            }

            self.emit_void_runtime_call("willow_panic_enter_defer", &[]);
            self.panic_defer_codegen_depth = codegen_depth_before + 1;
            self.recover_eligible_depth =
                eligible_depth_before + if entry.recovery_capable { 1 } else { 0 };
            self.emit_deferred_action(&entry.action);
            if !self.terminated {
                self.emit_void_runtime_call("willow_panic_leave_defer", &[]);
                self.builder.ins().jump(next, &[]);
            }

            // The flag-false predecessor always reaches `next`, even when the
            // run predecessor raised a nested panic and terminated.
            self.builder.switch_to_block(next);
            self.builder.seal_block(next);
            self.terminated = false;
            self.panic_defer_codegen_depth = codegen_depth_before;
            self.recover_eligible_depth = eligible_depth_before;
            self.vars = vars_before.clone();
        }

        // The number of roots pushed in this scope depends on the panic path.
        // Restore the exact entry depth dynamically instead of baking in the
        // final source-order count.
        let current_depth = self.emit_value_runtime_call("willow_root_depth", &[]);
        let target_depth = if let Some(depth) = scope.coop_root_depth_at_entry {
            let depth = self.builder.ins().iconst(types::I32, depth as i64);
            self.builder.ins().iadd(scope.root_depth_at_entry, depth)
        } else {
            scope.root_depth_at_entry
        };
        let roots_to_pop = self.builder.ins().isub(current_depth, target_depth);
        self.emit_void_runtime_call("willow_pop_roots", &[roots_to_pop]);
        self.vars = scope.vars_before.clone();
        if let Some(depth) = scope.coop_root_depth_at_entry {
            self.gc_root_count = depth;
            self.coop_shadow_roots
                .as_mut()
                .expect("cooperative panic scope requires a root tracker")
                .active
                .truncate(depth);
        }

        let propagate = if scope_can_recover {
            let active = self.emit_value_runtime_call("willow_panic_active", &[]);
            let propagate = self.builder.create_block();
            self.builder
                .ins()
                .brif(active, propagate, &[], scope.resume, &[]);
            self.panic_recovery_targets.insert(scope.resume);
            self.builder.switch_to_block(propagate);
            self.builder.seal_block(propagate);
            Some(propagate)
        } else {
            None
        };

        let parent_cleanup = self
            .panic_scopes
            .iter()
            .rev()
            .nth(1)
            .map(|parent| parent.cleanup);
        if let Some(parent) = parent_cleanup {
            self.builder.ins().jump(parent, &[]);
        } else if let Some(panic_return) = self.panic_return_block {
            self.builder.ins().jump(panic_return, &[]);
        } else {
            self.emit_unhandled_panic_exit();
        }
        let _ = propagate;
        self.vars = vars_before;
        self.gc_root_count = roots_before;
        self.unavailable_defer_ids = unavailable_before;
        self.panic_defer_codegen_depth = codegen_depth_before;
        self.recover_eligible_depth = eligible_depth_before;
        if let (Some(active), Some(roots)) = (coop_active_before, self.coop_shadow_roots.as_mut()) {
            roots.active = active;
        }
        self.terminated = true;
    }

    /// Leave the generated Willow boundary while preserving the active panic.
    /// Cooperative polls publish the Panicked outcome to the scheduler first;
    /// synchronous native boundaries report and abort here.
    fn emit_unhandled_panic_exit(&mut self) {
        if self.coop_frame.is_some() {
            // Unlike an ordinary suspension boundary, an unhandled panic can
            // be observed immediately after a call while expression
            // temporaries are still rooted.  The native poll frame is being
            // abandoned, so remove every root registered on this CFG path;
            // requiring all of them to be tracked lexical bindings would
            // reject otherwise-valid allocating expressions.
            if self.gc_root_count > 0 {
                self.emit_pop_roots_n(self.gc_root_count);
            }
            let panicked = self.builder.ins().iconst(types::I32, COOP_POLL_PANICKED);
            self.builder.ins().return_(&[panicked]);
        } else {
            self.emit_void_runtime_call("willow_panic_finish_unhandled", &[]);
            self.builder.ins().trap(TrapCode::unwrap_user(1));
        }
    }

    pub(super) fn emit_void_runtime_call(
        &mut self,
        name: &str,
        args: &[cranelift_codegen::ir::Value],
    ) {
        let fid = self.func_id(name);
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        self.builder.ins().call(fref, args);
    }

    pub(super) fn emit_value_runtime_call(
        &mut self,
        name: &str,
        args: &[cranelift_codegen::ir::Value],
    ) -> cranelift_codegen::ir::Value {
        let fid = self.func_id(name);
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, args);
        self.builder.inst_results(call)[0]
    }

    /// Snapshot active panic depth before a participating Willow call. During
    /// panic-defer execution the depth may already be non-zero.
    pub(super) fn emit_pre_willow_call_panic_depth(
        &mut self,
    ) -> Option<cranelift_codegen::ir::Value> {
        self.emit_fault_site();
        Some(self.emit_value_runtime_call("willow_panic_depth", &[]))
    }

    /// Start recoverable-panic propagation for a runtime ABI that is declared
    /// `MAY_PANIC`. Unlike an unclassified string-based call, this makes the
    /// ABI schema participate in code generation and fails closed when a call
    /// site and its runtime effect declaration drift apart.
    pub(super) fn emit_pre_runtime_call_panic_depth(
        &mut self,
        name: &str,
    ) -> Option<cranelift_codegen::ir::Value> {
        let symbol = crate::backend::abi::runtime_symbol(name)
            .unwrap_or_else(|| panic!("runtime call `{name}` is missing from the ABI schema"));
        assert!(
            symbol
                .effects()
                .contains(crate::backend::abi::RuntimeEffects::MAY_PANIC),
            "runtime call `{name}` uses recoverable-panic propagation but its ABI row lacks MAY_PANIC"
        );
        self.emit_pre_willow_call_panic_depth()
    }

    /// Debug builds: publish the statement being executed before a runtime call
    /// that can raise. Faults raised inside a runtime helper (array bounds, a
    /// blocked channel op, an awaited cancelled task) carry no location of
    /// their own, so without this their `PanicInfo` would report `:0:0`
    /// (willow-s9ej.7). Release builds skip it: the store is not worth a call
    /// on every collection access.
    pub(super) fn emit_fault_site(&mut self) {
        if self.build_mode != BuildMode::Debug {
            return;
        }
        let Some(span) = self.fault_site_span else {
            return;
        };
        let file = self.source_file.to_string();
        let Some((file_ptr, file_len)) = self.emit_static_str_bytes(&file) else {
            return;
        };
        let line = self.builder.ins().iconst(types::I64, span.line as i64);
        let column = self.builder.ins().iconst(types::I64, span.col as i64);
        self.emit_void_runtime_call("willow_fault_site_set", &[file_ptr, file_len, line, column]);
    }

    /// Branch away before observing a neutral result only when the callee
    /// added a new panic record (willow-s9ej.4).
    pub(super) fn emit_post_willow_call_panic_check(
        &mut self,
        depth_before: Option<cranelift_codegen::ir::Value>,
    ) {
        let Some(depth_before) = depth_before else {
            return;
        };
        let depth_after = self.emit_value_runtime_call("willow_panic_depth", &[]);
        let raised = self
            .builder
            .ins()
            .icmp(IntCC::UnsignedGreaterThan, depth_after, depth_before);
        let panicked = self.builder.create_block();
        let normal = self.builder.create_block();
        self.builder.ins().brif(raised, panicked, &[], normal, &[]);

        self.builder.switch_to_block(panicked);
        self.builder.seal_block(panicked);
        if self.coop_frame.is_some() {
            self.emit_sync_panic_unwind();
        } else if self.is_async {
            self.emit_void_runtime_call("willow_panic_finish_unhandled", &[]);
            self.builder.ins().trap(TrapCode::unwrap_user(1));
            self.terminated = true;
        } else {
            self.emit_sync_panic_unwind();
        }

        self.builder.switch_to_block(normal);
        self.builder.seal_block(normal);
        self.terminated = false;
    }

    /// Raise a user-visible language fault whose message is already a Willow
    /// String, then leave the current expression through the same lexical
    /// panic path as an explicit `panic(...)`.  The message is kept rooted
    /// while source metadata is materialized.
    pub(super) fn emit_language_panic(
        &mut self,
        message: cranelift_codegen::ir::Value,
        span: Option<crate::diagnostics::Span>,
    ) {
        self.emit_push_root(message);
        let source_file = self.source_file.to_string();
        let file = self.emit_string_literal(&source_file);
        let line = self
            .builder
            .ins()
            .iconst(types::I64, span.map_or(0, |value| value.line) as i64);
        let column = self
            .builder
            .ins()
            .iconst(types::I64, span.map_or(0, |value| value.col) as i64);
        self.emit_void_runtime_call("willow_panic_raise", &[message, file, line, column]);
        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;

        if self.coop_frame.is_some() {
            self.emit_sync_panic_unwind();
        } else if self.is_async {
            self.emit_void_runtime_call("willow_panic_finish_unhandled", &[]);
            self.builder.ins().trap(TrapCode::unwrap_user(1));
            self.terminated = true;
        } else {
            self.emit_sync_panic_unwind();
        }
    }

    /// Materialize the shared abnormal ABI return of a synchronous callee.
    /// Restore the caller's exact root depth, then return a typed neutral value
    /// without consuming or clearing the active panic.
    pub(super) fn emit_panic_return(&mut self, return_ty: &Type, force_void: bool) {
        let Some(block) = self.panic_return_block else {
            return;
        };
        let entry_depth = self
            .panic_function_root_depth
            .expect("panic return requires an entry root-depth snapshot");
        self.builder.switch_to_block(block);
        self.builder.seal_block(block);
        let current_depth = self.emit_value_runtime_call("willow_root_depth", &[]);
        let roots_to_pop = self.builder.ins().isub(current_depth, entry_depth);
        self.emit_void_runtime_call("willow_pop_roots", &[roots_to_pop]);
        if *return_ty == Type::Void || force_void {
            self.builder.ins().return_(&[]);
        } else {
            let zero = match clif_type(return_ty) {
                types::F64 => self.builder.ins().f64const(0.0),
                ty => self.builder.ins().iconst(ty, 0),
            };
            self.builder.ins().return_(&[zero]);
        }
        self.terminated = true;
    }

    /// Register a `defer`. A direct call preserves the original semantics:
    /// evaluate receiver/args NOW into hidden rooted locals and queue a
    /// synthetic call that reads them back at flush time. A match/block queues
    /// its body without evaluating it, then discards its value when flushed.
    fn emit_defer_register(&mut self, d: &DeferStmt) {
        let n = self.defer_counter;
        self.defer_counter += 1;
        let async_frame = self.coop_frame;
        let deferred_body_uses_lexical_vars = matches!(
            &d.body,
            DeferBody::Expr(Expr::Match(_)) | DeferBody::Block(_)
        );
        let mut bindings: Vec<(String, i32, Type)> = Vec::new();
        let mut stash = |fg: &mut Self, label: String, expr: &Expr| -> Expr {
            let ty = fg.ast_type_of(expr);
            let val = fg.emit_expr(expr);
            if let Some(frame_ptr) = async_frame {
                // Async: the operand lives in a frame slot (GC-masked by the
                // layout) so it survives suspension and is visible to the
                // cancel entry (willow-vynv.3).
                let off = fg.async_frame_offsets[&expr.span()];
                fg.emit_gc_heap_store(frame_ptr, off, val, &ty, GcStoreDestination::AsyncFrameSlot);
                bindings.push((label.clone(), off, ty.clone()));
                fg.vars
                    .insert(label.clone(), VarStorage::Frame { offset: off, ty });
            } else {
                let slot = fg.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    0,
                ));
                fg.stack_store(val, slot);
                if is_gc_managed(&ty, fg.enum_infos) {
                    fg.emit_push_root_slot(slot);
                }
                fg.vars
                    .insert(label.clone(), VarStorage::Stack { slot, ty });
            }
            Expr::Var(label, expr.span())
        };
        let action = match &d.body {
            DeferBody::Expr(Expr::Call(c)) => {
                let args = c
                    .args
                    .iter()
                    .enumerate()
                    .map(|(i, a)| CallArg {
                        expr: stash(self, format!("\0defer{n}_a{i}"), &a.expr),
                        mode: a.mode.clone(),
                        span: a.span,
                    })
                    .collect();
                super::DeferredAction::Stmt(Box::new(Stmt::Expr(ExprStmt {
                    expr: Expr::Call(Box::new(CallExpr {
                        callee: c.callee.clone(),
                        args,
                        span: c.span,
                    })),
                    span: d.span,
                })))
            }
            DeferBody::Expr(Expr::MethodCall(m)) => {
                let object = stash(self, format!("\0defer{n}_self"), &m.object);
                let args = m
                    .args
                    .iter()
                    .enumerate()
                    .map(|(i, a)| CallArg {
                        expr: stash(self, format!("\0defer{n}_a{i}"), &a.expr),
                        mode: a.mode.clone(),
                        span: a.span,
                    })
                    .collect();
                super::DeferredAction::Stmt(Box::new(Stmt::Expr(ExprStmt {
                    expr: Expr::MethodCall(Box::new(MethodCallExpr {
                        object,
                        method: m.method.clone(),
                        args,
                        span: m.span,
                    })),
                    span: d.span,
                })))
            }
            DeferBody::Expr(Expr::Print(arg, newline, span)) => {
                let stashed = stash(self, format!("\0defer{n}_p"), arg);
                super::DeferredAction::Stmt(Box::new(Stmt::Expr(ExprStmt {
                    expr: Expr::Print(Box::new(stashed), *newline, *span),
                    span: d.span,
                })))
            }
            DeferBody::Expr(expr @ Expr::Match(_)) => {
                super::DeferredAction::Stmt(Box::new(Stmt::Expr(ExprStmt {
                    expr: expr.clone(),
                    span: d.span,
                })))
            }
            DeferBody::Block(block) => super::DeferredAction::Block(block.clone()),
            // The checker rejects every other expression form (E0905).
            DeferBody::Expr(_) => unreachable!("unsupported defer expression reached codegen"),
        };
        if async_frame.is_some() && deferred_body_uses_lexical_vars {
            // The cancellation entry is a separate generated function. Rebind
            // the lexical variables that already live in the async frame so a
            // deferred match/block can read their latest values there. Pattern
            // and block-local bindings are created by the action itself.
            for (name, storage) in self.vars.clone() {
                if let VarStorage::Frame { offset, ty } = storage
                    && !bindings.iter().any(|(bound, ..)| bound == &name)
                {
                    bindings.push((name, offset, ty));
                }
            }
        }
        let flag_offset = if let Some(frame_ptr) = async_frame {
            // Mark the site REGISTERED for the cancel entry, cleared again by
            // any normal-path flush (willow-vynv.3).
            let off = self.async_frame_offsets[&d.span];
            let one = self.builder.ins().iconst(types::I64, 1);
            self.builder
                .ins()
                .store(MemFlagsData::new(), one, frame_ptr, off);
            self.collected_defer_sites.push(super::AsyncDeferSite {
                action: action.clone(),
                flag_offset: off,
                bindings: bindings.clone(),
                recovery_capable: crate::semantic::type_checker::defer_body_contains_direct_recover(
                    &d.body,
                ),
            });
            Some(off)
        } else {
            None
        };
        let sync_flag_slot = if async_frame.is_none() {
            let slot = self.sync_defer_flags.get(&d.span).copied();
            if let Some(slot) = slot {
                let one = self.builder.ins().iconst(types::I64, 1);
                self.stack_store(one, slot);
            }
            slot
        } else {
            None
        };
        let vars_at_registration = self.vars.clone();
        self.defer_stack
            .last_mut()
            .expect("defer outside any scope frame")
            .push(super::DeferEntry {
                id: n,
                action,
                flag_offset,
                sync_flag_slot,
                bindings,
                vars_at_registration,
                recovery_capable: crate::semantic::type_checker::defer_body_contains_direct_recover(
                    &d.body,
                ),
            });
    }

    pub(super) fn emit_stmt(&mut self, stmt: &Stmt) {
        // Debug builds report runtime-raised faults at statement granularity.
        self.fault_site_span = Some(stmt.span());
        match stmt {
            Stmt::Let(s) => {
                // With an interface annotation, a class initializer is boxed.
                let val = match &s.ty {
                    Some(target) => self.emit_expr_coerced(&s.init, &target.clone()),
                    None => self.emit_expr(&s.init),
                };
                // `_` is the wildcard name: evaluate for side effects but don't bind.
                if s.name == "_" {
                    return;
                }
                let ast_ty =
                    s.ty.clone()
                        .or_else(|| self.async_local_types.get(&s.span).cloned())
                        .unwrap_or_else(|| self.ast_type_of_init(&s.init));
                // In an async fn, a GC-managed local that is part of the frame
                // layout lives in the heap frame so it survives `await`
                // (willow-lpn.5b). The frame is already a GC root, so the local
                // needs no separate shadow-stack root.
                // Frame-back any local that has a frame offset. For eager async
                // only GC-managed locals get offsets (setup_async_frame); the
                // cooperative poll-fn path also assigns offsets to non-GC locals
                // so they survive suspension (willow-lpn.5.3 slice 3b). Non-GC
                // slots are not in the frame's GC mask, so they are not traced.
                if let Some(&offset) = self.async_frame_offsets.get(&s.span) {
                    let base = self
                        .async_frame
                        .expect("frame-backed local requires an allocated async frame");
                    self.emit_gc_heap_store(
                        base,
                        offset,
                        val,
                        &ast_ty,
                        GcStoreDestination::AsyncFrameSlot,
                    );
                    self.vars.insert(
                        s.name.clone(),
                        VarStorage::Frame {
                            offset,
                            ty: ast_ty.clone(),
                        },
                    );
                    return;
                }
                let storage = if is_gc_managed(&ast_ty, self.enum_infos) {
                    // GC-managed types: store in a stack slot so that the GC root
                    // slot and the variable slot are the SAME memory.  If we used
                    // an SSA variable for the value and a separate stack slot for
                    // the root, a reassignment (Stmt::Assign) would update the SSA
                    // variable but leave the root slot stale, allowing the GC to
                    // see old (possibly freed) pointers and collect the live new one.
                    let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        8,
                        0,
                    ));
                    self.stack_store(val, slot);
                    self.emit_push_root_slot(slot);
                    self.track_coop_binding_root(slot);
                    VarStorage::Stack {
                        slot,
                        ty: ast_ty.clone(),
                    }
                } else {
                    let ty = clif_type(&ast_ty);
                    let var = self.builder.declare_var(ty);
                    self.builder.def_var(var, val);
                    VarStorage::Value {
                        var,
                        ty: ast_ty.clone(),
                    }
                };
                self.vars.insert(s.name.clone(), storage);
            }
            Stmt::Assign(s) => {
                if let Some(storage) = self.vars.get(&s.name).cloned() {
                    let target_ty = storage.ty().clone();
                    let val = self.emit_expr_coerced(&s.value, &target_ty);
                    self.store_var(&storage, val);
                }
            }
            Stmt::FieldAssign(s) => {
                let ptr = self.emit_expr(&s.object);
                self.emit_nil_check(ptr, s.object.span(), &s.field);
                let obj_type = self.ast_type_of(&s.object);
                if let Some(class_name) = class_name_for_object_type(&obj_type)
                    && let Some(layout) = self.class_layouts.get(&class_name).cloned()
                    && let Some(idx) = layout.iter().position(|(n, _)| n == &s.field)
                {
                    // Word 0 is type_id; fields start at word 1 → offset = (idx + 1) * 8.
                    let offset = (idx as i32 + 1) * 8;
                    // Box a class value when the field's type is an interface.
                    let field_ty = layout[idx].1.clone();
                    let val = self.emit_expr_coerced(&s.value, &field_ty);
                    self.emit_gc_heap_store(
                        ptr,
                        offset,
                        val,
                        &field_ty,
                        GcStoreDestination::ObjectField,
                    );
                }
            }
            Stmt::SuperInit(s) => self.emit_super_init(s),
            Stmt::StaticFieldAssign(s) => {
                // `ClassName::property = value` for a `static mut` property: store
                // into the global slot (willow-qsqf §13.4). The slot was rooted
                // once by __willow_static_init, so the new value is traced too.
                let class_name = self.static_call_class_name(&s.class);
                if let Some(info) = self.lookup_static_storage(&class_name, &s.field) {
                    let val = self.emit_expr_coerced(&s.value, &info.ty);
                    let ptr_ty = self.module.target_config().pointer_type();
                    let gv = self
                        .module
                        .declare_data_in_func(info.data_id, self.builder.func);
                    let addr = self.builder.ins().symbol_value(ptr_ty, gv);
                    self.emit_gc_heap_store(
                        addr,
                        0,
                        val,
                        &info.ty,
                        GcStoreDestination::GlobalStatic,
                    );
                }
            }
            Stmt::IndexAssign(s) => {
                // Null and out-of-bounds are checked inside `willow_array_set`.
                let arr = self.emit_expr(&s.array);
                // Root the array while the value expression is evaluated — it may
                // allocate and trigger a collection.
                self.emit_push_root(arr);
                let idx = self.emit_expr(&s.index);
                let elem_ty = array_element_type(&self.ast_type_of(&s.array));
                // Box a class value when the array's element type is an interface.
                let val = self.emit_expr_coerced(&s.value, &elem_ty);
                let word = self.coerce_to_i64(val, &elem_ty);
                let set_id = self.func_id("willow_array_set");
                let set_ref = self.module.declare_func_in_func(set_id, self.builder.func);
                let panic_depth = self.emit_pre_runtime_call_panic_depth("willow_array_set");
                self.builder.ins().call(set_ref, &[arr, idx, word]);
                self.emit_pop_roots_n(1);
                self.gc_root_count -= 1;
                self.emit_post_willow_call_panic_check(panic_depth);
            }
            Stmt::If(s) => self.emit_if(s),
            Stmt::While(s) => self.emit_while(s),
            // break/continue (willow-kzka): pop GC roots acquired since loop
            // entry (this path diverges, so the runtime stack must balance),
            // jump, and mark the block terminated. The compile-time root
            // counter is NOT adjusted — enclosing scopes restore it, exactly
            // like the `return` path.
            Stmt::Break(_) | Stmt::Continue(_) => {
                let &(exit_block, continue_block, roots_at_entry, defer_depth) = self
                    .loop_stack
                    .last()
                    .expect("break/continue outside a loop reached codegen");
                // Scopes between this statement and the loop body unwind:
                // their defers run first (LIFO), then their roots pop.
                self.emit_flush_defers_from(defer_depth);
                let extra = self.gc_root_count - roots_at_entry;
                if extra > 0 {
                    self.emit_pop_roots_n(extra);
                }
                let target = if matches!(stmt, Stmt::Break(_)) {
                    exit_block
                } else {
                    continue_block
                };
                self.builder.ins().jump(target, &[]);
                self.terminated = true;
            }
            Stmt::For(s) => self.emit_for(s),
            // The type checker rejects every `lock` with E2502 until the
            // acquire/park/release lowering lands (willow-38w.1.3), so codegen
            // never sees one.
            Stmt::Lock(_) => unreachable!("lock lowering arrives in willow-38w.1.3"),
            Stmt::Defer(d) => self.emit_defer_register(d),
            Stmt::Return(s) => {
                // `fn main() -> Result<void, E>`: returns are turned into an exit
                // (Err -> report + non-zero; Ok / bare return -> exit 0), since
                // `willow_user_main` is void (willow-exg).
                if self.main_result_err_ty.is_some() {
                    match &s.value {
                        Some(val_expr) if is_zero_arg_result_ok(val_expr) => {
                            // `return Result::Ok();` — success, no construction.
                            self.emit_flush_defers_from(0);
                            if self.gc_root_count > 0 {
                                self.emit_pop_roots_n(self.gc_root_count);
                            }
                            self.builder.ins().return_(&[]);
                        }
                        Some(val_expr) => {
                            let result = self.emit_expr(val_expr);
                            self.emit_push_root(result);
                            self.emit_flush_defers_from(0);
                            self.emit_pop_roots_n(1);
                            self.gc_root_count -= 1;
                            self.emit_main_result_exit(result);
                        }
                        None => {
                            self.emit_flush_defers_from(0);
                            if self.gc_root_count > 0 {
                                self.emit_pop_roots_n(self.gc_root_count);
                            }
                            self.builder.ins().return_(&[]);
                        }
                    }
                    self.terminated = true;
                    return;
                }
                // Cooperative poll fn: a return stores into the frame's
                // `__result` slot and returns the Ready status (willow-zvkv —
                // reached from nested statement control flow like match arms).
                if let Some(frame) = self.coop_frame {
                    if let (Some(off), Some(val_expr)) = (self.coop_result_offset, &s.value) {
                        let result_ty = self.ast_type_of(val_expr);
                        let val = self.emit_expr(val_expr);
                        self.emit_gc_heap_store(
                            frame,
                            off,
                            val,
                            &result_ty,
                            GcStoreDestination::AsyncFrameSlot,
                        );
                    } else if let Some(val_expr) = &s.value {
                        self.emit_expr(val_expr);
                    }
                    // Run pending defers AFTER the result is stored in the
                    // frame (it is safe there across the flush) and clear
                    // their flags (willow-vynv.3).
                    if !self.defer_stack.is_empty() {
                        self.emit_flush_defers_from(0);
                    }
                    if self.gc_root_count > 0 {
                        self.emit_pop_roots_n(self.gc_root_count);
                    }
                    let ready = self.builder.ins().iconst(types::I32, 1);
                    self.builder.ins().return_(&[ready]);
                    self.terminated = true;
                    return;
                }
                if self.is_async {
                    let future = if let Some(val_expr) = &s.value {
                        if self.return_type == Type::Void {
                            self.emit_expr(val_expr);
                            self.emit_ready_future_void()
                        } else {
                            let return_type = self.return_type.clone();
                            let val = self.emit_expr(val_expr);
                            self.emit_ready_future(&return_type, val)
                        }
                    } else {
                        self.emit_ready_future_void()
                    };
                    if self.gc_root_count > 0 {
                        self.emit_pop_roots_n(self.gc_root_count);
                    }
                    self.builder.ins().return_(&[future]);
                } else {
                    if let Some(val_expr) = &s.value {
                        // Evaluate the return value BEFORE popping roots (it may load from GC objects).
                        let target = self.return_type.clone();
                        let val = self.emit_expr_coerced(val_expr, &target);
                        // Run pending defers AFTER the value is computed (Go
                        // semantics) — rooting it across the flush, which may
                        // allocate (willow-vynv.2).
                        if !self.defer_stack.iter().all(|f| f.is_empty()) {
                            let gc_val = is_gc_managed(&target, self.enum_infos);
                            if gc_val {
                                self.emit_push_root(val);
                            }
                            self.emit_flush_defers_from(0);
                            if gc_val {
                                self.emit_pop_roots_n(1);
                                self.gc_root_count -= 1;
                            }
                        }
                        if self.gc_root_count > 0 {
                            self.emit_pop_roots_n(self.gc_root_count);
                        }
                        self.builder.ins().return_(&[val]);
                    } else {
                        self.emit_flush_defers_from(0);
                        if self.gc_root_count > 0 {
                            self.emit_pop_roots_n(self.gc_root_count);
                        }
                        self.builder.ins().return_(&[]);
                    }
                }
                self.terminated = true;
            }
            Stmt::Expr(s) => {
                self.emit_expr(&s.expr);
            }
        }
    }

    pub(super) fn emit_if(&mut self, s: &IfStmt) {
        let cond = self.emit_expr(&s.cond);

        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let merge_block = self.builder.create_block();

        self.builder
            .ins()
            .brif(cond, then_block, &[], else_block, &[]);

        // then branch
        self.builder.switch_to_block(then_block);
        self.builder.seal_block(then_block);
        let outer_terminated = self.terminated;
        self.terminated = false;
        self.emit_block(&s.then_block);
        let then_terminated = self.terminated;
        if !self.terminated {
            self.builder.ins().jump(merge_block, &[]);
        }

        // else branch
        self.builder.switch_to_block(else_block);
        self.builder.seal_block(else_block);
        self.terminated = false;
        if let Some(else_b) = &s.else_block {
            self.emit_block(else_b);
        }
        let else_terminated = self.terminated;
        if !self.terminated {
            self.builder.ins().jump(merge_block, &[]);
        }

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.terminated = outer_terminated || (then_terminated && else_terminated);
    }

    pub(super) fn emit_while(&mut self, s: &WhileStmt) {
        let header = self.builder.create_block();
        let body_block = self.builder.create_block();
        let exit_block = self.builder.create_block();

        self.builder.ins().jump(header, &[]);

        self.builder.switch_to_block(header);
        let cond = self.emit_expr(&s.cond);
        self.builder
            .ins()
            .brif(cond, body_block, &[], exit_block, &[]);

        self.builder.switch_to_block(body_block);
        self.builder.seal_block(body_block);
        self.terminated = false;
        self.loop_stack.push((
            exit_block,
            header,
            self.gc_root_count,
            self.defer_stack.len(),
        ));
        self.emit_block(&s.body);
        self.loop_stack.pop();
        if !self.terminated {
            self.builder.ins().jump(header, &[]);
        }

        self.builder.seal_block(header);
        self.builder.switch_to_block(exit_block);
        self.builder.seal_block(exit_block);
        self.terminated = false;
    }

    pub(super) fn emit_for(&mut self, s: &ForStmt) {
        if let Expr::Range(range) = &s.iterable {
            self.emit_range_for(s, range);
            return;
        }

        let saved_vars = self.vars.clone();
        let roots_before = self.gc_root_count;
        let iterable_ty = self.ast_type_of(&s.iterable);
        // Iterating a `Range<i64>` held as a value.
        if matches!(&iterable_ty, Type::Generic(n, _) if n == "Range") {
            self.emit_range_for_value(s);
            self.vars = saved_vars;
            self.gc_root_count = roots_before;
            return;
        }
        let elem_ty = array_element_type(&iterable_ty);

        let arr = self.emit_expr(&s.iterable);
        self.emit_push_root(arr);

        let idx_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            0,
        ));
        let zero = self.builder.ins().iconst(types::I64, 0);
        self.stack_store(zero, idx_slot);

        if s.name != "_" {
            let elem_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                0,
            ));
            if is_gc_managed(&elem_ty, self.enum_infos) {
                let nil = self.builder.ins().iconst(types::I64, 0);
                self.stack_store(nil, elem_slot);
                self.emit_push_root_slot(elem_slot);
            }
            self.vars.insert(
                s.name.clone(),
                VarStorage::Stack {
                    slot: elem_slot,
                    ty: elem_ty.clone(),
                },
            );
        }

        let header = self.builder.create_block();
        let body_block = self.builder.create_block();
        // `continue` target: the increment must still run (willow-kzka).
        let inc_block = self.builder.create_block();
        let exit_block = self.builder.create_block();
        self.builder.ins().jump(header, &[]);

        self.builder.switch_to_block(header);
        let idx = self.stack_load(types::I64, idx_slot);
        // Inline `len` as a load from the handle (offset 0) instead of calling
        // willow_array_len (willow-pcoy). Re-read EVERY iteration on purpose:
        // the body may push/pop this same array, and the header must observe
        // the new length before each entry.
        let len = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), arr, 0i32);
        let keep_going = self.builder.ins().icmp(IntCC::SignedLessThan, idx, len);
        self.builder
            .ins()
            .brif(keep_going, body_block, &[], exit_block, &[]);

        self.builder.switch_to_block(body_block);
        self.builder.seal_block(body_block);
        if let Some(VarStorage::Stack { slot, .. }) = self.vars.get(&s.name).cloned() {
            // Inline element read (willow-pcoy): buffer = handle[3] (offset
            // 24), element i at buffer + 8 + i*8 (the buffer is
            // length-prefixed by cap). The BUFFER pointer is re-read each
            // iteration because a push in the body may reallocate it (the
            // HANDLE pointer is stable). Bounds are guaranteed by the header
            // check (0 <= idx < len re-checked every entry).
            let buffer = self
                .builder
                .ins()
                .load(types::I64, MemFlagsData::new(), arr, 24i32);
            let byte_off = self.builder.ins().ishl_imm_u(idx, 3);
            let addr = self.builder.ins().iadd(buffer, byte_off);
            let word = self
                .builder
                .ins()
                .load(types::I64, MemFlagsData::new(), addr, 8i32);
            let elem = self.coerce_i64_to(word, &elem_ty);
            self.stack_store(elem, slot);
        }
        self.terminated = false;
        self.loop_stack.push((
            exit_block,
            inc_block,
            self.gc_root_count,
            self.defer_stack.len(),
        ));
        self.emit_block(&s.body);
        self.loop_stack.pop();
        if !self.terminated {
            self.builder.ins().jump(inc_block, &[]);
        }
        self.builder.switch_to_block(inc_block);
        self.builder.seal_block(inc_block);
        let idx = self.stack_load(types::I64, idx_slot);
        let one = self.builder.ins().iconst(types::I64, 1);
        let next = self.builder.ins().iadd(idx, one);
        self.stack_store(next, idx_slot);
        self.builder.ins().jump(header, &[]);

        self.builder.seal_block(header);
        self.builder.switch_to_block(exit_block);
        self.builder.seal_block(exit_block);
        let loop_roots = self.gc_root_count - roots_before;
        if loop_roots > 0 {
            self.emit_pop_roots_n(loop_roots);
        }
        self.gc_root_count = roots_before;
        self.vars = saved_vars;
        self.terminated = false;
    }

    /// Materialize a `Range<i64>` value: a 2-word GC heap object `[start, end]`
    /// (both `i64`, so no GC-ref slots). Used when a range is held as a value
    /// rather than driven inline by a `for` loop.
    pub(super) fn emit_range_value(&mut self, range: &RangeExpr) -> cranelift_codegen::ir::Value {
        // start/end are i64 scalars (not GC), computed before the only allocation,
        // so they survive it in registers without rooting.
        let start = self.emit_expr(&range.start);
        let end = self.emit_expr(&range.end);
        let ptr = self.emit_gc_alloc(GcLayoutMetadata::new(GcObjectKind::Range, 16, 0, 0));
        self.builder
            .ins()
            .store(MemFlagsData::new(), start, ptr, 0i32);
        self.builder
            .ins()
            .store(MemFlagsData::new(), end, ptr, 8i32);
        ptr
    }

    pub(super) fn emit_range_for(&mut self, s: &ForStmt, range: &RangeExpr) {
        let start = self.emit_expr(&range.start);
        let end = self.emit_expr(&range.end);
        self.emit_range_for_bounds(s, start, end);
    }

    /// Iterate a `Range<i64>` VALUE (a variable or call result, not an inline
    /// literal): load its `start`/`end` words and drive the same counting loop.
    pub(super) fn emit_range_for_value(&mut self, s: &ForStmt) {
        let ptr = self.emit_expr(&s.iterable);
        let start = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), ptr, 0i32);
        let end = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), ptr, 8i32);
        self.emit_range_for_bounds(s, start, end);
    }

    pub(super) fn emit_range_for_bounds(
        &mut self,
        s: &ForStmt,
        start: cranelift_codegen::ir::Value,
        end: cranelift_codegen::ir::Value,
    ) {
        let saved_vars = self.vars.clone();

        let current_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            0,
        ));
        let end_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            0,
        ));
        self.stack_store(start, current_slot);
        self.stack_store(end, end_slot);

        if s.name != "_" {
            let elem_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                0,
            ));
            self.vars.insert(
                s.name.clone(),
                VarStorage::Stack {
                    slot: elem_slot,
                    ty: Type::I64,
                },
            );
        }

        let header = self.builder.create_block();
        let body_block = self.builder.create_block();
        // `continue` target: the increment must still run (willow-kzka).
        let inc_block = self.builder.create_block();
        let exit_block = self.builder.create_block();
        self.builder.ins().jump(header, &[]);

        self.builder.switch_to_block(header);
        let current = self.stack_load(types::I64, current_slot);
        let end = self.stack_load(types::I64, end_slot);
        let keep_going = self.builder.ins().icmp(IntCC::SignedLessThan, current, end);
        self.builder
            .ins()
            .brif(keep_going, body_block, &[], exit_block, &[]);

        self.builder.switch_to_block(body_block);
        self.builder.seal_block(body_block);
        if let Some(VarStorage::Stack { slot, .. }) = self.vars.get(&s.name).cloned() {
            self.stack_store(current, slot);
        }
        self.terminated = false;
        self.loop_stack.push((
            exit_block,
            inc_block,
            self.gc_root_count,
            self.defer_stack.len(),
        ));
        self.emit_block(&s.body);
        self.loop_stack.pop();
        if !self.terminated {
            self.builder.ins().jump(inc_block, &[]);
        }
        self.builder.switch_to_block(inc_block);
        self.builder.seal_block(inc_block);
        let current = self.stack_load(types::I64, current_slot);
        let one = self.builder.ins().iconst(types::I64, 1);
        let next = self.builder.ins().iadd(current, one);
        self.stack_store(next, current_slot);
        self.builder.ins().jump(header, &[]);

        self.builder.seal_block(header);
        self.builder.switch_to_block(exit_block);
        self.builder.seal_block(exit_block);
        self.vars = saved_vars;
        self.terminated = false;
    }
}
