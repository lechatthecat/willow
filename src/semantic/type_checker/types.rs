//! Pure type helpers for the type checker (extracted from `mod.rs`): type
//! formatting/`type_name`, channel/range/task-handle classification, module
//! qualification, and call return-type derivation. Re-exported from `mod.rs` so
//! existing `crate::semantic::type_checker::*` paths keep working.

use crate::parser::ast::*;
use crate::semantic::symbols::*;

/// True for the task-handle generic family produced by spawning/awaiting:
/// `Task<T>` / `Future<T>` / `JoinHandle<T>` (willow-h2vf case A).
pub(crate) fn is_task_handle_type(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Generic(name, args)
            if (name == "Task" || name == "Future" || name == "JoinHandle") && args.len() == 1
    )
}

pub(crate) fn type_name(ty: &Type) -> String {
    match ty {
        Type::I64 => "i64".to_string(),
        Type::F64 => "f64".to_string(),
        Type::Bool => "bool".to_string(),
        Type::String => "String".to_string(),
        Type::Void => "void".to_string(),
        Type::Nil => "nil".to_string(),
        Type::Never => "!".to_string(),
        Type::Named(n) => n.clone(),
        Type::Array(element) => format!("Array<{}>", type_name(element)),
        Type::Generic(name, args) => {
            let args = args.iter().map(type_name).collect::<Vec<_>>().join(", ");
            format!("{name}<{args}>")
        }
        Type::Nullable(inner) => format!("{}?", type_name(inner)),
        Type::Fn(params, ret) => {
            let param_str = params.iter().map(type_name).collect::<Vec<_>>().join(", ");
            format!("fn({}) -> {}", param_str, type_name(ret))
        }
    }
}

pub(crate) fn range_type() -> Type {
    Type::Generic("Range".to_string(), vec![Type::I64])
}

pub(crate) fn is_i64_range_type(ty: &Type) -> bool {
    matches!(ty, Type::Generic(name, args) if name == "Range" && args.as_slice() == [Type::I64])
}

pub(crate) fn function_call_return_type(info: &FuncInfo) -> Type {
    if info.is_async {
        Type::Generic("Task".to_string(), vec![info.return_type.clone()])
    } else {
        info.return_type.clone()
    }
}

pub(crate) fn method_call_return_type(info: &MethodInfo) -> Type {
    if info.is_async {
        Type::Generic("Task".to_string(), vec![info.return_type.clone()])
    } else {
        info.return_type.clone()
    }
}

pub(crate) fn channel_element_type(ty: &Type) -> Option<Type> {
    match ty {
        Type::Generic(name, args) if name == "Channel" && args.len() == 1 => Some(args[0].clone()),
        _ => None,
    }
}

pub(crate) fn is_untyped_channel_new_call(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::StaticCall(call)
            if call.class == "Channel"
                && call.type_args.is_empty()
                && call.method == "new"
                && call.args.is_empty()
    )
}

pub(crate) fn nullable_inner_has_pointer_representation(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Named(_) | Type::String | Type::Array(_) | Type::Generic(_, _) | Type::Fn(_, _)
    )
}

pub(crate) fn qualify_type_for_module(ty: &Type, module_prefix: Option<&str>) -> Type {
    match ty {
        Type::Named(name) => module_prefix
            .filter(|_| !name.contains("::"))
            .map(|module| Type::Named(format!("{module}::{name}")))
            .unwrap_or_else(|| ty.clone()),
        Type::Array(element) => {
            Type::Array(Box::new(qualify_type_for_module(element, module_prefix)))
        }
        Type::Generic(name, args) => Type::Generic(
            module_prefix
                .filter(|_| !name.contains("::"))
                .map(|module| format!("{module}::{name}"))
                .unwrap_or_else(|| name.clone()),
            args.iter()
                .map(|arg| qualify_type_for_module(arg, module_prefix))
                .collect(),
        ),
        Type::Nullable(inner) => {
            Type::Nullable(Box::new(qualify_type_for_module(inner, module_prefix)))
        }
        Type::Fn(params, ret) => Type::Fn(
            params
                .iter()
                .map(|param| qualify_type_for_module(param, module_prefix))
                .collect(),
            Box::new(qualify_type_for_module(ret, module_prefix)),
        ),
        Type::I64
        | Type::F64
        | Type::Bool
        | Type::String
        | Type::Void
        | Type::Nil
        | Type::Never => ty.clone(),
    }
}

pub(crate) fn type_path_name(path: &TypePath) -> String {
    qualified_type_path_name(path, None)
}

/// Render a required interface method as `name(self, T, U) -> R` for diagnostics.
pub(crate) fn interface_method_signature(m: &InterfaceMethodInfo) -> String {
    let mut parts: Vec<String> = Vec::new();
    if m.has_self {
        parts.push("self".to_string());
    }
    parts.extend(m.params.iter().map(type_name));
    let ret = if matches!(m.return_type, Type::Void) {
        String::new()
    } else {
        format!(" -> {}", type_name(&m.return_type))
    };
    format!("{}({}){}", m.name, parts.join(", "), ret)
}

pub(crate) fn qualified_type_path_name(path: &TypePath, module_prefix: Option<&str>) -> String {
    match path {
        TypePath::Local(name) => module_prefix
            .map(|module| format!("{module}::{name}"))
            .unwrap_or_else(|| name.clone()),
        TypePath::Qualified(parts) => parts.join("::"),
    }
}
