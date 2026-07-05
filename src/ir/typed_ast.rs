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
//! (`Array`/`Map`/`Task.join`/locks), array and range `for` loops, all
//! assignment forms, `await`, `?` propagation, and annotated lambdas. General
//! generic substitution and unannotated-lambda inference are future work, as is
//! the control-flow → basic-block lowering (`lowered.rs`). The backend is not
//! yet wired to consume this IR, so behavior is unchanged.

use crate::diagnostics::Span;
use crate::parser::ast::{BinOp, Type, UnaryOp};

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

/// A statement in typed HIR. Control flow keeps its high-level shape here; the
/// basic-block lowering happens in a later slice.
#[derive(Debug, Clone, PartialEq)]
pub enum HirStmt {
    /// `let [mut] name = value;` — `value` carries the inferred binding type.
    Let {
        name: String,
        mutable: bool,
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
    /// `nil`.
    Nil,
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
    /// `|params| body` lambda; `ty` is `fn(params) -> ret`. An expression body is
    /// represented as a single `Return` statement.
    Lambda {
        params: Vec<HirParam>,
        body: Vec<HirStmt>,
    },
    /// `match scrutinee { pat => body, ... }`; `ty` is the shared arm type.
    Match {
        scrutinee: Box<HirExpr>,
        arms: Vec<HirMatchArm>,
    },
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
    /// `Class(c)` — interface downcast binding `c: Class`.
    ClassDowncast {
        class_name: String,
        binding: String,
    },
}
