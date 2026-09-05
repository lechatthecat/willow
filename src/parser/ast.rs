use crate::diagnostics::Span;

/// `Eq`/`Hash` so a written type can key the checker's normalization table
/// (willow-0g8j.3): HIR lowering re-spells an AST annotation with what the
/// checker made of it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    I64,
    F64,
    Bool,
    String,
    Void,
    Named(String),
    Array(Box<Type>),
    Generic(String, Vec<Type>),
    /// `fn(T1, T2) -> R` — plain function pointer type (non-capturing)
    Fn(Vec<Type>, Box<Type>),
    /// Bottom type — coerces to any type (used for panic/return arms in match)
    Never,
}

#[derive(Debug, Clone)]
pub struct Program {
    /// Optional `module path;` declaration at the top of the file. The path is
    /// normalized to a `::`-joined canonical form (see ImportDecl.path).
    pub module: Option<ModuleDecl>,
    pub imports: Vec<ImportDecl>,
    pub items: Vec<Item>,
}

/// `module myapp::util;` — the namespace this source file claims to define.
#[derive(Debug, Clone)]
pub struct ModuleDecl {
    pub path: String,
    pub span: Span,
}

/// `import math;` or `import math as m;`. The parser expands
/// `import math::{add, sub};` into two ordinary declarations.
#[derive(Debug, Clone)]
pub struct ImportDecl {
    pub path: String,
    pub alias: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Item {
    Function(FunctionDecl),
    Class(ClassDecl),
    Enum(EnumDecl),
    // Payload consumed by the interface type checker/codegen (willow-t8b, willow-xds).
    #[allow(dead_code)]
    Interface(InterfaceDecl),
}

/// `interface Animal { fn speak(self) -> String; }`
///
/// An interface is a named set of required instance methods with no bodies,
/// no fields, and no constructors. Classes declare conformance via an
/// `implements` clause. See requirements/willow_interface_requirements.md.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields consumed by the interface type checker (willow-t8b)
pub struct InterfaceDecl {
    pub name: String,
    pub public: bool,
    /// Generic type parameter names in declaration order (`interface Foo<T, U>`).
    /// Empty for non-generic interfaces (willow-1js.1).
    pub type_params: Vec<String>,
    /// Super-interfaces this interface extends (`interface B extends A`),
    /// inheriting their required methods (willow-1js.2). Names may be `::`
    /// qualified. v1 supports a single super-interface.
    pub extends: Vec<String>,
    pub methods: Vec<InterfaceMethodDecl>,
    pub span: Span,
}

/// A required method signature inside an interface (no body).
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields consumed by the interface type checker (willow-t8b)
pub struct InterfaceMethodDecl {
    pub name: String,
    pub params: Vec<Param>,
    pub has_self: bool,
    pub return_type: Type,
    /// A default method body (`fn m(self) { ... }`), if provided. Classes that
    /// implement the interface but do not override `m` inherit this body
    /// (willow-1js.3). `None` for a signature-only (required) method.
    pub default_body: Option<Block>,
    pub span: Span,
}

/// Qualified type path: `Animal` or `animal::Animal`
#[derive(Debug, Clone)]
pub enum TypePath {
    Local(String),
    Qualified(Vec<String>),
}

impl TypePath {
    pub fn name(&self) -> &str {
        match self {
            TypePath::Local(n) => n,
            TypePath::Qualified(parts) => parts.last().map(|s| s.as_str()).unwrap_or(""),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ClassDecl {
    pub name: String,
    pub public: bool,
    pub is_open: bool,
    pub base_class: Option<TypePath>,
    /// Interfaces this class declares conformance to via `implements I, J`.
    /// Each entry is a `Type` so generic interfaces (`implements From<E>`) carry
    /// their type arguments (willow-1js.1). Consumed by the conformance checker.
    #[allow(dead_code)]
    pub implements: Vec<Type>,
    pub fields: Vec<FieldDecl>,
    pub methods: Vec<MethodDecl>,
    /// `init(self, ...)` constructors (willow-scq2). MVP allows at most one.
    /// Empty when the class relies on the implicit memberwise constructor.
    pub constructors: Vec<ConstructorDecl>,
    pub span: Span,
}

/// An `init(self, params...) { ... }` constructor declaration (willow-scq2). No
/// return type; `self` is bound in the body and omitted from `params`.
#[derive(Debug, Clone)]
pub struct ConstructorDecl {
    pub public: bool,
    pub protected: bool,
    pub params: Vec<Param>,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FieldDecl {
    pub name: String,
    pub ty: Type,
    pub public: bool,
    pub protected: bool,
    /// `static` class-level property (willow-qsqf). Static properties live in
    /// global storage and are not part of instance layout.
    pub is_static: bool,
    /// `static mut` — a mutable static property. Only meaningful with
    /// `is_static`; instance fields use their binding's mutability.
    pub is_mut: bool,
    /// Required initializer for static properties (MVP); `None` for instance
    /// fields, which are initialized through constructors.
    pub initializer: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MethodDecl {
    pub name: String,
    pub public: bool,
    pub protected: bool,
    pub is_async: bool,
    pub is_open: bool,
    pub is_override: bool,
    /// `static fn` (willow-qsqf): class-level method with no receiver. A static
    /// method is called as `Type::method(...)` and has no `self` in its body.
    pub is_static: bool,
    pub params: Vec<Param>,
    pub has_self: bool,
    pub return_type: Type,
    pub body: Block,
    pub span: Span,
    /// True for a default-interface-method body injected into this class by the
    /// desugar (willow-1js.7). The body is the canonical interface default, so
    /// for a NON-generic interface it is type-checked once at the interface level
    /// and skipped here to avoid duplicate diagnostics; generic-interface
    /// injections are left `false` so they are checked with substituted type args.
    pub is_default_injected: bool,
    /// True for ANY method synthesized by default-interface-method injection,
    /// including the generic and cross-module ones that `is_default_injected`
    /// leaves `false` because their bodies still need checking here.
    ///
    /// Default injection consults local and imported ancestor shapes so only the
    /// highest class that needs a default receives a copy; subclasses inherit it
    /// normally (willow-3eo1).
    pub is_interface_default: bool,
}

#[derive(Debug, Clone)]
pub struct FunctionDecl {
    pub name: String,
    pub public: bool,
    pub is_async: bool,
    pub params: Vec<Param>,
    pub return_type: Type,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub ty: Type,
    pub mode: ParamMode,
    pub span: Span,
    pub type_span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamMode {
    Value,
    Reference {
        mutable: bool,
        ampersand_span: Span,
        mut_span: Option<Span>,
    },
}

#[derive(Debug, Clone)]
pub struct CallArg {
    pub expr: Expr,
    pub mode: CallArgMode,
    pub span: Span,
}

impl CallArg {
    pub fn value(expr: Expr) -> Self {
        Self {
            span: expr.span(),
            expr,
            mode: CallArgMode::Value,
        }
    }
}

impl std::ops::Deref for CallArg {
    type Target = Expr;

    fn deref(&self) -> &Self::Target {
        &self.expr
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallArgMode {
    Value,
    Reference { ampersand_span: Span },
}

#[derive(Debug, Clone)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Let(LetStmt),
    Assign(AssignStmt),
    FieldAssign(FieldAssignStmt),
    /// `super.init(args...);` — constructor-only base initialization.
    SuperInit(SuperInitStmt),
    /// `ClassName::property = value;` — static property assignment (willow-qsqf).
    StaticFieldAssign(StaticFieldAssignStmt),
    IndexAssign(IndexAssignStmt),
    If(IfStmt),
    While(WhileStmt),
    /// `break;` — exit the innermost enclosing loop (willow-kzka).
    Break(crate::diagnostics::Span),
    /// `continue;` — skip to the next iteration of the innermost loop.
    Continue(crate::diagnostics::Span),
    /// `defer <call>;`, `defer match ... { ... }`, or `defer { ... }` — run the
    /// body when the enclosing SCOPE exits (LIFO; fallthrough/return/`?`/
    /// break/continue; not on panic) (willow-vynv.2 / willow-oorh).
    /// A direct call's receiver and arguments are evaluated at registration.
    Defer(DeferStmt),
    /// `lock <expr> as [mut] <ident> { ... }` — a compiler-managed critical
    /// section over a `Mutex<T>` or `RwLock<T>` (willow-38w.1.1).
    Lock(LockStmt),
    For(ForStmt),
    Return(ReturnStmt),
    Expr(ExprStmt),
}

impl Stmt {
    /// The statement's source span. Debug builds publish it as the runtime
    /// fault site so a fault raised inside a runtime helper (array bounds, a
    /// blocked channel op, an awaited cancelled task) still gets a source
    /// location in `PanicInfo` (willow-s9ej.7).
    pub fn span(&self) -> Span {
        match self {
            Stmt::Let(s) => s.span,
            Stmt::Assign(s) => s.span,
            Stmt::FieldAssign(s) => s.span,
            Stmt::SuperInit(s) => s.span,
            Stmt::StaticFieldAssign(s) => s.span,
            Stmt::IndexAssign(s) => s.span,
            Stmt::If(s) => s.span,
            Stmt::While(s) => s.span,
            Stmt::Break(s) | Stmt::Continue(s) => *s,
            Stmt::Defer(s) => s.span,
            Stmt::Lock(s) => s.span,
            Stmt::For(s) => s.span,
            Stmt::Return(s) => s.span,
            Stmt::Expr(s) => s.span,
        }
    }
}

/// Which lock discipline a `lock` statement acquires (willow-38w.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// `lock m as [mut] v { ... }` — exclusive access to a `Mutex<T>`.
    Mutex,
    /// `lock read r as v { ... }` — shared access to an `RwLock<T>`.
    Read,
    /// `lock write r as [mut] v { ... }` — exclusive access to an `RwLock<T>`.
    Write,
}

impl LockMode {
    /// How the statement is spelled in source, for diagnostics.
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Mutex => "lock",
            Self::Read => "lock read",
            Self::Write => "lock write",
        }
    }

    /// The lock type this mode requires as its target.
    pub fn lock_type_name(self) -> &'static str {
        match self {
            Self::Mutex => "Mutex",
            Self::Read | Self::Write => "RwLock",
        }
    }

    /// Read locks hand out a shared view, so their binding is never writable.
    pub fn allows_mut_binding(self) -> bool {
        !matches!(self, Self::Read)
    }
}

/// `lock <expr> as [mut] <ident> { ... }` — a lexical critical section whose
/// acquisition and release the compiler owns end to end (willow-38w.1).
///
/// `target` is evaluated exactly once, never re-evaluated when a contended
/// acquisition resumes. V1 allows the statement only inside an `async fn`, and
/// forbids both an `await` and a nested `lock` inside `body`.
#[derive(Debug, Clone)]
pub struct LockStmt {
    pub mode: LockMode,
    pub target: Expr,
    pub binding: String,
    pub binding_span: Span,
    pub mutable: bool,
    pub body: Block,
    pub span: Span,
}

impl LockStmt {
    /// `lock <target> as [mut] <binding>` without the body — the part a
    /// diagnostic should underline, since `span` covers the whole critical
    /// section and would render as a page-wide caret run.
    pub fn header_span(&self) -> Span {
        Span::in_file(
            self.span.file_id,
            self.span.start,
            self.binding_span.end,
            self.span.line,
            self.span.col,
        )
    }

    /// Stable synthetic key for the async-frame slot holding the evaluated lock
    /// target (willow-38w.1.4). The target is evaluated once, before the task
    /// can park, so the handle has to survive the park in the frame rather than
    /// on the native stack.
    pub fn handle_frame_key(&self) -> Span {
        self.synthetic_frame_key(1)
    }

    /// Stable synthetic key for the async-frame slot holding this acquisition's
    /// registration token. `release`/`commit` prove ownership with it, so a
    /// resumed poll — or the cancel entry — must be able to read it back.
    pub fn token_frame_key(&self) -> Span {
        self.synthetic_frame_key(2)
    }

    /// Stable synthetic key for the async-frame slot holding this acquisition's
    /// PHASE: `0` until the protected value has been loaded into its binding
    /// slot, `1` from then until the value is committed back.
    ///
    /// The cancellation entry needs it (willow-38w.1.4 review). A non-null
    /// handle alone does not mean the cancelled task owns the lock: it is also
    /// non-null while the task is queued, and while a release has already
    /// handed ownership over but the resumed poll has not consumed the handoff
    /// yet. In that last phase the task IS the owner, so an unconditional
    /// commit would publish whatever the binding slot happens to hold — a
    /// value the critical section never produced.
    pub fn phase_frame_key(&self) -> Span {
        self.synthetic_frame_key(3)
    }

    /// Distinct from `span`, `header_span`, `binding_span`, and every span the
    /// body can produce: only the column is perturbed, and no real token starts
    /// at `span.start` with a different column.
    fn synthetic_frame_key(&self, nonce: usize) -> Span {
        Span::in_file(
            self.span.file_id,
            self.span.start,
            self.span.end,
            self.span.line,
            self.span.col + nonce,
        )
    }
}

#[derive(Debug, Clone)]
pub struct LetStmt {
    pub name: String,
    pub mutable: bool,
    pub ty: Option<Type>,
    pub init: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct AssignStmt {
    pub name: String,
    pub value: Expr,
    pub span: Span,
}

/// `object.field = value;` — field assignment through self or any object.
#[derive(Debug, Clone)]
pub struct FieldAssignStmt {
    pub object: Expr,
    pub field: String,
    pub value: Expr,
    pub span: Span,
}

/// `super.init(args...);` — calls the base class constructor from inside an
/// `init` body. The type checker enforces placement and visibility.
#[derive(Debug, Clone)]
pub struct SuperInitStmt {
    pub args: Vec<CallArg>,
    pub span: Span,
}

/// `ClassName::property = value;` — assignment to a `static mut` property
/// (willow-qsqf). `class` may be module-qualified or `Self`.
#[derive(Debug, Clone)]
pub struct StaticFieldAssignStmt {
    pub class: String,
    pub field: String,
    pub value: Expr,
    pub span: Span,
}

/// `array[index] = value;` — array element assignment.
#[derive(Debug, Clone)]
pub struct IndexAssignStmt {
    pub array: Expr,
    pub index: Expr,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct IfStmt {
    pub cond: Expr,
    pub then_block: Block,
    pub else_block: Option<Block>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct DeferStmt {
    pub body: DeferBody,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum DeferBody {
    /// The parser accepts an expression here so the type checker can issue
    /// E0905 for unsupported forms. Valid forms are a direct call/print or a
    /// `match` expression.
    Expr(Expr),
    Block(Block),
}

impl DeferBody {
    pub fn span(&self) -> Span {
        match self {
            Self::Expr(expr) => expr.span(),
            Self::Block(block) => block.span,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WhileStmt {
    pub cond: Expr,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ForStmt {
    pub name: String,
    pub name_span: Span,
    pub iterable: Expr,
    pub body: Block,
    pub span: Span,
}

impl ForStmt {
    /// Stable synthetic keys for compiler-managed loop state in async frames.
    pub fn iter_frame_key(&self) -> Span {
        Span::in_file(
            self.span.file_id,
            self.span.start,
            self.span.end,
            self.span.line,
            self.span.col + 1,
        )
    }

    pub fn index_frame_key(&self) -> Span {
        Span::in_file(
            self.span.file_id,
            self.span.start,
            self.span.end,
            self.span.line,
            self.span.col + 2,
        )
    }
}

#[derive(Debug, Clone)]
pub struct ReturnStmt {
    pub value: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ExprStmt {
    pub expr: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Integer(i64, Span),
    Float(f64, Span),
    Bool(bool, Span),
    String(String, Span),
    Var(String, Span),
    Binary(Box<BinaryExpr>),
    Unary(Box<UnaryExpr>),
    Call(Box<CallExpr>),
    /// `obj.field`
    FieldAccess(Box<Expr>, String, Span),
    /// `obj.method(args)`
    MethodCall(Box<MethodCallExpr>),
    /// `ClassName::method(args)` — static/constructor call
    StaticCall(Box<StaticCallExpr>),
    /// `ClassName::property` — static property read (willow-qsqf). No parens; the
    /// value is loaded from the class's static global storage.
    StaticField(StaticFieldExpr),
    /// `new ClassName(args...)` — object construction via a constructor
    /// (willow-scq2). Resolves to an explicit `init` or an implicit memberwise
    /// constructor.
    New(Box<NewExpr>),
    /// `ClassName { field: value, ... }`
    ObjectLiteral(Box<ObjectLiteralExpr>),
    /// `await expr`
    Await(Box<AwaitExpr>),
    /// `select { ... }` placeholder for future async select lowering
    Select(SelectExpr),
    Print(Box<Expr>, bool, Span), // bool = newline
    Ternary(Box<TernaryExpr>),
    /// `start..end` — half-open i64 range for `for` loops
    Range(Box<RangeExpr>),
    /// `|params| expr` or `|params| { block }` — anonymous function (non-capturing for now)
    Lambda(Box<LambdaExpr>),
    Match(Box<MatchExpr>),
    /// `expr?` — propagate Result::Err early (the ? operator)
    TryPropagate(Box<Expr>, Span),
    /// `[a, b, c]` — array literal
    ArrayLiteral(Vec<Expr>, Span),
    /// `arr[index]` — array index access
    Index(Box<Expr>, Box<Expr>, Span),
}

#[derive(Debug, Clone)]
pub struct LambdaExpr {
    pub params: Vec<LambdaParam>,
    pub return_type: Option<Type>,
    pub body: LambdaBody,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct LambdaParam {
    pub name: String,
    pub ty: Option<Type>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum LambdaBody {
    Expr(Box<Expr>),
    Block(Block),
}

#[derive(Debug, Clone)]
pub struct TernaryExpr {
    pub condition: Expr,
    pub then_expr: Expr,
    pub else_expr: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct RangeExpr {
    pub start: Expr,
    pub end: Expr,
    pub span: Span,
}

/// `ClassName::property` — a static property read (willow-qsqf). `class` may be
/// module-qualified (e.g. `geom::Config`).
#[derive(Debug, Clone)]
pub struct StaticFieldExpr {
    pub class: String,
    pub field: String,
    pub span: Span,
}

/// `new ClassName(args...)` object construction (willow-scq2).
#[derive(Debug, Clone)]
pub struct NewExpr {
    pub class_name: String,
    pub type_args: Vec<Type>,
    pub args: Vec<CallArg>,
    pub span: Span,
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Integer(_, s)
            | Expr::Float(_, s)
            | Expr::Bool(_, s)
            | Expr::String(_, s)
            | Expr::Var(_, s)
            | Expr::Print(_, _, s) => *s,
            Expr::FieldAccess(_, _, s) => *s,
            Expr::Binary(b) => b.span,
            Expr::Unary(u) => u.span,
            Expr::Call(c) => c.span,
            Expr::MethodCall(m) => m.span,
            Expr::StaticCall(s) => s.span,
            Expr::StaticField(s) => s.span,
            Expr::New(n) => n.span,
            Expr::ObjectLiteral(o) => o.span,
            Expr::Await(a) => a.span,
            Expr::Select(s) => s.span,
            Expr::Ternary(t) => t.span,
            Expr::Range(r) => r.span,
            Expr::Lambda(l) => l.span,
            Expr::Match(m) => m.span,
            Expr::TryPropagate(_, s) => *s,
            Expr::ArrayLiteral(_, s) => *s,
            Expr::Index(_, _, s) => *s,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BinaryExpr {
    pub op: BinOp,
    pub lhs: Expr,
    pub rhs: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct UnaryExpr {
    pub op: UnaryOp,
    pub expr: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct CallExpr {
    pub callee: String,
    pub args: Vec<CallArg>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MethodCallExpr {
    pub object: Expr,
    pub method: String,
    pub args: Vec<CallArg>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct StaticCallExpr {
    pub class: String,
    pub type_args: Vec<Type>,
    pub method: String,
    pub args: Vec<CallArg>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ObjectLiteralExpr {
    pub class: String,
    pub fields: Vec<ObjectLiteralField>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ObjectLiteralField {
    pub name: String,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct AwaitExpr {
    pub expr: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct SelectExpr {
    pub cases: Vec<SelectCase>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct SelectCase {
    pub kind: SelectCaseKind,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum SelectCaseKind {
    /// `v = ch.recv() => { ... }` — ready when the channel has a value or is
    /// closed (closed-empty selection raises when the receive executes).
    Recv { binding: String, channel: Expr },
    /// `ch.send(x) => { ... }` — ready immediately for an (unbounded) channel.
    Send { channel: Expr, value: Expr },
    /// `sleep(ms) => { ... }` — ready once `ms` milliseconds have elapsed
    /// since select entry (deadline fixed at entry; willow-soro).
    Timeout { millis: Expr },
    /// `let v = await t => { ... }` — ready when the task reaches a terminal
    /// state; the binding takes the task result (willow-soro, willow-qrj9).
    ///
    /// The awaited expression is retained intact. Whether cancellation becomes
    /// a panic or `Err(Cancelled)` is determined from its type (`Task<T>` or
    /// `TaskResult<T>`), exactly as for an `await` outside `select`.
    Join { binding: String, task: Expr },
    /// `default => { ... }` — runs when no other case is ready (non-blocking).
    Default,
}

#[derive(Debug, Clone)]
pub struct EnumDecl {
    pub name: String,
    pub public: bool,
    /// Generic type parameter names, e.g. `["T"]` for `Option<T>`.
    pub type_params: Vec<String>,
    pub variants: Vec<EnumVariant>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EnumVariant {
    pub name: String,
    pub payload: Vec<Type>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Pattern {
    Wildcard(Span),
    Binding {
        name: String,
        span: Span,
    },
    LiteralBool(bool, Span),
    LiteralInt(i64, Span),
    /// `Color::Red` — fieldless enum variant
    EnumVariant {
        enum_name: String,
        variant: String,
        span: Span,
    },
    /// `Shape::Circle(r)` or `Shape::Rectangle(w, h)` — associated value enum variant
    EnumVariantTuple {
        enum_name: String,
        variant: String,
        bindings: Vec<String>,
        span: Span,
    },
    /// `Dog(d)` — downcast an interface-typed scrutinee to the concrete class
    /// `Dog`, binding `d: Dog` in the arm (willow-1js.4). `binding` may be `_`
    /// to match the type without binding.
    ClassDowncast {
        class_name: String,
        binding: String,
        span: Span,
    },
}

impl Pattern {
    pub fn span(&self) -> Span {
        match self {
            Pattern::Wildcard(s) => *s,
            Pattern::Binding { span, .. } => *span,
            Pattern::LiteralBool(_, s) => *s,
            Pattern::LiteralInt(_, s) => *s,
            Pattern::EnumVariant { span, .. } => *span,
            Pattern::EnumVariantTuple { span, .. } => *span,
            Pattern::ClassDowncast { span, .. } => *span,
        }
    }
}

#[derive(Debug, Clone)]
pub enum MatchBody {
    Expr(Box<Expr>),
    Block(Block),
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub body: MatchBody,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MatchExpr {
    pub scrutinee: Box<Expr>,
    pub arms: Vec<MatchArm>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    /// `**`. Right-associative and tighter than `*` and unary negation, so
    /// `-2 ** 2` is `-(2 ** 2)` and `2 ** 3 ** 2` is `2 ** (3 ** 2)`.
    Pow,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinOp {
    pub fn symbol(&self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::Pow => "**",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum UnaryOp {
    Neg,
    Not,
}
