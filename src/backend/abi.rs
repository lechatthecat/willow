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

/// What each effect alias claims, so a new row is classified against a rule
/// rather than against whatever the row above it happens to say.
///
/// `ALLOC` ([`RuntimeEffects::MAY_ALLOCATE`]) means the call can reach the
/// Willow GC allocator — `willow_gc_alloc_layout` and everything that funnels
/// into it, including `willow_string_from_str`, `willow_string_alloc`,
/// `willow_alloc_with_layout`, `willow_alloc_enum_variant`, and
/// `willow_array_new`. The point is not that memory is obtained; it is that a
/// COLLECTION can happen inside the call, so a GC value the caller is holding
/// in an unrooted place does not survive it. A helper that only does
/// `Box::into_raw` onto the native heap therefore stays `NONE` — no
/// safepoint, nothing to root against (willow-8hk7).
///
/// Over-declaring is safe and under-declaring is not: the effects are read to
/// decide what generated code may keep across a call, so an omission is a
/// silent GC bug the day the bit is wired into a rooting decision, while a
/// spurious bit only costs a root.
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
    ALLOC; "willow_f64_to_string" => ([F64] -> Some(Word));
    ALLOC; "willow_i64_to_string" => ([I64] -> Some(Word));
    ALLOC; "willow_bool_to_string" => ([I8] -> Some(Word));
    ALLOC; "willow_f64_parse" => ([Word] -> Some(Word));
    ALLOC; "willow_format_f64_17g" => ([F64] -> Some(Word));
    ALLOC; "willow_format_f64_16f" => ([F64] -> Some(Word));
    ALLOC; "willow_format_f64_6f" => ([F64] -> Some(Word));
    // --- string ---
    ALLOC; "willow_string_concat" => ([Word, Word] -> Some(Word));
    NONE; "willow_string_eq" => ([Word, Word] -> Some(I64));
    ALLOC; "willow_string_alloc" => ([Ptr, I64] -> Some(Word));
    ALLOC; "willow_string_literal" => ([Ptr, I64] -> Some(Word));
    // --- args ---
    NONE; "willow_runtime_args_len" => ([] -> Some(I64));
    ALLOC; "willow_runtime_arg" => ([I64] -> Some(Word));
    ALLOC; "willow_runtime_program_name" => ([] -> Some(Word));
    ALLOC; "willow_runtime_args_array" => ([] -> Some(Word));
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
    ALLOC; "willow_map_to_string" => ([Word, I64, I64] -> Some(Word));
    PANIC_ALLOC; "willow_array_element_addr" => ([Word, I64] -> Some(Ptr));
    // --- maps (std::collections::Map) ---
    ALLOC; "willow_map_new" => ([] -> Some(Word));
    ALLOC; "willow_map_copy" => ([Word] -> Some(Word));
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
    ALLOC; "willow_atomic_i64_new" => ([I64] -> Some(Word));
    NONE; "willow_atomic_i64_load" => ([Word] -> Some(I64));
    NONE; "willow_atomic_i64_store" => ([Word, I64] -> None);
    NONE; "willow_atomic_i64_add" => ([Word, I64] -> Some(I64));
    NONE; "willow_atomic_i64_sub" => ([Word, I64] -> Some(I64));
    NONE; "willow_atomic_i64_swap" => ([Word, I64] -> Some(I64));
    ALLOC; "willow_atomic_bool_new" => ([I8] -> Some(Word));
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
    ALLOC; "willow_channel_new" => ([I64] -> Some(Word));
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
    NONE; "willow_debug_reference_call_scope_push" => ([] -> None);
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
    ALLOC; "willow_fs_temp_path" => ([Word] -> Some(Word));
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
    // The mapper is a native function pointer. Generated Willow function
    // values are 64-bit on every accepted target, but the ABI classification
    // still distinguishes an address the runtime calls from an integer.
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
        // The array handle and the element are Willow words (either may hold a
        // GC handle); only the index is a plain scalar.
        assert_eq!(array_get.params, &[AbiTy::Word, AbiTy::I64]);
        assert_eq!(array_get.ret, Some(AbiTy::Word));
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
    fn parallel_mapper_abi_is_a_native_function_pointer() {
        let symbol = runtime_symbol("willow_parallel_map_i64").expect("parallel ABI");
        assert_eq!(symbol.params, &[AbiTy::Word, AbiTy::Ptr]);
        assert_eq!(symbol.ret, Some(AbiTy::Word));
    }

    #[test]
    fn word_pointer_and_integer_classes_remain_distinct() {
        let signature = |name| {
            let symbol = runtime_symbol(name).unwrap_or_else(|| panic!("missing {name}"));
            (symbol.params, symbol.ret)
        };

        // Opaque Willow values stay words even though the Rust exports spell
        // them as `*mut u8`/`*mut c_void`.
        assert_eq!(
            signature("willow_channel_try_send_ptr"),
            (&[AbiTy::Word, AbiTy::Word][..], Some(AbiTy::I32))
        );
        assert_eq!(
            signature("willow_frame_await"),
            (&[AbiTy::Word, AbiTy::I64][..], Some(AbiTy::I32))
        );

        // Slot addresses and callbacks are native pointers.
        assert_eq!(
            signature("willow_async_mutex_acquire"),
            (&[AbiTy::Word, AbiTy::Ptr][..], Some(AbiTy::I32))
        );
        assert_eq!(
            signature("willow_sched_spawn"),
            (&[AbiTy::Ptr, AbiTy::Word][..], Some(AbiTy::I64))
        );

        // Task ids and netpoll's cross-platform native-handle integer are
        // explicitly 64-bit numbers, not dereferenceable pointers.
        assert_eq!(signature("willow_sched_wake"), (&[AbiTy::I64][..], None));
        assert_eq!(
            signature("willow_netpoll_register"),
            (&[AbiTy::I64, AbiTy::I32][..], Some(AbiTy::I32))
        );
    }
}

/// The `MAY_ALLOCATE` column of [`RUNTIME_SYMBOLS`], pinned row by row.
///
/// The bit says a collection can happen inside the call, so generated code
/// cannot hold a GC value across it in an unrooted place. Nothing in the
/// emitter reads it yet — `emit_runtime_call_with_cleanup` acts on `MAY_PANIC`
/// and `NO_PREEMPT_REGION` only — which is exactly why it needs pinning: an
/// understated row is invisible until the day the bit is wired into a rooting
/// decision, and then it is a GC bug with no failing test behind it
/// (willow-8hk7).
///
/// Perspectives:
///
/// a01 the float/int/bool `to_string` conversions allocate their result
/// a02 the fixed-precision float formatters allocate their result
/// a03 `willow_f64_parse` allocates both the string and the `Result` box
/// a04 the process-argument accessors allocate
/// a05 the map constructors and the map's `to_string` allocate
/// a06 `willow_map_copy` inherits allocation from the `willow_map_new` it calls
/// a07 the atomic-cell constructors allocate their GC cell
/// a08 `willow_channel_new` allocates its GC-resident channel
/// a09 `willow_fs_temp_path` allocates the path string
/// a10 a native-heap constructor is NOT an allocation effect
/// a11 reading a runtime string without building one is not an allocation
/// a12 every row that returns a freshly built GC object carries the bit
/// a13 pure readers and stores stay `NONE`
/// a14 the GC statistic readers stay `NONE`
/// a15 correcting the effects did not disturb any signature
/// a16 an allocating row keeps whatever else it already declared
/// a17 `MAY_ALLOCATE` never arrives alone on a suspending row by accident
/// a18 the whole `MAY_ALLOCATE` set is pinned, so a new row must classify
/// a19 no row carries an effect bit outside the defined set
/// a20 the aliases spell the combinations they are named for
#[cfg(test)]
mod alloc_effects_tests {
    use super::*;

    fn effects(name: &str) -> RuntimeEffects {
        runtime_symbol(name)
            .unwrap_or_else(|| panic!("missing ABI symbol {name}"))
            .effects()
    }

    fn allocates(name: &str) -> bool {
        effects(name).contains(RuntimeEffects::MAY_ALLOCATE)
    }

    /// Every symbol whose implementation reaches the Willow GC allocator, and
    /// therefore every symbol this column must mark. Adding a runtime ABI that
    /// builds a GC object means adding it here too; `a18` fails otherwise.
    const ALLOCATING: &[&str] = &[
        "willow_pow_negative_exponent",
        "willow_f64_to_string",
        "willow_i64_to_string",
        "willow_bool_to_string",
        "willow_f64_parse",
        "willow_format_f64_17g",
        "willow_format_f64_16f",
        "willow_format_f64_6f",
        "willow_string_concat",
        "willow_string_alloc",
        "willow_string_literal",
        "willow_runtime_arg",
        "willow_runtime_program_name",
        "willow_runtime_args_array",
        "willow_alloc",
        "willow_alloc_typed",
        "willow_gc_alloc_layout",
        "willow_gc_alloc_slow",
        "willow_array_new",
        "willow_array_copy",
        "willow_array_len",
        "willow_array_get",
        "willow_array_set",
        "willow_array_push",
        "willow_array_pop",
        "willow_array_to_string",
        "willow_map_to_string",
        "willow_array_element_addr",
        "willow_map_new",
        "willow_map_copy",
        "willow_map_get",
        "willow_atomic_i64_new",
        "willow_atomic_bool_new",
        "willow_async_mutex_new",
        "willow_async_mutex_recursive_panic",
        "willow_async_rwlock_new",
        "willow_async_rwlock_recursive_panic",
        "willow_channel_new",
        "willow_channel_send_i64",
        "willow_channel_send_bool",
        "willow_channel_send_f64",
        "willow_channel_send_ptr",
        "willow_channel_recv_i64",
        "willow_channel_recv_bool",
        "willow_channel_recv_f64",
        "willow_channel_recv_ptr",
        "willow_channel_new_bounded",
        "willow_nil_deref",
        "willow_int_div_panic",
        "willow_panic_raise",
        "willow_async_frame_alloc",
        "willow_select_idle_wait",
        "willow_fs_temp_path",
        "willow_fs_read_to_string",
        "willow_fs_write_string",
        "willow_fs_exists",
        "willow_fs_remove_file",
        "willow_fs_read_to_string_async",
        "willow_fs_write_string_async",
        "willow_fs_exists_async",
        "willow_fs_remove_file_async",
        "willow_net_bind",
        "willow_net_local_addr",
        "willow_net_peer_addr",
        "willow_net_shutdown",
        "willow_net_connect_async",
        "willow_net_accept_async",
        "willow_net_read_async",
        "willow_net_write_async",
        "willow_cancellation_token_new",
        "willow_cancellation_token_child",
        "willow_cancellation_token_attach",
        "willow_cancellation_token_cancel",
        "willow_task_scope_new",
        "willow_task_scope_child",
        "willow_task_scope_add",
        "willow_task_scope_cancel",
        "willow_task_scope_finish",
        "willow_parallel_map_i64",
        "willow_frame_await_check",
    ];

    #[test]
    fn a01_scalar_to_string_conversions_allocate() {
        // Each is `willow_string_from_str` of a formatted scalar, and that is
        // `willow_string_alloc` -> `willow_alloc_with_layout`.
        for name in [
            "willow_f64_to_string",
            "willow_i64_to_string",
            "willow_bool_to_string",
        ] {
            assert!(allocates(name), "{name} builds a WillowString");
        }
    }

    #[test]
    fn a02_fixed_precision_float_formatters_allocate() {
        for name in [
            "willow_format_f64_17g",
            "willow_format_f64_16f",
            "willow_format_f64_6f",
        ] {
            assert!(allocates(name), "{name} builds a WillowString");
        }
    }

    #[test]
    fn a03_float_parse_allocates_its_result_box() {
        // `willow_f64_parse` returns `Result<f64, String>`: the Ok path is one
        // `willow_alloc_enum_variant`, the Err path a message string plus a
        // second variant allocation. Two chances to collect, not zero.
        assert!(allocates("willow_f64_parse"));
    }

    #[test]
    fn a04_process_argument_accessors_allocate() {
        // `willow_runtime_args_array` roots the array across the element
        // strings precisely because each one can collect; the ABI row has to
        // say the same thing about the call as a whole.
        for name in [
            "willow_runtime_arg",
            "willow_runtime_program_name",
            "willow_runtime_args_array",
        ] {
            assert!(allocates(name), "{name} returns a GC value it built");
        }
        // Reading the count builds nothing.
        assert!(!allocates("willow_runtime_args_len"));
    }

    #[test]
    fn a05_map_constructors_and_display_allocate() {
        for name in ["willow_map_new", "willow_map_to_string", "willow_map_get"] {
            assert!(allocates(name), "{name}");
        }
    }

    #[test]
    fn a06_map_copy_inherits_allocation_from_map_new() {
        // The effect is transitive: `willow_map_copy` allocates nothing
        // directly, it calls `willow_map_new`. A row classified by what its
        // own body spells would miss this.
        assert!(allocates("willow_map_copy"));
    }

    #[test]
    fn a07_atomic_cell_constructors_allocate() {
        for name in ["willow_atomic_i64_new", "willow_atomic_bool_new"] {
            assert!(allocates(name), "{name} allocates a GC-resident cell");
        }
        // Load/store/swap touch the cell already handed to them.
        for name in [
            "willow_atomic_i64_load",
            "willow_atomic_i64_store",
            "willow_atomic_i64_add",
            "willow_atomic_bool_load",
            "willow_atomic_bool_swap",
        ] {
            assert!(!allocates(name), "{name} only touches an existing cell");
        }
    }

    #[test]
    fn a08_channel_construction_allocates_but_closing_does_not() {
        assert!(allocates("willow_channel_new"));
        assert!(allocates("willow_channel_new_bounded"));
        assert!(!allocates("willow_channel_close"));
        assert!(!allocates("willow_channel_unregister_waiter"));
    }

    #[test]
    fn a09_temp_path_allocates_the_string_it_returns() {
        assert!(allocates("willow_fs_temp_path"));
    }

    #[test]
    fn a10_a_native_heap_constructor_is_not_an_allocation_effect() {
        // These are `Box::into_raw` onto the process heap. No safepoint, no
        // collection, nothing for a caller to root against — the bit would be
        // a lie in the other direction.
        for name in [
            "willow_future_ready_void",
            "willow_future_ready_i64",
            "willow_future_ready_bool",
            "willow_future_ready_f64",
            "willow_future_ready_ptr",
            "willow_blocking_cell_new",
            "willow_blocking_rw_cell_new",
            "willow_preempt_flag_new",
        ] {
            assert!(!allocates(name), "{name} allocates native, not GC, memory");
        }
    }

    #[test]
    fn a11_reading_a_runtime_string_is_not_an_allocation() {
        // `willow_debug_reference_call` copies its arguments into a Rust
        // `String` for a thread-local; it never asks the GC for anything.
        assert!(!allocates("willow_debug_reference_call_scope_push"));
        assert!(!allocates("willow_debug_reference_call"));
        assert!(!allocates("willow_debug_reference_call_clear"));
        assert!(!allocates("willow_string_eq"));
    }

    #[test]
    fn a12_every_row_returning_a_freshly_built_gc_object_carries_the_bit() {
        // The shape that motivated the sweep: a row that hands back a GC value
        // it just constructed. Listed by name rather than inferred from the
        // signature, because plenty of rows return a word they were given.
        for name in [
            "willow_string_concat",
            "willow_array_new",
            "willow_array_to_string",
            "willow_map_new",
            "willow_i64_to_string",
            "willow_runtime_args_array",
            "willow_channel_new",
            "willow_async_mutex_new",
            "willow_cancellation_token_new",
            "willow_task_scope_new",
            "willow_async_frame_alloc",
        ] {
            assert!(allocates(name), "{name}");
        }
    }

    #[test]
    fn a13_pure_readers_and_stores_stay_none() {
        for name in [
            "willow_print_i64",
            "willow_println_string",
            "willow_map_len",
            "willow_map_contains",
            "willow_gc_write_barrier",
            "willow_push_root",
            "willow_pop_roots",
            "willow_root_depth",
            "willow_monotonic_millis",
            "willow_cancellation_token_is_cancelled",
            "willow_task_scope_is_cancelled",
        ] {
            assert_eq!(effects(name), RuntimeEffects::NONE, "{name}");
        }
    }

    #[test]
    fn a14_gc_statistic_readers_stay_none() {
        // They report on the heap; they do not touch it.
        for name in [
            "willow_gc_allocated_bytes",
            "willow_gc_minor_collections",
            "willow_gc_major_collections",
            "willow_gc_promoted_objects",
            "willow_gc_moved_objects",
            "willow_gc_remembered_set_size",
            "willow_gc_write_barrier_hits",
        ] {
            assert_eq!(effects(name), RuntimeEffects::NONE, "{name}");
        }
    }

    #[test]
    fn a15_correcting_the_effects_did_not_disturb_any_signature() {
        let signature = |name: &str| {
            let symbol = runtime_symbol(name).unwrap_or_else(|| panic!("missing {name}"));
            (symbol.params, symbol.ret)
        };
        assert_eq!(
            signature("willow_i64_to_string"),
            (&[AbiTy::I64][..], Some(AbiTy::Word))
        );
        assert_eq!(
            signature("willow_f64_parse"),
            (&[AbiTy::Word][..], Some(AbiTy::Word))
        );
        assert_eq!(signature("willow_map_new"), (&[][..], Some(AbiTy::Word)));
        assert_eq!(
            signature("willow_channel_new"),
            (&[AbiTy::I64][..], Some(AbiTy::Word))
        );
        assert_eq!(
            signature("willow_runtime_program_name"),
            (&[][..], Some(AbiTy::Word))
        );
    }

    #[test]
    fn a16_an_allocating_row_keeps_whatever_else_it_already_declared() {
        // The sweep added one bit; it must not have replaced a combination.
        assert!(effects("willow_array_get").contains(RuntimeEffects::MAY_PANIC));
        assert!(effects("willow_fs_read_to_string").contains(RuntimeEffects::MAY_BLOCK));
        assert!(effects("willow_select_idle_wait").contains(RuntimeEffects::MAY_BLOCK));
        assert!(effects("willow_select_idle_wait").contains(RuntimeEffects::MAY_PANIC));
    }

    #[test]
    fn a17_allocation_and_suspension_are_independent_columns() {
        // An eager async constructor allocates its Task without suspending its
        // caller; a suspension point need not allocate at all.
        assert!(allocates("willow_net_read_async"));
        assert!(!effects("willow_net_read_async").contains(RuntimeEffects::MAY_SUSPEND));
        assert!(effects("willow_sched_await").contains(RuntimeEffects::MAY_SUSPEND));
        assert!(!allocates("willow_sched_await"));
    }

    #[test]
    fn a18_the_allocation_column_is_pinned_row_by_row() {
        let declared: Vec<&str> = RUNTIME_SYMBOLS
            .iter()
            .filter(|symbol| symbol.effects().contains(RuntimeEffects::MAY_ALLOCATE))
            .map(|symbol| symbol.name)
            .collect();
        let mut expected = ALLOCATING.to_vec();
        expected.sort_unstable();
        let mut actual = declared.clone();
        actual.sort_unstable();
        assert_eq!(
            actual, expected,
            "MAY_ALLOCATE set drifted; classify the row against the rule on \
             the effect aliases (reaches the GC allocator?) and update ALLOCATING"
        );
    }

    #[test]
    fn a19_no_row_carries_a_bit_outside_the_defined_set() {
        for symbol in RUNTIME_SYMBOLS {
            let extra = symbol.effects().difference(RuntimeEffects::ALL);
            assert!(
                extra.is_empty(),
                "{} declares an undefined effect bit",
                symbol.name
            );
        }
    }

    #[test]
    fn a20_the_aliases_spell_the_combinations_they_are_named_for() {
        assert_eq!(ALLOC, RuntimeEffects::MAY_ALLOCATE);
        assert!(PANIC_ALLOC.contains(ALLOC));
        assert!(PANIC_ALLOC.contains(RuntimeEffects::MAY_PANIC));
        assert!(BLOCK_ALLOC.contains(ALLOC));
        assert!(BLOCK_ALLOC.contains(BLOCK));
        assert!(BLOCK_PANIC_ALLOC.contains(BLOCK_ALLOC));
        assert!(BLOCK_PANIC_ALLOC.contains(RuntimeEffects::MAY_PANIC));
        // The remaining aliases deliberately do not imply allocation.
        assert!(!SUSPEND.contains(ALLOC));
        assert!(!PREEMPT.contains(ALLOC));
        assert!(!NO_PREEMPT.contains(ALLOC));
        assert!(NONE.is_empty());
    }
}
