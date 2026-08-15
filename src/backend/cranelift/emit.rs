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
    vtable_ids: &HashMap<(String, String), DataId>,
    interface_infos: &HashMap<String, InterfaceInfo>,
    class_name: &str,
    interface_name: &str,
) -> Option<DataId> {
    let canonical_iface = interface_infos
        .get(interface_name)
        .map(|i| i.name.clone())
        .unwrap_or_else(|| interface_name.to_string());
    vtable_ids
        .get(&(class_name.to_string(), canonical_iface))
        .or_else(|| vtable_ids.get(&(class_name.to_string(), interface_name.to_string())))
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
                if cls == class_name && iface.rsplit("::").next().unwrap_or(iface) == short {
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

    pub(super) fn emit_static_call(&mut self, s: &StaticCallExpr) -> cranelift_codegen::ir::Value {
        let class_name = self.static_call_class_name(&s.class);

        // Built-in `Map::new()` constructor.
        if class_name == "Map" && s.method == "new" {
            let new_id = self.func_id("willow_map_new");
            let new_ref = self.module.declare_func_in_func(new_id, self.builder.func);
            let call = self.builder.ins().call(new_ref, &[]);
            return self.builder.inst_results(call)[0];
        }

        // Check if class is an enum — handle variant construction
        if let Some(enum_info) = self.enum_infos.get(&class_name).cloned()
            && let Some(variant) = enum_info
                .variants
                .iter()
                .find(|v| v.name == s.method)
                .cloned()
        {
            if variant.payload_types.is_empty() && !self.enum_is_gc_object_type(&class_name) {
                return self.builder.ins().iconst(types::I64, variant.tag);
            }
            let result_ty = self
                .expr_types
                .get(&s.span)
                .cloned()
                .unwrap_or_else(|| Type::Named(class_name.clone()));
            if variant.payload_types.is_empty() {
                return self.emit_enum_variant_alloc(&result_ty, variant.tag, &[], &[]);
            }
            let payload_types =
                self.resolve_variant_payload_types(&class_name, &s.method, &result_ty);
            return self.emit_enum_variant_alloc(&result_ty, variant.tag, &s.args, &payload_types);
        }

        // Lock primitives (willow-dgwo.3): a Box-allocated word cell. The value
        // is coerced to a 64-bit word; the is_ref flag lets the collector trace
        // a held GC reference. All three constructors share that shape, but
        // `Mutex::new` builds the SCHEDULER-AWARE cell (willow-38w.1.4): its
        // acquisition parks the task instead of blocking the OS thread, and it
        // is reachable only through `lock <mutex> as [mut] value { .. }`.
        // `RwLock` uses its scheduler-aware reader/writer state as of Stage 5;
        // only `BlockingCell` keeps the native-blocking cell.
        if matches!(
            class_name.as_str(),
            "Mutex" | "RwLock" | "BlockingCell" | "BlockingRwCell"
        ) && s.method == "new"
        {
            let elem_ty = self.ast_type_of(&s.args[0].expr);
            let mut val = self.emit_expr(&s.args[0].expr);
            let is_ref = is_gc_managed(&elem_ty, self.enum_infos);
            // Scheduler-aware lock construction allocates a GC handle and may
            // collect. Root a freshly produced protected reference until the
            // runtime has installed it in the handle's traced value slot.
            let rooted = is_ref && matches!(class_name.as_str(), "Mutex" | "RwLock");
            if rooted {
                let slot = self.emit_push_root(val);
                val = self.stack_load(self.module.target_config().pointer_type(), slot);
            }
            let word = self.coerce_to_i64(val, &elem_ty);
            let flag = self.builder.ins().iconst(types::I64, is_ref as i64);
            let rt = match class_name.as_str() {
                "Mutex" => "willow_async_mutex_new",
                "RwLock" => "willow_async_rwlock_new",
                "BlockingRwCell" => "willow_blocking_rw_cell_new",
                _ => "willow_blocking_cell_new",
            };
            let handle = self.emit_value_runtime_call(rt, &[word, flag]);
            if rooted {
                self.emit_pop_roots_n(1);
                self.gc_root_count -= 1;
            }
            return handle;
        }

        // Atomic primitives (willow-dgwo.3): `AtomicI64::new(x)` /
        // `AtomicBool::new(b)` -> a GC-allocated atomic cell pointer.
        if (class_name == "AtomicI64" || class_name == "AtomicBool") && s.method == "new" {
            let rt = if class_name == "AtomicI64" {
                "willow_atomic_i64_new"
            } else {
                "willow_atomic_bool_new"
            };
            let arg = self.emit_expr(&s.args[0].expr);
            return self.emit_value_runtime_call(rt, &[arg]);
        }

        if matches!(class_name.as_str(), "CancellationToken" | "TaskScope") && s.method == "new" {
            let runtime = if class_name == "CancellationToken" {
                "willow_cancellation_token_new"
            } else {
                "willow_task_scope_new"
            };
            return self.emit_value_runtime_call(runtime, &[]);
        }

        if class_name == "Channel" && s.method == "with_capacity" {
            let elem_ty = s.type_args.first().cloned().unwrap_or(Type::I64);
            let is_ref = is_gc_managed(&elem_ty, self.enum_infos);
            let cap = self.emit_expr(&s.args[0].expr);
            let flag = self.builder.ins().iconst(types::I64, is_ref as i64);
            return self.emit_value_runtime_call("willow_channel_new_bounded", &[flag, cap]);
        }

        if class_name == "Channel" && s.method == "new" {
            // Pass is_ref so the runtime can GC-trace the buffer for GC-element
            // channels (Channel<String>, Channel<class>, ...) (willow-dsw).
            let elem_ty = s.type_args.first().cloned().unwrap_or(Type::I64);
            let is_ref = is_gc_managed(&elem_ty, self.enum_infos);
            let fid = self.func_id("willow_channel_new");
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let flag = self.builder.ins().iconst(types::I64, is_ref as i64);
            let call = self.builder.ins().call(fref, &[flag]);
            return self.builder.inst_results(call)[0];
        }

        if class_name == "f64" && s.method == "to_string" {
            let fid = self.func_id("willow_f64_to_string");
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let (args, temp_roots) = self.emit_call_args_rooted(None, None, None, &s.args);
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            let result = results[0];
            if temp_roots > 0 {
                self.emit_pop_roots_n(temp_roots);
                self.gc_root_count -= temp_roots;
            }
            return result;
        }

        if class_name == "f64" && s.method == "parse" {
            let fid = self.func_id("willow_f64_parse");
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let (args, temp_roots) = self.emit_call_args_rooted(None, None, None, &s.args);
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            let result = results[0];
            if temp_roots > 0 {
                self.emit_pop_roots_n(temp_roots);
                self.gc_root_count -= temp_roots;
            }
            return result;
        }

        // Builtin schema modules (fs/env/net) dispatch by canonical name — the
        // per-module normalization pass has already rewritten std aliases
        // (`import std::fs as files;`), and a USER module registered under
        // the name (incl. `import mine as fs;`) always wins (willow-2s3
        // review fixes).
        let builtin_module: Option<&str> = if self.known_modules.contains_key(&class_name) {
            None
        } else {
            match class_name.as_str() {
                canonical @ ("fs" | "env" | "net" | "parallel") => Some(canonical),
                _ => None,
            }
        };

        if builtin_module == Some("fs") {
            let runtime_name = match s.method.as_str() {
                "temp_path" => "willow_fs_temp_path",
                "read_to_string" => "willow_fs_read_to_string",
                "write_string" => "willow_fs_write_string",
                "exists" => "willow_fs_exists",
                "remove_file" => "willow_fs_remove_file",
                "read_to_string_async" => "willow_fs_read_to_string_async",
                "write_string_async" => "willow_fs_write_string_async",
                "exists_async" => "willow_fs_exists_async",
                "remove_file_async" => "willow_fs_remove_file_async",
                _ => "",
            };
            if !runtime_name.is_empty() {
                let (args, temp_roots) = self.emit_call_args_rooted(None, None, None, &s.args);
                let raw = self
                    .emit_runtime_call_with_cleanup(runtime_name, &args, |this| {
                        if temp_roots > 0 {
                            this.emit_pop_roots_n(temp_roots);
                            this.gc_root_count -= temp_roots;
                        }
                    })
                    .expect("filesystem runtime call returns a value");
                let result = if s.method == "exists" {
                    self.builder.ins().ireduce(types::I8, raw)
                } else {
                    raw
                };
                return result;
            }
        }

        if builtin_module == Some("env") {
            let runtime_name = match s.method.as_str() {
                "args_len" => "willow_runtime_args_len",
                "arg" => "willow_runtime_arg",
                "program_name" => "willow_runtime_program_name",
                "args" => "willow_runtime_args_array",
                _ => "",
            };
            if !runtime_name.is_empty() {
                let (args, temp_roots) = self.emit_call_args_rooted(None, None, None, &s.args);
                return self
                    .emit_runtime_call_with_cleanup(runtime_name, &args, |this| {
                        if temp_roots > 0 {
                            this.emit_pop_roots_n(temp_roots);
                            this.gc_root_count -= temp_roots;
                        }
                    })
                    .unwrap_or_else(|| self.builder.ins().iconst(types::I8, 0));
            }
        }

        if builtin_module == Some("net") {
            let runtime_name = match s.method.as_str() {
                "bind" => "willow_net_bind",
                "local_addr" => "willow_net_local_addr",
                "peer_addr" => "willow_net_peer_addr",
                "shutdown" => "willow_net_shutdown",
                "connect_async" => "willow_net_connect_async",
                "accept_async" => "willow_net_accept_async",
                "read_async" => "willow_net_read_async",
                "write_async" => "willow_net_write_async",
                _ => "",
            };
            if !runtime_name.is_empty() {
                let (args, temp_roots) = self.emit_call_args_rooted(None, None, None, &s.args);
                return self
                    .emit_runtime_call_with_cleanup(runtime_name, &args, |this| {
                        if temp_roots > 0 {
                            this.emit_pop_roots_n(temp_roots);
                            this.gc_root_count -= temp_roots;
                        }
                    })
                    .expect("network runtime call returns a value");
            }
        }

        if builtin_module == Some("parallel") && s.method == "map" {
            let (args, temp_roots) = self.emit_call_args_rooted(None, None, None, &s.args);
            return self
                .emit_runtime_call_with_cleanup("willow_parallel_map_i64", &args, |this| {
                    if temp_roots > 0 {
                        this.emit_pop_roots_n(temp_roots);
                        this.gc_root_count -= temp_roots;
                    }
                })
                .expect("parallel map returns a Task handle");
        }

        // Module call: `math::add(args)` → mangled name `math__add`
        if let Some(module_prefix) = self.known_modules.get(&class_name) {
            let mangled = format!("{}__{}", module_prefix, s.method);
            let fid = match self.func_ids.get(&mangled) {
                Some(&id) => id,
                None => panic!("undefined module function: {}", mangled),
            };
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let modes = self.func_param_modes.get(&mangled).cloned();
            let param_debug = self.func_param_debug.get(&mangled).cloned();
            let has_reference_args = has_reference_args(modes.as_deref(), &s.args);
            let user_callee = format!("{}::{}", class_name, s.method);
            let (args, temp_roots) = self.emit_call_args_rooted(
                Some(&user_callee),
                modes.as_deref(),
                param_debug.as_deref(),
                &s.args,
            );
            let panic_depth = self.emit_pre_user_call_panic_depth(&mangled);
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            let result = if results.is_empty() {
                self.builder.ins().iconst(types::I8, 0)
            } else {
                results[0]
            };
            if has_reference_args {
                self.emit_debug_reference_call_clear();
            }
            self.emit_pop_roots_n(temp_roots);
            self.gc_root_count -= temp_roots;
            self.emit_post_willow_call_panic_check(panic_depth);
            return result;
        }
        // Class static call: dispatch to the mangled class method function.
        // Class methods always have a hidden first `self` parameter (i64), so we
        // pass 0 (null) as the dummy self pointer for static (constructor-style) calls.
        let mangled = class_method_symbol_name(self.known_modules, &class_name, &s.method);
        if let Some(&fid) = self.func_ids.get(&mangled) {
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let dummy_self = self.builder.ins().iconst(types::I64, 0);
            let modes = self.func_param_modes.get(&mangled).cloned();
            let param_debug = self.func_param_debug.get(&mangled).cloned();
            let has_reference_args = has_reference_args(modes.as_deref(), &s.args);
            let user_callee = format!("{}::{}", class_name, s.method);
            let (arg_vals, temp_roots) = self.emit_call_args_rooted(
                Some(&user_callee),
                modes.as_deref(),
                param_debug.as_deref(),
                &s.args,
            );
            let mut args = vec![dummy_self];
            args.extend(arg_vals);
            // Match ordinary and instance-method calls: argument evaluation is
            // outside the callee frame, while a panic in the static method body
            // retains this frame in the debug call chain.
            let pushed = self.emit_callstack_push(&s.method, s.span);
            let panic_depth = self.emit_pre_user_call_panic_depth(&mangled);
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            let result = if results.is_empty() {
                self.builder.ins().iconst(types::I8, 0)
            } else {
                results[0]
            };
            if pushed {
                self.emit_callstack_pop();
            }
            if has_reference_args {
                self.emit_debug_reference_call_clear();
            }
            self.emit_pop_roots_n(temp_roots);
            self.gc_root_count -= temp_roots;
            self.emit_post_willow_call_panic_check(panic_depth);
            return result;
        }
        self.builder.ins().iconst(types::I64, 0)
    }
}
