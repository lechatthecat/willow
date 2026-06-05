use super::symbols::{
    ClassInfo, EnumInfo, EnumVariantInfo, FieldInfo, FuncInfo, InterfaceInfo, InterfaceMethodInfo,
    MethodInfo, ModuleInfo, ParamInfo, SymbolTable, VarInfo,
};
use crate::diagnostics::{Diagnostic, ErrorCode, FixSuggestion, Label, Severity, Span};
use crate::module::std_registry;
use crate::parser::ast::*;
use std::collections::{HashMap, HashSet};

pub struct TypeChecker {
    pub symbols: SymbolTable,
    pub errors: Vec<Diagnostic>,
    /// Maps each lambda's span to its inferred (or annotated) return type.
    /// Populated during check_lambda; consumed by the backend for correct codegen.
    pub lambda_return_types: HashMap<Span, Type>,
    /// Resolved types of `let` locals declared inside `async fn` bodies, keyed by
    /// the let statement's span. Lets the backend frame-back UNANNOTATED locals
    /// that must survive `await` (willow-lpn.5c). Populated in `check`.
    pub async_local_types: HashMap<Span, Type>,
    current_return_type: Type,
    /// Stack of lambda return types being inferred. When non-empty, `return` stmts
    /// record their type here instead of checking against `current_return_type`.
    lambda_return_stack: Vec<Option<Type>>,
    current_class: Option<String>,
    current_async_context: bool,
    narrowed_vars: Vec<HashMap<String, NarrowedVar>>,
    /// Names introduced by imports (module access names and item-import locals),
    /// used to reject local declarations that collide with an import. The span
    /// is the item-import's location, or `None` for module access names.
    imported_names: HashMap<String, Option<Span>>,
    /// Collection type names made available by `std::collections` imports.
    imported_collection_types: HashSet<String>,
    /// Local aliases for collection types imported from `std::collections`.
    imported_collection_aliases: HashMap<String, String>,
    /// Collection type names referenced through fully-qualified `std` paths.
    fully_qualified_collection_types: HashSet<String>,
    /// Imported std module namespaces, keyed by their local access name.
    imported_std_modules: HashMap<String, ImportedStdModule>,
    /// Suppress duplicate missing-import diagnostics per type name.
    missing_collection_imports_reported: HashSet<String>,
    allow_range_expr: bool,
}

#[derive(Clone)]
struct NarrowedVar {
    ty: Type,
    declaration_span: Span,
}

#[derive(Clone)]
struct ImportedStdModule {
    module: String,
    span: Span,
}

#[derive(Clone)]
struct NilCheckNarrowing {
    name: String,
    narrowed_ty: Type,
    declaration_span: Span,
    non_nil_when_true: bool,
}

struct ReferencePlaceInfo {
    name: String,
    ty: Type,
    mutable: bool,
    is_param: bool,
    declaration_span: Span,
}

impl TypeChecker {
    pub fn new() -> Self {
        let mut checker = Self {
            symbols: SymbolTable::default(),
            errors: Vec::new(),
            lambda_return_types: HashMap::new(),
            async_local_types: HashMap::new(),
            current_return_type: Type::Void,
            lambda_return_stack: Vec::new(),
            current_class: None,
            current_async_context: false,
            narrowed_vars: Vec::new(),
            imported_names: HashMap::new(),
            imported_collection_types: HashSet::new(),
            imported_collection_aliases: HashMap::new(),
            fully_qualified_collection_types: HashSet::new(),
            imported_std_modules: HashMap::new(),
            missing_collection_imports_reported: HashSet::new(),
            allow_range_expr: false,
        };
        checker.register_builtin_functions();
        checker.register_builtin_modules();
        checker
    }

    fn register_builtin_functions(&mut self) {
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
            "gc_allocated_bytes".to_string(),
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
    }

    fn register_builtin_modules(&mut self) {
        let mut env_functions = std::collections::HashMap::new();
        env_functions.insert(
            "args_len".to_string(),
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
        let arg_params = vec![Type::I64];
        env_functions.insert(
            "arg".to_string(),
            FuncInfo {
                param_infos: value_param_infos(&arg_params),
                params: arg_params,
                return_type: Type::String,
                public: true,
                is_async: false,
                declaration_span: Span::dummy(),
                module_path: None,
            },
        );
        env_functions.insert(
            "program_name".to_string(),
            FuncInfo {
                param_infos: vec![],
                params: vec![],
                return_type: Type::String,
                public: true,
                is_async: false,
                declaration_span: Span::dummy(),
                module_path: None,
            },
        );
        env_functions.insert(
            "args".to_string(),
            FuncInfo {
                param_infos: vec![],
                params: vec![],
                return_type: Type::Array(Box::new(Type::String)),
                public: true,
                is_async: false,
                declaration_span: Span::dummy(),
                module_path: None,
            },
        );
        self.symbols.define_module(
            "env".to_string(),
            ModuleInfo {
                functions: env_functions,
            },
        );
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
        let mut functions = HashMap::new();
        for item in &program.items {
            match item {
                Item::Function(f) => {
                    let params = f.params.iter().map(|p| p.ty.clone()).collect::<Vec<_>>();
                    functions.insert(
                        f.name.clone(),
                        FuncInfo {
                            param_infos: param_infos_from_decl(&f.params, None),
                            params,
                            return_type: f.return_type.clone(),
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
        self.symbols
            .define_module(name.to_string(), ModuleInfo { functions });
        self.imported_names.insert(name.to_string(), None);
    }

    /// Report E2003 if `name` (a local declaration) collides with an imported
    /// name (a module access name or a directly imported item).
    fn check_local_decl_collision(&mut self, name: &str, span: Span) {
        if let Some(import_span) = self.imported_names.get(name).copied() {
            let mut diag = Diagnostic::new(
                Severity::Error,
                ErrorCode::E2003,
                format!("name `{name}` is defined both by an import and a local declaration"),
            )
            .with_label(Label::primary(span, "local declaration here"));
            if let Some(s) = import_span {
                diag = diag.with_label(Label::secondary(s, "imported here"));
            }
            self.push(diag.with_help("rename the local declaration or the import"));
        }
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

    pub fn check_program(&mut self, program: &Program) {
        self.register_std_imports(&program.imports);

        // Pass 1: register class shapes, enum declarations, and interfaces.
        // Interfaces share the top-level namespace with classes/enums/functions
        // and must be registered before class conformance is validated.
        for item in &program.items {
            match item {
                Item::Class(c) => {
                    self.check_local_decl_collision(&c.name, c.span);
                    self.register_class(c);
                }
                Item::Enum(e) => {
                    self.check_local_decl_collision(&e.name, e.span);
                    self.register_enum(e);
                }
                Item::Interface(i) => {
                    self.check_local_decl_collision(&i.name, i.span);
                    self.register_interface(i, None);
                }
                _ => {}
            }
        }

        // Pass 2: register all top-level function signatures
        for item in &program.items {
            if let Item::Function(f) = item {
                self.check_local_decl_collision(&f.name, f.span);
                let params = self.normalize_param_types(&f.params);
                let param_infos = self.normalize_param_infos(&f.params);
                let return_type = self.normalize_type(&f.return_type, f.span);
                self.symbols.define_func(
                    f.name.clone(),
                    FuncInfo {
                        param_infos,
                        params,
                        return_type,
                        public: f.public,
                        is_async: f.is_async,
                        declaration_span: f.span,
                        module_path: None,
                    },
                );
            }
        }

        // Pass 3: check bodies
        for item in &program.items {
            match item {
                Item::Function(f) => self.check_function(f),
                Item::Class(c) => self.check_class(c),
                Item::Enum(_) => {} // already registered
                Item::Interface(i) => self.check_interface(i), // validate `extends`
            }
        }
    }

    /// Validate an interface's `extends` clause (willow-1js.2 / willow-1js.8):
    /// each super must be a (single) registered interface, with no cycle.
    fn check_interface(&mut self, decl: &InterfaceDecl) {
        // v1 supports a single super-interface: a sub-interface's vtable is laid
        // out to be compatible with ONE super, so multiple supers cannot all be
        // dispatched correctly yet.
        if decl.extends.len() > 1 {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0424,
                    format!(
                        "interface `{}` extends {} interfaces; only one is supported",
                        decl.name,
                        decl.extends.len()
                    ),
                )
                .with_label(Label::primary(decl.span, "multiple super-interfaces"))
                .with_help("extend a single interface for now"),
            );
        }
        // Each super-interface must exist and be an interface.
        for sup in &decl.extends {
            if self.symbols.lookup_interface(sup).is_none() {
                if self.symbols.lookup_class(sup).is_some() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0411,
                            format!("`{sup}` is a class, not an interface"),
                        )
                        .with_label(Label::primary(decl.span, "cannot extend a class"))
                        .with_help("interfaces may only `extends` other interfaces"),
                    );
                } else {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0410,
                            format!("cannot find interface `{sup}`"),
                        )
                        .with_label(Label::primary(decl.span, "unknown super-interface")),
                    );
                }
            }
        }
        // Detect an `extends` cycle (e.g. `A extends B`, `B extends A`).
        if self.interface_extends(&decl.name, &decl.name) {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0423,
                    format!("cyclic interface inheritance involving `{}`", decl.name),
                )
                .with_label(Label::primary(
                    decl.span,
                    "interface cannot transitively extend itself",
                )),
            );
        }
    }

    fn register_std_imports(&mut self, imports: &[ImportDecl]) {
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
                    }
                }
                _ => {}
            }
        }
    }

    fn resolve_imported_std_module_item(
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

    fn resolve_fully_qualified_std_item(
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

    fn normalize_type(&mut self, ty: &Type, span: Span) -> Type {
        match ty {
            Type::Array(element) => {
                Type::Array(Box::new(self.normalize_type(element.as_ref(), span)))
            }
            Type::Generic(name, args) => {
                let args = args
                    .iter()
                    .map(|arg| self.normalize_type(arg, span))
                    .collect::<Vec<_>>();
                if let Some(item) = self.imported_collection_aliases.get(name).cloned() {
                    return self.normalize_std_type_item(name, "collections", &item, args, span);
                }
                if let Some((module, item)) = self.resolve_fully_qualified_std_item(name, span) {
                    if module == "collections" {
                        self.fully_qualified_collection_types.insert(item.clone());
                    }
                    return self.normalize_std_type_item(name, &module, &item, args, span);
                }
                if let Some((module, item)) = self.resolve_imported_std_module_item(name, span) {
                    return self.normalize_std_type_item(name, &module, &item, args, span);
                }
                Type::Generic(name.clone(), args)
            }
            Type::Named(name) => {
                if self.imported_collection_aliases.contains_key(name) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("type `{name}` expects type arguments"),
                        )
                        .with_label(Label::primary(span, "missing type arguments")),
                    );
                    Type::Void
                } else if let Some((module, item)) =
                    self.resolve_fully_qualified_std_item(name, span)
                {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("type `{}.{}` expects type arguments", module, item),
                        )
                        .with_label(Label::primary(span, "missing type arguments")),
                    );
                    Type::Void
                } else if let Some((module, item)) =
                    self.resolve_imported_std_module_item(name, span)
                {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("type `{}.{}` expects type arguments", module, item),
                        )
                        .with_label(Label::primary(span, "missing type arguments")),
                    );
                    Type::Void
                } else {
                    ty.clone()
                }
            }
            Type::Nullable(inner) => {
                Type::Nullable(Box::new(self.normalize_type(inner.as_ref(), span)))
            }
            Type::Fn(params, ret) => Type::Fn(
                params
                    .iter()
                    .map(|param| self.normalize_type(param, span))
                    .collect(),
                Box::new(self.normalize_type(ret.as_ref(), span)),
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

    fn normalize_param_types(&mut self, params: &[Param]) -> Vec<Type> {
        params
            .iter()
            .map(|param| self.normalize_type(&param.ty, param.type_span))
            .collect()
    }

    fn normalize_param_infos(&mut self, params: &[Param]) -> Vec<ParamInfo> {
        params
            .iter()
            .map(|param| ParamInfo {
                ty: self.normalize_type(&param.ty, param.type_span),
                mode: param.mode.clone(),
                span: param.span,
                type_span: param.type_span,
            })
            .collect()
    }

    fn normalize_std_type_item(
        &mut self,
        source_name: &str,
        module: &str,
        item: &str,
        args: Vec<Type>,
        span: Span,
    ) -> Type {
        match (module, item) {
            ("collections", "Array") => {
                if args.len() != 1 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "`{source_name}` expects 1 type argument, got {}",
                                args.len()
                            ),
                        )
                        .with_label(Label::primary(span, "wrong number of type arguments")),
                    );
                    Type::Array(Box::new(Type::Void))
                } else {
                    Type::Array(Box::new(args.into_iter().next().unwrap()))
                }
            }
            ("collections", "Map") => {
                if args.len() != 2 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "`{source_name}` expects 2 type arguments, got {}",
                                args.len()
                            ),
                        )
                        .with_label(Label::primary(span, "wrong number of type arguments")),
                    );
                }
                Type::Generic("Map".to_string(), args)
            }
            ("option", "Option") => {
                if args.len() != 1 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "`{source_name}` expects 1 type argument, got {}",
                                args.len()
                            ),
                        )
                        .with_label(Label::primary(span, "wrong number of type arguments")),
                    );
                }
                Type::Generic("Option".to_string(), args)
            }
            ("result", "Result") => {
                if args.len() != 2 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "`{source_name}` expects 2 type arguments, got {}",
                                args.len()
                            ),
                        )
                        .with_label(Label::primary(span, "wrong number of type arguments")),
                    );
                }
                Type::Generic("Result".to_string(), args)
            }
            _ => Type::Generic(source_name.to_string(), args),
        }
    }

    fn check_collection_type_imported(&mut self, name: &str, span: Span) {
        if self.imported_collection_types.contains(name)
            || self.fully_qualified_collection_types.contains(name)
            || self
                .imported_collection_aliases
                .values()
                .any(|item| item == name)
        {
            return;
        }
        if !self
            .missing_collection_imports_reported
            .insert(name.to_string())
        {
            return;
        }
        let (code, help) = match name {
            "Array" => (ErrorCode::E2001, "add `import std::collections::Array;`"),
            "Map" => (ErrorCode::E2002, "add `import std::collections::Map;`"),
            _ => return,
        };
        self.push(
            Diagnostic::new(
                Severity::Error,
                code,
                format!("cannot find type `{name}` in scope"),
            )
            .with_label(Label::primary(span, "collection type requires an import"))
            .with_help(help),
        );
    }

    fn register_enum(&mut self, decl: &EnumDecl) {
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
    fn register_enum_with_module(&mut self, decl: &EnumDecl, module: &str) {
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

    fn register_class(&mut self, c: &ClassDecl) {
        let info = self.class_info_from_decl(c, &c.name, None);
        self.symbols.define_class(c.name.clone(), info);
    }

    fn class_info_from_decl(
        &mut self,
        class: &ClassDecl,
        registered_name: &str,
        module_prefix: Option<&str>,
    ) -> ClassInfo {
        let mut fields = HashMap::new();
        let mut methods = HashMap::new();

        for field in &class.fields {
            fields.insert(
                field.name.clone(),
                FieldInfo {
                    ty: self.normalize_decl_type(&field.ty, field.span, module_prefix),
                    public: field.public,
                    protected: field.protected,
                    declaration_span: field.span,
                },
            );
        }
        for method in &class.methods {
            let params = method
                .params
                .iter()
                .map(|param| self.normalize_decl_type(&param.ty, param.type_span, module_prefix))
                .collect();
            methods.insert(
                method.name.clone(),
                MethodInfo {
                    param_infos: self.normalize_decl_param_infos(&method.params, module_prefix),
                    params,
                    has_self: method.has_self,
                    return_type: self.normalize_decl_type(
                        &method.return_type,
                        method.span,
                        module_prefix,
                    ),
                    public: method.public,
                    protected: method.protected,
                    is_open: method.is_open,
                    is_override: method.is_override,
                    declaration_span: method.span,
                },
            );
        }

        ClassInfo {
            name: registered_name.to_string(),
            public: class.public,
            is_open: class.is_open,
            base_class: class
                .base_class
                .as_ref()
                .map(|base| qualified_type_path_name(base, module_prefix)),
            implements: class
                .implements
                .iter()
                .map(|iface| qualify_type_for_module(iface, module_prefix))
                .collect(),
            declaration_span: class.span,
            fields,
            methods,
        }
    }

    fn normalize_decl_type(&mut self, ty: &Type, span: Span, module_prefix: Option<&str>) -> Type {
        if module_prefix.is_some() {
            qualify_type_for_module(ty, module_prefix)
        } else {
            self.normalize_type(ty, span)
        }
    }

    fn normalize_decl_param_infos(
        &mut self,
        params: &[Param],
        module_prefix: Option<&str>,
    ) -> Vec<ParamInfo> {
        params
            .iter()
            .map(|param| ParamInfo {
                ty: self.normalize_decl_type(&param.ty, param.type_span, module_prefix),
                mode: param.mode.clone(),
                span: param.span,
                type_span: param.type_span,
            })
            .collect()
    }

    fn register_interface(&mut self, decl: &InterfaceDecl, module_path: Option<&str>) {
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

    fn check_class(&mut self, c: &ClassDecl) {
        self.check_class_inheritance(c);
        self.check_class_implements(c);
        for field in &c.fields {
            let ty = self.normalize_type(&field.ty, field.span);
            self.validate_type(&ty, field.span);
        }
        for m in &c.methods {
            self.check_method(m, &c.name);
        }
    }

    /// Validate a class's `implements` clause: each named interface must exist
    /// and be an interface, must not be repeated, and the class (including its
    /// inherited methods) must satisfy every required method signature exactly.
    fn check_class_implements(&mut self, c: &ClassDecl) {
        let mut seen: HashSet<String> = HashSet::new();
        for iface_ty in &c.implements {
            // Split the implemented interface into its name and type arguments:
            // `Animal` -> ("Animal", []), `From<Err>` -> ("From", [Err]).
            let (iface_name, type_args): (String, Vec<Type>) = match iface_ty {
                Type::Named(n) => (n.clone(), Vec::new()),
                Type::Generic(n, args) => (n.clone(), args.clone()),
                other => (type_name(other), Vec::new()),
            };

            // A class may implement a given interface instantiation at most once,
            // keyed by the FULL instantiated type (name + type arguments). Two
            // distinct instantiations of the same generic interface
            // (`Container<i64>`, `Container<String>`) are allowed (willow-1js.6):
            // each compiled class method is monomorphic and a generic interface's
            // vtable slot order is independent of its type arguments, so all
            // instantiations of one interface on one class share a single,
            // byte-identical vtable (keyed by interface name in codegen — see
            // `declare_one_vtable`). Conformance still rejects any case where one
            // method body cannot satisfy every instantiation (e.g. `get(self)->T`
            // cannot return both `i64` and `String`, E0417); only interfaces whose
            // type parameters appear in no method signature can be implemented at
            // multiple instantiations. An EXACT-duplicate instantiation
            // (`Container<i64>` twice) remains an error.
            let inst_key = type_name(iface_ty);
            if !seen.insert(inst_key.clone()) {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0414,
                        format!("interface `{inst_key}` is implemented more than once"),
                    )
                    .with_label(Label::primary(c.span, "duplicate interface"))
                    .with_help(
                        "a class may implement a given interface instantiation only once; remove the duplicate",
                    ),
                );
                continue;
            }

            // Resolve: interface? class (wrong kind)? or unknown?
            let iface = match self.symbols.lookup_interface(&iface_name) {
                Some(info) => info.clone(),
                None => {
                    if self.symbols.lookup_class(&iface_name).is_some() {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0411,
                                format!("`{iface_name}` is a class, not an interface"),
                            )
                            .with_label(Label::primary(c.span, "not an interface"))
                            .with_help("a class can only `implements` interfaces; use `extends` for a base class"),
                        );
                    } else {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0410,
                                format!("cannot find interface `{iface_name}`"),
                            )
                            .with_label(Label::primary(c.span, "unknown interface"))
                            .with_help(
                                "define an `interface` with this name, or check the spelling",
                            ),
                        );
                    }
                    continue;
                }
            };

            // Type-argument arity must match the interface's generic parameters.
            if type_args.len() != iface.type_params.len() {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0422,
                        format!(
                            "interface `{}` takes {} type argument(s), but {} were given",
                            iface_name,
                            iface.type_params.len(),
                            type_args.len()
                        ),
                    )
                    .with_label(Label::primary(c.span, "wrong number of type arguments")),
                );
                continue;
            }

            // Instantiate the interface for this class: substitute its type
            // parameters with the given arguments and `Self` with the class
            // (so `fn from(e: E) -> Self` conforms to a concrete signature).
            let instantiated = self.instantiate_interface(&iface, &type_args, &c.name);
            self.check_interface_conformance(c, &instantiated);
        }
    }

    /// Substitute an interface's generic type parameters with `type_args` and
    /// `Self` with `class_name`, yielding concrete required method signatures
    /// for conformance checking (willow-1js.1).
    fn instantiate_interface(
        &self,
        iface: &InterfaceInfo,
        type_args: &[Type],
        class_name: &str,
    ) -> InterfaceInfo {
        let mut param_map: HashMap<String, Type> = iface
            .type_params
            .iter()
            .cloned()
            .zip(type_args.iter().cloned())
            .collect();
        param_map.insert("Self".to_string(), Type::Named(class_name.to_string()));
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

    /// Check that class `c` provides every method required by `iface` with an
    /// exact (MVP: invariant) signature match. Inherited methods count.
    fn check_interface_conformance(&mut self, c: &ClassDecl, iface: &InterfaceInfo) {
        for req_name in &iface.method_order {
            let req = &iface.methods[req_name];
            // A method declared on the class itself or inherited from an ancestor
            // can satisfy the requirement.
            let found = self.lookup_method_in_hierarchy(&c.name, req_name);
            let Some((owner, method)) = found else {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0415,
                        format!(
                            "class `{}` does not implement interface `{}`",
                            c.name, iface.name
                        ),
                    )
                    .with_label(Label::primary(
                        c.span,
                        format!("missing method `{}`", interface_method_signature(req)),
                    ))
                    .with_label(Label::secondary(
                        req.declaration_span,
                        "required by this interface method",
                    ))
                    .with_help(format!("add `pub fn {}` to `{}`", req_name, c.name)),
                );
                continue;
            };

            // The implementing method must be public so it is callable through
            // the interface reference.
            if !method.public {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0415,
                        format!(
                            "method `{}` on `{}` must be `pub` to satisfy interface `{}`",
                            req_name, owner, iface.name
                        ),
                    )
                    .with_label(Label::primary(method.declaration_span, "method is private"))
                    .with_help("interface methods are public by contract; mark it `pub`"),
                );
            }

            // Receiver compatibility: an interface instance method requires `self`.
            if req.has_self && !method.has_self {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0416,
                        format!(
                            "method `{}` on `{}` must take `self` to satisfy interface `{}`",
                            req_name, owner, iface.name
                        ),
                    )
                    .with_label(Label::primary(
                        method.declaration_span,
                        "missing `self` receiver",
                    ))
                    .with_label(Label::secondary(
                        req.declaration_span,
                        "interface requires `self`",
                    )),
                );
            }

            // Parameter count and types must match exactly (no variance in MVP).
            if method.params != req.params {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0416,
                        format!(
                            "method `{}` parameters do not match interface `{}`",
                            req_name, iface.name
                        ),
                    )
                    .with_label(Label::primary(
                        method.declaration_span,
                        format!(
                            "found `({})`",
                            method
                                .params
                                .iter()
                                .map(type_name)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    ))
                    .with_label(Label::secondary(
                        req.declaration_span,
                        format!(
                            "interface requires `({})`",
                            req.params
                                .iter()
                                .map(type_name)
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    )),
                );
            }

            // Return type must match exactly.
            if method.return_type != req.return_type {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0417,
                        format!(
                            "method `{}` returns `{}`, but interface `{}` requires `{}`",
                            req_name,
                            type_name(&method.return_type),
                            iface.name,
                            type_name(&req.return_type)
                        ),
                    )
                    .with_label(Label::primary(
                        method.declaration_span,
                        "return type mismatch",
                    ))
                    .with_label(Label::secondary(
                        req.declaration_span,
                        "required return type declared here",
                    )),
                );
            }
        }
    }

    fn check_class_inheritance(&mut self, c: &ClassDecl) {
        let Some(base_name) = c.base_class.as_ref().map(type_path_name) else {
            for method in &c.methods {
                if method.is_override {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0702,
                            format!(
                                "method `{}` is marked `override`, but `{}` has no base class",
                                method.name, c.name
                            ),
                        )
                        .with_label(Label::primary(method.span, "nothing to override"))
                        .with_help("remove `override` or add a base class with a matching method"),
                    );
                }
            }
            return;
        };

        match self.symbols.lookup_class(&base_name).cloned() {
            None => {
                // A class may not extend an interface; that is what `implements` is for.
                if self.symbols.lookup_interface(&base_name).is_some() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0412,
                            format!("`{base_name}` is an interface and cannot be extended"),
                        )
                        .with_label(Label::primary(c.span, "cannot `extends` an interface"))
                        .with_help(format!("use `implements {base_name}` instead of `extends`")),
                    );
                    return;
                }
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0350,
                        format!("base class `{}` not found", base_name),
                    )
                    .with_label(Label::primary(c.span, "unknown base class")),
                );
                return;
            }
            Some(base) => {
                if !base.is_open {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0701,
                            format!("class `{}` is not open for inheritance", base_name),
                        )
                        .with_label(Label::primary(c.span, "cannot extend this class"))
                        .with_label(Label::secondary(
                            base.declaration_span,
                            "base class defined here",
                        ))
                        .with_help(format!(
                            "declare the base class as `open class {}`",
                            base.name
                        )),
                    );
                }
            }
        }

        for method in &c.methods {
            // Static methods (no `self`) participate in the class namespace but are
            // not inherited/overridable in the same way as instance methods.
            // Skip override validation for static methods.
            if !method.has_self {
                continue;
            }
            let inherited = self.lookup_method_in_ancestors(&base_name, &method.name);
            match (method.is_override, inherited) {
                (false, Some((owner, _))) => {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0702,
                            format!(
                                "method `{}` overrides `{}` but is missing `override`",
                                method.name, owner
                            ),
                        )
                        .with_label(Label::primary(method.span, "missing `override`"))
                        .with_help(format!("write `override fn {}`", method.name)),
                    );
                }
                (true, None) => {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0702,
                            format!(
                                "method `{}` is marked `override`, but no inherited method exists",
                                method.name
                            ),
                        )
                        .with_label(Label::primary(method.span, "no matching base method"))
                        .with_help("remove `override` or add a matching method to the base class"),
                    );
                }
                (true, Some((owner, base_method))) => {
                    if !base_method.is_open {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0703,
                                format!(
                                    "method `{}` in `{}` is not open for override",
                                    method.name, owner
                                ),
                            )
                            .with_label(Label::primary(method.span, "cannot override"))
                            .with_label(Label::secondary(
                                base_method.declaration_span,
                                "base method defined here",
                            ))
                            .with_help(format!(
                                "declare the base method as `open fn {}`",
                                method.name
                            )),
                        );
                    }

                    let method_params = method
                        .params
                        .iter()
                        .map(|param| self.normalize_type(&param.ty, param.type_span))
                        .collect::<Vec<_>>();
                    let method_return_type = self.normalize_type(&method.return_type, method.span);
                    if method_params != base_method.params
                        || method_return_type != base_method.return_type
                    {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0703,
                                format!(
                                    "override `{}` does not match the inherited method signature",
                                    method.name
                                ),
                            )
                            .with_label(Label::primary(method.span, "signature mismatch"))
                            .with_label(Label::secondary(
                                base_method.declaration_span,
                                "inherited signature defined here",
                            ))
                            .with_help(
                                "use the same parameter and return types as the base method",
                            ),
                        );
                    }
                }
                (false, None) => {}
            }
        }
    }

    fn check_method(&mut self, m: &MethodDecl, class_name: &str) {
        let return_type = self.normalize_type(&m.return_type, m.span);
        let param_types = self.normalize_param_types(&m.params);
        self.validate_type(&return_type, m.span);
        for (param, ty) in m.params.iter().zip(param_types.iter()) {
            self.validate_type(ty, param.span);
        }
        if m.is_async {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0807,
                    "async methods are not supported yet",
                )
                .with_label(Label::primary(m.span, "async method parsed here"))
                .with_help("async lowering and runtime support are tracked separately"),
            );
        }
        let previous_class = self.current_class.replace(class_name.to_string());
        let previous_async_context = self.current_async_context;
        self.current_async_context = m.is_async;
        self.current_return_type = return_type;
        self.symbols.push_scope();

        // `self` has the type of the enclosing class inside instance methods.
        if m.has_self {
            let receiver_ty = Type::Named(class_name.to_string());
            self.symbols.define_var(
                "self".to_string(),
                VarInfo {
                    ty: receiver_ty.clone(),
                    mutable: false,
                    is_param: true,
                    declaration_span: m.span,
                },
            );
        }

        for (param, ty) in m.params.iter().zip(param_types.iter()) {
            self.symbols.define_var(
                param.name.clone(),
                VarInfo {
                    ty: ty.clone(),
                    mutable: matches!(&param.mode, ParamMode::Reference { mutable: true, .. }),
                    is_param: true,
                    declaration_span: param.span,
                },
            );
        }

        self.check_block(&m.body);
        self.symbols.pop_scope();
        self.current_class = previous_class;
        self.current_async_context = previous_async_context;
    }

    fn check_function(&mut self, f: &FunctionDecl) {
        let return_type = self.normalize_type(&f.return_type, f.span);
        let param_types = self.normalize_param_types(&f.params);
        self.validate_type(&return_type, f.span);
        for (param, ty) in f.params.iter().zip(param_types.iter()) {
            self.validate_type(ty, param.span);
        }
        let previous_async_context = self.current_async_context;
        self.current_async_context = f.is_async;
        self.current_return_type = return_type;
        self.symbols.push_scope();
        for (param, ty) in f.params.iter().zip(param_types.iter()) {
            self.symbols.define_var(
                param.name.clone(),
                VarInfo {
                    ty: ty.clone(),
                    mutable: matches!(&param.mode, ParamMode::Reference { mutable: true, .. }),
                    is_param: true,
                    declaration_span: param.span,
                },
            );
        }
        self.check_block(&f.body);
        self.symbols.pop_scope();
        self.current_async_context = previous_async_context;
    }

    fn check_block(&mut self, block: &Block) {
        self.symbols.push_scope();
        self.narrowed_vars.push(HashMap::new());
        for stmt in &block.stmts {
            self.check_stmt(stmt);
        }
        self.narrowed_vars.pop();
        self.symbols.pop_scope();
    }

    fn check_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(s) => {
                let annotation = s.ty.as_ref().map(|ty| self.normalize_type(ty, s.span));
                // A `let xs: Array<I> = [..]` literal is checked element-wise
                // against `I`, so classes implementing interface `I` are accepted.
                let inferred = match (&annotation, &s.init) {
                    (Some(Type::Array(elem)), Expr::ArrayLiteral(elements, lit_span)) => {
                        self.check_array_literal_expecting(elements, *lit_span, Some(elem.as_ref()))
                    }
                    _ => self.check_expr(&s.init),
                };
                let ty = if let Some(ann) = &annotation {
                    self.validate_type(ann, s.span);
                    let channel_new_infers_from_annotation =
                        channel_element_type(ann).is_some() && is_untyped_channel_new_call(&s.init);
                    if !channel_new_infers_from_annotation && !self.types_compatible(ann, &inferred)
                    {
                        let code = self.type_mismatch_error_code(ann, &inferred);
                        let message = if code == ErrorCode::E0704 {
                            format!(
                                "cannot assign `{}` to variable `{}` of type `{}`",
                                type_name(&inferred),
                                s.name,
                                type_name(ann)
                            )
                        } else {
                            format!(
                                "mismatched types: expected `{}`, found `{}`",
                                type_name(ann),
                                type_name(&inferred)
                            )
                        };
                        let label = if code == ErrorCode::E0704 {
                            format!(
                                "expected `{}` because of this type annotation",
                                type_name(ann)
                            )
                        } else {
                            format!("expected `{}`", type_name(ann))
                        };
                        self.push(
                            Diagnostic::new(Severity::Error, code, message)
                                .with_label(Label::primary(s.span, label)),
                        );
                    }
                    ann.clone()
                } else {
                    if inferred == Type::Nil {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                "cannot infer the type of `nil`",
                            )
                            .with_label(Label::primary(
                                s.init.span(),
                                "`nil` needs a nullable type",
                            ))
                            .with_help(
                                "add a nullable type annotation, e.g. `let value: Node? = nil;`",
                            ),
                        );
                    } else if let Some(diag) = self.unresolved_generic_enum_diagnostic(
                        &s.init,
                        &inferred,
                        s.init.span(),
                        &s.name,
                    ) {
                        self.push(diag);
                    }
                    inferred
                };
                // Record the resolved type of locals inside async fns so the
                // backend can frame-back unannotated live-across-await locals
                // (willow-lpn.5c).
                if self.current_async_context {
                    self.async_local_types.insert(s.span, ty.clone());
                }
                // `_` is a wildcard: evaluate the initializer for side effects but do
                // not bind a variable (allows multiple `let _ = expr;` in the same scope).
                if s.name == "_" {
                    return;
                }
                // E0351: reject redeclaration in the same scope.
                if let Some(_prev) = self.symbols.lookup_var_current_scope(&s.name) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0351,
                            format!("variable `{}` is already defined in this scope", s.name),
                        )
                        .with_label(Label::primary(s.span, "previous definition here")),
                    );
                }
                self.symbols.define_var(
                    s.name.clone(),
                    VarInfo {
                        ty,
                        mutable: s.mutable,
                        is_param: false,
                        declaration_span: s.span,
                    },
                );
            }
            Stmt::FieldAssign(s) => {
                let obj_ty = self.check_expr(&s.object);
                let field_ty = self.resolve_field(&obj_ty, &s.field, s.span, true);
                let val_ty = self.check_expr(&s.value);
                if field_ty != Type::Void && !self.types_compatible(&field_ty, &val_ty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            self.type_mismatch_error_code(&field_ty, &val_ty),
                            format!(
                                "mismatched types: expected `{}`, found `{}`",
                                type_name(&field_ty),
                                type_name(&val_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            s.span,
                            format!("expected `{}`", type_name(&field_ty)),
                        )),
                    );
                }
            }
            Stmt::IndexAssign(s) => {
                let arr_ty = self.check_expr(&s.array);
                let idx_ty = self.check_expr(&s.index);
                if !matches!(idx_ty, Type::I64) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("array index must be `i64`, found `{}`", type_name(&idx_ty)),
                        )
                        .with_label(Label::primary(s.index.span(), "index is not an `i64`")),
                    );
                }
                let val_ty = self.check_expr(&s.value);
                match &arr_ty {
                    Type::Array(elem) => {
                        if !self.types_compatible(elem, &val_ty) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    self.type_mismatch_error_code(elem, &val_ty),
                                    format!(
                                        "cannot assign `{}` to an element of `Array<{}>`",
                                        type_name(&val_ty),
                                        type_name(elem)
                                    ),
                                )
                                .with_label(Label::primary(
                                    s.span,
                                    format!("expected `{}`", type_name(elem)),
                                )),
                            );
                        }
                    }
                    Type::Void => {}
                    other => {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!("cannot index a value of type `{}`", type_name(other)),
                            )
                            .with_label(Label::primary(s.span, "not an array")),
                        );
                    }
                }
            }
            Stmt::Assign(s) => {
                if s.name == "this" {
                    self.push_legacy_this_error(s.span);
                    return;
                }
                // Reject direct assignment to `self`.
                if s.name == "self" {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0552,
                            format!("cannot assign to `{}`", s.name),
                        )
                        .with_label(Label::primary(s.span, "cannot assign to receiver"))
                        .with_help(format!("to mutate fields, use `{}.field = value`", s.name)),
                    );
                    return;
                }
                let info = self.symbols.lookup_var(&s.name).cloned();
                match info {
                    None => self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0350,
                            format!("cannot find variable `{}`", s.name),
                        )
                        .with_label(Label::primary(s.span, "not found in this scope")),
                    ),
                    Some(info) => {
                        if !info.mutable {
                            if info.is_param {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0302,
                                        format!(
                                            "cannot assign to immutable parameter `{}`",
                                            s.name
                                        ),
                                    )
                                    .with_label(Label::primary(
                                        s.span,
                                        "cannot assign to parameter",
                                    ))
                                    .with_help(format!(
                                        "introduce a mutable local variable: `let mut {} = {};`",
                                        s.name, s.name
                                    )),
                                );
                            } else {
                                // Build an insertion span just after "let " in the declaration.
                                let decl = info.declaration_span;
                                let insert_span = Span::new(
                                    decl.start + 4,
                                    decl.start + 4,
                                    decl.line,
                                    decl.col + 4,
                                );
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0301,
                                        format!("cannot assign to immutable variable `{}`", s.name),
                                    )
                                    .with_label(Label::primary(s.span, "cannot assign"))
                                    .with_label(Label::secondary(
                                        info.declaration_span,
                                        "declared immutable here",
                                    ))
                                    .with_help(format!(
                                        "declare it as mutable: `let mut {} = ...`",
                                        s.name
                                    ))
                                    .with_fix(
                                        FixSuggestion::insertion(
                                            insert_span,
                                            "mut ",
                                            "add `mut` here",
                                        ),
                                    ),
                                );
                            }
                        }
                        let got = self.check_expr(&s.value);
                        if !self.types_compatible(&info.ty, &got) {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    self.type_mismatch_error_code(&info.ty, &got),
                                    format!(
                                        "mismatched types: expected `{}`, found `{}`",
                                        type_name(&info.ty),
                                        type_name(&got)
                                    ),
                                )
                                .with_label(Label::primary(
                                    s.span,
                                    format!("expected `{}`", type_name(&info.ty)),
                                )),
                            );
                        }
                        self.clear_narrowing(&s.name);
                    }
                }
            }
            Stmt::If(s) => {
                let cond_ty = self.check_expr(&s.cond);
                let nil_narrowing = self.nil_check_narrowing(&s.cond);
                if cond_ty != Type::Bool {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0203,
                            format!("condition must be `bool`, found `{}`", type_name(&cond_ty)),
                        )
                        .with_label(Label::primary(
                            s.cond.span(),
                            format!("expected `bool`, found `{}`", type_name(&cond_ty)),
                        ))
                        .with_help("use an explicit comparison, e.g. `!= 0`"),
                    );
                }
                match nil_narrowing.as_ref() {
                    Some(narrowing) if narrowing.non_nil_when_true => {
                        self.check_block_with_narrowing(&s.then_block, narrowing);
                    }
                    _ => self.check_block(&s.then_block),
                }
                if let Some(else_b) = &s.else_block {
                    match nil_narrowing.as_ref() {
                        Some(narrowing) if !narrowing.non_nil_when_true => {
                            self.check_block_with_narrowing(else_b, narrowing);
                        }
                        _ => self.check_block(else_b),
                    }
                } else if let Some(narrowing) = nil_narrowing.as_ref() {
                    if !narrowing.non_nil_when_true && block_always_returns(&s.then_block) {
                        self.add_narrowing_to_current_scope(narrowing);
                    }
                }
            }
            Stmt::While(s) => {
                let cond_ty = self.check_expr(&s.cond);
                if cond_ty != Type::Bool {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0203,
                            format!("condition must be `bool`, found `{}`", type_name(&cond_ty)),
                        )
                        .with_label(Label::primary(
                            s.cond.span(),
                            format!("expected `bool`, found `{}`", type_name(&cond_ty)),
                        ))
                        .with_help("use an explicit comparison, e.g. `!= 0`"),
                    );
                }
                self.check_block(&s.body);
            }
            Stmt::For(s) => {
                let prev_allow_range = self.allow_range_expr;
                self.allow_range_expr = true;
                let iterable_ty = self.check_expr(&s.iterable);
                self.allow_range_expr = prev_allow_range;
                let elem_ty = match &iterable_ty {
                    Type::Array(elem) => (**elem).clone(),
                    Type::Generic(name, args)
                        if name == "Range" && args.as_slice() == [Type::I64] =>
                    {
                        Type::I64
                    }
                    Type::Void => Type::Void,
                    other => {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!("cannot iterate over `{}`", type_name(other)),
                            )
                            .with_label(Label::primary(
                                s.iterable.span(),
                                "for-in requires an array or i64 range",
                            ))
                            .with_help(
                                "use `for item in array { ... }` with `Array<T>` or `for n in start..end { ... }`",
                            ),
                        );
                        Type::Void
                    }
                };

                if self.current_async_context {
                    let iter_slot_ty = if is_i64_range_type(&iterable_ty) {
                        Type::I64
                    } else {
                        iterable_ty.clone()
                    };
                    self.async_local_types
                        .insert(s.iter_frame_key(), iter_slot_ty);
                    self.async_local_types
                        .insert(s.index_frame_key(), Type::I64);
                    if s.name != "_" {
                        self.async_local_types.insert(s.name_span, elem_ty.clone());
                    }
                }

                self.symbols.push_scope();
                self.narrowed_vars.push(HashMap::new());
                if s.name != "_" {
                    self.symbols.define_var(
                        s.name.clone(),
                        VarInfo {
                            ty: elem_ty,
                            mutable: false,
                            is_param: false,
                            declaration_span: s.name_span,
                        },
                    );
                }
                for stmt in &s.body.stmts {
                    self.check_stmt(stmt);
                }
                self.narrowed_vars.pop();
                self.symbols.pop_scope();
            }
            Stmt::Return(s) => {
                // `return Result::Ok();` (zero-arg) is the success value of a
                // `Result<void, E>` function: the Ok payload is void, so no
                // argument is required (willow-exg).
                if let Some(Expr::StaticCall(sc)) = &s.value {
                    let returns_result_void = matches!(
                        &self.current_return_type,
                        Type::Generic(n, args)
                            if n == "Result" && args.len() == 2 && args[0] == Type::Void
                    );
                    if returns_result_void
                        && sc.class == "Result"
                        && sc.method == "Ok"
                        && sc.args.is_empty()
                    {
                        return;
                    }
                }
                let ret_ty = s
                    .value
                    .as_ref()
                    .map(|v| self.check_expr(v))
                    .unwrap_or(Type::Void);
                // Inside a lambda with no annotation: record the return type for inference.
                if let Some(slot) = self.lambda_return_stack.last_mut() {
                    if slot.is_none() {
                        *slot = Some(ret_ty.clone());
                    }
                    return; // don't validate against outer current_return_type
                }
                if !self.types_compatible(&self.current_return_type, &ret_ty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            self.type_mismatch_error_code(&self.current_return_type, &ret_ty),
                            format!(
                                "mismatched types: expected `{}`, found `{}`",
                                type_name(&self.current_return_type),
                                type_name(&ret_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            s.span,
                            format!("expected `{}`", type_name(&self.current_return_type)),
                        )),
                    );
                }
            }
            Stmt::Expr(s) => {
                self.check_expr(&s.expr);
            }
        }
    }

    fn check_expr(&mut self, expr: &Expr) -> Type {
        match expr {
            Expr::Integer(_, _) => Type::I64,
            Expr::Float(_, _) => Type::F64,
            Expr::Bool(_, _) => Type::Bool,
            Expr::Nil(_) => Type::Nil,
            Expr::String(_, _) => Type::String,
            Expr::Var(name, span) => {
                if name == "this" {
                    self.push_legacy_this_error(*span);
                    return Type::Void;
                }
                // Local variable?
                if let Some(info) = self.symbols.lookup_var(name) {
                    if let Some(narrowed_ty) = self.lookup_narrowed_type(name) {
                        return narrowed_ty;
                    }
                    return info.ty.clone();
                }
                // Named function used as a value: `apply(10, double)` where `double: fn(...)`
                if let Some(info) = self.symbols.lookup_func(name) {
                    let params = info.params.clone();
                    let ret = info.return_type.clone();
                    return Type::Fn(params, Box::new(ret));
                }
                // Give a specialized error for receiver keywords used outside instance methods.
                if name == "self" {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0550,
                            "`self` can only be used inside an instance method",
                        )
                        .with_label(Label::primary(*span, "`self` used outside instance method"))
                        .with_help("declare the method with `self` as the first parameter"),
                    );
                    return Type::Void;
                }
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0350,
                        format!("cannot find variable `{}`", name),
                    )
                    .with_label(Label::primary(*span, "not found in this scope")),
                );
                Type::I64
            }
            Expr::Binary(b) => self.check_binary(b),
            Expr::Unary(u) => self.check_unary(u),
            Expr::Call(c) => {
                if c.callee == "format" {
                    return self.check_format_call(c);
                }

                // Direct call to a named function.
                if let Some(info) = self.symbols.lookup_func(&c.callee).cloned() {
                    if info.params.len() != c.args.len() {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "function `{}` takes {} argument(s) but {} were supplied",
                                    c.callee,
                                    info.params.len(),
                                    c.args.len()
                                ),
                            )
                            .with_label(Label::primary(c.span, "wrong number of arguments")),
                        );
                    }
                    self.check_call_args_against_param_infos(&info.param_infos, &c.args);
                    return function_call_return_type(&info);
                }

                // Indirect call through a function-type local variable.
                if let Some(var_info) = self.symbols.lookup_var(&c.callee).cloned() {
                    if let Type::Fn(param_types, ret) = var_info.ty {
                        if param_types.len() != c.args.len() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "function value `{}` takes {} argument(s) but {} were supplied",
                                        c.callee,
                                        param_types.len(),
                                        c.args.len()
                                    ),
                                )
                                .with_label(Label::primary(c.span, "wrong number of arguments")),
                            );
                        }
                        self.check_value_call_args(&param_types, &c.args);
                        return *ret;
                    }
                }

                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0350,
                        format!("cannot find function `{}`", c.callee),
                    )
                    .with_label(Label::primary(c.span, "not found in this scope")),
                );
                Type::Void
            }
            Expr::FieldAccess(obj, field_name, span) => {
                let obj_ty = self.check_expr(obj);
                self.resolve_field(&obj_ty, field_name, *span, true)
            }
            Expr::MethodCall(m) => {
                // `.` is instance member access; module items use `::`. Using
                // `math.add(..)` on a module is an error that points at `::`.
                if let Expr::Var(name, _) = &m.object {
                    if self.symbols.lookup_var(name).is_none()
                        && self.symbols.lookup_module(name).is_some()
                    {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0350,
                                format!("`{name}` is a module; use `::` to access its items"),
                            )
                            .with_label(Label::primary(m.span, "module accessed with `.`"))
                            .with_help(format!(
                                "write `{name}::{method}(...)` instead of `{name}.{method}(...)`",
                                method = m.method
                            )),
                        );
                        return Type::Void;
                    }
                }
                let obj_ty = self.check_expr(&m.object);
                if let Some(ret) = self.check_option_result_method_call(&obj_ty, m) {
                    return ret;
                }
                if let Some(ret) = self.check_concurrency_method_call(&obj_ty, m) {
                    return ret;
                }
                if let Some(ret) = self.check_array_method_call(&obj_ty, m) {
                    return ret;
                }
                if let Some(ret) = self.check_map_method_call(&obj_ty, m) {
                    return ret;
                }
                let ret = self.resolve_method(&obj_ty, &m.method, &m.args, m.span);
                ret
            }
            Expr::StaticCall(s) => {
                self.resolve_static_call(&s.class, &s.type_args, &s.method, &s.args, s.span)
            }
            Expr::ObjectLiteral(o) => self.check_object_literal(o),
            Expr::Spawn(s) => self.check_spawn(s),
            Expr::Await(a) => {
                let awaited_ty = self.check_expr(&a.expr);
                if !self.current_async_context {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0801,
                            "`await` can only be used inside an async function",
                        )
                        .with_label(Label::primary(
                            a.span,
                            "`await` used in a non-async function",
                        ))
                        .with_help("make the enclosing function `async`"),
                    );
                    return Type::Void;
                }
                match awaited_ty {
                    Type::Generic(name, mut args) if name == "Future" && args.len() == 1 => {
                        args.remove(0)
                    }
                    other => {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0803,
                                format!("cannot await value of type `{}`", type_name(&other)),
                            )
                            .with_label(Label::primary(a.expr.span(), "expected `Future<T>`"))
                            .with_help(
                                "await only values returned by async functions or Future APIs",
                            ),
                        );
                        Type::Void
                    }
                }
            }
            Expr::Select(s) => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0807,
                        "select blocks are not supported yet",
                    )
                    .with_label(Label::primary(s.span, "select block parsed here"))
                    .with_help("select lowering and async channel support are tracked separately"),
                );
                Type::Void
            }
            Expr::Print(arg, _, _) => {
                self.check_expr(arg);
                Type::Void
            }
            Expr::Ternary(t) => {
                let cond_ty = self.check_expr(&t.condition);
                if cond_ty != Type::Bool {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0901,
                            format!(
                                "ternary condition must be `bool`, found `{}`",
                                type_name(&cond_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            t.condition.span(),
                            format!("expected `bool`, found `{}`", type_name(&cond_ty)),
                        )),
                    );
                }
                let then_ty = self.check_expr(&t.then_expr);
                let else_ty = self.check_expr(&t.else_expr);
                if let Some(unified_ty) = self.unify_ternary_types(&then_ty, &else_ty) {
                    self.validate_type(&unified_ty, t.span);
                    unified_ty
                } else {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0902,
                            format!(
                                "ternary branches have incompatible types: `{}` and `{}`",
                                type_name(&then_ty),
                                type_name(&else_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            t.else_expr.span(),
                            format!(
                                "expected `{}`, found `{}`",
                                type_name(&then_ty),
                                type_name(&else_ty)
                            ),
                        ))
                        .with_label(Label::secondary(
                            t.then_expr.span(),
                            format!("this branch has type `{}`", type_name(&then_ty)),
                        )),
                    );
                    Type::Void
                }
            }
            Expr::Range(r) => self.check_range(r),
            Expr::Lambda(l) => self.check_lambda(l),
            Expr::Match(m) => self.check_match_expr(m),
            Expr::TryPropagate(inner, span) => self.check_try_propagate(inner, *span),
            Expr::ArrayLiteral(elements, span) => self.check_array_literal(elements, *span),
            Expr::Index(arr, index, span) => self.check_index(arr, index, *span),
        }
    }

    fn check_range(&mut self, range: &RangeExpr) -> Type {
        if !self.allow_range_expr {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    "range expressions are only supported in `for` loops",
                )
                .with_label(Label::primary(
                    range.span,
                    "range used outside a `for` iterable",
                ))
                .with_help("write `for n in start..end { ... }`"),
            );
        }

        let prev_allow_range = self.allow_range_expr;
        self.allow_range_expr = false;
        let start_ty = self.check_expr(&range.start);
        let end_ty = self.check_expr(&range.end);
        self.allow_range_expr = prev_allow_range;

        if start_ty != Type::I64 || end_ty != Type::I64 {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "range bounds must be `i64`, found `{}` and `{}`",
                        type_name(&start_ty),
                        type_name(&end_ty)
                    ),
                )
                .with_label(Label::primary(range.span, "range bounds must be `i64`")),
            );
        }

        range_type()
    }

    /// Type-check an array literal `[e0, e1, ...]`. The element type is inferred
    /// from the first element; all elements must agree. An empty literal yields
    /// `Array<Void>`, an unresolved placeholder that a type annotation resolves
    /// (e.g. `let xs: Array<i64> = [];`).
    fn check_array_literal(&mut self, elements: &[Expr], span: Span) -> Type {
        self.check_array_literal_expecting(elements, span, None)
    }

    /// Type-check an array literal. When `expected_elem` is given (e.g. from a
    /// `let xs: Array<Animal> = [...]` annotation), each element is checked
    /// against it — this allows a heterogeneous literal of classes that all
    /// implement the same interface, and the literal takes the expected type.
    fn check_array_literal_expecting(
        &mut self,
        elements: &[Expr],
        _span: Span,
        expected_elem: Option<&Type>,
    ) -> Type {
        if let Some(expected) = expected_elem {
            for el in elements {
                let ty = self.check_expr(el);
                if !self.types_compatible(expected, &ty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            self.type_mismatch_error_code(expected, &ty),
                            format!(
                                "array element expects `{}`, found `{}`",
                                type_name(expected),
                                type_name(&ty)
                            ),
                        )
                        .with_label(Label::primary(el.span(), "mismatched element type")),
                    );
                }
            }
            return Type::Array(Box::new(expected.clone()));
        }

        if elements.is_empty() {
            return Type::Array(Box::new(Type::Void));
        }
        let first_ty = self.check_expr(&elements[0]);
        for el in elements.iter().skip(1) {
            let ty = self.check_expr(el);
            if !self.types_compatible(&first_ty, &ty) {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "array elements must have the same type: expected `{}`, found `{}`",
                            type_name(&first_ty),
                            type_name(&ty)
                        ),
                    )
                    .with_label(Label::primary(el.span(), "mismatched element type")),
                );
            }
        }
        Type::Array(Box::new(first_ty))
    }

    /// Type-check an index expression `arr[index]`. `arr` must be `Array<T>` and
    /// `index` must be `i64`; the result type is `T`.
    fn check_index(&mut self, arr: &Expr, index: &Expr, span: Span) -> Type {
        let arr_ty = self.check_expr(arr);
        let idx_ty = self.check_expr(index);
        if !matches!(idx_ty, Type::I64) {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!("array index must be `i64`, found `{}`", type_name(&idx_ty)),
                )
                .with_label(Label::primary(index.span(), "index is not an `i64`")),
            );
        }
        match &arr_ty {
            Type::Array(elem) => (**elem).clone(),
            // Recover quietly from an earlier error that produced Void.
            Type::Void => Type::Void,
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("cannot index a value of type `{}`", type_name(other)),
                    )
                    .with_label(Label::primary(span, "not an array"))
                    .with_help("indexing with `[..]` requires an `Array<T>`"),
                );
                Type::Void
            }
        }
    }

    /// Builtin methods on `Array<T>`. Returns `Some(ret)` when `obj_ty` is an
    /// array (handling the method or reporting an unknown one), `None` otherwise.
    fn check_array_method_call(&mut self, obj_ty: &Type, m: &MethodCallExpr) -> Option<Type> {
        let Type::Array(elem) = obj_ty else {
            return None;
        };
        match m.method.as_str() {
            "len" => {
                if !m.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            "`Array::len` takes no arguments",
                        )
                        .with_label(Label::primary(m.span, "unexpected arguments")),
                    );
                }
                Some(Type::I64)
            }
            "push" => {
                let elem_ty = (**elem).clone();
                if m.args.len() != 1 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("`Array::push` expects 1 argument, got {}", m.args.len()),
                        )
                        .with_label(Label::primary(m.span, "expected `push(value)`")),
                    );
                } else {
                    let v = self.check_expr(&m.args[0].expr);
                    if !self.types_compatible(&elem_ty, &v) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "cannot push `{}` to `Array<{}>`",
                                    type_name(&v),
                                    type_name(&elem_ty)
                                ),
                            )
                            .with_label(Label::primary(
                                m.args[0].expr.span(),
                                "wrong element type",
                            )),
                        );
                    }
                }
                Some(Type::Void)
            }
            "pop" => {
                if !m.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            "`Array::pop` takes no arguments",
                        )
                        .with_label(Label::primary(m.span, "unexpected arguments")),
                    );
                }
                Some((**elem).clone())
            }
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("no method `{}` on `Array<{}>`", other, type_name(elem)),
                    )
                    .with_label(Label::primary(m.span, "unknown array method"))
                    .with_help(
                        "arrays support `.len()`, `.push(v)`, `.pop()`, and indexing `arr[i]`",
                    ),
                );
                Some(Type::Void)
            }
        }
    }

    /// Builtin methods on `Map<K, V>`: `insert(k, v)`, `get(k) -> Option<V>`,
    /// `contains(k) -> bool`, `len() -> i64`. Returns `Some(ret)` when `obj_ty`
    /// is a map, `None` otherwise.
    fn check_map_method_call(&mut self, obj_ty: &Type, m: &MethodCallExpr) -> Option<Type> {
        let Type::Generic(name, args) = obj_ty else {
            return None;
        };
        if name != "Map" || args.len() != 2 {
            return None;
        }
        let key_ty = args[0].clone();
        let val_ty = args[1].clone();

        let check_key = |checker: &mut Self, arg: &CallArg| {
            let k = checker.check_expr(&arg.expr);
            if key_ty != Type::Void && !checker.types_compatible(&key_ty, &k) {
                checker.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "map key type mismatch: expected `{}`, found `{}`",
                            type_name(&key_ty),
                            type_name(&k)
                        ),
                    )
                    .with_label(Label::primary(arg.expr.span(), "wrong key type")),
                );
            }
        };

        match m.method.as_str() {
            "insert" => {
                if m.args.len() != 2 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("`Map::insert` expects 2 arguments, got {}", m.args.len()),
                        )
                        .with_label(Label::primary(m.span, "expected `insert(key, value)`")),
                    );
                } else {
                    check_key(self, &m.args[0]);
                    let v = self.check_expr(&m.args[1].expr);
                    if val_ty != Type::Void && !self.types_compatible(&val_ty, &v) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "map value type mismatch: expected `{}`, found `{}`",
                                    type_name(&val_ty),
                                    type_name(&v)
                                ),
                            )
                            .with_label(Label::primary(m.args[1].expr.span(), "wrong value type")),
                        );
                    }
                }
                Some(Type::Void)
            }
            "get" => {
                if m.args.len() != 1 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("`Map::get` expects 1 argument, got {}", m.args.len()),
                        )
                        .with_label(Label::primary(m.span, "expected `get(key)`")),
                    );
                } else {
                    check_key(self, &m.args[0]);
                }
                Some(Type::Generic("Option".to_string(), vec![val_ty]))
            }
            "contains" => {
                if m.args.len() != 1 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("`Map::contains` expects 1 argument, got {}", m.args.len()),
                        )
                        .with_label(Label::primary(m.span, "expected `contains(key)`")),
                    );
                } else {
                    check_key(self, &m.args[0]);
                }
                Some(Type::Bool)
            }
            "len" => {
                if !m.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            "`Map::len` takes no arguments",
                        )
                        .with_label(Label::primary(m.span, "unexpected arguments")),
                    );
                }
                Some(Type::I64)
            }
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "no method `{}` on `Map<{}, {}>`",
                            other,
                            type_name(&key_ty),
                            type_name(&val_ty)
                        ),
                    )
                    .with_label(Label::primary(m.span, "unknown map method"))
                    .with_help("maps support `.insert(k, v)`, `.get(k)`, `.contains(k)`, `.len()`"),
                );
                Some(Type::Void)
            }
        }
    }

    fn check_try_propagate(&mut self, inner: &Expr, span: Span) -> Type {
        let operand_ty = self.check_expr(inner);

        if let Type::Generic(name, args) = &operand_ty {
            if name == "Option" && args.len() == 1 {
                let some_ty = args[0].clone();
                let return_ty = self.current_return_type.clone();
                match &return_ty {
                    Type::Generic(ret_name, ret_args)
                        if ret_name == "Option" && ret_args.len() == 1 =>
                    {
                        return some_ty;
                    }
                    other => {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1807,
                                format!(
                                    "`?` on `Option<T>` can only be used inside a function returning `Option<U>`, found `{}`",
                                    type_name(other)
                                ),
                            )
                            .with_label(Label::primary(span, "invalid context for Option `?`"))
                            .with_help("change the function return type to `Option<U>`"),
                        );
                        return some_ty;
                    }
                }
            }
        }

        // Otherwise the operand must be Result<T,E>.
        let (ok_ty, err_ty) = match &operand_ty {
            Type::Generic(name, args) if name == "Result" && args.len() == 2 => {
                (args[0].clone(), args[1].clone())
            }
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1806,
                        format!(
                            "the `?` operator requires `Result<T,E>` or `Option<T>`, found `{}`",
                            type_name(other)
                        ),
                    )
                    .with_label(Label::primary(span, "not a Result or Option"))
                    .with_help(
                        "wrap the value in `Result::Ok(...)`, `Result::Err(...)`, or `Option::Some(...)`",
                    ),
                );
                return Type::Void;
            }
        };

        // The enclosing function must return Result<U,E> with matching error type
        let return_ty = self.current_return_type.clone();
        match &return_ty {
            Type::Generic(name, args) if name == "Result" && args.len() == 2 => {
                if args[1] == err_ty || args[1] == Type::Void || err_ty == Type::Void {
                    // ok_ty is the success value type
                    ok_ty
                } else if self.err_converts_via_into(&err_ty, &args[1]) {
                    // Automatic error conversion (willow-1ow): the operand error
                    // `E1` implements `Into<E2>`, so `?` converts `E1 -> E2` on
                    // the Err early-return path. Codegen emits `e1.into()`.
                    ok_ty
                } else {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E1805,
                            format!(
                                "error type mismatch: function returns `Result<_, {}>` but `?` propagates `{}`",
                                type_name(&args[1]),
                                type_name(&err_ty)
                            ),
                        )
                        .with_label(Label::primary(span, "error type mismatch"))
                        .with_help(format!(
                            "implement `Into<{}>` on `{}` to allow `?` to convert the error",
                            type_name(&args[1]),
                            type_name(&err_ty)
                        )),
                    );
                    ok_ty
                }
            }
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1807,
                        format!(
                            "`?` can only be used inside a function returning `Result<T,E>`, found `{}`",
                            type_name(other)
                        ),
                    )
                    .with_label(Label::primary(span, "invalid context for `?`"))
                    .with_help("change the function return type to `Result<T, E>`"),
                );
                ok_ty
            }
        }
    }

    fn check_lambda(&mut self, l: &LambdaExpr) -> Type {
        // All params must have type annotations (or infer from expected type — not yet supported).
        let mut param_types = Vec::new();
        for p in &l.params {
            match &p.ty {
                Some(ty) => {
                    self.validate_type(ty, p.span);
                    param_types.push(ty.clone());
                }
                None => {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E1001,
                            format!("cannot infer type for lambda parameter `{}`", p.name),
                        )
                        .with_label(Label::primary(p.span, "type annotation required"))
                        .with_help("add a parameter type, e.g. `|x: i64|`"),
                    );
                    param_types.push(Type::I64); // recover
                }
            }
        }

        // Determine expected return type from annotation (if any) for use in the body.
        let expected_ret = l.return_type.clone();
        if let Some(ret) = &expected_ret {
            self.validate_type(ret, l.span);
        }

        // Type-check the body with params in scope.
        self.symbols.push_scope();
        for (p, ty) in l.params.iter().zip(&param_types) {
            self.symbols.define_var(
                p.name.clone(),
                crate::semantic::symbols::VarInfo {
                    ty: ty.clone(),
                    mutable: false,
                    is_param: true,
                    declaration_span: p.span,
                },
            );
        }

        // Save/restore outer return type so `return` stmts in the lambda body
        // are checked against the lambda's return type, not the enclosing function's.
        let saved_ret_ty = self.current_return_type.clone();

        let body_ty = match &l.body {
            LambdaBody::Expr(e) => self.check_expr(e),
            LambdaBody::Block(b) => {
                if let Some(ref ann) = expected_ret {
                    // Annotation provided: validate return stmts against it.
                    self.current_return_type = ann.clone();
                    for stmt in &b.stmts {
                        self.check_stmt(stmt);
                    }
                    ann.clone()
                } else {
                    // No annotation: collect the return type via the lambda stack.
                    self.lambda_return_stack.push(None);
                    for stmt in &b.stmts {
                        self.check_stmt(stmt);
                    }
                    let inferred = self
                        .lambda_return_stack
                        .pop()
                        .flatten()
                        .unwrap_or(Type::Void);
                    inferred
                }
            }
        };
        self.current_return_type = saved_ret_ty;
        self.symbols.pop_scope();

        let ret_ty = match &l.return_type {
            Some(ann) => {
                if !self.types_compatible(ann, &body_ty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            self.type_mismatch_error_code(ann, &body_ty),
                            format!(
                                "lambda return type mismatch: expected `{}`, found `{}`",
                                type_name(ann),
                                type_name(&body_ty)
                            ),
                        )
                        .with_label(Label::primary(l.span, "return type mismatch")),
                    );
                }
                ann.clone()
            }
            None => body_ty,
        };

        // Record the inferred return type so the backend can use it without
        // falling back to I64 when no explicit annotation is present.
        self.lambda_return_types.insert(l.span, ret_ty.clone());

        Type::Fn(param_types, Box::new(ret_ty))
    }

    fn check_match_expr(&mut self, m: &MatchExpr) -> Type {
        let scrutinee_ty = self.check_expr(&m.scrutinee);

        if m.arms.is_empty() {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E1202,
                    "match expression has no arms",
                )
                .with_label(Label::primary(m.span, "no arms in match")),
            );
            return Type::Void;
        }

        let mut covered_variants: HashSet<String> = HashSet::new();
        let mut has_wildcard = false;
        let mut has_true = false;
        let mut has_false = false;
        let mut result_type: Option<Type> = None;
        let mut found_unreachable = false;

        for arm in &m.arms {
            // Check if arm is unreachable (after a wildcard/binding)
            if has_wildcard && !found_unreachable {
                self.push(
                    Diagnostic::new(Severity::Warning, ErrorCode::W1201, "unreachable match arm")
                        .with_label(Label::primary(arm.span, "this arm is unreachable")),
                );
                found_unreachable = true;
            }

            // Validate pattern and track coverage
            match &arm.pattern {
                Pattern::Wildcard(_) => {
                    has_wildcard = true;
                }
                Pattern::Binding { .. } => {
                    has_wildcard = true; // binding covers everything
                }
                Pattern::LiteralBool(b, span) => {
                    if scrutinee_ty != Type::Bool {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1205,
                                format!(
                                    "bool pattern cannot match scrutinee of type `{}`",
                                    type_name(&scrutinee_ty)
                                ),
                            )
                            .with_label(Label::primary(*span, "pattern type mismatch")),
                        );
                    }
                    if *b {
                        has_true = true;
                    } else {
                        has_false = true;
                    }
                }
                Pattern::LiteralInt(_, span) => {
                    if scrutinee_ty != Type::I64 {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1205,
                                format!(
                                    "integer pattern cannot match scrutinee of type `{}`",
                                    type_name(&scrutinee_ty)
                                ),
                            )
                            .with_label(Label::primary(*span, "pattern type mismatch")),
                        );
                    }
                }
                Pattern::EnumVariant {
                    enum_name,
                    variant,
                    span,
                } => {
                    // Generic enum variant patterns: the scrutinee may be
                    // Generic(enum_name, type_args) rather than Named(enum_name).
                    let is_builtin_match = matches!(&scrutinee_ty,
                        Type::Generic(n, _) if n == enum_name
                    );
                    // Verify enum_name matches scrutinee type
                    if !is_builtin_match {
                        match &scrutinee_ty {
                            Type::Named(sname) if sname == enum_name => {}
                            _ => {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E1205,
                                        format!(
                                            "enum pattern `{}::{}` cannot match scrutinee of type `{}`",
                                            enum_name,
                                            variant,
                                            type_name(&scrutinee_ty)
                                        ),
                                    )
                                    .with_label(Label::primary(*span, "pattern type mismatch")),
                                );
                            }
                        }
                        // Verify variant exists
                        let variant_valid = self
                            .symbols
                            .lookup_enum(enum_name)
                            .and_then(|e| e.variants.iter().find(|v| v.name == *variant))
                            .is_some();
                        if !variant_valid {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E1208,
                                    format!("no variant `{}` in enum `{}`", variant, enum_name),
                                )
                                .with_label(Label::primary(*span, "unknown enum variant")),
                            );
                        }
                    }
                    covered_variants.insert(variant.clone());
                }
                Pattern::EnumVariantTuple {
                    enum_name,
                    variant,
                    bindings,
                    span,
                } => {
                    // Generic enum variant: resolve concrete payload types from scrutinee.
                    let builtin_payload: Option<Vec<Type>> =
                        self.resolve_generic_variant_payload(enum_name, variant, &scrutinee_ty);

                    if let Some(ref pts) = builtin_payload {
                        // Built-in generic variant — validate binding count
                        if bindings.len() != pts.len() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E1209,
                                    format!(
                                        "variant `{}::{}` expects {} field(s), found {}",
                                        enum_name,
                                        variant,
                                        pts.len(),
                                        bindings.len()
                                    ),
                                )
                                .with_label(Label::primary(*span, "wrong number of bindings")),
                            );
                        }
                    } else {
                        // User-defined enum variant
                        match &scrutinee_ty {
                            Type::Named(sname) if sname == enum_name => {}
                            _ => {
                                self.push(Diagnostic::new(Severity::Error, ErrorCode::E1205,
                                    format!("enum pattern `{}::{}(..)` cannot match scrutinee of type `{}`",
                                        enum_name, variant, type_name(&scrutinee_ty)))
                                    .with_label(Label::primary(*span, "pattern type mismatch")));
                            }
                        }
                        let payload_types = self
                            .symbols
                            .lookup_enum(enum_name)
                            .and_then(|e| e.variants.iter().find(|v| v.name == *variant))
                            .map(|v| v.payload_types.clone());
                        match payload_types {
                            None => {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E1208,
                                        format!("no variant `{}` in enum `{}`", variant, enum_name),
                                    )
                                    .with_label(Label::primary(*span, "unknown enum variant")),
                                );
                            }
                            Some(ref pts) => {
                                if pts.is_empty() {
                                    self.push(
                                        Diagnostic::new(
                                            Severity::Error,
                                            ErrorCode::E1209,
                                            format!(
                                                "variant `{}::{}` has no payload; remove `(..)`",
                                                enum_name, variant
                                            ),
                                        )
                                        .with_label(
                                            Label::primary(
                                                *span,
                                                "fieldless variant used with payload pattern",
                                            ),
                                        ),
                                    );
                                } else if bindings.len() != pts.len() {
                                    self.push(
                                        Diagnostic::new(
                                            Severity::Error,
                                            ErrorCode::E1209,
                                            format!(
                                                "variant `{}::{}` expects {} field(s), found {}",
                                                enum_name,
                                                variant,
                                                pts.len(),
                                                bindings.len()
                                            ),
                                        )
                                        .with_label(
                                            Label::primary(*span, "wrong number of bindings"),
                                        ),
                                    );
                                }
                            }
                        }
                    }
                    covered_variants.insert(variant.clone());
                }
                Pattern::ClassDowncast {
                    class_name, span, ..
                } => {
                    // `Dog(d)` downcasts an interface scrutinee to a concrete
                    // class. The scrutinee must be an interface, and the class
                    // must implement it (else the arm can never match).
                    // Class patterns do not contribute to exhaustiveness, so a
                    // wildcard arm is still required.
                    let scrut_is_interface = matches!(&scrutinee_ty,
                        Type::Named(n) | Type::Generic(n, _)
                            if self.symbols.lookup_interface(n).is_some());
                    if !scrut_is_interface {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1205,
                                format!(
                                    "class pattern `{}(..)` requires an interface scrutinee, found `{}`",
                                    class_name,
                                    type_name(&scrutinee_ty)
                                ),
                            )
                            .with_label(Label::primary(*span, "scrutinee is not an interface"))
                            .with_help("match on a value of interface type to downcast to a class"),
                        );
                    } else if self.symbols.lookup_class(class_name).is_none() {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0350,
                                format!("cannot find class `{class_name}`"),
                            )
                            .with_label(Label::primary(*span, "unknown class in pattern")),
                        );
                    } else if !self.class_implements_interface(class_name, &scrutinee_ty) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0415,
                                format!(
                                    "class `{}` does not implement `{}`, so this pattern can never match",
                                    class_name,
                                    type_name(&scrutinee_ty)
                                ),
                            )
                            .with_label(Label::primary(*span, "unrelated class")),
                        );
                    }
                }
            }

            // Check arm body in a new scope
            self.symbols.push_scope();
            // For EnumVariantTuple: bind payload variables in arm scope
            if let Pattern::EnumVariantTuple {
                enum_name,
                variant,
                bindings,
                ..
            } = &arm.pattern
            {
                // Resolve payload types: first check built-in generic types
                // Resolve concrete payload types: use generic instantiation when available.
                let payload_types: Vec<Type> = self
                    .resolve_generic_variant_payload(enum_name, variant, &scrutinee_ty)
                    .unwrap_or_default();
                for (binding, ty) in bindings.iter().zip(payload_types.iter()) {
                    self.symbols.define_var(
                        binding.clone(),
                        VarInfo {
                            ty: ty.clone(),
                            mutable: false,
                            is_param: false,
                            declaration_span: arm.pattern.span(),
                        },
                    );
                }
            }
            // For a class downcast pattern, bind the downcast value as the
            // concrete class (willow-1js.4). `_` does not bind.
            if let Pattern::ClassDowncast {
                class_name,
                binding,
                span: bspan,
            } = &arm.pattern
                && binding != "_"
            {
                self.symbols.define_var(
                    binding.clone(),
                    VarInfo {
                        ty: Type::Named(class_name.clone()),
                        mutable: false,
                        is_param: false,
                        declaration_span: *bspan,
                    },
                );
            }
            // For binding patterns, define the variable
            if let Pattern::Binding { name, span: bspan } = &arm.pattern {
                self.symbols.define_var(
                    name.clone(),
                    VarInfo {
                        ty: scrutinee_ty.clone(),
                        mutable: false,
                        is_param: false,
                        declaration_span: *bspan,
                    },
                );
            }
            let arm_ty = self.check_match_body(&arm.body);
            self.symbols.pop_scope();

            // Never arms don't constrain result type
            if arm_ty == Type::Never {
                continue;
            }

            // When the new arm type is a partial-generic (e.g. `Option<void>` from `None`),
            // keep the richer type already recorded rather than replacing it.
            let arm_ty = match (&result_type, &arm_ty) {
                (Some(existing), arm) if self.generic_partially_matches(existing, arm) => {
                    existing.clone()
                }
                (Some(existing), arm) if self.generic_partially_matches(arm, existing) => {
                    arm.clone()
                }
                _ => arm_ty,
            };

            match &result_type {
                None => result_type = Some(arm_ty),
                Some(existing) => {
                    if !self.types_compatible(existing, &arm_ty) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1201,
                                format!(
                                    "match arms have incompatible types: `{}` and `{}`",
                                    type_name(existing),
                                    type_name(&arm_ty)
                                ),
                            )
                            .with_label(Label::primary(
                                arm.span,
                                format!("found `{}`", type_name(&arm_ty)),
                            )),
                        );
                    }
                }
            }
        }

        // Exhaustiveness check
        if !has_wildcard {
            match &scrutinee_ty {
                Type::Bool => {
                    if !has_true || !has_false {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1207,
                                "non-exhaustive match: missing bool patterns",
                            )
                            .with_label(Label::primary(m.span, "match is not exhaustive"))
                            .with_help("add `true` and `false` patterns, or use a wildcard `_`"),
                        );
                    }
                }
                Type::I64 => {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E1206,
                            "non-exhaustive match on `i64`: add a wildcard arm `_ => ...`",
                        )
                        .with_label(Label::primary(m.span, "match is not exhaustive")),
                    );
                }
                Type::Named(enum_name) => {
                    if let Some(enum_info) = self.symbols.lookup_enum(enum_name).cloned() {
                        for variant in &enum_info.variants {
                            if !covered_variants.contains(&variant.name) {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E1202,
                                        format!(
                                            "non-exhaustive match: variant `{}::{}` not covered",
                                            enum_name, variant.name
                                        ),
                                    )
                                    .with_label(Label::primary(m.span, "match is not exhaustive")),
                                );
                            }
                        }
                    }
                }
                // Generic enum: check all variants are covered.
                // Uses registered enum info so any stdlib or user generic enum is handled.
                Type::Generic(enum_name, _) => {
                    if let Some(enum_info) = self.symbols.lookup_enum(enum_name).cloned() {
                        for variant in &enum_info.variants {
                            if !covered_variants.contains(&variant.name) {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E1202,
                                        format!(
                                            "non-exhaustive match: variant `{}::{}` not covered",
                                            enum_name, variant.name
                                        ),
                                    )
                                    .with_label(Label::primary(m.span, "match is not exhaustive"))
                                    .with_help("add the missing variant or use a wildcard `_` arm"),
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        result_type.unwrap_or(Type::Void)
    }

    fn check_match_body(&mut self, body: &MatchBody) -> Type {
        match body {
            MatchBody::Expr(expr) => self.check_expr(expr),
            MatchBody::Block(block) => {
                self.check_block(block);
                Type::Void
            }
        }
    }

    fn check_spawn(&mut self, spawn: &SpawnExpr) -> Type {
        if let Some(info) = self.symbols.lookup_func(&spawn.callee).cloned() {
            self.check_call_argument_count(
                &format!("spawn target `{}`", spawn.callee),
                info.params.len(),
                spawn.args.len(),
                spawn.span,
            );
            self.check_call_args_against_param_infos(&info.param_infos, &spawn.args);
            return Type::Generic(
                "JoinHandle".to_string(),
                vec![function_call_return_type(&info)],
            );
        }

        if let Some(var_info) = self.symbols.lookup_var(&spawn.callee).cloned() {
            if let Type::Fn(params, ret) = var_info.ty {
                self.check_call_arguments(
                    &format!("spawn target `{}`", spawn.callee),
                    &params,
                    &spawn.args,
                    spawn.span,
                );
                return Type::Generic("JoinHandle".to_string(), vec![*ret]);
            }
        }

        for arg in &spawn.args {
            self.check_expr(&arg.expr);
        }
        self.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0804,
                format!("spawn target `{}` is not callable", spawn.callee),
            )
            .with_label(Label::primary(
                spawn.span,
                "not a function or function value",
            ))
            .with_help("spawn a named function call, e.g. `spawn work(10)`"),
        );
        Type::Void
    }

    /// Type-check method calls on `Option<T>` and `Result<T,E>`.
    /// Returns `Some(return_type)` if the call was handled, `None` to fall through.
    fn check_option_result_method_call(
        &mut self,
        obj_ty: &Type,
        call: &MethodCallExpr,
    ) -> Option<Type> {
        match obj_ty {
            Type::Generic(name, args) if name == "Option" => {
                let inner = args.first().cloned().unwrap_or(Type::Void);
                match call.method.as_str() {
                    "is_some" | "is_none" => {
                        if !call.args.is_empty() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!("`Option::{}` takes no arguments", call.method),
                                )
                                .with_label(Label::primary(call.span, "unexpected arguments")),
                            );
                        }
                        Some(Type::Bool)
                    }
                    "unwrap" => {
                        if !call.args.is_empty() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    "`Option::unwrap` takes no arguments",
                                )
                                .with_label(Label::primary(call.span, "unexpected arguments")),
                            );
                        }
                        Some(inner)
                    }
                    "expect" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Option::expect` expects 1 argument (message), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                        } else {
                            let msg_ty = self.check_expr(&call.args[0].expr);
                            if msg_ty != Type::String {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0201,
                                        format!(
                                            "expect message must be `String`, found `{}`",
                                            type_name(&msg_ty)
                                        ),
                                    )
                                    .with_label(
                                        Label::primary(
                                            call.args[0].expr.span(),
                                            "expected `String`",
                                        ),
                                    ),
                                );
                            }
                        }
                        Some(inner)
                    }
                    "unwrap_or" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Option::unwrap_or` expects 1 argument, got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                        } else {
                            let default_ty = self.check_expr(&call.args[0].expr);
                            if !self.types_compatible(&inner, &default_ty) {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0201,
                                        format!(
                                            "mismatched types: expected `{}`, found `{}`",
                                            type_name(&inner),
                                            type_name(&default_ty)
                                        ),
                                    )
                                    .with_label(
                                        Label::primary(
                                            call.args[0].expr.span(),
                                            format!("expected `{}`", type_name(&inner)),
                                        ),
                                    ),
                                );
                            }
                        }
                        Some(inner)
                    }
                    "map" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Option::map` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_expr(&call.args[0].expr);
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.len() == 1 => {
                                    if !self.types_compatible(&inner, &params[0]) {
                                        self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                            format!("`Option::map` closure argument type mismatch: expected `{}`, found `{}`",
                                                type_name(&inner), type_name(&params[0])))
                                            .with_label(Label::primary(call.args[0].expr.span(), "type mismatch")));
                                    }
                                    Some(Type::Generic("Option".to_string(), vec![*ret.clone()]))
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Option::map` expects a function `fn({}) -> U`, found `{}`",
                                            type_name(&inner), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    "and_then" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Option::and_then` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_expr(&call.args[0].expr);
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.len() == 1 => {
                                    if !self.types_compatible(&inner, &params[0]) {
                                        self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                            format!("`Option::and_then` closure argument type mismatch: expected `{}`, found `{}`",
                                                type_name(&inner), type_name(&params[0])))
                                            .with_label(Label::primary(call.args[0].expr.span(), "type mismatch")));
                                    }
                                    Some(*ret.clone())
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Option::and_then` expects a function `fn({}) -> Option<U>`, found `{}`",
                                            type_name(&inner), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    "or_else" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Option::or_else` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_expr(&call.args[0].expr);
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.is_empty() => {
                                    Some(*ret.clone())
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Option::or_else` expects a function `fn() -> Option<{}>`, found `{}`",
                                            type_name(&inner), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    _ => None,
                }
            }
            Type::Generic(name, args) if name == "Result" => {
                let ok_ty = args.first().cloned().unwrap_or(Type::Void);
                let err_ty = args.get(1).cloned().unwrap_or(Type::Void);
                match call.method.as_str() {
                    "is_ok" | "is_err" => {
                        if !call.args.is_empty() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!("`Result::{}` takes no arguments", call.method),
                                )
                                .with_label(Label::primary(call.span, "unexpected arguments")),
                            );
                        }
                        Some(Type::Bool)
                    }
                    "unwrap" => {
                        if !call.args.is_empty() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    "`Result::unwrap` takes no arguments",
                                )
                                .with_label(Label::primary(call.span, "unexpected arguments")),
                            );
                        }
                        Some(ok_ty)
                    }
                    "expect" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Result::expect` expects 1 argument (message), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                        } else {
                            let msg_ty = self.check_expr(&call.args[0].expr);
                            if msg_ty != Type::String {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0201,
                                        format!(
                                            "expect message must be `String`, found `{}`",
                                            type_name(&msg_ty)
                                        ),
                                    )
                                    .with_label(
                                        Label::primary(
                                            call.args[0].expr.span(),
                                            "expected `String`",
                                        ),
                                    ),
                                );
                            }
                        }
                        Some(ok_ty)
                    }
                    "unwrap_or" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Result::unwrap_or` expects 1 argument, got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                        } else {
                            let default_ty = self.check_expr(&call.args[0].expr);
                            if !self.types_compatible(&ok_ty, &default_ty) {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0201,
                                        format!(
                                            "mismatched types: expected `{}`, found `{}`",
                                            type_name(&ok_ty),
                                            type_name(&default_ty)
                                        ),
                                    )
                                    .with_label(
                                        Label::primary(
                                            call.args[0].expr.span(),
                                            format!("expected `{}`", type_name(&ok_ty)),
                                        ),
                                    ),
                                );
                            }
                        }
                        Some(ok_ty)
                    }
                    "unwrap_err" => {
                        if !call.args.is_empty() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    "`Result::unwrap_err` takes no arguments",
                                )
                                .with_label(Label::primary(call.span, "unexpected arguments")),
                            );
                        }
                        Some(err_ty)
                    }
                    "map" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Result::map` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_expr(&call.args[0].expr);
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.len() == 1 => {
                                    if !self.types_compatible(&ok_ty, &params[0]) {
                                        self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                            format!("`Result::map` closure argument type mismatch: expected `{}`, found `{}`",
                                                type_name(&ok_ty), type_name(&params[0])))
                                            .with_label(Label::primary(call.args[0].expr.span(), "type mismatch")));
                                    }
                                    Some(Type::Generic(
                                        "Result".to_string(),
                                        vec![*ret.clone(), err_ty],
                                    ))
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Result::map` expects a function `fn({}) -> U`, found `{}`",
                                            type_name(&ok_ty), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    "map_err" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Result::map_err` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_expr(&call.args[0].expr);
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.len() == 1 => {
                                    if !self.types_compatible(&err_ty, &params[0]) {
                                        self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                            format!("`Result::map_err` closure argument type mismatch: expected `{}`, found `{}`",
                                                type_name(&err_ty), type_name(&params[0])))
                                            .with_label(Label::primary(call.args[0].expr.span(), "type mismatch")));
                                    }
                                    Some(Type::Generic(
                                        "Result".to_string(),
                                        vec![ok_ty, *ret.clone()],
                                    ))
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Result::map_err` expects a function `fn({}) -> F`, found `{}`",
                                            type_name(&err_ty), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    "and_then" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Result::and_then` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_expr(&call.args[0].expr);
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.len() == 1 => {
                                    if !self.types_compatible(&ok_ty, &params[0]) {
                                        self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                            format!("`Result::and_then` closure argument type mismatch: expected `{}`, found `{}`",
                                                type_name(&ok_ty), type_name(&params[0])))
                                            .with_label(Label::primary(call.args[0].expr.span(), "type mismatch")));
                                    }
                                    Some(*ret.clone())
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Result::and_then` expects a function `fn({}) -> Result<U, E>`, found `{}`",
                                            type_name(&ok_ty), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    "or_else" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Result::or_else` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_expr(&call.args[0].expr);
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.len() == 1 => {
                                    if !self.types_compatible(&err_ty, &params[0]) {
                                        self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                            format!("`Result::or_else` closure argument type mismatch: expected `{}`, found `{}`",
                                                type_name(&err_ty), type_name(&params[0])))
                                            .with_label(Label::primary(call.args[0].expr.span(), "type mismatch")));
                                    }
                                    Some(*ret.clone())
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Result::or_else` expects a function `fn({}) -> Result<T, F>`, found `{}`",
                                            type_name(&err_ty), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn check_concurrency_method_call(
        &mut self,
        obj_ty: &Type,
        call: &MethodCallExpr,
    ) -> Option<Type> {
        match call.method.as_str() {
            "join" => {
                if !call.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("join expects 0 arguments, got {}", call.args.len()),
                        )
                        .with_label(Label::primary(call.span, "wrong number of arguments")),
                    );
                }
                match obj_ty {
                    Type::Generic(name, args) if name == "JoinHandle" && args.len() == 1 => {
                        Some(args[0].clone())
                    }
                    _ => {
                        for arg in &call.args {
                            self.check_expr(&arg.expr);
                        }
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0805,
                                format!("cannot call `join` on `{}`", type_name(obj_ty)),
                            )
                            .with_label(Label::primary(call.span, "expected `JoinHandle<T>`")),
                        );
                        Some(Type::Void)
                    }
                }
            }
            "send" => {
                let channel_type = channel_element_type(obj_ty);
                if channel_type.is_none() {
                    for arg in &call.args {
                        self.check_expr(&arg.expr);
                    }
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0806,
                            format!("cannot call `send` on `{}`", type_name(obj_ty)),
                        )
                        .with_label(Label::primary(call.span, "expected `Channel<T>`")),
                    );
                    return Some(Type::Void);
                }
                let element_ty = channel_type.unwrap();
                if call.args.len() != 1 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("send expects 1 argument, got {}", call.args.len()),
                        )
                        .with_label(Label::primary(call.span, "wrong number of arguments")),
                    );
                }
                if let Some(arg) = call.args.first() {
                    let arg_ty = self.check_expr(&arg.expr);
                    if matches!(arg.mode, CallArgMode::Reference { .. }) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1703,
                                "unexpected reference argument",
                            )
                            .with_label(Label::primary(
                                arg.span,
                                format!(
                                    "send expects `{}`, not `& {}`",
                                    type_name(&element_ty),
                                    type_name(&arg_ty)
                                ),
                            )),
                        );
                    } else if !self.types_compatible(&element_ty, &arg_ty) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0802,
                                format!(
                                    "cannot send `{}` into `Channel<{}>`",
                                    type_name(&arg_ty),
                                    type_name(&element_ty)
                                ),
                            )
                            .with_label(Label::primary(
                                arg.expr.span(),
                                format!(
                                    "expected `{}`, found `{}`",
                                    type_name(&element_ty),
                                    type_name(&arg_ty)
                                ),
                            )),
                        );
                    }
                }
                Some(Type::Void)
            }
            "recv" => {
                if !call.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("recv expects 0 arguments, got {}", call.args.len()),
                        )
                        .with_label(Label::primary(call.span, "wrong number of arguments")),
                    );
                }
                match channel_element_type(obj_ty) {
                    Some(element_ty) => Some(element_ty),
                    None => {
                        for arg in &call.args {
                            self.check_expr(&arg.expr);
                        }
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0806,
                                format!("cannot call `recv` on `{}`", type_name(obj_ty)),
                            )
                            .with_label(Label::primary(call.span, "expected `Channel<T>`")),
                        );
                        Some(Type::Void)
                    }
                }
            }
            "close" => {
                if !call.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("close expects 0 arguments, got {}", call.args.len()),
                        )
                        .with_label(Label::primary(call.span, "wrong number of arguments")),
                    );
                }
                if channel_element_type(obj_ty).is_none() {
                    for arg in &call.args {
                        self.check_expr(&arg.expr);
                    }
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0806,
                            format!("cannot call `close` on `{}`", type_name(obj_ty)),
                        )
                        .with_label(Label::primary(call.span, "expected `Channel<T>`")),
                    );
                }
                Some(Type::Void)
            }
            _ => None,
        }
    }

    fn check_call_arguments(
        &mut self,
        callee: &str,
        params: &[Type],
        args: &[CallArg],
        span: Span,
    ) {
        self.check_call_argument_count(callee, params.len(), args.len(), span);
        self.check_value_call_args(params, args);
    }

    fn check_call_argument_count(
        &mut self,
        callee: &str,
        expected: usize,
        supplied: usize,
        span: Span,
    ) {
        if expected != supplied {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "{} takes {} argument(s) but {} were supplied",
                        callee, expected, supplied
                    ),
                )
                .with_label(Label::primary(span, "wrong number of arguments")),
            );
        }
    }

    fn check_value_call_args(&mut self, params: &[Type], args: &[CallArg]) {
        let param_infos = value_param_infos(params);
        self.check_call_args_against_param_infos(&param_infos, args);
    }

    fn check_call_args_against_param_infos(&mut self, params: &[ParamInfo], args: &[CallArg]) {
        for (param, arg) in params.iter().zip(args) {
            self.check_call_arg_against_param(param, arg);
        }
        self.check_mut_reference_aliases(params, args);
    }

    fn check_call_arg_against_param(&mut self, param: &ParamInfo, arg: &CallArg) {
        match (&param.mode, &arg.mode) {
            (ParamMode::Value, CallArgMode::Value) => {
                self.check_value_arg_type(&param.ty, arg);
            }
            (ParamMode::Value, CallArgMode::Reference { .. }) => {
                let arg_ty = self.check_expr(&arg.expr);
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1703,
                        "unexpected reference argument",
                    )
                    .with_label(Label::primary(
                        arg.span,
                        format!(
                            "parameter expects `{}`, not `& {}`",
                            type_name(&param.ty),
                            type_name(&arg_ty)
                        ),
                    )),
                );
            }
            (ParamMode::Reference { .. }, CallArgMode::Value) => {
                self.check_expr(&arg.expr);
                let expr_span = arg.expr.span();
                let mut diagnostic = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E1702,
                    "expected reference argument for reference parameter",
                )
                .with_label(Label::primary(
                    expr_span,
                    "expected `&` before this argument",
                ))
                .with_help("pass the mutable place by reference");

                if let Expr::Var(name, _) = &arg.expr {
                    diagnostic = diagnostic.with_help(format!("write `&{}`", name));
                    diagnostic = diagnostic.with_fix(FixSuggestion::insertion(
                        Span::new(
                            expr_span.start,
                            expr_span.start,
                            expr_span.line,
                            expr_span.col,
                        ),
                        "&",
                        "pass the variable by reference",
                    ));
                }

                self.push(diagnostic);
            }
            (ParamMode::Reference { mutable, .. }, CallArgMode::Reference { .. }) => {
                self.check_reference_argument(param, arg, *mutable);
            }
        }
    }

    fn check_value_arg_type(&mut self, param_ty: &Type, arg: &CallArg) {
        let arg_ty = self.check_expr(&arg.expr);
        if !self.types_compatible(param_ty, &arg_ty) {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    self.type_mismatch_error_code(param_ty, &arg_ty),
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

    fn check_reference_argument(
        &mut self,
        param: &ParamInfo,
        arg: &CallArg,
        require_mutable: bool,
    ) {
        let Some(place) = self.reference_place_info(&arg.expr, arg.span) else {
            return;
        };

        if require_mutable && !place.mutable {
            let mut diagnostic = Diagnostic::new(
                Severity::Error,
                ErrorCode::E1701,
                format!("cannot pass immutable variable `{}` as `&mut`", place.name),
            )
            .with_label(Label::primary(
                arg.span,
                "cannot pass immutable variable by mutable reference",
            ))
            .with_label(Label::secondary(
                place.declaration_span,
                "declared immutable here",
            ))
            .with_help("declare the variable as mutable");

            if !place.is_param {
                let decl = place.declaration_span;
                let insert_span =
                    Span::new(decl.start + 4, decl.start + 4, decl.line, decl.col + 4);
                diagnostic = diagnostic.with_fix(FixSuggestion::insertion(
                    insert_span,
                    "mut ",
                    "add `mut` here",
                ));
            }

            self.push(diagnostic);
        }

        if place.ty != param.ty {
            let mut diagnostic = Diagnostic::new(
                Severity::Error,
                ErrorCode::E1705,
                "reference argument type mismatch",
            )
            .with_label(Label::primary(
                arg.span,
                format!("found `{}`", type_name(&place.ty)),
            ));

            if param.type_span != Span::dummy() {
                diagnostic = diagnostic.with_label(Label::secondary(
                    param.type_span,
                    format!("expected `{}`", type_name(&param.ty)),
                ));
            } else {
                diagnostic = diagnostic.with_label(Label::secondary(
                    param.span,
                    format!("expected `{}`", type_name(&param.ty)),
                ));
            }

            self.push(diagnostic);
        }
    }

    fn reference_place_info(&mut self, expr: &Expr, arg_span: Span) -> Option<ReferencePlaceInfo> {
        match expr {
            Expr::Var(name, _) => {
                let Some(var_info) = self.symbols.lookup_var(name).cloned() else {
                    self.check_expr(expr);
                    return None;
                };
                Some(ReferencePlaceInfo {
                    name: name.clone(),
                    ty: var_info.ty,
                    mutable: var_info.mutable,
                    is_param: var_info.is_param,
                    declaration_span: var_info.declaration_span,
                })
            }
            Expr::FieldAccess(obj, field_name, span) => {
                let obj_ty = self.check_expr(obj);
                let field_ty = self.resolve_field(&obj_ty, field_name, *span, true);
                if matches!(field_ty, Type::Void) {
                    return None;
                }
                Some(ReferencePlaceInfo {
                    name: reference_place_key(expr).unwrap_or_else(|| field_name.clone()),
                    ty: field_ty,
                    mutable: true,
                    is_param: false,
                    declaration_span: *span,
                })
            }
            Expr::Index(array, index, span) => {
                let elem_ty = self.check_index(array, index, *span);
                if matches!(elem_ty, Type::Void) {
                    return None;
                }
                Some(ReferencePlaceInfo {
                    name: reference_place_key(expr).unwrap_or_else(|| "array element".to_string()),
                    ty: elem_ty,
                    mutable: true,
                    is_param: false,
                    declaration_span: *span,
                })
            }
            _ => {
                self.check_expr(expr);
                let mut diagnostic = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E1704,
                    "cannot pass non-place expression by reference",
                )
                .with_label(Label::primary(arg_span, "not an assignable place"));

                if matches!(expr, Expr::Call(_)) {
                    diagnostic = diagnostic.with_help("function call results are temporaries");
                }

                self.push(diagnostic);
                None
            }
        }
    }

    fn check_mut_reference_aliases(&mut self, params: &[ParamInfo], args: &[CallArg]) {
        let mut seen_mut_refs: Vec<(String, Span)> = Vec::new();
        let mut seen_other_uses: Vec<(String, Span)> = Vec::new();

        for (param, arg) in params.iter().zip(args) {
            let Some(name) = reference_place_key(&arg.expr) else {
                continue;
            };
            let is_mut_reference = matches!(
                (&param.mode, &arg.mode),
                (
                    ParamMode::Reference { mutable: true, .. },
                    CallArgMode::Reference { .. }
                )
            );

            if is_mut_reference {
                for (previous_name, previous_span) in &seen_mut_refs {
                    if previous_name == &name {
                        self.push_mut_reference_alias_diagnostic(
                            &name,
                            arg.span,
                            *previous_span,
                            "same mutable place passed here",
                        );
                    }
                }
                for (previous_name, previous_span) in &seen_other_uses {
                    if previous_name == &name {
                        self.push_mut_reference_alias_diagnostic(
                            &name,
                            arg.span,
                            *previous_span,
                            "same place used by another argument",
                        );
                    }
                }
                seen_mut_refs.push((name, arg.span));
            } else {
                for (previous_name, previous_span) in &seen_mut_refs {
                    if previous_name == &name {
                        self.push_mut_reference_alias_diagnostic(
                            &name,
                            arg.span,
                            *previous_span,
                            "mutable reference passed here",
                        );
                    }
                }
                seen_other_uses.push((name, arg.span));
            }
        }
    }

    fn push_mut_reference_alias_diagnostic(
        &mut self,
        name: &str,
        current_span: Span,
        previous_span: Span,
        previous_label: &'static str,
    ) {
        self.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E1706,
                format!(
                    "cannot pass `{}` while it aliases a mutable reference",
                    name
                ),
            )
            .with_label(Label::primary(
                current_span,
                "same place aliases a mutable reference argument",
            ))
            .with_label(Label::secondary(previous_span, previous_label))
            .with_help("pass distinct mutable locals or split the call into separate steps"),
        );
    }

    fn check_format_call(&mut self, c: &CallExpr) -> Type {
        if c.args.len() != 2 {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!("format expects 2 arguments, got {}", c.args.len()),
                )
                .with_label(Label::primary(c.span, "wrong number of arguments")),
            );
            for arg in &c.args {
                self.check_expr(&arg.expr);
            }
            return Type::String;
        }

        match &c.args[0].expr {
            Expr::String(spec, span) if is_supported_f64_format(spec) => {
                let _ = span;
            }
            Expr::String(spec, span) => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1401,
                        format!("invalid format specifier `{}`", spec),
                    )
                    .with_label(Label::primary(*span, "unsupported format specifier"))
                    .with_help("supported f64 formats are `{:.17g}`, `{:.16f}`, and `{:.6f}`"),
                );
            }
            other => {
                self.check_expr(other);
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1401,
                        "format specifier must be a string literal",
                    )
                    .with_label(Label::primary(other.span(), "expected string literal"))
                    .with_help("write the format as a literal, e.g. `format(\"{:.6f}\", value)`"),
                );
            }
        }

        let value_ty = self.check_expr(&c.args[1].expr);
        if value_ty != Type::F64 {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "mismatched types: expected `f64`, found `{}`",
                        type_name(&value_ty)
                    ),
                )
                .with_label(Label::primary(c.args[1].expr.span(), "expected `f64`")),
            );
        }

        Type::String
    }

    fn check_object_literal(&mut self, literal: &ObjectLiteralExpr) -> Type {
        // Reject constructing a private module class from another module.
        self.check_type_visibility(&literal.class, literal.span);
        if self.symbols.lookup_class(&literal.class).is_none() {
            for field in &literal.fields {
                self.check_expr(&field.value);
            }
            // An interface has no object layout and cannot be instantiated.
            if self.symbols.lookup_interface(&literal.class).is_some() {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0413,
                        format!("cannot instantiate interface `{}`", literal.class),
                    )
                    .with_label(Label::primary(
                        literal.span,
                        "interfaces have no constructor",
                    ))
                    .with_help("instantiate a class that implements this interface instead"),
                );
                return Type::Void;
            }
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0350,
                    format!("class `{}` not found", literal.class),
                )
                .with_label(Label::primary(literal.span, "unknown class")),
            );
            return Type::Void;
        }

        // Collect all fields reachable through the inheritance hierarchy.
        // Child classes must supply all fields: their own AND those inherited from base classes.
        let all_fields = self.collect_all_fields_in_hierarchy(&literal.class);

        let mut seen = HashSet::new();
        for field in &literal.fields {
            let value_ty = self.check_expr(&field.value);
            if !seen.insert(field.name.clone()) {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0502,
                        format!("field `{}` is initialized more than once", field.name),
                    )
                    .with_label(Label::primary(field.span, "duplicate field initializer")),
                );
                continue;
            }

            match all_fields.get(&field.name) {
                Some(info) => {
                    if !self.types_compatible(&info.ty, &value_ty) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                self.type_mismatch_error_code(&info.ty, &value_ty),
                                format!(
                                    "field `{}` expects `{}`, found `{}`",
                                    field.name,
                                    type_name(&info.ty),
                                    type_name(&value_ty)
                                ),
                            )
                            .with_label(Label::primary(
                                field.value.span(),
                                format!("expected `{}`", type_name(&info.ty)),
                            ))
                            .with_label(Label::secondary(
                                info.declaration_span,
                                "field declared here",
                            )),
                        );
                    }
                }
                None => {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0502,
                            format!("no field `{}` on class `{}`", field.name, literal.class),
                        )
                        .with_label(Label::primary(field.span, "unknown field")),
                    );
                }
            }
        }

        for (field_name, field_info) in &all_fields {
            if !seen.contains(field_name) {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0502,
                        format!(
                            "missing field `{}` in `{}` literal",
                            field_name, literal.class
                        ),
                    )
                    .with_label(Label::primary(literal.span, "missing field initializer"))
                    .with_label(Label::secondary(
                        field_info.declaration_span,
                        "field declared here",
                    )),
                );
            }
        }

        Type::Named(literal.class.clone())
    }

    /// Collect all fields visible in a class including those inherited from base classes.
    /// Fields in derived classes shadow base-class fields of the same name.
    fn collect_all_fields_in_hierarchy(&self, class_name: &str) -> HashMap<String, FieldInfo> {
        let mut result: HashMap<String, FieldInfo> = HashMap::new();
        let mut chain: Vec<String> = Vec::new();
        let mut current = Some(class_name.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                break;
            }
            chain.push(name.clone());
            current = self
                .symbols
                .lookup_class(&name)
                .and_then(|c| c.base_class.clone());
        }
        // Walk from root to leaf so that derived-class fields shadow base-class fields.
        for class_name in chain.iter().rev() {
            if let Some(class) = self.symbols.lookup_class(class_name) {
                for (name, info) in &class.fields {
                    result.insert(name.clone(), info.clone());
                }
            }
        }
        result
    }

    fn check_binary(&mut self, b: &BinaryExpr) -> Type {
        let lty = self.check_expr(&b.lhs);
        let rty = self.check_expr(&b.rhs);

        match &b.op {
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                if b.op == BinOp::Add && lty == Type::String && rty == Type::String {
                    return Type::String;
                }

                // String concatenation is strongly typed: `String + non-String`
                // (or the reverse) is rejected with a `toString()` suggestion
                // rather than an implicit stringify (willow-fvfc).
                if b.op == BinOp::Add && (lty == Type::String || rty == Type::String) {
                    let (non_str, side) = if lty == Type::String {
                        (&rty, "right")
                    } else {
                        (&lty, "left")
                    };
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0202,
                            format!(
                                "cannot concatenate `String` with `{}`",
                                type_name(non_str)
                            ),
                        )
                        .with_label(Label::primary(
                            b.span,
                            format!("the {side} operand is `{}`, not `String`", type_name(non_str)),
                        ))
                        .with_help(
                            "convert explicitly with `.toString()`, e.g. `\"x = \" + value.toString()`",
                        ),
                    );
                    return Type::String;
                }

                if (lty != Type::I64 && lty != Type::F64) || lty != rty {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0202,
                            format!(
                                "cannot apply operator `{}` to `{}` and `{}`",
                                b.op.symbol(),
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )
                        .with_label(Label::primary(
                            b.span,
                            format!(
                                "`{}` not defined for `{}` and `{}`",
                                b.op.symbol(),
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )),
                    );
                }
                lty
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                if (lty != Type::I64 && lty != Type::F64) || lty != rty {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0202,
                            format!(
                                "cannot compare `{}` and `{}`",
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )
                        .with_label(Label::primary(
                            b.span,
                            format!(
                                "comparison not defined for `{}` and `{}`",
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )),
                    );
                }
                Type::Bool
            }
            BinOp::Eq | BinOp::Ne => {
                if lty == Type::Nil || rty == Type::Nil {
                    self.check_nil_comparison(&lty, &rty, b.span);
                    return Type::Bool;
                }

                if !self.types_compatible(&lty, &rty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "mismatched types: `{}` and `{}`",
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )
                        .with_label(Label::primary(
                            b.span,
                            format!(
                                "cannot compare `{}` and `{}`",
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )),
                    );
                }
                Type::Bool
            }
            BinOp::And | BinOp::Or => {
                if lty != Type::Bool || rty != Type::Bool {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0202,
                            format!(
                                "logical operator requires `bool` operands, found `{}` and `{}`",
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )
                        .with_label(Label::primary(b.span, "operands must be `bool`")),
                    );
                }
                Type::Bool
            }
        }
    }

    fn check_unary(&mut self, u: &UnaryExpr) -> Type {
        let ty = self.check_expr(&u.expr);
        match &u.op {
            UnaryOp::Neg => {
                if ty != Type::I64 && ty != Type::F64 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0202,
                            format!("unary `-` cannot be applied to `{}`", type_name(&ty)),
                        )
                        .with_label(Label::primary(
                            u.span,
                            format!("requires `i64` or `f64`, found `{}`", type_name(&ty)),
                        )),
                    );
                }
                ty
            }
            UnaryOp::Not => {
                if ty != Type::Bool {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0202,
                            format!("unary `!` cannot be applied to `{}`", type_name(&ty)),
                        )
                        .with_label(Label::primary(
                            u.span,
                            format!("requires `bool`, found `{}`", type_name(&ty)),
                        )),
                    );
                }
                Type::Bool
            }
        }
    }

    fn resolve_field(
        &mut self,
        obj_ty: &Type,
        field_name: &str,
        span: Span,
        check_visibility: bool,
    ) -> Type {
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

    fn resolve_method(
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
        if let Type::Generic(name, type_args) = obj_ty {
            if let Some(iface) = self.symbols.lookup_interface(name).cloned() {
                let instantiated = self.instantiate_interface(&iface, type_args, name);
                return self.resolve_interface_method(&instantiated, method_name, args, span);
            }
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
                if !mi.has_self {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "method `{}` of class `{}` is static; call it with `::`",
                                method_name, owner
                            ),
                        )
                        .with_label(Label::primary(span, "static method called with `.`"))
                        .with_help(format!("write `{}::{}` instead", owner, method_name)),
                    );
                    return mi.return_type.clone();
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
                mi.return_type.clone()
            }
        }
    }

    /// Resolve a method call on an interface-typed receiver. Only methods declared
    /// by the interface are callable; the return type is the interface method's.
    fn resolve_interface_method(
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
            if let Some(param_ty) = m.params.get(idx) {
                if !self.types_compatible(param_ty, &arg_ty) {
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
        }
        m.return_type.clone()
    }

    fn resolve_static_call(
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
        if let Some(enum_info) = self.symbols.lookup_enum(class_name).cloned() {
            if !enum_info.type_params.is_empty() {
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
                    let valid: Vec<&str> =
                        enum_info.variants.iter().map(|v| v.name.as_str()).collect();
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
                    fi.return_type.clone()
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
                if mi.has_self {
                    let mut diagnostic = Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "method `{}` of class `{}` is an instance method",
                            method_name, class_name
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
                    return mi.return_type.clone();
                }
                if !mi.public {
                    if mi.protected {
                        if !self.can_access_protected_member(&class_name) {
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
                mi.return_type.clone()
            }
        }
    }

    fn resolve_fully_qualified_std_module_call(
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
                if !matches!(item.as_str(), "print" | "println" | "eprintln") {
                    return None;
                }
                if args.len() != 1 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "function `std::io::{item}` expects 1 argument, got {}",
                                args.len()
                            ),
                        )
                        .with_label(Label::primary(span, "wrong number of arguments")),
                    );
                }
                for arg in args {
                    self.check_expr(&arg.expr);
                }
                Some(Type::Void)
            }
            Err(diag) => {
                self.push(diag);
                Some(Type::Void)
            }
            _ => None,
        }
    }

    fn resolve_static_call_class_name(&mut self, class_name: &str, span: Span) -> Option<String> {
        if class_name != "Self" {
            if let Some(item) = self.imported_collection_aliases.get(class_name).cloned() {
                return Some(item);
            }
            if let Some((module, item)) = self.resolve_fully_qualified_std_item(class_name, span) {
                if module == "collections" {
                    self.fully_qualified_collection_types.insert(item.clone());
                }
                return match (module.as_str(), item.as_str()) {
                    ("collections", "Array" | "Map")
                    | ("option", "Option")
                    | ("result", "Result") => Some(item),
                    _ => Some(format!("{module}::{item}")),
                };
            }
            if let Some((module, item)) = self.resolve_imported_std_module_item(class_name, span) {
                return match (module.as_str(), item.as_str()) {
                    ("collections", "Array" | "Map") => Some(item),
                    _ => Some(format!("{module}::{item}")),
                };
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

    fn lookup_field_in_hierarchy(
        &self,
        class_name: &str,
        field_name: &str,
    ) -> Option<(String, FieldInfo)> {
        let mut current = Some(class_name.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                return None;
            }
            let class = self.symbols.lookup_class(&name)?;
            if let Some(field) = class.fields.get(field_name) {
                return Some((name, field.clone()));
            }
            current = class.base_class.clone();
        }
        None
    }

    fn lookup_method_in_hierarchy(
        &self,
        class_name: &str,
        method_name: &str,
    ) -> Option<(String, MethodInfo)> {
        let mut current = Some(class_name.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                return None;
            }
            let class = self.symbols.lookup_class(&name)?;
            if let Some(method) = class.methods.get(method_name) {
                return Some((name, method.clone()));
            }
            current = class.base_class.clone();
        }
        None
    }

    fn lookup_method_in_ancestors(
        &self,
        base_class_name: &str,
        method_name: &str,
    ) -> Option<(String, MethodInfo)> {
        self.lookup_method_in_hierarchy(base_class_name, method_name)
    }

    fn method_names_in_hierarchy(&self, class_name: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut current = Some(class_name.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                break;
            }
            let Some(class) = self.symbols.lookup_class(&name) else {
                break;
            };
            names.extend(class.methods.keys().cloned());
            current = class.base_class.clone();
        }
        names
    }

    fn check_block_with_narrowing(&mut self, block: &Block, narrowing: &NilCheckNarrowing) {
        self.narrowed_vars.push(HashMap::new());
        self.add_narrowing_to_current_scope(narrowing);
        self.check_block(block);
        self.narrowed_vars.pop();
    }

    fn add_narrowing_to_current_scope(&mut self, narrowing: &NilCheckNarrowing) {
        if let Some(scope) = self.narrowed_vars.last_mut() {
            scope.insert(
                narrowing.name.clone(),
                NarrowedVar {
                    ty: narrowing.narrowed_ty.clone(),
                    declaration_span: narrowing.declaration_span,
                },
            );
        }
    }

    fn clear_narrowing(&mut self, name: &str) {
        let Some(declaration_span) = self
            .symbols
            .lookup_var(name)
            .map(|info| info.declaration_span)
        else {
            return;
        };

        for scope in &mut self.narrowed_vars {
            if matches!(scope.get(name), Some(n) if n.declaration_span == declaration_span) {
                scope.remove(name);
            }
        }
    }

    fn lookup_narrowed_type(&self, name: &str) -> Option<Type> {
        let declaration_span = self.symbols.lookup_var(name)?.declaration_span;
        for scope in self.narrowed_vars.iter().rev() {
            if let Some(narrowed) = scope.get(name) {
                if narrowed.declaration_span == declaration_span {
                    return Some(narrowed.ty.clone());
                }
            }
        }
        None
    }

    fn nil_check_narrowing(&self, expr: &Expr) -> Option<NilCheckNarrowing> {
        let Expr::Binary(binary) = expr else {
            return None;
        };
        let non_nil_when_true = match binary.op {
            BinOp::Eq => false,
            BinOp::Ne => true,
            _ => return None,
        };
        let name = self.var_name_compared_with_nil(&binary.lhs, &binary.rhs)?;
        let info = self.symbols.lookup_var(name)?;
        let Type::Nullable(inner) = &info.ty else {
            return None;
        };
        Some(NilCheckNarrowing {
            name: name.to_string(),
            narrowed_ty: inner.as_ref().clone(),
            declaration_span: info.declaration_span,
            non_nil_when_true,
        })
    }

    fn var_name_compared_with_nil<'a>(&self, lhs: &'a Expr, rhs: &'a Expr) -> Option<&'a str> {
        match (lhs, rhs) {
            (Expr::Var(name, _), Expr::Nil(_)) | (Expr::Nil(_), Expr::Var(name, _)) => {
                Some(name.as_str())
            }
            _ => None,
        }
    }

    fn unify_ternary_types(&self, then_ty: &Type, else_ty: &Type) -> Option<Type> {
        if then_ty == else_ty {
            return Some(then_ty.clone());
        }

        match (then_ty, else_ty) {
            (Type::Nil, Type::Nil) => None,
            (Type::Nullable(_), Type::Nil) => Some(then_ty.clone()),
            (Type::Nil, Type::Nullable(_)) => Some(else_ty.clone()),
            (Type::Nil, other) => Some(Type::Nullable(Box::new(other.clone()))),
            (other, Type::Nil) => Some(Type::Nullable(Box::new(other.clone()))),
            (Type::Nullable(inner), other) if self.types_compatible(inner, other) => {
                Some(then_ty.clone())
            }
            (other, Type::Nullable(inner)) if self.types_compatible(inner, other) => {
                Some(else_ty.clone())
            }
            _ if self.types_compatible(then_ty, else_ty) => Some(then_ty.clone()),
            _ if self.types_compatible(else_ty, then_ty) => Some(else_ty.clone()),
            _ => None,
        }
    }

    fn check_nil_comparison(&mut self, lty: &Type, rty: &Type, span: Span) {
        match (lty, rty) {
            (Type::Nullable(_), Type::Nil) | (Type::Nil, Type::Nullable(_)) => {}
            (Type::Nil, Type::Nil) => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        "cannot compare `nil` with `nil` without a nullable type context",
                    )
                    .with_label(Label::primary(span, "both sides are `nil`"))
                    .with_help("compare a nullable value with `nil` instead"),
                );
            }
            (Type::Nil, other) | (other, Type::Nil) => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "cannot compare non-nullable type `{}` with `nil`",
                            type_name(other)
                        ),
                    )
                    .with_label(Label::primary(
                        span,
                        "only nullable values can be compared with `nil`",
                    ))
                    .with_help("make the value nullable with `?` or remove the `nil` comparison"),
                );
            }
            _ => {}
        }
    }

    fn validate_type(&mut self, ty: &Type, span: Span) {
        match ty {
            Type::Nullable(inner) => {
                if !nullable_inner_has_pointer_representation(inner) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            "nullable primitive types are not implemented yet",
                        )
                        .with_label(Label::primary(
                            span,
                            format!("cannot lower `{}` yet", type_name(ty)),
                        ))
                        .with_help("use a wrapper class or avoid nullable primitive types for now"),
                    );
                }
                self.validate_type(inner, span);
            }
            Type::Array(element) => {
                self.check_collection_type_imported("Array", span);
                self.validate_type(element, span);
            }
            Type::Generic(name, args) => {
                if name == "Map" {
                    self.check_collection_type_imported("Map", span);
                }
                for arg in args {
                    self.validate_type(arg, span);
                }
            }
            Type::Fn(params, ret) => {
                for param in params {
                    self.validate_type(param, span);
                }
                self.validate_type(ret, span);
            }
            Type::I64
            | Type::F64
            | Type::Bool
            | Type::String
            | Type::Void
            | Type::Nil
            | Type::Never => {}
            Type::Named(name) => {
                // A named type must resolve to a known class or enum (including
                // module-qualified ones like `geometry::Point`, which are
                // registered under that key). Reject unknown names and module
                // names used as a type.
                if self.symbols.lookup_class(name).is_none()
                    && self.symbols.lookup_enum(name).is_none()
                    && self.symbols.lookup_interface(name).is_none()
                {
                    let diag = if self.symbols.lookup_module(name).is_some() {
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0350,
                            format!("`{name}` is a module, not a type"),
                        )
                        .with_label(Label::primary(span, "module used as a type"))
                        .with_help(format!(
                            "a module is a namespace; import a type from it or write `{name}::TypeName`"
                        ))
                    } else {
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0350,
                            format!("cannot find type `{name}`"),
                        )
                        .with_label(Label::primary(span, "not a known type"))
                        .with_help("define a class or enum with this name, or check the spelling")
                    };
                    self.push(diag);
                }
                self.check_type_visibility(name, span);
            }
        }
    }

    /// Reject a module-qualified reference to a non-`pub` type (class, interface,
    /// or enum) from another module (willow-7ihl). A module-qualified name
    /// contains `::`; same-module references are unqualified and never checked.
    fn check_type_visibility(&mut self, name: &str, span: Span) {
        if !name.contains("::") {
            return;
        }
        let (is_private, kind) = if let Some(c) = self.symbols.lookup_class(name) {
            (!c.public, "class")
        } else if let Some(i) = self.symbols.lookup_interface(name) {
            (!i.public, "interface")
        } else if let Some(e) = self.symbols.lookup_enum(name) {
            (!e.public, "enum")
        } else {
            return;
        };
        if is_private {
            let simple = name.rsplit("::").next().unwrap_or(name);
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0419,
                    format!("{kind} `{name}` is private to its module"),
                )
                .with_label(Label::primary(
                    span,
                    "private type accessed from another module",
                ))
                .with_help(format!(
                    "mark it `pub {kind} {simple}` to use it outside its module"
                )),
            );
        }
    }

    /// Resolve the concrete payload types for a variant of a generic enum.
    /// Uses the type arguments from the scrutinee type to instantiate the enum.
    fn resolve_generic_variant_payload(
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

    /// When a `let` has no type annotation, a bare `Option`/`Result` variant
    /// constructor whose type parameters could not be inferred is represented
    /// with `Type::Void` placeholders (e.g. `Option::None` → `Option<Void>`,
    /// `Result::Ok(10)` → `Result<i64, Void>`). Such an unresolved type cannot
    /// be the final type of a binding, so report the spec's E1801/E1803
    /// diagnostics.
    ///
    /// This is gated on `init` being a direct `Option`/`Result` variant
    /// construction: a `Void` placeholder reaching the binding through a method
    /// chain (e.g. `r.and_then(|v| Result::Ok(v))`) is benign — the error type
    /// is simply never observed — and must not be reported. Returns `None` for
    /// fully-resolved types, non-constructor initializers, and other types.
    fn unresolved_generic_enum_diagnostic(
        &self,
        init: &Expr,
        ty: &Type,
        span: Span,
        var: &str,
    ) -> Option<Diagnostic> {
        // Only a bare `Option::`/`Result::` variant constructor triggers this.
        let is_variant_ctor = matches!(
            init,
            Expr::StaticCall(s)
                if (s.class == "Option" && (s.method == "None" || s.method == "Some"))
                    || (s.class == "Result" && (s.method == "Ok" || s.method == "Err"))
        );
        if !is_variant_ctor {
            return None;
        }
        let Type::Generic(name, args) = ty else {
            return None;
        };
        if !args.contains(&Type::Void) {
            return None; // every type parameter is resolved
        }
        let var_label = if var == "_" { "x" } else { var };
        let (code, message, label, hint) = match (name.as_str(), args.as_slice()) {
            ("Option", _) => (
                ErrorCode::E1801,
                "cannot infer type parameter `T` for `Option::None`".to_string(),
                "type annotation required",
                format!("add a type annotation, e.g. `let {var_label}: Option<i64> = ...;`"),
            ),
            ("Result", [ok, Type::Void]) if *ok != Type::Void => (
                ErrorCode::E1803,
                "cannot infer error type `E` for `Result::Ok`".to_string(),
                "error type is unknown",
                format!(
                    "add a type annotation, e.g. `let {var_label}: Result<i64, String> = ...;`"
                ),
            ),
            ("Result", [Type::Void, err]) if *err != Type::Void => (
                ErrorCode::E1803,
                "cannot infer success type `T` for `Result::Err`".to_string(),
                "success type is unknown",
                format!(
                    "add a type annotation, e.g. `let {var_label}: Result<i64, String> = ...;`"
                ),
            ),
            ("Result", _) => (
                ErrorCode::E1803,
                "cannot infer type parameters `T` and `E` for `Result`".to_string(),
                "type annotation required",
                format!(
                    "add a type annotation, e.g. `let {var_label}: Result<i64, String> = ...;`"
                ),
            ),
            // Other generic enums are out of scope for E1801/E1803; leave their
            // inference behavior unchanged.
            _ => return None,
        };
        Some(
            Diagnostic::new(Severity::Error, code, message)
                .with_label(Label::primary(span, label))
                .with_help(hint),
        )
    }

    fn types_compatible(&self, expected: &Type, actual: &Type) -> bool {
        expected == actual
            || matches!(
                (expected, actual),
                (Type::Nullable(_), Type::Nil) | (Type::Nil, Type::Nullable(_))
            )
            // A Void-placeholder generic (e.g. Option<Void> from None) matches any
            // concrete instantiation of the same generic enum.
            || matches!((expected, actual),
                (Type::Generic(en, _), Type::Generic(an, args))
                    if en == an && args.iter().all(|a| *a == Type::Void)
                        && self.symbols.lookup_enum(en).map(|e| !e.type_params.is_empty()).unwrap_or(false))
            // Result::Ok(v) produces Result<T,Void>; Result::Err(e) → Result<Void,E>
            // Accept if the non-Void type parameters match
            || self.generic_partially_matches(expected, actual)
            // An empty array literal `[]` produces `Array<Void>`, an unresolved
            // element type that a concrete `Array<T>` annotation resolves.
            || matches!((expected, actual),
                (Type::Array(e), Type::Array(a)) if **e == Type::Void || **a == Type::Void)
            // `Map::new()` produces `Map<Void, Void>`, resolved by the annotation.
            || matches!((expected, actual),
                (Type::Generic(en, eargs), Type::Generic(an, aargs))
                    if en == "Map" && an == "Map" && eargs.len() == 2 && aargs.len() == 2
                        && aargs.iter().all(|a| *a == Type::Void))
            || self.is_subtype(actual, expected)
    }

    /// Allow `GenericEnum<Void, ...>` to match `GenericEnum<T, ...>` when
    /// Void is used as a placeholder for an unresolved type parameter.
    /// Only applied to generic enums registered in the symbol table (e.g. Option, Result).
    /// NOT applied to built-in non-enum generics like Channel, Future, JoinHandle.
    fn generic_partially_matches(&self, expected: &Type, actual: &Type) -> bool {
        match (expected, actual) {
            (Type::Generic(en, eargs), Type::Generic(an, aargs)) if en == an => {
                // Only apply to registered generic enums (not Channel/Future/JoinHandle)
                let is_enum = self
                    .symbols
                    .lookup_enum(en)
                    .map(|e| !e.type_params.is_empty())
                    .unwrap_or(false);
                is_enum
                    && eargs.len() == aargs.len()
                    && eargs
                        .iter()
                        .zip(aargs.iter())
                        .all(|(e, a)| e == a || *e == Type::Void || *a == Type::Void)
            }
            _ => false,
        }
    }

    fn is_subtype(&self, actual: &Type, expected: &Type) -> bool {
        match (actual, expected) {
            (Type::Named(child), Type::Named(parent)) => {
                // A class is a subtype of its base class, and of any interface it
                // implements (directly or through an ancestor); an interface is a
                // subtype of any interface it transitively extends (willow-1js.2).
                self.class_extends(child, parent)
                    || self.class_implements_interface(child, expected)
                    || self.interface_extends(child, parent)
            }
            // A class is a subtype of a generic interface instantiation it
            // implements, e.g. `Dog` <: `Box<String>` (willow-1js.1).
            (Type::Named(child), Type::Generic(_, _)) => {
                self.class_implements_interface(child, expected)
            }
            (Type::Nullable(actual_inner), Type::Nullable(expected_inner)) => {
                self.is_subtype(actual_inner, expected_inner)
            }
            // General T → T?: any non-nullable, non-nil value is compatible with T?
            // when the value's type is compatible with the inner type T.
            (actual, Type::Nullable(expected_inner))
                if !matches!(actual, Type::Nullable(_) | Type::Nil) =>
            {
                self.types_compatible(expected_inner, actual)
            }
            _ => false,
        }
    }

    /// True when `child` is an interface that transitively extends interface
    /// `parent` (willow-1js.2).
    fn interface_extends(&self, child: &str, parent: &str) -> bool {
        if self.symbols.lookup_interface(child).is_none() {
            return false;
        }
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
                if sup == parent {
                    return true;
                }
                stack.push(sup.clone());
            }
        }
        false
    }

    /// True when error type `e1` can be converted to `e2` for `?` automatic
    /// error conversion: `e1` is a concrete class implementing `Into<e2>`
    /// (willow-1ow).
    fn err_converts_via_into(&self, e1: &Type, e2: &Type) -> bool {
        let Type::Named(e1_name) = e1 else {
            return false;
        };
        self.class_implements_interface(
            e1_name,
            &Type::Generic("Into".to_string(), vec![e2.clone()]),
        )
    }

    /// True when `class` (or one of its ancestors) declares `implements target`,
    /// where `target` is the (possibly generic) interface type. `target` must
    /// name a registered interface, and generic instantiations must match
    /// exactly (e.g. `Box<String>` != `Box<i64>`).
    fn class_implements_interface(&self, class: &str, target: &Type) -> bool {
        let target_name = match target {
            Type::Named(n) | Type::Generic(n, _) => n,
            _ => return false,
        };
        if self.symbols.lookup_interface(target_name).is_none() {
            return false;
        }
        let mut current = Some(class.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                return false;
            }
            let Some(info) = self.symbols.lookup_class(&name) else {
                return false;
            };
            if info.implements.iter().any(|i| i == target) {
                return true;
            }
            current = info.base_class.clone();
        }
        false
    }

    fn class_extends(&self, child: &str, parent: &str) -> bool {
        if child == parent {
            return true;
        }
        let mut current = Some(child.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                return false;
            }
            let Some(class) = self.symbols.lookup_class(&name) else {
                return false;
            };
            let Some(base) = &class.base_class else {
                return false;
            };
            if base == parent {
                return true;
            }
            current = Some(base.clone());
        }
        false
    }

    fn type_mismatch_error_code(&self, expected: &Type, actual: &Type) -> ErrorCode {
        if self.is_class_type(expected) && self.is_class_type(actual) {
            ErrorCode::E0704
        } else {
            ErrorCode::E0201
        }
    }

    fn is_class_type(&self, ty: &Type) -> bool {
        match ty {
            Type::Named(name) => self.symbols.lookup_class(name).is_some(),
            Type::Nullable(inner) => self.is_class_type(inner),
            _ => false,
        }
    }

    fn can_access_private_member(&self, owner: &str) -> bool {
        self.current_class.as_deref() == Some(owner)
    }

    /// Returns true when the current class is `owner` or a subclass of `owner`.
    fn can_access_protected_member(&self, owner: &str) -> bool {
        match self.current_class.as_deref() {
            Some(current) => current == owner || self.class_extends(current, owner),
            None => false,
        }
    }

    fn push(&mut self, d: Diagnostic) {
        self.errors.push(d);
    }

    fn push_legacy_this_error(&mut self, span: Span) {
        self.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0550,
                "receiver alias `this` is not supported",
            )
            .with_label(Label::primary(span, "`this` used as a receiver"))
            .with_help("use `self` inside instance methods"),
        );
    }
}

fn type_name(ty: &Type) -> String {
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

fn range_type() -> Type {
    Type::Generic("Range".to_string(), vec![Type::I64])
}

fn is_i64_range_type(ty: &Type) -> bool {
    matches!(ty, Type::Generic(name, args) if name == "Range" && args.as_slice() == [Type::I64])
}

fn reference_place_key(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Var(name, _) => Some(name.clone()),
        Expr::FieldAccess(obj, field_name, _) => {
            reference_place_key(obj).map(|base| format!("{base}.{field_name}"))
        }
        Expr::Index(array, index, _) => {
            let base = reference_place_key(array)?;
            match &**index {
                Expr::Integer(value, _) => Some(format!("{base}[{value}]")),
                _ => None,
            }
        }
        _ => None,
    }
}

fn function_call_return_type(info: &FuncInfo) -> Type {
    if info.is_async {
        Type::Generic("Future".to_string(), vec![info.return_type.clone()])
    } else {
        info.return_type.clone()
    }
}

fn channel_element_type(ty: &Type) -> Option<Type> {
    match ty {
        Type::Generic(name, args) if name == "Channel" && args.len() == 1 => Some(args[0].clone()),
        _ => None,
    }
}

fn is_untyped_channel_new_call(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::StaticCall(call)
            if call.class == "Channel"
                && call.type_args.is_empty()
                && call.method == "new"
                && call.args.is_empty()
    )
}

fn block_always_returns(block: &Block) -> bool {
    block.stmts.iter().any(stmt_always_returns)
}

fn stmt_always_returns(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Return(_) => true,
        Stmt::If(s) => s
            .else_block
            .as_ref()
            .map(|else_block| {
                block_always_returns(&s.then_block) && block_always_returns(else_block)
            })
            .unwrap_or(false),
        Stmt::Let(_)
        | Stmt::Assign(_)
        | Stmt::FieldAssign(_)
        | Stmt::IndexAssign(_)
        | Stmt::While(_)
        | Stmt::For(_)
        | Stmt::Expr(_) => false,
    }
}

fn nullable_inner_has_pointer_representation(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Named(_) | Type::String | Type::Array(_) | Type::Generic(_, _) | Type::Fn(_, _)
    )
}

fn value_param_infos(params: &[Type]) -> Vec<ParamInfo> {
    params
        .iter()
        .map(|ty| ParamInfo {
            ty: ty.clone(),
            mode: ParamMode::Value,
            span: Span::dummy(),
            type_span: Span::dummy(),
        })
        .collect()
}

fn param_infos_from_decl(params: &[Param], module_prefix: Option<&str>) -> Vec<ParamInfo> {
    params
        .iter()
        .map(|param| ParamInfo {
            ty: qualify_type_for_module(&param.ty, module_prefix),
            mode: param.mode.clone(),
            span: param.span,
            type_span: param.type_span,
        })
        .collect()
}

fn class_info_from_decl(
    class: &ClassDecl,
    registered_name: &str,
    module_prefix: Option<&str>,
) -> ClassInfo {
    let mut fields = HashMap::new();
    let mut methods = HashMap::new();

    for field in &class.fields {
        fields.insert(
            field.name.clone(),
            FieldInfo {
                ty: qualify_type_for_module(&field.ty, module_prefix),
                public: field.public,
                protected: field.protected,
                declaration_span: field.span,
            },
        );
    }
    for method in &class.methods {
        let params = method
            .params
            .iter()
            .map(|param| qualify_type_for_module(&param.ty, module_prefix))
            .collect();
        methods.insert(
            method.name.clone(),
            MethodInfo {
                param_infos: param_infos_from_decl(&method.params, module_prefix),
                params,
                has_self: method.has_self,
                return_type: qualify_type_for_module(&method.return_type, module_prefix),
                public: method.public,
                protected: method.protected,
                is_open: method.is_open,
                is_override: method.is_override,
                declaration_span: method.span,
            },
        );
    }

    ClassInfo {
        name: registered_name.to_string(),
        public: class.public,
        is_open: class.is_open,
        base_class: class
            .base_class
            .as_ref()
            .map(|base| qualified_type_path_name(base, module_prefix)),
        implements: class
            .implements
            .iter()
            .map(|iface| qualify_type_for_module(iface, module_prefix))
            .collect(),
        declaration_span: class.span,
        fields,
        methods,
    }
}

fn qualify_type_for_module(ty: &Type, module_prefix: Option<&str>) -> Type {
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

fn type_path_name(path: &TypePath) -> String {
    qualified_type_path_name(path, None)
}

/// Render a required interface method as `name(self, T, U) -> R` for diagnostics.
fn interface_method_signature(m: &InterfaceMethodInfo) -> String {
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

fn qualified_type_path_name(path: &TypePath, module_prefix: Option<&str>) -> String {
    match path {
        TypePath::Local(name) => module_prefix
            .map(|module| format!("{module}::{name}"))
            .unwrap_or_else(|| name.clone()),
        TypePath::Qualified(parts) => parts.join("::"),
    }
}

fn is_supported_f64_format(spec: &str) -> bool {
    matches!(spec, "{:.17g}" | "{:.16f}" | "{:.6f}")
}

fn suggest_similar_name<'a>(
    target: &str,
    candidates: impl Iterator<Item = &'a String>,
) -> Option<String> {
    let max_distance = if target.len() <= 4 { 1 } else { 2 };
    candidates
        .map(|candidate| (levenshtein(target, candidate), candidate))
        .filter(|(distance, _)| *distance <= max_distance)
        .min_by_key(|(distance, candidate)| (*distance, candidate.len()))
        .map(|(_, candidate)| candidate.clone())
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b_chars = b.chars().collect::<Vec<_>>();
    let mut prev = (0..=b_chars.len()).collect::<Vec<_>>();
    let mut curr = vec![0; b_chars.len() + 1];

    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            curr[j + 1] = (curr[j] + 1).min(prev[j + 1] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b_chars.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn check_source(source: &str) -> Vec<Diagnostic> {
        let tokens = Lexer::new(source).tokenize().expect("lexing failed");
        let (program, parse_errors) = Parser::new(tokens).parse();
        assert!(
            parse_errors.is_empty(),
            "unexpected parse errors: {parse_errors:?}"
        );

        let mut checker = TypeChecker::new();
        checker.check_program(&program);
        checker.errors
    }

    fn assert_typecheck_ok(source: &str) {
        let errors = check_source(source);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    fn assert_typecheck_error_contains(source: &str, code: ErrorCode, expected_message: &str) {
        let errors = check_source(source);
        assert!(
            errors
                .iter()
                .any(|error| error.code == code && error.message.contains(expected_message)),
            "expected {code:?} containing `{expected_message}`, got {errors:?}",
        );
    }

    const NODE_CLASS: &str = r#"
class Node {
    pub value: i64;
    pub next: Node?;

    pub fn get(self) -> i64 {
        return self.value;
    }
}
"#;

    macro_rules! reference_ok_case {
        ($name:ident, $source:expr) => {
            #[test]
            fn $name() {
                assert_typecheck_ok($source);
            }
        };
    }

    macro_rules! reference_error_case {
        ($name:ident, $source:expr, $code:expr, $message:expr) => {
            #[test]
            fn $name() {
                assert_typecheck_error_contains($source, $code, $message);
            }
        };
    }

    #[test]
    fn unit_async_sleep_01_call_expression_typechecks_without_await() {
        assert_typecheck_ok(
            r#"
fn f() {
    sleep(0);
}
"#,
        );
    }

    #[test]
    fn unit_async_sleep_02_await_sleep_in_async_function_typechecks() {
        assert_typecheck_ok(
            r#"
async fn f() {
    await sleep(0);
}
"#,
        );
    }

    #[test]
    fn unit_async_sleep_03_await_sleep_negative_duration_typechecks() {
        assert_typecheck_ok(
            r#"
async fn f() {
    await sleep(-1);
}
"#,
        );
    }

    #[test]
    fn unit_async_sleep_04_await_sleep_can_return_from_void_async() {
        assert_typecheck_ok(
            r#"
async fn f() {
    return await sleep(0);
}
"#,
        );
    }

    #[test]
    fn unit_async_sleep_05_sleep_accepts_i64_variable() {
        assert_typecheck_ok(
            r#"
fn f() {
    let ms = 10;
    sleep(ms);
}
"#,
        );
    }

    #[test]
    fn unit_async_sleep_06_sleep_rejects_bool_argument() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    sleep(true);
}
"#,
            ErrorCode::E0201,
            "mismatched types: expected `i64`, found `bool`",
        );
    }

    #[test]
    fn unit_async_sleep_07_sleep_rejects_string_argument() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    sleep("slow");
}
"#,
            ErrorCode::E0201,
            "mismatched types: expected `i64`, found `String`",
        );
    }

    #[test]
    fn unit_async_sleep_08_sleep_rejects_missing_argument() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    sleep();
}
"#,
            ErrorCode::E0201,
            "function `sleep` takes 1 argument(s) but 0 were supplied",
        );
    }

    #[test]
    fn unit_async_sleep_09_sleep_rejects_extra_argument() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    sleep(1, 2);
}
"#,
            ErrorCode::E0201,
            "function `sleep` takes 1 argument(s) but 2 were supplied",
        );
    }

    #[test]
    fn unit_async_sleep_10_sleep_rejects_reference_argument() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let ms = 1;
    sleep(&ms);
}
"#,
            ErrorCode::E1703,
            "unexpected reference argument",
        );
    }

    #[test]
    fn unit_async_sleep_11_await_sleep_outside_async_is_rejected() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    await sleep(0);
}
"#,
            ErrorCode::E0801,
            "`await` can only be used inside an async function",
        );
    }

    #[test]
    fn unit_async_sleep_12_await_sleep_cannot_initialize_i64() {
        assert_typecheck_error_contains(
            r#"
async fn f() {
    let value: i64 = await sleep(0);
}
"#,
            ErrorCode::E0201,
            "mismatched types: expected `i64`, found `void`",
        );
    }

    #[test]
    fn unit_async_sleep_13_await_sleep_cannot_return_i64() {
        assert_typecheck_error_contains(
            r#"
async fn f() -> i64 {
    return await sleep(0);
}
"#,
            ErrorCode::E0201,
            "mismatched types: expected `i64`, found `void`",
        );
    }

    #[test]
    fn unit_async_sleep_14_sleep_future_cannot_be_passed_to_future_i64() {
        assert_typecheck_error_contains(
            r#"
fn takes_future(f: Future<i64>) {
}

fn f() {
    takes_future(sleep(0));
}
"#,
            ErrorCode::E0201,
            "mismatched types: expected `Future<i64>`, found `Future<void>`",
        );
    }

    #[test]
    fn unit_async_sleep_15_sleep_future_can_be_stored_and_awaited() {
        assert_typecheck_ok(
            r#"
async fn f() {
    let future = sleep(0);
    await future;
}
"#,
        );
    }

    #[test]
    fn unit_channel_01_new_with_i64_annotation_typechecks() {
        assert_typecheck_ok(
            r#"
fn f() {
    let ch: Channel<i64> = Channel::new();
}
"#,
        );
    }

    #[test]
    fn unit_channel_21_typed_new_infers_channel_type_without_annotation() {
        assert_typecheck_ok(
            r#"
fn f() {
    let ch = Channel<i64>::new();
    ch.send(10);
    let value: i64 = ch.recv();
}
"#,
        );
    }

    #[test]
    fn unit_channel_22_typed_new_mismatch_reports_e0201() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let ch: Channel<i64> = Channel<bool>::new();
}
"#,
            ErrorCode::E0201,
            "mismatched types: expected `Channel<i64>`, found `Channel<bool>`",
        );
    }

    #[test]
    fn unit_channel_02_i64_send_recv_typechecks() {
        assert_typecheck_ok(
            r#"
fn f() {
    let ch: Channel<i64> = Channel::new();
    ch.send(10);
    let value: i64 = ch.recv();
}
"#,
        );
    }

    #[test]
    fn unit_channel_03_bool_send_recv_typechecks() {
        assert_typecheck_ok(
            r#"
fn f() {
    let ch: Channel<bool> = Channel::new();
    ch.send(true);
    let value: bool = ch.recv();
}
"#,
        );
    }

    #[test]
    fn unit_channel_04_f64_send_recv_typechecks() {
        assert_typecheck_ok(
            r#"
fn f() {
    let ch: Channel<f64> = Channel::new();
    ch.send(1.5);
    let value: f64 = ch.recv();
}
"#,
        );
    }

    #[test]
    fn unit_channel_05_string_send_recv_typechecks() {
        assert_typecheck_ok(
            r#"
fn f() {
    let ch: Channel<String> = Channel::new();
    ch.send("hello");
    let value: String = ch.recv();
}
"#,
        );
    }

    #[test]
    fn unit_channel_06_class_send_recv_typechecks() {
        assert_typecheck_ok(
            r#"
class Boxed {
    pub value: i64;
}

fn f() {
    let ch: Channel<Boxed> = Channel::new();
    let value = Boxed { value: 1 };
    ch.send(value);
    let out: Boxed = ch.recv();
}
"#,
        );
    }

    #[test]
    fn unit_channel_07_nullable_class_accepts_nil_and_value() {
        assert_typecheck_ok(
            r#"
class Node {
    pub value: i64;
}

fn f() {
    let ch: Channel<Node?> = Channel::new();
    let node = Node { value: 1 };
    ch.send(nil);
    ch.send(node);
    let out: Node? = ch.recv();
}
"#,
        );
    }

    #[test]
    fn unit_channel_08_close_typechecks() {
        assert_typecheck_ok(
            r#"
fn f() {
    let ch: Channel<i64> = Channel::new();
    ch.close();
}
"#,
        );
    }

    #[test]
    fn unit_channel_09_recv_i64_can_be_used_in_arithmetic() {
        assert_typecheck_ok(
            r#"
fn f() {
    let ch: Channel<i64> = Channel::new();
    ch.send(20);
    let value = ch.recv() + 22;
}
"#,
        );
    }

    #[test]
    fn unit_channel_10_recv_bool_can_be_used_as_condition() {
        assert_typecheck_ok(
            r#"
fn f() {
    let ch: Channel<bool> = Channel::new();
    ch.send(true);
    if ch.recv() {
        let value = 1;
    }
}
"#,
        );
    }

    #[test]
    fn unit_channel_11_send_type_mismatch_reports_e0802() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let ch: Channel<i64> = Channel::new();
    ch.send(true);
}
"#,
            ErrorCode::E0802,
            "cannot send `bool` into `Channel<i64>`",
        );
    }

    #[test]
    fn unit_channel_12_recv_type_mismatch_reports_e0201() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let ch: Channel<i64> = Channel::new();
    let value: bool = ch.recv();
}
"#,
            ErrorCode::E0201,
            "mismatched types: expected `bool`, found `i64`",
        );
    }

    #[test]
    fn unit_channel_13_send_wrong_arity_reports_e0201() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let ch: Channel<i64> = Channel::new();
    ch.send();
}
"#,
            ErrorCode::E0201,
            "send expects 1 argument, got 0",
        );
    }

    #[test]
    fn unit_channel_14_recv_wrong_arity_reports_e0201() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let ch: Channel<i64> = Channel::new();
    ch.recv(1);
}
"#,
            ErrorCode::E0201,
            "recv expects 0 arguments, got 1",
        );
    }

    #[test]
    fn unit_channel_15_close_wrong_arity_reports_e0201() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let ch: Channel<i64> = Channel::new();
    ch.close(1);
}
"#,
            ErrorCode::E0201,
            "close expects 0 arguments, got 1",
        );
    }

    #[test]
    fn unit_channel_16_send_on_non_channel_reports_e0806() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let value = 1;
    value.send(2);
}
"#,
            ErrorCode::E0806,
            "cannot call `send` on `i64`",
        );
    }

    #[test]
    fn unit_channel_17_recv_on_non_channel_reports_e0806() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let value = 1;
    value.recv();
}
"#,
            ErrorCode::E0806,
            "cannot call `recv` on `i64`",
        );
    }

    #[test]
    fn unit_channel_18_close_on_non_channel_reports_e0806() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let value = 1;
    value.close();
}
"#,
            ErrorCode::E0806,
            "cannot call `close` on `i64`",
        );
    }

    #[test]
    fn unit_channel_19_new_wrong_arity_reports_e0201() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let ch: Channel<i64> = Channel::new(1);
}
"#,
            ErrorCode::E0201,
            "function `Channel::new` expects 0 arguments, got 1",
        );
    }

    #[test]
    fn unit_channel_20_send_reference_argument_reports_e1703() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let ch: Channel<i64> = Channel::new();
    let value = 1;
    ch.send(&value);
}
"#,
            ErrorCode::E1703,
            "unexpected reference argument",
        );
    }

    #[test]
    fn unit_reference_01_accepts_mutable_local_mut_reference_argument() {
        assert_typecheck_ok(
            r#"
fn increment(x: &mut i64) {
    x = x + 1;
}

fn f() {
    let mut n = 10;
    increment(&n);
}
"#,
        );
    }

    #[test]
    fn unit_reference_02_rejects_immutable_local_mut_reference_argument() {
        assert_typecheck_error_contains(
            r#"
fn increment(x: &mut i64) {
}

fn f() {
    let n = 10;
    increment(&n);
}
"#,
            ErrorCode::E1701,
            "cannot pass immutable variable `n` as `&mut`",
        );
    }

    #[test]
    fn unit_reference_03_rejects_missing_reference_marker() {
        assert_typecheck_error_contains(
            r#"
fn increment(x: &mut i64) {
}

fn f() {
    let mut n = 10;
    increment(n);
}
"#,
            ErrorCode::E1702,
            "expected reference argument for reference parameter",
        );
    }

    #[test]
    fn unit_reference_04_rejects_unexpected_reference_marker_for_value_param() {
        assert_typecheck_error_contains(
            r#"
fn take_value(x: i64) {
}

fn f() {
    let mut n = 10;
    take_value(&n);
}
"#,
            ErrorCode::E1703,
            "unexpected reference argument",
        );
    }

    #[test]
    fn unit_reference_05_rejects_non_place_reference_argument() {
        assert_typecheck_error_contains(
            r#"
fn increment(x: &mut i64) {
}

fn f() {
    let mut n = 10;
    increment(&(n + 1));
}
"#,
            ErrorCode::E1704,
            "cannot pass non-place expression by reference",
        );
    }

    #[test]
    fn unit_reference_06_rejects_reference_argument_type_mismatch() {
        assert_typecheck_error_contains(
            r#"
fn set_bool(x: &mut bool) {
}

fn f() {
    let mut n: i64 = 0;
    set_bool(&n);
}
"#,
            ErrorCode::E1705,
            "reference argument type mismatch",
        );
    }

    #[test]
    fn unit_reference_07_accepts_immutable_local_immutable_reference_argument() {
        assert_typecheck_ok(
            r#"
fn read(x: & i64) -> i64 {
    return x;
}

fn f() {
    let n = 10;
    let value = read(&n);
}
"#,
        );
    }

    #[test]
    fn unit_reference_08_rejects_assignment_through_immutable_reference_parameter() {
        assert_typecheck_error_contains(
            r#"
fn increment(x: & i64) {
    x = x + 1;
}
"#,
            ErrorCode::E0302,
            "cannot assign to immutable parameter `x`",
        );
    }

    reference_ok_case!(
        unit_reference_09_accepts_immutable_reference_to_mutable_local,
        r#"
fn read(x: & i64) -> i64 {
    return x;
}

fn f() {
    let mut n = 10;
    let value = read(&n);
}
"#
    );

    reference_ok_case!(
        unit_reference_10_accepts_mutable_bool_reference_assignment,
        r#"
fn flip(x: &mut bool) {
    x = !x;
}

fn f() {
    let mut flag = false;
    flip(&flag);
}
"#
    );

    reference_ok_case!(
        unit_reference_11_accepts_mutable_f64_reference_assignment,
        r#"
fn add_half(x: &mut f64) {
    x = x + 0.5;
}

fn f() {
    let mut value: f64 = 1.5;
    add_half(&value);
}
"#
    );

    reference_ok_case!(
        unit_reference_12_accepts_immutable_bool_reference_in_condition,
        r#"
fn choose(flag: & bool) -> i64 {
    if flag {
        return 1;
    }
    return 0;
}

fn f() {
    let flag = true;
    let value = choose(&flag);
}
"#
    );

    reference_ok_case!(
        unit_reference_13_accepts_multiple_reference_parameters,
        r#"
fn set_if_positive(n: & i64, flag: &mut bool) {
    if n > 0 {
        flag = true;
    }
}

fn f() {
    let n = 1;
    let mut flag = false;
    set_if_positive(&n, &flag);
}
"#
    );

    reference_ok_case!(
        unit_reference_14_accepts_mixed_value_and_reference_parameters,
        r#"
fn mix(prefix: String, n: & i64, enabled: bool, out: &mut bool) {
    if enabled && n > 0 {
        out = true;
    }
}

fn f() {
    let n = 3;
    let mut out = false;
    mix("ok", &n, true, &out);
}
"#
    );

    reference_ok_case!(
        unit_reference_15_accepts_mut_reference_read_before_write,
        r#"
fn increment(x: &mut i64) {
    let next = x + 1;
    x = next;
}

fn f() {
    let mut n = 3;
    increment(&n);
}
"#
    );

    reference_ok_case!(
        unit_reference_16_accepts_mut_reference_return_after_write,
        r#"
fn increment(x: &mut i64) -> i64 {
    x = x + 1;
    return x;
}

fn f() {
    let mut n = 3;
    let next = increment(&n);
}
"#
    );

    reference_ok_case!(
        unit_reference_17_accepts_forwarding_mut_reference_parameter,
        r#"
fn increment(x: &mut i64) {
    x = x + 1;
}

fn caller(x: &mut i64) {
    increment(&x);
}
"#
    );

    reference_ok_case!(
        unit_reference_18_accepts_forwarding_immutable_reference_parameter,
        r#"
fn read(x: & i64) -> i64 {
    return x;
}

fn caller(x: & i64) -> i64 {
    return read(&x);
}
"#
    );

    reference_ok_case!(
        unit_reference_19_accepts_string_immutable_reference,
        r#"
fn identity(text: & String) -> String {
    return text;
}

fn f() {
    let text = "hello";
    let copied = identity(&text);
}
"#
    );

    reference_ok_case!(
        unit_reference_20_accepts_string_mutable_reference_assignment,
        r#"
fn replace(text: &mut String) {
    text = "next";
}

fn f() {
    let mut text = "old";
    replace(&text);
}
"#
    );

    #[test]
    fn unit_reference_21_accepts_nullable_class_immutable_reference() {
        assert_typecheck_ok(&format!(
            r#"
{NODE_CLASS}

fn is_missing(node: & Node?) -> bool {{
    return node == nil;
}}

fn f() {{
    let node: Node? = nil;
    let missing = is_missing(&node);
}}
"#
        ));
    }

    #[test]
    fn unit_reference_22_accepts_nullable_class_mutable_reference_assignment() {
        assert_typecheck_ok(&format!(
            r#"
{NODE_CLASS}

fn clear(node: &mut Node?) {{
    node = nil;
}}

fn f() {{
    let mut node: Node? = nil;
    clear(&node);
}}
"#
        ));
    }

    reference_ok_case!(
        unit_reference_23_accepts_method_immutable_reference_argument,
        r#"
class Counter {
    pub value: i64;

    pub fn add(self, amount: & i64) -> i64 {
        return self.value + amount;
    }
}

fn f() {
    let counter = Counter { value: 3 };
    let amount = 2;
    let result = counter.add(&amount);
}
"#
    );

    reference_ok_case!(
        unit_reference_24_accepts_method_mutable_reference_argument,
        r#"
class Counter {
    pub value: i64;

    pub fn add_to(self, out: &mut i64) {
        out = out + self.value;
    }
}

fn f() {
    let counter = Counter { value: 3 };
    let mut total = 2;
    counter.add_to(&total);
}
"#
    );

    reference_ok_case!(
        unit_reference_25_accepts_shadowed_reference_arguments,
        r#"
fn read(x: & i64) -> i64 {
    return x;
}

fn f() {
    let n = 1;
    if true {
        let n = 2;
        let inner = read(&n);
    }
    let outer = read(&n);
}
"#
    );

    reference_ok_case!(
        unit_reference_26_accepts_reference_parameter_in_ternary_condition,
        r#"
fn choose(flag: & bool, a: i64, b: i64) -> i64 {
    return flag ? a : b;
}

fn f() {
    let flag = true;
    let value = choose(&flag, 1, 2);
}
"#
    );

    reference_ok_case!(
        unit_reference_27_accepts_reference_parameter_in_while_condition,
        r#"
fn wait(flag: & bool) {
    while flag {
        return;
    }
}

fn f() {
    let flag = false;
    wait(&flag);
}
"#
    );

    reference_ok_case!(
        unit_reference_28_accepts_reference_argument_in_expression_result,
        r#"
fn read(x: & i64) -> i64 {
    return x;
}

fn f() {
    let n = 3;
    let value = read(&n) + 1;
}
"#
    );

    reference_ok_case!(
        unit_reference_29_accepts_reference_argument_order_mixed_with_values,
        r#"
fn mix(a: i64, b: & i64, c: bool, d: &mut bool) {
    if c && b > a {
        d = true;
    }
}

fn f() {
    let n = 2;
    let mut out = false;
    mix(1, &n, true, &out);
}
"#
    );

    reference_ok_case!(
        unit_reference_30_accepts_class_reference_exact_type,
        r#"
class User {
    pub id: i64;
}

fn id(user: & User) -> i64 {
    return user.id;
}

fn f() {
    let user = User { id: 42 };
    let value = id(&user);
}
"#
    );

    reference_ok_case!(
        unit_reference_31_accepts_mut_class_reference_assignment,
        r#"
class User {
    pub id: i64;
}

fn replace(user: &mut User, next: User) {
    user = next;
}

fn f() {
    let mut user = User { id: 1 };
    let next = User { id: 2 };
    replace(&user, next);
}
"#
    );

    #[test]
    fn unit_reference_32_accepts_nullable_narrowing_on_reference_parameter() {
        assert_typecheck_ok(&format!(
            r#"
{NODE_CLASS}

fn value_or_zero(node: & Node?) -> i64 {{
    if node == nil {{
        return 0;
    }}
    return node.value;
}}
"#
        ));
    }

    reference_error_case!(
        unit_reference_33_rejects_missing_marker_for_immutable_reference_parameter,
        r#"
fn read(x: & i64) {
}

fn f() {
    let n = 1;
    read(n);
}
"#,
        ErrorCode::E1702,
        "expected reference argument for reference parameter"
    );

    reference_error_case!(
        unit_reference_34_rejects_value_parameter_reference_argument_for_bool,
        r#"
fn take(flag: bool) {
}

fn f() {
    let flag = true;
    take(&flag);
}
"#,
        ErrorCode::E1703,
        "unexpected reference argument"
    );

    reference_error_case!(
        unit_reference_35_rejects_integer_literal_reference_argument,
        r#"
fn read(x: & i64) {
}

fn f() {
    read(&42);
}
"#,
        ErrorCode::E1704,
        "cannot pass non-place expression by reference"
    );

    reference_error_case!(
        unit_reference_36_rejects_bool_literal_reference_argument,
        r#"
fn read(flag: & bool) {
}

fn f() {
    read(&true);
}
"#,
        ErrorCode::E1704,
        "cannot pass non-place expression by reference"
    );

    reference_error_case!(
        unit_reference_37_rejects_nil_reference_argument,
        r#"
class Node {
    pub value: i64;
}

fn visit(node: & Node?) {
}

fn f() {
    visit(&nil);
}
"#,
        ErrorCode::E1704,
        "cannot pass non-place expression by reference"
    );

    reference_error_case!(
        unit_reference_38_rejects_call_result_reference_argument,
        r#"
fn source() -> i64 {
    return 1;
}

fn read(x: & i64) {
}

fn f() {
    read(&source());
}
"#,
        ErrorCode::E1704,
        "cannot pass non-place expression by reference"
    );

    reference_error_case!(
        unit_reference_39_rejects_ternary_reference_argument,
        r#"
fn read(x: & i64) {
}

fn f() {
    let flag = true;
    let a = 1;
    let b = 2;
    read(&(flag ? a : b));
}
"#,
        ErrorCode::E1704,
        "cannot pass non-place expression by reference"
    );

    reference_error_case!(
        unit_reference_40_rejects_unary_reference_argument,
        r#"
fn read(x: & i64) {
}

fn f() {
    let n = 1;
    read(&(-n));
}
"#,
        ErrorCode::E1704,
        "cannot pass non-place expression by reference"
    );

    reference_ok_case!(
        unit_reference_41_accepts_field_reference_argument,
        r#"
class User {
    pub id: i64;
}

fn read(x: & i64) {
}

fn f() {
    let user = User { id: 1 };
    read(&user.id);
}
"#
    );

    reference_error_case!(
        unit_reference_42_rejects_method_result_reference_argument,
        r#"
class User {
    pub id: i64;

    pub fn get(self) -> i64 {
        return self.id;
    }
}

fn read(x: & i64) {
}

fn f() {
    let user = User { id: 1 };
    read(&user.get());
}
"#,
        ErrorCode::E1704,
        "cannot pass non-place expression by reference"
    );

    reference_error_case!(
        unit_reference_43_rejects_mut_reference_to_immutable_value_parameter,
        r#"
fn increment(x: &mut i64) {
}

fn caller(x: i64) {
    increment(&x);
}
"#,
        ErrorCode::E1701,
        "cannot pass immutable variable `x` as `&mut`"
    );

    reference_error_case!(
        unit_reference_44_rejects_mut_reference_to_immutable_reference_parameter,
        r#"
fn increment(x: &mut i64) {
}

fn caller(x: & i64) {
    increment(&x);
}
"#,
        ErrorCode::E1701,
        "cannot pass immutable variable `x` as `&mut`"
    );

    reference_error_case!(
        unit_reference_45_rejects_mut_reference_type_mismatch_bool_to_i64,
        r#"
fn increment(x: &mut i64) {
}

fn f() {
    let mut flag = true;
    increment(&flag);
}
"#,
        ErrorCode::E1705,
        "reference argument type mismatch"
    );

    reference_error_case!(
        unit_reference_46_rejects_immutable_reference_type_mismatch_bool_to_i64,
        r#"
fn read(x: & i64) {
}

fn f() {
    let flag = true;
    read(&flag);
}
"#,
        ErrorCode::E1705,
        "reference argument type mismatch"
    );

    reference_error_case!(
        unit_reference_47_rejects_string_mut_reference_type_mismatch,
        r#"
fn replace(text: &mut String) {
}

fn f() {
    let mut n = 1;
    replace(&n);
}
"#,
        ErrorCode::E1705,
        "reference argument type mismatch"
    );

    reference_error_case!(
        unit_reference_48_rejects_nullable_reference_to_non_nullable_parameter,
        r#"
class Node {
    pub value: i64;
}

fn visit(node: & Node) {
}

fn f() {
    let node: Node? = nil;
    visit(&node);
}
"#,
        ErrorCode::E1705,
        "reference argument type mismatch"
    );

    reference_error_case!(
        unit_reference_49_rejects_nonnullable_reference_to_nullable_parameter,
        r#"
class Node {
    pub value: i64;
}

fn visit(node: & Node?) {
}

fn f() {
    let node = Node { value: 1 };
    visit(&node);
}
"#,
        ErrorCode::E1705,
        "reference argument type mismatch"
    );

    reference_error_case!(
        unit_reference_50_rejects_assignment_through_immutable_bool_reference,
        r#"
fn set(flag: & bool) {
    flag = true;
}
"#,
        ErrorCode::E0302,
        "cannot assign to immutable parameter `flag`"
    );

    reference_error_case!(
        unit_reference_51_rejects_assignment_through_immutable_string_reference,
        r#"
fn replace(text: & String) {
    text = "next";
}
"#,
        ErrorCode::E0302,
        "cannot assign to immutable parameter `text`"
    );

    reference_error_case!(
        unit_reference_52_rejects_assignment_through_method_immutable_reference,
        r#"
class Box {
    pub fn bad(self, x: & i64) {
        x = 1;
    }
}
"#,
        ErrorCode::E0302,
        "cannot assign to immutable parameter `x`"
    );

    reference_error_case!(
        unit_reference_53_rejects_method_missing_reference_marker,
        r#"
class Box {
    pub fn set(self, x: &mut i64) {
    }
}

fn f() {
    let box = Box {};
    let mut n = 1;
    box.set(n);
}
"#,
        ErrorCode::E1702,
        "expected reference argument for reference parameter"
    );

    reference_error_case!(
        unit_reference_54_rejects_method_non_place_reference_argument,
        r#"
class Box {
    pub fn set(self, x: &mut i64) {
    }
}

fn f() {
    let box = Box {};
    let n = 1;
    box.set(&(n + 1));
}
"#,
        ErrorCode::E1704,
        "cannot pass non-place expression by reference"
    );

    reference_error_case!(
        unit_reference_55_rejects_method_reference_type_mismatch,
        r#"
class Box {
    pub fn set(self, x: &mut i64) {
    }
}

fn f() {
    let box = Box {};
    let mut flag = true;
    box.set(&flag);
}
"#,
        ErrorCode::E1705,
        "reference argument type mismatch"
    );

    reference_error_case!(
        unit_reference_56_rejects_wrong_argument_count_for_reference_function,
        r#"
fn read(x: & i64) {
}

fn f() {
    read();
}
"#,
        ErrorCode::E0201,
        "takes 1 argument(s) but 0 were supplied"
    );

    reference_error_case!(
        unit_reference_57_rejects_unknown_reference_variable,
        r#"
fn read(x: & i64) {
}

fn f() {
    read(&missing);
}
"#,
        ErrorCode::E0350,
        "cannot find variable `missing`"
    );

    reference_error_case!(
        unit_reference_58_rejects_value_parameter_reference_in_second_argument,
        r#"
fn mix(a: i64, b: bool) {
}

fn f() {
    let flag = true;
    mix(1, &flag);
}
"#,
        ErrorCode::E1703,
        "unexpected reference argument"
    );

    reference_error_case!(
        unit_reference_59_rejects_non_place_reference_in_second_argument,
        r#"
fn mix(a: i64, b: & i64) {
}

fn f() {
    let n = 1;
    mix(0, &(n + 1));
}
"#,
        ErrorCode::E1704,
        "cannot pass non-place expression by reference"
    );

    reference_error_case!(
        unit_reference_60_rejects_missing_reference_marker_in_second_argument,
        r#"
fn mix(a: i64, b: & i64) {
}

fn f() {
    let n = 1;
    mix(0, n);
}
"#,
        ErrorCode::E1702,
        "expected reference argument for reference parameter"
    );

    reference_error_case!(
        unit_reference_61_rejects_mut_reference_to_shadowed_immutable_local,
        r#"
fn increment(x: &mut i64) {
}

fn f() {
    let mut n = 1;
    if true {
        let n = 2;
        increment(&n);
    }
}
"#,
        ErrorCode::E1701,
        "cannot pass immutable variable `n` as `&mut`"
    );

    reference_ok_case!(
        unit_reference_62_accepts_distinct_mutable_reference_arguments,
        r#"
fn swap_like(a: &mut i64, b: &mut i64) {
    a = a + 1;
    b = b + 1;
}

fn f() {
    let mut a = 1;
    let mut b = 2;
    swap_like(&a, &b);
}
"#
    );

    reference_error_case!(
        unit_reference_63_rejects_same_local_passed_to_two_mutable_references,
        r#"
fn swap_like(a: &mut i64, b: &mut i64) {
}

fn f() {
    let mut n = 1;
    swap_like(&n, &n);
}
"#,
        ErrorCode::E1706,
        "aliases a mutable reference"
    );

    reference_error_case!(
        unit_reference_64_rejects_mutable_reference_then_immutable_reference_alias,
        r#"
fn observe(a: &mut i64, b: & i64) {
}

fn f() {
    let mut n = 1;
    observe(&n, &n);
}
"#,
        ErrorCode::E1706,
        "aliases a mutable reference"
    );

    reference_error_case!(
        unit_reference_65_rejects_immutable_reference_then_mutable_reference_alias,
        r#"
fn observe(a: & i64, b: &mut i64) {
}

fn f() {
    let mut n = 1;
    observe(&n, &n);
}
"#,
        ErrorCode::E1706,
        "aliases a mutable reference"
    );

    reference_error_case!(
        unit_reference_66_rejects_mutable_reference_then_value_alias,
        r#"
fn use_both(a: &mut i64, b: i64) {
}

fn f() {
    let mut n = 1;
    use_both(&n, n);
}
"#,
        ErrorCode::E1706,
        "aliases a mutable reference"
    );

    reference_error_case!(
        unit_reference_67_rejects_value_then_mutable_reference_alias,
        r#"
fn use_both(a: i64, b: &mut i64) {
}

fn f() {
    let mut n = 1;
    use_both(n, &n);
}
"#,
        ErrorCode::E1706,
        "aliases a mutable reference"
    );

    reference_ok_case!(
        unit_reference_68_accepts_same_local_passed_to_two_immutable_references,
        r#"
fn compare(a: & i64, b: & i64) -> bool {
    return a == b;
}

fn f() {
    let n = 1;
    let same = compare(&n, &n);
}
"#
    );

    reference_ok_case!(
        unit_reference_69_accepts_mutable_and_immutable_references_to_distinct_locals,
        r#"
fn observe(a: &mut i64, b: & i64) {
    a = a + b;
}

fn f() {
    let mut a = 1;
    let b = 2;
    observe(&a, &b);
}
"#
    );

    reference_error_case!(
        unit_reference_70_rejects_method_duplicate_mutable_reference_alias,
        r#"
class Box {
    pub fn pair(self, a: &mut i64, b: &mut i64) {
    }
}

fn f() {
    let box = Box {};
    let mut n = 1;
    box.pair(&n, &n);
}
"#,
        ErrorCode::E1706,
        "aliases a mutable reference"
    );

    reference_error_case!(
        unit_reference_71_rejects_method_mutable_reference_and_value_alias,
        r#"
class Box {
    pub fn use_both(self, a: &mut i64, b: i64) {
    }
}

fn f() {
    let box = Box {};
    let mut n = 1;
    box.use_both(&n, n);
}
"#,
        ErrorCode::E1706,
        "aliases a mutable reference"
    );

    reference_ok_case!(
        unit_reference_72_accepts_method_distinct_mutable_reference_arguments,
        r#"
class Box {
    pub fn pair(self, a: &mut i64, b: &mut i64) {
    }
}

fn f() {
    let box = Box {};
    let mut a = 1;
    let mut b = 2;
    box.pair(&a, &b);
}
"#
    );

    reference_ok_case!(
        unit_reference_73_accepts_array_element_reference_argument,
        r#"
import std::collections::Array;

fn increment(x: &mut i64) {
    x = x + 1;
}

fn f() {
    let mut xs: Array<i64> = [1, 2];
    increment(&xs[0]);
}
"#
    );

    #[test]
    fn unit_for_loop_01_array_element_type_flows_into_body() {
        assert_typecheck_ok(
            r#"
import std::collections::Array;

fn f() -> i64 {
    let xs: Array<i64> = [1, 2, 3];
    let mut total = 0;
    for value in xs {
        total = total + value;
    }
    return total;
}
"#,
        );
    }

    #[test]
    fn unit_for_loop_02_rejects_non_array_iterable() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    for value in 123 {
        println(value);
    }
}
"#,
            ErrorCode::E0201,
            "cannot iterate over `i64`",
        );
    }

    #[test]
    fn unit_for_loop_03_underscore_binding_is_not_visible() {
        assert_typecheck_error_contains(
            r#"
import std::collections::Array;

fn f() {
    let xs: Array<i64> = [1, 2];
    for _ in xs {
        println(1);
    }
    println(_);
}
"#,
            ErrorCode::E0350,
            "cannot find variable `_`",
        );
    }

    #[test]
    fn unit_for_loop_04_accepts_i64_range_iterable() {
        assert_typecheck_ok(
            r#"
fn f() -> i64 {
    let mut total = 0;
    for n in 1..4 {
        total = total + n;
    }
    return total;
}
"#,
        );
    }

    #[test]
    fn unit_for_loop_05_rejects_range_expression_outside_for() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let r = 1..4;
}
"#,
            ErrorCode::E0201,
            "range expressions are only supported in `for` loops",
        );
    }

    #[test]
    fn unit_for_loop_06_rejects_non_i64_range_bounds() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    for n in true..4 {
        println(n);
    }
}
"#,
            ErrorCode::E0201,
            "range bounds must be `i64`",
        );
    }

    #[test]
    fn unit_nil_01_accepts_annotated_nullable_contexts() {
        assert_typecheck_ok(&format!(
            r#"
{NODE_CLASS}

fn empty() -> Node? {{
    let node: Node? = nil;
    return nil;
}}
"#
        ));
    }

    #[test]
    fn unit_nil_02_rejects_unannotated_nil_local() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let value = nil;
}
"#,
            ErrorCode::E0201,
            "cannot infer the type of `nil`",
        );
    }

    #[test]
    fn unit_nil_03_rejects_nil_for_non_nullable_local() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let value: i64 = nil;
}
"#,
            ErrorCode::E0201,
            "mismatched types: expected `i64`, found `nil`",
        );
    }

    #[test]
    fn unit_nil_04_rejects_nil_for_non_nullable_return() {
        assert_typecheck_error_contains(
            &format!(
                r#"
{NODE_CLASS}

fn missing() -> Node {{
    return nil;
}}
"#
            ),
            ErrorCode::E0201,
            "mismatched types: expected `Node`, found `nil`",
        );
    }

    #[test]
    fn unit_nil_05_nullable_parameter_accepts_value_and_nil() {
        assert_typecheck_ok(&format!(
            r#"
{NODE_CLASS}

fn visit(node: Node?) {{
}}

fn f(node: Node) {{
    visit(node);
    visit(nil);
}}
"#
        ));
    }

    #[test]
    fn unit_nil_06_rejects_nullable_value_for_non_nullable_parameter() {
        assert_typecheck_error_contains(
            &format!(
                r#"
{NODE_CLASS}

fn use_node(node: Node) {{
}}

fn f(node: Node?) {{
    use_node(node);
}}
"#
            ),
            ErrorCode::E0704,
            "mismatched types: expected `Node`, found `Node?`",
        );
    }

    #[test]
    fn unit_nil_07_object_literal_nullable_field_accepts_nil_and_value() {
        assert_typecheck_ok(&format!(
            r#"
{NODE_CLASS}

fn make() -> Node {{
    let tail = Node {{ value: 2, next: nil }};
    return Node {{ value: 1, next: tail }};
}}
"#
        ));
    }

    #[test]
    fn unit_nil_08_rejects_direct_field_access_on_nullable_value() {
        assert_typecheck_error_contains(
            &format!(
                r#"
{NODE_CLASS}

fn value(node: Node?) -> i64 {{
    return node.value;
}}
"#
            ),
            ErrorCode::E0201,
            "cannot access field `value` on nullable type `Node?`",
        );
    }

    #[test]
    fn unit_nil_09_rejects_direct_method_call_on_nullable_value() {
        assert_typecheck_error_contains(
            &format!(
                r#"
{NODE_CLASS}

fn value(node: Node?) -> i64 {{
    return node.get();
}}
"#
            ),
            ErrorCode::E0201,
            "cannot call method `get` on nullable type `Node?`",
        );
    }

    #[test]
    fn unit_nil_10_if_not_nil_narrows_then_branch() {
        assert_typecheck_ok(&format!(
            r#"
{NODE_CLASS}

fn value(node: Node?) -> i64 {{
    if node != nil {{
        return node.value;
    }}
    return 0;
}}
"#
        ));
    }

    #[test]
    fn unit_nil_11_nil_guard_return_narrows_following_code() {
        assert_typecheck_ok(&format!(
            r#"
{NODE_CLASS}

fn value(node: Node?) -> i64 {{
    if node == nil {{
        return 0;
    }}
    return node.value;
}}
"#
        ));
    }

    #[test]
    fn unit_nil_12_nil_check_narrows_else_branch() {
        assert_typecheck_ok(&format!(
            r#"
{NODE_CLASS}

fn value(node: Node?) -> i64 {{
    if node == nil {{
        return 0;
    }} else {{
        return node.value;
    }}
}}
"#
        ));
    }

    #[test]
    fn unit_nil_13_assignment_invalidates_narrowing() {
        assert_typecheck_error_contains(
            &format!(
                r#"
{NODE_CLASS}

fn value(node: Node?) -> i64 {{
    let mut current: Node? = node;
    if current != nil {{
        current = nil;
        return current.value;
    }}
    return 0;
}}
"#
            ),
            ErrorCode::E0201,
            "cannot access field `value` on nullable type `Node?`",
        );
    }

    #[test]
    fn unit_nil_14_ternary_unifies_value_and_nil_as_nullable() {
        assert_typecheck_ok(&format!(
            r#"
{NODE_CLASS}

fn selected_is_missing(cond: bool, node: Node) -> bool {{
    let selected = cond ? node : nil;
    return selected == nil;
}}
"#
        ));
    }

    #[test]
    fn unit_nil_15_rejects_nil_comparison_with_non_nullable_value() {
        assert_typecheck_error_contains(
            r#"
fn f(value: i64) -> bool {
    return value == nil;
}
"#,
            ErrorCode::E0201,
            "cannot compare non-nullable type `i64` with `nil`",
        );
    }

    // ── Interface conformance (willow-t8b, spec 7 / 15) ────────────────────

    #[test]
    fn iface_01_exact_match_ok() {
        assert_typecheck_ok(
            r#"
interface Animal { fn speak(self) -> String; }
class Dog implements Animal {
    pub fn speak(self) -> String { return "woof"; }
}
"#,
        );
    }

    #[test]
    fn iface_02_multiple_interfaces_ok() {
        assert_typecheck_ok(
            r#"
interface Animal { fn speak(self) -> String; }
interface Named { fn name(self) -> String; }
class Dog implements Animal, Named {
    pub fn speak(self) -> String { return "woof"; }
    pub fn name(self) -> String { return "dog"; }
}
"#,
        );
    }

    #[test]
    fn iface_03_marker_interface_ok() {
        assert_typecheck_ok(
            r#"
interface Marker {}
class Dog implements Marker {}
"#,
        );
    }

    #[test]
    fn iface_04_interface_as_param_type_validates() {
        // The interface name is a recognized type (coercion/dispatch is Stage 3).
        assert_typecheck_ok(
            r#"
interface Animal { fn speak(self) -> String; }
fn take(a: Animal) {}
"#,
        );
    }

    #[test]
    fn iface_05_interface_as_field_type_validates() {
        assert_typecheck_ok(
            r#"
interface Animal { fn speak(self) -> String; }
class Holder { pub a: Animal; }
"#,
        );
    }

    #[test]
    fn iface_06_inherited_method_satisfies() {
        assert_typecheck_ok(
            r#"
interface Animal { fn speak(self) -> String; }
open class Base {
    pub open fn speak(self) -> String { return "base"; }
}
class Dog extends Base implements Animal {}
"#,
        );
    }

    #[test]
    fn iface_07_method_with_params_matches() {
        assert_typecheck_ok(
            r#"
interface Adder { fn add(self, a: i64, b: i64) -> i64; }
class Calc implements Adder {
    pub fn add(self, a: i64, b: i64) -> i64 { return a + b; }
}
"#,
        );
    }

    #[test]
    fn iface_08_missing_method_rejected() {
        assert_typecheck_error_contains(
            r#"
interface Animal { fn speak(self) -> String; }
class Dog implements Animal {}
"#,
            ErrorCode::E0415,
            "does not implement interface `Animal`",
        );
    }

    #[test]
    fn iface_09_wrong_return_type_rejected() {
        assert_typecheck_error_contains(
            r#"
interface Animal { fn speak(self) -> String; }
class Dog implements Animal {
    pub fn speak(self) -> i64 { return 1; }
}
"#,
            ErrorCode::E0417,
            "requires `String`",
        );
    }

    #[test]
    fn iface_10_wrong_param_type_rejected() {
        assert_typecheck_error_contains(
            r#"
interface Adder { fn add(self, a: i64) -> i64; }
class Calc implements Adder {
    pub fn add(self, a: bool) -> i64 { return 1; }
}
"#,
            ErrorCode::E0416,
            "parameters do not match",
        );
    }

    #[test]
    fn iface_11_wrong_param_count_rejected() {
        assert_typecheck_error_contains(
            r#"
interface Adder { fn add(self, a: i64, b: i64) -> i64; }
class Calc implements Adder {
    pub fn add(self, a: i64) -> i64 { return a; }
}
"#,
            ErrorCode::E0416,
            "parameters do not match",
        );
    }

    #[test]
    fn iface_12_unknown_interface_rejected() {
        assert_typecheck_error_contains(
            r#"
class Dog implements Animal {}
"#,
            ErrorCode::E0410,
            "cannot find interface `Animal`",
        );
    }

    #[test]
    fn iface_13_implements_a_class_rejected() {
        assert_typecheck_error_contains(
            r#"
class Mammal {}
class Dog implements Mammal {}
"#,
            ErrorCode::E0411,
            "is a class, not an interface",
        );
    }

    #[test]
    fn iface_14_extends_an_interface_rejected() {
        assert_typecheck_error_contains(
            r#"
interface Animal { fn speak(self) -> String; }
class Dog extends Animal {}
"#,
            ErrorCode::E0412,
            "is an interface and cannot be extended",
        );
    }

    #[test]
    fn iface_15_instantiate_interface_rejected() {
        assert_typecheck_error_contains(
            r#"
interface Animal { fn speak(self) -> String; }
fn f() {
    let a = Animal {};
}
"#,
            ErrorCode::E0413,
            "cannot instantiate interface `Animal`",
        );
    }

    #[test]
    fn iface_16_duplicate_implements_rejected() {
        assert_typecheck_error_contains(
            r#"
interface Animal { fn speak(self) -> String; }
class Dog implements Animal, Animal {
    pub fn speak(self) -> String { return "woof"; }
}
"#,
            ErrorCode::E0414,
            "implemented more than once",
        );
    }

    #[test]
    fn iface_16b_phantom_generic_two_instantiations_ok() {
        // A phantom type parameter (used in no method signature) lets a class
        // implement two instantiations of the same generic interface; the dup
        // check keys on the full instantiated type, not the name (willow-1js.6).
        assert_typecheck_ok(
            r#"
interface Tagged<T> { fn tag_name(self) -> String; }
class Item implements Tagged<i64>, Tagged<String> {
    pub fn tag_name(self) -> String { return "item"; }
}
"#,
        );
    }

    #[test]
    fn iface_16c_exact_duplicate_instantiation_rejected() {
        // The same instantiation twice is still a duplicate (E0414), keyed by
        // the full instantiated type `Tagged<i64>`.
        assert_typecheck_error_contains(
            r#"
interface Tagged<T> { fn tag_name(self) -> String; }
class Item implements Tagged<i64>, Tagged<i64> {
    pub fn tag_name(self) -> String { return "item"; }
}
"#,
            ErrorCode::E0414,
            "implemented more than once",
        );
    }

    #[test]
    fn iface_16d_two_instantiations_unsatisfiable_rejected() {
        // Distinct instantiations are allowed past the dup check, but a single
        // `get(self) -> T` cannot satisfy both `i64` and `String` (E0417).
        assert_typecheck_error_contains(
            r#"
interface Container<T> { fn get(self) -> T; }
class C implements Container<i64>, Container<String> {
    pub fn get(self) -> i64 { return 1; }
}
"#,
            ErrorCode::E0417,
            "but interface `Container` requires",
        );
    }

    #[test]
    fn iface_17_private_method_rejected() {
        assert_typecheck_error_contains(
            r#"
interface Animal { fn speak(self) -> String; }
class Dog implements Animal {
    fn speak(self) -> String { return "woof"; }
}
"#,
            ErrorCode::E0415,
            "must be `pub`",
        );
    }

    #[test]
    fn iface_18_missing_self_receiver_rejected() {
        assert_typecheck_error_contains(
            r#"
interface Animal { fn speak(self) -> String; }
class Dog implements Animal {
    pub fn speak() -> String { return "woof"; }
}
"#,
            ErrorCode::E0416,
            "must take `self`",
        );
    }

    #[test]
    fn iface_19_duplicate_interface_method_rejected() {
        assert_typecheck_error_contains(
            r#"
interface Animal {
    fn speak(self) -> String;
    fn speak(self) -> i64;
}
"#,
            ErrorCode::E0502,
            "declared more than once in interface",
        );
    }

    #[test]
    fn iface_20_void_return_method_ok() {
        assert_typecheck_ok(
            r#"
interface Sink { fn push(self, x: i64); }
class Bucket implements Sink {
    pub fn push(self, x: i64) {}
}
"#,
        );
    }

    #[test]
    fn iface_21_unknown_type_still_errors() {
        // Interfaces must not mask the normal "unknown type" diagnostic.
        assert_typecheck_error_contains(
            r#"
fn f(a: Animal) {}
"#,
            ErrorCode::E0350,
            "cannot find type `Animal`",
        );
    }

    #[test]
    fn iface_22_partial_conformance_reports_each_missing() {
        // Two required methods, neither provided: both surface.
        let errors = check_source(
            r#"
interface Animal {
    fn speak(self) -> String;
    fn legs(self) -> i64;
}
class Dog implements Animal {}
"#,
        );
        let missing = errors.iter().filter(|e| e.code == ErrorCode::E0415).count();
        assert_eq!(
            missing, 2,
            "expected two missing-method errors, got {errors:?}"
        );
    }

    #[test]
    fn iface_23_class_without_implements_unaffected() {
        // Regression: a plain class with methods of the same name as some
        // interface is fine when it does not declare `implements`.
        assert_typecheck_ok(
            r#"
interface Animal { fn speak(self) -> String; }
class Robot {
    pub fn speak(self) -> i64 { return 1; }
}
"#,
        );
    }

    // ── Interface assignability + method resolution (willow-xds type side) ──

    const ANIMAL_DOG: &str = r#"
interface Animal { fn speak(self) -> String; }
class Dog implements Animal {
    pub fn speak(self) -> String { return "woof"; }
    pub fn wag(self) {}
}
"#;

    #[test]
    fn iface_24_class_assignable_to_interface_let() {
        assert_typecheck_ok(&format!(
            "{ANIMAL_DOG}\nfn f() {{ let a: Animal = Dog {{}}; }}"
        ));
    }

    #[test]
    fn iface_25_class_coerces_as_function_argument() {
        assert_typecheck_ok(&format!(
            "{ANIMAL_DOG}\nfn say(a: Animal) {{}}\nfn f() {{ say(Dog {{}}); }}"
        ));
    }

    #[test]
    fn iface_26_interface_method_call_returns_interface_return_type() {
        assert_typecheck_ok(&format!(
            "{ANIMAL_DOG}\nfn say(a: Animal) -> String {{ return a.speak(); }}"
        ));
    }

    #[test]
    fn iface_27_non_interface_method_rejected() {
        // `wag` exists on Dog but is not part of the Animal interface.
        assert_typecheck_error_contains(
            &format!("{ANIMAL_DOG}\nfn f(a: Animal) {{ a.wag(); }}"),
            ErrorCode::E0418,
            "no method `wag` on interface `Animal`",
        );
    }

    #[test]
    fn iface_28_return_class_as_interface_ok() {
        assert_typecheck_ok(&format!(
            "{ANIMAL_DOG}\nfn make() -> Animal {{ return Dog {{}}; }}"
        ));
    }

    #[test]
    fn iface_29_class_assignable_to_nullable_interface() {
        // spec 7.3.5: non-null Dog assignable to Animal?
        assert_typecheck_ok(&format!(
            "{ANIMAL_DOG}\nfn f() {{ let a: Animal? = Dog {{}}; }}"
        ));
    }

    #[test]
    fn iface_30_unrelated_class_not_assignable_to_interface() {
        assert_typecheck_error_contains(
            r#"
interface Animal { fn speak(self) -> String; }
class Rock {}
fn f() { let a: Animal = Rock {}; }
"#,
            ErrorCode::E0201,
            "expected `Animal`",
        );
    }

    #[test]
    fn iface_31_interface_field_accepts_class_value() {
        assert_typecheck_ok(&format!(
            "{ANIMAL_DOG}\nclass Holder {{ pub value: Animal; }}\nfn f() {{ let h = Holder {{ value: Dog {{}} }}; }}"
        ));
    }

    #[test]
    fn iface_32_interface_field_method_call_typechecks() {
        assert_typecheck_ok(&format!(
            "{ANIMAL_DOG}\nclass Holder {{ pub value: Animal; }}\nfn f(h: Holder) -> String {{ return h.value.speak(); }}"
        ));
    }

    #[test]
    fn iface_33_interface_field_rejects_unrelated_class() {
        assert_typecheck_error_contains(
            r#"
interface Animal { fn speak(self) -> String; }
class Rock {}
class Holder { pub value: Animal; }
fn f() { let h = Holder { value: Rock {} }; }
"#,
            ErrorCode::E0201,
            "expects `Animal`",
        );
    }

    #[test]
    fn iface_34_array_interface_push_accepts_class() {
        assert_typecheck_ok(&format!(
            "import std::collections::Array;\n{ANIMAL_DOG}\nfn f() {{ let xs: Array<Animal> = []; xs.push(Dog {{}}); }}"
        ));
    }

    #[test]
    fn iface_35_array_interface_index_returns_interface() {
        // Indexing an Array<Animal> yields an Animal, whose interface methods are callable.
        assert_typecheck_ok(&format!(
            "import std::collections::Array;\n{ANIMAL_DOG}\nfn f() -> String {{ let xs: Array<Animal> = []; xs.push(Dog {{}}); return xs[0].speak(); }}"
        ));
    }

    #[test]
    fn iface_36_nonempty_array_literal_with_interface_annotation() {
        // Differing classes that both implement the interface are accepted
        // element-wise against the annotation (willow-w8af).
        assert_typecheck_ok(&format!(
            "import std::collections::Array;\n{ANIMAL_DOG}\nclass Cat implements Animal {{ pub fn speak(self) -> String {{ return \"meow\"; }} }}\nfn f() {{ let xs: Array<Animal> = [Dog {{}}, Cat {{}}]; }}"
        ));
    }

    #[test]
    fn iface_37_array_literal_element_must_implement_interface() {
        assert_typecheck_error_contains(
            r#"
import std::collections::Array;

interface Animal { fn speak(self) -> String; }
class Dog implements Animal { pub fn speak(self) -> String { return "woof"; } }
class Rock {}
fn f() { let xs: Array<Animal> = [Dog {}, Rock {}]; }
"#,
            ErrorCode::E0201,
            "array element expects `Animal`",
        );
    }

    #[test]
    fn iface_38_mixed_array_without_annotation_still_rejected() {
        // Regression: without an interface annotation, element homogeneity holds.
        assert_typecheck_error_contains(
            "fn f() { let xs = [1, true]; }",
            ErrorCode::E0201,
            "array elements must have the same type",
        );
    }
}
