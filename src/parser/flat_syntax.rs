//! Constant-depth wire representation for recursive executable syntax.
//! Parent records precede their children; restoration consumes child records
//! in reverse order without recursive deserialization or unbounded Rust frames.
use super::ast::*;
use super::ownership::{NodeMut, NodeOwned};
use crate::diagnostics::Span;
use serde::{Deserialize, Serialize};

// JSON numbers cannot represent infinities/NaNs and can alter negative zero.
// Artifacts preserve the exact IEEE payload accepted by the lexer/parser.
mod float_bits {
    use serde::{Deserialize, Serialize};
    pub fn serialize<S: serde::Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
        value.to_bits().serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
        u64::deserialize(deserializer).map(f64::from_bits)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "Expr")]
enum ExprWire {
    Integer(i64, Span, ExprId),
    Float(#[serde(with = "float_bits")] f64, Span, ExprId),
    Bool(bool, Span, ExprId),
    String(String, Span, ExprId),
    Var(String, Span, ExprId),
    Binary(Box<BinaryExpr>),
    Unary(Box<UnaryExpr>),
    Call(Box<CallExpr>),
    /// `obj.field`
    FieldAccess(Box<Expr>, String, Span, ExprId),
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
    Print(Box<Expr>, bool, Span, ExprId), // bool = newline
    Ternary(Box<TernaryExpr>),
    /// `start..end` — half-open i64 range for `for` loops
    Range(Box<RangeExpr>),
    /// `|params| expr` or `|params| { block }` — anonymous function. It is a
    /// `fn` value when it captures nothing and a `closure` value when it does
    /// (willow-0g8j.2.12); the two are different types.
    Lambda(Box<LambdaExpr>),
    Match(Box<MatchExpr>),
    /// `expr?` — propagate Result::Err early (the ? operator)
    TryPropagate(Box<Expr>, Span, ExprId),
    /// `[a, b, c]` — array literal
    ArrayLiteral(Vec<Expr>, Span, ExprId),
    /// `arr[index]` — array index access
    Index(Box<Expr>, Box<Expr>, Span, ExprId),
}

#[derive(Serialize, Deserialize)]
#[serde(remote = "Stmt")]
enum StmtWire {
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

#[derive(Serialize, Deserialize)]
enum Shell {
    Expr(#[serde(with = "ExprWire")] Expr),
    Stmt(
        #[serde(
            serialize_with = "StmtWire::serialize",
            deserialize_with = "deserialize_boxed_stmt"
        )]
        Box<Stmt>,
    ),
}
fn deserialize_boxed_stmt<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Box<Stmt>, D::Error> {
    StmtWire::deserialize(deserializer).map(Box::new)
}

impl Shell {
    fn as_mut(&mut self) -> NodeMut<'_> {
        match self {
            Self::Expr(expr) => NodeMut::Expr(expr),
            Self::Stmt(stmt) => NodeMut::Stmt(stmt),
        }
    }
}
impl From<NodeOwned> for Shell {
    fn from(node: NodeOwned) -> Self {
        match node {
            NodeOwned::Expr(expr) => Self::Expr(expr),
            NodeOwned::Stmt(stmt) => Self::Stmt(stmt),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Record {
    shell: Shell,
    children: Vec<usize>,
}

#[derive(Serialize, Deserialize)]
struct FlatSyntax {
    nodes: Vec<Record>,
}
impl FlatSyntax {
    fn new(root: NodeOwned) -> Self {
        let mut pending = std::collections::VecDeque::from([root]);
        let mut nodes = Vec::new();
        while let Some(mut node) = pending.pop_front() {
            let children = node
                .as_mut()
                .children()
                .into_iter()
                .map(|child| {
                    let id = nodes.len() + pending.len() + 1;
                    pending.push_back(child.take());
                    id
                })
                .collect();
            nodes.push(Record {
                shell: node.into(),
                children,
            });
        }
        Self { nodes }
    }

    fn restore(self) -> Result<Shell, String> {
        let count = self.nodes.len();
        let mut completed: Vec<Option<Shell>> = (0..count).map(|_| None).collect();
        for (
            index,
            Record {
                mut shell,
                children,
            },
        ) in self.nodes.into_iter().enumerate().rev()
        {
            let slots = shell.as_mut().children();
            if slots.len() != children.len() {
                return Err("syntax child count mismatch".into());
            }
            for (slot, child) in slots.into_iter().zip(children) {
                if child <= index || child >= count {
                    return Err("invalid syntax child index".into());
                }
                let child = completed[child].take().ok_or("reused syntax child index")?;
                match (slot, child) {
                    (NodeMut::Expr(slot), Shell::Expr(expr)) => *slot = expr,
                    (NodeMut::Stmt(slot), Shell::Stmt(stmt)) => *slot = *stmt,
                    _ => return Err("syntax child kind mismatch".into()),
                }
            }
            completed[index] = Some(shell);
        }
        if completed.iter().skip(1).any(Option::is_some) {
            return Err("unreachable syntax record".into());
        }
        completed
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| "missing syntax root".into())
    }
}

impl Serialize for Expr {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        FlatSyntax::new(NodeOwned::Expr(self.clone())).serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Expr {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match FlatSyntax::deserialize(deserializer)?
            .restore()
            .map_err(serde::de::Error::custom)?
        {
            Shell::Expr(expr) => Ok(expr),
            Shell::Stmt(_) => Err(serde::de::Error::custom("expected expression root")),
        }
    }
}
impl Serialize for Stmt {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        FlatSyntax::new(NodeOwned::Stmt(Box::new(self.clone()))).serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for Stmt {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match FlatSyntax::deserialize(deserializer)?
            .restore()
            .map_err(serde::de::Error::custom)?
        {
            Shell::Stmt(stmt) => Ok(*stmt),
            Shell::Expr(_) => Err(serde::de::Error::custom("expected statement root")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn flat_wire_roundtrips_deep_expression_and_statement_edges_on_small_stack() {
        std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(|| {
                let mut expr = Expr::Integer(42, Span::dummy(), ExprId::fresh());
                for _ in 0..8_000 {
                    expr = Expr::Print(Box::new(expr), false, Span::dummy(), ExprId::fresh());
                }
                let id = expr.id();
                let wire = serde_json::to_vec(&expr).unwrap();
                let restored: Expr = serde_json::from_slice(&wire).unwrap();
                assert_eq!(restored.id(), id);
                drop(restored);
                let mut stmt = Stmt::Expr(ExprStmt {
                    expr,
                    span: Span::dummy(),
                });
                for _ in 0..8_000 {
                    stmt = Stmt::Defer(DeferStmt {
                        body: DeferBody::Block(Block {
                            stmts: vec![stmt],
                            span: Span::dummy(),
                        }),
                        span: Span::dummy(),
                    });
                }
                let wire = serde_json::to_vec(&stmt).unwrap();
                let restored: Stmt = serde_json::from_slice(&wire).unwrap();
                let mut count = 0;
                let mut work = vec![super::super::ownership::NodeRef::Stmt(&restored)];
                while let Some(node) = work.pop() {
                    count += 1;
                    work.extend(node.children());
                }
                assert_eq!(count, 16_002);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn float_wire_preserves_every_ieee_payload() {
        for bits in [
            0,
            1_u64 << 63,
            f64::INFINITY.to_bits(),
            f64::NEG_INFINITY.to_bits(),
            0x7ff8_0000_0000_002a,
            1.25_f64.to_bits(),
        ] {
            let expr = Expr::Float(f64::from_bits(bits), Span::dummy(), ExprId::fresh());
            let restored: Expr =
                serde_json::from_slice(&serde_json::to_vec(&expr).unwrap()).unwrap();
            let Expr::Float(value, _, _) = &restored else {
                panic!("float variant changed")
            };
            assert_eq!(value.to_bits(), bits);
        }
    }

    #[test]
    fn flat_wire_rejects_invalid_edges_without_panicking() {
        let bad = FlatSyntax {
            nodes: vec![Record {
                shell: Shell::Expr(Expr::Print(
                    Box::new(Expr::Integer(0, Span::dummy(), ExprId::fresh())),
                    false,
                    Span::dummy(),
                    ExprId::fresh(),
                )),
                children: vec![0],
            }],
        };
        assert!(bad.restore().is_err());
    }
}
