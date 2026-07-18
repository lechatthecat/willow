use cranelift_codegen::ir::{InstBuilder, MemFlagsData, condcodes::IntCC, types};
use cranelift_module::Module;

use super::*;

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Dispatch a method call through an interface box: load the concrete object
    /// (word 0) and vtable (word 1), load the slot's function pointer, and make an
    /// indirect call with the object as the first argument (spec §8.3 / §9.4).
    pub(super) fn emit_interface_dispatch(
        &mut self,
        box_ptr: cranelift_codegen::ir::Value,
        iface: &InterfaceInfo,
        m: &MethodCallExpr,
    ) -> cranelift_codegen::ir::Value {
        let Some(slot) = iface.method_order.iter().position(|n| n == &m.method) else {
            // Not an interface method — already rejected by the type checker (E0418).
            return self.builder.ins().iconst(types::I64, 0);
        };
        let method = iface.methods[&m.method].clone();
        let ret_type = method.return_type.clone();
        let param_types = method.params.clone();

        // Load object (word 0, GC ref) and vtable (word 1, raw) from the box.
        let obj = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), box_ptr, 0i32);
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
            None,
            None,
            Some(&param_types),
            &m.args,
        );

        // Indirect-call signature: (object ptr, params...) -> ret.
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        for pt in &param_types {
            sig.params.push(AbiParam::new(clif_type(pt)));
        }
        if ret_type != Type::Void {
            sig.returns.push(AbiParam::new(clif_type(&ret_type)));
        }
        let sig_ref = self.builder.import_signature(sig);

        let mut call_args = vec![obj];
        call_args.extend(arg_vals);
        let call = self.builder.ins().call_indirect(sig_ref, fnptr, &call_args);
        let mut result = if ret_type != Type::Void {
            self.builder.inst_results(call)[0]
        } else {
            self.builder.ins().iconst(types::I64, 0)
        };

        // A method returning `Self` dispatched through the interface yields a
        // concrete object of the SAME class as the receiver. Re-box it with the
        // receiver's own vtable so the caller can keep using it as the interface
        // (the concrete type is unknown statically, but Self guarantees it equals
        // the receiver's class) (willow-1js.5).
        if matches!(&method.return_type, Type::Named(n) if n == "Self") {
            result = self.emit_box_with_vtable(result, vtable);
        }

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
        let size = self.builder.ins().iconst(types::I64, 16);
        let mask = self.builder.ins().iconst(types::I64, 0b01);
        let alloc_id = self.func_id("willow_alloc_typed");
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self.builder.ins().call(alloc_ref, &[size, mask]);
        let box_ptr = self.builder.inst_results(call)[0];
        self.builder
            .ins()
            .store(MemFlagsData::new(), object, box_ptr, 0i32);
        self.builder
            .ins()
            .store(MemFlagsData::new(), vtable_ptr, box_ptr, 8i32);
        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;
        box_ptr
    }

    pub(super) fn emit_method_call(&mut self, m: &MethodCallExpr) -> cranelift_codegen::ir::Value {
        let self_ptr = self.emit_expr(&m.object);
        let obj_type = self.ast_type_of(&m.object);

        // Built-in primitive `toString()` (willow-fvfc): i64/f64/bool convert via
        // a runtime call; String is identity (no allocation).
        if m.method == "toString" && m.args.is_empty() {
            match obj_type {
                Type::String => return self_ptr,
                Type::I64 | Type::F64 | Type::Bool => {
                    let runtime = match obj_type {
                        Type::I64 => "willow_i64_to_string",
                        Type::F64 => "willow_f64_to_string",
                        _ => "willow_bool_to_string",
                    };
                    let fid = self.func_ids[runtime];
                    let fref = self.module.declare_func_in_func(fid, self.builder.func);
                    let call = self.builder.ins().call(fref, &[self_ptr]);
                    return self.builder.inst_results(call)[0];
                }
                _ => {}
            }
        }

        if let Some(val) = self.emit_option_result_method_call(self_ptr, &obj_type.clone(), m) {
            return val;
        }

        // Task/JoinHandle cancel()/is_cancelled() (willow-0a6k.7): the frame's
        // slot 1 holds the task id, same as join.
        if (m.method == "cancel" || m.method == "is_cancelled")
            && join_handle_result_type(&obj_type).is_some()
        {
            let task_id = self.builder.ins().load(
                types::I64,
                MemFlagsData::new(),
                self_ptr,
                async_frame_slot_offset(FRAME_SLOT_TASK_ID),
            );
            if m.method == "cancel" {
                let fid = self.func_id("willow_sched_cancel");
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder.ins().call(fref, &[task_id]);
                return self.builder.ins().iconst(types::I8, 0);
            }
            let fid = self.func_id("willow_sched_is_cancelled");
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let call = self.builder.ins().call(fref, &[task_id]);
            let raw = self.builder.inst_results(call)[0];
            return self.builder.ins().ireduce(types::I8, raw);
        }

        if m.method == "join"
            && let Some(result_ty) = join_handle_result_type(&obj_type)
        {
            // Drive the cooperative scheduler until THIS task (and anything it
            // depends on) completes, then read the result from the frame's
            // slot 0 (willow-bsqy). `self_ptr` is the task frame; slot 1 holds
            // its task id. Driving until just this task — not to quiescence —
            // means joining one task does not run unrelated tasks to
            // completion and cannot hang on an unrelated non-terminating task.
            self.emit_push_root(self_ptr);
            let task_id = self.builder.ins().load(
                types::I64,
                MemFlagsData::new(),
                self_ptr,
                async_frame_slot_offset(FRAME_SLOT_TASK_ID),
            );
            let run_fid = self.func_id("willow_sched_run_until");
            let run_fref = self.module.declare_func_in_func(run_fid, self.builder.func);
            self.builder.ins().call(run_fref, &[task_id]);
            // Joining a cancelled task has no result — located runtime panic
            // instead of reading garbage (willow-0a6k.7).
            let check_fid = self.func_id("willow_sched_join_check");
            let check_fref = self
                .module
                .declare_func_in_func(check_fid, self.builder.func);
            self.builder.ins().call(check_fref, &[task_id]);

            if result_ty == Type::Void {
                self.emit_pop_roots_n(1);
                self.gc_root_count -= 1;
                return self.builder.ins().iconst(types::I8, 0);
            }
            let clif_ret_ty = clif_type(&result_ty);
            let result_off = async_frame_slot_offset(FRAME_SLOT_RESULT);
            let result =
                self.builder
                    .ins()
                    .load(clif_ret_ty, MemFlagsData::new(), self_ptr, result_off);
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
            return result;
        }

        if let Type::Named(n) = &obj_type
            && (n == "AtomicI64" || n == "AtomicBool")
        {
            let is_i64 = n == "AtomicI64";
            return self.emit_atomic_method_call(self_ptr, is_i64, m);
        }

        if let Type::Generic(n, args) = &obj_type
            && (n == "Mutex" || n == "RwLock")
            && args.len() == 1
        {
            let elem_ty = args[0].clone();
            let is_mutex = n == "Mutex";
            return self.emit_lock_method_call(self_ptr, is_mutex, &elem_ty, m);
        }

        if let Some(element_ty) = channel_element_type(&obj_type) {
            return self.emit_channel_method_call(self_ptr, &element_ty, m);
        }

        // Array `.len()` → willow_array_len(arr).
        if let Type::Array(elem_ty) = &obj_type {
            let elem_ty = (**elem_ty).clone();
            match m.method.as_str() {
                "len" => {
                    let id = self.func_id("willow_array_len");
                    let r = self.module.declare_func_in_func(id, self.builder.func);
                    let call = self.builder.ins().call(r, &[self_ptr]);
                    return self.builder.inst_results(call)[0];
                }
                "push" => {
                    // Root the array while the value is evaluated (it may allocate).
                    self.emit_push_root(self_ptr);
                    // Box a class argument when the element type is an interface.
                    let v = self.emit_expr_coerced(&m.args[0].expr, &elem_ty);
                    let word = self.coerce_to_i64(v, &elem_ty);
                    let id = self.func_id("willow_array_push");
                    let r = self.module.declare_func_in_func(id, self.builder.func);
                    self.builder.ins().call(r, &[self_ptr, word]);
                    self.emit_pop_roots_n(1);
                    self.gc_root_count -= 1;
                    return self.builder.ins().iconst(types::I8, 0); // void
                }
                "pop" => {
                    let id = self.func_id("willow_array_pop");
                    let r = self.module.declare_func_in_func(id, self.builder.func);
                    let call = self.builder.ins().call(r, &[self_ptr]);
                    let word = self.builder.inst_results(call)[0];
                    return self.coerce_i64_to(word, &elem_ty);
                }
                // `arr.toString()` -> "[1, 2, 3]" (willow-vwn6).
                "toString" => {
                    if let Some(kind) = collection_elem_kind(&elem_ty) {
                        let kind_val = self.builder.ins().iconst(types::I64, kind);
                        let id = self.func_id("willow_array_to_string");
                        let r = self.module.declare_func_in_func(id, self.builder.func);
                        let call = self.builder.ins().call(r, &[self_ptr, kind_val]);
                        return self.builder.inst_results(call)[0];
                    }
                }
                // `arr.freeze()` -> an immutable copy (willow-dgwo.7).
                "freeze" => {
                    let id = self.func_id("willow_array_copy");
                    let r = self.module.declare_func_in_func(id, self.builder.func);
                    let call = self.builder.ins().call(r, &[self_ptr]);
                    return self.builder.inst_results(call)[0];
                }
                _ => {}
            }
        }

        // FrozenArray<T>.len() — backed by the same array handle (willow-dgwo.7).
        if let Type::Generic(name, fargs) = &obj_type
            && name == "FrozenArray"
            && fargs.len() == 1
            && m.method == "len"
        {
            let id = self.func_id("willow_array_len");
            let r = self.module.declare_func_in_func(id, self.builder.func);
            let call = self.builder.ins().call(r, &[self_ptr]);
            return self.builder.inst_results(call)[0];
        }

        // Map<K,V> and the immutable FrozenMap<K,V> share the same runtime map
        // object, so reads dispatch identically (willow-dgwo.10).
        if let Type::Generic(name, margs) = &obj_type
            && (name == "Map" || name == "FrozenMap")
            && margs.len() == 2
        {
            let key_ty = margs[0].clone();
            let val_ty = margs[1].clone();
            return self.emit_map_method_call(self_ptr, &key_ty, &val_ty, m);
        }

        // Debug build: guard against nil dereference with a source-aware runtime error.
        if self.build_mode == BuildMode::Debug {
            let span = m.object.span();
            self.emit_nil_check(self_ptr, span, &m.method.clone());
        }

        // Interface dispatch: the receiver is an interface box {object, vtable}.
        // Must be checked before class dispatch, since an interface is also a
        // `Type::Named` that `class_name_for_object_type` would accept. A generic
        // interface instantiation (`Box<String>`) dispatches identically — the
        // vtable is keyed by the interface name (willow-1js.1).
        if let Type::Generic(name, _) = &obj_type
            && let Some(iface) = self.interface_infos.get(name).cloned()
        {
            let pushed = self.emit_callstack_push(&m.method, m.span);
            let r = self.emit_interface_dispatch(self_ptr, &iface, m);
            if pushed {
                self.emit_callstack_pop();
            }
            return r;
        }
        if let Some(iface_name) = class_name_for_object_type(&obj_type)
            && let Some(iface) = self.interface_infos.get(&iface_name).cloned()
        {
            let pushed = self.emit_callstack_push(&m.method, m.span);
            let r = self.emit_interface_dispatch(self_ptr, &iface, m);
            if pushed {
                self.emit_callstack_pop();
            }
            return r;
        }

        if let Some(class_name) = class_name_for_object_type(&obj_type) {
            let method_name = m.method.clone();

            // Build the dispatch list keyed by runtime type_id. For each class,
            // resolve the method to the nearest class in its hierarchy (itself
            // then ancestors) that defines it, so a subclass that INHERITS the
            // method (no override) still dispatches to the inherited
            // implementation instead of falling through (willow-ftk).
            let mut dispatch_list: Vec<(i64, String)> = self
                .class_type_ids
                .iter()
                .filter_map(|(cls, &id)| {
                    let mut search = Some(cls.clone());
                    let mut seen = HashSet::new();
                    while let Some(name) = search {
                        if !seen.insert(name.clone()) {
                            break;
                        }
                        let mangled =
                            class_method_symbol_name(self.known_modules, &name, &method_name);
                        if self.func_ids.contains_key(&mangled) {
                            return Some((id, name));
                        }
                        search = self.class_base.get(&name).cloned();
                    }
                    None
                })
                .collect();
            dispatch_list.sort_by_key(|(id, _)| *id);

            if dispatch_list.is_empty() {
                return self.builder.ins().iconst(types::I64, 0);
            }

            // Debug call-chain frame for the method invocation (willow-phx3).
            // Pushed once in the entry block; popped before the single-dispatch
            // return and in the dynamic-dispatch merge block (a panicking method
            // never reaches the pop, leaving its frame on the chain).
            let method_frame_pushed = self.emit_callstack_push(&method_name, m.span);

            // Root the receiver across argument evaluation and the call. The
            // receiver is a live GC object, but a temporary one (e.g.
            // `make_obj().m(make_gc())`) is reachable only through `self_ptr`; an
            // allocating argument expression could otherwise collect it before
            // the call dereferences it in the callee (willow-oewp.6). Popped on
            // the single-dispatch return and in the dynamic-dispatch merge block.
            self.emit_push_root(self_ptr);
            let base_mangled =
                class_method_symbol_name(self.known_modules, &class_name, &method_name);
            let ret_type = self
                .func_return_types
                .get(&base_mangled)
                .cloned()
                .or_else(|| {
                    dispatch_list.first().and_then(|(_, cls)| {
                        let mn = class_method_symbol_name(self.known_modules, cls, &method_name);
                        self.func_return_types.get(&mn).cloned()
                    })
                })
                .unwrap_or(Type::Void);

            if dispatch_list.len() == 1 {
                // Fast path: only one implementation, no need for a dispatch chain.
                let (_, cls) = &dispatch_list[0];
                let mangled = class_method_symbol_name(self.known_modules, cls, &method_name);
                let &func_id = self.func_ids.get(&mangled).unwrap();
                let func_ref = self.module.declare_func_in_func(func_id, self.builder.func);
                let modes = self.func_param_modes.get(&mangled).cloned();
                let param_debug = self.func_param_debug.get(&mangled).cloned();
                let param_types = self.method_param_types(&mangled);
                let has_reference_args = has_reference_args(modes.as_deref(), &m.args);
                let user_callee = format!("{cls}::{method_name}");
                let (arg_vals, temp_roots) = self.emit_call_args_rooted_coerced(
                    Some(&user_callee),
                    modes.as_deref(),
                    param_debug.as_deref(),
                    param_types.as_deref(),
                    &m.args,
                );
                let mut call_args = vec![self_ptr];
                call_args.extend(arg_vals);
                let call = self.builder.ins().call(func_ref, &call_args);
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
                return result;
            }

            // Dynamic dispatch: load runtime type_id from word 0 of the object.
            let runtime_type_id =
                self.builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), self_ptr, 0i32);

            // Use an SSA variable to collect the result across dispatch arms
            // (matches the pattern used by emit_short_circuit_and/or and emit_match).
            let ret_clif_ty = clif_type(&ret_type);
            let result_var = if ret_type != Type::Void {
                let v = self.builder.declare_var(ret_clif_ty);
                let zero = if ret_clif_ty == types::F64 {
                    let bits = self.builder.ins().iconst(types::I64, 0);
                    self.builder
                        .ins()
                        .bitcast(types::F64, MemFlagsData::new(), bits)
                } else if ret_clif_ty == types::I8 {
                    self.builder.ins().iconst(types::I8, 0)
                } else {
                    self.builder.ins().iconst(types::I64, 0)
                };
                self.builder.def_var(v, zero);
                Some(v)
            } else {
                None
            };

            let merge_block = self.builder.create_block();

            let dispatch_len = dispatch_list.len();
            for (i, (type_id, cls)) in dispatch_list.iter().enumerate() {
                let mangled = class_method_symbol_name(self.known_modules, cls, &method_name);
                let &func_id = match self.func_ids.get(&mangled) {
                    Some(id) => id,
                    None => continue,
                };

                let type_id_const = self.builder.ins().iconst(types::I64, *type_id);
                let is_match =
                    self.builder
                        .ins()
                        .icmp(IntCC::Equal, runtime_type_id, type_id_const);

                let match_block = self.builder.create_block();
                let next_block = self.builder.create_block();
                self.builder
                    .ins()
                    .brif(is_match, match_block, &[], next_block, &[]);

                // --- match arm ---
                self.builder.switch_to_block(match_block);
                self.builder.seal_block(match_block);
                let func_ref = self.module.declare_func_in_func(func_id, self.builder.func);
                let modes = self.func_param_modes.get(&mangled).cloned();
                let param_debug = self.func_param_debug.get(&mangled).cloned();
                let param_types = self.method_param_types(&mangled);
                let has_reference_args = has_reference_args(modes.as_deref(), &m.args);
                let user_callee = format!("{cls}::{method_name}");
                let (arg_vals, temp_roots) = self.emit_call_args_rooted_coerced(
                    Some(&user_callee),
                    modes.as_deref(),
                    param_debug.as_deref(),
                    param_types.as_deref(),
                    &m.args,
                );
                let mut call_args = vec![self_ptr];
                call_args.extend(arg_vals);
                let call = self.builder.ins().call(func_ref, &call_args);
                if let Some(rv) = result_var {
                    let result = self.builder.inst_results(call)[0];
                    self.builder.def_var(rv, result);
                }
                if has_reference_args {
                    self.emit_debug_reference_call_clear();
                }
                self.emit_pop_roots_n(temp_roots);
                self.gc_root_count -= temp_roots;
                self.builder.ins().jump(merge_block, &[]);

                // --- no-match: continue to next candidate ---
                self.builder.switch_to_block(next_block);
                self.builder.seal_block(next_block);

                // On the last candidate, fall through to merge with the default (zero) result.
                if i + 1 == dispatch_len {
                    self.builder.ins().jump(merge_block, &[]);
                }
            }

            self.builder.switch_to_block(merge_block);
            self.builder.seal_block(merge_block);
            // Pop the method call-chain frame on the normal-return path (a
            // panicking arm jumps to abort and never reaches here) (willow-phx3).
            if method_frame_pushed {
                self.emit_callstack_pop();
            }
            // Pop the receiver root pushed before the dispatch loop. Each arm
            // already balanced its own argument temp roots (willow-oewp.6).
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
            if let Some(rv) = result_var {
                return self.builder.use_var(rv);
            }
            return self.builder.ins().iconst(types::I64, 0);
        }
        self.builder.ins().iconst(types::I64, 0)
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
