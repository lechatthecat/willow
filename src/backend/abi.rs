//! Single source of truth for the runtime ABI surface imported by the backend.
//!
//! Every runtime symbol the Cranelift backend calls into `libwillow_runtime`
//! is listed in [`RUNTIME_SYMBOLS`] together with its parameter and return
//! kinds. `Codegen::declare_runtime` iterates over this table instead of
//! hand-writing one `declare_function` block per symbol, so the backend's view
//! of the ABI lives in exactly one place.
//!
//! Integration link tests keep this table and the actual exported staticlib
//! symbols in sync.

use cranelift_codegen::ir::{AbiParam, Type, types};

/// ABI-level scalar kind for a runtime parameter or return value.
///
/// `Ptr` is kept distinct from `I64` to preserve the backend's intent even
/// though both lower to the 64-bit pointer type on the currently supported
/// targets. Keeping the distinction makes the table read like the runtime's
/// own `extern "C"` signatures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbiTy {
    I64,
    I32,
    I8,
    F64,
    Ptr,
}

impl AbiTy {
    /// Lower to the concrete Cranelift type, given the module's pointer type.
    pub fn clif(self, ptr_ty: Type) -> Type {
        match self {
            AbiTy::I64 => types::I64,
            AbiTy::I32 => types::I32,
            AbiTy::I8 => types::I8,
            AbiTy::F64 => types::F64,
            AbiTy::Ptr => ptr_ty,
        }
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
}

impl RuntimeSymbol {
    /// Push this symbol's parameters and return onto a Cranelift signature.
    ///
    /// The caller supplies a signature created via `Module::make_signature`
    /// (which carries the module's default call convention) and the module's
    /// pointer type for lowering [`AbiTy::Ptr`].
    pub fn fill_signature(&self, sig: &mut cranelift_codegen::ir::Signature, ptr_ty: Type) {
        for param in self.params {
            sig.params.push(AbiParam::new(param.clif(ptr_ty)));
        }
        if let Some(ret) = self.ret {
            sig.returns.push(AbiParam::new(ret.clif(ptr_ty)));
        }
    }

    /// Scheduler/GC effects used when deciding whether generated code may keep
    /// an unrooted value across this runtime call (preemption spec §21).
    pub const fn effects(&self) -> RuntimeEffects {
        runtime_effects(self.name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeEffects(u8);

impl RuntimeEffects {
    pub const NONE: Self = Self(0);
    pub const MAY_ALLOCATE: Self = Self(1 << 0);
    pub const MAY_BLOCK: Self = Self(1 << 1);
    pub const MAY_SUSPEND: Self = Self(1 << 2);
    pub const MAY_PREEMPT: Self = Self(1 << 3);
    pub const NO_PREEMPT_REGION: Self = Self(1 << 4);

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn contains(self, effect: Self) -> bool {
        self.0 & effect.0 == effect.0
    }
}

const fn runtime_effects(name: &str) -> RuntimeEffects {
    match name.as_bytes() {
        b"willow_alloc"
        | b"willow_alloc_typed"
        | b"willow_gc_alloc_layout"
        | b"willow_gc_alloc_slow"
        | b"willow_async_frame_alloc"
        | b"willow_string_alloc"
        | b"willow_string_concat" => RuntimeEffects::MAY_ALLOCATE,
        b"willow_fs_read_to_string"
        | b"willow_fs_write_string"
        | b"willow_fs_exists"
        | b"willow_fs_remove_file" => RuntimeEffects::MAY_BLOCK.union(RuntimeEffects::MAY_ALLOCATE),
        b"willow_fs_read_to_string_async"
        | b"willow_fs_write_string_async"
        | b"willow_fs_exists_async"
        | b"willow_fs_remove_file_async" => {
            RuntimeEffects::MAY_ALLOCATE.union(RuntimeEffects::MAY_SUSPEND)
        }
        b"willow_sched_await"
        | b"willow_sched_sleep"
        | b"willow_channel_recv_ready"
        | b"willow_netpoll_wait" => RuntimeEffects::MAY_SUSPEND,
        b"willow_gc_collect"
        | b"willow_gc_minor_collect"
        | b"willow_gc_safepoint"
        | b"willow_preempt_check" => RuntimeEffects::MAY_PREEMPT,
        b"willow_push_root"
        | b"willow_pop_root"
        | b"willow_pop_roots"
        | b"willow_sched_spawn"
        | b"willow_channel_unregister_waiter" => RuntimeEffects::NO_PREEMPT_REGION,
        _ => RuntimeEffects::NONE,
    }
}

use AbiTy::{F64, I8, I32, I64, Ptr};

/// Declare the backend-facing runtime ABI once and generate the typed table
/// consumed by Cranelift. Keeping the compact signatures in one invocation
/// makes additions reviewable and prevents declaration code from drifting.
macro_rules! runtime_abi_schema {
    ($($name:literal => ([$($param:ident),* $(,)?] -> $ret:expr);)*) => {
        &[
            $(RuntimeSymbol {
                name: $name,
                params: &[$($param),*],
                ret: $ret,
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
    "willow_print_i64" => ([I64] -> None);
    "willow_println_i64" => ([I64] -> None);
    "willow_print_bool" => ([I8] -> None);
    "willow_println_bool" => ([I8] -> None);
    "willow_print_f64" => ([F64] -> None);
    "willow_println_f64" => ([F64] -> None);
    "willow_print_string" => ([I64] -> None);
    "willow_println_string" => ([I64] -> None);
    // --- math / float formatting ---
    "willow_pow_f64" => ([F64, F64] -> Some(F64));
    "willow_f64_to_string" => ([F64] -> Some(I64));
    "willow_i64_to_string" => ([I64] -> Some(I64));
    "willow_bool_to_string" => ([I8] -> Some(I64));
    "willow_f64_parse" => ([I64] -> Some(I64));
    "willow_format_f64_17g" => ([F64] -> Some(I64));
    "willow_format_f64_16f" => ([F64] -> Some(I64));
    "willow_format_f64_6f" => ([F64] -> Some(I64));
    // --- string ---
    "willow_string_concat" => ([I64, I64] -> Some(I64));
    "willow_string_eq" => ([Ptr, Ptr] -> Some(I64));
    "willow_string_alloc" => ([I64, I64] -> Some(I64));
    "willow_string_literal" => ([I64, I64] -> Some(I64));
    // --- args ---
    "willow_runtime_args_len" => ([] -> Some(I64));
    "willow_runtime_arg" => ([I64] -> Some(I64));
    "willow_runtime_program_name" => ([] -> Some(I64));
    "willow_runtime_args_array" => ([] -> Some(I64));
    // --- GC allocation ---
    "willow_alloc" => ([I64] -> Some(I64));
    "willow_alloc_typed" => ([I64, I64] -> Some(I64));
    "willow_gc_alloc_layout" => ([I64, I64, I64, I64] -> Some(I64));
    "willow_gc_alloc_slow" => ([Ptr, I64, I64, I64, I64] -> Some(I64));
    "willow_gc_write_barrier" => ([Ptr, Ptr, I64] -> None);
    "willow_gc_collect" => ([] -> None);
    "willow_gc_minor_collect" => ([] -> None);
    "willow_gc_allocated_bytes" => ([] -> Some(I64));
    "willow_gc_tlab_fast_allocations" => ([] -> Some(I64));
    "willow_gc_tlab_slow_allocations" => ([] -> Some(I64));
    "willow_gc_tlab_refills" => ([] -> Some(I64));
    "willow_gc_tlab_large_allocations" => ([] -> Some(I64));
    "willow_gc_tlab_reserved_bytes" => ([] -> Some(I64));
    "willow_gc_minor_collections" => ([] -> Some(I64));
    "willow_gc_promoted_objects" => ([] -> Some(I64));
    "willow_gc_moved_objects" => ([] -> Some(I64));
    "willow_gc_remembered_set_size" => ([] -> Some(I64));
    "willow_gc_dirty_card_count" => ([] -> Some(I64));
    "willow_gc_write_barrier_hits" => ([] -> Some(I64));
    // --- multi-mutator coordination (willow-6fv.5.6) ---
    "willow_gc_register_mutator" => ([] -> None);
    "willow_gc_unregister_mutator" => ([] -> None);
    "willow_gc_safepoint" => ([] -> None);
    // --- arrays (std::collections::Array) ---
    "willow_array_new" => ([I64, I64] -> Some(I64));
    "willow_array_copy" => ([I64] -> Some(I64));
    "willow_array_len" => ([I64] -> Some(I64));
    "willow_array_get" => ([I64, I64] -> Some(I64));
    "willow_array_set" => ([I64, I64, I64] -> None);
    "willow_array_push" => ([I64, I64] -> None);
    "willow_array_pop" => ([I64] -> Some(I64));
    "willow_array_to_string" => ([Ptr, I64] -> Some(Ptr));
    "willow_map_to_string" => ([Ptr, I64] -> Some(Ptr));
    "willow_array_element_addr" => ([I64, I64] -> Some(I64));
    // --- maps (std::collections::Map) ---
    "willow_map_new" => ([] -> Some(I64));
    "willow_map_copy" => ([I64] -> Some(I64));
    "willow_map_insert" => ([I64, I64, I64, I64, I64] -> None);
    "willow_map_get" => ([I64, I64, I64] -> Some(I64));
    "willow_map_len" => ([I64] -> Some(I64));
    "willow_map_contains" => ([I64, I64, I64] -> Some(I64));
    // --- timer ---
    "willow_runtime_sleep" => ([I64] -> Some(I64));
    "willow_runtime_yield" => ([] -> Some(I64));
    // --- netpoll ---
    "willow_netpoll_init" => ([] -> Some(I32));
    "willow_netpoll_register" => ([I64, I32] -> Some(I32));
    "willow_netpoll_reregister" => ([I64, I32] -> Some(I32));
    "willow_netpoll_deregister" => ([I64] -> Some(I32));
    "willow_netpoll_wait" => ([I64] -> Some(I64));
    "willow_netpoll_wake" => ([I64] -> Some(I64));
    // --- futures ---
    "willow_future_ready_void" => ([] -> Some(I64));
    "willow_future_ready_i64" => ([I64] -> Some(I64));
    "willow_future_ready_bool" => ([I8] -> Some(I64));
    "willow_future_ready_f64" => ([F64] -> Some(I64));
    "willow_future_ready_ptr" => ([I64] -> Some(I64));
    "willow_future_await_void" => ([I64] -> Some(I8));
    "willow_future_await_i64" => ([I64] -> Some(I64));
    "willow_future_await_bool" => ([I64] -> Some(I8));
    "willow_future_await_f64" => ([I64] -> Some(F64));
    "willow_future_await_ptr" => ([I64] -> Some(I64));
    // --- channels ---
    // Atomic primitives (willow-dgwo.3). Pointers are I64; AtomicBool values I8.
    "willow_atomic_i64_new" => ([I64] -> Some(I64));
    "willow_atomic_i64_load" => ([I64] -> Some(I64));
    "willow_atomic_i64_store" => ([I64, I64] -> None);
    "willow_atomic_i64_add" => ([I64, I64] -> Some(I64));
    "willow_atomic_i64_sub" => ([I64, I64] -> Some(I64));
    "willow_atomic_i64_swap" => ([I64, I64] -> Some(I64));
    "willow_atomic_bool_new" => ([I8] -> Some(I64));
    "willow_atomic_bool_load" => ([I64] -> Some(I8));
    "willow_atomic_bool_store" => ([I64, I8] -> None);
    "willow_atomic_bool_swap" => ([I64, I8] -> Some(I8));
    // Mutex<T> / RwLock<T> (willow-dgwo.3): word-based cells. (ptr, value) words.
    "willow_mutex_new" => ([I64, I64] -> Some(I64));
    "willow_mutex_get" => ([I64] -> Some(I64));
    "willow_mutex_set" => ([I64, I64] -> None);
    "willow_rwlock_new" => ([I64, I64] -> Some(I64));
    "willow_rwlock_read" => ([I64] -> Some(I64));
    "willow_rwlock_write" => ([I64, I64] -> None);
    "willow_channel_new" => ([I64] -> Some(I64));
    "willow_channel_send_i64" => ([I64, I64] -> None);
    "willow_channel_send_bool" => ([I64, I8] -> None);
    "willow_channel_send_f64" => ([I64, F64] -> None);
    "willow_channel_send_ptr" => ([I64, I64] -> None);
    "willow_channel_recv_i64" => ([I64] -> Some(I64));
    "willow_channel_recv_bool" => ([I64] -> Some(I8));
    "willow_channel_recv_f64" => ([I64] -> Some(F64));
    "willow_channel_recv_ptr" => ([I64] -> Some(I64));
    "willow_channel_close" => ([I64] -> None);
    "willow_channel_recv_ready" => ([I64] -> Some(I32));
    "willow_channel_unregister_waiter" => ([I64] -> None);
    "willow_select_rotation" => ([] -> Some(I64));
    // --- GC roots ---
    "willow_push_root" => ([I64] -> None);
    "willow_pop_roots" => ([I32] -> None);
    // --- panic ---
    "willow_nil_deref" => ([Ptr, I32, I32, Ptr] -> None);
    "willow_int_div_panic" => ([I64, Ptr, I32, I32] -> None);
    "willow_panic" => ([Ptr] -> None);
    "willow_main_fail" => ([Ptr] -> None);
    "willow_panic_at" => ([Ptr, Ptr, I32, I32] -> None);
    // --- debug call-chain stack (willow-992h) ---
    "willow_callstack_push" => ([Ptr, I64, Ptr, I64, I32, I32] -> None);
    "willow_callstack_pop" => ([] -> None);
    // --- reference debug metadata ---
    "willow_debug_reference_call" => ([Ptr, I32, I32, Ptr, Ptr, Ptr, Ptr, Ptr, Ptr] -> None);
    "willow_debug_reference_call_clear" => ([] -> None);
    // Async frame allocator + cooperative scheduler (willow-lpn.5 / willow-fqg.1).
    // Imported so the async state-machine lowering can emit frame allocation and
    // cooperative spawn/poll/wake calls.
    "willow_async_frame_alloc" => ([I64, I64] -> Some(Ptr));
    "willow_sched_spawn" => ([Ptr, Ptr] -> Some(I64));
    "willow_sched_run" => ([] -> Some(I64));
    "willow_sched_run_until" => ([I64] -> Some(I64));
    "willow_sched_wake" => ([I64] -> None);
    "willow_sched_cancel" => ([I64] -> None);
    "willow_sched_is_cancelled" => ([I64] -> Some(I64));
    "willow_sched_join_check" => ([I64] -> None);
    "willow_sched_set_spawn_site" => ([I64, Ptr, I64] -> None);
    "willow_sched_set_cancel_fn" => ([I64, Ptr] -> None);
    "willow_fs_temp_path" => ([Ptr] -> Some(Ptr));
    "willow_fs_read_to_string" => ([Ptr] -> Some(Ptr));
    "willow_fs_write_string" => ([Ptr, Ptr] -> Some(Ptr));
    "willow_fs_exists" => ([Ptr] -> Some(I64));
    "willow_fs_remove_file" => ([Ptr] -> Some(Ptr));
    "willow_fs_read_to_string_async" => ([Ptr] -> Some(Ptr));
    "willow_fs_write_string_async" => ([Ptr, Ptr] -> Some(Ptr));
    "willow_fs_exists_async" => ([Ptr] -> Some(Ptr));
    "willow_fs_remove_file_async" => ([Ptr] -> Some(Ptr));
    "willow_blocking_active_jobs" => ([] -> Some(I64));
    "willow_blocking_completed_jobs" => ([] -> Some(I64));
    "willow_sched_current_task" => ([] -> Some(I64));
    // Tag the running task with its async fn name for async stack traces
    // (willow-9lw): (name_ptr, name_len).
    "willow_sched_tag_current_task" => ([Ptr, I64] -> None);
    "willow_sched_sleep" => ([I64] -> None);
    "willow_sched_yield" => ([] -> None);
    "willow_sched_await" => ([I64] -> Some(I32));
    "willow_sched_task_state" => ([I64] -> Some(I32));
    // --- preemption (willow-0a6k.1, spec §7-9,22-23). Flag pointers are I64.
    // Emitted by compiler-inserted safepoints in willow-0a6k.2; declared here so
    // the runtime ABI surface + symbol-export tests cover them from stage 1. ---
    "willow_preempt_task_budget" => ([] -> Some(I64));
    "willow_preempt_time_quantum_ms" => ([] -> Some(I64));
    "willow_preempt_flag_new" => ([] -> Some(I64));
    "willow_preempt_flag_free" => ([I64] -> None);
    "willow_preempt_request" => ([I64] -> None);
    "willow_preempt_clear" => ([I64] -> None);
    "willow_preempt_requested" => ([I64] -> Some(I32));
    "willow_preempt_begin" => ([I64] -> None);
    "willow_preempt_end" => ([] -> None);
    "willow_preempt_check" => ([] -> Some(I32));
    "willow_preempt_enter_no_preempt" => ([] -> None);
    "willow_preempt_leave_no_preempt" => ([] -> None);
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn abity_lowers_pointer_to_module_pointer_type() {
        // On the supported 64-bit targets the pointer type is I64; the lowering
        // must route Ptr through the supplied pointer type, not a hard-coded one.
        assert_eq!(AbiTy::Ptr.clif(types::I64), types::I64);
        assert_eq!(AbiTy::Ptr.clif(types::I32), types::I32);
        assert_eq!(AbiTy::I8.clif(types::I64), types::I8);
        assert_eq!(AbiTy::I32.clif(types::I64), types::I32);
        assert_eq!(AbiTy::F64.clif(types::I64), types::F64);
        assert_eq!(AbiTy::I64.clif(types::I32), types::I64);
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
        assert!(
            effects("willow_fs_read_to_string_async")
                .contains(RuntimeEffects::MAY_SUSPEND.union(RuntimeEffects::MAY_ALLOCATE))
        );
        assert!(effects("willow_sched_await").contains(RuntimeEffects::MAY_SUSPEND));
        assert!(effects("willow_gc_safepoint").contains(RuntimeEffects::MAY_PREEMPT));
        assert!(
            effects("willow_channel_unregister_waiter").contains(RuntimeEffects::NO_PREEMPT_REGION)
        );
        assert_eq!(effects("willow_print_i64"), RuntimeEffects::NONE);
    }
}
