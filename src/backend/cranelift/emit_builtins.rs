//! Codegen for builtin sync-primitive method calls (`AtomicI64`/`AtomicBool`,
//! `Mutex`/`RwLock`) — `impl FuncGen` block extracted from `mod.rs`. Methods are
//! `pub(super)` so the dispatch in `mod.rs` can call them; they reach FuncGen's
//! private fields/methods as a child module of the backend.

use cranelift_codegen::ir::{InstBuilder, types};
use cranelift_module::Module;

use crate::parser::ast::*;

use super::{FuncGen, channel_runtime_suffix};

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Emit a method call on a `Mutex<T>` (`get`/`set`) or `RwLock<T>`
    /// (`read`/`write`) value (willow-dgwo.3). Values are coerced through the
    /// word-based lock ABI.
    pub(super) fn emit_lock_method_call(
        &mut self,
        lock_ptr: cranelift_codegen::ir::Value,
        is_mutex: bool,
        elem_ty: &Type,
        m: &MethodCallExpr,
    ) -> cranelift_codegen::ir::Value {
        let rt = match (is_mutex, m.method.as_str()) {
            (true, "get") => "willow_mutex_get",
            (true, "set") => "willow_mutex_set",
            (false, "read") => "willow_rwlock_read",
            (false, "write") => "willow_rwlock_write",
            _ => unreachable!("lock method validated by the type checker"),
        };
        let fid = self.func_ids[rt];
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let mut args = vec![lock_ptr];
        if let Some(arg) = m.args.first() {
            let val = self.emit_expr(&arg.expr);
            args.push(self.coerce_to_i64(val, elem_ty));
        }
        let call = self.builder.ins().call(fref, &args);
        let results = self.builder.inst_results(call);
        if results.is_empty() {
            // `set` / `write` return void.
            self.builder.ins().iconst(types::I8, 0)
        } else {
            // `get` / `read` return a word — coerce back to the element type.
            self.coerce_i64_to(results[0], elem_ty)
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
        let fid = self.func_ids[rt.as_str()];
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let mut args = vec![atomic_ptr];
        if let Some(arg) = m.args.first() {
            args.push(self.emit_expr(&arg.expr));
        }
        let call = self.builder.ins().call(fref, &args);
        let results = self.builder.inst_results(call);
        if results.is_empty() {
            // `store` returns void.
            self.builder.ins().iconst(types::I8, 0)
        } else {
            results[0]
        }
    }

    pub(super) fn emit_channel_method_call(
        &mut self,
        channel_ptr: cranelift_codegen::ir::Value,
        element_ty: &Type,
        m: &MethodCallExpr,
    ) -> cranelift_codegen::ir::Value {
        match m.method.as_str() {
            "send" => {
                if let Some(arg) = m.args.first() {
                    let runtime_name =
                        format!("willow_channel_send_{}", channel_runtime_suffix(element_ty));
                    let fid = self.func_ids[&runtime_name];
                    let fref = self.module.declare_func_in_func(fid, self.builder.func);
                    let value = self.emit_expr(&arg.expr);
                    self.builder.ins().call(fref, &[channel_ptr, value]);
                }
                self.builder.ins().iconst(types::I8, 0)
            }
            "recv" => {
                let runtime_name =
                    format!("willow_channel_recv_{}", channel_runtime_suffix(element_ty));
                let fid = self.func_ids[&runtime_name];
                let fref = self.module.declare_func_in_func(fid, self.builder.func);
                let call = self.builder.ins().call(fref, &[channel_ptr]);
                self.builder.inst_results(call)[0]
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
