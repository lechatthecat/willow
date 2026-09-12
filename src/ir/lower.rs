//! Lowering: type-checked AST → typed HIR ([`super::typed_ast`]) — willow-mb5.
//!
//! Coverage so far: the MVP-core constructs (literals, variables, arithmetic/
//! comparison/logical/unary operators, free-function calls, `print`, and
//! the `let`/assign/`if`/`while`/`return` statements); array literals, indexing,
//! and the ternary operator; classes — `new`, object literals, field access,
//! method calls (instance members resolved along the base-class chain, so
//! inheritance works), static field reads and static calls, and class method
//! bodies (receiver bound as `self`); array `for` loops; and field/index/static
//! assignment statements. Type information flows in through a [`LowerCtx`]
//! (parameter/`let` bindings, free-function return types, and per-class
//! field/method/static-member types) and is attached to every [`HirExpr`], so a
//! downstream consumer never has to re-derive a type from the AST. Also
//! covered: ranges and range `for` (`Range<i64>`, i64 elements), async calls
//! (`Task<T>` at the call site) and `await` (unwraps `Task`/`Future`), `?`
//! propagation (unwraps `Result`/`Option`), annotated lambdas (`fn(..) -> R`
//! types, indirect calls through fn-typed variables), constructors (lowered as
//! `init` with `self` bound) with `super.init`, and the checker's builtin
//! functions (seeded registry). Constructs not yet covered (`match`, maps,
//! unannotated lambda params, and generic substitution) return a diagnostic
//! rather than silently dropping work, so later slices can extend coverage
//! incrementally without changing behavior.

use std::collections::HashMap;

use crate::diagnostics::{Diagnostic, ErrorCode, Severity, Span};
use crate::parser::ast::{
    AwaitExpr, BinOp, BinaryExpr, Block, CallArg, CallArgMode, CallExpr, DeferBody, Expr, ExprId,
    FunctionDecl, Item, LambdaExpr, MethodCallExpr, MethodDecl, NewExpr, ObjectLiteralExpr,
    PatternId, Program, RangeExpr, SelectCaseKind, SelectExpr, StaticCallExpr, StaticFieldExpr,
    Stmt, TernaryExpr, Type, UnaryExpr, UnaryOp,
};
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::symbols;
use crate::semantic::type_checker::LambdaCapture;
use crate::semantic::type_checker::types::await_output_type;

use super::typed_ast::{
    HirCapture, HirClass, HirDeferBody, HirDeferId, HirExpr, HirExprKind, HirFunction, HirMatchArm,
    HirParam, HirPattern, HirProgram, HirSelectCase, HirSelectCaseKind, HirStmt,
};

/// Type-checker side tables the lowering can consume to close gaps the
/// immutable AST cannot express (willow-mb5 checker pivot, step 1). These are
/// the same tables the checker already hands to the backend.
#[derive(Default)]
pub struct CheckerTables<'a> {
    /// Canonical global declarations copied into the owned HIR snapshot.
    pub symbols: Option<&'a symbols::SymbolTable>,
    /// Unqualified enum-variant constructions (`Ok(42)` in an expected-enum
    /// position), keyed by the call's node ID; the value is the resolved enum
    /// name and the variant is the call's callee.
    pub enum_variant_resolutions: Option<&'a HashMap<ExprId, String>>,
    /// Checked reinterpretations of bare match patterns. When this table is
    /// present, an absent entry preserves the parsed binding or downcast;
    /// knowing an enum's identity does not grant access to its bare variants.
    pub pattern_resolutions: Option<&'a HashMap<PatternId, crate::parser::ast::Pattern>>,
    /// The checker's authoritative type for every checked expression, keyed by
    /// node ID — the final fallback when the structural lowering cannot derive
    /// a type (generic constructions, `Self::` calls, module-qualified items).
    pub expr_types: Option<&'a HashMap<ExprId, Type>>,
    /// Every enum the checker registered, under every name it registered it
    /// by: a program's own `Color`, an imported module's `palette::Color`, and
    /// the bare local name a direct type import binds (`Kind` for
    /// `import shapes::Kind;`). The lowering's own sweep only sees THIS unit's
    /// `enum` items, so without this table a `match` on any enum that came
    /// from another file decides its scrutinee is not an enum at all
    /// (willow-28h8 case B).
    pub enums: Option<&'a HashMap<crate::semantic::ids::TypeId, symbols::EnumInfo>>,
    /// What the checker made of each type ANNOTATION it normalized, keyed by
    /// the written spelling (willow-0g8j.3). Lowering reads annotations off the
    /// AST, where `Arr<i64>` (an aliased import), `std::result::Result<i64, E>`
    /// (a fully qualified std path) and a module's own `Level` (an enum whose
    /// identity is `signal::Level`) are still the words the source used; every
    /// other phase sees the normalized type, so without this the LIR walker
    /// meets a named type nothing has heard of and turns the body down.
    pub normalized_types: Option<&'a HashMap<Type, Type>>,
    /// What a static call's written class name resolved to, keyed by the call's
    /// node ID, for the calls where the two differ (willow-0g8j.3).
    pub static_call_classes: Option<&'a HashMap<ExprId, String>>,
    /// What each lambda captures from its enclosing function, keyed by the
    /// lambda's node ID and ordered as the closure environment lays its slots out
    /// (willow-0g8j.2.12). Only the checker knows this: capture is decided by
    /// name resolution, which the AST does not record.
    pub lambda_captures: Option<&'a HashMap<ExprId, Vec<LambdaCapture>>>,
}

#[willow_continuations::methods(normalize)]
impl<'a> CheckerTables<'a> {
    /// Borrow the relevant tables from a run type checker.
    pub fn from_checker(checker: &'a crate::semantic::TypeChecker) -> Self {
        Self {
            symbols: Some(&checker.symbols),
            enum_variant_resolutions: Some(&checker.enum_variant_resolutions),
            pattern_resolutions: Some(&checker.pattern_resolutions),
            expr_types: Some(&checker.expr_types),
            enums: Some(&checker.symbols.enums),
            normalized_types: Some(&checker.normalized_types),
            static_call_classes: Some(&checker.static_call_classes),
            lambda_captures: Some(&checker.lambda_captures),
        }
    }

    /// Re-spell a written annotation as the checker's type. Bottom-up, so a
    /// spelling inside a type argument is rewritten even when the whole type
    /// was never written that way.
    pub(crate) fn normalize(&self, ty: &Type) -> Type {
        self.normalize_declared(ty, &[])
    }

    /// Symbolic declaration parameters must survive until instantiation.
    fn normalize_declared(&self, ty: &Type, bound: &[String]) -> Type {
        if let Type::Named(name) = ty
            && bound.contains(name)
        {
            return ty.clone();
        }
        let ty = self.normalized_types.and_then(|m| m.get(ty)).unwrap_or(ty);
        let rebuilt = match ty {
            Type::Named(name) => self
                .enums
                .and_then(|enums| enums.get(&crate::semantic::ids::TypeId::from_source_name(name)))
                .map(|info| Type::Named(info.name.clone()))
                .or_else(|| {
                    self.symbols
                        .and_then(|symbols| symbols.lookup_class(name))
                        .map(|info| Type::Named(info.name.clone()))
                })
                .or_else(|| {
                    self.symbols
                        .and_then(|symbols| symbols.lookup_interface(name))
                        .map(|info| Type::Named(info.name.clone()))
                })
                .unwrap_or_else(|| ty.clone()),
            Type::Array(elem) => Type::Array(Box::new(self.normalize_declared(elem, bound))),
            Type::Generic(name, args) => Type::Generic(
                self.symbols
                    .and_then(|symbols| symbols.lookup_interface(name))
                    .map(|info| info.name.clone())
                    .or_else(|| {
                        self.enums
                            .and_then(|enums| {
                                enums.get(&crate::semantic::ids::TypeId::from_source_name(name))
                            })
                            .map(|info| info.name.clone())
                    })
                    .unwrap_or_else(|| name.clone()),
                args.iter()
                    .map(|a| self.normalize_declared(a, bound))
                    .collect(),
            ),
            Type::Fn(params, ret) => Type::Fn(
                params
                    .iter()
                    .map(|p| self.normalize_declared(p, bound))
                    .collect(),
                Box::new(self.normalize_declared(ret, bound)),
            ),
            Type::Closure(params, ret) => Type::Closure(
                params
                    .iter()
                    .map(|p| self.normalize_declared(p, bound))
                    .collect(),
                Box::new(self.normalize_declared(ret, bound)),
            ),
            other => other.clone(),
        };
        match self.normalized_types.and_then(|m| m.get(&rebuilt)) {
            Some(normalized) => normalized.clone(),
            None => rebuilt,
        }
    }

    /// A lambda's full inferred callable type, from the checker's expression
    /// table — including the parameter types a call site supplied, which the
    /// AST cannot spell. Either callable form answers: which one it is says
    /// whether the lambda captures, not what its signature is.
    fn lambda_fn_type(&self, id: &ExprId) -> Option<&Type> {
        match self.expr_types.and_then(|m| m.get(id)) {
            Some(ty @ (Type::Fn(..) | Type::Closure(..))) => Some(ty),
            _ => None,
        }
    }

    /// What the lambda at `span` captures, in environment-slot order.
    fn lambda_captures(&self, id: &ExprId) -> &[LambdaCapture] {
        self.lambda_captures
            .and_then(|m| m.get(id))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    fn enum_variant_resolution(&self, id: &ExprId) -> Option<&String> {
        self.enum_variant_resolutions.and_then(|m| m.get(id))
    }

    /// The class a static call actually names, given the written spelling.
    fn static_call_class(&self, id: &ExprId, written: &str) -> String {
        self.static_call_classes
            .and_then(|m| m.get(id))
            .cloned()
            .unwrap_or_else(|| written.to_string())
    }

    fn expr_type(&self, id: &ExprId) -> Option<Type> {
        self.expr_types.and_then(|m| m.get(id).cloned())
    }

    /// The checker's enums, re-keyed by the name they are WRITTEN with —
    /// `TypeId`'s own rendering, so `palette::Color` stays qualified and a
    /// direct type import's local `Kind` stays bare. Those are exactly the
    /// names a scrutinee's `Type::Named` carries (willow-28h8 case B).
    fn declared_enums(&self) -> impl Iterator<Item = (String, EnumInfo)> + '_ {
        self.enums.into_iter().flatten().map(|(id, info)| {
            (
                id.to_string(),
                EnumInfo {
                    type_params: info.type_params.clone(),
                    variants: info
                        .variants
                        .iter()
                        .map(|v| (v.name.clone(), v.payload_types.clone()))
                        .collect(),
                },
            )
        })
    }
}

/// Lower a whole program's free functions to typed HIR. Non-function items and
/// constructs outside slice 1 are reported as diagnostics; the functions that
/// do lower cleanly are still returned, so callers can make progress.
pub fn lower_program(program: &Program) -> (HirProgram, Vec<Diagnostic>) {
    lower_program_with(program, &CheckerTables::default())
}

/// Like [`lower_program`], additionally consulting the type checker's side
/// tables for information the structural lowering cannot derive.
pub fn lower_program_with(
    program: &Program,
    tables: &CheckerTables,
) -> (HirProgram, Vec<Diagnostic>) {
    // Builtin functions the checker registers (register_builtin_functions):
    // their call-site types, so calls to them lower like any other call.
    let mut fn_returns: HashMap<String, Type> = HashMap::from([
        ("pow".to_string(), Type::F64),
        ("powf".to_string(), Type::F64),
        ("gc_collect".to_string(), Type::Void),
        ("gc_minor_collect".to_string(), Type::Void),
        ("gc_allocated_bytes".to_string(), Type::I64),
        ("gc_tlab_fast_allocations".to_string(), Type::I64),
        ("gc_tlab_slow_allocations".to_string(), Type::I64),
        ("gc_tlab_refills".to_string(), Type::I64),
        ("gc_tlab_large_allocations".to_string(), Type::I64),
        ("gc_tlab_reserved_bytes".to_string(), Type::I64),
        ("gc_minor_collections".to_string(), Type::I64),
        ("gc_promoted_objects".to_string(), Type::I64),
        ("gc_moved_objects".to_string(), Type::I64),
        ("gc_remembered_set_size".to_string(), Type::I64),
        ("gc_dirty_card_count".to_string(), Type::I64),
        ("gc_write_barrier_hits".to_string(), Type::I64),
        ("gc_old_region_count".to_string(), Type::I64),
        ("gc_old_region_reserved_bytes".to_string(), Type::I64),
        ("gc_old_region_live_bytes".to_string(), Type::I64),
        ("gc_old_region_fragmentation_bytes".to_string(), Type::I64),
        ("gc_large_object_region_count".to_string(), Type::I64),
        ("gc_pinned_region_count".to_string(), Type::I64),
        ("gc_old_region_allocations".to_string(), Type::I64),
        ("gc_old_region_reuses".to_string(), Type::I64),
        ("gc_old_regions_released".to_string(), Type::I64),
        ("gc_major_collections".to_string(), Type::I64),
        ("panic".to_string(), Type::Never),
        (
            "recover".to_string(),
            Type::Generic(
                "Option".to_string(),
                vec![Type::Named("PanicInfo".to_string())],
            ),
        ),
        ("format".to_string(), Type::String),
        (
            "sleep".to_string(),
            Type::Generic("Future".to_string(), vec![Type::Void]),
        ),
        (
            "yield".to_string(),
            Type::Generic("Future".to_string(), vec![Type::Void]),
        ),
    ]);
    // `PanicInfo` first: a program cannot redeclare it (the checker refuses),
    // so nothing here overwrites it.
    let mut classes = Classes::with_runtime_types();
    let mut enums = Enums::with_prelude();
    // Before this unit's own items, so a locally declared enum still shadows
    // an imported one of the same name below (willow-28h8 case B).
    for (name, info) in tables.declared_enums() {
        enums.map.insert(name, info);
    }
    for item in &program.items {
        match item {
            Item::Function(f) => {
                fn_returns.insert(
                    f.name.clone(),
                    call_site_type(&tables.normalize(&f.return_type), f.is_async),
                );
            }
            Item::Class(c) => {
                let mut info = ClassInfo {
                    base: c.base_class.as_ref().map(|b| b.name().to_string()),
                    ..ClassInfo::default()
                };
                for f in &c.fields {
                    if f.is_static {
                        info.static_fields
                            .insert(f.name.clone(), tables.normalize(&f.ty));
                    } else {
                        info.fields.insert(f.name.clone(), tables.normalize(&f.ty));
                    }
                }
                for m in &c.methods {
                    let call_ty = call_site_type(&tables.normalize(&m.return_type), m.is_async);
                    if m.is_static {
                        info.static_methods.insert(m.name.clone(), call_ty);
                    } else {
                        info.methods.insert(m.name.clone(), call_ty);
                    }
                }
                classes.map.insert(c.name.clone(), info);
            }
            // After the prelude's own, so a program enum of the same name
            // shadows it — the order `register_prelude` gives the checker.
            Item::Enum(e) => {
                enums.map.insert(e.name.clone(), enum_info(e));
            }
            _ => {}
        }
    }

    let mut functions = Vec::new();
    let mut hir_classes = Vec::new();
    let mut diagnostics = Vec::new();
    for item in &program.items {
        match item {
            Item::Function(f) => match lower_function(f, &fn_returns, &classes, &enums, tables) {
                Ok(func) => functions.push(func),
                Err(d) => diagnostics.push(d),
            },
            Item::Class(c) => {
                let mut methods = Vec::new();
                for ctor in &c.constructors {
                    match lower_constructor(ctor, &c.name, &fn_returns, &classes, &enums, tables) {
                        Ok(func) => methods.push(func),
                        Err(d) => diagnostics.push(d),
                    }
                }
                for m in &c.methods {
                    match lower_method(m, &c.name, &fn_returns, &classes, &enums, tables) {
                        Ok(func) => methods.push(func),
                        Err(d) => diagnostics.push(d),
                    }
                }
                for field in c.fields.iter().filter(|field| field.is_static) {
                    if let Some(init) = &field.initializer {
                        let mut ctx = LowerCtx::new(&fn_returns, &classes, &enums, tables);
                        match lower_expr(init, &mut ctx) {
                            Ok(mut value) => {
                                let return_type = ctx.normalize(&field.ty);
                                retype_array_literal(&mut value, &return_type);
                                functions.push(HirFunction {
                                    name: crate::semantic::ids::FunctionId::method(
                                        crate::semantic::ids::TypeId::from_source_name(&c.name),
                                        format!("$static_init.{}", field.name),
                                    ),
                                    is_async: false,
                                    params: Vec::new(),
                                    return_type: return_type.into(),
                                    body: vec![HirStmt::Return {
                                        value: Some(value),
                                        span: init.span(),
                                    }],
                                    span: init.span(),
                                });
                            }
                            Err(diagnostic) => diagnostics.push(diagnostic),
                        }
                    }
                }
                hir_classes.push(HirClass {
                    name: c.name.clone().into(),
                    methods,
                    span: c.span,
                });
            }
            _ => {}
        }
    }
    (
        HirProgram {
            functions,
            classes: hir_classes,
            resolution: lower_resolution(program, tables),
        },
        diagnostics,
    )
}

/// The type a CALL to a function/method produces at the call site: an async
/// fn's call captures its arguments into a `Task<T>`; `await` unwraps it.
fn call_site_type(return_type: &Type, is_async: bool) -> Type {
    if is_async {
        Type::Generic("Task".to_string(), vec![return_type.clone()])
    } else {
        return_type.clone()
    }
}

/// One enum's declared shape: its type parameters and each variant's payload
/// types (with type parameters still symbolic, substituted per use site).
#[derive(Default, Clone)]
struct EnumInfo {
    type_params: Vec<String>,
    variants: HashMap<String, Vec<Type>>,
}

/// All enums in the program plus the prelude's, used to type variant
/// constructions and to bind `match` pattern payloads.
#[derive(Default)]
struct Enums {
    map: HashMap<String, EnumInfo>,
}

/// The prelude's enums, read from [`crate::prelude::PRELUDE_SOURCE`] itself.
///
/// Parsed rather than transcribed (willow-0g8j.2.13): the list used to be a
/// hand-written `Option` + `Result` pair, so `IoError`, `ParseFloatError` and
/// `Cancelled` had no entry here. An unqualified pattern on one of them —
/// `Failed(msg)` on an `IoError` — then missed the variant lookup in
/// [`lower_match`] and lowered as an interface downcast onto a class named
/// `Failed`, a type that does not exist. Reading the prelude keeps the two in
/// step for every enum it declares now and every one it gains later.
fn prelude_enums() -> &'static HashMap<String, EnumInfo> {
    static PRELUDE: std::sync::OnceLock<HashMap<String, EnumInfo>> = std::sync::OnceLock::new();
    PRELUDE.get_or_init(|| {
        let tokens = crate::lexer::Lexer::new(crate::prelude::PRELUDE_SOURCE)
            .tokenize()
            .expect("the prelude lexes");
        let (prelude, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "the prelude parses: {errors:?}");
        prelude
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Enum(e) => Some((e.name.clone(), enum_info(e))),
                _ => None,
            })
            .collect()
    })
}

/// One enum declaration's lowering-side shape.
fn enum_info(e: &crate::parser::ast::EnumDecl) -> EnumInfo {
    EnumInfo {
        type_params: e.type_params.clone(),
        variants: e
            .variants
            .iter()
            .map(|v| (v.name.clone(), v.payload.clone()))
            .collect(),
    }
}

impl Enums {
    fn with_prelude() -> Self {
        Self {
            map: prelude_enums().clone(),
        }
    }
}

/// Substitute symbolic type parameters (`Named(param)`) with concrete types.
fn subst_type(ty: &Type, subst: &HashMap<String, Type>) -> Type {
    ty.substitute_names(|name| subst.get(name).cloned())
}

/// Type information for one class, collected from its declaration.
#[derive(Default)]
struct ClassInfo {
    fields: HashMap<String, Type>,
    methods: HashMap<String, Type>,
    static_fields: HashMap<String, Type>,
    static_methods: HashMap<String, Type>,
    base: Option<String>,
}

/// All classes in the program, used to type field/method access, `new`, static
/// reads/calls, and to resolve inherited members along the base-class chain.
#[derive(Default)]
struct Classes {
    map: HashMap<String, ClassInfo>,
}

impl Classes {
    /// The classes no source file declares. `PanicInfo` is the only one: the
    /// checker defines it, the runtime is the only thing that builds one, and a
    /// `recover()` handler reads its fields like any other object's. Without it
    /// here, `info.line` found no field on its receiver and the enclosing
    /// function fell back to the AST emitter whole (willow-0g8j.3).
    fn with_runtime_types() -> Self {
        let panic_info = ClassInfo {
            fields: builtin_types::panic_info_fields()
                .into_iter()
                .map(|(name, ty)| (name.to_string(), ty))
                .collect(),
            ..ClassInfo::default()
        };
        Self {
            map: HashMap::from([("PanicInfo".to_string(), panic_info)]),
        }
    }

    /// Walk the base-class chain from `class`, returning the first member type
    /// `pick` finds. Stops if a base class is not in the program (e.g. external).
    fn resolve<F: Fn(&ClassInfo) -> Option<Type>>(&self, class: &str, pick: F) -> Option<Type> {
        let mut current = Some(class);
        while let Some(name) = current {
            let info = self.map.get(name)?;
            if let Some(ty) = pick(info) {
                return Some(ty);
            }
            current = info.base.as_deref();
        }
        None
    }

    fn field_type(&self, class: &str, field: &str) -> Option<Type> {
        self.resolve(class, |info| info.fields.get(field).cloned())
    }

    fn method_type(&self, class: &str, method: &str) -> Option<Type> {
        self.resolve(class, |info| info.methods.get(method).cloned())
    }

    fn static_field_type(&self, class: &str, field: &str) -> Option<Type> {
        self.resolve(class, |info| info.static_fields.get(field).cloned())
    }

    fn static_method_type(&self, class: &str, method: &str) -> Option<Type> {
        self.resolve(class, |info| info.static_methods.get(method).cloned())
    }
}

/// Lower a single free function against the program's function signatures.
fn lower_function(
    f: &FunctionDecl,
    fn_returns: &HashMap<String, Type>,
    classes: &Classes,
    enums: &Enums,
    tables: &CheckerTables,
) -> Result<HirFunction, Diagnostic> {
    let mut ctx = LowerCtx::new(fn_returns, classes, enums, tables);
    let mut params = Vec::with_capacity(f.params.len());
    for p in &f.params {
        let ty = ctx.normalize(&p.ty);
        let name = ctx.bind(p.name.clone(), ty.clone());
        params.push(HirParam {
            name,
            ty: ty.into(),
            by_reference: !matches!(p.mode, crate::parser::ast::ParamMode::Value),
            span: p.span,
        });
    }
    let return_type = ctx.normalize(&f.return_type);
    let body = lower_block(&f.body, &mut ctx)?;
    Ok(HirFunction {
        name: f.name.clone().into(),
        is_async: f.is_async,
        params,
        return_type: return_type.into(),
        body,
        span: f.span,
    })
}

/// Lower a class method. An instance method's receiver is bound as `self` (typed
/// as the class) so `self.field` / `self.method()` in the body resolve against
/// the class registry; static methods have no receiver.
fn lower_method(
    m: &MethodDecl,
    class_name: &str,
    fn_returns: &HashMap<String, Type>,
    classes: &Classes,
    enums: &Enums,
    tables: &CheckerTables,
) -> Result<HirFunction, Diagnostic> {
    let mut ctx = LowerCtx::new(fn_returns, classes, enums, tables);
    ctx.current_class = Some(class_name.to_owned());
    let mut params = Vec::with_capacity(m.params.len() + 1);
    // Explicit and implicit `self` spellings normalize to the same receiver.
    if !m.is_static {
        let self_ty = ctx.normalize(&Type::Named(class_name.to_string()));
        ctx.bind("self".to_string(), self_ty.clone());
        params.push(HirParam {
            name: "self".to_string(),
            ty: self_ty.into(),
            by_reference: false,
            span: m.span,
        });
    }
    for p in &m.params {
        let ty = ctx.normalize(&p.ty);
        let name = ctx.bind(p.name.clone(), ty.clone());
        params.push(HirParam {
            name,
            ty: ty.into(),
            by_reference: !matches!(p.mode, crate::parser::ast::ParamMode::Value),
            span: p.span,
        });
    }
    let return_type = ctx.normalize(&m.return_type);
    let body = lower_block(&m.body, &mut ctx)?;
    Ok(HirFunction {
        name: m.name.clone().into(),
        is_async: m.is_async,
        params,
        return_type: return_type.into(),
        body,
        span: m.span,
    })
}

/// Lower an `init` constructor as a method named `init` with `self` bound to
/// the class and a `void` return type.
fn lower_constructor(
    ctor: &crate::parser::ast::ConstructorDecl,
    class_name: &str,
    fn_returns: &HashMap<String, Type>,
    classes: &Classes,
    enums: &Enums,
    tables: &CheckerTables,
) -> Result<HirFunction, Diagnostic> {
    let mut ctx = LowerCtx::new(fn_returns, classes, enums, tables);
    ctx.current_class = Some(class_name.to_owned());
    let self_ty = ctx.normalize(&Type::Named(class_name.to_string()));
    ctx.bind("self".to_string(), self_ty.clone());
    let mut params = Vec::with_capacity(ctor.params.len() + 1);
    params.push(HirParam {
        name: "self".to_string(),
        ty: self_ty.into(),
        by_reference: false,
        span: ctor.span,
    });
    for p in &ctor.params {
        let ty = ctx.normalize(&p.ty);
        let name = ctx.bind(p.name.clone(), ty.clone());
        params.push(HirParam {
            name,
            ty: ty.into(),
            by_reference: !matches!(p.mode, crate::parser::ast::ParamMode::Value),
            span: p.span,
        });
    }
    let body = lower_block(&ctor.body, &mut ctx)?;
    Ok(HirFunction {
        name: "init".into(),
        is_async: false,
        params,
        return_type: Type::Void,
        body,
        span: ctor.span,
    })
}

/// One name in scope: the type it was bound with plus the name it carries in
/// the HIR.
#[derive(Clone)]
struct Binding {
    ty: Type,
    /// The HIR name. This is the source name for the first binding of that
    /// name in a function, and `name$n` for every later one
    /// (willow-0g8j.2.10). LIR has a single flat namespace per function, so
    /// two source bindings that shadow each other — sibling `for x` loops, a
    /// `let` over a parameter, a `match` arm binding over an outer `let` —
    /// must not arrive there under one name.
    hir_name: String,
}

/// Lowering scope: variables (innermost-last) plus the free-function
/// return types used to type `Call` expressions.
struct LowerCtx<'a> {
    scopes: Vec<HashMap<String, Binding>>,
    /// Scope depth immediately outside the current function/lambda body.
    /// Used to distinguish a body-wide callable shadow from a nested one.
    namespace_scope_base: usize,
    /// How many bindings of each source name this function has lowered so far.
    /// Not popped with a scope: the suffix has to be unique across the whole
    /// function, not only across the scopes currently open.
    binds_seen: HashMap<String, usize>,
    next_defer_id: u32,
    current_class: Option<String>,
    fn_returns: &'a HashMap<String, Type>,
    classes: &'a Classes,
    enums: &'a Enums,
    tables: &'a CheckerTables<'a>,
}

impl<'a> LowerCtx<'a> {
    fn new(
        fn_returns: &'a HashMap<String, Type>,
        classes: &'a Classes,
        enums: &'a Enums,
        tables: &'a CheckerTables<'a>,
    ) -> Self {
        Self {
            scopes: vec![HashMap::new()],
            namespace_scope_base: 1,
            binds_seen: HashMap::new(),
            next_defer_id: 0,
            current_class: None,
            fn_returns,
            classes,
            enums,
            tables,
        }
    }

    /// The checker's type for a written annotation (willow-0g8j.3).
    fn normalize(&self, ty: &Type) -> Type {
        self.tables.normalize(ty)
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    /// Bind `name` in the innermost scope and return the name the HIR uses for
    /// it. Callers MUST build their HIR node from the returned name — it is the
    /// source name only when nothing else in this function has claimed it.
    fn bind(&mut self, name: String, ty: Type) -> String {
        let seen = self.binds_seen.entry(name.clone()).or_insert(0);
        *seen += 1;
        // A nested local function value and a free function also share the
        // walker's flat lookup namespace. Rename the nested local even on its
        // first binding so leaving that lexical scope exposes the free function
        // again. A function-body binding deliberately keeps the source name:
        // it shadows the free function for the rest of that body.
        let nested_callable_shadow = self.scopes.len() > self.namespace_scope_base + 1
            && self.fn_returns.contains_key(&name);
        let hir_name = if *seen == 1 && !nested_callable_shadow {
            name.clone()
        } else {
            // `$` cannot appear in a source identifier, so a renamed binding
            // can never collide with one the program wrote.
            format!("{name}${seen}")
        };
        self.scopes.last_mut().expect("at least one scope").insert(
            name,
            Binding {
                ty,
                hir_name: hir_name.clone(),
            },
        );
        hir_name
    }

    fn lookup(&self, name: &str) -> Option<&Binding> {
        self.scopes.iter().rev().find_map(|s| s.get(name))
    }

    /// The HIR name of a local binding, or `name` itself when nothing in scope
    /// carries it — a free function, an enum variant, anything lowering
    /// resolves by its source spelling.
    fn hir_name(&self, name: &str) -> String {
        self.lookup(name)
            .map(|b| b.hir_name.clone())
            .unwrap_or_else(|| name.to_string())
    }
}

/// Re-type an array literal (and any array literal nested directly inside it)
/// to the element type its annotated slot declares.
///
/// The type checker already checked every element against that type — see
/// [`TypeChecker::check_array_literal_expecting`] — and made the literal's own
/// type the annotation. Lowering infers it from element 0 instead, which is the
/// same answer whenever the elements agree and a WRONG one when the annotation
/// widened them (a base class, an interface). Only the recorded type moves; the
/// elements are untouched, so nothing about what is stored changes.
///
/// [`TypeChecker::check_array_literal_expecting`]: crate::semantic::type_checker::TypeChecker
fn retype_array_literal(value: &mut HirExpr, target: &Type) {
    let mut pending = vec![(value, target)];
    while let Some((value, target)) = pending.pop() {
        let Type::Array(elem) = target else {
            continue;
        };
        let HirExprKind::Array { elements } = &mut value.kind else {
            continue;
        };
        value.ty = target.into();
        pending.extend(elements.iter_mut().map(|element| (element, &**elem)));
    }
}

#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_block(block: &Block, ctx: &mut LowerCtx) -> Result<Vec<HirStmt>, Diagnostic> {
    ctx.push_scope();
    let mut out = Vec::with_capacity(block.stmts.len());
    for stmt in &block.stmts {
        out.push(lower_stmt(stmt, ctx)?);
    }
    ctx.pop_scope();
    Ok(out)
}

#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_stmt(stmt: &Stmt, ctx: &mut LowerCtx) -> Result<HirStmt, Diagnostic> {
    match stmt {
        Stmt::Let(l) => {
            let mut value = lower_expr(&l.init, ctx)?;
            // A `let x: T = ..` annotation pins the binding type; otherwise the
            // type flows from the value expression.
            let binding_ty =
                l.ty.as_ref()
                    .map(|ty| ctx.normalize(ty))
                    .unwrap_or_else(|| (value.ty.clone()).to_source());
            // An annotated array literal takes the ANNOTATION's element type,
            // exactly as `check_array_literal_expecting` typed it. Without this
            // the literal keeps the type of its first element, so
            // `let shapes: Array<Shape> = [new Square(..), new Box2(..)]`
            // reaches HIR as an `Array<Square>` holding a `Box2` — a type the
            // checker never accepted and no consumer can reason from.
            retype_array_literal(&mut value, &binding_ty);
            let name = ctx.bind(l.name.clone(), binding_ty.clone());
            Ok(HirStmt::Let {
                name,
                mutable: l.mutable,
                ty: binding_ty.into(),
                value,
                span: l.span,
            })
        }
        Stmt::Assign(a) => {
            let value = lower_expr(&a.value, ctx)?;
            Ok(HirStmt::Assign {
                name: ctx.hir_name(&a.name),
                value,
                span: a.span,
            })
        }
        Stmt::If(i) => {
            let cond = lower_expr(&i.cond, ctx)?;
            let then_branch = lower_block(&i.then_block, ctx)?;
            let else_branch = match &i.else_block {
                Some(b) => Some(lower_block(b, ctx)?),
                None => None,
            };
            Ok(HirStmt::If {
                cond,
                then_branch,
                else_branch,
                span: i.span,
            })
        }
        Stmt::Break(span) => Ok(HirStmt::Break { span: *span }),
        Stmt::Continue(span) => Ok(HirStmt::Continue { span: *span }),
        // Every source form of `defer` has a HIR shape (willow-0g8j.2.3). The
        // body is lowered where it was written, so a deferred `match
        // recover()` sees the same bindings the registering scope did; a block
        // body gets its own scope, exactly as `lower_block` gives any block.
        Stmt::Defer(d) => {
            let id = HirDeferId(ctx.next_defer_id);
            ctx.next_defer_id = ctx
                .next_defer_id
                .checked_add(1)
                .expect("too many defer sites");
            let body = match &d.body {
                DeferBody::Expr(expr) => HirDeferBody::Expr(lower_expr(expr, ctx)?),
                DeferBody::Block(block) => HirDeferBody::Block(lower_block(block, ctx)?),
            };
            Ok(HirStmt::Defer {
                id,
                body,
                span: d.span,
            })
        }
        // HIR needs explicit acquire/cleanup edges for a critical section, which
        // arrive with the backend lowering (willow-38w.1.3). Until then the
        // statement has no HIR shape at all — the type checker rejects it with
        // E2502, so this is unreachable in practice and only keeps the lowering
        // gap explicit.
        Stmt::Lock(l) => {
            let target = lower_expr(&l.target, ctx)?;
            let binding_ty = match &target.ty {
                Type::Generic(_, args) if args.len() == 1 => args[0].clone(),
                _ => return Err(unsupported(l.span, "lock target type")),
            };
            ctx.push_scope();
            let binding = ctx.bind(l.binding.clone(), (binding_ty).to_source());
            let mut body = Vec::with_capacity(l.body.stmts.len());
            for stmt in &l.body.stmts {
                body.push(lower_stmt(stmt, ctx)?);
            }
            ctx.pop_scope();
            Ok(HirStmt::Lock {
                mode: l.mode,
                target,
                binding,
                mutable: l.mutable,
                body,
                span: l.span,
            })
        }
        Stmt::While(w) => {
            let cond = lower_expr(&w.cond, ctx)?;
            let body = lower_block(&w.body, ctx)?;
            Ok(HirStmt::While {
                cond,
                body,
                span: w.span,
            })
        }
        Stmt::Return(r) => {
            let value = match &r.value {
                Some(e) => Some(lower_expr(e, ctx)?),
                None => None,
            };
            Ok(HirStmt::Return {
                value,
                span: r.span,
            })
        }
        Stmt::Expr(e) => Ok(HirStmt::Expr(lower_expr(&e.expr, ctx)?)),
        Stmt::FieldAssign(s) => {
            let object = lower_expr(&s.object, ctx)?;
            let value = lower_expr(&s.value, ctx)?;
            Ok(HirStmt::FieldAssign {
                object,
                field: s.field.clone(),
                value,
                span: s.span,
            })
        }
        Stmt::SuperInit(s) => {
            let args = lower_value_args(&s.args, ctx)?;
            Ok(HirStmt::SuperInit { args, span: s.span })
        }
        Stmt::StaticFieldAssign(s) => {
            let value = lower_expr(&s.value, ctx)?;
            let source = if s.class == "Self" {
                ctx.current_class.as_deref().unwrap_or(&s.class)
            } else {
                &s.class
            };
            let class = match ctx.normalize(&Type::Named(source.to_owned())) {
                Type::Named(ref name) => name.clone(),
                _ => source.to_owned(),
            };
            Ok(HirStmt::StaticFieldAssign {
                class: class.into(),
                field: s.field.clone(),
                value,
                span: s.span,
            })
        }
        Stmt::IndexAssign(s) => {
            let array = lower_expr(&s.array, ctx)?;
            let index = lower_expr(&s.index, ctx)?;
            let value = lower_expr(&s.value, ctx)?;
            Ok(HirStmt::IndexAssign {
                array,
                index,
                value,
                span: s.span,
            })
        }
        Stmt::For(s) => {
            let iterable = lower_expr(&s.iterable, ctx)?;
            let element_ty = match &iterable.ty {
                Type::Array(inner) => (**inner).clone(),
                // An i64 range yields i64 elements.
                Type::Generic(name, args)
                    if name.name() == "Range" && args.first() == Some(&Type::I64) =>
                {
                    Type::I64
                }
                _ => {
                    return Err(unsupported(
                        s.span,
                        "for over a non-array, non-range iterable",
                    ));
                }
            };
            ctx.push_scope();
            let name = ctx.bind(s.name.clone(), (element_ty).to_source());
            let body = lower_block(&s.body, ctx)?;
            ctx.pop_scope();
            Ok(HirStmt::For {
                name,
                iterable,
                body,
                span: s.span,
            })
        }
    }
}

#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_expr(expr: &Expr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    let mut lowered = lower_expr_inner(expr, ctx)?;
    lowered.ty = ctx.normalize(&lowered.ty.to_source()).into();
    match &mut lowered.kind {
        HirExprKind::StaticCall { class, .. } | HirExprKind::StaticField { class, .. } => {
            let source = if *class == crate::semantic::ids::TypeId::local("Self") {
                ctx.current_class
                    .as_deref()
                    .map(str::to_owned)
                    .unwrap_or_else(|| class.to_string())
            } else {
                class.to_string()
            };
            if let Type::Named(name) = &ctx.normalize(&Type::Named(source)) {
                *class = name.as_str().into();
            }
        }
        _ => {}
    }
    // Construction records the identity being allocated, not the source's
    // import alias. Keep it identical to the canonical result type.
    if let crate::semantic::ids::SemanticType::Named(identity) = &lowered.ty {
        match &mut lowered.kind {
            HirExprKind::New { class, .. } | HirExprKind::ObjectLiteral { class, .. } => {
                *class = *identity
            }
            _ => {}
        }
    }
    Ok(lowered)
}

#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_expr_inner(expr: &Expr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    match expr {
        Expr::Integer(n, span, _) => Ok(lit(HirExprKind::Int(*n), Type::I64, *span)),
        Expr::Float(f, span, _) => Ok(lit(HirExprKind::Float(*f), Type::F64, *span)),
        Expr::Bool(b, span, _) => Ok(lit(HirExprKind::Bool(*b), Type::Bool, *span)),
        Expr::String(s, span, _) => Ok(lit(HirExprKind::Str(s.clone()), Type::String, *span)),
        Expr::Var(name, span, _) => lower_var_expr(expr, name, *span, ctx),
        Expr::Binary(_) | Expr::Unary(_) => lower_operator_tree(expr, ctx),
        Expr::Call(c) => lower_call_expr(c, ctx),
        Expr::Print(inner, newline, span, _) => lower_print_expr(inner, *newline, *span, ctx),
        Expr::ArrayLiteral(elements, span, _) => {
            lower_array_literal_expr(expr, elements, *span, ctx)
        }
        Expr::Index(array, index, span, _) => lower_index_expr(array, index, *span, ctx),
        Expr::Ternary(t) => lower_ternary_expr(t, ctx),
        Expr::New(n) => lower_new_expr(n, ctx),
        Expr::FieldAccess(object, field, span, _) => {
            lower_field_access_expr(expr, object, field, *span, ctx)
        }
        Expr::MethodCall(m) => lower_method_call_expr(m, ctx),
        Expr::ObjectLiteral(o) => lower_object_literal_expr(o, ctx),
        Expr::StaticField(s) => lower_static_field_expr(s, ctx),
        Expr::StaticCall(s) => lower_static_call_expr(s, ctx),
        Expr::Range(r) => lower_range_expr(r, ctx),
        Expr::Select(select) => lower_select_expr(select, ctx),
        Expr::Await(a) => lower_await_expr(a, ctx),
        Expr::TryPropagate(inner, span, _) => lower_try_propagate_expr(inner, *span, ctx),
        Expr::Lambda(l) => lower_lambda_expr(l, ctx),
        Expr::Match(m) => lower_match(m, ctx),
    }
}

/// Each non-trivial arm of [`lower_expr_inner`] lives in its own function so a
/// nested expression costs one SMALL frame per level instead of one frame
/// holding every arm's locals at once: a debug build gives the match's frame
/// room for all of them, which cost ~63KB per level and overflowed a 2MiB
/// thread at around 30 levels of nesting (willow-t0uy.1). `inline(never)` keeps
/// that true in release builds, where the arms would otherwise be folded back
/// into one frame.
#[inline(never)]
fn lower_var_expr(
    expr: &Expr,
    name: &str,
    span: Span,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    if let Some(binding) = ctx.lookup(name) {
        return Ok(HirExpr {
            kind: HirExprKind::Var(binding.hir_name.to_string()),
            ty: binding.ty.clone().into(),
            span,
        });
    }
    // A bare fieldless unqualified variant (`None`, `Halt`) parses as a
    // variable; the checker resolves it against the expected enum and
    // records both the enum and the expression type.
    if let Some(enum_name) = ctx.tables.enum_variant_resolution(&expr.id()) {
        let ty = ctx
            .tables
            .expr_type(&expr.id())
            .unwrap_or_else(|| Type::Named(enum_name.to_string()));
        return Ok(HirExpr {
            kind: HirExprKind::StaticField {
                class: enum_name.to_string().into(),
                field: name.to_string(),
            },
            ty: ty.into(),
            span,
        });
    }
    // A named top-level function used as a value (`apply(10, double)`,
    // willow-0g8j.2.2). The checker types the bare identifier as the
    // function's `fn(...) -> ...`; taking that from the checker rather
    // than rebuilding it keeps the parameter modes and the return type
    // exactly what the call site was checked against.
    if ctx.fn_returns.contains_key(name)
        && let Some(ty @ Type::Fn(..)) = ctx.tables.expr_type(&expr.id())
    {
        return Ok(HirExpr {
            kind: HirExprKind::FnRef(name.into()),
            ty: ty.into(),
            span,
        });
    }
    Err(internal(
        span,
        format!("unbound variable `{name}` reached HIR lowering"),
    ))
}

/// Operator chains have no lexical boundaries. Keep their construction frames
/// on the heap, delegating other syntax to its context-sensitive lowering.
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_operator_tree(expr: &Expr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    enum Step<'a> {
        Enter(&'a Expr),
        Binary(&'a BinaryExpr),
        Unary(&'a UnaryExpr),
    }
    let mut work = vec![Step::Enter(expr)];
    let mut values: Vec<HirExpr> = Vec::new();
    while let Some(step) = work.pop() {
        let mut value = match step {
            Step::Enter(Expr::Binary(binary)) => {
                work.push(Step::Binary(binary));
                work.push(Step::Enter(&binary.rhs));
                work.push(Step::Enter(&binary.lhs));
                continue;
            }
            Step::Enter(Expr::Unary(unary)) => {
                work.push(Step::Unary(unary));
                work.push(Step::Enter(&unary.expr));
                continue;
            }
            Step::Enter(other) => match lower_expr(other, ctx) {
                Ok(value) => {
                    values.push(value);
                    continue;
                }
                Err(error) => {
                    // HIR ownership drains partial results iteratively.
                    values.clear();
                    return Err(error);
                }
            },
            Step::Binary(binary) => {
                let rhs = values.pop().expect("lowered right operand");
                let lhs = values.pop().expect("lowered left operand");
                let ty = binary_result_type(&binary.op, &lhs.ty.to_source());
                HirExpr {
                    kind: HirExprKind::Binary {
                        op: binary.op.clone(),
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                    },
                    ty: ty.into(),
                    span: binary.span,
                }
            }
            Step::Unary(unary) => {
                let operand = values.pop().expect("lowered unary operand");
                let ty = match unary.op {
                    UnaryOp::Neg => operand.ty.clone(),
                    UnaryOp::Not => Type::Bool,
                };
                HirExpr {
                    kind: HirExprKind::Unary {
                        op: unary.op.clone(),
                        operand: Box::new(operand),
                    },
                    ty,
                    span: unary.span,
                }
            }
        };
        // The outer lower_expr normalizes the root once, just as before.
        if !work.is_empty() {
            value.ty = ctx.normalize(&value.ty.to_source()).into();
        }
        values.push(value);
    }
    Ok(values.pop().expect("lowered operator root"))
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_call_expr(c: &CallExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    let args = lower_value_args(&c.args, ctx)?;
    // An unqualified enum-variant construction (`Ok(42)` in an
    // expected-enum position) parses as a call; the checker records
    // which enum it resolved to (willow-60o.1).
    if let Some(enum_name) = ctx.tables.enum_variant_resolution(&c.id) {
        // Prefer the checker's recorded type (it carries generic type
        // arguments, e.g. `Result<i64, String>` for `Ok(42)`).
        let ty = match ctx.tables.expr_type(&c.id) {
            Some(ty) => ty,
            None => enum_variant_construction_type(ctx.enums, enum_name, &args, c.span)?,
        };
        return Ok(HirExpr {
            kind: HirExprKind::StaticCall {
                class: enum_name.clone().into(),
                method: c.callee.clone(),
                args,
            },
            ty: ty.into(),
            span: c.span,
        });
    }
    // A local fn-typed variable shadows a free function; its call is an
    // indirect call typed by the variable's `fn(..) -> R`, and it is
    // named by the binding's HIR name (willow-0g8j.2.10).
    let indirect = ctx.lookup(&c.callee).and_then(|b| match &b.ty {
        Type::Fn(_, ret) => Some((b.hir_name.clone(), (**ret).clone())),
        _ => None,
    });
    let callee = match &indirect {
        Some((name, _)) => name.clone(),
        None => c.callee.clone(),
    };
    let ty = indirect
        .map(|(_, ret)| ret)
        .or_else(|| ctx.fn_returns.get(&c.callee).cloned())
        // A function bound by an ITEM import (`import calc::add;`) is a
        // plain unqualified call in this file, but it is not one of the
        // program's own items, so `fn_returns` has never heard of it
        // (willow-28h8). The checker did resolve the import and typed
        // the call, so its type is what makes the call lowerable at
        // all. The back end binds this same local name to the mangled
        // symbol of the module THIS unit imported, rebinding it before
        // each unit's bodies, so the call the type came from is the
        // call that gets emitted (willow-28h8).
        .or_else(|| ctx.tables.expr_type(&c.id))
        .ok_or_else(|| {
            internal(
                c.span,
                format!(
                    "call to unknown function `{}` reached HIR lowering",
                    c.callee
                ),
            )
        })?;
    Ok(HirExpr {
        kind: HirExprKind::Call {
            callee: callee.into(),
            args,
        },
        ty: ty.into(),
        span: c.span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_print_expr(
    inner: &Expr,
    newline: bool,
    span: Span,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let value = lower_expr(inner, ctx)?;
    Ok(HirExpr {
        kind: HirExprKind::Print {
            value: Box::new(value),
            newline,
        },
        ty: Type::Void,
        span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_array_literal_expr(
    expr: &Expr,
    elements: &[Expr],
    span: Span,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let mut lowered = Vec::with_capacity(elements.len());
    for element in elements {
        lowered.push(lower_expr(element, ctx)?);
    }
    // A non-empty literal is typed by its first element, exactly as
    // before; `retype_array_literal` then widens it to an annotated
    // slot's element type where there is one.
    //
    // An empty literal has no element to infer from, so its type comes
    // from the checker, which typed it against the slot it was written
    // into (willow-0g8j.2.10). A literal the checker recorded nothing
    // for — or recorded a non-array for — leaves the element type
    // genuinely unknown, and the function falls back rather than the
    // lowering picking one.
    let ty = match lowered.first() {
        Some(first) => Type::Array(Box::new(first.ty.clone())),
        None => match ctx.tables.expr_type(&expr.id()) {
            Some(recorded @ Type::Array(_)) => recorded,
            _ => {
                return Err(unsupported(
                    span,
                    "empty array literal with no recorded element type",
                ));
            }
        }
        .into(),
    };
    Ok(HirExpr {
        kind: HirExprKind::Array { elements: lowered },
        ty,
        span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_index_expr(
    array: &Expr,
    index: &Expr,
    span: Span,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let array = lower_expr(array, ctx)?;
    let index = lower_expr(index, ctx)?;
    // A `FrozenArray<T>` is the same runtime handle as the `Array<T>` it
    // was frozen from, and `arr[i]` reads it the same way, so both lower
    // to one `Index` node (willow-0g8j.7). `Range<i64>` also spells a
    // read this way but is not a handle at all, so it stays unsupported.
    let element = match &array.ty {
        Type::Array(element) => (**element).clone(),
        ty => match builtin_types::unary_arg(ty, B::FrozenArray) {
            Some(element) => element.clone(),
            None => return Err(unsupported(span, "index of a non-array value")),
        },
    };
    let ty = element;
    Ok(HirExpr {
        kind: HirExprKind::Index {
            array: Box::new(array),
            index: Box::new(index),
        },
        ty,
        span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_ternary_expr(t: &TernaryExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    let condition = lower_expr(&t.condition, ctx)?;
    let then_expr = lower_expr(&t.then_expr, ctx)?;
    let else_expr = lower_expr(&t.else_expr, ctx)?;
    // Both arms share a type (the checker enforces it); use the `then`
    // arm's resolved type as the ternary's type.
    let ty = then_expr.ty.clone();
    Ok(HirExpr {
        kind: HirExprKind::Ternary {
            condition: Box::new(condition),
            then_expr: Box::new(then_expr),
            else_expr: Box::new(else_expr),
        },
        ty,
        span: t.span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_new_expr(n: &NewExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    let args = lower_value_args(&n.args, ctx)?;
    Ok(HirExpr {
        kind: HirExprKind::New {
            class: n.class_name.clone().into(),
            args,
        },
        ty: Type::Named(n.class_name.clone().into()),
        span: n.span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_field_access_expr(
    expr: &Expr,
    object: &Expr,
    field: &str,
    span: Span,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let object = lower_expr(object, ctx)?;
    let ty = {
        match class_name_of(&object.ty.to_source())
            .and_then(|class| ctx.classes.field_type(class, field))
        {
            Some(ty) => ty,
            None => ctx
                .tables
                .expr_type(&expr.id())
                .ok_or_else(|| unsupported(span, "field not found on receiver"))?,
        }
    };
    Ok(HirExpr {
        kind: HirExprKind::FieldAccess {
            object: Box::new(object),
            field: field.to_string(),
        },
        ty: ty.into(),
        span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_method_call_expr(m: &MethodCallExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    let object = lower_expr(&m.object, ctx)?;
    let ty = if let Some(ty) = builtin_method_type(&object.ty.to_source(), &m.method) {
        ty
    } else if let Some(ty) = class_name_of(&object.ty.to_source())
        .and_then(|class| ctx.classes.method_type(class, &m.method))
    {
        ty
    } else {
        // Checker authority: interface methods, generic receivers,
        // Option/Result methods, and anything else it typed.
        ctx.tables
            .expr_type(&m.id)
            .ok_or_else(|| unsupported(m.span, "method not found on receiver"))?
    };
    let args = lower_value_args(&m.args, ctx)?;
    Ok(HirExpr {
        kind: HirExprKind::MethodCall {
            object: Box::new(object),
            method: m.method.clone(),
            args,
        },
        ty: ty.into(),
        span: m.span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_object_literal_expr(
    o: &ObjectLiteralExpr,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let mut fields = Vec::with_capacity(o.fields.len());
    for f in &o.fields {
        fields.push((f.name.clone(), lower_expr(&f.value, ctx)?));
    }
    Ok(HirExpr {
        kind: HirExprKind::ObjectLiteral {
            class: o.class.clone().into(),
            fields,
        },
        ty: Type::Named(o.class.clone().into()),
        span: o.span,
    })
}

#[inline(never)]
fn lower_static_field_expr(s: &StaticFieldExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    // `Enum::Variant` (fieldless) parses like a static property read.
    let class = if s.class == "Self" {
        ctx.current_class.as_deref().unwrap_or(&s.class)
    } else {
        &s.class
    };
    let variant_ty = enum_variant_value_type(ctx.enums, class, &s.field, true);
    let ty = variant_ty
        .or_else(|| ctx.classes.static_field_type(class, &s.field))
        // Checker authority, exactly as the static-CALL arm below uses
        // it: `Self::prop` inside a class body names a class the
        // registry has no entry for, and the checker has already
        // resolved and typed the read (willow-0g8j.13).
        .or_else(|| ctx.tables.expr_type(&s.id))
        .ok_or_else(|| unsupported(s.span, "static property not found"))?;
    Ok(HirExpr {
        kind: HirExprKind::StaticField {
            class: s.class.clone().into(),
            field: s.field.clone(),
        },
        ty: ty.into(),
        span: s.span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_static_call_expr(s: &StaticCallExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    // `Enum::Variant(args)` construction parses like a static call.
    let class = if s.class == "Self" {
        ctx.current_class.as_deref().unwrap_or(&s.class)
    } else {
        &s.class
    };
    let variant_ty = enum_variant_value_type(ctx.enums, class, &s.method, false);
    let ty = variant_ty
        .or_else(|| ctx.classes.static_method_type(class, &s.method))
        // Checker authority: generic-enum construction, `Self::`,
        // module-qualified statics, constructors.
        .or_else(|| ctx.tables.expr_type(&s.id))
        .ok_or_else(|| {
            unsupported(
                s.span,
                "static method not found (and no checker-recorded type)",
            )
        })?;
    let args = lower_value_args(&s.args, ctx)?;
    Ok(HirExpr {
        kind: HirExprKind::StaticCall {
            class: ctx.tables.static_call_class(&s.id, &s.class).into(),
            method: s.method.clone(),
            args,
        },
        ty: ty.into(),
        span: s.span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_range_expr(r: &RangeExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    let start = lower_expr(&r.start, ctx)?;
    let end = lower_expr(&r.end, ctx)?;
    Ok(HirExpr {
        kind: HirExprKind::Range {
            start: Box::new(start),
            end: Box::new(end),
        },
        ty: Type::Generic("Range".to_string().into(), vec![Type::I64]),
        span: r.span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_select_expr(select: &SelectExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    let mut cases = Vec::with_capacity(select.cases.len());
    for case in &select.cases {
        let kind = match &case.kind {
            SelectCaseKind::Recv { binding, channel } => {
                let channel = lower_expr(channel, ctx)?;
                let binding_ty = builtin_types::unary_arg(&channel.ty, B::Channel)
                    .ok_or_else(|| unsupported(case.span, "select receive channel"))?
                    .clone();
                ctx.push_scope();
                let binding = ctx.bind(binding.clone(), (binding_ty).to_source());
                let mut body = Vec::with_capacity(case.body.stmts.len());
                for stmt in &case.body.stmts {
                    body.push(lower_stmt(stmt, ctx)?);
                }
                ctx.pop_scope();
                cases.push(HirSelectCase {
                    kind: HirSelectCaseKind::Recv { binding, channel },
                    body,
                    span: case.span,
                });
                continue;
            }
            SelectCaseKind::Send { channel, value } => HirSelectCaseKind::Send {
                channel: lower_expr(channel, ctx)?,
                value: lower_expr(value, ctx)?,
            },
            SelectCaseKind::Timeout { millis } => HirSelectCaseKind::Timeout {
                millis: lower_expr(millis, ctx)?,
            },
            SelectCaseKind::Join { binding, task } => {
                let task = lower_expr(task, ctx)?;
                // What awaiting the operand YIELDS: `T` for `Task<T>`,
                // but `Result<T, Cancelled>` for `TaskResult<T>`. Taken
                // from the type checker's own definition so the HIR
                // binding cannot disagree with what the checker typed
                // the case body against.
                let binding_ty = await_output_type(&task.ty).ok_or_else(|| {
                    internal(
                        task.span,
                        "select join operand has no checked await output type".to_string(),
                    )
                })?;
                ctx.push_scope();
                let binding = ctx.bind(binding.clone(), (binding_ty).to_source());
                let mut body = Vec::with_capacity(case.body.stmts.len());
                for stmt in &case.body.stmts {
                    body.push(lower_stmt(stmt, ctx)?);
                }
                ctx.pop_scope();
                cases.push(HirSelectCase {
                    kind: HirSelectCaseKind::Join { binding, task },
                    body,
                    span: case.span,
                });
                continue;
            }
            SelectCaseKind::Default => HirSelectCaseKind::Default,
        };
        let body = lower_block(&case.body, ctx)?;
        cases.push(HirSelectCase {
            kind,
            body,
            span: case.span,
        });
    }
    Ok(HirExpr {
        kind: HirExprKind::Select { cases },
        ty: Type::Void,
        span: select.span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_await_expr(a: &AwaitExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    let inner = lower_expr(&a.expr, ctx)?;
    let resolved = builtin_types::resolve(&inner.ty)
        .filter(|resolved| resolved.args.len() == 1)
        .ok_or_else(|| unsupported(a.span, "await of a non-Task/Future value"))?;
    let ty = match resolved.id {
        B::Task | B::Future | B::JoinHandle => resolved.args[0].clone(),
        // `await task.result()` yields `Result<T, Cancelled>`.
        B::TaskResult => {
            B::Result.apply(vec![resolved.args[0].clone(), B::Cancelled.apply(vec![])])
        }
        _ => return Err(unsupported(a.span, "await of a non-Task/Future value")),
    };
    Ok(HirExpr {
        kind: HirExprKind::Await {
            inner: Box::new(inner),
        },
        ty,
        span: a.span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_try_propagate_expr(
    inner: &Expr,
    span: Span,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    let inner = lower_expr(inner, ctx)?;
    let ty = builtin_types::resolve(&inner.ty)
        .filter(|resolved| matches!(resolved.id, B::Result | B::Option))
        .and_then(|resolved| resolved.args.first().cloned())
        .ok_or_else(|| unsupported(span, "`?` on a non-Result/Option value"))?;
    Ok(HirExpr {
        kind: HirExprKind::TryPropagate {
            inner: Box::new(inner),
        },
        ty,
        span,
    })
}

#[inline(never)]
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_lambda_expr(l: &LambdaExpr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    // The checker's inferred full `fn(...) -> ...` type fills in what
    // the AST cannot store: unannotated parameter types and, for
    // block-bodied lambdas, the inferred return type.
    let inferred = match ctx.tables.lambda_fn_type(&l.id) {
        Some(Type::Fn(params, ret) | Type::Closure(params, ret)) => {
            Some((params.clone(), (**ret).clone()))
        }
        _ => None,
    };
    let is_closure = matches!(ctx.tables.lambda_fn_type(&l.id), Some(Type::Closure(..)));
    // Resolve every capture in the ENCLOSING namespace first: after the
    // scope below, `n` means the lambda's own copy (willow-0g8j.2.12).
    let capture_sources: Vec<(String, String, Type)> = ctx
        .tables
        .lambda_captures(&l.id)
        .iter()
        .map(|c| {
            (
                c.name.clone(),
                ctx.hir_name(&c.name),
                ctx.tables.normalize(&c.ty),
            )
        })
        .collect();
    let mut params = Vec::with_capacity(l.params.len());
    let mut param_tys = Vec::with_capacity(l.params.len());
    ctx.push_scope();
    // A lambda body is lifted into a LirFunction of its own, so its
    // flat namespace is its own too and the shadow suffixes restart
    // here (willow-0g8j.2.10). Without this a lambda parameter could
    // arrive renamed after an unrelated binding of the same name in the
    // enclosing function — and the backend, which binds the lifted
    // parameters by their source names, would never bind it.
    let outer_binds = std::mem::take(&mut ctx.binds_seen);
    let outer_namespace_scope_base = ctx.namespace_scope_base;
    ctx.namespace_scope_base = ctx.scopes.len();
    // Bind the captures BEFORE anything else in the lambda's fresh
    // namespace. They are the first names it sees, so a body-local
    // rebinding of a captured name gets the shadow suffix and the two
    // stay distinct locals in the lifted function.
    let captures: Vec<HirCapture> = capture_sources
        .into_iter()
        .map(|(source_name, source, ty)| HirCapture {
            name: ctx.bind(source_name, ty.clone()),
            source,
            ty: ty.into(),
        })
        .collect();
    for (i, p) in l.params.iter().enumerate() {
        let inferred_param = inferred
            .as_ref()
            .and_then(|(params, _)| params.get(i).cloned());
        let Some(ty) = p.ty.as_ref().map(|ty| ctx.normalize(ty)).or(inferred_param) else {
            ctx.pop_scope();
            ctx.binds_seen = outer_binds;
            ctx.namespace_scope_base = outer_namespace_scope_base;
            return Err(unsupported(p.span, "unannotated lambda parameter"));
        };
        let name = ctx.bind(p.name.clone(), ty.clone());
        param_tys.push(ty.clone());
        params.push(HirParam {
            name,
            ty: ty.into(),
            by_reference: false,
            span: p.span,
        });
    }
    let inferred_ret = inferred.map(|(_, ret)| ret);
    let (body, ret) = match &l.body {
        crate::parser::ast::LambdaBody::Expr(e) => {
            let value = match lower_expr(e, ctx) {
                Ok(value) => value,
                Err(err) => {
                    ctx.pop_scope();
                    ctx.binds_seen = outer_binds;
                    ctx.namespace_scope_base = outer_namespace_scope_base;
                    return Err(err);
                }
            };
            let ret = l
                .return_type
                .as_ref()
                .map(|ty| ctx.normalize(ty))
                .or(inferred_ret)
                .unwrap_or_else(|| (value.ty.clone()).to_source());
            let span = value.span;
            // A `void` body has no value to return: `|s| println(s)` is
            // a statement, and returning its non-existent value would
            // build a `return` against a signature with no result slot
            // (willow-0g8j.2.2).
            let body = if matches!(ret, Type::Void) {
                vec![HirStmt::Expr(value), HirStmt::Return { value: None, span }]
            } else {
                vec![HirStmt::Return {
                    value: Some(value),
                    span,
                }]
            };
            (body, ret)
        }
        crate::parser::ast::LambdaBody::Block(block) => {
            let Some(ret) = l
                .return_type
                .as_ref()
                .map(|ty| ctx.normalize(ty))
                .or(inferred_ret)
            else {
                ctx.pop_scope();
                ctx.binds_seen = outer_binds;
                ctx.namespace_scope_base = outer_namespace_scope_base;
                return Err(unsupported(
                    l.span,
                    "block-bodied lambda without a return type annotation",
                ));
            };
            let block = lower_block(block, ctx);
            match block {
                Ok(block) => (block, ret),
                Err(err) => {
                    ctx.pop_scope();
                    ctx.binds_seen = outer_binds;
                    ctx.namespace_scope_base = outer_namespace_scope_base;
                    return Err(err);
                }
            }
        }
    };
    ctx.pop_scope();
    ctx.binds_seen = outer_binds;
    ctx.namespace_scope_base = outer_namespace_scope_base;
    // A capture-free lambda that a `closure(...)` slot expects is a
    // closure too: the slot decides the calling convention, so the
    // lifted body still takes the (empty) environment.
    let ty = if is_closure {
        Type::Closure(param_tys, Box::new(ret))
    } else {
        Type::Fn(param_tys, Box::new(ret))
    };
    Ok(HirExpr {
        kind: HirExprKind::Lambda {
            id: l.id,
            params,
            captures,
            body,
        },
        ty: ty.into(),
        span: l.span,
    })
}

/// The value type of a checker-resolved unqualified variant construction.
/// Non-generic enums type as `Named(enum)`; a generic enum's type arguments
/// are not recorded in the resolution table, so they stay unsupported here.
fn enum_variant_construction_type(
    enums: &Enums,
    enum_name: &str,
    _args: &[HirExpr],
    span: Span,
) -> Result<Type, Diagnostic> {
    match enums.map.get(enum_name) {
        Some(info) if info.type_params.is_empty() => Ok(Type::Named(enum_name.to_string())),
        Some(_) => Err(unsupported(
            span,
            "generic enum construction (type arguments not in the resolution table)",
        )),
        None => Err(unsupported(span, "construction of an unknown enum")),
    }
}

/// The value type of constructing `Enum::Variant`. Non-generic enums only —
/// a generic variant's type arguments need inference from the expected type,
/// which the structural lowering does not thread through yet.
fn enum_variant_value_type(
    enums: &Enums,
    enum_name: &str,
    variant: &str,
    fieldless_only: bool,
) -> Option<Type> {
    let info = enums.map.get(enum_name)?;
    let payload = info.variants.get(variant)?;
    if !info.type_params.is_empty() || (fieldless_only && !payload.is_empty()) {
        return None;
    }
    Some(Type::Named(enum_name.to_string()))
}

/// Lower a `match` expression. Pattern bindings are typed from the scrutinee's
/// enum (type parameters substituted from its type arguments); the match's type
/// is the first arm type that is not `Never` (`Void` for block-bodied arms).
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_match(
    m: &crate::parser::ast::MatchExpr,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    use crate::parser::ast::{MatchBody, Pattern};

    let scrutinee = lower_expr(&m.scrutinee, ctx)?;

    // The scrutinee's enum context: its EnumInfo plus the substitution from the
    // enum's type parameters to the scrutinee's type arguments.
    let enum_context: Option<(String, EnumInfo, HashMap<String, Type>)> =
        match &scrutinee.ty.to_source() {
            Type::Named(name) => ctx
                .enums
                .map
                .get(name)
                .map(|info| (name.clone(), info.clone(), HashMap::new())),
            Type::Generic(name, args) => ctx.enums.map.get(name).map(|info| {
                let subst = info
                    .type_params
                    .iter()
                    .cloned()
                    .zip(args.iter().cloned())
                    .collect();
                (name.clone(), info.clone(), subst)
            }),
            _ => None,
        };

    let mut arms = Vec::with_capacity(m.arms.len());
    for arm in &m.arms {
        ctx.push_scope();
        // The checker owns whether a bare spelling is a variant visible in
        // this unit. Structural inference is only for lowering without checker
        // tables; redoing it for checked input would turn catch-all bindings
        // into inaccessible imported variants.
        let infer_variant = ctx.tables.pattern_resolutions.is_none();
        let checked_pattern = ctx
            .tables
            .pattern_resolutions
            .and_then(|patterns| patterns.get(&arm.pattern.id()))
            .unwrap_or(&arm.pattern);
        let pattern = match checked_pattern {
            Pattern::Wildcard(_, _) => HirPattern::Wildcard,
            Pattern::LiteralBool(b, _, _) => HirPattern::LiteralBool(*b),
            Pattern::LiteralInt(n, _, _) => HirPattern::LiteralInt(*n),
            Pattern::Binding { name, .. } => {
                if let Some((enum_name, info, _)) = &enum_context
                    && infer_variant
                    && info.variants.get(name).is_some_and(Vec::is_empty)
                {
                    HirPattern::EnumVariant {
                        enum_name: enum_name.clone().into(),
                        variant: name.clone(),
                    }
                } else {
                    HirPattern::Binding {
                        name: ctx.bind(name.clone(), (scrutinee.ty.clone()).to_source()),
                        ty: scrutinee.ty.clone(),
                    }
                }
            }
            Pattern::EnumVariant { variant, .. } => {
                let Some((enum_name, _, _)) = &enum_context else {
                    ctx.pop_scope();
                    return Err(unsupported(
                        arm.span,
                        "enum pattern on a non-enum scrutinee",
                    ));
                };
                HirPattern::EnumVariant {
                    enum_name: enum_name.clone().into(),
                    variant: variant.clone(),
                }
            }
            Pattern::EnumVariantTuple {
                variant, bindings, ..
            } => bind_variant_tuple(&enum_context, variant, bindings, arm.span, ctx)?,
            Pattern::ClassDowncast {
                class_name,
                binding,
                ..
            } => {
                // Unqualified variant like `Some(x)` if it names a variant of the
                // scrutinee's enum; otherwise a real interface downcast.
                if infer_variant
                    && enum_context
                        .as_ref()
                        .is_some_and(|(_, info, _)| info.variants.contains_key(class_name))
                {
                    bind_variant_tuple(
                        &enum_context,
                        class_name,
                        std::slice::from_ref(binding),
                        arm.span,
                        ctx,
                    )?
                } else {
                    let binding_ty = Type::Named(class_name.clone());
                    let binding = if binding == "_" {
                        binding.clone()
                    } else {
                        ctx.bind(binding.clone(), binding_ty.clone())
                    };
                    HirPattern::ClassDowncast {
                        class_name: class_name.clone().into(),
                        binding,
                        binding_ty: binding_ty.into(),
                    }
                }
            }
        };

        let (body, arm_ty) = match &arm.body {
            MatchBody::Expr(e) => {
                let value = lower_expr(e, ctx)?;
                let ty = value.ty.clone();
                (vec![HirStmt::Expr(value)], ty)
            }
            MatchBody::Block(block) => {
                let ty = if crate::semantic::type_checker::analysis::block_always_returns(block) {
                    Type::Never
                } else {
                    Type::Void
                };
                (lower_block(block, ctx)?, ty)
            }
        };
        ctx.pop_scope();
        arms.push(HirMatchArm {
            pattern,
            body,
            ty: arm_ty,
            span: arm.span,
        });
    }

    // The arms share a type (the checker enforces it); `Never` arms (panic)
    // coerce to the others.
    let ty = arms
        .iter()
        .map(|a| &a.ty)
        .find(|t| **t != Type::Never)
        .cloned()
        .unwrap_or(Type::Never);
    Ok(HirExpr {
        kind: HirExprKind::Match {
            scrutinee: Box::new(scrutinee),
            arms,
        },
        ty,
        span: m.span,
    })
}

/// Build an `EnumVariantTuple` pattern, binding each payload name to the
/// variant's (substituted) payload type. `_` bindings match without binding.
fn bind_variant_tuple(
    enum_context: &Option<(String, EnumInfo, HashMap<String, Type>)>,
    variant: &str,
    bindings: &[String],
    span: Span,
    ctx: &mut LowerCtx,
) -> Result<HirPattern, Diagnostic> {
    let Some((enum_name, info, subst)) = enum_context else {
        return Err(unsupported(span, "enum pattern on a non-enum scrutinee"));
    };
    let Some(payload) = info.variants.get(variant) else {
        return Err(unsupported(span, "unknown enum variant in pattern"));
    };
    if payload.len() != bindings.len() {
        return Err(unsupported(span, "enum pattern arity mismatch"));
    }
    let mut typed = Vec::with_capacity(bindings.len());
    for (name, payload_ty) in bindings.iter().zip(payload) {
        let ty = subst_type(payload_ty, subst);
        let name = if name == "_" {
            name.clone()
        } else {
            ctx.bind(name.clone(), ty.clone())
        };
        typed.push((name, ty));
    }
    Ok(HirPattern::EnumVariantTuple {
        enum_name: enum_name.clone().into(),
        variant: variant.to_string(),
        bindings: typed
            .into_iter()
            .map(|(name, ty)| (name, ty.into()))
            .collect(),
    })
}

/// Lower call/constructor arguments. A reference argument retains its place as
/// a marker expression so eligibility can verify it against the callee mode and
/// codegen can pass its address rather than its value (willow-0g8j.2.7).
#[willow_continuations::function(
    lower_array_literal_expr,
    lower_await_expr,
    lower_block,
    lower_call_expr,
    lower_expr,
    lower_expr_inner,
    lower_field_access_expr,
    lower_index_expr,
    lower_lambda_expr,
    lower_match,
    lower_method_call_expr,
    lower_new_expr,
    lower_object_literal_expr,
    lower_operator_tree,
    lower_print_expr,
    lower_range_expr,
    lower_select_expr,
    lower_static_call_expr,
    lower_stmt,
    lower_ternary_expr,
    lower_try_propagate_expr,
    lower_value_args
)]
fn lower_value_args(args: &[CallArg], ctx: &mut LowerCtx) -> Result<Vec<HirExpr>, Diagnostic> {
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        let place = lower_expr(&arg.expr, ctx)?;
        match &arg.mode {
            CallArgMode::Value => out.push(place),
            CallArgMode::Reference { .. } => out.push(HirExpr {
                ty: place.ty.clone(),
                span: arg.span,
                kind: HirExprKind::ReferenceArg {
                    place: Box::new(place),
                },
            }),
        }
    }
    Ok(out)
}

/// The class name a value's type names, if it is a (possibly generic) class
/// type — the receiver position for field access and method calls.
fn class_name_of(ty: &Type) -> Option<&str> {
    match ty {
        Type::Named(name) => Some(name),
        Type::Generic(name, _) => Some(name),
        _ => None,
    }
}

/// Return types of the compiler-builtin collection/concurrency methods, keyed
/// by the receiver type (mirrors the checker's method tables).
fn builtin_method_type(receiver: &Type, method: &str) -> Option<Type> {
    match receiver {
        Type::Array(elem) => match method {
            "len" => Some(Type::I64),
            "toString" => Some(Type::String),
            "push" => Some(Type::Void),
            "pop" => Some((**elem).clone()),
            "freeze" => Some(B::FrozenArray.apply(vec![(**elem).clone()])),
            _ => None,
        },
        Type::Generic(_, args) => match (
            builtin_types::resolve(receiver)?.id,
            args.as_slice(),
            method,
        ) {
            (B::Map, [k, v], _) => match method {
                "insert" => Some(Type::Void),
                "toString" => Some(Type::String),
                "get" => Some(B::Option.apply(vec![v.clone()])),
                "contains" => Some(Type::Bool),
                "len" => Some(Type::I64),
                "freeze" => Some(B::FrozenMap.apply(vec![k.clone(), v.clone()])),
                _ => None,
            },
            (B::FrozenArray, [_], "len") | (B::FrozenMap, [_, _], "len") => Some(Type::I64),
            (B::FrozenMap, [_, v], "get") => Some(B::Option.apply(vec![v.clone()])),
            (B::FrozenMap, [_, _], "contains") => Some(Type::Bool),
            (B::Task | B::JoinHandle, [_], "cancel") => Some(Type::Void),
            // Cancellation-aware awaitable view of the same frame.
            (B::Task | B::JoinHandle, [t], "result") => Some(B::TaskResult.apply(vec![t.clone()])),
            (B::Task | B::JoinHandle, [_], "is_cancelled") => Some(Type::Bool),
            (B::BlockingCell, [t], "get") => Some(t.clone()),
            (B::BlockingCell, [_], "set") => Some(Type::Void),
            (B::BlockingRwCell, [t], "read") => Some(t.clone()),
            (B::BlockingRwCell, [_], "write") => Some(Type::Void),
            _ => None,
        },
        _ => None,
    }
}

fn binary_result_type(op: &BinOp, lhs_ty: &Type) -> Type {
    match op {
        // Arithmetic preserves the (already type-checked) operand type.
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem | BinOp::Pow => {
            lhs_ty.clone()
        }
        // Comparisons and logical operators always produce `Bool`.
        BinOp::Eq
        | BinOp::Ne
        | BinOp::Lt
        | BinOp::Le
        | BinOp::Gt
        | BinOp::Ge
        | BinOp::And
        | BinOp::Or => Type::Bool,
    }
}

fn lit(kind: HirExprKind, ty: Type, span: Span) -> HirExpr {
    HirExpr {
        kind,
        ty: ty.into(),
        span,
    }
}

fn unsupported(span: Span, what: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        ErrorCode::E0800,
        format!("HIR lowering does not yet support {what} (willow-mb5 slice 1)"),
    )
    .with_label(crate::diagnostics::Label::primary(span, "here"))
}

fn internal(span: Span, msg: String) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        ErrorCode::E0800,
        format!("internal compiler error: {msg}"),
    )
    .with_label(crate::diagnostics::Label::primary(span, "here"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    #[test]
    fn construction_identity_uses_normalized_type_alias() {
        use crate::semantic::ids::{SemanticType, TypeId};
        let tokens = Lexer::new(
            "class Alias { n: i64; } fn f() { let a = new Alias(1); let b = Alias { n: 2 }; }",
        )
        .tokenize()
        .unwrap();
        let (program, errors) = Parser::new(tokens).parse();
        assert!(errors.is_empty());
        let normalized = HashMap::from([(
            Type::Named("Alias".into()),
            Type::Named("sales::Amount".into()),
        )]);
        let tables = CheckerTables {
            normalized_types: Some(&normalized),
            ..Default::default()
        };
        let (hir, diagnostics) = lower_program_with(&program, &tables);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        for statement in &hir.functions[0].body {
            if let HirStmt::Let { value, .. } = statement {
                let (HirExprKind::New { class, .. } | HirExprKind::ObjectLiteral { class, .. }) =
                    &value.kind
                else {
                    panic!("constructor expected");
                };
                assert_eq!(*class, TypeId::from_source_name("sales::Amount"));
                assert_eq!(value.ty, SemanticType::Named(*class));
            }
        }
    }

    #[test]
    fn resolution_inherits_static_fields_and_resolves_self() {
        let (hir, diagnostics) = lower_src(
            "open class Base { pub static value: i64 = 7; } class Child extends Base { pub static fn read() -> i64 { return Self::value; } }",
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let child = crate::semantic::ids::TypeId::local("Child");
        assert_eq!(
            hir.resolution.classes[&child].static_fields["value"],
            crate::semantic::ids::SemanticType::I64
        );
        let method = hir
            .classes
            .iter()
            .find(|class| class.name == child)
            .unwrap()
            .methods
            .iter()
            .find(|method| method.name.unqualified_name() == "read")
            .unwrap();
        let HirStmt::Return {
            value: Some(value), ..
        } = &method.body[0]
        else {
            panic!("return expected")
        };
        assert!(matches!(&value.kind, HirExprKind::StaticField { class, .. } if *class == child));
    }

    #[test]
    fn resolution_snapshot_preserves_payload_order_and_parameter_modes() {
        use crate::semantic::ids::{FunctionId, TypeId};
        let (hir, diagnostics) = lower_src(
            "interface Reader<T> { fn read(self) -> T; } interface NamedReader extends Reader { fn name(self) -> String; } enum Pair { Empty, Values(i64, String) } class Item { pub z: i64; pub a: String; pub init(self, n: i64) { self.z = n; self.a = \"x\"; } pub static fn twice(n: i64) -> i64 { return n + n; } } fn touch(x: &mut i64) { x = 1; }",
        );
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        let reader = &hir.resolution.interfaces[&TypeId::local("Reader")];
        assert_eq!(reader.type_params, vec![TypeId::local("T")]);
        assert_eq!(
            reader.methods["read"].return_type,
            crate::semantic::ids::SemanticType::Named(TypeId::local("T"))
        );
        assert_eq!(
            hir.resolution.interfaces[&TypeId::local("NamedReader")].extends,
            vec![TypeId::local("Reader")]
        );
        let pair = &hir.resolution.enums[&TypeId::local("Pair")];
        assert_eq!(pair.variants[1].name, "Values");
        assert_eq!(pair.variants[1].tag, 1);
        assert_eq!(
            pair.variants[1].payloads,
            vec![
                crate::semantic::ids::SemanticType::I64,
                crate::semantic::ids::SemanticType::String
            ]
        );
        let item = &hir.resolution.classes[&TypeId::local("Item")];
        assert_eq!(
            item.fields
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["z", "a"]
        );
        assert_eq!(
            item.constructor.as_ref().unwrap().params,
            vec![crate::semantic::ids::SemanticType::I64]
        );
        assert!(item.methods["twice"].is_static);
        assert!(matches!(
            hir.resolution.functions[&FunctionId::free("touch")].param_modes[0],
            crate::parser::ast::ParamMode::Reference { mutable: true, .. }
        ));
        assert_eq!(
            hir.resolution.enums[&TypeId::local("Option")].variants[0].name,
            "Some"
        );
    }

    /// Deeply nested expressions must lower without exhausting a modest
    /// stack. Lowering is recursive, so the cost that matters is the size of
    /// ONE level: the arms of `lower_expr_inner` live in their own functions
    /// precisely so a level pays for the arm it takes instead of for every arm
    /// at once (willow-t0uy.1). With them inline, a debug build spent ~63KB per
    /// level and 256 levels needed 16MiB; a level now costs ~6KB, so this fits
    /// several times over.
    #[test]
    fn deep_expression_nesting_lowers_within_a_four_megabyte_stack() {
        const DEPTH: usize = 256;
        std::thread::Builder::new()
            .stack_size(4 * 1024 * 1024)
            .spawn(|| {
                let chain = vec!["1"; DEPTH].join(" + ");
                let src = format!("fn main() {{ let total = {chain}; println(total); }}");
                let (hir, diags) = lower_src(&src);
                assert!(diags.is_empty(), "{diags:?}");
                assert_eq!(hir.functions.len(), 1);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn operator_lowering_uses_heap_frames_on_success_and_error() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                for fail_after_left in [false, true] {
                    let span = Span::new(0, 1, 1, 1);
                    let mut expr = Expr::Integer(1, span, ExprId::fresh());
                    for depth in 0..50_000 {
                        expr = if depth % 2 == 0 {
                            Expr::Unary(Box::new(UnaryExpr {
                                id: ExprId::fresh(),
                                op: UnaryOp::Neg,
                                expr,
                                span,
                            }))
                        } else {
                            Expr::Binary(Box::new(BinaryExpr {
                                id: ExprId::fresh(),
                                op: BinOp::Add,
                                lhs: expr,
                                rhs: Expr::Integer(1, span, ExprId::fresh()),
                                span,
                            }))
                        };
                    }
                    if fail_after_left {
                        expr = Expr::Binary(Box::new(BinaryExpr {
                            id: ExprId::fresh(),
                            op: BinOp::Add,
                            lhs: expr,
                            rhs: Expr::Var("missing".into(), span, ExprId::fresh()),
                            span,
                        }));
                    }
                    let program = Program {
                        module: None,
                        imports: vec![],
                        items: vec![Item::Function(FunctionDecl {
                            name: "main".into(),
                            public: false,
                            is_async: false,
                            params: vec![],
                            return_type: Type::Void,
                            body: Block {
                                stmts: vec![Stmt::Expr(crate::parser::ast::ExprStmt {
                                    expr,
                                    span,
                                })],
                                span,
                            },
                            span,
                        })],
                    };
                    let (hir, diagnostics) = lower_program(&program);
                    let mut count = 0;
                    let mut root_type = None;
                    for function in &hir.functions {
                        for statement in &function.body {
                            if let HirStmt::Expr(expr) = statement {
                                count = expr.walk_postorder(true).count();
                                root_type = Some(expr.ty.to_source());
                            }
                        }
                    }
                    drop(hir);
                    drop(program);
                    if fail_after_left {
                        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
                    } else {
                        assert!(diagnostics.is_empty(), "{diagnostics:?}");
                        assert_eq!(root_type, Some(Type::I64));
                        assert_eq!(count, 75_001);
                    }
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    fn lower_src(src: &str) -> (HirProgram, Vec<Diagnostic>) {
        let tokens = Lexer::new(src).tokenize().expect("lexing failed");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "unexpected parse errors: {errs:?}");
        lower_program(&program)
    }

    /// Lower a program expected to be wholly within slice 1; assert no
    /// diagnostics and return the first function's body.
    fn lower_body(src: &str) -> Vec<HirStmt> {
        let (hir, diags) = lower_src(src);
        assert!(
            diags.is_empty(),
            "unexpected lowering diagnostics: {diags:?}"
        );
        hir.functions
            .into_iter()
            .next()
            .expect("at least one function")
            .body
    }

    /// Extract the value expression of the first `return` statement in a body.
    fn first_return(body: &[HirStmt]) -> &HirExpr {
        body.iter()
            .find_map(|s| match s {
                HirStmt::Return { value: Some(v), .. } => Some(v),
                _ => None,
            })
            .expect("a return with a value")
    }

    fn return_ty(src: &str) -> Type {
        let body = lower_body(src);
        first_return(&body).ty.to_source()
    }

    // 1. integer literal → I64
    #[test]
    fn p01_integer_literal_is_i64() {
        assert_eq!(return_ty("fn f() -> i64 { return 7; }"), Type::I64);
    }

    // 2. float literal → F64
    #[test]
    fn p02_float_literal_is_f64() {
        assert_eq!(return_ty("fn f() -> f64 { return 1.5; }"), Type::F64);
    }

    // 3. bool literal → Bool
    #[test]
    fn p03_bool_literal_is_bool() {
        assert_eq!(return_ty("fn f() -> bool { return true; }"), Type::Bool);
    }

    // 4. string literal → String
    #[test]
    fn p04_string_literal_is_string() {
        assert_eq!(
            return_ty("fn f() -> String { return \"hi\"; }"),
            Type::String
        );
    }

    // 5. parameter variable read carries its declared type
    #[test]
    fn p05_param_var_has_declared_type() {
        assert_eq!(return_ty("fn f(a: i64) -> i64 { return a; }"), Type::I64);
    }

    // 6. f64 parameter variable read
    #[test]
    fn p06_param_var_f64() {
        assert_eq!(return_ty("fn f(a: f64) -> f64 { return a; }"), Type::F64);
    }

    // 7. integer addition → I64
    #[test]
    fn p07_i64_add_is_i64() {
        assert_eq!(return_ty("fn f() -> i64 { return 1 + 2; }"), Type::I64);
    }

    // 8. float addition → F64
    #[test]
    fn p08_f64_add_is_f64() {
        assert_eq!(return_ty("fn f() -> f64 { return 1.0 + 2.0; }"), Type::F64);
    }

    // 9. subtraction/multiplication/division/remainder preserve operand type
    #[test]
    fn p09_arithmetic_preserves_operand_type() {
        for op in ["-", "*", "/", "%"] {
            let src = format!("fn f() -> i64 {{ return 6 {op} 3; }}");
            assert_eq!(return_ty(&src), Type::I64, "op {op}");
        }
    }

    // 10. equality → Bool
    #[test]
    fn p10_eq_is_bool() {
        assert_eq!(return_ty("fn f() -> bool { return 1 == 2; }"), Type::Bool);
    }

    // 11. relational comparisons → Bool
    #[test]
    fn p11_relational_is_bool() {
        for op in ["<", "<=", ">", ">=", "!="] {
            let src = format!("fn f() -> bool {{ return 1 {op} 2; }}");
            assert_eq!(return_ty(&src), Type::Bool, "op {op}");
        }
    }

    // 12. logical and/or → Bool
    #[test]
    fn p12_logical_is_bool() {
        assert_eq!(
            return_ty("fn f() -> bool { return true && false; }"),
            Type::Bool
        );
        assert_eq!(
            return_ty("fn f() -> bool { return true || false; }"),
            Type::Bool
        );
    }

    // 13. unary negation preserves operand type
    #[test]
    fn p13_unary_neg_preserves_type() {
        assert_eq!(return_ty("fn f(a: i64) -> i64 { return -a; }"), Type::I64);
    }

    // 14. unary not → Bool
    #[test]
    fn p14_unary_not_is_bool() {
        assert_eq!(
            return_ty("fn f(a: bool) -> bool { return !a; }"),
            Type::Bool
        );
    }

    // 15. nested binary propagates the operand type outward
    #[test]
    fn p15_nested_binary_type() {
        assert_eq!(
            return_ty("fn f() -> i64 { return (1 + 2) * 3 - 4; }"),
            Type::I64
        );
    }

    // 16. comparison of arithmetic sub-expressions is still Bool
    #[test]
    fn p16_compare_of_arithmetic_is_bool() {
        assert_eq!(
            return_ty("fn f() -> bool { return 1 + 2 < 3 * 4; }"),
            Type::Bool
        );
    }

    // 17. free-function call carries the callee's return type
    #[test]
    fn p17_call_has_callee_return_type() {
        let ty = return_ty("fn g() -> i64 { return 1; } fn f() -> i64 { return g(); }");
        assert_eq!(ty, Type::I64);
    }

    // 18. call with arguments lowers each argument
    #[test]
    fn p18_call_with_args() {
        let (hir, diags) = lower_src(
            "fn add(a: i64, b: i64) -> i64 { return a + b; } \
             fn f() -> i64 { return add(1, 2); }",
        );
        assert!(diags.is_empty(), "{diags:?}");
        let f = hir
            .functions
            .iter()
            .find(|fun| fun.name == crate::semantic::ids::FunctionId::free_from_source_name("f"))
            .expect("function f");
        match &first_return(&f.body).kind {
            HirExprKind::Call { callee, args } => {
                assert_eq!(
                    callee,
                    &crate::semantic::ids::FunctionId::free_from_source_name("add")
                );
                assert_eq!(args.len(), 2);
                assert_eq!(args[0].ty, Type::I64);
            }
            other => panic!("expected call, got {other:?}"),
        }
    }

    // 19. print expression is Void
    #[test]
    fn p19_print_is_void() {
        let body = lower_body("fn f() { print(1); }");
        match &body[0] {
            HirStmt::Expr(e) => {
                assert_eq!(e.ty, Type::Void);
                assert!(matches!(e.kind, HirExprKind::Print { newline: false, .. }));
            }
            other => panic!("expected expr stmt, got {other:?}"),
        }
    }

    // 20. println sets the newline flag
    #[test]
    fn p20_println_newline_flag() {
        let body = lower_body("fn f() { println(1); }");
        match &body[0] {
            HirStmt::Expr(e) => {
                assert!(matches!(e.kind, HirExprKind::Print { newline: true, .. }));
            }
            other => panic!("expected expr stmt, got {other:?}"),
        }
    }

    // 21. let binds the inferred value type into scope
    #[test]
    fn p21_let_binds_inferred_type() {
        let ty = return_ty("fn f() -> i64 { let x = 5; return x; }");
        assert_eq!(ty, Type::I64);
    }

    // 22. let with an explicit annotation pins the binding type
    #[test]
    fn p22_let_annotation_pins_type() {
        let body = lower_body("fn f() { let x: f64 = 2.0; }");
        match &body[0] {
            HirStmt::Let { mutable, value, .. } => {
                assert!(!mutable);
                assert_eq!(value.ty, Type::F64);
            }
            other => panic!("expected let, got {other:?}"),
        }
    }

    // 23. let mut records mutability
    #[test]
    fn p23_let_mut_records_mutability() {
        let body = lower_body("fn f() { let mut x = 1; }");
        assert!(matches!(body[0], HirStmt::Let { mutable: true, .. }));
    }

    // 24. assignment lowers its value expression
    #[test]
    fn p24_assign_lowers_value() {
        let body = lower_body("fn f() { let mut x = 1; x = 2; }");
        match &body[1] {
            HirStmt::Assign { name, value, .. } => {
                assert_eq!(name, "x");
                assert_eq!(value.ty, Type::I64);
            }
            other => panic!("expected assign, got {other:?}"),
        }
    }

    // 25. if lowers a Bool condition and both branches
    #[test]
    fn p25_if_cond_is_bool_with_branches() {
        let body = lower_body("fn f(a: i64) { if a > 0 { print(1); } else { print(2); } }");
        match &body[0] {
            HirStmt::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                assert_eq!(cond.ty, Type::Bool);
                assert_eq!(then_branch.len(), 1);
                assert_eq!(else_branch.as_ref().map(|b| b.len()), Some(1));
            }
            other => panic!("expected if, got {other:?}"),
        }
    }

    // 26. if without else has no else branch
    #[test]
    fn p26_if_without_else() {
        let body = lower_body("fn f(a: i64) { if a > 0 { print(1); } }");
        assert!(matches!(
            &body[0],
            HirStmt::If {
                else_branch: None,
                ..
            }
        ));
    }

    // 27. while lowers a Bool condition and a body
    #[test]
    fn p27_while_cond_is_bool() {
        let body = lower_body("fn f(a: bool) { while a { print(1); } }");
        match &body[0] {
            HirStmt::While { cond, body, .. } => {
                assert_eq!(cond.ty, Type::Bool);
                assert_eq!(body.len(), 1);
            }
            other => panic!("expected while, got {other:?}"),
        }
    }

    // 28. return without a value lowers to None
    #[test]
    fn p28_bare_return() {
        let body = lower_body("fn f() { return; }");
        assert!(matches!(body[0], HirStmt::Return { value: None, .. }));
    }

    // 29. function parameters and return type are carried on HirFunction
    #[test]
    fn p29_function_signature_carried() {
        let (hir, diags) = lower_src("fn f(a: i64, b: bool) -> i64 { return a; }");
        assert!(diags.is_empty(), "{diags:?}");
        let f = &hir.functions[0];
        assert_eq!(f.name.name(), "f");
        assert_eq!(f.params.len(), 2);
        assert_eq!(f.params[0].ty, Type::I64);
        assert_eq!(f.params[1].ty, Type::Bool);
        assert_eq!(f.return_type, Type::I64);
    }

    // 30. a block-scoped binding does not leak to an outer scope
    #[test]
    fn p30_block_scope_does_not_leak() {
        // `x` is declared inside the `if` block; reading `x` after the block
        // would be an unbound-variable internal error if scopes leaked. Here the
        // outer `return y` only sees the outer binding, so lowering succeeds.
        let (_, diags) = lower_src(
            "fn f(c: bool) -> i64 { let y = 1; if c { let x = 2; print(x); } return y; }",
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    // 31. an out-of-slice construct (`for` over a Map) is reported, not
    // panicked on.
    #[test]
    fn p31_unsupported_construct_reports_diagnostic() {
        let (_, diags) = lower_src("fn f(m: Map<String, i64>) { for v in m { print(v); } }");
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("does not yet support")),
            "expected an unsupported-construct diagnostic, got {diags:?}"
        );
    }

    // 32. multi-statement function lowers every statement in order
    #[test]
    fn p32_multi_statement_function() {
        let body = lower_body("fn f() -> i64 { let a = 1; let b = 2; let c = a + b; return c; }");
        assert_eq!(body.len(), 4);
        assert!(matches!(body[0], HirStmt::Let { .. }));
        assert!(matches!(body[3], HirStmt::Return { .. }));
    }

    // 33. an i64 array literal has type Array<i64>
    #[test]
    fn p33_array_literal_i64() {
        let body = lower_body("fn f() { let xs = [1, 2, 3]; }");
        match &body[0] {
            HirStmt::Let { value, .. } => {
                assert_eq!(value.ty, Type::Array(Box::new(Type::I64)));
                assert!(
                    matches!(&value.kind, HirExprKind::Array { elements } if elements.len() == 3)
                );
            }
            other => panic!("expected let, got {other:?}"),
        }
    }

    // 34. an f64 array literal has type Array<f64>
    #[test]
    fn p34_array_literal_f64() {
        let body = lower_body("fn f() { let xs = [1.0, 2.0]; }");
        match &body[0] {
            HirStmt::Let { value, .. } => {
                assert_eq!(value.ty, Type::Array(Box::new(Type::F64)));
            }
            other => panic!("expected let, got {other:?}"),
        }
    }

    // 35. indexing an array yields its element type
    #[test]
    fn p35_index_yields_element_type() {
        assert_eq!(
            return_ty("fn f() -> i64 { let xs = [4, 5, 6]; return xs[0]; }"),
            Type::I64
        );
    }

    // 36. a ternary takes the (shared) branch type
    #[test]
    fn p36_ternary_branch_type() {
        assert_eq!(
            return_ty("fn f(c: bool) -> i64 { return c ? 1 : 2; }"),
            Type::I64
        );
    }

    // 37. an index expression composes inside arithmetic
    #[test]
    fn p37_index_in_arithmetic() {
        assert_eq!(
            return_ty("fn f() -> i64 { let xs = [1, 2]; return xs[0] + xs[1]; }"),
            Type::I64
        );
    }

    // 38. an empty array literal is reported (element type needs context)
    #[test]
    fn p38_empty_array_literal_reports() {
        let (_, diags) = lower_src("fn f() { let xs: Array<i64> = []; }");
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("empty array literal")),
            "expected an empty-array diagnostic, got {diags:?}"
        );
    }

    // 39. `new Class(...)` has the class type
    #[test]
    fn p39_new_has_class_type() {
        let body = lower_body("class Box { pub v: i64; } fn f() { let b = new Box(7); }");
        match &body[0] {
            HirStmt::Let { value, .. } => {
                assert_eq!(value.ty, Type::Named("Box".into()));
                assert!(
                    matches!(&value.kind, HirExprKind::New { class, .. } if class == &crate::semantic::ids::TypeId::local("Box"))
                );
            }
            other => panic!("expected let, got {other:?}"),
        }
    }

    // 40. a field access takes the field's declared type
    #[test]
    fn p40_field_access_type() {
        assert_eq!(
            return_ty(
                "class Box { pub v: i64; } fn f() -> i64 { let b = new Box(7); return b.v; }"
            ),
            Type::I64
        );
    }

    // 41. a method call takes the method's return type
    #[test]
    fn p41_method_call_return_type() {
        let src = "class Box { pub v: i64; pub fn get(self) -> i64 { return self.v; } } \
                   fn f() -> i64 { let b = new Box(7); return b.get(); }";
        assert_eq!(return_ty(src), Type::I64);
    }

    // 42. an unknown field is reported (not yet covered, e.g. inherited)
    #[test]
    fn p42_unknown_field_reports() {
        let (_, diags) = lower_src(
            "class Box { pub v: i64; } fn f() -> i64 { let b = new Box(7); return b.missing; }",
        );
        assert!(
            diags.iter().any(|d| d.message.contains("field not found")),
            "expected a field-not-found diagnostic, got {diags:?}"
        );
    }

    // 43. an unknown method is reported
    #[test]
    fn p43_unknown_method_reports() {
        let (_, diags) = lower_src(
            "class Box { pub v: i64; } fn f() -> i64 { let b = new Box(7); return b.gone(); }",
        );
        assert!(
            diags.iter().any(|d| d.message.contains("method not found")),
            "expected a method-not-found diagnostic, got {diags:?}"
        );
    }

    // 44. a class method body lowers into HirProgram.classes with a typed `self`
    #[test]
    fn p44_class_method_lowered() {
        let (hir, diags) =
            lower_src("class Box { pub v: i64; pub fn get(self) -> i64 { return self.v; } }");
        assert!(diags.is_empty(), "{diags:?}");
        assert_eq!(hir.classes.len(), 1);
        let class = &hir.classes[0];
        assert_eq!(class.name, crate::semantic::ids::TypeId::local("Box"));
        assert_eq!(class.methods.len(), 1);
        let m = &class.methods[0];
        assert_eq!(m.name.name(), "get");
        assert_eq!(m.params[0].name, "self");
        assert_eq!(m.params[0].ty, Type::Named("Box".into()));
    }

    // 45. `self.field` inside a method body resolves to the field's type
    #[test]
    fn p45_self_field_access_typed_in_method() {
        let (hir, diags) =
            lower_src("class Box { pub v: i64; pub fn get(self) -> i64 { return self.v; } }");
        assert!(diags.is_empty(), "{diags:?}");
        let m = &hir.classes[0].methods[0];
        assert_eq!(first_return(&m.body).ty, Type::I64);
    }

    // 46. a method parameter is bound in the method body
    #[test]
    fn p46_method_param_bound_in_body() {
        let (hir, diags) =
            lower_src("class Box { pub fn echo(self, n: i64) -> i64 { return n; } }");
        assert!(diags.is_empty(), "{diags:?}");
        let m = &hir.classes[0].methods[0];
        assert_eq!(m.params.len(), 2, "self + n");
        assert_eq!(m.params[1].name, "n");
        assert_eq!(first_return(&m.body).ty, Type::I64);
    }

    // 47. a static method has no `self` parameter
    #[test]
    fn p47_static_method_has_no_self_param() {
        let (hir, diags) = lower_src("class Box { static fn make() -> i64 { return 1; } }");
        assert!(diags.is_empty(), "{diags:?}");
        let m = &hir.classes[0].methods[0];
        assert!(
            m.params.is_empty(),
            "static method should have no self param"
        );
    }

    // 48. an inherited field resolves along the base-class chain
    #[test]
    fn p48_inherited_field_type() {
        let src = "class A { v: i64; } class B extends A {} \
                   fn f() -> i64 { let b = new B(1); return b.v; }";
        assert_eq!(return_ty(src), Type::I64);
    }

    // 49. an inherited method's return type resolves along the chain
    #[test]
    fn p49_inherited_method_type() {
        let src = "class A { pub fn m(self) -> i64 { return 1; } } class B extends A {} \
                   fn f() -> i64 { let b = new B(); return b.m(); }";
        assert_eq!(return_ty(src), Type::I64);
    }

    // 50. a `for` over an array binds the loop variable to the element type
    #[test]
    fn p50_for_binds_element_type() {
        let (hir, diags) = lower_src(
            "fn f() -> i64 { let xs = [10]; let mut s = 0; for v in xs { s = s + v; } return s; }",
        );
        assert!(diags.is_empty(), "{diags:?}");
        let body = &hir.functions[0].body;
        assert!(body.iter().any(|s| matches!(
            s,
            HirStmt::For { iterable, .. } if iterable.ty == Type::Array(Box::new(Type::I64))
        )));
    }

    // 51. an object literal has the class type
    #[test]
    fn p51_object_literal_type() {
        let body = lower_body("class P { x: i64; } fn f() { let p = P { x: 1 }; }");
        match &body[0] {
            HirStmt::Let { value, .. } => {
                assert_eq!(value.ty, Type::Named("P".into()));
            }
            other => panic!("expected let, got {other:?}"),
        }
    }

    // 53. a static property read takes the property's type
    #[test]
    fn p53_static_field_read_type() {
        assert_eq!(
            return_ty("class C { static v: i64 = 0; } fn f() -> i64 { return C::v; }"),
            Type::I64
        );
    }

    // 54. a static call takes the static method's return type
    #[test]
    fn p54_static_call_return_type() {
        assert_eq!(
            return_ty(
                "class C { static fn make() -> i64 { return 1; } } fn f() -> i64 { return C::make(); }"
            ),
            Type::I64
        );
    }

    // 55. field/index/static-field assignment statements lower cleanly
    #[test]
    fn p55_assignment_statements() {
        let (hir, diags) = lower_src(
            "class C { x: i64; static mut t: i64 = 0; } \
             fn f() { let p = new C(1); p.x = 2; let xs = [1]; xs[0] = 9; C::t = 5; }",
        );
        assert!(diags.is_empty(), "{diags:?}");
        // A class with a static initializer contributes its own `$static_init`
        // function ahead of the user's, so pick `f` by name (willow-t0uy.3).
        let body = &hir
            .functions
            .iter()
            .find(|f| f.name == crate::semantic::ids::FunctionId::free_from_source_name("f"))
            .expect("`f` lowers")
            .body;
        assert!(
            body.iter()
                .any(|s| matches!(s, HirStmt::FieldAssign { .. }))
        );
        assert!(
            body.iter()
                .any(|s| matches!(s, HirStmt::IndexAssign { .. }))
        );
        assert!(
            body.iter()
                .any(|s| matches!(s, HirStmt::StaticFieldAssign { .. }))
        );
    }

    // 56. a range expression has type Range<i64>
    #[test]
    fn p56_range_expr_type() {
        let body = lower_body("fn f() { let r = 0..3; }");
        match &body[0] {
            HirStmt::Let { value, .. } => {
                assert_eq!(value.ty, Type::Generic("Range".into(), vec![Type::I64]));
            }
            other => panic!("expected let, got {other:?}"),
        }
    }

    // 57. a `for` over a range binds the loop variable to i64
    #[test]
    fn p57_range_for_binds_i64() {
        let (hir, diags) =
            lower_src("fn f() -> i64 { let mut s = 0; for i in 0..4 { s = s + i; } return s; }");
        assert!(diags.is_empty(), "{diags:?}");
        // Body lowered without an unbound-variable error means `i` was bound;
        // the addition typing i64 confirms the element type.
        let body = &hir.functions[0].body;
        assert!(body.iter().any(|s| matches!(s, HirStmt::For { .. })));
    }

    // 58. calling an async fn yields Task<T> at the call site
    #[test]
    fn p58_async_call_yields_task() {
        let (hir, diags) = lower_src("async fn g() -> i64 { return 1; } fn f() { let t = g(); }");
        assert!(diags.is_empty(), "{diags:?}");
        let f = hir
            .functions
            .iter()
            .find(|x| x.name == crate::semantic::ids::FunctionId::free_from_source_name("f"))
            .unwrap();
        let body = &f.body;
        match &body[0] {
            HirStmt::Let { value, .. } => {
                assert_eq!(value.ty, Type::Generic("Task".into(), vec![Type::I64]));
            }
            other => panic!("expected let, got {other:?}"),
        }
    }

    // 59. awaiting a Task<T> yields T
    #[test]
    fn p59_await_task_unwraps() {
        let src = "async fn g() -> i64 { return 1; } async fn f() -> i64 { return await g(); }";
        let (hir, diags) = lower_src(src);
        assert!(diags.is_empty(), "{diags:?}");
        let f = hir
            .functions
            .iter()
            .find(|x| x.name == crate::semantic::ids::FunctionId::free_from_source_name("f"))
            .unwrap();
        assert_eq!(first_return(&f.body).ty, Type::I64);
    }

    // 60. awaiting the builtin sleep (Future<void>) lowers cleanly
    #[test]
    fn p60_await_builtin_sleep() {
        let (_, diags) = lower_src("async fn f() { await sleep(1); }");
        assert!(diags.is_empty(), "{diags:?}");
    }

    // 61. `?` on a Result<T, E> yields T
    #[test]
    fn p61_try_on_result_yields_ok_type() {
        assert_eq!(
            return_ty("fn f(r: Result<i64, String>) -> i64 { return r?; }"),
            Type::I64
        );
    }

    // 62. `?` on an Option<T> yields T
    #[test]
    fn p62_try_on_option_yields_some_type() {
        assert_eq!(
            return_ty("fn f(o: Option<i64>) -> i64 { return o?; }"),
            Type::I64
        );
    }

    // 63. a lambda with annotated params has an fn(..) -> ret type
    #[test]
    fn p63_lambda_fn_type() {
        let body = lower_body("fn f() { let d = |x: i64| x * 2; }");
        match &body[0] {
            HirStmt::Let { value, .. } => {
                assert_eq!(value.ty, Type::Fn(vec![Type::I64], Box::new(Type::I64)));
            }
            other => panic!("expected let, got {other:?}"),
        }
    }

    // 64. an unannotated lambda parameter is reported, not guessed
    #[test]
    fn p64_unannotated_lambda_param_reports() {
        let (_, diags) = lower_src("fn f() { let d = |x| x; }");
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unannotated lambda parameter")),
            "{diags:?}"
        );
    }

    // 65. calling a local fn-typed variable is an indirect call typed by fn(..)->R
    #[test]
    fn p65_indirect_call_via_fn_var() {
        assert_eq!(
            return_ty("fn f(g: fn(i64) -> i64) -> i64 { return g(1); }"),
            Type::I64
        );
    }

    // 66. a constructor lowers as `init` with self bound; super.init lowers too
    #[test]
    fn p66_constructor_and_super_init() {
        let src = "open class A { v: i64; init(self, v: i64) { self.v = v; } } \
                   class B extends A { init(self, v: i64) { super.init(v); } }";
        let (hir, diags) = lower_src(src);
        assert!(diags.is_empty(), "{diags:?}");
        let b = hir
            .classes
            .iter()
            .find(|c| c.name == crate::semantic::ids::TypeId::local("B"))
            .unwrap();
        let init = &b.methods[0];
        assert_eq!(init.name.name(), "init");
        assert_eq!(init.params[0].name, "self");
        assert!(
            init.body
                .iter()
                .any(|s| matches!(s, HirStmt::SuperInit { .. }))
        );
    }

    // 67. a builtin function call is typed from the seeded builtin registry
    #[test]
    fn p67_builtin_call_typed() {
        assert_eq!(
            return_ty("fn f() -> i64 { return gc_allocated_bytes(); }"),
            Type::I64
        );
    }

    // 68. match on a user enum: arm bodies typed, payload bindings typed
    #[test]
    fn p68_match_user_enum() {
        let src = "enum Shape { Circle(i64), Empty, } \
                   fn f(s: Shape) -> i64 { return match s { Shape::Circle(r) => r * 2, Shape::Empty => 0, }; }";
        assert_eq!(return_ty(src), Type::I64);
    }

    // 69. match on Option<i64>: variant payload substituted to i64
    #[test]
    fn p69_match_option_substitutes() {
        let src = "fn f(o: Option<i64>) -> i64 { return match o { Some(v) => v, None => -1, }; }";
        assert_eq!(return_ty(src), Type::I64);
    }

    // 70. match on Result<i64, String>: Ok/Err payloads substituted
    #[test]
    fn p70_match_result_substitutes() {
        let src = "fn f(r: Result<i64, String>) -> i64 { \
                   return match r { Ok(v) => v, Err(_) => 0, }; }";
        assert_eq!(return_ty(src), Type::I64);
    }

    // 71. a panicking (Never) arm coerces to the other arms' type
    #[test]
    fn p71_never_arm_coerces() {
        let src = "fn f(o: Option<i64>) -> i64 { \
                   return match o { Some(v) => v, None => panic(\"none\"), }; }";
        assert_eq!(return_ty(src), Type::I64);
    }

    // 72. enum variant construction has the enum type
    #[test]
    fn p72_enum_construction() {
        let src = "enum Shape { Circle(i64), } fn f() -> Shape { return Shape::Circle(1); }";
        assert_eq!(return_ty(src), Type::Named("Shape".into()));
    }

    // 73. a fieldless variant value read has the enum type
    #[test]
    fn p73_fieldless_variant_value() {
        let src = "enum Color { Red, Blue, } fn f() -> Color { return Color::Red; }";
        assert_eq!(return_ty(src), Type::Named("Color".into()));
    }

    // 74. builtin array methods: len/pop/freeze types
    #[test]
    fn p74_array_builtin_methods() {
        assert_eq!(
            return_ty("fn f() -> i64 { let xs = [1]; return xs.len(); }"),
            Type::I64
        );
        assert_eq!(
            return_ty("fn f() -> i64 { let mut xs = [1]; return xs.pop(); }"),
            Type::I64
        );
        let body = lower_body("fn f() { let xs = [1]; let fr = xs.freeze(); }");
        match &body[1] {
            HirStmt::Let { value, .. } => {
                assert_eq!(
                    value.ty,
                    Type::Generic("FrozenArray".into(), vec![Type::I64])
                );
            }
            other => panic!("expected let, got {other:?}"),
        }
    }

    // 75. builtin map/task/lock methods: get/contains/len/result/lock types
    #[test]
    fn p75_map_task_lock_builtin_methods() {
        assert_eq!(
            return_ty("fn f(m: Map<String, i64>) -> bool { return m.contains(\"k\"); }"),
            Type::Bool
        );
        let body = lower_body("fn f(m: Map<String, i64>) { let v = m.get(\"k\"); }");
        match &body[0] {
            HirStmt::Let { value, .. } => {
                assert_eq!(value.ty, Type::Generic("Option".into(), vec![Type::I64]));
            }
            other => panic!("expected let, got {other:?}"),
        }
        assert_eq!(
            return_ty(
                "async fn g() -> i64 { return 1; } async fn f() -> i64 { let t = g(); return await t; }"
            ),
            Type::I64
        );
        // `Mutex<T>` lost its accessors in willow-38w.1.4; the blocking cell
        // and the RwLock keep theirs, and both must still lower.
        assert_eq!(
            return_ty("fn f(c: BlockingCell<i64>) -> i64 { return c.get(); }"),
            Type::I64
        );
        assert_eq!(
            return_ty("fn f(r: BlockingRwCell<i64>) -> i64 { return r.read(); }"),
            Type::I64
        );
    }

    /// Run the real type checker, then lower with its side tables.
    fn lower_with_checker(src: &str) -> (HirProgram, Vec<Diagnostic>) {
        let tokens = Lexer::new(src).tokenize().expect("lexing failed");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "parse errors: {errs:?}");
        let mut checker = crate::semantic::TypeChecker::new();
        crate::register_prelude(&mut checker).expect("prelude registers");
        checker.check_program(&program);
        assert!(
            checker.errors.is_empty(),
            "checker errors: {:?}",
            checker.errors
        );
        let tables = CheckerTables::from_checker(&checker);
        lower_program_with(&program, &tables)
    }

    // 76. an unannotated lambda types via the checker's inferred fn(..) table
    #[test]
    fn p76_unannotated_lambda_via_checker_tables() {
        let src = "fn apply(f: fn(i64) -> i64, v: i64) -> i64 { return f(v); } \
                   fn g() -> i64 { return apply(|x| x * 3, 14); }";
        let (hir, diags) = lower_with_checker(src);
        assert!(diags.is_empty(), "{diags:?}");
        let g = hir
            .functions
            .iter()
            .find(|f| f.name == crate::semantic::ids::FunctionId::free_from_source_name("g"))
            .unwrap();
        // The lambda argument inside the call carries the full fn type.
        let HirStmt::Return { value: Some(v), .. } = &g.body[0] else {
            panic!("expected return");
        };
        let HirExprKind::Call { args, .. } = &v.kind else {
            panic!("expected call");
        };
        assert_eq!(args[0].ty, Type::Fn(vec![Type::I64], Box::new(Type::I64)));
    }

    // 77. an unqualified variant construction resolves via the checker table
    #[test]
    fn p77_unqualified_enum_construction_via_checker_tables() {
        let src = "enum Msg { Num(i64), } fn f(b: bool) -> Msg { return Num(7); }";
        let (hir, diags) = lower_with_checker(src);
        assert!(diags.is_empty(), "{diags:?}");
        let f = &hir.functions[0];
        assert_eq!(first_return(&f.body).ty, Type::Named("Msg".into()));
        assert!(matches!(
            &first_return(&f.body).kind,
            HirExprKind::StaticCall { class, method, .. } if class == &crate::semantic::ids::TypeId::local("Msg") && method == "Num"
        ));
    }

    // 79. generic enum construction carries full type args via expr_types
    #[test]
    fn p79_generic_enum_construction_via_expr_types() {
        let src = "fn f(b: bool) -> Result<i64, String> { \
                   if b { return Err(\"x\"); } return Ok(1); }";
        let (hir, diags) = lower_with_checker(src);
        assert!(diags.is_empty(), "{diags:?}");
        let f = &hir.functions[0];
        let expected = Type::Generic("Result".into(), vec![Type::I64, Type::String]);
        assert_eq!(first_return(&f.body).ty, expected);
    }

    // 80. a bare fieldless `None` resolves to Option<T> via the checker tables
    #[test]
    fn p80_bare_none_via_checker_tables() {
        let src = "fn f() -> Option<i64> { return None; }";
        let (hir, diags) = lower_with_checker(src);
        assert!(diags.is_empty(), "{diags:?}");
        let ret = first_return(&hir.functions[0].body);
        assert_eq!(ret.ty, Type::Generic("Option".into(), vec![Type::I64]));
        assert!(matches!(
            &ret.kind,
            HirExprKind::StaticField { class, field } if class == &crate::semantic::ids::TypeId::local("Option") && field == "None"
        ));
    }

    // 81. an interface-typed receiver's method call types via expr_types
    #[test]
    fn p81_interface_method_via_expr_types() {
        let src = "interface Animal { fn speak(self) -> i64; } \
                   class Dog implements Animal { pub fn speak(self) -> i64 { return 7; } } \
                   fn f(a: Animal) -> i64 { return a.speak(); }";
        let (hir, diags) = lower_with_checker(src);
        assert!(diags.is_empty(), "{diags:?}");
        let f = hir
            .functions
            .iter()
            .find(|x| x.name == crate::semantic::ids::FunctionId::free_from_source_name("f"))
            .unwrap();
        assert_eq!(first_return(&f.body).ty, Type::I64);
    }

    // 82. `return Result::Ok();` in a `Result<void, E>` function types through
    //     the checker table. The checker special-cases the zero-argument form
    //     and used to return before recording anything, so lowering could not
    //     type the call and reported a gap -- which dropped the WHOLE function
    //     from the HIR, and with it from the LIR (willow-0g8j.2.14).
    #[test]
    fn p82_zero_arg_result_ok_types_via_checker_tables() {
        let src = "fn f() -> Result<void, String> { return Result::Ok(); }";
        let (hir, diags) = lower_with_checker(src);
        assert!(diags.is_empty(), "{diags:?}");
        let f = hir
            .functions
            .iter()
            .find(|x| x.name == crate::semantic::ids::FunctionId::free_from_source_name("f"))
            .unwrap();
        assert_eq!(
            first_return(&f.body).ty,
            Type::Generic("Result".into(), vec![Type::Void, Type::String])
        );
    }

    // 83. The same shape as the entry point, which is the case that mattered:
    //     `main` has to survive into `HirProgram::functions` for the backend to
    //     have any lowered IR to select.
    #[test]
    fn p83_result_void_main_reaches_the_hir() {
        let src = "fn check(n: i64) -> Result<i64, String> { return Result::Ok(n); } \
                   fn main() -> Result<void, String> { let v = check(21)?; println(v); \
                   return Result::Ok(); }";
        let (hir, diags) = lower_with_checker(src);
        assert!(diags.is_empty(), "{diags:?}");
        assert!(
            hir.functions
                .iter()
                .any(|f| f.name == crate::semantic::ids::FunctionId::free_from_source_name("main")),
            "main must not be dropped: {:?}",
            hir.functions.iter().map(|f| &f.name).collect::<Vec<_>>()
        );
    }

    // 84. The argument-carrying `Result::Ok(v)` is the neighbouring case: it
    //     goes through the ordinary `check_expr` path, so its type was always
    //     recorded. The fix above must not have moved that.
    #[test]
    fn p84_result_ok_with_a_payload_still_types() {
        let (hir, diags) =
            lower_with_checker("fn f() -> Result<i64, String> { return Result::Ok(7); }");
        assert!(diags.is_empty(), "{diags:?}");
        let f = hir
            .functions
            .iter()
            .find(|x| x.name == crate::semantic::ids::FunctionId::free_from_source_name("f"))
            .unwrap();
        assert_eq!(
            first_return(&f.body).ty,
            Type::Generic("Result".into(), vec![Type::I64, Type::String])
        );
    }

    // 78. without checker tables, both cases still degrade to diagnostics
    #[test]
    fn p78_structural_fallback_still_reports() {
        let (_, diags) = lower_src("fn f() { let d = |x| x; }");
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unannotated lambda parameter")),
            "{diags:?}"
        );
        let (_, diags) = lower_src("enum Msg { Num(i64), } fn f() -> Msg { return Num(7); }");
        assert!(!diags.is_empty(), "unresolved construction must report");
    }
}

/// Freeze declaration facts before control-flow lowering. Source declarations
/// provide a complete fallback for checkerless tools/tests; checker entries
/// then replace them with the canonical, globally resolved definitions.
fn lower_resolution(
    program: &Program,
    tables: &CheckerTables<'_>,
) -> super::typed_ast::HirResolution {
    use super::typed_ast::{
        HirClassInfo, HirEnumInfo, HirEnumVariant, HirInterfaceInfo, HirResolution, HirSignature,
    };
    use crate::parser::ast::ParamMode;
    use crate::semantic::ids::{FunctionId, TypeId};
    let canonical = |name: &str| match tables.normalize(&Type::Named(name.to_owned())) {
        Type::Named(ref name) => TypeId::from_source_name(name),
        _ => TypeId::from_source_name(name),
    };
    let signature = |params: &[crate::parser::ast::Param],
                     result: &Type,
                     is_static,
                     is_async,
                     bound: &[String]| HirSignature {
        params: params
            .iter()
            .map(|p| tables.normalize_declared(&p.ty, bound).into())
            .collect(),
        param_modes: params.iter().map(|p| p.mode.clone()).collect(),
        return_type: tables.normalize_declared(result, bound).into(),
        is_static,
        is_async,
    };
    let mut out = HirResolution::default();
    for namespace in ["env", "fs", "net", "parallel", "f64"] {
        out.namespaces
            .insert(TypeId::local(namespace), namespace.to_owned());
    }
    for import in &program.imports {
        use crate::module::std_registry;
        if std_registry::is_std_path(&import.path) {
            if let Ok(std_registry::StdImport::Module { module }) =
                std_registry::resolve_std_import(&import.path, import.span)
            {
                if matches!(module.as_str(), "env" | "fs" | "net" | "parallel") {
                    out.namespaces.insert(
                        TypeId::local(import.alias.as_deref().unwrap_or(&module)),
                        module,
                    );
                }
            }
        } else {
            let access = import
                .alias
                .as_deref()
                .unwrap_or_else(|| import.path.rsplit("::").next().unwrap_or(&import.path));
            if access != "f64" {
                out.namespaces.remove(&TypeId::local(access));
            }
        }
    }
    // Preserve declaration order for builtin enum discriminants too.
    static PRELUDE: std::sync::OnceLock<Program> = std::sync::OnceLock::new();
    let prelude = PRELUDE.get_or_init(|| {
        let tokens = crate::lexer::Lexer::new(crate::prelude::PRELUDE_SOURCE)
            .tokenize()
            .expect("prelude lexes");
        let (prelude, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "prelude parses: {errors:?}");
        prelude
    });
    for item in prelude.items.iter().chain(&program.items) {
        match item {
            Item::Enum(e) => {
                out.enums.insert(
                    canonical(&e.name),
                    HirEnumInfo {
                        type_params: e
                            .type_params
                            .iter()
                            .map(|name| TypeId::from_source_name(name))
                            .collect(),
                        variants: e
                            .variants
                            .iter()
                            .enumerate()
                            .map(|(tag, variant)| HirEnumVariant {
                                name: variant.name.clone(),
                                tag: tag as i64,
                                payloads: variant
                                    .payload
                                    .iter()
                                    .map(|ty| tables.normalize_declared(ty, &e.type_params).into())
                                    .collect(),
                            })
                            .collect(),
                    },
                );
            }
            Item::Function(f) => {
                out.functions.insert(
                    FunctionId::free_from_source_name(&f.name),
                    signature(&f.params, &f.return_type, true, f.is_async, &[]),
                );
            }
            Item::Class(c) => {
                let mut info = HirClassInfo {
                    implements: c
                        .implements
                        .iter()
                        .map(|ty| tables.normalize(ty).into())
                        .collect(),
                    base: c.base_class.as_ref().map(|base| {
                        canonical(&match base {
                            crate::parser::ast::TypePath::Local(name) => name.clone(),
                            crate::parser::ast::TypePath::Qualified(parts) => parts.join("::"),
                        })
                    }),
                    ..HirClassInfo::default()
                };
                for field in &c.fields {
                    let ty = tables.normalize(&field.ty).into();
                    if field.is_static {
                        info.static_fields.insert(field.name.clone(), ty);
                    } else {
                        info.fields.push((field.name.clone(), ty));
                    }
                }
                info.constructor = c
                    .constructors
                    .first()
                    .map(|ctor| signature(&ctor.params, &Type::Void, false, false, &[]));
                for method in &c.methods {
                    info.methods.insert(
                        method.name.clone(),
                        signature(
                            &method.params,
                            &method.return_type,
                            method.is_static,
                            method.is_async,
                            &[],
                        ),
                    );
                }
                out.classes.insert(canonical(&c.name), info);
            }
            Item::Interface(interface) => {
                out.interfaces.insert(
                    canonical(&interface.name),
                    HirInterfaceInfo {
                        type_params: interface
                            .type_params
                            .iter()
                            .map(|name| TypeId::from_source_name(name))
                            .collect(),
                        extends: interface
                            .extends
                            .iter()
                            .map(|name| canonical(name))
                            .collect(),
                        methods: interface
                            .methods
                            .iter()
                            .map(|method| {
                                (
                                    method.name.clone(),
                                    signature(
                                        &method.params,
                                        &method.return_type,
                                        method.is_static,
                                        false,
                                        &interface
                                            .type_params
                                            .iter()
                                            .cloned()
                                            .chain(std::iter::once("Self".to_string()))
                                            .collect::<Vec<_>>(),
                                    ),
                                )
                            })
                            .collect(),
                    },
                );
            }
        }
    }
    let checked_signature = |params: &[Type],
                             infos: &[symbols::ParamInfo],
                             result: &Type,
                             is_static,
                             is_async,
                             bound: &[String]| {
        HirSignature {
            params: params
                .iter()
                .map(|ty| tables.normalize_declared(ty, bound).into())
                .collect(),
            param_modes: params
                .iter()
                .enumerate()
                .map(|(i, _)| infos.get(i).map_or(ParamMode::Value, |p| p.mode.clone()))
                .collect(),
            return_type: tables.normalize_declared(result, bound).into(),
            is_static,
            is_async,
        }
    };
    if let Some(enums) = tables.enums {
        for info in enums.values() {
            out.enums.insert(
                TypeId::from_source_name(&info.name),
                HirEnumInfo {
                    type_params: info
                        .type_params
                        .iter()
                        .map(|name| TypeId::from_source_name(name))
                        .collect(),
                    variants: info
                        .variants
                        .iter()
                        .map(|variant| HirEnumVariant {
                            name: variant.name.clone(),
                            tag: variant.tag,
                            payloads: variant
                                .payload_types
                                .iter()
                                .map(|ty| tables.normalize_declared(ty, &info.type_params).into())
                                .collect(),
                        })
                        .collect(),
                },
            );
        }
    }
    if let Some(symbols) = tables.symbols {
        for (access, module) in symbols.module_accesses() {
            let functions = module
                .functions
                .ids()
                .map(|id| {
                    let function = module
                        .functions
                        .get_id(id)
                        .expect("declared module function");
                    (
                        id.unqualified_name().to_owned(),
                        checked_signature(
                            &function.params,
                            &function.param_infos,
                            &function.return_type,
                            true,
                            function.is_async,
                            &[],
                        ),
                    )
                })
                .collect();
            out.modules
                .insert(TypeId::from_source_name(access), functions);
        }
        for (id, function) in &symbols.functions {
            out.functions.insert(
                id.clone(),
                checked_signature(
                    &function.params,
                    &function.param_infos,
                    &function.return_type,
                    true,
                    function.is_async,
                    &[],
                ),
            );
        }
        for interface in symbols.interfaces.values() {
            out.interfaces.insert(
                TypeId::from_source_name(&interface.name),
                HirInterfaceInfo {
                    type_params: interface
                        .type_params
                        .iter()
                        .map(|name| TypeId::from_source_name(name))
                        .collect(),
                    extends: interface
                        .extends
                        .iter()
                        .map(|name| canonical(name))
                        .collect(),
                    methods: interface
                        .methods
                        .iter()
                        .map(|(name, method)| {
                            (
                                name.clone(),
                                checked_signature(
                                    &method.params,
                                    &method.param_infos,
                                    &method.return_type,
                                    method.is_static,
                                    false,
                                    &interface
                                        .type_params
                                        .iter()
                                        .cloned()
                                        .chain(std::iter::once("Self".to_string()))
                                        .collect::<Vec<_>>(),
                                ),
                            )
                        })
                        .collect(),
                },
            );
        }
        for class in symbols.classes.values() {
            out.classes.insert(
                TypeId::from_source_name(&class.name),
                HirClassInfo {
                    base: class.base_class.as_ref().map(|name| canonical(name)),
                    implements: class
                        .implements
                        .iter()
                        .map(|ty| tables.normalize(ty).into())
                        .collect(),
                    fields: class
                        .instance_field_order
                        .iter()
                        .map(|(name, ty)| (name.clone(), tables.normalize(ty).into()))
                        .collect(),
                    static_fields: class
                        .static_props
                        .iter()
                        .map(|(name, field)| (name.clone(), tables.normalize(&field.ty).into()))
                        .collect(),
                    constructor: class.constructor.as_ref().map(|ctor| {
                        checked_signature(
                            &ctor.params,
                            &ctor.param_infos,
                            &Type::Void,
                            false,
                            false,
                            &[],
                        )
                    }),
                    methods: class
                        .methods
                        .iter()
                        .map(|(name, method)| {
                            (
                                name.clone(),
                                checked_signature(
                                    &method.params,
                                    &method.param_infos,
                                    &method.return_type,
                                    method.is_static,
                                    method.is_async,
                                    &[],
                                ),
                            )
                        })
                        .collect(),
                },
            );
        }
    }
    // A static property keeps its ancestor's storage while remaining visible
    // through a derived class. Preserve own-property shadowing in the snapshot.
    let declared = out.classes.clone();
    for (identity, info) in &mut out.classes {
        let mut visited = std::collections::HashSet::from([*identity]);
        let mut base = info.base;
        while let Some(parent) = base {
            if !visited.insert(parent) {
                break;
            }
            let Some(parent_info) = declared.get(&parent) else {
                break;
            };
            for (name, ty) in &parent_info.static_fields {
                info.static_fields
                    .entry(name.clone())
                    .or_insert_with(|| ty.clone());
            }
            base = parent_info.base;
        }
    }

    out
}
