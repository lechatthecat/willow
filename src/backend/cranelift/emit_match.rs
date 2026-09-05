use cranelift_codegen::ir::{InstBuilder, MemFlagsData, condcodes::IntCC, types};
use cranelift_module::Module;

use super::*;

/// Emit the process-boundary branch shared by synchronous and cooperative
/// `Result<void, E>` mains. The caller owns any GC-root cleanup: after this
/// point no operation can allocate before the tag/payload is consumed.
pub(super) fn emit_main_result_exit_raw(
    builder: &mut FunctionBuilder<'_>,
    module: &mut ObjectModule,
    fail_id: FuncId,
    result_ptr: cranelift_codegen::ir::Value,
    err_is_string: bool,
) {
    let tag = builder
        .ins()
        .load(types::I64, MemFlagsData::new(), result_ptr, 0i32);
    let err_tag = builder.ins().iconst(types::I64, 1); // Err = tag 1
    let is_err = builder.ins().icmp(IntCC::Equal, tag, err_tag);
    let err_block = builder.create_block();
    let ok_block = builder.create_block();
    builder.ins().brif(is_err, err_block, &[], ok_block, &[]);

    builder.switch_to_block(err_block);
    builder.seal_block(err_block);
    let msg = if err_is_string {
        builder
            .ins()
            .load(types::I64, MemFlagsData::new(), result_ptr, 8i32)
    } else {
        builder.ins().iconst(types::I64, 0)
    };
    let fail_ref = module.declare_func_in_func(fail_id, builder.func);
    builder.ins().call(fail_ref, &[msg]);
    // willow_main_fail is noreturn; trap to satisfy the verifier.
    builder
        .ins()
        .trap(cranelift_codegen::ir::TrapCode::unwrap_user(1));

    builder.switch_to_block(ok_block);
    builder.seal_block(ok_block);
    builder.ins().return_(&[]);
}

impl<'a, 'b> FuncGen<'a, 'b> {
    pub(super) fn emit_ternary(&mut self, t: &TernaryExpr) -> cranelift_codegen::ir::Value {
        let result_ty = clif_type(&checked_expr_type(
            &Expr::Ternary(Box::new(t.clone())),
            self.expr_types,
        ));
        let result_var = self.builder.declare_var(result_ty);

        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let merge_block = self.builder.create_block();

        let cond = self.emit_expr(&t.condition);
        self.builder
            .ins()
            .brif(cond, then_block, &[], else_block, &[]);

        // then branch — only this runs when condition is true (lazy)
        self.builder.switch_to_block(then_block);
        self.builder.seal_block(then_block);
        let then_val = self.emit_expr(&t.then_expr);
        self.builder.def_var(result_var, then_val);
        self.builder.ins().jump(merge_block, &[]);

        // else branch — only this runs when condition is false (lazy)
        self.builder.switch_to_block(else_block);
        self.builder.seal_block(else_block);
        let else_val = self.emit_expr(&t.else_expr);
        self.builder.def_var(result_var, else_val);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.builder.use_var(result_var)
    }

    /// Convert error `e1_payload` (static class `e1_name`, implementing
    /// `Into<E2>`) to `E2` by calling `into`, dispatching VIRTUALLY on the
    /// payload's runtime type so a subclass override is honored (willow-bpk6).
    pub(super) fn emit_into_conversion(
        &mut self,
        e1_payload: cranelift_codegen::ir::Value,
        e1_name: &str,
    ) -> cranelift_codegen::ir::Value {
        // Candidate runtime types: e1_name and its subclasses that resolve `into`.
        let mut dispatch: Vec<(i64, FuncId)> = self
            .class_type_ids
            .iter()
            .filter(|(cls, _)| self.class_is_a(cls, e1_name))
            .filter_map(|(cls, &id)| {
                self.resolve_method_func_id(cls, "into")
                    .map(|fid| (id, fid))
            })
            .collect();
        dispatch.sort_by_key(|(id, _)| *id);

        // Zero or one candidate: a plain direct call (no subclass override).
        if dispatch.len() <= 1 {
            let fid = dispatch
                .first()
                .map(|(_, f)| *f)
                .or_else(|| self.resolve_method_func_id(e1_name, "into"))
                .expect("Into impl must exist (verified by the type checker)");
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let call = self.builder.ins().call(fref, &[e1_payload]);
            return self.builder.inst_results(call)[0];
        }

        // Multiple candidates: switch on the payload's runtime type_id, read
        // through its class descriptor (willow-fm7t).
        let type_id = self.emit_load_runtime_type_id(e1_payload);
        let result_var = self.builder.declare_var(types::I64);
        let zero = self.builder.ins().iconst(types::I64, 0);
        self.builder.def_var(result_var, zero);
        let merge = self.builder.create_block();
        let n = dispatch.len();
        for (i, (tid, fid)) in dispatch.into_iter().enumerate() {
            let tid_c = self.builder.ins().iconst(types::I64, tid);
            let is_match = self.builder.ins().icmp(IntCC::Equal, type_id, tid_c);
            let arm = self.builder.create_block();
            let next = self.builder.create_block();
            self.builder.ins().brif(is_match, arm, &[], next, &[]);
            self.builder.switch_to_block(arm);
            self.builder.seal_block(arm);
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let call = self.builder.ins().call(fref, &[e1_payload]);
            let r = self.builder.inst_results(call)[0];
            self.builder.def_var(result_var, r);
            self.builder.ins().jump(merge, &[]);
            self.builder.switch_to_block(next);
            self.builder.seal_block(next);
            if i + 1 == n {
                self.builder.ins().jump(merge, &[]);
            }
        }
        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Lower `expr?` into control flow:
    /// - Result::Ok / Option::Some (tag == 0): extract and return the payload.
    /// - Result::Err / Option::None (tag == 1): early-return the enum pointer.
    pub(super) fn emit_try_propagate(&mut self, inner: &Expr) -> cranelift_codegen::ir::Value {
        let operand_ty = self.ast_type_of(inner);
        let result_ptr = self.emit_expr(inner);
        if self.terminated {
            return result_ptr;
        }
        let payload_ty = try_propagate_payload_type(&operand_ty);
        let option_payload_ty = option_inner(&operand_ty).cloned();

        // Automatic error conversion (willow-1ow): if the operand's error type
        // `E1` differs from the enclosing function's error type `E2` (and neither
        // is `void`), the type checker has already verified `E1: Into<E2>`. On
        // the Err path we must convert `e1.into() -> e2` and re-wrap `Err(e2)`,
        // rather than returning the original `Result<_, E1>`.
        let convert: Option<(String, Type)> = match (
            result_err_type(&operand_ty),
            result_err_type(&self.return_type),
        ) {
            (Some(Type::Named(e1)), Some(e2))
                if Type::Named(e1.clone()) != e2 && e2 != Type::Void =>
            {
                Some((e1, e2))
            }
            _ => None,
        };

        let is_ok = if option_payload_ty.is_some()
            && option_repr(&operand_ty, self.enum_infos) == Some(OptionRepr::NullableGcPointer)
        {
            self.builder
                .ins()
                .icmp_imm_u(IntCC::NotEqual, result_ptr, 0)
        } else {
            // Boxed Result and boxed Option use tag 0 for Ok/Some.
            let tag = self
                .builder
                .ins()
                .load(types::I64, MemFlagsData::new(), result_ptr, 0i32);
            let ok_tag = self.builder.ins().iconst(types::I64, 0);
            self.builder.ins().icmp(IntCC::Equal, tag, ok_tag)
        };

        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        let branch_root_depth = self.gc_root_count;
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], err_block, &[]);

        // ── Propagate branch: early-return the Err ────────────────────────────
        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        // When the error types differ, convert `e1.into() -> e2` and re-wrap.
        let return_ptr = if option_payload_ty.is_some() {
            // `?` may cross representation boundaries. For example, a niche
            // `Option<String>` operand can propagate into a boxed
            // `Option<i64>` return, and the reverse is also valid. Construct
            // the destination's None instead of returning the source bits.
            let return_inner = option_inner(&self.return_type)
                .cloned()
                .expect("Option `?` must be inside an Option-returning function");
            self.emit_alloc_option_none(&return_inner)
        } else if let Some((e1_name, e2_ty)) = &convert {
            let e1_payload =
                self.builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), result_ptr, 8i32);
            let e1_is_gc = is_gc_managed(&Type::Named(e1_name.clone()), self.enum_infos);
            if e1_is_gc {
                self.emit_push_root(e1_payload);
            }
            // Dispatch `into` on the payload's runtime type so a subclassed
            // error that overrides `into` converts correctly (willow-bpk6).
            let e2_val = self.emit_into_conversion(e1_payload, e1_name);
            if e1_is_gc {
                self.emit_pop_roots_n(1);
                self.gc_root_count -= 1;
            }
            self.emit_alloc_enum_variant(1, e2_ty, e2_val)
        } else {
            result_ptr
        };
        if let Some(frame) = self.coop_frame {
            // A cooperative async function returns the poll status (i32), not
            // its language-level Result pointer. Publish the Err in the
            // frame's result slot before cleanup, then finish this poll as
            // Ready. The frame slot roots the Result while defers allocate.
            if let Some(offset) = self.coop_result_offset {
                let return_type = self.return_type.clone();
                self.emit_gc_heap_store(
                    frame,
                    offset,
                    return_ptr,
                    &return_type,
                    GcStoreDestination::AsyncFrameSlot,
                );
            }
            self.emit_flush_defers_from(0);
            if !self.terminated {
                self.emit_coop_unwind_poll_roots();
                let ready = self.builder.ins().iconst(types::I32, 1);
                self.builder.ins().return_(&[ready]);
            }
        } else if self.main_result_err_ty.is_some() {
            // In a `Result<void, E>` main, an Err is reported and exits non-zero
            // rather than being returned (willow_user_main is void). Roots are
            // popped inside emit_main_result_exit.
            if !self.defer_stack.iter().all(|f| f.is_empty()) {
                self.emit_push_root(return_ptr);
                self.emit_flush_defers_from(0);
                if !self.terminated {
                    self.emit_pop_roots_n(1);
                    self.gc_root_count -= 1;
                }
            }
            if !self.terminated {
                self.emit_main_result_exit(return_ptr);
            }
        } else {
            // `?` leaves a synchronous function: pending defers run first,
            // with the outgoing Err pointer rooted across them (they may
            // allocate) (willow-vynv.2).
            if !self.defer_stack.iter().all(|f| f.is_empty()) {
                self.emit_push_root(return_ptr);
                self.emit_flush_defers_from(0);
                if !self.terminated {
                    self.emit_pop_roots_n(1);
                    self.gc_root_count -= 1;
                }
            }
            if !self.terminated {
                if self.gc_root_count > 0 {
                    self.emit_pop_roots_n(self.gc_root_count);
                }
                // Return the (possibly converted) Result/Option pointer.
                self.builder.ins().return_(&[return_ptr]);
            }
        }

        // ── Success branch: extract payload from word 1 ───────────────────────
        // The error arm may have terminated while running a defer. The success
        // arm is an independent predecessor and starts with the root/tracker
        // state from before the branch (willow-s9ej.12).
        self.gc_root_count = branch_root_depth;
        self.terminated = false;
        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let payload = if option_payload_ty.is_some()
            && option_repr(&operand_ty, self.enum_infos) == Some(OptionRepr::NullableGcPointer)
        {
            result_ptr
        } else {
            self.builder
                .ins()
                .load(types::I64, MemFlagsData::new(), result_ptr, 8i32)
        };
        self.coerce_i64_to(payload, &payload_ty)
    }

    /// Leave a `Result<void, E>` main by inspecting the `Result` value: `Err`
    /// reports the payload and exits non-zero; `Ok` returns void (exit 0). Pops
    /// the function's GC roots first (we leave the function on both paths).
    /// See willow-exg.
    pub(super) fn emit_main_result_exit(&mut self, result_ptr: cranelift_codegen::ir::Value) {
        if self.gc_root_count > 0 {
            self.emit_pop_roots_n(self.gc_root_count);
        }
        let err_is_string = self.main_result_err_ty.as_ref() == Some(&Type::String);

        let fail_id = self.func_id("willow_main_fail");
        emit_main_result_exit_raw(
            self.builder,
            self.module,
            fail_id,
            result_ptr,
            err_is_string,
        );
    }

    /// The pattern an arm lowers as: the type checker's reinterpretation of an
    /// unqualified pattern (`Ok(v)` → EnumVariantTuple) if any, else the parsed
    /// pattern (willow-60o.1).
    pub(super) fn resolved_pattern(&self, arm: &MatchArm) -> Pattern {
        self.pattern_resolutions
            .get(&arm.pattern.span())
            .cloned()
            .unwrap_or_else(|| arm.pattern.clone())
    }

    /// Bring one `match` pattern binding into scope.
    ///
    /// A GC-managed binding gets a ROOTED STACK SLOT, not a Cranelift variable
    /// (willow-10zt). An address-taken scalar gets an unrooted slot too, created
    /// here at the binding rather than lazily at one `&` use (willow-0g8j.12).
    /// The minor collector promotes a direct root in place, so an
    /// SSA copy of one keeps naming the right object; but a young object it
    /// reaches only through a heap slot is EVACUATED — copied to an old region,
    /// the owning slot fixed up, the source reclaimed. An enum payload and the
    /// object inside an interface box are that second case: the scrutinee is
    /// rooted and stays put while its child moves out from under a binding that
    /// copied the old address. Rooting the binding makes it a direct root of its
    /// own, which the collector pins.
    ///
    /// The slot IS the root slot, exactly as a GC-managed `let` gets in
    /// [`FuncGen::emit_stmt`], so an assignment through the binding cannot leave
    /// the root behind. Non-GC bindings keep their Cranelift variable: rooting a
    /// scalar word would have the collector trace it as an object pointer.
    ///
    /// The caller pops what this pushed when it closes the arm's bracket.
    fn bind_match_binding(&mut self, name: &str, ty: &Type, val: cranelift_codegen::ir::Value) {
        let rooted = is_gc_managed(ty, self.enum_infos);
        let storage = if rooted || self.address_taken.contains(name) {
            let storage = self.create_local_stack_slot(ty, val);
            if rooted && let VarStorage::Stack { slot, .. } = &storage {
                self.emit_push_root_slot(*slot);
                // An arm body in an async fn may suspend, and a poll return
                // pops every ACTIVE binding root and re-registers it on re-poll.
                // A root the shadow list does not know about would be left
                // pointing into the dead native frame — and
                // `emit_coop_unwind_poll_roots` asserts the two depths agree.
                self.track_coop_binding_root(*slot);
            }
            storage
        } else {
            let var = self.builder.declare_var(clif_type(ty));
            self.builder.def_var(var, val);
            VarStorage::Value {
                var,
                ty: ty.clone(),
            }
        };
        self.vars.insert(name.to_string(), storage);
    }

    pub(super) fn emit_match(&mut self, m: &MatchExpr) -> cranelift_codegen::ir::Value {
        let scrutinee = self.emit_expr(&m.scrutinee);
        let scrutinee_ast_type = self.ast_type_of(&m.scrutinee);
        // A temporary enum/interface/class scrutinee owns every GC payload
        // used by pattern bindings. Keep it rooted through the selected arm;
        // the arm may allocate before reading a bound payload (notably
        // `match recover() { Some(info) => ... }`, willow-s9ej.3).
        let rooted_scrutinee = is_gc_managed(&scrutinee_ast_type, self.enum_infos);
        if rooted_scrutinee {
            self.emit_push_root(scrutinee);
        }

        // Determine the result type: the checker's recorded type is
        // authoritative (a statement-position match is Void, willow-zvkv);
        // the structural arm-walk below only covers synthesized nodes.
        let result_ast_type = self
            .expr_types
            .get(&m.span)
            .expect("internal compiler error: match has no checked result type")
            .clone();
        let result_clif_type = clif_type(&result_ast_type);
        let result_var = self.builder.declare_var(result_clif_type);
        let mut any_arm_merges = false;
        let zero = if result_clif_type == types::F64 {
            let bits = self.builder.ins().iconst(types::I64, 0);
            self.builder
                .ins()
                .bitcast(types::F64, MemFlagsData::new(), bits)
        } else if result_clif_type == types::I8 {
            self.builder.ins().iconst(types::I8, 0)
        } else {
            self.builder.ins().iconst(types::I64, 0)
        };
        self.builder.def_var(result_var, zero);

        let merge_block = self.builder.create_block();

        let mut remaining = m.arms.as_slice();

        while !remaining.is_empty() {
            let arm = &remaining[0];
            remaining = &remaining[1..];

            let is_last = remaining.is_empty();
            let pat = self.resolved_pattern(arm);

            let always_matches = matches!(pat, Pattern::Wildcard(_) | Pattern::Binding { .. });

            let arm_block = self.builder.create_block();
            let next_block = if always_matches || is_last {
                None
            } else {
                Some(self.builder.create_block())
            };

            if always_matches {
                self.builder.ins().jump(arm_block, &[]);
            } else {
                let cond = self.emit_pattern_check(scrutinee, &scrutinee_ast_type, &pat);
                let fallthrough = next_block.unwrap_or(merge_block);
                self.builder
                    .ins()
                    .brif(cond, arm_block, &[], fallthrough, &[]);
            }

            self.builder.switch_to_block(arm_block);
            self.builder.seal_block(arm_block);

            // For binding patterns, define the variable. A GC-managed one
            // pushes a rooted slot that has to be popped again before this arm
            // reaches the merge block, so that sibling arms and the merge all
            // agree on the root depth (willow-10zt).
            let arm_roots_before = self.gc_root_count;
            let arm_coop_roots_before = self.coop_shadow_roots.as_ref().map(|r| r.active.len());
            let saved_vars = match &pat {
                // Binds the scrutinee itself, which `rooted_scrutinee` already
                // holds as a direct root for the whole `match` — and the
                // collector pins a direct root in place, so this alias cannot go
                // stale the way a payload binding can.
                Pattern::Binding { name, .. } => {
                    let saved = self.vars.clone();
                    // A whole-value binding has the SCRUTINEE'S type. In
                    // particular an f64 must not be declared as an i64 while
                    // carrying the match expression's (often void) type.
                    self.bind_match_binding(name, &scrutinee_ast_type, scrutinee);
                    Some(saved)
                }
                Pattern::EnumVariantTuple {
                    enum_name,
                    variant,
                    bindings,
                    ..
                } => {
                    let saved = self.vars.clone();
                    let payload_types =
                        self.resolve_variant_payload_types(enum_name, variant, &scrutinee_ast_type);
                    for (i, (binding, payload_ty)) in
                        bindings.iter().zip(payload_types.iter()).enumerate()
                    {
                        let clif_ty = clif_type(payload_ty);
                        let raw = if i == 0
                            && builtin_types::is(&scrutinee_ast_type, B::Option)
                            && option_repr(&scrutinee_ast_type, self.enum_infos)
                                == Some(OptionRepr::NullableGcPointer)
                        {
                            scrutinee
                        } else {
                            let offset = (1 + i) as i32 * 8;
                            self.builder.ins().load(
                                types::I64,
                                MemFlagsData::new(),
                                scrutinee,
                                offset,
                            )
                        };
                        let val = if clif_ty == types::F64 {
                            self.builder
                                .ins()
                                .bitcast(types::F64, MemFlagsData::new(), raw)
                        } else if clif_ty == types::I8 {
                            self.builder.ins().ireduce(types::I8, raw)
                        } else {
                            raw
                        };
                        self.bind_match_binding(binding, payload_ty, val);
                    }
                    Some(saved)
                }
                Pattern::ClassDowncast {
                    class_name,
                    binding,
                    ..
                } if binding != "_" => {
                    // Bind the downcast value: the box's object pointer (word 0),
                    // typed as the concrete class (willow-1js.4).
                    let saved = self.vars.clone();
                    let obj =
                        self.builder
                            .ins()
                            .load(types::I64, MemFlagsData::new(), scrutinee, 0i32);
                    self.bind_match_binding(binding, &Type::Named(class_name.clone()), obj);
                    Some(saved)
                }
                _ => None,
            };

            let outer_terminated = self.terminated;
            self.terminated = false;
            let arm_val = self.emit_match_body(&arm.body, result_clif_type);

            if !self.terminated {
                // The arm falls through to the merge. Drop its binding roots
                // first; a `return` or a `panic` arm needs no pop here, having
                // already unwound the whole root depth itself.
                self.emit_pop_roots_n(self.gc_root_count - arm_roots_before);
                if let Some(arm_val) = arm_val {
                    self.builder.def_var(result_var, arm_val);
                    self.builder.ins().jump(merge_block, &[]);
                    any_arm_merges = true;
                }
            }
            self.terminated = outer_terminated;
            self.gc_root_count = arm_roots_before;
            if let (Some(depth), Some(roots)) =
                (arm_coop_roots_before, self.coop_shadow_roots.as_mut())
            {
                assert!(
                    roots.active.len() >= depth,
                    "cooperative match arm root scope underflow"
                );
                roots.active.truncate(depth);
            }

            if let Some(saved) = saved_vars {
                self.vars = saved;
            }

            if let Some(next) = next_block {
                self.builder.switch_to_block(next);
                self.builder.seal_block(next);
            }

            if always_matches {
                break;
            }
        }

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        let result = if any_arm_merges {
            self.builder.use_var(result_var)
        } else {
            // Every arm terminated (returned): the merge block is unreachable
            // and `result_var` was never defined on any path — produce a typed
            // dummy so the verifier is satisfied (willow-zvkv).
            match result_clif_type {
                types::F64 => self.builder.ins().f64const(0.0),
                ty => self.builder.ins().iconst(ty, 0),
            }
        };
        if rooted_scrutinee {
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
        }
        result
    }

    /// Emit a match arm's body. Returns `None` when the body terminated the
    /// current block (e.g. a `return` arm, willow-zvkv) — no value may be
    /// produced then, because the block is already filled.
    pub(super) fn emit_match_body(
        &mut self,
        body: &MatchBody,
        result_ty: cranelift_codegen::ir::Type,
    ) -> Option<cranelift_codegen::ir::Value> {
        match body {
            MatchBody::Expr(expr) => Some(self.emit_expr(expr)),
            MatchBody::Block(block) => {
                self.emit_block(block);
                if self.terminated {
                    None
                } else {
                    // A non-returning block arm has no value; feed the merge a
                    // dummy of the RESULT type (a Void match uses I8).
                    Some(match result_ty {
                        types::F64 => self.builder.ins().f64const(0.0),
                        ty => self.builder.ins().iconst(ty, 0),
                    })
                }
            }
        }
    }

    pub(super) fn emit_pattern_check(
        &mut self,
        scrutinee: cranelift_codegen::ir::Value,
        scrutinee_ty: &Type,
        pattern: &Pattern,
    ) -> cranelift_codegen::ir::Value {
        match pattern {
            Pattern::Wildcard(_) | Pattern::Binding { .. } => {
                self.builder.ins().iconst(types::I8, 1)
            }
            Pattern::LiteralBool(b, _) => {
                let expected = self.builder.ins().iconst(types::I8, if *b { 1 } else { 0 });
                self.builder.ins().icmp(IntCC::Equal, scrutinee, expected)
            }
            Pattern::LiteralInt(n, _) => {
                let expected = self.builder.ins().iconst(types::I64, *n);
                self.builder.ins().icmp(IntCC::Equal, scrutinee, expected)
            }
            Pattern::EnumVariant {
                enum_name, variant, ..
            } => {
                let tag = self.enum_variant_tag(enum_name, variant);
                if builtin_types::is(scrutinee_ty, B::Option)
                    && option_repr(scrutinee_ty, self.enum_infos)
                        == Some(OptionRepr::NullableGcPointer)
                {
                    return if tag == 0 {
                        self.builder.ins().icmp_imm_u(IntCC::NotEqual, scrutinee, 0)
                    } else {
                        self.builder.ins().icmp_imm_u(IntCC::Equal, scrutinee, 0)
                    };
                }
                let expected = self.builder.ins().iconst(types::I64, tag);
                if self.enum_is_gc_object_type(enum_name) {
                    let actual_tag = self.emit_load_enum_tag(scrutinee);
                    self.builder.ins().icmp(IntCC::Equal, actual_tag, expected)
                } else {
                    self.builder.ins().icmp(IntCC::Equal, scrutinee, expected)
                }
            }
            Pattern::EnumVariantTuple {
                enum_name, variant, ..
            } => {
                let tag = self.enum_variant_tag(enum_name, variant);
                if builtin_types::is(scrutinee_ty, B::Option)
                    && option_repr(scrutinee_ty, self.enum_infos)
                        == Some(OptionRepr::NullableGcPointer)
                {
                    return if tag == 0 {
                        self.builder.ins().icmp_imm_u(IntCC::NotEqual, scrutinee, 0)
                    } else {
                        self.builder.ins().icmp_imm_u(IntCC::Equal, scrutinee, 0)
                    };
                }
                let expected = self.builder.ins().iconst(types::I64, tag);
                let actual_tag = self.emit_load_enum_tag(scrutinee);
                self.builder.ins().icmp(IntCC::Equal, actual_tag, expected)
            }
            Pattern::ClassDowncast { class_name, .. } => {
                // The scrutinee is an interface box {object@0, vtable@8}. Match
                // when the boxed object's runtime type_id — read through the
                // class descriptor its word 0 points at (willow-fm7t) — equals
                // the target class's type_id (willow-1js.4).
                // The type checker rejects an unknown class in a downcast
                // pattern with E0350, so a miss here is a compiler bug. Falling
                // back to "never matches" would silently drop a live arm and
                // send the program down the wildcard instead
                // (willow-uqzx, catalog item 14).
                let type_id = self.class_type_ids.get(class_name).copied().unwrap_or_else(|| {
                    panic!(
                        "compiler invariant violated: checked downcast pattern class `{class_name}` has no type id"
                    )
                });
                let obj = self
                    .builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), scrutinee, 0i32);
                let actual = self.emit_load_runtime_type_id(obj);
                let expected = self.builder.ins().iconst(types::I64, type_id);
                self.builder.ins().icmp(IntCC::Equal, actual, expected)
            }
        }
    }

    pub(super) fn emit_load_enum_tag(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        self.builder
            .ins()
            .load(types::I64, MemFlagsData::new(), ptr, 0i32)
    }

    pub(super) fn emit_enum_variant_alloc(
        &mut self,
        enum_ty: &Type,
        tag: i64,
        args: &[crate::parser::ast::CallArg],
        payload_types: &[Type],
    ) -> cranelift_codegen::ir::Value {
        if option_repr(enum_ty, self.enum_infos) == Some(OptionRepr::NullableGcPointer) {
            return match tag {
                // Option::Some(T) is the non-null managed payload itself.
                0 => {
                    let arg = args
                        .first()
                        .expect("type-checked Option::Some must have one payload");
                    let stored_ty = payload_types
                        .first()
                        .cloned()
                        .or_else(|| option_inner(enum_ty).cloned())
                        .unwrap_or_else(|| self.ast_type_of(&arg.expr));
                    self.emit_expr_coerced(&arg.expr, &stored_ty)
                }
                // Option::None occupies the pointer niche and never allocates.
                1 => self.builder.ins().iconst(types::I64, 0),
                _ => panic!("invalid Option variant tag {tag}"),
            };
        }

        let field_count = args.len();
        let slot_kinds = args
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let stored_ty = payload_types
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| self.ast_type_of(&arg.expr));
                if is_gc_managed(&stored_ty, self.enum_infos) {
                    willow_abi::SlotKind::GcRef
                } else {
                    willow_abi::SlotKind::Word
                }
            })
            .collect::<Vec<_>>();
        let layout = willow_abi::EnumVariantLayout::new(tag as u32, &slot_kinds);
        let pointer_bytes = self.module.target_config().pointer_type().bytes();
        let ptr = self.emit_gc_alloc(GcLayoutMetadata::new(
            GcObjectKind::Enum,
            i64::from(layout.payload_bytes(pointer_bytes)),
            0,
            layout.gc_ref_mask(),
        ));
        let tag_val = self.builder.ins().iconst(types::I64, tag);
        self.builder
            .ins()
            .store(MemFlagsData::new(), tag_val, ptr, 0i32);
        // Root the freshly allocated enum across argument evaluation: each
        // `emit_expr(&arg.expr)` below can allocate (e.g. a class payload), and
        // that allocation may trigger a collection.  Without this root the
        // half-built enum (tag stored, payload slots still zero) would be
        // reclaimed and we would store into freed memory.  The payload is
        // alloc_zeroed, so tracing the enum before all slots are stored is safe
        // (unstored ref slots read as null and are skipped).
        // Root whenever there is at least one argument to evaluate: even a
        // scalar-payload enum must survive an allocation inside an argument
        // expression (e.g. `Option::Some(f())` where `f` allocates internally).
        let needs_root = field_count > 0;
        if needs_root {
            self.emit_push_root(ptr);
        }
        for (i, arg) in args.iter().enumerate() {
            let offset = layout.payload_byte_offset(pointer_bytes) as i32
                + (i as i32 * pointer_bytes as i32);
            // The instantiated variant payload is the STORAGE type. In
            // `Option<Greeter> = Some(new Dog())`, the expression itself has
            // type `Dog`, but the payload must contain the ordinary
            // class-to-interface box, not the raw Dog pointer (willow-glaj.2).
            // Context is construction-only: an already-built `Option<Dog>` is
            // still not convertible to `Option<Greeter>`.
            let stored_ty = payload_types
                .get(i)
                .cloned()
                .unwrap_or_else(|| self.ast_type_of(&arg.expr));
            let val = self.emit_expr_coerced(&arg.expr, &stored_ty);
            let val_i64 = if matches!(stored_ty, Type::F64) {
                self.builder
                    .ins()
                    .bitcast(types::I64, MemFlagsData::new(), val)
            } else {
                val
            };
            self.emit_gc_heap_store(
                ptr,
                offset,
                val_i64,
                &stored_ty,
                GcStoreDestination::EnumPayload,
            );
        }
        if needs_root {
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
        }
        ptr
    }

    pub(super) fn emit_static_field_read(
        &mut self,
        class: &str,
        field: &str,
    ) -> cranelift_codegen::ir::Value {
        let class_name = self.static_call_class_name(class);
        if let Some(info) = self.lookup_static_storage(&class_name, field) {
            let ty = clif_type(&info.ty);
            let ptr_ty = self.module.target_config().pointer_type();
            let gv = self
                .module
                .declare_data_in_func(info.data_id, self.builder.func);
            let addr = self.builder.ins().symbol_value(ptr_ty, gv);
            return self.builder.ins().load(ty, MemFlagsData::new(), addr, 0);
        }
        // Should be unreachable after type checking; fall back to a zero value.
        self.builder.ins().iconst(types::I64, 0)
    }
}
