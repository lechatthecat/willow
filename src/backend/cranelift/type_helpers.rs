//! Pure type/builtin helper functions for the Cranelift backend (extracted from
//! `mod.rs` to shrink the god file — willow refactor). These map Willow `Type`s
//! to clif types, GC properties, runtime symbol names, and builtin return types;
//! none of them touch codegen state.

use std::collections::HashMap;

use cranelift_codegen::ir::types;

use crate::parser::ast::*;
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::symbols::EnumInfo;

/// The Cranelift type of a Willow FUNCTION VALUE — the one place the backend
/// decides how wide a function address is.
///
/// It is a fixed 64-bit word, NOT `target_config().pointer_type()`, and the
/// difference is deliberate. Every reference in Willow's ABI crosses the
/// runtime boundary as a 64-bit word — GC handles, strings, arrays, class
/// objects, async frames and function addresses alike — which is what lets
/// `crates/willow_runtime` declare them as plain `i64` without a per-target
/// signature (see the `willow_parallel_map_i64` note in `backend::abi`).
/// [`super::Codegen::new`] rejects any target whose pointer is not 64 bits, so
/// on every target the compiler accepts this constant and `pointer_type()`
/// agree. Widening Willow to a 32-bit target is an ABI-wide change, not a
/// matter of editing this line.
pub(crate) const FN_ADDR_TYPE: cranelift_codegen::ir::Type = types::I64;

pub(crate) fn clif_type(ty: &Type) -> cranelift_codegen::ir::Type {
    match ty {
        Type::I64 => types::I64,
        Type::F64 => types::F64,
        Type::Bool => types::I8,
        Type::String => types::I64,
        Type::Never => types::I64, // bottom type — treated as I64 for codegen purposes
        Type::Array(_) => types::I64,
        // Task<T>/JoinHandle<T> are pointers to async task frames.
        // `TaskResult<T>` is the SAME pointer viewed cancellation-awarely
        // (willow-qrj9): `result()` is an identity adapter, so it must never
        // gain a distinct representation.
        Type::Generic(_, _)
            if builtin_types::resolve(ty).is_some_and(|resolved| {
                matches!(resolved.id, B::Task | B::JoinHandle | B::TaskResult)
            }) =>
        {
            types::I64
        }
        // Future<T> is an opaque runtime future pointer.
        Type::Generic(_, _) if builtin_types::unary_arg(ty, B::Future).is_some() => types::I64,
        Type::Generic(_, _) => types::I64,
        // A function address, a fixed 64-bit word — see [`FN_ADDR_TYPE`].
        Type::Fn(_, _) => FN_ADDR_TYPE,
        Type::Named(_) => types::I64,
        Type::Void => types::I8,
    }
}

// `join_handle_result_type` / `task_result_output_type` / `awaitable_task_type`
// live in the type checker's pure type helpers: the backend must classify
// awaitable handles exactly the way the checker did, so there is one definition
// (willow-qrj9). The backend's own uses of `join_handle_result_type` went away
// with the method-call string waterfall — `Task`/`JoinHandle` receivers are now
// recognised by `intrinsics::resolve` (willow-uqzx, catalog item 7).
pub(crate) use crate::semantic::type_checker::awaitable_task_type;

pub(crate) fn task_output_type(ty: &Type) -> Option<Type> {
    builtin_types::unary_arg(ty, B::Task).cloned()
}

pub(crate) fn future_output_type(ty: &Type) -> Option<Type> {
    builtin_types::unary_arg(ty, B::Future).cloned()
}

pub(crate) fn debug_type_name(ty: &Type) -> String {
    match ty {
        Type::I64 => "i64".to_string(),
        Type::F64 => "f64".to_string(),
        Type::Bool => "bool".to_string(),
        Type::String => "String".to_string(),
        Type::Void => "void".to_string(),
        Type::Never => "!".to_string(),
        Type::Named(name) => name.clone(),
        Type::Array(element) => format!("Array<{}>", debug_type_name(element)),
        Type::Generic(name, args) => {
            let args = args
                .iter()
                .map(debug_type_name)
                .collect::<Vec<_>>()
                .join(",");
            format!("{name}<{args}>")
        }
        Type::Fn(params, ret) => {
            let param_str = params
                .iter()
                .map(debug_type_name)
                .collect::<Vec<_>>()
                .join(",");
            format!("fn({}) -> {}", param_str, debug_type_name(ret))
        }
    }
}

pub(crate) fn future_ready_runtime_name(ty: &Type) -> &'static str {
    match ty {
        Type::Void => "willow_future_ready_void",
        Type::I64 => "willow_future_ready_i64",
        Type::Bool => "willow_future_ready_bool",
        Type::F64 => "willow_future_ready_f64",
        _ => "willow_future_ready_ptr",
    }
}

pub(crate) fn future_await_runtime_name(ty: &Type) -> &'static str {
    match ty {
        Type::Void => "willow_future_await_void",
        Type::I64 => "willow_future_await_i64",
        Type::Bool => "willow_future_await_bool",
        Type::F64 => "willow_future_await_f64",
        _ => "willow_future_await_ptr",
    }
}

pub(crate) fn channel_element_type(ty: &Type) -> Option<Type> {
    builtin_types::unary_arg(ty, B::Channel).cloned()
}

/// Whether a Willow type is represented at runtime as a GC-managed heap pointer
/// (and therefore must be rooted when live across an allocation and traced when
/// stored inside another object).
///
/// `enum_infos` is required because a *fieldless* (C-like) enum — every variant
/// has no payload — is lowered to an immediate integer tag, NOT a heap pointer
/// (see `emit_static_call`).  Treating such a value as GC-managed would root or
/// trace a small integer as if it were an object pointer, and the collector
/// would dereference it as a header and crash.  An enum with at least one
/// payload-carrying variant is always heap-allocated and so is GC-managed.
/// Generic types that are opaque RUNTIME pointers (`Box::into_raw` / task-data
/// areas) WITHOUT a `willow_alloc_object` GcHeader: the collector must never
/// root or trace them as heap objects (it would read a bogus header at
/// `payload_to_header` and crash — see willow-lpn.9). Any GC references they
/// hold are kept alive by a runtime registry instead (channel buffers, lock
/// cells — willow-dsw/dgwo.3). All other generics (`Task`/`JoinHandle` async
/// frames, `Range`, `Map`, user generics) are real GC heap objects.
pub(crate) fn is_opaque_runtime_pointer_type(name: &str) -> bool {
    // Channel left this list when channels became GC-MANAGED objects
    // (willow-p4er): their handles must be traced from frames/fields like
    // any reference, or the collector reclaims a live channel. The rest are
    // Mutex/RwLock left this list in willow-38w.1.6 when their public handles
    // became traced GC objects with finalizers. The remaining cells are leaked
    // raw runtime pointers by design.
    matches!(name, "Future" | "BlockingCell" | "BlockingRwCell")
}

pub(crate) fn is_gc_managed(ty: &Type, enum_infos: &HashMap<String, EnumInfo>) -> bool {
    match ty {
        Type::Named(name) => match enum_infos.get(name) {
            // Fieldless enum → immediate tag; with-payload enum → heap object.
            Some(info) => info.variants.iter().any(|v| !v.payload_types.is_empty()),
            // Classes and other named heap types.
            None => true,
        },
        // Array<T> is a GC-managed heap object (handle + buffer); locals,
        // parameters, and class fields of array type must be rooted/traced.
        Type::Array(_) => true,
        // Opaque runtime-pointer generics (Future/Blocking* compatibility cells) are NOT
        // GC heap objects (see `is_opaque_runtime_pointer_type`); every other
        // generic — Task/JoinHandle async frames, Range, Map, user generics — is.
        Type::Generic(name, _) => !is_opaque_runtime_pointer_type(name),
        // String is now a GC-managed WillowString heap object (payload: len + bytes).
        // It is allocated through the central GC path and has a valid GcHeader.
        Type::String => true,
        _ => false,
    }
}

pub(crate) fn builtin_static_return_type(
    class: &str,
    type_args: &[Type],
    method: &str,
) -> Option<Type> {
    match (class, method) {
        ("Channel", "with_capacity") => Some(Type::Generic(
            "Channel".to_string(),
            vec![type_args.first().cloned().unwrap_or(Type::Void)],
        )),
        ("Channel", "new") => Some(Type::Generic(
            "Channel".to_string(),
            vec![type_args.first().cloned().unwrap_or(Type::Void)],
        )),
        ("AtomicI64", "new") => Some(Type::Named("AtomicI64".to_string())),
        ("AtomicBool", "new") => Some(Type::Named("AtomicBool".to_string())),
        ("Mutex", "new") => Some(Type::Generic(
            "Mutex".to_string(),
            vec![type_args.first().cloned().unwrap_or(Type::Void)],
        )),
        ("RwLock", "new") => Some(Type::Generic(
            "RwLock".to_string(),
            vec![type_args.first().cloned().unwrap_or(Type::Void)],
        )),
        ("BlockingCell", "new") => Some(Type::Generic(
            "BlockingCell".to_string(),
            vec![type_args.first().cloned().unwrap_or(Type::Void)],
        )),
        ("BlockingRwCell", "new") => Some(Type::Generic(
            "BlockingRwCell".to_string(),
            vec![type_args.first().cloned().unwrap_or(Type::Void)],
        )),
        ("CancellationToken", "new") => Some(Type::Named("CancellationToken".to_string())),
        ("TaskScope", "new") => Some(Type::Named("TaskScope".to_string())),
        ("fs", "read_to_string") => Some(Type::Generic(
            "Result".to_string(),
            vec![Type::String, Type::Named("IoError".to_string())],
        )),
        ("fs", "write_string") | ("fs", "remove_file") => Some(Type::Generic(
            "Result".to_string(),
            vec![Type::Void, Type::Named("IoError".to_string())],
        )),
        ("fs", "read_to_string_async") => Some(Type::Generic(
            "Task".to_string(),
            vec![Type::Generic(
                "Result".to_string(),
                vec![Type::String, Type::Named("IoError".to_string())],
            )],
        )),
        ("fs", "write_string_async") | ("fs", "remove_file_async") => Some(Type::Generic(
            "Task".to_string(),
            vec![Type::Generic(
                "Result".to_string(),
                vec![Type::Void, Type::Named("IoError".to_string())],
            )],
        )),
        ("fs", "exists_async") => Some(Type::Generic("Task".to_string(), vec![Type::Bool])),
        ("fs", "exists") => Some(Type::Bool),
        ("fs", "temp_path") => Some(Type::String),
        ("net", "bind") => Some(Type::Generic(
            "Result".to_string(),
            vec![
                Type::Named("TcpListener".to_string()),
                Type::Named("IoError".to_string()),
            ],
        )),
        ("net", "local_addr") | ("net", "peer_addr") => Some(Type::Generic(
            "Result".to_string(),
            vec![Type::String, Type::Named("IoError".to_string())],
        )),
        ("net", "shutdown") => Some(Type::Generic(
            "Result".to_string(),
            vec![Type::Void, Type::Named("IoError".to_string())],
        )),
        ("net", "connect_async") | ("net", "accept_async") => Some(Type::Generic(
            "Task".to_string(),
            vec![Type::Generic(
                "Result".to_string(),
                vec![
                    Type::Named("TcpStream".to_string()),
                    Type::Named("IoError".to_string()),
                ],
            )],
        )),
        ("net", "read_async") => Some(Type::Generic(
            "Task".to_string(),
            vec![Type::Generic(
                "Result".to_string(),
                vec![Type::String, Type::Named("IoError".to_string())],
            )],
        )),
        ("net", "write_async") => Some(Type::Generic(
            "Task".to_string(),
            vec![Type::Generic(
                "Result".to_string(),
                vec![Type::Void, Type::Named("IoError".to_string())],
            )],
        )),
        ("parallel", "map") => Some(Type::Generic(
            "Task".to_string(),
            vec![Type::Array(Box::new(Type::I64))],
        )),
        ("env", "args_len") => Some(Type::I64),
        ("env", "arg") => Some(Type::Generic("Option".to_string(), vec![Type::String])),
        ("env", "program_name") => Some(Type::String),
        ("env", "args") => Some(Type::Array(Box::new(Type::String))),
        ("f64", "to_string") => Some(Type::String),
        ("f64", "parse") => Some(Type::Generic(
            "Result".to_string(),
            vec![Type::F64, Type::Named("ParseFloatError".to_string())],
        )),
        _ => None,
    }
}

pub(crate) fn builtin_call_return_type(callee: &str) -> Option<Type> {
    if callee == "panic" {
        return Some(Type::Never);
    }
    match callee {
        "pow" | "powf" => Some(Type::F64),
        "format" => Some(Type::String),
        "gc_allocated_bytes"
        | "gc_tlab_fast_allocations"
        | "gc_tlab_slow_allocations"
        | "gc_tlab_refills"
        | "gc_tlab_large_allocations"
        | "gc_tlab_reserved_bytes"
        | "gc_minor_collections"
        | "gc_promoted_objects"
        | "gc_moved_objects"
        | "gc_remembered_set_size"
        | "gc_dirty_card_count"
        | "gc_write_barrier_hits" => Some(Type::I64),
        "gc_collect" | "gc_minor_collect" => Some(Type::Void),
        "sleep" | "yield" => Some(Type::Generic("Future".to_string(), vec![Type::Void])),
        _ => None,
    }
}

pub(crate) fn builtin_call_runtime_name(callee: &str) -> Option<&'static str> {
    match callee {
        "gc_collect" => Some("willow_gc_collect"),
        "gc_minor_collect" => Some("willow_gc_minor_collect"),
        "gc_allocated_bytes" => Some("willow_gc_allocated_bytes"),
        "gc_tlab_fast_allocations" => Some("willow_gc_tlab_fast_allocations"),
        "gc_tlab_slow_allocations" => Some("willow_gc_tlab_slow_allocations"),
        "gc_tlab_refills" => Some("willow_gc_tlab_refills"),
        "gc_tlab_large_allocations" => Some("willow_gc_tlab_large_allocations"),
        "gc_tlab_reserved_bytes" => Some("willow_gc_tlab_reserved_bytes"),
        "gc_minor_collections" => Some("willow_gc_minor_collections"),
        "gc_promoted_objects" => Some("willow_gc_promoted_objects"),
        "gc_moved_objects" => Some("willow_gc_moved_objects"),
        "gc_remembered_set_size" => Some("willow_gc_remembered_set_size"),
        "gc_dirty_card_count" => Some("willow_gc_dirty_card_count"),
        "gc_write_barrier_hits" => Some("willow_gc_write_barrier_hits"),
        "gc_old_region_count" => Some("willow_gc_old_region_count"),
        "gc_old_region_reserved_bytes" => Some("willow_gc_old_region_reserved_bytes"),
        "gc_old_region_live_bytes" => Some("willow_gc_old_region_live_bytes"),
        "gc_old_region_fragmentation_bytes" => Some("willow_gc_old_region_fragmentation_bytes"),
        "gc_large_object_region_count" => Some("willow_gc_large_object_region_count"),
        "gc_pinned_region_count" => Some("willow_gc_pinned_region_count"),
        "gc_old_region_allocations" => Some("willow_gc_old_region_allocations"),
        "gc_old_region_reuses" => Some("willow_gc_old_region_reuses"),
        "gc_old_regions_released" => Some("willow_gc_old_regions_released"),
        "gc_major_collections" => Some("willow_gc_major_collections"),
        "sleep" => Some("willow_runtime_sleep"),
        "yield" => Some("willow_runtime_yield"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every Willow reference — GC handle, string, array, class object,
    /// generic instance and function address — is the SAME 64-bit word. The
    /// runtime declares all of them as `i64`, so a type that disagreed here
    /// would cross the boundary truncated or widened (willow-0g8j ABI audit).
    #[test]
    fn every_reference_type_is_one_64_bit_word() {
        let reference_types = [
            Type::String,
            Type::Array(Box::new(Type::I64)),
            Type::Named("Point".to_string()),
            Type::Generic("Option".to_string(), vec![Type::I64]),
            Type::Fn(vec![Type::I64], Box::new(Type::I64)),
        ];
        for ty in reference_types {
            assert_eq!(
                clif_type(&ty).bits(),
                64,
                "reference type {ty:?} must be a 64-bit word"
            );
        }
    }

    /// The function-address width has exactly one definition. A call through a
    /// function value loads the address with `clif_type`, and the address
    /// itself is produced by `func_addr(FN_ADDR_TYPE, ..)`; if those two ever
    /// disagreed, Cranelift would reject the `call_indirect` — or worse,
    /// accept a truncated address.
    #[test]
    fn a_function_value_has_the_function_address_type() {
        let f = Type::Fn(vec![Type::String], Box::new(Type::Bool));
        assert_eq!(clif_type(&f), FN_ADDR_TYPE);
        assert_eq!(FN_ADDR_TYPE.bits(), 64);
        // A function type's own shape must not change its representation: an
        // address is an address whatever it points at.
        assert_eq!(
            clif_type(&Type::Fn(vec![], Box::new(Type::Void))),
            clif_type(&f)
        );
    }

    /// The scalars are the types that are NOT one word, and they are the
    /// reason the check above cannot simply be "everything is 64 bits".
    #[test]
    fn scalars_keep_their_own_widths() {
        assert_eq!(clif_type(&Type::I64).bits(), 64);
        assert_eq!(clif_type(&Type::F64).bits(), 64);
        assert!(clif_type(&Type::F64).is_float());
        assert_eq!(clif_type(&Type::Bool).bits(), 8);
        assert_eq!(clif_type(&Type::Void).bits(), 8);
    }
}
