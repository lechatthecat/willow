pub(crate) mod analysis;
mod check;
mod check_calls;
mod check_collections;
mod check_concurrency;
mod check_decls;
mod check_lambda_match;
mod check_ops;
mod diagnostics;
mod resolve;
mod send_sync;
mod types;
pub(crate) use analysis::*;
#[cfg(test)]
use check::check_source;
pub(crate) use check::defer_body_contains_direct_recover;
use diagnostics::*;
pub(crate) use types::*;

use super::symbols::{ClassInfo, FieldInfo, MethodInfo, ParamInfo, StaticPropInfo, SymbolTable};
use crate::diagnostics::{Diagnostic, ErrorCode, FixSuggestion, Label, Severity, Span};
use crate::module::std_registry;
use crate::parser::ast::*;
use crate::semantic::concurrency::{NonpreemptibleHelper, NonpreemptibleReason};
use crate::semantic::ids::{FunctionId, TypeId};
use crate::stdlib_schema;
use std::collections::{HashMap, HashSet};

pub struct TypeChecker {
    pub symbols: SymbolTable,
    pub errors: Vec<Diagnostic>,
    /// Nesting depth of enclosing loops; `break`/`continue` outside a loop is
    /// E0904. Reset to 0 inside a lambda body (a loop outside the lambda is
    /// not breakable from within it) (willow-kzka).
    pub(crate) loop_depth: u32,
    /// Nesting depth of enclosing `lock` statements. V1 rejects a `lock` inside
    /// another lock's critical section (E2605), and the depth also keeps the
    /// await scan from reporting the same `await` once per enclosing lock
    /// (willow-38w.1.1).
    pub(crate) lock_depth: u32,
    /// Lexical statement-block depth within the current function-like body.
    /// The outer function/method body is depth 1. Reset for lambdas so recovery
    /// capability cannot cross a function boundary (willow-s9ej.3).
    pub(crate) lexical_block_depth: u32,
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
    /// Maps the span of an UNQUALIFIED enum-variant construction (`Ok(42)` in an
    /// expected-enum position) to the enum it resolved to. The backend consults
    /// this to lower such a `Call` as a variant allocation instead of a function
    /// call (willow-60o.1). The variant name is the call's callee.
    pub enum_variant_resolutions: HashMap<Span, String>,
    /// Maps an unqualified match-pattern span (`Ok(v)` / `Closed`, which parse as
    /// `ClassDowncast` / `Binding`) to the enum-variant pattern it was
    /// reinterpreted as when the scrutinee is an enum. The backend consults this
    /// to lower the arm as a variant match (willow-60o.1).
    pub pattern_resolutions: HashMap<Span, Pattern>,
    /// The resolved type of every checked expression, keyed by its span. The
    /// authoritative record for consumers (HIR lowering) that must not
    /// re-derive types from the AST (willow-mb5 checker pivot).
    pub expr_types: HashMap<Span, Type>,
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
    /// Directly imported synchronous std::fs functions, keyed by their local
    /// alias. These block a native worker and therefore participate in E2604.
    imported_blocking_std_functions: HashMap<String, &'static str>,
    /// Suppress duplicate missing-import diagnostics per type name.
    missing_collection_imports_reported: HashSet<String>,
    /// Enforce the Send/Sync async checks (E2402-E2405). Off by default for the
    /// multi-worker target; enabled by the five-worker default or explicitly
    /// via WILLOW_DATA_RACE_CHECK
    /// (willow-dgwo.4/.9).
    enforce_send_sync: bool,
    /// Synchronous helpers that contain or transitively reach a loop, or that
    /// belong to or reach a recursive call cycle, keyed by `Class::method` (and
    /// bare fn names), with the helper's definition span and the reason. Used
    /// to flag such a method called through a typed NON-`self` receiver from a
    /// task context (E0810) — the AST-only `ConcurrencyAnalyzer` cannot resolve
    /// such a receiver's class (willow-0a6k.2).
    nonpreemptible_methods: HashMap<FunctionId, NonpreemptibleHelper>,
    /// Non-preemptible methods of IMPORTED classes, keyed by the receiver class
    /// name as the type checker sees it (`module::Class::method` for a
    /// whole-module import, `Local::method` for a direct class import), mapped
    /// to the source module's display name and the reason. Seeded from
    /// `main.rs` before `check_program`. The helper's definition span lives in
    /// another file the entry diagnostic map cannot render, so the E0810 uses a
    /// note instead of a secondary label (willow-0a6k.2).
    nonpreemptible_module_methods: HashMap<FunctionId, (String, NonpreemptibleReason)>,
    /// The function-like body currently being checked. Lambdas deliberately
    /// clear this: they are separate callable values and are not reached by a
    /// named user-call edge.
    current_effect_callable: Option<FunctionId>,
    /// Same-program functions and methods known to the transitive lock-effect
    /// analysis. The value is `true` for async callables: invoking one only
    /// creates a `Task`, so a bare call must not propagate the callee's waits
    /// into the caller (willow-38w.1.4).
    lock_effect_callables: HashMap<FunctionId, bool>,
    /// Typed, same-program calls made by each named callable. Only synchronous
    /// callees are entered, because those execute before returning to the
    /// caller and can therefore wait while its lock remains held.
    lock_effect_edges: HashMap<FunctionId, HashSet<FunctionId>>,
    /// A direct blocking/suspending operation in each callable. One witness is
    /// enough to reject every lock-held call that reaches it.
    lock_direct_effects: HashMap<FunctionId, LockEffectCause>,
    /// Direct effects physically written in a critical section. The legacy
    /// typed syntax walker still reports await/select/channel sites; this list
    /// additionally covers blocking stdlib calls and is deduplicated by span.
    lock_direct_effect_callsites: Vec<LockEffectCause>,
    /// Calls written in a lock body. They are diagnosed after all bodies have
    /// been checked and the call graph fixpoint is complete, which also covers
    /// forward declarations and recursion.
    lock_effect_callsites: Vec<LockEffectCallsite>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockEffectKind {
    Suspend,
    Block,
    /// A separately compiled/imported synchronous implementation whose body
    /// is not available to this checker. Treat it conservatively as capable of
    /// either kind of wait instead of silently weakening E2604.
    SuspendOrBlock,
}

#[derive(Debug, Clone, Copy)]
struct LockEffectCause {
    span: Span,
    operation: &'static str,
    kind: LockEffectKind,
}

#[derive(Debug, Clone)]
struct LockEffectCallsite {
    callee: FunctionId,
    span: Span,
}

/// Internal callable identity for a non-generic interface default body. The
/// `$default$` spelling cannot be written as a Willow method name, so it cannot
/// collide with a source declaration. Keeping this distinct from the interface
/// dispatch union means an unused default (all implementations override it)
/// does not taint the interface method.
fn interface_default_effect_id(interface: &str, method: &str) -> FunctionId {
    FunctionId::method(
        TypeId::from_source_name(interface),
        format!("$default${method}"),
    )
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
    /// Set when the place is immutable because of what it *is* rather than how
    /// it was declared (a `PanicInfo` field). `&mut` on such a place is
    /// reported with this message instead of the "declare it `mut`" advice,
    /// which would be unactionable (willow-s9ej.7).
    immutable_reason: Option<&'static str>,
}

impl Default for TypeChecker {
    fn default() -> Self {
        Self::new()
    }
}

impl TypeChecker {
    pub fn new() -> Self {
        let mut checker = Self {
            symbols: SymbolTable::default(),
            errors: Vec::new(),
            loop_depth: 0,
            lock_depth: 0,
            lexical_block_depth: 0,
            lambda_return_types: HashMap::new(),
            lambda_fn_types: HashMap::new(),
            async_local_types: HashMap::new(),
            enum_variant_resolutions: HashMap::new(),
            pattern_resolutions: HashMap::new(),
            expr_types: HashMap::new(),
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
            imported_blocking_std_functions: HashMap::new(),
            missing_collection_imports_reported: HashSet::new(),
            enforce_send_sync: false,
            nonpreemptible_methods: HashMap::new(),
            nonpreemptible_module_methods: HashMap::new(),
            current_effect_callable: None,
            lock_effect_callables: HashMap::new(),
            lock_effect_edges: HashMap::new(),
            lock_direct_effects: HashMap::new(),
            lock_direct_effect_callsites: Vec::new(),
            lock_effect_callsites: Vec::new(),
        };
        checker.register_builtin_functions();
        checker.register_builtin_modules();
        checker.register_builtin_panic_surface();
        checker
    }

    /// Enable the Send/Sync async checks. Turned on when targeting multi-worker
    /// execution (willow-dgwo.4/.9).
    pub fn set_enforce_send_sync(&mut self, on: bool) {
        self.enforce_send_sync = on;
    }

    /// Seed the non-preemptible methods of imported classes (keyed by receiver
    /// class name -> source module display name and reason) so a cross-module
    /// typed-receiver call to one is flagged E0810 (willow-0a6k.2). Call before
    /// `check_program`.
    pub fn set_nonpreemptible_module_methods(
        &mut self,
        methods: HashMap<FunctionId, (String, NonpreemptibleReason)>,
    ) {
        self.nonpreemptible_module_methods = methods;
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
                    self.normalize_std_type_item(name, &module, &item, Vec::new(), span)
                } else if let Some((module, item)) =
                    self.resolve_imported_std_module_item(name, span)
                {
                    self.normalize_std_type_item(name, &module, &item, Vec::new(), span)
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
        let Some((expected, builtin_name)) = stdlib_schema::type_item(module, item) else {
            return Type::Generic(source_name.to_string(), args);
        };
        if args.len() != expected {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "`{source_name}` expects {expected} type argument{}, got {}",
                        if expected == 1 { "" } else { "s" },
                        args.len()
                    ),
                )
                .with_label(Label::primary(span, "wrong number of type arguments")),
            );
            // Do not manufacture a malformed builtin generic and let later
            // phases index missing arguments. The diagnostic above owns this
            // expression; Void is the compiler's established fail-safe type.
            return Type::Void;
        }

        if expected == 0 {
            Type::Named(builtin_name.to_string())
        } else if builtin_name == "Array" {
            Type::Array(Box::new(
                args.into_iter().next().expect("validated Array arity"),
            ))
        } else {
            Type::Generic(builtin_name.to_string(), args)
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
                    immutable_reason: None,
                })
            }
            Expr::FieldAccess(obj, field_name, span) => {
                let obj_ty = self.check_expr(obj);
                let field_ty = self.resolve_field(&obj_ty, field_name, *span, true);
                if matches!(field_ty, Type::Void) {
                    return None;
                }
                // `PanicInfo` is runtime-owned panic metadata: direct assignment
                // is already rejected, and `&mut info.line` must not be a way
                // around that (willow-s9ej.7).
                let panic_info_field = obj_ty == Type::Named("PanicInfo".to_string());
                Some(ReferencePlaceInfo {
                    name: reference_place_key(expr).unwrap_or_else(|| field_name.clone()),
                    ty: field_ty,
                    mutable: !panic_info_field,
                    is_param: false,
                    declaration_span: *span,
                    immutable_reason: panic_info_field
                        .then_some("fields of `PanicInfo` are read-only"),
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
                    immutable_reason: None,
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
            if let Some(narrowed) = scope.get(name)
                && narrowed.declaration_span == declaration_span
            {
                return Some(narrowed.ty.clone());
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
            Type::Named(name)
                if matches!(
                    name.as_str(),
                    "AtomicI64"
                        | "AtomicBool"
                        | "TcpListener"
                        | "TcpStream"
                        | "CancellationToken"
                        | "TaskScope"
                        | "Cancelled"
                ) =>
            {
                // Compiler-known runtime primitives. TCP handles are opaque,
                // GC-managed objects constructed only by `std::net`.
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

    macro_rules! for_loop_ok_case {
        ($name:ident, $perspective:literal, $source:expr $(,)?) => {
            #[test]
            fn $name() {
                // Perspective: keep the user-facing for-loop coverage matrix
                // explicit in test output/source, not just in a checklist.
                let _perspective = $perspective;
                assert_typecheck_ok($source);
            }
        };
    }

    macro_rules! for_loop_error_case {
        ($name:ident, $perspective:literal, $source:expr, $code:expr, $message:expr $(,)?) => {
            #[test]
            fn $name() {
                // Perspective: keep the user-facing for-loop coverage matrix
                // explicit in test output/source, not just in a checklist.
                let _perspective = $perspective;
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

    // Builtin static constructors have no `ParamInfo` to check the argument
    // mode against, so each one rejects `&arg` explicitly (willow-o038 review).
    #[test]
    fn unit_builtin_ref_01_channel_with_capacity_rejects_reference_argument() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let n = 2;
    let ch = Channel<i64>::with_capacity(&n);
}
"#,
            ErrorCode::E1703,
            "unexpected reference argument",
        );
    }

    #[test]
    fn unit_builtin_ref_02_channel_with_capacity_accepts_a_value_argument() {
        assert_typecheck_ok(
            r#"
fn f() {
    let n = 2;
    let ch = Channel<i64>::with_capacity(n);
}
"#,
        );
    }

    #[test]
    fn unit_builtin_ref_03_mutex_new_rejects_reference_argument() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let v = 1;
    let m = Mutex<i64>::new(&v);
}
"#,
            ErrorCode::E1703,
            "unexpected reference argument",
        );
    }

    #[test]
    fn unit_builtin_ref_04_rwlock_new_rejects_reference_argument() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let v = 1;
    let l = RwLock<i64>::new(&v);
}
"#,
            ErrorCode::E1703,
            "unexpected reference argument",
        );
    }

    #[test]
    fn unit_builtin_ref_05_atomic_i64_new_rejects_reference_argument() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let v = 1;
    let a = AtomicI64::new(&v);
}
"#,
            ErrorCode::E1703,
            "unexpected reference argument",
        );
    }

    #[test]
    fn unit_builtin_ref_06_atomic_bool_new_rejects_reference_argument() {
        assert_typecheck_error_contains(
            r#"
fn f() {
    let v = true;
    let a = AtomicBool::new(&v);
}
"#,
            ErrorCode::E1703,
            "unexpected reference argument",
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

    // ── 50 for-loop unit perspectives (willow-u39v) ─────────────────────────
    // Range basics and static loop-variable rules.

    for_loop_ok_case!(
        unit_for_matrix_01_basic_range,
        "1. `for i in 0..3` basic range iteration shape type-checks",
        r#"
fn f() -> i64 {
    let mut total = 0;
    for i in 0..3 {
        total = total + i;
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_02_zero_range,
        "2. `0..0` is a valid empty range loop",
        r#"
fn f() {
    for i in 0..0 {
        println(i);
    }
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_03_reversed_range,
        "3. `5..3` is a valid no-iteration range loop",
        r#"
fn f() {
    for i in 5..3 {
        println(i);
    }
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_04_negative_start,
        "4. negative range start is accepted",
        r#"
fn f() -> i64 {
    let mut total = 0;
    for i in -3..2 {
        total = total + i;
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_05_negative_end,
        "5. negative range end is accepted",
        r#"
fn f() -> i64 {
    let mut total = 0;
    for i in -5..-1 {
        total = total + i;
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_06_large_range,
        "6. large range bounds remain ordinary i64 loop bounds",
        r#"
fn f() -> i64 {
    let mut total = 0;
    for i in 0..10000 {
        total = total + i;
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_07_range_bound_expressions,
        "7. range start/end may be non-literal i64 expressions",
        r#"
fn start() -> i64 { return 1; }
fn stop() -> i64 { return 4; }
fn f() -> i64 {
    let mut total = 0;
    for i in start()..stop() {
        total = total + i;
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_08_range_bound_expression_order_shape,
        "8. distinct range bound calls type-check as the iterable expression",
        r#"
fn left() -> i64 { return 0; }
fn right() -> i64 { return 3; }
fn f() {
    for i in left()..right() {
        println(i);
    }
}
"#,
    );

    for_loop_error_case!(
        unit_for_matrix_09_loop_var_scope,
        "9. loop variable is scoped to the loop body",
        r#"
fn f() {
    for i in 0..3 {
        println(i);
    }
    println(i);
}
"#,
        ErrorCode::E0350,
        "cannot find variable `i`",
    );

    for_loop_error_case!(
        unit_for_matrix_10_loop_var_immutable,
        "10. loop variable is immutable",
        r#"
fn f() {
    for i in 0..3 {
        i = i + 1;
    }
}
"#,
        ErrorCode::E0301,
        "cannot assign to immutable variable `i`",
    );

    for_loop_ok_case!(
        unit_for_matrix_11_loop_var_shadows_outer,
        "11. loop variable may shadow an outer variable without corrupting it",
        r#"
fn f() -> i64 {
    let i = 10;
    let mut total = 0;
    for i in 0..3 {
        total = total + i;
    }
    return total + i;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_12_nested_range_loop_vars,
        "12. nested range loops keep independent loop variables",
        r#"
fn f() -> i64 {
    let mut total = 0;
    for i in 0..2 {
        for j in 0..2 {
            total = total + i + j;
        }
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_13_underscore_binding,
        "13. `_` binding runs the body without introducing a visible variable",
        r#"
fn f() -> i64 {
    let mut count = 0;
    for _ in 0..3 {
        count = count + 1;
    }
    return count;
}
"#,
    );

    for_loop_error_case!(
        unit_for_matrix_14_rejects_non_iterable,
        "14. non-array/non-range iterable is rejected",
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

    for_loop_error_case!(
        unit_for_matrix_15_rejects_non_i64_range_bounds,
        "15. range endpoint types must be i64",
        r#"
fn f() {
    for value in 0.5..4 {
        println(value);
    }
}
"#,
        ErrorCode::E0201,
        "range bounds must be `i64`",
    );

    // Array element typing and mutation-during-iteration shapes.

    for_loop_ok_case!(
        unit_for_matrix_16_array_i64,
        "16. Array<i64> element type flows into the body",
        r#"
import std::collections::Array;

fn f() -> i64 {
    let xs: Array<i64> = [1, 2, 3];
    let mut total = 0;
    for x in xs {
        total = total + x;
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_17_array_string,
        "17. Array<String> iteration preserves GC-managed element type",
        r#"
import std::collections::Array;

fn f() -> String {
    let xs: Array<String> = ["a", "b"];
    let mut out = "";
    for x in xs {
        out = out + x;
    }
    return out;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_18_array_f64,
        "18. Array<f64> iteration preserves f64 element type",
        r#"
import std::collections::Array;

fn f() -> f64 {
    let xs: Array<f64> = [0.5, 1.25];
    let mut total = 0.0;
    for x in xs {
        total = total + x;
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_19_array_bool,
        "19. Array<bool> iteration preserves bool element type",
        r#"
import std::collections::Array;

fn f() -> i64 {
    let xs: Array<bool> = [true, false, true];
    let mut count = 0;
    for b in xs {
        if b {
            count = count + 1;
        }
    }
    return count;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_20_empty_array,
        "20. empty array is a valid for-loop iterable",
        r#"
import std::collections::Array;

fn f() -> i64 {
    let xs: Array<i64> = [];
    let mut count = 0;
    for x in xs {
        count = count + x;
    }
    return count;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_21_single_element_array,
        "21. single-element array loop binds one element type",
        r#"
import std::collections::Array;

fn f() -> i64 {
    let xs: Array<i64> = [7];
    let mut total = 0;
    for x in xs {
        total = total + x;
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_22_array_order_shape,
        "22. array loop body may depend on source-order element type",
        r#"
import std::collections::Array;

fn f() -> i64 {
    let xs: Array<i64> = [3, 1, 2];
    let mut total = 0;
    for x in xs {
        total = total * 10 + x;
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_23_loop_var_is_copy_shape,
        "23. loop variable can be used after mutating the backing array slot",
        r#"
import std::collections::Array;

fn f() -> i64 {
    let xs: Array<i64> = [5, 6];
    let mut total = 0;
    for x in xs {
        xs[0] = 99;
        total = total + x;
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_24_push_during_array_iteration,
        "24. array push inside the loop body is an accepted mutation shape",
        r#"
import std::collections::Array;

fn f() -> i64 {
    let xs: Array<i64> = [1];
    let mut count = 0;
    for x in xs {
        count = count + 1;
        if count < 3 {
            xs.push(x + 1);
        }
    }
    return count;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_25_pop_during_array_iteration,
        "25. array pop inside the loop body is an accepted mutation shape",
        r#"
import std::collections::Array;

fn f() -> i64 {
    let xs: Array<i64> = [1, 2, 3, 4];
    let mut count = 0;
    for _ in xs {
        count = count + 1;
        xs.pop();
    }
    return count;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_26_growth_reallocation_shape,
        "26. repeated push during iteration type-checks across potential growth",
        r#"
import std::collections::Array;

fn f() -> i64 {
    let xs: Array<i64> = [1];
    let mut i = 0;
    for x in xs {
        if i < 8 {
            xs.push(x + 1);
        }
        i = i + 1;
    }
    return xs.len();
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_27_two_sequential_loops,
        "27. the same array can be iterated by two sequential loops",
        r#"
import std::collections::Array;

fn f() -> i64 {
    let xs: Array<i64> = [1, 2, 3];
    let mut a = 0;
    for x in xs { a = a + x; }
    let mut b = 0;
    for x in xs { b = b + x * 2; }
    return a + b;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_28_nested_same_array,
        "28. nested loops over the same array keep independent element bindings",
        r#"
import std::collections::Array;

fn f() -> i64 {
    let xs: Array<i64> = [1, 2];
    let mut total = 0;
    for a in xs {
        for b in xs {
            total = total + a * b;
        }
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_29_array_class_elements,
        "29. Array<Class> iteration exposes class fields/methods",
        r#"
import std::collections::Array;

class Cell {
    pub v: i64;
}

fn f() -> i64 {
    let xs: Array<Cell> = [new Cell(1), new Cell(2)];
    let mut total = 0;
    for cell in xs {
        total = total + cell.v;
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_30_array_interface_elements,
        "30. Array<Interface> iteration exposes interface methods",
        r#"
import std::collections::Array;

interface Thing {
    fn value(self) -> i64;
}

class Widget implements Thing {
    pub v: i64;
    pub fn value(self) -> i64 { return self.v; }
}

fn f() -> i64 {
    let xs: Array<Thing> = [new Widget(3)];
    let mut total = 0;
    for x in xs {
        total = total + x.value();
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_31_array_option_elements,
        "31. Array<Option<T>> iteration supports matching each element",
        r#"
import std::collections::Array;

enum Option<T> { Some(T), None, }

fn f() -> i64 {
    let missing: Option<i64> = Option::None;
    let xs: Array<Option<i64>> = [Option::Some(1), missing];
    let mut total = 0;
    for item in xs {
        match item {
            Option::Some(v) => { total = total + v; }
            Option::None => {}
        }
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_32_array_result_elements,
        "32. Array<Result<T,E>> iteration supports matching each element",
        r#"
import std::collections::Array;

enum Result<T, E> { Ok(T), Err(E), }

fn f() -> i64 {
    let bad: Result<i64, String> = Result::Err("bad");
    let xs: Array<Result<i64, String>> = [Result::Ok(1), bad];
    let mut total = 0;
    for item in xs {
        match item {
            Result::Ok(v) => { total = total + v; }
            Result::Err(_) => {}
        }
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_33_nested_array_elements,
        "33. Array<Array<i64>> iteration preserves nested array type",
        r#"
import std::collections::Array;

fn f() -> i64 {
    let a: Array<i64> = [1];
    let b: Array<i64> = [2, 3];
    let xs: Array<Array<i64>> = [a, b];
    let mut total = 0;
    for inner in xs {
        total = total + inner.len();
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_34_fresh_array_expression,
        "34. a freshly returned array expression may be iterated directly",
        r#"
import std::collections::Array;

fn make() -> Array<i64> {
    return [4, 5];
}

fn f() -> i64 {
    let mut total = 0;
    for x in make() {
        total = total + x;
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_35_method_call_iterable,
        "35. a method call result may be the for-loop iterable",
        r#"
import std::collections::Array;

class Bag {
    pub fn values(self) -> Array<i64> {
        return [1, 2, 3];
    }
}

fn f() -> i64 {
    let bag = new Bag();
    let mut total = 0;
    for x in bag.values() {
        total = total + x;
    }
    return total;
}
"#,
    );

    // break/continue legality and nesting.

    for_loop_ok_case!(
        unit_for_matrix_36_break_range,
        "36. break exits a range for-loop",
        r#"
fn f() -> i64 {
    let mut total = 0;
    for i in 0..10 {
        if i == 4 { break; }
        total = total + i;
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_37_break_array,
        "37. break exits an array for-loop",
        r#"
import std::collections::Array;

fn f() -> i64 {
    let xs: Array<i64> = [1, 2, 3, 4];
    let mut total = 0;
    for x in xs {
        if x == 3 { break; }
        total = total + x;
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_38_nested_break_inner_only_shape,
        "38. nested break targets the innermost loop",
        r#"
fn f() -> i64 {
    let mut count = 0;
    for a in 0..3 {
        for b in 0..10 {
            if b == 2 { break; }
            count = count + a + b;
        }
    }
    return count;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_39_continue_range,
        "39. continue is legal in a range for-loop",
        r#"
fn f() -> i64 {
    let mut total = 0;
    for i in 0..5 {
        if i == 2 { continue; }
        total = total + i;
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_40_continue_array,
        "40. continue is legal in an array for-loop",
        r#"
import std::collections::Array;

fn f() -> i64 {
    let xs: Array<i64> = [1, 2, 3, 4];
    let mut total = 0;
    for x in xs {
        if x == 2 { continue; }
        total = total + x;
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_41_nested_continue_inner_only_shape,
        "41. nested continue targets the innermost loop",
        r#"
fn f() -> i64 {
    let mut count = 0;
    for a in 0..2 {
        for b in 0..4 {
            if b == 1 { continue; }
            count = count + a + b;
        }
    }
    return count;
}
"#,
    );

    for_loop_error_case!(
        unit_for_matrix_42_break_outside_loop,
        "42. break outside a loop is rejected",
        r#"
fn f() {
    break;
}
"#,
        ErrorCode::E0904,
        "`break` outside of a loop",
    );

    for_loop_error_case!(
        unit_for_matrix_43_continue_outside_loop,
        "43. continue outside a loop is rejected",
        r#"
fn f() {
    continue;
}
"#,
        ErrorCode::E0904,
        "`continue` outside of a loop",
    );

    for_loop_error_case!(
        unit_for_matrix_44_lambda_is_loop_boundary,
        "44. lambda body cannot break an enclosing for-loop",
        r#"
fn f() {
    for i in 0..3 {
        let g = || { break; return 1; };
    }
}
"#,
        ErrorCode::E0904,
        "`break` outside of a loop",
    );

    for_loop_ok_case!(
        unit_for_matrix_45_break_continue_under_if,
        "45. break/continue remain legal under if branches inside a loop",
        r#"
fn f() -> i64 {
    let mut total = 0;
    for i in 0..10 {
        if i == 2 {
            continue;
        } else {
            total = total + i;
        }
        if total > 10 {
            break;
        }
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_46_break_continue_inside_match_arm,
        "46. break/continue remain legal inside match arms in a loop",
        r#"
enum Signal { Go, Skip, Stop, }

fn f() -> i64 {
    let mut total = 0;
    for i in 0..10 {
        let signal = i == 2 ? Signal::Skip : (i == 5 ? Signal::Stop : Signal::Go);
        match signal {
            Go => { total = total + i; }
            Skip => { continue; }
            Stop => { break; }
        }
    }
    return total;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_47_break_and_return_same_body,
        "47. return and break may both appear in the same loop body",
        r#"
fn f(flag: bool) -> i64 {
    for i in 0..10 {
        if flag { return 100; }
        if i == 3 { break; }
    }
    return 1;
}
"#,
    );

    // GC-managed locals and async for-loop shapes.

    for_loop_ok_case!(
        unit_for_matrix_48_gc_local_then_break,
        "48. GC-managed locals before break type-check in a for-loop",
        r#"
import std::collections::Array;

fn f() -> String {
    let xs: Array<String> = ["a", "b", "c"];
    let mut out = "";
    for s in xs {
        let t = s + "!";
        out = out + t;
        if out != "" { break; }
    }
    return out;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_49_gc_local_then_continue,
        "49. GC-managed locals before continue type-check in a for-loop",
        r#"
import std::collections::Array;

fn f() -> String {
    let xs: Array<String> = ["a", "b"];
    let mut out = "";
    for s in xs {
        let t = s + "!";
        if t != "" { continue; }
        out = out + t;
    }
    return out;
}
"#,
    );

    for_loop_ok_case!(
        unit_for_matrix_50_async_for_await_continue,
        "50. async for-loop may combine await and continue",
        r#"
async fn f() -> i64 {
    let mut total = 0;
    for i in 0..5 {
        if i == 2 { continue; }
        await sleep(0);
        total = total + i;
    }
    return total;
}
"#,
    );

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

    // ── Interface method parameter MODES (willow-0g8j.9)
    //
    // A `&`/`&mut` parameter is passed as a POINTER, so the mode is part of an
    // interface method's ABI, not decoration. Dropping it let a by-value
    // implementation satisfy a reference requirement and let a call site hand
    // the callee an integer to dereference — a segfault on both backends.
    // Perspectives 1..18 below cover conformance, call sites and the places
    // where a signature is rendered or instantiated; 19..30 continue in the
    // LIR eligibility tests (`k24`..`k27`) and the codegen differentials.
    //
    //  1 matching `&mut` conforms · 2 value impl vs `&mut` requirement ·
    //  3 `&mut` impl vs value requirement · 4 `&` impl vs `&mut` requirement ·
    //  5 `&mut` impl vs `&` requirement · 6 missing `&` at a call site ·
    //  7 correct `&` at a call site · 8 stray `&` on a value parameter ·
    //  9 `&mut` needs a mutable place · 10 `&` accepts an immutable one ·
    //  11 reference argument type mismatch · 12 two `&mut` args aliasing ·
    //  13 a missing-method diagnostic renders the mode · 14 a generic
    //  interface keeps modes through instantiation · 15 an inherited
    //  (`extends`) requirement keeps its mode · 16 a default-bodied method
    //  keeps its mode · 17 `&mut` on a field place · 18 `&mut` on an array
    //  element.

    const MUTATOR: &str = r#"
interface Mutator {
    fn bump(self, value: &mut i64);
}
"#;

    /// 1. The implementing method declares the same mode: conformance holds and
    ///    the call site passes a reference.
    #[test]
    fn ifacemode_01_matching_reference_parameter_conforms() {
        assert_typecheck_ok(&format!(
            r#"{MUTATOR}
class Adder implements Mutator {{
    pub n: i64;
    pub fn bump(self, value: &mut i64) {{ value = value + self.n; }}
}}
fn f() {{
    let m: Mutator = new Adder(1);
    let mut x = 0;
    m.bump(&x);
}}
"#
        ));
    }

    /// 2. A by-value implementation cannot satisfy a `&mut` requirement: the
    ///    vtable slot would receive a pointer and use it as an integer.
    #[test]
    fn ifacemode_02_value_impl_rejected_for_reference_requirement() {
        assert_typecheck_error_contains(
            &format!(
                r#"{MUTATOR}
class Adder implements Mutator {{
    pub n: i64;
    pub fn bump(self, value: i64) {{ println(value + self.n); }}
}}
"#
            ),
            ErrorCode::E0416,
            "parameters do not match interface `Mutator`",
        );
    }

    /// 3. The other direction is just as wrong: the interface promises a value
    ///    and the class dereferences it.
    #[test]
    fn ifacemode_03_reference_impl_rejected_for_value_requirement() {
        assert_typecheck_error_contains(
            r#"
interface Sink { fn take(self, value: i64); }
class Store implements Sink {
    pub fn take(self, value: &mut i64) { value = value + 1; }
}
"#,
            ErrorCode::E0416,
            "parameters do not match interface `Sink`",
        );
    }

    /// 4. `&` and `&mut` are both pointers, but they are not interchangeable:
    ///    an immutable-reference implementation cannot promise mutation.
    #[test]
    fn ifacemode_04_shared_impl_rejected_for_mut_requirement() {
        assert_typecheck_error_contains(
            &format!(
                r#"{MUTATOR}
class Adder implements Mutator {{
    pub fn bump(self, value: & i64) {{ println(value); }}
}}
"#
            ),
            ErrorCode::E0416,
            "parameters do not match interface `Mutator`",
        );
    }

    /// 5. And a `&mut` implementation may not widen a `&` requirement, which
    ///    would let it write through a caller's read-only place.
    #[test]
    fn ifacemode_05_mut_impl_rejected_for_shared_requirement() {
        assert_typecheck_error_contains(
            r#"
interface Reader { fn read(self, value: & i64) -> i64; }
class Peek implements Reader {
    pub fn read(self, value: &mut i64) -> i64 { value = value + 1; return value; }
}
"#,
            ErrorCode::E0416,
            "parameters do not match interface `Reader`",
        );
    }

    /// 6. The call site is checked too: without `&` the argument would be
    ///    passed by value into a parameter the callee dereferences.
    #[test]
    fn ifacemode_06_call_without_ampersand_rejected() {
        assert_typecheck_error_contains(
            &format!(
                r#"{MUTATOR}
class Adder implements Mutator {{
    pub fn bump(self, value: &mut i64) {{ value = value + 1; }}
}}
fn f() {{
    let m: Mutator = new Adder();
    let mut x = 0;
    m.bump(x);
}}
"#
            ),
            ErrorCode::E1702,
            "expected reference argument for reference parameter",
        );
    }

    /// 7. Passing a reference to a mutable local is the accepted spelling.
    #[test]
    fn ifacemode_07_call_with_ampersand_accepted() {
        assert_typecheck_ok(&format!(
            r#"{MUTATOR}
class Adder implements Mutator {{
    pub fn bump(self, value: &mut i64) {{ value = value + 1; }}
}}
fn f() {{
    let m: Mutator = new Adder();
    let mut x = 0;
    m.bump(&x);
    println(x);
}}
"#
        ));
    }

    /// 8. The mirror image: `&` written at a call site whose interface
    ///    parameter is by value used to be silently dropped.
    #[test]
    fn ifacemode_08_stray_ampersand_on_value_parameter_rejected() {
        assert_typecheck_error_contains(
            r#"
interface Sink { fn take(self, value: i64); }
class Store implements Sink { pub fn take(self, value: i64) { println(value); } }
fn f() {
    let s: Sink = new Store();
    let mut x = 0;
    s.take(&x);
}
"#,
            ErrorCode::E1703,
            "unexpected reference argument",
        );
    }

    /// 9. A `&mut` argument needs a MUTABLE place, exactly as on a direct call.
    #[test]
    fn ifacemode_09_mut_reference_requires_mutable_place() {
        let errors = check_source(&format!(
            r#"{MUTATOR}
class Adder implements Mutator {{
    pub fn bump(self, value: &mut i64) {{ value = value + 1; }}
}}
fn f() {{
    let m: Mutator = new Adder();
    let x = 0;
    m.bump(&x);
}}
"#
        ));
        assert!(
            !errors.is_empty(),
            "an immutable local must not satisfy `&mut`"
        );
    }

    /// 10. A `&` parameter has no such requirement: reading an immutable local
    ///     through the interface is fine.
    #[test]
    fn ifacemode_10_shared_reference_accepts_immutable_place() {
        assert_typecheck_ok(
            r#"
interface Reader { fn read(self, value: & i64) -> i64; }
class Peek implements Reader { pub fn read(self, value: & i64) -> i64 { return value; } }
fn f() {
    let r: Reader = new Peek();
    let x = 7;
    println(r.read(&x));
}
"#,
        );
    }

    /// 11. The referenced place still has to have the parameter's type.
    #[test]
    fn ifacemode_11_reference_argument_type_mismatch_rejected() {
        let errors = check_source(&format!(
            r#"{MUTATOR}
class Adder implements Mutator {{
    pub fn bump(self, value: &mut i64) {{ value = value + 1; }}
}}
fn f() {{
    let m: Mutator = new Adder();
    let mut s = "text";
    m.bump(&s);
}}
"#
        ));
        assert!(
            !errors.is_empty(),
            "a `&mut String` place must not satisfy `&mut i64`"
        );
    }

    /// 12. Two `&mut` arguments naming the same place alias through the call —
    ///     the same rule a direct call enforces.
    #[test]
    fn ifacemode_12_aliasing_mut_references_rejected() {
        let errors = check_source(
            r#"
interface Pair { fn swap(self, a: &mut i64, b: &mut i64); }
class Swapper implements Pair {
    pub fn swap(self, a: &mut i64, b: &mut i64) { a = a + b; }
}
fn f() {
    let p: Pair = new Swapper();
    let mut x = 1;
    p.swap(&x, &x);
}
"#,
        );
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("aliases a mutable reference")),
            "expected an aliasing diagnostic, got {errors:?}"
        );
    }

    /// 13. A missing-method diagnostic prints the required signature; the mode
    ///     belongs in it, or the reader cannot tell what to declare.
    #[test]
    fn ifacemode_13_missing_method_diagnostic_shows_mode() {
        let errors = check_source(&format!(
            r#"{MUTATOR}
class Empty implements Mutator {{}}
"#
        ));
        assert!(
            errors.iter().any(|e| e.code == ErrorCode::E0415
                && e.labels
                    .iter()
                    .any(|l| l.message.contains("bump(self, &mut i64)"))),
            "the required signature must show the mode, got {errors:?}"
        );
    }

    /// 14. Instantiating a generic interface substitutes TYPES; the passing
    ///     mode is unaffected and must survive.
    #[test]
    fn ifacemode_14_generic_interface_keeps_mode_through_instantiation() {
        assert_typecheck_error_contains(
            r#"
interface Cell<T> { fn put(self, value: &mut T); }
class IntCell implements Cell<i64> {
    pub fn put(self, value: i64) { println(value); }
}
"#,
            ErrorCode::E0416,
            "parameters do not match interface",
        );
    }

    /// 15. An inherited requirement (`extends`) is composed into the child's
    ///     method set by desugaring; the mode rides along.
    #[test]
    fn ifacemode_15_inherited_requirement_keeps_mode() {
        let source = r#"
interface Base { fn bump(self, value: &mut i64); }
interface Ext extends Base { fn tag(self) -> i64; }
class Impl implements Ext {
    pub fn bump(self, value: i64) { println(value); }
    pub fn tag(self) -> i64 { return 1; }
}
"#;
        let tokens = crate::lexer::Lexer::new(source)
            .tokenize()
            .expect("lexing failed");
        let (mut program, parse_errors) = crate::parser::Parser::new(tokens).parse();
        assert!(parse_errors.is_empty(), "{parse_errors:?}");
        crate::desugar::DesugarPass::run(&mut program, &mut []);
        let mut checker = TypeChecker::new();
        checker.check_program(&program);
        assert!(
            checker.errors.iter().any(
                |e| e.code == ErrorCode::E0416 && e.message.contains("parameters do not match")
            ),
            "inherited `&mut` requirement must still be enforced, got {:?}",
            checker.errors
        );
    }

    /// 16. A DEFAULT-bodied interface method has a mode too, and its call sites
    ///     are checked against it like any other slot.
    #[test]
    fn ifacemode_16_default_method_keeps_mode_at_call_site() {
        assert_typecheck_error_contains(
            r#"
interface Counter {
    fn base(self) -> i64;
    fn add(self, value: &mut i64) { value = value + self.base(); }
}
class Two implements Counter { pub fn base(self) -> i64 { return 2; } }
fn f() {
    let c: Counter = new Two();
    let mut x = 0;
    c.add(x);
}
"#,
            ErrorCode::E1702,
            "expected reference argument for reference parameter",
        );
    }

    /// 17. The referenced place may be a FIELD, not just a local.
    #[test]
    fn ifacemode_17_reference_to_field_place_accepted() {
        assert_typecheck_ok(&format!(
            r#"{MUTATOR}
class Adder implements Mutator {{
    pub fn bump(self, value: &mut i64) {{ value = value + 1; }}
}}
class Box {{ pub n: i64; }}
fn f() {{
    let m: Mutator = new Adder();
    let b = new Box(0);
    m.bump(&b.n);
    println(b.n);
}}
"#
        ));
    }

    /// 18. …or an ARRAY ELEMENT, whose address is computed from the buffer.
    #[test]
    fn ifacemode_18_reference_to_array_element_accepted() {
        assert_typecheck_ok(&format!(
            r#"import std::collections::Array;
{MUTATOR}
class Adder implements Mutator {{
    pub fn bump(self, value: &mut i64) {{ value = value + 1; }}
}}
fn f() {{
    let m: Mutator = new Adder();
    let mut xs: Array<i64> = [1, 2];
    m.bump(&xs[1]);
    println(xs[1]);
}}
"#
        ));
    }

    // ── Task completion surface: `await` / `await t.result()`
    //
    // Waiting on a task is spelled `await` and nothing else. `join()` is gone,
    // `try_join()` is gone, and `result()` is a zero-cost cancellation-aware
    // VIEW of the same task:
    //
    //   | operation        | waits | type                 |
    //   | await t          | yes   | T                    |
    //   | await t.result() | yes   | Result<T, Cancelled> |
    //
    // 33 perspectives, one per test below:
    //  1 `await t` yields T · 2 `await t` needs an async fn (E0801) ·
    //  3 `await t.result()` yields Result<T, Cancelled> · 4 `await t.result()`
    //  needs an async fn too · 5 a bare `result()` is `TaskResult<T>`, not `T` ·
    //  6 `result()` takes no arguments · 7 `result()` only exists on tasks ·
    //  8 `try_join()` is removed on Task · 9 it is removed on JoinHandle ·
    //  10 its migration names `await task` · 11 its migration names
    //  `await task.result()` · 12 unrelated classes may define `try_join` ·
    //  13 wrapping the removed spelling in `await` remains E0813 ·
    //  14 a handle is a first-class parameter · 15 `join()` on a Task is
    //  the E0812 migration · 16 `join()` on a JoinHandle likewise · 17 the
    //  migration text names `await task` · 18 a class may still define its own
    //  `join()` · 19 awaiting a non-awaitable is E0803 · 20 awaiting a
    //  `TaskResult` twice is fine (non-consuming) · 21 a void task awaits as a
    //  statement · 22 select binds `let v = await t` · 23 select binds
    //  `let r = await t.result()` as a Result · 24 select rejects a
    //  non-task await with E0805 · 25 a select TASK case needs an async fn,
    //  while its channel/sleep cases do not · 26 a `TaskResult<T>` held in a
    //  VARIABLE is selectable, exactly like an inline `.result()` · 27 that
    //  variable form binds the cancellation-aware type · 28 a variable holding a
    //  plain `Task<T>` still binds `T` · 29 `await` on a `JoinHandle<T>` — the
    //  type E0812 tells users to migrate to — yields `T` · 30 `JoinHandle<T>` is
    //  the same type as `Task<T>`, so an async call initializes one · 31 a
    //  `JoinHandle<T>` is selectable · 32 `JoinHandle<T>.result()` is
    //  cancellation-aware like `Task<T>.result()` · 33 a user-defined
    //  `result()` returning `Task<T>` stays a plain task in select.

    const TASK_FIXTURE: &str = "async fn work() -> i64 { await sleep(1); return 7; }\n";

    fn assert_task_ok(body: &str) {
        assert_typecheck_ok(&format!("{TASK_FIXTURE}{body}"));
    }

    fn assert_task_error(body: &str, code: ErrorCode, message: &str) {
        assert_typecheck_error_contains(&format!("{TASK_FIXTURE}{body}"), code, message);
    }

    #[test]
    fn taskwait_01_await_yields_the_output_type() {
        assert_task_ok("async fn f() { let v: i64 = await work(); }");
    }

    #[test]
    fn taskwait_02_await_requires_an_async_function() {
        assert_task_error(
            "fn f() { let v = await work(); }",
            ErrorCode::E0801,
            "`await` can only be used inside an async function",
        );
    }

    #[test]
    fn taskwait_03_await_result_yields_result_of_cancelled() {
        // The unit harness has no prelude, so the type is pinned through the
        // mismatch text rather than through an `Ok`/`Err` match.
        assert_task_error(
            "async fn f() { let r: i64 = await work().result(); }",
            ErrorCode::E0201,
            "found `Result<i64, Cancelled>`",
        );
    }

    #[test]
    fn taskwait_04_await_result_also_requires_an_async_function() {
        // `result()` changes the mapping of a cancelled task, not the fact
        // that awaiting it is a wait.
        assert_task_error(
            "fn f() { let r = await work().result(); }",
            ErrorCode::E0801,
            "`await` can only be used inside an async function",
        );
    }

    #[test]
    fn taskwait_05_bare_result_is_a_task_view_not_the_value() {
        // Without `await` it is `TaskResult<i64>`: a view of the same task.
        assert_task_ok("fn f() { let r = work().result(); }");
        assert_task_error(
            "fn f() { let r: i64 = work().result(); }",
            ErrorCode::E0201,
            "expected `i64`",
        );
    }

    #[test]
    fn taskwait_06_result_takes_no_arguments() {
        assert_task_error(
            "async fn f() { let r = await work().result(1); }",
            ErrorCode::E0201,
            "result expects 0 arguments",
        );
    }

    #[test]
    fn taskwait_07_result_is_not_a_method_on_plain_values() {
        assert_task_error(
            "fn f() { let x = 1; let r = x.result(); }",
            ErrorCode::E0201,
            "type `i64` has no methods",
        );
    }

    #[test]
    fn taskwait_08_try_join_on_task_is_removed() {
        assert_task_error(
            "fn f() { let o = work().try_join(); }",
            ErrorCode::E0813,
            "has been removed",
        );
    }

    #[test]
    fn taskwait_09_try_join_on_join_handle_is_removed() {
        assert_task_error(
            "fn f(h: JoinHandle<i64>) { let o = h.try_join(); }",
            ErrorCode::E0813,
            "has been removed",
        );
    }

    #[test]
    fn taskwait_10_try_join_migration_points_at_plain_await() {
        let errors = check_source(&format!("{TASK_FIXTURE}fn f() {{ work().try_join(); }}"));
        let error = errors
            .iter()
            .find(|e| e.code == ErrorCode::E0813)
            .expect("expected the try_join migration diagnostic");
        assert!(
            error.helps.iter().any(|h| h.contains("await task")),
            "migration help should name `await task`: {error:?}",
        );
    }

    #[test]
    fn taskwait_11_try_join_migration_points_at_cancellation_aware_await() {
        let errors = check_source(&format!("{TASK_FIXTURE}fn f() {{ work().try_join(); }}"));
        let error = errors
            .iter()
            .find(|e| e.code == ErrorCode::E0813)
            .expect("expected the try_join migration diagnostic");
        assert!(
            error
                .helps
                .iter()
                .any(|h| h.contains("await task.result()")),
            "migration help should name `await task.result()`: {error:?}",
        );
    }

    #[test]
    fn taskwait_12_a_class_may_still_define_its_own_try_join() {
        assert_typecheck_ok(
            r#"
class Probe {
    pub fn try_join(self) -> i64 { return 7; }
}
fn f() { let probe = new Probe(); let n: i64 = probe.try_join(); }
"#,
        );
    }

    #[test]
    fn taskwait_13_awaiting_try_join_still_reports_the_removal() {
        assert_task_error(
            "async fn f() { let o = await work().try_join(); }",
            ErrorCode::E0813,
            "has been removed",
        );
    }

    #[test]
    fn taskwait_14_a_task_handle_is_a_first_class_parameter() {
        // Handles cross function boundaries, so waiting can be delegated.
        assert_task_ok(
            "async fn wait(h: Task<i64>) -> i64 { return await h; }\nasync fn f() { let n = await wait(work()); }",
        );
    }

    #[test]
    fn taskwait_15_join_on_a_task_is_the_migration_error() {
        assert_task_error(
            "fn f() { let v = work().join(); }",
            ErrorCode::E0812,
            "has been removed",
        );
    }

    #[test]
    fn taskwait_16_join_on_a_spawn_handle_is_the_migration_error() {
        // `JoinHandle<T>` is the legacy spelling of the same handle and gets
        // the same migration, not a "no such method" error.
        assert_task_error(
            "fn f(h: JoinHandle<i64>) { let v = h.join(); }",
            ErrorCode::E0812,
            "has been removed",
        );
    }

    #[test]
    fn taskwait_17_join_migration_points_at_await() {
        let errors = check_source(&format!("{TASK_FIXTURE}fn f() {{ work().join(); }}"));
        let join_error = errors
            .iter()
            .find(|e| e.code == ErrorCode::E0812)
            .expect("expected the join migration diagnostic");
        assert!(
            join_error.helps.iter().any(|h| h.contains("await task")),
            "migration help should name `await task`: {join_error:?}",
        );
    }

    #[test]
    fn taskwait_18_a_class_may_still_define_its_own_join() {
        // The removal is scoped to task handles; unrelated APIs keep the name.
        assert_typecheck_ok(
            r#"
class Rope {
    pub fn join(self) -> i64 { return 1; }
}
fn f() { let r = new Rope(); let n: i64 = r.join(); }
"#,
        );
    }

    #[test]
    fn taskwait_19_awaiting_a_non_awaitable_is_rejected() {
        assert_typecheck_error_contains(
            "async fn f() { let x = 1; let v = await x; }",
            ErrorCode::E0803,
            "cannot await value of type `i64`",
        );
    }

    #[test]
    fn fsawait_01_sync_filesystem_call_has_targeted_migration() {
        let errors = check_source(
            "import std::fs;\nasync fn f() { let value = await fs::read_to_string(\"x\"); }",
        );
        let error = errors
            .iter()
            .find(|error| error.code == ErrorCode::E0803)
            .expect("awaiting a synchronous filesystem call must be rejected");
        assert!(
            error
                .message
                .contains("synchronous filesystem operations cannot be awaited"),
            "unexpected diagnostic: {error:?}",
        );
        assert!(
            error
                .helps
                .iter()
                .any(|help| help.contains("await fs::read_to_string_async(...)")),
            "migration must name the blocking-pool API: {error:?}",
        );
    }

    #[test]
    fn fsawait_02_module_alias_is_preserved_in_migration() {
        let errors = check_source(
            "import std::fs as files;\nasync fn f() { let value = await files::exists(\"x\"); }",
        );
        let error = errors
            .iter()
            .find(|error| error.code == ErrorCode::E0803)
            .expect("awaiting an aliased synchronous filesystem call must be rejected");
        assert!(
            error
                .helps
                .iter()
                .any(|help| help.contains("await files::exists_async(...)")),
            "migration must preserve the user's module alias: {error:?}",
        );
    }

    #[test]
    fn fsawait_03_async_filesystem_call_is_awaitable() {
        assert_typecheck_ok(
            "import std::fs;\nasync fn f() { let value = await fs::read_to_string_async(\"x\"); }",
        );
    }

    #[test]
    fn fsawait_04_sync_filesystem_call_remains_compatible_without_await() {
        assert_typecheck_ok("import std::fs;\nfn f() { let value = fs::read_to_string(\"x\"); }");
    }

    #[test]
    fn structuredtype_01_token_attach_preserves_task_output_type() {
        assert_typecheck_ok(
            "async fn work() -> i64 { return 1; }\nasync fn f() { let token = CancellationToken::new(); let task: Task<i64> = token.attach(work()); let value: i64 = await task; }",
        );
    }

    #[test]
    fn structuredtype_02_scope_add_preserves_task_output_type() {
        assert_typecheck_ok(
            "async fn work() -> String { return \"ok\"; }\nasync fn f() { let scope = TaskScope::new(); let task: Task<String> = scope.add(work()); let value: String = await task; }",
        );
    }

    #[test]
    fn structuredtype_03_scope_finish_is_cancellation_aware_result() {
        assert_typecheck_ok(
            "async fn f() { let scope = TaskScope::new(); let result: Result<void, Cancelled> = await scope.finish(); }",
        );
    }

    #[test]
    fn structuredtype_04_nested_token_and_scope_types_are_stable() {
        assert_typecheck_ok(
            "fn f() { let token: CancellationToken = CancellationToken::new().child(); let scope: TaskScope = TaskScope::new().child(); }",
        );
    }

    #[test]
    fn structuredtype_05_token_rejects_non_task_attachment() {
        assert_typecheck_error_contains(
            "fn f() { let token = CancellationToken::new(); token.attach(1); }",
            ErrorCode::E0201,
            "expects `Task<T>`",
        );
    }

    #[test]
    fn structuredtype_06_scope_rejects_reference_task_argument() {
        assert_typecheck_error_contains(
            "async fn work() {}\nfn f() { let scope = TaskScope::new(); let task = work(); scope.add(&task); }",
            ErrorCode::E1703,
            "unexpected reference argument",
        );
    }

    #[test]
    fn taskwait_20_a_task_can_be_awaited_through_both_views() {
        // `result()` does not consume the handle: the same task can also be
        // awaited directly, because both are views of one frame.
        assert_task_ok(
            "async fn f() { let h = work(); let r = await h.result(); let s = await h.result(); }",
        );
    }

    #[test]
    fn taskwait_21_a_void_task_is_awaited_as_a_statement() {
        assert_typecheck_ok(
            "async fn tick() { await sleep(1); }\nasync fn f() { let h = tick(); await h; }",
        );
    }

    #[test]
    fn taskwait_22_select_binds_a_plain_await() {
        assert_task_ok(
            "async fn f() { let h = work(); select { let v = await h => { let n: i64 = v; } } }",
        );
    }

    #[test]
    fn taskwait_23_select_binds_await_result_as_a_result() {
        // The select binding carries the cancellation-aware type, matching
        // `await t.result()` outside a select.
        assert_task_error(
            "async fn f() { let h = work(); select { let r = await h.result() => { let n: i64 = r; } } }",
            ErrorCode::E0201,
            "found `Result<i64, Cancelled>`",
        );
    }

    #[test]
    fn taskwait_24_select_rejects_awaiting_a_non_task() {
        assert_typecheck_error_contains(
            "async fn f() { let x = 1; select { let v = await x => { } } }",
            ErrorCode::E0805,
            "in a select case",
        );
    }

    #[test]
    fn taskwait_25_select_task_case_requires_an_async_function() {
        // A task case is an `await`, so a sync `select` may still wait on a
        // channel or a `sleep` but never on a task.
        assert_task_error(
            "fn f() { let h = work(); select { let v = await h => { } sleep(1) => { } } }",
            ErrorCode::E0801,
            "`await` can only be used inside an async function",
        );
        assert_typecheck_ok(
            "fn f() { let ch = Channel<i64>::new(); select { let v = ch.recv() => { } sleep(1) => { } } }",
        );
    }

    #[test]
    fn taskwait_26_select_accepts_a_task_result_held_in_a_variable() {
        // Cancellation-awareness is a property of the awaited TYPE, not of how
        // the case was spelled: hoisting `t.result()` into a local must not
        // change whether the case type-checks.
        assert_task_ok(
            "async fn f() { let h = work(); let view = h.result(); select { let r = await view => { } sleep(1) => { } } }",
        );
    }

    #[test]
    fn taskwait_27_select_binds_a_task_result_variable_as_a_result() {
        assert_task_error(
            "async fn f() { let h = work(); let view = h.result(); select { let r = await view => { let n: i64 = r; } } }",
            ErrorCode::E0201,
            "found `Result<i64, Cancelled>`",
        );
    }

    #[test]
    fn taskwait_28_select_binds_a_plain_task_variable_as_the_output() {
        // The mirror of 27: without a `result()` view the binding is bare `T`,
        // so the type-driven decision has not made every case cancel-aware.
        assert_task_error(
            "async fn f() { let h = work(); select { let v = await h => { let s: String = v; } } }",
            ErrorCode::E0201,
            "found `i64`",
        );
    }

    #[test]
    fn taskwait_29_await_accepts_a_join_handle() {
        // E0812 tells users to replace `join()` with `await task`, so the type
        // it names must actually be awaitable.
        assert_task_error(
            "async fn f(h: JoinHandle<i64>) { let s: String = await h; }",
            ErrorCode::E0201,
            "found `i64`",
        );
    }

    #[test]
    fn taskwait_30_join_handle_is_the_same_type_as_task() {
        // `JoinHandle<T>` is a legacy spelling, not a second type; if it were
        // distinct no expression in the language could produce one.
        assert_task_ok("async fn f() { let h: JoinHandle<i64> = work(); let v = await h; }");
    }

    #[test]
    fn taskwait_31_select_accepts_a_join_handle_case() {
        assert_task_ok(
            "async fn f(h: JoinHandle<i64>) { select { let v = await h => { } sleep(1) => { } } }",
        );
    }

    #[test]
    fn taskwait_32_join_handle_result_is_cancel_aware() {
        assert_task_error(
            "async fn f(h: JoinHandle<i64>) { let r: i64 = await h.result(); }",
            ErrorCode::E0201,
            "found `Result<i64, Cancelled>`",
        );
    }

    #[test]
    fn taskwait_33_select_classifies_user_result_method_by_return_type() {
        assert_task_ok(
            r#"
class Wrapper {
    pub fn result(self) -> Task<i64> { return work(); }
}
async fn f() {
    let wrapper = new Wrapper();
    select { let value = await wrapper.result() => { let n: i64 = value; } }
}
"#,
        );
    }

    // ── Exponentiation `**` typing (willow-n5yv.2, willow-n5yv.3) ────────────
    //
    // `**` has exactly two signatures: `i64 ** i64 -> i64` and
    // `f64 ** f64 -> f64`. Both lower natively; mixed numeric operands remain
    // explicit type errors.

    /// Type-check `source`. The historical name is retained locally to keep the
    /// rejection tests compact; E2501 is no longer produced.
    fn pow_errors_ignoring_gate(source: &str) -> Vec<Diagnostic> {
        check_source(source)
            .into_iter()
            .filter(|error| error.code != ErrorCode::E2501)
            .collect()
    }

    /// Assert `source` type-checks with no diagnostic whatsoever.
    fn assert_pow_accepted(source: &str) {
        let all = check_source(source);
        assert!(all.is_empty(), "unexpected errors: {all:?}");
    }

    /// Assert `source` is rejected by the exponent type rule with a message
    /// mentioning both operand types.
    fn assert_pow_rejected(source: &str, expected_message: &str) {
        let errors = pow_errors_ignoring_gate(source);
        assert!(
            errors
                .iter()
                .any(|e| e.code == ErrorCode::E0202 && e.message.contains(expected_message)),
            "expected E0202 containing `{expected_message}`, got {errors:?}",
        );
    }

    // Perspective 1: `i64 ** i64` is accepted and has type `i64`.
    #[test]
    fn pow_type_01_i64_base_and_exponent() {
        assert_pow_accepted("fn main() { let x: i64 = 2 ** 3; }");
    }

    // Perspective 2: `f64 ** f64` is accepted and has type `f64`.
    #[test]
    fn pow_type_02_f64_base_and_exponent() {
        assert_pow_accepted("fn main() { let x: f64 = 2.0 ** 3.0; }");
    }

    // Perspective 3: an `i64` power does not silently produce `f64`.
    #[test]
    fn pow_type_03_i64_result_is_not_f64() {
        let errors = pow_errors_ignoring_gate("fn main() { let x: f64 = 2 ** 3; }");
        assert!(
            errors.iter().any(|e| e.code == ErrorCode::E0201),
            "{errors:?}"
        );
    }

    // Perspective 4: an `f64` power does not silently truncate to `i64`.
    #[test]
    fn pow_type_04_f64_result_is_not_i64() {
        let errors = pow_errors_ignoring_gate("fn main() { let x: i64 = 2.0 ** 3.0; }");
        assert!(
            errors.iter().any(|e| e.code == ErrorCode::E0201),
            "{errors:?}"
        );
    }

    // Perspective 5: `i64 ** f64` is rejected — there is no mixed form.
    #[test]
    fn pow_type_05_i64_base_with_f64_exponent_is_rejected() {
        assert_pow_rejected(
            "fn main() { let x = 2 ** 3.0; }",
            "cannot raise `i64` to the power of `f64`",
        );
    }

    // Perspective 6: `f64 ** i64` is rejected for the same reason.
    #[test]
    fn pow_type_06_f64_base_with_i64_exponent_is_rejected() {
        assert_pow_rejected(
            "fn main() { let x = 2.0 ** 3; }",
            "cannot raise `f64` to the power of `i64`",
        );
    }

    // Perspective 7: a mixed power gets an explicit "make both the same type"
    // help, which the generic arithmetic diagnostic does not provide.
    #[test]
    fn pow_type_07_mixed_numeric_power_suggests_matching_types() {
        let errors = pow_errors_ignoring_gate("fn main() { let x = 2 ** 3.0; }");
        let diagnostic = errors
            .iter()
            .find(|e| e.code == ErrorCode::E0202)
            .expect("E0202");
        assert!(
            diagnostic
                .helps
                .iter()
                .any(|help| help.contains("does not mix `i64` and `f64`")),
            "{diagnostic:?}"
        );
    }

    // Perspective 8: `bool ** bool` is rejected.
    #[test]
    fn pow_type_08_bool_operands_are_rejected() {
        assert_pow_rejected(
            "fn main() { let x = true ** false; }",
            "cannot raise `bool` to the power of `bool`",
        );
    }

    // Perspective 9: `String ** String` is rejected with the exponent message,
    // not the string-concatenation `toString()` help.
    #[test]
    fn pow_type_09_string_operands_are_rejected() {
        let errors = pow_errors_ignoring_gate("fn main() { let x = \"a\" ** \"b\"; }");
        let diagnostic = errors
            .iter()
            .find(|e| e.code == ErrorCode::E0202)
            .expect("E0202");
        assert!(
            diagnostic.message.contains("to the power of"),
            "{diagnostic:?}"
        );
        assert!(
            !diagnostic
                .helps
                .iter()
                .any(|help| help.contains("toString")),
            "{diagnostic:?}"
        );
    }

    // Perspective 10: a bool exponent on a numeric base is rejected.
    #[test]
    fn pow_type_10_bool_exponent_on_numeric_base_is_rejected() {
        assert_pow_rejected(
            "fn main() { let x = 2 ** true; }",
            "cannot raise `i64` to the power of `bool`",
        );
    }

    // Perspective 11: the diagnostic names both defined signatures so the user
    // learns the rule from one message.
    #[test]
    fn pow_type_11_label_lists_the_defined_signatures() {
        let errors = pow_errors_ignoring_gate("fn main() { let x = 2 ** true; }");
        let diagnostic = errors
            .iter()
            .find(|e| e.code == ErrorCode::E0202)
            .expect("E0202");
        assert!(
            diagnostic
                .labels
                .iter()
                .any(|label| label.message.contains("`i64 ** i64` and `f64 ** f64`")),
            "{diagnostic:?}"
        );
    }

    // Perspective 12: a bad exponent recovers with the base type, so one
    // mistake does not cascade into a second unrelated error.
    #[test]
    fn pow_type_12_recovers_with_the_base_type() {
        let errors = pow_errors_ignoring_gate("fn main() { let x: i64 = 2 ** true; }");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert_eq!(errors[0].code, ErrorCode::E0202);
    }

    // Perspective 13: right associativity is type-checked as written —
    // `2 ** 3 ** 2` is three `i64` operands and stays `i64`.
    #[test]
    fn pow_type_13_right_associative_chain_stays_i64() {
        assert_pow_accepted("fn main() { let x: i64 = 2 ** 3 ** 2; }");
    }

    // Perspective 14: a mixed chain reports the inner power, not just the
    // outer one.
    #[test]
    fn pow_type_14_mixed_chain_reports_the_inner_power() {
        assert_pow_rejected(
            "fn main() { let x = 2 ** 3 ** 2.0; }",
            "cannot raise `i64` to the power of `f64`",
        );
    }

    // Perspective 15: `-2 ** 2` type-checks as `-(2 ** 2)`, an `i64`.
    #[test]
    fn pow_type_15_negated_power_is_i64() {
        assert_pow_accepted("fn main() { let x: i64 = -2 ** 2; }");
    }

    // Perspective 16: a negative exponent still parses without parentheses, but
    // an integer power has no fractional result, so the literal form is a
    // compile error rather than a guaranteed runtime panic (willow-n5yv.3).
    #[test]
    fn pow_type_16_negative_integer_exponent_literal_is_rejected() {
        let errors = check_source("fn main() { let x: i64 = 2 ** -3; }");
        let negative = errors
            .iter()
            .find(|e| e.code == ErrorCode::E0204)
            .expect("E0204");
        assert!(
            negative.message.contains("negative exponent"),
            "{negative:?}"
        );
        assert!(!negative.labels.is_empty(), "{negative:?}");
        assert!(!negative.helps.is_empty(), "{negative:?}");
        // The power still has type `i64`, so nothing cascades.
        assert_eq!(errors.len(), 1, "{errors:?}");

        // `x ** -0` is `x ** 0`, which is well defined.
        assert_pow_accepted("fn main() { let x: i64 = 2 ** -0; }");
        // A negative FLOAT exponent is fine; only integers lack the result.
        assert_pow_accepted("fn main() { let x: f64 = 2.0 ** -3.0; }");
        // A dynamic negative exponent is a runtime fault, not a compile error.
        assert_pow_accepted("fn main() { let e = 0 - 3; let x: i64 = 2 ** e; }");
    }

    // Perspective 17: variables work as base and exponent.
    #[test]
    fn pow_type_17_variable_operands() {
        assert_pow_accepted("fn main() { let b = 2; let e = 3; let x: i64 = b ** e; }");
    }

    // Perspective 18: a call result works as an operand and is checked by its
    // return type.
    #[test]
    fn pow_type_18_call_result_operands() {
        assert_pow_accepted(
            "fn base() -> i64 { return 2; }\nfn main() { let x: i64 = base() ** 3; }",
        );
        assert_pow_rejected(
            "fn base() -> f64 { return 2.0; }\nfn main() { let x = base() ** 3; }",
            "cannot raise `f64` to the power of `i64`",
        );
    }

    // Perspective 19: a power is usable where an `i64` is expected — as a call
    // argument and as a return value.
    #[test]
    fn pow_type_19_power_flows_into_i64_positions() {
        assert_pow_accepted(
            "fn take(n: i64) -> i64 { return n; }\nfn main() { let x = take(2 ** 3); }",
        );
        assert_pow_accepted("fn square() -> i64 { return 2 ** 2; }\nfn main() { }");
    }

    // Perspective 20: a power is a valid comparison operand, so `2 ** 3 < 9`
    // is a `bool` rather than a type error.
    #[test]
    fn pow_type_20_power_in_a_comparison_is_bool() {
        assert_pow_accepted("fn main() { let x: bool = 2 ** 3 < 9; }");
    }

    // Perspective 21: `nil ** i64` is rejected rather than treated as numeric.
    #[test]
    fn pow_type_21_nil_operand_is_rejected() {
        let errors = pow_errors_ignoring_gate("fn main() { let x = nil ** 2; }");
        assert!(!errors.is_empty(), "expected an error for `nil ** 2`");
    }

    // Perspective 22: the retired float staging gate never fires; well-typed
    // float and integer powers are accepted while an ill-typed one retains E0202.
    #[test]
    fn pow_type_22_float_codegen_gate_is_retired() {
        let float = check_source("fn main() { let x = 2.0 ** 3.0; }");
        assert!(float.is_empty(), "{float:?}");
        let integer = check_source("fn main() { let x = 2 ** 3; }");
        assert!(integer.is_empty(), "{integer:?}");
        let mistyped = check_source("fn main() { let x = 2 ** true; }");
        assert!(mistyped.iter().any(|e| e.code == ErrorCode::E0202));
        assert!(!mistyped.iter().any(|e| e.code == ErrorCode::E2501));
    }

    // Perspective 23: a float power composes in a larger float expression with
    // no historical workaround diagnostic.
    #[test]
    fn pow_type_23_float_power_composes_without_a_gate() {
        assert_pow_accepted("fn main() { let x: f64 = 1.0 + 2.0 ** 3.0 / 4.0; }");
    }

    // Perspective 24: programs with no `**` are untouched by the gate.
    #[test]
    fn pow_type_24_programs_without_pow_are_unaffected() {
        assert_typecheck_ok("fn main() { let x: i64 = 2 * 3; }");
    }

    // Perspective 25: the compatibility spellings retain the exact f64
    // signature, while integer powers remain a distinct operator overload.
    #[test]
    fn pow_type_25_pow_and_powf_compatibility_signatures() {
        assert_pow_accepted(
            "fn main() { let a: f64 = pow(2.0, 3.0); let b: f64 = powf(2.0, 3.0); }",
        );
        assert_pow_accepted("fn main() { let x: i64 = 2 ** 3; }");
    }

    // ── Scheduler-aware `lock` statement (willow-38w.1.1) ────────────────────
    //
    // The Mutex form is fully lowered as of willow-38w.1.4, so a well-formed
    // `lock <mutex> as [mut] v` now type-checks with NO diagnostics at all.
    // `lock read`/`lock write` are fully lowered as of willow-38w.1.5 too.

    /// Historical helper name retained for the perspective table; no staged
    /// gate exists after Stage 5, so this is the complete error list.
    fn lock_errors_ignoring_gate(source: &str) -> Vec<Diagnostic> {
        check_source(source)
    }

    /// Assert `source` type-checks with no diagnostics whatsoever — the Mutex
    /// form is no longer gated, so "accepted" means literally accepted.
    fn assert_lock_accepted(source: &str) {
        let all = check_source(source);
        assert!(all.is_empty(), "unexpected errors: {all:?}");
    }

    /// Assert `source` raises `code` (ignoring the stage-1 gate), `count` times.
    fn assert_lock_error_count(source: &str, code: ErrorCode, count: usize) {
        let errors = lock_errors_ignoring_gate(source);
        assert_eq!(
            errors.iter().filter(|e| e.code == code).count(),
            count,
            "expected {count} x {code:?}, got {errors:?}"
        );
    }

    // Perspective 1: the canonical Mutex form is accepted and gated once.
    #[test]
    fn lock_check_01_mutex_form_accepted() {
        assert_lock_accepted(
            "async fn main() { let m = Mutex::new(0); lock m as value { println(value); } }",
        );
    }

    // Perspective 2: the binding has the PROTECTED type `T`, not the lock type.
    #[test]
    fn lock_check_02_binding_has_protected_type() {
        assert_lock_accepted(
            "async fn main() { let m = Mutex::new(0); lock m as value { let n: i64 = value; } }",
        );
    }

    // Perspective 3: the protected type is checked, not assumed — binding an
    // `i64` view to a `String` local is still an error.
    #[test]
    fn lock_check_03_binding_type_is_enforced() {
        assert_lock_error_count(
            "async fn main() { let m = Mutex::new(0); lock m as value { let s: String = value; } }",
            ErrorCode::E0201,
            1,
        );
    }

    // Perspective 4: without `mut` the binding is immutable.
    #[test]
    fn lock_check_04_binding_is_immutable_by_default() {
        assert_lock_error_count(
            "async fn main() { let m = Mutex::new(0); lock m as value { value = 1; } }",
            ErrorCode::E0301,
            1,
        );
    }

    // Perspective 5: `as mut` makes it writable.
    #[test]
    fn lock_check_05_mut_binding_is_writable() {
        assert_lock_accepted(
            "async fn main() { let m = Mutex::new(0); lock m as mut value { value = 1; } }",
        );
    }

    // Perspective 6: `lock read` over an `RwLock<T>` is accepted.
    #[test]
    fn lock_check_06_read_form_accepted() {
        assert_lock_accepted(
            "async fn main() { let r = RwLock::new(0); lock read r as value { println(value); } }",
        );
    }

    // Perspective 7: `lock write` over an `RwLock<T>` is accepted, `mut` and all.
    #[test]
    fn lock_check_07_write_form_accepted() {
        assert_lock_accepted(
            "async fn main() { let r = RwLock::new(0); lock write r as mut value { value = 1; } }",
        );
    }

    // Perspective 8: the Mutex form over an `RwLock` is E2602, and the help
    // names the two statement forms that RwLock actually has.
    #[test]
    fn lock_check_08_mutex_form_over_rwlock() {
        let source = "async fn main() { let r = RwLock::new(0); lock r as value { } }";
        assert_lock_error_count(source, ErrorCode::E2602, 1);
        let errors = lock_errors_ignoring_gate(source);
        let e = errors.iter().find(|e| e.code == ErrorCode::E2602).unwrap();
        assert!(e.message.contains("`lock` requires `Mutex<T>`"), "{e:?}");
        assert!(
            e.helps
                .iter()
                .any(|h| h.contains("lock read") && h.contains("lock write")),
            "{e:?}"
        );
    }

    // Perspective 9: `lock read`/`lock write` over a `Mutex` is E2602, and the
    // help names the Mutex form.
    #[test]
    fn lock_check_09_rwlock_forms_over_mutex() {
        for source in [
            "async fn main() { let m = Mutex::new(0); lock read m as value { } }",
            "async fn main() { let m = Mutex::new(0); lock write m as value { } }",
        ] {
            assert_lock_error_count(source, ErrorCode::E2602, 1);
            let errors = lock_errors_ignoring_gate(source);
            let e = errors.iter().find(|e| e.code == ErrorCode::E2602).unwrap();
            assert!(e.message.contains("requires `RwLock<T>`"), "{e:?}");
            assert!(e.helps.iter().any(|h| h.contains("lock <mutex>")), "{e:?}");
        }
    }

    // Perspective 10: a target that is not a lock at all is E2602 too, with no
    // form-specific help to mislead the reader.
    #[test]
    fn lock_check_10_non_lock_target() {
        let source = "async fn main() { let n = 1; lock n as value { } }";
        assert_lock_error_count(source, ErrorCode::E2602, 1);
        let errors = lock_errors_ignoring_gate(source);
        let e = errors.iter().find(|e| e.code == ErrorCode::E2602).unwrap();
        assert!(e.message.contains("found `i64`"), "{e:?}");
        assert!(e.helps.is_empty(), "{e:?}");
    }

    // Perspective 11: an unresolved target reports only the name error — the
    // lock rule must not pile a second diagnostic onto a broken expression.
    #[test]
    fn lock_check_11_unknown_target_reports_once() {
        assert_lock_error_count(
            "async fn main() { lock nope as value { } }",
            ErrorCode::E2602,
            0,
        );
    }

    // Perspective 12: a `lock` in a synchronous function is E2603 — V1 parks
    // the task, which only an async frame can do.
    #[test]
    fn lock_check_12_sync_function_rejected() {
        let source = "fn helper(m: Mutex<i64>) { lock m as value { println(value); } }";
        assert_lock_error_count(source, ErrorCode::E2603, 1);
        let errors = lock_errors_ignoring_gate(source);
        let e = errors.iter().find(|e| e.code == ErrorCode::E2603).unwrap();
        assert!(
            e.message.contains("only allowed in an async function"),
            "{e:?}"
        );
        assert!(e.helps.iter().any(|h| h.contains("async fn")), "{e:?}");
    }

    // Perspective 13: E2603 names the form that was used, so `lock read` in a
    // sync fn does not talk about plain `lock`.
    #[test]
    fn lock_check_13_sync_diagnostic_names_the_form() {
        let errors =
            lock_errors_ignoring_gate("fn helper(r: RwLock<i64>) { lock read r as value { } }");
        let e = errors.iter().find(|e| e.code == ErrorCode::E2603).unwrap();
        assert!(
            e.labels.iter().any(|l| l.message.contains("`lock read`")),
            "{e:?}"
        );
    }

    // Perspective 14: an `await` inside the critical section is E2604 — holding
    // a lock across an unbounded wait is what deadlocks a task scheduler.
    #[test]
    fn lock_check_14_await_in_body_rejected() {
        assert_lock_error_count(
            "async fn main() { let m = Mutex::new(0); lock m as value { await sleep(1); } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 15: a `select` suspends too, so it is rejected the same way.
    #[test]
    fn lock_check_15_select_in_body_rejected() {
        assert_lock_error_count(
            "async fn main() { let m = Mutex::new(0); let ch = Channel<i64>::new(); \
             lock m as value { select { let v = ch.recv() => { } } } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 16: the scan reaches nested control flow, not just the top
    // level of the body.
    #[test]
    fn lock_check_16_await_scan_reaches_nested_blocks() {
        assert_lock_error_count(
            "async fn main() { let m = Mutex::new(0); \
             lock m as value { if value > 0 { while value > 0 { await sleep(1); } } } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 17: an `await` in the TARGET is fine — it is evaluated before
    // the lock is held.
    #[test]
    fn lock_check_17_await_in_target_allowed() {
        assert_lock_accepted(
            "async fn number() -> i64 { await sleep(1); return 0; } \
             async fn main() { let m = Mutex::new(0); \
             lock mutex_at(await number()) as value { println(value); } } \
             fn mutex_at(i: i64) -> Mutex<i64> { return Mutex::new(i); }",
        );
    }

    // Perspective 18: an `await` inside a `defer` in the body is left to the
    // defer rule (E0905), not double-reported as E2604.
    #[test]
    fn lock_check_18_await_inside_defer_is_not_double_reported() {
        let source = "async fn main() { let m = Mutex::new(0); \
                      lock m as value { defer { await sleep(1); } } }";
        assert_lock_error_count(source, ErrorCode::E2604, 0);
        let errors = lock_errors_ignoring_gate(source);
        assert!(
            errors.iter().any(|e| e.code == ErrorCode::E0905),
            "{errors:?}"
        );
    }

    // Perspective 19: a nested acquisition is E2605 — V1 has no lock ordering,
    // so it refuses the shape that would need one.
    #[test]
    fn lock_check_19_nested_lock_rejected() {
        let source = "async fn main() { let a = Mutex::new(0); let b = Mutex::new(0); \
                      lock a as x { lock b as y { println(x + y); } } }";
        assert_lock_error_count(source, ErrorCode::E2605, 1);
        let errors = lock_errors_ignoring_gate(source);
        let e = errors.iter().find(|e| e.code == ErrorCode::E2605).unwrap();
        assert!(
            e.helps.iter().any(|h| h.contains("one after another")),
            "{e:?}"
        );
    }

    // Perspective 20: nesting through an intervening block still nests.
    #[test]
    fn lock_check_20_nested_lock_through_control_flow() {
        assert_lock_error_count(
            "async fn main() { let a = Mutex::new(0); let b = Mutex::new(0); \
             lock a as x { if x > 0 { lock b as y { println(y); } } } }",
            ErrorCode::E2605,
            1,
        );
    }

    // Perspective 21: one `await` under two nested locks is reported ONCE —
    // only the outermost lock scans, so the diagnostics do not multiply.
    #[test]
    fn lock_check_21_await_under_nested_locks_reported_once() {
        assert_lock_error_count(
            "async fn main() { let a = Mutex::new(0); let b = Mutex::new(0); \
             lock a as x { lock b as y { await sleep(1); } } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 22: two SEQUENTIAL locks are fine and gated twice — the V1
    // restriction is about nesting, not about count.
    #[test]
    fn lock_check_22_sequential_locks_accepted() {
        assert_lock_accepted(
            "async fn main() { let a = Mutex::new(0); let b = Mutex::new(0); \
             lock a as x { println(x); } lock b as y { println(y); } }",
        );
    }

    // Perspective 23: the binding is scoped to the body and gone after it.
    #[test]
    fn lock_check_23_binding_scope_ends_with_body() {
        assert_lock_error_count(
            "async fn main() { let m = Mutex::new(0); lock m as value { } println(value); }",
            ErrorCode::E0350,
            1,
        );
    }

    // Perspective 24: the binding shadows an outer variable inside the body and
    // the outer one is restored afterwards, with its own type.
    #[test]
    fn lock_check_24_binding_shadows_then_restores() {
        assert_lock_accepted(
            "async fn main() { let value = \"outer\"; let m = Mutex::new(0); \
             lock m as value { let n: i64 = value; } let s: String = value; }",
        );
    }

    // Perspective 25: a `return` inside the critical section counts as a return
    // for the enclosing function's definite-return check.
    #[test]
    fn lock_check_25_return_inside_body_satisfies_return_check() {
        assert_lock_accepted(
            "async fn main() { println(await get()); } \
             async fn get() -> i64 { let m = Mutex::new(7); lock m as value { return value; } }",
        );
    }

    // Perspective 26: a lock inside a loop is fine, and `break`/`continue` in
    // the body still bind to the loop.
    #[test]
    fn lock_check_26_lock_inside_loop() {
        assert_lock_accepted(
            "async fn main() { let m = Mutex::new(0); let mut i = 0; \
             while i < 3 { lock m as value { if value > 0 { break; } } i = i + 1; } }",
        );
    }

    // Perspective 27: a class method that is `async` may lock, including
    // `lock self.<field>`.
    #[test]
    fn lock_check_27_async_method_with_self_target() {
        assert_lock_accepted(
            "class Counter { pub m: Mutex<i64>; \
             pub async fn bump(self) { lock self.m as mut value { value = value + 1; } } } \
             fn main() { }",
        );
    }

    // Perspective 28: a malformed lock does NOT raise the staged gate — the
    // type error is the useful message, and the gate would bury it. The Mutex
    // form never gates at all now that it is lowered (willow-38w.1.4).
    #[test]
    fn lock_check_28_gate_only_fires_for_well_formed_locks() {
        for source in [
            // Malformed, in any mode: no gate.
            "async fn main() { let n = 1; lock n as value { } }",
            "fn helper(m: Mutex<i64>) { lock m as value { } }",
            "async fn main() { let m = Mutex::new(0); lock m as value { await sleep(1); } }",
            "async fn main() { let n = 1; lock read n as value { } }",
            // Well formed AND lowered: the Mutex form is past the gate.
            "async fn main() { let m = Mutex::new(0); lock m as value { println(value); } }",
        ] {
            let gates = check_source(source)
                .iter()
                .filter(|e| e.code == ErrorCode::E2502)
                .count();
            assert_eq!(gates, 0, "gate should stay quiet for `{source}`");
        }
        // Nesting rejects the INNER lock only. Both are Mutex sections, so no
        // gate survives — but the RwLock equivalent still gates its outer lock.
        let nested = check_source(
            "async fn main() { let a = Mutex::new(0); let b = Mutex::new(0); \
             lock a as x { lock b as y { } } }",
        );
        assert_eq!(
            nested.iter().filter(|e| e.code == ErrorCode::E2502).count(),
            0,
            "{nested:?}"
        );
        let nested_rw = check_source(
            "async fn main() { let a = RwLock::new(0); let b = RwLock::new(0); \
             lock read a as x { lock read b as y { } } }",
        );
        assert_eq!(
            nested_rw
                .iter()
                .filter(|e| e.code == ErrorCode::E2502)
                .count(),
            0,
            "{nested_rw:?}"
        );
    }

    // Perspective 29: the RwLock accessor cutover is actionable and names both
    // lexical forms plus the explicitly blocking compatibility type.
    #[test]
    fn lock_check_29_rwlock_accessor_cutover_is_actionable() {
        for source in [
            "async fn main() { let r = RwLock::new(0); println(r.read()); }",
            "async fn main() { let r = RwLock::new(0); r.write(1); }",
        ] {
            let errors = check_source(source);
            let denied = errors
                .iter()
                .find(|e| e.code == ErrorCode::E0806)
                .unwrap_or_else(|| panic!("E0806 for `{source}`: {errors:?}"));
            assert!(
                denied.helps.iter().any(|h| h.contains("lock read")
                    && h.contains("lock write")
                    && h.contains("BlockingRwCell<T>")),
                "{denied:?}"
            );
        }
    }

    // Perspective 30: the Mutex API cutover (willow-38w.1.4). `Mutex<T>` has no
    // accessors at all — the help routes to `lock` and to `BlockingCell<T>` —
    // while `BlockingCell<T>` and `BlockingRwCell<T>` keep theirs.
    #[test]
    fn lock_check_30_mutex_has_no_accessor_methods() {
        for source in [
            "async fn main() { let m = Mutex::new(0); println(m.get()); }",
            "async fn main() { let m = Mutex::new(0); m.set(1); }",
            "async fn main() { let m = Mutex::new(0); let v = m.lock(); }",
        ] {
            let errors = check_source(source);
            let denied = errors
                .iter()
                .find(|e| e.code == ErrorCode::E0806)
                .unwrap_or_else(|| panic!("E0806 for `{source}`: {errors:?}"));
            assert!(
                denied
                    .helps
                    .iter()
                    .any(|h| h.contains("lock <mutex> as") && h.contains("BlockingCell<T>")),
                "{denied:?}"
            );
        }
        assert_typecheck_ok(
            "fn main() { let c = BlockingCell::new(0); c.set(c.get() + 1); println(c.get()); }",
        );
        assert_typecheck_ok(
            "fn main() { let r = BlockingRwCell::new(0); r.write(r.read() + 1); println(r.read()); }",
        );
    }

    // Perspective 31: a synchronous `main` is rejected like any other sync fn —
    // the entry point gets no exemption from the async requirement.
    #[test]
    fn lock_check_31_sync_main_rejected() {
        assert_lock_error_count(
            "fn main() { let m = Mutex::new(0); lock m as value { println(value); } }",
            ErrorCode::E2603,
            1,
        );
    }

    // Perspective 32: an `await` buried in a SUB-EXPRESSION of a body statement
    // is caught — the scan is not limited to statement-position awaits.
    #[test]
    fn lock_check_32_await_in_subexpression_rejected() {
        assert_lock_error_count(
            "async fn number() -> i64 { await sleep(1); return 1; } \
             async fn main() { let m = Mutex::new(0); \
             lock m as value { let n = value + await number(); } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 33: an `await` AFTER the critical section is fine — the lock
    // is already released there.
    #[test]
    fn lock_check_33_await_after_body_accepted() {
        assert_lock_accepted(
            "async fn main() { let m = Mutex::new(0); \
             lock m as value { println(value); } await sleep(1); }",
        );
    }

    // Perspective 34: the target is type-checked exactly once, so a target that
    // is itself ill-typed reports its own error a single time.
    #[test]
    fn lock_check_34_target_is_checked_once() {
        let errors = lock_errors_ignoring_gate(
            "fn mutex_at(i: i64) -> Mutex<i64> { return Mutex::new(i); } \
             async fn main() { lock mutex_at(true) as value { } }",
        );
        assert_eq!(
            errors.iter().filter(|e| e.code == ErrorCode::E0201).count(),
            1,
            "{errors:?}"
        );
    }

    // Perspectives 35-38 cover what makes the E2604 scan SEMANTIC rather than
    // the Stage 1 `await`/`select` syntax scan (willow-38w.1.4). Removing the
    // codegen gate for the Mutex form is only safe if every operation that can
    // park the task is caught, and whether an operation parks depends on the
    // RECEIVER'S TYPE, which syntax alone cannot answer.

    // Perspective 35: `Channel.send` parks when the channel is full, and the
    // scheduler may then run a task that wants this same lock. It suspends even
    // though there is no `await` token anywhere in the statement.
    #[test]
    fn lock_check_35_channel_send_in_body_rejected() {
        assert_lock_error_count(
            "async fn main() { let m = Mutex::new(0); let ch: Channel<i64> = Channel::new(); \
             lock m as value { ch.send(value); } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 36: `Channel.recv` parks when the channel is empty.
    #[test]
    fn lock_check_36_channel_recv_in_body_rejected() {
        assert_lock_error_count(
            "async fn main() { let m = Mutex::new(0); let ch: Channel<i64> = Channel::new(); \
             lock m as value { let got = ch.recv(); } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 37: and the converse — a `send` method on something that is
    // NOT a `Channel<T>` does not suspend. A name-based scan would reject this
    // valid program; the type-directed one accepts it.
    #[test]
    fn lock_check_37_send_on_a_non_channel_is_not_a_suspension() {
        assert_lock_accepted(
            "class Mailbox { pub fn send(self, n: i64) { println(n); } } \
             async fn main() { let m = Mutex::new(0); let box = new Mailbox(); \
             lock m as value { box.send(value); } }",
        );
    }

    // Perspective 38: a bare async call inside the body is NOT a suspension.
    // Willow has no `spawn` — the call itself starts a task and returns a
    // `Task<T>` without parking the caller, so only the `await` would suspend.
    #[test]
    fn lock_check_38_bare_async_call_in_body_is_not_a_suspension() {
        assert_lock_accepted(
            "async fn work() -> i64 { await sleep(1); return 1; } \
             async fn main() { let m = Mutex::new(0); \
             lock m as value { let t = work(); } }",
        );
    }

    // Perspective 39: E2604 follows a synchronous helper call. Looking only at
    // expressions physically inside the lock would miss this Channel wait.
    #[test]
    fn lock_check_39_helper_channel_recv_is_rejected_transitively() {
        assert_lock_error_count(
            "fn receive(ch: Channel<i64>) -> i64 { return ch.recv(); } \
             async fn main() { let m = Mutex::new(0); let ch: Channel<i64> = Channel::new(); \
             lock m as value { let got = receive(ch); } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 40: the fixpoint covers forward declarations and more than
    // one helper edge, not only an immediate callee.
    #[test]
    fn lock_check_40_forward_two_hop_wait_is_rejected() {
        assert_lock_error_count(
            "fn outer(ch: Channel<i64>) -> i64 { return inner(ch); } \
             fn inner(ch: Channel<i64>) -> i64 { return ch.recv(); } \
             async fn main() { let m = Mutex::new(0); let ch: Channel<i64> = Channel::new(); \
             lock m as value { let got = outer(ch); } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 41: concrete instance dispatch is resolved by receiver type
    // and follows the actual declaring class body.
    #[test]
    fn lock_check_41_concrete_method_wait_is_rejected() {
        assert_lock_error_count(
            "class Receiver { pub fn take(self, ch: Channel<i64>) -> i64 { return ch.recv(); } } \
             async fn main() { let m = Mutex::new(0); let ch: Channel<i64> = Channel::new(); \
             let r = new Receiver(); lock m as value { let got = r.take(ch); } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 42: static user methods participate in the same call graph.
    #[test]
    fn lock_check_42_static_method_wait_is_rejected() {
        assert_lock_error_count(
            "class Receiver { pub static fn take(ch: Channel<i64>) -> i64 { return ch.recv(); } } \
             async fn main() { let m = Mutex::new(0); let ch: Channel<i64> = Channel::new(); \
             lock m as value { let got = Receiver::take(ch); } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 43: native-blocking compatibility accessors are forbidden
    // directly; otherwise one lock holder can block an entire worker on a
    // second native lock.
    #[test]
    fn lock_check_43_direct_blocking_cell_access_is_rejected() {
        assert_lock_error_count(
            "async fn main() { let m = Mutex::new(0); let c = BlockingCell::new(1); \
             lock m as value { let got = c.get(); } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 44: MAY_BLOCK propagates through helpers just like
    // MAY_SUSPEND.
    #[test]
    fn lock_check_44_helper_rwlock_access_is_rejected_transitively() {
        assert_lock_error_count(
            "fn read(r: BlockingRwCell<i64>) -> i64 { return r.read(); } \
             async fn main() { let m = Mutex::new(0); let r = BlockingRwCell::new(1); \
             lock m as value { let got = read(r); } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 45: recursive SCCs converge once any member reaches a wait.
    #[test]
    fn lock_check_45_recursive_helper_cycle_reaching_wait_is_rejected() {
        assert_lock_error_count(
            "fn a(ch: Channel<i64>, n: i64) -> i64 { if n == 0 { return ch.recv(); } return b(ch, n - 1); } \
             fn b(ch: Channel<i64>, n: i64) -> i64 { return a(ch, n); } \
             async fn main() { let m = Mutex::new(0); let ch: Channel<i64> = Channel::new(); \
             lock m as value { let got = b(ch, 1); } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 46: constructing a lambda does not execute its body, so its
    // wait is not attributed to the enclosing function or lock.
    #[test]
    fn lock_check_46_lambda_body_is_an_effect_boundary() {
        assert_lock_accepted(
            "async fn main() { let m = Mutex::new(0); let ch: Channel<i64> = Channel::new(); \
             lock m as value { let receive = |c: Channel<i64>| c.recv(); } }",
        );
    }

    // Perspective 47: an effect-free synchronous helper remains legal.
    #[test]
    fn lock_check_47_effect_free_helper_is_accepted() {
        assert_lock_accepted(
            "fn add_one(n: i64) -> i64 { return n + 1; } \
             async fn main() { let m = Mutex::new(0); \
             lock m as value { let next = add_one(value); } }",
        );
    }

    // Perspective 48: synchronous filesystem compatibility APIs block the
    // current worker, and that MAY_BLOCK effect propagates through user code.
    #[test]
    fn lock_check_48_helper_sync_fs_call_is_rejected_transitively() {
        assert_lock_error_count(
            "import std::fs; \
             fn exists(path: String) -> bool { return fs::exists(path); } \
             async fn main() { let m = Mutex::new(0); \
             lock m as value { let present = exists(\"/tmp/willow-lock-effect\"); } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 49: an aliased std module retains its effect identity.
    #[test]
    fn lock_check_49_aliased_sync_fs_call_is_rejected_directly() {
        assert_lock_error_count(
            "import std::fs as files; async fn main() { let m = Mutex::new(0); \
             lock m as value { let present = files::exists(\"/tmp/willow-lock-effect\"); } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 50: calling an async FS API only creates its Task. As with
    // any async fn, a bare call is legal here; awaiting it would be rejected by
    // the ordinary await rule.
    #[test]
    fn lock_check_50_bare_async_fs_call_is_accepted() {
        assert_lock_accepted(
            "import std::fs; async fn main() { let m = Mutex::new(0); \
             lock m as value { let task = fs::exists_async(\"/tmp/willow-lock-effect\"); } }",
        );
    }

    // Perspective 51: explicit constructors are synchronous callable bodies,
    // so `new` must carry their transitive effect too.
    #[test]
    fn lock_check_51_constructor_wait_is_rejected_transitively() {
        assert_lock_error_count(
            "class Receiver { init(self, ch: Channel<i64>) { let got = ch.recv(); } } \
             async fn main() { let m = Mutex::new(0); let ch: Channel<i64> = Channel::new(); \
             lock m as value { let receiver = new Receiver(ch); } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 52: two reachable effects must not let HashSet randomization
    // choose a different secondary diagnostic from process to process.
    #[test]
    fn lock_check_52_transitive_effect_witness_is_deterministic() {
        let source = "fn a_block(r: BlockingRwCell<i64>) -> i64 { return r.read(); } \
                      fn z_suspend(ch: Channel<i64>) -> i64 { return ch.recv(); } \
                      fn both(r: BlockingRwCell<i64>, ch: Channel<i64>) -> i64 { \
                          let ignored = z_suspend(ch); return a_block(r); \
                      } \
                      async fn main() { let m = Mutex::new(0); let r = BlockingRwCell::new(1); \
                          let ch: Channel<i64> = Channel::new(); \
                          lock m as value { let got = both(r, ch); } }";
        for _ in 0..32 {
            let errors = lock_errors_ignoring_gate(source);
            let diagnostic = errors
                .iter()
                .find(|error| error.code == ErrorCode::E2604)
                .expect("transitive lock effect");
            assert!(
                diagnostic
                    .labels
                    .iter()
                    .any(|label| label.message.contains("BlockingRwCell.read")),
                "stable lexical witness must be a_block: {diagnostic:?}"
            );
            assert!(
                !diagnostic
                    .labels
                    .iter()
                    .any(|label| label.message.contains("Channel.recv")),
                "randomized witness: {diagnostic:?}"
            );
        }
    }

    // Perspective 53: dynamic interface dispatch is a union over concrete
    // implementations. A blocking body must not disappear merely because the
    // receiver was widened to the interface type.
    #[test]
    fn lock_check_53_interface_dispatch_wait_is_rejected() {
        assert_lock_error_count(
            "interface Receiver { fn run(self); } \
             class Impl implements Receiver { \
                 pub fn run(self) { let got = self.channel.recv(); } \
                 pub channel: Channel<i64>; \
             } \
             async fn main() { let m = Mutex::new(0); let ch: Channel<i64> = Channel::new(); \
                 let receiver: Receiver = new Impl(ch); \
                 lock m as value { receiver.run(); } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 54: the union is effect-based, not a blanket ban on
    // interface calls. An interface whose implementations are all pure stays
    // usable inside a critical section.
    #[test]
    fn lock_check_54_pure_interface_dispatch_is_accepted() {
        assert_lock_accepted(
            "interface Reader { fn read(self) -> i64; } \
             class One implements Reader { pub fn read(self) -> i64 { return 1; } } \
             class Two implements Reader { pub fn read(self) -> i64 { return 2; } } \
             async fn main() { let m = Mutex::new(0); let reader: Reader = new One(); \
                 lock m as value { println(reader.read()); } }",
        );
    }

    // Perspective 55: one waiting implementation taints the interface method
    // even when the value currently assigned at the call site is a pure class.
    // Dispatch remains open to every conforming implementation of the type.
    #[test]
    fn lock_check_55_interface_union_includes_every_implementation() {
        assert_lock_error_count(
            "interface Receiver { fn run(self); } \
             class Pure implements Receiver { pub fn run(self) { println(1); } } \
             class Waiting implements Receiver { \
                 pub channel: Channel<i64>; \
                 pub fn run(self) { let got = self.channel.recv(); } \
             } \
             async fn main() { let m = Mutex::new(0); let receiver: Receiver = new Pure(); \
                 lock m as value { receiver.run(); } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 56: the interface union participates in the ordinary
    // fixpoint on both sides: lock -> helper -> interface -> implementation ->
    // helper -> Channel.recv.
    #[test]
    fn lock_check_56_interface_dispatch_propagates_through_helpers() {
        assert_lock_error_count(
            "fn receive(ch: Channel<i64>) -> i64 { return ch.recv(); } \
             interface Receiver { fn run(self) -> i64; } \
             class Impl implements Receiver { \
                 pub channel: Channel<i64>; \
                 pub fn run(self) -> i64 { return receive(self.channel); } \
             } \
             fn invoke(receiver: Receiver) -> i64 { return receiver.run(); } \
             async fn main() { let m = Mutex::new(0); let ch: Channel<i64> = Channel::new(); \
                 let receiver: Receiver = new Impl(ch); \
                 lock m as value { let got = invoke(receiver); } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 57: conformance can be satisfied by a base-class method;
    // the interface edge must target that declaring body, not a nonexistent
    // method node on the derived class.
    #[test]
    fn lock_check_57_interface_dispatch_follows_inherited_method_body() {
        assert_lock_error_count(
            "interface Receiver { fn run(self); } \
             open class Base { \
                 pub channel: Channel<i64>; \
                 pub fn run(self) { let got = self.channel.recv(); } \
             } \
             class Impl extends Base implements Receiver {} \
             async fn main() { let m = Mutex::new(0); let ch: Channel<i64> = Channel::new(); \
                 let receiver: Receiver = new Impl(ch); \
                 lock m as value { receiver.run(); } }",
            ErrorCode::E2604,
            1,
        );
    }

    // Perspective 58: effects are only rejected at lock-held call sites. The
    // same interface dispatch remains legal in ordinary synchronous code.
    #[test]
    fn lock_check_58_waiting_interface_call_outside_lock_is_accepted() {
        assert_lock_accepted(
            "interface Receiver { fn run(self) -> i64; } \
             class Impl implements Receiver { \
                 pub channel: Channel<i64>; \
                 pub fn run(self) -> i64 { return self.channel.recv(); } \
             } \
             fn invoke(receiver: Receiver) -> i64 { return receiver.run(); } \
             fn main() {}",
        );
    }

    #[test]
    fn malformed_builtin_generic_arity_stops_at_void_fail_safe() {
        for (source_name, module, item, args) in [
            ("Map", "collections", "Map", vec![Type::I64]),
            ("Option", "option", "Option", vec![Type::I64, Type::String]),
            ("Result", "result", "Result", vec![Type::I64]),
        ] {
            let mut checker = TypeChecker::new();
            let normalized =
                checker.normalize_std_type_item(source_name, module, item, args, Span::dummy());
            assert_eq!(normalized, Type::Void);
            assert!(
                checker
                    .errors
                    .iter()
                    .any(|error| error.code == ErrorCode::E0201),
                "missing arity diagnostic: {:?}",
                checker.errors
            );
        }
    }

    // ── Panic/recover runtime surface (willow-s9ej.2) ──────────────────────

    #[test]
    fn panic_surface_01_recover_has_option_panic_info_type() {
        assert_typecheck_ok("fn main() { let value: Option<PanicInfo> = recover(); }");
    }

    #[test]
    fn panic_surface_02_panic_info_fields_are_readable_with_fixed_types() {
        assert_typecheck_ok(
            "fn inspect(info: PanicInfo) { let message: String = info.message; let file: String = info.file; let line: i64 = info.line; let column: i64 = info.column; } fn main() {}",
        );
    }

    #[test]
    fn panic_surface_03_runtime_only_construction_is_rejected() {
        let errors =
            check_source("fn main() { let info = new PanicInfo(\"boom\", \"main.wi\", 1, 2); }");
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("only be constructed by the runtime")),
            "{errors:?}"
        );
    }

    #[test]
    fn panic_surface_04_subclassing_is_rejected() {
        let errors = check_source("class Child extends PanicInfo {} fn main() {}");
        assert!(
            errors.iter().any(|error| error.code == ErrorCode::E0701),
            "{errors:?}"
        );
    }

    #[test]
    fn panic_surface_05_field_assignment_is_rejected() {
        let errors =
            check_source("fn alter(info: PanicInfo) { info.message = \"changed\"; } fn main() {}");
        assert!(
            errors.iter().any(|error| error
                .message
                .contains("fields of `PanicInfo` are read-only")),
            "{errors:?}"
        );
    }

    #[test]
    fn panic_surface_06_panic_info_name_is_reserved() {
        let errors = check_source("class PanicInfo {} fn main() {}");
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("reserved runtime type")),
            "{errors:?}"
        );
    }

    #[test]
    fn panic_surface_07_recover_name_is_reserved() {
        let errors = check_source("fn recover() -> i64 { return 1; } fn main() {}");
        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("reserved builtin function")),
            "{errors:?}"
        );
    }

    #[test]
    fn panic_surface_08_direct_discarded_defer_recover_is_rejected() {
        let errors = check_source("fn main() { defer recover(); }");
        assert!(
            errors.iter().any(|error| {
                error.code == ErrorCode::E0905
                    && error.message.contains("discards the panic metadata")
            }),
            "{errors:?}"
        );
    }

    #[test]
    fn panic_surface_09_deferred_block_can_typecheck_recover_result() {
        assert_typecheck_ok("fn main() { defer { let value: Option<PanicInfo> = recover(); } }");
    }

    // Review follow-ups (willow-s9ej.10). A bare `recover(...)` call always
    // lowers to the builtin, so any binding of that name would be silently
    // ignored at the call site: the name is reserved at every binding site,
    // not only for top-level functions.

    #[test]
    fn panic_surface_10_let_binding_named_recover_is_rejected() {
        assert_typecheck_error_contains(
            "fn main() { let recover = 1; }",
            ErrorCode::E0351,
            "reserved builtin function",
        );
    }

    #[test]
    fn panic_surface_11_parameter_named_recover_is_rejected() {
        assert_typecheck_error_contains(
            "fn take(recover: i64) -> i64 { return recover; } fn main() {}",
            ErrorCode::E0351,
            "reserved builtin function",
        );
    }

    #[test]
    fn panic_surface_12_for_binding_named_recover_is_rejected() {
        assert_typecheck_error_contains(
            "fn main() { for recover in 0..3 { println(recover); } }",
            ErrorCode::E0351,
            "reserved builtin function",
        );
    }

    #[test]
    fn panic_surface_13_lambda_parameter_named_recover_is_rejected() {
        assert_typecheck_error_contains(
            "fn main() { let f = |recover: i64| { return recover; }; println(f(1)); }",
            ErrorCode::E0351,
            "reserved builtin function",
        );
    }

    #[test]
    fn panic_surface_14_match_binding_named_recover_is_rejected() {
        assert_typecheck_error_contains(
            r#"fn main() { let value: Option<i64> = Some(1); match value { Some(recover) => println(recover), None => {} } }"#,
            ErrorCode::E0351,
            "reserved builtin function",
        );
    }

    #[test]
    fn panic_surface_15_reservation_does_not_reach_class_members() {
        // A member named `recover` is only reachable through a receiver, so it
        // can never be confused with the bare-name builtin call.
        assert_typecheck_ok(
            r#"
class Box {
    recover: i64;

    pub init(self, value: i64) { self.recover = value; }

    pub fn recover(self) -> i64 { return self.recover; }
}

fn main() { let b = new Box(1); println(b.recover()); }
"#,
        );
    }

    #[test]
    fn panic_surface_16_mutable_reference_to_panic_info_field_is_rejected() {
        assert_typecheck_error_contains(
            "fn overwrite(value: &mut i64) { value = 1; } fn alter(info: PanicInfo) { overwrite(&info.line); } fn main() {}",
            ErrorCode::E1701,
            "fields of `PanicInfo` are read-only",
        );
    }

    #[test]
    fn panic_surface_17_shared_reference_to_panic_info_field_is_allowed() {
        assert_typecheck_ok(
            "fn peek(value: &i64) { println(value); } fn show(info: PanicInfo) { peek(&info.line); } fn main() {}",
        );
    }

    #[test]
    fn panic_surface_18_mutable_reference_to_an_ordinary_field_still_works() {
        assert_typecheck_ok(
            r#"
class Counter {
    pub value: i64;

    pub init(self, value: i64) { self.value = value; }
}

fn bump(value: &mut i64) { value = value + 1; }

fn main() { let c = new Counter(1); bump(&c.value); println(c.value); }
"#,
        );
    }
}
