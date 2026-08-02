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

/// The task's own result type `T` behind a panic-on-cancel handle:
/// `Task<T>` (an async call's eager task) or `JoinHandle<T>`. The frame's slot 0
/// holds the result (willow-h2vf).
pub(crate) fn join_handle_result_type(ty: &Type) -> Option<Type> {
    match ty {
        Type::Generic(name, args)
            if (name == "JoinHandle" || name == "Task") && args.len() == 1 =>
        {
            Some(args[0].clone())
        }
        _ => None,
    }
}

/// `TaskResult<T>` — the cancellation-aware awaitable returned by
/// `Task<T>.result()` (willow-qrj9). Returns the task's own result type `T`;
/// awaiting it produces `Result<T, Cancelled>`.
pub(crate) fn task_result_output_type(ty: &Type) -> Option<Type> {
    match ty {
        Type::Generic(name, args) if name == "TaskResult" && args.len() == 1 => {
            Some(args[0].clone())
        }
        _ => None,
    }
}

/// The task's own result type `T` behind any awaitable task handle, plus whether
/// awaiting it is cancellation-aware (`Result<T, Cancelled>`) rather than
/// panic-on-cancel (`T`).
///
/// Cancellation-awareness is a property of the awaited TYPE, not of how the
/// await was spelled: `await t.result()` and `let v = t.result(); await v` must
/// behave identically (willow-qrj9). This one definition is shared by the type
/// checker and the Cranelift backend so the two can never disagree about which
/// handles are awaitable.
pub(crate) fn awaitable_task_type(ty: &Type) -> Option<(Type, bool)> {
    join_handle_result_type(ty)
        .map(|t| (t, false))
        .or_else(|| task_result_output_type(ty).map(|t| (t, true)))
}

/// The result type produced by `await`ing `ty`: `T` for `Task<T>`/`JoinHandle<T>`
/// and `Future<T>`, `Result<T, Cancelled>` for `TaskResult<T>`.
pub(crate) fn await_output_type(ty: &Type) -> Option<Type> {
    if let Some((task_ty, cancel_aware)) = awaitable_task_type(ty) {
        return Some(if cancel_aware {
            Type::Generic(
                "Result".to_string(),
                vec![task_ty, Type::Named("Cancelled".to_string())],
            )
        } else {
            task_ty
        });
    }
    match ty {
        Type::Generic(name, args) if name == "Future" && args.len() == 1 => Some(args[0].clone()),
        _ => None,
    }
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
/// Whether two parameter modes are the same passing convention. `ParamMode`
/// carries spans, so it cannot be compared with `==` across declarations.
pub(crate) fn param_modes_match(a: &ParamMode, b: &ParamMode) -> bool {
    match (a, b) {
        (ParamMode::Value, ParamMode::Value) => true,
        (
            ParamMode::Reference { mutable: a_mut, .. },
            ParamMode::Reference { mutable: b_mut, .. },
        ) => a_mut == b_mut,
        _ => false,
    }
}

/// Render a parameter the way it is written in source, including its `&`/`&mut`
/// mode. The mode is part of the signature a class must match, because a
/// reference parameter is passed as a pointer (willow-0g8j.9).
pub(crate) fn param_info_name(p: &ParamInfo) -> String {
    match &p.mode {
        ParamMode::Reference { mutable: true, .. } => format!("&mut {}", type_name(&p.ty)),
        ParamMode::Reference { mutable: false, .. } => format!("& {}", type_name(&p.ty)),
        ParamMode::Value => type_name(&p.ty),
    }
}

pub(crate) fn interface_method_signature(m: &InterfaceMethodInfo) -> String {
    let mut parts: Vec<String> = Vec::new();
    if m.has_self {
        parts.push("self".to_string());
    }
    parts.extend(m.param_infos.iter().map(param_info_name));
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
