use cranelift_codegen::ir::{InstBuilder, MemFlagsData, condcodes::IntCC, types};
use cranelift_module::Module;

use crate::semantic::intrinsics::Intrinsic;

use super::*;

/// How one class-method call site must be emitted (willow-fm7t).
///
/// Produced by [`FuncGen::plan_virtual_call`] and consumed by both backends, so
/// the AST emitter and the LIR walker cannot disagree about whether a call is
/// virtual, which slot it uses, or which implementation's ABI describes it.
pub(super) struct VirtualCallPlan {
    /// The nearest class in the receiver's ancestry that defines the method.
    /// Its signature describes every target, since an `override` may not change
    /// one.
    pub(super) static_class: String,
    /// The mangled symbol of `static_class`'s implementation: the direct
    /// callee, and the source of the return type, parameter modes and debug
    /// metadata for both call shapes.
    pub(super) mangled: String,
    /// Every implementation the receiver could reach. Compile-time only — used
    /// for panic-effect analysis, never to branch on.
    pub(super) dispatch_targets: Vec<String>,
    /// `Some(slot)` when the call must go through the descriptor; `None` when
    /// exactly one implementation exists and the call is direct.
    pub(super) virtual_slot: Option<usize>,
}

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Defense-in-depth check for the two raw pointers used by interface
    /// dispatch: the outer box and its concrete-object word. Safe Willow code
    /// cannot construct either invalid value, but checking this ABI boundary
    /// keeps a corrupt/test-only box from becoming an unchecked native load.
    pub(super) fn emit_interface_dispatch_nil_check(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        span: crate::diagnostics::Span,
        context: &str,
    ) {
        let zero = self.builder.ins().iconst(types::I64, 0);
        let is_nil = self.builder.ins().icmp(IntCC::Equal, ptr, zero);

        let nil_block = self.builder.create_block();
        let ok_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_nil, nil_block, &[], ok_block, &[]);

        self.builder.switch_to_block(nil_block);
        self.builder.seal_block(nil_block);

        let source_file = self.source_file.to_string();
        let context_owned = context.to_string();
        let file_ptr = self.emit_string_literal(&source_file);
        let ctx_ptr = self.emit_string_literal(&context_owned);
        let line_val = self.builder.ins().iconst(types::I32, span.line as i64);
        let col_val = self.builder.ins().iconst(types::I32, span.col as i64);

        self.emit_void_runtime_call("willow_nil_deref", &[file_ptr, line_val, col_val, ctx_ptr]);
        // The runtime helper raises and returns a neutral continuation. Reaching
        // this trap means its panic contract was violated.
        self.builder.ins().trap(TrapCode::unwrap_user(1));

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
    }

    /// Dispatch a method call through an interface box: load the concrete object
    /// (word 0) and vtable (word 1), load the slot's function pointer, and make an
    /// indirect call with the object as the first argument (spec §8.3 / §9.4).
    pub(super) fn emit_interface_dispatch(
        &mut self,
        box_ptr: cranelift_codegen::ir::Value,
        iface: &InterfaceInfo,
        m: &MethodCallExpr,
    ) -> cranelift_codegen::ir::Value {
        let Some(slot) =
            super::vtable_layout::slot_of(self.interface_infos, &iface.name, &m.method)
        else {
            panic!(
                "compiler invariant violated: checked interface method `{}` has no vtable slot",
                m.method
            );
        };
        let method = iface.methods[&m.method].clone();
        let ret_type = method.return_type.clone();
        let param_types = method.params.clone();
        // A `&`/`&mut` parameter is passed as a POINTER, exactly as the concrete
        // method receives it (`param_abi_type`). Dispatching by type alone would
        // hand the callee a value to dereference (willow-0g8j.9).
        let param_modes: Vec<ParamMode> =
            method.param_infos.iter().map(|p| p.mode.clone()).collect();
        let ptr_ty = self.module.target_config().pointer_type();

        // The caller checked the outer box before any field load. Validate the
        // concrete object word before using it as the hidden receiver.
        let obj = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), box_ptr, 0i32);
        // A nil concrete value is represented by a non-nil interface box whose
        // object word is zero. Checking only `box_ptr` therefore lets a
        // fieldless method execute with a nil receiver. Interface downcasts
        // already validate both layers; dispatch must do the same.
        self.emit_interface_dispatch_nil_check(obj, m.object.span(), &m.method);
        let vtable = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), box_ptr, 8i32);
        let fnptr =
            self.builder
                .ins()
                .load(types::I64, MemFlagsData::new(), vtable, (slot * 8) as i32);

        // Root the concrete object across argument evaluation (args may allocate).
        self.emit_push_root(obj);
        let (arg_vals, temp_roots) = self.emit_call_args_rooted_coerced(
            Some(&m.method),
            Some(&param_modes),
            None,
            Some(&param_types),
            &m.args,
        );

        // Indirect-call signature: (object ptr, params...) -> ret.
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        for (idx, pt) in param_types.iter().enumerate() {
            let abi = match param_modes.get(idx) {
                Some(ParamMode::Reference { .. }) => ptr_ty,
                _ => clif_type(pt),
            };
            sig.params.push(AbiParam::new(abi));
        }
        if ret_type != Type::Void {
            sig.returns.push(AbiParam::new(clif_type(&ret_type)));
        }
        let sig_ref = self.builder.import_signature(sig);

        let mut call_args = vec![obj];
        call_args.extend(arg_vals);
        let call = self.builder.ins().call_indirect(sig_ref, fnptr, &call_args);
        let result = if ret_type != Type::Void {
            self.builder.inst_results(call)[0]
        } else {
            self.builder.ins().iconst(types::I64, 0)
        };

        // Pop arg roots + the object root.
        self.emit_pop_roots_n(temp_roots + 1);
        self.gc_root_count -= temp_roots + 1;
        result
    }

    /// Box a concrete object with an already-loaded vtable pointer (no vtable-id
    /// lookup). Used to re-box a `Self`-returning interface method result with the
    /// receiver's vtable (willow-1js.5). Layout matches `emit_interface_box`.
    pub(super) fn emit_box_with_vtable(
        &mut self,
        object: cranelift_codegen::ir::Value,
        vtable_ptr: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        self.emit_push_root(object);
        let box_ptr = self.emit_gc_alloc(GcLayoutMetadata::new(
            GcObjectKind::InterfaceBox,
            16,
            0,
            0b01,
        ));
        self.emit_gc_heap_store_classified(
            box_ptr,
            0,
            object,
            true,
            GcStoreDestination::InterfaceObject,
        );
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

    /// Widen an interface box to a SUPER-interface whose vtable is embedded
    /// `slot_offset` slots into the source's (willow-1fc6).
    ///
    /// The object word is carried over unchanged — widening never changes which
    /// object a value denotes — and the vtable pointer is advanced to the
    /// embedded region, which is a byte-identical copy of the target
    /// interface's own vtable for that class. See [`super::vtable_layout`] for
    /// why the region is there and why the copy cannot drift.
    ///
    /// Only called with a non-zero offset: an offset of 0 means the source box
    /// already answers the target's slot numbering, so there is nothing to do.
    ///
    /// Deliberately no [`Self::emit_interface_dispatch_nil_check`]. A widening
    /// cannot produce a nil box that its input did not already carry, and the
    /// only thing that dereferences a box is dispatch, which checks it before
    /// the first load — at the call sites of
    /// [`Self::emit_interface_dispatch`], and in `emit_lir_interface_call` on
    /// the LIR path, since that is where a span for the diagnostic exists.
    /// Guarding here would only move an identical panic earlier, at the cost of
    /// a branch on every coercion and of threading a span through
    /// `coerce_to_target`.
    pub(super) fn emit_interface_rewiden(
        &mut self,
        box_ptr: cranelift_codegen::ir::Value,
        slot_offset: usize,
    ) -> cranelift_codegen::ir::Value {
        let object = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), box_ptr, 0i32);
        let vtable = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), box_ptr, 8i32);
        // Interior pointer into a static data symbol, not into the GC heap, so
        // it needs no rooting and the collector never sees it as a reference.
        let target_vtable = self
            .builder
            .ins()
            .iadd_imm_s(vtable, (slot_offset * 8) as i64);
        self.emit_box_with_vtable(object, target_vtable)
    }

    /// Lower a builtin method call whose identity `intrinsics::resolve` has
    /// already settled (willow-uqzx, catalog item 7).
    ///
    /// The receiver type is still needed here, but only for representation:
    /// element widths, key/value types, and the `AtomicI64`/`AtomicBool` split
    /// decide how a value crosses the runtime ABI, not what the call means.
    /// Every semantic decision was made by the resolver, so this function is a
    /// pure lowering table — adding a builtin adds a variant, and this `match`
    /// stops compiling until the lowering exists.
    ///
    /// The shape accessors below panic rather than substitute a default. The
    /// resolver only produces `MapGet` for a two-argument `Map`, so a mismatch
    /// is a compiler bug, and guessing a key type would emit a runtime call
    /// with the wrong ABI instead of stopping (willow-uqzx, catalog item 14).
    fn emit_intrinsic_method_call(
        &mut self,
        self_ptr: cranelift_codegen::ir::Value,
        obj_type: &Type,
        intrinsic: Intrinsic,
        m: &MethodCallExpr,
    ) -> cranelift_codegen::ir::Value {
        match intrinsic {
            // Built-in primitive `toString()` (willow-fvfc): i64/f64/bool
            // convert through a runtime call; String is the identity and
            // allocates nothing.
            Intrinsic::StringToString => self_ptr,
            Intrinsic::I64ToString => {
                self.emit_value_runtime_call("willow_i64_to_string", &[self_ptr])
            }
            Intrinsic::F64ToString => {
                self.emit_value_runtime_call("willow_f64_to_string", &[self_ptr])
            }
            Intrinsic::BoolToString => {
                self.emit_value_runtime_call("willow_bool_to_string", &[self_ptr])
            }

            // Task/JoinHandle cancellation (willow-0a6k.7): the frame's slot 1
            // holds the task id used by await.
            Intrinsic::TaskCancel => {
                let task_id = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    self_ptr,
                    async_frame_slot_offset(FRAME_SLOT_TASK_ID),
                );
                let fid = self.func_id("willow_sched_cancel");
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder.ins().call(fref, &[task_id]);
                self.builder.ins().iconst(types::I8, 0)
            }
            // Cancellation is answered from the frame HEADER (willow-ezs.1.3),
            // not from the scheduler's task table: the handle we already hold IS
            // the frame, so this is one Acquire load with no lock and no
            // dependency on the task still being retained by the scheduler.
            Intrinsic::TaskIsCancelled => {
                let fid = self.func_id("willow_frame_is_cancelled");
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                let call = self.builder.ins().call(fref, &[self_ptr]);
                let raw = self.builder.inst_results(call)[0];
                self.builder.ins().ireduce(types::I8, raw)
            }
            // `task.result()` (willow-qrj9): a compiler-known awaitable adapter
            // over the SAME task and the SAME async frame. It starts nothing,
            // waits for nothing, registers no waiter and duplicates no
            // computation — `TaskResult<T>` is represented by the very frame
            // pointer `Task<T>` already is, so the adapter is the identity and
            // only the type checker distinguishes the two (`await t` -> `T`,
            // `await t.result()` -> `Result<T, Cancelled>`).
            Intrinsic::TaskResult => self_ptr,

            Intrinsic::TokenIsCancelled | Intrinsic::ScopeIsCancelled => {
                let prefix = cancellation_runtime_prefix(intrinsic);
                let raw =
                    self.emit_value_runtime_call(&format!("{prefix}_is_cancelled"), &[self_ptr]);
                self.builder.ins().ireduce(types::I8, raw)
            }
            // Token/scope cancellation fans out through scheduler cancellation,
            // whose stress safepoints may collect.
            Intrinsic::TokenCancel | Intrinsic::ScopeCancel => {
                let prefix = cancellation_runtime_prefix(intrinsic);
                self.emit_push_root(self_ptr);
                self.emit_runtime_call_with_cleanup(
                    &format!("{prefix}_cancel"),
                    &[self_ptr],
                    |this| {
                        this.emit_pop_roots_n(1);
                        this.gc_root_count -= 1;
                    },
                );
                self.builder.ins().iconst(types::I8, 0)
            }
            Intrinsic::TokenChild | Intrinsic::ScopeChild => {
                let prefix = cancellation_runtime_prefix(intrinsic);
                self.emit_push_root(self_ptr);
                self.emit_runtime_call_with_cleanup(
                    &format!("{prefix}_child"),
                    &[self_ptr],
                    |this| {
                        this.emit_pop_roots_n(1);
                        this.gc_root_count -= 1;
                    },
                )
                .expect("child token/scope call returns a handle")
            }
            Intrinsic::TokenAttach | Intrinsic::ScopeAdd => {
                let (runtime, what) = if intrinsic == Intrinsic::TokenAttach {
                    ("willow_cancellation_token_attach", "token attach")
                } else {
                    ("willow_task_scope_add", "scope add")
                };
                self.emit_push_root(self_ptr);
                let task = self.emit_expr(&m.args[0].expr);
                self.emit_push_root(task);
                self.emit_runtime_call_with_cleanup(runtime, &[self_ptr, task], |this| {
                    this.emit_pop_roots_n(2);
                    this.gc_root_count -= 2;
                })
                .unwrap_or_else(|| panic!("{what} returns the same Task handle"))
            }
            Intrinsic::ScopeFinish => {
                self.emit_push_root(self_ptr);
                self.emit_runtime_call_with_cleanup(
                    "willow_task_scope_finish",
                    &[self_ptr],
                    |this| {
                        this.emit_pop_roots_n(1);
                        this.gc_root_count -= 1;
                    },
                )
                .expect("scope finish returns a Task handle")
            }

            Intrinsic::AtomicLoad
            | Intrinsic::AtomicStore
            | Intrinsic::AtomicSwap
            | Intrinsic::AtomicAdd
            | Intrinsic::AtomicSub => {
                let is_i64 = builtin_types::is(obj_type, B::AtomicI64);
                self.emit_atomic_method_call(self_ptr, is_i64, intrinsic, m)
            }

            Intrinsic::CellGet
            | Intrinsic::CellSet
            | Intrinsic::RwCellRead
            | Intrinsic::RwCellWrite => {
                let elem_ty = intrinsic_type_arg(obj_type, 0, intrinsic);
                self.emit_lock_method_call(self_ptr, &elem_ty, intrinsic, m)
            }

            Intrinsic::ChannelSend | Intrinsic::ChannelRecv | Intrinsic::ChannelClose => {
                let element_ty = intrinsic_type_arg(obj_type, 0, intrinsic);
                self.emit_channel_method_call(self_ptr, &element_ty, intrinsic, m)
            }

            Intrinsic::ArrayLen | Intrinsic::FrozenArrayLen => {
                // A `FrozenArray<T>` is backed by the same runtime array handle
                // as the `Array<T>` it was frozen from (willow-dgwo.7).
                self.emit_value_runtime_call("willow_array_len", &[self_ptr])
            }
            Intrinsic::ArrayPush => {
                let elem_ty = intrinsic_array_element(obj_type, intrinsic);
                // Root the array while the value is evaluated (it may allocate).
                self.emit_push_root(self_ptr);
                // Box a class argument when the element type is an interface.
                let v = self.emit_expr_coerced(&m.args[0].expr, &elem_ty);
                let word = self.coerce_to_i64(v, &elem_ty);
                self.emit_runtime_call_with_cleanup(
                    "willow_array_push",
                    &[self_ptr, word],
                    |this| {
                        this.emit_pop_roots_n(1);
                        this.gc_root_count -= 1;
                    },
                );
                self.builder.ins().iconst(types::I8, 0) // void
            }
            Intrinsic::ArrayPop => {
                let elem_ty = intrinsic_array_element(obj_type, intrinsic);
                let word = self.emit_value_runtime_call("willow_array_pop", &[self_ptr]);
                self.coerce_i64_to(word, &elem_ty)
            }
            // `arr.toString()` -> "[1, 2, 3]" (willow-vwn6). The runtime renders
            // only the four scalar/String element kinds, and the checker rejects
            // any other element type with E1402, so a missing kind here is a
            // compiler bug rather than a program error.
            Intrinsic::ArrayToString => {
                let elem_ty = intrinsic_array_element(obj_type, intrinsic);
                let kind = collection_elem_kind(&elem_ty).unwrap_or_else(|| {
                    panic!(
                        "compiler invariant violated: checked `Array::toString` reached codegen with unrenderable element type"
                    )
                });
                let kind_val = self.builder.ins().iconst(types::I64, kind);
                self.emit_value_runtime_call("willow_array_to_string", &[self_ptr, kind_val])
            }
            // `arr.freeze()` -> an immutable copy (willow-dgwo.7).
            Intrinsic::ArrayFreeze => {
                self.emit_value_runtime_call("willow_array_copy", &[self_ptr])
            }

            // `Map<K,V>` and the immutable `FrozenMap<K,V>` share the same
            // runtime map object, so reads lower identically (willow-dgwo.10).
            Intrinsic::MapInsert
            | Intrinsic::MapGet
            | Intrinsic::MapContains
            | Intrinsic::MapLen
            | Intrinsic::MapToString
            | Intrinsic::MapFreeze
            | Intrinsic::FrozenMapGet
            | Intrinsic::FrozenMapContains
            | Intrinsic::FrozenMapLen => {
                let key_ty = intrinsic_type_arg(obj_type, 0, intrinsic);
                let val_ty = intrinsic_type_arg(obj_type, 1, intrinsic);
                self.emit_map_method_call(self_ptr, &key_ty, &val_ty, intrinsic, m)
            }
        }
    }

    /// This program's `extends` graph in runtime-`type_id` space — the form the
    /// dispatch-chain filter asks its question in (willow-au5k).
    fn class_base_ids(&self) -> HashMap<i64, i64> {
        class_base_ids(self.class_base, self.class_type_ids)
    }

    /// Every class the receiver could actually BE at runtime that supplies its
    /// own implementation of `method_name`, with the statically resolved one
    /// first.
    ///
    /// Only used to answer compile-time questions about the call — whether any
    /// reachable target can panic, and whether there is exactly one target and
    /// the call can therefore be made directly. The dispatch itself no longer
    /// enumerates classes at all (willow-fm7t).
    ///
    /// The question is asked in `type_id` space rather than over class NAMES,
    /// because a directly imported class is registered twice — once
    /// canonically (`zoo::Dog`) and once under the local alias (`Dog`) — while
    /// `class_base` keeps whichever spelling each declaration used. Both
    /// spellings share one `type_id`, so ids are the canonical form here. Over
    /// names, an aliased base class would look like a leaf and its subclasses
    /// would drop out of their own candidate set (the `lir_diff_74`
    /// regression).
    ///
    /// The result is deduplicated by resolved `FuncId` for the same reason: two
    /// spellings of one class mangle to two symbols that share a function, and
    /// counting both would report a monomorphic call as polymorphic.
    fn virtual_dispatch_candidates(&self, class_name: &str, method_name: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen_targets: HashSet<FuncId> = HashSet::new();
        let push = |out: &mut Vec<String>, seen: &mut HashSet<FuncId>, cls: String| {
            let mangled = class_method_symbol_name(self.known_modules, &cls, method_name);
            match self.func_ids.get(&mangled) {
                Some(&fid) if seen.insert(fid) => out.push(cls),
                _ => {}
            }
        };

        if let Some(defining) = self.resolve_defining_class(class_name, method_name) {
            push(&mut out, &mut seen_targets, defining);
        }

        let Some(receiver_id) = self.class_type_ids.get(class_name).copied() else {
            // Not a registered class at all. The only receiver that reaches
            // class dispatch this way is an interface whose `InterfaceInfo` did
            // not resolve under the spelling this compilation unit used — a
            // cross-module default method (`iface_adv_07`). The ancestry walk
            // cannot answer for it, so fall back to every class that supplies
            // the method, which is what this call site emitted before
            // willow-fm7t.
            let mut names: Vec<&String> = self.class_type_ids.keys().collect();
            names.sort();
            for cls in names {
                if let Some(defining) = self.resolve_defining_class(cls, method_name) {
                    push(&mut out, &mut seen_targets, defining);
                }
            }
            return out;
        };

        let base_ids = self.class_base_ids();
        let mut descendants: Vec<(i64, String)> = self
            .class_type_ids
            .iter()
            .filter(|&(_, &id)| {
                id != receiver_id && is_self_or_descendant(&base_ids, id, receiver_id)
            })
            .map(|(cls, &id)| (id, cls.clone()))
            .collect();
        descendants.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        for (_, cls) in descendants {
            if let Some(defining) = self.resolve_defining_class(&cls, method_name) {
                push(&mut out, &mut seen_targets, defining);
            }
        }
        out
    }

    /// How a call to `class_name::method_name` on a receiver of STATIC type
    /// `class_name` must be emitted (willow-fm7t, shared by both backends).
    ///
    /// Everything here is compile-time reasoning over the class tables, so the
    /// AST emitter and the LIR walker ask one function rather than each
    /// deciding for itself — a divergence between them is a miscompile that
    /// only shows up on whichever backend a given function happened to take.
    pub(super) fn plan_virtual_call(&self, class_name: &str, method_name: &str) -> VirtualCallPlan {
        // A method with no slot is neither `open` nor an `override`. It can
        // neither be overridden nor override anything, so its callee is fixed
        // at compile time and a direct call is the whole answer.
        let vslot = self
            .class_vslots
            .get(class_name)
            .and_then(|slots| slots.iter().position(|n| n == method_name));

        // The candidate set answers two compile-time questions only: can any
        // reachable target panic, and is there exactly one target (so the
        // indirect call can be devirtualized).
        let candidates = self.virtual_dispatch_candidates(class_name, method_name);
        let Some(static_class) = candidates.first().cloned() else {
            panic!(
                "compiler invariant violated: checked class method `{class_name}::{method_name}` has no dispatch target"
            );
        };

        // Devirtualize when the hierarchy holds exactly one implementation: the
        // slot can only ever contain that address, so the load and the indirect
        // call buy nothing. This is also the only path for a method with no
        // slot at all.
        let virtual_slot = match vslot {
            Some(slot) if candidates.len() > 1 => Some(slot),
            None if candidates.len() > 1 => panic!(
                "compiler invariant violated: method `{class_name}::{method_name}` has no virtual slot but {} candidate implementations",
                candidates.len()
            ),
            _ => None,
        };

        let dispatch_targets = candidates
            .iter()
            .map(|cls| class_method_symbol_name(self.known_modules, cls, method_name))
            .collect::<Vec<_>>();
        VirtualCallPlan {
            mangled: class_method_symbol_name(self.known_modules, &static_class, method_name),
            static_class,
            dispatch_targets,
            virtual_slot,
        }
    }

    /// Load the function address in virtual slot `slot` of `self_ptr`'s class.
    ///
    /// Two dependent loads: word 0 of every object points at its class
    /// DESCRIPTOR, and slot `k` of that descriptor holds the k-th virtual
    /// method's address. The index is the one computed from the receiver's
    /// STATIC class, and it is valid for every class the receiver can actually
    /// be because a subclass's slot order EXTENDS its base's — an `override`
    /// rewrote that slot, an inherited method left the ancestor's address in
    /// it, and an unrelated class that merely shares the method NAME has its
    /// own descriptor and is never consulted.
    ///
    /// Emit this BEFORE evaluating arguments: an argument expression may itself
    /// allocate or dispatch, and reading the descriptor first keeps the two
    /// dependent loads next to each other.
    pub(super) fn emit_vtable_slot_load(
        &mut self,
        self_ptr: cranelift_codegen::ir::Value,
        slot: usize,
    ) -> cranelift_codegen::ir::Value {
        let ptr_ty = self.module.target_config().pointer_type();
        let descriptor = self
            .builder
            .ins()
            .load(ptr_ty, MemFlagsData::new(), self_ptr, 0i32);
        let offset = (CLASS_DESCRIPTOR_HEADER_BYTES as usize + slot * 8) as i32;
        self.builder
            .ins()
            .load(ptr_ty, MemFlagsData::new(), descriptor, offset)
    }

    /// The nearest class in `class_name`'s own ancestry — itself first — that
    /// defines `method_name`, so a subclass that INHERITS a method resolves to
    /// the implementation it actually inherits (willow-ftk).
    fn resolve_defining_class(&self, class_name: &str, method_name: &str) -> Option<String> {
        let mut search = Some(class_name.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = search {
            if !seen.insert(name.clone()) {
                break;
            }
            let mangled = class_method_symbol_name(self.known_modules, &name, method_name);
            if self.func_ids.contains_key(&mangled) {
                return Some(name);
            }
            search = self.class_base.get(&name).cloned();
        }
        None
    }

    pub(super) fn emit_method_call(&mut self, m: &MethodCallExpr) -> cranelift_codegen::ir::Value {
        let self_ptr = self.emit_expr(&m.object);
        let obj_type = self.ast_type_of(&m.object);

        // Builtin methods are recognised once, by resolving the call to an
        // intrinsic identity (willow-uqzx, catalog item 7), instead of being
        // re-recognised here through a waterfall of `m.method == "..."` and
        // `type_name == "..."` tests. The order those tests were written in was
        // load-bearing and invisible; it now lives in `intrinsics::resolve`,
        // which the backend's structural type walk consults too, so the two can
        // no longer disagree about what a call means or what it produces.
        //
        // Resolution still runs ahead of interface and class dispatch for the
        // reason the string tests did: `Map<K, V>` and `Channel<T>` are
        // `Type::Generic`s that the interface lookup below would otherwise
        // claim.
        if let Some(resolved) = intrinsics::resolve(&obj_type, &m.method, m.args.len()) {
            return self.emit_intrinsic_method_call(self_ptr, &obj_type, resolved.intrinsic, m);
        }

        if let Some(val) = self.emit_option_result_method_call(self_ptr, &obj_type.clone(), m) {
            return val;
        }

        // Interface dispatch: the receiver is an interface box {object, vtable}.
        // Must be checked before class dispatch, since an interface is also a
        // `Type::Named` that `class_name_for_object_type` would accept. A generic
        // interface instantiation (`Box<String>`) dispatches identically — the
        // vtable is keyed by the interface name (willow-1js.1).
        if let Type::Generic(name, _) = &obj_type
            && let Some(iface) = self.interface_infos.get(name).cloned()
        {
            self.emit_interface_dispatch_nil_check(self_ptr, m.object.span(), &m.method);
            let returns_self = iface
                .methods
                .get(&m.method)
                .is_some_and(|method| matches!(&method.return_type, Type::Named(n) if n == "Self"));
            let vtable = returns_self.then(|| {
                self.builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), self_ptr, 8i32)
            });
            let pushed = self.emit_callstack_push(&m.method, m.span);
            let panic_depth = self.emit_pre_willow_call_panic_depth();
            let mut r = self.emit_interface_dispatch(self_ptr, &iface, m);
            if pushed {
                self.emit_callstack_pop();
            }
            self.emit_post_willow_call_panic_check(panic_depth);
            if let Some(vtable) = vtable {
                r = self.emit_box_with_vtable(r, vtable);
            }
            return r;
        }
        if let Some(iface_name) = class_name_for_object_type(&obj_type)
            && let Some(iface) = self.interface_infos.get(&iface_name).cloned()
        {
            self.emit_interface_dispatch_nil_check(self_ptr, m.object.span(), &m.method);
            let returns_self = iface
                .methods
                .get(&m.method)
                .is_some_and(|method| matches!(&method.return_type, Type::Named(n) if n == "Self"));
            let vtable = returns_self.then(|| {
                self.builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), self_ptr, 8i32)
            });
            let pushed = self.emit_callstack_push(&m.method, m.span);
            let panic_depth = self.emit_pre_willow_call_panic_depth();
            let mut r = self.emit_interface_dispatch(self_ptr, &iface, m);
            if pushed {
                self.emit_callstack_pop();
            }
            self.emit_post_willow_call_panic_check(panic_depth);
            if let Some(vtable) = vtable {
                r = self.emit_box_with_vtable(r, vtable);
            }
            return r;
        }

        if let Some(class_name) = class_name_for_object_type(&obj_type) {
            let method_name = m.method.clone();

            let VirtualCallPlan {
                static_class,
                mangled,
                dispatch_targets,
                virtual_slot,
            } = self.plan_virtual_call(&class_name, &method_name);

            // Debug call-chain frame for the method invocation (willow-phx3).
            // Pushed once in the entry block; popped before the return (a
            // panicking method never reaches the pop, leaving its frame on the
            // chain).
            let method_frame_pushed = self.emit_callstack_push(&method_name, m.span);

            // Root the receiver across argument evaluation and the call. The
            // receiver is a live GC object, but a temporary one (e.g.
            // `make_obj().m(make_gc())`) is reachable only through `self_ptr`; an
            // allocating argument expression could otherwise collect it before
            // the call dereferences it in the callee (willow-oewp.6).
            self.emit_push_root(self_ptr);
            let dispatch_panic_depth = self
                .emit_pre_user_dispatch_panic_depth(dispatch_targets.iter().map(String::as_str));

            let ret_type = self
                .func_return_types
                .get(&mangled)
                .cloned()
                .unwrap_or(Type::Void);
            let modes = self.func_param_modes.get(&mangled).cloned();
            let param_debug = self.func_param_debug.get(&mangled).cloned();
            let param_types = self.method_param_types(&mangled);
            let has_reference_args = has_reference_args(modes.as_deref(), &m.args);
            let user_callee = format!("{static_class}::{method_name}");

            let fnptr = virtual_slot.map(|slot| self.emit_vtable_slot_load(self_ptr, slot));

            let (arg_vals, temp_roots) = self.emit_call_args_rooted_coerced(
                Some(&user_callee),
                modes.as_deref(),
                param_debug.as_deref(),
                param_types.as_deref(),
                &m.args,
            );
            let mut call_args = vec![self_ptr];
            call_args.extend(arg_vals);

            let call = match fnptr {
                None => {
                    let &func_id = self.func_ids.get(&mangled).unwrap_or_else(|| {
                        panic!(
                            "compiler invariant violated: resolved class method `{mangled}` has no function id"
                        )
                    });
                    let func_ref = self.module.declare_func_in_func(func_id, self.builder.func);
                    self.builder.ins().call(func_ref, &call_args)
                }
                Some(fnptr) => {
                    // Every implementation in the hierarchy has the same
                    // signature — an `override` may not change it — so the
                    // statically resolved method's ABI describes them all.
                    let ptr_ty = self.module.target_config().pointer_type();
                    let mut sig = self.module.make_signature();
                    sig.params.push(AbiParam::new(types::I64));
                    for (idx, pt) in param_types.iter().flat_map(|p| p.iter()).enumerate() {
                        let abi = match modes.as_ref().and_then(|ms| ms.get(idx)) {
                            Some(ParamMode::Reference { .. }) => ptr_ty,
                            _ => clif_type(pt),
                        };
                        sig.params.push(AbiParam::new(abi));
                    }
                    if ret_type != Type::Void {
                        sig.returns.push(AbiParam::new(clif_type(&ret_type)));
                    }
                    let sig_ref = self.builder.import_signature(sig);
                    self.builder.ins().call_indirect(sig_ref, fnptr, &call_args)
                }
            };

            let result = if ret_type != Type::Void {
                self.builder.inst_results(call)[0]
            } else {
                self.builder.ins().iconst(types::I64, 0)
            };
            if has_reference_args {
                self.emit_debug_reference_call_clear();
            }
            if method_frame_pushed {
                self.emit_callstack_pop();
            }
            // Pop the argument temp roots and the receiver root (+1).
            self.emit_pop_roots_n(temp_roots + 1);
            self.gc_root_count -= temp_roots + 1;
            self.emit_post_willow_call_panic_check(dispatch_panic_depth);
            return result;
        }
        panic!(
            "compiler invariant violated: checked method `{}` has unsupported receiver type `{obj_type:?}`",
            m.method
        )
    }
}

/// The runtime symbol prefix for the cancellation handle an intrinsic belongs
/// to. A token and a scope expose the same four operations over different
/// runtime objects, so the prefix — not the operation — is what differs.
fn cancellation_runtime_prefix(intrinsic: Intrinsic) -> &'static str {
    match intrinsic {
        Intrinsic::TokenIsCancelled
        | Intrinsic::TokenCancel
        | Intrinsic::TokenChild
        | Intrinsic::TokenAttach => "willow_cancellation_token",
        Intrinsic::ScopeIsCancelled
        | Intrinsic::ScopeCancel
        | Intrinsic::ScopeChild
        | Intrinsic::ScopeAdd
        | Intrinsic::ScopeFinish => "willow_task_scope",
        other => panic!(
            "compiler invariant violated: intrinsic `{other:?}` is not a cancellation-handle method"
        ),
    }
}

/// The `index`-th type argument of a resolved intrinsic's receiver.
///
/// `intrinsics::resolve` only produces these intrinsics for a receiver of the
/// right family and type-argument count, so a mismatch is a compiler bug. A
/// default element type would silently pick the wrong runtime ABI — a `Map`
/// key read as `i64` when it is a `String` is a raw pointer hashed as a number
/// (willow-uqzx, catalog item 14).
fn intrinsic_type_arg(obj_type: &Type, index: usize, intrinsic: Intrinsic) -> Type {
    match obj_type {
        Type::Generic(_, args) if index < args.len() => args[index].clone(),
        _ => panic!(
            "compiler invariant violated: intrinsic `{intrinsic:?}` resolved for receiver `{obj_type:?}` with no type argument {index}"
        ),
    }
}

/// The element type of a resolved array intrinsic's receiver.
fn intrinsic_array_element(obj_type: &Type, intrinsic: Intrinsic) -> Type {
    match obj_type {
        Type::Array(elem) => (**elem).clone(),
        _ => panic!(
            "compiler invariant violated: array intrinsic `{intrinsic:?}` resolved for receiver `{obj_type:?}`"
        ),
    }
}

/// Element-kind tag for the collection debug-display runtime calls
/// (willow-vwn6). Must match the runtime's `ELEM_KIND_*` constants.
pub(super) fn collection_elem_kind(ty: &Type) -> Option<i64> {
    match ty {
        Type::I64 => Some(0),
        Type::F64 => Some(1),
        Type::Bool => Some(2),
        Type::String => Some(3),
        _ => None,
    }
}

/// This program's `extends` graph keyed by runtime `type_id`.
///
/// `class_base` is keyed by class NAME, and one class can appear under more
/// than one name: a directly imported class (`import zoo::Dog;`) is registered
/// both canonically and under its local alias, and the two entries can record
/// their base under different spellings. Every name for one class shares one
/// `type_id`, so projecting the graph into id space collapses the aliases and
/// leaves exactly the relation the emitted dispatch chain tests (willow-au5k).
///
/// Names with no `type_id` are dropped: a class that has no runtime id is not a
/// dispatch candidate in the first place.
pub(super) fn class_base_ids(
    class_base: &HashMap<String, String>,
    class_type_ids: &HashMap<String, i64>,
) -> HashMap<i64, i64> {
    class_base
        .iter()
        .filter_map(|(child, base)| Some((*class_type_ids.get(child)?, *class_type_ids.get(base)?)))
        .collect()
}

/// Whether a receiver whose static class has `ancestor_id` can hold an object
/// whose runtime class has `class_id` — the dispatch-chain filter's question.
///
/// The relation is DIRECTED: a base class is not a candidate for a receiver
/// typed as one of its subclasses. The `seen` set makes a malformed `extends`
/// cycle terminate instead of hanging the compiler — a cycle is a checker
/// error, and codegen must not be the place where it turns into a hang.
pub(super) fn is_self_or_descendant(
    base_of: &HashMap<i64, i64>,
    class_id: i64,
    ancestor_id: i64,
) -> bool {
    let mut seen = HashSet::new();
    let mut current = Some(class_id);
    while let Some(id) = current {
        if id == ancestor_id {
            return true;
        }
        if !seen.insert(id) {
            return false;
        }
        current = base_of.get(&id).copied();
    }
    false
}

#[cfg(test)]
mod tests {
    use object::{Object, ObjectSection, ObjectSymbol, RelocationTarget};

    use super::*;

    /// The ancestry perspectives below run in `type_id` space, because that is
    /// what the emitted dispatch chain compares and what collapses a class's
    /// aliases onto one identity.
    ///
    /// ```text
    /// BASE(1) <- MIDDLE(2) <- LEAF(3)      UNRELATED(4)
    /// ```
    const BASE: i64 = 1;
    const MIDDLE: i64 = 2;
    const LEAF: i64 = 3;
    const UNRELATED: i64 = 4;

    fn hierarchy() -> HashMap<i64, i64> {
        HashMap::from([(MIDDLE, BASE), (LEAF, MIDDLE)])
    }

    #[test]
    fn dispatch_01_a_class_is_a_candidate_for_its_own_type() {
        assert!(is_self_or_descendant(&hierarchy(), BASE, BASE));
        assert!(is_self_or_descendant(&hierarchy(), UNRELATED, UNRELATED));
    }

    #[test]
    fn dispatch_02_a_direct_subclass_is_a_candidate() {
        assert!(is_self_or_descendant(&hierarchy(), MIDDLE, BASE));
    }

    #[test]
    fn dispatch_03_a_transitive_subclass_is_a_candidate() {
        assert!(is_self_or_descendant(&hierarchy(), LEAF, BASE));
    }

    /// The whole point of the filter: a class that merely shares a method NAME
    /// with the receiver's class can never carry the receiver's type_id.
    #[test]
    fn dispatch_04_an_unrelated_class_is_not_a_candidate() {
        assert!(!is_self_or_descendant(&hierarchy(), UNRELATED, BASE));
        assert!(!is_self_or_descendant(&hierarchy(), BASE, UNRELATED));
    }

    /// Direction matters. A receiver typed `Leaf` holds a `Leaf`, never the
    /// `Base` it inherits from, so `Base` is not one of its candidates.
    #[test]
    fn dispatch_05_the_relation_is_directed() {
        assert!(is_self_or_descendant(&hierarchy(), LEAF, BASE));
        assert!(!is_self_or_descendant(&hierarchy(), BASE, LEAF));
    }

    /// A sibling branch is excluded even though both sides share an ancestor.
    #[test]
    fn dispatch_06_a_sibling_branch_is_not_a_candidate() {
        const OTHER: i64 = 5;
        let mut classes = hierarchy();
        classes.insert(OTHER, BASE);
        assert!(!is_self_or_descendant(&classes, OTHER, MIDDLE));
        assert!(!is_self_or_descendant(&classes, MIDDLE, OTHER));
        assert!(is_self_or_descendant(&classes, OTHER, BASE));
    }

    /// An `extends` cycle is a checker error. If one ever reaches codegen the
    /// walk must terminate — a hung compiler is a far worse failure than a
    /// wrong dispatch list.
    #[test]
    fn dispatch_07_an_extends_cycle_terminates() {
        let cyclic = HashMap::from([(1i64, 2i64), (2, 1)]);
        assert!(!is_self_or_descendant(&cyclic, 1, 99));
    }

    /// ...and still answers correctly when the target IS in the cycle.
    #[test]
    fn dispatch_08_a_cycle_still_reports_a_reachable_ancestor() {
        let cyclic = HashMap::from([(1i64, 2i64), (2, 1)]);
        assert!(is_self_or_descendant(&cyclic, 1, 2));
        assert!(is_self_or_descendant(&cyclic, 2, 1));
    }

    /// With no inheritance at all, identity is the only relation — which is
    /// what makes the filter collapse a flat program's chain to one entry.
    #[test]
    fn dispatch_09_without_inheritance_only_identity_holds() {
        let flat = HashMap::new();
        assert!(is_self_or_descendant(&flat, BASE, BASE));
        assert!(!is_self_or_descendant(&flat, UNRELATED, BASE));
    }

    /// A class the graph has never heard of resolves to nothing rather than to
    /// a default answer.
    #[test]
    fn dispatch_10_an_unknown_class_is_not_a_candidate() {
        assert!(!is_self_or_descendant(&hierarchy(), 404, BASE));
    }

    /// Depth is not bounded by anything in the language, so the walk must not
    /// be either.
    #[test]
    fn dispatch_11_a_deep_hierarchy_resolves_to_its_root() {
        let deep: HashMap<i64, i64> = (1..64).map(|level| (level, level - 1)).collect();
        assert!(is_self_or_descendant(&deep, 63, 0));
        assert!(is_self_or_descendant(&deep, 63, 62));
        assert!(!is_self_or_descendant(&deep, 0, 63));
    }

    /// The reason the graph is projected into id space at all: a directly
    /// imported class is registered under BOTH its canonical name and the local
    /// alias, and the two entries can record their base under different
    /// spellings. Over names, `zoo::Dog extends zoo::Animal` and a receiver
    /// typed `Animal` (the alias) never meet, the base looks like a leaf, and
    /// the subclass is filtered out of its own chain.
    #[test]
    fn dispatch_12_aliased_import_names_collapse_onto_one_id() {
        let type_ids = HashMap::from([
            ("zoo::Animal".to_string(), 1i64),
            ("Animal".to_string(), 1),
            ("zoo::Dog".to_string(), 2),
            ("Dog".to_string(), 2),
        ]);
        let class_base = HashMap::from([
            ("zoo::Dog".to_string(), "zoo::Animal".to_string()),
            ("Dog".to_string(), "zoo::Animal".to_string()),
        ]);

        let base_ids = class_base_ids(&class_base, &type_ids);
        assert_eq!(base_ids, HashMap::from([(2i64, 1i64)]));
        assert!(is_self_or_descendant(
            &base_ids,
            type_ids["Dog"],
            type_ids["Animal"]
        ));
    }

    /// An edge naming a class with no runtime id contributes nothing instead of
    /// a bogus relation.
    #[test]
    fn dispatch_12b_edges_without_ids_are_dropped() {
        let type_ids = HashMap::from([("Known".to_string(), 1i64)]);
        let class_base = HashMap::from([
            ("Known".to_string(), "Vanished".to_string()),
            ("Vanished".to_string(), "Known".to_string()),
        ]);
        assert!(class_base_ids(&class_base, &type_ids).is_empty());
    }

    const INVALID_BOX_FIXTURE_SOURCE: &str = r#"
interface FixtureReader { fn read(self) -> i64; }
class FixtureReaderImpl implements FixtureReader {
    pub fn read(self) -> i64 { return 1; }
}
class DirectReader {
    pub value: i64;
    pub fn read(self) -> i64 { return self.value; }
}
fn interface_probe(reader: FixtureReader) -> i64 {
    return reader.read();
}
fn direct_field_probe(reader: DirectReader) -> i64 {
    return reader.value;
}
fn direct_method_probe(reader: DirectReader) -> i64 {
    return reader.read();
}
fn main() {}
"#;

    /// Compile an interface call without constructing its receiver in Willow
    /// source. The exported `interface_probe` parameter is the backend fixture
    /// boundary: its two-word representation can have a valid outer box with a
    /// zero object word, without preserving a safe-language path that creates
    /// that invalid state (willow-glaj.8).
    fn compile_interface_probe(use_lir: bool) -> Vec<u8> {
        let tokens = crate::lexer::Lexer::new(INVALID_BOX_FIXTURE_SOURCE)
            .tokenize()
            .expect("fixture should lex");
        let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
        assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");

        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).expect("prelude should register");
        checker.check_program(&program);
        assert!(
            checker.errors.is_empty(),
            "type errors: {:?}",
            checker.errors
        );

        let mut codegen =
            Codegen::new(&CompilerOptions::debug()).expect("codegen should initialize");
        codegen.register_builtin_generic_enums();
        for (name, info) in &checker.symbols.enums {
            codegen.register_enum_info(name.to_string(), info.clone());
        }
        for (name, info) in &checker.symbols.interfaces {
            codegen.register_interface_info(name.to_string(), info.clone());
        }
        codegen.register_expr_types(checker.expr_types.clone());
        if use_lir {
            let tables = crate::ir::lower::CheckerTables::from_checker(&checker);
            let (hir, gaps) = crate::ir::lower::lower_program_with(&program, &tables);
            assert!(gaps.is_empty(), "fixture lowering gaps: {gaps:?}");
            codegen.register_lir_functions(crate::ir::lowered::lower_program(&hir));
        }
        codegen
            .compile_program(&program, "interface_invalid_box_fixture.wi")
            .expect("fixture should compile");
        codegen.finish().expect("fixture object should finish")
    }

    fn nil_check_relocations_in_symbol(bytes: &[u8], symbol_name: &str) -> usize {
        let file = object::File::parse(bytes).expect("fixture object should parse");
        let probe = file
            .symbols()
            .find(|symbol| symbol.name().ok() == Some(symbol_name))
            .unwrap_or_else(|| panic!("{symbol_name} symbol should exist"));
        let section = file
            .section_by_index(probe.section_index().expect("probe should have a section"))
            .expect("probe section should exist");
        let start = probe.address();
        // COFF function symbols commonly report a size of zero. In that case,
        // use the next symbol in the same section as the function boundary.
        let end = if probe.size() != 0 {
            start + probe.size()
        } else {
            file.symbols()
                .filter(|symbol| symbol.section_index() == probe.section_index())
                .map(|symbol| symbol.address())
                .filter(|address| *address > start)
                .min()
                .unwrap_or(section.address() + section.size())
        };
        section
            .relocations()
            .filter(|(offset, relocation)| {
                if *offset < start || *offset >= end {
                    return false;
                }
                let RelocationTarget::Symbol(index) = relocation.target() else {
                    return false;
                };
                file.symbol_by_index(index)
                    .ok()
                    .and_then(|symbol| symbol.name().ok())
                    == Some("willow_nil_deref")
            })
            .count()
    }

    fn assert_invalid_object_word_guard(use_lir: bool) {
        let bytes = compile_interface_probe(use_lir);
        // One check validates the outer box and the second validates word 0.
        // A regression to checking only the box therefore drops this to one.
        assert_eq!(
            nil_check_relocations_in_symbol(&bytes, "interface_probe"),
            2
        );
        // The nil helper receives the method name, so a fault from the second
        // check retains `read` as its call-site context.
        assert!(
            bytes
                .windows(b"read\0".len())
                .any(|window| window == b"read\0"),
            "fixture object lost the interface method context"
        );
    }

    #[test]
    fn interface_guard_01_ast_invalid_object_word_keeps_method_context() {
        assert_invalid_object_word_guard(false);
    }

    #[test]
    fn interface_guard_02_lir_invalid_object_word_keeps_method_context() {
        assert_invalid_object_word_guard(true);
    }

    fn assert_direct_class_access_has_no_nil_guard(use_lir: bool) {
        let bytes = compile_interface_probe(use_lir);
        assert_eq!(
            nil_check_relocations_in_symbol(&bytes, "direct_field_probe"),
            0,
            "direct class field access retained an obsolete nullable guard"
        );
        assert_eq!(
            nil_check_relocations_in_symbol(&bytes, "direct_method_probe"),
            0,
            "direct class method dispatch retained an obsolete nullable guard"
        );
    }

    #[test]
    fn interface_guard_03_ast_direct_class_access_has_no_nil_guard() {
        assert_direct_class_access_has_no_nil_guard(false);
    }

    #[test]
    fn interface_guard_04_lir_direct_class_access_has_no_nil_guard() {
        assert_direct_class_access_has_no_nil_guard(true);
    }
}
