use cranelift_codegen::ir::{InstBuilder, MemFlags, condcodes::IntCC, types};
use cranelift_module::Module;

use super::*;

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Emit code for Option<T> and Result<T,E> built-in helper methods.
    /// Returns Some(value) if the method was handled, None to fall through to
    /// the regular class-method dispatch.
    pub(super) fn emit_option_result_method_call(
        &mut self,
        enum_ptr: cranelift_codegen::ir::Value,
        obj_ty: &Type,
        m: &MethodCallExpr,
    ) -> Option<cranelift_codegen::ir::Value> {
        // Tag layout: Ok/Some = tag 0, Err/None = tag 1 (declaration order).
        const SOME_TAG: i64 = 0;
        const NONE_TAG: i64 = 1;
        const OK_TAG: i64 = 0;
        const ERR_TAG: i64 = 1;

        match obj_ty {
            Type::Generic(name, args) if name == "Option" => {
                let inner_ty = args.first().cloned().unwrap_or(Type::Void);
                match m.method.as_str() {
                    "is_some" => {
                        let tag =
                            self.builder
                                .ins()
                                .load(types::I64, MemFlags::new(), enum_ptr, 0i32);
                        let some = self.builder.ins().iconst(types::I64, SOME_TAG);
                        Some(self.builder.ins().icmp(IntCC::Equal, tag, some))
                    }
                    "is_none" => {
                        let tag =
                            self.builder
                                .ins()
                                .load(types::I64, MemFlags::new(), enum_ptr, 0i32);
                        let none = self.builder.ins().iconst(types::I64, NONE_TAG);
                        Some(self.builder.ins().icmp(IntCC::Equal, tag, none))
                    }
                    "unwrap" => {
                        let msg =
                            self.emit_string_literal("called `Option::unwrap()` on a `None` value");
                        Some(self.emit_enum_unwrap(enum_ptr, &inner_ty, SOME_TAG, msg))
                    }
                    "expect" => {
                        let msg = if let Some(arg) = m.args.first() {
                            self.emit_expr(&arg.expr)
                        } else {
                            self.emit_string_literal("called `Option::expect()` on a `None` value")
                        };
                        Some(self.emit_enum_unwrap(enum_ptr, &inner_ty, SOME_TAG, msg))
                    }
                    "unwrap_or" => {
                        let default_val = m
                            .args
                            .first()
                            .map(|a| self.emit_expr(&a.expr))
                            .unwrap_or_else(|| self.builder.ins().iconst(types::I64, 0));
                        Some(self.emit_enum_unwrap_or(enum_ptr, &inner_ty, SOME_TAG, default_val))
                    }
                    "map" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            let ret_ty = match &f_ty {
                                Type::Fn(_, ret) => *ret.clone(),
                                _ => Type::Void,
                            };
                            Some(self.emit_option_map(enum_ptr, &inner_ty, &ret_ty, f_val, &f_ty))
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    "and_then" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            Some(self.emit_option_and_then(enum_ptr, &inner_ty, f_val, &f_ty))
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    "or_else" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            Some(self.emit_option_or_else(enum_ptr, f_val, &f_ty))
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    _ => None,
                }
            }
            Type::Generic(name, args) if name == "Result" => {
                let ok_ty = args.first().cloned().unwrap_or(Type::Void);
                let err_ty = args.get(1).cloned().unwrap_or(Type::Void);
                match m.method.as_str() {
                    "is_ok" => {
                        let tag =
                            self.builder
                                .ins()
                                .load(types::I64, MemFlags::new(), enum_ptr, 0i32);
                        let ok = self.builder.ins().iconst(types::I64, OK_TAG);
                        Some(self.builder.ins().icmp(IntCC::Equal, tag, ok))
                    }
                    "is_err" => {
                        let tag =
                            self.builder
                                .ins()
                                .load(types::I64, MemFlags::new(), enum_ptr, 0i32);
                        let err = self.builder.ins().iconst(types::I64, ERR_TAG);
                        Some(self.builder.ins().icmp(IntCC::Equal, tag, err))
                    }
                    "unwrap" => {
                        let msg =
                            self.emit_string_literal("called `Result::unwrap()` on an `Err` value");
                        Some(self.emit_enum_unwrap(enum_ptr, &ok_ty, OK_TAG, msg))
                    }
                    "expect" => {
                        let msg = if let Some(arg) = m.args.first() {
                            self.emit_expr(&arg.expr)
                        } else {
                            self.emit_string_literal("called `Result::expect()` on an `Err` value")
                        };
                        Some(self.emit_enum_unwrap(enum_ptr, &ok_ty, OK_TAG, msg))
                    }
                    "unwrap_or" => {
                        let default_val = m
                            .args
                            .first()
                            .map(|a| self.emit_expr(&a.expr))
                            .unwrap_or_else(|| self.builder.ins().iconst(types::I64, 0));
                        Some(self.emit_enum_unwrap_or(enum_ptr, &ok_ty, OK_TAG, default_val))
                    }
                    "unwrap_err" => {
                        let msg = self
                            .emit_string_literal("called `Result::unwrap_err()` on an `Ok` value");
                        Some(self.emit_enum_unwrap(enum_ptr, &err_ty, ERR_TAG, msg))
                    }
                    "map" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            let ret_ty = match &f_ty {
                                Type::Fn(_, ret) => *ret.clone(),
                                _ => Type::Void,
                            };
                            Some(
                                self.emit_result_map(
                                    enum_ptr, &ok_ty, &err_ty, &ret_ty, f_val, &f_ty,
                                ),
                            )
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    "map_err" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            let ret_ty = match &f_ty {
                                Type::Fn(_, ret) => *ret.clone(),
                                _ => Type::Void,
                            };
                            Some(self.emit_result_map_err(
                                enum_ptr, &ok_ty, &err_ty, &ret_ty, f_val, &f_ty,
                            ))
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    "and_then" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            Some(self.emit_result_and_then(enum_ptr, &ok_ty, f_val, &f_ty))
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    "or_else" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            Some(self.emit_result_or_else(enum_ptr, &err_ty, f_val, &f_ty))
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Emit: if enum tag == success_tag, return payload at offset 8; else panic(msg).
    pub(super) fn emit_enum_unwrap(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        payload_ty: &Type,
        success_tag: i64,
        msg: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let expected = self.builder.ins().iconst(types::I64, success_tag);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, expected);

        let ok_block = self.builder.create_block();
        let fail_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], fail_block, &[]);

        self.builder.switch_to_block(fail_block);
        self.builder.seal_block(fail_block);
        let fid = self.func_id("willow_panic");
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        self.builder.ins().call(fref, &[msg]);
        self.builder.ins().trap(TrapCode::unwrap_user(1));

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let clif_ty = clif_type(payload_ty);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        if clif_ty == types::F64 {
            self.builder.ins().bitcast(types::F64, MemFlags::new(), raw)
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
        let clif_ty = clif_type(payload_ty);
        let result_var = self.builder.declare_var(clif_ty);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
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
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = if clif_ty == types::F64 {
            self.builder.ins().bitcast(types::F64, MemFlags::new(), raw)
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
                sig.params.push(AbiParam::new(clif_type(pt)));
            }
            let has_return = **ret_type != Type::Void;
            if has_return {
                sig.returns.push(AbiParam::new(clif_type(ret_type)));
            }
            let sig_ref = self.builder.import_signature(sig);
            let call = self.builder.ins().call_indirect(sig_ref, f_val, args);
            let results = self.builder.inst_results(call);
            if results.is_empty() {
                self.builder.ins().iconst(types::I64, 0)
            } else {
                results[0]
            }
        } else {
            self.builder.ins().iconst(types::I64, 0)
        }
    }

    /// Emit Option<T>.map(f) → Option<U>
    pub(super) fn emit_option_map(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        inner_ty: &Type,
        ret_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let some_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_some = self.builder.ins().icmp(IntCC::Equal, tag, some_tag_val);

        let some_block = self.builder.create_block();
        let none_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_some, some_block, &[], none_block, &[]);

        self.builder.switch_to_block(some_block);
        self.builder.seal_block(some_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = self.coerce_i64_to(raw, inner_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        let new_some = self.emit_alloc_enum_variant(0, ret_ty, result);
        self.builder.def_var(result_var, new_some);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(none_block);
        self.builder.seal_block(none_block);
        let new_none = self.emit_alloc_none();
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
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let some_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_some = self.builder.ins().icmp(IntCC::Equal, tag, some_tag_val);

        let some_block = self.builder.create_block();
        let none_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_some, some_block, &[], none_block, &[]);

        self.builder.switch_to_block(some_block);
        self.builder.seal_block(some_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = self.coerce_i64_to(raw, inner_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        self.builder.def_var(result_var, result);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(none_block);
        self.builder.seal_block(none_block);
        let new_none = self.emit_alloc_none();
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
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let some_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_some = self.builder.ins().icmp(IntCC::Equal, tag, some_tag_val);

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

    /// Emit Result<T,E>.map(f) where f: fn(T) -> U → Result<U, E>
    pub(super) fn emit_result_map(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        ok_ty: &Type,
        err_ty: &Type,
        ret_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let ok_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, ok_tag_val);

        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], err_block, &[]);

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = self.coerce_i64_to(raw, ok_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        let new_ok = self.emit_alloc_enum_variant(0, ret_ty, result);
        self.builder.def_var(result_var, new_ok);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        let err_raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let new_err = self.emit_alloc_enum_variant_raw(1, err_ty, err_raw);
        self.builder.def_var(result_var, new_err);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit Result<T,E>.map_err(f) where f: fn(E) -> F → Result<T, F>
    pub(super) fn emit_result_map_err(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        ok_ty: &Type,
        err_ty: &Type,
        ret_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let ok_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, ok_tag_val);

        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], err_block, &[]);

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let ok_raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let new_ok = self.emit_alloc_enum_variant_raw(0, ok_ty, ok_raw);
        self.builder.def_var(result_var, new_ok);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = self.coerce_i64_to(raw, err_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        let new_err = self.emit_alloc_enum_variant(1, ret_ty, result);
        self.builder.def_var(result_var, new_err);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit Result<T,E>.and_then(f) where f: fn(T) -> Result<U,E>
    pub(super) fn emit_result_and_then(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        ok_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let ok_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, ok_tag_val);

        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], err_block, &[]);

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = self.coerce_i64_to(raw, ok_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        self.builder.def_var(result_var, result);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        self.builder.def_var(result_var, ptr);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit Result<T,E>.or_else(f) where f: fn(E) -> Result<T,F>
    pub(super) fn emit_result_or_else(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        err_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let ok_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, ok_tag_val);

        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], err_block, &[]);

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        self.builder.def_var(result_var, ptr);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = self.coerce_i64_to(raw, err_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        self.builder.def_var(result_var, result);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Allocate a new 2-word enum (tag + payload) where payload is a typed value.
    pub(super) fn emit_alloc_enum_variant(
        &mut self,
        tag: i64,
        payload_ty: &Type,
        payload_val: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let payload_is_gc = is_gc_managed(payload_ty, self.enum_infos);
        let gc_mask: i64 = if payload_is_gc { 0b10 } else { 0 };
        // Root the payload across the enum allocation: a GC-managed payload is a
        // live pointer that must survive the collection `willow_alloc_typed` may
        // trigger before we store it into the new enum. Only reference payloads
        // are rooted; rooting a scalar word would make the GC mark it as a
        // bogus object pointer.
        if payload_is_gc {
            self.emit_push_root(payload_val);
        }
        let size = self.builder.ins().iconst(types::I64, 16);
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
            .store(MemFlags::new(), tag_val, ptr, 0i32);
        let payload_i64 = if matches!(payload_ty, Type::F64) {
            self.builder
                .ins()
                .bitcast(types::I64, MemFlags::new(), payload_val)
        } else if matches!(payload_ty, Type::Bool) {
            self.builder.ins().uextend(types::I64, payload_val)
        } else {
            payload_val
        };
        self.builder
            .ins()
            .store(MemFlags::new(), payload_i64, ptr, 8i32);
        if payload_is_gc {
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
        }
        ptr
    }

    /// Allocate a new 2-word enum (tag + payload) where payload is already an i64 raw word.
    pub(super) fn emit_alloc_enum_variant_raw(
        &mut self,
        tag: i64,
        payload_ty: &Type,
        payload_raw: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let payload_is_gc = is_gc_managed(payload_ty, self.enum_infos);
        let gc_mask: i64 = if payload_is_gc { 0b10 } else { 0 };
        // See `emit_alloc_enum_variant`: root a GC-managed payload across the
        // allocation so a collection cannot free it before it is stored.
        if payload_is_gc {
            self.emit_push_root(payload_raw);
        }
        let size = self.builder.ins().iconst(types::I64, 16);
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
            .store(MemFlags::new(), tag_val, ptr, 0i32);
        self.builder
            .ins()
            .store(MemFlags::new(), payload_raw, ptr, 8i32);
        if payload_is_gc {
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
        }
        ptr
    }

    /// Allocate an Option::None (1-word enum, tag=1, no payload).
    pub(super) fn emit_alloc_none(&mut self) -> cranelift_codegen::ir::Value {
        let size = self.builder.ins().iconst(types::I64, 8);
        let mask = self.builder.ins().iconst(types::I64, 0);
        let alloc_id = self.func_id("willow_alloc_typed");
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self.builder.ins().call(alloc_ref, &[size, mask]);
        let ptr = self.builder.inst_results(call)[0];
        let none_tag = self.builder.ins().iconst(types::I64, 1);
        self.builder
            .ins()
            .store(MemFlags::new(), none_tag, ptr, 0i32);
        ptr
    }
}
