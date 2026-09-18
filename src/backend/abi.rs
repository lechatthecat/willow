//! Cranelift lowering of the runtime ABI surface imported by the backend.
//!
//! The schema itself — every runtime symbol the backend calls into
//! `libwillow_runtime`, with its parameter, return, and effect kinds — is
//! [`willow_abi::runtime_symbols`], shared with the runtime so the same table
//! is lowered here and pinned against the Rust `extern "C"` declarations in
//! `crates/willow_runtime/src/abi_signature_tests.rs`. This module only maps
//! the target-independent [`AbiTy`] kinds to Cranelift types.
//! `Codegen::declare_runtime` iterates over [`RUNTIME_SYMBOLS`] instead of
//! hand-writing one `declare_function` block per symbol, so the backend's view
//! of the ABI lives in exactly one place.

use cranelift_codegen::ir::{AbiParam, Type, types};
pub use willow_abi::{AbiTy, RUNTIME_SYMBOLS, RuntimeEffects, RuntimeSymbol, runtime_symbol};

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

/// Rust's extern-C `u8` boolean exports require zero extension at the ABI
/// boundary. Without it an optimized callee may inspect the full argument
/// register and see stale high bits even when the low boolean byte is zero.
fn clif_abi_param(ty: AbiTy, ptr_ty: Type) -> AbiParam {
    let param = AbiParam::new(clif_abi_ty(ty, ptr_ty));
    if ty == AbiTy::I8 { param.uext() } else { param }
}

/// Push a runtime symbol's parameters and return onto a Cranelift signature.
///
/// The caller supplies a signature created via `Module::make_signature`
/// (which carries the module's default call convention) and the module's
/// pointer type for lowering [`AbiTy::Ptr`].
pub fn fill_signature(
    symbol: &RuntimeSymbol,
    sig: &mut cranelift_codegen::ir::Signature,
    ptr_ty: Type,
) {
    for param in symbol.params {
        sig.params.push(clif_abi_param(*param, ptr_ty));
    }
    if let Some(ret) = symbol.ret {
        sig.returns.push(clif_abi_param(ret, ptr_ty));
    }
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
    fn generic_payloads_remain_64_bit_on_32_bit_targets() {
        use cranelift_codegen::isa::CallConv;
        let cases = [
            (
                "willow_array_get",
                vec![types::I32, types::I64],
                Some(types::I64),
            ),
            (
                "willow_array_set",
                vec![types::I32, types::I64, types::I64],
                None,
            ),
            (
                "willow_map_get",
                vec![types::I32, types::I64, types::I64, types::I64],
                Some(types::I32),
            ),
            (
                "willow_async_mutex_commit",
                vec![types::I32, types::I64, types::I64],
                Some(types::I32),
            ),
            (
                "willow_channel_send_ptr",
                vec![types::I32, types::I32],
                None,
            ),
            (
                "willow_sched_spawn",
                vec![types::I32, types::I32],
                Some(types::I64),
            ),
        ];
        for (name, params, ret) in cases {
            let mut sig = cranelift_codegen::ir::Signature::new(CallConv::SystemV);
            fill_signature(runtime_symbol(name).unwrap(), &mut sig, types::I32);
            assert_eq!(
                sig.params.iter().map(|p| p.value_type).collect::<Vec<_>>(),
                params,
                "{name}"
            );
            assert_eq!(sig.returns.first().map(|p| p.value_type), ret, "{name}");
        }
    }

    #[test]
    fn c_boolean_arguments_and_returns_are_zero_extended() {
        use cranelift_codegen::ir::ArgumentExtension;
        use cranelift_codegen::isa::CallConv;
        for convention in [
            CallConv::SystemV,
            CallConv::WindowsFastcall,
            CallConv::AppleAarch64,
        ] {
            for symbol in RUNTIME_SYMBOLS {
                let mut sig = cranelift_codegen::ir::Signature::new(convention);
                fill_signature(symbol, &mut sig, types::I64);
                for (kind, param) in symbol
                    .params
                    .iter()
                    .zip(&sig.params)
                    .chain(symbol.ret.iter().zip(&sig.returns))
                {
                    let expected = if *kind == AbiTy::I8 {
                        ArgumentExtension::Uext
                    } else {
                        ArgumentExtension::None
                    };
                    assert_eq!(param.extension, expected, "{} {convention:?}", symbol.name);
                }
            }
        }
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
        // The array is a native pointer; its generic element remains a
        // 64-bit payload, and the index is a fixed-width integer.
        assert_eq!(array_get.params, &[AbiTy::Ptr, AbiTy::I64]);
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
        assert_eq!(symbol.params, &[AbiTy::Ptr, AbiTy::Ptr]);
        assert_eq!(symbol.ret, Some(AbiTy::Ptr));
    }

    #[test]
    fn word_pointer_and_integer_classes_remain_distinct() {
        let signature = |name| {
            let symbol = runtime_symbol(name).unwrap_or_else(|| panic!("missing {name}"));
            (symbol.params, symbol.ret)
        };

        // Reference-only handles follow the target pointer width.
        assert_eq!(
            signature("willow_channel_try_send_ptr"),
            (&[AbiTy::Ptr, AbiTy::Ptr][..], Some(AbiTy::I32))
        );
        assert_eq!(
            signature("willow_frame_await"),
            (&[AbiTy::Ptr, AbiTy::I64][..], Some(AbiTy::I32))
        );

        // Slot addresses and callbacks are native pointers.
        assert_eq!(
            signature("willow_async_mutex_acquire"),
            (&[AbiTy::Ptr, AbiTy::Ptr][..], Some(AbiTy::I32))
        );
        assert_eq!(
            signature("willow_sched_spawn"),
            (&[AbiTy::Ptr, AbiTy::Ptr][..], Some(AbiTy::I64))
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
/// a20 the aliases spell the combinations they are named for (pinned beside
///     the aliases themselves in `willow_abi::runtime_symbols`)
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
        "willow_gc_alloc_bitmap",
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
        "willow_array_reference_owner",
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
        "willow_task_stack_enter",
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
    fn gc_stats_snapshot_signature_matches_runtime() {
        let symbol = runtime_symbol("willow_gc_stats_snapshot_v1").unwrap();
        assert_eq!(symbol.params, &[AbiTy::Ptr]);
        assert_eq!(symbol.ret, Some(AbiTy::I32));
        assert_eq!(symbol.effects, RuntimeEffects::NONE);
    }

    #[test]
    fn gc_stats_negotiation_signatures_match_runtime() {
        for (name, params) in [
            ("willow_gc_stats_size", &[AbiTy::I64][..]),
            (
                "willow_gc_stats_snapshot",
                &[AbiTy::I64, AbiTy::Ptr, AbiTy::I64][..],
            ),
        ] {
            let symbol = runtime_symbol(name).unwrap();
            assert_eq!(symbol.params, params);
            assert_eq!(symbol.ret, Some(AbiTy::I64));
            assert_eq!(symbol.effects, RuntimeEffects::NONE);
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
            (&[AbiTy::I64][..], Some(AbiTy::Ptr))
        );
        assert_eq!(
            signature("willow_f64_parse"),
            (&[AbiTy::Ptr][..], Some(AbiTy::Ptr))
        );
        assert_eq!(
            signature("willow_map_new"),
            (&[AbiTy::I64, AbiTy::I64, AbiTy::I64][..], Some(AbiTy::Ptr))
        );
        assert_eq!(
            signature("willow_channel_new"),
            (&[AbiTy::I64][..], Some(AbiTy::Ptr))
        );
        assert_eq!(
            signature("willow_runtime_program_name"),
            (&[][..], Some(AbiTy::Ptr))
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
}
