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
/// hold are kept alive by the runtime instead. All other generics
/// (`Task`/`JoinHandle` async frames, `Range`, `Map`, user generics) are real
/// GC heap objects.
pub(crate) fn is_gc_managed(ty: &Type, enum_infos: &TypeMap<EnumInfo>) -> bool {
    crate::compiler_db::layout::is_gc_managed(ty, |name| {
        // Fieldless enums are immediate tags; payload enums and classes are heap objects.
        enum_infos.get(&name).is_none_or(|info| {
            info.variants
                .iter()
                .any(|variant| !variant.payload_types.is_empty())
        })
    })
}

// Runtime identity belongs to semantic lowering; backend consumers share it.
pub(crate) use crate::semantic::intrinsics::{
    builtin_call_runtime_name, runtime_stat_builtin_runtime_name,
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
