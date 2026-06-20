mod analysis;
mod check;
mod diagnostics;
mod resolve;
mod send_sync;
mod types;
pub(crate) use analysis::*;
#[cfg(test)]
use check::check_source;
use diagnostics::*;
pub(crate) use types::*;

use super::symbols::{ClassInfo, FieldInfo, MethodInfo, ParamInfo, StaticPropInfo, SymbolTable};
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
    /// Maps each lambda's span to its full inferred `fn(...) -> ...` type.
    /// This includes parameter types inferred from call-site context, which the
    /// immutable AST cannot store directly.
    pub lambda_fn_types: HashMap<Span, Type>,
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
    /// Set while checking a `static fn` body — `self` is unavailable there
    /// (willow-qsqf §9.2 → E0831).
    in_static_method: bool,
    /// Set while checking a `static` property initializer — `self` is unavailable
    /// there (willow-qsqf §10.3 → E0837).
    in_static_initializer: bool,
    /// Set while checking an `init(...)` constructor body — `return <value>` is
    /// rejected (willow-scq2 §8 → E0841).
    in_constructor: bool,
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
    /// Enforce the Send/Sync async checks (E2402-E2405). Off by default for the
    /// ambient single-worker target; enabled when compiling for multi-worker
    /// execution (WILLOW_WORKERS > 1) or explicitly via WILLOW_DATA_RACE_CHECK
    /// (willow-dgwo.4/.9).
    enforce_send_sync: bool,
    /// Synchronous helpers that contain or transitively reach a loop, keyed by
    /// `Class::method` (and bare fn names), with the helper's definition span.
    /// Used to flag a looping method called through a typed NON-`self` receiver
    /// from a task context (E0810) — the AST-only `ConcurrencyAnalyzer` cannot
    /// resolve such a receiver's class (willow-0a6k.2).
    nonpreemptible_methods: HashMap<String, Span>,
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
            lambda_fn_types: HashMap::new(),
            async_local_types: HashMap::new(),
            current_return_type: Type::Void,
            lambda_return_stack: Vec::new(),
            current_class: None,
            current_async_context: false,
            in_static_method: false,
            in_static_initializer: false,
            in_constructor: false,
            narrowed_vars: Vec::new(),
            imported_names: HashMap::new(),
            imported_collection_types: HashSet::new(),
            imported_collection_aliases: HashMap::new(),
            fully_qualified_collection_types: HashSet::new(),
            imported_std_modules: HashMap::new(),
            missing_collection_imports_reported: HashSet::new(),
            enforce_send_sync: false,
            nonpreemptible_methods: HashMap::new(),
        };
        checker.register_builtin_functions();
        checker.register_builtin_modules();
        checker
    }

    /// Enable the Send/Sync async checks. Turned on when targeting multi-worker
    /// execution (willow-dgwo.4/.9).
    pub fn set_enforce_send_sync(&mut self, on: bool) {
        self.enforce_send_sync = on;
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

    fn class_info_from_decl(
        &mut self,
        class: &ClassDecl,
        registered_name: &str,
        module_prefix: Option<&str>,
    ) -> ClassInfo {
        let mut fields = HashMap::new();
        let mut methods = HashMap::new();
        let mut static_props = HashMap::new();
        let mut instance_field_order: Vec<(String, Type)> = Vec::new();

        for (decl_index, field) in class.fields.iter().enumerate() {
            let ty = self.normalize_decl_type(&field.ty, field.span, module_prefix);
            if field.is_static {
                // Static properties live in global storage, not instance layout.
                static_props.insert(
                    field.name.clone(),
                    StaticPropInfo {
                        ty,
                        is_mut: field.is_mut,
                        public: field.public,
                        protected: field.protected,
                        decl_index,
                        declaration_span: field.span,
                    },
                );
            } else {
                instance_field_order.push((field.name.clone(), ty.clone()));
                fields.insert(
                    field.name.clone(),
                    FieldInfo {
                        ty,
                        public: field.public,
                        protected: field.protected,
                        declaration_span: field.span,
                    },
                );
            }
        }
        let constructor = class.constructors.first().map(|ctor| {
            let params = ctor
                .params
                .iter()
                .map(|p| self.normalize_decl_type(&p.ty, p.type_span, module_prefix))
                .collect();
            crate::semantic::symbols::ConstructorInfo {
                param_infos: self.normalize_decl_param_infos(&ctor.params, module_prefix),
                params,
                public: ctor.public,
                protected: ctor.protected,
                declaration_span: ctor.span,
            }
        });
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
                    is_static: method.is_static,
                    is_async: method.is_async,
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
            static_props,
            instance_field_order,
            constructor,
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

    fn base_class_requiring_initialization(&self, base_name: &str) -> Option<ClassInfo> {
        let mut current = Some(base_name.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                return None;
            }
            let class = self.symbols.lookup_class(&name)?;
            if class.constructor.is_some() || !class.instance_field_order.is_empty() {
                return Some(class.clone());
            }
            current = class.base_class.clone();
        }
        None
    }

    fn implicit_constructor_field_infos(
        &self,
        class_name: &str,
    ) -> Vec<(String, String, FieldInfo)> {
        let mut chain = Vec::new();
        let mut current = Some(class_name.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                break;
            }
            let Some(class) = self.symbols.lookup_class(&name) else {
                break;
            };
            let class_fields = class
                .instance_field_order
                .iter()
                .filter_map(|(field_name, _)| {
                    class
                        .fields
                        .get(field_name)
                        .cloned()
                        .map(|field| (field_name.clone(), field))
                })
                .collect::<Vec<_>>();
            chain.push((class.name.clone(), class_fields));
            current = class.base_class.clone();
        }

        let mut fields = Vec::new();
        let mut names = HashSet::new();
        for (owner, class_fields) in chain.into_iter().rev() {
            for (name, field) in class_fields {
                if names.insert(name.clone()) {
                    fields.push((owner.clone(), name, field));
                }
            }
        }
        fields
    }

    /// Element type `T` of a `Channel<T>` used in a select case, or `Void` with a
    /// diagnostic if the operand is not a channel.
    fn select_channel_elem(&mut self, ch_ty: &Type, span: Span) -> Type {
        match channel_element_type(ch_ty) {
            Some(t) => t,
            None => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0807,
                        format!(
                            "select case requires a `Channel<T>`, found `{}`",
                            type_name(ch_ty)
                        ),
                    )
                    .with_label(Label::primary(span, "not a channel")),
                );
                Type::Void
            }
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

    fn lookup_static_prop_in_hierarchy(
        &self,
        class_name: &str,
        prop_name: &str,
    ) -> Option<(String, StaticPropInfo)> {
        let mut current = Some(class_name.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if !seen.insert(name.clone()) {
                return None;
            }
            let class = self.symbols.lookup_class(&name)?;
            if let Some(prop) = class.static_props.get(prop_name) {
                return Some((name, prop.clone()));
            }
            current = class.base_class.clone();
        }
        None
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
            Type::Named(name) if name == "AtomicI64" || name == "AtomicBool" => {
                // Compiler-known atomic primitives (willow-dgwo.3).
            }
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
    /// NOT applied to built-in non-enum generics like Channel, Future, Task, JoinHandle.
    fn generic_partially_matches(&self, expected: &Type, actual: &Type) -> bool {
        match (expected, actual) {
            (Type::Generic(en, eargs), Type::Generic(an, aargs)) if en == an => {
                // Only apply to registered generic enums (not Channel/Future/Task/JoinHandle)
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

    fn class_extends(&self, child: &str, parent: &str) -> bool {
        // Compare class identity by the registered (canonical) name so a
        // directly-imported subclass alias (`Dog`) is recognized as extending a
        // module base whose `base_class` is qualified (`shp::Animal`), and the
        // bare imported parent alias (`Animal`) matches it (willow-2egr).
        let canon = |n: &str| -> String {
            self.symbols
                .lookup_class(n)
                .map(|c| c.name.clone())
                .unwrap_or_else(|| n.to_string())
        };
        let parent_canon = canon(parent);
        let mut current = Some(child.to_string());
        let mut seen = HashSet::new();
        while let Some(name) = current {
            if canon(&name) == parent_canon {
                return true;
            }
            if !seen.insert(name.clone()) {
                return false;
            }
            let Some(class) = self.symbols.lookup_class(&name) else {
                return false;
            };
            let Some(base) = &class.base_class else {
                return false;
            };
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
    let mut static_props = HashMap::new();
    let mut instance_field_order: Vec<(String, Type)> = Vec::new();

    for (decl_index, field) in class.fields.iter().enumerate() {
        let ty = qualify_type_for_module(&field.ty, module_prefix);
        if field.is_static {
            static_props.insert(
                field.name.clone(),
                StaticPropInfo {
                    ty,
                    is_mut: field.is_mut,
                    public: field.public,
                    protected: field.protected,
                    decl_index,
                    declaration_span: field.span,
                },
            );
        } else {
            instance_field_order.push((field.name.clone(), ty.clone()));
            fields.insert(
                field.name.clone(),
                FieldInfo {
                    ty,
                    public: field.public,
                    protected: field.protected,
                    declaration_span: field.span,
                },
            );
        }
    }
    let constructor = class.constructors.first().map(|ctor| {
        let params = ctor
            .params
            .iter()
            .map(|p| qualify_type_for_module(&p.ty, module_prefix))
            .collect();
        crate::semantic::symbols::ConstructorInfo {
            param_infos: param_infos_from_decl(&ctor.params, module_prefix),
            params,
            public: ctor.public,
            protected: ctor.protected,
            declaration_span: ctor.span,
        }
    });
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
                is_static: method.is_static,
                is_async: method.is_async,
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
        static_props,
        instance_field_order,
        constructor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

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
    fn unit_async_yield_01_call_expression_typechecks_without_await() {
        assert_typecheck_ok(
            r#"
fn f() {
    yield();
}
"#,
        );
    }

    #[test]
    fn unit_async_yield_02_await_yield_in_async_function_typechecks() {
        assert_typecheck_ok(
            r#"
async fn f() {
    await yield();
}
"#,
        );
    }

    #[test]
    fn unit_async_yield_03_yield_rejects_argument() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    yield(1);
}
"#,
            ErrorCode::E0201,
            "function `yield` takes 0 argument(s) but 1 were supplied",
        );
    }

    #[test]
    fn unit_async_yield_04_await_yield_outside_async_is_rejected() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    await yield();
}
"#,
            ErrorCode::E0801,
            "`await` can only be used inside an async function",
        );
    }

    #[test]
    fn unit_async_yield_05_await_yield_cannot_initialize_i64() {
        assert_typecheck_error_contains(
            r#"
async fn f() {
    let value: i64 = await yield();
}
"#,
            ErrorCode::E0201,
            "mismatched types: expected `i64`, found `void`",
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
    let value = new Boxed(1);
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
    let node = new Node(1);
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
    let counter = new Counter(3);
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
    let counter = new Counter(3);
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
    let user = new User(42);
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
    let mut user = new User(1);
    let next = new User(2);
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
    let user = new User(1);
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
    let user = new User(1);
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
    let node = new Node(1);
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
    let box = new Box();
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
    let box = new Box();
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
    let box = new Box();
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
    let box = new Box();
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
    let box = new Box();
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
    let box = new Box();
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
    fn unit_for_loop_05_accepts_range_value_outside_for() {
        // A range is a first-class `Range<i64>` value; holding it (and reading
        // its `.start` / `.end` bounds) outside a `for` loop type-checks.
        assert_typecheck_ok(
            r#"
fn f() -> i64 {
    let r = 1..4;
    return r.end - r.start;
}
"#,
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
    let tail = new Node(2, nil);
    return new Node(1, tail);
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
    fn unit_nil_12b_while_not_nil_narrows_body() {
        assert_typecheck_ok(&format!(
            r#"
{NODE_CLASS}

fn sum(node: Node?) -> i64 {{
    let mut current: Node? = node;
    let mut total = 0;
    while current != nil {{
        total = total + current.value;
        current = current.next;
    }}
    return total;
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
    let a = new Animal();
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
    fn iface_18_static_method_cannot_satisfy_instance_requirement() {
        // With implicit `self`, a plain `fn speak()` IS an instance method and
        // satisfies the interface. Only a `static fn` (no receiver) is rejected
        // (willow-qsqf).
        assert_typecheck_error_contains(
            r#"
interface Animal { fn speak(self) -> String; }
class Dog implements Animal {
    pub static fn speak() -> String { return "woof"; }
}
"#,
            ErrorCode::E0416,
            "cannot satisfy instance method",
        );
    }

    #[test]
    fn iface_18b_implicit_self_method_satisfies_interface() {
        // A plain `fn` (implicit self) satisfies an interface instance method.
        assert_typecheck_ok(
            r#"
interface Animal { fn speak(self) -> String; }
class Dog implements Animal {
    pub fn speak() -> String { return "woof"; }
}
"#,
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
            "{ANIMAL_DOG}\nfn f() {{ let a: Animal = new Dog(); }}"
        ));
    }

    #[test]
    fn iface_25_class_coerces_as_function_argument() {
        assert_typecheck_ok(&format!(
            "{ANIMAL_DOG}\nfn say(a: Animal) {{}}\nfn f() {{ say(new Dog()); }}"
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
            "{ANIMAL_DOG}\nfn make() -> Animal {{ return new Dog(); }}"
        ));
    }

    #[test]
    fn iface_29_class_assignable_to_nullable_interface() {
        // spec 7.3.5: non-null Dog assignable to Animal?
        assert_typecheck_ok(&format!(
            "{ANIMAL_DOG}\nfn f() {{ let a: Animal? = new Dog(); }}"
        ));
    }

    #[test]
    fn iface_30_unrelated_class_not_assignable_to_interface() {
        assert_typecheck_error_contains(
            r#"
interface Animal { fn speak(self) -> String; }
class Rock {}
fn f() { let a: Animal = new Rock(); }
"#,
            ErrorCode::E0201,
            "expected `Animal`",
        );
    }

    #[test]
    fn iface_31_interface_field_accepts_class_value() {
        assert_typecheck_ok(&format!(
            "{ANIMAL_DOG}\nclass Holder {{ pub value: Animal; }}\nfn f() {{ let h = new Holder(new Dog()); }}"
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
fn f() { let h = new Holder(new Rock()); }
"#,
            ErrorCode::E0201,
            "expects `Animal`",
        );
    }

    #[test]
    fn iface_34_array_interface_push_accepts_class() {
        assert_typecheck_ok(&format!(
            "import std::collections::Array;\n{ANIMAL_DOG}\nfn f() {{ let xs: Array<Animal> = []; xs.push(new Dog()); }}"
        ));
    }

    #[test]
    fn iface_35_array_interface_index_returns_interface() {
        // Indexing an Array<Animal> yields an Animal, whose interface methods are callable.
        assert_typecheck_ok(&format!(
            "import std::collections::Array;\n{ANIMAL_DOG}\nfn f() -> String {{ let xs: Array<Animal> = []; xs.push(new Dog()); return xs[0].speak(); }}"
        ));
    }

    #[test]
    fn iface_36_nonempty_array_literal_with_interface_annotation() {
        // Differing classes that both implement the interface are accepted
        // element-wise against the annotation (willow-w8af).
        assert_typecheck_ok(&format!(
            "import std::collections::Array;\n{ANIMAL_DOG}\nclass Cat implements Animal {{ pub fn speak(self) -> String {{ return \"meow\"; }} }}\nfn f() {{ let xs: Array<Animal> = [new Dog(), new Cat()]; }}"
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
fn f() { let xs: Array<Animal> = [new Dog(), new Rock()]; }
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
