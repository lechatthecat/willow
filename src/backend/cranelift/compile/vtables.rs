use super::*;

impl UnitCodegenContext<'_> {
    /// Emit a static vtable per `(class, implemented-interface)` pair. Each
    /// vtable is `slot_count` function pointers in the interface's declaration
    /// (method) order; slot K points at the concrete method the class provides
    /// (found in the class itself or an ancestor). See spec §8.2 / §9.5.
    pub(in crate::backend::cranelift) fn declare_interface_vtables(
        &mut self,
        program: &Program,
    ) -> Result<()> {
        let classes: Vec<ClassDecl> = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Class(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        self.declare_vtables_for_classes(&classes)
    }

    /// Emit `(class, interface)` vtables for the given (already module-qualified)
    /// class declarations. Used for both the entry program and imported modules.
    pub(in crate::backend::cranelift) fn declare_vtables_for_classes(
        &mut self,
        classes: &[ClassDecl],
    ) -> Result<()> {
        for c in classes {
            for iface_ty in &c.implements {
                // The vtable layout (method slots) is keyed by the interface name;
                // generic type arguments do not change the class's method func ids.
                // A class may implement several instantiations of one generic
                // interface (`Container<i64>`, `Container<String>`): every slot
                // points to a monomorphic class method (default-method bodies are
                // injected once as class methods too), so all instantiations yield
                // a byte-identical vtable and correctly share this single name-keyed
                // entry — `declare_one_vtable` dedups them.
                let iface_name = match iface_ty {
                    crate::parser::ast::Type::Named(n)
                    | crate::parser::ast::Type::Generic(n, _) => n.clone(),
                    _ => continue,
                };
                let Some(iface) = self.classes().interface(&iface_name) else {
                    continue; // unknown interface already reported by the type checker
                };
                self.declare_one_vtable(&c.name, &iface, c.span)?;
            }
        }
        Ok(())
    }

    pub(in crate::backend::cranelift) fn declare_one_vtable(
        &mut self,
        class_name: &str,
        iface: &InterfaceInfo,
        span: crate::diagnostics::Span,
    ) -> Result<()> {
        let mut pending = vec![iface.name];
        let mut definitions = Vec::new();
        // Declare every reachable symbol before writing references. Explicit
        // worklists keep deep inheritance independent of the compiler stack.
        while let Some(name) = pending.pop() {
            let canonical = self
                .classes()
                .interface(&name)
                .map(|info| info.name)
                .unwrap_or(name);
            let key = (TypeId::from_source_name(class_name), canonical);
            if self.vtable_ids.contains_key(&key) {
                continue;
            }
            let info = self
                .classes()
                .interface(&canonical)
                .unwrap_or_else(|| std::sync::Arc::new(iface.clone()));
            let symbol = vtable_symbol(class_name, &info.name.to_string());
            self.claim_symbol(
                &symbol,
                format!("interface implementation `{class_name}: {}`", info.name),
                span,
            )?;
            let id = self
                .output
                .module
                .declare_data(&symbol, Linkage::Local, false, false)?;
            self.vtable_ids.insert(key, id);
            for sup in &info.extends {
                if let Some(super_info) = self.classes().interface(sup) {
                    pending.push(super_info.name);
                }
            }
            definitions.push(info);
        }
        for info in definitions {
            self.define_one_vtable(class_name, &info, span)?;
        }
        Ok(())
    }

    pub(super) fn define_one_vtable(
        &mut self,
        class_name: &str,
        iface: &InterfaceInfo,
        span: crate::diagnostics::Span,
    ) -> Result<()> {
        let key = (TypeId::from_source_name(class_name), iface.name);
        let data_id = self.vtable_ids[&key];
        let slots = crate::backend::cranelift::vtable_layout::slots(&self.classes(), &iface.name);
        let method_words = slots.len().max(1);
        let slot_count = method_words + iface.extends.len();
        let mut data = DataDescription::new();
        // Explicit zeroed bytes (not `define_zeroinit`, which is BSS and cannot
        // carry the function-address relocations written below).
        let pointer_bytes = reference_type(self.output.module.target_config()).bytes();
        data.set_align(pointer_bytes as u64);
        data.define(
            vec![
                0u8;
                willow_abi::dispatch_layout::table_slot_offset(slot_count as u32, pointer_bytes)
                    as usize
            ]
            .into_boxed_slice(),
        );
        // Filled BY NAME, which is what lets one method occupy several slots
        // (a diamond's shared grandparent) without the copies disagreeing.
        for (slot, method_name) in slots.iter().enumerate() {
            let symbol = self.resolve_class_method_symbol(class_name, method_name);
            let func_id = symbol
                .as_deref()
                .and_then(|symbol| self.func_ids.get(symbol).copied());
            // An `open`/`override` method must not be nailed to the body THIS
            // class would inherit: the box may have been built from a
            // base-typed expression whose object is really a subclass, and the
            // subclass's override has to win. Its slot therefore
            // holds a thunk that re-dispatches through the receiver's own class
            // descriptor. A method with no virtual slot is neither `open` nor
            // an `override`, so no subclass can replace it and the direct
            // address is both correct and cheaper.
            //
            // Slotted methods take the thunk unconditionally rather than
            // devirtualizing a lone implementation the way `plan_virtual_call`
            // does: vtables are laid out per UNIT, and a class declared by a
            // later unit — the entry file, typically — can still contribute the
            // override that makes the count two.
            let vslot = self
                .classes()
                .slots(class_name)
                .and_then(|slots| slots.slot_of(method_name));
            // Keep the static class for a thunk. Body-time closed-world
            // analysis can resolve it only after every unit is declared.
            let target = match (&symbol, func_id, vslot) {
                (Some(symbol), Some(_), _) => {
                    let id = self.func_ids.scope().lookup_id(symbol);
                    Some(crate::backend::cranelift::SlotTarget {
                        id,
                        virtual_class: vslot.map(|_| TypeId::from_source_name(class_name)),
                        roots_receiver: vslot.is_none() && !self.async_methods.contains(&id),
                    })
                }
                _ => None,
            };
            self.output.interface_slot_summaries.get_mut().clear();
            self.interface_slot_targets
                .entry((iface.name, method_name.to_string()))
                .or_default()
                .push(target);
            let Some(func_id) = func_id else {
                continue;
            };
            let entry = match vslot {
                Some(vslot) => {
                    self.declare_vtable_thunk(class_name, method_name, vslot, func_id, span)?
                }
                None => func_id,
            };
            let func_ref = self.output.module.declare_func_in_data(entry, &mut data);
            data.write_function_addr(
                willow_abi::dispatch_layout::table_slot_offset(slot as u32, pointer_bytes),
                func_ref,
            );
        }
        for (index, sup) in iface.extends.iter().enumerate() {
            let canonical = self
                .classes()
                .interface(sup)
                .map(|info| info.name)
                .unwrap_or(*sup);
            if let Some(&target) = self.vtable_ids.get(&(key.0, canonical)) {
                let reference = self.output.module.declare_data_in_data(target, &mut data);
                data.write_data_addr(
                    willow_abi::dispatch_layout::table_slot_offset(
                        (method_words + index) as u32,
                        pointer_bytes,
                    ),
                    reference,
                    0,
                );
            }
        }
        self.output.module.define_data(data_id, &data)?;
        Ok(())
    }

    /// Emit (once per `(class, method)` pair) the virtual-dispatch thunk a
    /// vtable slot points at instead of a method body.
    ///
    /// The thunk takes the target method's own signature, loads the receiver's
    /// class descriptor from word 0 of the object, reads virtual slot `vslot`
    /// from it, and calls that. `vslot` is the index the STATIC class assigns
    /// the method, and it is valid for every class the receiver can actually be
    /// because a subclass's slot order EXTENDS its base's — the same invariant
    /// [`FuncGen::emit_vtable_slot_load`] relies on for a class-typed call.
    ///
    /// Nothing is allocated between entry and the call, so no GC can run inside
    /// the thunk and its arguments need no roots of their own; the callee roots
    /// its parameters exactly as it does under a direct call.
    pub(super) fn declare_vtable_thunk(
        &mut self,
        class_name: &str,
        method_name: &str,
        vslot: usize,
        target: FuncId,
        span: crate::diagnostics::Span,
    ) -> Result<FuncId> {
        let key = (class_name.to_string(), method_name.to_string());
        if let Some(&existing) = self.vtable_thunk_ids.get(&key) {
            return Ok(existing);
        }
        // The body it forwards to is already declared, so its signature is the
        // one authority on the thunk's own: an `override` must match the method
        // it replaces, so every class the receiver can be agrees with it.
        let sig = self
            .output
            .module
            .declarations()
            .get_function_decl(target)
            .signature
            .clone();
        if sig.params.is_empty() {
            // No receiver to read a descriptor from. Unreachable for an
            // instance method, and a vtable slot is only ever entered through
            // one, but codegen stays total rather than indexing into nothing.
            return Ok(target);
        }
        let symbol = vtable_thunk_symbol(class_name, method_name);
        // Shares the one linker namespace with every other symbol the backend
        // hands out, so it is claimed like the rest (shared compiler infrastructure, item 8).
        self.claim_symbol(
            &symbol,
            format!("virtual dispatch thunk for `{class_name}::{method_name}`"),
            span,
        )?;
        let func_id = self
            .output
            .module
            .declare_function(&symbol, Linkage::Local, &sig)?;

        let mut ctx = self.output.module.make_context();
        ctx.func.signature = sig.clone();
        ctx.func.name = UserFuncName::user(0, func_id.as_u32());
        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        let params: Vec<cranelift_codegen::ir::Value> = builder.block_params(entry).to_vec();
        let ptr_ty = reference_type(self.output.module.target_config());
        // Parameter 0 is the receiver for every instance method, which is what
        // a vtable slot is only ever reached through.
        let self_ptr = params[0];
        let descriptor = builder
            .ins()
            .load(ptr_ty, MemFlagsData::new(), self_ptr, 0i32);
        let offset =
            willow_abi::dispatch_layout::class_slot_offset(vslot as u32, ptr_ty.bytes()) as i32;
        let callee = builder
            .ins()
            .load(ptr_ty, MemFlagsData::new(), descriptor, offset);
        let sig_ref = builder.import_signature(sig);
        let call = builder.ins().call_indirect(sig_ref, callee, &params);
        let results = builder.inst_results(call).to_vec();
        builder.ins().return_(&results);
        builder.finalize(self.output.module.target_config());
        self.output.module.define_function(func_id, &mut ctx)?;
        self.output.module.clear_context(&mut ctx);

        self.vtable_thunk_ids.insert(key, func_id);
        Ok(func_id)
    }

    /// Emit one descriptor per class named in `classes` (already
    /// module-qualified), plus one for every ancestor they name.
    ///
    /// Must run AFTER `declare_class_methods`, because every slot is filled by
    /// `resolve_class_method_func_id`, which reads `func_ids`.
    /// Emit a descriptor for every class in `program`.
    pub(in crate::backend::cranelift) fn declare_class_descriptors(
        &mut self,
        program: &Program,
    ) -> Result<()> {
        let classes: Vec<ClassDecl> = program
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Class(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        self.declare_class_descriptors_for(&classes)
    }

    pub(in crate::backend::cranelift) fn declare_class_descriptors_for(
        &mut self,
        classes: &[ClassDecl],
    ) -> Result<()> {
        for c in classes {
            self.declare_one_class_descriptor(&c.name, c.span)?;
        }
        Ok(())
    }

    /// Emit `class_name`'s descriptor: `type_id` at offset 0, then one function
    /// pointer per entry of the class's frozen virtual slot order.
    ///
    /// Every class gets one, INCLUDING a class with no virtual methods at all —
    /// otherwise word 0 of an object would sometimes be a descriptor address
    /// and sometimes a bare type_id, and no reader could tell which.
    ///
    pub(in crate::backend::cranelift) fn declare_one_class_descriptor(
        &mut self,
        class_name: &str,
        span: crate::diagnostics::Span,
    ) -> Result<()> {
        if self.class_descriptor_ids.contains_key(class_name) {
            return Ok(());
        }
        let Some(type_id) = self.classes().type_id(class_name) else {
            return Ok(()); // not a registered class; nothing to describe
        };
        let slots = self.classes().slots(class_name);
        let slots: &[String] = slots.as_ref().map_or(&[], |slots| slots.as_slice());
        let symbol = class_descriptor_symbol(class_name);
        // Shares the one linker namespace with every other symbol the backend
        // hands out, so it is claimed like the rest (shared compiler infrastructure, item 8).
        self.claim_symbol(&symbol, format!("class `{class_name}`"), span)?;
        let data_id = self
            .output
            .module
            .declare_data(&symbol, Linkage::Local, false, false)?;
        let mut data = DataDescription::new();
        // Explicit zeroed bytes (not `define_zeroinit`, which is BSS and cannot
        // carry the function-address relocations written below) — the same
        // constraint `declare_one_vtable` works under.
        let pointer_bytes = reference_type(self.output.module.target_config()).bytes();
        data.set_align(willow_abi::dispatch_layout::CLASS_ID_BYTES as u64);
        let mut bytes = vec![
            0u8;
            willow_abi::dispatch_layout::class_slot_offset(slots.len() as u32, pointer_bytes)
                as usize
        ];
        // The type_id is a plain constant, not a relocation, so it is written
        // into the bytes directly — in the TARGET's byte order, which is what
        // the generated `load` will read it back in.
        let type_id_bytes = match self.output.module.isa().endianness() {
            cranelift_codegen::ir::Endianness::Little => type_id.to_le_bytes(),
            cranelift_codegen::ir::Endianness::Big => type_id.to_be_bytes(),
        };
        bytes[..8].copy_from_slice(&type_id_bytes);
        data.define(bytes.into_boxed_slice());
        for (slot, method_name) in slots.iter().enumerate() {
            // Resolves to an ancestor's body when this class did not redeclare
            // the method, which is exactly what an inherited slot must hold.
            if let Some(func_id) = self.resolve_class_method_func_id(class_name, method_name) {
                let func_ref = self.output.module.declare_func_in_data(func_id, &mut data);
                data.write_function_addr(
                    willow_abi::dispatch_layout::class_slot_offset(slot as u32, pointer_bytes),
                    func_ref,
                );
            }
        }
        self.output.module.define_data(data_id, &data)?;
        self.class_descriptor_ids
            .insert(class_name.to_string(), data_id);
        Ok(())
    }
}
