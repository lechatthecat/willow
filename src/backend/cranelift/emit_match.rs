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
        // Reuse ordinary virtual dispatch: inherited implementations collapse
        // to one target; real overrides use the class descriptor's shared slot.
        let plan = self.plan_virtual_call(e1_name, "into");
        let fid = self.func_ids[&plan.mangled];
        let call = if let Some(slot) = plan.virtual_slot {
            let fnptr = self.emit_vtable_slot_load(e1_payload, slot);
            // Every `into` in the hierarchy shares one signature (an
            // `override` may not change it), so the resolved target's return
            // type describes them all: `Into<f64>` returns a scalar, not a
            // pointer.
            let pointer = reference_type(self.module.target_config());
            let ret_type = self
                .func_return_types
                .get(&plan.mangled)
                .cloned()
                .unwrap_or(Type::Void);
            let mut signature = self.module.make_signature();
            signature
                .params
                .push(cranelift_codegen::ir::AbiParam::new(pointer));
            if ret_type != Type::Void {
                signature
                    .returns
                    .push(cranelift_codegen::ir::AbiParam::new(clif_type(
                        pointer, &ret_type,
                    )));
            }
            let signature = self.builder.import_signature(signature);
            self.builder
                .ins()
                .call_indirect(signature, fnptr, &[e1_payload])
        } else {
            let fref = self.module.declare_func_in_func(fid, self.builder.func);
            self.builder.ins().call(fref, &[e1_payload])
        };
        self.builder.inst_results(call)[0]
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
