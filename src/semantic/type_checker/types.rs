//! Pure type helpers for the type checker (extracted from `mod.rs`): type
//! formatting/`type_name`, channel/range/task-handle classification, module
//! qualification, and call return-type derivation. Re-exported from `mod.rs` so
//! existing `crate::semantic::type_checker::*` paths keep working.

use crate::parser::ast::*;
use crate::semantic::builtin_types::{self, BuiltinTypeId as B, TypeName};
use crate::semantic::symbols::*;

/// True for the task-handle generic family produced by spawning/awaiting:
/// `Task<T>` / `Future<T>` / `JoinHandle<T>` (willow-h2vf case A).
pub(crate) fn is_task_handle_type<N: TypeName + Clone>(ty: &Type<N>) -> bool {
    builtin_types::resolve(ty).is_some_and(|resolved| {
        resolved.args.len() == 1 && matches!(resolved.id, B::Task | B::Future | B::JoinHandle)
    })
}

/// The task's own result type `T` behind a panic-on-cancel handle:
/// `Task<T>` (an async call's eager task) or `JoinHandle<T>`. The frame's slot 0
/// holds the result (willow-h2vf).
pub(crate) fn join_handle_result_type<N: TypeName + Clone>(ty: &Type<N>) -> Option<Type<N>> {
    let resolved = builtin_types::resolve(ty)?;
    (resolved.args.len() == 1 && matches!(resolved.id, B::JoinHandle | B::Task))
        .then(|| resolved.args[0].clone())
}

/// `TaskResult<T>` — the cancellation-aware awaitable returned by
/// `Task<T>.result()` (willow-qrj9). Returns the task's own result type `T`;
/// awaiting it produces `Result<T, Cancelled>`.
pub(crate) fn task_result_output_type<N: TypeName + Clone>(ty: &Type<N>) -> Option<Type<N>> {
    builtin_types::unary_arg(ty, B::TaskResult).cloned()
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
pub(crate) fn awaitable_task_type<N: TypeName + Clone>(ty: &Type<N>) -> Option<(Type<N>, bool)> {
    join_handle_result_type(ty)
        .map(|t| (t, false))
        .or_else(|| task_result_output_type(ty).map(|t| (t, true)))
}

/// The result type produced by `await`ing `ty`: `T` for `Task<T>`/`JoinHandle<T>`
/// and `Future<T>`, `Result<T, Cancelled>` for `TaskResult<T>`.
pub(crate) fn await_output_type<N: TypeName + Clone + From<&'static str>>(
    ty: &Type<N>,
) -> Option<Type<N>> {
    if let Some((task_ty, cancel_aware)) = awaitable_task_type(ty) {
        return Some(if cancel_aware {
            B::Result.apply(vec![task_ty, B::Cancelled.apply(vec![])])
        } else {
            task_ty
        });
    }
    builtin_types::unary_arg(ty, B::Future).cloned()
}

pub(crate) fn type_name<N: std::fmt::Display>(ty: &Type<N>) -> String {
    enum Part<'a, N> {
        Type(&'a Type<N>),
        Text(&'static str),
        Name(&'a N),
    }
    let mut output = String::new();
    let mut work = vec![Part::Type(ty)];
    while let Some(part) = work.pop() {
        match part {
            Part::Text(text) => output.push_str(text),
            Part::Name(name) => {
                use std::fmt::Write;
                write!(output, "{name}").expect("write into string");
            }
            Part::Type(ty) => match ty {
                Type::I64 => output.push_str("i64"),
                Type::F64 => output.push_str("f64"),
                Type::Bool => output.push_str("bool"),
                Type::String => output.push_str("String"),
                Type::Void => output.push_str("void"),
                Type::Never => output.push('!'),
                Type::Named(name) => work.push(Part::Name(name)),
                Type::Array(element) => {
                    work.push(Part::Text(">"));
                    work.push(Part::Type(element));
                    work.push(Part::Text("Array<"));
                }
                Type::Generic(name, args) => {
                    work.push(Part::Text(">"));
                    for (index, arg) in args.iter().enumerate().rev() {
                        work.push(Part::Type(arg));
                        if index > 0 {
                            work.push(Part::Text(", "));
                        }
                    }
                    work.push(Part::Text("<"));
                    work.push(Part::Name(name));
                }
                Type::Fn(params, ret) | Type::Closure(params, ret) => {
                    work.push(Part::Type(ret));
                    work.push(Part::Text(") -> "));
                    for (index, param) in params.iter().enumerate().rev() {
                        work.push(Part::Type(param));
                        if index > 0 {
                            work.push(Part::Text(", "));
                        }
                    }
                    work.push(Part::Text(if matches!(ty, Type::Fn(..)) {
                        "fn("
                    } else {
                        "closure("
                    }));
                }
            },
        }
    }
    output
}

pub(crate) fn range_type() -> Type {
    Type::Generic("Range".to_string(), vec![Type::I64])
}

pub(crate) fn is_i64_range_type<N: TypeName>(ty: &Type<N>) -> bool {
    matches!(ty, Type::Generic(name, args) if name.builtin_name() == "Range" && matches!(args.as_slice(), [Type::I64]))
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

pub(crate) fn channel_element_type<N: TypeName + Clone>(ty: &Type<N>) -> Option<Type<N>> {
    builtin_types::unary_arg(ty, B::Channel).cloned()
}

/// A `Channel` construction that names no element type: `Channel::new()` or
/// `Channel::with_capacity(n)`.
///
/// Both are typed `Channel<void>` from the call alone, and both are then
/// entitled to take the element from a `let` annotation. They are one predicate
/// because they carry the same hazard: the element type is what decides whether
/// the runtime traces the channel's buffer, so a construction left as
/// `Channel<void>` allocates an untraced buffer regardless of which of the two
/// built it.
pub(crate) fn is_untyped_channel_ctor_call(expr: &Expr) -> bool {
    let Expr::StaticCall(call) = expr else {
        return false;
    };
    if call.class != "Channel" || !call.type_args.is_empty() {
        return false;
    }
    match call.method.as_str() {
        "new" => call.args.is_empty(),
        // The one argument is the capacity, not the element.
        "with_capacity" => call.args.len() == 1,
        _ => false,
    }
}

#[willow_continuations::function(qualify_type_for_module)]
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
        Type::Fn(params, ret) => Type::Fn(
            params
                .iter()
                .map(|param| qualify_type_for_module(param, module_prefix))
                .collect(),
            Box::new(qualify_type_for_module(ret, module_prefix)),
        ),
        Type::Closure(params, ret) => Type::Closure(
            params
                .iter()
                .map(|param| qualify_type_for_module(param, module_prefix))
                .collect(),
            Box::new(qualify_type_for_module(ret, module_prefix)),
        ),
        Type::I64 | Type::F64 | Type::Bool | Type::String | Type::Void | Type::Never => ty.clone(),
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
    if !m.is_static {
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
