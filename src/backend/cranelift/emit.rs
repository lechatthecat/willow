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
    /// One cold capacity/ownership path per synchronous function. The final
    /// count is the maximum live lexical root depth across all emitted paths,
    /// including cleanup. Loop scopes pop before their next iteration.
    pub(super) fn begin_root_reservation(&mut self) {
        self.root_reservation = Some(self.emit_root_reservation_check());
    }

    fn emit_root_reservation_check(&mut self) -> RootReservation {
        let ty = reference_type(self.module.target_config());
        let count = self.builder.ins().iconst(ty, 0);
        let count_inst = self.builder.func.dfg.value_def(count).inst().unwrap();
        let context = self.builder.ins().get_pinned_reg(ty);
        let top = self.builder.ins().load(
            ty,
            MemFlagsData::trusted(),
            context,
            willow_abi::generated_context::ROOT_TOP_OFFSET,
        );
        let limit = self.builder.ins().load(
            ty,
            MemFlagsData::trusted(),
            context,
            willow_abi::generated_context::ROOT_LIMIT_OFFSET,
        );
        let needed = self.builder.ins().iadd(top, count);
        let fits = self
            .builder
            .ins()
            .icmp(IntCC::UnsignedLessThanOrEqual, needed, limit);
        let reservation = RootReservation {
            count: count_inst,
            check: self.builder.func.dfg.value_def(fits).inst().unwrap(),
            base: self.gc_root_count,
        };
        let cold = self.builder.create_block();
        let ready = self.builder.create_block();
        self.builder.set_cold_block(cold);
        self.builder.ins().brif(fits, ready, &[], cold, &[]);
        self.builder.switch_to_block(cold);
        self.builder.seal_block(cold);
        let id = self.func_id("willow_reserve_roots");
        let callee = self.module.declare_func_in_func(id, self.builder.func);
        self.builder.ins().call(callee, &[count]);
        self.builder.ins().jump(ready, &[]);
        self.builder.switch_to_block(ready);
        self.builder.seal_block(ready);
        reservation
    }

    pub(super) fn finish_root_reservation(&mut self) {
        if let Some(reservation) = self.root_reservation {
            self.patch_root_reservation(reservation, self.root_peak_count);
        }
    }

    /// Panic edges reach a shared cleanup block with a path-dependent live
    /// root depth above the static count, so the function-entry reservation
    /// does not cover pushes made by deferred actions there. Check capacity
    /// again on cleanup entry for the roots the cleanup itself pushes.
    pub(super) fn begin_cleanup_root_reservation(&mut self) -> Option<(RootReservation, usize)> {
        let outer = self.root_reservation?;
        let outer_peak = std::mem::replace(&mut self.root_peak_count, self.gc_root_count);
        self.root_reservation = Some(self.emit_root_reservation_check());
        Some((outer, outer_peak))
    }

    pub(super) fn finish_cleanup_root_reservation(
        &mut self,
        saved: Option<(RootReservation, usize)>,
    ) {
        let Some((outer, outer_peak)) = saved else {
            return;
        };
        let inner = self
            .root_reservation
            .replace(outer)
            .expect("cleanup root reservation");
        self.patch_root_reservation(inner, self.root_peak_count);
        self.root_peak_count = self.root_peak_count.max(outer_peak);
    }

    fn patch_root_reservation(&mut self, reservation: RootReservation, peak: usize) {
        let count = peak.saturating_sub(reservation.base);
        if std::env::var_os("WILLOW_PANIC_DEPTH_LOAD_LOG").is_some() {
            eprintln!("[root-reservation:{count}]");
        }
        if let cranelift_codegen::ir::InstructionData::UnaryImm { imm, .. } =
            &mut self.builder.func.dfg.insts[reservation.count]
        {
            *imm = (count as i64).into();
        } else {
            unreachable!("root reservation constant");
        }
        if count == 0 {
            self.builder.func.dfg.insts[reservation.check] =
                cranelift_codegen::ir::InstructionData::UnaryImm {
                    opcode: cranelift_codegen::ir::Opcode::Iconst,
                    imm: 1.into(),
                };
        }
    }

    /// Root a value that is only passed to the immediately following call.
    /// The callee roots its own copy before its first safepoint, and the
    /// caller does not use `val` after the call, so the referent may move.
    /// No allocation or other GC point may sit between this push and the call.
    pub(super) fn emit_push_call_root(&mut self, val: cranelift_codegen::ir::Value) {
        self.emit_push_relocatable_root(val);
    }

    /// Spill and root `val` in a relocatable slot. A minor collection may move
    /// the referent and rewrite the slot, so after any call that can reach a GC
    /// point the caller must use [`Self::emit_reload_root`] instead of `val`.
    pub(super) fn emit_push_relocatable_root(
        &mut self,
        val: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::StackSlot {
        let spill_bytes = self.builder.func.dfg.value_type(val).bytes();
        // Equal root depth means the earlier temporary has been popped (or
        // belongs to an alternate path). Never share with entry binding roots:
        // those have their own slots and remain included in gc_root_count.
        let key = (self.gc_root_count, spill_bytes);
        let slot = *self.temporary_root_slots.entry(key).or_insert_with(|| {
            self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                spill_bytes,
                0,
            ))
        });
        self.stack_store(val, slot);
        self.emit_push_root_slot(slot);
        slot
    }

    /// Reload a value rooted by [`Self::emit_push_relocatable_root`], with the
    /// type of its original SSA value.
    pub(super) fn emit_reload_root(
        &mut self,
        slot: cranelift_codegen::ir::StackSlot,
        original: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let ty = self.builder.func.dfg.value_type(original);
        self.stack_load(ty, slot)
    }

    /// Register a relocatable root slot. Every use of the binding must reload
    /// it from `slot`: a minor collection may move the referent and rewrite
    /// the slot at any call that can reach a GC point.
    pub(super) fn emit_push_root_slot(&mut self, slot: cranelift_codegen::ir::StackSlot) {
        self.emit_push_root_entry(slot, "willow_push_root");
    }

    fn emit_push_root_entry(&mut self, slot: cranelift_codegen::ir::StackSlot, symbol: &str) {
        let ptr_ty = reference_type(self.module.target_config());
        let addr = self.builder.ins().stack_addr(ptr_ty, slot, 0);
        if self.root_reservation.is_some() {
            let context = self.builder.ins().get_pinned_reg(ptr_ty);
            let flags = MemFlagsData::trusted();
            let top = self.builder.ins().load(
                ptr_ty,
                flags,
                context,
                willow_abi::generated_context::ROOT_TOP_OFFSET,
            );
            let base = self.builder.ins().load(
                ptr_ty,
                flags,
                context,
                willow_abi::generated_context::ROOT_BASE_OFFSET,
            );
            let offset = self.builder.ins().ishl_imm_u(top, 3);
            let entry = self.builder.ins().iadd(base, offset);
            self.builder.ins().store(flags, addr, entry, 0);
            let next = self.builder.ins().iadd_imm_u(top, 1);
            self.builder.ins().store(
                flags,
                next,
                context,
                willow_abi::generated_context::ROOT_TOP_OFFSET,
            );
            if std::env::var_os("WILLOW_PANIC_DEPTH_LOAD_LOG").is_some() {
                eprintln!("[inline-root-push]");
            }
            self.root_peak_count = self.root_peak_count.max(self.gc_root_count + 1);
        } else {
            let push_id = self.func_id(symbol);
            let push_ref = self.module.declare_func_in_func(push_id, self.builder.func);
            self.builder.ins().call(push_ref, &[addr]);
        }
        self.gc_root_count += 1;
    }

    /// Pop `n` GC roots by calling `willow_pop_roots(n)`.
    pub(super) fn emit_pop_roots_n(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let n_val = self.builder.ins().iconst(types::I32, n as i64);
        self.emit_void_runtime_call("willow_pop_roots", &[n_val]);
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
