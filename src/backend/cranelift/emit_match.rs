use cranelift_codegen::ir::{InstBuilder, MemFlagsData, condcodes::IntCC, types};
use cranelift_module::Module;

use super::*;

impl<'a, 'b> FuncGen<'a, 'b> {
    pub(super) fn emit_ternary(&mut self, t: &TernaryExpr) -> cranelift_codegen::ir::Value {
        let result_ty = clif_type(&ast_type_of_ternary(
            t,
            &self.vars,
            self.func_return_types,
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

        // Multiple candidates: switch on the payload's runtime type_id (word 0).
        let type_id = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), e1_payload, 0i32);
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
        let payload_ty = try_propagate_payload_type(&operand_ty);

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

        // Load the enum tag from word 0.
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), result_ptr, 0i32);
        let ok_tag = self.builder.ins().iconst(types::I64, 0); // Ok = tag 0
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, ok_tag);

        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], err_block, &[]);

        // ── Propagate branch: early-return the Err ────────────────────────────
        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        // When the error types differ, convert `e1.into() -> e2` and re-wrap.
        let return_ptr = if let Some((e1_name, e2_ty)) = &convert {
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
        if self.main_result_err_ty.is_some() {
            // In a `Result<void, E>` main, an Err is reported and exits non-zero
            // rather than being returned (willow_user_main is void). Roots are
            // popped inside emit_main_result_exit.
            self.emit_main_result_exit(return_ptr);
        } else {
            if self.gc_root_count > 0 {
                self.emit_pop_roots_n(self.gc_root_count);
            }
            // Return the (possibly converted) Result/Option pointer.
            self.builder.ins().return_(&[return_ptr]);
        }

        // ── Success branch: extract payload from word 1 ───────────────────────
        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let payload = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), result_ptr, 8i32);
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

        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), result_ptr, 0i32);
        let err_tag = self.builder.ins().iconst(types::I64, 1); // Err = tag 1
        let is_err = self.builder.ins().icmp(IntCC::Equal, tag, err_tag);
        let err_block = self.builder.create_block();
        let ok_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_err, err_block, &[], ok_block, &[]);

        // Err: print the payload (a WillowString for E=String, else a generic
        // report via a null message) and exit non-zero.
        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        let msg = if err_is_string {
            self.builder
                .ins()
                .load(types::I64, MemFlagsData::new(), result_ptr, 8i32)
        } else {
            self.builder.ins().iconst(types::I64, 0)
        };
        let fail_id = self.func_id("willow_main_fail");
        let fail_ref = self.module.declare_func_in_func(fail_id, self.builder.func);
        self.builder.ins().call(fail_ref, &[msg]);
        // willow_main_fail is noreturn; trap to satisfy the verifier.
        self.builder.ins().trap(TrapCode::unwrap_user(1));

        // Ok: success — return void (process exits 0).
        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        self.builder.ins().return_(&[]);
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

    pub(super) fn emit_match(&mut self, m: &MatchExpr) -> cranelift_codegen::ir::Value {
        let scrutinee = self.emit_expr(&m.scrutinee);
        let scrutinee_ast_type = self.ast_type_of(&m.scrutinee);

        // Determine the result type: the checker's recorded type is
        // authoritative (a statement-position match is Void, willow-zvkv);
        // the structural arm-walk below only covers synthesized nodes.
        let result_ast_type = if let Some(ty) = self.expr_types.get(&m.span) {
            ty.clone()
        } else {
            let mut scratch = self.vars.clone();
            let mut found = Type::I64;
            'outer: for arm in &m.arms {
                match &self.resolved_pattern(arm) {
                    Pattern::Binding { name, .. } => {
                        let sty = scrutinee_ast_type.clone();
                        scratch.insert(
                            name.clone(),
                            VarStorage::Value {
                                var: self.builder.declare_var(clif_type(&sty)),
                                ty: sty,
                            },
                        );
                    }
                    Pattern::EnumVariantTuple {
                        enum_name,
                        variant,
                        bindings,
                        ..
                    } => {
                        // Resolve actual payload types — for generic types like Option/Result,
                        // use the type argument from the scrutinee rather than the placeholder.
                        let pts = self.resolve_variant_payload_types(
                            enum_name,
                            variant,
                            &scrutinee_ast_type,
                        );
                        for (name, ty) in bindings.iter().zip(pts.iter()) {
                            scratch.insert(
                                name.clone(),
                                VarStorage::Value {
                                    var: self.builder.declare_var(clif_type(ty)),
                                    ty: ty.clone(),
                                },
                            );
                        }
                    }
                    Pattern::ClassDowncast {
                        class_name,
                        binding,
                        ..
                    } if binding != "_" => {
                        let ty = Type::Named(class_name.clone());
                        scratch.insert(
                            binding.clone(),
                            VarStorage::Value {
                                var: self.builder.declare_var(clif_type(&ty)),
                                ty,
                            },
                        );
                    }
                    _ => {}
                }
                let ty = match &arm.body {
                    MatchBody::Expr(e) => {
                        ast_type_of_expr(e, &scratch, self.func_return_types, self.expr_types)
                    }
                    MatchBody::Block(_) => Type::Void,
                };
                if ty != Type::Void && ty != Type::Never {
                    found = ty;
                    break 'outer;
                }
            }
            found
        };
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
                let cond = self.emit_pattern_check(scrutinee, &pat);
                let fallthrough = next_block.unwrap_or(merge_block);
                self.builder
                    .ins()
                    .brif(cond, arm_block, &[], fallthrough, &[]);
            }

            self.builder.switch_to_block(arm_block);
            self.builder.seal_block(arm_block);

            // For binding patterns, define the variable
            let saved_vars = match &pat {
                Pattern::Binding { name, .. } => {
                    let var = self.builder.declare_var(types::I64);
                    self.builder.def_var(var, scrutinee);
                    let saved = self.vars.clone();
                    self.vars.insert(
                        name.clone(),
                        VarStorage::Value {
                            var,
                            ty: result_ast_type.clone(),
                        },
                    );
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
                        let offset = (1 + i) as i32 * 8;
                        let clif_ty = clif_type(payload_ty);
                        let raw = self.builder.ins().load(
                            types::I64,
                            MemFlagsData::new(),
                            scrutinee,
                            offset,
                        );
                        let val = if clif_ty == types::F64 {
                            self.builder
                                .ins()
                                .bitcast(types::F64, MemFlagsData::new(), raw)
                        } else if clif_ty == types::I8 {
                            self.builder.ins().ireduce(types::I8, raw)
                        } else {
                            raw
                        };
                        let var = self.builder.declare_var(clif_ty);
                        self.builder.def_var(var, val);
                        self.vars.insert(
                            binding.clone(),
                            VarStorage::Value {
                                var,
                                ty: payload_ty.clone(),
                            },
                        );
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
                    let var = self.builder.declare_var(types::I64);
                    self.builder.def_var(var, obj);
                    self.vars.insert(
                        binding.clone(),
                        VarStorage::Value {
                            var,
                            ty: Type::Named(class_name.clone()),
                        },
                    );
                    Some(saved)
                }
                _ => None,
            };

            let outer_terminated = self.terminated;
            self.terminated = false;
            let arm_val = self.emit_match_body(&arm.body, result_clif_type);

            if !self.terminated
                && let Some(arm_val) = arm_val
            {
                self.builder.def_var(result_var, arm_val);
                self.builder.ins().jump(merge_block, &[]);
                any_arm_merges = true;
            }
            self.terminated = outer_terminated;

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
        if any_arm_merges {
            self.builder.use_var(result_var)
        } else {
            // Every arm terminated (returned): the merge block is unreachable
            // and `result_var` was never defined on any path — produce a typed
            // dummy so the verifier is satisfied (willow-zvkv).
            match result_clif_type {
                types::F64 => self.builder.ins().f64const(0.0),
                ty => self.builder.ins().iconst(ty, 0),
            }
        }
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
                let expected = self.builder.ins().iconst(types::I64, tag);
                let actual_tag = self.emit_load_enum_tag(scrutinee);
                self.builder.ins().icmp(IntCC::Equal, actual_tag, expected)
            }
            Pattern::ClassDowncast { class_name, .. } => {
                // The scrutinee is an interface box {object@0, vtable@8}. Match
                // when the boxed object's runtime type_id (object word 0) equals
                // the target class's type_id (willow-1js.4).
                let Some(&type_id) = self.class_type_ids.get(class_name) else {
                    return self.builder.ins().iconst(types::I8, 0); // unknown class: never matches
                };
                if self.build_mode == BuildMode::Debug {
                    self.emit_nil_check(scrutinee, pattern.span(), "interface downcast box");
                }
                let obj = self
                    .builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), scrutinee, 0i32);
                if self.build_mode == BuildMode::Debug {
                    self.emit_nil_check(obj, pattern.span(), "interface downcast object");
                }
                let actual = self
                    .builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), obj, 0i32);
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
        tag: i64,
        args: &[crate::parser::ast::CallArg],
    ) -> cranelift_codegen::ir::Value {
        let field_count = args.len();
        let total_words = 1 + field_count;
        let size = self
            .builder
            .ins()
            .iconst(types::I64, (total_words * 8) as i64);
        // Compute gc_ref_mask: layout is [tag_word, payload_0, payload_1, ...].
        // Word 0 = tag (never a GC ref).
        // Word i+1 = args[i]; set bit i+1 if that arg is GC-managed.
        let gc_mask: i64 = args.iter().enumerate().fold(0i64, |mask, (i, arg)| {
            if is_gc_managed(&self.ast_type_of(&arg.expr), self.enum_infos) {
                mask | (1i64 << (i + 1))
            } else {
                mask
            }
        });
        let mask = self.builder.ins().iconst(types::I64, gc_mask);
        let alloc_id = self.func_id("willow_alloc_typed");
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self.builder.ins().call(alloc_ref, &[size, mask]);
        let ptr = self.builder.inst_results(call)[0];
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
            let offset = (1 + i) as i32 * 8;
            let val = self.emit_expr(&arg.expr);
            let val_i64 = if matches!(self.ast_type_of(&arg.expr), Type::F64) {
                self.builder
                    .ins()
                    .bitcast(types::I64, MemFlagsData::new(), val)
            } else {
                val
            };
            self.builder
                .ins()
                .store(MemFlagsData::new(), val_i64, ptr, offset);
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
            let addr = self.builder.ins().global_value(ptr_ty, gv);
            return self.builder.ins().load(ty, MemFlagsData::new(), addr, 0);
        }
        // Should be unreachable after type checking; fall back to a zero value.
        self.builder.ins().iconst(types::I64, 0)
    }
}
