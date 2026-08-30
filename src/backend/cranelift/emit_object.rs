use cranelift_codegen::ir::{InstBuilder, MemFlagsData, types};
use cranelift_module::Module;

use super::*;

impl<'a, 'b> FuncGen<'a, 'b> {
    /// The address of `class`'s DESCRIPTOR: the value that lives in word 0 of
    /// every object of that class (willow-fm7t).
    ///
    /// The descriptor holds the class's `type_id` at its own offset 0, followed
    /// by one word per virtual method slot. One store of this pointer at
    /// construction is therefore what makes both `is`/downcast and O(1) virtual
    /// dispatch work, without growing the object or moving any field.
    pub(super) fn class_descriptor_addr(&mut self, class: &str) -> cranelift_codegen::ir::Value {
        let data_id = self
            .class_descriptor_ids
            .get(class)
            .copied()
            .unwrap_or_else(|| {
                panic!("compiler invariant violated: checked class `{class}` has no descriptor")
            });
        let gv = self.module.declare_data_in_func(data_id, self.builder.func);
        let ptr_ty = self.module.target_config().pointer_type();
        self.builder.ins().symbol_value(ptr_ty, gv)
    }

    /// Store `class`'s descriptor address into word 0 of a freshly allocated
    /// object (willow-fm7t).
    pub(super) fn emit_store_class_descriptor(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        class: &str,
    ) {
        let descriptor = self.class_descriptor_addr(class);
        self.builder
            .ins()
            .store(MemFlagsData::new(), descriptor, ptr, 0i32);
    }

    /// Load the runtime `type_id` of the object `ptr` points at (willow-fm7t).
    ///
    /// Two dependent loads rather than one: word 0 of the object is the
    /// descriptor address, and offset 0 of the descriptor is the id. Only the
    /// comparatively rare `is`/downcast paths pay for this; virtual dispatch
    /// reads a slot from the same descriptor and never materialises the id.
    pub(super) fn emit_load_runtime_type_id(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let ptr_ty = self.module.target_config().pointer_type();
        let descriptor = self
            .builder
            .ins()
            .load(ptr_ty, MemFlagsData::new(), ptr, 0i32);
        self.builder
            .ins()
            .load(types::I64, MemFlagsData::new(), descriptor, 0i32)
    }
}

impl<'a, 'b> FuncGen<'a, 'b> {
    pub(super) fn emit_field_address(
        &mut self,
        obj: &Expr,
        field_name: &str,
        _span: crate::diagnostics::Span,
    ) -> cranelift_codegen::ir::Value {
        let ptr = self.emit_expr(obj);

        let obj_type = self.ast_type_of(obj);
        if let Some(class_name) = class_name_for_object_type(&obj_type)
            && let Some(layout) = self.class_layouts.get(&class_name)
            && let Some(idx) = layout.iter().position(|(n, _)| n == field_name)
        {
            let offset = (idx as i64 + 1) * 8;
            return self.builder.ins().iadd_imm_s(ptr, offset);
        }
        panic!(
            "compiler invariant violated: checked field `{field_name}` has no object-layout slot"
        )
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
        let address = self.emit_value_runtime_call("willow_array_element_addr", &[arr, index]);
        // The caller owns the array root because the returned element address
        // stays valid only while that handle/buffer is alive. It is therefore
        // intentionally still present on both normal and abnormal paths.
        (address, 1)
    }

    pub(super) fn emit_format_call(&mut self, c: &CallExpr) -> cranelift_codegen::ir::Value {
        let Some(Expr::String(spec, _)) = c.args.first().map(|arg| &arg.expr) else {
            panic!("compiler invariant violated: checked format call has no literal format string");
        };
        let spec = spec.clone();
        self.emit_interpolated_string(&spec, &c.args[1..])
    }

    /// Assemble an interpolated `String` from a validated format spec and its
    /// arguments (willow-csax): literal segments become string literals, `{}`
    /// converts the next argument via its type's `toString` runtime call, and
    /// the f64 precision placeholders use the fixed-format runtime helpers.
    /// Pieces are folded left with `willow_string_concat`; every intermediate
    /// is GC-rooted because each concat can allocate (and collect).
    pub(super) fn emit_interpolated_string(
        &mut self,
        spec: &str,
        args: &[CallArg],
    ) -> cranelift_codegen::ir::Value {
        self.emit_interpolated_with(spec, args.len(), |fg, i| {
            let value = fg.emit_expr(&args[i].expr);
            let ty = fg.ast_type_of(&args[i].expr);
            (value, ty)
        })
    }

    /// The body of [`FuncGen::emit_interpolated_string`], with the arguments
    /// supplied by a callback rather than read from the AST.
    ///
    /// The callback exists so the LIR walker can reach the same emitter with
    /// typed HIR operands (willow-0g8j.2.5): a format string that assembled its
    /// pieces differently on the two paths would produce two different strings
    /// for one program. Arguments stay LAZY — each is emitted only when its
    /// placeholder is reached — because an operand that has not been evaluated
    /// yet cannot be collected, which is what lets the rooting below be exactly
    /// one push per live piece.
    pub(super) fn emit_interpolated_with(
        &mut self,
        spec: &str,
        arg_count: usize,
        mut emit_arg: impl FnMut(&mut Self, usize) -> (cranelift_codegen::ir::Value, Type),
    ) -> cranelift_codegen::ir::Value {
        let segments = match crate::interpolate::parse_spec(spec) {
            Ok(segments) => segments,
            // The checker rejected invalid specs; only synthesized nodes could
            // land here.
            Err(_) => return self.emit_string_literal(spec),
        };
        let mut next_arg = 0usize;
        let mut acc: Option<cranelift_codegen::ir::Value> = None;
        let mut temp_roots = 0usize;
        for segment in &segments {
            // Every step below can allocate (toString / concat), and any
            // allocation can collect — so each live string is rooted the
            // instant it exists, and stays rooted until the final pop.
            let piece = match segment {
                crate::interpolate::Segment::Literal(text) => {
                    // Literals are permanent (runtime-rooted) — no root needed.
                    let text = text.clone();
                    self.emit_string_literal(&text)
                }
                crate::interpolate::Segment::Display => {
                    if next_arg >= arg_count {
                        break;
                    }
                    let (val, ty) = emit_arg(self, next_arg);
                    next_arg += 1;
                    let converted = match ty {
                        Type::String => val,
                        Type::F64 => self.emit_runtime_call1("willow_f64_to_string", val),
                        Type::Bool => self.emit_runtime_call1("willow_bool_to_string", val),
                        _ => self.emit_runtime_call1("willow_i64_to_string", val),
                    };
                    self.emit_push_root(converted);
                    temp_roots += 1;
                    converted
                }
                crate::interpolate::Segment::F64(format) => {
                    if next_arg >= arg_count {
                        break;
                    }
                    let (val, _) = emit_arg(self, next_arg);
                    next_arg += 1;
                    let converted = self.emit_runtime_call1(format.runtime_symbol(), val);
                    self.emit_push_root(converted);
                    temp_roots += 1;
                    converted
                }
            };
            acc = Some(match acc {
                None => piece,
                Some(prev) => {
                    // Both operands are rooted; the result gets rooted too so
                    // it survives the NEXT piece's allocations.
                    let joined =
                        self.emit_value_runtime_call("willow_string_concat", &[prev, piece]);
                    self.emit_push_root(joined);
                    temp_roots += 1;
                    joined
                }
            });
        }
        if temp_roots > 0 {
            self.emit_pop_roots_n(temp_roots);
            self.gc_root_count -= temp_roots;
        }
        acc.unwrap_or_else(|| self.emit_string_literal(""))
    }

    /// Call a one-argument runtime function and return its single result.
    fn emit_runtime_call1(
        &mut self,
        symbol: &str,
        arg: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        self.emit_value_runtime_call(symbol, &[arg])
    }

    pub(super) fn emit_object_literal(
        &mut self,
        o: &ObjectLiteralExpr,
    ) -> cranelift_codegen::ir::Value {
        let layout = self
            .class_layouts
            .get(&o.class)
            .cloned()
            .unwrap_or_else(|| {
                panic!(
                    "compiler invariant violated: checked object literal class `{}` has no layout",
                    o.class
                )
            });
        // Object layout: word 0 = class descriptor pointer, words 1..N = fields.
        let type_id = self
            .class_type_ids
            .get(&o.class)
            .copied()
            .unwrap_or_else(|| {
                panic!(
                    "compiler invariant violated: checked object literal class `{}` has no type id",
                    o.class
                )
            });
        let gc_layout = GcLayoutMetadata::class(&o.class, type_id, &layout, self.enum_infos);
        let ptr = self.emit_gc_alloc(gc_layout);

        // Root ptr immediately: evaluating field initialiser expressions below
        // may trigger allocations and GC cycles before all fields are stored.
        // Without this root, GC could collect the partially-initialised object.
        self.emit_push_root(ptr);

        // Store the class descriptor at offset 0.
        self.emit_store_class_descriptor(ptr, &o.class);

        // Store each field at offset (idx + 1) * 8 to leave word 0 for the descriptor.
        for field in &o.fields {
            if let Some(idx) = layout.iter().position(|(n, _)| n == &field.name) {
                let offset = (idx as i32 + 1) * 8;
                // Box a class value when the field's declared type is an interface.
                let field_ty = layout[idx].1.clone();
                let val = self.emit_expr_coerced(&field.value, &field_ty);
                self.emit_gc_heap_store(
                    ptr,
                    offset,
                    val,
                    &field_ty,
                    GcStoreDestination::ObjectField,
                );
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
        let layout = self
            .class_layouts
            .get(&n.class_name)
            .cloned()
            .unwrap_or_else(|| {
                panic!(
                    "compiler invariant violated: checked class `{}` has no object layout",
                    n.class_name
                )
            });
        // Object layout: word 0 = class descriptor pointer, words 1..N = fields. Allocating
        // with the GC ref-mask leaves reference fields zero/null until assigned,
        // so a collection mid-construction is safe (willow-scq2 §12.3).
        let type_id = self
            .class_type_ids
            .get(&n.class_name)
            .copied()
            .unwrap_or_else(|| {
                panic!(
                    "compiler invariant violated: checked class `{}` has no type id",
                    n.class_name
                )
            });
        let gc_layout = GcLayoutMetadata::class(&n.class_name, type_id, &layout, self.enum_infos);
        let ptr = self.emit_gc_alloc(gc_layout);
        self.emit_store_class_descriptor(ptr, &n.class_name);

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
            // A constructor parameter may be `&`/`&mut`, and `init` has no
            // other way to be called, so the modes have to reach the argument
            // emitter or a `&place` argument is passed as a VALUE that the
            // callee then dereferences as a pointer (willow-0g8j.10). The modes
            // recorded for a method exclude the `self` receiver, so they line up
            // with `n.args` directly.
            let modes = self.func_param_modes.get(&mangled).cloned();
            let param_debug = self.func_param_debug.get(&mangled).cloned();
            let has_reference_args = has_reference_args(modes.as_deref(), &n.args);
            let (arg_vals, arg_roots) = self.emit_call_args_rooted_coerced(
                Some(&mangled),
                modes.as_deref(),
                param_debug.as_deref(),
                Some(&param_types),
                &n.args,
            );
            let init_ref = self
                .module
                .declare_func_in_func(init_fid, self.builder.func);
            let mut call_args = vec![ptr];
            call_args.extend(arg_vals);
            let pushed = self.emit_callstack_push("init", n.span);
            let panic_depth = self.emit_pre_user_call_panic_depth(&mangled);
            self.builder.ins().call(init_ref, &call_args);
            if pushed {
                self.emit_callstack_pop();
            }
            // The reference-call record names the `&place` this call passed;
            // leaving it set would attribute a LATER panic in this function to a
            // constructor call that already returned (willow-0g8j.11).
            if has_reference_args {
                self.emit_debug_reference_call_clear();
            }
            if arg_roots > 0 {
                self.emit_pop_roots_n(arg_roots);
                self.gc_root_count -= arg_roots;
            }
            self.emit_post_willow_call_panic_check(panic_depth);
        } else {
            // Implicit memberwise constructor: store each arg positionally into
            // its field slot (declaration order).
            for (i, arg) in n.args.iter().enumerate() {
                if let Some((_, field_ty)) = layout.get(i) {
                    let field_ty = field_ty.clone();
                    let val = self.emit_expr_coerced(&arg.expr, &field_ty);
                    let offset = (i as i32 + 1) * 8;
                    self.emit_gc_heap_store(
                        ptr,
                        offset,
                        val,
                        &field_ty,
                        GcStoreDestination::ObjectField,
                    );
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
            // Same as `emit_new`: a base constructor's reference parameters
            // have to reach the argument emitter (willow-0g8j.10).
            let modes = self.func_param_modes.get(&mangled).cloned();
            let param_debug = self.func_param_debug.get(&mangled).cloned();
            let has_reference_args = has_reference_args(modes.as_deref(), &s.args);
            let (arg_vals, arg_roots) = self.emit_call_args_rooted_coerced(
                Some(&mangled),
                modes.as_deref(),
                param_debug.as_deref(),
                Some(&param_types),
                &s.args,
            );
            let init_ref = self
                .module
                .declare_func_in_func(init_fid, self.builder.func);
            let mut call_args = vec![self_ptr];
            call_args.extend(arg_vals);
            let pushed = self.emit_callstack_push("init", s.span);
            let panic_depth = self.emit_pre_user_call_panic_depth(&mangled);
            self.builder.ins().call(init_ref, &call_args);
            if pushed {
                self.emit_callstack_pop();
            }
            if has_reference_args {
                self.emit_debug_reference_call_clear();
            }
            if arg_roots > 0 {
                self.emit_pop_roots_n(arg_roots);
                self.gc_root_count -= arg_roots;
            }
            self.emit_post_willow_call_panic_check(panic_depth);
            return;
        }

        if let Some(layout) = self.class_layouts.get(&base_name).cloned() {
            for (i, arg) in s.args.iter().enumerate() {
                if let Some((_, field_ty)) = layout.get(i) {
                    let field_ty = field_ty.clone();
                    let val = self.emit_expr_coerced(&arg.expr, &field_ty);
                    let offset = (i as i32 + 1) * 8;
                    self.emit_gc_heap_store(
                        self_ptr,
                        offset,
                        val,
                        &field_ty,
                        GcStoreDestination::ObjectField,
                    );
                }
            }
        } else {
            for arg in &s.args {
                self.emit_expr(&arg.expr);
            }
        }
    }

    pub(super) fn emit_field_access(
        &mut self,
        obj: &Expr,
        field_name: &str,
    ) -> cranelift_codegen::ir::Value {
        let ptr = self.emit_expr(obj);

        let obj_type = self.ast_type_of(obj);
        // Range<i64> bounds: word 0 = start, word 1 = end.
        if matches!(&obj_type, Type::Generic(n, _) if n == "Range") {
            let offset = if field_name == "end" { 8i32 } else { 0i32 };
            return self
                .builder
                .ins()
                .load(types::I64, MemFlagsData::new(), ptr, offset);
        }
        if let Some(class_name) = class_name_for_object_type(&obj_type)
            && let Some(layout) = self.class_layouts.get(&class_name).cloned()
            && let Some(idx) = layout.iter().position(|(n, _)| n == field_name)
        {
            // Word 0 is the descriptor; fields start at word 1 → offset = (idx + 1) * 8.
            let offset = (idx as i32 + 1) * 8;
            let (_, field_ty) = &layout[idx];
            let load_ty = clif_type(field_ty);
            return self
                .builder
                .ins()
                .load(load_ty, MemFlagsData::new(), ptr, offset);
        }
        panic!(
            "compiler invariant violated: checked field `{field_name}` has no loadable slot on `{obj_type:?}`"
        )
    }
}
