//! Typed high-level IR (HIR) — willow-mb5.
//!
//! The compiler pipeline is meant to be `AST → Typed AST → Lowered IR →
//! Cranelift IR`, but today the backend consumes the raw AST and re-derives
//! types (via `ast_type_of_expr`) and looks them up by `Span`. This module is
//! the first step toward fixing that: a typed IR where **every expression
//! carries its resolved [`Type`]**, so a consumer reads the type instead of
//! recomputing it.
//!
//! The node set covers most of the language: literals, variables, operators,
//! calls (free, method, static, indirect, builtin), `print`, arrays/indexing,
//! ternaries, ranges, classes (`new`, object literals, field access, method
//! bodies, constructors with `super.init`, static members, inheritance), enums
//! (variant construction and `match` with typed pattern bindings, including
//! `Option`/`Result` substitution), builtin collection/concurrency methods
//! (`Array`/`Map`/Task await/locks), array and range `for` loops, all
//! assignment forms, `await`, `?` propagation, and annotated lambdas. General
//! generic substitution and unannotated-lambda inference are future work, as is
//! the control-flow → basic-block lowering (`lowered.rs`). The backend is not
//! yet wired to consume this IR, so behavior is unchanged.

use crate::diagnostics::Span;
use crate::parser::ast::{BinOp, LockMode, Type, UnaryOp};

/// A whole program lowered to typed HIR. Slice 1 only carries free functions.
#[derive(Debug, Clone, PartialEq)]
pub struct HirProgram {
    pub functions: Vec<HirFunction>,
    pub classes: Vec<HirClass>,
}

/// A class and its lowered methods. Each method is a [`HirFunction`] whose first
/// parameter is the receiver `self` (typed as the class) when present.
#[derive(Debug, Clone, PartialEq)]
pub struct HirClass {
    pub name: String,
    pub methods: Vec<HirFunction>,
    pub span: Span,
}

/// A free function (or class method) with typed parameters, a declared return
/// type, and a typed statement body.
#[derive(Debug, Clone, PartialEq)]
pub struct HirFunction {
    pub name: String,
    /// Whether this body is compiled as a cooperative poll state machine.
    pub is_async: bool,
    pub params: Vec<HirParam>,
    pub return_type: Type,
    pub body: Vec<HirStmt>,
    pub span: Span,
}

/// A function parameter and its declared type. `by_reference` is true for
/// `&`/`&mut` parameters (pointers at the ABI level), so consumers never need
/// to reach back into the AST for the parameter mode.
#[derive(Debug, Clone, PartialEq)]
pub struct HirParam {
    pub name: String,
    pub ty: Type,
    pub by_reference: bool,
    pub span: Span,
}

/// One value a lambda body reads from the enclosing function, lifted into the
/// closure environment (willow-0g8j.2.12).
///
/// The two names are the SAME source variable seen from the two sides of the
/// lift: `source` is what the enclosing function calls it, `name` is what the
/// lifted body calls it. They differ whenever shadowing renamed one of them,
/// because the lifted body restarts the flat namespace (willow-0g8j.2.10).
#[derive(Debug, Clone, PartialEq)]
pub struct HirCapture {
    /// The binding's name inside the lifted lambda body.
    pub name: String,
    /// The binding's name in the enclosing function, which is where the value
    /// is read from when the environment is built.
    pub source: String,
    pub ty: Type,
}

/// What a `defer` runs when its scope exits (willow-0g8j.2.3).
///
/// The two arms mirror the source forms and differ in SCOPE, not just shape: a
/// block body owns its own bindings, while an expression body is evaluated in
/// the scope that registered it.
#[derive(Debug, Clone, PartialEq)]
pub enum HirDeferBody {
    /// `defer f(x);`, `defer obj.close();`, `defer print(x);` and
    /// `defer match recover() { .. }` — one expression, run for its effect.
    Expr(HirExpr),
    /// `defer { .. }` — a statement list with its own scope.
    Block(Vec<HirStmt>),
}

/// A statement in typed HIR. Control flow keeps its high-level shape here; the
/// basic-block lowering happens in a later slice.
#[derive(Debug, Clone, PartialEq)]
pub enum HirStmt {
    /// `let [mut] name: ty = value;` — `ty` is the type the name is *bound*
    /// with: the annotation when written, otherwise the initialiser's inferred
    /// type. The two differ whenever the annotation is a supertype of the
    /// initialiser (`let a: Animal = new Dog();`), so a consumer that stores or
    /// reloads the variable must use `ty`, not `value.ty` (willow-0g8j.5).
    Let {
        name: String,
        mutable: bool,
        ty: Type,
        value: HirExpr,
        span: Span,
    },
    /// `name = value;`
    Assign {
        name: String,
        value: HirExpr,
        span: Span,
    },
    /// `if cond { .. } else { .. }` — `cond` is always `Bool`.
    If {
        cond: HirExpr,
        then_branch: Vec<HirStmt>,
        else_branch: Option<Vec<HirStmt>>,
        span: Span,
    },
    /// `while cond { .. }` — `cond` is always `Bool`.
    While {
        cond: HirExpr,
        body: Vec<HirStmt>,
        span: Span,
    },
    /// `return [value];`
    Return { value: Option<HirExpr>, span: Span },
    /// `break;` — exit the innermost loop (willow-kzka).
    Break { span: Span },
    /// `continue;` — next iteration of the innermost loop.
    Continue { span: Span },
    /// `defer <body>;` — scope-exit cleanup (willow-vynv.2). `span` is the
    /// `defer` statement's own span, which is the key the backend registers
    /// the site's cleanup flag under (willow-0g8j.2.3).
    Defer { body: HirDeferBody, span: Span },
    Lock {
        mode: LockMode,
        target: HirExpr,
        binding: String,
        mutable: bool,
        body: Vec<HirStmt>,
        span: Span,
    },
    /// A bare expression evaluated for its effect.
    Expr(HirExpr),
    /// `for name in iterable { .. }`; `iterable` is an array or range.
    For {
        name: String,
        iterable: HirExpr,
        body: Vec<HirStmt>,
        span: Span,
    },
    /// `object.field = value;`
    FieldAssign {
        object: HirExpr,
        field: String,
        value: HirExpr,
        span: Span,
    },
    /// `array[index] = value;`
    IndexAssign {
        array: HirExpr,
        index: HirExpr,
        value: HirExpr,
        span: Span,
    },
    /// `Class::field = value;`
    StaticFieldAssign {
        class: String,
        field: String,
        value: HirExpr,
        span: Span,
    },
    /// `super.init(args);` — base-class construction inside an `init` body.
    SuperInit { args: Vec<HirExpr>, span: Span },
}

/// A typed expression: a [`HirExprKind`] plus its resolved [`Type`].
#[derive(Debug, Clone, PartialEq)]
pub struct HirExpr {
    pub kind: HirExprKind,
    pub ty: Type,
    pub span: Span,
}

impl HirExpr {
    /// The resolved type of this expression. The whole point of the HIR: a
    /// consumer reads this instead of re-deriving the type from the AST.
    pub fn ty(&self) -> &Type {
        &self.ty
    }

    /// Every sub-expression one level down, in evaluation order.
    ///
    /// The match is exhaustive on purpose: a new [`HirExprKind`] has to be
    /// threaded through here or this stops compiling, so a consumer that walks
    /// the tree cannot silently miss a node kind (the same discipline as
    /// `parser::visit`).
    ///
    /// Two kinds carry STATEMENT bodies that introduce their own bindings —
    /// [`HirExprKind::Lambda`] (its parameters) and [`HirExprKind::Match`] (each
    /// arm's pattern). Their expressions are included here because this is a
    /// structural walk, but a consumer that tracks names in scope must handle
    /// those two kinds itself rather than treating the extra children as
    /// siblings of the rest.
    pub fn children(&self) -> Vec<&HirExpr> {
        match &self.kind {
            HirExprKind::Int(_)
            | HirExprKind::Float(_)
            | HirExprKind::Bool(_)
            | HirExprKind::Str(_)
            | HirExprKind::Var(_)
            | HirExprKind::FnRef(_)
            | HirExprKind::StaticField { .. } => Vec::new(),
            HirExprKind::Binary { lhs, rhs, .. } => vec![lhs, rhs],
            HirExprKind::Unary { operand, .. } => vec![operand],
            HirExprKind::Call { args, .. }
            | HirExprKind::New { args, .. }
            | HirExprKind::StaticCall { args, .. } => args.iter().collect(),
            HirExprKind::Print { value, .. } => vec![value],
            HirExprKind::Array { elements } => elements.iter().collect(),
            HirExprKind::Index { array, index } => vec![array, index],
            HirExprKind::Ternary {
                condition,
                then_expr,
                else_expr,
            } => vec![condition, then_expr, else_expr],
            HirExprKind::FieldAccess { object, .. } => vec![object],
            HirExprKind::MethodCall { object, args, .. } => {
                let mut out = vec![&**object];
                out.extend(args);
                out
            }
            HirExprKind::ObjectLiteral { fields, .. } => fields.iter().map(|(_, e)| e).collect(),
            HirExprKind::ReferenceArg { place } => vec![place],
            HirExprKind::Range { start, end } => vec![start, end],
            HirExprKind::Await { inner } | HirExprKind::TryPropagate { inner } => vec![inner],
            HirExprKind::Lambda { body, .. } => nested_exprs(body),
            HirExprKind::Match { scrutinee, arms } => {
                let mut out = vec![&**scrutinee];
                for arm in arms {
                    out.extend(nested_exprs(&arm.body));
                }
                out
            }
            HirExprKind::Select { cases } => {
                let mut out = Vec::new();
                for case in cases {
                    match &case.kind {
                        HirSelectCaseKind::Recv { channel, .. } => out.push(channel),
                        HirSelectCaseKind::Send { channel, value } => {
                            out.push(channel);
                            out.push(value);
                        }
                        HirSelectCaseKind::Timeout { millis } => out.push(millis),
                        HirSelectCaseKind::Join { task, .. } => out.push(task),
                        HirSelectCaseKind::Default => {}
                    }
                    out.extend(nested_exprs(&case.body));
                }
                out
            }
        }
    }
}

/// Every expression inside a statement body, flattened. Free rather than a
/// closure so the returned borrows keep the body's lifetime instead of the
/// caller's frame.
fn nested_exprs(body: &[HirStmt]) -> Vec<&HirExpr> {
    body.iter().flat_map(|stmt| stmt.child_exprs()).collect()
}

impl HirStmt {
    /// Every expression this statement holds directly, including those inside
    /// its nested statement bodies. Exhaustive for the same reason as
    /// [`HirExpr::children`].
    pub fn child_exprs(&self) -> Vec<&HirExpr> {
        match self {
            HirStmt::Break { .. } | HirStmt::Continue { .. } => Vec::new(),
            HirStmt::Let { value, .. } | HirStmt::Assign { value, .. } => vec![value],
            HirStmt::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                let mut out = vec![cond];
                out.extend(nested_exprs(then_branch));
                if let Some(else_branch) = else_branch {
                    out.extend(nested_exprs(else_branch));
                }
                out
            }
            HirStmt::While { cond, body, .. } => {
                let mut out = vec![cond];
                out.extend(nested_exprs(body));
                out
            }
            HirStmt::Return { value, .. } => value.iter().collect(),
            HirStmt::Defer { body, .. } => match body {
                HirDeferBody::Expr(e) => vec![e],
                HirDeferBody::Block(stmts) => nested_exprs(stmts),
            },
            HirStmt::Lock { target, body, .. } => {
                let mut out = vec![target];
                out.extend(nested_exprs(body));
                out
            }
            HirStmt::Expr(e) => vec![e],
            HirStmt::For { iterable, body, .. } => {
                let mut out = vec![iterable];
                out.extend(nested_exprs(body));
                out
            }
            HirStmt::FieldAssign { object, value, .. } => vec![object, value],
            HirStmt::IndexAssign {
                array,
                index,
                value,
                ..
            } => vec![array, index, value],
            HirStmt::StaticFieldAssign { value, .. } => vec![value],
            HirStmt::SuperInit { args, .. } => args.iter().collect(),
        }
    }
}

/// The expression forms covered by slice 1.
#[derive(Debug, Clone, PartialEq)]
pub enum HirExprKind {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
    /// A variable read; its [`HirExpr::ty`] is the type it was bound with.
    Var(String),
    /// A named top-level function used as a VALUE rather than called —
    /// `apply(10, double)` (willow-0g8j.2.2). Spelled as a bare identifier in
    /// source, so it is only distinguishable from [`HirExprKind::Var`] by what
    /// the name resolves to; `ty` is the function's `fn(...) -> ...` type.
    FnRef(String),
    Binary {
        op: BinOp,
        lhs: Box<HirExpr>,
        rhs: Box<HirExpr>,
    },
    Unary {
        op: UnaryOp,
        operand: Box<HirExpr>,
    },
    /// A free-function call; `ty` is the callee's return type.
    Call {
        callee: String,
        args: Vec<HirExpr>,
    },
    /// `print(value)` / `println(value)`; always `Void`.
    Print {
        value: Box<HirExpr>,
        newline: bool,
    },
    /// `[e0, e1, ...]` array literal; `ty` is `Array<element>`.
    Array {
        elements: Vec<HirExpr>,
    },
    /// `array[index]`; `ty` is the array's element type.
    Index {
        array: Box<HirExpr>,
        index: Box<HirExpr>,
    },
    /// `cond ? then : else`; `ty` is the shared branch type.
    Ternary {
        condition: Box<HirExpr>,
        then_expr: Box<HirExpr>,
        else_expr: Box<HirExpr>,
    },
    /// `new Class(args)`; `ty` is the class type.
    New {
        class: String,
        args: Vec<HirExpr>,
    },
    /// `object.field`; `ty` is the field's declared type.
    FieldAccess {
        object: Box<HirExpr>,
        field: String,
    },
    /// `object.method(args)`; `ty` is the method's return type.
    MethodCall {
        object: Box<HirExpr>,
        method: String,
        args: Vec<HirExpr>,
    },
    /// `Class { field: value, ... }` object literal; `ty` is the class type.
    ObjectLiteral {
        class: String,
        fields: Vec<(String, HirExpr)>,
    },
    /// `Class::field` static property read; `ty` is the property's type.
    StaticField {
        class: String,
        field: String,
    },
    /// `Class::method(args)` static call; `ty` is the static method's return type.
    StaticCall {
        class: String,
        method: String,
        args: Vec<HirExpr>,
    },
    /// `&place` / `&mut place` in a call argument. `ty` remains the place's
    /// value type; codegen pairs this marker with the callee's `ParamMode` and
    /// passes the place address through the pointer ABI.
    ReferenceArg {
        place: Box<HirExpr>,
    },
    /// `start..end` half-open i64 range; `ty` is `Range<i64>`.
    Range {
        start: Box<HirExpr>,
        end: Box<HirExpr>,
    },
    /// `await task`; `ty` is the `T` of the awaited `Task<T>`/`Future<T>`.
    Await {
        inner: Box<HirExpr>,
    },
    /// `expr?`; `ty` is the success `T` of the inner `Result<T, E>`/`Option<T>`.
    TryPropagate {
        inner: Box<HirExpr>,
    },
    /// `|params| body` lambda; `ty` is `fn(params) -> ret` when it captures
    /// nothing and `closure(params) -> ret` when it does. An expression body is
    /// represented as a single `Return` statement.
    Lambda {
        params: Vec<HirParam>,
        /// What the body reads from the enclosing function, in environment-slot
        /// order (willow-0g8j.2.12). Empty for a `fn`-typed lambda.
        captures: Vec<HirCapture>,
        body: Vec<HirStmt>,
    },
    /// `match scrutinee { pat => body, ... }`; `ty` is the shared arm type.
    Match {
        scrutinee: Box<HirExpr>,
        arms: Vec<HirMatchArm>,
    },
    Select {
        cases: Vec<HirSelectCase>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct HirSelectCase {
    pub kind: HirSelectCaseKind,
    pub body: Vec<HirStmt>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HirSelectCaseKind {
    Recv { binding: String, channel: HirExpr },
    Send { channel: HirExpr, value: HirExpr },
    Timeout { millis: HirExpr },
    Join { binding: String, task: HirExpr },
    Default,
}

/// One `pattern => body` arm of a [`HirExprKind::Match`]. An expression body is
/// a single [`HirStmt::Expr`]; `ty` is that expression's type (`Void` for block
/// bodies, `Never` for panicking arms).
#[derive(Debug, Clone, PartialEq)]
pub struct HirMatchArm {
    pub pattern: HirPattern,
    pub body: Vec<HirStmt>,
    pub ty: Type,
    pub span: Span,
}

/// A match pattern with resolved binding types.
#[derive(Debug, Clone, PartialEq)]
pub enum HirPattern {
    Wildcard,
    /// Binds the whole scrutinee under `name`.
    Binding {
        name: String,
        ty: Type,
    },
    LiteralBool(bool),
    LiteralInt(i64),
    /// `Enum::Variant` — fieldless.
    EnumVariant {
        enum_name: String,
        variant: String,
    },
    /// `Enum::Variant(a, b)` — each binding carries its payload type
    /// (type parameters substituted from the scrutinee's type arguments).
    EnumVariantTuple {
        enum_name: String,
        variant: String,
        bindings: Vec<(String, Type)>,
    },
    /// `Class(c)` — interface downcast binding `c: Class`. The binding's type
    /// is recorded next to it, as `EnumVariantTuple` records its payloads': a
    /// consumer that has to name the binding's type must not have to rebuild it
    /// from `class_name` and hope the two agree.
    ClassDowncast {
        class_name: String,
        binding: String,
        binding_ty: Type,
    },
}
