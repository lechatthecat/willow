//! Lowering: type-checked AST → typed HIR ([`super::typed_ast`]) — willow-mb5.
//!
//! Coverage so far: the MVP-core constructs (literals, variables, arithmetic/
//! comparison/logical/unary operators, free-function calls, `print`, `nil`, and
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
    BinOp, Block, CallArg, CallArgMode, Expr, FunctionDecl, Item, MethodDecl, Program, Stmt, Type,
    UnaryOp,
};

use super::typed_ast::{
    HirClass, HirExpr, HirExprKind, HirFunction, HirMatchArm, HirParam, HirPattern, HirProgram,
    HirStmt,
};

/// Type-checker side tables the lowering can consume to close gaps the
/// immutable AST cannot express (willow-mb5 checker pivot, step 1). These are
/// the same tables the checker already hands to the backend.
#[derive(Default)]
pub struct CheckerTables<'a> {
    /// Each lambda's full inferred `fn(...) -> ...` type, keyed by its span —
    /// includes parameter types inferred from call-site context, so
    /// unannotated lambda parameters become typeable.
    pub lambda_fn_types: Option<&'a HashMap<Span, Type>>,
    /// Unqualified enum-variant constructions (`Ok(42)` in an expected-enum
    /// position), keyed by the call's span; the value is the resolved enum
    /// name and the variant is the call's callee.
    pub enum_variant_resolutions: Option<&'a HashMap<Span, String>>,
    /// The checker's authoritative type for every checked expression, keyed by
    /// span — the final fallback when the structural lowering cannot derive a
    /// type (generic constructions, `Self::` calls, module-qualified items).
    pub expr_types: Option<&'a HashMap<Span, Type>>,
}

impl<'a> CheckerTables<'a> {
    /// Borrow the relevant tables from a run type checker.
    pub fn from_checker(checker: &'a crate::semantic::TypeChecker) -> Self {
        Self {
            lambda_fn_types: Some(&checker.lambda_fn_types),
            enum_variant_resolutions: Some(&checker.enum_variant_resolutions),
            expr_types: Some(&checker.expr_types),
        }
    }

    fn lambda_fn_type(&self, span: &Span) -> Option<&Type> {
        self.lambda_fn_types.and_then(|m| m.get(span))
    }

    fn enum_variant_resolution(&self, span: &Span) -> Option<&String> {
        self.enum_variant_resolutions.and_then(|m| m.get(span))
    }

    fn expr_type(&self, span: &Span) -> Option<Type> {
        self.expr_types.and_then(|m| m.get(span).cloned())
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
        ("gc_allocated_bytes".to_string(), Type::I64),
        ("panic".to_string(), Type::Never),
        (
            "sleep".to_string(),
            Type::Generic("Future".to_string(), vec![Type::Void]),
        ),
        (
            "yield".to_string(),
            Type::Generic("Future".to_string(), vec![Type::Void]),
        ),
    ]);
    let mut classes = Classes::default();
    let mut enums = Enums::with_prelude();
    for item in &program.items {
        match item {
            Item::Function(f) => {
                fn_returns.insert(f.name.clone(), call_site_type(&f.return_type, f.is_async));
            }
            Item::Class(c) => {
                let mut info = ClassInfo {
                    base: c.base_class.as_ref().map(|b| b.name().to_string()),
                    ..ClassInfo::default()
                };
                for f in &c.fields {
                    if f.is_static {
                        info.static_fields.insert(f.name.clone(), f.ty.clone());
                    } else {
                        info.fields.insert(f.name.clone(), f.ty.clone());
                    }
                }
                for m in &c.methods {
                    let call_ty = call_site_type(&m.return_type, m.is_async);
                    if m.is_static {
                        info.static_methods.insert(m.name.clone(), call_ty);
                    } else {
                        info.methods.insert(m.name.clone(), call_ty);
                    }
                }
                classes.map.insert(c.name.clone(), info);
            }
            Item::Enum(e) => {
                enums.map.insert(
                    e.name.clone(),
                    EnumInfo {
                        type_params: e.type_params.clone(),
                        variants: e
                            .variants
                            .iter()
                            .map(|v| (v.name.clone(), v.payload.clone()))
                            .collect(),
                    },
                );
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
                hir_classes.push(HirClass {
                    name: c.name.clone(),
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

/// All enums in the program plus the prelude's `Option`/`Result`, used to type
/// variant constructions and to bind `match` pattern payloads.
#[derive(Default)]
struct Enums {
    map: HashMap<String, EnumInfo>,
}

impl Enums {
    fn with_prelude() -> Self {
        let mut map = HashMap::new();
        map.insert(
            "Option".to_string(),
            EnumInfo {
                type_params: vec!["T".to_string()],
                variants: HashMap::from([
                    ("Some".to_string(), vec![Type::Named("T".to_string())]),
                    ("None".to_string(), vec![]),
                ]),
            },
        );
        map.insert(
            "Result".to_string(),
            EnumInfo {
                type_params: vec!["T".to_string(), "E".to_string()],
                variants: HashMap::from([
                    ("Ok".to_string(), vec![Type::Named("T".to_string())]),
                    ("Err".to_string(), vec![Type::Named("E".to_string())]),
                ]),
            },
        );
        Self { map }
    }
}

/// Substitute symbolic type parameters (`Named(param)`) with concrete types.
fn subst_type(ty: &Type, subst: &HashMap<String, Type>) -> Type {
    match ty {
        Type::Named(name) => subst.get(name).cloned().unwrap_or_else(|| ty.clone()),
        Type::Array(inner) => Type::Array(Box::new(subst_type(inner, subst))),
        Type::Nullable(inner) => Type::Nullable(Box::new(subst_type(inner, subst))),
        Type::Generic(name, args) => Type::Generic(
            name.clone(),
            args.iter().map(|a| subst_type(a, subst)).collect(),
        ),
        Type::Fn(params, ret) => Type::Fn(
            params.iter().map(|p| subst_type(p, subst)).collect(),
            Box::new(subst_type(ret, subst)),
        ),
        _ => ty.clone(),
    }
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
        ctx.bind(p.name.clone(), p.ty.clone());
        params.push(HirParam {
            name: p.name.clone(),
            ty: p.ty.clone(),
            by_reference: !matches!(p.mode, crate::parser::ast::ParamMode::Value),
            span: p.span,
        });
    }
    let body = lower_block(&f.body, &mut ctx)?;
    Ok(HirFunction {
        name: f.name.clone(),
        params,
        return_type: f.return_type.clone(),
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
    let mut params = Vec::with_capacity(m.params.len() + 1);
    if m.has_self {
        let self_ty = Type::Named(class_name.to_string());
        ctx.bind("self".to_string(), self_ty.clone());
        params.push(HirParam {
            name: "self".to_string(),
            ty: self_ty,
            by_reference: false,
            span: m.span,
        });
    }
    for p in &m.params {
        ctx.bind(p.name.clone(), p.ty.clone());
        params.push(HirParam {
            name: p.name.clone(),
            ty: p.ty.clone(),
            by_reference: !matches!(p.mode, crate::parser::ast::ParamMode::Value),
            span: p.span,
        });
    }
    let body = lower_block(&m.body, &mut ctx)?;
    Ok(HirFunction {
        name: m.name.clone(),
        params,
        return_type: m.return_type.clone(),
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
    let self_ty = Type::Named(class_name.to_string());
    ctx.bind("self".to_string(), self_ty.clone());
    let mut params = Vec::with_capacity(ctor.params.len() + 1);
    params.push(HirParam {
        name: "self".to_string(),
        ty: self_ty,
        by_reference: false,
        span: ctor.span,
    });
    for p in &ctor.params {
        ctx.bind(p.name.clone(), p.ty.clone());
        params.push(HirParam {
            name: p.name.clone(),
            ty: p.ty.clone(),
            by_reference: !matches!(p.mode, crate::parser::ast::ParamMode::Value),
            span: p.span,
        });
    }
    let body = lower_block(&ctor.body, &mut ctx)?;
    Ok(HirFunction {
        name: "init".to_string(),
        params,
        return_type: Type::Void,
        body,
        span: ctor.span,
    })
}

/// Lowering scope: variable types (innermost-last) plus the free-function
/// return types used to type `Call` expressions.
struct LowerCtx<'a> {
    scopes: Vec<HashMap<String, Type>>,
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
            fn_returns,
            classes,
            enums,
            tables,
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn bind(&mut self, name: String, ty: Type) {
        self.scopes
            .last_mut()
            .expect("at least one scope")
            .insert(name, ty);
    }

    fn lookup_var(&self, name: &str) -> Option<Type> {
        self.scopes.iter().rev().find_map(|s| s.get(name).cloned())
    }
}

fn lower_block(block: &Block, ctx: &mut LowerCtx) -> Result<Vec<HirStmt>, Diagnostic> {
    ctx.push_scope();
    let mut out = Vec::with_capacity(block.stmts.len());
    for stmt in &block.stmts {
        out.push(lower_stmt(stmt, ctx)?);
    }
    ctx.pop_scope();
    Ok(out)
}

fn lower_stmt(stmt: &Stmt, ctx: &mut LowerCtx) -> Result<HirStmt, Diagnostic> {
    match stmt {
        Stmt::Let(l) => {
            let value = lower_expr(&l.init, ctx)?;
            // A `let x: T = ..` annotation pins the binding type; otherwise the
            // type flows from the value expression.
            let binding_ty = l.ty.clone().unwrap_or_else(|| value.ty.clone());
            ctx.bind(l.name.clone(), binding_ty);
            Ok(HirStmt::Let {
                name: l.name.clone(),
                mutable: l.mutable,
                value,
                span: l.span,
            })
        }
        Stmt::Assign(a) => {
            let value = lower_expr(&a.value, ctx)?;
            Ok(HirStmt::Assign {
                name: a.name.clone(),
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
            Ok(HirStmt::StaticFieldAssign {
                class: s.class.clone(),
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
                    if name == "Range" && args.first() == Some(&Type::I64) =>
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
            ctx.bind(s.name.clone(), element_ty);
            let body = lower_block(&s.body, ctx)?;
            ctx.pop_scope();
            Ok(HirStmt::For {
                name: s.name.clone(),
                iterable,
                body,
                span: s.span,
            })
        }
    }
}

fn lower_expr(expr: &Expr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    match expr {
        Expr::Integer(n, span) => Ok(lit(HirExprKind::Int(*n), Type::I64, *span)),
        Expr::Float(f, span) => Ok(lit(HirExprKind::Float(*f), Type::F64, *span)),
        Expr::Bool(b, span) => Ok(lit(HirExprKind::Bool(*b), Type::Bool, *span)),
        Expr::String(s, span) => Ok(lit(HirExprKind::Str(s.clone()), Type::String, *span)),
        Expr::Var(name, span) => {
            if let Some(ty) = ctx.lookup_var(name) {
                return Ok(HirExpr {
                    kind: HirExprKind::Var(name.clone()),
                    ty,
                    span: *span,
                });
            }
            // A bare fieldless unqualified variant (`None`, `Halt`) parses as a
            // variable; the checker resolves it against the expected enum and
            // records both the enum and the expression type.
            if let Some(enum_name) = ctx.tables.enum_variant_resolution(span) {
                let ty = ctx
                    .tables
                    .expr_type(span)
                    .unwrap_or_else(|| Type::Named(enum_name.clone()));
                return Ok(HirExpr {
                    kind: HirExprKind::StaticField {
                        class: enum_name.clone(),
                        field: name.clone(),
                    },
                    ty,
                    span: *span,
                });
            }
            Err(internal(
                *span,
                format!("unbound variable `{name}` reached HIR lowering"),
            ))
        }
        Expr::Binary(b) => {
            let lhs = lower_expr(&b.lhs, ctx)?;
            let rhs = lower_expr(&b.rhs, ctx)?;
            let ty = binary_result_type(&b.op, &lhs.ty);
            Ok(HirExpr {
                kind: HirExprKind::Binary {
                    op: b.op.clone(),
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                ty,
                span: b.span,
            })
        }
        Expr::Unary(u) => {
            let operand = lower_expr(&u.expr, ctx)?;
            let ty = match u.op {
                UnaryOp::Neg => operand.ty.clone(),
                UnaryOp::Not => Type::Bool,
            };
            Ok(HirExpr {
                kind: HirExprKind::Unary {
                    op: u.op.clone(),
                    operand: Box::new(operand),
                },
                ty,
                span: u.span,
            })
        }
        Expr::Call(c) => {
            let args = lower_value_args(&c.args, ctx)?;
            // An unqualified enum-variant construction (`Ok(42)` in an
            // expected-enum position) parses as a call; the checker records
            // which enum it resolved to (willow-60o.1).
            if let Some(enum_name) = ctx.tables.enum_variant_resolution(&c.span) {
                // Prefer the checker's recorded type (it carries generic type
                // arguments, e.g. `Result<i64, String>` for `Ok(42)`).
                let ty = match ctx.tables.expr_type(&c.span) {
                    Some(ty) => ty,
                    None => enum_variant_construction_type(ctx.enums, enum_name, &args, c.span)?,
                };
                return Ok(HirExpr {
                    kind: HirExprKind::StaticCall {
                        class: enum_name.clone(),
                        method: c.callee.clone(),
                        args,
                    },
                    ty,
                    span: c.span,
                });
            }
            // A local fn-typed variable shadows a free function; its call is an
            // indirect call typed by the variable's `fn(..) -> R`.
            let indirect = ctx.lookup_var(&c.callee).and_then(|ty| match ty {
                Type::Fn(_, ret) => Some((*ret).clone()),
                _ => None,
            });
            let ty = indirect
                .or_else(|| ctx.fn_returns.get(&c.callee).cloned())
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
                    callee: c.callee.clone(),
                    args,
                },
                ty,
                span: c.span,
            })
        }
        Expr::Print(inner, newline, span) => {
            let value = lower_expr(inner, ctx)?;
            Ok(HirExpr {
                kind: HirExprKind::Print {
                    value: Box::new(value),
                    newline: *newline,
                },
                ty: Type::Void,
                span: *span,
            })
        }
        Expr::ArrayLiteral(elements, span) => {
            if elements.is_empty() {
                // An empty literal's element type comes from context the lowering
                // does not yet thread through (willow-mb5).
                return Err(unsupported(*span, "empty array literal"));
            }
            let mut lowered = Vec::with_capacity(elements.len());
            for element in elements {
                lowered.push(lower_expr(element, ctx)?);
            }
            let element_ty = lowered[0].ty.clone();
            Ok(HirExpr {
                kind: HirExprKind::Array { elements: lowered },
                ty: Type::Array(Box::new(element_ty)),
                span: *span,
            })
        }
        Expr::Index(array, index, span) => {
            let array = lower_expr(array, ctx)?;
            let index = lower_expr(index, ctx)?;
            let Type::Array(element) = &array.ty else {
                return Err(unsupported(*span, "index of a non-array value"));
            };
            let ty = (**element).clone();
            Ok(HirExpr {
                kind: HirExprKind::Index {
                    array: Box::new(array),
                    index: Box::new(index),
                },
                ty,
                span: *span,
            })
        }
        Expr::Ternary(t) => {
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
        Expr::New(n) => {
            let args = lower_value_args(&n.args, ctx)?;
            Ok(HirExpr {
                kind: HirExprKind::New {
                    class: n.class_name.clone(),
                    args,
                },
                ty: Type::Named(n.class_name.clone()),
                span: n.span,
            })
        }
        Expr::FieldAccess(object, field, span) => {
            let object = lower_expr(object, ctx)?;
            let ty = {
                match class_name_of(&object.ty)
                    .and_then(|class| ctx.classes.field_type(class, field))
                {
                    Some(ty) => ty,
                    None => ctx
                        .tables
                        .expr_type(span)
                        .ok_or_else(|| unsupported(*span, "field not found on receiver"))?,
                }
            };
            Ok(HirExpr {
                kind: HirExprKind::FieldAccess {
                    object: Box::new(object),
                    field: field.clone(),
                },
                ty,
                span: *span,
            })
        }
        Expr::MethodCall(m) => {
            let object = lower_expr(&m.object, ctx)?;
            let ty = if let Some(ty) = builtin_method_type(&object.ty, &m.method) {
                ty
            } else if let Some(ty) = class_name_of(&object.ty)
                .and_then(|class| ctx.classes.method_type(class, &m.method))
            {
                ty
            } else {
                // Checker authority: interface methods, generic receivers,
                // Option/Result methods, and anything else it typed.
                ctx.tables
                    .expr_type(&m.span)
                    .ok_or_else(|| unsupported(m.span, "method not found on receiver"))?
            };
            let args = lower_value_args(&m.args, ctx)?;
            Ok(HirExpr {
                kind: HirExprKind::MethodCall {
                    object: Box::new(object),
                    method: m.method.clone(),
                    args,
                },
                ty,
                span: m.span,
            })
        }
        Expr::ObjectLiteral(o) => {
            let mut fields = Vec::with_capacity(o.fields.len());
            for f in &o.fields {
                fields.push((f.name.clone(), lower_expr(&f.value, ctx)?));
            }
            Ok(HirExpr {
                kind: HirExprKind::ObjectLiteral {
                    class: o.class.clone(),
                    fields,
                },
                ty: Type::Named(o.class.clone()),
                span: o.span,
            })
        }
        Expr::Nil(span) => Ok(lit(HirExprKind::Nil, Type::Nil, *span)),
        Expr::StaticField(s) => {
            // `Enum::Variant` (fieldless) parses like a static property read.
            let variant_ty = enum_variant_value_type(ctx.enums, &s.class, &s.field, true);
            let ty = variant_ty
                .or_else(|| ctx.classes.static_field_type(&s.class, &s.field))
                .ok_or_else(|| unsupported(s.span, "static property not found"))?;
            Ok(HirExpr {
                kind: HirExprKind::StaticField {
                    class: s.class.clone(),
                    field: s.field.clone(),
                },
                ty,
                span: s.span,
            })
        }
        Expr::StaticCall(s) => {
            // `Enum::Variant(args)` construction parses like a static call.
            let variant_ty = enum_variant_value_type(ctx.enums, &s.class, &s.method, false);
            let ty = variant_ty
                .or_else(|| ctx.classes.static_method_type(&s.class, &s.method))
                // Checker authority: generic-enum construction, `Self::`,
                // module-qualified statics, constructors.
                .or_else(|| ctx.tables.expr_type(&s.span))
                .ok_or_else(|| {
                    unsupported(
                        s.span,
                        "static method not found (and no checker-recorded type)",
                    )
                })?;
            let args = lower_value_args(&s.args, ctx)?;
            Ok(HirExpr {
                kind: HirExprKind::StaticCall {
                    class: s.class.clone(),
                    method: s.method.clone(),
                    args,
                },
                ty,
                span: s.span,
            })
        }
        Expr::Range(r) => {
            let start = lower_expr(&r.start, ctx)?;
            let end = lower_expr(&r.end, ctx)?;
            Ok(HirExpr {
                kind: HirExprKind::Range {
                    start: Box::new(start),
                    end: Box::new(end),
                },
                ty: Type::Generic("Range".to_string(), vec![Type::I64]),
                span: r.span,
            })
        }
        Expr::Await(a) => {
            let inner = lower_expr(&a.expr, ctx)?;
            let ty = match &inner.ty {
                Type::Generic(name, args)
                    if (name == "Task" || name == "Future") && args.len() == 1 =>
                {
                    args[0].clone()
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
        Expr::TryPropagate(inner, span) => {
            let inner = lower_expr(inner, ctx)?;
            let ty = match &inner.ty {
                Type::Generic(name, args)
                    if (name == "Result" || name == "Option") && !args.is_empty() =>
                {
                    args[0].clone()
                }
                _ => return Err(unsupported(*span, "`?` on a non-Result/Option value")),
            };
            Ok(HirExpr {
                kind: HirExprKind::TryPropagate {
                    inner: Box::new(inner),
                },
                ty,
                span: *span,
            })
        }
        Expr::Lambda(l) => {
            // The checker's inferred full `fn(...) -> ...` type fills in what
            // the AST cannot store: unannotated parameter types and, for
            // block-bodied lambdas, the inferred return type.
            let inferred = match ctx.tables.lambda_fn_type(&l.span) {
                Some(Type::Fn(params, ret)) => Some((params.clone(), (**ret).clone())),
                _ => None,
            };
            let mut params = Vec::with_capacity(l.params.len());
            let mut param_tys = Vec::with_capacity(l.params.len());
            ctx.push_scope();
            for (i, p) in l.params.iter().enumerate() {
                let inferred_param = inferred
                    .as_ref()
                    .and_then(|(params, _)| params.get(i).cloned());
                let Some(ty) = p.ty.clone().or(inferred_param) else {
                    ctx.pop_scope();
                    return Err(unsupported(p.span, "unannotated lambda parameter"));
                };
                ctx.bind(p.name.clone(), ty.clone());
                param_tys.push(ty.clone());
                params.push(HirParam {
                    name: p.name.clone(),
                    ty,
                    by_reference: false,
                    span: p.span,
                });
            }
            let inferred_ret = inferred.map(|(_, ret)| ret);
            let (body, ret) = match &l.body {
                crate::parser::ast::LambdaBody::Expr(e) => {
                    let value = lower_expr(e, ctx)?;
                    let ret = l
                        .return_type
                        .clone()
                        .or(inferred_ret)
                        .unwrap_or_else(|| value.ty.clone());
                    let span = value.span;
                    (
                        vec![HirStmt::Return {
                            value: Some(value),
                            span,
                        }],
                        ret,
                    )
                }
                crate::parser::ast::LambdaBody::Block(block) => {
                    let Some(ret) = l.return_type.clone().or(inferred_ret) else {
                        ctx.pop_scope();
                        return Err(unsupported(
                            l.span,
                            "block-bodied lambda without a return type annotation",
                        ));
                    };
                    (lower_block(block, ctx)?, ret)
                }
            };
            ctx.pop_scope();
            Ok(HirExpr {
                kind: HirExprKind::Lambda { params, body },
                ty: Type::Fn(param_tys, Box::new(ret)),
                span: l.span,
            })
        }
        Expr::Match(m) => lower_match(m, ctx),
        other => Err(unsupported(other.span(), "expression form")),
    }
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
fn lower_match(
    m: &crate::parser::ast::MatchExpr,
    ctx: &mut LowerCtx,
) -> Result<HirExpr, Diagnostic> {
    use crate::parser::ast::{MatchBody, Pattern};

    let scrutinee = lower_expr(&m.scrutinee, ctx)?;

    // The scrutinee's enum context: its EnumInfo plus the substitution from the
    // enum's type parameters to the scrutinee's type arguments.
    let enum_context: Option<(String, EnumInfo, HashMap<String, Type>)> = match &scrutinee.ty {
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
        // Normalize parse-level shapes: an unqualified `Some(x)` parses as a
        // class downcast and a bare `None` as a binding; if the name matches a
        // variant of the scrutinee's enum, it is that variant (the checker's
        // pattern_resolutions does the same).
        let pattern = match &arm.pattern {
            Pattern::Wildcard(_) => HirPattern::Wildcard,
            Pattern::LiteralBool(b, _) => HirPattern::LiteralBool(*b),
            Pattern::LiteralInt(n, _) => HirPattern::LiteralInt(*n),
            Pattern::Binding { name, .. } => {
                if let Some((enum_name, info, _)) = &enum_context
                    && info.variants.get(name).is_some_and(Vec::is_empty)
                {
                    HirPattern::EnumVariant {
                        enum_name: enum_name.clone(),
                        variant: name.clone(),
                    }
                } else {
                    ctx.bind(name.clone(), scrutinee.ty.clone());
                    HirPattern::Binding {
                        name: name.clone(),
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
                    enum_name: enum_name.clone(),
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
                if enum_context
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
                    if binding != "_" {
                        ctx.bind(binding.clone(), Type::Named(class_name.clone()));
                    }
                    HirPattern::ClassDowncast {
                        class_name: class_name.clone(),
                        binding: binding.clone(),
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
        if name != "_" {
            ctx.bind(name.clone(), ty.clone());
        }
        typed.push((name.clone(), ty));
    }
    Ok(HirPattern::EnumVariantTuple {
        enum_name: enum_name.clone(),
        variant: variant.to_string(),
        bindings: typed,
    })
}

/// Lower call/constructor value arguments (reference arguments are not yet
/// covered by the HIR).
fn lower_value_args(args: &[CallArg], ctx: &mut LowerCtx) -> Result<Vec<HirExpr>, Diagnostic> {
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        if arg.mode != CallArgMode::Value {
            return Err(unsupported(arg.span, "reference call argument"));
        }
        out.push(lower_expr(&arg.expr, ctx)?);
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
            "push" => Some(Type::Void),
            "pop" => Some((**elem).clone()),
            "freeze" => Some(Type::Generic(
                "FrozenArray".to_string(),
                vec![(**elem).clone()],
            )),
            _ => None,
        },
        Type::Generic(name, args) => match (name.as_str(), args.as_slice(), method) {
            ("Map", [k, v], _) => match method {
                "insert" => Some(Type::Void),
                "get" => Some(Type::Generic("Option".to_string(), vec![v.clone()])),
                "contains" => Some(Type::Bool),
                "len" => Some(Type::I64),
                "freeze" => Some(Type::Generic(
                    "FrozenMap".to_string(),
                    vec![k.clone(), v.clone()],
                )),
                _ => None,
            },
            ("FrozenArray", [_], "len") | ("FrozenMap", [_, _], "len") => Some(Type::I64),
            ("FrozenMap", [_, v], "get") => {
                Some(Type::Generic("Option".to_string(), vec![v.clone()]))
            }
            ("FrozenMap", [_, _], "contains") => Some(Type::Bool),
            // Both a spawned JoinHandle<T> and an async call's Task<T> join to T.
            ("Task" | "JoinHandle", [t], "join") => Some(t.clone()),
            ("Mutex" | "RwLock", [t], "get" | "read") => Some(t.clone()),
            ("Mutex" | "RwLock", [_], "set" | "write") => Some(Type::Void),
            _ => None,
        },
        _ => None,
    }
}

fn binary_result_type(op: &BinOp, lhs_ty: &Type) -> Type {
    match op {
        // Arithmetic preserves the (already type-checked) operand type.
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => lhs_ty.clone(),
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
    HirExpr { kind, ty, span }
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
        first_return(&body).ty.clone()
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
            .find(|fun| fun.name == "f")
            .expect("function f");
        match &first_return(&f.body).kind {
            HirExprKind::Call { callee, args } => {
                assert_eq!(callee, "add");
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
        assert_eq!(f.name, "f");
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
                assert_eq!(value.ty, Type::Named("Box".to_string()));
                assert!(matches!(&value.kind, HirExprKind::New { class, .. } if class == "Box"));
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
        assert_eq!(class.name, "Box");
        assert_eq!(class.methods.len(), 1);
        let m = &class.methods[0];
        assert_eq!(m.name, "get");
        assert_eq!(m.params[0].name, "self");
        assert_eq!(m.params[0].ty, Type::Named("Box".to_string()));
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
                assert_eq!(value.ty, Type::Named("P".to_string()));
            }
            other => panic!("expected let, got {other:?}"),
        }
    }

    // 52. `nil` lowers to the nil type
    #[test]
    fn p52_nil_type() {
        let body = lower_body("fn f() { let x = nil; }");
        match &body[0] {
            HirStmt::Let { value, .. } => assert_eq!(value.ty, Type::Nil),
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
        let body = &hir.functions[0].body;
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
                assert_eq!(
                    value.ty,
                    Type::Generic("Range".to_string(), vec![Type::I64])
                );
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
        let f = hir.functions.iter().find(|x| x.name == "f").unwrap();
        let body = &f.body;
        match &body[0] {
            HirStmt::Let { value, .. } => {
                assert_eq!(value.ty, Type::Generic("Task".to_string(), vec![Type::I64]));
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
        let f = hir.functions.iter().find(|x| x.name == "f").unwrap();
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
        let b = hir.classes.iter().find(|c| c.name == "B").unwrap();
        let init = &b.methods[0];
        assert_eq!(init.name, "init");
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
        assert_eq!(return_ty(src), Type::Named("Shape".to_string()));
    }

    // 73. a fieldless variant value read has the enum type
    #[test]
    fn p73_fieldless_variant_value() {
        let src = "enum Color { Red, Blue, } fn f() -> Color { return Color::Red; }";
        assert_eq!(return_ty(src), Type::Named("Color".to_string()));
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
                    Type::Generic("FrozenArray".to_string(), vec![Type::I64])
                );
            }
            other => panic!("expected let, got {other:?}"),
        }
    }

    // 75. builtin map/task/lock methods: get/contains/len/join/lock types
    #[test]
    fn p75_map_task_lock_builtin_methods() {
        assert_eq!(
            return_ty("fn f(m: Map<String, i64>) -> bool { return m.contains(\"k\"); }"),
            Type::Bool
        );
        let body = lower_body("fn f(m: Map<String, i64>) { let v = m.get(\"k\"); }");
        match &body[0] {
            HirStmt::Let { value, .. } => {
                assert_eq!(
                    value.ty,
                    Type::Generic("Option".to_string(), vec![Type::I64])
                );
            }
            other => panic!("expected let, got {other:?}"),
        }
        assert_eq!(
            return_ty(
                "async fn g() -> i64 { return 1; } fn f() -> i64 { let t = g(); return t.join(); }"
            ),
            Type::I64
        );
        assert_eq!(
            return_ty("fn f(m: Mutex<i64>) -> i64 { return m.get(); }"),
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
        let g = hir.functions.iter().find(|f| f.name == "g").unwrap();
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
        assert_eq!(first_return(&f.body).ty, Type::Named("Msg".to_string()));
        assert!(matches!(
            &first_return(&f.body).kind,
            HirExprKind::StaticCall { class, method, .. } if class == "Msg" && method == "Num"
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
        let expected = Type::Generic("Result".to_string(), vec![Type::I64, Type::String]);
        assert_eq!(first_return(&f.body).ty, expected);
    }

    // 80. a bare fieldless `None` resolves to Option<T> via the checker tables
    #[test]
    fn p80_bare_none_via_checker_tables() {
        let src = "fn f() -> Option<i64> { return None; }";
        let (hir, diags) = lower_with_checker(src);
        assert!(diags.is_empty(), "{diags:?}");
        let ret = first_return(&hir.functions[0].body);
        assert_eq!(ret.ty, Type::Generic("Option".to_string(), vec![Type::I64]));
        assert!(matches!(
            &ret.kind,
            HirExprKind::StaticField { class, field } if class == "Option" && field == "None"
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
        let f = hir.functions.iter().find(|x| x.name == "f").unwrap();
        assert_eq!(first_return(&f.body).ty, Type::I64);
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
