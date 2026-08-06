//! Async / cooperative-scheduler codegen for the Cranelift backend (extracted
//! from `mod.rs`): cooperative `main`/leaf compilation and the await / coop
//! statement / select / range-for lowering. `pub(super)` so the main codegen
//! driver can call them; child-module access reaches private FuncGen/Codegen
//! state.

use anyhow::Result;
use cranelift_codegen::ir::{InstBuilder, MemFlagsData, condcodes::IntCC, types};
use cranelift_module::Module;

use super::*;

/// One `await <task>` / `await <task>.result()` site in a cooperative poll fn
/// (willow-qrj9). Both forms wait on the SAME task and the same frame; only
/// the cancelled mapping differs, so they share one lowering and differ by
/// `cancel_aware`.
pub(super) struct TaskAwaitSite<'e> {
    /// Expression the task frame comes from. For `await t.result()` this is
    /// the receiver `t`, not the `result()` call: `result()` is an identity
    /// view, so the frame slot and the resume reload both key off `t`.
    task_expr: &'e Expr,
    await_span: crate::diagnostics::Span,
    /// The task's own output type `T`.
    task_result_ty: Type,
    /// `true` for `await t.result()`, whose value is `Result<T, Cancelled>`.
    cancel_aware: bool,
}

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
        self.set_coop_live_spans(&f.params, &f.body);
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
        // Cancellation cleanup entry (willow-vynv.3), defined after the poll
        // fn (it needs the defer sites collected while emitting the body).
        let cancel_symbol = format!("{name}__coop_cancel");
        let mut cancel_sig = self.module.make_signature();
        cancel_sig.params.push(AbiParam::new(types::I64));
        let cancel_fid =
            self.module
                .declare_function(&cancel_symbol, Linkage::Local, &cancel_sig)?;

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
        self.set_coop_live_spans(&f.params, &f.body);
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
        let barrier_fid = self.func_id("willow_gc_write_barrier");
        let barrier_ref = self.module.declare_func_in_func(barrier_fid, builder.func);
        let sc = builder.ins().iconst(types::I64, slot_count);
        let mk = builder.ins().iconst(types::I64, mask);
        let call = builder.ins().call(alloc_ref, &[sc, mk]);
        let frame = builder.inst_results(call)[0];
        // Store args into their param slots (slots 2..) before spawning (no
        // allocation happens between alloc and spawn, so the unrooted frame is
        // safe).
        for (i, p) in f.params.iter().enumerate() {
            let arg = builder.block_params(entry)[i];
            let off = async_frame_slot_offset(2 + i);
            emit_gc_heap_store_raw(
                &mut builder,
                is_gc_managed(&p.ty, &self.enum_infos).then_some(barrier_ref),
                frame,
                off,
                arg,
                GcStoreDestination::AsyncFrameSlot,
                MemFlagsData::trusted(),
            );
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
            .store(MemFlagsData::trusted(), task_id, frame, task_id_offset);
        // Attach the cancellation cleanup entry (willow-vynv.3).
        let cancel_ref = self.module.declare_func_in_func(cancel_fid, builder.func);
        let cancel_addr = builder.ins().func_addr(types::I64, cancel_ref);
        let set_fid = self.func_id("willow_sched_set_cancel_fn");
        let set_ref = self.module.declare_func_in_func(set_fid, builder.func);
        builder.ins().call(set_ref, &[task_id, cancel_addr]);
        builder.ins().return_(&[frame]);
        builder.finalize();
        self.module
            .define_function(ctor_fid, &mut ctx)
            .map_err(|e| {
                if std::env::var("WILLOW_VERIFY_DEBUG").is_ok() {
                    eprintln!("[verify] {e:?}");
                }
                e
            })?;
        self.module.clear_context(&mut ctx);

        // Poll fn = the state machine; params are bound from their frame slots
        // and locals are frame-backed via `offsets`.
        let sites = self.compile_coop_main_poll(
            &poll_symbol,
            f,
            offsets,
            Some(result_offset),
            &param_bindings,
            None,
        )?;
        self.compile_async_cancel_fn(cancel_fid, &sites, &f.return_type)?;
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
        // Cancellation cleanup entry (willow-vynv.3).
        let cancel_symbol = format!("{mangled}__coop_cancel");
        let mut cancel_sig = self.module.make_signature();
        cancel_sig.params.push(AbiParam::new(types::I64));
        let cancel_fid =
            self.module
                .declare_function(&cancel_symbol, Linkage::Local, &cancel_sig)?;

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
        self.set_coop_live_spans(&m.params, &m.body);
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
        let barrier_fid = self.func_id("willow_gc_write_barrier");
        let barrier_ref = self.module.declare_func_in_func(barrier_fid, builder.func);
        let sc = builder.ins().iconst(types::I64, slot_count);
        let mk = builder.ins().iconst(types::I64, mask);
        let call = builder.ins().call(alloc_ref, &[sc, mk]);
        let frame = builder.inst_results(call)[0];

        if let Some(offset) = self_offset {
            let self_arg = builder.block_params(entry)[0];
            emit_gc_heap_store_raw(
                &mut builder,
                Some(barrier_ref),
                frame,
                offset,
                self_arg,
                GcStoreDestination::AsyncFrameSlot,
                MemFlagsData::trusted(),
            );
        }
        for (i, p) in m.params.iter().enumerate() {
            let arg = builder.block_params(entry)[i + 1];
            let off = async_frame_slot_offset(first_param_slot + i);
            emit_gc_heap_store_raw(
                &mut builder,
                is_gc_managed(&p.ty, &self.enum_infos).then_some(barrier_ref),
                frame,
                off,
                arg,
                GcStoreDestination::AsyncFrameSlot,
                MemFlagsData::trusted(),
            );
        }

        let poll_ref = self.module.declare_func_in_func(poll_fid, builder.func);
        let poll_addr = builder.ins().func_addr(types::I64, poll_ref);
        let spawn_fid = self.func_id("willow_sched_spawn");
        let spawn_ref = self.module.declare_func_in_func(spawn_fid, builder.func);
        let spawn_call = builder.ins().call(spawn_ref, &[poll_addr, frame]);
        let task_id = builder.inst_results(spawn_call)[0];
        builder
            .ins()
            .store(MemFlagsData::trusted(), task_id, frame, task_id_offset);
        // Attach the cancellation cleanup entry (willow-vynv.3).
        let cancel_ref = self.module.declare_func_in_func(cancel_fid, builder.func);
        let cancel_addr = builder.ins().func_addr(types::I64, cancel_ref);
        let set_fid = self.func_id("willow_sched_set_cancel_fn");
        let set_ref = self.module.declare_func_in_func(set_fid, builder.func);
        builder.ins().call(set_ref, &[task_id, cancel_addr]);
        builder.ins().return_(&[frame]);
        builder.finalize();
        self.module
            .define_function(ctor_fid, &mut ctx)
            .map_err(|e| {
                if std::env::var("WILLOW_VERIFY_DEBUG").is_ok() {
                    eprintln!("[verify] {e:?}");
                }
                e
            })?;
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
        let sites = self.compile_coop_main_poll(
            &poll_symbol,
            &poll_decl,
            offsets,
            Some(result_offset),
            &param_bindings,
            Some(class_name),
        )?;
        self.compile_async_cancel_fn(cancel_fid, &sites, &m.return_type)?;
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
        let barrier_fid = self.func_id("willow_gc_write_barrier");
        let barrier_ref = self.module.declare_func_in_func(barrier_fid, builder.func);
        let slot_count_v = builder.ins().iconst(types::I64, slot_count);
        let mask_v = builder.ins().iconst(types::I64, mask);
        let call = builder.ins().call(alloc_ref, &[slot_count_v, mask_v]);
        let frame = builder.inst_results(call)[0];

        if let Some(param) = params.first() {
            let arr_id = self.func_id("willow_runtime_args_array");
            let arr_ref = self.module.declare_func_in_func(arr_id, builder.func);
            let arr_call = builder.ins().call(arr_ref, &[]);
            let arr = builder.inst_results(arr_call)[0];
            emit_gc_heap_store_raw(
                &mut builder,
                Some(barrier_ref),
                frame,
                async_frame_slot_offset(FRAME_SLOT_RESULT),
                arr,
                GcStoreDestination::AsyncFrameSlot,
                MemFlagsData::trusted(),
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
        // unawaited task to quiescence (which could hang on a non-terminating
        // background task). Programs that need a result await their tasks before main
        // returns, so nothing is left to run anyway.
        let run_fid = self.func_id("willow_sched_run_until");
        let run_ref = self.module.declare_func_in_func(run_fid, builder.func);
        builder.ins().call(run_ref, &[main_task_id]);

        builder.ins().return_(&[]);
        builder.finalize();
        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| {
                if std::env::var("WILLOW_VERIFY_DEBUG").is_ok() {
                    eprintln!("[verify] {e:?}");
                }
                e
            })?;
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
    ) -> Result<Vec<AsyncDeferSite>> {
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
        let defer_sites: Vec<AsyncDeferSite>;
        {
            let mut fg = FuncGen {
                builder: &mut builder,
                loop_stack: Vec::new(),
                defer_stack: Vec::new(),
                defer_counter: 0,
                collected_defer_sites: Vec::new(),
                module: &mut self.module,
                gc_tlab_state: self.gc_tlab_state,
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
                expr_types: &self.expr_types,
                coop_frame: None,
                coop_result_offset: None,
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
            fg.coop_frame = Some(frame);
            fg.coop_result_offset = result_offset;
            // Async defer v1 (willow-vynv.3): ONE function-level scope frame —
            // the coop path does not run emit_block's scope machinery, so all
            // defers flush at function exit (return/?/fallthrough); the frame
            // FLAGS make cancellation cleanup exact regardless.
            fg.defer_stack.push(Vec::new());
            let falls_through =
                fg.emit_coop_stmts(&f.body.stmts, &mut suspends, frame, result_offset);
            // Fell off the end of the body → the task is Ready.
            if falls_through {
                fg.emit_flush_defers_from(0);
                let ready = fg.builder.ins().iconst(types::I32, 1);
                fg.builder.ins().return_(&[ready]);
            }
            fg.defer_stack.pop();
            defer_sites = std::mem::take(&mut fg.collected_defer_sites);
        }

        // Dispatch on the state word (offset 0): state 0 → body_start,
        // state k → suspends[k-1].
        builder.switch_to_block(dispatch);
        let state = builder
            .ins()
            .load(types::I64, MemFlagsData::new(), frame, 0i32);
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
        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| {
                if std::env::var("WILLOW_VERIFY_DEBUG").is_ok() {
                    eprintln!("[verify] {e:?}");
                }
                e
            })?;
        self.module.clear_context(&mut ctx);
        Ok(defer_sites)
    }

    /// Emit the compiler-generated cancellation cleanup entry
    /// `extern "C" fn(frame)` for an async fn (willow-vynv.3): for each defer
    /// site in REVERSE lexical order, if its frame flag is still set, run the
    /// synthetic call against the frame-backed operands and clear the flag.
    pub(super) fn compile_async_cancel_fn(
        &mut self,
        cancel_fid: cranelift_module::FuncId,
        sites: &[AsyncDeferSite],
        return_type: &Type,
    ) -> Result<()> {
        let mut sig = self.module.make_signature();
        sig.params.push(AbiParam::new(types::I64));
        let mut ctx = self.module.make_context();
        ctx.func.signature = sig;
        ctx.func.name = UserFuncName::user(0, cancel_fid.as_u32());
        let mut fn_ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
        let entry = builder.create_block();
        builder.append_block_params_for_function_params(entry);
        builder.switch_to_block(entry);
        let frame = builder.block_params(entry)[0];
        {
            let mut fg = FuncGen {
                builder: &mut builder,
                loop_stack: Vec::new(),
                defer_stack: Vec::new(),
                defer_counter: 0,
                collected_defer_sites: Vec::new(),
                module: &mut self.module,
                gc_tlab_state: self.gc_tlab_state,
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
                expr_types: &self.expr_types,
                coop_frame: None,
                coop_result_offset: None,
                enum_variant_resolutions: &self.enum_variant_resolutions,
                pattern_resolutions: &self.pattern_resolutions,
                async_frame: Some(frame),
                async_frame_offsets: HashMap::new(),
                main_result_err_ty: None,
                vars: HashMap::new(),
                return_type: return_type.clone(),
                current_class: None,
                is_async: false,
                terminated: false,
                gc_root_count: 0,
                build_mode: self.build_mode,
                source_file: &self.source_file,
            };
            for site in sites.iter().rev() {
                for (name, offset, ty) in &site.bindings {
                    fg.vars.insert(
                        name.clone(),
                        VarStorage::Frame {
                            offset: *offset,
                            ty: ty.clone(),
                        },
                    );
                }
                let flag =
                    fg.builder
                        .ins()
                        .load(types::I64, MemFlagsData::new(), frame, site.flag_offset);
                let run_b = fg.builder.create_block();
                let skip_b = fg.builder.create_block();
                fg.builder.ins().brif(flag, run_b, &[], skip_b, &[]);
                fg.builder.switch_to_block(run_b);
                fg.builder.seal_block(run_b);
                fg.emit_deferred_action(&site.action);
                let zero = fg.builder.ins().iconst(types::I64, 0);
                fg.builder
                    .ins()
                    .store(MemFlagsData::new(), zero, frame, site.flag_offset);
                fg.builder.ins().jump(skip_b, &[]);
                fg.builder.switch_to_block(skip_b);
                fg.builder.seal_block(skip_b);
            }
            fg.builder.ins().return_(&[]);
        }
        builder.seal_all_blocks();
        builder.finalize();
        self.module
            .define_function(cancel_fid, &mut ctx)
            .map_err(|e| {
                if std::env::var("WILLOW_VERIFY_DEBUG").is_ok() {
                    eprintln!("[verify] {e:?}");
                }
                e
            })?;
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
            .store(MemFlagsData::new(), state_value, frame, 0i32);
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

        // `await task` / `await task.result()`: the expression evaluates to an
        // eager task frame (the `.result()` adapter is the identity, so both
        // forms yield the SAME frame). Outside a cooperative poll lowering,
        // block-run the scheduler until just this task reaches a terminal state
        // (slot 1 = task id), then map that state to a value — panic-on-cancel
        // for `await task`, `Result<T, Cancelled>` for `await task.result()`
        // (willow-bsqy, willow-qrj9).
        if let Some((task_result_ty, cancel_aware)) = awaitable_task_type(&awaitable_ty) {
            let frame = self.emit_expr(&await_expr.expr);
            self.emit_push_root(frame);
            let task_id = self.builder.ins().load(
                types::I64,
                MemFlagsData::new(),
                frame,
                async_frame_slot_offset(FRAME_SLOT_TASK_ID),
            );
            let run_fid = self.func_id("willow_sched_run_until");
            let run_ref = self.module.declare_func_in_func(run_fid, self.builder.func);
            self.builder.ins().call(run_ref, &[task_id]);
            let value =
                self.emit_task_terminal_value(frame, task_id, &task_result_ty, cancel_aware);
            self.emit_pop_roots_n(1);
            self.gc_root_count -= 1;
            return match value {
                Some(v) => v,
                // `await` on a `void` task: no result slot to read.
                None => self.builder.ins().iconst(types::I8, 0),
            };
        }

        let output_ty = future_output_type(&awaitable_ty).unwrap_or(Type::Void);
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
    /// Record the call-site of a task spawn for panic/debug traces
    /// (willow-0a6k.7): willow_sched_set_spawn_site(id, file, line).
    pub(super) fn emit_set_spawn_site(
        &mut self,
        task_id: cranelift_codegen::ir::Value,
        line: usize,
    ) {
        if self.build_mode != super::BuildMode::Debug {
            return;
        }
        let source_file = self.source_file.to_string();
        let file_ptr = self.emit_string_literal(&source_file);
        let line_val = self.builder.ins().iconst(types::I64, line as i64);
        let fid = self.func_id("willow_sched_set_spawn_site");
        let fref = self.module.declare_func_in_func(fid, self.builder.func);
        self.builder
            .ins()
            .call(fref, &[task_id, file_ptr, line_val]);
    }

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
        // Pass the declared parameter types so arguments are coerced the same
        // way a plain call coerces them; without this a class argument reaches
        // an interface-typed parameter un-boxed.
        let param_types = self.fn_param_types(&call.callee);
        let (arg_vals, arg_roots) = self.emit_call_args_rooted_coerced(
            Some(&call.callee),
            modes.as_deref(),
            None,
            param_types.as_deref(),
            &call.args,
        );
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
        self.emit_gc_heap_store_classified(
            frame,
            callee_off,
            callee_frame,
            true,
            GcStoreDestination::AsyncFrameSlot,
        );
        // 3. id = callee[TASK_ID] (slot 1).
        let id = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            callee_frame,
            async_frame_slot_offset(FRAME_SLOT_TASK_ID),
        );
        self.emit_set_spawn_site(id, await_span.line);
        // 4. done = willow_frame_await(frame, id): 1 = already complete,
        // 0 = registered. The frame's own header answers "already terminal?"
        // without a scheduler lookup (willow-ezs.1.3); the id is still needed
        // to register this task as a waiter when it is not.
        let await_fid = self.func_id("willow_frame_await");
        let await_ref = self
            .module
            .declare_func_in_func(await_fid, self.builder.func);
        let dcall = self.builder.ins().call(await_ref, &[callee_frame, id]);
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
        self.builder
            .ins()
            .store(MemFlagsData::new(), st, frame, 0i32);
        let pending = self.builder.ins().iconst(types::I32, 0);
        self.builder.ins().return_(&[pending]);
        suspends.push(resume_b);
        // resume (reached from the dispatch on wake AND the already-complete brif):
        // reload the callee frame, read its RESULT slot, bind.
        self.builder.switch_to_block(resume_b);
        // A CANCELLED callee has no result to read — the same located panic
        // as await (willow-vynv.1), instead of reading garbage from the slot.
        {
            let callee2 =
                self.builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), frame, callee_off);
            let cid = self.builder.ins().load(
                types::I64,
                MemFlagsData::new(),
                callee2,
                async_frame_slot_offset(FRAME_SLOT_TASK_ID),
            );
            let check_fid = self.func_id("willow_frame_await_check");
            let check_ref = self
                .module
                .declare_func_in_func(check_fid, self.builder.func);
            self.builder.ins().call(check_ref, &[callee2, cid]);
        }
        let result_ty = bind.as_ref().map(|(_, _, ty)| ty.clone()).or(result_ty);
        let result = result_ty.map(|ty| {
            let callee2 =
                self.builder
                    .ins()
                    .load(types::I64, MemFlagsData::new(), frame, callee_off);
            self.builder.ins().load(
                clif_type(&ty),
                MemFlagsData::new(),
                callee2,
                async_frame_slot_offset(FRAME_SLOT_RESULT),
            )
        });
        if let Some((name, x_off, x_ty)) = bind {
            let result = result.expect("binding a call-await requires a result value");
            self.emit_gc_heap_store(
                frame,
                x_off,
                result,
                &x_ty,
                GcStoreDestination::AsyncFrameSlot,
            );
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
        site: TaskAwaitSite<'_>,
        bind: Option<(String, i32, Type)>,
        result_ty: Option<Type>,
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
    ) -> Option<cranelift_codegen::ir::Value> {
        let TaskAwaitSite {
            task_expr,
            await_span,
            task_result_ty,
            cancel_aware,
        } = site;
        let task_frame = self.emit_expr(task_expr);
        let stored_task_slot = self.async_frame_offsets.get(&await_span).copied();
        if let Some(off) = stored_task_slot {
            self.emit_gc_heap_store_classified(
                frame,
                off,
                task_frame,
                true,
                GcStoreDestination::AsyncFrameSlot,
            );
        }

        let id = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            task_frame,
            async_frame_slot_offset(FRAME_SLOT_TASK_ID),
        );
        let await_fid = self.func_id("willow_frame_await");
        let await_ref = self
            .module
            .declare_func_in_func(await_fid, self.builder.func);
        let dcall = self.builder.ins().call(await_ref, &[task_frame, id]);
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
        self.builder
            .ins()
            .store(MemFlagsData::new(), st, frame, 0i32);
        let pending = self.builder.ins().iconst(types::I32, 0);
        self.builder.ins().return_(&[pending]);
        suspends.push(resume_b);

        self.builder.switch_to_block(resume_b);
        let task_frame = if let Some(off) = stored_task_slot {
            self.builder
                .ins()
                .load(types::I64, MemFlagsData::new(), frame, off)
        } else {
            self.emit_expr(task_expr)
        };
        // `willow_frame_await` reports every terminal state as ready, so the
        // terminal status is resolved explicitly before slot 0 is read (or a
        // void await returns): `await task` panics on a cancelled task,
        // `await task.result()` maps it to `Err(Cancelled)` (willow-qrj9).
        let id = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            task_frame,
            async_frame_slot_offset(FRAME_SLOT_TASK_ID),
        );
        let wants_value = bind.is_some() || result_ty.is_some();
        let value = self.emit_task_terminal_value(task_frame, id, &task_result_ty, cancel_aware);
        let result = if wants_value { value } else { None };
        if let Some((name, x_off, x_ty)) = bind {
            let result = result.expect("binding a task-await requires a result value");
            self.emit_gc_heap_store(
                frame,
                x_off,
                result,
                &x_ty,
                GcStoreDestination::AsyncFrameSlot,
            );
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

    /// `expr` is `ch.recv()` where the RECEIVER really is a `Channel<T>`:
    /// returns the method call + element type. Name-only matching would
    /// hijack a user-defined `recv()` method into the channel runtime
    /// (review fix on willow-0a6k.6).
    fn channel_recv_typed<'e>(&mut self, expr: &'e Expr) -> Option<(&'e MethodCallExpr, Type)> {
        let m = is_channel_recv(expr)?;
        let elem = channel_element_type(&self.ast_type_of(&m.object))?;
        Some((m, elem))
    }

    /// `expr` is `ch.send(v)` where the RECEIVER really is a `Channel<T>`:
    /// returns the method call + element type. As with `recv`, name-only
    /// matching would hijack a user-defined `send()` method.
    fn channel_send_typed<'e>(&mut self, expr: &'e Expr) -> Option<(&'e MethodCallExpr, Type)> {
        let m = is_channel_send(expr)?;
        let elem = channel_element_type(&self.ast_type_of(&m.object))?;
        // Frame slots for both operands are allocated by the slot collector;
        // without them the value could not survive a park, so fall back to the
        // eager (blocking) send.
        if !self.async_frame_offsets.contains_key(&m.span)
            || !self
                .async_frame_offsets
                .contains_key(&m.args[0].expr.span())
        {
            return None;
        }
        Some((m, elem))
    }

    /// Emit a cooperative channel `send` (willow-o038) as a suspend point. The
    /// channel and the sent value are evaluated ONCE before the check block and
    /// stashed in frame slots, because the native stack is gone after a park.
    /// The check block (a resume target) calls `willow_channel_try_send_*`: it
    /// returns 1 when the value was enqueued (or the channel is closed, where
    /// send is a documented no-op), and 0 after registering the running task as
    /// a SEND waiter on a full bounded buffer — then we record the resume state
    /// and return Pending. A later `recv`/`close` wakes us to retry.
    pub(super) fn emit_coop_send(
        &mut self,
        method: &MethodCallExpr,
        elem_ty: &Type,
        suspends: &mut Vec<cranelift_codegen::ir::Block>,
        frame: cranelift_codegen::ir::Value,
    ) {
        let ch_off = self.async_frame_offsets[&method.span];
        let ch = self.emit_expr(&method.object);
        self.emit_gc_heap_store_classified(
            frame,
            ch_off,
            ch,
            true,
            GcStoreDestination::AsyncFrameSlot,
        );
        let value_expr = method.args[0].expr.clone();
        let val_off = self.async_frame_offsets[&value_expr.span()];
        let raw_val = self.emit_expr(&value_expr);
        let val_ty = self.ast_type_of(&value_expr);
        let val = self.coerce_to_target(raw_val, &val_ty, elem_ty);
        self.emit_gc_heap_store(
            frame,
            val_off,
            val,
            elem_ty,
            GcStoreDestination::AsyncFrameSlot,
        );

        let check_b = self.builder.create_block();
        self.builder.ins().jump(check_b, &[]);
        let state = (suspends.len() + 1) as i64;
        suspends.push(check_b);
        self.builder.switch_to_block(check_b);
        let ch = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), frame, ch_off);
        let val = self
            .builder
            .ins()
            .load(clif_type(elem_ty), MemFlagsData::new(), frame, val_off);
        let try_name = format!(
            "willow_channel_try_send_{}",
            channel_runtime_suffix(elem_ty)
        );
        let try_fid = self.func_ids[&try_name];
        let try_ref = self.module.declare_func_in_func(try_fid, self.builder.func);
        let scall = self.builder.ins().call(try_ref, &[ch, val]);
        let sent = self.builder.inst_results(scall)[0];
        let done_b = self.builder.create_block();
        let suspend_b = self.builder.create_block();
        let sent_ok = self.builder.ins().icmp_imm(IntCC::NotEqual, sent, 0);
        self.builder
            .ins()
            .brif(sent_ok, done_b, &[], suspend_b, &[]);

        // Full: registered as a send waiter; park and re-enter the check block.
        self.builder.switch_to_block(suspend_b);
        self.builder.seal_block(suspend_b);
        let st = self.builder.ins().iconst(types::I64, state);
        self.builder
            .ins()
            .store(MemFlagsData::new(), st, frame, 0i32);
        let pending = self.builder.ins().iconst(types::I32, 0);
        self.builder.ins().return_(&[pending]);

        self.builder.switch_to_block(done_b);
        self.builder.seal_block(done_b);
    }

    /// The value of a task that has REACHED a terminal state, given its frame.
    ///
    /// This is the one place the two await forms differ (willow-qrj9):
    ///
    /// * `cancel_aware == false` (`await task`) — a cancelled task is a located
    ///   runtime panic (`willow_frame_await_check`), and the result slot is read
    ///   only on the surviving path. `None` for a `void` task.
    /// * `cancel_aware == true` (`await task.result()`) — the terminal status is
    ///   mapped to a value: `Ok(result slot)` or `Err(Cancelled)`, never a
    ///   panic, and no result-slot read happens on the cancelled path.
    ///
    /// A panicked task is handled by the runtime's own panic policy in both
    /// cases, so it needs no branch here.
    pub(super) fn emit_task_terminal_value(
        &mut self,
        task_frame: cranelift_codegen::ir::Value,
        task_id: cranelift_codegen::ir::Value,
        result_ty: &Type,
        cancel_aware: bool,
    ) -> Option<cranelift_codegen::ir::Value> {
        if !cancel_aware {
            let check_id = self.func_id("willow_frame_await_check");
            let check_ref = self
                .module
                .declare_func_in_func(check_id, self.builder.func);
            self.builder.ins().call(check_ref, &[task_frame, task_id]);
            if *result_ty == Type::Void {
                return None;
            }
            return Some(self.builder.ins().load(
                clif_type(result_ty),
                MemFlagsData::new(),
                task_frame,
                async_frame_slot_offset(FRAME_SLOT_RESULT),
            ));
        }
        Some(self.emit_task_result_value(task_frame, result_ty))
    }

    /// `Result<T, Cancelled>` for a task frame that is already terminal: an
    /// Acquire load of the frame-backed status decides the variant, and the
    /// result slot is touched only after `Completed` was observed.
    pub(super) fn emit_task_result_value(
        &mut self,
        task_frame: cranelift_codegen::ir::Value,
        result_ty: &Type,
    ) -> cranelift_codegen::ir::Value {
        let status_id = self.func_id("willow_frame_status");
        let status_ref = self
            .module
            .declare_func_in_func(status_id, self.builder.func);
        let status_call = self.builder.ins().call(status_ref, &[task_frame]);
        let status = self.builder.inst_results(status_call)[0];
        let terminal = self
            .builder
            .ins()
            .band_imm(status, WILLOW_FRAME_STATUS_TERMINAL_MASK);
        let is_cancelled =
            self.builder
                .ins()
                .icmp_imm(IntCC::Equal, terminal, WILLOW_FRAME_STATUS_CANCELLED);

        let cancelled_b = self.builder.create_block();
        let ok_b = self.builder.create_block();
        let merge_b = self.builder.create_block();
        self.builder.append_block_param(merge_b, types::I64);
        self.builder
            .ins()
            .brif(is_cancelled, cancelled_b, &[], ok_b, &[]);

        self.builder.switch_to_block(cancelled_b);
        self.builder.seal_block(cancelled_b);
        // `Err(Cancelled)`: an all-fieldless enum value is an IMMEDIATE i64 tag
        // (not a heap object) — variant `Cancelled` = tag 0.
        let cancelled_val = self.builder.ins().iconst(types::I64, 0);
        let err =
            self.emit_alloc_enum_variant(1, &Type::Named("Cancelled".to_string()), cancelled_val);
        self.builder.ins().jump(merge_b, &[err.into()]);

        self.builder.switch_to_block(ok_b);
        self.builder.seal_block(ok_b);
        let payload_ty = if *result_ty == Type::Void {
            Type::I64
        } else {
            result_ty.clone()
        };
        let raw = if *result_ty == Type::Void {
            self.builder.ins().iconst(types::I64, 0)
        } else {
            self.builder.ins().load(
                clif_type(result_ty),
                MemFlagsData::new(),
                task_frame,
                async_frame_slot_offset(FRAME_SLOT_RESULT),
            )
        };
        let ok = self.emit_alloc_enum_variant(0, &payload_ty, raw);
        self.builder.ins().jump(merge_b, &[ok.into()]);

        self.builder.switch_to_block(merge_b);
        // Sync functions do not `seal_all_blocks`, so seal here. The helper is
        // normally reached from cooperative `await task.result()` lowering.
        self.builder.seal_block(merge_b);
        self.builder.block_params(merge_b)[0]
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
        self.builder
            .ins()
            .store(MemFlagsData::new(), st, frame, 0i32);
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
    pub(super) fn emit_select_unregister_all(
        &mut self,
        sel: &SelectExpr,
        frame: cranelift_codegen::ir::Value,
    ) {
        let unreg_fid = self.func_id("willow_channel_unregister_waiter");
        for case in &sel.cases {
            match &case.kind {
                // Send cases register on the SEND waiter list when the channel
                // is full (willow-o038); `willow_channel_unregister_waiter`
                // clears both lists, so one call per channel covers each.
                SelectCaseKind::Recv { channel, .. } | SelectCaseKind::Send { channel, .. } => {
                    // Re-load the once-evaluated channel from its frame slot.
                    let off = self.async_frame_offsets[&channel.span()];
                    let ch = self
                        .builder
                        .ins()
                        .load(types::I64, MemFlagsData::new(), frame, off);
                    let unreg_ref = self
                        .module
                        .declare_func_in_func(unreg_fid, self.builder.func);
                    self.builder.ins().call(unreg_ref, &[ch]);
                }
                // A task-await case registered on the task's waiter list — remove
                // it so completion does not spuriously wake us (willow-soro).
                SelectCaseKind::Join { task, .. } => {
                    let off = self.async_frame_offsets[&task.span()];
                    let t = self
                        .builder
                        .ins()
                        .load(types::I64, MemFlagsData::new(), frame, off);
                    let id = self.builder.ins().load(
                        types::I64,
                        MemFlagsData::new(),
                        t,
                        async_frame_slot_offset(FRAME_SLOT_TASK_ID),
                    );
                    let tu_fid = self.func_id("willow_sched_unregister_task_waiter");
                    let tu_ref = self.module.declare_func_in_func(tu_fid, self.builder.func);
                    self.builder.ins().call(tu_ref, &[id]);
                }
                _ => {}
            }
        }
    }

    /// Cooperative `select` as a suspend point (willow-7aj). Probe each case in
    /// source order: a recv case is ready when its channel has a value or is
    /// closed (`willow_channel_recv_ready` otherwise registers the running task
    /// as a waiter on that channel); a send case is ready when its channel has room
    /// (`willow_channel_send_ready` otherwise registers the task as a SEND waiter —
    /// unbounded channels always have room). When a case is
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
        // Evaluate every recv-case CHANNEL expression exactly once, before
        // the resumable check block, stashing the pointers in frame slots —
        // re-entries and the probe/unregister/recv sequence all re-load the
        // same channel (willow-0a6k.6 review fix: a side-effecting channel
        // expression must not be re-evaluated per phase or per wakeup).
        for case in &sel.cases {
            match &case.kind {
                SelectCaseKind::Recv { channel, .. } => {
                    let ch = self.emit_expr(channel);
                    let off = self.async_frame_offsets[&channel.span()];
                    self.emit_gc_heap_store_classified(
                        frame,
                        off,
                        ch,
                        true,
                        GcStoreDestination::AsyncFrameSlot,
                    );
                }
                // A send case's channel AND value are entry-evaluated too
                // (willow-o038): a bounded channel can be full, so the probe
                // re-runs on every wakeup and must not re-run either operand.
                SelectCaseKind::Send { channel, value } => {
                    let ch = self.emit_expr(channel);
                    let off = self.async_frame_offsets[&channel.span()];
                    self.emit_gc_heap_store_classified(
                        frame,
                        off,
                        ch,
                        true,
                        GcStoreDestination::AsyncFrameSlot,
                    );
                    let elem_ty =
                        channel_element_type(&self.ast_type_of(channel)).unwrap_or(Type::I64);
                    let v = self.emit_expr(value);
                    let voff = self.async_frame_offsets[&value.span()];
                    self.emit_gc_heap_store(
                        frame,
                        voff,
                        v,
                        &elem_ty,
                        GcStoreDestination::AsyncFrameSlot,
                    );
                }
                // Timeout deadline fixed ONCE at select entry (willow-soro).
                SelectCaseKind::Timeout { millis } => {
                    let ms = self.emit_expr(millis);
                    let now_fid = self.func_id("willow_monotonic_millis");
                    let now_ref = self.module.declare_func_in_func(now_fid, self.builder.func);
                    let ncall = self.builder.ins().call(now_ref, &[]);
                    let now = self.builder.inst_results(ncall)[0];
                    let deadline = self.builder.ins().iadd(now, ms);
                    let off = self.async_frame_offsets[&millis.span()];
                    self.builder
                        .ins()
                        .store(MemFlagsData::new(), deadline, frame, off);
                }
                // Task handle stashed once, like channels (willow-soro).
                SelectCaseKind::Join { task, .. } => {
                    let t = self.emit_expr(task);
                    let off = self.async_frame_offsets[&task.span()];
                    self.emit_gc_heap_store_classified(
                        frame,
                        off,
                        t,
                        true,
                        GcStoreDestination::AsyncFrameSlot,
                    );
                }
                _ => {}
            }
        }
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

        // Pseudo-randomized probe (willow-0a6k.6): probe EVERY case's
        // readiness first (recv_ready registers the running task as a waiter
        // on not-ready channels — the chosen exec block unregisters from
        // all), count the ready ones, then pick the (rotation % count)-th
        // ready case. This avoids SYSTEMATIC source-order starvation; it is
        // not a bounded-fairness guarantee (the rotation is a mixed global
        // counter, not a per-select scheduler).
        let zero32 = self.builder.ins().iconst(types::I32, 0);
        let mut ready_flags: Vec<Option<cranelift_codegen::ir::Value>> = Vec::new();
        for case in sel.cases.iter() {
            match &case.kind {
                SelectCaseKind::Recv { channel, .. } => {
                    let off = self.async_frame_offsets[&channel.span()];
                    let ch = self
                        .builder
                        .ins()
                        .load(types::I64, MemFlagsData::new(), frame, off);
                    let ready_fid = self.func_id("willow_channel_recv_ready");
                    let ready_ref = self
                        .module
                        .declare_func_in_func(ready_fid, self.builder.func);
                    let rcall = self.builder.ins().call(ready_ref, &[ch]);
                    let raw = self.builder.inst_results(rcall)[0];
                    let is_ready = self.builder.ins().icmp(IntCC::NotEqual, raw, zero32);
                    let flag = self.builder.ins().uextend(types::I64, is_ready);
                    ready_flags.push(Some(flag));
                }
                SelectCaseKind::Send { channel, .. } => {
                    // A send case is ready when the channel is unbounded, not
                    // full, or closed (willow-o038). On a full channel
                    // `willow_channel_send_ready` registers the running task as
                    // a send waiter, so a later recv wakes us to re-probe.
                    let off = self.async_frame_offsets[&channel.span()];
                    let ch = self
                        .builder
                        .ins()
                        .load(types::I64, MemFlagsData::new(), frame, off);
                    let ready_fid = self.func_id("willow_channel_send_ready");
                    let ready_ref = self
                        .module
                        .declare_func_in_func(ready_fid, self.builder.func);
                    let rcall = self.builder.ins().call(ready_ref, &[ch]);
                    let raw = self.builder.inst_results(rcall)[0];
                    let is_ready = self.builder.ins().icmp(IntCC::NotEqual, raw, zero32);
                    let flag = self.builder.ins().uextend(types::I64, is_ready);
                    ready_flags.push(Some(flag));
                }
                SelectCaseKind::Timeout { millis } => {
                    let off = self.async_frame_offsets[&millis.span()];
                    let deadline =
                        self.builder
                            .ins()
                            .load(types::I64, MemFlagsData::new(), frame, off);
                    let now_fid = self.func_id("willow_monotonic_millis");
                    let now_ref = self.module.declare_func_in_func(now_fid, self.builder.func);
                    let ncall = self.builder.ins().call(now_ref, &[]);
                    let now = self.builder.inst_results(ncall)[0];
                    let due =
                        self.builder
                            .ins()
                            .icmp(IntCC::SignedGreaterThanOrEqual, now, deadline);
                    let flag = self.builder.ins().uextend(types::I64, due);
                    ready_flags.push(Some(flag));
                }
                SelectCaseKind::Join { task, .. } => {
                    // willow_sched_await both PROBES completion and registers
                    // the running task as a waiter when pending (willow-soro).
                    let off = self.async_frame_offsets[&task.span()];
                    let t = self
                        .builder
                        .ins()
                        .load(types::I64, MemFlagsData::new(), frame, off);
                    let id = self.builder.ins().load(
                        types::I64,
                        MemFlagsData::new(),
                        t,
                        async_frame_slot_offset(FRAME_SLOT_TASK_ID),
                    );
                    let await_fid = self.func_id("willow_frame_await");
                    let await_ref = self
                        .module
                        .declare_func_in_func(await_fid, self.builder.func);
                    let acall = self.builder.ins().call(await_ref, &[t, id]);
                    let raw = self.builder.inst_results(acall)[0];
                    let done = self.builder.ins().icmp(IntCC::NotEqual, raw, zero32);
                    let flag = self.builder.ins().uextend(types::I64, done);
                    ready_flags.push(Some(flag));
                }
                SelectCaseKind::Default => ready_flags.push(None),
            }
        }
        let mut total = self.builder.ins().iconst(types::I64, 0);
        for flag in ready_flags.iter().flatten() {
            total = self.builder.ins().iadd(total, *flag);
        }
        let none_ready = self.builder.ins().icmp_imm(IntCC::Equal, total, 0);
        let pick_b = self.builder.create_block();
        let idle_b = self.builder.create_block();
        self.builder
            .ins()
            .brif(none_ready, idle_b, &[], pick_b, &[]);

        self.builder.switch_to_block(idle_b);
        self.builder.seal_block(idle_b);
        if let Some(di) = default_idx {
            self.builder.ins().jump(exec_blocks[di], &[]);
        } else {
            // Arm the NEAREST timeout deadline (if any) so the scheduler
            // wakes this task when it becomes due (willow-soro).
            let timeout_offsets: Vec<i32> = sel
                .cases
                .iter()
                .filter_map(|case| match &case.kind {
                    SelectCaseKind::Timeout { millis } => {
                        Some(self.async_frame_offsets[&millis.span()])
                    }
                    _ => None,
                })
                .collect();
            if !timeout_offsets.is_empty() {
                let mut min_deadline: Option<cranelift_codegen::ir::Value> = None;
                for off in timeout_offsets {
                    let d = self
                        .builder
                        .ins()
                        .load(types::I64, MemFlagsData::new(), frame, off);
                    min_deadline = Some(match min_deadline {
                        None => d,
                        Some(m) => {
                            let lt = self.builder.ins().icmp(IntCC::SignedLessThan, d, m);
                            self.builder.ins().select(lt, d, m)
                        }
                    });
                }
                let m = min_deadline.expect("at least one timeout");
                let now_fid = self.func_id("willow_monotonic_millis");
                let now_ref = self.module.declare_func_in_func(now_fid, self.builder.func);
                let ncall = self.builder.ins().call(now_ref, &[]);
                let now = self.builder.inst_results(ncall)[0];
                let remaining = self.builder.ins().isub(m, now);
                let zero = self.builder.ins().iconst(types::I64, 0);
                let neg = self
                    .builder
                    .ins()
                    .icmp(IntCC::SignedLessThan, remaining, zero);
                let clamped = self.builder.ins().select(neg, zero, remaining);
                let sleep_fid = self.func_id("willow_sched_sleep");
                let sleep_ref = self
                    .module
                    .declare_func_in_func(sleep_fid, self.builder.func);
                self.builder.ins().call(sleep_ref, &[clamped]);
            }
            // Not ready: registered on all recv channels; suspend.
            let st = self.builder.ins().iconst(types::I64, state);
            self.builder
                .ins()
                .store(MemFlagsData::new(), st, frame, 0i32);
            let pending = self.builder.ins().iconst(types::I32, 0);
            self.builder.ins().return_(&[pending]);
        }

        // pick: k = rotation % total, then jump to the k-th READY case.
        self.builder.switch_to_block(pick_b);
        self.builder.seal_block(pick_b);
        let rot_fid = self.func_id("willow_select_rotation");
        let rot_ref = self.module.declare_func_in_func(rot_fid, self.builder.func);
        let rot_call = self.builder.ins().call(rot_ref, &[]);
        let rotation = self.builder.inst_results(rot_call)[0];
        let k = self.builder.ins().urem(rotation, total);
        let mut acc = self.builder.ins().iconst(types::I64, 0);
        let mut current = pick_b;
        for (i, flag) in ready_flags.iter().enumerate() {
            let Some(flag) = flag else { continue };
            // acc' = acc + flag; this case is chosen when it is ready and
            // acc' == k + 1 (i.e. it is the (k+1)-th ready case).
            let next_acc = self.builder.ins().iadd(acc, *flag);
            let k1 = self.builder.ins().iadd_imm(k, 1);
            let is_kth = self.builder.ins().icmp(IntCC::Equal, next_acc, k1);
            let one64 = self.builder.ins().iconst(types::I64, 1);
            let is_ready_now =
                self.builder
                    .ins()
                    .icmp(IntCC::SignedGreaterThanOrEqual, *flag, one64);
            let chosen = self.builder.ins().band(is_kth, is_ready_now);
            let cont = self.builder.create_block();
            self.builder
                .ins()
                .brif(chosen, exec_blocks[i], &[], cont, &[]);
            self.builder.switch_to_block(cont);
            self.builder.seal_block(cont);
            acc = next_acc;
            let _ = current;
            current = cont;
        }
        // Unreachable when total > 0; keep the CFG well-formed.
        self.builder
            .ins()
            .trap(cranelift_codegen::ir::TrapCode::unwrap_user(1));

        // Exec blocks: unregister from all recv channels, then run the case body.
        let mut any_falls = false;
        for (i, case) in sel.cases.iter().enumerate() {
            self.builder.switch_to_block(exec_blocks[i]);
            let saved_vars = self.vars.clone();
            let saved_roots = self.gc_root_count;
            self.terminated = false;
            self.emit_select_unregister_all(sel, frame);
            match &case.kind {
                SelectCaseKind::Recv { binding, channel } => {
                    let elem_ty =
                        channel_element_type(&self.ast_type_of(channel)).unwrap_or(Type::I64);
                    let off = self.async_frame_offsets[&channel.span()];
                    let ch = self
                        .builder
                        .ins()
                        .load(types::I64, MemFlagsData::new(), frame, off);
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
                        self.emit_gc_heap_store(
                            frame,
                            off,
                            v,
                            &elem_ty,
                            GcStoreDestination::AsyncFrameSlot,
                        );
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
                    // Both operands were evaluated once at select entry; reload
                    // them from the frame (willow-o038).
                    let ch_off = self.async_frame_offsets[&channel.span()];
                    let ch =
                        self.builder
                            .ins()
                            .load(types::I64, MemFlagsData::new(), frame, ch_off);
                    let val_off = self.async_frame_offsets[&value.span()];
                    let val = self.builder.ins().load(
                        clif_type(&elem_ty),
                        MemFlagsData::new(),
                        frame,
                        val_off,
                    );
                    // A non-blocking send: the probe said not-full, but another
                    // worker may have filled the channel in between. On failure
                    // re-enter the probe rather than blocking or dropping the
                    // value (willow-o038).
                    let try_name = format!(
                        "willow_channel_try_send_{}",
                        channel_runtime_suffix(&elem_ty)
                    );
                    let try_fid = self.func_ids[&try_name];
                    let try_ref = self.module.declare_func_in_func(try_fid, self.builder.func);
                    let scall = self.builder.ins().call(try_ref, &[ch, val]);
                    let sent = self.builder.inst_results(scall)[0];
                    let body_b = self.builder.create_block();
                    let sent_ok = self.builder.ins().icmp_imm(IntCC::NotEqual, sent, 0);
                    self.builder.ins().brif(sent_ok, body_b, &[], check_b, &[]);
                    self.builder.switch_to_block(body_b);
                    self.builder.seal_block(body_b);
                    let falls =
                        self.emit_coop_stmts(&case.body.stmts, suspends, frame, result_offset);
                    if falls {
                        self.builder.ins().jump(done_b, &[]);
                        any_falls = true;
                    }
                }
                SelectCaseKind::Timeout { .. } => {
                    // The deadline already fired (its ready flag chose this
                    // case); nothing to consume — run the body.
                    let falls =
                        self.emit_coop_stmts(&case.body.stmts, suspends, frame, result_offset);
                    if falls {
                        self.builder.ins().jump(done_b, &[]);
                        any_falls = true;
                    }
                }
                SelectCaseKind::Join { binding, task } => {
                    // Task already terminal (the probe's willow_sched_await
                    // returned done). `await t` panics on a cancelled task;
                    // `await t.result()` binds `Result<T, Cancelled>`
                    // (willow-soro, willow-qrj9).
                    let t_off = self.async_frame_offsets[&task.span()];
                    let t = self
                        .builder
                        .ins()
                        .load(types::I64, MemFlagsData::new(), frame, t_off);
                    let id = self.builder.ins().load(
                        types::I64,
                        MemFlagsData::new(),
                        t,
                        async_frame_slot_offset(FRAME_SLOT_TASK_ID),
                    );
                    // Cancellation-awareness is a property of the awaited TYPE,
                    // so inline and held `TaskResult<T>` values lower identically
                    // (willow-qrj9).
                    let (task_result_ty, cancel_aware) =
                        awaitable_task_type(&self.ast_type_of(task)).unwrap_or((Type::I64, false));
                    let value = self.emit_task_terminal_value(t, id, &task_result_ty, cancel_aware);
                    if binding != "_" {
                        let result_ty = self
                            .async_local_types
                            .get(&case.span)
                            .cloned()
                            .unwrap_or(Type::I64);
                        if result_ty != Type::Void
                            && let Some(v) = value
                        {
                            let off = self.async_frame_offsets[&case.span];
                            // The frame is heap-allocated and may already be
                            // old, while a String/class result is typically
                            // young: go through the write barrier so the
                            // remembered set sees the edge (willow-o038 review).
                            self.emit_gc_heap_store(
                                frame,
                                off,
                                v,
                                &result_ty,
                                GcStoreDestination::AsyncFrameSlot,
                            );
                            self.vars.insert(
                                binding.clone(),
                                VarStorage::Frame {
                                    offset: off,
                                    ty: result_ty,
                                },
                            );
                        }
                    }
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

    /// `await <task>` / `await <task>.result()` in a cooperative poll: the
    /// expression the task frame comes from, the await span, the task's own
    /// result type `T`, the await's OUTPUT type (`T`, or `Result<T, Cancelled>`
    /// for the cancellation-aware form), and which form it is (willow-qrj9).
    fn await_contextual_task_expr<'e>(&self, expr: &'e Expr) -> Option<(TaskAwaitSite<'e>, Type)> {
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
            if let Some((task_result_ty, cancel_aware)) = awaitable_task_type(&awaited_ty) {
                // Preserve and evaluate the complete awaitable expression
                // exactly once. The builtin `Task.result()` is already an
                // identity in expression lowering; syntactically peeling any
                // method call here would miscompile a user method that returns
                // `TaskResult<T>` (willow-qrj9).
                let task_expr = &a.expr;
                let output_ty = if cancel_aware {
                    Type::Generic(
                        "Result".to_string(),
                        vec![task_result_ty.clone(), Type::Named("Cancelled".to_string())],
                    )
                } else {
                    task_result_ty.clone()
                };
                return Some((
                    TaskAwaitSite {
                        task_expr,
                        await_span: a.span,
                        task_result_ty,
                        cancel_aware,
                    },
                    output_ty,
                ));
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
                    self.builder
                        .ins()
                        .store(MemFlagsData::new(), st, frame, 0i32);
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
                    self.builder
                        .ins()
                        .store(MemFlagsData::new(), st, frame, 0i32);
                    let pending = self.builder.ins().iconst(types::I32, 0);
                    self.builder.ins().return_(&[pending]);
                    let resume = self.builder.create_block();
                    suspends.push(resume);
                    self.builder.switch_to_block(resume);
                    true
                }
                Stmt::Expr(es) if self.await_contextual_task_expr(&es.expr).is_some() => {
                    let (site, _) = self.await_contextual_task_expr(&es.expr).unwrap();
                    self.emit_coop_task_await(site, None, None, suspends, frame);
                    true
                }
                Stmt::Let(l) if self.await_contextual_task_expr(&l.init).is_some() => {
                    let (site, output_ty) = self.await_contextual_task_expr(&l.init).unwrap();
                    let x_ty =
                        l.ty.clone()
                            .or_else(|| self.async_local_types.get(&l.span).cloned())
                            .unwrap_or(output_ty);
                    let x_off = self.async_frame_offsets[&l.span];
                    self.emit_coop_task_await(
                        site,
                        Some((l.name.clone(), x_off, x_ty)),
                        None,
                        suspends,
                        frame,
                    );
                    true
                }
                Stmt::Assign(a) if self.await_contextual_task_expr(&a.value).is_some() => {
                    let (site, _) = self.await_contextual_task_expr(&a.value).unwrap();
                    if let Some(storage) = self.vars.get(&a.name).cloned() {
                        let target_ty = storage.ty().clone();
                        let result = self
                            .emit_coop_task_await(site, None, Some(target_ty), suspends, frame)
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
                // `return ch.recv();` is a suspend point too (willow-0a6k.6):
                // without this arm it fell into the SYNC recv path, which
                // BLOCK-DRIVES the scheduler from inside this poll (nested
                // run), cannot park, and cannot be cancelled.
                Stmt::Return(r)
                    if r.value
                        .as_ref()
                        .is_some_and(|v| self.channel_recv_typed(v).is_some()) =>
                {
                    let value = r.value.as_ref().unwrap();
                    let (m, elem_ty) = self.channel_recv_typed(value).unwrap();
                    let (object, elem_ty) = (m.object.clone(), elem_ty);
                    let v = self.emit_coop_recv(&object, &elem_ty, suspends, frame);
                    if let Some(off) = result_offset {
                        self.emit_gc_heap_store(
                            frame,
                            off,
                            v,
                            &elem_ty,
                            GcStoreDestination::AsyncFrameSlot,
                        );
                    }
                    self.emit_flush_defers_from(0);
                    let ready = self.builder.ins().iconst(types::I32, 1);
                    self.builder.ins().return_(&[ready]);
                    self.terminated = true;
                    false
                }
                Stmt::Let(l) if self.channel_recv_typed(&l.init).is_some() => {
                    let (m, elem_ty) = self.channel_recv_typed(&l.init).unwrap();
                    let (object, elem_ty) = (m.object.clone(), elem_ty);
                    let v = self.emit_coop_recv(&object, &elem_ty, suspends, frame);
                    let x_off = self.async_frame_offsets[&l.span];
                    let x_ty =
                        l.ty.clone()
                            .or_else(|| self.async_local_types.get(&l.span).cloned())
                            .unwrap_or_else(|| elem_ty.clone());
                    self.emit_gc_heap_store(
                        frame,
                        x_off,
                        v,
                        &x_ty,
                        GcStoreDestination::AsyncFrameSlot,
                    );
                    self.vars.insert(
                        l.name.clone(),
                        VarStorage::Frame {
                            offset: x_off,
                            ty: x_ty,
                        },
                    );
                    true
                }
                Stmt::Assign(a) if self.channel_recv_typed(&a.value).is_some() => {
                    let (m, elem_ty) = self.channel_recv_typed(&a.value).unwrap();
                    let (object, elem_ty) = (m.object.clone(), elem_ty);
                    let v = self.emit_coop_recv(&object, &elem_ty, suspends, frame);
                    if let Some(storage) = self.vars.get(&a.name).cloned() {
                        self.store_var(&storage, v);
                    }
                    true
                }
                Stmt::Expr(es) if self.channel_recv_typed(&es.expr).is_some() => {
                    let (m, elem_ty) = self.channel_recv_typed(&es.expr).unwrap();
                    let (object, elem_ty) = (m.object.clone(), elem_ty);
                    self.emit_coop_recv(&object, &elem_ty, suspends, frame);
                    true
                }
                Stmt::Expr(es) if self.channel_send_typed(&es.expr).is_some() => {
                    let (m, elem_ty) = self.channel_send_typed(&es.expr).unwrap();
                    let method = m.clone();
                    self.emit_coop_send(&method, &elem_ty, suspends, frame);
                    true
                }
                Stmt::Expr(es) if matches!(&es.expr, Expr::Select(_)) => {
                    let Expr::Select(sel) = &es.expr else {
                        unreachable!("guard matched Select");
                    };
                    self.emit_coop_select(sel, suspends, frame, result_offset)
                }
                Stmt::FieldAssign(s) if self.await_contextual_task_expr(&s.value).is_some() => {
                    let (site, value_ty) = self.await_contextual_task_expr(&s.value).unwrap();
                    let obj_type = self.ast_type_of(&s.object);
                    if let Some(class_name) = class_name_for_object_type(&obj_type)
                        && let Some(layout) = self.class_layouts.get(&class_name).cloned()
                        && let Some(idx) = layout.iter().position(|(n, _)| n == &s.field)
                    {
                        let field_ty = layout[idx].1.clone();
                        let result = self
                            .emit_coop_task_await(
                                site,
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
                        self.emit_gc_heap_store(
                            ptr,
                            offset,
                            val,
                            &field_ty,
                            GcStoreDestination::ObjectField,
                        );

                        self.emit_pop_roots_n(1);
                        self.gc_root_count -= 1;
                    } else {
                        self.emit_coop_task_await(site, None, None, suspends, frame);
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
                        self.emit_gc_heap_store(
                            ptr,
                            offset,
                            val,
                            &field_ty,
                            GcStoreDestination::ObjectField,
                        );

                        self.emit_pop_roots_n(1);
                        self.gc_root_count -= 1;
                    } else {
                        self.emit_coop_call_await(call, await_span, None, None, suspends, frame);
                    }
                    true
                }
                Stmt::IndexAssign(s) if self.await_contextual_task_expr(&s.value).is_some() => {
                    let (site, value_ty) = self.await_contextual_task_expr(&s.value).unwrap();
                    let elem_ty = array_element_type(&self.ast_type_of(&s.array));
                    let result = self
                        .emit_coop_task_await(site, None, Some(value_ty.clone()), suspends, frame)
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
                    let (site, result_ty) = self
                        .await_contextual_task_expr(value)
                        .expect("return task-await guard matched");
                    let result = self.emit_coop_task_await(
                        site,
                        None,
                        Some(result_ty.clone()),
                        suspends,
                        frame,
                    );
                    if let (Some(off), Some(result)) = (result_offset, result) {
                        self.emit_gc_heap_store(
                            frame,
                            off,
                            result,
                            &result_ty,
                            GcStoreDestination::AsyncFrameSlot,
                        );
                    }
                    // Run pending defers (result already stored in the
                    // frame) and clear their flags (willow-vynv.3).
                    self.emit_flush_defers_from(0);
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
                            Some(result_ty.clone()),
                            suspends,
                            frame,
                        )
                        .expect("return call-await requires a result value");
                    if let Some(off) = result_offset {
                        self.emit_gc_heap_store(
                            frame,
                            off,
                            result,
                            &result_ty,
                            GcStoreDestination::AsyncFrameSlot,
                        );
                    }
                    // Run pending defers (result already stored in the
                    // frame) and clear their flags (willow-vynv.3).
                    self.emit_flush_defers_from(0);
                    let ready = self.builder.ins().iconst(types::I32, 1);
                    self.builder.ins().return_(&[ready]);
                    self.terminated = true;
                    false
                }
                Stmt::Return(r) => {
                    if let (Some(off), Some(v)) = (result_offset, &r.value) {
                        let result_ty = self.ast_type_of(v);
                        let val = self.emit_expr(v);
                        self.emit_gc_heap_store(
                            frame,
                            off,
                            val,
                            &result_ty,
                            GcStoreDestination::AsyncFrameSlot,
                        );
                    }
                    // Run pending defers (result already stored in the
                    // frame) and clear their flags (willow-vynv.3).
                    self.emit_flush_defers_from(0);
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
                    // `continue` target: runs the safepoint back edge, so a
                    // `continue` BEFORE any await in the body cannot busy-loop
                    // past the scheduler (willow-kzka review fix).
                    let cont_b = self.builder.create_block();
                    let exit_b = self.builder.create_block();
                    self.builder.ins().jump(header, &[]);
                    self.builder.switch_to_block(header);
                    let cond = self.emit_expr(&s.cond);
                    self.builder.ins().brif(cond, body_b, &[], exit_b, &[]);
                    self.builder.switch_to_block(body_b);
                    self.terminated = false;
                    let saved_vars = self.vars.clone();
                    let saved_roots = self.gc_root_count;
                    self.loop_stack.push((
                        exit_b,
                        cont_b,
                        self.gc_root_count,
                        self.defer_stack.len(),
                    ));
                    let body_falls =
                        self.emit_coop_stmts(&s.body.stmts, suspends, frame, result_offset);
                    self.loop_stack.pop();
                    if body_falls {
                        self.builder.ins().jump(cont_b, &[]);
                    }
                    self.builder.switch_to_block(cont_b);
                    self.emit_coop_safepoint_to(suspends, frame, header);
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
        self.emit_gc_heap_store(
            frame,
            iter_off,
            arr,
            &iterable_ty,
            GcStoreDestination::AsyncFrameSlot,
        );
        let zero = self.builder.ins().iconst(types::I64, 0);
        self.builder
            .ins()
            .store(MemFlagsData::new(), zero, frame, index_off);

        let header = self.builder.create_block();
        let body_b = self.builder.create_block();
        // `continue` target: increment + safepoint back edge (willow-kzka).
        let inc_b = self.builder.create_block();
        let exit_b = self.builder.create_block();
        self.builder.ins().jump(header, &[]);

        self.builder.switch_to_block(header);
        let arr = self.builder.ins().load(
            clif_type(&iterable_ty),
            MemFlagsData::new(),
            frame,
            iter_off,
        );
        let idx = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), frame, index_off);
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
            self.emit_gc_heap_store(
                frame,
                off,
                item,
                &elem_ty,
                GcStoreDestination::AsyncFrameSlot,
            );
            self.vars.insert(
                s.name.clone(),
                VarStorage::Frame {
                    offset: off,
                    ty: elem_ty,
                },
            );
        }

        self.loop_stack
            .push((exit_b, inc_b, self.gc_root_count, self.defer_stack.len()));
        let body_falls = self.emit_coop_stmts(&s.body.stmts, suspends, frame, result_offset);
        self.loop_stack.pop();
        if body_falls {
            self.builder.ins().jump(inc_b, &[]);
        }
        self.builder.switch_to_block(inc_b);
        let idx = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), frame, index_off);
        let one = self.builder.ins().iconst(types::I64, 1);
        let next = self.builder.ins().iadd(idx, one);
        self.builder
            .ins()
            .store(MemFlagsData::new(), next, frame, index_off);
        self.emit_coop_safepoint_to(suspends, frame, header);
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
            .load(types::I64, MemFlagsData::new(), ptr, 0i32);
        let end = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), ptr, 8i32);
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
            .store(MemFlagsData::new(), start, frame, current_off);
        self.builder
            .ins()
            .store(MemFlagsData::new(), end, frame, end_off);

        let header = self.builder.create_block();
        let body_b = self.builder.create_block();
        // `continue` target: increment + safepoint back edge (willow-kzka).
        let inc_b = self.builder.create_block();
        let exit_b = self.builder.create_block();
        self.builder.ins().jump(header, &[]);

        self.builder.switch_to_block(header);
        let current = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), frame, current_off);
        let end = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), frame, end_off);
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
                .store(MemFlagsData::new(), current, frame, off);
            self.vars.insert(
                s.name.clone(),
                VarStorage::Frame {
                    offset: off,
                    ty: Type::I64,
                },
            );
        }

        self.loop_stack
            .push((exit_b, inc_b, self.gc_root_count, self.defer_stack.len()));
        let body_falls = self.emit_coop_stmts(&s.body.stmts, suspends, frame, result_offset);
        self.loop_stack.pop();
        if body_falls {
            self.builder.ins().jump(inc_b, &[]);
        }
        self.builder.switch_to_block(inc_b);
        let current = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), frame, current_off);
        let one = self.builder.ins().iconst(types::I64, 1);
        let next = self.builder.ins().iadd(current, one);
        self.builder
            .ins()
            .store(MemFlagsData::new(), next, frame, current_off);
        self.emit_coop_safepoint_to(suspends, frame, header);
        self.vars = saved_vars;
        self.gc_root_count = saved_roots;
        self.builder.switch_to_block(exit_b);
        self.terminated = false;
        true
    }

    /// Eager (block-driving) `select` (willow-7aj): probe each case in source
    /// order — a recv case is ready when its channel has a value or is closed; a
    /// send case is ready while its channel has room (always, when unbounded).
    /// If none is ready and there is a `default`, it runs; otherwise the scheduler
    /// is driven and the probe retried (giving up if no task could progress). In a
    /// non-task context recv_ready does not register a waiter (current task is 0),
    /// so it is a pure readiness probe here.
    /// Register a `select` case's stashed channel pointer as a shadow-stack
    /// root. Channels are GC objects, so the stash is a real reference; the
    /// type guard keeps this uniform with the other stash slots.
    fn root_select_channel_slot(&mut self, channel: &Expr, slot: cranelift_codegen::ir::StackSlot) {
        let ty = self.ast_type_of(channel);
        if is_gc_managed(&ty, self.enum_infos) {
            self.emit_push_root_slot(slot);
        }
    }

    pub(super) fn emit_select(&mut self, s: &SelectExpr) {
        // Evaluate each case's CHANNEL expression exactly once (stack-slot
        // stash): the retry loop re-probes without re-running side effects,
        // and probe/recv/send all target the same channel (willow-0a6k.6
        // review fix).
        let mut chan_slots: Vec<Option<cranelift_codegen::ir::StackSlot>> = Vec::new();
        // Timeout deadlines / task-await handles, stashed once per case index
        // (willow-soro), parallel to chan_slots.
        let mut aux_slots: Vec<Option<cranelift_codegen::ir::StackSlot>> = Vec::new();
        // GC-managed stash slots (send values, task handles) must be shadow-stack
        // roots for as long as the select loop can collect: the probe loop drives
        // the scheduler, which allocates. Popped on the `done_b` exit; a `return`
        // out of a case body pops them with the rest via `gc_root_count`.
        let roots_before_select = self.gc_root_count;
        for case in &s.cases {
            match &case.kind {
                SelectCaseKind::Recv { channel, .. } => {
                    let ch = self.emit_expr(channel);
                    let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        8,
                        0,
                    ));
                    self.builder.ins().stack_store(ch, slot, 0);
                    // A channel is a GC object (willow-p4er) and this stash may
                    // be its only reference — a temporary or factory-returned
                    // channel would otherwise be collected while the probe loop
                    // drives the scheduler, leaving a dangling pointer.
                    self.root_select_channel_slot(channel, slot);
                    chan_slots.push(Some(slot));
                    aux_slots.push(None);
                }
                // A send case stashes BOTH operands: with bounded channels the
                // probe loop can spin several times before the send fits, and
                // neither the channel nor the value may be re-evaluated
                // (willow-o038).
                SelectCaseKind::Send { channel, value } => {
                    let ch = self.emit_expr(channel);
                    let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        8,
                        0,
                    ));
                    self.builder.ins().stack_store(ch, slot, 0);
                    // Rooted before the value is evaluated: that expression can
                    // allocate and collect on its own.
                    self.root_select_channel_slot(channel, slot);
                    chan_slots.push(Some(slot));
                    let v = self.emit_expr(value);
                    let vslot = self.builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        8,
                        0,
                    ));
                    self.builder.ins().stack_store(v, vslot, 0);
                    let elem_ty =
                        channel_element_type(&self.ast_type_of(channel)).unwrap_or(Type::I64);
                    if is_gc_managed(&elem_ty, self.enum_infos) {
                        self.emit_push_root_slot(vslot);
                    }
                    aux_slots.push(Some(vslot));
                }
                SelectCaseKind::Timeout { millis } => {
                    let ms = self.emit_expr(millis);
                    let now_fid = self.func_id("willow_monotonic_millis");
                    let now_ref = self.module.declare_func_in_func(now_fid, self.builder.func);
                    let ncall = self.builder.ins().call(now_ref, &[]);
                    let now = self.builder.inst_results(ncall)[0];
                    let deadline = self.builder.ins().iadd(now, ms);
                    let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        8,
                        0,
                    ));
                    self.builder.ins().stack_store(deadline, slot, 0);
                    chan_slots.push(None);
                    aux_slots.push(Some(slot));
                }
                SelectCaseKind::Join { task, .. } => {
                    let t = self.emit_expr(task);
                    let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
                        StackSlotKind::ExplicitSlot,
                        8,
                        0,
                    ));
                    self.builder.ins().stack_store(t, slot, 0);
                    // The task handle IS the async frame, a GC object: root it,
                    // or a later case's allocation can collect the task we are
                    // still probing.
                    let task_ty = self.ast_type_of(task);
                    if is_gc_managed(&task_ty, self.enum_infos) {
                        self.emit_push_root_slot(slot);
                    }
                    chan_slots.push(None);
                    aux_slots.push(Some(slot));
                }
                SelectCaseKind::Default => {
                    chan_slots.push(None);
                    aux_slots.push(None);
                }
            }
        }
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
        // Pseudo-randomized pick, mirroring the cooperative select
        // (willow-0a6k.6): probe ALL cases' readiness, then run the
        // (rotation % ready_count)-th one. Avoids systematic source-order
        // starvation; not a bounded-fairness guarantee.
        let zero32 = self.builder.ins().iconst(types::I32, 0);
        let mut ready_flags: Vec<Option<cranelift_codegen::ir::Value>> = Vec::new();
        for (i, case) in s.cases.iter().enumerate() {
            match &case.kind {
                SelectCaseKind::Recv { .. } => {
                    let slot = chan_slots[i].expect("recv case has a channel slot");
                    let ch = self.builder.ins().stack_load(types::I64, slot, 0);
                    let ready_fid = self.func_id("willow_channel_recv_ready");
                    let ready_ref = self
                        .module
                        .declare_func_in_func(ready_fid, self.builder.func);
                    let rcall = self.builder.ins().call(ready_ref, &[ch]);
                    let raw = self.builder.inst_results(rcall)[0];
                    let is_ready = self.builder.ins().icmp(IntCC::NotEqual, raw, zero32);
                    let flag = self.builder.ins().uextend(types::I64, is_ready);
                    ready_flags.push(Some(flag));
                }
                SelectCaseKind::Send { .. } => {
                    // Ready when unbounded, not full, or closed (willow-o038).
                    let slot = chan_slots[i].expect("send case has a channel slot");
                    let ch = self.builder.ins().stack_load(types::I64, slot, 0);
                    let ready_fid = self.func_id("willow_channel_send_ready");
                    let ready_ref = self
                        .module
                        .declare_func_in_func(ready_fid, self.builder.func);
                    let rcall = self.builder.ins().call(ready_ref, &[ch]);
                    let raw = self.builder.inst_results(rcall)[0];
                    let is_ready = self.builder.ins().icmp(IntCC::NotEqual, raw, zero32);
                    let flag = self.builder.ins().uextend(types::I64, is_ready);
                    ready_flags.push(Some(flag));
                }
                SelectCaseKind::Timeout { .. } => {
                    let slot = aux_slots[i].expect("timeout case has a deadline slot");
                    let deadline = self.builder.ins().stack_load(types::I64, slot, 0);
                    let now_fid = self.func_id("willow_monotonic_millis");
                    let now_ref = self.module.declare_func_in_func(now_fid, self.builder.func);
                    let ncall = self.builder.ins().call(now_ref, &[]);
                    let now = self.builder.inst_results(ncall)[0];
                    let due =
                        self.builder
                            .ins()
                            .icmp(IntCC::SignedGreaterThanOrEqual, now, deadline);
                    let flag = self.builder.ins().uextend(types::I64, due);
                    ready_flags.push(Some(flag));
                }
                SelectCaseKind::Join { .. } => {
                    // Pure completion probe in sync context (current task is
                    // 0, so willow_sched_await registers nothing).
                    let slot = aux_slots[i].expect("task-await case has a task slot");
                    let t = self.builder.ins().stack_load(types::I64, slot, 0);
                    let id = self.builder.ins().load(
                        types::I64,
                        MemFlagsData::new(),
                        t,
                        async_frame_slot_offset(FRAME_SLOT_TASK_ID),
                    );
                    let await_fid = self.func_id("willow_frame_await");
                    let await_ref = self
                        .module
                        .declare_func_in_func(await_fid, self.builder.func);
                    let acall = self.builder.ins().call(await_ref, &[t, id]);
                    let raw = self.builder.inst_results(acall)[0];
                    let done = self.builder.ins().icmp(IntCC::NotEqual, raw, zero32);
                    let flag = self.builder.ins().uextend(types::I64, done);
                    ready_flags.push(Some(flag));
                }
                SelectCaseKind::Default => ready_flags.push(None),
            }
        }
        let mut total = self.builder.ins().iconst(types::I64, 0);
        for flag in ready_flags.iter().flatten() {
            total = self.builder.ins().iadd(total, *flag);
        }
        let none_ready = self.builder.ins().icmp_imm(IntCC::Equal, total, 0);
        let pick_b = self.builder.create_block();
        let idle_b = self.builder.create_block();
        self.builder
            .ins()
            .brif(none_ready, idle_b, &[], pick_b, &[]);

        self.builder.switch_to_block(pick_b);
        self.builder.seal_block(pick_b);
        let rot_fid = self.func_id("willow_select_rotation");
        let rot_ref = self.module.declare_func_in_func(rot_fid, self.builder.func);
        let rot_call = self.builder.ins().call(rot_ref, &[]);
        let rotation = self.builder.inst_results(rot_call)[0];
        let k = self.builder.ins().urem(rotation, total);
        let mut acc = self.builder.ins().iconst(types::I64, 0);
        for (i, flag) in ready_flags.iter().enumerate() {
            let Some(flag) = flag else { continue };
            let next_acc = self.builder.ins().iadd(acc, *flag);
            let k1 = self.builder.ins().iadd_imm(k, 1);
            let is_kth = self.builder.ins().icmp(IntCC::Equal, next_acc, k1);
            let one64 = self.builder.ins().iconst(types::I64, 1);
            let is_ready_now =
                self.builder
                    .ins()
                    .icmp(IntCC::SignedGreaterThanOrEqual, *flag, one64);
            let chosen = self.builder.ins().band(is_kth, is_ready_now);
            let cont = self.builder.create_block();
            cont_blocks.push(cont);
            self.builder
                .ins()
                .brif(chosen, case_blocks[i], &[], cont, &[]);
            self.builder.switch_to_block(cont);
            acc = next_acc;
        }
        self.builder
            .ins()
            .trap(cranelift_codegen::ir::TrapCode::unwrap_user(1));

        self.builder.switch_to_block(idle_b);
        self.builder.seal_block(idle_b);
        {
            if let Some(di) = default_idx {
                self.builder.ins().jump(case_blocks[di], &[]);
            } else {
                let timeout_slots: Vec<_> = s
                    .cases
                    .iter()
                    .enumerate()
                    .filter_map(|(i, case)| match &case.kind {
                        SelectCaseKind::Timeout { .. } => aux_slots[i],
                        _ => None,
                    })
                    .collect();
                if timeout_slots.is_empty() {
                    let run_fid = self.func_id("willow_sched_run");
                    let run_ref = self.module.declare_func_in_func(run_fid, self.builder.func);
                    let rcall = self.builder.ins().call(run_ref, &[]);
                    let completed = self.builder.inst_results(rcall)[0];
                    let zero = self.builder.ins().iconst(types::I64, 0);
                    let progressed = self.builder.ins().icmp(IntCC::NotEqual, completed, zero);
                    self.builder
                        .ins()
                        .brif(progressed, loop_b, &[], done_b, &[]);
                } else {
                    // With a timeout case the drive MUST be bounded by the
                    // NEAREST deadline: an unbounded `willow_sched_run` runs
                    // unrelated tasks to quiescence first, so a 30ms timeout
                    // could lose to a 5s task (willow-o038 review).
                    let mut min_deadline: Option<cranelift_codegen::ir::Value> = None;
                    for slot in timeout_slots {
                        let d = self.builder.ins().stack_load(types::I64, slot, 0);
                        min_deadline = Some(match min_deadline {
                            None => d,
                            Some(m) => {
                                let lt = self.builder.ins().icmp(IntCC::SignedLessThan, d, m);
                                self.builder.ins().select(lt, d, m)
                            }
                        });
                    }
                    let m = min_deadline.expect("at least one timeout slot");
                    let run_fid = self.func_id("willow_sched_run_until_deadline");
                    let run_ref = self.module.declare_func_in_func(run_fid, self.builder.func);
                    let rcall = self.builder.ins().call(run_ref, &[m]);
                    let completed = self.builder.inst_results(rcall)[0];
                    let zero = self.builder.ins().iconst(types::I64, 0);
                    let progressed = self.builder.ins().icmp(IntCC::NotEqual, completed, zero);
                    // Progress can have made a case ready: re-probe. Otherwise
                    // the scheduler had nothing left to do before the deadline,
                    // so wait it out here and let the timeout flag fire
                    // (willow-soro).
                    let wait_b = self.builder.create_block();
                    self.builder
                        .ins()
                        .brif(progressed, loop_b, &[], wait_b, &[]);
                    self.builder.switch_to_block(wait_b);
                    self.builder.seal_block(wait_b);
                    let sleep_fid = self.func_id("willow_sleep_until_monotonic");
                    let sleep_ref = self
                        .module
                        .declare_func_in_func(sleep_fid, self.builder.func);
                    self.builder.ins().call(sleep_ref, &[m]);
                    self.builder.ins().jump(loop_b, &[]);
                }
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
                    let slot = chan_slots[i].expect("recv case has a channel slot");
                    let ch = self.builder.ins().stack_load(types::I64, slot, 0);
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
                        // The received value has just left the channel: this slot
                        // is now its only reference, so it must be a root.
                        if let VarStorage::Stack { slot, .. } = &storage
                            && is_gc_managed(&elem_ty, self.enum_infos)
                        {
                            let slot = *slot;
                            self.emit_push_root_slot(slot);
                        }
                        self.vars.insert(binding.clone(), storage);
                    }
                    self.emit_block(&case.body);
                }
                SelectCaseKind::Send { channel, .. } => {
                    let elem_ty =
                        channel_element_type(&self.ast_type_of(channel)).unwrap_or(Type::I64);
                    let slot = chan_slots[i].expect("send case has a channel slot");
                    let ch = self.builder.ins().stack_load(types::I64, slot, 0);
                    let vslot = aux_slots[i].expect("send case has a value slot");
                    let val = self.builder.ins().stack_load(clif_type(&elem_ty), vslot, 0);
                    // Non-blocking: the probe said not-full, but a task on
                    // another worker may have filled the channel since. Retry
                    // the whole probe instead of blocking (willow-o038).
                    let try_name = format!(
                        "willow_channel_try_send_{}",
                        channel_runtime_suffix(&elem_ty)
                    );
                    let try_fid = self.func_ids[&try_name];
                    let try_ref = self.module.declare_func_in_func(try_fid, self.builder.func);
                    let scall = self.builder.ins().call(try_ref, &[ch, val]);
                    let sent = self.builder.inst_results(scall)[0];
                    let body_b = self.builder.create_block();
                    let sent_ok = self.builder.ins().icmp_imm(IntCC::NotEqual, sent, 0);
                    self.builder.ins().brif(sent_ok, body_b, &[], loop_b, &[]);
                    self.builder.switch_to_block(body_b);
                    self.builder.seal_block(body_b);
                    self.emit_block(&case.body);
                }
                SelectCaseKind::Timeout { .. } => self.emit_block(&case.body),
                SelectCaseKind::Join { binding, .. } => {
                    let slot = aux_slots[i].expect("task-await case has a task slot");
                    let t = self.builder.ins().stack_load(types::I64, slot, 0);
                    let id = self.builder.ins().load(
                        types::I64,
                        MemFlagsData::new(),
                        t,
                        async_frame_slot_offset(FRAME_SLOT_TASK_ID),
                    );
                    let check_fid = self.func_id("willow_frame_await_check");
                    let check_ref = self
                        .module
                        .declare_func_in_func(check_fid, self.builder.func);
                    self.builder.ins().call(check_ref, &[t, id]);
                    if binding != "_" {
                        let result_ty = self
                            .async_local_types
                            .get(&case.span)
                            .cloned()
                            .unwrap_or(Type::I64);
                        if result_ty != Type::Void {
                            let v = self.builder.ins().load(
                                clif_type(&result_ty),
                                MemFlagsData::new(),
                                t,
                                async_frame_slot_offset(FRAME_SLOT_RESULT),
                            );
                            let bind_slot = self.builder.create_sized_stack_slot(
                                StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 0),
                            );
                            self.builder.ins().stack_store(v, bind_slot, 0);
                            if is_gc_managed(&result_ty, self.enum_infos) {
                                self.emit_push_root_slot(bind_slot);
                            }
                            self.vars.insert(
                                binding.clone(),
                                VarStorage::Stack {
                                    slot: bind_slot,
                                    ty: result_ty,
                                },
                            );
                        }
                    }
                    self.emit_block(&case.body);
                }
                SelectCaseKind::Default => self.emit_block(&case.body),
            }
            if !self.terminated {
                // Roots pushed for this case's binding (the body's own roots were
                // already popped by `emit_block`). Terminated paths popped
                // everything through the return/break handler.
                let case_roots = self.gc_root_count - saved_roots;
                if case_roots > 0 {
                    self.emit_pop_roots_n(case_roots);
                }
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
        let select_roots = self.gc_root_count - roots_before_select;
        if select_roots > 0 {
            self.emit_pop_roots_n(select_roots);
            self.gc_root_count = roots_before_select;
        }
    }
}
