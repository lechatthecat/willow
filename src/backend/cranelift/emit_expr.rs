use cranelift_codegen::ir::{
    InstBuilder, StackSlotData, StackSlotKind,
    condcodes::{FloatCC, IntCC},
    types,
};
use cranelift_module::Module;

use super::*;

impl<'a, 'b> FuncGen<'a, 'b> {
    pub(super) fn emit_expr(&mut self, expr: &Expr) -> cranelift_codegen::ir::Value {
        match expr {
            Expr::Integer(n, _) => self.builder.ins().iconst(types::I64, *n),
            Expr::Float(f, _) => self.builder.ins().f64const(*f),
            Expr::Bool(b, _) => self.builder.ins().iconst(types::I8, if *b { 1 } else { 0 }),
            Expr::Nil(_) => self.builder.ins().iconst(types::I64, 0),
            Expr::String(value, _) => self.emit_string_literal(value),
            Expr::Var(name, span) => {
                // A resolved unqualified fieldless enum variant (`Closed`),
                // lowered like the qualified `Enum::Closed` form (willow-60o.1).
                if let Some(enum_name) = self.enum_variant_resolutions.get(span).cloned()
                    && let Some(enum_info) = self.enum_infos.get(&enum_name).cloned()
                    && let Some(variant) = enum_info.variants.iter().find(|v| v.name == *name)
                {
                    if variant.payload_types.is_empty() && !self.enum_is_gc_object_type(&enum_name)
                    {
                        return self.builder.ins().iconst(types::I64, variant.tag);
                    }
                    return self.emit_enum_variant_alloc(variant.tag, &[]);
                }
                // Local variable or function value?
                if let Some(storage) = self.vars.get(name.as_str()).cloned() {
                    return self.load_var(&storage);
                }
                // Named function used as a first-class value — emit its address.
                if let Some(&fid) = self.func_ids.get(name.as_str()) {
                    let fref = self.module.declare_func_in_func(fid, self.builder.func);
                    return self.builder.ins().func_addr(types::I64, fref);
                }
                // The checker guarantees every variable resolves; reaching
                // here means checker and codegen scopes disagree — the exact
                // failure mode that silently compiled a lambda capture to 0
                // (willow-thqe). Fail LOUDLY so the test suite detects the
                // whole bug class instead of shipping wrong values.
                panic!(
                    "internal compiler error: variable `{name}` reached codegen unbound \
                     (checker/codegen scope mismatch)"
                );
            }
            Expr::Binary(b) => self.emit_binary(b),
            Expr::Unary(u) => self.emit_unary(u),
            Expr::Call(c) => self.emit_call(c),
            Expr::Print(arg, newline, _) => {
                let val = self.emit_expr(arg);
                let arg_ty_raw = self.ast_type_of(arg);
                // Unwrap Nullable so that printing a nil-checked T? behaves like T.
                let arg_ty = match arg_ty_raw {
                    Type::Nullable(inner) => *inner,
                    ty => ty,
                };
                let fn_name = match (arg_ty, newline) {
                    (Type::I64, false) => "willow_print_i64",
                    (Type::I64, true) => "willow_println_i64",
                    (Type::F64, false) => "willow_print_f64",
                    (Type::F64, true) => "willow_println_f64",
                    (Type::Bool, false) => "willow_print_bool",
                    (Type::Bool, true) => "willow_println_bool",
                    (Type::String, false) => "willow_print_string",
                    (Type::String, true) => "willow_println_string",
                    _ => "",
                };
                if !fn_name.is_empty() {
                    let fid = self.func_ids[fn_name];
                    let fref = self.module.declare_func_in_func(fid, self.builder.func);
                    self.builder.ins().call(fref, &[val]);
                }
                self.builder.ins().iconst(types::I8, 0)
            }
            Expr::Ternary(t) => self.emit_ternary(t),
            Expr::Range(r) => self.emit_range_value(r),
            // Lambda: emit the address of the pre-compiled private function.
            Expr::Lambda(l) => {
                if let Some(name) = self.lambda_names.get(&l.span)
                    && let Some(&fid) = self.func_ids.get(name.as_str())
                {
                    let fref = self.module.declare_func_in_func(fid, self.builder.func);
                    return self.builder.ins().func_addr(types::I64, fref);
                }
                self.builder.ins().iconst(types::I64, 0)
            }
            // Field/method access: codegen deferred to willow-jbf
            Expr::FieldAccess(obj, field_name, _) => self.emit_field_access(obj, field_name),
            Expr::MethodCall(m) => self.emit_method_call(m),
            Expr::ObjectLiteral(o) => self.emit_object_literal(o),
            Expr::StaticCall(s) => self.emit_static_call(s),
            Expr::StaticField(s) => self.emit_static_field_read(&s.class, &s.field),
            Expr::New(n) => self.emit_new(n),
            Expr::Await(a) => self.emit_await(a),
            Expr::Select(s) => {
                self.emit_select(s);
                self.builder.ins().iconst(types::I8, 0)
            }
            Expr::Match(m) => self.emit_match(m),
            Expr::TryPropagate(inner, _) => self.emit_try_propagate(inner),
            Expr::ArrayLiteral(elements, _) => {
                let elem_ty = elements
                    .first()
                    .map(|e| self.ast_type_of(e))
                    .unwrap_or(Type::Void);
                self.emit_array_literal(elements, &elem_ty)
            }
            Expr::Index(arr, index, _) => {
                // Null and out-of-bounds are checked inside `willow_array_get`,
                // which aborts with a clear message.
                let elem_ty = array_element_type(&self.ast_type_of(arr));
                self.emit_index(arr, index, &elem_ty)
            }
        }
    }

    pub(super) fn emit_string_literal(&mut self, value: &str) -> cranelift_codegen::ir::Value {
        if let Some(data_id) = self.string_literals.get(value) {
            // Load the address of the static raw bytes.
            let gv = self
                .module
                .declare_data_in_func(*data_id, self.builder.func);
            let ptr_ty = self.module.target_config().pointer_type();
            let bytes_ptr = self.builder.ins().global_value(ptr_ty, gv);
            // Call willow_string_literal to get (or create) a permanent WillowString.
            let len_val = self.builder.ins().iconst(types::I64, value.len() as i64);
            let fid = self.func_id("willow_string_literal");
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let call = self.builder.ins().call(fref, &[bytes_ptr, len_val]);
            return self.builder.inst_results(call)[0];
        }
        self.builder.ins().iconst(types::I64, 0)
    }

    pub(super) fn emit_binary(&mut self, b: &BinaryExpr) -> cranelift_codegen::ir::Value {
        let lhs = self.emit_expr(&b.lhs);
        let lty = self.ast_type_of(&b.lhs);
        let is_float = lty == Type::F64;
        match &b.op {
            BinOp::And => self.emit_short_circuit_and(lhs, &b.rhs),
            BinOp::Or => self.emit_short_circuit_or(lhs, &b.rhs),
            BinOp::Add => {
                if lty == Type::String {
                    // Root lhs before evaluating rhs: rhs evaluation may call
                    // willow_alloc_typed (e.g. concat or object allocation) which
                    // triggers a GC cycle.  Without rooting, an intermediate concat
                    // result held only in lhs's SSA register would be freed.
                    self.emit_push_root(lhs);
                    let rhs = self.emit_expr(&b.rhs);
                    // Root rhs before the concat call itself, which also allocates.
                    self.emit_push_root(rhs);
                    let fid = self.func_id("willow_string_concat");
                    let fref = self.module.declare_func_in_func(fid, self.builder.func);
                    let call = self.builder.ins().call(fref, &[lhs, rhs]);
                    let result = self.builder.inst_results(call)[0];
                    // Pop the two temporary roots (net gc_root_count change is 0).
                    self.emit_pop_roots_n(2);
                    self.gc_root_count -= 2;
                    result
                } else {
                    let rhs = self.emit_expr(&b.rhs);
                    if is_float {
                        self.builder.ins().fadd(lhs, rhs)
                    } else {
                        self.builder.ins().iadd(lhs, rhs)
                    }
                }
            }
            BinOp::Sub => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    self.builder.ins().fsub(lhs, rhs)
                } else {
                    self.builder.ins().isub(lhs, rhs)
                }
            }
            BinOp::Mul => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    self.builder.ins().fmul(lhs, rhs)
                } else {
                    self.builder.ins().imul(lhs, rhs)
                }
            }
            BinOp::Div => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    self.builder.ins().fdiv(lhs, rhs)
                } else {
                    self.emit_int_div_guard(lhs, rhs, false, b.span);
                    self.builder.ins().sdiv(lhs, rhs)
                }
            }
            BinOp::Rem => {
                let rhs = self.emit_expr(&b.rhs);
                self.emit_int_div_guard(lhs, rhs, true, b.span);
                self.builder.ins().srem(lhs, rhs)
            }
            BinOp::Lt => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::LessThan, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedLessThan, lhs, rhs)
                }
            }
            BinOp::Le => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::LessThanOrEqual, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedLessThanOrEqual, lhs, rhs)
                }
            }
            BinOp::Gt => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::GreaterThan, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedGreaterThan, lhs, rhs)
                }
            }
            BinOp::Ge => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::GreaterThanOrEqual, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedGreaterThanOrEqual, lhs, rhs)
                }
            }
            BinOp::Eq => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::Equal, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::Equal, lhs, rhs)
                }
            }
            BinOp::Ne => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::NotEqual, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::NotEqual, lhs, rhs)
                }
            }
        }
    }

    pub(super) fn emit_short_circuit_and(
        &mut self,
        lhs: cranelift_codegen::ir::Value,
        rhs_expr: &Expr,
    ) -> cranelift_codegen::ir::Value {
        let result_var = self.builder.declare_var(types::I8);
        let rhs_block = self.builder.create_block();
        let false_block = self.builder.create_block();
        let merge_block = self.builder.create_block();

        self.builder
            .ins()
            .brif(lhs, rhs_block, &[], false_block, &[]);

        self.builder.switch_to_block(rhs_block);
        self.builder.seal_block(rhs_block);
        let rhs = self.emit_expr(rhs_expr);
        self.builder.def_var(result_var, rhs);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(false_block);
        self.builder.seal_block(false_block);
        let false_value = self.builder.ins().iconst(types::I8, 0);
        self.builder.def_var(result_var, false_value);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.builder.use_var(result_var)
    }

    pub(super) fn emit_short_circuit_or(
        &mut self,
        lhs: cranelift_codegen::ir::Value,
        rhs_expr: &Expr,
    ) -> cranelift_codegen::ir::Value {
        let result_var = self.builder.declare_var(types::I8);
        let true_block = self.builder.create_block();
        let rhs_block = self.builder.create_block();
        let merge_block = self.builder.create_block();

        self.builder
            .ins()
            .brif(lhs, true_block, &[], rhs_block, &[]);

        self.builder.switch_to_block(true_block);
        self.builder.seal_block(true_block);
        let true_value = self.builder.ins().iconst(types::I8, 1);
        self.builder.def_var(result_var, true_value);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(rhs_block);
        self.builder.seal_block(rhs_block);
        let rhs = self.emit_expr(rhs_expr);
        self.builder.def_var(result_var, rhs);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.builder.use_var(result_var)
    }

    pub(super) fn emit_unary(&mut self, u: &UnaryExpr) -> cranelift_codegen::ir::Value {
        let val = self.emit_expr(&u.expr);
        let ty = ast_type_of_expr(&u.expr, &self.vars, self.func_return_types, self.expr_types);
        match &u.op {
            UnaryOp::Neg => {
                if ty == Type::F64 {
                    self.builder.ins().fneg(val)
                } else {
                    self.builder.ins().ineg(val)
                }
            }
            UnaryOp::Not => {
                let one = self.builder.ins().iconst(types::I8, 1);
                self.builder.ins().bxor(val, one)
            }
        }
    }

    pub(super) fn emit_call(&mut self, c: &CallExpr) -> cranelift_codegen::ir::Value {
        if c.callee == "format" {
            return self.emit_format_call(c);
        }

        // An unqualified enum-variant construction (`Ok(42)`) the type checker
        // resolved to an enum: lower like the qualified `Enum::Variant(..)` form
        // (willow-60o.1).
        if let Some(enum_name) = self.enum_variant_resolutions.get(&c.span).cloned()
            && let Some(enum_info) = self.enum_infos.get(&enum_name).cloned()
            && let Some(variant) = enum_info.variants.iter().find(|v| v.name == c.callee)
        {
            if variant.payload_types.is_empty() && !self.enum_is_gc_object_type(&enum_name) {
                return self.builder.ins().iconst(types::I64, variant.tag);
            }
            if variant.payload_types.is_empty() {
                return self.emit_enum_variant_alloc(variant.tag, &[]);
            }
            return self.emit_enum_variant_alloc(variant.tag, &c.args);
        }

        // Direct call to a known function.
        if let Some(&fid) = self.func_ids.get(&c.callee) {
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let modes = self.func_param_modes.get(&c.callee).cloned();
            let param_debug = self.func_param_debug.get(&c.callee).cloned();
            let param_types = self.fn_param_types(&c.callee);
            let has_reference_args = has_reference_args(modes.as_deref(), &c.args);
            let (args, temp_roots) = self.emit_call_args_rooted_coerced(
                Some(&c.callee),
                modes.as_deref(),
                param_debug.as_deref(),
                param_types.as_deref(),
                &c.args,
            );
            // Debug builds: record this call on the call-chain stack so a panic
            // inside the callee prints an ordered trace (willow-992h). Pushed
            // after args are evaluated (so nested calls nest correctly) and
            // popped right after the call returns.
            let pushed_frame = self.emit_callstack_push(&c.callee, c.span);
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            let result = if results.is_empty() {
                self.builder.ins().iconst(types::I8, 0)
            } else {
                results[0]
            };
            if pushed_frame {
                self.emit_callstack_pop();
            }
            if has_reference_args {
                self.emit_debug_reference_call_clear();
            }
            self.emit_pop_roots_n(temp_roots);
            self.gc_root_count -= temp_roots;
            // A call to an async fn spawned a task and returned its frame:
            // record the spawn call-site for traces (willow-0a6k.7).
            if self.cooperative_leaves.contains(
                &crate::semantic::ids::FunctionId::free_from_source_name(&c.callee),
            ) {
                let task_id = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    result,
                    super::async_frame_slot_offset(super::FRAME_SLOT_TASK_ID),
                );
                self.emit_set_spawn_site(task_id, c.span.line);
            }
            return result;
        }

        // panic(message) / panic(spec, args...) — assemble the message, call
        // willow_panic and trap (noreturn). Multi-arg panics interpolate the
        // spec exactly like `format` (willow-csax).
        if c.callee == "panic" {
            let msg = if c.args.len() > 1 {
                if let Expr::String(spec, _) = &c.args[0].expr {
                    let spec = spec.clone();
                    self.emit_interpolated_string(&spec, &c.args[1..])
                } else {
                    self.emit_expr(&c.args[0].expr)
                }
            } else {
                c.args
                    .first()
                    .map(|a| self.emit_expr(&a.expr))
                    .unwrap_or_else(|| self.emit_string_literal("explicit panic"))
            };
            // Debug builds report the panic source location via willow_panic_at;
            // release builds use the plain willow_panic (willow-4j6).
            if self.build_mode == BuildMode::Debug {
                let source_file = self.source_file.to_string();
                let file_ptr = self.emit_string_literal(&source_file);
                let line = self.builder.ins().iconst(types::I32, c.span.line as i64);
                let col = self.builder.ins().iconst(types::I32, c.span.col as i64);
                let fid = self.func_id("willow_panic_at");
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder.ins().call(fref, &[msg, file_ptr, line, col]);
            } else {
                let fid = self.func_id("willow_panic");
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder.ins().call(fref, &[msg]);
            }
            // Produce the (unreachable) result value BEFORE the trap: `trap`
            // terminates the block, so no instruction may follow it. willow_panic
            // is noreturn; the trap just gives the block a terminator.
            let result = self.builder.ins().iconst(types::I64, 0);
            self.builder.ins().trap(TrapCode::unwrap_user(1));
            self.terminated = true;
            return result;
        }

        if let Some(runtime_name) = builtin_call_runtime_name(&c.callee) {
            let fid = self.func_ids[runtime_name];
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let args: Vec<_> = c.args.iter().map(|a| self.emit_expr(&a.expr)).collect();
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            return if results.is_empty() {
                self.builder.ins().iconst(types::I8, 0)
            } else {
                results[0]
            };
        }

        // Indirect call through a function-value local variable.
        if let Some(storage) = self.vars.get(&c.callee).cloned()
            && let Type::Fn(param_types, ret_type) = storage.ty().clone()
        {
            let callee_val = self.load_var(&storage);
            let args: Vec<_> = c.args.iter().map(|a| self.emit_expr(&a.expr)).collect();

            // Build the Cranelift signature matching the function type.
            let mut sig = self.module.make_signature();
            for pt in &param_types {
                sig.params.push(AbiParam::new(clif_type(pt)));
            }
            let ret_clif = clif_type(&ret_type);
            let has_return = *ret_type != Type::Void;
            if has_return {
                sig.returns.push(AbiParam::new(ret_clif));
            }
            let sig_ref = self.builder.import_signature(sig);
            let call = self.builder.ins().call_indirect(sig_ref, callee_val, &args);
            let results = self.builder.inst_results(call);
            return if results.is_empty() {
                self.builder.ins().iconst(types::I8, 0)
            } else {
                results[0]
            };
        }

        // Should not reach here after type checking.
        self.builder.ins().iconst(types::I64, 0)
    }

    /// Evaluate call arguments left-to-right, pushing each GC-managed argument
    /// value as a temporary root before evaluating subsequent arguments.
    /// Returns (arg_values, number_of_temporary_roots_pushed).
    /// The caller must call emit_pop_roots_n(temp_roots) + gc_root_count -= temp_roots
    /// immediately after emitting the call instruction.
    pub(super) fn emit_call_args_rooted(
        &mut self,
        callee: Option<&str>,
        modes: Option<&[ParamMode]>,
        param_debug: Option<&[ParamDebug]>,
        args: &[CallArg],
    ) -> (Vec<cranelift_codegen::ir::Value>, usize) {
        self.emit_call_args_rooted_coerced(callee, modes, param_debug, None, args)
    }

    /// Like [`emit_call_args_rooted`] but, when `param_types` is provided, boxes
    /// each class argument passed to an interface-typed parameter (spec §9.3).
    pub(super) fn emit_call_args_rooted_coerced(
        &mut self,
        callee: Option<&str>,
        modes: Option<&[ParamMode]>,
        param_debug: Option<&[ParamDebug]>,
        param_types: Option<&[Type]>,
        args: &[CallArg],
    ) -> (Vec<cranelift_codegen::ir::Value>, usize) {
        let mut values = Vec::with_capacity(args.len());
        let mut temp_roots = 0usize;

        for (idx, arg) in args.iter().enumerate() {
            let is_ref = matches!(
                modes.and_then(|m| m.get(idx)),
                Some(ParamMode::Reference { .. })
            );
            let (val, reference_roots) = if is_ref {
                self.emit_debug_reference_call_hook(callee, idx, arg, modes, param_debug);
                self.emit_reference_arg_address(arg)
            } else {
                let raw = self.emit_expr(&arg.expr);
                // Box a class argument when the parameter type is an interface.
                let coerced = match param_types.and_then(|pt| pt.get(idx)) {
                    Some(target) => {
                        let value_ty = self.ast_type_of(&arg.expr);
                        self.coerce_to_target(raw, &value_ty, target)
                    }
                    None => raw,
                };
                (coerced, 0)
            };
            values.push(val);
            temp_roots += reference_roots;

            // Root GC-managed arguments so later argument evaluations (which may
            // allocate and trigger GC) cannot collect them.
            if !is_ref {
                let arg_ty = self.ast_type_of_init(&arg.expr);
                if is_gc_managed(&arg_ty, self.enum_infos) {
                    self.emit_push_root(val);
                    temp_roots += 1;
                }
            }
        }

        (values, temp_roots)
    }

    pub(super) fn emit_debug_reference_call_hook(
        &mut self,
        callee: Option<&str>,
        idx: usize,
        arg: &CallArg,
        modes: Option<&[ParamMode]>,
        param_debug: Option<&[ParamDebug]>,
    ) {
        if self.build_mode != BuildMode::Debug {
            return;
        }

        let ampersand_span = match &arg.mode {
            CallArgMode::Reference { ampersand_span } => *ampersand_span,
            CallArgMode::Value => return,
        };

        let param = param_debug.and_then(|params| params.get(idx));
        let param_mode = param
            .map(|param| &param.mode)
            .or_else(|| modes.and_then(|modes| modes.get(idx)));
        let mode = param_mode.map(reference_mode_name).unwrap_or("&");
        let param_name = param
            .map(|param| param.name.as_str())
            .unwrap_or("<unknown>");
        let param_type = param
            .map(|param| debug_type_name(&param.ty))
            .unwrap_or_else(|| "<unknown>".to_string());
        let callee = callee.unwrap_or("<unknown>");
        let place_kind = reference_place_kind(&arg.expr);
        let place_name = reference_place_name(&arg.expr);

        let source_file = self.source_file.to_string();
        let file_ptr = self.emit_string_literal(&source_file);
        let line_val = self
            .builder
            .ins()
            .iconst(types::I32, ampersand_span.line as i64);
        let col_val = self
            .builder
            .ins()
            .iconst(types::I32, ampersand_span.col as i64);
        let callee_ptr = self.emit_string_literal(callee);
        let param_ptr = self.emit_string_literal(param_name);
        let param_type_ptr = self.emit_string_literal(&param_type);
        let mode_ptr = self.emit_string_literal(mode);
        let place_kind_ptr = self.emit_string_literal(place_kind);
        let place_name_ptr = self.emit_string_literal(&place_name);
        let hook_id = self.func_id("willow_debug_reference_call");
        let hook_ref = self.module.declare_func_in_func(hook_id, self.builder.func);
        self.builder.ins().call(
            hook_ref,
            &[
                file_ptr,
                line_val,
                col_val,
                callee_ptr,
                param_ptr,
                param_type_ptr,
                mode_ptr,
                place_kind_ptr,
                place_name_ptr,
            ],
        );
    }

    pub(super) fn emit_debug_reference_call_clear(&mut self) {
        if self.build_mode != BuildMode::Debug {
            return;
        }
        let clear_id = self.func_id("willow_debug_reference_call_clear");
        let clear_ref = self
            .module
            .declare_func_in_func(clear_id, self.builder.func);
        self.builder.ins().call(clear_ref, &[]);
    }

    /// Address + length of a declared string literal's raw static UTF-8 bytes,
    /// without interning a (GC-heap) WillowString. `None` if the literal was not
    /// collected/declared.
    pub(super) fn emit_static_str_bytes(
        &mut self,
        value: &str,
    ) -> Option<(cranelift_codegen::ir::Value, cranelift_codegen::ir::Value)> {
        let data_id = *self.string_literals.get(value)?;
        let gv = self.module.declare_data_in_func(data_id, self.builder.func);
        let ptr_ty = self.module.target_config().pointer_type();
        let bytes_ptr = self.builder.ins().global_value(ptr_ty, gv);
        let len = self.builder.ins().iconst(types::I64, value.len() as i64);
        Some((bytes_ptr, len))
    }

    /// Debug builds: push a call-chain frame (callee name + call-site location)
    /// before a user-function call. Returns `true` when a frame was pushed (so
    /// the caller knows to emit the matching pop). Passes raw static bytes (not
    /// WillowStrings) so the call stack does not allocate on the GC heap. Release
    /// builds are untouched (willow-992h).
    /// Debug builds: guard an integer `/` or `%` against a zero divisor and
    /// the `i64::MIN / -1` overflow, reporting a located runtime panic instead
    /// of a raw hardware trap (willow-l9lx). No-op in release builds (the
    /// Cranelift trap still aborts safely there).
    pub(super) fn emit_int_div_guard(
        &mut self,
        lhs: cranelift_codegen::ir::Value,
        rhs: cranelift_codegen::ir::Value,
        is_rem: bool,
        span: crate::diagnostics::Span,
    ) {
        if self.build_mode != BuildMode::Debug {
            return;
        }
        let panic_block = self.builder.create_block();
        self.builder.append_block_param(panic_block, types::I64); // kind
        let overflow_check = self.builder.create_block();
        let ok_block = self.builder.create_block();

        let zero_kind = self
            .builder
            .ins()
            .iconst(types::I64, if is_rem { 2 } else { 0 });
        let is_zero = self.builder.ins().icmp_imm(IntCC::Equal, rhs, 0);
        self.builder.ins().brif(
            is_zero,
            panic_block,
            &[zero_kind.into()],
            overflow_check,
            &[],
        );

        self.builder.switch_to_block(overflow_check);
        self.builder.seal_block(overflow_check);
        let is_min = self.builder.ins().icmp_imm(IntCC::Equal, lhs, i64::MIN);
        let is_neg1 = self.builder.ins().icmp_imm(IntCC::Equal, rhs, -1);
        let overflows = self.builder.ins().band(is_min, is_neg1);
        let ovf_kind = self
            .builder
            .ins()
            .iconst(types::I64, if is_rem { 3 } else { 1 });
        self.builder
            .ins()
            .brif(overflows, panic_block, &[ovf_kind.into()], ok_block, &[]);

        self.builder.switch_to_block(panic_block);
        self.builder.seal_block(panic_block);
        let kind = self.builder.block_params(panic_block)[0];
        let source_file = self.source_file.to_string();
        let file_ptr = self.emit_string_literal(&source_file);
        let line_val = self.builder.ins().iconst(types::I32, span.line as i64);
        let col_val = self.builder.ins().iconst(types::I32, span.col as i64);
        let panic_id = self.func_id("willow_int_div_panic");
        let panic_ref = self
            .module
            .declare_func_in_func(panic_id, self.builder.func);
        self.builder
            .ins()
            .call(panic_ref, &[kind, file_ptr, line_val, col_val]);
        self.builder.ins().trap(TrapCode::unwrap_user(1));

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
    }

    pub(super) fn emit_callstack_push(
        &mut self,
        callee: &str,
        span: crate::diagnostics::Span,
    ) -> bool {
        if self.build_mode != BuildMode::Debug {
            return false;
        }
        let Some((name_ptr, name_len)) = self.emit_static_str_bytes(callee) else {
            return false;
        };
        let file = self.source_file.to_string();
        let Some((file_ptr, file_len)) = self.emit_static_str_bytes(&file) else {
            return false;
        };
        let line = self.builder.ins().iconst(types::I32, span.line as i64);
        let col = self.builder.ins().iconst(types::I32, span.col as i64);
        let push_id = self.func_id("willow_callstack_push");
        let push_ref = self.module.declare_func_in_func(push_id, self.builder.func);
        self.builder.ins().call(
            push_ref,
            &[name_ptr, name_len, file_ptr, file_len, line, col],
        );
        true
    }

    /// Debug builds: pop the most recent call-chain frame after a call returns.
    pub(super) fn emit_callstack_pop(&mut self) {
        let pop_id = self.func_id("willow_callstack_pop");
        let pop_ref = self.module.declare_func_in_func(pop_id, self.builder.func);
        self.builder.ins().call(pop_ref, &[]);
    }

    pub(super) fn emit_reference_arg_address(
        &mut self,
        arg: &CallArg,
    ) -> (cranelift_codegen::ir::Value, usize) {
        match &arg.expr {
            Expr::Var(name, _) => {
                let storage = self.vars.get(name.as_str()).cloned();
                match storage {
                    Some(VarStorage::Stack { slot, .. }) => {
                        let ptr_ty = self.module.target_config().pointer_type();
                        return (self.builder.ins().stack_addr(ptr_ty, slot, 0), 0);
                    }
                    Some(VarStorage::Value { var, ty }) => {
                        // Lazy promotion: this variable is being passed by &mut for the first
                        // time.  Promote it from a Cranelift SSA variable to a stack slot so
                        // the callee can write through the pointer and future reads see the
                        // updated value.
                        let ptr_ty = self.module.target_config().pointer_type();
                        let val = self.builder.use_var(var);
                        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                            StackSlotKind::ExplicitSlot,
                            8,
                            0,
                        ));
                        self.builder.ins().stack_store(val, slot, 0);
                        let ty_clone = ty.clone();
                        self.vars
                            .insert(name.clone(), VarStorage::Stack { slot, ty: ty_clone });
                        return (self.builder.ins().stack_addr(ptr_ty, slot, 0), 0);
                    }
                    Some(VarStorage::ReferencePtr { var, .. }) => {
                        return (self.builder.use_var(var), 0);
                    }
                    Some(VarStorage::Frame { offset, .. }) => {
                        // Address of the frame slot (frame is GC-rooted, non-moving).
                        let base = self
                            .async_frame
                            .expect("frame-backed var requires an allocated async frame");
                        return (self.builder.ins().iadd_imm(base, offset as i64), 0);
                    }
                    None => {}
                }
            }
            Expr::FieldAccess(obj, field_name, span) => {
                return (self.emit_field_address(obj, field_name, *span), 0);
            }
            Expr::Index(array, index, _) => {
                return self.emit_array_element_address(array, index);
            }
            _ => {}
        }
        (self.builder.ins().iconst(types::I64, 0), 0)
    }
}
