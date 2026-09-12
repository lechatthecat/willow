use cranelift_codegen::ir::{InstBuilder, MemFlagsData, condcodes::IntCC, types};

use super::*;

#[willow_continuations::methods(
    emit_deferred_action,
    emit_flush_defers_from,
    emit_sync_try_defer_flush
)]
impl<'a, 'b> FuncGen<'a, 'b> {
    /// Allocate one position in the cancellation cleanup stream shared by
    /// deferred actions and lexical lock releases.  A lock takes its position
    /// before its body is emitted, so reversing this sequence produces
    /// `body defers -> lock cleanup -> enclosing defers`.
    pub(super) fn next_cleanup_order(&mut self) -> usize {
        let order = self.collected_cleanup_order;
        self.collected_cleanup_order = self
            .collected_cleanup_order
            .checked_add(1)
            .expect("async cleanup order overflow");
        order
    }

    /// Run every registered defer from frame `depth` outward, innermost frame
    /// first, newest registration first (LIFO). Frames are left in place —
    /// scope bookkeeping pops them (willow-vynv.2).
    pub(super) fn emit_flush_defers_from(&mut self, depth: usize) {
        let frames: Vec<Vec<super::DeferEntry>> = self.defer_stack[depth..].to_vec();
        let unavailable_before = self.unavailable_defer_ids.clone();
        // Each registration is emitted under the bindings it captured, so the
        // flush rewrites `vars`. Whatever comes after it — the `return` whose
        // value was bound AFTER the registration, the rest of an enclosing
        // scope — still expects its own bindings (willow-0g8j.2.15).
        let vars_before = self.vars.clone();
        'frames: for (index, frame) in frames.iter().enumerate().rev() {
            // A `lock` critical section is released as its own defer frame
            // finishes unwinding, which is what orders the section's defers
            // (still holding the lock) before the enclosing scopes' defers
            // (after the release) on EVERY exit path — fallthrough, `return`,
            // `?`, `break` and `continue` all reach here (willow-38w.1.4).
            let lock_depth = depth + index;
            for entry in frame.iter().rev() {
                if self.unavailable_defer_ids.contains(&entry.id) {
                    continue;
                }
                let inactive = if let (Some(off), Some(frame_ptr)) =
                    (entry.flag_offset, self.coop_frame)
                {
                    let flag =
                        self.builder
                            .ins()
                            .load(types::I64, MemFlagsData::new(), frame_ptr, off);
                    let active = self.builder.create_block();
                    let inactive = self.builder.create_block();
                    self.builder.ins().brif(flag, active, &[], inactive, &[]);
                    self.builder.switch_to_block(active);
                    self.builder.seal_block(active);
                    Some(inactive)
                } else {
                    None
                };
                // Consume before entering user code. If this action panics, its
                // nested unwind sees the registration as unavailable and cannot
                // execute it a second time.
                self.unavailable_defer_ids.insert(entry.id);
                self.vars = entry.vars_at_registration.clone();
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
                // Async: leave REGISTERED before entering user cleanup code.
                // A future recoverable panic may unwind out of that code; if
                // the flag stayed set until afterwards, cancellation/recovery
                // could run this same registration twice (willow-s9ej.1).
                if let (Some(off), Some(frame_ptr)) = (entry.flag_offset, self.coop_frame) {
                    let zero = self.builder.ins().iconst(types::I64, 0);
                    self.builder
                        .ins()
                        .store(MemFlagsData::new(), zero, frame_ptr, off);
                }
                if let Some(slot) = entry.sync_flag_slot {
                    let zero = self.builder.ins().iconst(types::I64, 0);
                    self.stack_store(zero, slot);
                }
                // Normal-exit cleanup is NOT a recovery capability. The runtime
                // panic-defer depth is per execution context, so a defer that
                // runs while an OUTER frame is unwinding would otherwise be able
                // to consume that frame's panic — a helper called from panic
                // cleanup could steal its caller's panic (willow-s9ej.7). Only
                // the cleanup emitted for the unwinding scope itself may
                // recover; here `recover()` lowers to a constant `None`.
                let eligible_before = self.recover_eligible_depth;
                self.recover_eligible_depth = 0;
                self.emit_deferred_action(&entry.action);
                self.recover_eligible_depth = eligible_before;
                if let Some(inactive) = inactive {
                    if !self.terminated {
                        self.builder.ins().jump(inactive, &[]);
                    }
                    self.builder.switch_to_block(inactive);
                    self.builder.seal_block(inactive);
                    self.terminated = false;
                } else if self.terminated {
                    break 'frames;
                }
            }
            // A terminated path left through panic unwinding, whose cleanup
            // releases the lock itself; releasing here too would be dead code
            // after a block terminator.
            self.emit_lock_release_at_depth(lock_depth);
        }
        self.unavailable_defer_ids = unavailable_before;
        self.vars = vars_before;
    }

    /// Commit and release every `lock` whose critical section owns the defer
    /// frame at `depth` (willow-38w.1.4), innermost first.
    ///
    /// The handle slot is cleared after the runtime relationship is gone, so a
    /// path that reaches this hook twice — a recovered panic that then falls
    /// out of the section, nested flushes that overlap — still commits and
    /// releases exactly once at run time. Ownership is proven by the token, so
    /// a stale release can never steal a lock a later task now owns.
    pub(super) fn emit_lock_release_at_depth(&mut self, depth: usize) {
        if self.terminated || self.lock_scopes.is_empty() {
            return;
        }
        let scopes: Vec<super::CoopLockScope> = self
            .lock_scopes
            .iter()
            .filter(|scope| scope.defer_depth == depth)
            .cloned()
            .collect();
        for scope in scopes.iter().rev() {
            self.emit_lock_scope_release(scope);
        }
    }

    fn emit_lock_scope_release(&mut self, scope: &super::CoopLockScope) {
        self.emit_lock_frame_cleanup(
            scope.mode,
            [
                scope.handle_offset,
                scope.token_offset,
                scope.phase_offset,
                scope.value_offset,
            ],
            &scope.value_ty,
            false,
        );
    }

    /// Finish one frame-backed Mutex acquisition.
    ///
    /// `cancel_wait` first reconciles a possible `Waiting`/`HandoffOwned`
    /// reverse link.  A non-zero phase means the protected value was loaded, so
    /// cancellation has the same commit-before-release contract as every other
    /// scope exit.  The handle and value slots are cleared only after all native
    /// relationship work is complete; this is both the lifetime ordering needed
    /// by future lock reclamation and the point at which a GC value ceases to be
    /// retained by the task frame.
    pub(super) fn emit_lock_frame_cleanup(
        &mut self,
        mode: LockMode,
        frame_offsets: [i32; 4],
        value_ty: &Type,
        cancel_wait: bool,
    ) {
        let [handle_offset, token_offset, phase_offset, value_offset] = frame_offsets;
        let frame = self
            .async_frame
            .expect("lock cleanup requires its compiler-generated async frame");
        let handle = self.builder.ins().load(
            reference_type(self.module.target_config()),
            MemFlagsData::new(),
            frame,
            handle_offset,
        );
        let held_b = self.builder.create_block();
        let done_b = self.builder.create_block();
        self.builder.ins().brif(handle, held_b, &[], done_b, &[]);

        self.builder.switch_to_block(held_b);
        self.builder.seal_block(held_b);
        let token = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), frame, token_offset);
        if cancel_wait {
            // Task-directed and O(1): Waiting is removed; HandoffOwned is
            // released/re-handed.  A held owner has no reverse link, so this is
            // a no-op before the phase-sensitive commit below.
            let cancel = match mode {
                LockMode::Mutex => "willow_async_mutex_cancel",
                LockMode::Read | LockMode::Write => "willow_async_rwlock_cancel",
            };
            self.emit_value_runtime_call(cancel, &[]);
        }

        let phase = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), frame, phase_offset);
        let commit_b = self.builder.create_block();
        let release_b = self.builder.create_block();
        if mode == LockMode::Read {
            self.builder.ins().jump(release_b, &[]);
        } else {
            self.builder
                .ins()
                .brif(phase, commit_b, &[], release_b, &[]);
        }

        self.builder.switch_to_block(commit_b);
        self.builder.seal_block(commit_b);
        let value = self.builder.ins().load(
            clif_type(reference_type(self.module.target_config()), value_ty),
            MemFlagsData::new(),
            frame,
            value_offset,
        );
        let word = self.coerce_to_i64(value, value_ty);
        // Route the publish through the central barrier hook rather than
        // letting the runtime cell be a store the collector never sees. The
        // owner is the GC-managed lock handle, so this also records old->young
        // protected-value edges for the generational collector.
        if is_gc_managed(value_ty, self.enum_infos) {
            let barrier_id = self.func_id("willow_gc_write_barrier");
            let barrier = self
                .module
                .declare_func_in_func(barrier_id, self.builder.func);
            let destination_kind = match mode {
                LockMode::Mutex => GcStoreDestination::AsyncMutexCell,
                LockMode::Read | LockMode::Write => GcStoreDestination::AsyncRwLockCell,
            };
            let destination = self
                .builder
                .ins()
                .iconst(types::I64, destination_kind as i64);
            self.builder
                .ins()
                .call(barrier, &[handle, word, destination]);
        }
        let commit = match mode {
            LockMode::Mutex => "willow_async_mutex_commit",
            // The read-mode commit block has no predecessor, but Cranelift
            // still emits it structurally. Point it at the ownership-checking
            // RwLock ABI; it is unreachable at run time and cannot publish.
            LockMode::Read | LockMode::Write => "willow_async_rwlock_commit",
        };
        self.emit_value_runtime_call(commit, &[handle, token, word]);
        self.builder.ins().jump(release_b, &[]);

        self.builder.switch_to_block(release_b);
        self.builder.seal_block(release_b);
        let release = match mode {
            LockMode::Mutex => "willow_async_mutex_release",
            LockMode::Read | LockMode::Write => "willow_async_rwlock_release",
        };
        self.emit_value_runtime_call(release, &[handle, token]);

        // Native pointer use is over. Mark the acquisition inactive before
        // dropping the frame references so any later cleanup is an idempotent
        // no-op and dead GC values do not survive for the rest of the Task.
        let zero64 = self.builder.ins().iconst(types::I64, 0);
        self.builder
            .ins()
            .store(MemFlagsData::new(), zero64, frame, phase_offset);
        let handle_ty = Type::Generic(
            match mode {
                LockMode::Mutex => "Mutex",
                LockMode::Read | LockMode::Write => "RwLock",
            }
            .to_string()
            .into(),
            vec![value_ty.clone()],
        );
        let null_handle = self
            .builder
            .ins()
            .iconst(reference_type(self.module.target_config()), 0);
        self.emit_gc_heap_store(
            frame,
            handle_offset,
            null_handle,
            &handle_ty,
            GcStoreDestination::AsyncFrameSlot,
        );
        let zero_value = match clif_type(reference_type(self.module.target_config()), value_ty) {
            types::F64 => self.builder.ins().f64const(0.0),
            ty => self.builder.ins().iconst(ty, 0),
        };
        self.emit_gc_heap_store(
            frame,
            value_offset,
            zero_value,
            value_ty,
            GcStoreDestination::AsyncFrameSlot,
        );
        self.builder.ins().jump(done_b, &[]);

        self.builder.switch_to_block(done_b);
        self.builder.seal_block(done_b);
    }

    pub(super) fn emit_deferred_action(&mut self, action: &super::DeferredAction) {
        self.emit_lir_cleanup_region(&action.function);
    }

    /// Preserve the native call chain when a synchronous helper exhausts its
    /// task budget. Ordinary synchronous loops only load the live GC gate;
    /// they enter the runtime when a collector actually requests a stop.
    pub(super) fn emit_sync_safepoint(
        &mut self,
        native_active: cranelift_codegen::ir::Value,
        gc_stop_flag: cranelift_codegen::ir::Value,
    ) {
        // Atomic loads cannot be hoisted out of the source loop. The GC flag
        // may change concurrently even though native activity is invocation-
        // constant (nested scheduler drives restore the enclosing context).
        let stop = self
            .builder
            .ins()
            .atomic_load(types::I8, MemFlagsData::trusted(), gc_stop_flag);
        let stop = self.builder.ins().uextend(types::I32, stop);
        let required = self.builder.ins().bor(native_active, stop);
        let slow = self.builder.create_block();
        self.builder.set_cold_block(slow);
        let resume = self.builder.create_block();
        self.builder.ins().brif(required, slow, &[], resume, &[]);
        self.builder.switch_to_block(slow);
        self.builder.seal_block(slow);
        let cancelled = self.emit_value_runtime_call("willow_sync_safepoint", &[]);
        self.emit_sync_cancel_branch(cancelled);
        self.builder
            .set_cold_block(self.builder.current_block().expect("poll continuation"));
        self.builder.ins().jump(resume, &[]);
        self.builder.switch_to_block(resume);
        self.builder.seal_block(resume);
    }

    fn emit_sync_cancel_check(&mut self) {
        if self.emitting_sync_cancel_cleanup {
            return;
        }
        // Synchronous main/thread execution has no cancellable native task.
        // Nested scheduler drives restore the enclosing native context, so
        // the activity value obtained before a user call is still valid here.
        let resume = self.sync_native_active.map(|active| {
            let check = self.builder.create_block();
            self.builder.set_cold_block(check);
            let resume = self.builder.create_block();
            self.builder.ins().brif(active, check, &[], resume, &[]);
            self.builder.switch_to_block(check);
            self.builder.seal_block(check);
            resume
        });
        let cancelled = self.emit_value_runtime_call("willow_sync_cancelled", &[]);
        self.emit_sync_cancel_branch(cancelled);
        if let Some(resume) = resume {
            self.builder.ins().jump(resume, &[]);
            self.builder.switch_to_block(resume);
            self.builder.seal_block(resume);
        }
    }

    fn emit_sync_cancel_branch(&mut self, cancelled: cranelift_codegen::ir::Value) {
        if self.emitting_sync_cancel_cleanup {
            return;
        }
        let unwind = self.builder.create_block();
        self.builder.set_cold_block(unwind);
        let resume = self.builder.create_block();
        self.builder.ins().brif(cancelled, unwind, &[], resume, &[]);
        self.builder.switch_to_block(unwind);
        self.builder.seal_block(unwind);
        let roots_before = self.gc_root_count;
        let defer_depth_before = self.panic_defer_codegen_depth;
        let eligible_depth_before = self.recover_eligible_depth;
        let callstack_depth_before = self.callstack_frame_depth;
        let prepared_frames_before = std::mem::take(&mut self.lir_call_frames);
        let reference_scopes_before = std::mem::take(&mut self.lir_reference_scopes);
        for _ in 0..defer_depth_before {
            self.emit_void_runtime_call("willow_panic_leave_defer", &[]);
        }
        for _ in 0..callstack_depth_before {
            self.emit_callstack_pop();
        }
        for _ in 0..reference_scopes_before.len() {
            self.emit_debug_reference_call_clear();
        }
        self.panic_defer_codegen_depth = 0;
        self.recover_eligible_depth = 0;
        self.emitting_sync_cancel_cleanup = true;
        self.emit_void_runtime_call("willow_sync_cleanup_enter", &[]);
        if self.coop_frame.is_some() {
            // The runtime invokes this poll's collected cleanup entry, whose
            // frame flags cover every registration on the actual runtime path.
            self.emit_void_runtime_call("willow_sync_poll_cancel_cleanup", &[]);
            self.emit_void_runtime_call("willow_sync_cleanup_leave", &[]);
            // Cancellation abandons this poll rather than resuming at a LIR
            // suspension point. Constructor/call temporaries can still be
            // rooted here; pop them together with lexical binding roots.
            self.emit_pop_roots_n(self.gc_root_count);
            // Cleanup can raise a panic even while cancellation is sticky.
            // Publish that outcome before the scheduler wakes any awaiter.
            let active = self.emit_value_runtime_call("willow_panic_active", &[]);
            let ready = self.builder.ins().iconst(types::I32, 1);
            let panicked = self.builder.ins().iconst(types::I32, COOP_POLL_PANICKED);
            let outcome = self.builder.ins().select(active, panicked, ready);
            self.builder.ins().return_(&[outcome]);
        } else if let Some(return_block) = self.panic_return_block {
            self.emit_flush_defers_from(0);
            if !self.terminated {
                self.emit_void_runtime_call("willow_sync_cleanup_leave", &[]);
                self.builder.ins().jump(return_block, &[]);
            }
        } else {
            // Compiler-owned init/cleanup entries cannot originate a native
            // task safepoint. They only execute under their caller's cleanup.
            self.emit_void_runtime_call("willow_sync_cleanup_leave", &[]);
            self.builder.ins().jump(resume, &[]);
        }
        self.emitting_sync_cancel_cleanup = false;
        self.gc_root_count = roots_before;
        self.panic_defer_codegen_depth = defer_depth_before;
        self.recover_eligible_depth = eligible_depth_before;
        self.callstack_frame_depth = callstack_depth_before;
        self.lir_call_frames = prepared_frames_before;
        self.lir_reference_scopes = reference_scopes_before;
        self.builder.switch_to_block(resume);
        self.builder.seal_block(resume);
        self.terminated = false;
    }

    /// Branch an already-raised synchronous language panic to the nearest
    /// shared lexical cleanup. The cleanup flags, rather than duplicated AST,
    /// decide which registrations were active at this exact panic site.
    pub(super) fn emit_sync_panic_unwind(&mut self) {
        if self.emitting_sync_cancel_cleanup {
            // A panic exits the cancellation cleanup path before its normal
            // epilogue. Balance suppression even if a defer later recovers it.
            self.emit_void_runtime_call("willow_sync_cleanup_leave", &[]);
        }
        let codegen_depth_before = self.panic_defer_codegen_depth;
        let eligible_depth_before = self.recover_eligible_depth;
        let callstack_depth_before = self.callstack_frame_depth;
        for _ in 0..codegen_depth_before {
            self.emit_void_runtime_call("willow_panic_leave_defer", &[]);
        }
        // The raise-time diagnostic already owns a snapshot. Balance every
        // caller-owned debug frame before recovery can resume normal code.
        for _ in 0..callstack_depth_before {
            self.emit_callstack_pop();
        }
        for _ in 0..self.lir_reference_scopes.len() {
            self.emit_debug_reference_call_clear();
        }
        self.panic_defer_codegen_depth = 0;
        self.recover_eligible_depth = 0;
        if let Some(scope) = self.panic_scopes.last() {
            self.builder.ins().jump(scope.cleanup, &[]);
        } else if let Some(panic_return) = self.panic_return_block {
            self.builder.ins().jump(panic_return, &[]);
        } else {
            if self.coop_frame.is_some() {
                self.emit_unhandled_panic_exit();
            } else {
                if self.gc_root_count > 0 {
                    self.emit_pop_roots_n(self.gc_root_count);
                }
                self.emit_unhandled_panic_exit();
            }
        }
        self.panic_defer_codegen_depth = codegen_depth_before;
        self.recover_eligible_depth = eligible_depth_before;
        self.callstack_frame_depth = callstack_depth_before;
        self.terminated = true;
    }

    /// Emit one shared panic cleanup block for a defer-owning lexical scope.
    /// Synchronous scopes use stack flags; cooperative async scopes use frame
    /// flags. Both consume a registration before user cleanup, so a nested
    /// panic cannot run the same action twice.
    pub(super) fn emit_shared_panic_cleanup(&mut self, scope: &super::PanicScope) {
        let vars_before = self.vars.clone();
        // Every incoming panic edge already removed the function-owned frames.
        // Cleanup calls therefore start with an empty native debug stack suffix.
        let prepared_frames_before = std::mem::take(&mut self.lir_call_frames);
        let reference_scopes_before = std::mem::take(&mut self.lir_reference_scopes);
        let callstack_depth_before = std::mem::replace(&mut self.callstack_frame_depth, 0);
        let roots_before = self.gc_root_count;
        let coop_active_before = self
            .coop_shadow_roots
            .as_ref()
            .map(|roots| roots.active.clone());
        let unavailable_before = self.unavailable_defer_ids.clone();
        let codegen_depth_before = self.panic_defer_codegen_depth;
        let eligible_depth_before = self.recover_eligible_depth;

        self.builder.switch_to_block(scope.cleanup);
        self.terminated = false;
        // Every incoming unwind removed this function's active panic-defer
        // suffix. Enclosing deferred actions are suspended, not installed,
        // until a recovered edge resumes them.
        self.panic_defer_codegen_depth = 0;
        if let Some(depth) = scope.coop_root_depth_at_entry {
            self.gc_root_count = depth;
            self.coop_shadow_roots
                .as_mut()
                .expect("cooperative panic scope requires a root tracker")
                .active
                .truncate(depth);
        }
        let entries = self.defer_stack[scope.defer_depth].clone();
        let scope_can_recover = entries.iter().any(|entry| entry.recovery_capable);

        for entry in entries.iter().rev() {
            let run = self.builder.create_block();
            let next = self.builder.create_block();
            let flag = if let Some(slot) = entry.sync_flag_slot {
                self.stack_load(types::I64, slot)
            } else {
                let frame = self
                    .coop_frame
                    .expect("async panic cleanup requires a cooperative frame");
                let offset = entry
                    .flag_offset
                    .expect("async panic cleanup requires a frame flag");
                self.builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), frame, offset)
            };
            let registered = self.builder.ins().icmp_imm_u(IntCC::NotEqual, flag, 0);
            self.builder.ins().brif(registered, run, &[], next, &[]);

            self.builder.switch_to_block(run);
            self.builder.seal_block(run);
            let zero = self.builder.ins().iconst(types::I64, 0);
            if let Some(slot) = entry.sync_flag_slot {
                self.stack_store(zero, slot);
            } else {
                let frame = self
                    .coop_frame
                    .expect("async panic cleanup requires a cooperative frame");
                let offset = entry
                    .flag_offset
                    .expect("async panic cleanup requires a frame flag");
                self.builder
                    .ins()
                    .store(MemFlagsData::new(), zero, frame, offset);
            }
            self.vars = entry.vars_at_registration.clone();
            for (name, offset, ty) in &entry.bindings {
                self.vars.insert(
                    name.clone(),
                    VarStorage::Frame {
                        offset: *offset,
                        ty: ty.clone(),
                    },
                );
            }

            self.emit_void_runtime_call("willow_panic_enter_defer", &[]);
            self.panic_defer_codegen_depth = 1;
            self.recover_eligible_depth =
                eligible_depth_before + if entry.recovery_capable { 1 } else { 0 };
            self.emit_deferred_action(&entry.action);
            if !self.terminated {
                self.emit_void_runtime_call("willow_panic_leave_defer", &[]);
                self.builder.ins().jump(next, &[]);
            }

            // The flag-false predecessor always reaches `next`, even when the
            // run predecessor raised a nested panic and terminated.
            self.builder.switch_to_block(next);
            self.builder.seal_block(next);
            self.terminated = false;
            self.panic_defer_codegen_depth = 0;
            self.recover_eligible_depth = eligible_depth_before;
            self.vars = vars_before.clone();
        }

        // A panic leaves the critical section, so the lock must not survive the
        // unwind: release it after the section's own defers have run (they are
        // entitled to see the protected value) and before the panic reaches any
        // enclosing scope. A recovered panic resumes in this same task, so
        // waiting for task teardown would deadlock every other waiter.
        self.emit_lock_release_at_depth(scope.defer_depth);

        // The number of roots pushed in this scope depends on the panic path.
        // Restore the exact entry depth dynamically instead of baking in the
        // final source-order count.
        let current_depth = self.emit_value_runtime_call("willow_root_depth", &[]);
        let target_depth = if let Some(depth) = scope.coop_root_depth_at_entry {
            let depth = self.builder.ins().iconst(types::I32, depth as i64);
            self.builder.ins().iadd(scope.root_depth_at_entry, depth)
        } else {
            scope.root_depth_at_entry
        };
        let roots_to_pop = self.builder.ins().isub(current_depth, target_depth);
        self.emit_void_runtime_call("willow_pop_roots", &[roots_to_pop]);
        self.vars = scope.vars_before.clone();
        if let Some(depth) = scope.coop_root_depth_at_entry {
            self.gc_root_count = depth;
            self.coop_shadow_roots
                .as_mut()
                .expect("cooperative panic scope requires a root tracker")
                .active
                .truncate(depth);
        }

        let propagate = if scope_can_recover {
            let active = self.emit_value_runtime_call("willow_panic_active", &[]);
            let propagate = self.builder.create_block();
            let recovered = self.builder.create_block();
            self.builder
                .ins()
                .brif(active, propagate, &[], recovered, &[]);
            self.builder.switch_to_block(recovered);
            self.builder.seal_block(recovered);
            for (name, span) in &scope.call_frames_at_entry {
                self.emit_callstack_push(name, *span);
            }
            self.lir_reference_scopes = scope.reference_scopes_at_entry.clone();
            self.emit_replay_reference_scopes();
            for _ in 0..codegen_depth_before {
                self.emit_void_runtime_call("willow_panic_enter_defer", &[]);
            }
            self.builder.ins().jump(scope.resume, &[]);
            // The propagation edge still has no prepared frames.
            self.callstack_frame_depth = 0;
            self.lir_reference_scopes.clear();
            self.panic_recovery_targets.insert(scope.resume);
            self.builder.switch_to_block(propagate);
            self.builder.seal_block(propagate);
            Some(propagate)
        } else {
            None
        };

        let parent_cleanup = self
            .panic_scopes
            .iter()
            .rev()
            .nth(1)
            .map(|parent| parent.cleanup);
        if let Some(parent) = parent_cleanup {
            self.builder.ins().jump(parent, &[]);
        } else if let Some(panic_return) = self.panic_return_block {
            self.builder.ins().jump(panic_return, &[]);
        } else {
            self.emit_unhandled_panic_exit();
        }
        let _ = propagate;
        self.lir_call_frames = prepared_frames_before;
        self.lir_reference_scopes = reference_scopes_before;
        self.callstack_frame_depth = callstack_depth_before;
        self.vars = vars_before;
        self.gc_root_count = roots_before;
        self.unavailable_defer_ids = unavailable_before;
        self.panic_defer_codegen_depth = codegen_depth_before;
        self.recover_eligible_depth = eligible_depth_before;
        if let (Some(active), Some(roots)) = (coop_active_before, self.coop_shadow_roots.as_mut()) {
            roots.active = active;
        }
        self.terminated = true;
    }

    /// Leave the generated Willow boundary while preserving the active panic.
    /// Cooperative polls publish the Panicked outcome to the scheduler first;
    /// synchronous native boundaries report and abort here.
    fn emit_unhandled_panic_exit(&mut self) {
        if self.coop_frame.is_some() {
            // Unlike an ordinary suspension boundary, an unhandled panic can
            // be observed immediately after a call while expression
            // temporaries are still rooted.  The native poll frame is being
            // abandoned, so remove every root registered on this CFG path;
            // requiring all of them to be tracked lexical bindings would
            // reject otherwise-valid allocating expressions.
            if self.gc_root_count > 0 {
                self.emit_pop_roots_n(self.gc_root_count);
            }
            let panicked = self.builder.ins().iconst(types::I32, COOP_POLL_PANICKED);
            self.builder.ins().return_(&[panicked]);
        } else {
            self.emit_void_runtime_call("willow_panic_finish_unhandled", &[]);
            self.builder.ins().trap(TrapCode::unwrap_user(1));
        }
    }

    pub(super) fn emit_void_runtime_call(
        &mut self,
        name: &str,
        args: &[cranelift_codegen::ir::Value],
    ) {
        assert!(
            self.emit_runtime_call_with_cleanup(name, args, |_| {})
                .is_none(),
            "runtime call `{name}` unexpectedly returned a value"
        );
    }

    pub(super) fn emit_value_runtime_call(
        &mut self,
        name: &str,
        args: &[cranelift_codegen::ir::Value],
    ) -> cranelift_codegen::ir::Value {
        self.emit_runtime_call_with_cleanup(name, args, |_| {})
            .unwrap_or_else(|| panic!("runtime call `{name}` unexpectedly returned void"))
    }

    /// Emit one runtime ABI call. Recoverable-panic propagation is selected
    /// automatically from the symbol's ABI metadata; callers cannot obtain a
    /// neutral return value from a `MAY_PANIC` helper without first branching
    /// on the runtime panic depth.
    ///
    /// `after_call` runs after the raw call and result capture but before the
    /// panic branch. It exists for transient GC roots whose compile-time and
    /// runtime depths must be balanced on both the normal and unwind paths.
    pub(super) fn emit_runtime_call_with_cleanup<F>(
        &mut self,
        name: &str,
        args: &[cranelift_codegen::ir::Value],
        after_call: F,
    ) -> Option<cranelift_codegen::ir::Value>
    where
        F: FnOnce(&mut Self),
    {
        let symbol = crate::backend::abi::runtime_symbol(name)
            .unwrap_or_else(|| panic!("runtime call `{name}` is missing from the ABI schema"));
        let effects = symbol.effects();
        let may_panic = effects.contains(crate::backend::abi::RuntimeEffects::MAY_PANIC);
        let no_preempt = effects.contains(crate::backend::abi::RuntimeEffects::NO_PREEMPT_REGION);
        let panic_depth = if may_panic {
            self.emit_pre_willow_call_panic_depth()
        } else {
            None
        };

        // Runtime ABI metadata is the source of truth for transitions that
        // require a generated-code guard. Helpers that already own a
        // runtime-side NoPreemptGuard intentionally omit this effect, avoiding
        // a second enter/leave pair on their hot paths.
        if no_preempt {
            self.emit_void_runtime_call("willow_preempt_enter_no_preempt", &[]);
        }

        // Deliberately bypass `func_id`: that ordinary lookup rejects
        // `MAY_PANIC` symbols so the raw id is available only inside this
        // metadata-driven call path.
        let fid = *self
            .func_ids
            .get(name)
            .unwrap_or_else(|| panic!("backend: undeclared runtime symbol `{name}`"));
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, args);
        let result = self.builder.inst_results(call).first().copied();
        assert!(
            self.builder.inst_results(call).len() <= 1,
            "runtime call `{name}` returned more than one ABI value"
        );

        // Restore the quantum state before both normal continuation and the
        // generated recoverable-panic branch.
        if no_preempt {
            self.emit_void_runtime_call("willow_preempt_leave_no_preempt", &[]);
        }

        after_call(self);
        if may_panic {
            self.emit_post_willow_call_panic_check(panic_depth);
        }
        result
    }

    /// Snapshot active panic depth before a participating Willow call. During
    /// panic-defer execution the depth may already be non-zero.
    pub(super) fn emit_pre_willow_call_panic_depth(
        &mut self,
    ) -> Option<cranelift_codegen::ir::Value> {
        let reusable = self.panic_depth_snapshot.and_then(|(block, depth)| {
            (self.builder.current_block() == Some(block)
                && std::env::var_os("WILLOW_PANIC_SNAPSHOT_REUSE").is_none_or(|value| value != "0")
                && self
                    .builder
                    .func
                    .layout
                    .block_insts(block)
                    .all(|inst| !self.builder.func.dfg.insts[inst].opcode().is_call()))
            .then_some(depth)
        });
        // Publishing diagnostic location cannot mutate panic depth. Query the
        // proof before emitting this marker so debug metadata is not itself
        // an artificial invalidation at this call's snapshot point.
        self.emit_fault_site();
        Some(reusable.unwrap_or_else(|| self.emit_value_runtime_call("willow_panic_depth", &[])))
    }

    /// Debug builds: publish the statement being executed before a runtime call
    /// that can raise. Faults raised inside a runtime helper (array bounds, a
    /// blocked channel op, an awaited cancelled task) carry no location of
    /// their own, so without this their `PanicInfo` would report `:0:0`
    /// (willow-s9ej.7). Release builds skip it: the store is not worth a call
    /// on every collection access.
    pub(super) fn emit_fault_site(&mut self) {
        if self.build_mode != BuildMode::Debug {
            return;
        }
        let Some(span) = self.fault_site_span else {
            return;
        };
        let file = self.source_file.to_string();
        let Some((file_ptr, file_len)) = self.emit_static_str_bytes(&file) else {
            return;
        };
        let line = self.builder.ins().iconst(types::I64, span.line as i64);
        let column = self.builder.ins().iconst(types::I64, span.col as i64);
        self.emit_void_runtime_call("willow_fault_site_set", &[file_ptr, file_len, line, column]);
    }

    /// Branch away before observing a neutral result only when the callee
    /// added a new panic record (willow-s9ej.4).
    pub(super) fn emit_post_willow_call_panic_check(
        &mut self,
        depth_before: Option<cranelift_codegen::ir::Value>,
    ) {
        // Cancellation is sticky even through functions whose panic summary
        // is empty, and is intentionally independent of recoverable panics.
        self.emit_sync_cancel_check();
        let Some(depth_before) = depth_before else {
            return;
        };
        let depth_after = self.emit_value_runtime_call("willow_panic_depth", &[]);
        let raised = self
            .builder
            .ins()
            .icmp(IntCC::UnsignedGreaterThan, depth_after, depth_before);
        let panicked = self.builder.create_block();
        let normal = self.builder.create_block();
        self.builder.ins().brif(raised, panicked, &[], normal, &[]);

        self.builder.switch_to_block(panicked);
        self.builder.seal_block(panicked);
        if self.coop_frame.is_some() {
            self.emit_sync_panic_unwind();
        } else if self.is_async {
            self.emit_void_runtime_call("willow_panic_finish_unhandled", &[]);
            self.builder.ins().trap(TrapCode::unwrap_user(1));
            self.terminated = true;
        } else {
            self.emit_sync_panic_unwind();
        }

        self.builder.switch_to_block(normal);
        self.builder.seal_block(normal);
        self.terminated = false;
        // Cache the OBSERVED depth, never the entry/expected depth: a callee
        // may recover an existing panic and lower the depth. The observation
        // dominates this new, empty continuation block. Any intervening call
        // (including recover, indirect calls, or context switches) invalidates
        // reuse, as does moving to another control-flow block.
        self.panic_depth_snapshot = Some((normal, depth_after));
    }

    /// Raise a user-visible language fault whose message is already a Willow
    /// String, then leave the current expression through the same lexical
    /// panic path as an explicit `panic(...)`.  The message is kept rooted
    /// while source metadata is materialized.
    pub(super) fn emit_language_panic(
        &mut self,
        message: cranelift_codegen::ir::Value,
        span: Option<crate::diagnostics::Span>,
    ) {
        self.emit_push_root(message);
        let source_file = self.source_file.to_string();
        let file = self.emit_string_literal(&source_file);
        let line = self
            .builder
            .ins()
            .iconst(types::I64, span.map_or(0, |value| value.line) as i64);
        let column = self
            .builder
            .ins()
            .iconst(types::I64, span.map_or(0, |value| value.col) as i64);
        self.emit_runtime_call_with_cleanup(
            "willow_panic_raise",
            &[message, file, line, column],
            |this| {
                this.emit_pop_roots_n(1);
                this.gc_root_count -= 1;
            },
        );
        // `willow_panic_raise` must always increase panic depth. Reaching the
        // metadata-generated normal branch is an ABI violation.
        self.builder.ins().trap(TrapCode::unwrap_user(1));
        self.terminated = true;
    }

    /// Materialize the shared abnormal ABI return of a synchronous callee.
    /// Restore the caller's exact root depth, then return a typed neutral value
    /// without consuming or clearing the active panic.
    pub(super) fn emit_panic_return(&mut self, return_ty: &Type, force_void: bool) {
        let Some(block) = self.panic_return_block else {
            return;
        };
        self.builder.switch_to_block(block);
        self.builder.seal_block(block);
        if let Some(entry_depth) = self.panic_function_root_depth {
            let current_depth = self.emit_value_runtime_call("willow_root_depth", &[]);
            let roots_to_pop = self.builder.ins().isub(current_depth, entry_depth);
            self.emit_void_runtime_call("willow_pop_roots", &[roots_to_pop]);
        }
        if *return_ty == Type::Void || force_void {
            self.builder.ins().return_(&[]);
        } else {
            let zero = match clif_type(reference_type(self.module.target_config()), return_ty) {
                types::F64 => self.builder.ins().f64const(0.0),
                ty => self.builder.ins().iconst(ty, 0),
            };
            self.builder.ins().return_(&[zero]);
        }
        self.terminated = true;
    }
}
