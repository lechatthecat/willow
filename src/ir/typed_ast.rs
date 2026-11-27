//! Typed high-level IR (HIR) — typed HIR lowering.
//!
//! Checked source functions lower through this typed tree into LIR and then
//! Cranelift. Every expression carries its resolved type; backend consumers
//! must not re-infer types from AST shape. Source spans remain diagnostic
//! locations. The remaining frontend side tables still use spans and are a
//! separate migration from typed body emission.

use crate::diagnostics::Span;
use crate::parser::ast::{BinOp, ExprId, LockMode, UnaryOp};
use crate::qualified_name::QualifiedName;
use crate::semantic::ids::{FunctionId, SemanticType as Type, TypeId};

/// Compiler-owned function identity for one static property's initializer.
/// `$` cannot occur in a source identifier.
pub fn static_initializer_name(class: &str, field: &str) -> String {
    QualifiedName::join(class, format_args!("$static_init.{field}"))
}

/// A whole program lowered to typed HIR. Slice 1 only carries free functions.
#[derive(Debug, Clone, PartialEq)]
pub struct HirProgram {
    pub functions: Vec<HirFunction>,
    pub classes: Vec<HirClass>,
    pub resolution: HirResolution,
}

/// Owned, canonical declaration metadata used by LIR lowering. It has no
/// references to the mutable checker or unit-local alias scopes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HirResolution {
    /// Unit-visible module spellings and their checked free-function signatures.
    pub modules: std::collections::HashMap<TypeId, std::collections::HashMap<String, HirSignature>>,

    pub enums: std::collections::HashMap<TypeId, HirEnumInfo>,
    pub classes: std::collections::HashMap<TypeId, HirClassInfo>,
    pub interfaces: std::collections::HashMap<TypeId, HirInterfaceInfo>,
    pub functions: std::collections::HashMap<FunctionId, HirSignature>,
    /// Exported interfaces/open classes may acquire async implementations in
    /// importing units absent from this unit's declaration snapshot.
    pub open_dispatch_types: std::collections::HashSet<TypeId>,
    /// Unit access spelling to canonical builtin namespace; user imports win
    /// over a canonical spelling without displacing an explicit std alias.
    pub namespaces: std::collections::HashMap<TypeId, String>,
}

/// Ordered generic parameters. Cloned declaration snapshots share the immutable
/// names and the lookup index, which is built only on a named lookup.
#[derive(Clone, Default)]
pub struct HirTypeParameters(ParameterNames);

#[derive(Clone, Default)]
enum ParameterNames {
    #[default]
    Empty,
    One(TypeId),
    Many(std::sync::Arc<IndexedNames>),
}

struct IndexedNames {
    ordered: Box<[TypeId]>,
    indices: std::sync::OnceLock<std::collections::HashMap<TypeId, usize>>,
}

#[cfg(test)]
std::thread_local! {
    pub(crate) static BINDING_INDEX_ENTRIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl FromIterator<TypeId> for HirTypeParameters {
    fn from_iter<I: IntoIterator<Item = TypeId>>(iter: I) -> Self {
        let mut names = iter.into_iter();
        let Some(first) = names.next() else {
            return Self::default();
        };
        let Some(second) = names.next() else {
            return Self(ParameterNames::One(first));
        };
        Self(ParameterNames::Many(std::sync::Arc::new(IndexedNames {
            ordered: [first, second].into_iter().chain(names).collect(),
            indices: Default::default(),
        })))
    }
}

impl std::fmt::Debug for HirTypeParameters {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_slice(), f)
    }
}

impl PartialEq for HirTypeParameters {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl HirTypeParameters {
    pub fn as_slice(&self) -> &[TypeId] {
        match &self.0 {
            ParameterNames::Empty => &[],
            ParameterNames::One(id) => std::slice::from_ref(id),
            ParameterNames::Many(params) => &params.ordered,
        }
    }

    pub(crate) fn position(&self, name: &TypeId) -> Option<usize> {
        let params = match &self.0 {
            ParameterNames::Empty => return None,
            ParameterNames::One(only) => return (name == only).then_some(0),
            ParameterNames::Many(params) => params,
        };
        let indices = params.indices.get_or_init(|| {
            let index: std::collections::HashMap<_, _> =
                params.ordered.iter().copied().zip(0..).collect();
            #[cfg(test)]
            BINDING_INDEX_ENTRIES.set(BINDING_INDEX_ENTRIES.get() + index.len());
            index
        });
        indices.get(name).copied()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HirEnumInfo {
    pub type_params: HirTypeParameters,
    pub variants: Vec<HirEnumVariant>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HirEnumVariant {
    pub name: String,
    pub tag: i64,
    pub payloads: Vec<Type>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HirClassInfo {
    pub base: Option<TypeId>,
    pub implements: Vec<Type>,
    /// Fields declared by this class, in declaration order. Inherited fields
    /// are obtained by following `base`, from the oldest ancestor first.
    pub fields: Vec<(String, Type)>,
    pub static_fields: std::collections::HashMap<String, Type>,
    pub constructor: Option<HirSignature>,
    pub methods: std::collections::HashMap<String, HirSignature>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HirInterfaceInfo {
    pub type_params: HirTypeParameters,
    pub extends: Vec<TypeId>,
    pub methods: std::collections::HashMap<String, HirSignature>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HirSignature {
    /// Source parameters, excluding any implicit receiver.
    pub params: Vec<Type>,
    pub param_modes: Vec<crate::parser::ast::ParamMode>,
    pub return_type: Type,
    pub is_static: bool,
    pub is_async: bool,
}

impl HirResolution {
    /// Representation-preserving class upcasts and declared interface
    /// conversions. Lowering uses declarations rather than backend layouts.
    pub(crate) fn can_coerce(&self, source: &Type, target: &Type) -> bool {
        self.can_coerce_matching(source, |ty, relax| {
            if relax {
                ty.same_modulo_callable_kind(target)
            } else {
                ty == target
            }
        })
    }

    /// Ancestors are nominal. Actual arguments affect the match, while the
    /// declaration alone determines which edges to visit next.
    pub(crate) fn can_coerce_matching(
        &self,
        source: &Type,
        matches: impl Fn(&Type, bool) -> bool,
    ) -> bool {
        if matches(source, true) {
            return true;
        }
        let (Type::Named(root) | Type::Generic(root, _)) = source else {
            return false;
        };
        let mut pending = vec![*root];
        let mut seen = std::collections::HashSet::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            if let Some(class) = self.classes.get(&id) {
                if let Some(base) = class.base {
                    if matches(&Type::Named(base), false) {
                        return true;
                    }
                    pending.push(base);
                }
                for ty in &class.implements {
                    if matches(ty, false) {
                        return true;
                    }
                    if let Type::Named(id) | Type::Generic(id, _) = ty {
                        pending.push(*id);
                    }
                }
            } else if let Some(interface) = self.interfaces.get(&id) {
                for &parent in &interface.extends {
                    if matches(&Type::Named(parent), false) {
                        return true;
                    }
                    pending.push(parent);
                }
            }
        }
        false
    }
}

/// A class and its lowered methods. Each method is a [`HirFunction`] whose first
/// parameter is the receiver `self` (typed as the class) when present.
#[derive(Debug, Clone, PartialEq)]
pub struct HirClass {
    pub name: TypeId,
    pub methods: Vec<HirFunction>,
    pub span: Span,
}

/// A free function (or class method) with typed parameters, a declared return
/// type, and a typed statement body.
#[derive(Debug, Clone, PartialEq)]
pub struct HirFunction {
    pub requires: Vec<HirExpr>,
    pub ensures: Vec<HirExpr>,
    pub name: FunctionId,
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
/// closure environment.
///
/// The two names are the SAME source variable seen from the two sides of the
/// lift: `source` is what the enclosing function calls it, `name` is what the
/// lifted body calls it. They differ whenever shadowing renamed one of them,
/// because the lifted body restarts the flat namespace.
#[derive(Debug, Clone, PartialEq)]
pub struct HirCapture {
    /// The binding's name inside the lifted lambda body.
    pub name: String,
    /// The binding's name in the enclosing function, which is where the value
    /// is read from when the environment is built.
    pub source: String,
    pub ty: Type,
}

/// What a `defer` runs when its scope exits.
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
pub struct HirLockTarget {
    pub mode: LockMode,
    pub target: HirExpr,
    pub binding: String,
    pub mutable: bool,
}

#[derive(Debug, PartialEq)]
pub enum HirStmt {
    /// `let [mut] name: ty = value;` — `ty` is the type the name is *bound*
    /// with: the annotation when written, otherwise the initialiser's inferred
    /// type. The two differ whenever the annotation is a supertype of the
    /// initialiser (`let a: Animal = new Dog();`), so a consumer that stores or
    /// reloads the variable must use `ty`, not `value.ty`.
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
        /// All reachable arms transfer control out of the enclosing statement list.
        exits: bool,
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
    /// `break;` — exit the innermost loop.
    Break { span: Span },
    /// `continue;` — next iteration of the innermost loop.
    Continue { span: Span },
    /// Scope-exit cleanup with an identity allocated during HIR lowering.
    /// Source coordinates remain diagnostic metadata only.
    Defer {
        id: HirDeferId,
        body: HirDeferBody,
        span: Span,
    },
    Lock {
        walk: bool,
        additional: Vec<HirLockTarget>,
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
        class: TypeId,
        field: String,
        value: HirExpr,
        span: Span,
    },
    /// `super.init(args);` — base-class construction inside an `init` body.
    SuperInit { args: Vec<HirExpr>, span: Span },
}

/// A defer site within one lowered callable, independent of source spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HirDeferId(pub u32);

/// A typed expression: a [`HirExprKind`] plus its resolved [`Type`].
#[derive(Debug, PartialEq)]
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

    /// Traverse in evaluation postorder, optionally treating lambdas as
    /// separate callables. The work list lives on the heap.
    pub fn walk_postorder(&self, include_lambdas: bool) -> HirExprWalk<'_> {
        HirExprWalk {
            pending: vec![(self, false)],
            include_lambdas,
        }
    }

    /// The physical ancestor path to one borrowed node, including both ends.
    /// Pointer identity distinguishes repeated expressions with equal spans.
    pub(crate) fn path_to(&self, target: &HirExpr) -> Option<Vec<&HirExpr>> {
        let mut pending = vec![(self, false)];
        let mut path = Vec::new();
        while let Some((node, leaving)) = pending.pop() {
            if leaving {
                path.pop();
                continue;
            }
            path.push(node);
            if std::ptr::eq(node, target) {
                return Some(path);
            }
            pending.push((node, true));
            pending.extend(
                node.children()
                    .into_iter()
                    .rev()
                    .map(|child| (child, false)),
            );
        }
        None
    }

    /// Every sub-expression one level down, in evaluation order.
    ///
    /// The match is exhaustive on purpose: a new [`HirExprKind`] has to be
    /// threaded through here or this stops compiling, so a consumer that walks
    /// the tree cannot silently miss a node kind (the same discipline as
    /// `parser::iter`).
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
            HirExprKind::Invoke { callee, args, .. } => {
                std::iter::once(&**callee).chain(args.iter()).collect()
            }
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
                    out.extend(arm.guard.iter().map(HirGuard::expr));
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

pub struct HirExprWalk<'a> {
    pending: Vec<(&'a HirExpr, bool)>,
    include_lambdas: bool,
}
#[cfg(test)]
std::thread_local! {
    pub(crate) static POSTORDER_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl<'a> Iterator for HirExprWalk<'a> {
    type Item = &'a HirExpr;
    fn next(&mut self) -> Option<Self::Item> {
        while let Some((expr, visited)) = self.pending.pop() {
            if !self.include_lambdas && matches!(expr.kind, HirExprKind::Lambda { .. }) {
                continue;
            }
            if visited {
                #[cfg(test)]
                POSTORDER_VISITS.set(POSTORDER_VISITS.get() + 1);
                return Some(expr);
            }
            self.pending.push((expr, true));
            self.pending.extend(
                expr.children()
                    .into_iter()
                    .rev()
                    .map(|child| (child, false)),
            );
        }
        None
    }
}

/// Every expression inside a statement body, flattened. Free rather than a
/// closure so the returned borrows keep the body's lifetime instead of the
/// caller's frame.
fn nested_exprs(body: &[HirStmt]) -> Vec<&HirExpr> {
    body.iter().flat_map(|stmt| stmt.child_exprs()).collect()
}

impl HirStmt {
    pub(crate) fn conditional(
        cond: HirExpr,
        then_branch: Vec<HirStmt>,
        else_branch: Option<Vec<HirStmt>>,
        span: Span,
    ) -> Self {
        // Child conditionals already summarize their arms. Inspect only this
        // list's immediate statements, even for deeply nested conditionals.
        fn arm_exits(body: &[HirStmt]) -> bool {
            for stmt in body {
                match stmt {
                    // A cleanup may recover and resume after this scope.
                    HirStmt::Defer { .. } => return false,
                    HirStmt::Expr(e) if e.ty == Type::Never => return false,
                    _ if stmt.transfers_control() => return true,
                    _ => {}
                }
            }
            false
        }

        let exits = match cond.kind {
            HirExprKind::Bool(true) => arm_exits(&then_branch),
            HirExprKind::Bool(false) => else_branch.as_deref().is_some_and(arm_exits),
            _ => else_branch.as_deref().is_some_and(arm_exits) && arm_exits(&then_branch),
        };
        Self::If {
            cond,
            then_branch,
            else_branch,
            span,
            exits,
        }
    }

    pub(crate) fn transfers_control(&self) -> bool {
        matches!(
            self,
            Self::Return { .. }
                | Self::Break { .. }
                | Self::Continue { .. }
                | Self::If { exits: true, .. }
        )
    }

    /// Every expression this statement holds directly, including those inside
    /// its nested statement bodies. Exhaustive for the same reason as
    /// [`HirExpr::children`].
    pub fn child_exprs(&self) -> Vec<&HirExpr> {
        enum Child<'a> {
            Stmt(&'a HirStmt),
            Expr(&'a HirExpr),
        }
        let mut stack = vec![Child::Stmt(self)];
        let mut pending = Vec::new();
        let mut out = Vec::new();
        while let Some(child) = stack.pop() {
            let stmt = match child {
                Child::Expr(expr) => {
                    out.push(expr);
                    continue;
                }
                Child::Stmt(stmt) => stmt,
            };
            match stmt {
                HirStmt::Break { .. } | HirStmt::Continue { .. } => {}
                HirStmt::Let { value, .. }
                | HirStmt::Assign { value, .. }
                | HirStmt::StaticFieldAssign { value, .. } => pending.push(Child::Expr(value)),
                HirStmt::If {
                    cond,
                    then_branch,
                    else_branch,
                    ..
                } => {
                    pending.push(Child::Expr(cond));
                    pending.extend(then_branch.iter().map(Child::Stmt));
                    if let Some(body) = else_branch {
                        pending.extend(body.iter().map(Child::Stmt));
                    }
                }
                HirStmt::While { cond, body, .. } => {
                    pending.push(Child::Expr(cond));
                    pending.extend(body.iter().map(Child::Stmt));
                }
                HirStmt::For { iterable, body, .. } => {
                    pending.push(Child::Expr(iterable));
                    pending.extend(body.iter().map(Child::Stmt));
                }
                HirStmt::Lock {
                    target,
                    additional,
                    body,
                    ..
                } => {
                    pending.push(Child::Expr(target));
                    pending.extend(additional.iter().map(|member| Child::Expr(&member.target)));
                    pending.extend(body.iter().map(Child::Stmt));
                }
                HirStmt::Return { value, .. } => pending.extend(value.iter().map(Child::Expr)),
                HirStmt::Defer { body, .. } => match body {
                    HirDeferBody::Expr(expr) => pending.push(Child::Expr(expr)),
                    HirDeferBody::Block(body) => pending.extend(body.iter().map(Child::Stmt)),
                },
                HirStmt::Expr(expr) => pending.push(Child::Expr(expr)),
                HirStmt::FieldAssign { object, value, .. } => {
                    pending.extend([Child::Expr(object), Child::Expr(value)]);
                }
                HirStmt::IndexAssign {
                    array,
                    index,
                    value,
                    ..
                } => {
                    pending.extend([Child::Expr(array), Child::Expr(index), Child::Expr(value)]);
                }
                HirStmt::SuperInit { args, .. } => pending.extend(args.iter().map(Child::Expr)),
            }
            stack.extend(pending.drain(..).rev());
        }
        out
    }
}

/// Lexical resolution of a named call. A deferred call can retain a stable
/// binding directly. A local that may change before cleanup is read at registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HirCallBinding {
    Function,
    Local { may_change: bool },
}

impl HirCallBinding {
    pub(crate) fn is_local(self) -> bool {
        matches!(self, Self::Local { .. })
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
    /// `apply(10, double)`. Spelled as a bare identifier in
    /// source, so it is only distinguishable from [`HirExprKind::Var`] by what
    /// the name resolves to; `ty` is the function's `fn(...) -> ...` type.
    FnRef(FunctionId),
    Binary {
        op: BinOp,
        lhs: Box<HirExpr>,
        rhs: Box<HirExpr>,
    },
    Unary {
        op: UnaryOp,
        operand: Box<HirExpr>,
    },
    /// A named call. Lexical resolution distinguishes a local callable from a function.
    Call {
        callee: FunctionId,
        binding: HirCallBinding,
        args: Vec<HirExpr>,
    },
    /// Call a computed callable value (including a mutable capture cell load).
    Invoke {
        name: FunctionId,
        callee: Box<HirExpr>,
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
        class: TypeId,
        args: Vec<HirExpr>,
    },
    /// `object.field`; `ty` is the field's declared type.
    FieldAccess {
        object: Box<HirExpr>,
        field: String,
    },
    /// `object.method(args)`; `ty` is the method's return type.
    MethodCall {
        builtin: Option<crate::semantic::intrinsics::BuiltinMethod>,
        object: Box<HirExpr>,
        method: crate::parser::ast::MethodSpelling,
        args: Vec<HirExpr>,
        /// `Some(base)` for `super.method(args)`: `object` is `self`, and the
        /// call targets the implementation `base` resolves `method` to —
        /// directly, never through `self`'s vtable.
        super_class: Option<TypeId>,
    },
    /// `Class { field: value, ... }` object literal; `ty` is the class type.
    ObjectLiteral {
        class: TypeId,
        fields: Vec<(String, HirExpr)>,
    },
    /// `Class::field` static property read; `ty` is the property's type.
    StaticField {
        class: TypeId,
        field: String,
    },
    /// `Class::method(args)` static call; `ty` is the static method's return type.
    StaticCall {
        class: TypeId,
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
    /// `|params| body` lambda; `ty` is the function type `fn(params) -> ret`,
    /// of the shareable kind (`Type::Fn`) when every capture is immutable and
    /// shareable and of the task-local kind (`Type::Closure`) otherwise. An
    /// expression body is represented as a single `Return` statement.
    Lambda {
        id: ExprId,
        params: Vec<HirParam>,
        /// What the body reads from the enclosing function, in environment-slot
        /// order. Empty for a lambda that captures nothing.
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
    /// `closed_aware`: `ch.recv_opt()` binds `Option<T>`.
    Recv {
        binding: String,
        channel: HirExpr,
        closed_aware: bool,
    },
    Send {
        channel: HirExpr,
        value: HirExpr,
    },
    Timeout {
        millis: HirExpr,
    },
    Join {
        binding: String,
        task: HirExpr,
    },
    Default,
}

/// One `pattern => body` arm of a [`HirExprKind::Match`]. An expression body is
/// a single [`HirStmt::Expr`]; `ty` is that expression's type (`Void` for block
/// bodies, `Never` for panicking arms).
#[derive(Debug, Clone, PartialEq)]
pub struct HirMatchArm {
    pub pattern: HirPattern,
    /// Steps run in order after `pattern` matched, each with the bindings of
    /// the pattern and of earlier steps in scope. A false `if` or a failed
    /// `let` falls through to the next arm. Nested source patterns are
    /// flattened into `let` steps over synthetic payload locals.
    pub guard: Vec<HirGuard>,
    pub body: Vec<HirStmt>,
    pub ty: Type,
    pub span: Span,
}

/// One step of a match arm's guard chain.
#[derive(Debug, Clone, PartialEq)]
pub enum HirGuard {
    /// `if cond` / `&& cond`.
    If(HirExpr),
    /// `&& let pattern = scrutinee`: `pattern` is flat (its payload positions
    /// are bindings or `_`).
    Let {
        scrutinee: HirExpr,
        pattern: HirPattern,
    },
}

impl HirGuard {
    /// The expression this step evaluates.
    pub fn expr(&self) -> &HirExpr {
        match self {
            Self::If(expr)
            | Self::Let {
                scrutinee: expr, ..
            } => expr,
        }
    }

    pub fn expr_mut(&mut self) -> &mut HirExpr {
        match self {
            Self::If(expr)
            | Self::Let {
                scrutinee: expr, ..
            } => expr,
        }
    }

    /// The same step evaluating `expr` instead.
    pub fn with_expr(&self, expr: HirExpr) -> Self {
        match self {
            Self::If(_) => Self::If(expr),
            Self::Let { pattern, .. } => Self::Let {
                scrutinee: expr,
                pattern: pattern.clone(),
            },
        }
    }
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
        enum_name: TypeId,
        variant: String,
    },
    /// `Enum::Variant(a, b)` — each binding carries its payload type
    /// (type parameters substituted from the scrutinee's type arguments).
    EnumVariantTuple {
        enum_name: TypeId,
        variant: String,
        bindings: Vec<(String, Type)>,
    },
    /// `Class(c)` — interface downcast binding `c: Class`. The binding's type
    /// is recorded next to it, as `EnumVariantTuple` records its payloads': a
    /// consumer that has to name the binding's type must not have to rebuild it
    /// from `class_name` and hope the two agree.
    ClassDowncast {
        class_name: TypeId,
        binding: String,
        binding_ty: Type,
    },
}

#[path = "typed_ast/ownership.rs"]
mod ownership;
pub(crate) use ownership::{
    ChildrenMut, NodeRef as HirNodeRef, Siblings, visit_expr_children_mut, visit_stmt_children_mut,
};
