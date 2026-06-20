//! Async / cooperative-scheduler codegen for the Cranelift backend (extracted
//! from `mod.rs`): cooperative `main`/leaf compilation and the await / coop
//! statement / select / range-for lowering. `pub(super)` so the main codegen
//! driver can call them; child-module access reaches private FuncGen/Codegen
//! state.

use anyhow::Result;
use cranelift_codegen::ir::{InstBuilder, MemFlags, condcodes::IntCC, types};
use cranelift_module::Module;

use super::*;

impl Codegen {
    /// Cooperative-async lowering (willow-lpn.5.3 / willow-h2vf):
    /// compile `async fn main` as a SUSPENDING poll function driven
    /// by the cooperative scheduler. `willow_user_main` becomes a driver that
    /// allocates the frame, spawns the poll fn as a task, and runs the scheduler;
    /// the poll fn is a state machine whose `await sleep(n)` points store the
    /// next state in the frame's state word (offset 0), call `willow_sched_sleep`,
    /// and return Pending — the timer-aware run loop resumes it.
    pub(super) fn compile_cooperative_main(&mut self, name: &str, f: &FunctionDecl) -> Result<()> {
        // Declare the poll fn `fn(frame: i64) -> i32`.
        let poll_symbol = format!("{}__poll", USER_MAIN_SYMBOL);
        let mut poll_sig = self.module.make_signature();
        poll_sig.params.push(AbiParam::new(types::I64));
        poll_sig.returns.push(AbiParam::new(types::I32));
        let poll_fid = self
            .module
            .declare_function(&poll_symbol, Linkage::Local, &poll_sig)?;
        self.func_ids.insert(poll_symbol.clone(), poll_fid);

        // Frame-back params and EVERY local (GC and non-GC) so they survive
        // suspension; only GC-managed slots are in `gc_slot_mask` (traced), so
        // non-GC slots hold plain scalars (willow-lpn.5.3 slice 3b).
        let mut slots: Vec<AsyncFrameSlot> = f
            .params
            .iter()
            .map(|p| AsyncFrameSlot {
                key: p.span,
                name: p.name.clone(),
                ty: p.ty.clone(),
            })
            .collect();
        let mut seen: HashSet<crate::diagnostics::Span> = f.params.iter().map(|p| p.span).collect();
        self.coop_collect_let_slots(&f.body, &mut slots, &mut seen);
        let layout = AsyncFrameLayout::try_new(slots, &self.enum_infos)?;
        self.record_async_frame_size_warning(&f.name, f.span, &layout);
        let mut offsets: HashMap<crate::diagnostics::Span, i32> = HashMap::new();
        for (i, slot) in layout.slots.iter().enumerate() {
            offsets.insert(slot.key, async_frame_slot_offset(i));
        }
        let slot_count = layout.slot_count() as i64;
        let mask = layout.gc_slot_mask as i64;
        let param_bindings: Vec<(String, i32, Type)> = f
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| (p.name.clone(), async_frame_slot_offset(i), p.ty.clone()))
            .collect();

        self.compile_coop_main_driver(name, &poll_symbol, slot_count, mask, &f.params)?;
        self.compile_coop_main_poll(&poll_symbol, f, offsets, None, &param_bindings, None)?;
        Ok(())
    }

    /// Cooperative task lowering (willow-lpn.5.3 / willow-h2vf): compile `name`
    /// as a CONSTRUCTOR (its public symbol: alloc a frame whose slot 0 is the
    /// RESULT and slot 1 is TASK_ID, spawn the poll fn as a task, return the
    /// frame ptr) plus a suspending poll fn whose `return v` stores `v` at the
    /// RESULT slot. The returned frame is the language-level `Task<T>`.
    pub(super) fn compile_cooperative_leaf(&mut self, name: &str, f: &FunctionDecl) -> Result<()> {
        let poll_symbol = format!("{name}__coop_poll");
        let mut poll_sig = self.module.make_signature();
        poll_sig.params.push(AbiParam::new(types::I64));
        poll_sig.returns.push(AbiParam::new(types::I32));
        let poll_fid = self
            .module
            .declare_function(&poll_symbol, Linkage::Local, &poll_sig)?;
        self.func_ids.insert(poll_symbol.clone(), poll_fid);

        // Frame layout: slot 0 = RESULT (return type), slot 1 = TASK_ID (the
        // scheduler task id, i64 non-GC, so an AWAITER can willow_sched_await it),
        // slots 2.. = params, then locals and call-await scratch slots. GC mask
        // marks GC-ref slots only.
        let mut slots = vec![
            AsyncFrameSlot {
                key: f.span,
                name: "__result".to_string(),
                ty: f.return_type.clone(),
            },
            AsyncFrameSlot {
                key: crate::diagnostics::Span::new(usize::MAX, usize::MAX, 0, 0),
                name: "__task_id".to_string(),
                ty: Type::I64,
            },
        ];
        for p in &f.params {
            slots.push(AsyncFrameSlot {
                key: p.span,
                name: p.name.clone(),
                ty: p.ty.clone(),
            });
        }
        // Locals after the params: frame-backed so they survive the task's own
        // suspensions, keyed by declaration span.
        let n_params = f.params.len();
        let mut seen: HashSet<crate::diagnostics::Span> = HashSet::new();
        self.coop_collect_let_slots(&f.body, &mut slots, &mut seen);
        let layout = AsyncFrameLayout::try_new(slots, &self.enum_infos)?;
        self.record_async_frame_size_warning(&f.name, f.span, &layout);
        let slot_count = layout.slot_count() as i64;
        let mask = layout.gc_slot_mask as i64;
        let result_offset = async_frame_slot_offset(FRAME_SLOT_RESULT);
        let task_id_offset = async_frame_slot_offset(FRAME_SLOT_TASK_ID);
        let param_bindings: Vec<(String, i32, Type)> = f
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| (p.name.clone(), async_frame_slot_offset(2 + i), p.ty.clone()))
            .collect();
        // Offsets for the poll fn's locals: layout slots from (2 + n_params) on.
        let mut offsets: HashMap<crate::diagnostics::Span, i32> = HashMap::new();
        for (i, slot) in layout.slots.iter().enumerate().skip(2 + n_params) {
            offsets.insert(slot.key, async_frame_slot_offset(i));
        }

        // Constructor = the fn's public symbol: alloc frame, store args into the
        // param slots, spawn the poll task, return the frame ptr (the Task).
        let ctor_fid = self.func_ids[name];
        let mut ctx = self.module.make_context();
        let mut sig = self.module.make_signature();
        for p in &f.params {
            sig.params.push(AbiParam::new(clif_type(&p.ty)));
        }
        sig.returns.push(AbiParam::new(types::I64));
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, ctor_fid.as_u32());
        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);
        let alloc_fid = self.func_id("willow_async_frame_alloc");
        let alloc_ref = self.module.declare_func_in_func(alloc_fid, builder.func);
        let sc = builder.ins().iconst(types::I64, slot_count);
        let mk = builder.ins().iconst(types::I64, mask);
        let call = builder.ins().call(alloc_ref, &[sc, mk]);
        let frame = builder.inst_results(call)[0];
        // Store args into their param slots (slots 2..) before spawning (no
        // allocation happens between alloc and spawn, so the unrooted frame is
        // safe).
        for (i, _p) in f.params.iter().enumerate() {
            let arg = builder.block_params(entry)[i];
            let off = async_frame_slot_offset(2 + i);
            builder.ins().store(MemFlags::trusted(), arg, frame, off);
        }
        let poll_ref = self.module.declare_func_in_func(poll_fid, builder.func);
        let poll_addr = builder.ins().func_addr(types::I64, poll_ref);
        let spawn_fid = self.func_id("willow_sched_spawn");
        let spawn_ref = self.module.declare_func_in_func(spawn_fid, builder.func);
        // Record the scheduler task id in slot 1 (TASK_ID) so an awaiter can
        // willow_sched_await it.
        let spawn_call = builder.ins().call(spawn_ref, &[poll_addr, frame]);
        let task_id = builder.inst_results(spawn_call)[0];
        builder
            .ins()
            .store(MemFlags::trusted(), task_id, frame, task_id_offset);
        builder.ins().return_(&[frame]);
        builder.finalize();
        self.module.define_function(ctor_fid, &mut ctx)?;
        self.module.clear_context(&mut ctx);

        // Poll fn = the state machine; params are bound from their frame slots
        // and locals are frame-backed via `offsets`.
        self.compile_coop_main_poll(
            &poll_symbol,
            f,
            offsets,
            Some(result_offset),
            &param_bindings,
            None,
        )?;
        Ok(())
    }

    /// Cooperative async method lowering: compile the method symbol itself as a
    /// task constructor with normal method ABI (`self`, params...) -> `Task<T>`,
    /// then compile a poll fn that binds `self`/params from the async frame.
    pub(super) fn compile_cooperative_method(
        &mut self,
        class_name: &str,
        mangled: &str,
        m: &MethodDecl,
    ) -> Result<()> {
        let poll_symbol = format!("{mangled}__coop_poll");
        let mut poll_sig = self.module.make_signature();
        poll_sig.params.push(AbiParam::new(types::I64));
        poll_sig.returns.push(AbiParam::new(types::I32));
        let poll_fid = self
            .module
            .declare_function(&poll_symbol, Linkage::Local, &poll_sig)?;
        self.func_ids.insert(poll_symbol.clone(), poll_fid);

        let mut slots = vec![
            AsyncFrameSlot {
                key: m.span,
                name: "__result".to_string(),
                ty: m.return_type.clone(),
            },
            AsyncFrameSlot {
                key: crate::diagnostics::Span::new(usize::MAX, usize::MAX, 0, 0),
                name: "__task_id".to_string(),
                ty: Type::I64,
            },
        ];
        let self_offset = if m.is_static {
            None
        } else {
            let offset = async_frame_slot_offset(slots.len());
            slots.push(AsyncFrameSlot {
                key: crate::diagnostics::Span::new(usize::MAX - 1, usize::MAX - 1, 0, 0),
                name: "self".to_string(),
                ty: Type::Named(class_name.to_string()),
            });
            Some(offset)
        };
        let first_param_slot = slots.len();
        for p in &m.params {
            slots.push(AsyncFrameSlot {
                key: p.span,
                name: p.name.clone(),
                ty: p.ty.clone(),
            });
        }

        let mut seen: HashSet<crate::diagnostics::Span> = HashSet::new();
        self.coop_collect_let_slots(&m.body, &mut slots, &mut seen);
        let layout = AsyncFrameLayout::try_new(slots, &self.enum_infos)?;
        self.record_async_frame_size_warning(&format!("{class_name}::{}", m.name), m.span, &layout);
        let slot_count = layout.slot_count() as i64;
        let mask = layout.gc_slot_mask as i64;
        let result_offset = async_frame_slot_offset(FRAME_SLOT_RESULT);
        let task_id_offset = async_frame_slot_offset(FRAME_SLOT_TASK_ID);

        let mut param_bindings: Vec<(String, i32, Type)> = Vec::new();
        if let Some(offset) = self_offset {
            param_bindings.push((
                "self".to_string(),
                offset,
                Type::Named(class_name.to_string()),
            ));
        }
        param_bindings.extend(m.params.iter().enumerate().map(|(i, p)| {
            (
                p.name.clone(),
                async_frame_slot_offset(first_param_slot + i),
                p.ty.clone(),
            )
        }));

        let reserved_slots = first_param_slot + m.params.len();
        let mut offsets: HashMap<crate::diagnostics::Span, i32> = HashMap::new();
        for (i, slot) in layout.slots.iter().enumerate().skip(reserved_slots) {
            offsets.insert(slot.key, async_frame_slot_offset(i));
        }

        let ctor_fid = self.func_ids[mangled];
        let mut ctx = self.module.make_context();
        let mut sig = self.module.make_signature();
        let ptr_ty = self.module.target_config().pointer_type();
        sig.params.push(AbiParam::new(types::I64)); // self/dummy method ABI slot
        for p in &m.params {
            sig.params.push(AbiParam::new(param_abi_type(p, ptr_ty)));
        }
        sig.returns.push(AbiParam::new(types::I64));
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, ctor_fid.as_u32());
        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        let alloc_fid = self.func_id("willow_async_frame_alloc");
        let alloc_ref = self.module.declare_func_in_func(alloc_fid, builder.func);
        let sc = builder.ins().iconst(types::I64, slot_count);
        let mk = builder.ins().iconst(types::I64, mask);
        let call = builder.ins().call(alloc_ref, &[sc, mk]);
        let frame = builder.inst_results(call)[0];

        if let Some(offset) = self_offset {
            let self_arg = builder.block_params(entry)[0];
            builder
                .ins()
                .store(MemFlags::trusted(), self_arg, frame, offset);
        }
        for (i, _p) in m.params.iter().enumerate() {
            let arg = builder.block_params(entry)[i + 1];
            let off = async_frame_slot_offset(first_param_slot + i);
            builder.ins().store(MemFlags::trusted(), arg, frame, off);
        }

        let poll_ref = self.module.declare_func_in_func(poll_fid, builder.func);
        let poll_addr = builder.ins().func_addr(types::I64, poll_ref);
        let spawn_fid = self.func_id("willow_sched_spawn");
        let spawn_ref = self.module.declare_func_in_func(spawn_fid, builder.func);
        let spawn_call = builder.ins().call(spawn_ref, &[poll_addr, frame]);
        let task_id = builder.inst_results(spawn_call)[0];
        builder
            .ins()
            .store(MemFlags::trusted(), task_id, frame, task_id_offset);
        builder.ins().return_(&[frame]);
        builder.finalize();
        self.module.define_function(ctor_fid, &mut ctx)?;
        self.module.clear_context(&mut ctx);

        let poll_decl = FunctionDecl {
            name: format!("{class_name}::{}", m.name),
            public: m.public,
            is_async: true,
            params: m.params.clone(),
            return_type: m.return_type.clone(),
            body: m.body.clone(),
            span: m.span,
        };
        self.compile_coop_main_poll(
            &poll_symbol,
            &poll_decl,
            offsets,
            Some(result_offset),
            &param_bindings,
            Some(class_name),
        )?;
        Ok(())
    }

    /// `willow_user_main` driver: alloc frame, bind any main args, spawn the
    /// poll task, run the scheduler to completion.
    pub(super) fn compile_coop_main_driver(
        &mut self,
        name: &str,
        poll_symbol: &str,
        slot_count: i64,
        mask: i64,
        params: &[Param],
    ) -> Result<()> {
        let func_id = self.func_ids[name];
        let sig = self.module.make_signature(); // void, no params
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, func_id.as_u32());
        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
        let entry = builder.create_block();
        builder.switch_to_block(entry);
        builder.seal_block(entry);

        // frame = willow_async_frame_alloc(slot_count, mask)
        let alloc_fid = self.func_id("willow_async_frame_alloc");
        let alloc_ref = self.module.declare_func_in_func(alloc_fid, builder.func);
        let slot_count_v = builder.ins().iconst(types::I64, slot_count);
        let mask_v = builder.ins().iconst(types::I64, mask);
        let call = builder.ins().call(alloc_ref, &[slot_count_v, mask_v]);
        let frame = builder.inst_results(call)[0];

        if let Some(param) = params.first() {
            let arr_id = self.func_id("willow_runtime_args_array");
            let arr_ref = self.module.declare_func_in_func(arr_id, builder.func);
            let arr_call = builder.ins().call(arr_ref, &[]);
            let arr = builder.inst_results(arr_call)[0];
            builder.ins().store(
                MemFlags::trusted(),
                arr,
                frame,
                async_frame_slot_offset(FRAME_SLOT_RESULT),
            );
            debug_assert_eq!(param.name, "args");
        }

        // willow_sched_spawn(poll_addr, frame) -> main's task id.
        let poll_fid = self.func_ids[poll_symbol];
        let poll_ref = self.module.declare_func_in_func(poll_fid, builder.func);
        let poll_addr = builder.ins().func_addr(types::I64, poll_ref);
        let spawn_fid = self.func_id("willow_sched_spawn");
        let spawn_ref = self.module.declare_func_in_func(spawn_fid, builder.func);
        let spawn_call = builder.ins().call(spawn_ref, &[poll_addr, frame]);
        let main_task_id = builder.inst_results(spawn_call)[0];

        // Drive the scheduler only until `main` itself completes (willow-bsqy):
        // the program exits when main returns, rather than draining every
        // un-joined task to quiescence (which could hang on a non-terminating
        // background task). Well-behaved programs join their tasks before main
        // returns, so nothing is left to run anyway.
        let run_fid = self.func_id("willow_sched_run_until");
        let run_ref = self.module.declare_func_in_func(run_fid, builder.func);
        builder.ins().call(run_ref, &[main_task_id]);

        builder.ins().return_(&[]);
        builder.finalize();
        self.module.define_function(func_id, &mut ctx)?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }

    /// The poll-fn state machine: split the body at `await sleep(n)` points into
    /// per-state segments; the entry dispatches on the frame state word.
    pub(super) fn compile_coop_main_poll(
        &mut self,
        poll_symbol: &str,
        f: &FunctionDecl,
        offsets: HashMap<crate::diagnostics::Span, i32>,
        result_offset: Option<i32>,
        param_bindings: &[(String, i32, Type)],
        current_class: Option<&str>,
    ) -> Result<()> {
        let func_id = self.func_ids[poll_symbol];
        // Declare the async fn name as static bytes so the poll fn can tag its
        // task for async stack traces (debug builds only; willow-9lw).
        let tag_name = if self.build_mode == BuildMode::Debug {
            self.declare_string_literal(&f.name)?;
            Some(f.name.clone())
        } else {
            None
        };
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        sig.returns.push(AbiParam::new(types::I32));
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, func_id.as_u32());
        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let frame = builder.block_params(entry)[0];
        // Tag the running task with this async fn's name on every poll entry (so
        // resumes re-tag too), before dispatch (willow-9lw).
        if let Some(name) = &tag_name
            && let Some(&data_id) = self.string_literals.get(name)
        {
            let gv = self.module.declare_data_in_func(data_id, builder.func);
            let ptr_ty = self.module.target_config().pointer_type();
            let name_ptr = builder.ins().global_value(ptr_ty, gv);
            let name_len = builder.ins().iconst(types::I64, name.len() as i64);
            let tag_id = self.func_id("willow_sched_tag_current_task");
            let tag_ref = self.module.declare_func_in_func(tag_id, builder.func);
            builder.ins().call(tag_ref, &[name_ptr, name_len]);
        }
        let dispatch = builder.create_block();
        builder.ins().jump(dispatch, &[]);

        // body_start is state 0; each `await` suspend appends a resume block
        // (state k = suspends[k-1]). Because all locals/params are frame-backed,
        // resume blocks need no SSA block params — we emit structured control
        // flow (if/while) directly and seal everything at the end (slice 5).
        let body_start = builder.create_block();
        let mut suspends: Vec<cranelift_codegen::ir::Block> = Vec::new();
        {
            let mut fg = FuncGen {
                builder: &mut builder,
                module: &mut self.module,
                func_ids: &self.func_ids,
                func_return_types: &self.func_return_types,
                fn_types: &self.fn_types,
                func_param_modes: &self.func_param_modes,
                func_param_debug: &self.func_param_debug,
                known_modules: &self.known_modules,
                lambda_names: &self.lambda_names,
                cooperative_leaves: &self.cooperative_leaves,
                string_literals: &self.string_literals,
                class_layouts: &self.class_layouts,
                static_storage: &self.static_storage,
                enum_infos: &self.enum_infos,
                class_base: &self.class_base,
                class_type_ids: &self.class_type_ids,
                lambda_return_types: &self.lambda_return_types,
                lambda_fn_types: &self.lambda_fn_types,
                interface_infos: &self.interface_infos,
                vtable_ids: &self.vtable_ids,
                async_local_types: &self.async_local_types,
                enum_variant_resolutions: &self.enum_variant_resolutions,
                pattern_resolutions: &self.pattern_resolutions,
                // The frame is the poll fn's parameter (allocated + GC-rooted by
                // the driver via willow_sched_spawn); locals are frame-backed via
                // these offsets so they survive suspension.
                async_frame: Some(frame),
                async_frame_offsets: offsets,
                main_result_err_ty: None,
                vars: HashMap::new(),
                return_type: f.return_type.clone(),
                current_class,
                is_async: false,
                terminated: false,
                gc_root_count: 0,
                build_mode: self.build_mode,
                source_file: &self.source_file,
            };
            // Bind params from their frame slots (cooperative leaf, slice 4b):
            // the constructor stored the args there before spawning.
            for (name, offset, ty) in param_bindings {
                fg.vars.insert(
                    name.clone(),
                    VarStorage::Frame {
                        offset: *offset,
                        ty: ty.clone(),
                    },
                );
            }
            fg.builder.switch_to_block(body_start);
            let falls_through =
                fg.emit_coop_stmts(&f.body.stmts, &mut suspends, frame, result_offset);
            // Fell off the end of the body → the task is Ready.
            if falls_through {
                let ready = fg.builder.ins().iconst(types::I32, 1);
                fg.builder.ins().return_(&[ready]);
            }
        }

        // Dispatch on the state word (offset 0): state 0 → body_start,
        // state k → suspends[k-1].
        builder.switch_to_block(dispatch);
        let state = builder.ins().load(types::I64, MemFlags::new(), frame, 0i32);
        for (k, resume) in suspends.iter().enumerate() {
            let want = builder.ins().iconst(types::I64, (k + 1) as i64);
            let is_k = builder.ins().icmp(IntCC::Equal, state, want);
            let next = builder.create_block();
            builder.ins().brif(is_k, *resume, &[], next, &[]);
            builder.switch_to_block(next);
        }
        builder.ins().jump(body_start, &[]);
        builder.seal_all_blocks();

        builder.finalize();
        self.module.define_function(func_id, &mut ctx)?;
        self.module.clear_context(&mut ctx);
        Ok(())
    }
}

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Emit a preemption check whose resumed poll continues at `resume`. A
    /// tripped check records that block as the frame state and returns
    /// `RUNTIME_POLL_PREEMPTED`; otherwise execution branches there directly.
    /// Cooperative locals are frame-backed, so no SSA values cross the boundary.
    fn emit_coop_safepoint_to(
        &mut self,
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
        resume: cranelift_codegen::ir::Block,
    ) {
        let check_id = self.func_id("willow_preempt_check");
        let check_ref = self
            .module
            .declare_func_in_func(check_id, self.builder.func);
        let call = self.builder.ins().call(check_ref, &[]);
        let requested = self.builder.inst_results(call)[0];
        let zero = self.builder.ins().iconst(types::I32, 0);
        let should_preempt = self.builder.ins().icmp(IntCC::NotEqual, requested, zero);
        let preempt_b = self.builder.create_block();
        self.builder
            .ins()
            .brif(should_preempt, preempt_b, &[], resume, &[]);

        self.builder.switch_to_block(preempt_b);
        let state = (suspends.len() + 1) as i64;
        let state_value = self.builder.ins().iconst(types::I64, state);
        self.builder
            .ins()
            .store(MemFlags::new(), state_value, frame, 0i32);
        let preempted = self.builder.ins().iconst(types::I32, COOP_POLL_PREEMPTED);
        self.builder.ins().return_(&[preempted]);
        suspends.push(resume);
    }

    /// Safepoint at a source statement boundary. Resumption targets the fresh
    /// continuation after the check, so a budget of one cannot repeatedly
    /// preempt at the same statement without executing it.
    fn emit_coop_statement_safepoint(
        &mut self,
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
    ) {
        let continuation = self.builder.create_block();
        self.emit_coop_safepoint_to(suspends, frame, continuation);
        self.builder.switch_to_block(continuation);
    }

    pub(super) fn emit_ready_future_void(&mut self) -> cranelift_codegen::ir::Value {
        let fid = self.func_id("willow_future_ready_void");
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, &[]);
        self.builder.inst_results(call)[0]
    }

    pub(super) fn emit_ready_future(
        &mut self,
        ty: &Type,
        value: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let runtime_name = future_ready_runtime_name(ty);
        let fid = self.func_ids[runtime_name];
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, &[value]);
        self.builder.inst_results(call)[0]
    }

    pub(super) fn emit_await(&mut self, await_expr: &AwaitExpr) -> cranelift_codegen::ir::Value {
        let awaitable_ty = self.ast_type_of(&await_expr.expr);
        let output_ty = task_output_type(&awaitable_ty)
            .or_else(|| future_output_type(&awaitable_ty))
            .unwrap_or(Type::Void);

        // `await task`: the expression evaluates to an eager task frame. Outside
        // a cooperative poll lowering, block-run the scheduler until just this
        // task completes (slot 1 = task id), then read slot 0 (willow-bsqy).
        if task_output_type(&awaitable_ty).is_some() {
            let frame = self.emit_expr(&await_expr.expr);
            self.emit_push_root(frame);
            let task_id = self.builder.ins().load(
                types::I64,
                MemFlags::new(),
                frame,
                async_frame_slot_offset(FRAME_SLOT_TASK_ID),
            );
            let run_fid = self.func_id("willow_sched_run_until");
            let run_ref = self.module.declare_func_in_func(run_fid, self.builder.func);
            self.builder.ins().call(run_ref, &[task_id]);
            if output_ty == Type::Void {
                self.emit_pop_roots_n(1);
                self.gc_root_count -= 1;
                return self.builder.ins().iconst(types::I8, 0);
            }
            let result = self.builder.ins().load(
                clif_type(&output_ty),
                MemFlags::new(),
                frame,
                async_frame_slot_offset(FRAME_SLOT_RESULT),
            );
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
            return result;
        }

        let future = self.emit_expr(&await_expr.expr);
        let runtime_name = future_await_runtime_name(&output_ty);
        let fid = self.func_ids[runtime_name];
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        let call = self.builder.ins().call(fref, &[future]);
        self.builder.inst_results(call)[0]
    }

    /// Emit a call-await (`await <coop-leaf-call>`) as a suspend point
    /// (willow-lpn.5.3.1): call the callee constructor (which schedules the
    /// callee task and returns its frame), stash the frame in the awaiter's
    /// callee-frame slot, then `willow_sched_await(callee task id)`. If the
    /// callee already completed (returns 1) resume inline; otherwise store the
    /// resume state, return Pending, and resume when the scheduler wakes us. On
    /// resume, optionally read the callee's RESULT slot for let/assign/return.
    pub(super) fn emit_coop_call_await(
        &mut self,
        call: &CallExpr,
        await_span: crate::diagnostics::Span,
        bind: Option<(String, i32, Type)>,
        result_ty: Option<Type>,
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
    ) -> Option<cranelift_codegen::ir::Value> {
        // 1. callee = ctor(args): schedules the callee task, returns its frame.
        let modes = self.func_param_modes.get(&call.callee).cloned();
        let (arg_vals, arg_roots) =
            self.emit_call_args_rooted(Some(&call.callee), modes.as_deref(), None, &call.args);
        let ctor_fid = self.func_ids[&call.callee];
        let ctor_ref = self
            .module
            .declare_func_in_func(ctor_fid, self.builder.func);
        let c = self.builder.ins().call(ctor_ref, &arg_vals);
        let callee_frame = self.builder.inst_results(c)[0];
        if arg_roots > 0 {
            self.emit_pop_roots_n(arg_roots);
            self.gc_root_count -= arg_roots;
        }
        // 2. keep the callee frame alive across our suspension (frame-backed slot).
        let callee_off = self.async_frame_offsets[&await_span];
        self.builder
            .ins()
            .store(MemFlags::new(), callee_frame, frame, callee_off);
        // 3. id = callee[TASK_ID] (slot 1).
        let id = self.builder.ins().load(
            types::I64,
            MemFlags::new(),
            callee_frame,
            async_frame_slot_offset(FRAME_SLOT_TASK_ID),
        );
        // 4. done = willow_sched_await(id): 1 = already complete, 0 = registered.
        let await_fid = self.func_id("willow_sched_await");
        let await_ref = self
            .module
            .declare_func_in_func(await_fid, self.builder.func);
        let dcall = self.builder.ins().call(await_ref, &[id]);
        let done = self.builder.inst_results(dcall)[0];
        let resume_b = self.builder.create_block();
        let suspend_b = self.builder.create_block();
        let zero = self.builder.ins().iconst(types::I32, 0);
        let is_done = self.builder.ins().icmp(IntCC::NotEqual, done, zero);
        self.builder
            .ins()
            .brif(is_done, resume_b, &[], suspend_b, &[]);
        // suspend: record resume state (1-based index of resume_b), return Pending.
        self.builder.switch_to_block(suspend_b);
        let state = (suspends.len() + 1) as i64;
        let st = self.builder.ins().iconst(types::I64, state);
        self.builder.ins().store(MemFlags::new(), st, frame, 0i32);
        let pending = self.builder.ins().iconst(types::I32, 0);
        self.builder.ins().return_(&[pending]);
        suspends.push(resume_b);
        // resume (reached from the dispatch on wake AND the already-complete brif):
        // reload the callee frame, read its RESULT slot, bind.
        self.builder.switch_to_block(resume_b);
        let result_ty = bind.as_ref().map(|(_, _, ty)| ty.clone()).or(result_ty);
        let result = result_ty.map(|ty| {
            let callee2 = self
                .builder
                .ins()
                .load(types::I64, MemFlags::new(), frame, callee_off);
            self.builder.ins().load(
                clif_type(&ty),
                MemFlags::new(),
                callee2,
                async_frame_slot_offset(FRAME_SLOT_RESULT),
            )
        });
        if let Some((name, x_off, x_ty)) = bind {
            let result = result.expect("binding a call-await requires a result value");
            self.builder
                .ins()
                .store(MemFlags::new(), result, frame, x_off);
            self.vars.insert(
                name,
                VarStorage::Frame {
                    offset: x_off,
                    ty: x_ty,
                },
            );
        }
        result
    }

    /// Emit `await <task-expr>` inside a cooperative poll fn. The awaited task's
    /// frame has slot 1 = scheduler task id and slot 0 = result. If the task is
    /// incomplete, register the current task as a waiter, store the resume state,
    /// and return Pending; on resume, read the result from slot 0.
    pub(super) fn emit_coop_task_await(
        &mut self,
        task_expr: &Expr,
        await_span: crate::diagnostics::Span,
        bind: Option<(String, i32, Type)>,
        result_ty: Option<Type>,
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
    ) -> Option<cranelift_codegen::ir::Value> {
        let task_frame = self.emit_expr(task_expr);
        let stored_task_slot = self.async_frame_offsets.get(&await_span).copied();
        if let Some(off) = stored_task_slot {
            self.builder
                .ins()
                .store(MemFlags::new(), task_frame, frame, off);
        }

        let id = self.builder.ins().load(
            types::I64,
            MemFlags::new(),
            task_frame,
            async_frame_slot_offset(FRAME_SLOT_TASK_ID),
        );
        let await_fid = self.func_id("willow_sched_await");
        let await_ref = self
            .module
            .declare_func_in_func(await_fid, self.builder.func);
        let dcall = self.builder.ins().call(await_ref, &[id]);
        let done = self.builder.inst_results(dcall)[0];
        let resume_b = self.builder.create_block();
        let suspend_b = self.builder.create_block();
        let zero = self.builder.ins().iconst(types::I32, 0);
        let is_done = self.builder.ins().icmp(IntCC::NotEqual, done, zero);
        self.builder
            .ins()
            .brif(is_done, resume_b, &[], suspend_b, &[]);

        self.builder.switch_to_block(suspend_b);
        let state = (suspends.len() + 1) as i64;
        let st = self.builder.ins().iconst(types::I64, state);
        self.builder.ins().store(MemFlags::new(), st, frame, 0i32);
        let pending = self.builder.ins().iconst(types::I32, 0);
        self.builder.ins().return_(&[pending]);
        suspends.push(resume_b);

        self.builder.switch_to_block(resume_b);
        let task_frame = if let Some(off) = stored_task_slot {
            self.builder
                .ins()
                .load(types::I64, MemFlags::new(), frame, off)
        } else {
            self.emit_expr(task_expr)
        };
        let result_ty = bind.as_ref().map(|(_, _, ty)| ty.clone()).or(result_ty);
        let result = result_ty.and_then(|ty| {
            if ty == Type::Void {
                None
            } else {
                Some(self.builder.ins().load(
                    clif_type(&ty),
                    MemFlags::new(),
                    task_frame,
                    async_frame_slot_offset(FRAME_SLOT_RESULT),
                ))
            }
        });
        if let Some((name, x_off, x_ty)) = bind {
            let result = result.expect("binding a task-await requires a result value");
            self.builder
                .ins()
                .store(MemFlags::new(), result, frame, x_off);
            self.vars.insert(
                name,
                VarStorage::Frame {
                    offset: x_off,
                    ty: x_ty,
                },
            );
        }
        result
    }

    /// Emit a cooperative channel `recv` (willow-dsw) as a suspend point and
    /// return the received value. A check block (a resume target) probes
    /// `willow_channel_recv_ready`: if ready (a value is queued or the channel is
    /// closed) it reads the value via `willow_channel_recv_*`; otherwise it stored
    /// the running task as a channel waiter, so we record the resume state and
    /// return Pending. A `send`/`close` later wakes us and re-enters the check.
    pub(super) fn emit_coop_recv(
        &mut self,
        ch_expr: &Expr,
        elem_ty: &Type,
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let check_b = self.builder.create_block();
        self.builder.ins().jump(check_b, &[]);
        let state = (suspends.len() + 1) as i64;
        suspends.push(check_b);
        self.builder.switch_to_block(check_b);
        let ch = self.emit_expr(ch_expr);
        let ready_fid = self.func_id("willow_channel_recv_ready");
        let ready_ref = self
            .module
            .declare_func_in_func(ready_fid, self.builder.func);
        let rcall = self.builder.ins().call(ready_ref, &[ch]);
        let ready = self.builder.inst_results(rcall)[0];
        let get_b = self.builder.create_block();
        let suspend_b = self.builder.create_block();
        let zero = self.builder.ins().iconst(types::I32, 0);
        let is_ready = self.builder.ins().icmp(IntCC::NotEqual, ready, zero);
        self.builder
            .ins()
            .brif(is_ready, get_b, &[], suspend_b, &[]);
        // Not ready: we were registered as a channel waiter; record the resume
        // state (this check block) and return Pending.
        self.builder.switch_to_block(suspend_b);
        let st = self.builder.ins().iconst(types::I64, state);
        self.builder.ins().store(MemFlags::new(), st, frame, 0i32);
        let pending = self.builder.ins().iconst(types::I32, 0);
        self.builder.ins().return_(&[pending]);
        // Ready: read the value (present, or a default if the channel is closed).
        self.builder.switch_to_block(get_b);
        let recv_name = format!("willow_channel_recv_{}", channel_runtime_suffix(elem_ty));
        let recv_fid = self.func_ids[&recv_name];
        let recv_ref = self
            .module
            .declare_func_in_func(recv_fid, self.builder.func);
        let ch2 = self.emit_expr(ch_expr);
        let vcall = self.builder.ins().call(recv_ref, &[ch2]);
        self.builder.inst_results(vcall)[0]
    }

    /// Emit `willow_channel_unregister_waiter` for every recv-case channel of a
    /// select. Probing registers the running task on each not-ready recv channel;
    /// once a case is chosen the task must unregister from all of them so a later
    /// send/close does not spuriously wake the already-resumed task (willow-7aj).
    pub(super) fn emit_select_unregister_all(&mut self, sel: &SelectExpr) {
        let unreg_fid = self.func_id("willow_channel_unregister_waiter");
        for case in &sel.cases {
            if let SelectCaseKind::Recv { channel, .. } = &case.kind {
                let ch = self.emit_expr(channel);
                let unreg_ref = self
                    .module
                    .declare_func_in_func(unreg_fid, self.builder.func);
                self.builder.ins().call(unreg_ref, &[ch]);
            }
        }
    }

    /// Cooperative `select` as a suspend point (willow-7aj). Probe each case in
    /// source order: a recv case is ready when its channel has a value or is
    /// closed (`willow_channel_recv_ready` otherwise registers the running task
    /// as a waiter on that channel); a send case is always ready. When a case is
    /// ready, unregister from every recv channel and run that case body. When none
    /// is ready: run `default` if present, otherwise store the resume state and
    /// return Pending — a later send/close on any registered channel wakes the
    /// task, which re-enters the `check` block and re-probes. Case bodies are
    /// lowered cooperatively, so they may contain their own suspend points. The
    /// recv binding is frame-backed (keyed by the case span) so it survives those
    /// nested suspensions. Returns whether control falls through to the next stmt.
    pub(super) fn emit_coop_select(
        &mut self,
        sel: &SelectExpr,
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
        result_offset: Option<i32>,
    ) -> bool {
        let check_b = self.builder.create_block();
        self.builder.ins().jump(check_b, &[]);
        let state = (suspends.len() + 1) as i64;
        suspends.push(check_b);
        self.builder.switch_to_block(check_b);

        let done_b = self.builder.create_block();
        let exec_blocks: Vec<_> = sel
            .cases
            .iter()
            .map(|_| self.builder.create_block())
            .collect();
        let default_idx = sel
            .cases
            .iter()
            .position(|c| matches!(c.kind, SelectCaseKind::Default));

        // Probe chain: recv = conditional jump to its exec; send = always ready.
        let mut chain_ended = false;
        for (i, case) in sel.cases.iter().enumerate() {
            match &case.kind {
                SelectCaseKind::Recv { channel, .. } => {
                    let ch = self.emit_expr(channel);
                    let ready_fid = self.func_id("willow_channel_recv_ready");
                    let ready_ref = self
                        .module
                        .declare_func_in_func(ready_fid, self.builder.func);
                    let rcall = self.builder.ins().call(ready_ref, &[ch]);
                    let ready = self.builder.inst_results(rcall)[0];
                    let cont = self.builder.create_block();
                    let zero = self.builder.ins().iconst(types::I32, 0);
                    let is_ready = self.builder.ins().icmp(IntCC::NotEqual, ready, zero);
                    self.builder
                        .ins()
                        .brif(is_ready, exec_blocks[i], &[], cont, &[]);
                    self.builder.switch_to_block(cont);
                }
                SelectCaseKind::Send { .. } => {
                    self.builder.ins().jump(exec_blocks[i], &[]);
                    chain_ended = true;
                    break;
                }
                SelectCaseKind::Default => {}
            }
        }
        if !chain_ended {
            if let Some(di) = default_idx {
                self.builder.ins().jump(exec_blocks[di], &[]);
            } else {
                // Not ready: registered on all recv channels; suspend.
                let st = self.builder.ins().iconst(types::I64, state);
                self.builder.ins().store(MemFlags::new(), st, frame, 0i32);
                let pending = self.builder.ins().iconst(types::I32, 0);
                self.builder.ins().return_(&[pending]);
            }
        }

        // Exec blocks: unregister from all recv channels, then run the case body.
        let mut any_falls = false;
        for (i, case) in sel.cases.iter().enumerate() {
            self.builder.switch_to_block(exec_blocks[i]);
            let saved_vars = self.vars.clone();
            let saved_roots = self.gc_root_count;
            self.terminated = false;
            self.emit_select_unregister_all(sel);
            match &case.kind {
                SelectCaseKind::Recv { binding, channel } => {
                    let elem_ty =
                        channel_element_type(&self.ast_type_of(channel)).unwrap_or(Type::I64);
                    let ch = self.emit_expr(channel);
                    let recv_name =
                        format!("willow_channel_recv_{}", channel_runtime_suffix(&elem_ty));
                    let recv_fid = self.func_ids[&recv_name];
                    let recv_ref = self
                        .module
                        .declare_func_in_func(recv_fid, self.builder.func);
                    let vcall = self.builder.ins().call(recv_ref, &[ch]);
                    let v = self.builder.inst_results(vcall)[0];
                    if binding != "_" {
                        let off = self.async_frame_offsets[&case.span];
                        self.builder.ins().store(MemFlags::new(), v, frame, off);
                        self.vars.insert(
                            binding.clone(),
                            VarStorage::Frame {
                                offset: off,
                                ty: elem_ty,
                            },
                        );
                    }
                    let falls =
                        self.emit_coop_stmts(&case.body.stmts, suspends, frame, result_offset);
                    if falls {
                        self.builder.ins().jump(done_b, &[]);
                        any_falls = true;
                    }
                }
                SelectCaseKind::Send { channel, value } => {
                    let elem_ty =
                        channel_element_type(&self.ast_type_of(channel)).unwrap_or(Type::I64);
                    let ch = self.emit_expr(channel);
                    let val = self.emit_expr(value);
                    let send_name =
                        format!("willow_channel_send_{}", channel_runtime_suffix(&elem_ty));
                    let send_fid = self.func_ids[&send_name];
                    let send_ref = self
                        .module
                        .declare_func_in_func(send_fid, self.builder.func);
                    self.builder.ins().call(send_ref, &[ch, val]);
                    let falls =
                        self.emit_coop_stmts(&case.body.stmts, suspends, frame, result_offset);
                    if falls {
                        self.builder.ins().jump(done_b, &[]);
                        any_falls = true;
                    }
                }
                SelectCaseKind::Default => {
                    let falls =
                        self.emit_coop_stmts(&case.body.stmts, suspends, frame, result_offset);
                    if falls {
                        self.builder.ins().jump(done_b, &[]);
                        any_falls = true;
                    }
                }
            }
            self.vars = saved_vars;
            self.gc_root_count = saved_roots;
        }

        if any_falls {
            self.builder.switch_to_block(done_b);
            self.terminated = false;
            true
        } else {
            self.terminated = true;
            false
        }
    }

    fn await_contextual_task_expr<'e>(
        &self,
        expr: &'e Expr,
    ) -> Option<(&'e Expr, crate::diagnostics::Span, Type)> {
        // A direct call to a cooperative-leaf async fn uses the dedicated
        // call-await lowering (it avoids the type lookup). A direct call to a
        // NON-leaf async fn — e.g. an imported/item-imported one, absent from
        // `cooperative_leaves` — must still take the general cooperative
        // task-await so it suspends rather than block-driving the scheduler with
        // `willow_sched_run_until` (willow-0a6k.6).
        if is_leaf_call_await(expr, self.cooperative_leaves) {
            return None;
        }
        if let Expr::Await(a) = expr {
            let awaited_ty = self.ast_type_of(&a.expr);
            if let Some(output_ty) = task_output_type(&awaited_ty) {
                return Some((&a.expr, a.span, output_ty));
            }
        }
        None
    }

    /// Emit a statement sequence for a cooperative poll fn (willow-lpn.5.3 slice
    /// 5). Structured control flow (`if`/`while`) becomes Cranelift blocks; each
    /// `await sleep(n)` suspends (registers the timer, stores the resume state,
    /// returns Pending) and continues in a fresh resume block whose state is its
    /// 1-based index in `suspends`. A call-await (`await <coop-leaf-call>`) and a
    /// channel `recv()` are suspend points too. `return v` stores `v` at
    /// `result_offset` and returns Ready. Locals/params are frame-backed, so no
    /// block params are needed and all blocks are sealed together by the caller.
    pub(super) fn emit_coop_stmts(
        &mut self,
        stmts: &[Stmt],
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
        result_offset: Option<i32>,
    ) -> bool {
        for stmt in stmts {
            self.emit_coop_statement_safepoint(suspends, frame);
            let falls_through = match stmt {
                Stmt::Expr(es) if await_sleep_arg(&es.expr).is_some() => {
                    let arg = await_sleep_arg(&es.expr).unwrap().clone();
                    let n = self.emit_expr(&arg);
                    let sleep_fid = self.func_id("willow_sched_sleep");
                    let sleep_ref = self
                        .module
                        .declare_func_in_func(sleep_fid, self.builder.func);
                    self.builder.ins().call(sleep_ref, &[n]);
                    let state = (suspends.len() + 1) as i64;
                    let st = self.builder.ins().iconst(types::I64, state);
                    self.builder.ins().store(MemFlags::new(), st, frame, 0i32);
                    let pending = self.builder.ins().iconst(types::I32, 0);
                    self.builder.ins().return_(&[pending]);
                    let resume = self.builder.create_block();
                    suspends.push(resume);
                    self.builder.switch_to_block(resume);
                    true
                }
                Stmt::Expr(es) if is_await_yield(&es.expr) => {
                    let yield_fid = self.func_id("willow_sched_yield");
                    let yield_ref = self
                        .module
                        .declare_func_in_func(yield_fid, self.builder.func);
                    self.builder.ins().call(yield_ref, &[]);
                    let state = (suspends.len() + 1) as i64;
                    let st = self.builder.ins().iconst(types::I64, state);
                    self.builder.ins().store(MemFlags::new(), st, frame, 0i32);
                    let pending = self.builder.ins().iconst(types::I32, 0);
                    self.builder.ins().return_(&[pending]);
                    let resume = self.builder.create_block();
                    suspends.push(resume);
                    self.builder.switch_to_block(resume);
                    true
                }
                Stmt::Expr(es) if self.await_contextual_task_expr(&es.expr).is_some() => {
                    let (task_expr, await_span, _) =
                        self.await_contextual_task_expr(&es.expr).unwrap();
                    self.emit_coop_task_await(task_expr, await_span, None, None, suspends, frame);
                    true
                }
                Stmt::Let(l) if self.await_contextual_task_expr(&l.init).is_some() => {
                    let (task_expr, await_span, output_ty) =
                        self.await_contextual_task_expr(&l.init).unwrap();
                    let x_ty =
                        l.ty.clone()
                            .or_else(|| self.async_local_types.get(&l.span).cloned())
                            .unwrap_or_else(|| output_ty.clone());
                    let x_off = self.async_frame_offsets[&l.span];
                    self.emit_coop_task_await(
                        task_expr,
                        await_span,
                        Some((l.name.clone(), x_off, x_ty)),
                        None,
                        suspends,
                        frame,
                    );
                    true
                }
                Stmt::Assign(a) if self.await_contextual_task_expr(&a.value).is_some() => {
                    let (task_expr, await_span, _) =
                        self.await_contextual_task_expr(&a.value).unwrap();
                    if let Some(storage) = self.vars.get(&a.name).cloned() {
                        let target_ty = storage.ty().clone();
                        let result = self
                            .emit_coop_task_await(
                                task_expr,
                                await_span,
                                None,
                                Some(target_ty),
                                suspends,
                                frame,
                            )
                            .expect("assignment task-await requires a result value");
                        self.store_var(&storage, result);
                    }
                    true
                }
                Stmt::Expr(es) if await_coop_call(&es.expr, self.cooperative_leaves).is_some() => {
                    let (call, await_span) =
                        await_coop_call(&es.expr, self.cooperative_leaves).unwrap();
                    self.emit_coop_call_await(call, await_span, None, None, suspends, frame);
                    true
                }
                Stmt::Let(l) if await_coop_call(&l.init, self.cooperative_leaves).is_some() => {
                    let (call, await_span) =
                        await_coop_call(&l.init, self.cooperative_leaves).unwrap();
                    let x_ty =
                        l.ty.clone()
                            .or_else(|| self.async_local_types.get(&l.span).cloned())
                            .unwrap_or(Type::I64);
                    let x_off = self.async_frame_offsets[&l.span];
                    self.emit_coop_call_await(
                        call,
                        await_span,
                        Some((l.name.clone(), x_off, x_ty)),
                        None,
                        suspends,
                        frame,
                    );
                    true
                }
                Stmt::Assign(a) if await_coop_call(&a.value, self.cooperative_leaves).is_some() => {
                    let (call, await_span) =
                        await_coop_call(&a.value, self.cooperative_leaves).unwrap();
                    if let Some(storage) = self.vars.get(&a.name).cloned() {
                        let target_ty = storage.ty().clone();
                        let result = self
                            .emit_coop_call_await(
                                call,
                                await_span,
                                None,
                                Some(target_ty),
                                suspends,
                                frame,
                            )
                            .expect("assignment call-await requires a result value");
                        self.store_var(&storage, result);
                    }
                    true
                }
                Stmt::Let(l) if is_channel_recv(&l.init).is_some() => {
                    let m = is_channel_recv(&l.init).unwrap();
                    let elem_ty =
                        channel_element_type(&self.ast_type_of(&m.object)).unwrap_or(Type::I64);
                    let v = self.emit_coop_recv(&m.object, &elem_ty, suspends, frame);
                    let x_off = self.async_frame_offsets[&l.span];
                    let x_ty =
                        l.ty.clone()
                            .or_else(|| self.async_local_types.get(&l.span).cloned())
                            .unwrap_or_else(|| elem_ty.clone());
                    self.builder.ins().store(MemFlags::new(), v, frame, x_off);
                    self.vars.insert(
                        l.name.clone(),
                        VarStorage::Frame {
                            offset: x_off,
                            ty: x_ty,
                        },
                    );
                    true
                }
                Stmt::Assign(a) if is_channel_recv(&a.value).is_some() => {
                    let m = is_channel_recv(&a.value).unwrap();
                    let elem_ty =
                        channel_element_type(&self.ast_type_of(&m.object)).unwrap_or(Type::I64);
                    let v = self.emit_coop_recv(&m.object, &elem_ty, suspends, frame);
                    if let Some(storage) = self.vars.get(&a.name).cloned() {
                        self.store_var(&storage, v);
                    }
                    true
                }
                Stmt::Expr(es) if is_channel_recv(&es.expr).is_some() => {
                    let m = is_channel_recv(&es.expr).unwrap();
                    let elem_ty =
                        channel_element_type(&self.ast_type_of(&m.object)).unwrap_or(Type::I64);
                    self.emit_coop_recv(&m.object, &elem_ty, suspends, frame);
                    true
                }
                Stmt::Expr(es) if matches!(&es.expr, Expr::Select(_)) => {
                    let Expr::Select(sel) = &es.expr else {
                        unreachable!("guard matched Select");
                    };
                    self.emit_coop_select(sel, suspends, frame, result_offset)
                }
                Stmt::FieldAssign(s) if self.await_contextual_task_expr(&s.value).is_some() => {
                    let (task_expr, await_span, value_ty) =
                        self.await_contextual_task_expr(&s.value).unwrap();
                    let obj_type = self.ast_type_of(&s.object);
                    if let Some(class_name) = class_name_for_object_type(&obj_type)
                        && let Some(layout) = self.class_layouts.get(&class_name).cloned()
                        && let Some(idx) = layout.iter().position(|(n, _)| n == &s.field)
                    {
                        let field_ty = layout[idx].1.clone();
                        let result = self
                            .emit_coop_task_await(
                                task_expr,
                                await_span,
                                None,
                                Some(value_ty.clone()),
                                suspends,
                                frame,
                            )
                            .expect("field assignment task-await requires a result value");

                        let ptr = self.emit_expr(&s.object);
                        self.emit_push_root(ptr);
                        if self.build_mode == BuildMode::Debug {
                            self.emit_nil_check(ptr, s.object.span(), &s.field);
                        }

                        let val = self.coerce_to_target(result, &value_ty, &field_ty);
                        let offset = (idx as i32 + 1) * 8;
                        self.builder.ins().store(MemFlags::new(), val, ptr, offset);

                        self.emit_pop_roots_n(1);
                        self.gc_root_count -= 1;
                    } else {
                        self.emit_coop_task_await(
                            task_expr, await_span, None, None, suspends, frame,
                        );
                    }
                    true
                }
                Stmt::FieldAssign(s)
                    if await_coop_call(&s.value, self.cooperative_leaves).is_some() =>
                {
                    let (call, await_span) =
                        await_coop_call(&s.value, self.cooperative_leaves).unwrap();
                    let obj_type = self.ast_type_of(&s.object);
                    if let Some(class_name) = class_name_for_object_type(&obj_type)
                        && let Some(layout) = self.class_layouts.get(&class_name).cloned()
                        && let Some(idx) = layout.iter().position(|(n, _)| n == &s.field)
                    {
                        let field_ty = layout[idx].1.clone();
                        let value_ty = self.ast_type_of(&s.value);
                        let result = self
                            .emit_coop_call_await(
                                call,
                                await_span,
                                None,
                                Some(value_ty.clone()),
                                suspends,
                                frame,
                            )
                            .expect("field assignment call-await requires a result value");

                        let ptr = self.emit_expr(&s.object);
                        self.emit_push_root(ptr);
                        if self.build_mode == BuildMode::Debug {
                            self.emit_nil_check(ptr, s.object.span(), &s.field);
                        }

                        let val = self.coerce_to_target(result, &value_ty, &field_ty);
                        let offset = (idx as i32 + 1) * 8;
                        self.builder.ins().store(MemFlags::new(), val, ptr, offset);

                        self.emit_pop_roots_n(1);
                        self.gc_root_count -= 1;
                    } else {
                        self.emit_coop_call_await(call, await_span, None, None, suspends, frame);
                    }
                    true
                }
                Stmt::IndexAssign(s) if self.await_contextual_task_expr(&s.value).is_some() => {
                    let (task_expr, await_span, value_ty) =
                        self.await_contextual_task_expr(&s.value).unwrap();
                    let elem_ty = array_element_type(&self.ast_type_of(&s.array));
                    let result = self
                        .emit_coop_task_await(
                            task_expr,
                            await_span,
                            None,
                            Some(value_ty.clone()),
                            suspends,
                            frame,
                        )
                        .expect("index assignment task-await requires a result value");

                    let arr = self.emit_expr(&s.array);
                    self.emit_push_root(arr);
                    let idx = self.emit_expr(&s.index);
                    let val = self.coerce_to_target(result, &value_ty, &elem_ty);
                    let word = self.coerce_to_i64(val, &elem_ty);
                    let set_id = self.func_id("willow_array_set");
                    let set_ref = self.module.declare_func_in_func(set_id, self.builder.func);
                    self.builder.ins().call(set_ref, &[arr, idx, word]);

                    self.emit_pop_roots_n(1);
                    self.gc_root_count -= 1;
                    true
                }
                Stmt::IndexAssign(s)
                    if await_coop_call(&s.value, self.cooperative_leaves).is_some() =>
                {
                    let (call, await_span) =
                        await_coop_call(&s.value, self.cooperative_leaves).unwrap();
                    let elem_ty = array_element_type(&self.ast_type_of(&s.array));
                    let value_ty = self.ast_type_of(&s.value);
                    let result = self
                        .emit_coop_call_await(
                            call,
                            await_span,
                            None,
                            Some(value_ty.clone()),
                            suspends,
                            frame,
                        )
                        .expect("index assignment call-await requires a result value");

                    let arr = self.emit_expr(&s.array);
                    self.emit_push_root(arr);
                    let idx = self.emit_expr(&s.index);
                    let val = self.coerce_to_target(result, &value_ty, &elem_ty);
                    let word = self.coerce_to_i64(val, &elem_ty);
                    let set_id = self.func_id("willow_array_set");
                    let set_ref = self.module.declare_func_in_func(set_id, self.builder.func);
                    self.builder.ins().call(set_ref, &[arr, idx, word]);

                    self.emit_pop_roots_n(1);
                    self.gc_root_count -= 1;
                    true
                }
                Stmt::Return(r)
                    if r.value
                        .as_ref()
                        .and_then(|value| self.await_contextual_task_expr(value))
                        .is_some() =>
                {
                    let value = r.value.as_ref().unwrap();
                    let (task_expr, await_span, result_ty) = self
                        .await_contextual_task_expr(value)
                        .expect("return task-await guard matched");
                    let result = self.emit_coop_task_await(
                        task_expr,
                        await_span,
                        None,
                        Some(result_ty),
                        suspends,
                        frame,
                    );
                    if let (Some(off), Some(result)) = (result_offset, result) {
                        self.builder
                            .ins()
                            .store(MemFlags::new(), result, frame, off);
                    }
                    let ready = self.builder.ins().iconst(types::I32, 1);
                    self.builder.ins().return_(&[ready]);
                    self.terminated = true;
                    false
                }
                Stmt::Return(r)
                    if r.value
                        .as_ref()
                        .and_then(|value| await_coop_call(value, self.cooperative_leaves))
                        .is_some() =>
                {
                    let value = r.value.as_ref().unwrap();
                    let (call, await_span) = await_coop_call(value, self.cooperative_leaves)
                        .expect("return call-await guard matched");
                    let result_ty = self.ast_type_of(value);
                    let result = self
                        .emit_coop_call_await(
                            call,
                            await_span,
                            None,
                            Some(result_ty),
                            suspends,
                            frame,
                        )
                        .expect("return call-await requires a result value");
                    if let Some(off) = result_offset {
                        self.builder
                            .ins()
                            .store(MemFlags::new(), result, frame, off);
                    }
                    let ready = self.builder.ins().iconst(types::I32, 1);
                    self.builder.ins().return_(&[ready]);
                    self.terminated = true;
                    false
                }
                Stmt::Return(r) => {
                    if let (Some(off), Some(v)) = (result_offset, &r.value) {
                        let val = self.emit_expr(v);
                        self.builder.ins().store(MemFlags::new(), val, frame, off);
                    }
                    let ready = self.builder.ins().iconst(types::I32, 1);
                    self.builder.ins().return_(&[ready]);
                    self.terminated = true;
                    false
                }
                Stmt::If(s) => {
                    let cond = self.emit_expr(&s.cond);
                    let then_b = self.builder.create_block();
                    let else_b = self.builder.create_block();
                    let join_b = self.builder.create_block();
                    self.builder.ins().brif(cond, then_b, &[], else_b, &[]);
                    self.builder.switch_to_block(then_b);
                    self.terminated = false;
                    let saved_vars = self.vars.clone();
                    let saved_roots = self.gc_root_count;
                    let then_falls =
                        self.emit_coop_stmts(&s.then_block.stmts, suspends, frame, result_offset);
                    if then_falls {
                        self.builder.ins().jump(join_b, &[]);
                    }
                    self.vars = saved_vars.clone();
                    self.gc_root_count = saved_roots;

                    self.builder.switch_to_block(else_b);
                    self.terminated = false;
                    let else_falls = if let Some(eb) = &s.else_block {
                        self.emit_coop_stmts(&eb.stmts, suspends, frame, result_offset)
                    } else {
                        true
                    };
                    if else_falls {
                        self.builder.ins().jump(join_b, &[]);
                    }
                    self.vars = saved_vars;
                    self.gc_root_count = saved_roots;

                    if then_falls || else_falls {
                        self.builder.switch_to_block(join_b);
                        self.terminated = false;
                        true
                    } else {
                        self.terminated = true;
                        false
                    }
                }
                Stmt::While(s) => {
                    let header = self.builder.create_block();
                    let body_b = self.builder.create_block();
                    let exit_b = self.builder.create_block();
                    self.builder.ins().jump(header, &[]);
                    self.builder.switch_to_block(header);
                    let cond = self.emit_expr(&s.cond);
                    self.builder.ins().brif(cond, body_b, &[], exit_b, &[]);
                    self.builder.switch_to_block(body_b);
                    self.terminated = false;
                    let saved_vars = self.vars.clone();
                    let saved_roots = self.gc_root_count;
                    let body_falls =
                        self.emit_coop_stmts(&s.body.stmts, suspends, frame, result_offset);
                    if body_falls {
                        self.emit_coop_safepoint_to(suspends, frame, header);
                    }
                    self.vars = saved_vars;
                    self.gc_root_count = saved_roots;
                    self.builder.switch_to_block(exit_b);
                    self.terminated = false;
                    true
                }
                Stmt::For(s) => self.emit_coop_for(s, suspends, frame, result_offset),
                _ => {
                    self.emit_stmt(stmt);
                    !self.terminated
                }
            };

            if !falls_through {
                return false;
            }
        }
        true
    }

    pub(super) fn emit_coop_for(
        &mut self,
        s: &ForStmt,
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
        result_offset: Option<i32>,
    ) -> bool {
        if let Expr::Range(range) = &s.iterable {
            return self.emit_coop_range_for(s, range, suspends, frame, result_offset);
        }
        // A `Range<i64>` held as a value (variable/call), not an inline literal.
        if matches!(self.ast_type_of(&s.iterable), Type::Generic(ref n, _) if n == "Range") {
            return self.emit_coop_range_for_value(s, suspends, frame, result_offset);
        }

        let iterable_ty = self
            .async_local_types
            .get(&s.iter_frame_key())
            .cloned()
            .unwrap_or_else(|| self.ast_type_of(&s.iterable));
        let elem_ty = self
            .async_local_types
            .get(&s.name_span)
            .cloned()
            .unwrap_or_else(|| array_element_type(&iterable_ty));
        let iter_off = self.async_frame_offsets[&s.iter_frame_key()];
        let index_off = self.async_frame_offsets[&s.index_frame_key()];
        let item_off = (s.name != "_").then(|| self.async_frame_offsets[&s.name_span]);

        let arr = self.emit_expr(&s.iterable);
        self.builder
            .ins()
            .store(MemFlags::new(), arr, frame, iter_off);
        let zero = self.builder.ins().iconst(types::I64, 0);
        self.builder
            .ins()
            .store(MemFlags::new(), zero, frame, index_off);

        let header = self.builder.create_block();
        let body_b = self.builder.create_block();
        let exit_b = self.builder.create_block();
        self.builder.ins().jump(header, &[]);

        self.builder.switch_to_block(header);
        let arr =
            self.builder
                .ins()
                .load(clif_type(&iterable_ty), MemFlags::new(), frame, iter_off);
        let idx = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), frame, index_off);
        let len_id = self.func_id("willow_array_len");
        let len_ref = self.module.declare_func_in_func(len_id, self.builder.func);
        let len_call = self.builder.ins().call(len_ref, &[arr]);
        let len = self.builder.inst_results(len_call)[0];
        let keep_going = self.builder.ins().icmp(IntCC::SignedLessThan, idx, len);
        self.builder
            .ins()
            .brif(keep_going, body_b, &[], exit_b, &[]);

        self.builder.switch_to_block(body_b);
        self.terminated = false;
        let saved_vars = self.vars.clone();
        let saved_roots = self.gc_root_count;
        if let Some(off) = item_off {
            let get_id = self.func_id("willow_array_get");
            let get_ref = self.module.declare_func_in_func(get_id, self.builder.func);
            let call = self.builder.ins().call(get_ref, &[arr, idx]);
            let word = self.builder.inst_results(call)[0];
            let item = self.coerce_i64_to(word, &elem_ty);
            self.builder.ins().store(MemFlags::new(), item, frame, off);
            self.vars.insert(
                s.name.clone(),
                VarStorage::Frame {
                    offset: off,
                    ty: elem_ty,
                },
            );
        }

        let body_falls = self.emit_coop_stmts(&s.body.stmts, suspends, frame, result_offset);
        if body_falls {
            let idx = self
                .builder
                .ins()
                .load(types::I64, MemFlags::new(), frame, index_off);
            let one = self.builder.ins().iconst(types::I64, 1);
            let next = self.builder.ins().iadd(idx, one);
            self.builder
                .ins()
                .store(MemFlags::new(), next, frame, index_off);
            self.emit_coop_safepoint_to(suspends, frame, header);
        }
        self.vars = saved_vars;
        self.gc_root_count = saved_roots;
        self.builder.switch_to_block(exit_b);
        self.terminated = false;
        true
    }

    pub(super) fn emit_coop_range_for(
        &mut self,
        s: &ForStmt,
        range: &RangeExpr,
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
        result_offset: Option<i32>,
    ) -> bool {
        let start = self.emit_expr(&range.start);
        let end = self.emit_expr(&range.end);
        self.emit_coop_range_for_bounds(s, start, end, suspends, frame, result_offset)
    }

    /// Cooperative `for` over a `Range<i64>` VALUE: load its bounds from the heap
    /// object, then drive the same frame-backed counting loop as the literal
    /// form (the bounds are copied into I64 frame slots, so they survive awaits).
    pub(super) fn emit_coop_range_for_value(
        &mut self,
        s: &ForStmt,
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
        result_offset: Option<i32>,
    ) -> bool {
        let ptr = self.emit_expr(&s.iterable);
        let start = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 0i32);
        let end = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), ptr, 8i32);
        self.emit_coop_range_for_bounds(s, start, end, suspends, frame, result_offset)
    }

    pub(super) fn emit_coop_range_for_bounds(
        &mut self,
        s: &ForStmt,
        start: cranelift_codegen::ir::Value,
        end: cranelift_codegen::ir::Value,
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
        result_offset: Option<i32>,
    ) -> bool {
        let end_off = self.async_frame_offsets[&s.iter_frame_key()];
        let current_off = self.async_frame_offsets[&s.index_frame_key()];
        let item_off = (s.name != "_").then(|| self.async_frame_offsets[&s.name_span]);

        self.builder
            .ins()
            .store(MemFlags::new(), start, frame, current_off);
        self.builder
            .ins()
            .store(MemFlags::new(), end, frame, end_off);

        let header = self.builder.create_block();
        let body_b = self.builder.create_block();
        let exit_b = self.builder.create_block();
        self.builder.ins().jump(header, &[]);

        self.builder.switch_to_block(header);
        let current = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), frame, current_off);
        let end = self
            .builder
            .ins()
            .load(types::I64, MemFlags::new(), frame, end_off);
        let keep_going = self.builder.ins().icmp(IntCC::SignedLessThan, current, end);
        self.builder
            .ins()
            .brif(keep_going, body_b, &[], exit_b, &[]);

        self.builder.switch_to_block(body_b);
        self.terminated = false;
        let saved_vars = self.vars.clone();
        let saved_roots = self.gc_root_count;
        if let Some(off) = item_off {
            self.builder
                .ins()
                .store(MemFlags::new(), current, frame, off);
            self.vars.insert(
                s.name.clone(),
                VarStorage::Frame {
                    offset: off,
                    ty: Type::I64,
                },
            );
        }

        let body_falls = self.emit_coop_stmts(&s.body.stmts, suspends, frame, result_offset);
        if body_falls {
            let current = self
                .builder
                .ins()
                .load(types::I64, MemFlags::new(), frame, current_off);
            let one = self.builder.ins().iconst(types::I64, 1);
            let next = self.builder.ins().iadd(current, one);
            self.builder
                .ins()
                .store(MemFlags::new(), next, frame, current_off);
            self.emit_coop_safepoint_to(suspends, frame, header);
        }
        self.vars = saved_vars;
        self.gc_root_count = saved_roots;
        self.builder.switch_to_block(exit_b);
        self.terminated = false;
        true
    }

    /// Eager (block-driving) `select` (willow-7aj): probe each case in source
    /// order — a recv case is ready when its channel has a value or is closed; a
    /// send case (unbounded channel) is always ready. The first ready case runs.
    /// If none is ready and there is a `default`, it runs; otherwise the scheduler
    /// is driven and the probe retried (giving up if no task could progress). In a
    /// non-task context recv_ready does not register a waiter (current task is 0),
    /// so it is a pure readiness probe here.
    pub(super) fn emit_select(&mut self, s: &SelectExpr) {
        let loop_b = self.builder.create_block();
        let done_b = self.builder.create_block();
        let case_blocks: Vec<_> = s
            .cases
            .iter()
            .map(|_| self.builder.create_block())
            .collect();
        let mut cont_blocks = Vec::new();
        let default_idx = s
            .cases
            .iter()
            .position(|c| matches!(c.kind, SelectCaseKind::Default));

        self.builder.ins().jump(loop_b, &[]);
        self.builder.switch_to_block(loop_b);
        let mut chain_ended = false;
        for (i, case) in s.cases.iter().enumerate() {
            match &case.kind {
                SelectCaseKind::Recv { channel, .. } => {
                    let ch = self.emit_expr(channel);
                    let ready_fid = self.func_id("willow_channel_recv_ready");
                    let ready_ref = self
                        .module
                        .declare_func_in_func(ready_fid, self.builder.func);
                    let rcall = self.builder.ins().call(ready_ref, &[ch]);
                    let ready = self.builder.inst_results(rcall)[0];
                    let cont = self.builder.create_block();
                    cont_blocks.push(cont);
                    let zero = self.builder.ins().iconst(types::I32, 0);
                    let is_ready = self.builder.ins().icmp(IntCC::NotEqual, ready, zero);
                    self.builder
                        .ins()
                        .brif(is_ready, case_blocks[i], &[], cont, &[]);
                    self.builder.switch_to_block(cont);
                }
                SelectCaseKind::Send { .. } => {
                    self.builder.ins().jump(case_blocks[i], &[]);
                    chain_ended = true;
                    break;
                }
                SelectCaseKind::Default => {}
            }
        }
        if !chain_ended {
            if let Some(di) = default_idx {
                self.builder.ins().jump(case_blocks[di], &[]);
            } else {
                let run_fid = self.func_id("willow_sched_run");
                let run_ref = self.module.declare_func_in_func(run_fid, self.builder.func);
                let rcall = self.builder.ins().call(run_ref, &[]);
                let completed = self.builder.inst_results(rcall)[0];
                let zero = self.builder.ins().iconst(types::I64, 0);
                let progressed = self.builder.ins().icmp(IntCC::NotEqual, completed, zero);
                self.builder
                    .ins()
                    .brif(progressed, loop_b, &[], done_b, &[]);
            }
        }

        for (i, case) in s.cases.iter().enumerate() {
            self.builder.switch_to_block(case_blocks[i]);
            let saved_vars = self.vars.clone();
            let saved_roots = self.gc_root_count;
            self.terminated = false;
            match &case.kind {
                SelectCaseKind::Recv { binding, channel } => {
                    let elem_ty =
                        channel_element_type(&self.ast_type_of(channel)).unwrap_or(Type::I64);
                    let ch = self.emit_expr(channel);
                    let recv_name =
                        format!("willow_channel_recv_{}", channel_runtime_suffix(&elem_ty));
                    let recv_fid = self.func_ids[&recv_name];
                    let recv_ref = self
                        .module
                        .declare_func_in_func(recv_fid, self.builder.func);
                    let vcall = self.builder.ins().call(recv_ref, &[ch]);
                    let v = self.builder.inst_results(vcall)[0];
                    if binding != "_" {
                        let storage = self.create_local_stack_slot(&elem_ty, v);
                        self.vars.insert(binding.clone(), storage);
                    }
                    self.emit_block(&case.body);
                }
                SelectCaseKind::Send { channel, value } => {
                    let elem_ty =
                        channel_element_type(&self.ast_type_of(channel)).unwrap_or(Type::I64);
                    let ch = self.emit_expr(channel);
                    let val = self.emit_expr(value);
                    let send_name =
                        format!("willow_channel_send_{}", channel_runtime_suffix(&elem_ty));
                    let send_fid = self.func_ids[&send_name];
                    let send_ref = self
                        .module
                        .declare_func_in_func(send_fid, self.builder.func);
                    self.builder.ins().call(send_ref, &[ch, val]);
                    self.emit_block(&case.body);
                }
                SelectCaseKind::Default => self.emit_block(&case.body),
            }
            if !self.terminated {
                self.builder.ins().jump(done_b, &[]);
            }
            self.vars = saved_vars;
            self.gc_root_count = saved_roots;
        }

        self.builder.seal_block(loop_b);
        for c in &cont_blocks {
            self.builder.seal_block(*c);
        }
        for c in &case_blocks {
            self.builder.seal_block(*c);
        }
        self.builder.seal_block(done_b);
        self.builder.switch_to_block(done_b);
        self.terminated = false;
    }
}
