//! Expression and statement codegen for the Cranelift backend (the `emit_*`
//! methods, extracted from `mod.rs`). `pub(super)` so the codegen driver can
//! call them; as a child module these reach FuncGen's private fields/methods.

use cranelift_codegen::ir::{InstBuilder, StackSlotData, StackSlotKind, types};
use cranelift_module::Module;

use super::*;

/// The vtable data symbol for boxing `class_name` into `interface_name`, or
/// `None` when no such vtable was registered.
///
/// The vtable is keyed by the registered (canonical) interface name. A
/// directly-imported interface alias (`import mod::Iface` -> bare `Iface`)
/// names the box site with the local alias, so canonicalize before the lookup;
/// otherwise the box silently falls back to the raw object and dispatch
/// crashes (willow-64gs.1).
///
/// A free function rather than a `FuncGen` method so LIR eligibility can ask
/// the same question before emission (willow-j260): the walker may only admit
/// a class → interface coercion that [`FuncGen::emit_interface_box`] can
/// actually build, and the two must use the same scoped type identities.
pub(super) fn resolve_vtable_id(
    vtable_ids: &VtableMap<DataId>,
    classes: &ClassView<'_>,
    class_name: &str,
    interface_name: &str,
) -> Option<DataId> {
    let canonical_iface = classes
        .interface(interface_name)
        .map(|i| i.name)
        .unwrap_or_else(|| interface_name.to_string().into());
    vtable_ids
        .get(&(TypeId::from_source_name(class_name), canonical_iface))
        .or_else(|| {
            vtable_ids.get(&(
                TypeId::from_source_name(class_name),
                TypeId::from_source_name(interface_name),
            ))
        })
        .copied()
}

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Spill a GC pointer or an inline interface pair, rooting only word zero.
    /// Scalar ADT pairs must not be passed here: their first word is a tag.
    ///
    /// The slot is returned so a caller that roots a temporary across a call
    /// which may collect can reload the pointer from the root afterwards.
    pub(super) fn emit_push_root(
        &mut self,
        val: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::StackSlot {
        let spill_bytes = self.builder.func.dfg.value_type(val).bytes();
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            spill_bytes,
            0,
        ));
        self.stack_store(val, slot);
        self.emit_push_root_slot(slot);
        slot
    }

    pub(super) fn emit_push_root_slot(&mut self, slot: cranelift_codegen::ir::StackSlot) {
        let ptr_ty = reference_type(self.module.target_config());
        let addr = self.builder.ins().stack_addr(ptr_ty, slot, 0);
        let push_id = self.func_id("willow_push_root");
        let push_ref = self.module.declare_func_in_func(push_id, self.builder.func);
        self.builder.ins().call(push_ref, &[addr]);
        self.gc_root_count += 1;
    }

    /// Pop `n` GC roots by calling `willow_pop_roots(n)`.
    pub(super) fn emit_pop_roots_n(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let pop_id = self.func_id("willow_pop_roots");
        let pop_ref = self.module.declare_func_in_func(pop_id, self.builder.func);
        let n_val = self.builder.ins().iconst(types::I32, n as i64);
        self.builder.ins().call(pop_ref, &[n_val]);
    }

    /// Form an inline `[object (GC ref) | vtable (raw)]` interface value.
    /// The pair requires no wrapper allocation; only its object word is traced.
    pub(super) fn emit_interface_box(
        &mut self,
        object: cranelift_codegen::ir::Value,
        class_name: &str,
        interface_name: &str,
    ) -> cranelift_codegen::ir::Value {
        let vtable_id =
            resolve_vtable_id(self.vtable_ids, &self.classes, class_name, interface_name);
        let ptr_ty = reference_type(self.module.target_config());
        let vtable_ptr = if let Some(vtable_id) = vtable_id {
            let gv = self
                .module
                .declare_data_in_func(vtable_id, self.builder.func);
            self.builder.ins().symbol_value(ptr_ty, gv)
        } else {
            // The missing implementation has already been diagnosed. Preserve
            // the pair representation even on this recovery path.
            self.builder.ins().iconst(ptr_ty, 0)
        };
        self.builder.ins().iconcat(object, vtable_ptr)
    }
}

#[cfg(test)]
mod vtable_resolution_tests {
    use super::*;

    #[test]
    fn missing_qualified_interface_never_matches_another_modules_short_name() {
        let class = TypeId::from_source_name("Concrete");
        for count in [1, 16, 256, 4096] {
            let mut tables = VtableMap::from([(
                (class, TypeId::from_source_name("one::View")),
                DataId::from_u32(0),
            )]);
            for index in 1..count {
                tables.insert(
                    (
                        class,
                        TypeId::from_source_name(&format!("other{index}::View")),
                    ),
                    DataId::from_u32(index as u32),
                );
            }
            // No interface is registered, so every name stands for itself.
            let scope = type_index::TypeScope::default();
            let layouts = crate::compiler_db::layout::LayoutQueries::default();
            let classes = ClassView::new(&scope, &layouts);
            assert_eq!(
                resolve_vtable_id(&tables, &classes, "Concrete", "one::View"),
                Some(DataId::from_u32(0))
            );
            assert_eq!(
                resolve_vtable_id(&tables, &classes, "Concrete", "two::View"),
                None
            );
            assert_eq!(
                resolve_vtable_id(&tables, &classes, "Concrete", "View"),
                None
            );
        }
    }
}
