//! Single source of truth for the runtime ABI surface imported by the backend.
//!
//! Every runtime symbol the Cranelift backend calls into `libwillow_runtime`
//! is listed in [`RUNTIME_SYMBOLS`] together with its parameter, return, and
//! effect kinds. `Codegen::declare_runtime` iterates over this table instead of
//! hand-writing one `declare_function` block per symbol, so the backend's view
//! of the ABI lives in exactly one place.
//!
//! Integration link tests keep this table and the actual exported staticlib
//! symbols in sync.

use cranelift_codegen::ir::{AbiParam, Type, types};
pub use willow_abi::{AbiTy, RuntimeEffects};

/// Lower a target-independent ABI representation to Cranelift.
fn clif_abi_ty(ty: AbiTy, ptr_ty: Type) -> Type {
    match ty {
        AbiTy::Word | AbiTy::I64 => types::I64,
        AbiTy::I32 => types::I32,
        AbiTy::I8 => types::I8,
        AbiTy::F64 => types::F64,
        AbiTy::Ptr => ptr_ty,
    }
}

/// One runtime ABI symbol imported by the backend with `Linkage::Import`.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeSymbol {
    /// Exported C symbol name in `libwillow_runtime`.
    pub name: &'static str,
    /// Parameter kinds in declaration order.
    pub params: &'static [AbiTy],
    /// Return kind, or `None` for `void`.
    pub ret: Option<AbiTy>,
    /// Scheduler, GC, and recoverable-panic effects. This lives beside the
    /// signature deliberately: adding an ABI row without classifying its
    /// effects is a macro error, rather than silently falling through to
    /// `NONE` in a second name-based table.
    pub effects: RuntimeEffects,
}

impl RuntimeSymbol {
    /// Push this symbol's parameters and return onto a Cranelift signature.
    ///
    /// The caller supplies a signature created via `Module::make_signature`
    /// (which carries the module's default call convention) and the module's
    /// pointer type for lowering [`AbiTy::Ptr`].
    pub fn fill_signature(&self, sig: &mut cranelift_codegen::ir::Signature, ptr_ty: Type) {
        for param in self.params {
            sig.params.push(AbiParam::new(clif_abi_ty(*param, ptr_ty)));
        }
        if let Some(ret) = self.ret {
            sig.returns.push(AbiParam::new(clif_abi_ty(ret, ptr_ty)));
        }
    }

    /// Scheduler/GC effects used when deciding whether generated code may keep
    /// an unrooted value across this runtime call (preemption spec §21).
    pub const fn effects(&self) -> RuntimeEffects {
        self.effects
    }
}

const NONE: RuntimeEffects = RuntimeEffects::NONE;
const ALLOC: RuntimeEffects = RuntimeEffects::MAY_ALLOCATE;
const BLOCK: RuntimeEffects = RuntimeEffects::MAY_BLOCK;
const PANIC_ALLOC: RuntimeEffects = RuntimeEffects::MAY_PANIC.union(RuntimeEffects::MAY_ALLOCATE);
const BLOCK_ALLOC: RuntimeEffects = RuntimeEffects::MAY_BLOCK.union(RuntimeEffects::MAY_ALLOCATE);
const BLOCK_PANIC_ALLOC: RuntimeEffects = RuntimeEffects::MAY_BLOCK
    .union(RuntimeEffects::MAY_PANIC)
    .union(RuntimeEffects::MAY_ALLOCATE);
const SUSPEND: RuntimeEffects = RuntimeEffects::MAY_SUSPEND;
const PREEMPT: RuntimeEffects = RuntimeEffects::MAY_PREEMPT;
const NO_PREEMPT: RuntimeEffects = RuntimeEffects::NO_PREEMPT_REGION;

use AbiTy::{F64, I8, I32, I64, Ptr, Word};

/// Declare the backend-facing runtime ABI once and generate the typed table
/// consumed by Cranelift. Keeping the compact signatures in one invocation
/// makes additions reviewable and prevents signatures or effects from
/// drifting into separate name-based registries. Every row must state an
/// effect constant explicitly; there is no fail-open default.
macro_rules! runtime_abi_schema {
    ($($effects:ident; $name:literal => ([$($param:ident),* $(,)?] -> $ret:expr);)*) => {
        &[
            $(RuntimeSymbol {
                name: $name,
                params: &[$($param),*],
                ret: $ret,
                effects: $effects,
            },)*
        ]
    };
}

/// The complete set of runtime symbols the backend imports.
///
/// This is the generated-code-facing ABI surface; runtime-only symbols are
/// called from within the runtime and are not emitted by the backend.
pub const RUNTIME_SYMBOLS: &[RuntimeSymbol] = runtime_abi_schema! {
    // --- print ---
    NONE; "willow_print_i64" => ([I64] -> None);
    NONE; "willow_println_i64" => ([I64] -> None);
    NONE; "willow_print_bool" => ([I8] -> None);
    NONE; "willow_println_bool" => ([I8] -> None);
    NONE; "willow_print_f64" => ([F64] -> None);
    NONE; "willow_println_f64" => ([F64] -> None);
    NONE; "willow_print_string" => ([Word] -> None);
    NONE; "willow_println_string" => ([Word] -> None);
    // --- math / float formatting ---
    PANIC_ALLOC; "willow_pow_negative_exponent" => ([I64, Ptr, I32, I32] -> None);
    NONE; "willow_f64_to_string" => ([F64] -> Some(Word));
    NONE; "willow_i64_to_string" => ([I64] -> Some(Word));
    NONE; "willow_bool_to_string" => ([I8] -> Some(Word));
    NONE; "willow_f64_parse" => ([Word] -> Some(Word));
    NONE; "willow_format_f64_17g" => ([F64] -> Some(Word));
    NONE; "willow_format_f64_16f" => ([F64] -> Some(Word));
    NONE; "willow_format_f64_6f" => ([F64] -> Some(Word));
    // --- string ---
    ALLOC; "willow_string_concat" => ([Word, Word] -> Some(Word));
    NONE; "willow_string_eq" => ([Word, Word] -> Some(I64));
    ALLOC; "willow_string_alloc" => ([Ptr, I64] -> Some(Word));
    ALLOC; "willow_string_literal" => ([Ptr, I64] -> Some(Word));
    // --- args ---
    NONE; "willow_runtime_args_len" => ([] -> Some(I64));
    NONE; "willow_runtime_arg" => ([I64] -> Some(Word));
    NONE; "willow_runtime_program_name" => ([] -> Some(Word));
    NONE; "willow_runtime_args_array" => ([] -> Some(Word));
    // --- GC allocation ---
    ALLOC; "willow_alloc" => ([I64] -> Some(Word));
    ALLOC; "willow_alloc_typed" => ([I64, I64] -> Some(Word));
    ALLOC; "willow_gc_alloc_layout" => ([I64, I64, I64, I64] -> Some(Word));
    ALLOC; "willow_gc_alloc_slow" => ([Ptr, I64, I64, I64, I64] -> Some(Word));
    NONE; "willow_gc_write_barrier" => ([Ptr, Word, I64] -> None);
    PREEMPT; "willow_gc_collect" => ([] -> None);
    PREEMPT; "willow_gc_minor_collect" => ([] -> None);
    NONE; "willow_gc_allocated_bytes" => ([] -> Some(I64));
    NONE; "willow_gc_tlab_fast_allocations" => ([] -> Some(I64));
    NONE; "willow_gc_tlab_slow_allocations" => ([] -> Some(I64));
    NONE; "willow_gc_tlab_refills" => ([] -> Some(I64));
    NONE; "willow_gc_tlab_large_allocations" => ([] -> Some(I64));
    NONE; "willow_gc_tlab_reserved_bytes" => ([] -> Some(I64));
    NONE; "willow_gc_minor_collections" => ([] -> Some(I64));
    NONE; "willow_gc_promoted_objects" => ([] -> Some(I64));
    NONE; "willow_gc_moved_objects" => ([] -> Some(I64));
    NONE; "willow_gc_remembered_set_size" => ([] -> Some(I64));
    NONE; "willow_gc_dirty_card_count" => ([] -> Some(I64));
    NONE; "willow_gc_write_barrier_hits" => ([] -> Some(I64));
    NONE; "willow_gc_old_region_count" => ([] -> Some(I64));
    NONE; "willow_gc_old_region_reserved_bytes" => ([] -> Some(I64));
    NONE; "willow_gc_old_region_live_bytes" => ([] -> Some(I64));
    NONE; "willow_gc_old_region_fragmentation_bytes" => ([] -> Some(I64));
    NONE; "willow_gc_large_object_region_count" => ([] -> Some(I64));
    NONE; "willow_gc_pinned_region_count" => ([] -> Some(I64));
    NONE; "willow_gc_old_region_allocations" => ([] -> Some(I64));
    NONE; "willow_gc_old_region_reuses" => ([] -> Some(I64));
    NONE; "willow_gc_old_regions_released" => ([] -> Some(I64));
    NONE; "willow_gc_major_collections" => ([] -> Some(I64));
    // --- multi-mutator coordination (willow-6fv.5.6) ---
    NONE; "willow_gc_register_mutator" => ([] -> None);
    NONE; "willow_gc_unregister_mutator" => ([] -> None);
    PREEMPT; "willow_gc_safepoint" => ([] -> None);
    // --- arrays (std::collections::Array) ---
    PANIC_ALLOC; "willow_array_new" => ([I64, I64] -> Some(Word));
    PANIC_ALLOC; "willow_array_copy" => ([Word] -> Some(Word));
    PANIC_ALLOC; "willow_array_len" => ([Word] -> Some(I64));
    PANIC_ALLOC; "willow_array_get" => ([Word, I64] -> Some(Word));
    PANIC_ALLOC; "willow_array_set" => ([Word, I64, Word] -> None);
    PANIC_ALLOC; "willow_array_push" => ([Word, Word] -> None);
    PANIC_ALLOC; "willow_array_pop" => ([Word] -> Some(Word));
    PANIC_ALLOC; "willow_array_to_string" => ([Word, I64] -> Some(Word));
    NONE; "willow_map_to_string" => ([Word, I64] -> Some(Word));
    PANIC_ALLOC; "willow_array_element_addr" => ([Word, I64] -> Some(Ptr));
    // --- maps (std::collections::Map) ---
    NONE; "willow_map_new" => ([] -> Some(Word));
    NONE; "willow_map_copy" => ([Word] -> Some(Word));
    NONE; "willow_map_insert" => ([Word, Word, Word, I64, I64] -> None);
    ALLOC; "willow_map_get" => ([Word, Word, I64, I64] -> Some(Word));
    NONE; "willow_map_len" => ([Word] -> Some(I64));
    NONE; "willow_map_contains" => ([Word, Word, I64] -> Some(I64));
    // --- timer ---
    NONE; "willow_runtime_sleep" => ([I64] -> Some(Word));
    NONE; "willow_runtime_yield" => ([] -> Some(Word));
    // --- netpoll ---
    NONE; "willow_netpoll_init" => ([] -> Some(I32));
    NONE; "willow_netpoll_register" => ([I64, I32] -> Some(I32));
    NONE; "willow_netpoll_reregister" => ([I64, I32] -> Some(I32));
    NONE; "willow_netpoll_deregister" => ([I64] -> Some(I32));
    SUSPEND; "willow_netpoll_wait" => ([I64] -> Some(I64));
    NONE; "willow_netpoll_wake" => ([I64] -> Some(I64));
    // --- futures ---
    NONE; "willow_future_ready_void" => ([] -> Some(Word));
    NONE; "willow_future_ready_i64" => ([I64] -> Some(Word));
    NONE; "willow_future_ready_bool" => ([I8] -> Some(Word));
    NONE; "willow_future_ready_f64" => ([F64] -> Some(Word));
    NONE; "willow_future_ready_ptr" => ([Word] -> Some(Word));
    NONE; "willow_future_await_void" => ([Word] -> Some(I8));
    NONE; "willow_future_await_i64" => ([Word] -> Some(I64));
    NONE; "willow_future_await_bool" => ([Word] -> Some(I8));
    NONE; "willow_future_await_f64" => ([Word] -> Some(F64));
    NONE; "willow_future_await_ptr" => ([Word] -> Some(Word));
    // --- channels ---
    // Atomic primitives (willow-dgwo.3). Handles are Willow words;
    // AtomicBool values use the dedicated I8 representation.
    NONE; "willow_atomic_i64_new" => ([I64] -> Some(Word));
    NONE; "willow_atomic_i64_load" => ([Word] -> Some(I64));
    NONE; "willow_atomic_i64_store" => ([Word, I64] -> None);
    NONE; "willow_atomic_i64_add" => ([Word, I64] -> Some(I64));
    NONE; "willow_atomic_i64_sub" => ([Word, I64] -> Some(I64));
    NONE; "willow_atomic_i64_swap" => ([Word, I64] -> Some(I64));
    NONE; "willow_atomic_bool_new" => ([I8] -> Some(Word));
    NONE; "willow_atomic_bool_load" => ([Word] -> Some(I8));
    NONE; "willow_atomic_bool_store" => ([Word, I8] -> None);
    NONE; "willow_atomic_bool_swap" => ([Word, I8] -> Some(I8));
    // Blocking cells hold a generic Willow word plus an is-reference flag.
    NONE; "willow_blocking_cell_new" => ([Word, I64] -> Some(Word));
    BLOCK; "willow_blocking_cell_get" => ([Word] -> Some(Word));
    BLOCK; "willow_blocking_cell_set" => ([Word, Word] -> None);
    NONE; "willow_blocking_rw_cell_new" => ([Word, I64] -> Some(Word));
    BLOCK; "willow_blocking_rw_cell_read" => ([Word] -> Some(Word));
    BLOCK; "willow_blocking_rw_cell_write" => ([Word, Word] -> None);
    // Scheduler-aware Mutex<T> (willow-38w.1.3): acquire/poll return a status
    // code (1 acquired, 0 pending, -1 recursive, -2 lost, -3 cancelled) and publish the
    // registration token through the out-parameter, so a parked acquire can
    // re-identify its own generation after a wake.
    ALLOC; "willow_async_mutex_new" => ([Word, I64] -> Some(Word));
    SUSPEND; "willow_async_mutex_acquire" => ([Word, Ptr] -> Some(I32));
    SUSPEND; "willow_async_mutex_poll" => ([Word, I64] -> Some(I32));
    NONE; "willow_async_mutex_load" => ([Word, I64] -> Some(Word));
    NONE; "willow_async_mutex_commit" => ([Word, I64, Word] -> Some(I32));
    NONE; "willow_async_mutex_release" => ([Word, I64] -> Some(I32));
    NONE; "willow_async_mutex_cancel" => ([] -> Some(I32));
    PANIC_ALLOC; "willow_async_mutex_recursive_panic" => ([Ptr, I32, I32] -> None);
    NONE; "willow_async_mutex_invalid_status" => ([I32, I32] -> None);
    // Scheduler-aware RwLock<T> (willow-38w.1.5). Mode is 1=read, 2=write;
    // handoff wakes either one writer or the contiguous reader prefix.
    ALLOC; "willow_async_rwlock_new" => ([Word, I64] -> Some(Word));
    SUSPEND; "willow_async_rwlock_acquire" => ([Word, I32, Ptr] -> Some(I32));
    SUSPEND; "willow_async_rwlock_poll" => ([Word, I64] -> Some(I32));
    NONE; "willow_async_rwlock_load" => ([Word, I64] -> Some(Word));
    NONE; "willow_async_rwlock_commit" => ([Word, I64, Word] -> Some(I32));
    NONE; "willow_async_rwlock_release" => ([Word, I64] -> Some(I32));
    NONE; "willow_async_rwlock_cancel" => ([] -> Some(I32));
    PANIC_ALLOC; "willow_async_rwlock_recursive_panic" => ([Ptr, I32, I32] -> None);
    NONE; "willow_async_rwlock_invalid_status" => ([I32, I32] -> None);
    NONE; "willow_channel_new" => ([I64] -> Some(Word));
    PANIC_ALLOC; "willow_channel_send_i64" => ([Word, I64] -> None);
    PANIC_ALLOC; "willow_channel_send_bool" => ([Word, I8] -> None);
    PANIC_ALLOC; "willow_channel_send_f64" => ([Word, F64] -> None);
    PANIC_ALLOC; "willow_channel_send_ptr" => ([Word, Word] -> None);
    PANIC_ALLOC; "willow_channel_recv_i64" => ([Word] -> Some(I64));
    PANIC_ALLOC; "willow_channel_recv_bool" => ([Word] -> Some(I8));
    PANIC_ALLOC; "willow_channel_recv_f64" => ([Word] -> Some(F64));
    PANIC_ALLOC; "willow_channel_recv_ptr" => ([Word] -> Some(Word));
    NONE; "willow_channel_close" => ([Word] -> None);
    SUSPEND; "willow_channel_recv_ready" => ([Word] -> Some(I32));
    NONE; "willow_channel_unregister_waiter" => ([Word] -> None);
    PANIC_ALLOC; "willow_channel_new_bounded" => ([I64, I64] -> Some(Word));
    NONE; "willow_channel_send_ready" => ([Word] -> Some(I32));
    NONE; "willow_channel_try_send_i64" => ([Word, I64] -> Some(I32));
    NONE; "willow_channel_try_send_bool" => ([Word, I8] -> Some(I32));
    NONE; "willow_channel_try_send_f64" => ([Word, F64] -> Some(I32));
    NONE; "willow_channel_try_send_ptr" => ([Word, Word] -> Some(I32));
    NONE; "willow_select_rotation" => ([] -> Some(I64));
    NONE; "willow_monotonic_millis" => ([] -> Some(I64));
    NONE; "willow_sleep_until_monotonic" => ([I64] -> None);
    NONE; "willow_sched_unregister_task_waiter" => ([I64] -> None);
    // --- GC roots ---
    NONE; "willow_push_root" => ([Ptr] -> None);
    NONE; "willow_pop_roots" => ([I32] -> None);
    NONE; "willow_root_depth" => ([] -> Some(I32));
    // --- panic ---
    PANIC_ALLOC; "willow_nil_deref" => ([Ptr, I32, I32, Ptr] -> None);
    PANIC_ALLOC; "willow_int_div_panic" => ([I64, Ptr, I32, I32] -> None);
    NONE; "willow_panic" => ([Word] -> None);
    NONE; "willow_main_fail" => ([Word] -> None);
    NONE; "willow_panic_at" => ([Word, Ptr, I32, I32] -> None);
    PANIC_ALLOC; "willow_panic_raise" => ([Word, Ptr, I64, I64] -> None);
    NONE; "willow_panic_active" => ([] -> Some(I32));
    NONE; "willow_panic_depth" => ([] -> Some(I32));
    NONE; "willow_panic_enter_defer" => ([] -> None);
    NONE; "willow_panic_leave_defer" => ([] -> None);
    NONE; "willow_panic_recover" => ([] -> Some(Word));
    NONE; "willow_panic_release_recovered" => ([Word] -> None);
    NONE; "willow_panic_finish_unhandled" => ([] -> None);
    // --- debug call-chain stack (willow-992h) ---
    NONE; "willow_callstack_push" => ([Ptr, I64, Ptr, I64, I32, I32] -> None);
    NONE; "willow_callstack_pop" => ([] -> None);
    // --- debug fault site: source location for runtime-raised faults ---
    NONE; "willow_fault_site_set" => ([Ptr, I64, I64, I64] -> None);
    NONE; "willow_fault_site_clear" => ([] -> None);
    // --- reference debug metadata ---
    NONE; "willow_debug_reference_call" => ([Ptr, I32, I32, Ptr, Ptr, Ptr, Ptr, Ptr, Ptr] -> None);
    NONE; "willow_debug_reference_call_clear" => ([] -> None);
    // Async frame allocator + cooperative scheduler (willow-lpn.5 / willow-fqg.1).
    // Imported so the async state-machine lowering can emit frame allocation and
    // cooperative spawn/poll/wake calls.
    ALLOC; "willow_async_frame_alloc" => ([I64, I64] -> Some(Word));
    NO_PREEMPT; "willow_sched_spawn" => ([Ptr, Word] -> Some(I64));
    NONE; "willow_sched_run" => ([] -> Some(I64));
    NONE; "willow_sched_run_until" => ([I64] -> Some(I64));
    NONE; "willow_sched_run_until_deadline" => ([I64] -> Some(I64));
    BLOCK_PANIC_ALLOC; "willow_select_idle_wait" => ([] -> None);
    NONE; "willow_sched_wake" => ([I64] -> None);
    NONE; "willow_sched_cancel" => ([I64] -> None);
    NONE; "willow_sched_is_cancelled" => ([I64] -> Some(I64));
    NONE; "willow_sched_set_spawn_site" => ([I64, Word, I64] -> None);
    NONE; "willow_sched_set_cancel_fn" => ([I64, Ptr] -> None);
    NONE; "willow_fs_temp_path" => ([Word] -> Some(Word));
    BLOCK_ALLOC; "willow_fs_read_to_string" => ([Word] -> Some(Word));
    BLOCK_ALLOC; "willow_fs_write_string" => ([Word, Word] -> Some(Word));
    BLOCK_ALLOC; "willow_fs_exists" => ([Word] -> Some(I64));
    BLOCK_ALLOC; "willow_fs_remove_file" => ([Word] -> Some(Word));
    ALLOC; "willow_fs_read_to_string_async" => ([Word] -> Some(Word));
    ALLOC; "willow_fs_write_string_async" => ([Word, Word] -> Some(Word));
    ALLOC; "willow_fs_exists_async" => ([Word] -> Some(Word));
    ALLOC; "willow_fs_remove_file_async" => ([Word] -> Some(Word));
    // --- scheduler-aware TCP (`std::net`) ---
    BLOCK_ALLOC; "willow_net_bind" => ([Word] -> Some(Word));
    ALLOC; "willow_net_local_addr" => ([Word] -> Some(Word));
    ALLOC; "willow_net_peer_addr" => ([Word] -> Some(Word));
    ALLOC; "willow_net_shutdown" => ([Word] -> Some(Word));
    ALLOC; "willow_net_connect_async" => ([Word] -> Some(Word));
    ALLOC; "willow_net_accept_async" => ([Word] -> Some(Word));
    ALLOC; "willow_net_read_async" => ([Word, I64] -> Some(Word));
    ALLOC; "willow_net_write_async" => ([Word, Word] -> Some(Word));
    // --- cancellation tokens and structured scopes ---
    ALLOC; "willow_cancellation_token_new" => ([] -> Some(Word));
    ALLOC; "willow_cancellation_token_child" => ([Word] -> Some(Word));
    ALLOC; "willow_cancellation_token_attach" => ([Word, Word] -> Some(Word));
    ALLOC; "willow_cancellation_token_cancel" => ([Word] -> None);
    NONE; "willow_cancellation_token_is_cancelled" => ([Word] -> Some(I64));
    ALLOC; "willow_task_scope_new" => ([] -> Some(Word));
    ALLOC; "willow_task_scope_child" => ([Word] -> Some(Word));
    ALLOC; "willow_task_scope_add" => ([Word, Word] -> Some(Word));
    ALLOC; "willow_task_scope_cancel" => ([Word] -> None);
    NONE; "willow_task_scope_is_cancelled" => ([Word] -> Some(I64));
    ALLOC; "willow_task_scope_finish" => ([Word] -> Some(Word));
    // --- bounded parallel collection mapping ---
    PANIC_ALLOC; "willow_parallel_map_i64" => ([Word, Ptr] -> Some(Word));
    NONE; "willow_blocking_active_jobs" => ([] -> Some(I64));
    NONE; "willow_blocking_completed_jobs" => ([] -> Some(I64));
    NONE; "willow_sched_current_task" => ([] -> Some(I64));
    // Tag the running task with its async fn name for async stack traces
    // (willow-9lw): (name_ptr, name_len).
    NONE; "willow_sched_tag_current_task" => ([Ptr, I64] -> None);
    SUSPEND; "willow_sched_sleep" => ([I64] -> None);
    NONE; "willow_sched_yield" => ([] -> None);
    SUSPEND; "willow_sched_await" => ([I64] -> Some(I32));
    NONE; "willow_sched_task_state" => ([I64] -> Some(I32));
    // --- frame-backed task status (willow-ezs.1.3). Terminal status lives in
    // the async frame HEADER, so a holder of the task handle answers
    // await/result/is_cancelled with one Acquire load instead of a
    // scheduler-table lookup under the global lock. ---
    NONE; "willow_frame_status" => ([Word] -> Some(I64));
    NONE; "willow_frame_is_cancelled" => ([Word] -> Some(I64));
    SUSPEND; "willow_frame_await" => ([Word, I64] -> Some(I32));
    PANIC_ALLOC; "willow_frame_await_check" => ([Word, I64] -> None);
    // --- preemption (willow-0a6k.1, spec §7-9,22-23). Flags are native pointers.
    // Emitted by compiler-inserted safepoints in willow-0a6k.2; declared here so
    // the runtime ABI surface + symbol-export tests cover them from stage 1. ---
    NONE; "willow_preempt_task_budget" => ([] -> Some(I64));
    NONE; "willow_preempt_time_quantum_ms" => ([] -> Some(I64));
    NONE; "willow_preempt_flag_new" => ([] -> Some(Ptr));
    NONE; "willow_preempt_flag_free" => ([Ptr] -> None);
    NONE; "willow_preempt_request" => ([Ptr] -> None);
    NONE; "willow_preempt_clear" => ([Ptr] -> None);
    NONE; "willow_preempt_requested" => ([Ptr] -> Some(I32));
    NONE; "willow_preempt_begin" => ([Ptr] -> None);
    NONE; "willow_preempt_end" => ([] -> None);
    PREEMPT; "willow_preempt_check" => ([] -> Some(I32));
    NONE; "willow_preempt_enter_no_preempt" => ([] -> None);
    NONE; "willow_preempt_leave_no_preempt" => ([] -> None);
};

/// Look up one generated-code-facing runtime symbol.
///
/// Call emission uses this lookup for effect-sensitive decisions. Keeping the
/// signature and effects in the same row prevents a newly added runtime ABI
/// from silently inheriting `NONE` through a second, permissive name match.
pub fn runtime_symbol(name: &str) -> Option<&'static RuntimeSymbol> {
    RUNTIME_SYMBOLS.iter().find(|symbol| symbol.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn abity_lowers_pointer_to_module_pointer_type() {
        // On the supported 64-bit targets the pointer type is I64; the lowering
        // must route Ptr through the supplied pointer type, not a hard-coded one.
        assert_eq!(clif_abi_ty(AbiTy::Ptr, types::I64), types::I64);
        assert_eq!(clif_abi_ty(AbiTy::Ptr, types::I32), types::I32);
        assert_eq!(clif_abi_ty(AbiTy::I8, types::I64), types::I8);
        assert_eq!(clif_abi_ty(AbiTy::I32, types::I64), types::I32);
        assert_eq!(clif_abi_ty(AbiTy::F64, types::I64), types::F64);
        assert_eq!(clif_abi_ty(AbiTy::I64, types::I32), types::I64);
    }

    #[test]
    fn no_duplicate_symbols() {
        let mut seen = HashSet::new();
        for sym in RUNTIME_SYMBOLS {
            assert!(
                seen.insert(sym.name),
                "duplicate runtime symbol in RUNTIME_SYMBOLS: {}",
                sym.name
            );
        }
    }

    #[test]
    fn all_names_are_well_formed() {
        for sym in RUNTIME_SYMBOLS {
            assert!(
                sym.name.starts_with("willow_"),
                "runtime symbol must start with `willow_`: {}",
                sym.name
            );
            assert!(
                !sym.name.is_empty()
                    && sym
                        .name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "runtime symbol has invalid characters: {}",
                sym.name
            );
        }
    }

    #[test]
    fn table_is_non_empty() {
        assert!(
            RUNTIME_SYMBOLS.len() >= 50,
            "expected the full runtime ABI surface, got {} symbols",
            RUNTIME_SYMBOLS.len()
        );
    }

    #[test]
    fn scheduler_and_gc_effects_are_classified_conservatively() {
        let effects = |name| {
            RUNTIME_SYMBOLS
                .iter()
                .find(|symbol| symbol.name == name)
                .unwrap_or_else(|| panic!("missing ABI symbol {name}"))
                .effects()
        };

        assert!(effects("willow_alloc").contains(RuntimeEffects::MAY_ALLOCATE));
        assert!(
            effects("willow_fs_read_to_string")
                .contains(RuntimeEffects::MAY_BLOCK.union(RuntimeEffects::MAY_ALLOCATE))
        );
        assert!(effects("willow_fs_read_to_string_async").contains(RuntimeEffects::MAY_ALLOCATE));
        assert!(!effects("willow_fs_read_to_string_async").contains(RuntimeEffects::MAY_SUSPEND));
        assert!(effects("willow_sched_await").contains(RuntimeEffects::MAY_SUSPEND));
        assert!(effects("willow_blocking_cell_get").contains(RuntimeEffects::MAY_BLOCK));
        assert!(effects("willow_blocking_rw_cell_write").contains(RuntimeEffects::MAY_BLOCK));
        assert!(effects("willow_gc_safepoint").contains(RuntimeEffects::MAY_PREEMPT));
        assert!(
            effects("willow_array_get")
                .contains(RuntimeEffects::MAY_ALLOCATE.union(RuntimeEffects::MAY_PANIC))
        );
        assert!(effects("willow_frame_await_check").contains(RuntimeEffects::MAY_PANIC));
        assert!(
            effects("willow_select_idle_wait")
                .contains(RuntimeEffects::MAY_BLOCK.union(RuntimeEffects::MAY_PANIC))
        );
        assert!(
            !effects("willow_panic").contains(RuntimeEffects::MAY_PANIC),
            "legacy fatal ABI must not be mistaken for recoverable propagation"
        );
        assert!(
            !effects("willow_gc_collect").contains(RuntimeEffects::MAY_PANIC),
            "GC integrity failures bypass language recovery"
        );
        assert!(
            !effects("willow_channel_unregister_waiter")
                .contains(RuntimeEffects::NO_PREEMPT_REGION)
        );
        assert_eq!(effects("willow_print_i64"), RuntimeEffects::NONE);
    }

    #[test]
    fn no_preempt_runtime_policy_avoids_double_bracketing_runtime_owned_guards() {
        let effects = |name| runtime_symbol(name).expect("known ABI symbol").effects();
        assert!(
            effects("willow_sched_spawn").contains(RuntimeEffects::NO_PREEMPT_REGION),
            "willow_sched_spawn must be bracketed by the central runtime-call emitter"
        );
        for name in [
            "willow_async_mutex_acquire",
            "willow_async_mutex_poll",
            "willow_async_mutex_release",
            "willow_async_mutex_cancel",
            "willow_channel_unregister_waiter",
            "willow_push_root",
            "willow_pop_roots",
        ] {
            assert!(
                !effects(name).contains(RuntimeEffects::NO_PREEMPT_REGION),
                "{name} owns its runtime-side NoPreemptGuard and must not be double-bracketed"
            );
        }
        assert!(!effects("willow_print_i64").contains(RuntimeEffects::NO_PREEMPT_REGION));
    }

    #[test]
    fn runtime_symbol_lookup_uses_the_schema_without_a_default() {
        let array_get = runtime_symbol("willow_array_get").expect("known ABI symbol");
        assert_eq!(array_get.params, &[AbiTy::I64, AbiTy::I64]);
        assert_eq!(array_get.ret, Some(AbiTy::I64));
        assert!(array_get.effects().contains(RuntimeEffects::MAY_PANIC));
        assert!(runtime_symbol("willow_not_a_runtime_symbol").is_none());
    }

    #[test]
    fn eager_task_constructors_allocate_but_do_not_suspend_the_caller() {
        let effects = |name| runtime_symbol(name).expect("known ABI symbol").effects();
        for name in [
            "willow_fs_read_to_string_async",
            "willow_fs_write_string_async",
            "willow_fs_exists_async",
            "willow_fs_remove_file_async",
            "willow_net_connect_async",
            "willow_net_accept_async",
            "willow_net_read_async",
            "willow_net_write_async",
            "willow_task_scope_finish",
            "willow_parallel_map_i64",
        ] {
            assert!(
                effects(name).contains(RuntimeEffects::MAY_ALLOCATE),
                "{name}"
            );
            assert!(
                !effects(name).contains(RuntimeEffects::MAY_SUSPEND),
                "{name} only constructs and schedules a Task"
            );
        }
    }

    #[test]
    fn parallel_mapper_abi_is_an_i64_function_address_word() {
        let symbol = runtime_symbol("willow_parallel_map_i64").expect("parallel ABI");
        assert_eq!(symbol.params, &[AbiTy::Ptr, AbiTy::I64]);
    }
}
