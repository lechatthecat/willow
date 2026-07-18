//! Pure type/builtin helper functions for the Cranelift backend (extracted from
//! `mod.rs` to shrink the god file — willow refactor). These map Willow `Type`s
//! to clif types, GC properties, runtime symbol names, and builtin return types;
//! none of them touch codegen state.

use std::collections::HashMap;

use cranelift_codegen::ir::types;

use crate::parser::ast::*;
use crate::semantic::symbols::EnumInfo;

pub(crate) fn clif_type(ty: &Type) -> cranelift_codegen::ir::Type {
    match ty {
        Type::I64 => types::I64,
        Type::F64 => types::F64,
        Type::Bool => types::I8,
        Type::String => types::I64,
        Type::Nil => types::I64,
        Type::Never => types::I64, // bottom type — treated as I64 for codegen purposes
        Type::Array(_) => types::I64,
        // Task<T>/JoinHandle<T> are pointers to async task frames.
        Type::Generic(name, _) if name == "Task" || name == "JoinHandle" => types::I64,
        // Future<T> is an opaque runtime future pointer.
        Type::Generic(name, args) if name == "Future" && args.len() == 1 => types::I64,
        Type::Generic(_, _) => types::I64,
        Type::Nullable(_) => types::I64,
        Type::Fn(_, _) => types::I64, // function pointer (pointer-sized)
        Type::Named(_) => types::I64,
        Type::Void => types::I8,
    }
}

pub(crate) fn join_handle_result_type(ty: &Type) -> Option<Type> {
    match ty {
        // An async fn call returns an eager `Task<T>`, joinable just like a
        // `JoinHandle<T>`: the frame's slot 0 holds the result (willow-h2vf).
        Type::Generic(name, args)
            if (name == "JoinHandle" || name == "Task") && args.len() == 1 =>
        {
            Some(args[0].clone())
        }
        _ => None,
    }
}

pub(crate) fn task_output_type(ty: &Type) -> Option<Type> {
    match ty {
        Type::Generic(name, args) if name == "Task" && args.len() == 1 => Some(args[0].clone()),
        _ => None,
    }
}

pub(crate) fn future_output_type(ty: &Type) -> Option<Type> {
    match ty {
        Type::Generic(name, args) if name == "Future" && args.len() == 1 => Some(args[0].clone()),
        _ => None,
    }
}

pub(crate) fn debug_type_name(ty: &Type) -> String {
    match ty {
        Type::I64 => "i64".to_string(),
        Type::F64 => "f64".to_string(),
        Type::Bool => "bool".to_string(),
        Type::String => "String".to_string(),
        Type::Void => "void".to_string(),
        Type::Nil => "nil".to_string(),
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
        Type::Nullable(inner) => format!("{}?", debug_type_name(inner)),
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
    match ty {
        Type::Generic(name, args) if name == "Channel" && args.len() == 1 => Some(args[0].clone()),
        _ => None,
    }
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
    matches!(name, "Channel" | "Future" | "Mutex" | "RwLock")
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
        Type::Nullable(inner) => is_gc_managed(inner, enum_infos),
        // Opaque runtime-pointer generics (Channel/Future/Mutex/RwLock) are NOT
        // GC heap objects (see `is_opaque_runtime_pointer_type`); every other
        // generic — Task/JoinHandle async frames, Range, Map, user generics — is.
        Type::Generic(name, _) => !is_opaque_runtime_pointer_type(name),
        // String is now a GC-managed WillowString heap object (payload: len + bytes).
        // It is allocated via willow_alloc_typed and has a valid GcHeader.
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
        ("fs", "read_to_string") => Some(Type::Generic(
            "Result".to_string(),
            vec![Type::String, Type::Named("IoError".to_string())],
        )),
        ("fs", "write_string") | ("fs", "remove_file") => Some(Type::Generic(
            "Result".to_string(),
            vec![Type::Void, Type::Named("IoError".to_string())],
        )),
        ("fs", "exists") => Some(Type::Bool),
        ("fs", "temp_path") => Some(Type::String),
        ("env", "args_len") => Some(Type::I64),
        ("env", "arg") => Some(Type::String),
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
        "gc_allocated_bytes" => Some(Type::I64),
        "gc_collect" => Some(Type::Void),
        "sleep" | "yield" => Some(Type::Generic("Future".to_string(), vec![Type::Void])),
        _ => None,
    }
}

pub(crate) fn builtin_call_runtime_name(callee: &str) -> Option<&'static str> {
    match callee {
        "pow" | "powf" => Some("willow_pow_f64"),
        "gc_collect" => Some("willow_gc_collect"),
        "gc_allocated_bytes" => Some("willow_gc_allocated_bytes"),
        "sleep" => Some("willow_runtime_sleep"),
        "yield" => Some("willow_runtime_yield"),
        _ => None,
    }
}
