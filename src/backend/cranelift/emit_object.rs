use cranelift_codegen::ir::{InstBuilder, MemFlags, condcodes::IntCC, types};
use cranelift_module::Module;

use super::*;

impl<'a, 'b> FuncGen<'a, 'b> {
    pub(super) fn emit_field_address(
        &mut self,
        obj: &Expr,
        field_name: &str,
        _span: crate::diagnostics::Span,
    ) -> cranelift_codegen::ir::Value {
        let ptr = self.emit_expr(obj);

        if self.build_mode == BuildMode::Debug {
            self.emit_nil_check(ptr, obj.span(), field_name);
        }

        let obj_type = self.ast_type_of(obj);
        if let Some(class_name) = class_name_for_object_type(&obj_type)
            && let Some(layout) = self.class_layouts.get(&class_name)
            && let Some(idx) = layout.iter().position(|(n, _)| n == field_name)
        {
            let offset = (idx as i64 + 1) * 8;
            return self.builder.ins().iadd_imm(ptr, offset);
        }
        self.builder.ins().iconst(types::I64, 0)
    }

    pub(super) fn emit_array_element_address(
        &mut self,
        array: &Expr,
        index: &Expr,
    ) -> (cranelift_codegen::ir::Value, usize) {
        let arr = self.emit_expr(array);
        // Keep the array alive while evaluating the index and while the callee
        // reads/writes through the returned element slot pointer.
        self.emit_push_root(arr);
        let index = self.emit_expr(index);
        let addr_id = self.func_id("willow_array_element_addr");
        let addr_ref = self.module.declare_func_in_func(addr_id, self.builder.func);
        let call = self.builder.ins().call(addr_ref, &[arr, index]);
        (self.builder.inst_results(call)[0], 1)
    }

    pub(super) fn emit_format_call(&mut self, c: &CallExpr) -> cranelift_codegen::ir::Value {
        let runtime_name = match c.args.first().map(|arg| &arg.expr) {
            Some(Expr::String(spec, _)) => match spec.as_str() {
                "{:.17g}" => "willow_format_f64_17g",
                "{:.16f}" => "willow_format_f64_16f",
                "{:.6f}" => "willow_format_f64_6f",
                _ => "",
            },
            _ => "",
        };
        if runtime_name.is_empty() || c.args.len() < 2 {
            return self.builder.ins().iconst(types::I64, 0);
        }

        let value = self.emit_expr(&c.args[1].expr);
        let fid = self.func_ids[runtime_name];
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, &[value]);
        self.builder.inst_results(call)[0]
    }

    pub(super) fn emit_object_literal(
        &mut self,
        o: &ObjectLiteralExpr,
    ) -> cranelift_codegen::ir::Value {
        let layout = match self.class_layouts.get(&o.class).cloned() {
            Some(l) => l,
            None => return self.builder.ins().iconst(types::I64, 0),
        };
        // Object layout: word 0 = type_id (i64), words 1..N = fields.
        let size = (layout.len() as i64 + 1) * 8;
        let size_val = self.builder.ins().iconst(types::I64, size);
        let ref_mask = gc_ref_mask_for_layout(&o.class, &layout, self.enum_infos);
        let ref_mask_val = self.builder.ins().iconst(types::I64, ref_mask as i64);
        let alloc_id = self.func_id("willow_alloc_typed");
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self
            .builder
            .ins()
            .call(alloc_ref, &[size_val, ref_mask_val]);
        let ptr = self.builder.inst_results(call)[0];

        // Root ptr immediately: evaluating field initialiser expressions below
        // may trigger allocations and GC cycles before all fields are stored.
        // Without this root, GC could collect the partially-initialised object.
        self.emit_push_root(ptr);

        // Store the type_id at offset 0.
        let type_id = self.class_type_ids.get(&o.class).copied().unwrap_or(0);
        let type_id_val = self.builder.ins().iconst(types::I64, type_id);
        self.builder
            .ins()
            .store(MemFlags::new(), type_id_val, ptr, 0i32);

        // Store each field at offset (idx + 1) * 8 to leave word 0 for type_id.
        for field in &o.fields {
            if let Some(idx) = layout.iter().position(|(n, _)| n == &field.name) {
                let offset = (idx as i32 + 1) * 8;
                // Box a class value when the field's declared type is an interface.
                let field_ty = layout[idx].1.clone();
                let val = self.emit_expr_coerced(&field.value, &field_ty);
                self.builder.ins().store(MemFlags::new(), val, ptr, offset);
            }
        }

        // Pop the temporary construction root; the caller will root ptr via
        // its own let-binding or return value handling.
        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;

        ptr
    }

    /// Lower `new Class(args...)` (willow-scq2 §5/§12): allocate a zero-init
    /// object, call the explicit `Class__init(self, args...)` (or store args
    /// memberwise for the implicit constructor), and return the object.
    pub(super) fn emit_new(&mut self, n: &NewExpr) -> cranelift_codegen::ir::Value {
        let layout = match self.class_layouts.get(&n.class_name).cloned() {
            Some(l) => l,
            None => return self.builder.ins().iconst(types::I64, 0),
        };
        // Object layout: word 0 = type_id (i64), words 1..N = fields. Allocating
        // with the GC ref-mask leaves reference fields zero/null until assigned,
        // so a collection mid-construction is safe (willow-scq2 §12.3).
        let size = (layout.len() as i64 + 1) * 8;
        let size_val = self.builder.ins().iconst(types::I64, size);
        let ref_mask = gc_ref_mask_for_layout(&n.class_name, &layout, self.enum_infos);
        let ref_mask_val = self.builder.ins().iconst(types::I64, ref_mask as i64);
        let alloc_id = self.func_id("willow_alloc_typed");
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self
            .builder
            .ins()
            .call(alloc_ref, &[size_val, ref_mask_val]);
        let ptr = self.builder.inst_results(call)[0];

        let type_id = self.class_type_ids.get(&n.class_name).copied().unwrap_or(0);
        let type_id_val = self.builder.ins().iconst(types::I64, type_id);
        self.builder
            .ins()
            .store(MemFlags::new(), type_id_val, ptr, 0i32);

        // Root the new object across argument evaluation and the init body: both
        // may allocate and trigger a collection.
        self.emit_push_root(ptr);

        let mangled = class_method_symbol_name(self.known_modules, &n.class_name, "init");
        if let Some(&init_fid) = self.func_ids.get(&mangled) {
            // Explicit constructor. Param types come from the synthesized init
            // method's fn type (drop the leading `self`).
            let param_types: Vec<Type> = match self.fn_types.get(&mangled) {
                Some(Type::Fn(ps, _)) => ps.iter().skip(1).cloned().collect(),
                _ => Vec::new(),
            };
            let (arg_vals, arg_roots) = self.emit_call_args_rooted_coerced(
                Some(&mangled),
                None,
                None,
                Some(&param_types),
                &n.args,
            );
            let init_ref = self
                .module
                .declare_func_in_func(init_fid, self.builder.func);
            let mut call_args = vec![ptr];
            call_args.extend(arg_vals);
            self.builder.ins().call(init_ref, &call_args);
            if arg_roots > 0 {
                self.emit_pop_roots_n(arg_roots);
                self.gc_root_count -= arg_roots;
            }
        } else {
            // Implicit memberwise constructor: store each arg positionally into
            // its field slot (declaration order).
            for (i, arg) in n.args.iter().enumerate() {
                if let Some((_, field_ty)) = layout.get(i) {
                    let field_ty = field_ty.clone();
                    let val = self.emit_expr_coerced(&arg.expr, &field_ty);
                    let offset = (i as i32 + 1) * 8;
                    self.builder.ins().store(MemFlags::new(), val, ptr, offset);
                }
            }
        }

        // Drop the construction root; the caller roots `ptr` via its binding.
        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;
        ptr
    }

    /// Lower `super.init(args...)` inside a constructor. Explicit base
    /// constructors are normal `init` methods; implicit base constructors store
    /// memberwise args into the already-allocated `self` object's base slots.
    pub(super) fn emit_super_init(&mut self, s: &SuperInitStmt) {
        let Some(current_class) = self.current_class else {
            for arg in &s.args {
                self.emit_expr(&arg.expr);
            }
            return;
        };
        let Some(base_name) = self.class_base.get(current_class).cloned() else {
            for arg in &s.args {
                self.emit_expr(&arg.expr);
            }
            return;
        };
        let Some(self_storage) = self.vars.get("self").cloned() else {
            for arg in &s.args {
                self.emit_expr(&arg.expr);
            }
            return;
        };
        let self_ptr = self.load_var(&self_storage);

        let mangled = class_method_symbol_name(self.known_modules, &base_name, "init");
        if let Some(&init_fid) = self.func_ids.get(&mangled) {
            let param_types: Vec<Type> = match self.fn_types.get(&mangled) {
                Some(Type::Fn(ps, _)) => ps.iter().skip(1).cloned().collect(),
                _ => Vec::new(),
            };
            let (arg_vals, arg_roots) = self.emit_call_args_rooted_coerced(
                Some(&mangled),
                None,
                None,
                Some(&param_types),
                &s.args,
            );
            let init_ref = self
                .module
                .declare_func_in_func(init_fid, self.builder.func);
            let mut call_args = vec![self_ptr];
            call_args.extend(arg_vals);
            self.builder.ins().call(init_ref, &call_args);
            if arg_roots > 0 {
                self.emit_pop_roots_n(arg_roots);
                self.gc_root_count -= arg_roots;
            }
            return;
        }

        if let Some(layout) = self.class_layouts.get(&base_name).cloned() {
            for (i, arg) in s.args.iter().enumerate() {
                if let Some((_, field_ty)) = layout.get(i) {
                    let field_ty = field_ty.clone();
                    let val = self.emit_expr_coerced(&arg.expr, &field_ty);
                    let offset = (i as i32 + 1) * 8;
                    self.builder
                        .ins()
                        .store(MemFlags::new(), val, self_ptr, offset);
                }
            }
        } else {
            for arg in &s.args {
                self.emit_expr(&arg.expr);
            }
        }
    }

    /// Emit a nil pointer check in debug builds.
    /// If `ptr` is null at runtime, calls `willow_nil_deref` with source location and
    /// `context` (field or method name) then traps. Otherwise execution continues.
    pub(super) fn emit_nil_check(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        span: crate::diagnostics::Span,
        context: &str,
    ) {
        let zero = self.builder.ins().iconst(types::I64, 0);
        let is_nil = self.builder.ins().icmp(IntCC::Equal, ptr, zero);

        let nil_block = self.builder.create_block();
        let ok_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_nil, nil_block, &[], ok_block, &[]);

        // ── nil branch: report and abort ──────────────────────────────────────
        self.builder.switch_to_block(nil_block);
        self.builder.seal_block(nil_block);

        let source_file = self.source_file.to_string();
        let context_owned = context.to_string();
        let file_ptr = self.emit_string_literal(&source_file);
        let ctx_ptr = self.emit_string_literal(&context_owned);
        let line_val = self.builder.ins().iconst(types::I32, span.line as i64);
        let col_val = self.builder.ins().iconst(types::I32, span.col as i64);

        let nil_deref_id = self.func_id("willow_nil_deref");
        let nil_deref_ref = self
            .module
            .declare_func_in_func(nil_deref_id, self.builder.func);
        self.builder
            .ins()
            .call(nil_deref_ref, &[file_ptr, line_val, col_val, ctx_ptr]);
        self.builder.ins().trap(TrapCode::unwrap_user(1));

        // ── ok branch: continue ───────────────────────────────────────────────
        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
    }

    pub(super) fn emit_field_access(
        &mut self,
        obj: &Expr,
        field_name: &str,
    ) -> cranelift_codegen::ir::Value {
        let ptr = self.emit_expr(obj);

        // Debug build: guard against nil dereference with a source-aware runtime error.
        if self.build_mode == BuildMode::Debug {
            let span = obj.span();
            self.emit_nil_check(ptr, span, field_name);
        }

        let obj_type = self.ast_type_of(obj);
        // Range<i64> bounds: word 0 = start, word 1 = end.
        if matches!(&obj_type, Type::Generic(n, _) if n == "Range") {
            let offset = if field_name == "end" { 8i32 } else { 0i32 };
            return self
                .builder
                .ins()
                .load(types::I64, MemFlags::new(), ptr, offset);
        }
        if let Some(class_name) = class_name_for_object_type(&obj_type)
            && let Some(layout) = self.class_layouts.get(&class_name).cloned()
            && let Some(idx) = layout.iter().position(|(n, _)| n == field_name)
        {
            // Word 0 is type_id; fields start at word 1 → offset = (idx + 1) * 8.
            let offset = (idx as i32 + 1) * 8;
            let (_, field_ty) = &layout[idx];
            let load_ty = clif_type(field_ty);
            return self
                .builder
                .ins()
                .load(load_ty, MemFlags::new(), ptr, offset);
        }
        self.builder.ins().iconst(types::I64, 0)
    }
}
