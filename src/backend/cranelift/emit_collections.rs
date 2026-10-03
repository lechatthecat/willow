use super::emit_interface::collection_elem_kind;
use super::*;
use cranelift_codegen::ir::{InstBuilder, types};

impl<'a, 'b> FuncGen<'a, 'b> {
    pub(super) fn emit_map_get_value(
        &mut self,
        receiver: cranelift_codegen::ir::Value,
        key: cranelift_codegen::ir::Value,
        key_ref: cranelift_codegen::ir::Value,
        value_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            0,
        ));
        let ptr_ty = reference_type(self.module.target_config());
        let address = self.builder.ins().stack_addr(ptr_ty, slot, 0);
        let tag =
            self.emit_value_runtime_call("willow_map_get_into", &[receiver, key, key_ref, address]);
        let payload = self.stack_load(types::I64, slot);
        let option = Type::Generic("Option".into(), vec![value_ty.clone()]);
        if super::option_repr::is_scalar_pair(&option) {
            return self.emit_pair(tag, payload);
        }
        let result = self.builder.declare_var(clif_type(ptr_ty, &option));
        let some = self.builder.create_block();
        let none = self.builder.create_block();
        let done = self.builder.create_block();
        self.builder.ins().brif(tag, none, &[], some, &[]);
        self.builder.switch_to_block(some);
        self.builder.seal_block(some);
        let payload = self.emit_from_storage_word(payload, value_ty);
        let value = self.emit_alloc_option_some(value_ty, payload);
        self.builder.def_var(result, value);
        self.builder.ins().jump(done, &[]);
        self.builder.switch_to_block(none);
        self.builder.seal_block(none);
        let value = self.emit_alloc_option_none(value_ty);
        self.builder.def_var(result, value);
        self.builder.ins().jump(done, &[]);
        self.builder.switch_to_block(done);
        self.builder.seal_block(done);
        self.builder.use_var(result)
    }

    /// Allocate a map with immutable layout and GC metadata from its checked type.
    pub(super) fn emit_map_new(
        &mut self,
        key: &Type,
        value: &Type,
    ) -> cranelift_codegen::ir::Value {
        let key_kind = self
            .builder
            .ins()
            .iconst(types::I64, collection_elem_kind(key).unwrap_or(4));
        let value_kind = self
            .builder
            .ins()
            .iconst(types::I64, collection_elem_kind(value).unwrap_or(4));
        let value_is_ref = self.builder.ins().iconst(
            types::I64,
            i64::from(super::type_helpers::storage_is_gc_managed(
                value,
                self.enum_infos,
            )),
        );
        let constructor = if self.lir_confined_maps {
            "willow_map_new_local"
        } else {
            "willow_map_new"
        };
        self.emit_value_runtime_call(constructor, &[key_kind, value_kind, value_is_ref])
    }
}
