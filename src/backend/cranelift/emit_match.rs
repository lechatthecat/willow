use cranelift_codegen::ir::{InstBuilder, MemFlagsData, condcodes::IntCC, types};

use super::*;

/// Emit the process-boundary branch shared by synchronous and cooperative
/// `Result<void, E>` mains. The caller owns any GC-root cleanup: after this
/// point no operation can allocate before the tag/payload is consumed.
pub(super) fn emit_main_result_exit_raw(
    builder: &mut FunctionBuilder<'_>,
    module: &mut ObjectModule,
    fail_id: FuncId,
    result_ptr: cranelift_codegen::ir::Value,
    err_is_string: bool,
) {
    let tag = builder
        .ins()
        .load(types::I64, MemFlagsData::new(), result_ptr, 0i32);
    let err_tag = builder.ins().iconst(types::I64, 1); // Err = tag 1
    let is_err = builder.ins().icmp(IntCC::Equal, tag, err_tag);
    let err_block = builder.create_block();
    let ok_block = builder.create_block();
    builder.ins().brif(is_err, err_block, &[], ok_block, &[]);

    builder.switch_to_block(err_block);
    builder.seal_block(err_block);
    let msg = if err_is_string {
        builder.ins().load(
            reference_type(module.target_config()),
            MemFlagsData::new(),
            result_ptr,
            8i32,
        )
    } else {
        builder
            .ins()
            .iconst(reference_type(module.target_config()), 0)
    };
    let fail_ref = module.declare_func_in_func(fail_id, builder.func);
    builder.ins().call(fail_ref, &[msg]);
    // willow_main_fail is noreturn; trap to satisfy the verifier.
    builder
        .ins()
        .trap(cranelift_codegen::ir::TrapCode::unwrap_user(1));

    builder.switch_to_block(ok_block);
    builder.seal_block(ok_block);
    builder.ins().return_(&[]);
}

impl<'a, 'b> FuncGen<'a, 'b> {
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
            .filter(|(cls, _)| self.class_is_a(&cls.to_string(), e1_name))
            .filter_map(|(cls, &id)| {
                self.resolve_method_func_id(&cls.to_string(), "into")
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

        // Multiple candidates: switch on the payload's runtime type_id, read
        // through its class descriptor (willow-fm7t).
        let type_id = self.emit_load_runtime_type_id(e1_payload);
        let result_var = self
            .builder
            .declare_var(reference_type(self.module.target_config()));
        let zero = self
            .builder
            .ins()
            .iconst(reference_type(self.module.target_config()), 0);
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

    /// Leave a `Result<void, E>` main by inspecting the `Result` value: `Err`
    /// reports the payload and exits non-zero; `Ok` returns void (exit 0). Pops
    /// the function's GC roots first (we leave the function on both paths).
    /// See willow-exg.
    pub(super) fn emit_main_result_exit(&mut self, result_ptr: cranelift_codegen::ir::Value) {
        if self.gc_root_count > 0 {
            self.emit_pop_roots_n(self.gc_root_count);
        }
        let err_is_string = self.main_result_err_ty.as_ref() == Some(&Type::String);

        let fail_id = self.func_id("willow_main_fail");
        emit_main_result_exit_raw(
            self.builder,
            self.module,
            fail_id,
            result_ptr,
            err_is_string,
        );
    }

    pub(super) fn emit_load_enum_tag(
        &mut self,
        ptr: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        self.builder
            .ins()
            .load(types::I64, MemFlagsData::new(), ptr, 0i32)
    }
}
