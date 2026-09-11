//! Runtime leaf operations used by the explicit LIR control-flow graph.
use super::*;
use cranelift_codegen::ir::{InstBuilder, Value, types};
use cranelift_module::Module;

impl<'a, 'b> FuncGen<'a, 'b> {
    pub(super) fn emit_flat_start_task(
        &mut self,
        callee: crate::semantic::ids::FunctionId,
        args: &[Value],
        params: &[Type],
        span: crate::diagnostics::Span,
    ) -> Value {
        let mut roots = 0;
        for (&value, ty) in args.iter().zip(params) {
            if is_gc_managed(ty, self.enum_infos) {
                self.emit_push_root(value);
                roots += 1;
            }
        }
        let fid = *self
            .func_ids
            .get_id(&callee)
            .expect("task constructor registered");
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let pushed = self.emit_callstack_push(&callee.to_string(), span);
        let depth = self.emit_pre_user_call_panic_depth(&callee.to_string());
        let call = self.builder.ins().call(fref, args);
        let frame = self.builder.inst_results(call)[0];
        if pushed {
            self.emit_callstack_pop();
        }
        self.emit_pop_roots_n(roots);
        self.gc_root_count -= roots;
        self.emit_post_willow_call_panic_check(depth);
        let id = self.builder.ins().load(
            types::I64,
            cranelift_codegen::ir::MemFlagsData::new(),
            frame,
            super::async_frame_slot_offset(super::FRAME_SLOT_TASK_ID),
        );
        self.emit_set_spawn_site(id, span.line);
        frame
    }

    pub(super) fn emit_flat_await_future(&mut self, future: Value, result_ty: &Type) -> Value {
        // Future<T> is an opaque, nonmoving runtime Box, not a GC heap
        // object. Its runtime implementation owns any result roots.
        self.emit_value_runtime_call(
            super::type_helpers::future_await_runtime_name(result_ty),
            &[future],
        )
    }

    /// One idle iteration. Selection, retries and case bodies live in LIR.
    pub(super) fn emit_flat_select_idle_wait(&mut self, deadlines: &[Value]) -> Value {
        let minimum = deadlines.iter().copied().reduce(|current, deadline| {
            let before = self
                .builder
                .ins()
                .icmp(IntCC::SignedLessThan, deadline, current);
            self.builder.ins().select(before, deadline, current)
        });
        let completed = match minimum {
            Some(deadline) => {
                self.emit_value_runtime_call("willow_sched_run_until_deadline", &[deadline])
            }
            None => self.emit_value_runtime_call("willow_sched_run", &[]),
        };
        let progressed = self.builder.ins().icmp_imm_s(IntCC::NotEqual, completed, 0);
        let wait = self.builder.create_block();
        let done = self.builder.create_block();
        self.builder.ins().brif(progressed, done, &[], wait, &[]);
        self.builder.switch_to_block(wait);
        self.builder.seal_block(wait);
        match minimum {
            Some(deadline) => {
                self.emit_void_runtime_call("willow_sleep_until_monotonic", &[deadline])
            }
            None => self.emit_void_runtime_call("willow_select_idle_wait", &[]),
        }
        self.builder.ins().jump(done, &[]);
        self.builder.switch_to_block(done);
        self.builder.seal_block(done);
        self.builder.ins().iconst(types::I64, 0)
    }
}
