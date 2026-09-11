//! Pure type/builtin helper functions for the Cranelift backend (extracted from
//! `mod.rs` to shrink the god file — willow refactor). These map Willow `Type`s
//! to clif types, GC properties, runtime symbol names, and builtin return types;
//! none of them touch codegen state.

use super::type_index::TypeMap;

use cranelift_codegen::ir::types;

use super::EnumInfo;
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::ids::SemanticType as Type;

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
/// matter of editing this line; it is tracked as `willow-d9lm`.
pub(crate) const FN_ADDR_TYPE: cranelift_codegen::ir::Type = types::I64;

pub(crate) fn clif_type<N: builtin_types::TypeName>(
    ty: &crate::parser::ast::Type<N>,
) -> cranelift_codegen::ir::Type {
    match ty {
        crate::parser::ast::Type::I64 => types::I64,
        crate::parser::ast::Type::F64 => types::F64,
        crate::parser::ast::Type::Bool => types::I8,
        crate::parser::ast::Type::String => types::I64,
        crate::parser::ast::Type::Never => types::I64, // bottom type — treated as I64 for codegen purposes
        crate::parser::ast::Type::Array(_) => types::I64,
        // Task<T>/JoinHandle<T> are pointers to async task frames.
        // `TaskResult<T>` is the SAME pointer viewed cancellation-awarely
        // (willow-qrj9): `result()` is an identity adapter, so it must never
        // gain a distinct representation.
        crate::parser::ast::Type::Generic(_, _)
            if builtin_types::resolve(ty).is_some_and(|resolved| {
                matches!(resolved.id, B::Task | B::JoinHandle | B::TaskResult)
            }) =>
        {
            types::I64
        }
        // Future<T> is an opaque runtime future pointer.
        crate::parser::ast::Type::Generic(_, _)
            if builtin_types::unary_arg(ty, B::Future).is_some() =>
        {
            types::I64
        }
        crate::parser::ast::Type::Generic(_, _) => types::I64,
        // A function address, a fixed 64-bit word — see [`FN_ADDR_TYPE`].
        crate::parser::ast::Type::Fn(_, _) => FN_ADDR_TYPE,
        // A closure VALUE is the environment object, so it is a GC pointer and
        // not a code address; the code pointer lives in its word 0
        // (willow-0g8j.2.12).
        crate::parser::ast::Type::Closure(_, _) => types::I64,
        crate::parser::ast::Type::Named(_) => types::I64,
        crate::parser::ast::Type::Void => types::I8,
    }
}

pub(crate) fn debug_type_name<N: std::fmt::Display>(ty: &crate::parser::ast::Type<N>) -> String {
    match ty {
        crate::parser::ast::Type::I64 => "i64".to_string(),
        crate::parser::ast::Type::F64 => "f64".to_string(),
        crate::parser::ast::Type::Bool => "bool".to_string(),
        crate::parser::ast::Type::String => "String".to_string(),
        crate::parser::ast::Type::Void => "void".to_string(),
        crate::parser::ast::Type::Never => "!".to_string(),
        crate::parser::ast::Type::Named(name) => name.to_string(),
        crate::parser::ast::Type::Array(element) => format!("Array<{}>", debug_type_name(element)),
        crate::parser::ast::Type::Generic(name, args) => {
            let args = args
                .iter()
                .map(debug_type_name)
                .collect::<Vec<_>>()
                .join(",");
            format!("{name}<{args}>")
        }
        crate::parser::ast::Type::Fn(params, ret) => {
            let param_str = params
                .iter()
                .map(debug_type_name)
                .collect::<Vec<_>>()
                .join(",");
            format!("fn({}) -> {}", param_str, debug_type_name(ret))
        }
        crate::parser::ast::Type::Closure(params, ret) => {
            let param_str = params
                .iter()
                .map(debug_type_name)
                .collect::<Vec<_>>()
                .join(",");
            format!("closure({}) -> {}", param_str, debug_type_name(ret))
        }
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
/// (see `emit_lir_enum_variant`).  Treating such a value as GC-managed would root or
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

pub(crate) fn is_gc_managed(ty: &Type, enum_infos: &TypeMap<EnumInfo>) -> bool {
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
        Type::Generic(name, _) => !is_opaque_runtime_pointer_type(name.name()),
        // String is now a GC-managed WillowString heap object (payload: len + bytes).
        // It is allocated through the central GC path and has a valid GcHeader.
        Type::String => true,
        // A closure value is the environment OBJECT — allocated through the
        // same central GC path, with a valid GcHeader and a ref mask covering
        // its captured words (willow-0g8j.2.12). `Type::Fn` stays out: that is
        // a bare code address in a text section, not a heap object.
        Type::Closure(_, _) => true,
        _ => false,
    }
}

// Runtime identity belongs to semantic lowering; backend consumers share it.
pub(crate) use crate::semantic::intrinsics::{builtin_call_runtime_name, gc_stat_builtin_runtime_name};

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
            Type::Named("Point".to_string().into()),
            Type::Generic("Option".to_string().into(), vec![Type::I64]),
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
