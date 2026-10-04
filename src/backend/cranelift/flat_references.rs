//! Storage addresses and debug metadata for flattened reference arguments.
use super::*;
use crate::ir::lowered::{LirFunction, LirOperand, LirPlace};
use cranelift_codegen::ir::{InstBuilder, StackSlotData, StackSlotKind, Value, types};
use cranelift_module::Module;

impl<'a, 'b> FuncGen<'a, 'b> {
    pub(super) fn emit_flat_reference_call_end(&mut self) {
        self.emit_debug_reference_call_clear();
        self.lir_reference_scopes
            .pop()
            .expect("prepared reference scope");
    }

    /// Pass every reference argument as a pointer to a caller-owned
    /// [`REFERENCE_CELL_BYTES`] cell `{base, offset}` (willow-9tls.9).
    ///
    /// `base` is the managed allocation that owns the referenced storage, or
    /// null for a native stack slot; `offset` is the byte offset of the storage
    /// inside `base` (the absolute address when `base` is null). A heap base is
    /// registered as an ordinary relocatable root slot for the whole call, so a
    /// moving collection rewrites it in place and the callee re-derives the
    /// interior address from the cell at every access. Nothing pins the owner.
    /// A callee forwarding its own reference parameter passes the cell it
    /// received, whose base stays rooted by the frame that built it.
    ///
    /// Plain managed arguments get relocatable bridge roots: every caller
    /// passes the values straight to a call or runtime call without an
    /// intervening GC point, and a callee roots its own parameters. A caller
    /// that allocates before using the values (heap enum construction) roots
    /// and reloads them itself.
    pub(super) fn emit_flat_call_operands(
        &mut self,
        function: &LirFunction,
        args: &[LirOperand],
    ) -> (Vec<Value>, usize) {
        let before = self.gc_root_count;
        let mut values = Vec::with_capacity(args.len());
        for argument in args {
            let value = if let LirOperand::Reference { place, .. } = argument {
                self.emit_flat_reference_cell(function, place)
            } else {
                let value = self.emit_lir_operand(function, argument);
                if is_gc_managed(
                    &argument
                        .ty(&function.locals)
                        .expect("checked call argument"),
                    self.enum_infos,
                ) {
                    self.emit_push_call_root(value);
                }
                value
            };
            values.push(value);
        }
        (values, self.gc_root_count - before)
    }

    /// Build the reference cell for `place` and return its address. A managed
    /// base is rooted through the cell's base word.
    fn emit_flat_reference_cell(&mut self, function: &LirFunction, place: &LirPlace) -> Value {
        let ptr = reference_type(self.module.target_config());
        let (base, offset) = match place {
            LirPlace::Local(id) => {
                let storage = self
                    .vars
                    .get(&function.locals[id.0 as usize].name)
                    .cloned()
                    .expect("checked local place");
                match storage {
                    VarStorage::Stack { slot, .. } => {
                        let address = self.builder.ins().stack_addr(ptr, slot, 0);
                        (None, address)
                    }
                    // Forward the caller's cell; its base is rooted there.
                    VarStorage::ReferencePtr { var, .. } => return self.builder.use_var(var),
                    // Async frames are runtime allocations the function keeps
                    // using through its stable `async_frame` value; root the
                    // frame like any other base so the cell has one contract.
                    VarStorage::Frame { offset, .. } => {
                        let frame = self.async_frame.expect("frame place");
                        let offset = self.builder.ins().iconst(ptr, i64::from(offset));
                        (Some(frame), offset)
                    }
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
                let (offset, _) = self
                    .lir_class_layout(object_ty)
                    .field(field)
                    .expect("checked reference field");
                let owner = self.load_lir_local(function, *object);
                let offset = self.builder.ins().iconst(ptr, offset);
                (Some(owner), offset)
            }
            LirPlace::ArrayElement {
                owner,
                index,
                element,
                ..
            } => {
                let owner = self.load_lir_local(function, *owner);
                let index = self.load_lir_local(function, *index);
                let offset = self.builder.ins().imul_imm_s(index, 8);
                let offset = self.builder.ins().iadd_imm_s(offset, 8);
                if self.is_inline_pair(element) {
                    // Pop clears the pair's bits, not this pointer; no-growth
                    // push overwrites the same box. The box itself is the
                    // referenced storage and becomes the cell's base.
                    let address = self.builder.ins().iadd(owner, offset);
                    let pair = self
                        .builder
                        .ins()
                        .load(ptr, MemFlagsData::new(), address, 0);
                    let zero = self.builder.ins().iconst(ptr, 0);
                    (Some(pair), zero)
                } else {
                    // The buffer traces its high-water prefix, so the slot
                    // stays traced through the rooted base even after a pop.
                    (Some(owner), offset)
                }
            }
        };
        let cell = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            REFERENCE_CELL_BYTES,
            3,
        ));
        let base_value = base.unwrap_or_else(|| self.builder.ins().iconst(ptr, 0));
        self.builder.ins().stack_store(ptr, base_value, cell, 0);
        self.builder.ins().stack_store(ptr, offset, cell, 8);
        if base.is_some() {
            self.emit_push_root_slot(cell);
        }
        self.builder.ins().stack_addr(ptr, cell, 0)
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
            interface = self.classes.is_interface(&owner);
            if interface {
                user_callee = callee.name().to_string();
            } else if callee.name().as_ref() == "init" {
                // Constructors are statically selected, including super.init;
                // they have no virtual slot even when descendants define init.
                symbol = class_method_symbol_name(self.known_modules, &owner.to_string(), "init");
                user_callee = symbol.clone();
            } else {
                let plan = self.plan_virtual_call(&owner.to_string(), callee.name().as_ref());
                symbol = plan.mangled.clone();
                user_callee = if callee.name().as_ref() == "init" {
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
                .and_then(|owner| self.classes.interface(&owner))
                .and_then(|info| {
                    let method = info.methods.get(callee.name().as_ref())?;
                    Some(method.param_infos.get(index)?.mode.clone())
                })
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
