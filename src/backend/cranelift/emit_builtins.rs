//! Codegen for builtin sync-primitive method calls (`AtomicI64`/`AtomicBool`,
//! `Mutex`/`RwLock`) — `impl FuncGen` block extracted from `mod.rs`. Methods are
//! `pub(super)` so the dispatch in `mod.rs` can call them; they reach FuncGen's
//! private fields/methods as a child module of the backend.

use cranelift_codegen::ir::{InstBuilder, types};
use cranelift_module::Module;

use crate::parser::ast::*;

use super::type_helpers::is_gc_managed;
use super::{FuncGen, channel_runtime_suffix};

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Emit a method call on a `BlockingCell<T>` (`get`/`set`) or `BlockingRwCell<T>`
    /// (`read`/`write`) value (willow-dgwo.3). Values are coerced through the
    /// word-based lock ABI.
    ///
    /// `Mutex<T>` is deliberately absent: its accessors were removed in
    /// willow-38w.1.4 because a `get`/`set` pair around a read-modify-write
    /// loses updates. Scheduler-aware exclusive access goes through
    /// `lock <mutex> as [mut] value { .. }` and its own runtime ABI.
    pub(super) fn emit_lock_method_call(
        &mut self,
        lock_ptr: cranelift_codegen::ir::Value,
        lock: &str,
        elem_ty: &Type,
        m: &MethodCallExpr,
    ) -> cranelift_codegen::ir::Value {
        let rt = match (lock, m.method.as_str()) {
            ("BlockingCell", "get") => "willow_blocking_cell_get",
            ("BlockingCell", "set") => "willow_blocking_cell_set",
            ("BlockingRwCell", "read") => "willow_blocking_rw_cell_read",
            ("BlockingRwCell", "write") => "willow_blocking_rw_cell_write",
            _ => unreachable!("lock method validated by the type checker"),
        };
        let mut args = vec![lock_ptr];
        if let Some(arg) = m.args.first() {
            let val = self.emit_expr(&arg.expr);
            args.push(self.coerce_to_i64(val, elem_ty));
        }
        let result = self.emit_runtime_call_with_cleanup(rt, &args, |_| {});
        if let Some(result) = result {
            // `get` / `read` return a word — coerce back to the element type.
            self.coerce_i64_to(result, elem_ty)
        } else {
            // `set` / `write` return void.
            self.builder.ins().iconst(types::I8, 0)
        }
    }

    /// Emit a method call on an `AtomicI64` / `AtomicBool` value (willow-dgwo.3).
    /// `atomic_ptr` is the GC-allocated cell pointer; atomic ops never allocate,
    /// so no extra rooting is needed here.
    pub(super) fn emit_atomic_method_call(
        &mut self,
        atomic_ptr: cranelift_codegen::ir::Value,
        is_i64: bool,
        m: &MethodCallExpr,
    ) -> cranelift_codegen::ir::Value {
        let suffix = if is_i64 { "i64" } else { "bool" };
        let rt = format!("willow_atomic_{suffix}_{}", m.method);
        let mut args = vec![atomic_ptr];
        if let Some(arg) = m.args.first() {
            args.push(self.emit_expr(&arg.expr));
        }
        let result = self.emit_runtime_call_with_cleanup(&rt, &args, |_| {});
        if let Some(result) = result {
            result
        } else {
            // `store` returns void.
            self.builder.ins().iconst(types::I8, 0)
        }
    }

    pub(super) fn emit_channel_method_call(
        &mut self,
        channel_ptr: cranelift_codegen::ir::Value,
        element_ty: &Type,
        m: &MethodCallExpr,
    ) -> cranelift_codegen::ir::Value {
        // A synchronous `send` on a full bounded channel, and a synchronous
        // `recv` on an empty one, drive the scheduler — which allocates, so a
        // collection can happen inside the runtime call. The channel itself is
        // a GC object (willow-p4er) and here it is only an SSA temporary, so
        // both it and a freshly built argument (`ch.send(a + b)`) must be
        // shadow-stack roots across the call (willow-o038 review).
        let ptr_ty = self.module.target_config().pointer_type();
        match m.method.as_str() {
            "send" => {
                if let Some(arg) = m.args.first() {
                    let runtime_name =
                        format!("willow_channel_send_{}", channel_runtime_suffix(element_ty));
                    // Rooted BEFORE the argument is evaluated: that expression
                    // can allocate and collect on its own.
                    let ch_slot = self.emit_push_root(channel_ptr);
                    let mut roots = 1;
                    let mut value = self.emit_expr(&arg.expr);
                    if is_gc_managed(element_ty, self.enum_infos) {
                        let vslot = self.emit_push_root(value);
                        roots += 1;
                        value = self.stack_load(ptr_ty, vslot);
                    }
                    let channel_ptr = self.stack_load(ptr_ty, ch_slot);
                    self.emit_runtime_call_with_cleanup(
                        &runtime_name,
                        &[channel_ptr, value],
                        |this| {
                            this.emit_pop_roots_n(roots);
                            this.gc_root_count -= roots;
                        },
                    );
                }
                self.builder.ins().iconst(types::I8, 0)
            }
            "recv" => {
                let runtime_name =
                    format!("willow_channel_recv_{}", channel_runtime_suffix(element_ty));
                let ch_slot = self.emit_push_root(channel_ptr);
                let channel_ptr = self.stack_load(ptr_ty, ch_slot);
                self.emit_runtime_call_with_cleanup(&runtime_name, &[channel_ptr], |this| {
                    this.emit_pop_roots_n(1);
                    this.gc_root_count -= 1;
                })
                .expect("channel recv returns a value")
            }
            "close" => {
                let fid = self.func_id("willow_channel_close");
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                self.builder.ins().call(fref, &[channel_ptr]);
                self.builder.ins().iconst(types::I8, 0)
            }
            _ => self.builder.ins().iconst(types::I64, 0),
        }
    }
}
