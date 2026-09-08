use super::emit_interface::collection_elem_kind;
use super::*;
use cranelift_codegen::ir::{InstBuilder, types};

impl<'a, 'b> FuncGen<'a, 'b> {
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
        let value_is_ref = self
            .builder
            .ins()
            .iconst(types::I64, i64::from(is_gc_managed(value, self.enum_infos)));
        self.emit_value_runtime_call("willow_map_new", &[key_kind, value_kind, value_is_ref])
    }
}
