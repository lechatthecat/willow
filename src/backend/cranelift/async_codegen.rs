//! Async / cooperative-scheduler codegen for the Cranelift backend (extracted
//! from `mod.rs`): cooperative `main`/leaf compilation and the await / coop
//! statement / select / range-for lowering. `pub(super)` so the main codegen
//! driver can call them; child-module access reaches private FuncGen/Codegen
//! state.

use crate::ir::lowered::{LirDeferId, LirFunction, LirInst, LirLocalId};
use anyhow::Result;
use cranelift_codegen::ir::{InstBuilder, MemFlagsData, condcodes::IntCC, types};
use cranelift_module::Module;

use super::*;

/// C-ABI status codes returned by the scheduler-aware lock entry points. Both
/// compiler and runtime derive them from `willow_abi`.
const MUTEX_STATUS_ACQUIRED: i64 = willow_abi::LockAcquireStatus::Acquired as i64;
const MUTEX_STATUS_PENDING: i64 = willow_abi::LockAcquireStatus::Pending as i64;

/// Body-specific inputs to the shared cooperative poll-function builder.
struct CoopPollBody<'a> {
    current_class: Option<&'a str>,
    lir: &'a LirFunction,
    result_offset: Option<i32>,
    lir_defer_offsets: HashMap<LirDeferId, i32>,
}

/// Physical frame details the generated async-main driver shares with its poll
/// function. Result main reserves slot 0; void main does not.
pub(super) struct CoopMainDriverFrame {
    slot_count: i64,
    mask: i64,
    first_param_slot: usize,
    main_result: Option<(i32, Type)>,
}

type LirAsyncLayoutPlan = (
    AsyncFrameLayout,
    HashMap<LirLocalId, i32>,
    HashMap<LirDeferId, i32>,
);
const MUTEX_STATUS_RECURSIVE: i64 = willow_abi::LockAcquireStatus::Recursive as i64;
/// This acquisition's generation is dead; the caller must acquire again.
#[cfg_attr(not(test), allow(dead_code))]
const MUTEX_STATUS_LOST: i64 = willow_abi::LockAcquireStatus::Lost as i64;
/// The task may not park, so nothing was registered. Only the scheduler can
/// clear this, by running the task's cancellation entry.
const MUTEX_STATUS_CANCELLED: i64 = willow_abi::LockAcquireStatus::Cancelled as i64;
const MUTEX_STATUS_PHASE_ACQUIRE: i64 = willow_abi::LockStatusPhase::Acquire as i64;
const MUTEX_STATUS_PHASE_POLL: i64 = willow_abi::LockStatusPhase::Poll as i64;

/// Poll-function return codes (`fn(frame) -> i32`).
const RUNTIME_POLL_PENDING: i64 = willow_abi::RuntimePollResult::Pending as i64;

impl Codegen {
    /// Turn the LIR-owned logical frame into the runtime's physical data-slot
    /// layout. The returned identity map is keyed only by `LirLocalId`; the
    /// optional spans on physical slots are diagnostic metadata only.
    fn lir_async_layout(
        &self,
        lir: &LirFunction,
        mut reserved: Vec<AsyncFrameSlot>,
        first_parameter_slot: usize,
    ) -> Result<LirAsyncLayoutPlan> {
        let mut offsets = HashMap::new();
        for (index, local) in lir
            .locals
            .iter()
            .filter(|local| local.parameter)
            .enumerate()
        {
            offsets.insert(
                local.id,
                async_frame_slot_offset(first_parameter_slot + index),
            );
        }
        let frame_all = std::env::var("WILLOW_ASYNC_FRAME_ALL").is_ok();
        let selected: Vec<_> = if frame_all {
            lir.locals
                .iter()
                .filter(|local| !local.parameter)
                .map(|local| local.id)
                .collect()
        } else {
            lir.async_frame.slots.clone()
        };
        for local_id in &selected {
            let local = &lir.locals[local_id.0 as usize];
            if local.parameter {
                continue;
            }
            let index = reserved.len();
            offsets.insert(local.id, async_frame_slot_offset(index));
            reserved.push(AsyncFrameSlot {
                source_span: local.source_span,
                name: local.name.clone(),
                ty: local.ty.clone(),
            });
        }
        let mut defer_sites: Vec<_> = lir
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter_map(|inst| match inst {
                LirInst::Defer { id, span, .. } => Some((*id, *span)),
                _ => None,
            })
            .collect();
        defer_sites.sort_unstable_by_key(|(id, _)| *id);
        defer_sites.dedup_by_key(|(id, _)| *id);
        let mut defer_offsets = HashMap::new();
        for (id, span) in defer_sites {
            let index = reserved.len();
            defer_offsets.insert(id, async_frame_slot_offset(index));
            reserved.push(AsyncFrameSlot {
                source_span: Some(span),
                name: format!("__lir_defer_flag_{}", id.0),
                ty: Type::I64,
            });
        }
        let layout = AsyncFrameLayout::try_new(reserved, &self.enum_infos)?;
        Ok((layout, offsets, defer_offsets))
    }

    /// Cooperative-async lowering (willow-lpn.5.3 / willow-h2vf):
    /// compile `async fn main` as a SUSPENDING poll function driven
    /// by the cooperative scheduler. `willow_user_main` becomes a driver that
    /// allocates the frame, spawns the poll fn as a task, and runs the scheduler;
    /// the poll fn is a state machine whose `await sleep(n)` points store the
    /// next state in the frame's state word (offset 0), call `willow_sched_sleep`,
    /// and return Pending — the timer-aware run loop resumes it.
    pub(super) fn compile_cooperative_main(
        &mut self,
        name: &str,
        f: &FunctionDecl,
        lir: LirFunction,
    ) -> Result<()> {
        // Declare the poll fn `fn(frame: i64) -> i32`.
        let poll_symbol = poll_symbol(USER_MAIN_SYMBOL);
        let mut poll_sig = self.module.make_signature();
        poll_sig.params.push(AbiParam::new(types::I64));
        poll_sig.returns.push(AbiParam::new(types::I32));
        let poll_fid = self
            .module
            .declare_function(&poll_symbol, Linkage::Local, &poll_sig)?;
        self.func_ids.insert(poll_symbol.clone(), poll_fid);

        // A Result-returning main publishes its value in slot 0 for the driver
        // to inspect after the poll task completes. Void main keeps the
        // historical layout with params beginning at slot 0.
        let main_result_err_ty = main_result_err_type(f);
        let mut slots = Vec::new();
        let result_offset = main_result_err_ty.as_ref().map(|_| {
            slots.push(AsyncFrameSlot {
                source_span: Some(f.span),
                name: "__result".to_string(),
                ty: f.return_type.clone(),
            });
            async_frame_slot_offset(FRAME_SLOT_RESULT)
        });
        let first_param_slot = slots.len();

        // Frame-back params and EVERY local (GC and non-GC) so they survive
        // suspension; only GC-managed slots are in `gc_slot_mask` (traced), so
        // non-GC slots hold plain scalars (willow-lpn.5.3 slice 3b).
        slots.extend(f.params.iter().map(|p| AsyncFrameSlot {
            source_span: Some(p.span),
            name: p.name.clone(),
            ty: p.ty.clone(),
        }));
        let (layout, lir_offsets, lir_defer_offsets) =
            self.lir_async_layout(&lir, slots, first_param_slot)?;
        self.record_async_frame_size_warning(&f.name, f.span, &layout);
        let slot_count = layout.slot_count() as i64;
        let mask = layout.gc_slot_mask as i64;
        let param_bindings: Vec<(String, i32, Type)> = f
            .params
            .iter()
            .enumerate()
            .map(|(i, p)| {
                (
                    p.name.clone(),
                    async_frame_slot_offset(first_param_slot + i),
                    p.ty.clone(),
                )
            })
            .collect();

        self.compile_coop_main_driver(
            name,
            &poll_symbol,
            &f.params,
            CoopMainDriverFrame {
                slot_count,
                mask,
                first_param_slot,
                main_result: result_offset.zip(main_result_err_ty.clone()),
            },
        )?;
        self.compile_coop_main_poll(
            &poll_symbol,
            f,
            HashMap::new(),
            lir_offsets,
            &param_bindings,
            CoopPollBody {
                current_class: None,
                lir: &lir,
                result_offset,
                lir_defer_offsets,
            },
        )?;
        Ok(())
    }

    /// Cooperative task lowering (willow-lpn.5.3 / willow-h2vf): compile `name`
    /// as a CONSTRUCTOR (its public symbol: alloc a frame whose slot 0 is the
    /// RESULT and slot 1 is TASK_ID, spawn the poll fn as a task, return the
    /// frame ptr) plus a suspending poll fn whose `return v` stores `v` at the
    /// RESULT slot. The returned frame is the language-level `Task<T>`.
    pub(super) fn compile_cooperative_leaf(
        &mut self,
        name: &str,
        f: &FunctionDecl,
        lir: LirFunction,
    ) -> Result<()> {
        let poll_symbol = coop_poll_symbol(name);
        let mut poll_sig = self.module.make_signature();
        poll_sig.params.push(AbiParam::new(types::I64));
        poll_sig.returns.push(AbiParam::new(types::I32));
        let poll_fid = self
            .module
            .declare_function(&poll_symbol, Linkage::Local, &poll_sig)?;
        self.func_ids.insert(poll_symbol.clone(), poll_fid);
        // Cancellation cleanup entry (willow-vynv.3), defined after the poll
        // fn (it needs the defer sites collected while emitting the body).
        let cancel_symbol = coop_cancel_symbol(name);
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
                source_span: Some(f.span),
                name: "__result".to_string(),
                ty: f.return_type.clone(),
            },
            AsyncFrameSlot {
                source_span: None,
                name: "__task_id".to_string(),
                ty: Type::I64,
            },
        ];
        for p in &f.params {
            slots.push(AsyncFrameSlot {
                source_span: Some(p.span),
                name: p.name.clone(),
                ty: p.ty.clone(),
            });
        }
        // Locals after the params: frame-backed so they survive the task's own
        // suspensions, keyed by LirLocalId.
        let (layout, lir_offsets, lir_defer_offsets) = self.lir_async_layout(&lir, slots, 2)?;
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
        let poll_addr = builder
            .ins()
            .func_addr(super::type_helpers::FN_ADDR_TYPE, poll_ref);
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
        let cancel_addr = builder
            .ins()
            .func_addr(super::type_helpers::FN_ADDR_TYPE, cancel_ref);
        let set_fid = self.func_id("willow_sched_set_cancel_fn");
        let set_ref = self.module.declare_func_in_func(set_fid, builder.func);
        builder.ins().call(set_ref, &[task_id, cancel_addr]);
        builder.ins().return_(&[frame]);
        builder.finalize(self.module.target_config());
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
        let (sites, lock_sites) = self.compile_coop_main_poll(
            &poll_symbol,
            f,
            HashMap::new(),
            lir_offsets,
            &param_bindings,
            CoopPollBody {
                current_class: None,
                lir: &lir,
                result_offset: Some(result_offset),
                lir_defer_offsets,
            },
        )?;
        self.compile_async_cancel_fn(cancel_fid, &sites, &lock_sites, &f.return_type)?;
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
        lir: LirFunction,
    ) -> Result<()> {
        let poll_symbol = coop_poll_symbol(mangled);
        let mut poll_sig = self.module.make_signature();
        poll_sig.params.push(AbiParam::new(types::I64));
        poll_sig.returns.push(AbiParam::new(types::I32));
        let poll_fid = self
            .module
            .declare_function(&poll_symbol, Linkage::Local, &poll_sig)?;
        self.func_ids.insert(poll_symbol.clone(), poll_fid);
        // Cancellation cleanup entry (willow-vynv.3).
        let cancel_symbol = coop_cancel_symbol(mangled);
        let mut cancel_sig = self.module.make_signature();
        cancel_sig.params.push(AbiParam::new(types::I64));
        let cancel_fid =
            self.module
                .declare_function(&cancel_symbol, Linkage::Local, &cancel_sig)?;

        let mut slots = vec![
            AsyncFrameSlot {
                source_span: Some(m.span),
                name: "__result".to_string(),
                ty: m.return_type.clone(),
            },
            AsyncFrameSlot {
                source_span: None,
                name: "__task_id".to_string(),
                ty: Type::I64,
            },
        ];
        let self_offset = if m.is_static {
            None
        } else {
            let offset = async_frame_slot_offset(slots.len());
            slots.push(AsyncFrameSlot {
                source_span: None,
                name: "self".to_string(),
                ty: Type::Named(class_name.to_string()),
            });
            Some(offset)
        };
        let first_param_slot = slots.len();
        for p in &m.params {
            slots.push(AsyncFrameSlot {
                source_span: Some(p.span),
                name: p.name.clone(),
                ty: p.ty.clone(),
            });
        }

        // The LOWERED parameter list of an instance method starts with the
        // `self` receiver (`lower_method` pushes it first), and that receiver
        // already owns the frame slot just before the declared parameters -- so
        // the lowered parameters start one slot earlier than `m.params` does.
        // Handing `first_param_slot` straight to the LIR layout would map `self`
        // onto the first declared parameter's slot and shift every parameter by
        // one (willow-0g8j.2.18).
        let lir_first_param_slot = if m.is_static {
            first_param_slot
        } else {
            first_param_slot - 1
        };
        let (layout, lir_offsets, lir_defer_offsets) =
            self.lir_async_layout(&lir, slots, lir_first_param_slot)?;
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
        let poll_addr = builder
            .ins()
            .func_addr(super::type_helpers::FN_ADDR_TYPE, poll_ref);
        let spawn_fid = self.func_id("willow_sched_spawn");
        let spawn_ref = self.module.declare_func_in_func(spawn_fid, builder.func);
        let spawn_call = builder.ins().call(spawn_ref, &[poll_addr, frame]);
        let task_id = builder.inst_results(spawn_call)[0];
        builder
            .ins()
            .store(MemFlagsData::trusted(), task_id, frame, task_id_offset);
        // Attach the cancellation cleanup entry (willow-vynv.3).
        let cancel_ref = self.module.declare_func_in_func(cancel_fid, builder.func);
        let cancel_addr = builder
            .ins()
            .func_addr(super::type_helpers::FN_ADDR_TYPE, cancel_ref);
        let set_fid = self.func_id("willow_sched_set_cancel_fn");
        let set_ref = self.module.declare_func_in_func(set_fid, builder.func);
        builder.ins().call(set_ref, &[task_id, cancel_addr]);
        builder.ins().return_(&[frame]);
        builder.finalize(self.module.target_config());
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
        let (sites, lock_sites) = self.compile_coop_main_poll(
            &poll_symbol,
            &poll_decl,
            HashMap::new(),
            lir_offsets,
            &param_bindings,
            CoopPollBody {
                current_class: Some(class_name),
                lir: &lir,
                result_offset: Some(result_offset),
                lir_defer_offsets,
            },
        )?;
        self.compile_async_cancel_fn(cancel_fid, &sites, &lock_sites, &m.return_type)?;
        Ok(())
    }

    /// `willow_user_main` driver: alloc frame, bind any main args, spawn the
    /// poll task, run the scheduler to completion.
    pub(super) fn compile_coop_main_driver(
        &mut self,
        name: &str,
        poll_symbol: &str,
        params: &[Param],
        frame_layout: CoopMainDriverFrame,
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
        let slot_count_v = builder.ins().iconst(types::I64, frame_layout.slot_count);
        let mask_v = builder.ins().iconst(types::I64, frame_layout.mask);
        let call = builder.ins().call(alloc_ref, &[slot_count_v, mask_v]);
        let frame = builder.inst_results(call)[0];

        // The scheduler owns a frame root only until the poll task reaches a
        // terminal state. A Result main reads the frame after run_until
        // returns, so retain one native root across that handoff.
        if frame_layout.main_result.is_some() {
            let slot = builder.create_sized_stack_slot(StackSlotData::new(
                StackSlotKind::ExplicitSlot,
                8,
                0,
            ));
            builder.ins().stack_store(types::I64, frame, slot, 0);
            let ptr_ty = self.module.target_config().pointer_type();
            let addr = builder.ins().stack_addr(ptr_ty, slot, 0);
            let push_fid = self.func_id("willow_push_root");
            let push_ref = self.module.declare_func_in_func(push_fid, builder.func);
            builder.ins().call(push_ref, &[addr]);
        }

        if let Some(param) = params.first() {
            let arr_id = self.func_id("willow_runtime_args_array");
            let arr_ref = self.module.declare_func_in_func(arr_id, builder.func);
            let arr_call = builder.ins().call(arr_ref, &[]);
            let arr = builder.inst_results(arr_call)[0];
            emit_gc_heap_store_raw(
                &mut builder,
                Some(barrier_ref),
                frame,
                async_frame_slot_offset(frame_layout.first_param_slot),
                arr,
                GcStoreDestination::AsyncFrameSlot,
                MemFlagsData::trusted(),
            );
            debug_assert_eq!(param.name, "args");
        }

        // willow_sched_spawn(poll_addr, frame) -> main's task id.
        let poll_fid = self.func_ids[poll_symbol];
        let poll_ref = self.module.declare_func_in_func(poll_fid, builder.func);
        let poll_addr = builder
            .ins()
            .func_addr(super::type_helpers::FN_ADDR_TYPE, poll_ref);
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

        if let Some((result_offset, err_ty)) = frame_layout.main_result {
            let result =
                builder
                    .ins()
                    .load(types::I64, MemFlagsData::trusted(), frame, result_offset);
            let pop_fid = self.func_id("willow_pop_roots");
            let pop_ref = self.module.declare_func_in_func(pop_fid, builder.func);
            let one = builder.ins().iconst(types::I32, 1);
            builder.ins().call(pop_ref, &[one]);
            let fail_id = self.func_id("willow_main_fail");
            super::emit_match::emit_main_result_exit_raw(
                &mut builder,
                &mut self.module,
                fail_id,
                result,
                err_ty == Type::String,
            );
        } else {
            builder.ins().return_(&[]);
        }
        builder.finalize(self.module.target_config());
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
    fn compile_coop_main_poll(
        &mut self,
        poll_symbol: &str,
        f: &FunctionDecl,
        offsets: HashMap<crate::diagnostics::Span, i32>,
        lir_offsets: HashMap<crate::ir::lowered::LirLocalId, i32>,
        param_bindings: &[(String, i32, Type)],
        body: CoopPollBody<'_>,
    ) -> Result<(Vec<AsyncDeferSite>, Vec<AsyncLockSite>)> {
        let result_offset = body.result_offset;
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
        let ptr_ty = self.module.target_config().pointer_type();
        // Tag the running task with this async fn's name on every poll entry (so
        // resumes re-tag too), before dispatch (willow-9lw).
        if let Some(name) = &tag_name
            && let Some(&data_id) = self.string_literals.get(name)
        {
            let gv = self.module.declare_data_in_func(data_id, builder.func);
            let name_ptr = builder.ins().symbol_value(ptr_ty, gv);
            let name_len = builder.ins().iconst(types::I64, name.len() as i64);
            let tag_id = self.func_id("willow_sched_tag_current_task");
            let tag_ref = self.module.declare_func_in_func(tag_id, builder.func);
            builder.ins().call(tag_ref, &[name_ptr, name_len]);
        }
        // This value is defined in the poll entry and therefore dominates both
        // first-entry and resume dispatch. Cooperative panic scopes express
        // their lexical root depth relative to it; a value captured inside the
        // body would not dominate resume edges that jump past that capture.
        let root_depth_id = self.func_id("willow_root_depth");
        let root_depth_ref = self
            .module
            .declare_func_in_func(root_depth_id, builder.func);
        let root_depth_call = builder.ins().call(root_depth_ref, &[]);
        let poll_root_depth = builder.inst_results(root_depth_call)[0];
        let dispatch = builder.create_block();
        builder.ins().jump(dispatch, &[]);

        // body_start is state 0; each `await` suspend appends a resume block
        // (state k = suspends[k-1]). Because all locals/params are frame-backed,
        // resume blocks need no SSA block params — we emit structured control
        // flow (if/while) directly and seal everything at the end (slice 5).
        let body_start = builder.create_block();
        let mut suspends: Vec<CoopSuspendPoint> = Vec::new();
        let defer_sites: Vec<AsyncDeferSite>;
        let lock_sites: Vec<AsyncLockSite>;
        let coop_root_slots: Vec<cranelift_codegen::ir::StackSlot>;
        {
            let mut fg = FuncGen {
                builder: &mut builder,
                loop_stack: Vec::new(),
                defer_stack: Vec::new(),
                defer_counter: 0,
                sync_defer_flags: HashMap::new(),
                panic_scopes: Vec::new(),
                unavailable_defer_ids: HashSet::new(),
                panic_defer_codegen_depth: 0,
                recover_eligible_depth: 0,
                panic_recovery_targets: HashSet::new(),
                panic_return_block: None,
                panic_function_root_depth: Some(poll_root_depth),
                callstack_frame_depth: 0,
                fault_site_span: None,
                collected_defer_sites: Vec::new(),
                lock_scopes: Vec::new(),
                collected_lock_sites: Vec::new(),
                collected_cleanup_order: 0,
                module: &mut self.module,
                gc_tlab_state: self.gc_tlab_state,
                func_ids: &self.func_ids,
                func_return_types: &self.func_return_types,
                fn_types: &self.fn_types,
                func_param_modes: &self.func_param_modes,
                func_param_debug: &self.func_param_debug,
                function_may_panic: &self.function_may_panic,
                known_modules: &self.known_modules,
                visible_modules: &self.visible_modules,
                builtin_module_aliases: &self.builtin_module_aliases,
                lambda_names: &self.lambda_names,
                cooperative_leaves: &self.cooperative_leaves,
                string_literals: &self.string_literals,
                class_layouts: &self.class_layouts,
                static_storage: &self.static_storage,
                enum_infos: &self.enum_infos,
                class_base: &self.class_base,
                class_type_ids: &self.class_type_ids,
                class_descriptor_ids: &self.class_descriptor_ids,
                class_vslots: &self.class_vslots,
                interface_infos: &self.interface_infos,
                vtable_ids: &self.vtable_ids,
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
                lir_frame_offsets: lir_offsets,
                lir_defer_offsets: body.lir_defer_offsets.clone(),
                lir_hoisted_await: None,
                main_result_err_ty: None,
                vars: HashMap::new(),
                return_type: f.return_type.clone(),
                current_class: body.current_class,
                is_async: false,
                terminated: false,
                gc_root_count: 0,
                coop_shadow_roots: Some(CoopShadowRoots::default()),
                build_mode: self.build_mode,
                source_file: &self.source_file,
                address_taken: collect_address_taken_locals(&f.body),
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
            // The outer function body owns one lexical defer frame. Nested
            // statement bodies create their own frames in `emit_coop_stmts`;
            // return/`?` flush every active frame, while fallthrough,
            // break, and continue flush only the scopes they leave
            // (willow-s9ej.1).
            let outer_defer_depth = fg.defer_stack.len();
            let outer_root_depth = fg.coop_root_depth();
            let owns_outer_defer = f
                .body
                .stmts
                .iter()
                .any(|stmt| matches!(stmt, Stmt::Defer(_)));
            let outer_panic_scope = owns_outer_defer.then(|| PanicScope {
                cleanup: fg.builder.create_block(),
                resume: fg.builder.create_block(),
                root_depth_at_entry: fg
                    .panic_function_root_depth
                    .expect("poll root depth snapshot"),
                defer_depth: outer_defer_depth,
                vars_before: fg.vars.clone(),
                coop_root_depth_at_entry: Some(outer_root_depth),
            });
            fg.defer_stack.push(Vec::new());
            if let Some(scope) = outer_panic_scope.clone() {
                fg.panic_scopes.push(scope);
            }
            // The function body is the outermost root scope. Its fallthrough
            // defers must run before those roots are popped, so call the inner
            // emitter directly; nested blocks use `emit_coop_stmts`, which
            // balances their lexical roots on exit.
            fg.emit_coop_lir_function(body.lir, &mut suspends, frame);
            let source_falls_through = false;
            let mut normal_reaches_resume = false;
            if source_falls_through {
                fg.emit_flush_defers_from(0);
                if !fg.terminated {
                    if let Some(scope) = &outer_panic_scope {
                        let active = fg.coop_root_depth();
                        assert_eq!(fg.gc_root_count, active);
                        let extra = active - outer_root_depth;
                        if extra > 0 {
                            fg.emit_pop_roots_n(extra);
                        }
                        fg.builder.ins().jump(scope.resume, &[]);
                    }
                    normal_reaches_resume = true;
                }
            }
            if let Some(scope) = &outer_panic_scope {
                fg.emit_shared_panic_cleanup(scope);
                fg.builder.seal_block(scope.cleanup);
                fg.panic_scopes.pop();
            }
            let recovery_reaches_resume = outer_panic_scope
                .as_ref()
                .is_some_and(|scope| fg.panic_recovery_targets.remove(&scope.resume));
            let falls_through = normal_reaches_resume || recovery_reaches_resume;
            if let Some(scope) = outer_panic_scope {
                fg.gc_root_count = outer_root_depth;
                fg.coop_shadow_roots
                    .as_mut()
                    .expect("poll function root tracker")
                    .active
                    .truncate(outer_root_depth);
                if falls_through {
                    fg.builder.switch_to_block(scope.resume);
                    fg.builder.seal_block(scope.resume);
                    fg.terminated = false;
                }
            }
            // Fell off the end of the body → the task is Ready.
            if falls_through {
                fg.emit_coop_unwind_poll_roots();
                let ready = fg.builder.ins().iconst(types::I32, 1);
                fg.builder.ins().return_(&[ready]);
            }
            fg.defer_stack.pop();
            defer_sites = std::mem::take(&mut fg.collected_defer_sites);
            lock_sites = std::mem::take(&mut fg.collected_lock_sites);
            coop_root_slots = std::mem::take(
                &mut fg
                    .coop_shadow_roots
                    .as_mut()
                    .expect("poll function root tracker")
                    .all,
            );
        }

        // Dispatch on the state word (offset 0): state 0 → body_start,
        // state k → a trampoline that restores suspends[k-1]'s roots → resume.
        builder.switch_to_block(dispatch);
        // A resumed poll has a fresh native stack. Clear every local root slot
        // before registering any of them so a collection cannot interpret stale
        // stack bytes as object pointers. A later assignment updates the rooted
        // slot normally.
        let null = builder.ins().iconst(types::I64, 0);
        for slot in &coop_root_slots {
            builder.ins().stack_store(ptr_ty, null, *slot, 0);
        }
        let state = builder
            .ins()
            .load(types::I64, MemFlagsData::new(), frame, 0i32);
        for (k, suspend) in suspends.iter().enumerate() {
            let want = builder.ins().iconst(types::I64, (k + 1) as i64);
            let is_k = builder.ins().icmp(IntCC::Equal, state, want);
            let restore = builder.create_block();
            let next = builder.create_block();
            builder.ins().brif(is_k, restore, &[], next, &[]);
            builder.switch_to_block(restore);
            let push_id = self.func_id("willow_push_root");
            let push_ref = self.module.declare_func_in_func(push_id, builder.func);
            let ptr_ty = self.module.target_config().pointer_type();
            for slot in &suspend.roots {
                let addr = builder.ins().stack_addr(ptr_ty, *slot, 0);
                builder.ins().call(push_ref, &[addr]);
            }
            builder.ins().jump(suspend.resume, &[]);
            builder.switch_to_block(next);
        }
        builder.ins().jump(body_start, &[]);
        builder.seal_all_blocks();

        builder.finalize(self.module.target_config());
        self.module
            .define_function(func_id, &mut ctx)
            .map_err(|e| {
                if std::env::var("WILLOW_VERIFY_DEBUG").is_ok() {
                    eprintln!("[verify] {e:?}");
                }
                e
            })?;
        self.module.clear_context(&mut ctx);
        Ok((defer_sites, lock_sites))
    }

    /// Emit the compiler-generated cancellation cleanup entry
    /// `extern "C" fn(frame)` for an async fn (willow-vynv.3). Deferred actions
    /// and lexical locks share one registration order; walking it backwards
    /// gives the same unwind order as an ordinary exit:
    ///
    /// `inner defers -> commit/release inner lock -> enclosing defers`.
    pub(super) fn compile_async_cancel_fn(
        &mut self,
        cancel_fid: cranelift_module::FuncId,
        sites: &[AsyncDeferSite],
        lock_sites: &[AsyncLockSite],
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
                sync_defer_flags: HashMap::new(),
                panic_scopes: Vec::new(),
                unavailable_defer_ids: HashSet::new(),
                panic_defer_codegen_depth: 0,
                recover_eligible_depth: 0,
                panic_recovery_targets: HashSet::new(),
                panic_return_block: None,
                panic_function_root_depth: None,
                callstack_frame_depth: 0,
                fault_site_span: None,
                collected_defer_sites: Vec::new(),
                lock_scopes: Vec::new(),
                collected_lock_sites: Vec::new(),
                collected_cleanup_order: 0,
                module: &mut self.module,
                gc_tlab_state: self.gc_tlab_state,
                func_ids: &self.func_ids,
                func_return_types: &self.func_return_types,
                fn_types: &self.fn_types,
                func_param_modes: &self.func_param_modes,
                func_param_debug: &self.func_param_debug,
                function_may_panic: &self.function_may_panic,
                known_modules: &self.known_modules,
                visible_modules: &self.visible_modules,
                builtin_module_aliases: &self.builtin_module_aliases,
                lambda_names: &self.lambda_names,
                cooperative_leaves: &self.cooperative_leaves,
                string_literals: &self.string_literals,
                class_layouts: &self.class_layouts,
                static_storage: &self.static_storage,
                enum_infos: &self.enum_infos,
                class_base: &self.class_base,
                class_type_ids: &self.class_type_ids,
                class_descriptor_ids: &self.class_descriptor_ids,
                class_vslots: &self.class_vslots,
                interface_infos: &self.interface_infos,
                vtable_ids: &self.vtable_ids,
                expr_types: &self.expr_types,
                coop_frame: None,
                coop_result_offset: None,
                enum_variant_resolutions: &self.enum_variant_resolutions,
                pattern_resolutions: &self.pattern_resolutions,
                async_frame: Some(frame),
                async_frame_offsets: HashMap::new(),
                lir_frame_offsets: HashMap::new(),
                lir_defer_offsets: HashMap::new(),
                lir_hoisted_await: None,
                main_result_err_ty: None,
                vars: HashMap::new(),
                return_type: return_type.clone(),
                current_class: None,
                is_async: false,
                terminated: false,
                gc_root_count: 0,
                coop_shadow_roots: None,
                build_mode: self.build_mode,
                source_file: &self.source_file,
                // The cancel path runs cleanup only; it compiles no user body.
                address_taken: HashSet::new(),
            };
            // A single order is essential here. Running all defers before all
            // locks would execute an enclosing defer while an inner lock was
            // still held, unlike every normal exit path.
            let mut cleanup_events = Vec::with_capacity(sites.len() + lock_sites.len());
            cleanup_events.extend(
                sites
                    .iter()
                    .enumerate()
                    .map(|(index, site)| (site.order, false, index)),
            );
            cleanup_events.extend(
                lock_sites
                    .iter()
                    .enumerate()
                    .map(|(index, site)| (site.order, true, index)),
            );
            cleanup_events.sort_unstable_by_key(|(order, ..)| std::cmp::Reverse(*order));

            for (_, is_lock, index) in cleanup_events {
                if is_lock {
                    let site = &lock_sites[index];
                    fg.emit_lock_frame_cleanup(
                        site.mode,
                        [
                            site.handle_offset,
                            site.token_offset,
                            site.phase_offset,
                            site.value_offset,
                        ],
                        &site.value_ty,
                        true,
                    );
                    continue;
                }

                let site = &sites[index];
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
                // Consume the registration before user cleanup runs. This is
                // the exactly-once handoff required by recoverable unwinding:
                // a panic inside the cleanup must not leave it REGISTERED.
                let zero = fg.builder.ins().iconst(types::I64, 0);
                fg.builder
                    .ins()
                    .store(MemFlagsData::new(), zero, frame, site.flag_offset);
                let panic_next = fg.builder.create_block();
                fg.panic_return_block = Some(panic_next);
                fg.emit_void_runtime_call("willow_panic_enter_defer", &[]);
                fg.panic_defer_codegen_depth = 1;
                fg.recover_eligible_depth = usize::from(site.recovery_capable);
                fg.emit_deferred_action(&site.action);
                if !fg.terminated {
                    fg.emit_void_runtime_call("willow_panic_leave_defer", &[]);
                    fg.builder.ins().jump(panic_next, &[]);
                }
                fg.builder.switch_to_block(panic_next);
                fg.builder.seal_block(panic_next);
                fg.terminated = false;
                fg.panic_return_block = None;
                fg.panic_defer_codegen_depth = 0;
                fg.recover_eligible_depth = 0;
                fg.builder.ins().jump(skip_b, &[]);
                fg.builder.switch_to_block(skip_b);
                fg.builder.seal_block(skip_b);
            }
            fg.builder.ins().return_(&[]);
        }
        builder.seal_all_blocks();
        builder.finalize(self.module.target_config());
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
    /// Record a GC binding whose storage is a native poll-function stack slot.
    /// Such bindings are the only shadow roots allowed to survive until a
    /// cooperative suspension boundary; expression temporaries must have been
    /// popped before the boundary is emitted.
    pub(super) fn track_coop_binding_root(&mut self, slot: cranelift_codegen::ir::StackSlot) {
        let Some(roots) = self.coop_shadow_roots.as_mut() else {
            return;
        };
        roots.active.push(slot);
        roots.all.push(slot);
    }

    pub(super) fn coop_root_depth(&self) -> usize {
        self.coop_shadow_roots
            .as_ref()
            .map_or(0, |roots| roots.active.len())
    }

    /// A poll return destroys the native stack frame. Pop every active binding
    /// root before returning so the runtime never retains an address into that
    /// dead frame. Compile-time root state is intentionally left unchanged: the
    /// non-suspending CFG edge still reaches the common continuation with those
    /// roots registered, while the dispatch trampoline restores them on re-poll.
    pub(super) fn emit_coop_unwind_poll_roots(&mut self) {
        let active = self.coop_root_depth();
        assert_eq!(
            self.gc_root_count, active,
            "cooperative suspension reached with an untracked temporary GC root"
        );
        self.emit_pop_roots_n(active);
    }

    pub(super) fn record_coop_suspend(
        &mut self,
        suspends: &mut Vec<CoopSuspendPoint>,
        resume: cranelift_codegen::ir::Block,
    ) {
        let roots = self
            .coop_shadow_roots
            .as_ref()
            .expect("cooperative suspend outside a poll function")
            .active
            .clone();
        suspends.push(CoopSuspendPoint { resume, roots });
    }

    /// Emit a preemption check whose resumed poll continues at `resume`. A
    /// tripped check records that block as the frame state and returns
    /// `RUNTIME_POLL_PREEMPTED`; otherwise execution branches there directly.
    /// Cooperative locals are frame-backed, so no SSA values cross the boundary.
    fn emit_coop_safepoint_to(
        &mut self,
        suspends: &mut Vec<CoopSuspendPoint>,
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
        self.emit_coop_unwind_poll_roots();
        let preempted = self.builder.ins().iconst(types::I32, COOP_POLL_PREEMPTED);
        self.builder.ins().return_(&[preempted]);
        self.record_coop_suspend(suspends, resume);
    }

    /// Safepoint at a source statement boundary. Resumption targets the fresh
    /// continuation after the check, so a budget of one cannot repeatedly
    /// preempt at the same statement without executing it.
    pub(super) fn emit_coop_statement_safepoint(
        &mut self,
        suspends: &mut Vec<CoopSuspendPoint>,
        frame: cranelift_codegen::ir::Value,
    ) {
        let continuation = self.builder.create_block();
        self.emit_coop_safepoint_to(suspends, frame, continuation);
        self.builder.switch_to_block(continuation);
    }

    /// Suspend the current poll after a scheduler operation has registered its
    /// wakeup. Shared by AST and LIR lowering so their state numbering, root
    /// unwind, and resume registration cannot drift apart.
    fn finish_coop_builtin_suspend(
        &mut self,
        suspends: &mut Vec<CoopSuspendPoint>,
        frame: cranelift_codegen::ir::Value,
    ) {
        let state = (suspends.len() + 1) as i64;
        let st = self.builder.ins().iconst(types::I64, state);
        self.builder
            .ins()
            .store(MemFlagsData::new(), st, frame, 0i32);
        self.emit_coop_unwind_poll_roots();
        let pending = self.builder.ins().iconst(types::I32, 0);
        self.builder.ins().return_(&[pending]);
        let resume = self.builder.create_block();
        self.record_coop_suspend(suspends, resume);
        self.builder.switch_to_block(resume);
    }

    pub(super) fn emit_coop_sleep_value(
        &mut self,
        millis: cranelift_codegen::ir::Value,
        suspends: &mut Vec<CoopSuspendPoint>,
        frame: cranelift_codegen::ir::Value,
    ) {
        let sleep_fid = self.func_id("willow_sched_sleep");
        let sleep_ref = self
            .module
            .declare_func_in_func(sleep_fid, self.builder.func);
        self.builder.ins().call(sleep_ref, &[millis]);
        self.finish_coop_builtin_suspend(suspends, frame);
    }

    pub(super) fn emit_coop_yield(
        &mut self,
        suspends: &mut Vec<CoopSuspendPoint>,
        frame: cranelift_codegen::ir::Value,
    ) {
        let yield_fid = self.func_id("willow_sched_yield");
        let yield_ref = self
            .module
            .declare_func_in_func(yield_fid, self.builder.func);
        self.builder.ins().call(yield_ref, &[]);
        self.finish_coop_builtin_suspend(suspends, frame);
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
        self.emit_value_runtime_call(runtime_name, &[value])
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
        self.emit_value_runtime_call(runtime_name, &[future])
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

    /// The suspension core shared by every frame-backed await: `await
    /// <cooperative-leaf-call>`, `await <task>`, and their LIR counterparts
    /// (willow-0g8j.2.11).
    ///
    /// `awaited` is the callee/task frame, already evaluated. When `stored_slot`
    /// is `Some`, the frame is stashed there and RELOADED in the resume block —
    /// the native stack is gone after a park, and re-evaluating the awaited
    /// expression could call a function twice or select a different task
    /// (willow-0a6k.6). The reloaded frame is what this returns; `None` means
    /// the caller owns the reload, which is only correct for `await <var>`,
    /// where the local is itself frame-backed.
    ///
    /// On return the builder is positioned in the resume block, reached both
    /// from the scheduler dispatch on wake and from the already-terminal branch.
    pub(super) fn emit_coop_frame_await(
        &mut self,
        awaited: cranelift_codegen::ir::Value,
        stored_slot: Option<i32>,
        spawn_site_line: Option<usize>,
        suspends: &mut Vec<CoopSuspendPoint>,
        frame: cranelift_codegen::ir::Value,
    ) -> Option<cranelift_codegen::ir::Value> {
        if let Some(offset) = stored_slot {
            self.emit_gc_heap_store_classified(
                frame,
                offset,
                awaited,
                true,
                GcStoreDestination::AsyncFrameSlot,
            );
        }
        // id = awaited[TASK_ID] (slot 1).
        let id = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            awaited,
            async_frame_slot_offset(FRAME_SLOT_TASK_ID),
        );
        if let Some(line) = spawn_site_line {
            self.emit_set_spawn_site(id, line);
        }
        // done = willow_frame_await(awaited, id): 1 = already terminal,
        // 0 = registered as a waiter. The frame's own header answers "already
        // terminal?" without a scheduler lookup (willow-ezs.1.3); the id is
        // still needed to register this task when it is not.
        let await_fid = self.func_id("willow_frame_await");
        let await_ref = self
            .module
            .declare_func_in_func(await_fid, self.builder.func);
        let dcall = self.builder.ins().call(await_ref, &[awaited, id]);
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
        self.emit_coop_unwind_poll_roots();
        let pending = self.builder.ins().iconst(types::I32, 0);
        self.builder.ins().return_(&[pending]);
        self.record_coop_suspend(suspends, resume_b);
        self.builder.switch_to_block(resume_b);
        stored_slot.map(|offset| {
            self.builder
                .ins()
                .load(types::I64, MemFlagsData::new(), frame, offset)
        })
    }

    /// Read a completed callee frame's RESULT slot. A CANCELLED callee has no
    /// result to read, so the terminal state is resolved first — the same
    /// located panic as `await` (willow-vynv.1), instead of reading garbage.
    pub(super) fn emit_coop_awaited_result(
        &mut self,
        awaited: cranelift_codegen::ir::Value,
        result_ty: Option<&Type>,
        // `true` only for `await t.result()`, whose static type asks for a
        // `Result<T, Cancelled>` value instead of a located panic.
        cancel_aware: bool,
    ) -> Option<cranelift_codegen::ir::Value> {
        let cid = self.builder.ins().load(
            types::I64,
            MemFlagsData::new(),
            awaited,
            async_frame_slot_offset(FRAME_SLOT_TASK_ID),
        );
        // A plain `await` is not cancellation-aware: a cancelled callee is a
        // located panic, not a value. `await t.result()` opts out of that.
        self.emit_task_terminal_value(awaited, cid, result_ty.unwrap_or(&Type::Void), cancel_aware)
    }

    /// The LIR spelling of an acquisition (willow-0g8j.2.13): the same
    /// acquire/park/poll/owned state machine [`Self::emit_coop_lock`] builds,
    /// driven by frame offsets the LIR walker owns rather than by an AST
    /// statement.
    ///
    /// Three things the AST path does are already done by the time this runs.
    /// The handle was evaluated and stored by the `LirInst::Let` the lowerer
    /// hoisted the target into. The binding is a LIR local whose frame slot is
    /// bound in `vars` for the whole function, so no name is shadowed and none
    /// has to be restored. And the critical section is a separate LIR block,
    /// entered by the `jump` every suspension emits after this returns — which
    /// is why this leaves the builder in `owned_b` with the path still open.
    ///
    /// What it keeps is everything that is not a body: the status chain, the
    /// reentrancy fault, and the park/re-poll edges. The two cleanup
    /// registrations the AST path makes here are made by the body's
    /// `EnterDeferScope` instead — see the walker — because a suspension split
    /// can put this acquisition in a LATER block than the section it opens, and
    /// both registrations are ordered by emission.
    ///
    /// `offsets` are the section's frame slots in the order
    /// [`crate::ir::lowered::LirLockSlots::locals`] gives them: handle, token,
    /// phase, protected value.
    pub(super) fn emit_lir_lock_acquire(
        &mut self,
        mode: LockMode,
        offsets: [i32; 4],
        value_ty: &Type,
        header: crate::diagnostics::Span,
        suspends: &mut Vec<CoopSuspendPoint>,
        frame: cranelift_codegen::ir::Value,
    ) {
        let [handle_offset, token_offset, phase_offset, value_offset] = offsets;
        let (acquire_fn, poll_fn, load_fn, recursive_fn, invalid_fn) = match mode {
            LockMode::Mutex => (
                "willow_async_mutex_acquire",
                "willow_async_mutex_poll",
                "willow_async_mutex_load",
                "willow_async_mutex_recursive_panic",
                "willow_async_mutex_invalid_status",
            ),
            LockMode::Read | LockMode::Write => (
                "willow_async_rwlock_acquire",
                "willow_async_rwlock_poll",
                "willow_async_rwlock_load",
                "willow_async_rwlock_recursive_panic",
                "willow_async_rwlock_invalid_status",
            ),
        };
        let phase_idle = self.builder.ins().iconst(types::I64, 0);
        self.builder
            .ins()
            .store(MemFlagsData::new(), phase_idle, frame, phase_offset);

        let acquire_b = self.builder.create_block();
        let park_b = self.builder.create_block();
        let owned_b = self.builder.create_block();
        let poll_b = self.builder.create_block();
        self.builder.ins().jump(acquire_b, &[]);
        let park_state = (suspends.len() + 1) as i64;
        self.record_coop_suspend(suspends, poll_b);

        // --- acquire ---------------------------------------------------
        self.builder.switch_to_block(acquire_b);
        let handle = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), frame, handle_offset);
        let token_ptr = self.builder.ins().iadd_imm_s(frame, token_offset as i64);
        let status = if mode == LockMode::Mutex {
            self.emit_value_runtime_call(acquire_fn, &[handle, token_ptr])
        } else {
            let mode_value = self.builder.ins().iconst(
                types::I32,
                match mode {
                    LockMode::Read => 1,
                    LockMode::Write => 2,
                    LockMode::Mutex => unreachable!(),
                },
            );
            self.emit_value_runtime_call(acquire_fn, &[handle, mode_value, token_ptr])
        };
        let recursive_b = self.builder.create_block();
        let acquire_pending_b = self.builder.create_block();
        let acquire_not_recursive_b = self.builder.create_block();
        let acquire_not_cancelled_b = self.builder.create_block();
        let acquire_not_lost_b = self.builder.create_block();
        let acquire_invalid_b = self.builder.create_block();
        let acquired = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::Equal, status, MUTEX_STATUS_ACQUIRED);
        self.builder
            .ins()
            .brif(acquired, owned_b, &[], acquire_pending_b, &[]);
        self.builder.switch_to_block(acquire_pending_b);
        self.builder.seal_block(acquire_pending_b);
        let pending = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::Equal, status, MUTEX_STATUS_PENDING);
        self.builder
            .ins()
            .brif(pending, park_b, &[], acquire_not_recursive_b, &[]);
        self.builder.switch_to_block(acquire_not_recursive_b);
        self.builder.seal_block(acquire_not_recursive_b);
        let recursive = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::Equal, status, MUTEX_STATUS_RECURSIVE);
        self.builder
            .ins()
            .brif(recursive, recursive_b, &[], acquire_not_cancelled_b, &[]);
        self.builder.switch_to_block(acquire_not_cancelled_b);
        self.builder.seal_block(acquire_not_cancelled_b);
        let cancelled = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::Equal, status, MUTEX_STATUS_CANCELLED);
        self.builder
            .ins()
            .brif(cancelled, park_b, &[], acquire_not_lost_b, &[]);
        self.builder.switch_to_block(acquire_not_lost_b);
        self.builder.seal_block(acquire_not_lost_b);
        let lost = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::Equal, status, MUTEX_STATUS_LOST);
        self.builder
            .ins()
            .brif(lost, acquire_b, &[], acquire_invalid_b, &[]);

        self.builder.switch_to_block(acquire_invalid_b);
        self.builder.seal_block(acquire_invalid_b);
        let phase = self
            .builder
            .ins()
            .iconst(types::I32, MUTEX_STATUS_PHASE_ACQUIRE);
        self.emit_void_runtime_call(invalid_fn, &[status, phase]);
        self.builder.ins().trap(TrapCode::unwrap_user(1));

        // --- reentrancy fault ------------------------------------------
        self.builder.switch_to_block(recursive_b);
        let source_file = self.source_file.to_string();
        let file_ptr = self.emit_string_literal(&source_file);
        let line = self.builder.ins().iconst(types::I32, header.line as i64);
        let col = self.builder.ins().iconst(types::I32, header.col as i64);
        self.emit_void_runtime_call(recursive_fn, &[file_ptr, line, col]);
        self.builder.ins().trap(TrapCode::unwrap_user(1));

        // --- park -------------------------------------------------------
        self.builder.switch_to_block(park_b);
        let st = self.builder.ins().iconst(types::I64, park_state);
        self.builder
            .ins()
            .store(MemFlagsData::new(), st, frame, 0i32);
        self.emit_coop_unwind_poll_roots();
        let status_pending = self.builder.ins().iconst(types::I32, RUNTIME_POLL_PENDING);
        self.builder.ins().return_(&[status_pending]);

        // --- resumed poll ------------------------------------------------
        self.builder.switch_to_block(poll_b);
        let handle = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), frame, handle_offset);
        let token = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), frame, token_offset);
        let status = self.emit_value_runtime_call(poll_fn, &[handle, token]);
        let poll_pending_b = self.builder.create_block();
        let poll_not_pending_b = self.builder.create_block();
        let poll_not_recursive_b = self.builder.create_block();
        let poll_not_cancelled_b = self.builder.create_block();
        let poll_invalid_b = self.builder.create_block();
        let acquired = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::Equal, status, MUTEX_STATUS_ACQUIRED);
        self.builder
            .ins()
            .brif(acquired, owned_b, &[], poll_pending_b, &[]);
        self.builder.switch_to_block(poll_pending_b);
        self.builder.seal_block(poll_pending_b);
        let pending = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::Equal, status, MUTEX_STATUS_PENDING);
        self.builder
            .ins()
            .brif(pending, park_b, &[], poll_not_pending_b, &[]);
        self.builder.switch_to_block(poll_not_pending_b);
        self.builder.seal_block(poll_not_pending_b);
        let recursive = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::Equal, status, MUTEX_STATUS_RECURSIVE);
        self.builder
            .ins()
            .brif(recursive, recursive_b, &[], poll_not_recursive_b, &[]);
        self.builder.switch_to_block(poll_not_recursive_b);
        self.builder.seal_block(poll_not_recursive_b);
        let cancelled = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::Equal, status, MUTEX_STATUS_CANCELLED);
        self.builder
            .ins()
            .brif(cancelled, park_b, &[], poll_not_cancelled_b, &[]);
        self.builder.switch_to_block(poll_not_cancelled_b);
        self.builder.seal_block(poll_not_cancelled_b);
        let lost = self
            .builder
            .ins()
            .icmp_imm_s(IntCC::Equal, status, MUTEX_STATUS_LOST);
        self.builder
            .ins()
            .brif(lost, acquire_b, &[], poll_invalid_b, &[]);

        self.builder.switch_to_block(poll_invalid_b);
        self.builder.seal_block(poll_invalid_b);
        let phase = self
            .builder
            .ins()
            .iconst(types::I32, MUTEX_STATUS_PHASE_POLL);
        self.emit_void_runtime_call(invalid_fn, &[status, phase]);
        self.builder.ins().trap(TrapCode::unwrap_user(1));
        self.builder.seal_block(recursive_b);

        // --- owned --------------------------------------------------------
        self.builder.switch_to_block(owned_b);
        self.terminated = false;
        let handle = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), frame, handle_offset);
        let token = self
            .builder
            .ins()
            .load(types::I64, MemFlagsData::new(), frame, token_offset);
        let word = self.emit_value_runtime_call(load_fn, &[handle, token]);
        let value = self.coerce_i64_to(word, value_ty);
        self.emit_gc_heap_store(
            frame,
            value_offset,
            value,
            value_ty,
            GcStoreDestination::AsyncFrameSlot,
        );
        let phase_held = self.builder.ins().iconst(types::I64, 1);
        self.builder
            .ins()
            .store(MemFlagsData::new(), phase_held, frame, phase_offset);
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
            self.emit_void_runtime_call("willow_frame_await_check", &[task_frame, task_id]);
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
            .band_imm_s(status, WILLOW_FRAME_STATUS_TERMINAL_MASK);
        let is_cancelled =
            self.builder
                .ins()
                .icmp_imm_s(IntCC::Equal, terminal, WILLOW_FRAME_STATUS_CANCELLED);

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

    /// Register a `select` case's stashed channel pointer as a shadow-stack
    /// root. Channels are GC objects, so the stash is a real reference; the
    /// type guard keeps this uniform with the other stash slots.
    fn root_select_channel_slot(&mut self, channel: &Expr, slot: cranelift_codegen::ir::StackSlot) {
        let ty = self.ast_type_of(channel);
        if is_gc_managed(&ty, self.enum_infos) {
            self.emit_push_root_slot(slot);
        }
    }

    /// Eager (block-driving) `select` (willow-7aj): probe each case in source
    /// order — a recv case is ready when its channel has a value or is closed; a
    /// send case is ready while its channel has room (always, when unbounded).
    /// If none is ready and there is a `default`, it runs; otherwise the scheduler
    /// is driven and the probe retried (giving up if no task could progress). In a
    /// non-task context recv_ready does not register a waiter (current task is 0),
    /// so it is a pure readiness probe here.
    ///
    /// This is the AST emitter's `select`. Async bodies are lowered through
    /// `lir_gen`, so the only body that still reaches it is a synchronous one —
    /// a static-field initialiser — and a task-await case cannot appear there:
    /// the parser only builds one from `await`, which E0801 rejects outside an
    /// async function.
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
                    self.stack_store(ch, slot);
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
                    self.stack_store(ch, slot);
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
                    self.stack_store(v, vslot);
                    let elem_ty = channel_element_type(&self.ast_type_of(channel))
                        .expect("internal compiler error: missing checked payload type");
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
                    self.stack_store(deadline, slot);
                    chan_slots.push(None);
                    aux_slots.push(Some(slot));
                }
                SelectCaseKind::Join { .. } => {
                    unreachable!("select task-await case outside an async fn is rejected by E0801")
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
                    let ch = self.stack_load(types::I64, slot);
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
                    let ch = self.stack_load(types::I64, slot);
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
                    let deadline = self.stack_load(types::I64, slot);
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
                    unreachable!("select task-await case outside an async fn is rejected by E0801")
                }
                SelectCaseKind::Default => ready_flags.push(None),
            }
        }
        let mut total = self.builder.ins().iconst(types::I64, 0);
        for flag in ready_flags.iter().flatten() {
            total = self.builder.ins().iadd(total, *flag);
        }
        let none_ready = self.builder.ins().icmp_imm_s(IntCC::Equal, total, 0);
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
            let k1 = self.builder.ins().iadd_imm_s(k, 1);
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
                    // A select with no default and no timeout BLOCKS until a
                    // case is ready; it must never fall through to the merge
                    // block, which would run no case at all and continue past
                    // the select with its bindings unwritten. A drive that
                    // completed nothing is not proof that no case can become
                    // ready, so idle-wait and re-probe; the runtime raises a
                    // blocking-forever panic when there is genuinely no wake
                    // source left (willow-atth).
                    let idle_wait_b = self.builder.create_block();
                    self.builder
                        .ins()
                        .brif(progressed, loop_b, &[], idle_wait_b, &[]);
                    self.builder.switch_to_block(idle_wait_b);
                    self.builder.seal_block(idle_wait_b);
                    self.emit_void_runtime_call("willow_select_idle_wait", &[]);
                    self.builder.ins().jump(loop_b, &[]);
                } else {
                    // With a timeout case the drive MUST be bounded by the
                    // NEAREST deadline: an unbounded `willow_sched_run` runs
                    // unrelated tasks to quiescence first, so a 30ms timeout
                    // could lose to a 5s task (willow-o038 review).
                    let mut min_deadline: Option<cranelift_codegen::ir::Value> = None;
                    for slot in timeout_slots {
                        let d = self.stack_load(types::I64, slot);
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
                    let elem_ty = channel_element_type(&self.ast_type_of(channel))
                        .expect("internal compiler error: missing checked payload type");
                    let slot = chan_slots[i].expect("recv case has a channel slot");
                    let ch = self.stack_load(types::I64, slot);
                    let recv_name =
                        format!("willow_channel_recv_{}", channel_runtime_suffix(&elem_ty));
                    let v = self.emit_value_runtime_call(&recv_name, &[ch]);
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
                    let elem_ty = channel_element_type(&self.ast_type_of(channel))
                        .expect("internal compiler error: missing checked payload type");
                    let slot = chan_slots[i].expect("send case has a channel slot");
                    let ch = self.stack_load(types::I64, slot);
                    let vslot = aux_slots[i].expect("send case has a value slot");
                    let val = self.stack_load(clif_type(&elem_ty), vslot);
                    // Non-blocking: the probe said not-full, but a task on
                    // another worker may have filled the channel since. Retry
                    // the whole probe instead of blocking (willow-o038).
                    let try_name = format!(
                        "willow_channel_try_send_{}",
                        channel_runtime_suffix(&elem_ty)
                    );
                    let try_fid = self.func_id(&try_name);
                    let try_ref = self.module.declare_func_in_func(try_fid, self.builder.func);
                    let scall = self.builder.ins().call(try_ref, &[ch, val]);
                    let sent = self.builder.inst_results(scall)[0];
                    let body_b = self.builder.create_block();
                    let sent_ok = self.builder.ins().icmp_imm_s(IntCC::NotEqual, sent, 0);
                    self.builder.ins().brif(sent_ok, body_b, &[], loop_b, &[]);
                    self.builder.switch_to_block(body_b);
                    self.builder.seal_block(body_b);
                    self.emit_block(&case.body);
                }
                SelectCaseKind::Timeout { .. } => self.emit_block(&case.body),
                SelectCaseKind::Join { .. } => {
                    unreachable!("select task-await case outside an async fn is rejected by E0801")
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

#[cfg(test)]
mod async_mutex_abi_tests {
    use super::*;

    /// Perspective 1: the values codegen compares against are the WIRE values
    /// the runtime returns. Asserting the literals rather than re-deriving them
    /// from `LockAcquireStatus` is the whole point: the constants are defined
    /// as those enum casts, so comparing them to the same casts would hold for
    /// any renumbering. Change a discriminant and this fails here and in
    /// `willow_abi`'s own test, which is what forces the runtime to be updated
    /// with it.
    #[test]
    fn status_constants_are_the_documented_wire_values() {
        assert_eq!(MUTEX_STATUS_ACQUIRED, 1);
        assert_eq!(MUTEX_STATUS_PENDING, 0);
        assert_eq!(MUTEX_STATUS_RECURSIVE, -1);
        assert_eq!(MUTEX_STATUS_LOST, -2);
        assert_eq!(MUTEX_STATUS_CANCELLED, -3);
    }

    /// Perspective 2: the five statuses must stay mutually distinct, or the
    /// `brif` chain in the acquisition lowering folds two outcomes into one.
    #[test]
    fn status_constants_are_distinct() {
        let all = [
            MUTEX_STATUS_ACQUIRED,
            MUTEX_STATUS_PENDING,
            MUTEX_STATUS_RECURSIVE,
            MUTEX_STATUS_LOST,
            MUTEX_STATUS_CANCELLED,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b, "async-mutex statuses must be distinct: {all:?}");
            }
        }
    }

    /// Perspective 3: `park_b` returns the scheduler's Pending code, whose wire
    /// value is 0. Pinned as a literal for the reason perspective 1 gives.
    #[test]
    fn poll_pending_is_the_documented_wire_value() {
        assert_eq!(RUNTIME_POLL_PENDING, 0);
    }

    /// Perspective 4: PENDING is shared by both protocols — the acquisition
    /// status and the poll status. The lowering reuses one comparison for both,
    /// so if they ever diverge that shortcut becomes a bug.
    #[test]
    fn pending_status_and_poll_pending_agree() {
        assert_eq!(MUTEX_STATUS_PENDING, RUNTIME_POLL_PENDING);
    }

    /// Perspective 5: the fail-closed phase tags a bad status is reported with.
    /// Pinned as literals for the reason perspective 1 gives — the runtime
    /// prints these, so renumbering them silently changes a diagnostic.
    #[test]
    fn invalid_status_phase_constants_are_the_documented_wire_values() {
        assert_eq!(MUTEX_STATUS_PHASE_ACQUIRE, 0);
        assert_eq!(MUTEX_STATUS_PHASE_POLL, 1);
    }

    /// Perspective 6: acquire and poll diagnostics must remain distinguishable.
    #[test]
    fn invalid_status_phase_constants_are_distinct() {
        assert_ne!(MUTEX_STATUS_PHASE_ACQUIRE, MUTEX_STATUS_PHASE_POLL);
    }
}
