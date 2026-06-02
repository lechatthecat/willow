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
}

use AbiTy::{F64, I8, I32, I64, Ptr};

/// The complete set of runtime symbols the backend imports.
///
/// This is the generated-code-facing ABI surface; runtime-only symbols are
/// called from within the runtime and are not emitted by the backend.
pub const RUNTIME_SYMBOLS: &[RuntimeSymbol] = &[
    // --- print ---
    RuntimeSymbol {
        name: "willow_print_i64",
        params: &[I64],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_println_i64",
        params: &[I64],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_print_bool",
        params: &[I8],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_println_bool",
        params: &[I8],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_print_f64",
        params: &[F64],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_println_f64",
        params: &[F64],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_print_string",
        params: &[I64],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_println_string",
        params: &[I64],
        ret: None,
    },
    // --- math / float formatting ---
    RuntimeSymbol {
        name: "willow_pow_f64",
        params: &[F64, F64],
        ret: Some(F64),
    },
    RuntimeSymbol {
        name: "willow_f64_to_string",
        params: &[F64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_i64_to_string",
        params: &[I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_bool_to_string",
        params: &[I8],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_f64_parse",
        params: &[I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_format_f64_17g",
        params: &[F64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_format_f64_16f",
        params: &[F64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_format_f64_6f",
        params: &[F64],
        ret: Some(I64),
    },
    // --- string ---
    RuntimeSymbol {
        name: "willow_string_concat",
        params: &[I64, I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_string_alloc",
        params: &[I64, I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_string_literal",
        params: &[I64, I64],
        ret: Some(I64),
    },
    // --- args ---
    RuntimeSymbol {
        name: "willow_runtime_args_len",
        params: &[],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_runtime_arg",
        params: &[I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_runtime_program_name",
        params: &[],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_runtime_args_array",
        params: &[],
        ret: Some(I64),
    },
    // --- GC allocation ---
    RuntimeSymbol {
        name: "willow_alloc",
        params: &[I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_alloc_typed",
        params: &[I64, I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_gc_collect",
        params: &[],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_gc_allocated_bytes",
        params: &[],
        ret: Some(I64),
    },
    // --- arrays (std::collections::Array) ---
    RuntimeSymbol {
        name: "willow_array_new",
        params: &[I64, I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_array_len",
        params: &[I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_array_get",
        params: &[I64, I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_array_set",
        params: &[I64, I64, I64],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_array_push",
        params: &[I64, I64],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_array_pop",
        params: &[I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_array_element_addr",
        params: &[I64, I64],
        ret: Some(I64),
    },
    // --- maps (std::collections::Map) ---
    RuntimeSymbol {
        name: "willow_map_new",
        params: &[],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_map_insert",
        params: &[I64, I64, I64, I64, I64],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_map_get",
        params: &[I64, I64, I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_map_len",
        params: &[I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_map_contains",
        params: &[I64, I64, I64],
        ret: Some(I64),
    },
    // --- timer ---
    RuntimeSymbol {
        name: "willow_runtime_sleep",
        params: &[I64],
        ret: Some(I64),
    },
    // --- futures ---
    RuntimeSymbol {
        name: "willow_future_ready_void",
        params: &[],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_future_ready_i64",
        params: &[I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_future_ready_bool",
        params: &[I8],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_future_ready_f64",
        params: &[F64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_future_ready_ptr",
        params: &[I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_future_await_void",
        params: &[I64],
        ret: Some(I8),
    },
    RuntimeSymbol {
        name: "willow_future_await_i64",
        params: &[I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_future_await_bool",
        params: &[I64],
        ret: Some(I8),
    },
    RuntimeSymbol {
        name: "willow_future_await_f64",
        params: &[I64],
        ret: Some(F64),
    },
    RuntimeSymbol {
        name: "willow_future_await_ptr",
        params: &[I64],
        ret: Some(I64),
    },
    // --- channels ---
    RuntimeSymbol {
        name: "willow_channel_new",
        params: &[],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_channel_send_i64",
        params: &[I64, I64],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_channel_send_bool",
        params: &[I64, I8],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_channel_send_f64",
        params: &[I64, F64],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_channel_send_ptr",
        params: &[I64, I64],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_channel_recv_i64",
        params: &[I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_channel_recv_bool",
        params: &[I64],
        ret: Some(I8),
    },
    RuntimeSymbol {
        name: "willow_channel_recv_f64",
        params: &[I64],
        ret: Some(F64),
    },
    RuntimeSymbol {
        name: "willow_channel_recv_ptr",
        params: &[I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_channel_close",
        params: &[I64],
        ret: None,
    },
    // --- GC roots ---
    RuntimeSymbol {
        name: "willow_push_root",
        params: &[I64],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_pop_roots",
        params: &[I32],
        ret: None,
    },
    // --- panic ---
    RuntimeSymbol {
        name: "willow_nil_deref",
        params: &[Ptr, I32, I32, Ptr],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_panic",
        params: &[Ptr],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_main_fail",
        params: &[Ptr],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_panic_at",
        params: &[Ptr, Ptr, I32, I32],
        ret: None,
    },
    // --- reference debug metadata ---
    RuntimeSymbol {
        name: "willow_debug_reference_call",
        params: &[Ptr, I32, I32, Ptr, Ptr, Ptr, Ptr, Ptr, Ptr],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_debug_reference_call_clear",
        params: &[],
        ret: None,
    },
    // --- tasks ---
    RuntimeSymbol {
        name: "willow_task_alloc",
        params: &[I64],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_task_spawn",
        params: &[I64, I64],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_task_join",
        params: &[I64],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_task_complete",
        params: &[I64],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_task_set_spawn_location",
        params: &[I64, Ptr, I32, I32],
        ret: None,
    },
    // Async frame allocator + cooperative scheduler (willow-lpn.5 / willow-fqg.1).
    // Imported so the async state-machine lowering can emit frame allocation and
    // cooperative spawn/poll/wake calls.
    RuntimeSymbol {
        name: "willow_async_frame_alloc",
        params: &[I64, I64],
        ret: Some(Ptr),
    },
    RuntimeSymbol {
        name: "willow_sched_spawn",
        params: &[Ptr, Ptr],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_sched_run",
        params: &[],
        ret: Some(I64),
    },
    RuntimeSymbol {
        name: "willow_sched_wake",
        params: &[I64],
        ret: None,
    },
    RuntimeSymbol {
        name: "willow_sched_task_state",
        params: &[I64],
        ret: Some(I32),
    },
];

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
}
