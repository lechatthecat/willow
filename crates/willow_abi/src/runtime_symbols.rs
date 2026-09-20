//! Single source of truth for the runtime ABI surface imported by generated
//! code.
//!
//! Every runtime symbol the backend calls into `libwillow_runtime` is listed
//! in [`RUNTIME_SYMBOLS`] together with its parameter, return, and effect
//! kinds. The compiler lowers each row to a Cranelift signature
//! (`willow_compiler::backend::abi`); the runtime pins each row against the
//! Rust `extern "C"` declaration it describes
//! (`crates/willow_runtime/src/abi_signature_tests.rs`). The table lives here,
//! in the crate both sides depend on, so that the runtime can check the
//! *semantic* kinds ([`AbiTy::Word`] vs [`AbiTy::I64`]) of a row and not only
//! the lowered machine types, which coincide on every supported target.
//!
//! Integration link tests keep this table and the actual exported staticlib
//! symbols in sync.

use crate::{AbiTy, RuntimeEffects};

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
const TASK_STACK: RuntimeEffects = RuntimeEffects::MAY_PREEMPT
    .union(RuntimeEffects::MAY_PANIC)
    .union(RuntimeEffects::MAY_ALLOCATE);
const NO_PREEMPT: RuntimeEffects = RuntimeEffects::NO_PREEMPT_REGION;

use AbiTy::{F64, I8, I32, I64, Ptr, Word};

/// Declare the backend-facing runtime ABI once and generate the typed table
/// the compiler lowers and the runtime pins. Keeping the compact signatures in one invocation
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
    // Stdout locking, writing and flushing can block the current OS thread.
    // Native formatting allocations do not enter the Willow GC, and I/O
    // failures are fatal rather than recoverable Willow panics.
    BLOCK; "willow_print_i64" => ([I64] -> None);
    BLOCK; "willow_println_i64" => ([I64] -> None);
    BLOCK; "willow_print_bool" => ([I8] -> None);
    BLOCK; "willow_println_bool" => ([I8] -> None);
    BLOCK; "willow_print_f64" => ([F64] -> None);
    BLOCK; "willow_println_f64" => ([F64] -> None);
    BLOCK; "willow_print_string" => ([Ptr] -> None);
    BLOCK; "willow_println_string" => ([Ptr] -> None);
    // --- math / float formatting ---
    PANIC_ALLOC; "willow_pow_negative_exponent" => ([I64, Ptr, I32, I32] -> None);
    ALLOC; "willow_f64_to_string" => ([F64] -> Some(Ptr));
    ALLOC; "willow_i64_to_string" => ([I64] -> Some(Ptr));
    ALLOC; "willow_bool_to_string" => ([I8] -> Some(Ptr));
    ALLOC; "willow_f64_parse" => ([Ptr] -> Some(Ptr));
    ALLOC; "willow_format_f64_17g" => ([F64] -> Some(Ptr));
    ALLOC; "willow_format_f64_16f" => ([F64] -> Some(Ptr));
    ALLOC; "willow_format_f64_6f" => ([F64] -> Some(Ptr));
    // --- string ---
    PANIC_ALLOC; "willow_string_concat" => ([Ptr, Ptr] -> Some(Ptr));
    NONE; "willow_string_eq" => ([Ptr, Ptr] -> Some(I64));
    ALLOC; "willow_string_alloc" => ([Ptr, I64] -> Some(Ptr));
    ALLOC; "willow_string_literal" => ([Ptr, I64] -> Some(Ptr));
    // --- args ---
    NONE; "willow_runtime_args_len" => ([] -> Some(I64));
    ALLOC; "willow_runtime_arg" => ([I64] -> Some(Ptr));
    ALLOC; "willow_runtime_program_name" => ([] -> Some(Ptr));
    ALLOC; "willow_runtime_args_array" => ([] -> Some(Ptr));
    // --- GC allocation ---
    ALLOC; "willow_alloc" => ([I64] -> Some(Ptr));
    ALLOC; "willow_alloc_typed" => ([I64, I64] -> Some(Ptr));
    ALLOC; "willow_gc_alloc_layout" => ([I64, I64, I64, I64] -> Some(Ptr));
    // Arguments: stable layout fingerprint, payload bytes, immutable descriptor.
    ALLOC; "willow_gc_alloc_bitmap" => ([I64, I64, Ptr] -> Some(Ptr));
    ALLOC; "willow_gc_alloc_slow" => ([Ptr, I64, I64, I64, I64] -> Some(Ptr));
    NONE; "willow_gc_write_barrier" => ([Ptr, Ptr, Ptr, I64] -> None);
    PREEMPT; "willow_gc_collect" => ([] -> None);
    PREEMPT; "willow_gc_minor_collect" => ([] -> None);
    NONE; "willow_gc_stats_snapshot_v1" => ([Ptr] -> Some(I32));
    NONE; "willow_gc_stats_size" => ([I64] -> Some(I64));
    NONE; "willow_gc_stats_snapshot" => ([I64, Ptr, I64] -> Some(I64));
    NONE; "willow_gc_set_memory_limit" => ([I64] -> Some(I64));
    NONE; "willow_gc_allocated_bytes" => ([] -> Some(I64));
    NONE; "willow_gc_tlab_fast_allocations" => ([] -> Some(I64));
    NONE; "willow_gc_tlab_slow_allocations" => ([] -> Some(I64));
    NONE; "willow_gc_tlab_refills" => ([] -> Some(I64));
    NONE; "willow_gc_tlab_large_allocations" => ([] -> Some(I64));
    NONE; "willow_gc_tlab_reserved_bytes" => ([] -> Some(I64));
    NONE; "willow_sched_local_pop_hits" => ([] -> Some(I64));
    NONE; "willow_sched_global_pop_hits" => ([] -> Some(I64));
    NONE; "willow_sched_global_pop_attempts" => ([] -> Some(I64));
    NONE; "willow_sched_steal_attempts" => ([] -> Some(I64));
    NONE; "willow_sched_steal_successes" => ([] -> Some(I64));
    NONE; "willow_sched_steal_failures" => ([] -> Some(I64));
    NONE; "willow_sched_victim_locks" => ([] -> Some(I64));
    NONE; "willow_sched_global_pushes" => ([] -> Some(I64));
    NONE; "willow_sched_local_pushes" => ([] -> Some(I64));
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
    NONE; "willow_gc_stop_flag" => ([] -> Some(Ptr));
    PREEMPT; "willow_gc_safepoint" => ([] -> None);
    // --- arrays (std::collections::Array) ---
    PANIC_ALLOC; "willow_array_new" => ([I64, I64] -> Some(Ptr));
    PANIC_ALLOC; "willow_array_copy" => ([Ptr] -> Some(Ptr));
    PANIC_ALLOC; "willow_array_len" => ([Ptr] -> Some(I64));
    PANIC_ALLOC; "willow_array_get" => ([Ptr, I64] -> Some(Word));
    PANIC_ALLOC; "willow_array_set" => ([Ptr, I64, Word] -> None);
    PANIC_ALLOC; "willow_array_push" => ([Ptr, Word] -> None);
    PANIC_ALLOC; "willow_array_pop" => ([Ptr] -> Some(Word));
    PANIC_ALLOC; "willow_array_to_string" => ([Ptr, I64] -> Some(Ptr));
    ALLOC; "willow_map_to_string" => ([Ptr] -> Some(Ptr));
    PANIC_ALLOC; "willow_array_element_addr" => ([Ptr, I64] -> Some(Ptr));
    PANIC_ALLOC; "willow_array_reference_owner" => ([Ptr, I64] -> Some(Ptr));
    // --- maps (std::collections::Map) ---
    ALLOC; "willow_map_new" => ([I64, I64, I64] -> Some(Ptr));
    ALLOC; "willow_map_copy" => ([Ptr] -> Some(Ptr));
    // (map, key_word, key_is_ref, val_word, val_is_ref): each generic payload
    // word is followed by its own is-reference flag (willow-9tls.7).
    PANIC_ALLOC; "willow_map_insert" => ([Ptr, Word, I64, Word, I64] -> None);
    PANIC_ALLOC; "willow_map_get" => ([Ptr, Word, I64, I64] -> Some(Ptr));
    NONE; "willow_map_len" => ([Ptr] -> Some(I64));
    PANIC_ALLOC; "willow_map_contains" => ([Ptr, Word, I64] -> Some(I64));
    // --- timer ---
    NONE; "willow_runtime_sleep" => ([I64] -> Some(Ptr));
    NONE; "willow_runtime_yield" => ([] -> Some(Ptr));
    // --- netpoll ---
    NONE; "willow_netpoll_init" => ([] -> Some(I32));
    NONE; "willow_netpoll_register" => ([I64, I32] -> Some(I32));
    NONE; "willow_netpoll_reregister" => ([I64, I32] -> Some(I32));
    NONE; "willow_netpoll_deregister" => ([I64] -> Some(I32));
    SUSPEND; "willow_netpoll_wait" => ([I64] -> Some(I64));
    NONE; "willow_netpoll_wake" => ([I64] -> Some(I64));
    // --- futures ---
    NONE; "willow_future_ready_void" => ([] -> Some(Ptr));
    NONE; "willow_future_ready_i64" => ([I64] -> Some(Ptr));
    NONE; "willow_future_ready_bool" => ([I8] -> Some(Ptr));
    NONE; "willow_future_ready_f64" => ([F64] -> Some(Ptr));
    NONE; "willow_future_ready_ptr" => ([Ptr] -> Some(Ptr));
    NONE; "willow_future_await_void" => ([Ptr] -> Some(I8));
    NONE; "willow_future_await_i64" => ([Ptr] -> Some(I64));
    NONE; "willow_future_await_bool" => ([Ptr] -> Some(I8));
    NONE; "willow_future_await_f64" => ([Ptr] -> Some(F64));
    NONE; "willow_future_await_ptr" => ([Ptr] -> Some(Ptr));
    // --- channels ---
    // Atomic primitives (willow-dgwo.3). Handles are native pointers;
    // AtomicBool values use the dedicated I8 representation.
    ALLOC; "willow_atomic_i64_new" => ([I64] -> Some(Ptr));
    NONE; "willow_atomic_i64_load" => ([Ptr] -> Some(I64));
    NONE; "willow_atomic_i64_store" => ([Ptr, I64] -> None);
    NONE; "willow_atomic_i64_add" => ([Ptr, I64] -> Some(I64));
    NONE; "willow_atomic_i64_sub" => ([Ptr, I64] -> Some(I64));
    NONE; "willow_atomic_i64_swap" => ([Ptr, I64] -> Some(I64));
    ALLOC; "willow_atomic_bool_new" => ([I8] -> Some(Ptr));
    NONE; "willow_atomic_bool_load" => ([Ptr] -> Some(I8));
    NONE; "willow_atomic_bool_store" => ([Ptr, I8] -> None);
    NONE; "willow_atomic_bool_swap" => ([Ptr, I8] -> Some(I8));
    // Blocking cells hold a generic Willow word plus an is-reference flag.
    // The cells are GC-managed payloads (willow-9tls.5): `new` allocates the
    // handle on the GC heap and roots a reference word across that allocation.
    ALLOC; "willow_blocking_cell_new" => ([Word, I64] -> Some(Ptr));
    BLOCK; "willow_blocking_cell_get" => ([Ptr] -> Some(Word));
    BLOCK; "willow_blocking_cell_set" => ([Ptr, Word] -> None);
    ALLOC; "willow_blocking_rw_cell_new" => ([Word, I64] -> Some(Ptr));
    BLOCK; "willow_blocking_rw_cell_read" => ([Ptr] -> Some(Word));
    BLOCK; "willow_blocking_rw_cell_write" => ([Ptr, Word] -> None);
    // Scheduler-aware Mutex<T> (willow-38w.1.3): acquire/poll return a status
    // code (1 acquired, 0 pending, -1 recursive, -2 lost, -3 cancelled) and publish the
    // registration token through the out-parameter, so a parked acquire can
    // re-identify its own generation after a wake.
    ALLOC; "willow_async_mutex_new" => ([Word, I64] -> Some(Ptr));
    SUSPEND; "willow_async_mutex_acquire" => ([Ptr, Ptr] -> Some(I32));
    SUSPEND; "willow_async_mutex_poll" => ([Ptr, I64] -> Some(I32));
    NONE; "willow_async_mutex_load" => ([Ptr, I64] -> Some(Word));
    NONE; "willow_async_mutex_commit" => ([Ptr, I64, Word] -> Some(I32));
    NONE; "willow_async_mutex_release" => ([Ptr, I64] -> Some(I32));
    NONE; "willow_async_mutex_cancel" => ([] -> Some(I32));
    PANIC_ALLOC; "willow_async_mutex_recursive_panic" => ([Ptr, I32, I32] -> None);
    NONE; "willow_async_mutex_invalid_status" => ([I32, I32] -> None);
    // Scheduler-aware RwLock<T> (willow-38w.1.5). Mode is 1=read, 2=write;
    // handoff wakes either one writer or the contiguous reader prefix.
    ALLOC; "willow_async_rwlock_new" => ([Word, I64] -> Some(Ptr));
    SUSPEND; "willow_async_rwlock_acquire" => ([Ptr, I32, Ptr] -> Some(I32));
    SUSPEND; "willow_async_rwlock_poll" => ([Ptr, I64] -> Some(I32));
    NONE; "willow_async_rwlock_load" => ([Ptr, I64] -> Some(Word));
    NONE; "willow_async_rwlock_commit" => ([Ptr, I64, Word] -> Some(I32));
    NONE; "willow_async_rwlock_release" => ([Ptr, I64] -> Some(I32));
    NONE; "willow_async_rwlock_cancel" => ([] -> Some(I32));
    PANIC_ALLOC; "willow_async_rwlock_recursive_panic" => ([Ptr, I32, I32] -> None);
    NONE; "willow_async_rwlock_invalid_status" => ([I32, I32] -> None);
    ALLOC; "willow_channel_new" => ([I64] -> Some(Ptr));
    PANIC_ALLOC; "willow_channel_send_i64" => ([Ptr, I64] -> None);
    PANIC_ALLOC; "willow_channel_send_bool" => ([Ptr, I8] -> None);
    PANIC_ALLOC; "willow_channel_send_f64" => ([Ptr, F64] -> None);
    PANIC_ALLOC; "willow_channel_send_ptr" => ([Ptr, Ptr] -> None);
    PANIC_ALLOC; "willow_channel_recv_i64" => ([Ptr] -> Some(I64));
    PANIC_ALLOC; "willow_channel_recv_bool" => ([Ptr] -> Some(I8));
    PANIC_ALLOC; "willow_channel_recv_f64" => ([Ptr] -> Some(F64));
    PANIC_ALLOC; "willow_channel_recv_ptr" => ([Ptr] -> Some(Ptr));
    NONE; "willow_channel_close" => ([Ptr] -> None);
    SUSPEND; "willow_channel_recv_ready" => ([Ptr] -> Some(I32));
    NONE; "willow_channel_unregister_waiter" => ([Ptr] -> None);
    NONE; "willow_channel_select_cleanup" => ([Ptr, Ptr, I64] -> None);
    PANIC_ALLOC; "willow_channel_new_bounded" => ([I64, I64] -> Some(Ptr));
    NONE; "willow_channel_send_ready" => ([Ptr] -> Some(I32));
    NONE; "willow_channel_try_send_i64" => ([Ptr, I64] -> Some(I32));
    NONE; "willow_channel_try_send_bool" => ([Ptr, I8] -> Some(I32));
    NONE; "willow_channel_try_send_f64" => ([Ptr, F64] -> Some(I32));
    NONE; "willow_channel_try_send_ptr" => ([Ptr, Ptr] -> Some(I32));
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
    NONE; "willow_panic" => ([Ptr] -> None);
    NONE; "willow_main_fail" => ([Ptr] -> None);
    NONE; "willow_panic_at" => ([Ptr, Ptr, I32, I32] -> None);
    PANIC_ALLOC; "willow_panic_raise" => ([Ptr, Ptr, I64, I64] -> None);
    NONE; "willow_panic_active" => ([] -> Some(I32));
    NONE; "willow_panic_depth" => ([] -> Some(I32));
    NONE; "willow_panic_enter_defer" => ([] -> None);
    NONE; "willow_panic_leave_defer" => ([] -> None);
    NONE; "willow_panic_recover" => ([] -> Some(Ptr));
    NONE; "willow_panic_release_recovered" => ([Ptr] -> None);
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
    ALLOC; "willow_async_frame_alloc" => ([I64, I64] -> Some(Ptr));
    NO_PREEMPT; "willow_sched_spawn" => ([Ptr, Ptr] -> Some(I64));
    NONE; "willow_sched_run" => ([] -> Some(I64));
    NONE; "willow_sched_run_until" => ([I64] -> Some(I64));
    NONE; "willow_sched_run_until_deadline" => ([I64] -> Some(I64));
    BLOCK_PANIC_ALLOC; "willow_select_idle_wait" => ([] -> None);
    NONE; "willow_sched_wake" => ([I64] -> None);
    NONE; "willow_sched_cancel" => ([I64] -> None);
    NONE; "willow_sched_is_cancelled" => ([I64] -> Some(I64));
    NONE; "willow_sched_set_spawn_site" => ([I64, Ptr, I64] -> None);
    NONE; "willow_sched_set_cancel_fn" => ([I64, Ptr] -> None);
    NONE; "willow_sched_set_cancel_fn_cooperative" => ([I64, Ptr, I32] -> None);
    ALLOC; "willow_fs_temp_path" => ([Ptr] -> Some(Ptr));
    BLOCK_ALLOC; "willow_fs_read_to_string" => ([Ptr] -> Some(Ptr));
    BLOCK_ALLOC; "willow_fs_write_string" => ([Ptr, Ptr] -> Some(Ptr));
    BLOCK_ALLOC; "willow_fs_exists" => ([Ptr] -> Some(I64));
    BLOCK_ALLOC; "willow_fs_remove_file" => ([Ptr] -> Some(Ptr));
    ALLOC; "willow_fs_read_to_string_async" => ([Ptr] -> Some(Ptr));
    ALLOC; "willow_fs_write_string_async" => ([Ptr, Ptr] -> Some(Ptr));
    ALLOC; "willow_fs_exists_async" => ([Ptr] -> Some(Ptr));
    ALLOC; "willow_fs_remove_file_async" => ([Ptr] -> Some(Ptr));
    // --- scheduler-aware TCP (`std::net`) ---
    BLOCK_ALLOC; "willow_net_bind" => ([Ptr] -> Some(Ptr));
    ALLOC; "willow_net_local_addr" => ([Ptr] -> Some(Ptr));
    ALLOC; "willow_net_peer_addr" => ([Ptr] -> Some(Ptr));
    ALLOC; "willow_net_shutdown" => ([Ptr] -> Some(Ptr));
    ALLOC; "willow_net_connect_async" => ([Ptr] -> Some(Ptr));
    ALLOC; "willow_net_accept_async" => ([Ptr] -> Some(Ptr));
    ALLOC; "willow_net_read_async" => ([Ptr, I64] -> Some(Ptr));
    ALLOC; "willow_net_write_async" => ([Ptr, Ptr] -> Some(Ptr));
    // --- cancellation tokens and structured scopes ---
    ALLOC; "willow_cancellation_token_new" => ([] -> Some(Ptr));
    ALLOC; "willow_cancellation_token_child" => ([Ptr] -> Some(Ptr));
    ALLOC; "willow_cancellation_token_attach" => ([Ptr, Ptr] -> Some(Ptr));
    ALLOC; "willow_cancellation_token_cancel" => ([Ptr] -> None);
    NONE; "willow_cancellation_token_is_cancelled" => ([Ptr] -> Some(I64));
    ALLOC; "willow_task_scope_new" => ([] -> Some(Ptr));
    ALLOC; "willow_task_scope_child" => ([Ptr] -> Some(Ptr));
    ALLOC; "willow_task_scope_add" => ([Ptr, Ptr] -> Some(Ptr));
    ALLOC; "willow_task_scope_cancel" => ([Ptr] -> None);
    NONE; "willow_task_scope_is_cancelled" => ([Ptr] -> Some(I64));
    ALLOC; "willow_task_scope_finish" => ([Ptr] -> Some(Ptr));
    // --- bounded parallel collection mapping ---
    // The mapper is a native function pointer. Generated Willow function
    // values are 64-bit on every accepted target, but the ABI classification
    // still distinguishes an address the runtime calls from an integer.
    PANIC_ALLOC; "willow_parallel_map_i64" => ([Ptr, Ptr] -> Some(Ptr));
    NONE; "willow_blocking_active_jobs" => ([] -> Some(I64));
    NONE; "willow_blocking_completed_jobs" => ([] -> Some(I64));
    // Bounded blocking-pool queue gauges (willow-9tls.6).
    NONE; "willow_blocking_queued_jobs" => ([] -> Some(I64));
    NONE; "willow_blocking_slot_waiters" => ([] -> Some(I64));
    NONE; "willow_blocking_queue_capacity" => ([] -> Some(I64));
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
    NONE; "willow_frame_status" => ([Ptr] -> Some(I64));
    NONE; "willow_frame_is_cancelled" => ([Ptr] -> Some(I64));
    SUSPEND; "willow_frame_await" => ([Ptr, I64] -> Some(I32));
    PANIC_ALLOC; "willow_frame_await_check" => ([Ptr, I64] -> None);
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
    TASK_STACK; "willow_task_stack_enter" => ([Ptr, Ptr] -> Some(I32));
    NONE; "willow_task_stack_leave" => ([] -> None);
    NO_PREEMPT; "willow_sched_spawn_cooperative" => ([Ptr, Ptr] -> Some(I64));
    PREEMPT; "willow_sync_safepoint" => ([] -> Some(I32));
    NONE; "willow_sync_native_active" => ([] -> Some(I32));
    NONE; "willow_sync_cancelled" => ([] -> Some(I32));
    NONE; "willow_sync_cleanup_enter" => ([] -> None);
    NONE; "willow_sync_cleanup_leave" => ([] -> None);
    PREEMPT; "willow_sync_poll_cancel_cleanup" => ([] -> None);
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

    #[test]
    fn the_aliases_spell_the_combinations_they_are_named_for() {
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
