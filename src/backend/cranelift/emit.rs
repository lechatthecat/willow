//! Expression and statement codegen for the Cranelift backend (the `emit_*`
//! methods, extracted from `mod.rs`). `pub(super)` so the codegen driver can
//! call them; as a child module these reach FuncGen's private fields/methods.

use cranelift_codegen::ir::{InstBuilder, MemFlags, StackSlotData, StackSlotKind, types};
use cranelift_module::Module;

use super::*;

impl<'a, 'b> FuncGen<'a, 'b> {
    pub(super) fn emit_reference_write_barrier_hook(
        &mut self,
        _ptr: cranelift_codegen::ir::Value,
        _val: cranelift_codegen::ir::Value,
        ty: &Type,
    ) {
        if is_gc_managed(ty, self.enum_infos) {
            // Current stop-the-world GC does not require a write barrier. Keep all
            // indirect reference stores flowing through this hook so a future
            // generational/concurrent collector can attach one in a single place.
        }
    }

    /// Push a GC root for a pointer value. Creates a stack slot to hold the pointer so
    /// the GC can find and mark the object via `willow_push_root`.
    pub(super) fn emit_push_root(&mut self, val: cranelift_codegen::ir::Value) {
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            0,
        ));
        self.builder.ins().stack_store(val, slot, 0);
        let ptr_ty = self.module.target_config().pointer_type();
        let addr = self.builder.ins().stack_addr(ptr_ty, slot, 0);
        let push_id = self.func_id("willow_push_root");
        let push_ref = self.module.declare_func_in_func(push_id, self.builder.func);
        self.builder.ins().call(push_ref, &[addr]);
        self.gc_root_count += 1;
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
        // The vtable is keyed by the registered (canonical) interface name. A
        // directly-imported interface alias (`import mod::Iface` -> bare `Iface`)
        // names the box site with the local alias, so canonicalize before the
        // lookup; otherwise the box silently falls back to the raw object and
        // dispatch crashes (willow-64gs.1).
        let canonical_iface = self
            .interface_infos
            .get(interface_name)
            .map(|i| i.name.clone())
            .unwrap_or_else(|| interface_name.to_string());
        let vtable_id = self
            .vtable_ids
            .get(&(class_name.to_string(), canonical_iface))
            .or_else(|| {
                self.vtable_ids
                    .get(&(class_name.to_string(), interface_name.to_string()))
            })
            .copied()
            .or_else(|| {
                // The box site may name a module-local generic interface by its
                // bare name (`Box`) while its vtable is keyed by the qualified
                // name (`mod::Box`). Fall back to the class's unique vtable whose
                // interface short name (last `::` segment) matches (willow-1js.5).
                let short = interface_name.rsplit("::").next().unwrap_or(interface_name);
                let mut found: Option<DataId> = None;
                for (key, id) in self.vtable_ids.iter() {
                    let (cls, iface) = (&key.0, &key.1);
                    if cls == class_name && iface.rsplit("::").next().unwrap_or(iface) == short {
                        if found.is_some() {
                            return None; // ambiguous: more than one match
                        }
                        found = Some(*id);
                    }
                }
                found
            });
        let Some(vtable_id) = vtable_id else {
            // No vtable registered (e.g. unknown interface already diagnosed):
            // fall back to the raw object so codegen stays total.
            return object;
        };

        // Root the object across the box allocation (the alloc may collect).
        self.emit_push_root(object);
        let size = self.builder.ins().iconst(types::I64, 16);
        let mask = self.builder.ins().iconst(types::I64, 0b01);
        let alloc_id = self.func_id("willow_alloc_typed");
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self.builder.ins().call(alloc_ref, &[size, mask]);
        let box_ptr = self.builder.inst_results(call)[0];

        // word 0: concrete object pointer (GC-traced). The GC is non-moving, so
        // the rooted `object` value is still valid after any collection above.
        self.builder
            .ins()
            .store(MemFlags::new(), object, box_ptr, 0i32);

        // word 1: vtable address (a static data symbol; not a GC reference).
        let gv = self
            .module
            .declare_data_in_func(vtable_id, self.builder.func);
        let ptr_ty = self.module.target_config().pointer_type();
        let vtable_ptr = self.builder.ins().global_value(ptr_ty, gv);
        self.builder
            .ins()
            .store(MemFlags::new(), vtable_ptr, box_ptr, 8i32);

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
            if variant.payload_types.is_empty() {
                return self.emit_enum_variant_alloc(variant.tag, &[]);
            }
            return self.emit_enum_variant_alloc(variant.tag, &s.args);
        }

        // Lock primitives (willow-dgwo.3): `Mutex::new(v)` / `RwLock::new(v)` ->
        // a Box-allocated word cell. The value is coerced to a 64-bit word; the
        // is_ref flag lets the collector trace a held GC reference.
        if (class_name == "Mutex" || class_name == "RwLock") && s.method == "new" {
            let elem_ty = self.ast_type_of(&s.args[0].expr);
            let val = self.emit_expr(&s.args[0].expr);
            let word = self.coerce_to_i64(val, &elem_ty);
            let is_ref = is_gc_managed(&elem_ty, self.enum_infos);
            let flag = self.builder.ins().iconst(types::I64, is_ref as i64);
            let rt = if class_name == "Mutex" {
                "willow_mutex_new"
            } else {
                "willow_rwlock_new"
            };
            let fid = self.func_ids[rt];
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let call = self.builder.ins().call(fref, &[word, flag]);
            return self.builder.inst_results(call)[0];
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
            let fid = self.func_ids[rt];
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let call = self.builder.ins().call(fref, &[arg]);
            return self.builder.inst_results(call)[0];
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

        if class_name == "env" {
            let runtime_name = match s.method.as_str() {
                "args_len" => "willow_runtime_args_len",
                "arg" => "willow_runtime_arg",
                "program_name" => "willow_runtime_program_name",
                "args" => "willow_runtime_args_array",
                _ => "",
            };
            if !runtime_name.is_empty() {
                let fid = self.func_ids[runtime_name];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                let (args, temp_roots) = self.emit_call_args_rooted(None, None, None, &s.args);
                let call = self.builder.ins().call(fref, &args);
                let results = self.builder.inst_results(call);
                let result = if results.is_empty() {
                    self.builder.ins().iconst(types::I8, 0)
                } else {
                    results[0]
                };
                if temp_roots > 0 {
                    self.emit_pop_roots_n(temp_roots);
                    self.gc_root_count -= temp_roots;
                }
                return result;
            }
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
            return result;
        }
        self.builder.ins().iconst(types::I64, 0)
    }
}
