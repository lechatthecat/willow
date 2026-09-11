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
            i64::from(is_gc_managed(element_ty, self.enum_infos)),
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
        let array = self.emit_lir_operand(function, array);
        // A frame-backed local is only an interior edge of the rooted frame.
        // Pin its loaded SSA value across allocating interface coercions.
        self.emit_push_root(array);
        let index = self.emit_lir_operand(function, index);
        let source_ty = Self::flat_operand_type(function, value);
        let value = self.emit_lir_operand(function, value);
        let value = self.coerce_to_target(value, &source_ty, element_ty);
        let value = self.coerce_to_i64(value, element_ty);
        self.emit_void_runtime_call("willow_array_set", &[array, index, value]);
        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;
        self.builder.ins().iconst(types::I64, 0)
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
        let value = self
            .emit_runtime_call_with_cleanup("willow_array_get", &[array, index], |_| {})
            .expect("array getter returns a value");
        self.coerce_i64_to(value, element_ty)
    }

    pub(super) fn emit_flat_object_alloc(&mut self, class: &TypeId) -> Value {
        let layout = self
            .class_layouts
            .get(class)
            .cloned()
            .expect("checked class layout");
        let type_id = self
            .class_type_ids
            .get(class)
            .copied()
            .expect("checked class runtime type id");
        let name = class.to_string();
        let ptr = self.emit_gc_alloc(GcLayoutMetadata::class(
            &name,
            type_id,
            &layout,
            self.enum_infos,
        ));
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
        let index = layout
            .iter()
            .position(|(name, _)| name == field)
            .expect("checked field");
        self.builder.ins().load(
            clif_type(&layout[index].1),
            MemFlagsData::new(),
            object,
            (index as i32 + 1) * 8,
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
        let index = layout
            .iter()
            .position(|(name, _)| name == field)
            .expect("checked field");
        let target_ty = &layout[index].1;
        let object = self.emit_lir_operand(function, object);
        self.emit_push_root(object);
        let source_ty = Self::flat_operand_type(function, value);
        let value = self.emit_lir_operand(function, value);
        let value = self.coerce_to_target(value, &source_ty, target_ty);
        self.emit_gc_heap_store(
            object,
            (index as i32 + 1) * 8,
            value,
            target_ty,
            GcStoreDestination::ObjectField,
        );
        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;
        self.builder.ins().iconst(types::I64, 0)
    }

    pub(super) fn emit_flat_static_field(&mut self, class: &str, field: &str) -> Value {
        let class = self.static_call_class_name(class);
        let info = self
            .lookup_static_storage(&class, field)
            .expect("checked static property");
        let ptr_ty = self.module.target_config().pointer_type();
        let global = self
            .module
            .declare_data_in_func(info.data_id, self.builder.func);
        let address = self.builder.ins().symbol_value(ptr_ty, global);
        self.builder
            .ins()
            .load(clif_type(&info.ty), MemFlagsData::new(), address, 0)
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
        let ptr_ty = self.module.target_config().pointer_type();
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
        self.emit_push_root(ptr);
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
            // Interface coercions can allocate new boxes not held by the source
            // locals; retain them across later coercions and the constructor.
            if !reference && is_gc_managed(target_ty, self.enum_infos) {
                self.emit_push_root(value);
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
