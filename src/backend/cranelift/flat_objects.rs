//! Value-only lowering for aggregate allocation, access, and initialization.
//! Allocation and stores are separate LIR operations so child evaluation retains
//! source order and every intermediate allocation has a rooted LIR local.
use super::*;
use crate::diagnostics::Span;
use crate::ir::lowered::{LirFunction, LirOperand};
use cranelift_codegen::ir::{InstBuilder, MemFlagsData, Value, types};
use cranelift_module::Module;

impl<'a, 'b> FuncGen<'a, 'b> {
    fn flat_operand_type(function: &LirFunction, operand: &LirOperand) -> Type {
        operand
            .ty(&function.locals)
            .expect("typed language operand")
    }

    pub(super) fn emit_flat_capture_array_owner(
        &mut self,
        function: &LirFunction,
        array: &LirOperand,
        index: &LirOperand,
    ) -> Value {
        let array = self.emit_lir_operand(function, array);
        let index = self.emit_lir_operand(function, index);
        self.emit_value_runtime_call("willow_array_reference_owner", &[array, index])
    }

    pub(super) fn emit_flat_array_alloc(&mut self, len: usize, element_ty: &Type) -> Value {
        let len = self.builder.ins().iconst(types::I64, len as i64);
        let refs = self.builder.ins().iconst(
            types::I64,
            i64::from(super::type_helpers::storage_is_gc_managed(
                element_ty,
                self.enum_infos,
            )),
        );
        self.emit_value_runtime_call("willow_array_new", &[len, refs])
    }

    pub(super) fn emit_flat_array_store(
        &mut self,
        function: &LirFunction,
        array: &LirOperand,
        index: &LirOperand,
        value: &LirOperand,
        element_ty: &Type,
    ) -> Value {
        // Primitive coercions and operand loads cannot allocate or suspend.
        // Load the owner from its current storage and never retain a raw buffer
        // across a call, safepoint, or suspension on the successful path.
        if matches!(element_ty, Type::I64 | Type::F64 | Type::Bool) {
            let array = self.emit_lir_operand(function, array);
            let index = self.emit_lir_operand(function, index);
            let value = self.emit_lir_operand(function, value);
            let word = self.emit_to_storage_word(value, element_ty);
            return self.emit_array_access(array, Some(index), Some(word));
        }
        if self.is_inline_pair(element_ty) {
            let array = self.emit_lir_operand(function, array);
            let index = self.emit_lir_operand(function, index);
            let source_ty = Self::flat_operand_type(function, value);
            let value = self.emit_lir_operand(function, value);
            let value = self.coerce_to_target(value, &source_ty, element_ty);
            let array_root = self.emit_push_relocatable_root(array);
            let existing = self.emit_value_runtime_call("willow_array_get", &[array, index]);
            let update = self.builder.create_block();
            let initialize = self.builder.create_block();
            let done = self.builder.create_block();
            self.builder
                .ins()
                .brif(existing, update, &[], initialize, &[]);
            self.builder.switch_to_block(update);
            self.builder.seal_block(update);
            // Preserve the element storage identity for outstanding references.
            self.emit_gc_heap_store(
                existing,
                0,
                value,
                element_ty,
                GcStoreDestination::InterfaceObject,
            );
            self.builder.ins().jump(done, &[]);
            self.builder.switch_to_block(initialize);
            self.builder.seal_block(initialize);
            let boxed = self.emit_to_storage_word(value, element_ty);
            let array = self.stack_load(reference_type(self.module.target_config()), array_root);
            self.emit_word_array_store(array, index, boxed);
            self.builder.ins().jump(done, &[]);
            self.builder.switch_to_block(done);
            self.builder.seal_block(done);
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
            return self.builder.ins().iconst(types::I64, 0);
        }
        let array = self.emit_lir_operand(function, array);
        let source_ty = Self::flat_operand_type(function, value);
        // A frame-backed local is only an interior edge of the rooted frame.
        // Inline pairs, whose storage word allocates a box, returned above.
        // Neither the coercion (an inline interface pair, willow-9tls.12) nor
        // a non-pair storage word reaches a GC point, so `array` stays valid.
        let index = self.emit_lir_operand(function, index);
        let value = self.emit_lir_operand(function, value);
        let value = self.coerce_to_target(value, &source_ty, element_ty);
        let value = self.emit_to_storage_word(value, element_ty);
        self.emit_word_array_store(array, index, value);
        self.builder.ins().iconst(types::I64, 0)
    }

    /// Inline bounds-checked store of a non-scalar element word
    /// (willow-8hq4.15). Like `willow_array_set`, the buffer's `H_IS_REF`
    /// word selects the barrier, which the shared header-flag filter of
    /// `emit_filtered_reference_store` skips when it is a no-op. Invalid
    /// accesses take the panicking runtime store.
    fn emit_word_array_store(&mut self, array: Value, index: Value, word: Value) {
        use willow_abi::array_layout as layout;
        let ptr_ty = reference_type(self.module.target_config());
        let inspect = self.builder.create_block();
        let access = self.builder.create_block();
        let slow = self.builder.create_block();
        self.builder.set_cold_block(slow);
        let done = self.builder.create_block();

        let null = self.builder.ins().icmp_imm_s(IntCC::Equal, array, 0);
        self.builder.ins().brif(null, slow, &[], inspect, &[]);
        self.builder.switch_to_block(inspect);
        self.builder.seal_block(inspect);
        let len = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            array,
            layout::handle_offset(layout::H_LEN),
        );
        let in_bounds = self.builder.ins().icmp(IntCC::UnsignedLessThan, index, len);
        let nonnegative = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::SignedGreaterThanOrEqual, len, 0);
        let valid = self.builder.ins().band(in_bounds, nonnegative);
        self.builder.ins().brif(valid, access, &[], slow, &[]);

        self.builder.switch_to_block(access);
        self.builder.seal_block(access);
        let buffer = self.builder.ins().load(
            ptr_ty,
            MemFlagsData::new(),
            array,
            layout::handle_offset(layout::H_BUF),
        );
        let offset = self
            .builder
            .ins()
            .imul_imm_s(index, i64::from(layout::WORD_BYTES));
        let offset = if ptr_ty == types::I64 {
            offset
        } else {
            self.builder.ins().ireduce(ptr_ty, offset)
        };
        let slot = self.builder.ins().iadd(buffer, offset);
        let slot = self.builder.ins().iadd_imm_s(
            slot,
            i64::from(layout::BUFFER_HEADER_WORDS as i32 * layout::WORD_BYTES),
        );
        let is_ref = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            array,
            layout::handle_offset(layout::H_IS_REF),
        );
        let is_ref = self.builder.ins().icmp_imm_s(IntCC::NotEqual, is_ref, 0);
        self.emit_filtered_reference_store(
            buffer,
            slot,
            word,
            GcStoreDestination::ArrayElement,
            super::gc_codegen::BarrierRefinement::BufferIsRef(is_ref),
            MemFlagsData::trusted(),
        );
        self.builder.ins().jump(done, &[]);

        self.builder.switch_to_block(slow);
        self.builder.seal_block(slow);
        self.emit_void_runtime_call("willow_array_set", &[array, index, word]);
        self.builder.ins().jump(done, &[]);
        self.builder.switch_to_block(done);
        self.builder.seal_block(done);
    }

    pub(super) fn emit_flat_index(
        &mut self,
        function: &LirFunction,
        array: &LirOperand,
        index: &LirOperand,
        element_ty: &Type,
    ) -> Value {
        let array = self.emit_lir_operand(function, array);
        let index = self.emit_lir_operand(function, index);
        let value = self.emit_array_access(array, Some(index), None);
        self.emit_from_storage_word(value, element_ty)
    }

    /// Inline nonallocating access; retain ABI panic propagation on cold failures.
    /// A missing index requests len; a supplied word requests a scalar store.
    ///
    /// The slow path is entered only for a null array, a negative (malformed)
    /// length or an out-of-bounds index, and the runtime raises for each of
    /// them, so it never rejoins the fast path. The fast path therefore
    /// dominates the code after the access, and a read or scalar store
    /// records the header it validated for the rest of the current GC-free
    /// run (willow-nzsg, willow-ijui.10). The slow path takes its operands as
    /// block parameters and spills them to a frame slot the function's fault
    /// paths share; it reloads them only after the panic-depth snapshot call.
    /// Operands live across that call would otherwise make the register
    /// allocator split the loop's values and leave moves on the fast path.
    pub(super) fn emit_array_access(
        &mut self,
        array: Value,
        index: Option<Value>,
        word: Option<Value>,
    ) -> Value {
        use willow_abi::array_layout as layout;
        // A scalar store leaves the header alone, so reads and stores share
        // the facts of the current GC-free run (willow-ijui.10).
        let known = self
            .lir_reuse
            .as_ref()
            .and_then(|reuse| reuse.arrays.get(&array).copied());
        if let (Some(facts), None) = (known, index) {
            return facts.len;
        }
        let operands: Vec<Value> = std::iter::once(array).chain(index).chain(word).collect();
        let slow = self.builder.create_block();
        self.builder.set_cold_block(slow);
        for &operand in &operands {
            let ty = self.builder.func.dfg.value_type(operand);
            self.builder.append_block_param(slow, ty);
        }
        let slow_args: Vec<_> = operands.iter().map(|&operand| operand.into()).collect();
        let mut facts = match known {
            Some(facts) => facts,
            None => {
                let null = self.builder.ins().icmp_imm_s(IntCC::Equal, array, 0);
                self.emit_array_guard(null, slow, &slow_args);
                let len = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    array,
                    layout::handle_offset(layout::H_LEN),
                );
                let negative = self.builder.ins().icmp_imm_s(IntCC::SignedLessThan, len, 0);
                self.emit_array_guard(negative, slow, &slow_args);
                super::transient_roots::ArrayFacts { len, buffer: None }
            }
        };
        let result = if let Some(index) = index {
            // The length is not negative, so an unsigned comparison also
            // rejects negative indexes.
            let out_of_bounds =
                self.builder
                    .ins()
                    .icmp(IntCC::UnsignedGreaterThanOrEqual, index, facts.len);
            self.emit_array_guard(out_of_bounds, slow, &slow_args);
            let ptr_ty = reference_type(self.module.target_config());
            let buffer = match facts.buffer {
                Some(buffer) => buffer,
                None => self.builder.ins().load(
                    ptr_ty,
                    MemFlagsData::new(),
                    array,
                    layout::handle_offset(layout::H_BUF),
                ),
            };
            facts.buffer = Some(buffer);
            let offset = self
                .builder
                .ins()
                .imul_imm_s(index, i64::from(layout::WORD_BYTES));
            let offset = if ptr_ty == types::I64 {
                offset
            } else {
                self.builder.ins().ireduce(ptr_ty, offset)
            };
            let slot = self.builder.ins().iadd(buffer, offset);
            let header = layout::BUFFER_HEADER_WORDS as i32 * layout::WORD_BYTES;
            if let Some(word) = word {
                self.builder
                    .ins()
                    .store(MemFlagsData::new(), word, slot, header);
                self.builder.ins().iconst(types::I64, 0)
            } else {
                self.builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), slot, header)
            }
        } else {
            facts.len
        };
        if let Some(reuse) = &mut self.lir_reuse {
            reuse.arrays.insert(array, facts);
        }
        // The fast path is `done`'s only predecessor, so `result` dominates it.
        let done = self.builder.create_block();
        self.builder.ins().jump(done, &[]);

        self.builder.switch_to_block(slow);
        self.builder.seal_block(slow);
        let params = self.builder.block_params(slow).to_vec();
        let spill = *self.array_fault_spill.get_or_insert_with(|| {
            self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                3 * 8,
                3,
            ))
        });
        let ptr_ty = reference_type(self.module.target_config());
        for (k, &param) in params.iter().enumerate() {
            self.builder
                .ins()
                .stack_store(ptr_ty, param, spill, 8 * k as i32);
        }
        let name = match params.len() {
            3 => "willow_array_set",
            2 => "willow_array_get",
            1 => "willow_array_len",
            _ => unreachable!("array access takes one to three operands"),
        };
        let reload = move |this: &mut Self| {
            params
                .iter()
                .enumerate()
                .map(|(k, &param)| {
                    let ty = this.builder.func.dfg.value_type(param);
                    this.builder
                        .ins()
                        .stack_load(ptr_ty, ty, spill, 8 * k as i32)
                })
                .collect()
        };
        self.emit_runtime_call_late_args(name, reload, |_| {});
        // The runtime raised, so the panic check above has already diverged.
        if !self.terminated {
            self.builder.ins().trap(TrapCode::unwrap_user(1));
        }
        self.terminated = false;
        self.builder.switch_to_block(done);
        self.builder.seal_block(done);
        result
    }

    /// Leave for `slow` when `failed`, continuing in a fresh fast-path block.
    fn emit_array_guard(
        &mut self,
        failed: Value,
        slow: cranelift_codegen::ir::Block,
        slow_args: &[cranelift_codegen::ir::BlockArg],
    ) {
        let next = self.builder.create_block();
        self.builder.ins().brif(failed, slow, slow_args, next, &[]);
        self.builder.switch_to_block(next);
        self.builder.seal_block(next);
    }

    /// Scalar buffers have no concurrent GC tracer. Update their live length
    /// with ordinary stores; reference buffers retain runtime atomic publication
    /// and barriers. No raw buffer survives the allocating slow path.
    pub(super) fn emit_scalar_array_push(&mut self, array: Value, word: Value) -> Value {
        use willow_abi::array_layout as layout;
        let inspect = self.builder.create_block();
        let fast = self.builder.create_block();
        let slow = self.builder.create_block();
        self.builder.set_cold_block(slow);
        let done = self.builder.create_block();
        let null = self.builder.ins().icmp_imm_s(IntCC::Equal, array, 0);
        self.builder.ins().brif(null, slow, &[], inspect, &[]);
        self.builder.switch_to_block(inspect);
        self.builder.seal_block(inspect);
        let len = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            array,
            layout::handle_offset(layout::H_LEN),
        );
        let cap = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            array,
            layout::handle_offset(layout::H_CAP),
        );
        let available = self.builder.ins().icmp(IntCC::UnsignedLessThan, len, cap);
        self.builder.ins().brif(available, fast, &[], slow, &[]);
        self.builder.switch_to_block(fast);
        self.builder.seal_block(fast);
        let ptr_ty = reference_type(self.module.target_config());
        let buffer = self.builder.ins().load(
            ptr_ty,
            MemFlagsData::new(),
            array,
            layout::handle_offset(layout::H_BUF),
        );
        let offset = self
            .builder
            .ins()
            .imul_imm_s(len, i64::from(layout::WORD_BYTES));
        let offset = if ptr_ty == types::I64 {
            offset
        } else {
            self.builder.ins().ireduce(ptr_ty, offset)
        };
        let slot = self.builder.ins().iadd(buffer, offset);
        self.builder.ins().store(
            MemFlagsData::new(),
            word,
            slot,
            layout::BUFFER_HEADER_WORDS as i32 * layout::WORD_BYTES,
        );
        let next_len = self.builder.ins().iadd_imm_s(len, 1);
        self.builder
            .ins()
            .store(MemFlagsData::new(), next_len, buffer, 0);
        self.builder.ins().store(
            MemFlagsData::new(),
            next_len,
            array,
            layout::handle_offset(layout::H_LEN),
        );
        self.builder.ins().jump(done, &[]);

        self.builder.switch_to_block(slow);
        self.builder.seal_block(slow);
        // Also root frame-backed SSA receivers while runtime growth allocates;
        // nothing uses `array` after the call.
        self.emit_push_call_root(array);
        self.emit_void_runtime_call("willow_array_push", &[array, word]);
        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;
        self.builder.ins().jump(done, &[]);
        self.builder.switch_to_block(done);
        self.builder.seal_block(done);
        self.builder.ins().iconst(types::I8, 0)
    }

    pub(super) fn emit_flat_object_alloc(&mut self, class: &TypeId) -> Value {
        let layout = self
            .classes
            .object_layout(class, reference_type(self.module.target_config()).bytes())
            .expect("checked class layout");
        let type_id = self
            .classes
            .type_id(class)
            .expect("checked class runtime type id");
        let name = class.to_string();
        let ptr = self.emit_gc_alloc(GcLayoutMetadata::class(type_id, &layout, self.enum_infos));
        self.emit_store_class_descriptor(ptr, &name);
        ptr
    }

    pub(super) fn emit_flat_field(
        &mut self,
        function: &LirFunction,
        object: &LirOperand,
        field: &str,
        object_ty: &Type,
    ) -> Value {
        let object = self.emit_lir_operand(function, object);
        if matches!(object_ty, Type::Generic(name, args) if *name == TypeId::local("Range") && args == &[Type::I64])
        {
            return self.builder.ins().load(
                types::I64,
                MemFlagsData::new(),
                object,
                if field == "end" { 8 } else { 0 },
            );
        }
        let layout = self.lir_class_layout(object_ty);
        let (offset, field_ty) = layout.field(field).expect("checked field");
        self.builder.ins().load(
            self.classes
                .clif_type(reference_type(self.module.target_config()), field_ty),
            MemFlagsData::new(),
            object,
            offset as i32,
        )
    }

    pub(super) fn emit_flat_field_store(
        &mut self,
        function: &LirFunction,
        object: &LirOperand,
        field: &str,
        value: &LirOperand,
        object_ty: &Type,
    ) -> Value {
        let layout = self.lir_class_layout(object_ty);
        let (offset, target_ty) = layout.field(field).expect("checked field");
        let object = self.emit_lir_operand(function, object);
        let source_ty = Self::flat_operand_type(function, value);
        // Neither the coercion (an inline interface pair, willow-9tls.12) nor
        // the store and its barrier reach a safepoint, so `object` stays
        // valid without a root (willow-8hq4.16).
        let value = self.emit_lir_operand(function, value);
        let value = self.coerce_to_target(value, &source_ty, target_ty);
        self.emit_gc_heap_store(
            object,
            offset as i32,
            value,
            target_ty,
            GcStoreDestination::ObjectField,
        );
        self.builder.ins().iconst(types::I64, 0)
    }

    pub(super) fn emit_flat_static_field(&mut self, class: &str, field: &str) -> Value {
        let class = self.static_call_class_name(class);
        let info = self
            .lookup_static_storage(&class, field)
            .expect("checked static property");
        let ptr_ty = reference_type(self.module.target_config());
        let global = self
            .module
            .declare_data_in_func(info.data_id, self.builder.func);
        let address = self.builder.ins().symbol_value(ptr_ty, global);
        self.builder.ins().load(
            self.classes
                .clif_type(reference_type(self.module.target_config()), &info.ty),
            MemFlagsData::new(),
            address,
            0,
        )
    }

    pub(super) fn emit_flat_static_store(
        &mut self,
        function: &LirFunction,
        class: &str,
        field: &str,
        value: &LirOperand,
    ) -> Value {
        let class = self.static_call_class_name(class);
        let info = self
            .lookup_static_storage(&class, field)
            .expect("checked static property");
        let source_ty = Self::flat_operand_type(function, value);
        let value = self.emit_lir_operand(function, value);
        let value = self.coerce_to_target(value, &source_ty, &info.ty);
        let ptr_ty = reference_type(self.module.target_config());
        let global = self
            .module
            .declare_data_in_func(info.data_id, self.builder.func);
        let address = self.builder.ins().symbol_value(ptr_ty, global);
        self.emit_gc_heap_store(
            address,
            0,
            value,
            &info.ty,
            GcStoreDestination::GlobalStatic,
        );
        self.builder.ins().iconst(types::I64, 0)
    }

    pub(super) fn emit_flat_constructor_call(
        &mut self,
        function: &LirFunction,
        object: &LirOperand,
        class: &TypeId,
        args: &[LirOperand],
        arg_types: &[Type],
        span: Span,
    ) -> Value {
        let ptr = self.emit_lir_operand(function, object);
        // Only the `init` call below consumes `ptr`. Operand evaluation and
        // coercions in between reach no GC point: `coerce_to_target` builds
        // inline interface pairs without allocating (willow-9tls.12).
        self.emit_push_call_root(ptr);
        let mangled = class_method_symbol_name(self.known_modules, &class.to_string(), "init");
        let init_fid = self
            .func_ids
            .get(&mangled)
            .copied()
            .expect("explicit constructor call has an init method");
        let params = self.method_param_types(&mangled);
        let mut call_args = vec![ptr];
        let (values, argument_roots) = self.emit_flat_call_operands(function, args);
        let mut roots = 1 + argument_roots;
        for (index, (operand, value)) in args.iter().zip(values).enumerate() {
            let source_ty = &arg_types[index];
            let target_ty = params
                .as_ref()
                .and_then(|params| params.get(index))
                .unwrap_or(source_ty);
            let reference = matches!(operand, LirOperand::Reference { .. });
            let value = if reference {
                value
            } else {
                self.coerce_to_target(value, source_ty, target_ty)
            };
            // Coerced values are new SSA values not held by the source locals;
            // root them for the constructor, which re-roots its parameters.
            if !reference && is_gc_managed(target_ty, self.enum_infos) {
                self.emit_push_call_root(value);
                roots += 1;
            }
            call_args.push(value);
        }
        let init_ref = self
            .module
            .declare_func_in_func(init_fid, self.builder.func);
        let pushed = self.emit_callstack_push("init", span);
        let panic_depth = self.emit_pre_user_call_panic_depth(&mangled);
        self.builder.ins().call(init_ref, &call_args);
        if pushed {
            self.emit_callstack_pop();
        }
        if args
            .iter()
            .any(|arg| matches!(arg, LirOperand::Reference { .. }))
        {
            self.emit_flat_reference_call_end();
        }
        if roots > 0 {
            self.emit_pop_roots_n(roots);
            self.gc_root_count -= roots;
        }
        self.emit_post_willow_call_panic_check(panic_depth);
        self.builder.ins().iconst(types::I64, 0)
    }
}
