//! Storage addresses and debug metadata for flattened reference arguments.
use super::*;
use crate::ir::lowered::{LirFunction, LirOperand, LirPlace};
use cranelift_codegen::ir::{InstBuilder, Value, types};
use cranelift_module::Module;

impl<'a, 'b> FuncGen<'a, 'b> {
    pub(super) fn emit_flat_reference_call_end(&mut self) {
        self.emit_debug_reference_call_clear();
        self.lir_reference_scopes
            .pop()
            .expect("prepared reference scope");
    }

    /// Pin every captured owner before computing any interior address. The
    /// owning local may live only in an async frame and thus be movable.
    pub(super) fn emit_flat_call_operands(
        &mut self,
        function: &LirFunction,
        args: &[LirOperand],
    ) -> (Vec<Value>, usize) {
        let before = self.gc_root_count;
        for argument in args {
            if let LirOperand::Reference { place, .. } = argument {
                let owner = match place {
                    LirPlace::Field { object, .. } => Some(self.load_lir_local(function, *object)),
                    LirPlace::ArrayElement { owner, .. } => {
                        Some(self.load_lir_local(function, *owner))
                    }
                    LirPlace::Local(id) => {
                        match self.vars.get(&function.locals[id.0 as usize].name) {
                            Some(VarStorage::Frame { .. }) => self.async_frame,
                            _ => None,
                        }
                    }
                };
                if let Some(owner) = owner {
                    self.emit_push_root(owner);
                }
            }
        }
        let mut values = Vec::with_capacity(args.len());
        for argument in args {
            let value = self.emit_lir_operand(function, argument);
            if !matches!(argument, LirOperand::Reference { .. })
                && is_gc_managed(
                    &argument
                        .ty(&function.locals)
                        .expect("checked call argument"),
                    self.enum_infos,
                )
            {
                self.emit_push_root(value);
            }
            values.push(value);
        }
        (values, self.gc_root_count - before)
    }

    pub(super) fn emit_flat_reference_address(
        &mut self,
        function: &LirFunction,
        place: &LirPlace,
    ) -> Value {
        match place {
            LirPlace::Local(id) => {
                let storage = self
                    .vars
                    .get(&function.locals[id.0 as usize].name)
                    .cloned()
                    .expect("checked local place");
                match storage {
                    VarStorage::Stack { slot, .. } => {
                        let ptr = reference_type(self.module.target_config());
                        self.builder.ins().stack_addr(ptr, slot, 0)
                    }
                    VarStorage::ReferencePtr { var, .. } => self.builder.use_var(var),
                    VarStorage::Frame { offset, .. } => self
                        .builder
                        .ins()
                        .iadd_imm_s(self.async_frame.expect("frame place"), offset as i64),
                    VarStorage::Value { .. } => {
                        panic!("reference local was not prebound to addressable storage")
                    }
                }
            }
            LirPlace::Field {
                object,
                object_ty,
                field,
                ..
            } => {
                let layout = self.lir_class_layout(object_ty);
                let index = layout
                    .iter()
                    .position(|(name, _)| name == field)
                    .expect("checked reference field");
                let owner = self.load_lir_local(function, *object);
                self.builder.ins().iadd_imm_s(
                    owner,
                    (index as i64 + 1)
                        * willow_abi::storage_word_bytes(
                            reference_type(self.module.target_config()).bytes(),
                        ) as i64,
                )
            }
            LirPlace::ArrayElement { owner, index, .. } => {
                let owner = self.load_lir_local(function, *owner);
                let index = self.load_lir_local(function, *index);
                let offset = self.builder.ins().imul_imm_s(index, 8);
                let base = self.builder.ins().iadd_imm_s(owner, 8);
                self.builder.ins().iadd(base, offset)
            }
        }
    }

    pub(super) fn emit_flat_reference_debug(
        &mut self,
        argument: &LirOperand,
        callee: &FunctionId,
        index: usize,
    ) {
        if self.build_mode != BuildMode::Debug {
            return;
        }
        let LirOperand::Reference {
            place,
            span,
            display,
        } = argument
        else {
            return;
        };
        let mut symbol = callee.to_string();
        let mut user_callee = symbol.clone();
        let mut interface = false;
        if let Some(owner) = callee.owner_type() {
            interface = self.interface_infos.contains_key(&owner);
            if interface {
                user_callee = callee.name().to_owned();
            } else if callee.name() == "init" {
                // Constructors are statically selected, including super.init;
                // they have no virtual slot even when descendants define init.
                symbol = class_method_symbol_name(self.known_modules, &owner.to_string(), "init");
                user_callee = symbol.clone();
            } else {
                let plan = self.plan_virtual_call(&owner.to_string(), callee.name());
                symbol = plan.mangled;
                user_callee = if callee.name() == "init" {
                    symbol.clone()
                } else {
                    format!("{}::{}", plan.static_class, callee.name())
                };
            }
        }
        let param = if interface {
            None
        } else {
            self.func_param_debug
                .get(&symbol)
                .and_then(|params| params.get(index))
                .cloned()
        };
        let mode = if interface {
            callee
                .owner_type()
                .and_then(|owner| self.interface_infos.get(&owner))
                .and_then(|info| info.methods.get(callee.name()))
                .and_then(|method| method.param_infos.get(index))
                .map(|param| param.mode.clone())
        } else {
            self.func_param_modes
                .get(&symbol)
                .and_then(|modes| modes.get(index))
                .cloned()
        };
        let mode = param
            .as_ref()
            .map(|p| &p.mode)
            .or(mode.as_ref())
            .map(reference_mode_name)
            .unwrap_or("&")
            .to_owned();
        let param_name = param
            .as_ref()
            .map(|p| p.name.as_str())
            .unwrap_or("<unknown>")
            .to_owned();
        let param_type = param
            .as_ref()
            .map(|p| debug_type_name(&p.ty))
            .unwrap_or_else(|| "<unknown>".into());
        let kind = match place {
            LirPlace::Local(_) => "local",
            LirPlace::Field { .. } => "field",
            LirPlace::ArrayElement { .. } => "array_element",
        };
        let file = self.source_file.to_owned();
        let file = self.emit_string_literal(&file);
        let line = self.builder.ins().iconst(types::I32, span.line as i64);
        let column = self.builder.ins().iconst(types::I32, span.col as i64);
        let callee = self.emit_string_literal(&user_callee);
        let param = self.emit_string_literal(&param_name);
        let ty = self.emit_string_literal(&param_type);
        let mode = self.emit_string_literal(&mode);
        let kind = self.emit_string_literal(kind);
        let display = self.emit_string_literal(display);
        let id = self.func_id("willow_debug_reference_call");
        let function = self.module.declare_func_in_func(id, self.builder.func);
        self.builder.ins().call(
            function,
            &[file, line, column, callee, param, ty, mode, kind, display],
        );
    }
}
