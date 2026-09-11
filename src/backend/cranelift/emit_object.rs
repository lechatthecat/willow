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
