//! Pure type/builtin helper functions for the Cranelift backend (extracted from
//! `mod.rs` to shrink the god file — willow refactor). These map Willow `Type`s
//! to clif types, GC properties, runtime symbol names, and builtin return types;
//! none of them touch codegen state.

use super::type_index::TypeMap;

use cranelift_codegen::ir::types;

use super::EnumInfo;
use crate::semantic::builtin_types;
#[cfg(test)]
use crate::semantic::builtin_types::BuiltinTypeId as B;
use crate::semantic::ids::SemanticType as Type;

/// Reference and function-address width comes from the selected target.
/// The 64-bit target guard remains until the runtime and layouts are migrated
/// together (willow-d9lm.4 / willow-d9lm.5).
pub(crate) fn reference_type(
    config: cranelift_codegen::isa::TargetFrontendConfig,
) -> cranelift_codegen::ir::Type {
    config.pointer_type()
}

pub(crate) fn clif_type<N: builtin_types::TypeName>(
    reference_type: cranelift_codegen::ir::Type,
    ty: &crate::parser::ast::Type<N>,
) -> cranelift_codegen::ir::Type {
    match ty {
        crate::parser::ast::Type::I64 => types::I64,
        crate::parser::ast::Type::F64 => types::F64,
        crate::parser::ast::Type::Bool | crate::parser::ast::Type::Void => types::I8,
        // Never uses the reference representation for unreachable values.
        crate::parser::ast::Type::String
        | crate::parser::ast::Type::Never
        | crate::parser::ast::Type::Array(_)
        | crate::parser::ast::Type::Generic(_, _)
        | crate::parser::ast::Type::Fn(_, _)
        | crate::parser::ast::Type::Closure(_, _)
        | crate::parser::ast::Type::Named(_) => reference_type,
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

#[cfg(test)]
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
pub(crate) use crate::semantic::intrinsics::{
    builtin_call_runtime_name, gc_stat_builtin_runtime_name,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_types_follow_target_width_and_scalars_keep_their_widths() {
        let references = [
            Type::String,
            Type::Never,
            Type::Array(Box::new(Type::I64)),
            Type::Named("Point".to_string().into()),
            Type::Generic("Option".to_string().into(), vec![Type::I64]),
            Type::Generic("Task".to_string().into(), vec![Type::I64]),
            Type::Generic("JoinHandle".to_string().into(), vec![Type::I64]),
            Type::Generic("TaskResult".to_string().into(), vec![Type::I64]),
            Type::Generic("Future".to_string().into(), vec![Type::Void]),
            Type::Fn(vec![Type::I64], Box::new(Type::I64)),
            Type::Closure(vec![Type::String], Box::new(Type::Bool)),
        ];
        for pointer in [types::I32, types::I64] {
            for ty in &references {
                assert_eq!(clif_type(pointer, ty), pointer, "{ty:?}");
            }
            for (ty, expected) in [
                (Type::I64, types::I64),
                (Type::F64, types::F64),
                (Type::Bool, types::I8),
                (Type::Void, types::I8),
            ] {
                assert_eq!(clif_type(pointer, &ty), expected);
            }
        }
    }
}
