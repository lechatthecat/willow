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
/// actually build, and the two must agree on every aliasing fallback below.
pub(super) fn resolve_vtable_id(
    vtable_ids: &VtableMap<DataId>,
    interface_infos: &TypeMap<InterfaceInfo>,
    class_name: &str,
    interface_name: &str,
) -> Option<DataId> {
    let canonical_iface = interface_infos
        .get(interface_name)
        .map(|i| i.name.clone())
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
        .or_else(|| {
            // The box site may name a module-local generic interface by its
            // bare name (`Box`) while its vtable is keyed by the qualified
            // name (`mod::Box`). Fall back to the class's unique vtable whose
            // interface short name (last `::` segment) matches (willow-1js.5).
            let short = interface_name.rsplit("::").next().unwrap_or(interface_name);
            let mut found: Option<DataId> = None;
            for (key, id) in vtable_ids.iter() {
                let (cls, iface) = (&key.0, &key.1);
                if cls == &TypeId::from_source_name(class_name) && iface.name() == short {
                    if found.is_some() {
                        return None; // ambiguous: more than one match
                    }
                    found = Some(*id);
                }
            }
            found
        })
}

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Push a GC root for a pointer value. Creates a stack slot to hold the pointer so
    /// the GC can find and mark the object via `willow_push_root`.
    ///
    /// The slot is returned so a caller that roots a temporary across a call
    /// which may collect can reload the pointer from the root afterwards.
    pub(super) fn emit_push_root(
        &mut self,
        val: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::StackSlot {
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            0,
        ));
        self.stack_store(val, slot);
        self.emit_push_root_slot(slot);
        slot
    }

    pub(super) fn emit_push_root_slot(&mut self, slot: cranelift_codegen::ir::StackSlot) {
        let ptr_ty = self.module.target_config().pointer_type();
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

    /// Box a concrete class instance into an interface value: a 16-byte GC object
    /// `[object (GC ref) | vtable (raw)]` allocated with `gc_ref_mask = 0b01`.
    /// Returns the box pointer (spec §8.1 / §9.2).
    pub(super) fn emit_interface_box(
        &mut self,
        object: cranelift_codegen::ir::Value,
        class_name: &str,
        interface_name: &str,
    ) -> cranelift_codegen::ir::Value {
        let vtable_id = resolve_vtable_id(
            self.vtable_ids,
            self.interface_infos,
            class_name,
            interface_name,
        );
        let Some(vtable_id) = vtable_id else {
            // No vtable registered (e.g. unknown interface already diagnosed):
            // fall back to the raw object so codegen stays total.
            return object;
        };

        // Root the object across the box allocation (the alloc may collect).
        self.emit_push_root(object);
        let box_ptr = self.emit_gc_alloc(GcLayoutMetadata::new(
            GcObjectKind::InterfaceBox,
            16,
            0,
            0b01,
        ));

        // word 0: concrete object pointer (GC-traced). Direct roots are
        // pinned/promoted, so `object` remains valid across the allocation.
        self.emit_gc_heap_store_classified(
            box_ptr,
            0,
            object,
            true,
            GcStoreDestination::InterfaceObject,
        );

        // word 1: vtable address (a static data symbol; not a GC reference).
        let gv = self
            .module
            .declare_data_in_func(vtable_id, self.builder.func);
        let ptr_ty = self.module.target_config().pointer_type();
        let vtable_ptr = self.builder.ins().symbol_value(ptr_ty, gv);
        self.emit_gc_heap_store_classified(
            box_ptr,
            8,
            vtable_ptr,
            false,
            GcStoreDestination::InterfaceObject,
        );

        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;
        box_ptr
    }
}
