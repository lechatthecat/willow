//! Block, suspension, cleanup and return emission.

use super::*;
use crate::ir::ir_variants;
use cranelift_codegen::ir::TrapCode;
use willow_abi::runtime_symbols::names as rt;

impl<'a, 'b> FuncGen<'a, 'b> {
    #[allow(
        clippy::too_many_lines,
        reason = "the select-instruction dispatch keeps probe, fair choice, losing-arm cleanup and commit lowering together so shared wrapper roots and generation ownership remain visible"
    )]
    fn emit_lir_select_instruction(&mut self, function: &LirFunction, inst: &LirInst) {
        match inst {
            LirInst::SelectInit { operations } => {
                for operation in operations {
                    if let LirSelectOp::Timeout { millis, deadline } = operation {
                        let millis = self.load_lir_local(function, *millis);
                        let now = self.emit_value_runtime_call(rt::willow_monotonic_millis, &[]);
                        let deadline_value = self.builder.ins().iadd(now, millis);
                        self.store_lir_local(function, *deadline, deadline_value);
                    }
                }
            }
            LirInst::SelectProbe { operations, ready } => {
                for (operation, ready_local) in operations.iter().zip(ready) {
                    let Some(ready_local) = ready_local else {
                        continue;
                    };
                    let ready_value = match operation {
                        LirSelectOp::Recv { channel, .. } => {
                            let channel = self.load_lir_local(function, *channel);
                            let raw = self
                                .emit_value_runtime_call(rt::willow_channel_recv_ready, &[channel]);
                            self.builder.ins().icmp_imm_s(IntCC::NotEqual, raw, 0)
                        }
                        LirSelectOp::Send { channel, .. } => {
                            let channel = self.load_lir_local(function, *channel);
                            let raw = self
                                .emit_value_runtime_call(rt::willow_channel_send_ready, &[channel]);
                            self.builder.ins().icmp_imm_s(IntCC::NotEqual, raw, 0)
                        }
                        LirSelectOp::Join { task, .. } => {
                            let task = self.load_lir_local(function, *task);
                            let id = self.builder.ins().load(
                                types::I64,
                                MemFlagsData::new(),
                                task,
                                async_frame_slot_offset(
                                    FRAME_SLOT_TASK_ID,
                                    reference_type(self.module.target_config()).bytes(),
                                ),
                            );
                            let raw =
                                self.emit_value_runtime_call(rt::willow_frame_await, &[task, id]);
                            self.builder.ins().icmp_imm_s(IntCC::NotEqual, raw, 0)
                        }
                        LirSelectOp::Timeout { deadline, .. } => {
                            let deadline = self.load_lir_local(function, *deadline);
                            let now =
                                self.emit_value_runtime_call(rt::willow_monotonic_millis, &[]);
                            self.builder
                                .ins()
                                .icmp(IntCC::SignedGreaterThanOrEqual, now, deadline)
                        }
                        LirSelectOp::Default => unreachable!(),
                    };
                    let one = self.builder.ins().iconst(types::I8, 1);
                    let zero = self.builder.ins().iconst(types::I8, 0);
                    let ready_value = self.builder.ins().select(ready_value, one, zero);
                    self.store_lir_local(function, *ready_local, ready_value);
                }
            }
            LirInst::SelectPick { ready, chosen } => {
                let mut flags = Vec::with_capacity(ready.len());
                let mut total = self.builder.ins().iconst(types::I64, 0);
                for ready in ready {
                    let flag = if let Some(ready) = ready {
                        let ready = self.load_lir_local(function, *ready);
                        self.builder.ins().uextend(types::I64, ready)
                    } else {
                        self.builder.ins().iconst(types::I64, 0)
                    };
                    total = self.builder.ins().iadd(total, flag);
                    flags.push(flag);
                }
                // `willow_select_rotation` advances a PROCESS-WIDE counter, so
                // it may only be called on a probe that actually picks a case.
                // Calling it unconditionally would shift the fairness sequence
                // of every other select in the program.
                let none = self.builder.ins().icmp_imm_s(IntCC::Equal, total, 0);
                let unready = self.builder.ins().iconst(types::I64, -1);
                self.store_lir_local(function, *chosen, unready);
                let pick = self.builder.create_block();
                let done = self.builder.create_block();
                self.builder.ins().brif(none, done, &[], pick, &[]);

                self.builder.switch_to_block(pick);
                self.builder.seal_block(pick);
                let rotation = self.emit_value_runtime_call(rt::willow_select_rotation, &[]);
                let rank = self.builder.ins().urem(rotation, total);
                let mut accumulated = self.builder.ins().iconst(types::I64, 0);
                let mut selected = self.builder.ins().iconst(types::I64, -1);
                for (index, flag) in flags.into_iter().enumerate() {
                    let is_ready = self.builder.ins().icmp_imm_s(IntCC::NotEqual, flag, 0);
                    let at_rank = self.builder.ins().icmp(IntCC::Equal, accumulated, rank);
                    let hit = self.builder.ins().band(is_ready, at_rank);
                    let index = self.builder.ins().iconst(types::I64, index as i64);
                    selected = self.builder.ins().select(hit, index, selected);
                    accumulated = self.builder.ins().iadd(accumulated, flag);
                }
                self.store_lir_local(function, *chosen, selected);
                self.builder.ins().jump(done, &[]);

                self.builder.switch_to_block(done);
                self.builder.seal_block(done);
            }
            LirInst::SelectUnregister { operations, winner } => {
                let winner = self.load_lir_local(function, *winner);
                let mut winner_raw = self
                    .builder
                    .ins()
                    .iconst(reference_type(self.module.target_config()), 0);
                let mut direction = self.builder.ins().iconst(types::I64, -1);
                for (index, operation) in operations.iter().enumerate() {
                    let (channel, selected_direction) = match operation {
                        LirSelectOp::Recv { channel, .. } => (*channel, 0),
                        LirSelectOp::Send { channel, .. } => (*channel, 1),
                        _ => continue,
                    };
                    let selected =
                        self.builder
                            .ins()
                            .icmp_imm_s(IntCC::Equal, winner, index as i64);
                    let channel = self.load_lir_local(function, channel);
                    let selected_direction =
                        self.builder.ins().iconst(types::I64, selected_direction);
                    winner_raw = self.builder.ins().select(selected, channel, winner_raw);
                    direction = self
                        .builder
                        .ins()
                        .select(selected, selected_direction, direction);
                }
                // Cleanup may allocate a deferred channel-wake job. Keep the
                // chosen wrapper relocatable across all arm cleanups, without
                // rescanning the case list after each safepoint.
                let roots_before = self.roots.gc_root_count;
                let winner_root = self.emit_push_relocatable_root(winner_raw);
                let receive_only = !operations
                    .iter()
                    .any(|operation| matches!(operation, LirSelectOp::Send { .. }));
                for operation in operations {
                    match operation {
                        LirSelectOp::Recv { channel, .. } | LirSelectOp::Send { channel, .. } => {
                            let winner_raw = self.emit_reload_root(winner_root, winner_raw);
                            let channel = self.load_lir_local(function, *channel);
                            // Exact-generation cleanup is idempotent and keeps
                            // the winning direction even for aliased arms.
                            // One call per arm avoids quadratic alias checks.
                            // With no send arms, the winner has no losing send
                            // ownership to release. Its receive commit clears
                            // the retained claim/wait under the receive lock.
                            let done = receive_only.then(|| {
                                let cleanup = self.builder.create_block();
                                let done = self.builder.create_block();
                                let selected =
                                    self.builder.ins().icmp(IntCC::Equal, channel, winner_raw);
                                self.builder.ins().brif(selected, done, &[], cleanup, &[]);
                                self.builder.switch_to_block(cleanup);
                                self.builder.seal_block(cleanup);
                                done
                            });
                            self.emit_void_runtime_call(
                                rt::willow_channel_select_cleanup,
                                &[channel, winner_raw, direction],
                            );
                            if let Some(done) = done {
                                self.builder.ins().jump(done, &[]);
                                self.builder.switch_to_block(done);
                                self.builder.seal_block(done);
                            }
                        }
                        LirSelectOp::Join { task, .. } => {
                            let task = self.load_lir_local(function, *task);
                            let id = self.builder.ins().load(
                                types::I64,
                                MemFlagsData::new(),
                                task,
                                async_frame_slot_offset(
                                    FRAME_SLOT_TASK_ID,
                                    reference_type(self.module.target_config()).bytes(),
                                ),
                            );
                            self.emit_void_runtime_call(
                                rt::willow_sched_unregister_task_waiter,
                                &[id],
                            );
                        }
                        LirSelectOp::Timeout { .. } | LirSelectOp::Default => {}
                    }
                }
                self.emit_pop_roots_n(self.roots.gc_root_count - roots_before);
                self.roots.gc_root_count = roots_before;
            }
            LirInst::SelectCommit { operation, success } => {
                let mut committed = true;
                match operation {
                    LirSelectOp::Recv {
                        channel,
                        binding,
                        elem_ty,
                        closed_aware,
                    } => {
                        let channel = self.load_lir_local(function, *channel);
                        let value = self.emit_channel_recv(channel, elem_ty, *closed_aware);
                        if let Some(binding) = binding {
                            self.store_lir_local(function, *binding, value);
                        }
                    }
                    LirSelectOp::Send {
                        channel,
                        value,
                        elem_ty,
                    } => {
                        let channel = self.load_lir_local(function, *channel);
                        let raw = self.load_lir_local(function, *value);
                        let from_ty = &function.locals[value.0 as usize].ty;
                        let value = self.coerce_to_target(raw, from_ty, elem_ty);
                        let runtime = channel_runtime_symbol(
                            elem_ty,
                            [
                                rt::willow_channel_try_send_i64,
                                rt::willow_channel_try_send_bool,
                                rt::willow_channel_try_send_f64,
                                rt::willow_channel_try_send_ptr,
                            ],
                        );
                        let before = self.roots.gc_root_count;
                        let (channel, value) = if self.is_inline_pair(elem_ty) {
                            let channel_root = self.emit_push_relocatable_root(channel);
                            let word = self.emit_to_storage_word(value, elem_ty);
                            self.emit_push_call_root(word);
                            (self.emit_reload_root(channel_root, channel), word)
                        } else {
                            (channel, self.widen_integer(value, elem_ty))
                        };
                        // Drop temporary boxing roots before either the normal
                        // continuation or the closed-send panic cleanup edge.
                        let sent = self
                            .emit_runtime_call_with_cleanup(runtime, &[channel, value], |this| {
                                this.emit_pop_roots_n(this.roots.gc_root_count - before);
                                this.roots.gc_root_count = before;
                            })
                            .expect("channel try-send returns a status");
                        let sent = self.builder.ins().icmp_imm_s(IntCC::NotEqual, sent, 0);
                        let one = self.builder.ins().iconst(types::I8, 1);
                        let zero = self.builder.ins().iconst(types::I8, 0);
                        let sent = self.builder.ins().select(sent, one, zero);
                        self.store_lir_local(function, *success, sent);
                        committed = false;
                    }
                    LirSelectOp::Join {
                        task,
                        binding,
                        result_ty,
                        cancel_aware,
                    } => {
                        let task = self.load_lir_local(function, *task);
                        let id = self.builder.ins().load(
                            types::I64,
                            MemFlagsData::new(),
                            task,
                            async_frame_slot_offset(
                                FRAME_SLOT_TASK_ID,
                                reference_type(self.module.target_config()).bytes(),
                            ),
                        );
                        let value =
                            self.emit_task_terminal_value(task, id, result_ty, *cancel_aware);
                        if let (Some(binding), Some(value)) = (binding, value) {
                            self.store_lir_local(function, *binding, value);
                        }
                    }
                    LirSelectOp::Timeout { .. } | LirSelectOp::Default => {}
                }
                if committed {
                    let one = self.builder.ins().iconst(types::I8, 1);
                    self.store_lir_local(function, *success, one);
                }
            }
            ir_variants!(
                LirInst;
                Compute, Let, Assign, EnterDeferScope, LeaveDeferScope, FlushDefers, RestoreCallPreparation,
                ClearScopeRoots, Defer, ReleaseLock, MatchTest, MatchBind, Unsupported
            ) => unreachable!(),
        }
    }

    /// A `recv` suspension: one fused runtime take, parking only when the
    /// channel is empty and open.
    fn emit_lir_channel_recv(
        &mut self,
        function: &LirFunction,
        operation: &SuspendOp,
        suspends: &mut CoopSuspendPoints,
        frame: cranelift_codegen::ir::Value,
    ) {
        let &SuspendOp::ChannelRecv {
            channel,
            result,
            ref elem_ty,
            closed_aware,
        } = operation
        else {
            unreachable!("only a channel receive suspension is emitted here");
        };
        let check = self.builder.create_block();
        self.builder.ins().jump(check, &[]);
        let state = (suspends.len() + 1) as i64;
        self.record_coop_suspend(suspends, check);
        self.builder.switch_to_block(check);
        let channel_value = self.load_lir_local(function, channel);
        // One runtime call takes the value under one channel lock;
        // only a closed, drained channel falls back to the ordinary
        // receive, which raises or answers `None`.
        let (status, slot) = self.emit_channel_recv_take(channel_value);
        let taken = self.builder.create_block();
        let not_taken = self.builder.create_block();
        let closed = self.builder.create_block();
        let park = self.builder.create_block();
        let done = self.builder.create_block();
        let is_taken = self.builder.ins().icmp_imm_s(IntCC::Equal, status, 1);
        self.builder
            .ins()
            .brif(is_taken, taken, &[], not_taken, &[]);
        self.builder.switch_to_block(not_taken);
        self.builder.ins().brif(status, closed, &[], park, &[]);
        self.builder.switch_to_block(park);
        let state = self.builder.ins().iconst(types::I64, state);
        self.builder
            .ins()
            .store(MemFlagsData::new(), state, frame, 0);
        self.emit_coop_unwind_poll_roots();
        let pending = self.builder.ins().iconst(types::I32, 0);
        self.builder.ins().return_(&[pending]);
        self.builder.switch_to_block(taken);
        let value = self.emit_channel_taken_value(slot, elem_ty, closed_aware);
        if let Some(result) = result {
            self.store_lir_local(function, result, value);
        }
        self.builder.ins().jump(done, &[]);
        self.builder.switch_to_block(closed);
        let channel_value = self.load_lir_local(function, channel);
        let value = self.emit_channel_recv(channel_value, elem_ty, closed_aware);
        if let Some(result) = result {
            self.store_lir_local(function, result, value);
        }
        self.builder.ins().jump(done, &[]);
        self.builder.switch_to_block(done);
    }

    fn emit_lir_suspend(
        &mut self,
        function: &LirFunction,
        operation: &SuspendOp,
        resume: BlockId,
        blocks: &[cranelift_codegen::ir::Block],
        suspends: &mut CoopSuspendPoints,
        frame: cranelift_codegen::ir::Value,
    ) {
        match operation {
            SuspendOp::Sleep { millis } => {
                let deadline_slot = self.suspension.lir_frame_offsets[millis];
                let millis = self.load_lir_local(function, *millis);
                self.emit_coop_sleep_value(millis, deadline_slot, suspends, frame);
            }
            SuspendOp::Yield => self.emit_coop_yield(suspends, frame),
            SuspendOp::Preempt => self.emit_coop_statement_safepoint(suspends, frame),
            SuspendOp::LockAcquire { slots, span } => {
                // All four slots are frame-backed by construction: the
                // operation reports them to LIR liveness, which is what makes a
                // local framed. Nothing here may live on the
                // native stack, because a contended acquisition returns out of
                // the poll function entirely.
                let offsets = self.lir_lock_offsets(slots);
                self.emit_lir_lock_acquire(
                    slots,
                    offsets,
                    &self.lir_lock_group_members(slots),
                    *span,
                    suspends,
                    frame,
                );
            }
            SuspendOp::LockHandoff { slots, next, span } => {
                self.emit_lir_lock_handoff(function, slots, *next, *span, frame);
                self.emit_coop_statement_safepoint(suspends, frame);
            }
            SuspendOp::AwaitTask {
                task,
                result,
                result_ty,
                cancel_aware,
            } => {
                self.emit_coop_frame_await(
                    self.suspension.lir_frame_offsets[task],
                    suspends,
                    frame,
                );
                let task_frame = self.load_lir_local(function, *task);
                if let Some(result) = result {
                    let value = self
                        .emit_coop_awaited_result(task_frame, Some(result_ty), *cancel_aware)
                        .expect("value-producing await has a result");
                    if let Some(span) = function.locals[result.0 as usize].source_span {
                        self.emit_handle_debug_site(value, result_ty, span);
                    }
                    self.store_lir_local(function, *result, value);
                } else {
                    self.emit_coop_awaited_result(task_frame, None, *cancel_aware);
                }
            }
            SuspendOp::ChannelRecv { .. } => {
                self.emit_lir_channel_recv(function, operation, suspends, frame);
            }
            SuspendOp::ChannelSend {
                channel,
                value,
                elem_ty,
            } => {
                let scratch = self.suspension.lir_send_offsets.get(value).copied();
                if let Some(offset) = scratch {
                    let raw = self.load_lir_local(function, *value);
                    let from_ty = &function.locals[value.0 as usize].ty;
                    let sent_value = self.coerce_to_target(raw, from_ty, elem_ty);
                    let word = self.emit_to_storage_word(sent_value, elem_ty);
                    self.emit_gc_heap_store_classified(
                        frame,
                        offset,
                        word,
                        true,
                        GcStoreDestination::AsyncFrameSlot,
                    );
                }
                let check = self.builder.create_block();
                self.builder.ins().jump(check, &[]);
                let state = (suspends.len() + 1) as i64;
                self.record_coop_suspend(suspends, check);
                self.builder.switch_to_block(check);
                let channel_value = self.load_lir_local(function, *channel);
                let sent_value = if let Some(offset) = scratch {
                    self.builder.ins().load(
                        reference_type(self.module.target_config()),
                        MemFlagsData::new(),
                        frame,
                        offset,
                    )
                } else {
                    let raw = self.load_lir_local(function, *value);
                    let from_ty = &function.locals[value.0 as usize].ty;
                    self.coerce_to_target(raw, from_ty, elem_ty)
                };
                let runtime = channel_runtime_symbol(
                    elem_ty,
                    [
                        rt::willow_channel_try_send_i64,
                        rt::willow_channel_try_send_bool,
                        rt::willow_channel_try_send_f64,
                        rt::willow_channel_try_send_ptr,
                    ],
                );
                let sent_value = self.widen_integer(sent_value, elem_ty);
                let sent = self.emit_value_runtime_call(runtime, &[channel_value, sent_value]);
                let done = self.builder.create_block();
                let park = self.builder.create_block();
                let sent = self.builder.ins().icmp_imm_s(IntCC::NotEqual, sent, 0);
                self.builder.ins().brif(sent, done, &[], park, &[]);
                self.builder.switch_to_block(park);
                let state = self.builder.ins().iconst(types::I64, state);
                self.builder
                    .ins()
                    .store(MemFlagsData::new(), state, frame, 0);
                self.emit_coop_unwind_poll_roots();
                let pending = self.builder.ins().iconst(types::I32, 0);
                self.builder.ins().return_(&[pending]);
                self.builder.switch_to_block(done);
                if let Some(offset) = scratch {
                    let null = self
                        .builder
                        .ins()
                        .iconst(reference_type(self.module.target_config()), 0);
                    self.emit_gc_heap_store_classified(
                        frame,
                        offset,
                        null,
                        true,
                        GcStoreDestination::AsyncFrameSlot,
                    );
                }
            }
            SuspendOp::SelectWait { operations } => {
                let mut minimum = None;
                for operation in operations {
                    if let LirSelectWaitOp::Timeout { deadline } = operation {
                        let deadline = self.load_lir_local(function, *deadline);
                        minimum = Some(match minimum {
                            None => deadline,
                            Some(current) => {
                                let before = self.builder.ins().icmp(
                                    IntCC::SignedLessThan,
                                    deadline,
                                    current,
                                );
                                self.builder.ins().select(before, deadline, current)
                            }
                        });
                    }
                }
                if let Some(deadline) = minimum {
                    let now = self.emit_value_runtime_call(rt::willow_monotonic_millis, &[]);
                    let remaining = self.builder.ins().isub(deadline, now);
                    let zero = self.builder.ins().iconst(types::I64, 0);
                    let negative = self
                        .builder
                        .ins()
                        .icmp(IntCC::SignedLessThan, remaining, zero);
                    let remaining = self.builder.ins().select(negative, zero, remaining);
                    self.emit_void_runtime_call(rt::willow_sched_sleep, &[remaining]);
                }
                let state = (suspends.len() + 1) as i64;
                let state = self.builder.ins().iconst(types::I64, state);
                self.builder
                    .ins()
                    .store(MemFlagsData::new(), state, frame, 0);
                self.emit_coop_unwind_poll_roots();
                let pending = self.builder.ins().iconst(types::I32, 0);
                self.builder.ins().return_(&[pending]);
                let wake = self.builder.create_block();
                self.record_coop_suspend(suspends, wake);
                self.builder.switch_to_block(wake);
            }
        }
        self.builder.ins().jump(blocks[resume.0], &[]);
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one LIR block walk coordinates fault sites, GC cache invalidation, suspension and defer state before its terminator; keeping the dispatch together exposes per-instruction ordering"
    )]
    pub(super) fn emit_lir_block(
        &mut self,
        layout: LirBlockLayout<'_>,
        block: &LirBlock,
        mut coop: Option<(&mut CoopSuspendPoints, cranelift_codegen::ir::Value)>,
        sync_poll: Option<(crate::backend::cranelift::emit_stmt::SyncPoll, bool)>,
        then_fallthrough: bool,
        defers: &mut LirBlockDeferCtx<'_>,
    ) {
        let LirBlockLayout {
            function,
            blocks,
            return_type,
            preparation_depths,
        } = layout;
        if let Some((poll, in_loop)) = sync_poll
            && (self.cleanup.lir_cleanup_exit.is_none() || block.id.0 != 0)
        {
            // Native stacks preserve SSA values and roots across this hook;
            // function entry and cycle headers cover recursion and loops.
            self.emit_sync_safepoint(poll, in_loop);
        }
        for (inst_index, inst) in block.instrs.iter().enumerate() {
            if self.locals.terminated {
                continue;
            }
            // Debug builds report runtime-raised faults (array bounds, a
            // blocked channel op) at the location of the code that ran, so the
            // LIR path must publish its own site too. Without this the fault
            // would inherit the caller's statement.
            if let Some(span) = lir_inst_span(inst) {
                self.diagnostics.fault_site_span = Some(span);
            }
            // A GC point ends the run of reusable loads.
            if coop.is_some()
                || !crate::backend::cranelift::transient_roots::gc_free(function, inst)
            {
                self.reuse.lir_reuse = None;
            } else if self.reuse.lir_reuse.is_none() {
                self.reuse.lir_reuse = Some(Default::default());
            }
            match inst {
                LirInst::Compute { local, value, span } => {
                    if task_stack_boundary(value)
                        && let Some((suspends, frame)) = coop.as_mut()
                    {
                        let symbol = task_boundary_symbol(function, block.id.0, inst_index);
                        self.emit_task_boundary(Some(value), &symbol, suspends, *frame);
                        continue;
                    }
                    let result = self.emit_lir_rvalue(function, value, *span);
                    if !self.locals.terminated
                        && (function.locals[local.0 as usize].is_gc_owner()
                            || !matches!(
                                function.locals[local.0 as usize].ty,
                                Type::Void | Type::Never
                            ))
                    {
                        self.store_lir_local(function, *local, result);
                    }
                }
                LirInst::RestoreCallPreparation { entry } => {
                    let (methods, references) = preparation_depths[entry.0];
                    while self.diagnostics.lir_reference_scopes.len() > references {
                        self.emit_flat_reference_call_end();
                    }
                    while self.diagnostics.lir_call_frames.len() > methods {
                        self.emit_callstack_pop();
                        self.diagnostics.lir_call_frames.pop_back();
                    }
                }
                LirInst::EnterDeferScope { sites, .. } => {
                    let crate::ir::lowered::cleanup::CleanupTransition::Enter { scope: id, state } =
                        defers.cleanup.transitions[&(defers.block_index, inst_index)]
                    else {
                        unreachable!()
                    };
                    let scope = &mut defers.bindings.scopes[id.0];
                    scope.call_frames_at_entry = self.diagnostics.lir_call_frames.clone();
                    scope.reference_scopes_at_entry = self.diagnostics.lir_reference_scopes.clone();
                    if let crate::backend::cranelift::RootDepth::Variable(var) =
                        scope.root_depth_at_entry
                    {
                        let depth = self.emit_value_runtime_call(rt::willow_root_depth, &[]);
                        self.builder.def_var(var, depth);
                        for (site, _) in sites {
                            let zero = self.builder.ins().iconst(types::I64, 0);
                            self.stack_store(zero, self.cleanup.sync_defer_flags[site]);
                        }
                    }
                    self.install_lir_cleanup(defers.bindings.state(state));
                }
                LirInst::Defer { .. } => {
                    let crate::ir::lowered::cleanup::CleanupTransition::Register {
                        site: id,
                        state,
                    } = defers.cleanup.transitions[&(defers.block_index, inst_index)]
                    else {
                        unreachable!()
                    };
                    if let Some(frame) = self.suspension.coop_frame.filter(|_| coop.is_some()) {
                        let one = self.builder.ins().iconst(types::I64, 1);
                        self.builder.ins().store(
                            MemFlagsData::new(),
                            one,
                            frame,
                            self.suspension.lir_defer_offsets[&id],
                        );
                    } else {
                        let one = self.builder.ins().iconst(types::I64, 1);
                        self.stack_store(one, self.cleanup.sync_defer_flags[&id]);
                    }
                    self.install_lir_cleanup(defers.bindings.state(state));
                }
                LirInst::LeaveDeferScope { .. } | LirInst::FlushDefers { .. } => {
                    let crate::ir::lowered::cleanup::CleanupTransition::Exit { edge, state: after } =
                        defers.cleanup.transitions[&(defers.block_index, inst_index)]
                    else {
                        unreachable!()
                    };
                    let depth = defers.bindings.state(after).entries.len();
                    let abandoned_preparation = matches!(inst, LirInst::FlushDefers { .. })
                        && matches!(block.terminator, Terminator::Return(_));
                    if abandoned_preparation {
                        // An abrupt function exit abandons argument preparation.
                        // Release its diagnostic/reference protocol before shared
                        // cleanup; otherwise each `?` site duplicates every action.
                        // Other exits can stay inside a prepared expression.
                        for _ in 0..self.diagnostics.lir_reference_scopes.len() {
                            self.emit_debug_reference_call_clear();
                        }
                        for _ in 0..self.diagnostics.lir_call_frames.len() {
                            self.emit_callstack_pop();
                        }
                        self.diagnostics.lir_call_frames.clear();
                        self.diagnostics.lir_reference_scopes.clear();
                    }
                    if edge.scopes > 0 {
                        let protocol = if abandoned_preparation {
                            (None, None)
                        } else {
                            defers.bindings.protocols[&(defers.block_index, inst_index)]
                        };
                        self.emit_lir_shared_normal_cleanup(depth, defers.bindings, protocol);
                    }
                    if coop.is_none() && !self.locals.terminated {
                        let roots = self.roots.gc_root_count - defers.bindings.roots;
                        if roots > 0 {
                            self.emit_pop_roots_n(roots);
                        }
                        self.roots.gc_root_count = defers.bindings.roots;
                    }
                    self.install_lir_cleanup(defers.bindings.state(after));
                }
                LirInst::ClearScopeRoots { locals } => {
                    self.emit_lir_clear_scope_roots(function, locals);
                }
                // Commit the protected value and hand the lock back
                //. Idempotent at run time: the release is
                // guarded by the handle slot, which it clears, so a path that
                // reaches it after a recovered panic already released finds
                // nothing to do.
                LirInst::ReleaseLock(slots) => {
                    let offsets = self.lir_lock_offsets(slots);
                    self.emit_lock_frame_cleanup(
                        slots.mode,
                        offsets,
                        &slots.value_ty,
                        &self.lir_lock_group_members(slots),
                        false,
                    );
                }
                LirInst::Let {
                    local, ty, value, ..
                } => {
                    let val = self.emit_lir_operand_store(function, value, ty);
                    self.store_lir_local(function, *local, val);
                }
                LirInst::Assign { local, value, .. } => {
                    let target = function.locals[local.0 as usize].ty.clone();
                    let val = self.emit_lir_operand_store(function, value, &target);
                    self.store_lir_local(function, *local, val);
                }
                LirInst::Unsupported { reason, .. } => {
                    unreachable!("rejected LIR reached backend: {reason}")
                }
                // One arm of a `match` lowering split into blocks
                //. Both read the scrutinee back out of
                // its own local, so every arm tests and destructures the same
                // value however many blocks the dispatch chain spans.
                LirInst::MatchTest {
                    scrutinee,
                    pattern,
                    result,
                    ..
                } => {
                    let scrutinee_ty = function.locals[scrutinee.0 as usize].ty.clone();
                    let value = self.load_lir_local(function, *scrutinee);
                    let matched = self.emit_flat_pattern_check(value, &scrutinee_ty, pattern);
                    self.store_lir_local(function, *result, matched);
                }
                LirInst::MatchBind {
                    scrutinee,
                    pattern,
                    bindings,
                    ..
                } => {
                    let scrutinee_ty = function.locals[scrutinee.0 as usize].ty.clone();
                    let value = self.load_lir_local(function, *scrutinee);
                    let bound = self.flat_pattern_binding_values(value, &scrutinee_ty, pattern);
                    debug_assert_eq!(
                        bound.len(),
                        bindings.len(),
                        "a `match` arm's bindings must line up with the locals lowering declared"
                    );
                    for ((_, _, val), local) in bound.into_iter().zip(bindings) {
                        self.store_lir_local(function, *local, val);
                    }
                }
                LirInst::SelectInit { .. }
                | LirInst::SelectProbe { .. }
                | LirInst::SelectPick { .. }
                | LirInst::SelectUnregister { .. }
                | LirInst::SelectCommit { .. } => self.emit_lir_select_instruction(function, inst),
            }
        }
        if self.locals.terminated {
            return;
        }
        match &block.terminator {
            Terminator::Jump(b) => {
                self.builder.ins().jump(blocks[b.0], &[]);
            }
            Terminator::Branch {
                cond,
                then_block,
                else_block,
            } => {
                let c = self.emit_lir_operand(function, cond);
                if let Some(reuse) = &mut self.reuse.lir_reuse {
                    reuse.branch_bound = reuse.comparison.take().and_then(|(test, index, len)| {
                        (test == c).then_some((then_block.0, index, len))
                    });
                }
                if then_fallthrough {
                    // Cranelift places a branch's second target next; keep
                    // the loop body after its header. The
                    // optimizer folds the inversion into a compare operand.
                    let exit = self.builder.ins().icmp_imm_s(IntCC::Equal, c, 0);
                    self.builder.ins().brif(
                        exit,
                        blocks[else_block.0],
                        &[],
                        blocks[then_block.0],
                        &[],
                    );
                } else {
                    self.builder.ins().brif(
                        c,
                        blocks[then_block.0],
                        &[],
                        blocks[else_block.0],
                        &[],
                    );
                }
            }
            Terminator::CleanupReturn => {
                let (exit, roots, _) = self
                    .cleanup
                    .lir_cleanup_exit
                    .expect("cleanup terminator outside cleanup region");
                self.emit_pop_roots_n(self.roots.gc_root_count - roots);
                self.builder.ins().jump(exit, &[]);
                self.cleanup.lir_cleanup_exit.as_mut().unwrap().2 = true;
                self.locals.terminated = true;
            }
            Terminator::Return(v) => {
                self.emit_lir_operand_return(function, v.as_ref(), return_type);
            }
            Terminator::Suspend { operation, resume } => {
                let Some((suspends, frame)) = coop.as_mut() else {
                    unreachable!("explicit LIR suspension reached the synchronous emitter")
                };
                if !matches!(operation, SuspendOp::Preempt) {
                    self.emit_fault_site();
                }
                self.emit_lir_suspend(function, operation, *resume, blocks, suspends, *frame);
            }
        }
    }

    /// The function-exit sequence: pop every root this function pushed, then
    /// return. Shared by a block's `Return` terminator and by a `return` inside
    /// a `match` arm, which is why it sets `self.terminated`
    /// — the arm's Cranelift block ends here.
    ///
    /// `gc_root_count` is deliberately NOT decremented: the pops emitted here
    /// belong to this path only, and the counter still describes what the paths
    /// that did not return are holding.
    /// The four async-frame offsets of a critical section's slots, in the order
    /// the runtime hooks take them: handle, token, phase, protected value.
    ///
    /// Every one is frame-backed by construction — the acquisition reports them
    /// to LIR liveness, which is what gives a local a slot.
    pub(in crate::backend::cranelift) fn lir_lock_group_members(
        &self,
        slots: &LirLockSlots,
    ) -> Vec<crate::backend::cranelift::LockGroupMember> {
        slots
            .members
            .iter()
            .map(|member| crate::backend::cranelift::LockGroupMember {
                mode: member.mode,
                handle_offset: self.suspension.lir_frame_offsets[&member.handle],
                value_offset: self.suspension.lir_frame_offsets[&member.binding],
                value_ty: member.value_ty.clone(),
            })
            .collect()
    }

    pub(super) fn lir_lock_offsets(&self, slots: &LirLockSlots) -> [i32; 4] {
        slots
            .locals()
            .map(|local| self.suspension.lir_frame_offsets[&local])
    }

    fn emit_lir_lock_handoff(
        &mut self,
        function: &LirFunction,
        slots: &LirLockSlots,
        next: LirLocalId,
        span: crate::diagnostics::Span,
        frame: cranelift_codegen::ir::Value,
    ) {
        let [handle_offset, token_offset, phase_offset, _] = self.lir_lock_offsets(slots);
        let value = self.load_lir_local(function, slots.binding);
        let word = self.emit_to_storage_word(value, &slots.value_ty);
        // Boxing a pair can collect: reload the cursor from its rooted frame slot.
        let next = self.load_lir_local(function, next);
        let handle_at = self
            .builder
            .ins()
            .iconst(types::I64, i64::from(handle_offset));
        let token_at = self
            .builder
            .ins()
            .iconst(types::I64, i64::from(token_offset));
        let phase_at = self
            .builder
            .ins()
            .iconst(types::I64, i64::from(phase_offset));
        let acquired = self.emit_value_runtime_call(
            rt::willow_async_mutex_handoff,
            &[frame, handle_at, token_at, phase_at, next, word],
        );
        let owned = self.builder.create_block();
        let failed = self.builder.create_block();
        self.builder.ins().brif(acquired, owned, &[], failed, &[]);
        self.builder.switch_to_block(failed);
        self.builder.seal_block(failed);
        let file = self.emit_string_literal(self.diagnostics.source_file);
        let line = self.builder.ins().iconst(types::I32, span.line as i64);
        let col = self.builder.ins().iconst(types::I32, span.col as i64);
        self.emit_void_runtime_call(rt::willow_async_mutex_handoff_panic, &[file, line, col]);
        self.builder.ins().trap(TrapCode::unwrap_user(1));
        self.builder.switch_to_block(owned);
        self.builder.seal_block(owned);
        let handle = self.load_lir_local(function, slots.handle);
        let token = self.load_lir_local(function, slots.token);
        let word = self.emit_value_runtime_call(rt::willow_async_mutex_load, &[handle, token]);
        let value = self.emit_from_storage_word(word, &slots.value_ty);
        self.store_lir_local(function, slots.binding, value);
        let loaded = self.builder.ins().iconst(types::I64, 1);
        self.builder
            .ins()
            .store(MemFlagsData::new(), loaded, frame, phase_offset);
        // The binding store uses the normal frame GC barrier; the runtime
        // already published handle/token with phase zero inside NoPreemptGuard.
    }

    pub(super) fn load_cleanup_control(
        &mut self,
        slot: Option<cranelift_codegen::ir::StackSlot>,
        frame_offset: Option<i32>,
        word: i32,
    ) -> cranelift_codegen::ir::Value {
        if let Some(offset) = frame_offset {
            self.builder.ins().load(
                types::I32,
                MemFlagsData::new(),
                self.suspension.async_frame.unwrap(),
                offset + word * 4,
            )
        } else {
            self.builder.ins().stack_load(
                reference_type(self.module.target_config()),
                types::I32,
                slot.unwrap(),
                word * 4,
            )
        }
    }

    fn emit_lir_shared_normal_cleanup(
        &mut self,
        depth: usize,
        ledger: &mut LirCleanupBindings,
        protocol: (Option<usize>, Option<usize>),
    ) {
        if depth == self.cleanup.defer_stack.len() {
            return;
        }
        let key = NormalCleanupKey {
            protocol,
            roots: self.roots.gc_root_count,
            panic_depth: self.cleanup.panic_defer_codegen_depth,
            recover_depth: self.cleanup.recover_eligible_depth,
            call_depth: self.diagnostics.callstack_frame_depth,
            native_active: self.cleanup.sync_native_active.is_some(),
        };
        if !ledger.normal.contains_key(&key) {
            let selector_offset = self.suspension.cleanup_selector_offset;
            let slots = selector_offset.is_none().then(|| {
                self.builder.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    8,
                    2,
                ))
            });
            let dispatch = self.builder.create_block();
            ledger.normal.insert(
                key.clone(),
                NormalCleanup {
                    entries: HashMap::new(),
                    dispatch,
                    slots,
                    selector_offset,
                    continuations: Vec::new(),
                },
            );
        }
        let control = &ledger.normal[&key];
        let (slots, offset, dispatch) = (control.slots, control.selector_offset, control.dispatch);
        let index = control.continuations.len();
        for (word, value) in [(0, index), (1, depth)] {
            let value = self.builder.ins().iconst(types::I32, value as i64);
            if let Some(offset) = offset {
                self.builder.ins().store(
                    MemFlagsData::new(),
                    value,
                    self.suspension.async_frame.unwrap(),
                    offset + word * 4,
                );
            } else {
                self.builder.ins().stack_store(
                    reference_type(self.module.target_config()),
                    value,
                    slots.unwrap(),
                    word * 4,
                );
            }
        }
        let original = LirCleanupState {
            entries: self.cleanup.defer_stack.clone(),
            panic: self.cleanup.panic_scopes.clone(),
            locks: self.cleanup.lock_scopes.clone(),
        };
        let active_before = self.cleanup.sync_native_active;
        let continuation = self.builder.create_block();
        ledger
            .normal
            .get_mut(&key)
            .unwrap()
            .continuations
            .push(continuation);
        loop {
            let identity = self.cleanup.defer_stack.identity;
            let existing = ledger.normal[&key].entries.get(&identity).copied();
            let entry = existing.unwrap_or_else(|| {
                let entry = self.builder.create_block();
                if let Some(active) = self.cleanup.sync_native_active {
                    self.builder
                        .append_block_param(entry, self.builder.func.dfg.value_type(active));
                }
                ledger
                    .normal
                    .get_mut(&key)
                    .unwrap()
                    .entries
                    .insert(identity, entry);
                entry
            });
            let active = self.cleanup.sync_native_active.map(Into::into);
            self.builder.ins().jump(entry, active.as_slice());
            if existing.is_some() {
                break;
            }
            self.builder.switch_to_block(entry);
            if active_before.is_some() {
                self.cleanup.sync_native_active = Some(self.builder.block_params(entry)[0]);
            }
            let parent_depth = self.cleanup.defer_stack.len() - 1;
            // One body per lexical scope. Descendant exits join this same
            // suffix; neither bindings nor actions are copied per exit/depth.
            let selector = self.suspension.cleanup_selector_offset.take();
            self.emit_flush_defers_from(parent_depth);
            self.suspension.cleanup_selector_offset = selector;
            if self.locals.terminated {
                break;
            }
            if parent_depth == ledger.initial.entries.len() {
                self.builder.ins().jump(dispatch, &[]);
                break;
            }
            let stop = self.load_cleanup_control(slots, offset, 1);
            let done = self
                .builder
                .ins()
                .icmp_imm_s(IntCC::Equal, stop, parent_depth as i64);
            let next = self.builder.create_block();
            self.builder.ins().brif(done, dispatch, &[], next, &[]);
            self.builder.switch_to_block(next);
            self.builder.seal_block(next);
            self.pop_cleanup_scope();
        }
        self.install_lir_cleanup(&original);
        self.cleanup.sync_native_active = active_before;
        self.builder.switch_to_block(continuation);
        self.locals.terminated = false;
    }

    fn emit_lir_operand_return(
        &mut self,
        function: &LirFunction,
        value: Option<&crate::ir::lowered::LirOperand>,
        return_type: &Type,
    ) {
        if let Some(frame) = self.suspension.coop_frame {
            if let (Some(value), Some(offset)) = (value, self.suspension.coop_result_offset) {
                let result = self.emit_lir_operand_store(function, value, return_type);
                self.emit_gc_heap_store(
                    frame,
                    offset,
                    result,
                    return_type,
                    GcStoreDestination::AsyncFrameSlot,
                );
            } else if let Some(value) = value {
                // Async main is `void`, but preserve source evaluation if a
                // future front-end shape carries a value with no result slot.
                self.emit_lir_operand(function, value);
            }
            self.emit_flush_defers_from(0);
            if !self.locals.terminated {
                // A return nested inside a LIR expression (notably a
                // statement-position `match` arm) may still hold temporary
                // roots owned by that expression. They do not survive this
                // poll return, so pop the complete runtime depth here; keep
                // the compile-time count intact for sibling CFG paths.
                self.emit_callstack_unwind_edge();
                self.emit_pop_roots_n(self.roots.gc_root_count);
                let ready = self.builder.ins().iconst(types::I32, 1);
                self.builder.ins().return_(&[ready]);
                self.locals.terminated = true;
            }
            return;
        }
        // `fn main -> Result<void, E>`: `willow_user_main` is void, so a
        // `return` is an EXIT rather than a value handed back -- `Err` reports
        // its payload and exits non-zero, `Ok` exits 0.
        if self.locals.main_result_err_ty.is_some() {
            match value {
                Some(value) => {
                    let result = self.emit_lir_operand(function, value);
                    // The `Result` object outlives any allocating defer between
                    // here and the exit that reads its tag.
                    let slot = is_gc_managed(return_type, self.enum_infos)
                        .then(|| self.emit_push_relocatable_root(result));
                    self.emit_flush_defers_from(0);
                    if self.locals.terminated {
                        return;
                    }
                    let result = match slot {
                        Some(slot) => {
                            let result = self.emit_reload_root(slot, result);
                            self.emit_pop_roots_n(1);
                            self.roots.gc_root_count -= 1;
                            result
                        }
                        None => result,
                    };
                    // Pops the function's remaining roots on both of its arms.
                    self.emit_callstack_unwind_edge();
                    self.emit_main_result_exit(result);
                }
                // `return Result::Ok;` and a bare `return` are the same
                // success, and neither needs the object built to say so.
                None => {
                    self.emit_flush_defers_from(0);
                    if self.locals.terminated {
                        return;
                    }
                    self.emit_callstack_unwind_edge();
                    self.emit_pop_roots_n(self.roots.gc_root_count);
                    self.builder.ins().return_(&[]);
                }
            }
            self.locals.terminated = true;
            return;
        }
        match value {
            Some(v) => {
                // Evaluate (and box, for an interface-typed return) first: the
                // value may read through a rooted local, and the box allocates.

                let val = self.emit_lir_operand_store(function, v, return_type);
                self.emit_callstack_unwind_edge();
                self.emit_pop_roots_n(self.roots.gc_root_count);
                self.builder.ins().return_(&[val]);
            }
            None => {
                self.emit_callstack_unwind_edge();
                self.emit_pop_roots_n(self.roots.gc_root_count);
                if *return_type == Type::Void {
                    self.builder.ins().return_(&[]);
                } else {
                    // Unreachable fall-through in a value function (the checker
                    // guarantees returns); satisfy the signature with a zero.
                    let zero = match self.clif_type(return_type) {
                        types::F64 => self.builder.ins().f64const(0.0),
                        ty => self.emit_zero(ty),
                    };
                    self.builder.ins().return_(&[zero]);
                }
            }
        }
        self.locals.terminated = true;
    }
}
