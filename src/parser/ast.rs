use crate::diagnostics::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    I64,
    F64,
    Bool,
    String,
    Void,
    Nil,
    Named(String),
    Array(Box<Type>),
    Generic(String, Vec<Type>),
    Nullable(Box<Type>),
    /// `fn(T1, T2) -> R` — plain function pointer type (non-capturing)
    Fn(Vec<Type>, Box<Type>),
    /// Bottom type — coerces to any type (used for panic/return arms in match)
    Never,
}

#[derive(Debug, Clone)]
pub struct Program {
    pub imports: Vec<ImportDecl>,
    pub items: Vec<Item>,
}

/// `import math;` or `import math as m;`
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
    pub fields: Vec<FieldDecl>,
    pub methods: Vec<MethodDecl>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FieldDecl {
    pub name: String,
    pub ty: Type,
    pub public: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MethodDecl {
    pub name: String,
    pub public: bool,
    pub is_async: bool,
    pub is_open: bool,
    pub is_override: bool,
    pub params: Vec<Param>,
    pub has_self: bool,
    pub return_type: Type,
    pub body: Block,
    pub span: Span,
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
    If(IfStmt),
    While(WhileStmt),
    Return(ReturnStmt),
    Expr(ExprStmt),
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

#[derive(Debug, Clone)]
pub struct IfStmt {
    pub cond: Expr,
    pub then_block: Block,
    pub else_block: Option<Block>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct WhileStmt {
    pub cond: Expr,
    pub body: Block,
    pub span: Span,
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
    Nil(Span),
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
    /// `ClassName { field: value, ... }`
    ObjectLiteral(Box<ObjectLiteralExpr>),
    /// `spawn function(args)`
    Spawn(Box<SpawnExpr>),
    /// `await expr`
    Await(Box<AwaitExpr>),
    /// `select { ... }` placeholder for future async select lowering
    Select(SelectExpr),
    Print(Box<Expr>, bool, Span), // bool = newline
    Ternary(Box<TernaryExpr>),
    /// `|params| expr` or `|params| { block }` — anonymous function (non-capturing for now)
    Lambda(Box<LambdaExpr>),
    Match(Box<MatchExpr>),
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

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Integer(_, s)
            | Expr::Float(_, s)
            | Expr::Bool(_, s)
            | Expr::Nil(s)
            | Expr::String(_, s)
            | Expr::Var(_, s)
            | Expr::Print(_, _, s) => *s,
            Expr::FieldAccess(_, _, s) => *s,
            Expr::Binary(b) => b.span,
            Expr::Unary(u) => u.span,
            Expr::Call(c) => c.span,
            Expr::MethodCall(m) => m.span,
            Expr::StaticCall(s) => s.span,
            Expr::ObjectLiteral(o) => o.span,
            Expr::Spawn(s) => s.span,
            Expr::Await(a) => a.span,
            Expr::Select(s) => s.span,
            Expr::Ternary(t) => t.span,
            Expr::Lambda(l) => l.span,
            Expr::Match(m) => m.span,
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
pub struct SpawnExpr {
    pub callee: String,
    pub args: Vec<CallArg>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct AwaitExpr {
    pub expr: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct SelectExpr {
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct EnumDecl {
    pub name: String,
    pub public: bool,
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
    Binding { name: String, span: Span },
    LiteralBool(bool, Span),
    LiteralInt(i64, Span),
    /// `Color::Red` — fieldless enum variant
    EnumVariant { enum_name: String, variant: String, span: Span },
    /// `Shape::Circle(r)` or `Shape::Rectangle(w, h)` — associated value enum variant
    EnumVariantTuple { enum_name: String, variant: String, bindings: Vec<String>, span: Span },
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
