//! Operand and rvalue emission.

use super::*;

impl<'a, 'b> FuncGen<'a, 'b> {
    pub(super) fn emit_lir_operand_store(
        &mut self,
        function: &LirFunction,
        value: &crate::ir::lowered::LirOperand,
        target: &Type,
    ) -> cranelift_codegen::ir::Value {
        let source = value.ty(&function.locals).expect("validated value operand");
        let result = self.emit_lir_operand(function, value);
        self.coerce_to_target(result, &source, target)
    }

    pub(super) fn emit_flat_pattern_check(
        &mut self,
        scrutinee: cranelift_codegen::ir::Value,
        scrutinee_ty: &Type,
        pattern: &crate::ir::lowered::LirPattern,
    ) -> cranelift_codegen::ir::Value {
        match pattern {
            crate::ir::lowered::LirPattern::Wildcard
            | crate::ir::lowered::LirPattern::Binding { .. } => {
                self.builder.ins().iconst(types::I8, 1)
            }
            crate::ir::lowered::LirPattern::LiteralBool(b) => {
                let expected = self.builder.ins().iconst(types::I8, i64::from(*b));
                self.builder.ins().icmp(IntCC::Equal, scrutinee, expected)
            }
            crate::ir::lowered::LirPattern::LiteralInt(n) => {
                let expected = self.builder.ins().iconst(types::I64, *n);
                self.builder.ins().icmp(IntCC::Equal, scrutinee, expected)
            }
            crate::ir::lowered::LirPattern::EnumVariant { enum_name, variant }
            | crate::ir::lowered::LirPattern::EnumVariantTuple {
                enum_name, variant, ..
            } => {
                let tag = self.enum_variant_tag(&enum_name.to_string(), variant);
                // The `Option` pointer niche carries no tag word: `Some` is any
                // non-null payload and `None` is null.
                if builtin_types::is(scrutinee_ty, B::Option)
                    && self.option_repr(scrutinee_ty) == Some(OptionRepr::NullableGcPointer)
                {
                    let cc = if tag == 0 {
                        IntCC::NotEqual
                    } else {
                        IntCC::Equal
                    };
                    return self.builder.ins().icmp_imm_u(cc, scrutinee, 0);
                }
                let expected = self.builder.ins().iconst(types::I64, tag);
                // A payload-carrying enum is a heap object whose word 0 is the
                // tag; a fieldless one IS the tag.
                let actual = if self.enum_is_gc_object_type(&enum_name.to_string()) {
                    self.emit_load_enum_tag(scrutinee)
                } else {
                    scrutinee
                };
                self.builder.ins().icmp(IntCC::Equal, actual, expected)
            }
            // The scrutinee is an interface box `{object@0, vtable@8}`. Match
            // when the boxed object's runtime `type_id` — read through the
            // class descriptor its word 0 points at — equals the pattern
            // class's. Exactly [`FuncGen::emit_pattern_check`]'s
            // `Pattern::ClassDowncast`, including its exactness: a subclass
            // instance does NOT match its base's arm on either backend.
            crate::ir::lowered::LirPattern::ClassDowncast { class_name, .. } => {
                let type_id = self.classes.type_id(class_name).unwrap_or_else(|| {
                    panic!(
                        "compiler invariant violated: checked downcast pattern class `{class_name}` has no type id"
                    )
                });
                let (obj, _) = self.builder.ins().isplit(scrutinee);
                let actual = self.emit_load_runtime_type_id(obj);
                let expected = self.builder.ins().iconst(types::I64, type_id);
                self.builder.ins().icmp(IntCC::Equal, actual, expected)
            }
        }
    }

    pub(super) fn flat_pattern_binding_values(
        &mut self,
        scrutinee: cranelift_codegen::ir::Value,
        scrutinee_ty: &Type,
        pattern: &crate::ir::lowered::LirPattern,
    ) -> Vec<(String, Type, cranelift_codegen::ir::Value)> {
        match pattern {
            crate::ir::lowered::LirPattern::Binding { name, ty } => {
                vec![(name.clone(), ty.clone(), scrutinee)]
            }
            crate::ir::lowered::LirPattern::EnumVariantTuple {
                enum_name,
                variant,
                bindings,
            } => {
                // Read the DECLARED payload types rather than the pattern's
                // recorded ones, so the load width follows the layout the
                // constructor wrote.
                let mut payload_types = self.resolve_variant_payload_types(
                    &enum_name.to_string(),
                    variant,
                    scrutinee_ty,
                );
                normalize_void_payloads(&mut payload_types);
                let niche = builtin_types::is(scrutinee_ty, B::Option)
                    && self.option_repr(scrutinee_ty) == Some(OptionRepr::NullableGcPointer);
                let mut out = Vec::with_capacity(payload_types.len());
                let mut payload_word = 1;
                for (i, ((name, _), payload_ty)) in
                    bindings.iter().zip(payload_types.iter()).enumerate()
                {
                    let clif_ty = self.clif_type(payload_ty);
                    // In the niche the scrutinee IS the payload — there is no
                    // heap object to load word 1 from.
                    let raw = if niche && i == 0 {
                        scrutinee
                    } else {
                        if is_scalar_pair(scrutinee_ty) {
                            self.emit_enum_payload_word(scrutinee)
                        } else {
                            let offset = payload_word * 8;
                            self.builder.ins().load(
                                if clif_ty == types::I128 {
                                    types::I128
                                } else {
                                    types::I64
                                },
                                MemFlagsData::new(),
                                scrutinee,
                                offset,
                            )
                        }
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
                    payload_word += 1 + i32::from(self.is_inline_pair(payload_ty));
                    out.push((name.clone(), payload_ty.clone(), val));
                }
                out
            }
            // The arm only runs when the check above proved the box holds this
            // class, so the binding is the box's object word, unboxed. It stays
            // reachable through the rooted scrutinee for the whole arm.
            crate::ir::lowered::LirPattern::ClassDowncast {
                binding,
                binding_ty,
                ..
            } => {
                let (obj, _) = self.builder.ins().isplit(scrutinee);
                vec![(binding.clone(), binding_ty.clone(), obj)]
            }
            crate::ir::lowered::LirPattern::Wildcard
            | crate::ir::lowered::LirPattern::LiteralBool(_)
            | crate::ir::lowered::LirPattern::LiteralInt(_)
            | crate::ir::lowered::LirPattern::EnumVariant { .. } => Vec::new(),
        }
    }

    pub(in crate::backend::cranelift) fn emit_lir_operand(
        &mut self,
        function: &LirFunction,
        operand: &crate::ir::lowered::LirOperand,
    ) -> cranelift_codegen::ir::Value {
        use crate::ir::lowered::LirOperand;
        match operand {
            LirOperand::Local(local) => self.load_lir_local(function, *local),
            LirOperand::Int(value) => self.builder.ins().iconst(types::I64, *value),
            LirOperand::Float(value) => self.builder.ins().f64const(*value),
            LirOperand::Bool(value) => self.builder.ins().iconst(types::I8, i64::from(*value)),
            LirOperand::Reference { .. } => {
                unreachable!("reference operands are emitted as call cells")
            }
        }
    }

    /// One `willow_string_concat_n` call over a stack array of `[kind, value]`
    /// part words. The operands are read and stored after the
    /// call's panic-depth snapshot, so no GC point separates the stores from
    /// the call; the runtime roots the string slots itself across its
    /// allocation, and the caller never reads the array afterwards.
    fn emit_string_concat_n(
        &mut self,
        function: &LirFunction,
        parts: &[crate::ir::lowered::LirOperand],
        part_types: &[Type],
    ) -> cranelift_codegen::ir::Value {
        use willow_abi::string_concat::{PART_I64, PART_STRING};
        let bytes = u32::try_from(parts.len() * 16).expect("string concatenation part count");
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            bytes,
            3,
        ));
        let ptr_ty = reference_type(self.module.target_config());
        self.emit_runtime_call_late_args(
            "willow_string_concat_n",
            |this| {
                for (index, (part, ty)) in parts.iter().zip(part_types).enumerate() {
                    let kind = if *ty == Type::I64 {
                        PART_I64
                    } else {
                        PART_STRING
                    };
                    let offset = (index * 16) as i32;
                    let kind = this.builder.ins().iconst(types::I64, kind as i64);
                    this.builder.ins().stack_store(ptr_ty, kind, slot, offset);
                    let value = this.emit_lir_operand(function, part);
                    this.builder
                        .ins()
                        .stack_store(ptr_ty, value, slot, offset + 8);
                }
                let array = this.builder.ins().stack_addr(ptr_ty, slot, 0);
                let count = this.builder.ins().iconst(types::I64, parts.len() as i64);
                vec![array, count]
            },
            |_| {},
        )
        .expect("willow_string_concat_n returns a string")
    }

    pub(super) fn emit_lir_rvalue(
        &mut self,
        function: &LirFunction,
        value: &crate::ir::lowered::LirRvalue,
        span: Span,
    ) -> cranelift_codegen::ir::Value {
        use crate::ir::lowered::LirRvalue;
        match value {
            LirRvalue::BeginReferenceCall => {
                self.emit_debug_reference_call_scope_push();
                self.diagnostics.lir_reference_scopes.push(Vec::new());
                self.builder.ins().iconst(types::I8, 0)
            }
            LirRvalue::ReferenceDebug {
                argument,
                callee,
                index,
            } => {
                self.emit_flat_reference_debug(argument, callee, *index);
                self.diagnostics
                    .lir_reference_scopes
                    .last_mut()
                    .expect("prepared reference scope")
                    .push(FlatReferenceDebug {
                        argument: argument.clone(),
                        callee: *callee,
                        index: *index,
                    });
                self.builder.ins().iconst(types::I8, 0)
            }
            LirRvalue::StartTask { callee, args, .. } => {
                let has_references = args
                    .iter()
                    .any(|arg| matches!(arg, crate::ir::lowered::LirOperand::Reference { .. }));
                let (args, roots) = self.emit_flat_call_operands(function, args);
                let result = self.emit_flat_start_task(*callee, &args, span);
                self.emit_pop_roots_n(roots);
                self.roots.gc_root_count -= roots;
                if has_references {
                    self.emit_flat_reference_call_end();
                }
                result
            }
            LirRvalue::AwaitFuture { future, result } => {
                let future = self.emit_lir_operand(function, future);
                self.emit_flat_await_future(future, result)
            }
            LirRvalue::SelectIdleWait { deadlines } => {
                let deadlines: Vec<_> = deadlines
                    .iter()
                    .map(|deadline| self.emit_lir_operand(function, deadline))
                    .collect();
                self.emit_flat_select_idle_wait(&deadlines)
            }
            LirRvalue::PrepareMethod {
                receiver,
                receiver_ty,
                method,
            } => {
                let receiver = self.emit_lir_operand(function, receiver);
                let receiver =
                    self.emit_flat_prepare_method(receiver, receiver_ty, method, span, true);
                if self.flat_method_frame_enabled(method) {
                    self.diagnostics
                        .lir_call_frames
                        .push((method.clone(), span));
                }
                receiver
            }
            LirRvalue::MethodCall {
                receiver,
                receiver_ty,
                method,
                args,
                result,
                super_class,
                ..
            } => {
                if self.flat_method_frame_enabled(method) {
                    let (prepared, _) = self
                        .diagnostics
                        .lir_call_frames
                        .pop()
                        .expect("prepared method frame");
                    assert_eq!(prepared, *method);
                }
                // PrepareMethod snapshots the receiver before evaluating arguments.
                // A GC stack local is a direct root until after this instruction,
                // including when the original binding is reassigned by the call.
                // Heap-frame fields and borrowed storage do not pin an SSA alias.
                let receiver_rooted = match receiver {
                    crate::ir::lowered::LirOperand::Local(id) => matches!(
                        self.locals.vars.get(&function.locals[id.0 as usize].name),
                        Some(VarStorage::Stack { ty, .. })
                            if is_gc_managed(ty, self.enum_infos)
                    ),
                    _ => false,
                };
                let receiver = self.emit_lir_operand(function, receiver);
                let (args, roots) = self.emit_flat_call_operands(function, args);
                let result = if matches!(receiver_ty, Type::Named(name) | Type::Generic(name, _) if self.classes.is_interface(name))
                {
                    self.emit_flat_interface_call(
                        (receiver, receiver_ty),
                        method,
                        &args,
                        span,
                        true,
                        true,
                    )
                } else {
                    self.emit_flat_class_method(
                        receiver,
                        receiver_ty,
                        super::super::flat_calls::FlatClassCall {
                            super_class: *super_class,
                            method,
                            args: &args,
                            ret_ty: result,
                            span,
                        },
                        receiver_rooted,
                        true,
                    )
                };
                self.emit_pop_roots_n(roots);
                self.roots.gc_root_count -= roots;
                result
            }
            LirRvalue::Coerce {
                value,
                source,
                target,
            } => {
                let value = self.emit_lir_operand(function, value);
                self.coerce_to_target(value, source, target)
            }
            LirRvalue::ArrayAlloc { length, element } => {
                self.emit_flat_array_alloc(*length, element)
            }
            LirRvalue::CaptureArrayOwner { array, index } => {
                self.emit_flat_capture_array_owner(function, array, index)
            }
            LirRvalue::ArrayStore {
                array,
                index,
                value,
                element,
            } => self.emit_flat_array_store(function, array, index, value, element),
            LirRvalue::Index {
                array,
                index,
                element,
            } => self.emit_flat_index(function, array, index, element),
            LirRvalue::ObjectAlloc { class } => self.emit_flat_object_alloc(class),
            LirRvalue::FieldLoad {
                object,
                object_ty,
                field,
                ..
            } => self.emit_flat_field(function, object, field, object_ty),
            LirRvalue::FieldStore {
                object,
                object_ty,
                field,
                value,
            } => self.emit_flat_field_store(function, object, field, value, object_ty),
            LirRvalue::StaticField { class, field, .. } => {
                self.emit_flat_static_field(&class.to_string(), field)
            }
            LirRvalue::StaticStore {
                class,
                field,
                value,
            } => self.emit_flat_static_store(function, &class.to_string(), field, value),
            LirRvalue::StaticCall {
                class,
                method,
                args,
                arg_types,
                result,
                ..
            } => {
                // The relocatable operand roots stay valid: on every static
                // call path the operands reach the callee without an
                // intervening GC point, except heap enum construction, which
                // roots and reloads its payloads around the allocation.
                let (args, roots) = self.emit_flat_call_operands(function, args);
                let result = self.emit_lir_static_call_values(
                    &class.to_string(),
                    method,
                    &args,
                    arg_types,
                    result,
                    span,
                );
                if !self.locals.terminated {
                    self.emit_pop_roots_n(roots);
                }
                self.roots.gc_root_count -= roots;
                result
            }
            LirRvalue::EnumAlloc {
                class,
                variant,
                enum_ty,
            } => self.emit_flat_enum_alloc(&class.to_string(), variant, enum_ty),
            LirRvalue::EnumPayloadStore {
                object,
                class,
                variant,
                index,
                value,
                source,
                enum_ty,
            } => {
                let object = self.emit_lir_operand(function, object);
                let value = self.emit_lir_operand(function, value);
                self.emit_flat_enum_payload_store(
                    object,
                    &class.to_string(),
                    variant,
                    *index,
                    (value, source),
                    enum_ty,
                )
            }
            LirRvalue::ConstructorCall {
                object,
                class,
                args,
                arg_types,
            } => self.emit_flat_constructor_call(function, object, class, args, arg_types, span),
            LirRvalue::Range { start, end } => {
                let start = self.emit_lir_operand(function, start);
                let end = self.emit_lir_operand(function, end);
                let ptr = self.emit_gc_alloc(GcLayoutMetadata::new(GcObjectKind::Range, 16, 0, 0));
                self.builder.ins().store(MemFlagsData::new(), start, ptr, 0);
                self.builder.ins().store(MemFlagsData::new(), end, ptr, 8);
                ptr
            }
            LirRvalue::EnumMethod {
                receiver,
                receiver_ty,
                method,
                args,
                arg_types,
                ..
            } => {
                let receiver = self.emit_lir_operand(function, receiver);
                let args: Vec<_> = args
                    .iter()
                    .map(|arg| self.emit_lir_operand(function, arg))
                    .collect();
                self.emit_flat_enum_method(receiver, receiver_ty, method, &args, arg_types, span)
            }
            LirRvalue::BuiltinCall { callee, args, .. } => {
                let args: Vec<_> = args
                    .iter()
                    .map(|arg| self.emit_lir_operand(function, arg))
                    .collect();
                if callee.is_free_named("$contract_i64") || callee.is_free_named("$contract_check")
                {
                    let runtime = if callee.is_free_named("$contract_i64") {
                        "willow_contract_i64"
                    } else {
                        "willow_contract_check"
                    };
                    let mut args = args;
                    if callee.is_free_named("$contract_check") {
                        args[0] = self.builder.ins().uextend(types::I64, args[0]);
                    }
                    let file = self.emit_string_literal(self.diagnostics.source_file);
                    let line = self.builder.ins().iconst(types::I32, span.line as i64);
                    let col = self.builder.ins().iconst(types::I32, span.col as i64);
                    args.extend([file, line, col]);
                    self.emit_runtime_call_with_cleanup(runtime, &args, |_| {})
                        .unwrap_or_else(|| self.builder.ins().iconst(types::I8, 0))
                } else {
                    self.emit_flat_builtin_call(callee, &args)
                }
            }
            LirRvalue::FormatScalar { value, ty, format } => {
                let value = self.emit_lir_operand(function, value);
                self.emit_flat_format_scalar(value, ty, *format)
            }
            LirRvalue::Panic { message } => {
                let message = self.emit_lir_operand(function, message);
                self.emit_panic_with_message(message, span)
            }
            LirRvalue::Recover => self.emit_recover_call(),
            LirRvalue::RebindResultError {
                value,
                source,
                target,
            } => {
                let value = self.emit_lir_operand(function, value);
                if is_scalar_pair(source) == is_scalar_pair(target) {
                    value
                } else {
                    let (_, err_ty) = builtin_types::binary_args(source, B::Result)
                        .expect("checked Result propagation");
                    let bits = self.emit_enum_payload_bits(value, err_ty);
                    let payload = self.coerce_i64_to(bits, err_ty);
                    self.emit_alloc_result_variant(target, 1, err_ty, payload)
                }
            }
            LirRvalue::IntoError { value, source, .. } => {
                let value = self.emit_lir_operand(function, value);
                self.emit_push_call_root(value);
                let panic_depth = self.emit_pre_willow_call_panic_depth();
                let Type::Named(class) = source else {
                    unreachable!("Into error source class validated");
                };
                let converted = self.emit_into_conversion(value, &class.to_string());
                self.emit_pop_roots_n(1);
                self.roots.gc_root_count -= 1;
                self.emit_post_willow_call_panic_check(panic_depth);
                converted
            }
            LirRvalue::IntrinsicCall {
                intrinsic,
                receiver,
                receiver_ty,
                args,
                arg_types,
                result,
                ..
            } => {
                let receiver = self.emit_lir_operand(function, receiver);
                let args: Vec<_> = args
                    .iter()
                    .map(|arg| self.emit_lir_operand(function, arg))
                    .collect();
                self.emit_flat_intrinsic(
                    *intrinsic,
                    (receiver, receiver_ty),
                    &args,
                    arg_types,
                    result,
                    span,
                )
            }
            LirRvalue::Use(operand) => self.emit_lir_operand(function, operand),
            LirRvalue::StringLiteral(value) => self.emit_string_literal(value),
            LirRvalue::FunctionRef { function, .. } => {
                let fid = *self
                    .func_ids
                    .get_id(function)
                    .expect("registered function reference");
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder
                    .ins()
                    .func_addr(reference_type(self.module.target_config()), fref)
            }
            LirRvalue::Closure { id, captures, ty } => {
                let name = self.lambda_names[id];
                let fid = *self.func_ids.get_id(&name).expect("registered lambda");
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                let code = self
                    .builder
                    .ins()
                    .func_addr(reference_type(self.module.target_config()), fref);
                if !matches!(ty, Type::Closure(..)) {
                    return code;
                }
                let code = if self.lambda_may_panic(id, &name) {
                    code
                } else {
                    self.builder.ins().bor_imm_s(code, CLOSURE_CODE_NO_PANIC)
                };
                let slots: Vec<_> = captures
                    .iter()
                    .map(|capture| {
                        let ty = capture.ty(&function.locals).expect("capture type");
                        self.value_slot_kind(&ty)
                    })
                    .collect();
                let env = self.emit_gc_alloc(GcLayoutMetadata::aggregate(
                    GcObjectKind::Closure,
                    1,
                    &slots,
                    reference_type(self.module.target_config()).bytes(),
                ));
                self.emit_gc_heap_store_classified(
                    env,
                    0,
                    code,
                    false,
                    GcStoreDestination::ObjectField,
                );
                let mut capture_word = 1;
                for capture in captures {
                    let value = self.emit_lir_operand(function, capture);
                    let ty = capture.ty(&function.locals).expect("capture type");
                    self.emit_gc_heap_store(
                        env,
                        capture_word
                            * willow_abi::storage_word_bytes(
                                reference_type(self.module.target_config()).bytes(),
                            ) as i32,
                        value,
                        &ty,
                        GcStoreDestination::ObjectField,
                    );
                    capture_word += 1 + i32::from(self.is_inline_pair(&ty));
                }
                env
            }
            LirRvalue::Print { value, ty, newline } => {
                let value = self.emit_lir_operand(function, value);
                let name = match (ty, newline) {
                    (Type::I64, false) => "willow_print_i64",
                    (Type::I64, true) => "willow_println_i64",
                    (Type::F64, false) => "willow_print_f64",
                    (Type::F64, true) => "willow_println_f64",
                    (Type::Bool, false) => "willow_print_bool",
                    (Type::Bool, true) => "willow_println_bool",
                    (Type::String, false) => "willow_print_string",
                    (Type::String, true) => "willow_println_string",
                    _ => unreachable!("print type validated"),
                };
                self.emit_void_runtime_call(name, &[value]);
                self.builder.ins().iconst(types::I8, 0)
            }
            LirRvalue::DirectCall { callee, args, .. } => {
                let has_references = args
                    .iter()
                    .any(|arg| matches!(arg, crate::ir::lowered::LirOperand::Reference { .. }));
                let (values, roots) = self.emit_flat_call_operands(function, args);
                let fid = *self
                    .func_ids
                    .get_id(callee)
                    .expect("flat direct call is registered");
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                let pushed = self.emit_callstack_push(&callee.to_string(), span);
                let panic_depth = self.emit_pre_user_call_panic_depth(&callee.to_string());
                let call = self.builder.ins().call(fref, &values);
                let result = self
                    .builder
                    .inst_results(call)
                    .first()
                    .copied()
                    .unwrap_or_else(|| self.builder.ins().iconst(types::I8, 0));
                if pushed {
                    self.emit_callstack_pop();
                }
                if has_references {
                    self.emit_flat_reference_call_end();
                }
                self.emit_pop_roots_n(roots);
                self.roots.gc_root_count -= roots;
                self.emit_post_willow_call_panic_check(panic_depth);
                result
            }
            LirRvalue::Unary { op, operand, ty } => {
                let checked = self.overflow_checks()
                    && crate::ir::lowered::value::is_overflow_checked_unary(op, operand, ty);
                let operand = self.emit_lir_operand(function, operand);
                match op {
                    UnaryOp::Neg if *ty == Type::F64 => self.builder.ins().fneg(operand),
                    UnaryOp::Neg if checked => {
                        let (negated, overflowed) = self.emit_overflowing_neg(operand);
                        self.emit_int_overflow_guard(
                            overflowed,
                            crate::backend::cranelift::emit_expr::INT_OVERFLOW_NEG,
                            span,
                        );
                        negated
                    }
                    UnaryOp::Neg => self.builder.ins().ineg(operand),
                    UnaryOp::Not if *ty == Type::I64 => self.builder.ins().bnot(operand),
                    UnaryOp::Not => {
                        let one = self.builder.ins().iconst(types::I8, 1);
                        self.builder.ins().bxor(operand, one)
                    }
                }
            }
            LirRvalue::IndirectCall {
                callee,
                name,
                args,
                params,
                result,
            } => {
                let closure = matches!(callee.ty(&function.locals), Some(Type::Closure(..)));
                // Every addressable target is a compiled synchronous Willow
                // body, which roots its own GC parameters (the closure
                // environment included) before its first safepoint, or a
                // scalar builtin. Operands evaluate without reaching a GC
                // point, so the call site keeps no roots of its own
                //, as interface dispatch does for targets
                // that root their receiver.
                let target = self.emit_lir_operand(function, callee);
                let mut values = Vec::with_capacity(args.len() + usize::from(closure));
                if closure {
                    values.push(target);
                }
                for argument in args {
                    values.push(self.emit_lir_operand(function, argument));
                }
                let mut signature = self.module.make_signature();
                if closure {
                    signature
                        .params
                        .push(AbiParam::new(reference_type(self.module.target_config())));
                }
                signature
                    .params
                    .extend(params.iter().map(|ty| AbiParam::new(self.clif_type(ty))));
                if *result != Type::Void {
                    signature
                        .returns
                        .push(AbiParam::new(self.clif_type(result)));
                }
                let signature = self.builder.import_signature(signature);
                let name = name.to_string();
                if !closure {
                    return self.emit_promoted_fn_call(
                        signature,
                        target,
                        &values,
                        (params, result),
                        &name,
                        span,
                    );
                }
                // A closure whose body cannot raise tags its code word
                // (see `LirRvalue::Closure`); only an untagged one needs the
                // panic-depth protocol around the call.
                let word = self.builder.ins().load(
                    reference_type(self.module.target_config()),
                    MemFlagsData::trusted(),
                    target,
                    0,
                );
                let code = self.builder.ins().band_imm_s(word, !CLOSURE_CODE_NO_PANIC);
                let unchecked = self.builder.ins().band_imm_s(word, CLOSURE_CODE_NO_PANIC);
                self.emit_lir_indirect_call(signature, code, &values, &name, span, Some(unchecked))
            }
            LirRvalue::StringConcat { parts, part_types } => {
                self.emit_string_concat_n(function, parts, part_types)
            }
            LirRvalue::Binary {
                op,
                lhs,
                rhs,
                operand_ty,
            } => {
                // A literal divisor other than 0 and -1 can trip neither the
                // zero nor the overflow guard.
                let unguarded_divisor = matches!(rhs, crate::ir::lowered::LirOperand::Int(divisor) if *divisor != 0 && *divisor != -1);
                let nonnegative_lhs =
                    *operand_ty == Type::I64 && self.reuse.lir_nonnegative.contains(lhs);
                let unsigned_rem = if nonnegative_lhs && matches!(op, BinOp::Rem) {
                    match rhs {
                        crate::ir::lowered::LirOperand::Int(n) if *n > 0 => Some(*n as u64),
                        _ => None,
                    }
                } else {
                    None
                };
                let checked_shift = self.overflow_checks()
                    && crate::ir::lowered::value::is_overflow_checked_binary(op, rhs, operand_ty)
                    && matches!(op, BinOp::Shl | BinOp::Shr);
                let lhs = self.emit_lir_operand(function, lhs);
                let rhs = self.emit_lir_operand(function, rhs);
                if *operand_ty == Type::String {
                    let name = if matches!(op, BinOp::Add) {
                        "willow_string_concat"
                    } else if matches!(op, BinOp::Eq | BinOp::Ne) {
                        "willow_string_eq"
                    } else {
                        "willow_string_cmp"
                    };
                    self.emit_push_call_root(lhs);
                    self.emit_push_call_root(rhs);
                    let raw = self
                        .emit_runtime_call_with_cleanup(name, &[lhs, rhs], |this| {
                            this.emit_pop_roots_n(2);
                            this.roots.gc_root_count -= 2;
                        })
                        .expect("string runtime operation returns a value");
                    return match op {
                        BinOp::Add => raw,
                        BinOp::Eq => self.builder.ins().ireduce(types::I8, raw),
                        BinOp::Ne => {
                            let raw = self.builder.ins().bxor_imm_s(raw, 1);
                            self.builder.ins().ireduce(types::I8, raw)
                        }
                        BinOp::Lt => self.builder.ins().icmp_imm_s(IntCC::SignedLessThan, raw, 0),
                        BinOp::Le => {
                            self.builder
                                .ins()
                                .icmp_imm_s(IntCC::SignedLessThanOrEqual, raw, 0)
                        }
                        BinOp::Gt => {
                            self.builder
                                .ins()
                                .icmp_imm_s(IntCC::SignedGreaterThan, raw, 0)
                        }
                        BinOp::Ge => {
                            self.builder
                                .ins()
                                .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, raw, 0)
                        }
                        _ => unreachable!("string operator validated"),
                    };
                }
                // An interface value is an (object, vtable) pair; identity is
                // the object word alone.
                let (lhs, rhs) = if self.builder.func.dfg.value_type(lhs) == types::I128 {
                    let (lhs, _) = self.builder.ins().isplit(lhs);
                    let (rhs, _) = self.builder.ins().isplit(rhs);
                    (lhs, rhs)
                } else {
                    (lhs, rhs)
                };
                let float = *operand_ty == Type::F64;
                if !float && !unguarded_divisor && matches!(op, BinOp::Div | BinOp::Rem) {
                    self.emit_int_div_guard(lhs, rhs, matches!(op, BinOp::Rem), span);
                }
                if let Some(divisor) = unsigned_rem {
                    if divisor.is_power_of_two() {
                        self.builder.ins().band_imm_s(lhs, (divisor - 1) as i64)
                    } else {
                        let (multiplier, shift) =
                            crate::backend::cranelift::nonnegative::reciprocal(divisor);
                        let multiplier = self.builder.ins().iconst(types::I64, multiplier as i64);
                        let high = self.builder.ins().umulhi(lhs, multiplier);
                        let quotient = self.builder.ins().ushr_imm_s(high, i64::from(shift));
                        let product = self.builder.ins().imul_imm_s(quotient, divisor as i64);
                        self.builder.ins().isub(lhs, product)
                    }
                } else if nonnegative_lhs && matches!(op, BinOp::Lt) {
                    let test = self.builder.ins().icmp(IntCC::SignedLessThan, lhs, rhs);
                    if let Some(reuse) = &mut self.reuse.lir_reuse {
                        reuse.comparison = Some((test, lhs, rhs));
                    }
                    test
                } else if matches!(op, BinOp::Pow) {
                    if float {
                        self.emit_pow_f64(lhs, rhs)
                    } else {
                        self.emit_pow_i64(lhs, rhs, span)
                    }
                } else if !float
                    && self.overflow_checks()
                    && matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul)
                {
                    self.emit_checked_int_binop(op, lhs, rhs, span)
                } else if checked_shift {
                    self.emit_checked_shift(op, lhs, rhs, span)
                } else {
                    self.emit_lir_binop(op, lhs, rhs, float)
                }
            }
        }
    }

    /// The byte layout of the class `ty` names on this target: one shared
    /// result per class, so a field access indexes it instead of copying the
    /// field list.
    pub(in crate::backend::cranelift) fn lir_class_layout(
        &self,
        ty: &Type,
    ) -> std::sync::Arc<crate::compiler_db::layout::ObjectLayout> {
        let class =
            class_name_for_object_type(ty).expect("class receiver type vetted by LIR eligibility");
        // Resolved exactly as eligibility resolved it, so a bare module class
        // name reaches the layout the module registered.
        let key = resolve_class_key(
            &self.classes,
            self.known_modules,
            self.visible_modules,
            &class,
        )
        .expect("class layout vetted by LIR eligibility");
        self.classes
            .object_layout(&key, reference_type(self.module.target_config()).bytes())
            .expect("class layout vetted by LIR eligibility")
    }
    pub(super) fn map_is_ref_flag(&mut self, ty: &Type) -> cranelift_codegen::ir::Value {
        let flag = i64::from(
            crate::backend::cranelift::type_helpers::storage_is_gc_managed(ty, self.enum_infos),
        );
        self.builder.ins().iconst(types::I64, flag)
    }

    pub(in crate::backend::cranelift) fn emit_lir_binop(
        &mut self,
        op: &BinOp,
        l: cranelift_codegen::ir::Value,
        r: cranelift_codegen::ir::Value,
        float: bool,
    ) -> cranelift_codegen::ir::Value {
        let ins = self.builder.ins();
        if float {
            return match op {
                BinOp::Add => ins.fadd(l, r),
                BinOp::Sub => ins.fsub(l, r),
                BinOp::Mul => ins.fmul(l, r),
                BinOp::Div => ins.fdiv(l, r),
                BinOp::Rem => unreachable!("f64 % is rejected by the checker"),
                BinOp::Eq => ins.fcmp(FloatCC::Equal, l, r),
                BinOp::Ne => ins.fcmp(FloatCC::NotEqual, l, r),
                BinOp::Lt => ins.fcmp(FloatCC::LessThan, l, r),
                BinOp::Le => ins.fcmp(FloatCC::LessThanOrEqual, l, r),
                BinOp::Gt => ins.fcmp(FloatCC::GreaterThan, l, r),
                BinOp::Ge => ins.fcmp(FloatCC::GreaterThanOrEqual, l, r),
                BinOp::And | BinOp::Or => unreachable!("short-circuit ops rejected"),
                BinOp::Pow => unreachable!("`f64 **` is lowered by emit_pow_f64"),
                BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                    unreachable!("bitwise operators on f64 are rejected by the checker")
                }
            };
        }
        match op {
            BinOp::Add => ins.iadd(l, r),
            BinOp::Sub => ins.isub(l, r),
            BinOp::Mul => ins.imul(l, r),
            BinOp::Div => ins.sdiv(l, r),
            BinOp::Rem => ins.srem(l, r),
            BinOp::BitAnd => ins.band(l, r),
            BinOp::BitOr => ins.bor(l, r),
            BinOp::BitXor => ins.bxor(l, r),
            // Cranelift masks the amount to its low six bits, the release
            // contract for out-of-range shifts.
            BinOp::Shl => ins.ishl(l, r),
            BinOp::Shr => ins.sshr(l, r),
            BinOp::Eq => ins.icmp(IntCC::Equal, l, r),
            BinOp::Ne => ins.icmp(IntCC::NotEqual, l, r),
            BinOp::Lt => ins.icmp(IntCC::SignedLessThan, l, r),
            BinOp::Le => ins.icmp(IntCC::SignedLessThanOrEqual, l, r),
            BinOp::Gt => ins.icmp(IntCC::SignedGreaterThan, l, r),
            BinOp::Ge => ins.icmp(IntCC::SignedGreaterThanOrEqual, l, r),
            BinOp::And | BinOp::Or => unreachable!("short-circuit ops rejected"),
            // `i64 **` is lowered by `emit_pow_i64` before this table is
            // reached, because a dynamic exponent needs its own blocks.
            BinOp::Pow => unreachable!("`i64 **` is lowered by emit_pow_i64"),
        }
    }

    /// One indirect call with its call-stack frame and the panic-depth
    /// protocol. With `unchecked`, a nonzero flag skips the protocol at run
    /// time: only the depth snapshot and the depth comparison are branched
    /// around, so the call and its operands stay in one block. Duplicating the
    /// call per path would let the optimizer rematerialize pure operand
    /// computations on both paths, and a chain of such sites would then grow
    /// quadratically. Returns the result, or an `i8` zero
    /// for `void`.
    pub(in crate::backend::cranelift) fn emit_lir_indirect_call(
        &mut self,
        signature: cranelift_codegen::ir::SigRef,
        code: cranelift_codegen::ir::Value,
        values: &[cranelift_codegen::ir::Value],
        name: &str,
        span: Span,
        unchecked: Option<cranelift_codegen::ir::Value>,
    ) -> cranelift_codegen::ir::Value {
        let panic_depth = match unchecked {
            None => self.emit_pre_willow_call_panic_depth(),
            Some(unchecked) => {
                let depth = crate::backend::abi::runtime_symbol("willow_panic_depth")
                    .and_then(|symbol| symbol.ret)
                    .expect("willow_panic_depth returns the depth");
                let pointer = reference_type(self.module.target_config());
                let depth_ty = crate::backend::abi::clif_abi_param(depth, pointer).value_type;
                let call_block = self.builder.create_block();
                let depth = self.builder.append_block_param(call_block, depth_ty);
                let checked = self.builder.create_block();
                let skipped = self.builder.ins().iconst(depth_ty, 0);
                self.builder
                    .ins()
                    .brif(unchecked, call_block, &[skipped.into()], checked, &[]);
                self.builder.switch_to_block(checked);
                self.builder.seal_block(checked);
                let before = self
                    .emit_pre_willow_call_panic_depth()
                    .expect("Willow call panic-depth snapshot");
                self.builder.ins().jump(call_block, &[before.into()]);
                self.builder.switch_to_block(call_block);
                self.builder.seal_block(call_block);
                Some(depth)
            }
        };
        let pushed = self.emit_callstack_push(name, span);
        let call = self.builder.ins().call_indirect(signature, code, values);
        let result = self
            .builder
            .inst_results(call)
            .first()
            .copied()
            .unwrap_or_else(|| self.builder.ins().iconst(types::I8, 0));
        if pushed {
            self.emit_callstack_pop();
        }
        let Some(unchecked) = unchecked else {
            self.emit_post_willow_call_panic_check(panic_depth);
            return result;
        };
        // The unchecked closure continuation skips the shared post-call path.
        self.emit_stack_helper();
        self.emit_sync_cancel_check();
        let checked = self.builder.create_block();
        let done = self.builder.create_block();
        self.builder.ins().brif(unchecked, done, &[], checked, &[]);
        self.builder.switch_to_block(checked);
        self.builder.seal_block(checked);
        self.emit_panic_depth_check(panic_depth.expect("checked closure call depth"));
        self.builder.ins().jump(done, &[]);
        self.builder.switch_to_block(done);
        self.builder.seal_block(done);
        result
    }

    fn emit_lir_static_call_values(
        &mut self,
        class: &str,
        method: &str,
        args: &[cranelift_codegen::ir::Value],
        arg_types: &[Type],
        ret_ty: &Type,
        span: Span,
    ) -> cranelift_codegen::ir::Value {
        self.emit_lir_static_call_values_inner(class, method, args, arg_types, ret_ty, span)
    }
    fn emit_lir_static_call_values_inner(
        &mut self,
        class: &str,
        method: &str,
        args: &[cranelift_codegen::ir::Value],
        _arg_types: &[Type],
        ret_ty: &Type,
        span: crate::diagnostics::Span,
    ) -> cranelift_codegen::ir::Value {
        let resolved_class = self.static_call_class_name(class);
        let class = resolved_class.as_str();
        if class == "Map" && method == "new" {
            let (key, value) = builtin_types::binary_args(ret_ty, B::Map)
                .expect("map constructor must carry checked type arguments");
            return self.emit_map_new(key, value);
        }
        if class == "Channel"
            && let Type::Generic(_, type_args) = ret_ty
            && let Some(element_ty) = type_args.first()
        {
            let is_ref = self.builder.ins().iconst(
                types::I64,
                i64::from(
                    crate::backend::cranelift::type_helpers::storage_is_gc_managed(
                        element_ty,
                        self.enum_infos,
                    ),
                ),
            );
            if method == "new" {
                return self.emit_value_runtime_call("willow_channel_new", &[is_ref]);
            }
            if method == "with_capacity" {
                let capacity = args[0];
                return self
                    .emit_value_runtime_call("willow_channel_new_bounded", &[is_ref, capacity]);
            }
        }
        if let Some(handle) = cancellation_handle(ret_ty)
            && method == "new"
            && class == handle.class_name()
        {
            let runtime = format!("{}_new", handle.prefix());
            return self.emit_value_runtime_call(&runtime, &[]);
        }
        if let Some(cell) = atomic_cell(ret_ty)
            && method == "new"
            && class == cell.class_name()
        {
            let initial = args[0];
            let runtime = format!("willow_atomic_{}_new", cell.suffix());
            return self.emit_value_runtime_call(&runtime, &[initial]);
        }
        if let Some((kind, elem)) = blocking_cell(ret_ty)
            && method == "new"
            && class == kind.class_name()
        {
            // The runtime allocates the cell on the GC heap  and
            // roots a reference word itself across that allocation, so no
            // shadow-root push is needed here, unlike the scheduler locks.
            let elem = elem.clone();
            let initial = args[0];
            let word = self.emit_to_storage_word(initial, &elem);
            let is_ref = crate::backend::cranelift::type_helpers::storage_is_gc_managed(
                &elem,
                self.enum_infos,
            );
            let flag = self.builder.ins().iconst(types::I64, is_ref as i64);
            let runtime = format!("{}_new", kind.prefix());
            return self.emit_value_runtime_call(&runtime, &[word, flag]);
        }
        if let Some((prefix, protected)) = scheduler_lock(ret_ty)
            && method == "new"
            && matches!(class, "Mutex" | "RwLock")
        {
            let protected = protected.clone();
            let mut initial = self.emit_to_storage_word(args[0], &protected);
            let is_ref = crate::backend::cranelift::type_helpers::storage_is_gc_managed(
                &protected,
                self.enum_infos,
            );
            if is_ref {
                let slot = self.emit_push_relocatable_root(initial);
                initial = self.emit_reload_root(slot, initial);
            }
            let word = initial;
            let flag = self.builder.ins().iconst(types::I64, is_ref as i64);
            let runtime = format!("{prefix}_new");
            let handle = self.emit_value_runtime_call(&runtime, &[word, flag]);
            if is_ref {
                self.emit_pop_roots_n(1);
                self.roots.gc_root_count -= 1;
            }
            return handle;
        }
        if self.enum_infos.contains_key(class) {
            return self.emit_lir_enum_construction_values(class, method, args, ret_ty);
        }
        if let Some(entry) = namespace_builtin_call(
            self.known_modules,
            self.builtin_module_aliases,
            class,
            method,
        ) {
            let (arg_vals, arg_roots) = (args.to_vec(), 0usize);
            let result = self
                .emit_runtime_call_with_cleanup(entry.runtime, &arg_vals, |this| {
                    if arg_roots > 0 {
                        this.emit_pop_roots_n(arg_roots);
                        this.roots.gc_root_count -= arg_roots;
                    }
                })
                .expect("every builtin namespace entry returns a value");
            return if entry.narrow_to_bool {
                self.builder.ins().ireduce(types::I8, result)
            } else {
                result
            };
        }
        if let Some(module_prefix) = self.known_modules.linker_prefix(class).cloned() {
            let mangled = module_item_symbol(&module_prefix, method);
            let has_reference_args = self.func_param_modes.get(&mangled).is_some_and(|modes| {
                modes
                    .iter()
                    .any(|mode| matches!(mode, ParamMode::Reference { .. }))
            });
            let user_callee = QualifiedName::join(class, method);
            let (arg_vals, arg_roots) = (args.to_vec(), 0usize);
            let fid = *self.func_ids.get(mangled.as_str()).unwrap_or_else(|| {
                panic!("eligible LIR module call `{class}::{method}` has no declared function")
            });
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let pushed = self.emit_callstack_push(&user_callee, span);
            let panic_depth = self.emit_pre_user_call_panic_depth(&mangled);
            let call = self.builder.ins().call(fref, &arg_vals);
            let result = self
                .builder
                .inst_results(call)
                .first()
                .copied()
                .unwrap_or_else(|| {
                    self.builder.ins().iconst(
                        self.classes
                            .clif_type(reference_type(self.module.target_config()), ret_ty),
                        0,
                    )
                });
            if pushed {
                self.emit_callstack_pop();
            }
            if has_reference_args {
                self.emit_flat_reference_call_end();
            }
            if arg_roots > 0 {
                self.emit_pop_roots_n(arg_roots);
                self.roots.gc_root_count -= arg_roots;
            }
            self.emit_post_willow_call_panic_check(panic_depth);
            return result;
        }
        let mangled = class_method_symbol_name(self.known_modules, class, method);
        let fid = self.func_ids[&mangled];
        let dummy_self = self
            .builder
            .ins()
            .iconst(reference_type(self.module.target_config()), 0);
        let has_reference_args = self.func_param_modes.get(&mangled).is_some_and(|modes| {
            modes
                .iter()
                .any(|mode| matches!(mode, ParamMode::Reference { .. }))
        });
        let (arg_vals, arg_roots) = (args.to_vec(), 0usize);
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let mut call_args = vec![dummy_self];
        call_args.extend(arg_vals);
        let pushed = self.emit_callstack_push(method, span);
        let panic_depth = self.emit_pre_user_call_panic_depth(&mangled);
        let call = self.builder.ins().call(fref, &call_args);
        let result = self
            .builder
            .inst_results(call)
            .first()
            .copied()
            .unwrap_or_else(|| {
                self.builder.ins().iconst(
                    self.classes
                        .clif_type(reference_type(self.module.target_config()), ret_ty),
                    0,
                )
            });
        if pushed {
            self.emit_callstack_pop();
        }
        if has_reference_args {
            self.emit_flat_reference_call_end();
        }
        if arg_roots > 0 {
            self.emit_pop_roots_n(arg_roots);
            self.roots.gc_root_count -= arg_roots;
        }
        self.emit_post_willow_call_panic_check(panic_depth);
        result
    }

    /// Instantiate all variants once for this enum type. Physical offsets are
    /// accumulated once; payload stores only hash the type and index a field.
    fn enum_payload_layout(
        &mut self,
        class: &str,
        variant: &str,
        enum_ty: &Type,
    ) -> std::sync::Arc<crate::backend::cranelift::EnumPayloadLayout> {
        if let Some(layouts) = self.enum_payload_layouts.get(enum_ty) {
            return std::sync::Arc::clone(&layouts[variant]);
        }
        let info = self.enum_infos.get(class).expect("checked enum identity");
        let args = match enum_ty {
            Type::Generic(name, args) if name == &TypeId::from_source_name(class) => {
                args.as_slice()
            }
            _ => &[],
        };
        let parameters: HashMap<_, _> = info
            .type_params
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        let bytes = reference_type(self.module.target_config()).bytes();
        let is_heap = info
            .variants
            .iter()
            .any(|variant| !variant.payload_types.is_empty());
        let layouts: HashMap<_, _> = info
            .variants
            .iter()
            .map(|variant| {
                let mut payloads: Vec<_> = variant
                    .payload_types
                    .iter()
                    .map(|ty| crate::semantic::symbols::substitute_type(ty, &parameters))
                    .collect();
                normalize_void_payloads(&mut payloads);
                let kinds: Vec<_> = payloads.iter().map(|ty| self.value_slot_kind(ty)).collect();
                let wire = willow_abi::EnumVariantLayout::new(variant.tag as u32, &kinds);
                let mut offset = wire.payload_byte_offset(bytes) as i32;
                let fields = payloads
                    .into_iter()
                    .zip(&kinds)
                    .map(|(ty, kind)| {
                        let field = (ty, offset);
                        offset += (kind.word_count() * bytes) as i32;
                        field
                    })
                    .collect();
                (
                    variant.name.clone(),
                    std::sync::Arc::new(crate::backend::cranelift::EnumPayloadLayout {
                        tag: variant.tag,
                        is_heap,
                        fields,
                        gc_layout: GcLayoutMetadata::aggregate(
                            GcObjectKind::Enum,
                            willow_abi::EnumVariantLayout::TAG_WORDS,
                            &kinds,
                            bytes,
                        ),
                    }),
                )
            })
            .collect();
        let layout = std::sync::Arc::clone(&layouts[variant]);
        self.enum_payload_layouts.insert(enum_ty.clone(), layouts);
        layout
    }

    fn emit_lir_enum_construction_values(
        &mut self,
        enum_name: &str,
        variant: &str,
        args: &[cranelift_codegen::ir::Value],
        enum_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let layout = self.enum_payload_layout(enum_name, variant, enum_ty);
        let tag = layout.tag;
        if is_scalar_pair(enum_ty) {
            let payload = if let Some((value, (ty, _))) = args.first().zip(layout.fields.first()) {
                self.coerce_to_i64(*value, ty)
            } else {
                self.builder.ins().iconst(types::I64, 0)
            };
            let tag = self.builder.ins().iconst(types::I64, tag);
            return self.emit_pair(tag, payload);
        }
        if self.option_repr(enum_ty) == Some(OptionRepr::NullableGcPointer) {
            return if tag == 0 {
                args[0]
            } else {
                self.builder
                    .ins()
                    .iconst(reference_type(self.module.target_config()), 0)
            };
        }
        if !layout.is_heap {
            return self.builder.ins().iconst(types::I64, tag);
        }
        // The payload allocation may move managed arguments: root each in a
        // relocatable slot and store the reloaded value.
        let slots: Vec<_> = args
            .iter()
            .zip(&layout.fields)
            .map(|(&value, (ty, _))| {
                is_gc_managed(ty, self.enum_infos).then(|| self.emit_push_relocatable_root(value))
            })
            .collect();
        let ptr = self.emit_gc_alloc(layout.gc_layout.clone());
        let tag_value = self.builder.ins().iconst(types::I64, layout.tag);
        self.builder
            .ins()
            .store(MemFlagsData::new(), tag_value, ptr, 0i32);
        for ((&value, (ty, offset)), slot) in args.iter().zip(&layout.fields).zip(&slots) {
            let value = match slot {
                Some(slot) => self.emit_reload_root(*slot, value),
                None => value,
            };
            let word = self.coerce_to_i64(value, ty);
            self.emit_gc_heap_store(ptr, *offset, word, ty, GcStoreDestination::EnumPayload);
        }
        let roots = slots.iter().flatten().count();
        if roots > 0 {
            self.emit_pop_roots_n(roots);
            self.roots.gc_root_count -= roots;
        }
        ptr
    }

    /// Allocate and initialise the tag before evaluating any source payload.
    /// Nullable Option representations reserve no object: the payload store
    /// below replaces this null placeholder with the actual pointer.
    fn emit_flat_enum_alloc(
        &mut self,
        class: &str,
        variant: &str,
        enum_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let layout = self.enum_payload_layout(class, variant, enum_ty);
        let tag = layout.tag;
        if is_scalar_pair(enum_ty) {
            let tag = self.builder.ins().iconst(types::I64, tag);
            let payload = self.builder.ins().iconst(types::I64, 0);
            return self.emit_pair(tag, payload);
        }
        if self.option_repr(enum_ty) == Some(OptionRepr::NullableGcPointer) {
            return self
                .builder
                .ins()
                .iconst(reference_type(self.module.target_config()), 0);
        }
        if !layout.is_heap {
            return self.builder.ins().iconst(types::I64, tag);
        }
        let object = self.emit_gc_alloc(layout.gc_layout.clone());
        let tag_value = self.builder.ins().iconst(types::I64, tag);
        self.builder
            .ins()
            .store(MemFlagsData::new(), tag_value, object, 0i32);
        object
    }

    /// A payload store is its own LIR operation so coercion occurs before the
    /// next payload is evaluated. The returned representation is written back
    /// to the enum local (necessary for nullable Option::Some).
    // Keep explicit emission operands aligned with the LIR/runtime ABI.
    fn emit_flat_enum_payload_store(
        &mut self,
        object: cranelift_codegen::ir::Value,
        class: &str,
        variant: &str,
        index: usize,
        (value, source_ty): (cranelift_codegen::ir::Value, &Type),
        enum_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        if is_scalar_pair(enum_ty) {
            let payload = self.coerce_to_i64(value, source_ty);
            let tag = self.emit_load_enum_tag(object);
            return self.emit_pair(tag, payload);
        }
        let layout = self.enum_payload_layout(class, variant, enum_ty);
        // No GC point sits between these roots and the store below.
        let before = self.roots.gc_root_count;
        if layout.is_heap {
            self.emit_push_relocatable_root(object);
        }
        if is_gc_managed(source_ty, self.enum_infos) {
            self.emit_push_relocatable_root(value);
        }
        let (target, offset) = &layout.fields[index];
        let value = self.coerce_to_target(value, source_ty, target);
        let result = if self.option_repr(enum_ty) == Some(OptionRepr::NullableGcPointer) {
            assert_eq!(index, 0);
            value
        } else {
            let word = self.coerce_to_i64(value, target);
            self.emit_gc_heap_store(
                object,
                *offset,
                word,
                target,
                GcStoreDestination::EnumPayload,
            );
            object
        };
        self.emit_pop_roots_n(self.roots.gc_root_count - before);
        self.roots.gc_root_count = before;
        result
    }
}
