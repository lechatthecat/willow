use cranelift_codegen::ir::{
    InstBuilder, MemFlagsData, StackSlotData, StackSlotKind, condcodes::IntCC, types,
};
use cranelift_module::Module;

use super::*;

impl<'a, 'b> FuncGen<'a, 'b> {
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
        self.defer_stack.push(Vec::new());
        let defer_depth = self.defer_stack.len() - 1;

        for stmt in &block.stmts {
            if self.terminated {
                break;
            }
            self.emit_stmt(stmt);
        }

        // Scope fallthrough: run this scope's defers (LIFO), THEN pop the
        // block's GC roots (the defers read their operands from rooted slots).
        // On terminated paths the return/break/`?` handler already flushed.
        if !self.terminated {
            self.emit_flush_defers_from(defer_depth);
            let block_roots = self.gc_root_count - gc_roots_before;
            if block_roots > 0 {
                self.emit_pop_roots_n(block_roots);
            }
        }
        self.defer_stack.pop();
        // Restore scope: gc_root_count goes back to what it was before the block
        // (in the terminated path the return handler already popped all roots).
        self.gc_root_count = gc_roots_before;
        self.vars = saved_vars;
    }

    /// Run every registered defer from frame `depth` outward, innermost frame
    /// first, newest registration first (LIFO). Frames are left in place —
    /// scope bookkeeping pops them (willow-vynv.2).
    pub(super) fn emit_flush_defers_from(&mut self, depth: usize) {
        let frames: Vec<Vec<super::DeferEntry>> = self.defer_stack[depth..].to_vec();
        for frame in frames.iter().rev() {
            for entry in frame.iter().rev() {
                // Rebind the hidden frame operands: coop loop bodies restore
                // `vars`, wiping the names between registration and a
                // function-exit flush (willow-vynv.3).
                for (name, offset, ty) in &entry.bindings {
                    self.vars.insert(
                        name.clone(),
                        VarStorage::Frame {
                            offset: *offset,
                            ty: ty.clone(),
                        },
                    );
                }
                self.emit_stmt(&entry.stmt);
                // Async: a normal-path flush clears the registration flag so
                // the cancel entry never re-runs this defer (willow-vynv.3).
                if let (Some(off), Some(frame_ptr)) = (entry.flag_offset, self.coop_frame) {
                    let zero = self.builder.ins().iconst(types::I64, 0);
                    self.builder
                        .ins()
                        .store(MemFlagsData::new(), zero, frame_ptr, off);
                }
            }
        }
    }

    /// Register a `defer`: evaluate receiver/args NOW into hidden rooted
    /// locals, and queue a synthetic call statement that reads them back at
    /// flush time — so flush reuses the normal call/method emission
    /// (dispatch, coercion, rooting) unchanged (willow-vynv.2).
    fn emit_defer_register(&mut self, d: &DeferStmt) {
        let n = self.defer_counter;
        self.defer_counter += 1;
        let async_frame = self.coop_frame;
        let mut bindings: Vec<(String, i32, Type)> = Vec::new();
        let mut stash = |fg: &mut Self, label: String, expr: &Expr| -> Expr {
            let ty = fg.ast_type_of(expr);
            let val = fg.emit_expr(expr);
            if let Some(frame_ptr) = async_frame {
                // Async: the operand lives in a frame slot (GC-masked by the
                // layout) so it survives suspension and is visible to the
                // cancel entry (willow-vynv.3).
                let off = fg.async_frame_offsets[&expr.span()];
                fg.builder
                    .ins()
                    .store(MemFlagsData::new(), val, frame_ptr, off);
                bindings.push((label.clone(), off, ty.clone()));
                fg.vars
                    .insert(label.clone(), VarStorage::Frame { offset: off, ty });
            } else {
                let slot = fg.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    0,
                ));
                fg.builder.ins().stack_store(val, slot, 0);
                if is_gc_managed(&ty, fg.enum_infos) {
                    fg.emit_push_root_slot(slot);
                }
                fg.vars
                    .insert(label.clone(), VarStorage::Stack { slot, ty });
            }
            Expr::Var(label, expr.span())
        };
        let synthetic = match &d.call {
            Expr::Call(c) => {
                let args = c
                    .args
                    .iter()
                    .enumerate()
                    .map(|(i, a)| CallArg {
                        expr: stash(self, format!("\0defer{n}_a{i}"), &a.expr),
                        mode: a.mode.clone(),
                        span: a.span,
                    })
                    .collect();
                Expr::Call(Box::new(CallExpr {
                    callee: c.callee.clone(),
                    args,
                    span: c.span,
                }))
            }
            Expr::MethodCall(m) => {
                let object = stash(self, format!("\0defer{n}_self"), &m.object);
                let args = m
                    .args
                    .iter()
                    .enumerate()
                    .map(|(i, a)| CallArg {
                        expr: stash(self, format!("\0defer{n}_a{i}"), &a.expr),
                        mode: a.mode.clone(),
                        span: a.span,
                    })
                    .collect();
                Expr::MethodCall(Box::new(MethodCallExpr {
                    object,
                    method: m.method.clone(),
                    args,
                    span: m.span,
                }))
            }
            Expr::Print(arg, newline, span) => {
                let stashed = stash(self, format!("\0defer{n}_p"), arg);
                Expr::Print(Box::new(stashed), *newline, *span)
            }
            // The checker rejects everything else (E0905).
            _ => unreachable!("non-call defer reached codegen"),
        };
        let stmt = Stmt::Expr(ExprStmt {
            expr: synthetic,
            span: d.span,
        });
        let flag_offset = if let Some(frame_ptr) = async_frame {
            // Mark the site REGISTERED for the cancel entry, cleared again by
            // any normal-path flush (willow-vynv.3).
            let off = self.async_frame_offsets[&d.span];
            let one = self.builder.ins().iconst(types::I64, 1);
            self.builder
                .ins()
                .store(MemFlagsData::new(), one, frame_ptr, off);
            self.collected_defer_sites.push(super::AsyncDeferSite {
                stmt: stmt.clone(),
                flag_offset: off,
                bindings: bindings.clone(),
            });
            Some(off)
        } else {
            None
        };
        self.defer_stack
            .last_mut()
            .expect("defer outside any scope frame")
            .push(super::DeferEntry {
                stmt,
                flag_offset,
                bindings,
            });
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
                    self.builder
                        .ins()
                        .store(MemFlagsData::new(), val, base, offset);
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
                if let Some(class_name) = class_name_for_object_type(&obj_type)
                    && let Some(layout) = self.class_layouts.get(&class_name).cloned()
                    && let Some(idx) = layout.iter().position(|(n, _)| n == &s.field)
                {
                    // Word 0 is type_id; fields start at word 1 → offset = (idx + 1) * 8.
                    let offset = (idx as i32 + 1) * 8;
                    // Box a class value when the field's type is an interface.
                    let field_ty = layout[idx].1.clone();
                    let val = self.emit_expr_coerced(&s.value, &field_ty);
                    self.builder
                        .ins()
                        .store(MemFlagsData::new(), val, ptr, offset);
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
                    self.builder.ins().store(MemFlagsData::new(), val, addr, 0);
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
            // break/continue (willow-kzka): pop GC roots acquired since loop
            // entry (this path diverges, so the runtime stack must balance),
            // jump, and mark the block terminated. The compile-time root
            // counter is NOT adjusted — enclosing scopes restore it, exactly
            // like the `return` path.
            Stmt::Break(_) | Stmt::Continue(_) => {
                let &(exit_block, continue_block, roots_at_entry, defer_depth) = self
                    .loop_stack
                    .last()
                    .expect("break/continue outside a loop reached codegen");
                // Scopes between this statement and the loop body unwind:
                // their defers run first (LIFO), then their roots pop.
                self.emit_flush_defers_from(defer_depth);
                let extra = self.gc_root_count - roots_at_entry;
                if extra > 0 {
                    self.emit_pop_roots_n(extra);
                }
                let target = if matches!(stmt, Stmt::Break(_)) {
                    exit_block
                } else {
                    continue_block
                };
                self.builder.ins().jump(target, &[]);
                self.terminated = true;
            }
            Stmt::For(s) => self.emit_for(s),
            Stmt::Defer(d) => self.emit_defer_register(d),
            Stmt::Return(s) => {
                // `fn main() -> Result<void, E>`: returns are turned into an exit
                // (Err -> report + non-zero; Ok / bare return -> exit 0), since
                // `willow_user_main` is void (willow-exg).
                if self.main_result_err_ty.is_some() {
                    match &s.value {
                        Some(val_expr) if is_zero_arg_result_ok(val_expr) => {
                            // `return Result::Ok();` — success, no construction.
                            self.emit_flush_defers_from(0);
                            if self.gc_root_count > 0 {
                                self.emit_pop_roots_n(self.gc_root_count);
                            }
                            self.builder.ins().return_(&[]);
                        }
                        Some(val_expr) => {
                            let result = self.emit_expr(val_expr);
                            self.emit_push_root(result);
                            self.emit_flush_defers_from(0);
                            self.emit_pop_roots_n(1);
                            self.gc_root_count -= 1;
                            self.emit_main_result_exit(result);
                        }
                        None => {
                            self.emit_flush_defers_from(0);
                            if self.gc_root_count > 0 {
                                self.emit_pop_roots_n(self.gc_root_count);
                            }
                            self.builder.ins().return_(&[]);
                        }
                    }
                    self.terminated = true;
                    return;
                }
                // Cooperative poll fn: a return stores into the frame's
                // `__result` slot and returns the Ready status (willow-zvkv —
                // reached from nested statement control flow like match arms).
                if let Some(frame) = self.coop_frame {
                    if let (Some(off), Some(val_expr)) = (self.coop_result_offset, &s.value) {
                        let val = self.emit_expr(val_expr);
                        self.builder
                            .ins()
                            .store(MemFlagsData::new(), val, frame, off);
                    } else if let Some(val_expr) = &s.value {
                        self.emit_expr(val_expr);
                    }
                    // Run pending defers AFTER the result is stored in the
                    // frame (it is safe there across the flush) and clear
                    // their flags (willow-vynv.3).
                    if !self.defer_stack.is_empty() {
                        self.emit_flush_defers_from(0);
                    }
                    if self.gc_root_count > 0 {
                        self.emit_pop_roots_n(self.gc_root_count);
                    }
                    let ready = self.builder.ins().iconst(types::I32, 1);
                    self.builder.ins().return_(&[ready]);
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
                        // Run pending defers AFTER the value is computed (Go
                        // semantics) — rooting it across the flush, which may
                        // allocate (willow-vynv.2).
                        if !self.defer_stack.iter().all(|f| f.is_empty()) {
                            let gc_val = is_gc_managed(&target, self.enum_infos);
                            if gc_val {
                                self.emit_push_root(val);
                            }
                            self.emit_flush_defers_from(0);
                            if gc_val {
                                self.emit_pop_roots_n(1);
                                self.gc_root_count -= 1;
                            }
                        }
                        if self.gc_root_count > 0 {
                            self.emit_pop_roots_n(self.gc_root_count);
                        }
                        self.builder.ins().return_(&[val]);
                    } else {
                        self.emit_flush_defers_from(0);
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
        self.loop_stack.push((
            exit_block,
            header,
            self.gc_root_count,
            self.defer_stack.len(),
        ));
        self.emit_block(&s.body);
        self.loop_stack.pop();
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
        // `continue` target: the increment must still run (willow-kzka).
        let inc_block = self.builder.create_block();
        let exit_block = self.builder.create_block();
        self.builder.ins().jump(header, &[]);

        self.builder.switch_to_block(header);
        let idx = self.builder.ins().stack_load(types::I64, idx_slot, 0);
        // Inline `len` as a load from the handle (offset 0) instead of calling
        // willow_array_len (willow-pcoy). Re-read EVERY iteration on purpose:
        // the body may push/pop this same array, and the header must observe
        // the new length before each entry.
        let len = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), arr, 0i32);
        let keep_going = self.builder.ins().icmp(IntCC::SignedLessThan, idx, len);
        self.builder
            .ins()
            .brif(keep_going, body_block, &[], exit_block, &[]);

        self.builder.switch_to_block(body_block);
        self.builder.seal_block(body_block);
        if let Some(VarStorage::Stack { slot, .. }) = self.vars.get(&s.name).cloned() {
            // Inline element read (willow-pcoy): buffer = handle[3] (offset
            // 24), element i at buffer + 8 + i*8 (the buffer is
            // length-prefixed by cap). The BUFFER pointer is re-read each
            // iteration because a push in the body may reallocate it (the
            // HANDLE pointer is stable). Bounds are guaranteed by the header
            // check (0 <= idx < len re-checked every entry).
            let buffer = self
                .builder
                .ins()
                .load(types::I64, MemFlagsData::new(), arr, 24i32);
            let byte_off = self.builder.ins().ishl_imm(idx, 3);
            let addr = self.builder.ins().iadd(buffer, byte_off);
            let word = self
                .builder
                .ins()
                .load(types::I64, MemFlagsData::new(), addr, 8i32);
            let elem = self.coerce_i64_to(word, &elem_ty);
            self.builder.ins().stack_store(elem, slot, 0);
        }
        self.terminated = false;
        self.loop_stack.push((
            exit_block,
            inc_block,
            self.gc_root_count,
            self.defer_stack.len(),
        ));
        self.emit_block(&s.body);
        self.loop_stack.pop();
        if !self.terminated {
            self.builder.ins().jump(inc_block, &[]);
        }
        self.builder.switch_to_block(inc_block);
        self.builder.seal_block(inc_block);
        let idx = self.builder.ins().stack_load(types::I64, idx_slot, 0);
        let one = self.builder.ins().iconst(types::I64, 1);
        let next = self.builder.ins().iadd(idx, one);
        self.builder.ins().stack_store(next, idx_slot, 0);
        self.builder.ins().jump(header, &[]);

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
        self.builder
            .ins()
            .store(MemFlagsData::new(), start, ptr, 0i32);
        self.builder
            .ins()
            .store(MemFlagsData::new(), end, ptr, 8i32);
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
            .load(types::I64, MemFlagsData::new(), ptr, 0i32);
        let end = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), ptr, 8i32);
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
        // `continue` target: the increment must still run (willow-kzka).
        let inc_block = self.builder.create_block();
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
        self.loop_stack.push((
            exit_block,
            inc_block,
            self.gc_root_count,
            self.defer_stack.len(),
        ));
        self.emit_block(&s.body);
        self.loop_stack.pop();
        if !self.terminated {
            self.builder.ins().jump(inc_block, &[]);
        }
        self.builder.switch_to_block(inc_block);
        self.builder.seal_block(inc_block);
        let current = self.builder.ins().stack_load(types::I64, current_slot, 0);
        let one = self.builder.ins().iconst(types::I64, 1);
        let next = self.builder.ins().iadd(current, one);
        self.builder.ins().stack_store(next, current_slot, 0);
        self.builder.ins().jump(header, &[]);

        self.builder.seal_block(header);
        self.builder.switch_to_block(exit_block);
        self.builder.seal_block(exit_block);
        self.vars = saved_vars;
        self.terminated = false;
    }
}
