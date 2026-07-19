//! Symbol registration, name/import resolution, and interface instantiation/
//! conformance methods for the type checker (extracted from `mod.rs`). The
//! `register_*` entry points stay `pub`; the rest are `pub(super)`.

use std::collections::{HashMap, HashSet};

use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use crate::semantic::symbols::*;

use super::*;

/// Qualify a type's module-LOCAL declared type names to `module::Type`, leaving
/// builtin generics (Array/Map/Result/Option/Channel/Future/...) and primitives
/// untouched. Used so a module function signature that references one of its own
/// types resolves in the importing file (willow-1js.5).
pub(crate) fn qualify_local_type(
    ty: &Type,
    module: &str,
    local: &std::collections::HashSet<String>,
) -> Type {
    let qualify_name = |n: &str| -> String {
        if !n.contains("::") && local.contains(n) {
            format!("{module}::{n}")
        } else {
            n.to_string()
        }
    };
    match ty {
        Type::Named(n) => Type::Named(qualify_name(n)),
        Type::Generic(n, args) => Type::Generic(
            qualify_name(n),
            args.iter()
                .map(|a| qualify_local_type(a, module, local))
                .collect(),
        ),
        Type::Array(e) => Type::Array(Box::new(qualify_local_type(e, module, local))),
        Type::Nullable(i) => Type::Nullable(Box::new(qualify_local_type(i, module, local))),
        Type::Fn(ps, r) => Type::Fn(
            ps.iter()
                .map(|p| qualify_local_type(p, module, local))
                .collect(),
            Box::new(qualify_local_type(r, module, local)),
        ),
        _ => ty.clone(),
    }
}

fn std_schema_type(ty: crate::stdlib_schema::StdType) -> Type {
    use crate::stdlib_schema::StdType;

    match ty {
        StdType::I64 => Type::I64,
        StdType::Bool => Type::Bool,
        StdType::String => Type::String,
        StdType::StringArray => Type::Array(Box::new(Type::String)),
        StdType::Void => Type::Void,
        StdType::StringIoResult => Type::Generic(
            "Result".to_string(),
            vec![Type::String, Type::Named("IoError".to_string())],
        ),
        StdType::VoidIoResult => Type::Generic(
            "Result".to_string(),
            vec![Type::Void, Type::Named("IoError".to_string())],
        ),
        StdType::TaskStringIoResult => Type::Generic(
            "Task".to_string(),
            vec![Type::Generic(
                "Result".to_string(),
                vec![Type::String, Type::Named("IoError".to_string())],
            )],
        ),
        StdType::TaskVoidIoResult => Type::Generic(
            "Task".to_string(),
            vec![Type::Generic(
                "Result".to_string(),
                vec![Type::Void, Type::Named("IoError".to_string())],
            )],
        ),
        StdType::TaskBool => Type::Generic("Task".to_string(), vec![Type::Bool]),
        StdType::Printable => {
            unreachable!("polymorphic printable types are handled by std::io resolution")
        }
    }
}

impl TypeChecker {
    pub(super) fn register_builtin_functions(&mut self) {
        for name in ["pow", "powf"] {
            let params = vec![Type::F64, Type::F64];
            self.symbols.define_func(
                name.to_string(),
                FuncInfo {
                    param_infos: value_param_infos(&params),
                    params,
                    return_type: Type::F64,
                    public: true,
                    is_async: false,
                    declaration_span: Span::dummy(),
                    module_path: None,
                },
            );
        }
        self.symbols.define_func(
            "gc_collect".to_string(),
            FuncInfo {
                param_infos: vec![],
                params: vec![],
                return_type: Type::Void,
                public: true,
                is_async: false,
                declaration_span: Span::dummy(),
                module_path: None,
            },
        );
        self.symbols.define_func(
            "gc_minor_collect".to_string(),
            FuncInfo {
                param_infos: vec![],
                params: vec![],
                return_type: Type::Void,
                public: true,
                is_async: false,
                declaration_span: Span::dummy(),
                module_path: None,
            },
        );
        for name in [
            "gc_allocated_bytes",
            "gc_tlab_fast_allocations",
            "gc_tlab_slow_allocations",
            "gc_tlab_refills",
            "gc_tlab_large_allocations",
            "gc_tlab_reserved_bytes",
            "gc_minor_collections",
            "gc_promoted_objects",
            "gc_moved_objects",
            "gc_remembered_set_size",
            "gc_dirty_card_count",
            "gc_write_barrier_hits",
            "gc_old_region_count",
            "gc_old_region_reserved_bytes",
            "gc_old_region_live_bytes",
            "gc_old_region_fragmentation_bytes",
            "gc_large_object_region_count",
            "gc_pinned_region_count",
            "gc_old_region_allocations",
            "gc_old_region_reuses",
            "gc_old_regions_released",
            "gc_major_collections",
        ] {
            self.symbols.define_func(
                name.to_string(),
                FuncInfo {
                    param_infos: vec![],
                    params: vec![],
                    return_type: Type::I64,
                    public: true,
                    is_async: false,
                    declaration_span: Span::dummy(),
                    module_path: None,
                },
            );
        }
        // panic(message: String) — noreturn; type-checker returns Never
        let panic_params = vec![Type::String];
        self.symbols.define_func(
            "panic".to_string(),
            FuncInfo {
                param_infos: value_param_infos(&panic_params),
                params: panic_params,
                return_type: Type::Never,
                public: true,
                is_async: false,
                declaration_span: Span::dummy(),
                module_path: None,
            },
        );
        let sleep_params = vec![Type::I64];
        self.symbols.define_func(
            "sleep".to_string(),
            FuncInfo {
                param_infos: value_param_infos(&sleep_params),
                params: sleep_params,
                return_type: Type::Generic("Future".to_string(), vec![Type::Void]),
                public: true,
                is_async: false,
                declaration_span: Span::dummy(),
                module_path: None,
            },
        );
        self.symbols.define_func(
            "yield".to_string(),
            FuncInfo {
                param_infos: vec![],
                params: vec![],
                return_type: Type::Generic("Future".to_string(), vec![Type::Void]),
                public: true,
                is_async: false,
                declaration_span: Span::dummy(),
                module_path: None,
            },
        );
    }

    pub(super) fn register_builtin_modules(&mut self) {
        for module_name in ["env", "fs"] {
            self.register_schema_module(module_name);
        }
    }

    fn register_schema_module(&mut self, module_name: &str) {
        self.register_schema_module_as(module_name, module_name);
    }

    fn register_schema_module_as(&mut self, module_name: &str, local_name: &str) {
        let functions = crate::stdlib_schema::module(module_name)
            .expect("stdlib schema must define the builtin module")
            .items
            .iter()
            .filter_map(|item| {
                let crate::stdlib_schema::StdItemKind::Function {
                    params,
                    return_type,
                } = item.kind
                else {
                    return None;
                };
                let params = params
                    .iter()
                    .copied()
                    .map(std_schema_type)
                    .collect::<Vec<_>>();
                Some((
                    item.name.to_string(),
                    FuncInfo {
                        param_infos: value_param_infos(&params),
                        params,
                        return_type: std_schema_type(return_type),
                        public: true,
                        is_async: false,
                        declaration_span: Span::dummy(),
                        module_path: None,
                    },
                ))
            })
            .collect();
        self.symbols
            .define_module(local_name.to_string(), ModuleInfo { functions });
    }

    /// Register an imported module's items so cross-module calls can report
    /// missing and private-item diagnostics accurately.
    /// Register a prelude enum so it is available in all user programs.
    pub fn register_prelude_enum(&mut self, decl: &crate::parser::ast::EnumDecl) {
        self.register_enum(decl);
    }

    pub fn register_prelude_interface(&mut self, decl: &crate::parser::ast::InterfaceDecl) {
        self.register_interface(decl, None);
    }

    pub fn register_module(&mut self, name: &str, path: &str, program: &Program) {
        self.register_module_impl(None, name, path, program);
    }

    pub fn register_module_with_id(
        &mut self,
        id: crate::module::ModuleId,
        name: &str,
        path: &str,
        program: &Program,
    ) {
        self.register_module_impl(Some(id), name, path, program);
    }

    fn register_module_impl(
        &mut self,
        id: Option<crate::module::ModuleId>,
        name: &str,
        path: &str,
        program: &Program,
    ) {
        // INTERFACE names declared in this module. A module function signature
        // that names one of its own interfaces by bare name is qualified to
        // `module::Iface` so the importing file resolves it AND boxes interface
        // arguments against the right vtable. Only interfaces are qualified:
        // enum/class params are passed by value and a directly-imported alias
        // (`import mod::Color` -> bare `Color`) must keep matching the bare arg
        // type; builtin generics are left untouched (willow-1js.5).
        let local_types: HashSet<String> = program
            .items
            .iter()
            .filter_map(|it| match it {
                Item::Interface(i) => Some(i.name.clone()),
                _ => None,
            })
            .collect();

        let mut functions = crate::semantic::ids::FunctionMap::default();
        for item in &program.items {
            match item {
                Item::Function(f) => {
                    let params = f
                        .params
                        .iter()
                        .map(|p| qualify_local_type(&p.ty, name, &local_types))
                        .collect::<Vec<_>>();
                    let mut param_infos = param_infos_from_decl(&f.params, None);
                    for pi in &mut param_infos {
                        pi.ty = qualify_local_type(&pi.ty, name, &local_types);
                    }
                    functions.insert(
                        f.name.clone(),
                        FuncInfo {
                            param_infos,
                            params,
                            return_type: qualify_local_type(&f.return_type, name, &local_types),
                            public: f.public,
                            is_async: f.is_async,
                            declaration_span: f.span,
                            module_path: Some(path.to_string()),
                        },
                    );
                }
                Item::Class(c) => {
                    let class_name = format!("{name}::{}", c.name);
                    self.symbols.define_class(
                        class_name.clone(),
                        class_info_from_decl(c, &class_name, Some(name)),
                    );
                }
                Item::Enum(e) => self.register_enum_with_module(e, name),
                Item::Interface(i) => {
                    // Register imported interfaces under `module::Interface` so
                    // `animals::Animal` resolves as a type and in `implements`.
                    self.register_interface(i, Some(name));
                }
            }
        }
        let info = ModuleInfo { functions };
        if let Some(id) = id {
            self.symbols
                .define_module_with_id(name.to_string(), id, info);
        } else {
            self.symbols.define_module(name.to_string(), info);
        }
        self.imported_names.insert(name.to_string(), None);
    }

    /// Bind a single-item import (`import math::add;`) into the current scope:
    /// `local` resolves to the public function `item` of module `module`.
    pub fn register_item_import(&mut self, local: &str, module: &str, item: &str, span: Span) {
        self.imported_names.insert(local.to_string(), Some(span));

        // Functions are registered per-module in the ModuleInfo table.
        let func = self
            .symbols
            .lookup_module(module)
            .and_then(|m| m.functions.get(item).cloned());
        if let Some(info) = func {
            if info.public {
                self.symbols.define_func(local.to_string(), info);
            } else {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E2006,
                        format!("item `{item}` is private in module `{module}`"),
                    )
                    .with_label(Label::primary(span, "private item"))
                    .with_help(format!("mark `{item}` as `pub` in module `{module}`")),
                );
            }
            return;
        }

        // Types (class / interface / enum) are registered under `module::Item`.
        // Bind them under the unqualified local name too (willow-64gs), with a
        // visibility check (E0419 for a private type).
        let qualified = format!("{module}::{item}");
        let private_type = |this: &mut Self| {
            this.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0419,
                    format!("type `{item}` is private to module `{module}`"),
                )
                .with_label(Label::primary(span, "private type"))
                .with_help(format!("mark `{item}` as `pub` in module `{module}`")),
            );
        };
        if let Some(info) = self.symbols.lookup_class(&qualified).cloned() {
            if info.public {
                self.symbols.define_class(local.to_string(), info);
            } else {
                private_type(self);
            }
        } else if let Some(info) = self.symbols.lookup_interface(&qualified).cloned() {
            if info.public {
                self.symbols.define_interface(local.to_string(), info);
            } else {
                private_type(self);
            }
        } else if let Some(info) = self.symbols.lookup_enum(&qualified).cloned() {
            if info.public {
                self.symbols.define_enum(local.to_string(), info);
            } else {
                private_type(self);
            }
        } else {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2006,
                    format!("no item `{item}` in module `{module}`"),
                )
                .with_label(Label::primary(span, "unknown module item")),
            );
        }
    }

    pub(super) fn register_std_imports(&mut self, imports: &[ImportDecl]) {
        for import in imports {
            if !std_registry::is_std_path(&import.path) {
                continue;
            }
            if std_registry::resolve_std_import(&import.path, import.span).is_err() {
                continue;
            }
            let segs = std_registry::import_segments(&import.path);
            match segs.as_slice() {
                ["std", "collections"] => {
                    let local = import.alias.as_deref().unwrap_or("collections");
                    self.imported_names
                        .insert(local.to_string(), Some(import.span));
                    if import.alias.is_none() {
                        self.imported_std_modules.insert(
                            local.to_string(),
                            ImportedStdModule {
                                module: "collections".to_string(),
                                span: import.span,
                            },
                        );
                        self.imported_collection_types.insert("Array".to_string());
                        self.imported_collection_types.insert("Map".to_string());
                    }
                }
                ["std", "collections", item @ ("Array" | "Map")] => {
                    let local = import.alias.as_deref().unwrap_or(item);
                    self.imported_names
                        .insert(local.to_string(), Some(import.span));
                    self.imported_collection_aliases
                        .insert(local.to_string(), (*item).to_string());
                    if import.alias.is_none() {
                        self.imported_collection_types.insert((*item).to_string());
                    }
                }
                ["std", _, item] => {
                    let local = import.alias.as_deref().unwrap_or(item);
                    self.imported_names
                        .insert(local.to_string(), Some(import.span));
                }
                ["std", module] => {
                    let local = import.alias.as_deref().unwrap_or(module);
                    self.imported_names
                        .insert(local.to_string(), Some(import.span));
                    if import.alias.is_none() {
                        self.imported_std_modules.insert(
                            local.to_string(),
                            ImportedStdModule {
                                module: (*module).to_string(),
                                span: import.span,
                            },
                        );
                    } else if matches!(*module, "env" | "fs") {
                        // `import std::fs as files;` — make the builtin
                        // module's functions resolvable under the alias and
                        // record the mapping for codegen dispatch
                        // (willow-2s3 review fix).
                        self.register_schema_module_as(module, local);
                    }
                }
                _ => {}
            }
        }
    }

    pub(super) fn resolve_imported_std_module_item(
        &mut self,
        qualified_name: &str,
        span: Span,
    ) -> Option<(String, String)> {
        let (module_local, item) = qualified_name.split_once("::")?;
        let imported = self.imported_std_modules.get(module_local).cloned()?;
        let path = format!("std::{}::{}", imported.module, item);
        match std_registry::resolve_std_import(&path, span) {
            Ok(std_registry::StdImport::Item { module, item }) => Some((module, item)),
            Ok(std_registry::StdImport::Module { .. }) => None,
            Err(diag) => {
                self.push(diag.with_label(Label::secondary(imported.span, "module imported here")));
                None
            }
        }
    }

    pub(super) fn resolve_fully_qualified_std_item(
        &mut self,
        qualified_name: &str,
        span: Span,
    ) -> Option<(String, String)> {
        if !std_registry::is_std_path(qualified_name) {
            return None;
        }
        match std_registry::resolve_std_import(qualified_name, span) {
            Ok(std_registry::StdImport::Item { module, item }) => Some((module, item)),
            Ok(std_registry::StdImport::Module { .. }) => None,
            Err(diag) => {
                self.push(diag);
                None
            }
        }
    }

    pub(super) fn register_enum(&mut self, decl: &EnumDecl) {
        let mut variant_infos = Vec::new();
        for (tag, variant) in decl.variants.iter().enumerate() {
            variant_infos.push(EnumVariantInfo {
                name: variant.name.clone(),
                payload_types: variant
                    .payload
                    .iter()
                    .map(|ty| self.normalize_type(ty, variant.span))
                    .collect(),
                tag: tag as i64,
                declaration_span: variant.span,
            });
        }
        self.symbols.define_enum(
            decl.name.clone(),
            EnumInfo {
                name: decl.name.clone(),
                public: decl.public,
                type_params: decl.type_params.clone(),
                variants: variant_infos,
                declaration_span: decl.span,
            },
        );
    }

    /// Register an enum imported from a module under its `module::Name` key, so
    /// `module::Enum` resolves as a type and `module::Enum::Variant` constructs
    /// / matches (willow-64gs). Payload types are qualified for the owning module.
    pub(super) fn register_enum_with_module(&mut self, decl: &EnumDecl, module: &str) {
        let qualified = format!("{module}::{}", decl.name);
        let mut variant_infos = Vec::new();
        for (tag, variant) in decl.variants.iter().enumerate() {
            variant_infos.push(EnumVariantInfo {
                name: variant.name.clone(),
                payload_types: variant
                    .payload
                    .iter()
                    .map(|ty| qualify_type_for_module(ty, Some(module)))
                    .collect(),
                tag: tag as i64,
                declaration_span: variant.span,
            });
        }
        self.symbols.define_enum(
            qualified.clone(),
            EnumInfo {
                name: qualified,
                public: decl.public,
                type_params: decl.type_params.clone(),
                variants: variant_infos,
                declaration_span: decl.span,
            },
        );
    }

    pub(super) fn register_class(&mut self, c: &ClassDecl) {
        let info = self.class_info_from_decl(c, &c.name, None);
        self.symbols.define_class(c.name.clone(), info);
    }

    pub(super) fn register_interface(&mut self, decl: &InterfaceDecl, module_path: Option<&str>) {
        let registered_name = match module_path {
            Some(module) => format!("{module}::{}", decl.name),
            None => decl.name.clone(),
        };
        let mut methods = HashMap::new();
        let mut method_order = Vec::new();
        for m in &decl.methods {
            // Validate the signature types (params + return) against known types.
            let return_type = if module_path.is_none() {
                self.normalize_type(&m.return_type, m.span)
            } else {
                m.return_type.clone()
            };
            let params = if module_path.is_none() {
                self.normalize_param_types(&m.params)
            } else {
                m.params.iter().map(|p| p.ty.clone()).collect()
            };
            // For a generic interface, method signatures may reference the
            // interface's type parameters (e.g. `fn from(e: E) -> Self`), which
            // are not concrete types — skip validation, like generic enums
            // (willow-1js.1). Non-generic interfaces validate normally.
            if decl.type_params.is_empty() {
                self.validate_type(&return_type, m.span);
                for (param, ty) in m.params.iter().zip(params.iter()) {
                    self.validate_type(ty, param.span);
                }
            }
            if methods.contains_key(&m.name) {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0502,
                        format!(
                            "method `{}` is declared more than once in interface `{}`",
                            m.name, decl.name
                        ),
                    )
                    .with_label(Label::primary(m.span, "duplicate interface method")),
                );
                continue;
            }
            method_order.push(m.name.clone());
            methods.insert(
                m.name.clone(),
                InterfaceMethodInfo {
                    name: m.name.clone(),
                    params,
                    has_self: m.has_self,
                    return_type,
                    declaration_span: m.span,
                },
            );
        }
        self.symbols.define_interface(
            registered_name.clone(),
            InterfaceInfo {
                name: registered_name,
                public: decl.public,
                methods,
                method_order,
                type_params: decl.type_params.clone(),
                extends: decl
                    .extends
                    .iter()
                    .map(|s| match module_path {
                        Some(m) if !s.contains("::") => format!("{m}::{s}"),
                        _ => s.clone(),
                    })
                    .collect(),
                declaration_span: decl.span,
                module_path: module_path.map(|s| s.to_string()),
            },
        );
    }

    /// Substitute an interface's generic type parameters with `type_args` and
    /// `Self` with `class_name`, yielding concrete required method signatures
    /// for conformance checking (willow-1js.1).
    pub(super) fn instantiate_interface(
        &self,
        iface: &InterfaceInfo,
        type_args: &[Type],
        self_ty: &Type,
    ) -> InterfaceInfo {
        let mut param_map: HashMap<String, Type> = iface
            .type_params
            .iter()
            .cloned()
            .zip(type_args.iter().cloned())
            .collect();
        // `Self` resolves to the receiver type: the implementing class during
        // conformance checking, or the (possibly generic) interface instantiation
        // when a method is called on an interface-typed value so `-> Self` keeps
        // its type arguments (`Box<i64>`, not bare `Box`) (willow-1js.5).
        param_map.insert("Self".to_string(), self_ty.clone());
        if param_map.is_empty() {
            return iface.clone();
        }
        let methods = iface
            .methods
            .iter()
            .map(|(k, m)| {
                (
                    k.clone(),
                    InterfaceMethodInfo {
                        name: m.name.clone(),
                        params: m
                            .params
                            .iter()
                            .map(|t| crate::semantic::symbols::substitute_type(t, &param_map))
                            .collect(),
                        has_self: m.has_self,
                        return_type: crate::semantic::symbols::substitute_type(
                            &m.return_type,
                            &param_map,
                        ),
                        declaration_span: m.declaration_span,
                    },
                )
            })
            .collect();
        InterfaceInfo {
            methods,
            ..iface.clone()
        }
    }

    pub(super) fn resolve_field(
        &mut self,
        obj_ty: &Type,
        field_name: &str,
        span: Span,
        check_visibility: bool,
    ) -> Type {
        // `Range<i64>` exposes its bounds as read-only `.start` / `.end` (i64).
        if is_i64_range_type(obj_ty) {
            if field_name == "start" || field_name == "end" {
                return Type::I64;
            }
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!("`Range<i64>` has no field `{field_name}`"),
                )
                .with_label(Label::primary(span, "unknown range field"))
                .with_help("ranges expose `.start` and `.end`"),
            );
            return Type::Void;
        }
        let class_name = match obj_ty {
            Type::Named(n) => n.clone(),
            Type::Nullable(_) => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "cannot access field `{}` on nullable type `{}`",
                            field_name,
                            type_name(obj_ty)
                        ),
                    )
                    .with_label(Label::primary(span, "nullable value may be `nil`"))
                    .with_help("check the value with `!= nil` before accessing fields"),
                );
                return Type::Void;
            }
            _ => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("type `{}` has no fields", type_name(obj_ty)),
                    )
                    .with_label(Label::primary(span, "field access on non-class type")),
                );
                return Type::Void;
            }
        };
        if self.symbols.lookup_class(&class_name).is_none() {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0350,
                    format!("class `{}` not found", class_name),
                )
                .with_label(Label::primary(span, "unknown class")),
            );
            return Type::Void;
        }
        match self.lookup_field_in_hierarchy(&class_name, field_name) {
            None => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0502,
                        format!("no field `{}` on class `{}`", field_name, class_name),
                    )
                    .with_label(Label::primary(span, "field not found")),
                );
                Type::Void
            }
            Some((owner, fi)) => {
                if check_visibility && !fi.public {
                    if fi.protected {
                        if !self.can_access_protected_member(&owner) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0503,
                                    format!("field `{}` of class `{}` is protected", field_name, owner),
                                )
                                .with_label(Label::primary(span, "protected field"))
                                .with_label(Label::secondary(fi.declaration_span, "field defined here"))
                                .with_help(format!(
                                    "prot members are accessible only within `{}` and its subclasses",
                                    owner
                                )),
                            );
                        }
                    } else if !self.can_access_private_member(&owner) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0501,
                                format!("field `{}` of class `{}` is private", field_name, owner),
                            )
                            .with_label(Label::primary(span, "private field"))
                            .with_label(Label::secondary(fi.declaration_span, "field defined here"))
                            .with_help(format!(
                                "expose it using `pub {}: {}` or provide a public getter method",
                                field_name,
                                type_name(&fi.ty)
                            )),
                        );
                    }
                }
                fi.ty.clone()
            }
        }
    }

    pub(super) fn resolve_method(
        &mut self,
        obj_ty: &Type,
        method_name: &str,
        args: &[CallArg],
        span: Span,
    ) -> Type {
        // Built-in `toString()` on primitives: `i64`/`f64`/`bool`/`String` ->
        // `String` (willow-fvfc). Class `toString()` falls through to normal
        // instance-method resolution.
        if method_name == "toString"
            && matches!(obj_ty, Type::I64 | Type::F64 | Type::Bool | Type::String)
        {
            if !args.is_empty() {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        "`toString()` takes no arguments",
                    )
                    .with_label(Label::primary(span, "unexpected arguments")),
                );
            }
            return Type::String;
        }
        // A receiver typed as a generic interface instantiation (`Box<String>`):
        // resolve against the interface with its type parameters substituted, so
        // `fn get(self) -> T` reports `String` here (willow-1js.1).
        if let Type::Generic(name, type_args) = obj_ty
            && let Some(iface) = self.symbols.lookup_interface(name).cloned()
        {
            // `Self` in a method called through the interface is the full
            // receiver instantiation (`Box<i64>`), not bare `Box`.
            let instantiated = self.instantiate_interface(&iface, type_args, obj_ty);
            return self.resolve_interface_method(&instantiated, method_name, args, span);
        }
        let class_name = match obj_ty {
            Type::Named(n) => n.clone(),
            Type::Nullable(_) => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "cannot call method `{}` on nullable type `{}`",
                            method_name,
                            type_name(obj_ty)
                        ),
                    )
                    .with_label(Label::primary(span, "nullable value may be `nil`"))
                    .with_help("check the value with `!= nil` before calling methods"),
                );
                return Type::Void;
            }
            _ => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("type `{}` has no methods", type_name(obj_ty)),
                    )
                    .with_label(Label::primary(span, "method call on non-class type")),
                );
                return Type::Void;
            }
        };
        // Interface-typed receiver: only the interface's declared methods are
        // callable, and the call dispatches through the interface vtable.
        if let Some(iface) = self.symbols.lookup_interface(&class_name).cloned() {
            return self.resolve_interface_method(&iface, method_name, args, span);
        }
        if self.symbols.lookup_class(&class_name).is_none() {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0350,
                    format!("class `{}` not found", class_name),
                )
                .with_label(Label::primary(span, "unknown class")),
            );
            return Type::Void;
        }
        // Constructors are not directly callable as `obj.init(...)` (willow-scq2).
        if method_name == "init" {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0843,
                    "constructor `init` can only be called with `new`",
                )
                .with_label(Label::primary(span, "`init` called directly"))
                .with_help(format!("write `new {}(...)` instead", class_name)),
            );
            return Type::Void;
        }
        match self.lookup_method_in_hierarchy(&class_name, method_name) {
            None => {
                let mut diagnostic = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0502,
                    format!("no method `{}` on class `{}`", method_name, class_name),
                )
                .with_label(Label::primary(span, "method not found"));

                let method_names = self.method_names_in_hierarchy(&class_name);
                if let Some(suggestion) = suggest_similar_name(method_name, method_names.iter()) {
                    diagnostic = diagnostic
                        .with_help(format!(
                            "there is a method with a similar name: `{}`",
                            suggestion
                        ))
                        .with_fix(FixSuggestion::new(
                            span,
                            suggestion,
                            "replace with suggested method",
                        ));
                }

                self.push(diagnostic);
                Type::Void
            }
            Some((owner, mi)) => {
                if mi.is_static {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0834,
                            format!(
                                "static method `{}::{}` cannot be called through an instance",
                                owner, method_name
                            ),
                        )
                        .with_label(Label::primary(span, "static method called with `.`"))
                        .with_help(format!("write `{}::{}` instead", owner, method_name)),
                    );
                    return method_call_return_type(&mi);
                }
                if !mi.public {
                    if mi.protected {
                        if !self.can_access_protected_member(&owner) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0503,
                                    format!("method `{}` of class `{}` is protected", method_name, owner),
                                )
                                .with_label(Label::primary(span, "protected method"))
                                .with_label(Label::secondary(mi.declaration_span, "method defined here"))
                                .with_help(format!(
                                    "prot members are accessible only within `{}` and its subclasses",
                                    owner
                                )),
                            );
                        }
                    } else if !self.can_access_private_member(&owner) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0501,
                                format!("method `{}` of class `{}` is private", method_name, owner),
                            )
                            .with_label(Label::primary(span, "private method"))
                            .with_label(Label::secondary(
                                mi.declaration_span,
                                "method defined here",
                            ))
                            .with_help(format!("make it public with `pub fn {}`", method_name)),
                        );
                    }
                }
                if mi.params.len() != args.len() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "method `{}` takes {} argument(s) but {} were supplied",
                                method_name,
                                mi.params.len(),
                                args.len()
                            ),
                        )
                        .with_label(Label::primary(span, "wrong number of arguments")),
                    );
                }
                self.check_call_args_against_param_infos(&mi.param_infos, args);
                method_call_return_type(&mi)
            }
        }
    }

    /// Resolve a method call on an interface-typed receiver. Only methods declared
    /// by the interface are callable; the return type is the interface method's.
    pub(super) fn resolve_interface_method(
        &mut self,
        iface: &InterfaceInfo,
        method_name: &str,
        args: &[CallArg],
        span: Span,
    ) -> Type {
        let Some(m) = iface.methods.get(method_name) else {
            let mut diag = Diagnostic::new(
                Severity::Error,
                ErrorCode::E0418,
                format!("no method `{}` on interface `{}`", method_name, iface.name),
            )
            .with_label(Label::primary(
                span,
                "method not declared by this interface",
            ));
            if let Some(suggestion) = suggest_similar_name(method_name, iface.methods.keys()) {
                diag = diag.with_help(format!("the interface declares a method `{suggestion}`"));
            } else {
                diag = diag
                    .with_help("only methods declared in the interface are callable on its values");
            }
            self.push(diag);
            // Still check the args for internal errors.
            for arg in args {
                self.check_expr(&arg.expr);
            }
            return Type::Void;
        };

        if m.params.len() != args.len() {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "method `{}` takes {} argument(s) but {} were supplied",
                        method_name,
                        m.params.len(),
                        args.len()
                    ),
                )
                .with_label(Label::primary(span, "wrong number of arguments")),
            );
        }
        for (idx, arg) in args.iter().enumerate() {
            let arg_ty = self.check_expr(&arg.expr);
            if let Some(param_ty) = m.params.get(idx)
                && !self.types_compatible(param_ty, &arg_ty)
            {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        self.type_mismatch_error_code(param_ty, &arg_ty),
                        format!(
                            "argument {} of `{}` expects `{}`, found `{}`",
                            idx + 1,
                            method_name,
                            type_name(param_ty),
                            type_name(&arg_ty)
                        ),
                    )
                    .with_label(Label::primary(
                        arg.expr.span(),
                        format!("expected `{}`", type_name(param_ty)),
                    )),
                );
            }
        }
        m.return_type.clone()
    }

    pub(super) fn resolve_static_call(
        &mut self,
        class_name: &str,
        type_args: &[Type],
        method_name: &str,
        args: &[CallArg],
        span: Span,
    ) -> Type {
        if let Some(ty) =
            self.resolve_fully_qualified_std_module_call(class_name, method_name, args, span)
        {
            return ty;
        }

        let Some(resolved_class_name) = self.resolve_static_call_class_name(class_name, span)
        else {
            return Type::Void;
        };
        let class_name = resolved_class_name.as_str();
        // Reject a static call on a private module type from another module.
        self.check_type_visibility(class_name, span);
        // Constructors are not directly callable as `Type::init(...)` — use `new`
        // (willow-scq2 §10). Check args first so their errors still surface.
        if method_name == "init" && self.symbols.lookup_class(class_name).is_some() {
            for arg in args {
                self.check_expr(&arg.expr);
            }
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0843,
                    "constructor `init` can only be called with `new`",
                )
                .with_label(Label::primary(span, "`init` called directly"))
                .with_help(format!("write `new {}(...)` instead", class_name)),
            );
            return Type::Named(class_name.to_string());
        }
        let type_args = type_args
            .iter()
            .map(|ty| self.normalize_type(ty, span))
            .collect::<Vec<_>>();
        let type_args = type_args.as_slice();

        // Built-in `Map<K, V>` constructor. The type parameters are resolved
        // from the binding's annotation (Map<Void, Void> is a placeholder, like
        // an empty array literal).
        if class_name == "Map" && method_name == "new" {
            self.check_collection_type_imported("Map", span);
            if !args.is_empty() {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        "`Map::new` takes no arguments",
                    )
                    .with_label(Label::primary(span, "unexpected arguments")),
                );
            }
            return Type::Generic("Map".to_string(), vec![Type::Void, Type::Void]);
        }

        // Check if class_name refers to an enum — handle variant construction.
        // Generic enums (with type_params) are handled separately below.
        if let Some(enum_info) = self.symbols.lookup_enum(class_name).cloned() {
            if !enum_info.type_params.is_empty() {
                // Handled by the generic enum block further down.
            } else if let Some(variant) = enum_info.variants.iter().find(|v| v.name == method_name)
            {
                if variant.payload_types.is_empty() {
                    // Fieldless variant: no args expected
                    if !args.is_empty() {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "enum variant `{}::{}` takes no arguments, got {}",
                                    class_name,
                                    method_name,
                                    args.len()
                                ),
                            )
                            .with_label(Label::primary(span, "unexpected arguments")),
                        );
                    }
                    return Type::Named(class_name.to_string());
                } else {
                    // Payload variant: check arg count and types
                    let expected = variant.payload_types.len();
                    if args.len() != expected {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "enum variant `{}::{}` takes {} argument(s), got {}",
                                    class_name,
                                    method_name,
                                    expected,
                                    args.len()
                                ),
                            )
                            .with_label(Label::primary(span, "wrong number of arguments")),
                        );
                    }
                    let payload_types = variant.payload_types.clone();
                    for (param_ty, arg) in payload_types.iter().zip(args.iter()) {
                        let arg_ty = self.check_expr(&arg.expr);
                        if !self.types_compatible(param_ty, &arg_ty) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "mismatched types: expected `{}`, found `{}`",
                                        type_name(param_ty),
                                        type_name(&arg_ty)
                                    ),
                                )
                                .with_label(Label::primary(
                                    arg.expr.span(),
                                    format!("expected `{}`", type_name(param_ty)),
                                )),
                            );
                        }
                    }
                    return Type::Named(class_name.to_string());
                }
            } else {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1208,
                        format!("no variant `{}` in enum `{}`", method_name, class_name),
                    )
                    .with_label(Label::primary(span, "unknown enum variant")),
                );
                return Type::Named(class_name.to_string());
            }
        }

        // ── Generic enum constructors ────────────────────────────────────────
        // Handles any enum with type parameters, including Option<T> and Result<T,E>
        // defined in the prelude.
        if let Some(enum_info) = self.symbols.lookup_enum(class_name).cloned()
            && !enum_info.type_params.is_empty()
        {
            if let Some(variant) = enum_info.variants.iter().find(|v| v.name == method_name) {
                // Validate arg count.
                if args.len() != variant.payload_types.len() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "`{}::{}` expects {} argument(s), got {}",
                                class_name,
                                method_name,
                                variant.payload_types.len(),
                                args.len()
                            ),
                        )
                        .with_label(Label::primary(span, "wrong number of arguments")),
                    );
                    let void_args = vec![Type::Void; enum_info.type_params.len()];
                    return Type::Generic(class_name.to_string(), void_args);
                }
                // Type-check args and infer type parameters.
                let checked_args: Vec<Type> =
                    args.iter().map(|a| self.check_expr(&a.expr)).collect();

                // Build type argument vector: for each type param, find the
                // variant payload position that uses it and use the arg type.
                // Unknown parameters default to Void.
                let type_args: Vec<Type> = enum_info
                    .type_params
                    .iter()
                    .map(|param| {
                        variant
                            .payload_types
                            .iter()
                            .zip(checked_args.iter())
                            .find_map(|(payload_ty, arg_ty)| {
                                if matches!(payload_ty, Type::Named(n) if n == param) {
                                    Some(arg_ty.clone())
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(Type::Void)
                    })
                    .collect();
                return Type::Generic(class_name.to_string(), type_args);
            } else {
                // Unknown variant in generic enum
                let valid: Vec<&str> = enum_info.variants.iter().map(|v| v.name.as_str()).collect();
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1801,
                        format!(
                            "unknown variant `{}` in `{}`; expected one of: {}",
                            method_name,
                            class_name,
                            valid.join(", ")
                        ),
                    )
                    .with_label(Label::primary(span, "unknown variant")),
                );
                return Type::Void;
            }
        }

        // Lock primitives (willow-dgwo.3): `Mutex::new(v)` / `RwLock::new(v)`
        // return `Mutex<T>` / `RwLock<T>` where T is the argument type (or the
        // explicit type argument, e.g. `Mutex<i64>::new(0)`).
        if (class_name == "Mutex" || class_name == "RwLock") && method_name == "new" {
            if args.len() != 1 {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "function `{class_name}::new` expects 1 argument, got {}",
                            args.len()
                        ),
                    )
                    .with_label(Label::primary(span, "wrong number of arguments")),
                );
                return Type::Generic(class_name.to_string(), vec![Type::Void]);
            }
            let arg_ty = self.check_expr(&args[0].expr);
            let elem = match type_args {
                [] => arg_ty.clone(),
                [t] => {
                    if arg_ty != *t && arg_ty != Type::Never {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "`{class_name}<{}>::new` expects `{}`, got `{}`",
                                    type_name(t),
                                    type_name(t),
                                    type_name(&arg_ty)
                                ),
                            )
                            .with_label(Label::primary(args[0].expr.span(), "wrong argument type")),
                        );
                    }
                    t.clone()
                }
                _ => {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("`{class_name}::new` expects 1 type argument"),
                        )
                        .with_label(Label::primary(span, "too many type arguments")),
                    );
                    arg_ty.clone()
                }
            };
            return Type::Generic(class_name.to_string(), vec![elem]);
        }

        // Atomic primitives (willow-dgwo.3): `AtomicI64::new(i64)` /
        // `AtomicBool::new(bool)` return the (non-generic) atomic type.
        if (class_name == "AtomicI64" || class_name == "AtomicBool") && method_name == "new" {
            let expected = if class_name == "AtomicI64" {
                Type::I64
            } else {
                Type::Bool
            };
            if args.len() != 1 {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "function `{class_name}::new` expects 1 argument, got {}",
                            args.len()
                        ),
                    )
                    .with_label(Label::primary(span, "wrong number of arguments")),
                );
            } else {
                let arg_ty = self.check_expr(&args[0].expr);
                if arg_ty != expected && arg_ty != Type::Never {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "`{class_name}::new` expects `{}`, got `{}`",
                                type_name(&expected),
                                type_name(&arg_ty)
                            ),
                        )
                        .with_label(Label::primary(args[0].expr.span(), "wrong argument type")),
                    );
                }
            }
            return Type::Named(class_name.to_string());
        }

        if class_name == "Channel" && method_name == "new" {
            if !args.is_empty() {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "function `Channel::new` expects 0 arguments, got {}",
                            args.len()
                        ),
                    )
                    .with_label(Label::primary(span, "wrong number of arguments")),
                );
            }
            return match type_args {
                [] => Type::Generic("Channel".to_string(), vec![Type::Void]),
                [element_ty] => Type::Generic("Channel".to_string(), vec![element_ty.clone()]),
                _ => {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "function `Channel::new` expects 1 type argument, got {}",
                                type_args.len()
                            ),
                        )
                        .with_label(Label::primary(span, "wrong number of type arguments")),
                    );
                    Type::Void
                }
            };
        }

        if class_name == "f64" && method_name == "to_string" {
            if args.len() != 1 {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "function `f64::to_string` expects 1 argument, got {}",
                            args.len()
                        ),
                    )
                    .with_label(Label::primary(span, "wrong number of arguments")),
                );
            }
            let params = [Type::F64];
            self.check_value_call_args(&params, args);
            return Type::String;
        }

        if class_name == "f64" && method_name == "parse" {
            if args.len() != 1 {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "function `f64::parse` expects 1 argument, got {}",
                            args.len()
                        ),
                    )
                    .with_label(Label::primary(span, "wrong number of arguments")),
                );
            }
            let params = [Type::String];
            self.check_value_call_args(&params, args);
            return Type::Generic(
                "Result".to_string(),
                vec![Type::F64, Type::Named("ParseFloatError".to_string())],
            );
        }

        // Check if `class_name` refers to an imported module (e.g. `math::add`).
        if let Some(module) = self.symbols.lookup_module(class_name).cloned() {
            return match module.functions.get(method_name).cloned() {
                None => {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0350,
                            format!(
                                "function `{}` not found in module `{}`",
                                method_name, class_name
                            ),
                        )
                        .with_label(Label::primary(span, "not found in module")),
                    );
                    Type::Void
                }
                Some(fi) => {
                    if !fi.public {
                        let defined_at = fi
                            .module_path
                            .as_deref()
                            .map(|path| {
                                format!(
                                    "`{}` is defined at {}:{}:{}",
                                    method_name,
                                    path,
                                    fi.declaration_span.line,
                                    fi.declaration_span.col
                                )
                            })
                            .unwrap_or_else(|| {
                                format!(
                                    "`{}` is defined at line {}, column {}",
                                    method_name, fi.declaration_span.line, fi.declaration_span.col
                                )
                            });
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0402,
                                format!("function `{}` is private", method_name),
                            )
                            .with_label(Label::primary(span, "private function"))
                            .with_note(defined_at)
                            .with_help(format!("make it public with `pub fn {}`", method_name)),
                        );
                    }
                    if args.len() != fi.params.len() {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0203,
                                format!(
                                    "function `{}::{}` expects {} argument(s), got {}",
                                    class_name,
                                    method_name,
                                    fi.params.len(),
                                    args.len()
                                ),
                            )
                            .with_label(Label::primary(span, "wrong number of arguments")),
                        );
                    }
                    self.check_call_args_against_param_infos(&fi.param_infos, args);
                    // A module-qualified call to an async fn yields `Task<T>`,
                    // just like a local async call — without this the call site
                    // types as the bare `T` and `.join()`/`await` reject it
                    // (willow-887c).
                    function_call_return_type(&fi)
                }
            };
        }

        let class = match self.symbols.lookup_class(class_name).cloned() {
            Some(c) => c,
            None => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0350,
                        format!("unknown name `{}` (not a module or class)", class_name),
                    )
                    .with_label(Label::primary(span, "unknown module or class")),
                );
                return Type::Void;
            }
        };
        match class.methods.get(method_name).cloned() {
            None => {
                let mut diagnostic = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0502,
                    format!("no method `{}` on class `{}`", method_name, class_name),
                )
                .with_label(Label::primary(span, "method not found"));

                if let Some(suggestion) = suggest_similar_name(method_name, class.methods.keys()) {
                    diagnostic = diagnostic
                        .with_help(format!(
                            "there is a method with a similar name: `{}`",
                            suggestion
                        ))
                        .with_fix(FixSuggestion::new(
                            span,
                            suggestion,
                            "replace with suggested method",
                        ));
                }

                self.push(diagnostic);
                Type::Void
            }
            Some(mi) => {
                if !mi.is_static {
                    let mut diagnostic = Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0835,
                        format!(
                            "instance method `{}::{}` requires an object",
                            class_name, method_name
                        ),
                    )
                    .with_label(Label::primary(span, "instance method called with `::`"));

                    diagnostic = if class_name == self.current_class.as_deref().unwrap_or("")
                        && self.symbols.lookup_var("self").is_some()
                    {
                        diagnostic.with_help(format!("write `self.{}` instead", method_name))
                    } else {
                        diagnostic.with_help("call it on an object value with `object.method(...)`")
                    };

                    self.push(diagnostic);
                    return method_call_return_type(&mi);
                }
                if !mi.public {
                    if mi.protected {
                        if !self.can_access_protected_member(class_name) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0503,
                                    format!(
                                        "method `{}` of class `{}` is protected",
                                        method_name, class_name
                                    ),
                                )
                                .with_label(Label::primary(span, "protected method"))
                                .with_label(Label::secondary(mi.declaration_span, "method defined here"))
                                .with_help(format!(
                                    "prot members are accessible only within `{}` and its subclasses",
                                    class_name
                                )),
                            );
                        }
                    } else {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0501,
                                format!(
                                    "method `{}` of class `{}` is private",
                                    method_name, class_name
                                ),
                            )
                            .with_label(Label::primary(span, "private method"))
                            .with_label(Label::secondary(
                                mi.declaration_span,
                                "method defined here",
                            ))
                            .with_help(format!("make it public with `pub fn {}`", method_name)),
                        );
                    }
                }
                if mi.params.len() != args.len() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "function `{}::{}` expects {} argument(s), got {}",
                                class_name,
                                method_name,
                                mi.params.len(),
                                args.len()
                            ),
                        )
                        .with_label(Label::primary(span, "wrong number of arguments")),
                    );
                }
                self.check_call_args_against_param_infos(&mi.param_infos, args);
                method_call_return_type(&mi)
            }
        }
    }

    pub(super) fn resolve_fully_qualified_std_module_call(
        &mut self,
        class_name: &str,
        method_name: &str,
        args: &[CallArg],
        span: Span,
    ) -> Option<Type> {
        if class_name != "std::io" {
            return None;
        }
        let path = format!("{class_name}::{method_name}");
        match std_registry::resolve_std_import(&path, span) {
            Ok(std_registry::StdImport::Item { module, item }) if module == "io" => {
                let Some(crate::stdlib_schema::StdItemSchema {
                    kind:
                        crate::stdlib_schema::StdItemKind::Function {
                            params,
                            return_type,
                        },
                    ..
                }) = crate::stdlib_schema::item(&module, &item)
                else {
                    return None;
                };
                if args.len() != params.len() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "function `std::io::{item}` expects {} argument{}, got {}",
                                params.len(),
                                if params.len() == 1 { "" } else { "s" },
                                args.len()
                            ),
                        )
                        .with_label(Label::primary(span, "wrong number of arguments")),
                    );
                }
                for arg in args {
                    self.check_expr(&arg.expr);
                }
                Some(std_schema_type(*return_type))
            }
            Err(diag) => {
                self.push(diag);
                Some(Type::Void)
            }
            _ => None,
        }
    }

    pub(super) fn resolve_static_call_class_name(
        &mut self,
        class_name: &str,
        span: Span,
    ) -> Option<String> {
        if class_name != "Self" {
            if let Some(item) = self.imported_collection_aliases.get(class_name).cloned() {
                return Some(item);
            }
            if let Some((module, item)) = self.resolve_fully_qualified_std_item(class_name, span) {
                if module == "collections" {
                    self.fully_qualified_collection_types.insert(item.clone());
                }
                return Some(
                    crate::stdlib_schema::type_item(&module, &item)
                        .map(|(_, builtin)| builtin.to_string())
                        .unwrap_or_else(|| format!("{module}::{item}")),
                );
            }
            if let Some((module, item)) = self.resolve_imported_std_module_item(class_name, span) {
                return Some(
                    crate::stdlib_schema::type_item(&module, &item)
                        .map(|(_, builtin)| builtin.to_string())
                        .unwrap_or_else(|| format!("{module}::{item}")),
                );
            }
            return Some(class_name.to_string());
        }

        match self.current_class.clone() {
            Some(class_name) => Some(class_name),
            None => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0550,
                        "`Self` can only be used inside a class method",
                    )
                    .with_label(Label::primary(span, "`Self` used outside class method"))
                    .with_help("use an explicit class name for static calls outside a class"),
                );
                None
            }
        }
    }

    /// Type-check a `ClassName::property` static read (willow-qsqf §7.1).
    pub(super) fn resolve_static_field_read(
        &mut self,
        class_name: &str,
        field: &str,
        span: Span,
    ) -> Type {
        let Some(resolved) = self.resolve_static_call_class_name(class_name, span) else {
            return Type::Void;
        };
        match self.lookup_static_prop_in_hierarchy(&resolved, field) {
            Some((owner, info)) => {
                // Visibility: non-public static props are reachable only from
                // inside the class (private) or subclasses (protected).
                if !info.public {
                    if info.protected {
                        if !self.can_access_protected_member(&owner) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0503,
                                    format!("static property `{}::{}` is protected", owner, field),
                                )
                                .with_label(Label::primary(span, "protected static property")),
                            );
                        }
                    } else if !self.can_access_private_member(&owner) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0419,
                                format!("static property `{}::{}` is private", owner, field),
                            )
                            .with_label(Label::primary(span, "private static property"))
                            .with_help("declare it `pub static` to expose it"),
                        );
                    }
                }
                info.ty.clone()
            }
            None => {
                // Distinguish an instance field accessed via `::` from an unknown
                // static member, for a clearer diagnostic (willow-qsqf §7.4).
                if self.lookup_field_in_hierarchy(&resolved, field).is_some() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0835,
                            format!(
                                "instance field `{}::{}` requires an object",
                                resolved, field
                            ),
                        )
                        .with_label(Label::primary(span, "instance field accessed with `::`"))
                        .with_help("access it on an object value with `object.field`"),
                    );
                } else {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0502,
                            format!("no static property `{}::{}`", resolved, field),
                        )
                        .with_label(Label::primary(span, "static property not found")),
                    );
                }
                Type::Void
            }
        }
    }

    /// Resolve the concrete payload types for a variant of a generic enum.
    /// Uses the type arguments from the scrutinee type to instantiate the enum.
    pub(super) fn resolve_generic_variant_payload(
        &self,
        enum_name: &str,
        variant_name: &str,
        scrutinee_ty: &Type,
    ) -> Option<Vec<Type>> {
        let enum_info = self.symbols.lookup_enum(enum_name)?;
        let type_args: &[Type] = if let Type::Generic(n, args) = scrutinee_ty {
            if n == enum_name { args.as_slice() } else { &[] }
        } else {
            &[]
        };
        let concrete = if enum_info.type_params.is_empty() || type_args.is_empty() {
            enum_info.clone()
        } else {
            enum_info.instantiate(type_args)
        };
        let variant = concrete.variants.iter().find(|v| v.name == variant_name)?;
        Some(variant.payload_types.clone())
    }

    /// True when `child` is an interface that transitively extends interface
    /// `parent` (willow-1js.2).
    pub(super) fn interface_extends(&self, child: &str, parent: &str) -> bool {
        if self.symbols.lookup_interface(child).is_none() {
            return false;
        }
        // Compare super names canonically so a directly-imported alias (`Named`)
        // matches a module interface's qualified `extends` entry (`proto::Named`)
        // (willow-1js.8).
        let canon = |n: &str| -> String {
            self.symbols
                .lookup_interface(n)
                .map(|i| i.name.clone())
                .unwrap_or_else(|| n.to_string())
        };
        let parent_canon = canon(parent);
        let mut stack = vec![child.to_string()];
        let mut seen = HashSet::new();
        while let Some(name) = stack.pop() {
            if !seen.insert(name.clone()) {
                continue;
            }
            let Some(info) = self.symbols.lookup_interface(&name) else {
                continue;
            };
            for sup in &info.extends {
                if canon(sup) == parent_canon {
                    return true;
                }
                stack.push(sup.clone());
            }
        }
        false
    }

    /// True when `class` (or one of its ancestors) declares `implements target`,
    /// where `target` is the (possibly generic) interface type. `target` must
    /// name a registered interface, and generic instantiations must match
    /// exactly (e.g. `Box<String>` != `Box<i64>`).
    pub(super) fn class_implements_interface(&self, class: &str, target: &Type) -> bool {
        let target_name = match target {
            Type::Named(n) | Type::Generic(n, _) => n,
            _ => return false,
        };
        if self.symbols.lookup_interface(target_name).is_none() {
            return false;
        }
        // Compare interface identity by the registered (canonical) name so a
        // directly-imported local alias (`import mod::Iface` -> bare `Iface`)
        // matches a class's qualified `implements mod::Iface` entry. Without
        // this, dispatch through a directly-imported interface fails with E0201
        // (willow-64gs.1).
        let canon_target = self.canonical_interface_type(target);
        let mut current = Some(class.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                return false;
            }
            let Some(info) = self.symbols.lookup_class(&name) else {
                return false;
            };
            if info
                .implements
                .iter()
                .any(|i| self.canonical_interface_type(i) == canon_target)
            {
                return true;
            }
            current = info.base_class.clone();
        }
        false
    }

    /// Canonicalize an interface type to its registered `InterfaceInfo.name`,
    /// folding a directly-imported local alias (`Iface`) onto the qualified
    /// name (`mod::Iface`) it was bound from. Generic type arguments are left
    /// untouched (instantiation matching stays exact). Non-interface types pass
    /// through unchanged (willow-64gs.1).
    pub(super) fn canonical_interface_type(&self, ty: &Type) -> Type {
        match ty {
            Type::Named(n) => match self.symbols.lookup_interface(n) {
                Some(info) => Type::Named(info.name.clone()),
                None => ty.clone(),
            },
            Type::Generic(n, args) => {
                let canon = self
                    .symbols
                    .lookup_interface(n)
                    .map(|info| info.name.clone())
                    .unwrap_or_else(|| n.clone());
                Type::Generic(canon, args.clone())
            }
            _ => ty.clone(),
        }
    }
}
