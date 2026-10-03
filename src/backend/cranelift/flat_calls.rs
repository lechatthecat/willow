//! Direct dispatch over pre-evaluated, ABI-ready operands. Reference arguments
//! carry place addresses; their owners remain rooted by the call preparation.
use super::*;
use cranelift_codegen::ir::{AbiParam, InstBuilder, MemFlagsData, Value, types};
use cranelift_module::Module;

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Runs before argument evaluation. Its returned receiver is snapshotted in
    /// a rooted LIR local; the matching dispatch consumes the prepared frame.
    pub(super) fn emit_flat_prepare_method(
        &mut self,
        receiver: Value,
        receiver_ty: &Type,
        method: &str,
        span: crate::diagnostics::Span,
        push_frame: bool,
    ) -> Value {
        let interface = matches!(receiver_ty, Type::Named(name) | Type::Generic(name, _) if self.classes.is_interface(name));
        let interface_parts = interface.then(|| self.builder.ins().isplit(receiver));
        if let Some((_, vtable)) = interface_parts {
            self.emit_interface_dispatch_nil_check(vtable, span, method);
        }
        if push_frame {
            self.emit_callstack_push(method, span);
        }
        if let Some((object, _)) = interface_parts {
            self.emit_interface_dispatch_nil_check(object, span, method);
        }
        receiver
    }

    pub(super) fn flat_method_frame_enabled(&self, method: &str) -> bool {
        self.build_mode == BuildMode::Debug
            && self.string_literals.contains_key(method)
            && self.string_literals.contains_key(self.source_file)
    }

    fn root_flat_call_args(
        &mut self,
        args: &[Value],
        types: &[Type],
        modes: &[ParamMode],
    ) -> usize {
        let mut roots = 0;
        for (index, (&value, ty)) in args.iter().zip(types).enumerate() {
            if !matches!(modes.get(index), Some(ParamMode::Reference { .. }))
                && is_gc_managed(ty, self.enum_infos)
            {
                self.emit_push_root(value);
                roots += 1;
            }
        }
        roots
    }
    // Explicit receiver, ABI operands, and call-frame state are independent.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_flat_class_method(
        &mut self,
        receiver: Value,
        receiver_ty: &Type,
        method: &str,
        args: &[Value],
        ret_ty: &Type,
        span: crate::diagnostics::Span,
        receiver_rooted: bool,
        frame_prepared: bool,
    ) -> cranelift_codegen::ir::Value {
        let class = class_name_for_object_type(receiver_ty)
            .expect("class receiver type vetted by LIR eligibility");
        let self_ptr = receiver;
        let plan = self.plan_virtual_call(&class.to_string(), method);
        let mangled = plan.mangled.as_str();
        let virtual_slot = plan.virtual_slot;

        let pushed = if frame_prepared {
            self.flat_method_frame_enabled(method)
        } else {
            self.emit_callstack_push(method, span)
        };
        // Frame-backed receivers need a direct root to pin their loaded SSA
        // alias while the user method runs.
        let receiver_roots = usize::from(!receiver_rooted);
        if !receiver_rooted {
            self.emit_push_root(self_ptr);
        }
        // An indirect call cannot be cleared by one implementation's summary:
        // any reachable target's panic is this call site's panic.
        let panic_depth = plan
            .may_panic
            .then(|| self.emit_pre_willow_call_panic_depth())
            .flatten();
        // Descriptor and vtable slots are immutable. Resolve them without an
        // intervening allocation after the prepared arguments are available.
        let fnptr = virtual_slot.map(|slot| self.emit_vtable_slot_load(self_ptr, slot));
        let params = self.method_param_types(mangled);
        let modes = self.func_param_modes.get(mangled).cloned();
        let has_reference_args = modes.as_ref().is_some_and(|modes| {
            modes
                .iter()
                .any(|mode| matches!(mode, ParamMode::Reference { .. }))
        });
        let arg_roots = self.root_flat_call_args(
            args,
            params.as_deref().unwrap_or(&[]),
            modes.as_deref().unwrap_or(&[]),
        );
        let arg_vals = args.to_vec();
        let mut call_args = vec![self_ptr];
        call_args.extend(arg_vals);
        let call = match fnptr {
            None => {
                let fid = self.func_ids[mangled];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder.ins().call(fref, &call_args)
            }
            Some(fnptr) => {
                // Every implementation in the hierarchy shares one signature —
                // an `override` may not change it — so the resolved method's
                // ABI describes them all, `&`/`&mut` slots included: those take
                // the pointer ABI their declaration gave them.
                let ret_type = self
                    .func_return_types
                    .get(mangled)
                    .cloned()
                    .unwrap_or(Type::Void);
                let mut sig = self.module.make_signature();
                sig.params
                    .push(AbiParam::new(reference_type(self.module.target_config())));
                let ptr_ty = reference_type(self.module.target_config());
                for (idx, pt) in params.iter().flat_map(|p| p.iter()).enumerate() {
                    let abi = match modes.as_ref().and_then(|all| all.get(idx)) {
                        Some(ParamMode::Reference { .. }) => ptr_ty,
                        _ => self.clif_type(pt),
                    };
                    sig.params.push(AbiParam::new(abi));
                }
                if ret_type != Type::Void {
                    sig.returns.push(AbiParam::new(self.clif_type(&ret_type)));
                }
                let sig_ref = self.builder.import_signature(sig);
                self.builder.ins().call_indirect(sig_ref, fnptr, &call_args)
            }
        };
        let result = self
            .builder
            .inst_results(call)
            .first()
            .copied()
            .unwrap_or_else(|| {
                self.builder.ins().iconst(
                    self.classes
                        .clif_type(reference_type(self.module.target_config()), ret_ty),
                    0,
                )
            });
        if has_reference_args {
            self.emit_flat_reference_call_end();
        }
        if pushed {
            self.emit_callstack_pop();
        }
        self.emit_pop_roots_n(arg_roots + receiver_roots);
        self.gc_root_count -= arg_roots + receiver_roots;
        self.emit_post_willow_call_panic_check(panic_depth);
        result
    }

    /// `iface.method(args)` on an interface-typed receiver: an indirect call
    /// through the receiver pair's vtable (willow-0g8j.6).
    ///
    /// The pair is `[object | vtable]`, so the concrete class is not known
    /// statically: extract the object and vtable, load the slot's function
    /// pointer, and call it with the object as
    /// the hidden receiver. Eligibility proved the interface declares the method
    /// and so fixed its slot; the argument types come from the interface's
    /// declaration, which determines each class argument’s interface pair.
    ///
    /// Root the concrete object directly across dispatch so a moving minor
    /// collection cannot invalidate the hidden receiver SSA value.
    // Explicit receiver, ABI operands, and call-frame state are independent.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_flat_interface_call(
        &mut self,
        receiver: Value,
        receiver_ty: &Type,
        method: &str,
        args: &[Value],
        span: crate::diagnostics::Span,
        receiver_prepared: bool,
        frame_prepared: bool,
    ) -> cranelift_codegen::ir::Value {
        let iface_name = match receiver_ty {
            Type::Named(name) | Type::Generic(name, _) => *name,
            _ => unreachable!("interface receiver type vetted by LIR eligibility"),
        };
        let info = self
            .classes
            .interface(&iface_name)
            .expect("interface info vetted by LIR eligibility");
        // The composed layout the vtables are emitted from, so the slot indexed
        // here is the slot the data object holds (willow-1fc6).
        let slot = super::vtable_layout::slot_of(&self.classes, &info.name, method)
            .expect("interface method slot vetted by LIR eligibility");
        let sig_info = info.methods[method].clone();
        let type_args = match receiver_ty {
            Type::Generic(_, args) => args.as_slice(),
            _ => &[],
        };
        let substitutions: HashMap<TypeId, Type> = info
            .type_params
            .iter()
            .cloned()
            .zip(type_args.iter().cloned())
            .collect();
        let param_types: Vec<Type> = sig_info
            .params
            .iter()
            .map(|ty| crate::semantic::symbols::substitute_type(ty, &substitutions))
            .collect();
        let param_modes: Vec<ParamMode> = sig_info
            .param_infos
            .iter()
            .map(|p| p.mode.clone())
            .collect();
        let ret_type =
            crate::semantic::symbols::substitute_type(&sig_info.return_type, &substitutions);

        let (obj, vtable) = self.builder.ins().isplit(receiver);
        if !receiver_prepared {
            self.emit_interface_dispatch_nil_check(vtable, span, method);
        }
        // Install the method frame before validating the concrete object so a
        // invalid receiver retains the method context in its diagnostic.
        let pushed = if frame_prepared {
            self.flat_method_frame_enabled(method)
        } else {
            self.emit_callstack_push(method, span)
        };
        if !receiver_prepared {
            self.emit_interface_dispatch_nil_check(obj, span, method);
        }
        let fnptr = self.builder.ins().load(
            reference_type(self.module.target_config()),
            MemFlagsData::new(),
            vtable,
            willow_abi::dispatch_layout::table_slot_offset(
                slot as u32,
                reference_type(self.module.target_config()).bytes(),
            ) as i32,
        );

        let slots = self.interface_slot_summary(info.name, method);
        // Pin the hidden receiver even when the interface pair is held only by
        // an async frame's interior slot. Nothing between this load and the
        // callee's entry is a safepoint, so a call whose every target roots
        // `self` itself before its first safepoint needs no second root
        // (willow-8hq4.14).
        let receiver_roots = usize::from(slots.needs_receiver_root);
        if slots.needs_receiver_root {
            self.emit_push_root(obj);
        }
        // The callee is named by its bare method name, and no parameter debug
        // is recorded for it.
        let has_reference_args = param_modes
            .iter()
            .any(|mode| matches!(mode, ParamMode::Reference { .. }));
        let arg_roots = self.root_flat_call_args(args, &param_types, &param_modes);
        let arg_vals = args.to_vec();

        let mut sig = self.module.make_signature();
        sig.params
            .push(AbiParam::new(reference_type(self.module.target_config())));
        let ptr_ty = reference_type(self.module.target_config());
        for (idx, pt) in param_types.iter().enumerate() {
            let abi = match param_modes.get(idx) {
                Some(ParamMode::Reference { .. }) => ptr_ty,
                _ => self.clif_type(pt),
            };
            sig.params.push(AbiParam::new(abi));
        }
        if ret_type != Type::Void {
            sig.returns.push(AbiParam::new(self.clif_type(&ret_type)));
        }
        let sig_ref = self.builder.import_signature(sig);

        let mut call_args = vec![obj];
        call_args.extend(arg_vals);
        // Interface dispatch is indirect, so only a proof covering every
        // function any vtable slot for this method holds may skip the panic
        // bracket (willow-8hq4.14).
        let panic_depth = if slots.may_panic {
            self.emit_pre_willow_call_panic_depth()
        } else {
            None
        };
        let result_ty = (ret_type != Type::Void).then(|| self.clif_type(&ret_type));
        let merge = self.builder.create_block();
        if let Some(ty) = result_ty {
            self.builder.append_block_param(merge, ty);
        }
        // Promote a small polymorphic set without multiplying argument evaluation,
        // roots, panic handling or cleanup. Each guard compares the actual slot
        // address; unknown slots and ABI mismatches always retain the fallback.
        let direct_targets = if self.build_mode == BuildMode::Release {
            slots.direct_targets
        } else {
            [None; 4]
        };
        for fid in direct_targets.into_iter().flatten() {
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            // Only promote calls with exactly the same lowered ABI.
            let target_sig = self.builder.func.dfg.ext_funcs[fref].signature;
            if self.builder.func.dfg.signatures[target_sig]
                != self.builder.func.dfg.signatures[sig_ref]
            {
                continue;
            }
            let addr = self.builder.ins().func_addr(ptr_ty, fref);
            let matches = self.builder.ins().icmp(
                cranelift_codegen::ir::condcodes::IntCC::Equal,
                fnptr,
                addr,
            );
            let direct = self.builder.create_block();
            let next = self.builder.create_block();
            self.builder.ins().brif(matches, direct, &[], next, &[]);
            self.builder.switch_to_block(direct);
            self.builder.seal_block(direct);
            let call = self.builder.ins().call(fref, &call_args);
            let values = self.builder.inst_results(call).to_vec();
            let values = values.iter().copied().map(Into::into).collect::<Vec<_>>();
            self.builder.ins().jump(merge, &values);
            self.builder.switch_to_block(next);
            self.builder.seal_block(next);
        }
        let call = self.builder.ins().call_indirect(sig_ref, fnptr, &call_args);
        let values = self.builder.inst_results(call).to_vec();
        let values = values.iter().copied().map(Into::into).collect::<Vec<_>>();
        self.builder.ins().jump(merge, &values);
        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        let mut result = if result_ty.is_some() {
            self.builder.block_params(merge)[0]
        } else {
            self.builder.ins().iconst(types::I64, 0)
        };

        if pushed {
            self.emit_callstack_pop();
        }
        // The record naming this call's `&place` must not outlive the call
        // (willow-0g8j.11).
        if has_reference_args {
            self.emit_flat_reference_call_end();
        }
        self.emit_pop_roots_n(arg_roots + receiver_roots);
        self.gc_root_count -= arg_roots + receiver_roots;
        self.emit_post_willow_call_panic_check(panic_depth);
        // `-> Self` yields a bare object of the receiver's own class. Pair it
        // only after the panic edge has rejected the neutral placeholder.
        if matches!(&ret_type, Type::Named(n) if n == &TypeId::local("Self")) {
            result = self.emit_box_with_vtable(result, vtable);
        }
        result
    }
}
