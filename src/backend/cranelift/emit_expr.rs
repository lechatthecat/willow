use cranelift_codegen::ir::{InstBuilder, condcodes::IntCC, types};
use cranelift_module::Module;

use super::*;

/// `willow_int_overflow_panic` operator kinds (willow-jz15.14).
pub(super) const INT_OVERFLOW_ADD: i64 = 0;
pub(super) const INT_OVERFLOW_SUB: i64 = 1;
pub(super) const INT_OVERFLOW_MUL: i64 = 2;
pub(super) const INT_OVERFLOW_NEG: i64 = 3;
pub(super) const INT_OVERFLOW_POW: i64 = 4;
pub(super) const INT_OVERFLOW_SHL: i64 = 5;
pub(super) const INT_OVERFLOW_SHR: i64 = 6;

impl<'a, 'b> FuncGen<'a, 'b> {
    pub(super) fn emit_string_literal(&mut self, value: &str) -> cranelift_codegen::ir::Value {
        if let Some(data_id) = self.string_literals.get(value) {
            // Load the address of the static raw bytes.
            let gv = self
                .module
                .declare_data_in_func(data_id.bytes, self.builder.func);
            let ptr_ty = reference_type(self.module.target_config());
            let bytes_ptr = self.builder.ins().symbol_value(ptr_ty, gv);
            let slot_gv = self
                .module
                .declare_data_in_func(data_id.slot, self.builder.func);
            let slot_ptr = self.builder.ins().symbol_value(ptr_ty, slot_gv);
            // The helper performs an acquire load on hits; only first use allocates.
            let len_val = self.builder.ins().iconst(types::I64, value.len() as i64);
            let fid = self.func_id("willow_string_literal_slot");
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            let call = self
                .builder
                .ins()
                .call(fref, &[slot_ptr, bytes_ptr, len_val]);
            return self.builder.inst_results(call)[0];
        }
        self.builder
            .ins()
            .iconst(reference_type(self.module.target_config()), 0)
    }

    /// Emit the unwind for a `panic(...)` whose message is already in hand,
    /// and return the expression's unreachable placeholder value.
    ///
    /// Both emitters share this: the AST caller — static-property initializers,
    /// the only bodies still on that path — assembles the message from
    /// `CallArg`s, the LIR walker from `HirExpr`s (willow-0g8j.2.5), and the
    /// unwind protocol below — which branch of the three panic worlds applies,
    /// and in what order the file metadata is built relative to the message's
    /// root — must not be able to differ between them.
    ///
    /// The caller is responsible for `self.terminated`: this sets it, and no
    /// instruction may follow the returned value in the same Cranelift block.
    pub(super) fn emit_panic_with_message(
        &mut self,
        msg: cranelift_codegen::ir::Value,
        span: crate::diagnostics::Span,
    ) -> cranelift_codegen::ir::Value {
        if self.coop_frame.is_some() {
            let result = self
                .builder
                .ins()
                .iconst(reference_type(self.module.target_config()), 0);
            self.emit_language_panic(msg, Some(span));
            return result;
        }
        if !self.is_async {
            // Build file metadata while the message is rooted: creating the
            // file String may collect before the runtime has taken ownership
            // of either argument.
            let msg_root = self.emit_push_relocatable_root(msg);
            let source_file = self.source_file.to_string();
            let file_ptr = self.emit_string_literal(&source_file);
            let msg = self.emit_reload_root(msg_root, msg);
            let line = self.builder.ins().iconst(types::I64, span.line as i64);
            let col = self.builder.ins().iconst(types::I64, span.col as i64);
            // Produce the expression's unreachable placeholder before the
            // unwind emits a terminator. A recovery jumps to a lexical
            // scope continuation and never consumes this value.
            let result = self
                .builder
                .ins()
                .iconst(reference_type(self.module.target_config()), 0);
            self.emit_runtime_call_with_cleanup(
                "willow_panic_raise",
                &[msg, file_ptr, line, col],
                |this| {
                    this.emit_pop_roots_n(1);
                    this.gc_root_count -= 1;
                },
            );
            // `willow_panic_raise` must always increase panic depth.
            self.builder.ins().trap(TrapCode::unwrap_user(1));
            self.terminated = true;
            return result;
        }

        unreachable!("async panic emission requires a cooperative frame")
    }

    /// Lower the compiler-known `recover()` builtin. Runtime capability is
    /// consulted only for a direct call inside the deferred AST currently
    /// executing; ordinary code and helper/lambda bodies construct `None`
    /// without touching panic state (willow-s9ej.3).
    pub(super) fn emit_recover_call(&mut self) -> cranelift_codegen::ir::Value {
        let panic_info_ty = Type::Named("PanicInfo".to_string().into());
        if self.recover_eligible_depth == 0 {
            return self.emit_alloc_option_none(&panic_info_ty);
        }

        let recover_id = self.func_id("willow_panic_recover");
        let recover_ref = self
            .module
            .declare_func_in_func(recover_id, self.builder.func);
        let call = self.builder.ins().call(recover_ref, &[]);
        let info = self.builder.inst_results(call)[0];
        let is_none = self.builder.ins().icmp_imm_u(IntCC::Equal, info, 0);
        let none_block = self.builder.create_block();
        let some_block = self.builder.create_block();
        let merge = self.builder.create_block();
        let result = self
            .builder
            .declare_var(reference_type(self.module.target_config()));
        self.builder
            .ins()
            .brif(is_none, none_block, &[], some_block, &[]);

        self.builder.switch_to_block(none_block);
        self.builder.seal_block(none_block);
        let none = self.emit_alloc_option_none(&panic_info_ty);
        self.builder.def_var(result, none);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(some_block);
        self.builder.seal_block(some_block);
        let some = self.emit_alloc_option_some(&panic_info_ty, info);
        // The Option payload is now GC-visible; release the runtime's temporary
        // handoff root exactly once.
        let release_id = self.func_id("willow_panic_release_recovered");
        let release_ref = self
            .module
            .declare_func_in_func(release_id, self.builder.func);
        self.builder.ins().call(release_ref, &[info]);
        self.builder.def_var(result, some);
        self.builder.ins().jump(merge, &[]);

        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        self.builder.use_var(result)
    }

    pub(super) fn emit_debug_reference_call_scope_push(&mut self) {
        if self.build_mode != BuildMode::Debug {
            return;
        }
        let push_id = self.func_id("willow_debug_reference_call_scope_push");
        let push_ref = self.module.declare_func_in_func(push_id, self.builder.func);
        self.builder.ins().call(push_ref, &[]);
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
        let data_id = self.string_literals.get(value)?.bytes;
        let gv = self.module.declare_data_in_func(data_id, self.builder.func);
        let ptr_ty = reference_type(self.module.target_config());
        let bytes_ptr = self.builder.ins().symbol_value(ptr_ty, gv);
        let len = self.builder.ins().iconst(types::I64, value.len() as i64);
        Some((bytes_ptr, len))
    }

    /// Debug builds: push a call-chain frame (callee name + call-site location)
    /// before a user-function call. Returns `true` when a frame was pushed (so
    /// the caller knows to emit the matching pop). Passes raw static bytes (not
    /// WillowStrings) so the call stack does not allocate on the GC heap. Release
    /// builds are untouched (willow-992h).
    /// Guard an integer `/` or `%` against a zero divisor and the
    /// `i64::MIN / -1` overflow in every build mode. These are recoverable
    /// language faults, so the runtime raises and generated code propagates
    /// before Cranelift can execute the trapping arithmetic.
    pub(super) fn emit_int_div_guard(
        &mut self,
        lhs: cranelift_codegen::ir::Value,
        rhs: cranelift_codegen::ir::Value,
        is_rem: bool,
        span: crate::diagnostics::Span,
    ) {
        let panic_block = self.builder.create_block();
        self.builder.append_block_param(panic_block, types::I64); // kind
        let overflow_check = self.builder.create_block();
        let ok_block = self.builder.create_block();

        let zero_kind = self
            .builder
            .ins()
            .iconst(types::I64, if is_rem { 2 } else { 0 });
        let is_zero = self.builder.ins().icmp_imm_s(IntCC::Equal, rhs, 0);
        self.builder.ins().brif(
            is_zero,
            panic_block,
            &[zero_kind.into()],
            overflow_check,
            &[],
        );

        self.builder.switch_to_block(overflow_check);
        self.builder.seal_block(overflow_check);
        let is_min = self.builder.ins().icmp_imm_s(IntCC::Equal, lhs, i64::MIN);
        let is_neg1 = self.builder.ins().icmp_imm_s(IntCC::Equal, rhs, -1);
        let overflows = self.builder.ins().band(is_min, is_neg1);
        let ovf_kind = self
            .builder
            .ins()
            .iconst(types::I64, if is_rem { 3 } else { 1 });
        self.builder
            .ins()
            .brif(overflows, panic_block, &[ovf_kind.into()], ok_block, &[]);

        self.builder.switch_to_block(panic_block);
        self.builder.seal_block(panic_block);
        let kind = self.builder.block_params(panic_block)[0];
        let source_file = self.source_file.to_string();
        let file_ptr = self.emit_string_literal(&source_file);
        let line_val = self.builder.ins().iconst(types::I32, span.line as i64);
        let col_val = self.builder.ins().iconst(types::I32, span.col as i64);
        self.emit_void_runtime_call("willow_int_div_panic", &[kind, file_ptr, line_val, col_val]);
        // Runtime returning without raising would otherwise reach the unsafe
        // arithmetic. Treat that as an ABI violation.
        self.builder.ins().trap(TrapCode::unwrap_user(1));

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
    }

    /// Whether `+ - * **` and unary `-` on `i64` are checked for signed
    /// overflow: debug builds raise a recoverable panic, release builds wrap
    /// (willow-jz15.14).
    pub(super) fn overflow_checks(&self) -> bool {
        self.build_mode == BuildMode::Debug
    }

    /// Signed `+`, `-` or `*` and its overflow flag. Shared by the debug
    /// operator checks and the explicit `checked_*` methods.
    pub(super) fn emit_overflowing_binop(
        &mut self,
        op: &BinOp,
        lhs: cranelift_codegen::ir::Value,
        rhs: cranelift_codegen::ir::Value,
    ) -> (cranelift_codegen::ir::Value, cranelift_codegen::ir::Value) {
        let ins = self.builder.ins();
        match op {
            BinOp::Add => ins.sadd_overflow(lhs, rhs),
            BinOp::Sub => ins.ssub_overflow(lhs, rhs),
            BinOp::Mul => ins.smul_overflow(lhs, rhs),
            _ => unreachable!("only + - * have an overflowing form"),
        }
    }

    /// Signed negation and its overflow flag (`-i64::MIN`).
    pub(super) fn emit_overflowing_neg(
        &mut self,
        operand: cranelift_codegen::ir::Value,
    ) -> (cranelift_codegen::ir::Value, cranelift_codegen::ir::Value) {
        let negated = self.builder.ins().ineg(operand);
        let overflowed = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::Equal, operand, i64::MIN);
        (negated, overflowed)
    }

    /// Debug-build `+ - *` on `i64`: raise a recoverable panic on overflow.
    pub(super) fn emit_checked_int_binop(
        &mut self,
        op: &BinOp,
        lhs: cranelift_codegen::ir::Value,
        rhs: cranelift_codegen::ir::Value,
        span: crate::diagnostics::Span,
    ) -> cranelift_codegen::ir::Value {
        let (value, overflowed) = self.emit_overflowing_binop(op, lhs, rhs);
        let kind = match op {
            BinOp::Add => INT_OVERFLOW_ADD,
            BinOp::Sub => INT_OVERFLOW_SUB,
            _ => INT_OVERFLOW_MUL,
        };
        self.emit_int_overflow_guard(overflowed, kind, span);
        value
    }

    /// Debug-build `<<`/`>>` on `i64`: raise a recoverable panic when the
    /// amount is outside `0..64` (willow-jz15.8). One unsigned compare covers
    /// negative amounts too.
    pub(super) fn emit_checked_shift(
        &mut self,
        op: &BinOp,
        lhs: cranelift_codegen::ir::Value,
        rhs: cranelift_codegen::ir::Value,
        span: crate::diagnostics::Span,
    ) -> cranelift_codegen::ir::Value {
        let out_of_range =
            self.builder
                .ins()
                .icmp_imm_u(IntCC::UnsignedGreaterThanOrEqual, rhs, 64);
        let kind = if *op == BinOp::Shl {
            INT_OVERFLOW_SHL
        } else {
            INT_OVERFLOW_SHR
        };
        self.emit_int_overflow_guard(out_of_range, kind, span);
        if *op == BinOp::Shl {
            self.builder.ins().ishl(lhs, rhs)
        } else {
            self.builder.ins().sshr(lhs, rhs)
        }
    }

    /// Branch to a cold `willow_int_overflow_panic` path when `overflowed`
    /// (an `i8` flag) is set. Each site keeps its own block: the unwind path
    /// depends on the cleanup scope active at that site.
    pub(super) fn emit_int_overflow_guard(
        &mut self,
        overflowed: cranelift_codegen::ir::Value,
        kind: i64,
        span: crate::diagnostics::Span,
    ) {
        let panic_block = self.builder.create_block();
        let ok_block = self.builder.create_block();
        self.builder
            .ins()
            .brif(overflowed, panic_block, &[], ok_block, &[]);

        self.builder.switch_to_block(panic_block);
        self.builder.seal_block(panic_block);
        self.builder.set_cold_block(panic_block);
        let kind = self.builder.ins().iconst(types::I64, kind);
        let source_file = self.source_file.to_string();
        let file_ptr = self.emit_string_literal(&source_file);
        let line_val = self.builder.ins().iconst(types::I32, span.line as i64);
        let col_val = self.builder.ins().iconst(types::I32, span.col as i64);
        self.emit_void_runtime_call(
            "willow_int_overflow_panic",
            &[kind, file_ptr, line_val, col_val],
        );
        // Runtime returning without raising is an ABI violation.
        self.builder.ins().trap(TrapCode::unwrap_user(1));

        self.builder.switch_to_block(ok_block);
        self.builder.seal_block(ok_block);
    }

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
        self.callstack_frame_depth += 1;
        true
    }

    /// Debug builds: pop the most recent call-chain frame after a call returns.
    pub(super) fn emit_callstack_unwind_edge(&mut self) {
        for _ in 0..self.lir_reference_scopes.len() {
            self.emit_debug_reference_call_clear();
        }
        let depth = self.callstack_frame_depth;
        for _ in 0..depth {
            self.emit_callstack_pop();
        }
        self.callstack_frame_depth = depth;
    }

    pub(super) fn emit_callstack_pop(&mut self) {
        self.callstack_frame_depth = self
            .callstack_frame_depth
            .checked_sub(1)
            .expect("compiler call-stack frame underflow");
        let pop_id = self.func_id("willow_callstack_pop");
        let pop_ref = self.module.declare_func_in_func(pop_id, self.builder.func);
        self.builder.ins().call(pop_ref, &[]);
    }

    pub(super) fn emit_replay_reference_scopes(&mut self) {
        for scope in self.lir_reference_scopes.clone() {
            self.emit_debug_reference_call_scope_push();
            for record in scope {
                self.emit_flat_reference_debug(&record.argument, &record.callee, record.index);
            }
        }
    }
}
