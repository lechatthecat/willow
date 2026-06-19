//! Symbol-naming and module-qualification helpers for the Cranelift backend
//! (extracted from `mod.rs`). Map Willow names to mangled backend symbols and
//! qualify module-local types/classes; constructor->method desugaring lives here
//! too as a naming/shape transform.

use std::collections::HashMap;

use crate::parser::ast::*;

use super::USER_MAIN_SYMBOL;

pub(crate) fn module_symbol_prefix(module_path: &str) -> String {
    module_path.split("::").collect::<Vec<_>>().join("__")
}

/// Make a `::`-qualified name safe for use inside a linker symbol.
pub(crate) fn backend_symbol_component(name: &str) -> String {
    name.replace("::", "__")
}

/// Synthesize the `init` method that a constructor lowers to (willow-scq2): a
/// non-static instance method with a hidden `self` receiver and void return.
pub(crate) fn constructor_to_method(ctor: &ConstructorDecl) -> MethodDecl {
    MethodDecl {
        name: "init".to_string(),
        public: ctor.public,
        protected: ctor.protected,
        is_async: false,
        is_open: false,
        is_override: false,
        is_static: false,
        params: ctor.params.clone(),
        has_self: true,
        return_type: Type::Void,
        body: ctor.body.clone(),
        span: ctor.span,
        is_default_injected: false,
    }
}

pub(crate) fn class_method_symbol_name(
    known_modules: &HashMap<String, String>,
    class_name: &str,
    method_name: &str,
) -> String {
    let module_match = known_modules
        .iter()
        .filter_map(|(access_name, symbol_prefix)| {
            class_name
                .strip_prefix(access_name)
                .and_then(|rest| rest.strip_prefix("::"))
                .map(|suffix| (access_name.len(), symbol_prefix, suffix))
        })
        .max_by_key(|(len, _, _)| *len);

    if let Some((_, symbol_prefix, class_suffix)) = module_match {
        let class_suffix = module_symbol_prefix(class_suffix);
        format!("{symbol_prefix}__{class_suffix}__{method_name}")
    } else {
        format!("{class_name}__{method_name}")
    }
}

pub(crate) fn qualify_module_class_decl(class: &ClassDecl, module_name: &str) -> ClassDecl {
    let mut qualified = class.clone();
    qualified.name = format!("{module_name}::{}", class.name);
    qualified.implements = class
        .implements
        .iter()
        .map(|iface| qualify_module_type(iface, module_name))
        .collect();
    qualified.fields = class
        .fields
        .iter()
        .map(|field| {
            let mut field = field.clone();
            field.ty = qualify_module_type(&field.ty, module_name);
            field
        })
        .collect();
    qualified.methods = class
        .methods
        .iter()
        .map(|method| {
            let mut method = method.clone();
            method.params = method
                .params
                .iter()
                .map(|param| {
                    let mut param = param.clone();
                    param.ty = qualify_module_type(&param.ty, module_name);
                    param
                })
                .collect();
            method.return_type = qualify_module_type(&method.return_type, module_name);
            method
        })
        .collect();
    qualified.constructors = class
        .constructors
        .iter()
        .map(|ctor| {
            let mut ctor = ctor.clone();
            ctor.params = ctor
                .params
                .iter()
                .map(|param| {
                    let mut param = param.clone();
                    param.ty = qualify_module_type(&param.ty, module_name);
                    param
                })
                .collect();
            ctor
        })
        .collect();
    qualified
}

pub(crate) fn qualify_module_type(ty: &Type, module_name: &str) -> Type {
    match ty {
        Type::Named(name) if !name.contains("::") => Type::Named(format!("{module_name}::{name}")),
        Type::Array(element) => Type::Array(Box::new(qualify_module_type(element, module_name))),
        Type::Generic(name, args) => Type::Generic(
            name.clone(),
            args.iter()
                .map(|arg| qualify_module_type(arg, module_name))
                .collect(),
        ),
        Type::Nullable(inner) => Type::Nullable(Box::new(qualify_module_type(inner, module_name))),
        Type::Fn(params, ret) => Type::Fn(
            params
                .iter()
                .map(|param| qualify_module_type(param, module_name))
                .collect(),
            Box::new(qualify_module_type(ret, module_name)),
        ),
        _ => ty.clone(),
    }
}

/// Qualify a type's module-LOCAL declared type names (in `local`) to
/// `module::Type`, including a GENERIC head name (`Box<i64>` ->
/// `module::Box<i64>`), while leaving builtin generics (Array/Map/Result/...)
/// untouched. Used to qualify a module function's signature so the importing
/// file boxes interface arguments against the right (class, interface) vtable
/// (willow-1js.5).
pub(crate) fn qualify_module_local_type(
    ty: &Type,
    module_name: &str,
    local: &std::collections::HashSet<String>,
) -> Type {
    let qual = |n: &str| -> String {
        if !n.contains("::") && local.contains(n) {
            format!("{module_name}::{n}")
        } else {
            n.to_string()
        }
    };
    match ty {
        Type::Named(name) => Type::Named(qual(name)),
        Type::Generic(name, args) => Type::Generic(
            qual(name),
            args.iter()
                .map(|a| qualify_module_local_type(a, module_name, local))
                .collect(),
        ),
        Type::Array(e) => Type::Array(Box::new(qualify_module_local_type(e, module_name, local))),
        Type::Nullable(i) => {
            Type::Nullable(Box::new(qualify_module_local_type(i, module_name, local)))
        }
        Type::Fn(ps, r) => Type::Fn(
            ps.iter()
                .map(|p| qualify_module_local_type(p, module_name, local))
                .collect(),
            Box::new(qualify_module_local_type(r, module_name, local)),
        ),
        _ => ty.clone(),
    }
}

/// Clone a module function declaration with its SIGNATURE (parameter and return
/// types) qualified to module-local names. The body is left untouched (it is
/// compiled under the module's local-name aliases) (willow-1js.5).
pub(crate) fn qualify_module_fn_signature(
    f: &FunctionDecl,
    module_name: &str,
    local: &std::collections::HashSet<String>,
) -> FunctionDecl {
    let mut out = f.clone();
    for p in &mut out.params {
        p.ty = qualify_module_local_type(&p.ty, module_name, local);
    }
    out.return_type = qualify_module_local_type(&out.return_type, module_name, local);
    out
}

pub(crate) fn class_name_for_object_type(ty: &Type) -> Option<String> {
    match ty {
        Type::Named(name) => Some(name.clone()),
        Type::Nullable(inner) => class_name_for_object_type(inner),
        _ => None,
    }
}

pub(crate) fn user_function_symbol(name: &str) -> String {
    if name == "main" {
        USER_MAIN_SYMBOL.to_string()
    } else {
        name.to_string()
    }
}
