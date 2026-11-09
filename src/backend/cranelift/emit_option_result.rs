use crate::semantic::builtin_types::BuiltinTypeId as Builtin;
use cranelift_codegen::ir::{InstBuilder, MemFlagsData, condcodes::IntCC, types};
use cranelift_module::Module;

use super::*;

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Read an enum payload at its full representation width.
    pub(super) fn emit_enum_payload_bits(
        &mut self,
        value: cranelift_codegen::ir::Value,
        payload_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        if self.is_inline_pair(payload_ty) {
            self.builder
                .ins()
                .load(types::I128, MemFlagsData::new(), value, 8i32)
        } else {
            self.emit_enum_payload_word(value)
        }
    }

    pub(super) fn emit_option_is_some(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        inner_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let option_ty = Builtin::Option.apply(vec![inner_ty.clone()]);
        if self.option_repr(&option_ty) == Some(OptionRepr::NullableGcPointer) {
            return self.builder.ins().icmp_imm_u(IntCC::NotEqual, ptr, 0);
        }
        let tag = self.emit_load_enum_tag(ptr);
        let some = self.builder.ins().iconst(types::I64, 0);
        self.builder.ins().icmp(IntCC::Equal, tag, some)
    }

    pub(super) fn emit_option_payload(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        inner_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let option_ty = Builtin::Option.apply(vec![inner_ty.clone()]);
        let raw = if self.option_repr(&option_ty) == Some(OptionRepr::NullableGcPointer) {
            ptr
        } else {
            self.emit_enum_payload_bits(ptr, inner_ty)
        };
        self.coerce_i64_to(raw, inner_ty)
    }

    pub(super) fn emit_option_unwrap(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        inner_ty: &Type,
        msg: cranelift_codegen::ir::Value,
        span: Option<crate::diagnostics::Span>,
    ) -> cranelift_codegen::ir::Value {
        let is_some = self.emit_option_is_some(ptr, inner_ty);
        let some_block = self.builder.create_block();
        let none_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_some, some_block, &[], none_block, &[]);

        self.builder.switch_to_block(none_block);
        self.builder.seal_block(none_block);
        self.emit_language_panic(msg, span);

        self.builder.switch_to_block(some_block);
        self.builder.seal_block(some_block);
        self.locals.terminated = false;
        self.emit_option_payload(ptr, inner_ty)
    }

    pub(super) fn emit_option_unwrap_or(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        inner_ty: &Type,
        default_val: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let result_var = self.builder.declare_var(self.clif_type(inner_ty));
        let is_some = self.emit_option_is_some(ptr, inner_ty);
        let some_block = self.builder.create_block();
        let none_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_some, some_block, &[], none_block, &[]);

        self.builder.switch_to_block(some_block);
        self.builder.seal_block(some_block);
        let payload = self.emit_option_payload(ptr, inner_ty);
        self.builder.def_var(result_var, payload);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(none_block);
        self.builder.seal_block(none_block);
        self.builder.def_var(result_var, default_val);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit: if enum tag == success_tag, return payload at offset 8; else panic(msg).
    pub(super) fn emit_enum_unwrap(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        payload_ty: &Type,
        success_tag: i64,
        msg: cranelift_codegen::ir::Value,
        span: Option<crate::diagnostics::Span>,
    ) -> cranelift_codegen::ir::Value {
        let tag = self.emit_load_enum_tag(ptr);
        let expected = self.builder.ins().iconst(types::I64, success_tag);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, expected);

        let ok_block = self.builder.create_block();
        let fail_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], fail_block, &[]);

        self.builder.switch_to_block(fail_block);
        self.builder.seal_block(fail_block);
        self.emit_language_panic(msg, span);

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        self.locals.terminated = false;
        let clif_ty = self.clif_type(payload_ty);
        let raw = self.emit_enum_payload_bits(ptr, payload_ty);
        if clif_ty == types::F64 {
            self.builder
                .ins()
                .bitcast(types::F64, MemFlagsData::new(), raw)
        } else if clif_ty == types::I8 {
            self.builder.ins().ireduce(types::I8, raw)
        } else {
            raw
        }
    }

    /// Emit: if enum tag == success_tag, return payload at offset 8; else return default.
    pub(super) fn emit_enum_unwrap_or(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        payload_ty: &Type,
        success_tag: i64,
        default_val: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let clif_ty = self.clif_type(payload_ty);
        let result_var = self.builder.declare_var(clif_ty);
        let tag = self.emit_load_enum_tag(ptr);
        let expected = self.builder.ins().iconst(types::I64, success_tag);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, expected);

        let ok_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let merge = self.builder.create_block();

        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], else_block, &[]);

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let raw = self.emit_enum_payload_bits(ptr, payload_ty);
        let payload = if clif_ty == types::F64 {
            self.builder
                .ins()
                .bitcast(types::F64, MemFlagsData::new(), raw)
        } else if clif_ty == types::I8 {
            self.builder.ins().ireduce(types::I8, raw)
        } else {
            raw
        };
        self.builder.def_var(result_var, payload);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(else_block);
        self.builder.seal_block(else_block);
        self.builder.def_var(result_var, default_val);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit an indirect call through a function value.
    pub(super) fn emit_indirect_call(
        &mut self,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
        args: &[cranelift_codegen::ir::Value],
    ) -> cranelift_codegen::ir::Value {
        if let Type::Fn(param_types, ret_type) = f_ty {
            let mut sig = self.module.make_signature();
            for pt in param_types {
                sig.params.push(AbiParam::new(self.clif_type(pt)));
            }
            let has_return = **ret_type != Type::Void;
            if has_return {
                sig.returns.push(AbiParam::new(self.clif_type(ret_type)));
            }
            let sig_ref = self.builder.import_signature(sig);
            let panic_depth = self.emit_pre_willow_call_panic_depth();
            let call = self.builder.ins().call_indirect(sig_ref, f_val, args);
            let results = self.builder.inst_results(call);
            let result = if results.is_empty() {
                self.builder.ins().iconst(types::I64, 0)
            } else {
                results[0]
            };
            self.emit_post_willow_call_panic_check(panic_depth);
            result
        } else {
            panic!(
                "compiler invariant violated: indirect call target typed `{}` instead of a function",
                debug_type_name(f_ty)
            )
        }
    }

    // The combinators below read `ptr` and `f_val` only before their first GC
    // point (the callback or an allocation). Their callers root those values
    // relocatably and do not reload them, so keep that order.

    /// Emit Option<T>.map(f) → Option<U>
    pub(super) fn emit_option_map(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        inner_ty: &Type,
        ret_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let output_ty = Builtin::Option.apply(vec![ret_ty.clone()]);
        let result_var = self.builder.declare_var(self.clif_type(&output_ty));
        let is_some = self.emit_option_is_some(ptr, inner_ty);

        let some_block = self.builder.create_block();
        let none_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_some, some_block, &[], none_block, &[]);

        self.builder.switch_to_block(some_block);
        self.builder.seal_block(some_block);
        let payload = self.emit_option_payload(ptr, inner_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        let new_some = self.emit_alloc_option_some(ret_ty, result);
        self.builder.def_var(result_var, new_some);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(none_block);
        self.builder.seal_block(none_block);
        let new_none = self.emit_alloc_option_none(ret_ty);
        self.builder.def_var(result_var, new_none);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit Option<T>.and_then(f) where f: fn(T) -> Option<U>
    pub(super) fn emit_option_and_then(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        inner_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let output_ty = match f_ty {
            Type::Fn(_, ret) => (**ret).clone(),
            _ => unreachable!("combinator callback must be a function"),
        };
        let result_var = self.builder.declare_var(self.clif_type(&output_ty));
        let is_some = self.emit_option_is_some(ptr, inner_ty);
        let output_inner = match f_ty {
            Type::Fn(_, ret) => option_inner(ret).cloned().unwrap_or(Type::Void),
            _ => Type::Void,
        };

        let some_block = self.builder.create_block();
        let none_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_some, some_block, &[], none_block, &[]);

        self.builder.switch_to_block(some_block);
        self.builder.seal_block(some_block);
        let payload = self.emit_option_payload(ptr, inner_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        self.builder.def_var(result_var, result);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(none_block);
        self.builder.seal_block(none_block);
        let new_none = self.emit_alloc_option_none(&output_inner);
        self.builder.def_var(result_var, new_none);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit Option<T>.or_else(f) where f: fn() -> Option<T>
    pub(super) fn emit_option_or_else(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        inner_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let output_ty = Builtin::Option.apply(vec![inner_ty.clone()]);
        let result_var = self.builder.declare_var(self.clif_type(&output_ty));
        let is_some = self.emit_option_is_some(ptr, inner_ty);

        let some_block = self.builder.create_block();
        let none_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_some, some_block, &[], none_block, &[]);

        self.builder.switch_to_block(some_block);
        self.builder.seal_block(some_block);
        self.builder.def_var(result_var, ptr);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(none_block);
        self.builder.seal_block(none_block);
        let result = self.emit_indirect_call(f_val, f_ty, &[]);
        self.builder.def_var(result_var, result);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit `Result<T,E>.map(f)` (`side == Ok`, `f: fn(T) -> U`, giving
    /// `Result<U, E>`) or `Result<T,E>.map_err(f)` (`side == Err`,
    /// `f: fn(E) -> F`, giving `Result<T, F>`). The arm named by `side` calls
    /// `f` and rewraps its result; the other arm rewraps its payload unchanged.
    pub(super) fn emit_result_map_side(
        &mut self,
        side: ResultSide,
        ptr: cranelift_codegen::ir::Value,
        ok_ty: &Type,
        err_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ret_ty = &callback_return_type(f_ty);
        let type_args = match side {
            ResultSide::Ok => vec![ret_ty.clone(), err_ty.clone()],
            ResultSide::Err => vec![ok_ty.clone(), ret_ty.clone()],
        };
        let output_ty = Builtin::Result.apply(type_args);
        self.emit_result_arms(ptr, &output_ty, |this, arm| {
            let arm_ty = arm.payload_ty(ok_ty, err_ty);
            let payload = this.emit_result_arm_payload(ptr, arm_ty);
            if arm == side {
                let result = this.emit_indirect_call(f_val, f_ty, &[payload]);
                this.emit_alloc_result_variant(&output_ty, arm.tag(), ret_ty, result)
            } else {
                this.emit_alloc_result_variant(&output_ty, arm.tag(), arm_ty, payload)
            }
        })
    }

    /// Emit `Result<T,E>.and_then(f)` (`side == Ok`, `f: fn(T) -> Result<U,E>`)
    /// or `Result<T,E>.or_else(f)` (`side == Err`, `f: fn(E) -> Result<T,F>`).
    /// The arm named by `side` returns `f`'s result; the other arm passes the
    /// receiver through, rewrapping it only when the output representation
    /// differs from the receiver's.
    pub(super) fn emit_result_chain_side(
        &mut self,
        side: ResultSide,
        ptr: cranelift_codegen::ir::Value,
        ok_ty: &Type,
        err_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let output_ty = callback_return_type(f_ty);
        self.emit_result_arms(ptr, &output_ty, |this, arm| {
            let arm_ty = arm.payload_ty(ok_ty, err_ty);
            if arm == side {
                let payload = this.emit_result_arm_payload(ptr, arm_ty);
                this.emit_indirect_call(f_val, f_ty, &[payload])
            } else if this.builder.func.dfg.value_type(ptr) == this.clif_type(&output_ty) {
                ptr
            } else {
                let payload = this.emit_result_arm_payload(ptr, arm_ty);
                this.emit_alloc_result_variant(&output_ty, arm.tag(), arm_ty, payload)
            }
        })
    }

    /// Branch on a `Result` receiver's tag, emit the `Ok` arm then the `Err`
    /// arm with `emit_arm`, and merge their values into one `output_ty` value.
    fn emit_result_arms(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        output_ty: &Type,
        mut emit_arm: impl FnMut(&mut Self, ResultSide) -> cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let result_var = self.builder.declare_var(self.clif_type(output_ty));
        let tag = self.emit_load_enum_tag(ptr);
        let ok_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, ok_tag_val);

        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], err_block, &[]);

        for (block, arm) in [(ok_block, ResultSide::Ok), (err_block, ResultSide::Err)] {
            self.builder.switch_to_block(block);
            self.builder.seal_block(block);
            let value = emit_arm(self, arm);
            self.builder.def_var(result_var, value);
            self.builder.ins().jump(merge, &[]);
        }

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Read a `Result` arm's payload as a typed value.
    fn emit_result_arm_payload(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        payload_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let raw = self.emit_enum_payload_bits(ptr, payload_ty);
        self.coerce_i64_to(raw, payload_ty)
    }

    /// Construct a scalar tagged pair, falling back to the boxed runtime layout.
    pub(super) fn emit_alloc_result_variant(
        &mut self,
        result_ty: &Type,
        tag: i64,
        payload_ty: &Type,
        payload: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        if super::option_repr::is_scalar_pair(result_ty) {
            let payload = self.coerce_to_i64(payload, payload_ty);
            let tag = self.builder.ins().iconst(types::I64, tag);
            self.emit_pair(tag, payload)
        } else {
            self.emit_alloc_enum_variant(tag, payload_ty, payload)
        }
    }

    /// Allocate a new 2-word enum (tag + payload) where payload is a typed value.
    pub(super) fn emit_alloc_enum_variant(
        &mut self,
        tag: i64,
        payload_ty: &Type,
        payload_val: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let payload_is_gc = is_gc_managed(payload_ty, self.enum_infos);
        let slots = [self.value_slot_kind(payload_ty)];
        let layout = willow_abi::EnumVariantLayout::new(tag as u32, &slots);
        let pointer_bytes = reference_type(self.module.target_config()).bytes();
        // Root the payload across the enum allocation: a GC-managed payload is a
        // live pointer that must survive the collection the GC allocator may
        // trigger before we store it into the new enum. Only reference payloads
        // are rooted; rooting a scalar word would make the GC mark it as a
        // bogus object pointer.
        let payload_root = payload_is_gc.then(|| self.emit_push_relocatable_root(payload_val));
        let ptr = self.emit_gc_alloc(GcLayoutMetadata::new(
            GcObjectKind::Enum,
            i64::from(layout.payload_bytes(pointer_bytes)),
            0,
            layout.gc_ref_mask(),
        ));
        let payload_val = payload_root.map_or(payload_val, |slot| {
            self.stack_load(self.clif_type(payload_ty), slot)
        });
        let tag_val = self.builder.ins().iconst(types::I64, tag);
        self.builder
            .ins()
            .store(MemFlagsData::new(), tag_val, ptr, 0i32);
        let payload_i64 = if matches!(payload_ty, Type::F64) {
            self.builder
                .ins()
                .bitcast(types::I64, MemFlagsData::new(), payload_val)
        } else if matches!(payload_ty, Type::Bool) {
            self.builder.ins().uextend(types::I64, payload_val)
        } else {
            payload_val
        };
        self.emit_gc_heap_store(
            ptr,
            layout.payload_byte_offset(pointer_bytes) as i32,
            payload_i64,
            payload_ty,
            GcStoreDestination::EnumPayload,
        );
        if payload_is_gc {
            self.emit_pop_roots_n(1);
            self.roots.gc_root_count -= 1;
        }
        ptr
    }

    /// Construct `Some(payload)` using the central representation decision.
    pub(super) fn emit_alloc_option_some(
        &mut self,
        payload_ty: &Type,
        payload_val: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let option_ty = Builtin::Option.apply(vec![payload_ty.clone()]);
        if self.option_repr(&option_ty) == Some(OptionRepr::NullableGcPointer) {
            payload_val
        } else if super::option_repr::is_scalar_pair(&option_ty) {
            let payload = self.coerce_to_i64(payload_val, payload_ty);
            let tag = self.builder.ins().iconst(types::I64, 0);
            self.emit_pair(tag, payload)
        } else {
            self.emit_alloc_enum_variant(0, payload_ty, payload_val)
        }
    }

    /// Construct `None` using the central representation decision.
    pub(super) fn emit_alloc_option_none(
        &mut self,
        payload_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let option_ty = Builtin::Option.apply(vec![payload_ty.clone()]);
        if self.option_repr(&option_ty) == Some(OptionRepr::NullableGcPointer) {
            self.builder
                .ins()
                .iconst(reference_type(self.module.target_config()), 0)
        } else if super::option_repr::is_scalar_pair(&option_ty) {
            let tag = self.builder.ins().iconst(types::I64, 1);
            let payload = self.builder.ins().iconst(types::I64, 0);
            self.emit_pair(tag, payload)
        } else {
            self.emit_alloc_boxed_none()
        }
    }

    /// Allocate the boxed-tag representation of Option::None.
    fn emit_alloc_boxed_none(&mut self) -> cranelift_codegen::ir::Value {
        let ptr = self.emit_gc_alloc(GcLayoutMetadata::new(GcObjectKind::Enum, 8, 0, 0));
        let none_tag = self.builder.ins().iconst(types::I64, 1);
        self.builder
            .ins()
            .store(MemFlagsData::new(), none_tag, ptr, 0i32);
        ptr
    }
}

/// The return type of a combinator's callback.
fn callback_return_type(f_ty: &Type) -> Type {
    match f_ty {
        Type::Fn(_, ret) => (**ret).clone(),
        _ => unreachable!("combinator callback must be a function"),
    }
}

/// Which arm of a `Result` a combinator transforms.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ResultSide {
    Ok,
    Err,
}

impl ResultSide {
    /// The arm's enum tag.
    fn tag(self) -> i64 {
        match self {
            ResultSide::Ok => 0,
            ResultSide::Err => 1,
        }
    }

    /// The arm's payload type.
    fn payload_ty<'t>(self, ok_ty: &'t Type, err_ty: &'t Type) -> &'t Type {
        match self {
            ResultSide::Ok => ok_ty,
            ResultSide::Err => err_ty,
        }
    }
}
