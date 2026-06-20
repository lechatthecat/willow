//! Expression and statement codegen for the Cranelift backend (the `emit_*`
//! methods, extracted from `mod.rs`). `pub(super)` so the codegen driver can
//! call them; as a child module these reach FuncGen's private fields/methods.

use cranelift_codegen::ir::{
    InstBuilder, MemFlags, StackSlotData, StackSlotKind,
    condcodes::{FloatCC, IntCC},
    types,
};
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

    /// Emit `expr`, then coerce the result to `target_ty` (class→interface box).
    pub(super) fn emit_expr_coerced(
        &mut self,
        expr: &Expr,
        target_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        // An array literal with a known target element type emits its elements
        // boxed to that type (so `let a: Array<Animal> = [Dog {}]` stores boxes,
        // and `[]` becomes a reference-element array).
        if let Expr::ArrayLiteral(elements, _) = expr {
            let target_inner = match target_ty {
                Type::Nullable(inner) => inner.as_ref(),
                other => other,
            };
            if let Type::Array(elem) = target_inner {
                let elem = (**elem).clone();
                return self.emit_array_literal(elements, &elem);
            }
        }
        let value = self.emit_expr(expr);
        let value_ty = self.ast_type_of(expr);
        self.coerce_to_target(value, &value_ty, target_ty)
    }

    pub(super) fn emit_block(&mut self, block: &Block) {
        let saved_vars = self.vars.clone();
        let gc_roots_before = self.gc_root_count;

        for stmt in &block.stmts {
            if self.terminated {
                break;
            }
            self.emit_stmt(stmt);
        }

        // Pop any GC roots introduced by this block before the vars go out of scope.
        if !self.terminated {
            let block_roots = self.gc_root_count - gc_roots_before;
            if block_roots > 0 {
                self.emit_pop_roots_n(block_roots);
            }
        }
        // Restore scope: gc_root_count goes back to what it was before the block
        // (in the terminated path the return handler already popped all roots).
        self.gc_root_count = gc_roots_before;
        self.vars = saved_vars;
    }

    pub(super) fn emit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(s) => {
                // With an interface annotation, a class initializer is boxed.
                let val = match &s.ty {
                    Some(target) => self.emit_expr_coerced(&s.init, &target.clone()),
                    None => self.emit_expr(&s.init),
                };
                // `_` is the wildcard name: evaluate for side effects but don't bind.
                if s.name == "_" {
                    return;
                }
                let ast_ty =
                    s.ty.clone()
                        .or_else(|| self.async_local_types.get(&s.span).cloned())
                        .unwrap_or_else(|| self.ast_type_of_init(&s.init));
                // In an async fn, a GC-managed local that is part of the frame
                // layout lives in the heap frame so it survives `await`
                // (willow-lpn.5b). The frame is already a GC root, so the local
                // needs no separate shadow-stack root.
                // Frame-back any local that has a frame offset. For eager async
                // only GC-managed locals get offsets (setup_async_frame); the
                // cooperative poll-fn path also assigns offsets to non-GC locals
                // so they survive suspension (willow-lpn.5.3 slice 3b). Non-GC
                // slots are not in the frame's GC mask, so they are not traced.
                if let Some(&offset) = self.async_frame_offsets.get(&s.span) {
                    let base = self
                        .async_frame
                        .expect("frame-backed local requires an allocated async frame");
                    self.builder.ins().store(MemFlags::new(), val, base, offset);
                    self.vars.insert(
                        s.name.clone(),
                        VarStorage::Frame {
                            offset,
                            ty: ast_ty.clone(),
                        },
                    );
                    return;
                }
                let storage = if is_gc_managed(&ast_ty, self.enum_infos) {
                    // GC-managed types: store in a stack slot so that the GC root
                    // slot and the variable slot are the SAME memory.  If we used
                    // an SSA variable for the value and a separate stack slot for
                    // the root, a reassignment (Stmt::Assign) would update the SSA
                    // variable but leave the root slot stale, allowing the GC to
                    // see old (possibly freed) pointers and collect the live new one.
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
                    VarStorage::Stack {
                        slot,
                        ty: ast_ty.clone(),
                    }
                } else {
                    let ty = clif_type(&ast_ty);
                    let var = self.builder.declare_var(ty);
                    self.builder.def_var(var, val);
                    VarStorage::Value {
                        var,
                        ty: ast_ty.clone(),
                    }
                };
                self.vars.insert(s.name.clone(), storage);
            }
            Stmt::Assign(s) => {
                if let Some(storage) = self.vars.get(&s.name).cloned() {
                    let target_ty = storage.ty().clone();
                    let val = self.emit_expr_coerced(&s.value, &target_ty);
                    self.store_var(&storage, val);
                }
            }
            Stmt::FieldAssign(s) => {
                let ptr = self.emit_expr(&s.object);
                if self.build_mode == BuildMode::Debug {
                    self.emit_nil_check(ptr, s.object.span(), &s.field);
                }
                let obj_type = self.ast_type_of(&s.object);
                if let Some(class_name) = class_name_for_object_type(&obj_type) {
                    if let Some(layout) = self.class_layouts.get(&class_name).cloned() {
                        if let Some(idx) = layout.iter().position(|(n, _)| n == &s.field) {
                            // Word 0 is type_id; fields start at word 1 → offset = (idx + 1) * 8.
                            let offset = (idx as i32 + 1) * 8;
                            // Box a class value when the field's type is an interface.
                            let field_ty = layout[idx].1.clone();
                            let val = self.emit_expr_coerced(&s.value, &field_ty);
                            self.builder.ins().store(MemFlags::new(), val, ptr, offset);
                        }
                    }
                }
            }
            Stmt::SuperInit(s) => self.emit_super_init(s),
            Stmt::StaticFieldAssign(s) => {
                // `ClassName::property = value` for a `static mut` property: store
                // into the global slot (willow-qsqf §13.4). The slot was rooted
                // once by __willow_static_init, so the new value is traced too.
                let class_name = self.static_call_class_name(&s.class);
                if let Some(info) = self.lookup_static_storage(&class_name, &s.field) {
                    let val = self.emit_expr_coerced(&s.value, &info.ty);
                    let ptr_ty = self.module.target_config().pointer_type();
                    let gv = self
                        .module
                        .declare_data_in_func(info.data_id, self.builder.func);
                    let addr = self.builder.ins().global_value(ptr_ty, gv);
                    self.builder.ins().store(MemFlags::new(), val, addr, 0);
                }
            }
            Stmt::IndexAssign(s) => {
                // Null and out-of-bounds are checked inside `willow_array_set`.
                let arr = self.emit_expr(&s.array);
                // Root the array while the value expression is evaluated — it may
                // allocate and trigger a collection.
                self.emit_push_root(arr);
                let idx = self.emit_expr(&s.index);
                let elem_ty = array_element_type(&self.ast_type_of(&s.array));
                // Box a class value when the array's element type is an interface.
                let val = self.emit_expr_coerced(&s.value, &elem_ty);
                let word = self.coerce_to_i64(val, &elem_ty);
                let set_id = self.func_id("willow_array_set");
                let set_ref = self.module.declare_func_in_func(set_id, self.builder.func);
                self.builder.ins().call(set_ref, &[arr, idx, word]);
                self.emit_pop_roots_n(1);
                self.gc_root_count -= 1;
            }
            Stmt::If(s) => self.emit_if(s),
            Stmt::While(s) => self.emit_while(s),
            Stmt::For(s) => self.emit_for(s),
            Stmt::Return(s) => {
                // `fn main() -> Result<void, E>`: returns are turned into an exit
                // (Err -> report + non-zero; Ok / bare return -> exit 0), since
                // `willow_user_main` is void (willow-exg).
                if self.main_result_err_ty.is_some() {
                    match &s.value {
                        Some(val_expr) if is_zero_arg_result_ok(val_expr) => {
                            // `return Result::Ok();` — success, no construction.
                            if self.gc_root_count > 0 {
                                self.emit_pop_roots_n(self.gc_root_count);
                            }
                            self.builder.ins().return_(&[]);
                        }
                        Some(val_expr) => {
                            let result = self.emit_expr(val_expr);
                            self.emit_main_result_exit(result);
                        }
                        None => {
                            if self.gc_root_count > 0 {
                                self.emit_pop_roots_n(self.gc_root_count);
                            }
                            self.builder.ins().return_(&[]);
                        }
                    }
                    self.terminated = true;
                    return;
                }
                if self.is_async {
                    let future = if let Some(val_expr) = &s.value {
                        if self.return_type == Type::Void {
                            self.emit_expr(val_expr);
                            self.emit_ready_future_void()
                        } else {
                            let return_type = self.return_type.clone();
                            let val = self.emit_expr(val_expr);
                            self.emit_ready_future(&return_type, val)
                        }
                    } else {
                        self.emit_ready_future_void()
                    };
                    if self.gc_root_count > 0 {
                        self.emit_pop_roots_n(self.gc_root_count);
                    }
                    self.builder.ins().return_(&[future]);
                } else {
                    if let Some(val_expr) = &s.value {
                        // Evaluate the return value BEFORE popping roots (it may load from GC objects).
                        let target = self.return_type.clone();
                        let val = self.emit_expr_coerced(val_expr, &target);
                        if self.gc_root_count > 0 {
                            self.emit_pop_roots_n(self.gc_root_count);
                        }
                        self.builder.ins().return_(&[val]);
                    } else {
                        if self.gc_root_count > 0 {
                            self.emit_pop_roots_n(self.gc_root_count);
                        }
                        self.builder.ins().return_(&[]);
                    }
                }
                self.terminated = true;
            }
            Stmt::Expr(s) => {
                self.emit_expr(&s.expr);
            }
        }
    }

    pub(super) fn emit_if(&mut self, s: &IfStmt) {
        let cond = self.emit_expr(&s.cond);

        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let merge_block = self.builder.create_block();

        self.builder
            .ins()
            .brif(cond, then_block, &[], else_block, &[]);

        // then branch
        self.builder.switch_to_block(then_block);
        self.builder.seal_block(then_block);
        let outer_terminated = self.terminated;
        self.terminated = false;
        self.emit_block(&s.then_block);
        let then_terminated = self.terminated;
        if !self.terminated {
            self.builder.ins().jump(merge_block, &[]);
        }

        // else branch
        self.builder.switch_to_block(else_block);
        self.builder.seal_block(else_block);
        self.terminated = false;
        if let Some(else_b) = &s.else_block {
            self.emit_block(else_b);
        }
        let else_terminated = self.terminated;
        if !self.terminated {
            self.builder.ins().jump(merge_block, &[]);
        }

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.terminated = outer_terminated || (then_terminated && else_terminated);
    }

    pub(super) fn emit_while(&mut self, s: &WhileStmt) {
        let header = self.builder.create_block();
        let body_block = self.builder.create_block();
        let exit_block = self.builder.create_block();

        self.builder.ins().jump(header, &[]);

        self.builder.switch_to_block(header);
        let cond = self.emit_expr(&s.cond);
        self.builder
            .ins()
            .brif(cond, body_block, &[], exit_block, &[]);

        self.builder.switch_to_block(body_block);
        self.builder.seal_block(body_block);
        self.terminated = false;
        self.emit_block(&s.body);
        if !self.terminated {
            self.builder.ins().jump(header, &[]);
        }

        self.builder.seal_block(header);
        self.builder.switch_to_block(exit_block);
        self.builder.seal_block(exit_block);
        self.terminated = false;
    }

    pub(super) fn emit_for(&mut self, s: &ForStmt) {
        if let Expr::Range(range) = &s.iterable {
            self.emit_range_for(s, range);
            return;
        }

        let saved_vars = self.vars.clone();
        let roots_before = self.gc_root_count;
        let iterable_ty = self.ast_type_of(&s.iterable);
        // Iterating a `Range<i64>` held as a value.
        if matches!(&iterable_ty, Type::Generic(n, _) if n == "Range") {
            self.emit_range_for_value(s);
            self.vars = saved_vars;
            self.gc_root_count = roots_before;
            return;
        }
        let elem_ty = array_element_type(&iterable_ty);

        let arr = self.emit_expr(&s.iterable);
        self.emit_push_root(arr);

        let idx_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            0,
        ));
        let zero = self.builder.ins().iconst(types::I64, 0);
        self.builder.ins().stack_store(zero, idx_slot, 0);

        if s.name != "_" {
            let elem_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                0,
            ));
            if is_gc_managed(&elem_ty, self.enum_infos) {
                let nil = self.builder.ins().iconst(types::I64, 0);
                self.builder.ins().stack_store(nil, elem_slot, 0);
                self.emit_push_root_slot(elem_slot);
            }
            self.vars.insert(
                s.name.clone(),
                VarStorage::Stack {
                    slot: elem_slot,
                    ty: elem_ty.clone(),
                },
            );
        }

        let header = self.builder.create_block();
        let body_block = self.builder.create_block();
        let exit_block = self.builder.create_block();
        self.builder.ins().jump(header, &[]);

        self.builder.switch_to_block(header);
        let idx = self.builder.ins().stack_load(types::I64, idx_slot, 0);
        let len_id = self.func_id("willow_array_len");
        let len_ref = self.module.declare_func_in_func(len_id, self.builder.func);
        let len_call = self.builder.ins().call(len_ref, &[arr]);
        let len = self.builder.inst_results(len_call)[0];
        let keep_going = self.builder.ins().icmp(IntCC::SignedLessThan, idx, len);
        self.builder
            .ins()
            .brif(keep_going, body_block, &[], exit_block, &[]);

        self.builder.switch_to_block(body_block);
        self.builder.seal_block(body_block);
        if let Some(VarStorage::Stack { slot, .. }) = self.vars.get(&s.name).cloned() {
            let get_id = self.func_id("willow_array_get");
            let get_ref = self.module.declare_func_in_func(get_id, self.builder.func);
            let call = self.builder.ins().call(get_ref, &[arr, idx]);
            let word = self.builder.inst_results(call)[0];
            let elem = self.coerce_i64_to(word, &elem_ty);
            self.builder.ins().stack_store(elem, slot, 0);
        }
        self.terminated = false;
        self.emit_block(&s.body);
        if !self.terminated {
            let idx = self.builder.ins().stack_load(types::I64, idx_slot, 0);
            let one = self.builder.ins().iconst(types::I64, 1);
            let next = self.builder.ins().iadd(idx, one);
            self.builder.ins().stack_store(next, idx_slot, 0);
            self.builder.ins().jump(header, &[]);
        }

        self.builder.seal_block(header);
        self.builder.switch_to_block(exit_block);
        self.builder.seal_block(exit_block);
        let loop_roots = self.gc_root_count - roots_before;
        if loop_roots > 0 {
            self.emit_pop_roots_n(loop_roots);
        }
        self.gc_root_count = roots_before;
        self.vars = saved_vars;
        self.terminated = false;
    }

    /// Materialize a `Range<i64>` value: a 2-word GC heap object `[start, end]`
    /// (both `i64`, so no GC-ref slots). Used when a range is held as a value
    /// rather than driven inline by a `for` loop.
    pub(super) fn emit_range_value(&mut self, range: &RangeExpr) -> cranelift_codegen::ir::Value {
        // start/end are i64 scalars (not GC), computed before the only allocation,
        // so they survive it in registers without rooting.
        let start = self.emit_expr(&range.start);
        let end = self.emit_expr(&range.end);
        let size = self.builder.ins().iconst(types::I64, 16);
        let mask = self.builder.ins().iconst(types::I64, 0);
        let alloc_id = self.func_id("willow_alloc_typed");
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self.builder.ins().call(alloc_ref, &[size, mask]);
        let ptr = self.builder.inst_results(call)[0];
        self.builder.ins().store(MemFlags::new(), start, ptr, 0i32);
        self.builder.ins().store(MemFlags::new(), end, ptr, 8i32);
        ptr
    }

    pub(super) fn emit_range_for(&mut self, s: &ForStmt, range: &RangeExpr) {
        let start = self.emit_expr(&range.start);
        let end = self.emit_expr(&range.end);
        self.emit_range_for_bounds(s, start, end);
    }

    /// Iterate a `Range<i64>` VALUE (a variable or call result, not an inline
    /// literal): load its `start`/`end` words and drive the same counting loop.
    pub(super) fn emit_range_for_value(&mut self, s: &ForStmt) {
        let ptr = self.emit_expr(&s.iterable);
        let start = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let end = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        self.emit_range_for_bounds(s, start, end);
    }

    pub(super) fn emit_range_for_bounds(
        &mut self,
        s: &ForStmt,
        start: cranelift_codegen::ir::Value,
        end: cranelift_codegen::ir::Value,
    ) {
        let saved_vars = self.vars.clone();

        let current_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            0,
        ));
        let end_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            8,
            0,
        ));
        self.builder.ins().stack_store(start, current_slot, 0);
        self.builder.ins().stack_store(end, end_slot, 0);

        if s.name != "_" {
            let elem_slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                0,
            ));
            self.vars.insert(
                s.name.clone(),
                VarStorage::Stack {
                    slot: elem_slot,
                    ty: Type::I64,
                },
            );
        }

        let header = self.builder.create_block();
        let body_block = self.builder.create_block();
        let exit_block = self.builder.create_block();
        self.builder.ins().jump(header, &[]);

        self.builder.switch_to_block(header);
        let current = self.builder.ins().stack_load(types::I64, current_slot, 0);
        let end = self.builder.ins().stack_load(types::I64, end_slot, 0);
        let keep_going = self.builder.ins().icmp(IntCC::SignedLessThan, current, end);
        self.builder
            .ins()
            .brif(keep_going, body_block, &[], exit_block, &[]);

        self.builder.switch_to_block(body_block);
        self.builder.seal_block(body_block);
        if let Some(VarStorage::Stack { slot, .. }) = self.vars.get(&s.name).cloned() {
            self.builder.ins().stack_store(current, slot, 0);
        }
        self.terminated = false;
        self.emit_block(&s.body);
        if !self.terminated {
            let current = self.builder.ins().stack_load(types::I64, current_slot, 0);
            let one = self.builder.ins().iconst(types::I64, 1);
            let next = self.builder.ins().iadd(current, one);
            self.builder.ins().stack_store(next, current_slot, 0);
            self.builder.ins().jump(header, &[]);
        }

        self.builder.seal_block(header);
        self.builder.switch_to_block(exit_block);
        self.builder.seal_block(exit_block);
        self.vars = saved_vars;
        self.terminated = false;
    }

    pub(super) fn emit_expr(&mut self, expr: &Expr) -> cranelift_codegen::ir::Value {
        match expr {
            Expr::Integer(n, _) => self.builder.ins().iconst(types::I64, *n),
            Expr::Float(f, _) => self.builder.ins().f64const(*f),
            Expr::Bool(b, _) => self.builder.ins().iconst(types::I8, if *b { 1 } else { 0 }),
            Expr::Nil(_) => self.builder.ins().iconst(types::I64, 0),
            Expr::String(value, _) => self.emit_string_literal(value),
            Expr::Var(name, _) => {
                // Local variable or function value?
                if let Some(storage) = self.vars.get(name.as_str()).cloned() {
                    return self.load_var(&storage);
                }
                // Named function used as a first-class value — emit its address.
                if let Some(&fid) = self.func_ids.get(name.as_str()) {
                    let fref = self.module.declare_func_in_func(fid, self.builder.func);
                    return self.builder.ins().func_addr(types::I64, fref);
                }
                // Should not reach here after type checking, but produce a safe zero.
                self.builder.ins().iconst(types::I64, 0)
            }
            Expr::Binary(b) => self.emit_binary(b),
            Expr::Unary(u) => self.emit_unary(u),
            Expr::Call(c) => self.emit_call(c),
            Expr::Print(arg, newline, _) => {
                let val = self.emit_expr(arg);
                let arg_ty_raw = self.ast_type_of(arg);
                // Unwrap Nullable so that printing a nil-checked T? behaves like T.
                let arg_ty = match arg_ty_raw {
                    Type::Nullable(inner) => *inner,
                    ty => ty,
                };
                let fn_name = match (arg_ty, newline) {
                    (Type::I64, false) => "willow_print_i64",
                    (Type::I64, true) => "willow_println_i64",
                    (Type::F64, false) => "willow_print_f64",
                    (Type::F64, true) => "willow_println_f64",
                    (Type::Bool, false) => "willow_print_bool",
                    (Type::Bool, true) => "willow_println_bool",
                    (Type::String, false) => "willow_print_string",
                    (Type::String, true) => "willow_println_string",
                    _ => "",
                };
                if !fn_name.is_empty() {
                    let fid = self.func_ids[fn_name];
                    let fref = self.module.declare_func_in_func(fid, self.builder.func);
                    self.builder.ins().call(fref, &[val]);
                }
                self.builder.ins().iconst(types::I8, 0)
            }
            Expr::Ternary(t) => self.emit_ternary(t),
            Expr::Range(r) => self.emit_range_value(r),
            // Lambda: emit the address of the pre-compiled private function.
            Expr::Lambda(l) => {
                if let Some(name) = self.lambda_names.get(&l.span) {
                    if let Some(&fid) = self.func_ids.get(name.as_str()) {
                        let fref = self.module.declare_func_in_func(fid, self.builder.func);
                        return self.builder.ins().func_addr(types::I64, fref);
                    }
                }
                self.builder.ins().iconst(types::I64, 0)
            }
            // Field/method access: codegen deferred to willow-jbf
            Expr::FieldAccess(obj, field_name, _) => self.emit_field_access(obj, field_name),
            Expr::MethodCall(m) => self.emit_method_call(m),
            Expr::ObjectLiteral(o) => self.emit_object_literal(o),
            Expr::StaticCall(s) => self.emit_static_call(s),
            Expr::StaticField(s) => self.emit_static_field_read(&s.class, &s.field),
            Expr::New(n) => self.emit_new(n),
            Expr::Await(a) => self.emit_await(a),
            Expr::Select(s) => {
                self.emit_select(s);
                self.builder.ins().iconst(types::I8, 0)
            }
            Expr::Match(m) => self.emit_match(m),
            Expr::TryPropagate(inner, _) => self.emit_try_propagate(inner),
            Expr::ArrayLiteral(elements, _) => {
                let elem_ty = elements
                    .first()
                    .map(|e| self.ast_type_of(e))
                    .unwrap_or(Type::Void);
                self.emit_array_literal(elements, &elem_ty)
            }
            Expr::Index(arr, index, _) => {
                // Null and out-of-bounds are checked inside `willow_array_get`,
                // which aborts with a clear message.
                let elem_ty = array_element_type(&self.ast_type_of(arr));
                self.emit_index(arr, index, &elem_ty)
            }
        }
    }

    pub(super) fn emit_string_literal(&mut self, value: &str) -> cranelift_codegen::ir::Value {
        if let Some(data_id) = self.string_literals.get(value) {
            // Load the address of the static raw bytes.
            let gv = self
                .module
                .declare_data_in_func(*data_id, self.builder.func);
            let ptr_ty = self.module.target_config().pointer_type();
            let bytes_ptr = self.builder.ins().global_value(ptr_ty, gv);
            // Call willow_string_literal to get (or create) a permanent WillowString.
            let len_val = self.builder.ins().iconst(types::I64, value.len() as i64);
            let fid = self.func_id("willow_string_literal");
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let call = self.builder.ins().call(fref, &[bytes_ptr, len_val]);
            return self.builder.inst_results(call)[0];
        }
        self.builder.ins().iconst(types::I64, 0)
    }

    pub(super) fn emit_binary(&mut self, b: &BinaryExpr) -> cranelift_codegen::ir::Value {
        let lhs = self.emit_expr(&b.lhs);
        let lty = self.ast_type_of(&b.lhs);
        let is_float = lty == Type::F64;
        match &b.op {
            BinOp::And => self.emit_short_circuit_and(lhs, &b.rhs),
            BinOp::Or => self.emit_short_circuit_or(lhs, &b.rhs),
            BinOp::Add => {
                if lty == Type::String {
                    // Root lhs before evaluating rhs: rhs evaluation may call
                    // willow_alloc_typed (e.g. concat or object allocation) which
                    // triggers a GC cycle.  Without rooting, an intermediate concat
                    // result held only in lhs's SSA register would be freed.
                    self.emit_push_root(lhs);
                    let rhs = self.emit_expr(&b.rhs);
                    // Root rhs before the concat call itself, which also allocates.
                    self.emit_push_root(rhs);
                    let fid = self.func_id("willow_string_concat");
                    let fref = self.module.declare_func_in_func(fid, self.builder.func);
                    let call = self.builder.ins().call(fref, &[lhs, rhs]);
                    let result = self.builder.inst_results(call)[0];
                    // Pop the two temporary roots (net gc_root_count change is 0).
                    self.emit_pop_roots_n(2);
                    self.gc_root_count -= 2;
                    result
                } else {
                    let rhs = self.emit_expr(&b.rhs);
                    if is_float {
                        self.builder.ins().fadd(lhs, rhs)
                    } else {
                        self.builder.ins().iadd(lhs, rhs)
                    }
                }
            }
            BinOp::Sub => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    self.builder.ins().fsub(lhs, rhs)
                } else {
                    self.builder.ins().isub(lhs, rhs)
                }
            }
            BinOp::Mul => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    self.builder.ins().fmul(lhs, rhs)
                } else {
                    self.builder.ins().imul(lhs, rhs)
                }
            }
            BinOp::Div => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    self.builder.ins().fdiv(lhs, rhs)
                } else {
                    self.builder.ins().sdiv(lhs, rhs)
                }
            }
            BinOp::Rem => {
                let rhs = self.emit_expr(&b.rhs);
                self.builder.ins().srem(lhs, rhs)
            }
            BinOp::Lt => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::LessThan, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedLessThan, lhs, rhs)
                }
            }
            BinOp::Le => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::LessThanOrEqual, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedLessThanOrEqual, lhs, rhs)
                }
            }
            BinOp::Gt => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::GreaterThan, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedGreaterThan, lhs, rhs)
                }
            }
            BinOp::Ge => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::GreaterThanOrEqual, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::SignedGreaterThanOrEqual, lhs, rhs)
                }
            }
            BinOp::Eq => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::Equal, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::Equal, lhs, rhs)
                }
            }
            BinOp::Ne => {
                let rhs = self.emit_expr(&b.rhs);
                if is_float {
                    fcmp_to_i8(self.builder, FloatCC::NotEqual, lhs, rhs)
                } else {
                    icmp_to_i8(self.builder, IntCC::NotEqual, lhs, rhs)
                }
            }
        }
    }

    pub(super) fn emit_short_circuit_and(
        &mut self,
        lhs: cranelift_codegen::ir::Value,
        rhs_expr: &Expr,
    ) -> cranelift_codegen::ir::Value {
        let result_var = self.builder.declare_var(types::I8);
        let rhs_block = self.builder.create_block();
        let false_block = self.builder.create_block();
        let merge_block = self.builder.create_block();

        self.builder
            .ins()
            .brif(lhs, rhs_block, &[], false_block, &[]);

        self.builder.switch_to_block(rhs_block);
        self.builder.seal_block(rhs_block);
        let rhs = self.emit_expr(rhs_expr);
        self.builder.def_var(result_var, rhs);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(false_block);
        self.builder.seal_block(false_block);
        let false_value = self.builder.ins().iconst(types::I8, 0);
        self.builder.def_var(result_var, false_value);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.builder.use_var(result_var)
    }

    pub(super) fn emit_short_circuit_or(
        &mut self,
        lhs: cranelift_codegen::ir::Value,
        rhs_expr: &Expr,
    ) -> cranelift_codegen::ir::Value {
        let result_var = self.builder.declare_var(types::I8);
        let true_block = self.builder.create_block();
        let rhs_block = self.builder.create_block();
        let merge_block = self.builder.create_block();

        self.builder
            .ins()
            .brif(lhs, true_block, &[], rhs_block, &[]);

        self.builder.switch_to_block(true_block);
        self.builder.seal_block(true_block);
        let true_value = self.builder.ins().iconst(types::I8, 1);
        self.builder.def_var(result_var, true_value);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(rhs_block);
        self.builder.seal_block(rhs_block);
        let rhs = self.emit_expr(rhs_expr);
        self.builder.def_var(result_var, rhs);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.builder.use_var(result_var)
    }

    pub(super) fn emit_unary(&mut self, u: &UnaryExpr) -> cranelift_codegen::ir::Value {
        let val = self.emit_expr(&u.expr);
        let ty = ast_type_of_expr(&u.expr, &self.vars, self.func_return_types);
        match &u.op {
            UnaryOp::Neg => {
                if ty == Type::F64 {
                    self.builder.ins().fneg(val)
                } else {
                    self.builder.ins().ineg(val)
                }
            }
            UnaryOp::Not => {
                let one = self.builder.ins().iconst(types::I8, 1);
                self.builder.ins().bxor(val, one)
            }
        }
    }

    pub(super) fn emit_call(&mut self, c: &CallExpr) -> cranelift_codegen::ir::Value {
        if c.callee == "format" {
            return self.emit_format_call(c);
        }

        // An unqualified enum-variant construction (`Ok(42)`) the type checker
        // resolved to an enum: lower like the qualified `Enum::Variant(..)` form
        // (willow-60o.1).
        if let Some(enum_name) = self.enum_variant_resolutions.get(&c.span).cloned() {
            if let Some(enum_info) = self.enum_infos.get(&enum_name).cloned() {
                if let Some(variant) = enum_info.variants.iter().find(|v| v.name == c.callee) {
                    if variant.payload_types.is_empty()
                        && !self.enum_is_gc_object_type(&enum_name)
                    {
                        return self.builder.ins().iconst(types::I64, variant.tag);
                    }
                    if variant.payload_types.is_empty() {
                        return self.emit_enum_variant_alloc(variant.tag, &[]);
                    }
                    return self.emit_enum_variant_alloc(variant.tag, &c.args);
                }
            }
        }

        // Direct call to a known function.
        if let Some(&fid) = self.func_ids.get(&c.callee) {
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let modes = self.func_param_modes.get(&c.callee).cloned();
            let param_debug = self.func_param_debug.get(&c.callee).cloned();
            let param_types = self.fn_param_types(&c.callee);
            let has_reference_args = has_reference_args(modes.as_deref(), &c.args);
            let (args, temp_roots) = self.emit_call_args_rooted_coerced(
                Some(&c.callee),
                modes.as_deref(),
                param_debug.as_deref(),
                param_types.as_deref(),
                &c.args,
            );
            // Debug builds: record this call on the call-chain stack so a panic
            // inside the callee prints an ordered trace (willow-992h). Pushed
            // after args are evaluated (so nested calls nest correctly) and
            // popped right after the call returns.
            let pushed_frame = self.emit_callstack_push(&c.callee, c.span);
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            let result = if results.is_empty() {
                self.builder.ins().iconst(types::I8, 0)
            } else {
                results[0]
            };
            if pushed_frame {
                self.emit_callstack_pop();
            }
            if has_reference_args {
                self.emit_debug_reference_call_clear();
            }
            self.emit_pop_roots_n(temp_roots);
            self.gc_root_count -= temp_roots;
            return result;
        }

        // panic(message) — call willow_panic and trap (noreturn).
        if c.callee == "panic" {
            let msg = c
                .args
                .first()
                .map(|a| self.emit_expr(&a.expr))
                .unwrap_or_else(|| {
                    let empty = self.emit_string_literal("explicit panic");
                    empty
                });
            // Debug builds report the panic source location via willow_panic_at;
            // release builds use the plain willow_panic (willow-4j6).
            if self.build_mode == BuildMode::Debug {
                let source_file = self.source_file.to_string();
                let file_ptr = self.emit_string_literal(&source_file);
                let line = self.builder.ins().iconst(types::I32, c.span.line as i64);
                let col = self.builder.ins().iconst(types::I32, c.span.col as i64);
                let fid = self.func_id("willow_panic_at");
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder.ins().call(fref, &[msg, file_ptr, line, col]);
            } else {
                let fid = self.func_id("willow_panic");
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder.ins().call(fref, &[msg]);
            }
            // Produce the (unreachable) result value BEFORE the trap: `trap`
            // terminates the block, so no instruction may follow it. willow_panic
            // is noreturn; the trap just gives the block a terminator.
            let result = self.builder.ins().iconst(types::I64, 0);
            self.builder.ins().trap(TrapCode::unwrap_user(1));
            self.terminated = true;
            return result;
        }

        if let Some(runtime_name) = builtin_call_runtime_name(&c.callee) {
            let fid = self.func_ids[runtime_name];
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let args: Vec<_> = c.args.iter().map(|a| self.emit_expr(&a.expr)).collect();
            let call = self.builder.ins().call(fref, &args);
            let results = self.builder.inst_results(call);
            return if results.is_empty() {
                self.builder.ins().iconst(types::I8, 0)
            } else {
                results[0]
            };
        }

        // Indirect call through a function-value local variable.
        if let Some(storage) = self.vars.get(&c.callee).cloned() {
            if let Type::Fn(param_types, ret_type) = storage.ty().clone() {
                let callee_val = self.load_var(&storage);
                let args: Vec<_> = c.args.iter().map(|a| self.emit_expr(&a.expr)).collect();

                // Build the Cranelift signature matching the function type.
                let mut sig = self.module.make_signature();
                for pt in &param_types {
                    sig.params.push(AbiParam::new(clif_type(pt)));
                }
                let ret_clif = clif_type(&ret_type);
                let has_return = *ret_type != Type::Void;
                if has_return {
                    sig.returns.push(AbiParam::new(ret_clif));
                }
                let sig_ref = self.builder.import_signature(sig);
                let call = self.builder.ins().call_indirect(sig_ref, callee_val, &args);
                let results = self.builder.inst_results(call);
                return if results.is_empty() {
                    self.builder.ins().iconst(types::I8, 0)
                } else {
                    results[0]
                };
            }
        }

        // Should not reach here after type checking.
        self.builder.ins().iconst(types::I64, 0)
    }

    /// Evaluate call arguments left-to-right, pushing each GC-managed argument
    /// value as a temporary root before evaluating subsequent arguments.
    /// Returns (arg_values, number_of_temporary_roots_pushed).
    /// The caller must call emit_pop_roots_n(temp_roots) + gc_root_count -= temp_roots
    /// immediately after emitting the call instruction.
    pub(super) fn emit_call_args_rooted(
        &mut self,
        callee: Option<&str>,
        modes: Option<&[ParamMode]>,
        param_debug: Option<&[ParamDebug]>,
        args: &[CallArg],
    ) -> (Vec<cranelift_codegen::ir::Value>, usize) {
        self.emit_call_args_rooted_coerced(callee, modes, param_debug, None, args)
    }

    /// Like [`emit_call_args_rooted`] but, when `param_types` is provided, boxes
    /// each class argument passed to an interface-typed parameter (spec §9.3).
    pub(super) fn emit_call_args_rooted_coerced(
        &mut self,
        callee: Option<&str>,
        modes: Option<&[ParamMode]>,
        param_debug: Option<&[ParamDebug]>,
        param_types: Option<&[Type]>,
        args: &[CallArg],
    ) -> (Vec<cranelift_codegen::ir::Value>, usize) {
        let mut values = Vec::with_capacity(args.len());
        let mut temp_roots = 0usize;

        for (idx, arg) in args.iter().enumerate() {
            let is_ref = matches!(
                modes.and_then(|m| m.get(idx)),
                Some(ParamMode::Reference { .. })
            );
            let (val, reference_roots) = if is_ref {
                self.emit_debug_reference_call_hook(callee, idx, arg, modes, param_debug);
                self.emit_reference_arg_address(arg)
            } else {
                let raw = self.emit_expr(&arg.expr);
                // Box a class argument when the parameter type is an interface.
                let coerced = match param_types.and_then(|pt| pt.get(idx)) {
                    Some(target) => {
                        let value_ty = self.ast_type_of(&arg.expr);
                        self.coerce_to_target(raw, &value_ty, target)
                    }
                    None => raw,
                };
                (coerced, 0)
            };
            values.push(val);
            temp_roots += reference_roots;

            // Root GC-managed arguments so later argument evaluations (which may
            // allocate and trigger GC) cannot collect them.
            if !is_ref {
                let arg_ty = self.ast_type_of_init(&arg.expr);
                if is_gc_managed(&arg_ty, self.enum_infos) {
                    self.emit_push_root(val);
                    temp_roots += 1;
                }
            }
        }

        (values, temp_roots)
    }

    pub(super) fn emit_debug_reference_call_hook(
        &mut self,
        callee: Option<&str>,
        idx: usize,
        arg: &CallArg,
        modes: Option<&[ParamMode]>,
        param_debug: Option<&[ParamDebug]>,
    ) {
        if self.build_mode != BuildMode::Debug {
            return;
        }

        let ampersand_span = match &arg.mode {
            CallArgMode::Reference { ampersand_span } => *ampersand_span,
            CallArgMode::Value => return,
        };

        let param = param_debug.and_then(|params| params.get(idx));
        let param_mode = param
            .map(|param| &param.mode)
            .or_else(|| modes.and_then(|modes| modes.get(idx)));
        let mode = param_mode.map(reference_mode_name).unwrap_or("&");
        let param_name = param
            .map(|param| param.name.as_str())
            .unwrap_or("<unknown>");
        let param_type = param
            .map(|param| debug_type_name(&param.ty))
            .unwrap_or_else(|| "<unknown>".to_string());
        let callee = callee.unwrap_or("<unknown>");
        let place_kind = reference_place_kind(&arg.expr);
        let place_name = reference_place_name(&arg.expr);

        let source_file = self.source_file.to_string();
        let file_ptr = self.emit_string_literal(&source_file);
        let line_val = self
            .builder
            .ins()
            .iconst(types::I32, ampersand_span.line as i64);
        let col_val = self
            .builder
            .ins()
            .iconst(types::I32, ampersand_span.col as i64);
        let callee_ptr = self.emit_string_literal(callee);
        let param_ptr = self.emit_string_literal(param_name);
        let param_type_ptr = self.emit_string_literal(&param_type);
        let mode_ptr = self.emit_string_literal(mode);
        let place_kind_ptr = self.emit_string_literal(place_kind);
        let place_name_ptr = self.emit_string_literal(&place_name);
        let hook_id = self.func_id("willow_debug_reference_call");
        let hook_ref = self.module.declare_func_in_func(hook_id, self.builder.func);
        self.builder.ins().call(
            hook_ref,
            &[
                file_ptr,
                line_val,
                col_val,
                callee_ptr,
                param_ptr,
                param_type_ptr,
                mode_ptr,
                place_kind_ptr,
                place_name_ptr,
            ],
        );
    }

    pub(super) fn emit_debug_reference_call_clear(&mut self) {
        if self.build_mode != BuildMode::Debug {
            return;
        }
        let clear_id = self.func_id("willow_debug_reference_call_clear");
        let clear_ref = self
            .module
            .declare_func_in_func(clear_id, self.builder.func);
        self.builder.ins().call(clear_ref, &[]);
    }

    /// Address + length of a declared string literal's raw static UTF-8 bytes,
    /// without interning a (GC-heap) WillowString. `None` if the literal was not
    /// collected/declared.
    pub(super) fn emit_static_str_bytes(
        &mut self,
        value: &str,
    ) -> Option<(cranelift_codegen::ir::Value, cranelift_codegen::ir::Value)> {
        let data_id = *self.string_literals.get(value)?;
        let gv = self.module.declare_data_in_func(data_id, self.builder.func);
        let ptr_ty = self.module.target_config().pointer_type();
        let bytes_ptr = self.builder.ins().global_value(ptr_ty, gv);
        let len = self.builder.ins().iconst(types::I64, value.len() as i64);
        Some((bytes_ptr, len))
    }

    /// Debug builds: push a call-chain frame (callee name + call-site location)
    /// before a user-function call. Returns `true` when a frame was pushed (so
    /// the caller knows to emit the matching pop). Passes raw static bytes (not
    /// WillowStrings) so the call stack does not allocate on the GC heap. Release
    /// builds are untouched (willow-992h).
    pub(super) fn emit_callstack_push(
        &mut self,
        callee: &str,
        span: crate::diagnostics::Span,
    ) -> bool {
        if self.build_mode != BuildMode::Debug {
            return false;
        }
        let Some((name_ptr, name_len)) = self.emit_static_str_bytes(callee) else {
            return false;
        };
        let file = self.source_file.to_string();
        let Some((file_ptr, file_len)) = self.emit_static_str_bytes(&file) else {
            return false;
        };
        let line = self.builder.ins().iconst(types::I32, span.line as i64);
        let col = self.builder.ins().iconst(types::I32, span.col as i64);
        let push_id = self.func_id("willow_callstack_push");
        let push_ref = self.module.declare_func_in_func(push_id, self.builder.func);
        self.builder.ins().call(
            push_ref,
            &[name_ptr, name_len, file_ptr, file_len, line, col],
        );
        true
    }

    /// Debug builds: pop the most recent call-chain frame after a call returns.
    pub(super) fn emit_callstack_pop(&mut self) {
        let pop_id = self.func_id("willow_callstack_pop");
        let pop_ref = self.module.declare_func_in_func(pop_id, self.builder.func);
        self.builder.ins().call(pop_ref, &[]);
    }

    pub(super) fn emit_reference_arg_address(
        &mut self,
        arg: &CallArg,
    ) -> (cranelift_codegen::ir::Value, usize) {
        match &arg.expr {
            Expr::Var(name, _) => {
                let storage = self.vars.get(name.as_str()).cloned();
                match storage {
                    Some(VarStorage::Stack { slot, .. }) => {
                        let ptr_ty = self.module.target_config().pointer_type();
                        return (self.builder.ins().stack_addr(ptr_ty, slot, 0), 0);
                    }
                    Some(VarStorage::Value { var, ty }) => {
                        // Lazy promotion: this variable is being passed by &mut for the first
                        // time.  Promote it from a Cranelift SSA variable to a stack slot so
                        // the callee can write through the pointer and future reads see the
                        // updated value.
                        let ptr_ty = self.module.target_config().pointer_type();
                        let val = self.builder.use_var(var);
                        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                            StackSlotKind::ExplicitSlot,
                            8,
                            0,
                        ));
                        self.builder.ins().stack_store(val, slot, 0);
                        let ty_clone = ty.clone();
                        self.vars
                            .insert(name.clone(), VarStorage::Stack { slot, ty: ty_clone });
                        return (self.builder.ins().stack_addr(ptr_ty, slot, 0), 0);
                    }
                    Some(VarStorage::ReferencePtr { var, .. }) => {
                        return (self.builder.use_var(var), 0);
                    }
                    Some(VarStorage::Frame { offset, .. }) => {
                        // Address of the frame slot (frame is GC-rooted, non-moving).
                        let base = self
                            .async_frame
                            .expect("frame-backed var requires an allocated async frame");
                        return (self.builder.ins().iadd_imm(base, offset as i64), 0);
                    }
                    None => {}
                }
            }
            Expr::FieldAccess(obj, field_name, span) => {
                return (self.emit_field_address(obj, field_name, *span), 0);
            }
            Expr::Index(array, index, _) => {
                return self.emit_array_element_address(array, index);
            }
            _ => {}
        }
        (self.builder.ins().iconst(types::I64, 0), 0)
    }

    pub(super) fn emit_field_address(
        &mut self,
        obj: &Expr,
        field_name: &str,
        _span: crate::diagnostics::Span,
    ) -> cranelift_codegen::ir::Value {
        let ptr = self.emit_expr(obj);

        if self.build_mode == BuildMode::Debug {
            self.emit_nil_check(ptr, obj.span(), field_name);
        }

        let obj_type = self.ast_type_of(obj);
        if let Some(class_name) = class_name_for_object_type(&obj_type) {
            if let Some(layout) = self.class_layouts.get(&class_name) {
                if let Some(idx) = layout.iter().position(|(n, _)| n == field_name) {
                    let offset = (idx as i64 + 1) * 8;
                    return self.builder.ins().iadd_imm(ptr, offset);
                }
            }
        }
        self.builder.ins().iconst(types::I64, 0)
    }

    pub(super) fn emit_array_element_address(
        &mut self,
        array: &Expr,
        index: &Expr,
    ) -> (cranelift_codegen::ir::Value, usize) {
        let arr = self.emit_expr(array);
        // Keep the array alive while evaluating the index and while the callee
        // reads/writes through the returned element slot pointer.
        self.emit_push_root(arr);
        let index = self.emit_expr(index);
        let addr_id = self.func_id("willow_array_element_addr");
        let addr_ref = self.module.declare_func_in_func(addr_id, self.builder.func);
        let call = self.builder.ins().call(addr_ref, &[arr, index]);
        (self.builder.inst_results(call)[0], 1)
    }

    pub(super) fn emit_format_call(&mut self, c: &CallExpr) -> cranelift_codegen::ir::Value {
        let runtime_name = match c.args.first().map(|arg| &arg.expr) {
            Some(Expr::String(spec, _)) => match spec.as_str() {
                "{:.17g}" => "willow_format_f64_17g",
                "{:.16f}" => "willow_format_f64_16f",
                "{:.6f}" => "willow_format_f64_6f",
                _ => "",
            },
            _ => "",
        };
        if runtime_name.is_empty() || c.args.len() < 2 {
            return self.builder.ins().iconst(types::I64, 0);
        }

        let value = self.emit_expr(&c.args[1].expr);
        let fid = self.func_ids[runtime_name];
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, &[value]);
        self.builder.inst_results(call)[0]
    }

    pub(super) fn emit_object_literal(
        &mut self,
        o: &ObjectLiteralExpr,
    ) -> cranelift_codegen::ir::Value {
        let layout = match self.class_layouts.get(&o.class).cloned() {
            Some(l) => l,
            None => return self.builder.ins().iconst(types::I64, 0),
        };
        // Object layout: word 0 = type_id (i64), words 1..N = fields.
        let size = (layout.len() as i64 + 1) * 8;
        let size_val = self.builder.ins().iconst(types::I64, size);
        let ref_mask = gc_ref_mask_for_layout(&o.class, &layout, self.enum_infos);
        let ref_mask_val = self.builder.ins().iconst(types::I64, ref_mask as i64);
        let alloc_id = self.func_id("willow_alloc_typed");
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self
            .builder
            .ins()
            .call(alloc_ref, &[size_val, ref_mask_val]);
        let ptr = self.builder.inst_results(call)[0];

        // Root ptr immediately: evaluating field initialiser expressions below
        // may trigger allocations and GC cycles before all fields are stored.
        // Without this root, GC could collect the partially-initialised object.
        self.emit_push_root(ptr);

        // Store the type_id at offset 0.
        let type_id = self.class_type_ids.get(&o.class).copied().unwrap_or(0);
        let type_id_val = self.builder.ins().iconst(types::I64, type_id);
        self.builder
            .ins()
            .store(MemFlags::new(), type_id_val, ptr, 0i32);

        // Store each field at offset (idx + 1) * 8 to leave word 0 for type_id.
        for field in &o.fields {
            if let Some(idx) = layout.iter().position(|(n, _)| n == &field.name) {
                let offset = (idx as i32 + 1) * 8;
                // Box a class value when the field's declared type is an interface.
                let field_ty = layout[idx].1.clone();
                let val = self.emit_expr_coerced(&field.value, &field_ty);
                self.builder.ins().store(MemFlags::new(), val, ptr, offset);
            }
        }

        // Pop the temporary construction root; the caller will root ptr via
        // its own let-binding or return value handling.
        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;

        ptr
    }

    /// Lower `new Class(args...)` (willow-scq2 §5/§12): allocate a zero-init
    /// object, call the explicit `Class__init(self, args...)` (or store args
    /// memberwise for the implicit constructor), and return the object.
    pub(super) fn emit_new(&mut self, n: &NewExpr) -> cranelift_codegen::ir::Value {
        let layout = match self.class_layouts.get(&n.class_name).cloned() {
            Some(l) => l,
            None => return self.builder.ins().iconst(types::I64, 0),
        };
        // Object layout: word 0 = type_id (i64), words 1..N = fields. Allocating
        // with the GC ref-mask leaves reference fields zero/null until assigned,
        // so a collection mid-construction is safe (willow-scq2 §12.3).
        let size = (layout.len() as i64 + 1) * 8;
        let size_val = self.builder.ins().iconst(types::I64, size);
        let ref_mask = gc_ref_mask_for_layout(&n.class_name, &layout, self.enum_infos);
        let ref_mask_val = self.builder.ins().iconst(types::I64, ref_mask as i64);
        let alloc_id = self.func_id("willow_alloc_typed");
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self
            .builder
            .ins()
            .call(alloc_ref, &[size_val, ref_mask_val]);
        let ptr = self.builder.inst_results(call)[0];

        let type_id = self.class_type_ids.get(&n.class_name).copied().unwrap_or(0);
        let type_id_val = self.builder.ins().iconst(types::I64, type_id);
        self.builder
            .ins()
            .store(MemFlags::new(), type_id_val, ptr, 0i32);

        // Root the new object across argument evaluation and the init body: both
        // may allocate and trigger a collection.
        self.emit_push_root(ptr);

        let mangled = class_method_symbol_name(self.known_modules, &n.class_name, "init");
        if let Some(&init_fid) = self.func_ids.get(&mangled) {
            // Explicit constructor. Param types come from the synthesized init
            // method's fn type (drop the leading `self`).
            let param_types: Vec<Type> = match self.fn_types.get(&mangled) {
                Some(Type::Fn(ps, _)) => ps.iter().skip(1).cloned().collect(),
                _ => Vec::new(),
            };
            let (arg_vals, arg_roots) = self.emit_call_args_rooted_coerced(
                Some(&mangled),
                None,
                None,
                Some(&param_types),
                &n.args,
            );
            let init_ref = self
                .module
                .declare_func_in_func(init_fid, self.builder.func);
            let mut call_args = vec![ptr];
            call_args.extend(arg_vals);
            self.builder.ins().call(init_ref, &call_args);
            if arg_roots > 0 {
                self.emit_pop_roots_n(arg_roots);
                self.gc_root_count -= arg_roots;
            }
        } else {
            // Implicit memberwise constructor: store each arg positionally into
            // its field slot (declaration order).
            for (i, arg) in n.args.iter().enumerate() {
                if let Some((_, field_ty)) = layout.get(i) {
                    let field_ty = field_ty.clone();
                    let val = self.emit_expr_coerced(&arg.expr, &field_ty);
                    let offset = (i as i32 + 1) * 8;
                    self.builder.ins().store(MemFlags::new(), val, ptr, offset);
                }
            }
        }

        // Drop the construction root; the caller roots `ptr` via its binding.
        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;
        ptr
    }

    /// Lower `super.init(args...)` inside a constructor. Explicit base
    /// constructors are normal `init` methods; implicit base constructors store
    /// memberwise args into the already-allocated `self` object's base slots.
    pub(super) fn emit_super_init(&mut self, s: &SuperInitStmt) {
        let Some(current_class) = self.current_class else {
            for arg in &s.args {
                self.emit_expr(&arg.expr);
            }
            return;
        };
        let Some(base_name) = self.class_base.get(current_class).cloned() else {
            for arg in &s.args {
                self.emit_expr(&arg.expr);
            }
            return;
        };
        let Some(self_storage) = self.vars.get("self").cloned() else {
            for arg in &s.args {
                self.emit_expr(&arg.expr);
            }
            return;
        };
        let self_ptr = self.load_var(&self_storage);

        let mangled = class_method_symbol_name(self.known_modules, &base_name, "init");
        if let Some(&init_fid) = self.func_ids.get(&mangled) {
            let param_types: Vec<Type> = match self.fn_types.get(&mangled) {
                Some(Type::Fn(ps, _)) => ps.iter().skip(1).cloned().collect(),
                _ => Vec::new(),
            };
            let (arg_vals, arg_roots) = self.emit_call_args_rooted_coerced(
                Some(&mangled),
                None,
                None,
                Some(&param_types),
                &s.args,
            );
            let init_ref = self
                .module
                .declare_func_in_func(init_fid, self.builder.func);
            let mut call_args = vec![self_ptr];
            call_args.extend(arg_vals);
            self.builder.ins().call(init_ref, &call_args);
            if arg_roots > 0 {
                self.emit_pop_roots_n(arg_roots);
                self.gc_root_count -= arg_roots;
            }
            return;
        }

        if let Some(layout) = self.class_layouts.get(&base_name).cloned() {
            for (i, arg) in s.args.iter().enumerate() {
                if let Some((_, field_ty)) = layout.get(i) {
                    let field_ty = field_ty.clone();
                    let val = self.emit_expr_coerced(&arg.expr, &field_ty);
                    let offset = (i as i32 + 1) * 8;
                    self.builder
                        .ins()
                        .store(MemFlags::new(), val, self_ptr, offset);
                }
            }
        } else {
            for arg in &s.args {
                self.emit_expr(&arg.expr);
            }
        }
    }

    /// Emit a nil pointer check in debug builds.
    /// If `ptr` is null at runtime, calls `willow_nil_deref` with source location and
    /// `context` (field or method name) then traps. Otherwise execution continues.
    pub(super) fn emit_nil_check(
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

        // ── nil branch: report and abort ──────────────────────────────────────
        self.builder.switch_to_block(nil_block);
        self.builder.seal_block(nil_block);

        let source_file = self.source_file.to_string();
        let context_owned = context.to_string();
        let file_ptr = self.emit_string_literal(&source_file);
        let ctx_ptr = self.emit_string_literal(&context_owned);
        let line_val = self.builder.ins().iconst(types::I32, span.line as i64);
        let col_val = self.builder.ins().iconst(types::I32, span.col as i64);

        let nil_deref_id = self.func_id("willow_nil_deref");
        let nil_deref_ref = self
            .module
            .declare_func_in_func(nil_deref_id, self.builder.func);
        self.builder
            .ins()
            .call(nil_deref_ref, &[file_ptr, line_val, col_val, ctx_ptr]);
        self.builder.ins().trap(TrapCode::unwrap_user(1));

        // ── ok branch: continue ───────────────────────────────────────────────
        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
    }

    pub(super) fn emit_field_access(
        &mut self,
        obj: &Expr,
        field_name: &str,
    ) -> cranelift_codegen::ir::Value {
        let ptr = self.emit_expr(obj);

        // Debug build: guard against nil dereference with a source-aware runtime error.
        if self.build_mode == BuildMode::Debug {
            let span = obj.span();
            self.emit_nil_check(ptr, span, field_name);
        }

        let obj_type = self.ast_type_of(obj);
        // Range<i64> bounds: word 0 = start, word 1 = end.
        if matches!(&obj_type, Type::Generic(n, _) if n == "Range") {
            let offset = if field_name == "end" { 8i32 } else { 0i32 };
            return self
                .builder
                .ins()
                .load(types::I64, MemFlags::new(), ptr, offset);
        }
        if let Some(class_name) = class_name_for_object_type(&obj_type) {
            if let Some(layout) = self.class_layouts.get(&class_name).cloned() {
                if let Some(idx) = layout.iter().position(|(n, _)| n == field_name) {
                    // Word 0 is type_id; fields start at word 1 → offset = (idx + 1) * 8.
                    let offset = (idx as i32 + 1) * 8;
                    let (_, field_ty) = &layout[idx];
                    let load_ty = clif_type(field_ty);
                    return self
                        .builder
                        .ins()
                        .load(load_ty, MemFlags::new(), ptr, offset);
                }
            }
        }
        self.builder.ins().iconst(types::I64, 0)
    }

    /// Emit code for Option<T> and Result<T,E> built-in helper methods.
    /// Returns Some(value) if the method was handled, None to fall through to
    /// the regular class-method dispatch.
    pub(super) fn emit_option_result_method_call(
        &mut self,
        enum_ptr: cranelift_codegen::ir::Value,
        obj_ty: &Type,
        m: &MethodCallExpr,
    ) -> Option<cranelift_codegen::ir::Value> {
        // Tag layout: Ok/Some = tag 0, Err/None = tag 1 (declaration order).
        const SOME_TAG: i64 = 0;
        const NONE_TAG: i64 = 1;
        const OK_TAG: i64 = 0;
        const ERR_TAG: i64 = 1;

        match obj_ty {
            Type::Generic(name, args) if name == "Option" => {
                let inner_ty = args.first().cloned().unwrap_or(Type::Void);
                match m.method.as_str() {
                    "is_some" => {
                        let tag =
                            self.builder
                                .ins()
                                .load(types::I64, MemFlags::new(), enum_ptr, 0i32);
                        let some = self.builder.ins().iconst(types::I64, SOME_TAG);
                        Some(self.builder.ins().icmp(IntCC::Equal, tag, some))
                    }
                    "is_none" => {
                        let tag =
                            self.builder
                                .ins()
                                .load(types::I64, MemFlags::new(), enum_ptr, 0i32);
                        let none = self.builder.ins().iconst(types::I64, NONE_TAG);
                        Some(self.builder.ins().icmp(IntCC::Equal, tag, none))
                    }
                    "unwrap" => {
                        let msg =
                            self.emit_string_literal("called `Option::unwrap()` on a `None` value");
                        Some(self.emit_enum_unwrap(enum_ptr, &inner_ty, SOME_TAG, msg))
                    }
                    "expect" => {
                        let msg = if let Some(arg) = m.args.first() {
                            self.emit_expr(&arg.expr)
                        } else {
                            self.emit_string_literal("called `Option::expect()` on a `None` value")
                        };
                        Some(self.emit_enum_unwrap(enum_ptr, &inner_ty, SOME_TAG, msg))
                    }
                    "unwrap_or" => {
                        let default_val = m
                            .args
                            .first()
                            .map(|a| self.emit_expr(&a.expr))
                            .unwrap_or_else(|| self.builder.ins().iconst(types::I64, 0));
                        Some(self.emit_enum_unwrap_or(enum_ptr, &inner_ty, SOME_TAG, default_val))
                    }
                    "map" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            let ret_ty = match &f_ty {
                                Type::Fn(_, ret) => *ret.clone(),
                                _ => Type::Void,
                            };
                            Some(self.emit_option_map(enum_ptr, &inner_ty, &ret_ty, f_val, &f_ty))
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    "and_then" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            Some(self.emit_option_and_then(enum_ptr, &inner_ty, f_val, &f_ty))
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    "or_else" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            Some(self.emit_option_or_else(enum_ptr, f_val, &f_ty))
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    _ => None,
                }
            }
            Type::Generic(name, args) if name == "Result" => {
                let ok_ty = args.first().cloned().unwrap_or(Type::Void);
                let err_ty = args.get(1).cloned().unwrap_or(Type::Void);
                match m.method.as_str() {
                    "is_ok" => {
                        let tag =
                            self.builder
                                .ins()
                                .load(types::I64, MemFlags::new(), enum_ptr, 0i32);
                        let ok = self.builder.ins().iconst(types::I64, OK_TAG);
                        Some(self.builder.ins().icmp(IntCC::Equal, tag, ok))
                    }
                    "is_err" => {
                        let tag =
                            self.builder
                                .ins()
                                .load(types::I64, MemFlags::new(), enum_ptr, 0i32);
                        let err = self.builder.ins().iconst(types::I64, ERR_TAG);
                        Some(self.builder.ins().icmp(IntCC::Equal, tag, err))
                    }
                    "unwrap" => {
                        let msg =
                            self.emit_string_literal("called `Result::unwrap()` on an `Err` value");
                        Some(self.emit_enum_unwrap(enum_ptr, &ok_ty, OK_TAG, msg))
                    }
                    "expect" => {
                        let msg = if let Some(arg) = m.args.first() {
                            self.emit_expr(&arg.expr)
                        } else {
                            self.emit_string_literal("called `Result::expect()` on an `Err` value")
                        };
                        Some(self.emit_enum_unwrap(enum_ptr, &ok_ty, OK_TAG, msg))
                    }
                    "unwrap_or" => {
                        let default_val = m
                            .args
                            .first()
                            .map(|a| self.emit_expr(&a.expr))
                            .unwrap_or_else(|| self.builder.ins().iconst(types::I64, 0));
                        Some(self.emit_enum_unwrap_or(enum_ptr, &ok_ty, OK_TAG, default_val))
                    }
                    "unwrap_err" => {
                        let msg = self
                            .emit_string_literal("called `Result::unwrap_err()` on an `Ok` value");
                        Some(self.emit_enum_unwrap(enum_ptr, &err_ty, ERR_TAG, msg))
                    }
                    "map" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            let ret_ty = match &f_ty {
                                Type::Fn(_, ret) => *ret.clone(),
                                _ => Type::Void,
                            };
                            Some(
                                self.emit_result_map(
                                    enum_ptr, &ok_ty, &err_ty, &ret_ty, f_val, &f_ty,
                                ),
                            )
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    "map_err" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            let ret_ty = match &f_ty {
                                Type::Fn(_, ret) => *ret.clone(),
                                _ => Type::Void,
                            };
                            Some(self.emit_result_map_err(
                                enum_ptr, &ok_ty, &err_ty, &ret_ty, f_val, &f_ty,
                            ))
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    "and_then" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            Some(self.emit_result_and_then(enum_ptr, &ok_ty, f_val, &f_ty))
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    "or_else" => {
                        if let Some(arg) = m.args.first() {
                            let f_val = self.emit_expr(&arg.expr);
                            let f_ty = self.ast_type_of_init(&arg.expr);
                            Some(self.emit_result_or_else(enum_ptr, &err_ty, f_val, &f_ty))
                        } else {
                            Some(enum_ptr)
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Emit: if enum tag == success_tag, return payload at offset 8; else panic(msg).
    pub(super) fn emit_enum_unwrap(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        payload_ty: &Type,
        success_tag: i64,
        msg: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let expected = self.builder.ins().iconst(types::I64, success_tag);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, expected);

        let ok_block = self.builder.create_block();
        let fail_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], fail_block, &[]);

        self.builder.switch_to_block(fail_block);
        self.builder.seal_block(fail_block);
        let fid = self.func_id("willow_panic");
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        self.builder.ins().call(fref, &[msg]);
        self.builder.ins().trap(TrapCode::unwrap_user(1));

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let clif_ty = clif_type(payload_ty);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        if clif_ty == types::F64 {
            self.builder.ins().bitcast(types::F64, MemFlags::new(), raw)
        } else if clif_ty == types::I8 {
            self.builder.ins().ireduce(types::I8, raw)
        } else {
            raw
        }
    }

    /// Emit: if enum tag == success_tag, return payload at offset 8; else return default.
    pub(super) fn emit_enum_unwrap_or(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        payload_ty: &Type,
        success_tag: i64,
        default_val: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let clif_ty = clif_type(payload_ty);
        let result_var = self.builder.declare_var(clif_ty);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let expected = self.builder.ins().iconst(types::I64, success_tag);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, expected);

        let ok_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let merge = self.builder.create_block();

        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], else_block, &[]);

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = if clif_ty == types::F64 {
            self.builder.ins().bitcast(types::F64, MemFlags::new(), raw)
        } else if clif_ty == types::I8 {
            self.builder.ins().ireduce(types::I8, raw)
        } else {
            raw
        };
        self.builder.def_var(result_var, payload);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(else_block);
        self.builder.seal_block(else_block);
        self.builder.def_var(result_var, default_val);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit an indirect call through a function value.
    pub(super) fn emit_indirect_call(
        &mut self,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
        args: &[cranelift_codegen::ir::Value],
    ) -> cranelift_codegen::ir::Value {
        if let Type::Fn(param_types, ret_type) = f_ty {
            let mut sig = self.module.make_signature();
            for pt in param_types {
                sig.params.push(AbiParam::new(clif_type(pt)));
            }
            let has_return = **ret_type != Type::Void;
            if has_return {
                sig.returns.push(AbiParam::new(clif_type(ret_type)));
            }
            let sig_ref = self.builder.import_signature(sig);
            let call = self.builder.ins().call_indirect(sig_ref, f_val, args);
            let results = self.builder.inst_results(call);
            if results.is_empty() {
                self.builder.ins().iconst(types::I64, 0)
            } else {
                results[0]
            }
        } else {
            self.builder.ins().iconst(types::I64, 0)
        }
    }

    /// Emit Option<T>.map(f) → Option<U>
    pub(super) fn emit_option_map(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        inner_ty: &Type,
        ret_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let some_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_some = self.builder.ins().icmp(IntCC::Equal, tag, some_tag_val);

        let some_block = self.builder.create_block();
        let none_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_some, some_block, &[], none_block, &[]);

        self.builder.switch_to_block(some_block);
        self.builder.seal_block(some_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = self.coerce_i64_to(raw, inner_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        let new_some = self.emit_alloc_enum_variant(0, ret_ty, result);
        self.builder.def_var(result_var, new_some);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(none_block);
        self.builder.seal_block(none_block);
        let new_none = self.emit_alloc_none();
        self.builder.def_var(result_var, new_none);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit Option<T>.and_then(f) where f: fn(T) -> Option<U>
    pub(super) fn emit_option_and_then(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        inner_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let some_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_some = self.builder.ins().icmp(IntCC::Equal, tag, some_tag_val);

        let some_block = self.builder.create_block();
        let none_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_some, some_block, &[], none_block, &[]);

        self.builder.switch_to_block(some_block);
        self.builder.seal_block(some_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = self.coerce_i64_to(raw, inner_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        self.builder.def_var(result_var, result);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(none_block);
        self.builder.seal_block(none_block);
        let new_none = self.emit_alloc_none();
        self.builder.def_var(result_var, new_none);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit Option<T>.or_else(f) where f: fn() -> Option<T>
    pub(super) fn emit_option_or_else(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let some_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_some = self.builder.ins().icmp(IntCC::Equal, tag, some_tag_val);

        let some_block = self.builder.create_block();
        let none_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_some, some_block, &[], none_block, &[]);

        self.builder.switch_to_block(some_block);
        self.builder.seal_block(some_block);
        self.builder.def_var(result_var, ptr);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(none_block);
        self.builder.seal_block(none_block);
        let result = self.emit_indirect_call(f_val, f_ty, &[]);
        self.builder.def_var(result_var, result);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit Result<T,E>.map(f) where f: fn(T) -> U → Result<U, E>
    pub(super) fn emit_result_map(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        ok_ty: &Type,
        err_ty: &Type,
        ret_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let ok_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, ok_tag_val);

        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], err_block, &[]);

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = self.coerce_i64_to(raw, ok_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        let new_ok = self.emit_alloc_enum_variant(0, ret_ty, result);
        self.builder.def_var(result_var, new_ok);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        let err_raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let new_err = self.emit_alloc_enum_variant_raw(1, err_ty, err_raw);
        self.builder.def_var(result_var, new_err);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit Result<T,E>.map_err(f) where f: fn(E) -> F → Result<T, F>
    pub(super) fn emit_result_map_err(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        ok_ty: &Type,
        err_ty: &Type,
        ret_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let ok_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, ok_tag_val);

        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], err_block, &[]);

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let ok_raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let new_ok = self.emit_alloc_enum_variant_raw(0, ok_ty, ok_raw);
        self.builder.def_var(result_var, new_ok);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = self.coerce_i64_to(raw, err_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        let new_err = self.emit_alloc_enum_variant(1, ret_ty, result);
        self.builder.def_var(result_var, new_err);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit Result<T,E>.and_then(f) where f: fn(T) -> Result<U,E>
    pub(super) fn emit_result_and_then(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        ok_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let ok_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, ok_tag_val);

        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], err_block, &[]);

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = self.coerce_i64_to(raw, ok_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        self.builder.def_var(result_var, result);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        self.builder.def_var(result_var, ptr);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Emit Result<T,E>.or_else(f) where f: fn(E) -> Result<T,F>
    pub(super) fn emit_result_or_else(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
        err_ty: &Type,
        f_val: cranelift_codegen::ir::Value,
        f_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let ptr_type = self.module.target_config().pointer_type();
        let result_var = self.builder.declare_var(ptr_type);
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let ok_tag_val = self.builder.ins().iconst(types::I64, 0);
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, ok_tag_val);

        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        let merge = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], err_block, &[]);

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        self.builder.def_var(result_var, ptr);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        let raw = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        let payload = self.coerce_i64_to(raw, err_ty);
        let result = self.emit_indirect_call(f_val, f_ty, &[payload]);
        self.builder.def_var(result_var, result);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Allocate a new 2-word enum (tag + payload) where payload is a typed value.
    pub(super) fn emit_alloc_enum_variant(
        &mut self,
        tag: i64,
        payload_ty: &Type,
        payload_val: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let payload_is_gc = is_gc_managed(payload_ty, self.enum_infos);
        let gc_mask: i64 = if payload_is_gc { 0b10 } else { 0 };
        // Root the payload across the enum allocation: a GC-managed payload is a
        // live pointer that must survive the collection `willow_alloc_typed` may
        // trigger before we store it into the new enum. Only reference payloads
        // are rooted; rooting a scalar word would make the GC mark it as a
        // bogus object pointer.
        if payload_is_gc {
            self.emit_push_root(payload_val);
        }
        let size = self.builder.ins().iconst(types::I64, 16);
        let mask = self.builder.ins().iconst(types::I64, gc_mask);
        let alloc_id = self.func_id("willow_alloc_typed");
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self.builder.ins().call(alloc_ref, &[size, mask]);
        let ptr = self.builder.inst_results(call)[0];
        let tag_val = self.builder.ins().iconst(types::I64, tag);
        self.builder
            .ins()
            .store(MemFlags::new(), tag_val, ptr, 0i32);
        let payload_i64 = if matches!(payload_ty, Type::F64) {
            self.builder
                .ins()
                .bitcast(types::I64, MemFlags::new(), payload_val)
        } else if matches!(payload_ty, Type::Bool) {
            self.builder.ins().uextend(types::I64, payload_val)
        } else {
            payload_val
        };
        self.builder
            .ins()
            .store(MemFlags::new(), payload_i64, ptr, 8i32);
        if payload_is_gc {
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
        }
        ptr
    }

    /// Allocate a new 2-word enum (tag + payload) where payload is already an i64 raw word.
    pub(super) fn emit_alloc_enum_variant_raw(
        &mut self,
        tag: i64,
        payload_ty: &Type,
        payload_raw: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let payload_is_gc = is_gc_managed(payload_ty, self.enum_infos);
        let gc_mask: i64 = if payload_is_gc { 0b10 } else { 0 };
        // See `emit_alloc_enum_variant`: root a GC-managed payload across the
        // allocation so a collection cannot free it before it is stored.
        if payload_is_gc {
            self.emit_push_root(payload_raw);
        }
        let size = self.builder.ins().iconst(types::I64, 16);
        let mask = self.builder.ins().iconst(types::I64, gc_mask);
        let alloc_id = self.func_id("willow_alloc_typed");
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self.builder.ins().call(alloc_ref, &[size, mask]);
        let ptr = self.builder.inst_results(call)[0];
        let tag_val = self.builder.ins().iconst(types::I64, tag);
        self.builder
            .ins()
            .store(MemFlags::new(), tag_val, ptr, 0i32);
        self.builder
            .ins()
            .store(MemFlags::new(), payload_raw, ptr, 8i32);
        if payload_is_gc {
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
        }
        ptr
    }

    /// Allocate an Option::None (1-word enum, tag=1, no payload).
    pub(super) fn emit_alloc_none(&mut self) -> cranelift_codegen::ir::Value {
        let size = self.builder.ins().iconst(types::I64, 8);
        let mask = self.builder.ins().iconst(types::I64, 0);
        let alloc_id = self.func_id("willow_alloc_typed");
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self.builder.ins().call(alloc_ref, &[size, mask]);
        let ptr = self.builder.inst_results(call)[0];
        let none_tag = self.builder.ins().iconst(types::I64, 1);
        self.builder
            .ins()
            .store(MemFlags::new(), none_tag, ptr, 0i32);
        ptr
    }

    /// Emit `[e0, e1, ...]`: allocate an array sized to the literal, then store
    /// each element through the array ABI. The array is rooted during element
    /// evaluation so a GC triggered mid-construction keeps it (and its stored
    /// elements) alive.
    pub(super) fn emit_array_literal(
        &mut self,
        elements: &[Expr],
        elem_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let len_val = self.builder.ins().iconst(types::I64, elements.len() as i64);
        let is_ref = if is_gc_managed(elem_ty, self.enum_infos) {
            1
        } else {
            0
        };
        let is_ref_val = self.builder.ins().iconst(types::I64, is_ref);
        let new_id = self.func_id("willow_array_new");
        let new_ref = self.module.declare_func_in_func(new_id, self.builder.func);
        let call = self.builder.ins().call(new_ref, &[len_val, is_ref_val]);
        let arr = self.builder.inst_results(call)[0];

        self.emit_push_root(arr);
        for (i, el) in elements.iter().enumerate() {
            // Box class elements when the array's element type is an interface.
            let val = self.emit_expr_coerced(el, elem_ty);
            let word = self.coerce_to_i64(val, elem_ty);
            let idx_val = self.builder.ins().iconst(types::I64, i as i64);
            let set_id = self.func_id("willow_array_set");
            let set_ref = self.module.declare_func_in_func(set_id, self.builder.func);
            self.builder.ins().call(set_ref, &[arr, idx_val, word]);
        }
        self.emit_pop_roots_n(1);
        self.gc_root_count -= 1;
        arr
    }

    /// Emit `arr[index]`: bounds-checked element read, converted back to the
    /// element type.
    pub(super) fn emit_index(
        &mut self,
        arr_expr: &Expr,
        index_expr: &Expr,
        elem_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let arr = self.emit_expr(arr_expr);
        let index = self.emit_expr(index_expr);
        let get_id = self.func_id("willow_array_get");
        let get_ref = self.module.declare_func_in_func(get_id, self.builder.func);
        let call = self.builder.ins().call(get_ref, &[arr, index]);
        let word = self.builder.inst_results(call)[0];
        self.coerce_i64_to(word, elem_ty)
    }

    /// Emit a `Map<K, V>` method call. Keys/values cross the runtime ABI as raw
    /// 64-bit words plus ref-ness flags; `get` returns a runtime-built
    /// `Option<V>` pointer.
    pub(super) fn emit_map_method_call(
        &mut self,
        map: cranelift_codegen::ir::Value,
        key_ty: &Type,
        val_ty: &Type,
        m: &MethodCallExpr,
    ) -> cranelift_codegen::ir::Value {
        let key_is_ref = self.builder.ins().iconst(
            types::I64,
            i64::from(is_gc_managed(key_ty, self.enum_infos)),
        );
        match m.method.as_str() {
            "insert" => {
                // Root the map while evaluating key/value (either may allocate).
                // A GC-managed key must also stay rooted while the value is
                // evaluated, and both key and value must stay rooted across the
                // insert call itself, which may allocate (grow buckets / copy)
                // and trigger a collection before they are stored (willow-oewp.6).
                self.emit_push_root(map);
                let mut temp_roots = 1usize;
                let k = self.emit_expr(&m.args[0].expr);
                let k_word = self.coerce_to_i64(k, key_ty);
                if is_gc_managed(key_ty, self.enum_infos) {
                    self.emit_push_root(k);
                    temp_roots += 1;
                }
                let v = self.emit_expr(&m.args[1].expr);
                let v_word = self.coerce_to_i64(v, val_ty);
                if is_gc_managed(val_ty, self.enum_infos) {
                    self.emit_push_root(v);
                    temp_roots += 1;
                }
                let val_is_ref = self.builder.ins().iconst(
                    types::I64,
                    i64::from(is_gc_managed(val_ty, self.enum_infos)),
                );
                let id = self.func_id("willow_map_insert");
                let r = self.module.declare_func_in_func(id, self.builder.func);
                self.builder
                    .ins()
                    .call(r, &[map, k_word, key_is_ref, v_word, val_is_ref]);
                self.emit_pop_roots_n(temp_roots);
                self.gc_root_count -= temp_roots;
                self.builder.ins().iconst(types::I64, 0) // void
            }
            "get" => {
                // Root the map across the get call: it allocates the `Option<V>`
                // result, and a temporary map (reachable only here) must survive
                // that allocation so its stored value is not collected
                // (willow-oewp.6).
                self.emit_push_root(map);
                let k = self.emit_expr(&m.args[0].expr);
                let k_word = self.coerce_to_i64(k, key_ty);
                let id = self.func_id("willow_map_get");
                let r = self.module.declare_func_in_func(id, self.builder.func);
                let call = self.builder.ins().call(r, &[map, k_word, key_is_ref]);
                let result = self.builder.inst_results(call)[0]; // Option<V> pointer
                self.emit_pop_roots_n(1);
                self.gc_root_count -= 1;
                result
            }
            "contains" => {
                let k = self.emit_expr(&m.args[0].expr);
                let k_word = self.coerce_to_i64(k, key_ty);
                let id = self.func_id("willow_map_contains");
                let r = self.module.declare_func_in_func(id, self.builder.func);
                let call = self.builder.ins().call(r, &[map, k_word, key_is_ref]);
                let raw = self.builder.inst_results(call)[0];
                self.builder.ins().ireduce(types::I8, raw) // bool
            }
            "len" => {
                let id = self.func_id("willow_map_len");
                let r = self.module.declare_func_in_func(id, self.builder.func);
                let call = self.builder.ins().call(r, &[map]);
                self.builder.inst_results(call)[0]
            }
            // `map.freeze()` -> an immutable copy (willow-dgwo.10).
            "freeze" => {
                let id = self.func_id("willow_map_copy");
                let r = self.module.declare_func_in_func(id, self.builder.func);
                let call = self.builder.ins().call(r, &[map]);
                self.builder.inst_results(call)[0]
            }
            _ => self.builder.ins().iconst(types::I64, 0),
        }
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
            .load(types::I64, MemFlags::new(), box_ptr, 0i32);
        let vtable = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), box_ptr, 8i32);
        let fnptr = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), vtable, (slot * 8) as i32);

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
            .store(MemFlags::new(), object, box_ptr, 0i32);
        self.builder
            .ins()
            .store(MemFlags::new(), vtable_ptr, box_ptr, 8i32);
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

        if m.method == "join" {
            if let Some(result_ty) = join_handle_result_type(&obj_type) {
                // Drive the cooperative scheduler until THIS task (and anything it
                // depends on) completes, then read the result from the frame's
                // slot 0 (willow-bsqy). `self_ptr` is the task frame; slot 1 holds
                // its task id. Driving until just this task — not to quiescence —
                // means joining one task does not run unrelated tasks to
                // completion and cannot hang on an unrelated non-terminating task.
                self.emit_push_root(self_ptr);
                let task_id = self.builder.ins().load(
                    types::I64,
                    MemFlags::new(),
                    self_ptr,
                    async_frame_slot_offset(FRAME_SLOT_TASK_ID),
                );
                let run_fid = self.func_id("willow_sched_run_until");
                let run_fref = self.module.declare_func_in_func(run_fid, self.builder.func);
                self.builder.ins().call(run_fref, &[task_id]);

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
                        .load(clif_ret_ty, MemFlags::new(), self_ptr, result_off);
                self.emit_pop_roots_n(1);
                self.gc_root_count -= 1;
                return result;
            }
        }

        if let Type::Named(n) = &obj_type {
            if n == "AtomicI64" || n == "AtomicBool" {
                let is_i64 = n == "AtomicI64";
                return self.emit_atomic_method_call(self_ptr, is_i64, m);
            }
        }

        if let Type::Generic(n, args) = &obj_type {
            if (n == "Mutex" || n == "RwLock") && args.len() == 1 {
                let elem_ty = args[0].clone();
                let is_mutex = n == "Mutex";
                return self.emit_lock_method_call(self_ptr, is_mutex, &elem_ty, m);
            }
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
        if let Type::Generic(name, fargs) = &obj_type {
            if name == "FrozenArray" && fargs.len() == 1 && m.method == "len" {
                let id = self.func_id("willow_array_len");
                let r = self.module.declare_func_in_func(id, self.builder.func);
                let call = self.builder.ins().call(r, &[self_ptr]);
                return self.builder.inst_results(call)[0];
            }
        }

        // Map<K,V> and the immutable FrozenMap<K,V> share the same runtime map
        // object, so reads dispatch identically (willow-dgwo.10).
        if let Type::Generic(name, margs) = &obj_type {
            if (name == "Map" || name == "FrozenMap") && margs.len() == 2 {
                let key_ty = margs[0].clone();
                let val_ty = margs[1].clone();
                return self.emit_map_method_call(self_ptr, &key_ty, &val_ty, m);
            }
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
        if let Type::Generic(name, _) = &obj_type {
            if let Some(iface) = self.interface_infos.get(name).cloned() {
                let pushed = self.emit_callstack_push(&m.method, m.span);
                let r = self.emit_interface_dispatch(self_ptr, &iface, m);
                if pushed {
                    self.emit_callstack_pop();
                }
                return r;
            }
        }
        if let Some(iface_name) = class_name_for_object_type(&obj_type) {
            if let Some(iface) = self.interface_infos.get(&iface_name).cloned() {
                let pushed = self.emit_callstack_push(&m.method, m.span);
                let r = self.emit_interface_dispatch(self_ptr, &iface, m);
                if pushed {
                    self.emit_callstack_pop();
                }
                return r;
            }
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
                    .load(types::I64, MemFlags::new(), self_ptr, 0i32);

            // Use an SSA variable to collect the result across dispatch arms
            // (matches the pattern used by emit_short_circuit_and/or and emit_match).
            let ret_clif_ty = clif_type(&ret_type);
            let result_var = if ret_type != Type::Void {
                let v = self.builder.declare_var(ret_clif_ty);
                let zero = if ret_clif_ty == types::F64 {
                    let bits = self.builder.ins().iconst(types::I64, 0);
                    self.builder
                        .ins()
                        .bitcast(types::F64, MemFlags::new(), bits)
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

    pub(super) fn emit_ternary(&mut self, t: &TernaryExpr) -> cranelift_codegen::ir::Value {
        let result_ty = clif_type(&ast_type_of_ternary(t, &self.vars, self.func_return_types));
        let result_var = self.builder.declare_var(result_ty);

        let then_block = self.builder.create_block();
        let else_block = self.builder.create_block();
        let merge_block = self.builder.create_block();

        let cond = self.emit_expr(&t.condition);
        self.builder
            .ins()
            .brif(cond, then_block, &[], else_block, &[]);

        // then branch — only this runs when condition is true (lazy)
        self.builder.switch_to_block(then_block);
        self.builder.seal_block(then_block);
        let then_val = self.emit_expr(&t.then_expr);
        self.builder.def_var(result_var, then_val);
        self.builder.ins().jump(merge_block, &[]);

        // else branch — only this runs when condition is false (lazy)
        self.builder.switch_to_block(else_block);
        self.builder.seal_block(else_block);
        let else_val = self.emit_expr(&t.else_expr);
        self.builder.def_var(result_var, else_val);
        self.builder.ins().jump(merge_block, &[]);

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.builder.use_var(result_var)
    }

    /// Convert error `e1_payload` (static class `e1_name`, implementing
    /// `Into<E2>`) to `E2` by calling `into`, dispatching VIRTUALLY on the
    /// payload's runtime type so a subclass override is honored (willow-bpk6).
    pub(super) fn emit_into_conversion(
        &mut self,
        e1_payload: cranelift_codegen::ir::Value,
        e1_name: &str,
    ) -> cranelift_codegen::ir::Value {
        // Candidate runtime types: e1_name and its subclasses that resolve `into`.
        let mut dispatch: Vec<(i64, FuncId)> = self
            .class_type_ids
            .iter()
            .filter(|(cls, _)| self.class_is_a(cls, e1_name))
            .filter_map(|(cls, &id)| {
                self.resolve_method_func_id(cls, "into")
                    .map(|fid| (id, fid))
            })
            .collect();
        dispatch.sort_by_key(|(id, _)| *id);

        // Zero or one candidate: a plain direct call (no subclass override).
        if dispatch.len() <= 1 {
            let fid = dispatch
                .first()
                .map(|(_, f)| *f)
                .or_else(|| self.resolve_method_func_id(e1_name, "into"))
                .expect("Into impl must exist (verified by the type checker)");
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let call = self.builder.ins().call(fref, &[e1_payload]);
            return self.builder.inst_results(call)[0];
        }

        // Multiple candidates: switch on the payload's runtime type_id (word 0).
        let type_id = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), e1_payload, 0i32);
        let result_var = self.builder.declare_var(types::I64);
        let zero = self.builder.ins().iconst(types::I64, 0);
        self.builder.def_var(result_var, zero);
        let merge = self.builder.create_block();
        let n = dispatch.len();
        for (i, (tid, fid)) in dispatch.into_iter().enumerate() {
            let tid_c = self.builder.ins().iconst(types::I64, tid);
            let is_match = self.builder.ins().icmp(IntCC::Equal, type_id, tid_c);
            let arm = self.builder.create_block();
            let next = self.builder.create_block();
            self.builder.ins().brif(is_match, arm, &[], next, &[]);
            self.builder.switch_to_block(arm);
            self.builder.seal_block(arm);
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let call = self.builder.ins().call(fref, &[e1_payload]);
            let r = self.builder.inst_results(call)[0];
            self.builder.def_var(result_var, r);
            self.builder.ins().jump(merge, &[]);
            self.builder.switch_to_block(next);
            self.builder.seal_block(next);
            if i + 1 == n {
                self.builder.ins().jump(merge, &[]);
            }
        }
        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result_var)
    }

    /// Lower `expr?` into control flow:
    /// - Result::Ok / Option::Some (tag == 0): extract and return the payload.
    /// - Result::Err / Option::None (tag == 1): early-return the enum pointer.
    pub(super) fn emit_try_propagate(&mut self, inner: &Expr) -> cranelift_codegen::ir::Value {
        let operand_ty = self.ast_type_of(inner);
        let result_ptr = self.emit_expr(inner);
        let payload_ty = try_propagate_payload_type(&operand_ty);

        // Automatic error conversion (willow-1ow): if the operand's error type
        // `E1` differs from the enclosing function's error type `E2` (and neither
        // is `void`), the type checker has already verified `E1: Into<E2>`. On
        // the Err path we must convert `e1.into() -> e2` and re-wrap `Err(e2)`,
        // rather than returning the original `Result<_, E1>`.
        let convert: Option<(String, Type)> = match (
            result_err_type(&operand_ty),
            result_err_type(&self.return_type),
        ) {
            (Some(Type::Named(e1)), Some(e2))
                if Type::Named(e1.clone()) != e2 && e2 != Type::Void =>
            {
                Some((e1, e2))
            }
            _ => None,
        };

        // Load the enum tag from word 0.
        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), result_ptr, 0i32);
        let ok_tag = self.builder.ins().iconst(types::I64, 0); // Ok = tag 0
        let is_ok = self.builder.ins().icmp(IntCC::Equal, tag, ok_tag);

        let ok_block = self.builder.create_block();
        let err_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_ok, ok_block, &[], err_block, &[]);

        // ── Propagate branch: early-return the Err ────────────────────────────
        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        // When the error types differ, convert `e1.into() -> e2` and re-wrap.
        let return_ptr = if let Some((e1_name, e2_ty)) = &convert {
            let e1_payload = self
                .builder
                .ins()
                .load(types::I64, MemFlags::new(), result_ptr, 8i32);
            let e1_is_gc = is_gc_managed(&Type::Named(e1_name.clone()), self.enum_infos);
            if e1_is_gc {
                self.emit_push_root(e1_payload);
            }
            // Dispatch `into` on the payload's runtime type so a subclassed
            // error that overrides `into` converts correctly (willow-bpk6).
            let e2_val = self.emit_into_conversion(e1_payload, e1_name);
            if e1_is_gc {
                self.emit_pop_roots_n(1);
                self.gc_root_count -= 1;
            }
            self.emit_alloc_enum_variant(1, e2_ty, e2_val)
        } else {
            result_ptr
        };
        if self.main_result_err_ty.is_some() {
            // In a `Result<void, E>` main, an Err is reported and exits non-zero
            // rather than being returned (willow_user_main is void). Roots are
            // popped inside emit_main_result_exit.
            self.emit_main_result_exit(return_ptr);
        } else {
            if self.gc_root_count > 0 {
                self.emit_pop_roots_n(self.gc_root_count);
            }
            // Return the (possibly converted) Result/Option pointer.
            self.builder.ins().return_(&[return_ptr]);
        }

        // ── Success branch: extract payload from word 1 ───────────────────────
        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        let payload = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), result_ptr, 8i32);
        self.coerce_i64_to(payload, &payload_ty)
    }

    /// Leave a `Result<void, E>` main by inspecting the `Result` value: `Err`
    /// reports the payload and exits non-zero; `Ok` returns void (exit 0). Pops
    /// the function's GC roots first (we leave the function on both paths).
    /// See willow-exg.
    pub(super) fn emit_main_result_exit(&mut self, result_ptr: cranelift_codegen::ir::Value) {
        if self.gc_root_count > 0 {
            self.emit_pop_roots_n(self.gc_root_count);
        }
        let err_is_string = self.main_result_err_ty.as_ref() == Some(&Type::String);

        let tag = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), result_ptr, 0i32);
        let err_tag = self.builder.ins().iconst(types::I64, 1); // Err = tag 1
        let is_err = self.builder.ins().icmp(IntCC::Equal, tag, err_tag);
        let err_block = self.builder.create_block();
        let ok_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(is_err, err_block, &[], ok_block, &[]);

        // Err: print the payload (a WillowString for E=String, else a generic
        // report via a null message) and exit non-zero.
        self.builder.switch_to_block(err_block);
        self.builder.seal_block(err_block);
        let msg = if err_is_string {
            self.builder
                .ins()
                .load(types::I64, MemFlags::new(), result_ptr, 8i32)
        } else {
            self.builder.ins().iconst(types::I64, 0)
        };
        let fail_id = self.func_id("willow_main_fail");
        let fail_ref = self.module.declare_func_in_func(fail_id, self.builder.func);
        self.builder.ins().call(fail_ref, &[msg]);
        // willow_main_fail is noreturn; trap to satisfy the verifier.
        self.builder.ins().trap(TrapCode::unwrap_user(1));

        // Ok: success — return void (process exits 0).
        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
        self.builder.ins().return_(&[]);
    }

    pub(super) fn emit_match(&mut self, m: &MatchExpr) -> cranelift_codegen::ir::Value {
        let scrutinee = self.emit_expr(&m.scrutinee);
        let scrutinee_ast_type = self.ast_type_of(&m.scrutinee);

        // Determine the result type from match arms, accounting for pattern bindings.
        let result_ast_type = {
            let mut scratch = self.vars.clone();
            let mut found = Type::I64;
            'outer: for arm in &m.arms {
                match &arm.pattern {
                    Pattern::Binding { name, .. } => {
                        let sty = scrutinee_ast_type.clone();
                        scratch.insert(
                            name.clone(),
                            VarStorage::Value {
                                var: self.builder.declare_var(clif_type(&sty)),
                                ty: sty,
                            },
                        );
                    }
                    Pattern::EnumVariantTuple {
                        enum_name,
                        variant,
                        bindings,
                        ..
                    } => {
                        // Resolve actual payload types — for generic types like Option/Result,
                        // use the type argument from the scrutinee rather than the placeholder.
                        let pts = self.resolve_variant_payload_types(
                            enum_name,
                            variant,
                            &scrutinee_ast_type,
                        );
                        for (name, ty) in bindings.iter().zip(pts.iter()) {
                            scratch.insert(
                                name.clone(),
                                VarStorage::Value {
                                    var: self.builder.declare_var(clif_type(ty)),
                                    ty: ty.clone(),
                                },
                            );
                        }
                    }
                    Pattern::ClassDowncast {
                        class_name,
                        binding,
                        ..
                    } if binding != "_" => {
                        let ty = Type::Named(class_name.clone());
                        scratch.insert(
                            binding.clone(),
                            VarStorage::Value {
                                var: self.builder.declare_var(clif_type(&ty)),
                                ty,
                            },
                        );
                    }
                    _ => {}
                }
                let ty = match &arm.body {
                    MatchBody::Expr(e) => ast_type_of_expr(e, &scratch, self.func_return_types),
                    MatchBody::Block(_) => Type::Void,
                };
                if ty != Type::Void && ty != Type::Never {
                    found = ty;
                    break 'outer;
                }
            }
            found
        };
        let result_clif_type = clif_type(&result_ast_type);
        let result_var = self.builder.declare_var(result_clif_type);
        let zero = if result_clif_type == types::F64 {
            let bits = self.builder.ins().iconst(types::I64, 0);
            self.builder
                .ins()
                .bitcast(types::F64, MemFlags::new(), bits)
        } else if result_clif_type == types::I8 {
            self.builder.ins().iconst(types::I8, 0)
        } else {
            self.builder.ins().iconst(types::I64, 0)
        };
        self.builder.def_var(result_var, zero);

        let merge_block = self.builder.create_block();

        let mut remaining = m.arms.as_slice();

        while !remaining.is_empty() {
            let arm = &remaining[0];
            remaining = &remaining[1..];

            let is_last = remaining.is_empty();

            let always_matches =
                matches!(arm.pattern, Pattern::Wildcard(_) | Pattern::Binding { .. });

            let arm_block = self.builder.create_block();
            let next_block = if always_matches || is_last {
                None
            } else {
                Some(self.builder.create_block())
            };

            if always_matches {
                self.builder.ins().jump(arm_block, &[]);
            } else {
                let cond = self.emit_pattern_check(scrutinee, &arm.pattern);
                let fallthrough = next_block.unwrap_or(merge_block);
                self.builder
                    .ins()
                    .brif(cond, arm_block, &[], fallthrough, &[]);
            }

            self.builder.switch_to_block(arm_block);
            self.builder.seal_block(arm_block);

            // For binding patterns, define the variable
            let saved_vars = match &arm.pattern {
                Pattern::Binding { name, .. } => {
                    let var = self.builder.declare_var(types::I64);
                    self.builder.def_var(var, scrutinee);
                    let saved = self.vars.clone();
                    self.vars.insert(
                        name.clone(),
                        VarStorage::Value {
                            var,
                            ty: result_ast_type.clone(),
                        },
                    );
                    Some(saved)
                }
                Pattern::EnumVariantTuple {
                    enum_name,
                    variant,
                    bindings,
                    ..
                } => {
                    let saved = self.vars.clone();
                    let payload_types =
                        self.resolve_variant_payload_types(enum_name, variant, &scrutinee_ast_type);
                    for (i, (binding, payload_ty)) in
                        bindings.iter().zip(payload_types.iter()).enumerate()
                    {
                        let offset = (1 + i) as i32 * 8;
                        let clif_ty = clif_type(payload_ty);
                        let raw =
                            self.builder
                                .ins()
                                .load(types::I64, MemFlags::new(), scrutinee, offset);
                        let val = if clif_ty == types::F64 {
                            self.builder.ins().bitcast(types::F64, MemFlags::new(), raw)
                        } else if clif_ty == types::I8 {
                            self.builder.ins().ireduce(types::I8, raw)
                        } else {
                            raw
                        };
                        let var = self.builder.declare_var(clif_ty);
                        self.builder.def_var(var, val);
                        self.vars.insert(
                            binding.clone(),
                            VarStorage::Value {
                                var,
                                ty: payload_ty.clone(),
                            },
                        );
                    }
                    Some(saved)
                }
                Pattern::ClassDowncast {
                    class_name,
                    binding,
                    ..
                } if binding != "_" => {
                    // Bind the downcast value: the box's object pointer (word 0),
                    // typed as the concrete class (willow-1js.4).
                    let saved = self.vars.clone();
                    let obj = self
                        .builder
                        .ins()
                        .load(types::I64, MemFlags::new(), scrutinee, 0i32);
                    let var = self.builder.declare_var(types::I64);
                    self.builder.def_var(var, obj);
                    self.vars.insert(
                        binding.clone(),
                        VarStorage::Value {
                            var,
                            ty: Type::Named(class_name.clone()),
                        },
                    );
                    Some(saved)
                }
                _ => None,
            };

            let outer_terminated = self.terminated;
            self.terminated = false;
            let arm_val = self.emit_match_body(&arm.body);

            if !self.terminated {
                self.builder.def_var(result_var, arm_val);
                self.builder.ins().jump(merge_block, &[]);
            }
            self.terminated = outer_terminated;

            if let Some(saved) = saved_vars {
                self.vars = saved;
            }

            if let Some(next) = next_block {
                self.builder.switch_to_block(next);
                self.builder.seal_block(next);
            }

            if always_matches {
                break;
            }
        }

        self.builder.switch_to_block(merge_block);
        self.builder.seal_block(merge_block);
        self.builder.use_var(result_var)
    }

    pub(super) fn emit_match_body(&mut self, body: &MatchBody) -> cranelift_codegen::ir::Value {
        match body {
            MatchBody::Expr(expr) => self.emit_expr(expr),
            MatchBody::Block(block) => {
                self.emit_block(block);
                self.builder.ins().iconst(types::I64, 0)
            }
        }
    }

    pub(super) fn emit_pattern_check(
        &mut self,
        scrutinee: cranelift_codegen::ir::Value,
        pattern: &Pattern,
    ) -> cranelift_codegen::ir::Value {
        match pattern {
            Pattern::Wildcard(_) | Pattern::Binding { .. } => {
                self.builder.ins().iconst(types::I8, 1)
            }
            Pattern::LiteralBool(b, _) => {
                let expected = self.builder.ins().iconst(types::I8, if *b { 1 } else { 0 });
                self.builder.ins().icmp(IntCC::Equal, scrutinee, expected)
            }
            Pattern::LiteralInt(n, _) => {
                let expected = self.builder.ins().iconst(types::I64, *n);
                self.builder.ins().icmp(IntCC::Equal, scrutinee, expected)
            }
            Pattern::EnumVariant {
                enum_name, variant, ..
            } => {
                let tag = self.enum_variant_tag(enum_name, variant);
                let expected = self.builder.ins().iconst(types::I64, tag);
                if self.enum_is_gc_object_type(enum_name) {
                    let actual_tag = self.emit_load_enum_tag(scrutinee);
                    self.builder.ins().icmp(IntCC::Equal, actual_tag, expected)
                } else {
                    self.builder.ins().icmp(IntCC::Equal, scrutinee, expected)
                }
            }
            Pattern::EnumVariantTuple {
                enum_name, variant, ..
            } => {
                let tag = self.enum_variant_tag(enum_name, variant);
                let expected = self.builder.ins().iconst(types::I64, tag);
                let actual_tag = self.emit_load_enum_tag(scrutinee);
                self.builder.ins().icmp(IntCC::Equal, actual_tag, expected)
            }
            Pattern::ClassDowncast { class_name, .. } => {
                // The scrutinee is an interface box {object@0, vtable@8}. Match
                // when the boxed object's runtime type_id (object word 0) equals
                // the target class's type_id (willow-1js.4).
                let Some(&type_id) = self.class_type_ids.get(class_name) else {
                    return self.builder.ins().iconst(types::I8, 0); // unknown class: never matches
                };
                if self.build_mode == BuildMode::Debug {
                    self.emit_nil_check(scrutinee, pattern.span(), "interface downcast box");
                }
                let obj = self
                    .builder
                    .ins()
                    .load(types::I64, MemFlags::new(), scrutinee, 0i32);
                if self.build_mode == BuildMode::Debug {
                    self.emit_nil_check(obj, pattern.span(), "interface downcast object");
                }
                let actual = self
                    .builder
                    .ins()
                    .load(types::I64, MemFlags::new(), obj, 0i32);
                let expected = self.builder.ins().iconst(types::I64, type_id);
                self.builder.ins().icmp(IntCC::Equal, actual, expected)
            }
        }
    }

    pub(super) fn emit_load_enum_tag(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        self.builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32)
    }

    pub(super) fn emit_enum_variant_alloc(
        &mut self,
        tag: i64,
        args: &[crate::parser::ast::CallArg],
    ) -> cranelift_codegen::ir::Value {
        let field_count = args.len();
        let total_words = 1 + field_count;
        let size = self
            .builder
            .ins()
            .iconst(types::I64, (total_words * 8) as i64);
        // Compute gc_ref_mask: layout is [tag_word, payload_0, payload_1, ...].
        // Word 0 = tag (never a GC ref).
        // Word i+1 = args[i]; set bit i+1 if that arg is GC-managed.
        let gc_mask: i64 = args.iter().enumerate().fold(0i64, |mask, (i, arg)| {
            if is_gc_managed(&self.ast_type_of(&arg.expr), self.enum_infos) {
                mask | (1i64 << (i + 1))
            } else {
                mask
            }
        });
        let mask = self.builder.ins().iconst(types::I64, gc_mask);
        let alloc_id = self.func_id("willow_alloc_typed");
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self.builder.ins().call(alloc_ref, &[size, mask]);
        let ptr = self.builder.inst_results(call)[0];
        let tag_val = self.builder.ins().iconst(types::I64, tag);
        self.builder
            .ins()
            .store(MemFlags::new(), tag_val, ptr, 0i32);
        // Root the freshly allocated enum across argument evaluation: each
        // `emit_expr(&arg.expr)` below can allocate (e.g. a class payload), and
        // that allocation may trigger a collection.  Without this root the
        // half-built enum (tag stored, payload slots still zero) would be
        // reclaimed and we would store into freed memory.  The payload is
        // alloc_zeroed, so tracing the enum before all slots are stored is safe
        // (unstored ref slots read as null and are skipped).
        // Root whenever there is at least one argument to evaluate: even a
        // scalar-payload enum must survive an allocation inside an argument
        // expression (e.g. `Option::Some(f())` where `f` allocates internally).
        let needs_root = field_count > 0;
        if needs_root {
            self.emit_push_root(ptr);
        }
        for (i, arg) in args.iter().enumerate() {
            let offset = (1 + i) as i32 * 8;
            let val = self.emit_expr(&arg.expr);
            let val_i64 = if matches!(self.ast_type_of(&arg.expr), Type::F64) {
                self.builder.ins().bitcast(types::I64, MemFlags::new(), val)
            } else {
                val
            };
            self.builder
                .ins()
                .store(MemFlags::new(), val_i64, ptr, offset);
        }
        if needs_root {
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
        }
        ptr
    }

    pub(super) fn emit_static_field_read(
        &mut self,
        class: &str,
        field: &str,
    ) -> cranelift_codegen::ir::Value {
        let class_name = self.static_call_class_name(class);
        if let Some(info) = self.lookup_static_storage(&class_name, field) {
            let ty = clif_type(&info.ty);
            let ptr_ty = self.module.target_config().pointer_type();
            let gv = self
                .module
                .declare_data_in_func(info.data_id, self.builder.func);
            let addr = self.builder.ins().global_value(ptr_ty, gv);
            return self.builder.ins().load(ty, MemFlags::new(), addr, 0);
        }
        // Should be unreachable after type checking; fall back to a zero value.
        self.builder.ins().iconst(types::I64, 0)
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
        if let Some(enum_info) = self.enum_infos.get(&class_name).cloned() {
            if let Some(variant) = enum_info
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
