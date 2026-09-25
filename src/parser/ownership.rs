//! Shared heap-backed ownership traversal for syntax trees.
use super::ast::*;
use crate::diagnostics::Span;
pub(crate) fn placeholder_expr() -> Expr {
    Expr::Integer(0, Span::dummy(), ExprId::placeholder())
}
pub(crate) enum NodeRef<'a> {
    Expr(&'a Expr),
    Stmt(&'a Stmt),
}
pub(crate) enum NodeMut<'a> {
    Expr(&'a mut Expr),
    Stmt(&'a mut Stmt),
}
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) enum NodeOwned {
    Expr(Expr),
    Stmt(Box<Stmt>),
}
fn expr_children<'a>(node: &'a Expr, visit: &mut impl FnMut(NodeRef<'a>)) {
    match node {
        Expr::Binary(v) => {
            visit(NodeRef::Expr(&v.lhs));
            visit(NodeRef::Expr(&v.rhs));
        }
        Expr::Unary(v) => visit(NodeRef::Expr(&v.expr)),
        Expr::Call(v) => {
            for a in &v.args {
                visit(NodeRef::Expr(&a.expr));
            }
        }
        Expr::FieldAccess(v, ..) | Expr::Print(v, ..) | Expr::TryPropagate(v, ..) => {
            visit(NodeRef::Expr(v))
        }
        Expr::MethodCall(v) => {
            visit(NodeRef::Expr(&v.object));
            for a in &v.args {
                visit(NodeRef::Expr(&a.expr));
            }
        }
        Expr::StaticCall(v) => {
            for a in &v.args {
                visit(NodeRef::Expr(&a.expr));
            }
        }
        Expr::New(v) => {
            for a in &v.args {
                visit(NodeRef::Expr(&a.expr));
            }
        }
        Expr::ObjectLiteral(v) => {
            for f in &v.fields {
                visit(NodeRef::Expr(&f.value));
            }
        }
        Expr::Await(v) => visit(NodeRef::Expr(&v.expr)),
        Expr::Select(v) => {
            for c in &v.cases {
                match &c.kind {
                    SelectCaseKind::Recv { channel, .. } => visit(NodeRef::Expr(channel)),
                    SelectCaseKind::Send { channel, value } => {
                        visit(NodeRef::Expr(channel));
                        visit(NodeRef::Expr(value));
                    }
                    SelectCaseKind::Timeout { millis } => visit(NodeRef::Expr(millis)),
                    SelectCaseKind::Join { task, .. } => visit(NodeRef::Expr(task)),
                    SelectCaseKind::Default => {}
                }
                for st in &c.body.stmts {
                    visit(NodeRef::Stmt(st));
                }
            }
        }
        Expr::Ternary(v) => {
            visit(NodeRef::Expr(&v.condition));
            visit(NodeRef::Expr(&v.then_expr));
            visit(NodeRef::Expr(&v.else_expr));
        }
        Expr::Range(v) => {
            visit(NodeRef::Expr(&v.start));
            visit(NodeRef::Expr(&v.end));
        }
        Expr::Lambda(v) => match &v.body {
            LambdaBody::Expr(v) => visit(NodeRef::Expr(v)),
            LambdaBody::Block(b) => {
                for st in &b.stmts {
                    visit(NodeRef::Stmt(st));
                }
            }
        },
        Expr::Match(v) => {
            visit(NodeRef::Expr(&v.scrutinee));
            for arm in &v.arms {
                match &arm.body {
                    MatchBody::Expr(v) => visit(NodeRef::Expr(v)),
                    MatchBody::Block(b) => {
                        for st in &b.stmts {
                            visit(NodeRef::Stmt(st));
                        }
                    }
                }
            }
        }
        Expr::ArrayLiteral(v, ..) => {
            for v in v {
                visit(NodeRef::Expr(v));
            }
        }
        Expr::Index(a, b, ..) => {
            visit(NodeRef::Expr(a));
            visit(NodeRef::Expr(b));
        }
        Expr::Integer(..)
        | Expr::Float(..)
        | Expr::Bool(..)
        | Expr::String(..)
        | Expr::Var(..)
        | Expr::StaticField(..) => {}
    }
}
fn stmt_children<'a>(node: &'a Stmt, visit: &mut impl FnMut(NodeRef<'a>)) {
    match node {
        Stmt::Let(v) => visit(NodeRef::Expr(&v.init)),
        Stmt::Assign(v) => visit(NodeRef::Expr(&v.value)),
        Stmt::FieldAssign(v) => {
            visit(NodeRef::Expr(&v.object));
            visit(NodeRef::Expr(&v.value));
        }
        Stmt::SuperInit(v) => {
            for a in &v.args {
                visit(NodeRef::Expr(&a.expr));
            }
        }
        Stmt::StaticFieldAssign(v) => visit(NodeRef::Expr(&v.value)),
        Stmt::IndexAssign(v) => {
            visit(NodeRef::Expr(&v.array));
            visit(NodeRef::Expr(&v.index));
            visit(NodeRef::Expr(&v.value));
        }
        Stmt::If(v) => {
            visit(NodeRef::Expr(&v.cond));
            for st in &v.then_block.stmts {
                visit(NodeRef::Stmt(st));
            }
            if let Some(b) = &v.else_block {
                for st in &b.stmts {
                    visit(NodeRef::Stmt(st));
                }
            }
        }
        Stmt::While(v) => {
            visit(NodeRef::Expr(&v.cond));
            for st in &v.body.stmts {
                visit(NodeRef::Stmt(st));
            }
        }
        Stmt::Defer(v) => match &v.body {
            DeferBody::Expr(v) => visit(NodeRef::Expr(v)),
            DeferBody::Block(b) => {
                for st in &b.stmts {
                    visit(NodeRef::Stmt(st));
                }
            }
        },
        Stmt::Lock(v) => {
            visit(NodeRef::Expr(&v.target));
            for st in &v.body.stmts {
                visit(NodeRef::Stmt(st));
            }
        }
        Stmt::For(v) => {
            visit(NodeRef::Expr(&v.iterable));
            for st in &v.body.stmts {
                visit(NodeRef::Stmt(st));
            }
        }
        Stmt::Return(v) => {
            if let Some(v) = &v.value {
                visit(NodeRef::Expr(v));
            }
        }
        Stmt::Expr(v) => visit(NodeRef::Expr(&v.expr)),
        Stmt::Break(..) | Stmt::Continue(..) => {}
    }
}
impl<'a> NodeRef<'a> {
    /// Visit direct children in field order without temporary allocation.
    /// Immutable and mutable visitors use the same order. The flat wire format
    /// separately groups expressions before statements for compatibility.
    pub(crate) fn for_each_child(self, mut visit: impl FnMut(Self)) {
        match self {
            Self::Expr(v) => expr_children(v, &mut visit),
            Self::Stmt(v) => stmt_children(v, &mut visit),
        }
    }
}
fn expr_children_mut<'a>(node: &'a mut Expr, visit: &mut impl FnMut(NodeMut<'a>)) {
    match node {
        Expr::Binary(v) => {
            visit(NodeMut::Expr(&mut v.lhs));
            visit(NodeMut::Expr(&mut v.rhs));
        }
        Expr::Unary(v) => visit(NodeMut::Expr(&mut v.expr)),
        Expr::Call(v) => {
            for a in &mut v.args {
                visit(NodeMut::Expr(&mut a.expr));
            }
        }
        Expr::FieldAccess(v, ..) | Expr::Print(v, ..) | Expr::TryPropagate(v, ..) => {
            visit(NodeMut::Expr(v))
        }
        Expr::MethodCall(v) => {
            visit(NodeMut::Expr(&mut v.object));
            for a in &mut v.args {
                visit(NodeMut::Expr(&mut a.expr));
            }
        }
        Expr::StaticCall(v) => {
            for a in &mut v.args {
                visit(NodeMut::Expr(&mut a.expr));
            }
        }
        Expr::New(v) => {
            for a in &mut v.args {
                visit(NodeMut::Expr(&mut a.expr));
            }
        }
        Expr::ObjectLiteral(v) => {
            for f in &mut v.fields {
                visit(NodeMut::Expr(&mut f.value));
            }
        }
        Expr::Await(v) => visit(NodeMut::Expr(&mut v.expr)),
        Expr::Select(v) => {
            for c in &mut v.cases {
                match &mut c.kind {
                    SelectCaseKind::Recv { channel, .. } => visit(NodeMut::Expr(channel)),
                    SelectCaseKind::Send { channel, value } => {
                        visit(NodeMut::Expr(channel));
                        visit(NodeMut::Expr(value));
                    }
                    SelectCaseKind::Timeout { millis } => visit(NodeMut::Expr(millis)),
                    SelectCaseKind::Join { task, .. } => visit(NodeMut::Expr(task)),
                    SelectCaseKind::Default => {}
                }
                for st in &mut c.body.stmts {
                    visit(NodeMut::Stmt(st));
                }
            }
        }
        Expr::Ternary(v) => {
            visit(NodeMut::Expr(&mut v.condition));
            visit(NodeMut::Expr(&mut v.then_expr));
            visit(NodeMut::Expr(&mut v.else_expr));
        }
        Expr::Range(v) => {
            visit(NodeMut::Expr(&mut v.start));
            visit(NodeMut::Expr(&mut v.end));
        }
        Expr::Lambda(v) => match &mut v.body {
            LambdaBody::Expr(v) => visit(NodeMut::Expr(v)),
            LambdaBody::Block(b) => {
                for st in &mut b.stmts {
                    visit(NodeMut::Stmt(st));
                }
            }
        },
        Expr::Match(v) => {
            visit(NodeMut::Expr(&mut v.scrutinee));
            for arm in &mut v.arms {
                match &mut arm.body {
                    MatchBody::Expr(v) => visit(NodeMut::Expr(v)),
                    MatchBody::Block(b) => {
                        for st in &mut b.stmts {
                            visit(NodeMut::Stmt(st));
                        }
                    }
                }
            }
        }
        Expr::ArrayLiteral(v, ..) => {
            for v in v {
                visit(NodeMut::Expr(v));
            }
        }
        Expr::Index(a, b, ..) => {
            visit(NodeMut::Expr(a));
            visit(NodeMut::Expr(b));
        }
        Expr::Integer(..)
        | Expr::Float(..)
        | Expr::Bool(..)
        | Expr::String(..)
        | Expr::Var(..)
        | Expr::StaticField(..) => {}
    }
}
fn stmt_children_mut<'a>(node: &'a mut Stmt, visit: &mut impl FnMut(NodeMut<'a>)) {
    match node {
        Stmt::Let(v) => visit(NodeMut::Expr(&mut v.init)),
        Stmt::Assign(v) => visit(NodeMut::Expr(&mut v.value)),
        Stmt::FieldAssign(v) => {
            visit(NodeMut::Expr(&mut v.object));
            visit(NodeMut::Expr(&mut v.value));
        }
        Stmt::SuperInit(v) => {
            for a in &mut v.args {
                visit(NodeMut::Expr(&mut a.expr));
            }
        }
        Stmt::StaticFieldAssign(v) => visit(NodeMut::Expr(&mut v.value)),
        Stmt::IndexAssign(v) => {
            visit(NodeMut::Expr(&mut v.array));
            visit(NodeMut::Expr(&mut v.index));
            visit(NodeMut::Expr(&mut v.value));
        }
        Stmt::If(v) => {
            visit(NodeMut::Expr(&mut v.cond));
            for st in &mut v.then_block.stmts {
                visit(NodeMut::Stmt(st));
            }
            if let Some(b) = &mut v.else_block {
                for st in &mut b.stmts {
                    visit(NodeMut::Stmt(st));
                }
            }
        }
        Stmt::While(v) => {
            visit(NodeMut::Expr(&mut v.cond));
            for st in &mut v.body.stmts {
                visit(NodeMut::Stmt(st));
            }
        }
        Stmt::Defer(v) => match &mut v.body {
            DeferBody::Expr(v) => visit(NodeMut::Expr(v)),
            DeferBody::Block(b) => {
                for st in &mut b.stmts {
                    visit(NodeMut::Stmt(st));
                }
            }
        },
        Stmt::Lock(v) => {
            visit(NodeMut::Expr(&mut v.target));
            for st in &mut v.body.stmts {
                visit(NodeMut::Stmt(st));
            }
        }
        Stmt::For(v) => {
            visit(NodeMut::Expr(&mut v.iterable));
            for st in &mut v.body.stmts {
                visit(NodeMut::Stmt(st));
            }
        }
        Stmt::Return(v) => {
            if let Some(v) = &mut v.value {
                visit(NodeMut::Expr(v));
            }
        }
        Stmt::Expr(v) => visit(NodeMut::Expr(&mut v.expr)),
        Stmt::Break(..) | Stmt::Continue(..) => {}
    }
}
impl<'a> NodeMut<'a> {
    /// Visit direct children in field order without temporary allocation.
    /// Immutable and mutable visitors use the same order. The flat wire format
    /// separately groups expressions before statements for compatibility.
    pub(crate) fn for_each_child(self, mut visit: impl FnMut(Self)) {
        match self {
            Self::Expr(v) => expr_children_mut(v, &mut visit),
            Self::Stmt(v) => stmt_children_mut(v, &mut visit),
        }
    }
}
// Shell construction is explicit: ordinary Clone always clones a complete tree.
// Container recursion stops at Expr/Stmt slots, so its depth depends only on
// the fixed AST schema, never on input nesting. Metadata keeps its normal Clone.
trait CloneShell: Sized {
    fn clone_shell(&self) -> Self;
}
impl CloneShell for Expr {
    fn clone_shell(&self) -> Self {
        placeholder_expr()
    }
}
impl CloneShell for Stmt {
    fn clone_shell(&self) -> Self {
        Stmt::Break(Span::dummy())
    }
}
impl<T: CloneShell> CloneShell for Box<T> {
    fn clone_shell(&self) -> Self {
        Box::new((**self).clone_shell())
    }
}
impl<T: CloneShell> CloneShell for Vec<T> {
    fn clone_shell(&self) -> Self {
        self.iter().map(CloneShell::clone_shell).collect()
    }
}
impl<T: CloneShell> CloneShell for Option<T> {
    fn clone_shell(&self) -> Self {
        self.as_ref().map(CloneShell::clone_shell)
    }
}
// Exhaustive destructuring forces every added field to be classified as
// metadata or syntax. No struct update syntax may hide a new recursive field.
macro_rules! clone_shell_struct {
    ($ty:ident { metadata: [$($meta:ident),*], syntax: [$($child:ident),*] }) => {
        impl CloneShell for $ty {
            fn clone_shell(&self) -> Self {
                let Self { $($meta,)* $($child,)* } = self;
                Self {
                    $($meta: $meta.clone(),)*
                    $($child: $child.clone_shell(),)*
                }
            }
        }
    };
}
clone_shell_struct!(CallArg {
    metadata: [mode, span],
    syntax: [expr]
});
clone_shell_struct!(Block {
    metadata: [id, span],
    syntax: [stmts]
});
clone_shell_struct!(BinaryExpr {
    metadata: [id, op, span],
    syntax: [lhs, rhs]
});
clone_shell_struct!(UnaryExpr {
    metadata: [id, op, span],
    syntax: [expr]
});
clone_shell_struct!(CallExpr {
    metadata: [id, callee, span],
    syntax: [args]
});
clone_shell_struct!(MethodCallExpr {
    metadata: [id, method, span],
    syntax: [object, args]
});
clone_shell_struct!(StaticCallExpr {
    metadata: [id, class, type_args, method, span],
    syntax: [args]
});
clone_shell_struct!(NewExpr {
    metadata: [id, class_name, type_args, span],
    syntax: [args]
});
clone_shell_struct!(ObjectLiteralExpr {
    metadata: [id, class, span],
    syntax: [fields]
});
clone_shell_struct!(ObjectLiteralField {
    metadata: [name, span],
    syntax: [value]
});
clone_shell_struct!(AwaitExpr {
    metadata: [id, span],
    syntax: [expr]
});
clone_shell_struct!(SelectExpr {
    metadata: [id, span],
    syntax: [cases]
});
clone_shell_struct!(SelectCase {
    metadata: [span],
    syntax: [kind, body]
});
clone_shell_struct!(TernaryExpr {
    metadata: [id, span],
    syntax: [condition, then_expr, else_expr]
});
clone_shell_struct!(RangeExpr {
    metadata: [id, span],
    syntax: [start, end]
});
clone_shell_struct!(LambdaExpr {
    metadata: [id, params, return_type, span],
    syntax: [body]
});
clone_shell_struct!(MatchExpr {
    metadata: [id, span],
    syntax: [scrutinee, arms]
});
clone_shell_struct!(MatchArm {
    metadata: [pattern, span],
    syntax: [body]
});
clone_shell_struct!(LetStmt {
    metadata: [name, mutable, ty, span],
    syntax: [init]
});
clone_shell_struct!(AssignStmt {
    metadata: [name, span],
    syntax: [value]
});
clone_shell_struct!(FieldAssignStmt {
    metadata: [field, span],
    syntax: [object, value]
});
clone_shell_struct!(SuperInitStmt {
    metadata: [span],
    syntax: [args]
});
clone_shell_struct!(StaticFieldAssignStmt {
    metadata: [class, field, span],
    syntax: [value]
});
clone_shell_struct!(IndexAssignStmt {
    metadata: [span],
    syntax: [array, index, value]
});
clone_shell_struct!(IfStmt {
    metadata: [span],
    syntax: [cond, then_block, else_block]
});
clone_shell_struct!(WhileStmt {
    metadata: [span],
    syntax: [cond, body]
});
clone_shell_struct!(DeferStmt {
    metadata: [span],
    syntax: [body]
});
clone_shell_struct!(LockStmt {
    metadata: [mode, binding, binding_span, mutable, span],
    syntax: [target, body]
});
clone_shell_struct!(ForStmt {
    metadata: [name, name_span, span],
    syntax: [iterable, body]
});
clone_shell_struct!(ReturnStmt {
    metadata: [span],
    syntax: [value]
});
clone_shell_struct!(ExprStmt {
    metadata: [span],
    syntax: [expr]
});
macro_rules! clone_shell_body {
    ($ty:ident) => {
        impl CloneShell for $ty {
            fn clone_shell(&self) -> Self {
                match self {
                    Self::Expr(expr) => Self::Expr(expr.clone_shell()),
                    Self::Block(block) => Self::Block(block.clone_shell()),
                }
            }
        }
    };
}
clone_shell_body!(LambdaBody);
clone_shell_body!(MatchBody);
clone_shell_body!(DeferBody);
impl CloneShell for SelectCaseKind {
    fn clone_shell(&self) -> Self {
        match self {
            Self::Recv { binding, channel } => Self::Recv {
                binding: binding.clone(),
                channel: channel.clone_shell(),
            },
            Self::Send { channel, value } => Self::Send {
                channel: channel.clone_shell(),
                value: value.clone_shell(),
            },
            Self::Timeout { millis } => Self::Timeout {
                millis: millis.clone_shell(),
            },
            Self::Join { binding, task } => Self::Join {
                binding: binding.clone(),
                task: task.clone_shell(),
            },
            Self::Default => Self::Default,
        }
    }
}
impl Expr {
    fn shallow_clone(&self) -> Self {
        match self {
            Self::Binary(v) => Self::Binary(v.clone_shell()),
            Self::Unary(v) => Self::Unary(v.clone_shell()),
            Self::Call(v) => Self::Call(v.clone_shell()),
            Self::MethodCall(v) => Self::MethodCall(v.clone_shell()),
            Self::StaticCall(v) => Self::StaticCall(v.clone_shell()),
            Self::StaticField(v) => Self::StaticField(v.clone()),
            Self::New(v) => Self::New(v.clone_shell()),
            Self::ObjectLiteral(v) => Self::ObjectLiteral(v.clone_shell()),
            Self::Await(v) => Self::Await(v.clone_shell()),
            Self::Select(v) => Self::Select(v.clone_shell()),
            Self::Ternary(v) => Self::Ternary(v.clone_shell()),
            Self::Range(v) => Self::Range(v.clone_shell()),
            Self::Lambda(v) => Self::Lambda(v.clone_shell()),
            Self::Match(v) => Self::Match(v.clone_shell()),
            Self::Integer(v, s, id) => Self::Integer(*v, *s, *id),
            Self::Float(v, s, id) => Self::Float(*v, *s, *id),
            Self::Bool(v, s, id) => Self::Bool(*v, *s, *id),
            Self::String(v, s, id) => Self::String(v.clone(), *s, *id),
            Self::Var(v, s, id) => Self::Var(v.clone(), *s, *id),
            Self::FieldAccess(v, n, s, id) => {
                Self::FieldAccess(v.clone_shell(), n.clone(), *s, *id)
            }
            Self::Print(v, n, s, id) => Self::Print(v.clone_shell(), *n, *s, *id),
            Self::TryPropagate(v, s, id) => Self::TryPropagate(v.clone_shell(), *s, *id),
            Self::ArrayLiteral(v, s, id) => Self::ArrayLiteral(v.clone_shell(), *s, *id),
            Self::Index(a, b, s, id) => Self::Index(a.clone_shell(), b.clone_shell(), *s, *id),
        }
    }
}
impl Clone for Expr {
    fn clone(&self) -> Self {
        let mut result = self.shallow_clone();
        clone_descendants(NodeRef::Expr(self), NodeMut::Expr(&mut result));
        result
    }
}
impl Stmt {
    fn shallow_clone(&self) -> Self {
        match self {
            Self::Let(v) => Self::Let(v.clone_shell()),
            Self::Assign(v) => Self::Assign(v.clone_shell()),
            Self::FieldAssign(v) => Self::FieldAssign(v.clone_shell()),
            Self::SuperInit(v) => Self::SuperInit(v.clone_shell()),
            Self::StaticFieldAssign(v) => Self::StaticFieldAssign(v.clone_shell()),
            Self::IndexAssign(v) => Self::IndexAssign(v.clone_shell()),
            Self::If(v) => Self::If(v.clone_shell()),
            Self::While(v) => Self::While(v.clone_shell()),
            Self::Break(v) => Self::Break(*v),
            Self::Continue(v) => Self::Continue(*v),
            Self::Defer(v) => Self::Defer(v.clone_shell()),
            Self::Lock(v) => Self::Lock(v.clone_shell()),
            Self::For(v) => Self::For(v.clone_shell()),
            Self::Return(v) => Self::Return(v.clone_shell()),
            Self::Expr(v) => Self::Expr(v.clone_shell()),
        }
    }
}
impl Clone for Stmt {
    fn clone(&self) -> Self {
        let mut result = self.shallow_clone();
        clone_descendants(NodeRef::Stmt(self), NodeMut::Stmt(&mut result));
        result
    }
}
fn clone_descendants(source: NodeRef<'_>, target: NodeMut<'_>) {
    let mut work = vec![(source, target)];
    let mut source_children = Vec::new();
    while let Some((source, target)) = work.pop() {
        source.for_each_child(|child| source_children.push(child));
        let mut children = source_children.drain(..);
        target.for_each_child(|target| {
            let source = children.next().expect("syntax child count");
            match (source, target) {
                (NodeRef::Expr(a), NodeMut::Expr(b)) => {
                    *b = a.shallow_clone();
                    work.push((NodeRef::Expr(a), NodeMut::Expr(b)));
                }
                (NodeRef::Stmt(a), NodeMut::Stmt(b)) => {
                    *b = a.shallow_clone();
                    work.push((NodeRef::Stmt(a), NodeMut::Stmt(b)));
                }
                _ => unreachable!("syntax child kind"),
            }
        });
        assert!(children.next().is_none(), "syntax child count");
    }
}
impl NodeOwned {
    pub(crate) fn as_mut(&mut self) -> NodeMut<'_> {
        match self {
            Self::Expr(v) => NodeMut::Expr(v),
            Self::Stmt(v) => NodeMut::Stmt(v),
        }
    }
}
impl NodeMut<'_> {
    pub(crate) fn take(self) -> NodeOwned {
        match self {
            Self::Expr(v) => NodeOwned::Expr(std::mem::replace(v, placeholder_expr())),
            Self::Stmt(v) => {
                NodeOwned::Stmt(Box::new(std::mem::replace(v, Stmt::Break(Span::dummy()))))
            }
        }
    }
}
fn drain_child(child: NodeMut<'_>, work: &mut Vec<NodeOwned>) {
    // Leaves can drop in place. In particular, do not enqueue placeholders
    // left in detached shells: their normal Drop visits those leaves again.
    match child {
        NodeMut::Expr(
            Expr::Integer(..)
            | Expr::Float(..)
            | Expr::Bool(..)
            | Expr::String(..)
            | Expr::Var(..)
            | Expr::StaticField(..),
        )
        | NodeMut::Stmt(Stmt::Break(..) | Stmt::Continue(..)) => {}
        child => work.push(child.take()),
    }
}
fn drain(root: NodeMut<'_>) {
    let mut work = Vec::new();
    root.for_each_child(|child| drain_child(child, &mut work));
    while let Some(mut node) = work.pop() {
        node.as_mut()
            .for_each_child(|child| drain_child(child, &mut work));
    }
}
impl Drop for Expr {
    fn drop(&mut self) {
        drain(NodeMut::Expr(self));
    }
}
impl Drop for Stmt {
    fn drop(&mut self) {
        drain(NodeMut::Stmt(self));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;
    fn parse(source: &str) -> Program {
        let tokens = Lexer::new(source).tokenize().unwrap();
        let (program, errors) = Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        program
    }
    // Exhaustive matches make adding variants a compile-time coverage decision.
    fn expr_kind(expr: &Expr) -> &'static str {
        match expr {
            Expr::Integer(..) => "Integer",
            Expr::Float(..) => "Float",
            Expr::Bool(..) => "Bool",
            Expr::String(..) => "String",
            Expr::Var(..) => "Var",
            Expr::Binary(..) => "Binary",
            Expr::Unary(..) => "Unary",
            Expr::Call(..) => "Call",
            Expr::FieldAccess(..) => "FieldAccess",
            Expr::MethodCall(..) => "MethodCall",
            Expr::StaticCall(..) => "StaticCall",
            Expr::StaticField(..) => "StaticField",
            Expr::New(..) => "New",
            Expr::ObjectLiteral(..) => "ObjectLiteral",
            Expr::Await(..) => "Await",
            Expr::Select(..) => "Select",
            Expr::Print(..) => "Print",
            Expr::Ternary(..) => "Ternary",
            Expr::Range(..) => "Range",
            Expr::Lambda(..) => "Lambda",
            Expr::Match(..) => "Match",
            Expr::TryPropagate(..) => "TryPropagate",
            Expr::ArrayLiteral(..) => "ArrayLiteral",
            Expr::Index(..) => "Index",
        }
    }
    fn stmt_kind(stmt: &Stmt) -> &'static str {
        match stmt {
            Stmt::Let(..) => "Let",
            Stmt::Assign(..) => "Assign",
            Stmt::FieldAssign(..) => "FieldAssign",
            Stmt::SuperInit(..) => "SuperInit",
            Stmt::StaticFieldAssign(..) => "StaticFieldAssign",
            Stmt::IndexAssign(..) => "IndexAssign",
            Stmt::If(..) => "If",
            Stmt::While(..) => "While",
            Stmt::Break(..) => "Break",
            Stmt::Continue(..) => "Continue",
            Stmt::Defer(..) => "Defer",
            Stmt::Lock(..) => "Lock",
            Stmt::For(..) => "For",
            Stmt::Return(..) => "Return",
            Stmt::Expr(..) => "Expr",
        }
    }
    fn pattern_kind(pattern: &Pattern) -> &'static str {
        match pattern {
            Pattern::Wildcard(..) => "Wildcard",
            Pattern::Binding { .. } => "Binding",
            Pattern::LiteralBool(..) => "LiteralBool",
            Pattern::LiteralInt(..) => "LiteralInt",
            Pattern::EnumVariant { .. } => "EnumVariant",
            Pattern::EnumVariantTuple { .. } => "EnumVariantTuple",
            Pattern::ClassDowncast { .. } => "ClassDowncast",
        }
    }
    #[test]
    fn clone_preserves_every_syntax_variant_and_child_slot() {
        use std::collections::BTreeSet;
        let program = parse(include_str!(
            "../../tests/fixtures/ast_ownership_all_syntax.wi"
        ));
        let cloned = program.clone();
        // Debug derives visit fields independently of ownership and serialization;
        // a child omitted by BOTH ownership visitors must still fail this check.
        assert_eq!(format!("{program:?}"), format!("{cloned:?}"));
        assert_eq!(
            serde_json::to_string(&program).unwrap(),
            serde_json::to_string(&cloned).unwrap()
        );
        let mut exprs = BTreeSet::new();
        let mut stmts = BTreeSet::new();
        let mut patterns = BTreeSet::new();
        let Item::Function(function) = &program.items[0] else {
            panic!("fixture function")
        };
        let mut pending: Vec<_> = function.body.stmts.iter().map(NodeRef::Stmt).collect();
        while let Some(node) = pending.pop() {
            match &node {
                NodeRef::Expr(expr) => {
                    exprs.insert(expr_kind(expr));
                    if let Expr::Match(expr) = expr {
                        for arm in &expr.arms {
                            patterns.insert(pattern_kind(&arm.pattern));
                        }
                    }
                }
                NodeRef::Stmt(stmt) => {
                    stmts.insert(stmt_kind(stmt));
                }
            }
            node.for_each_child(|child| pending.push(child));
        }
        assert_eq!(
            exprs,
            BTreeSet::from([
                "Integer",
                "Float",
                "Bool",
                "String",
                "Var",
                "Binary",
                "Unary",
                "Call",
                "FieldAccess",
                "MethodCall",
                "StaticCall",
                "StaticField",
                "New",
                "ObjectLiteral",
                "Await",
                "Select",
                "Print",
                "Ternary",
                "Range",
                "Lambda",
                "Match",
                "TryPropagate",
                "ArrayLiteral",
                "Index"
            ])
        );
        assert_eq!(
            stmts,
            BTreeSet::from([
                "Let",
                "Assign",
                "FieldAssign",
                "SuperInit",
                "StaticFieldAssign",
                "IndexAssign",
                "If",
                "While",
                "Break",
                "Continue",
                "Defer",
                "Lock",
                "For",
                "Return",
                "Expr"
            ])
        );
        assert_eq!(
            patterns,
            BTreeSet::from([
                "Wildcard",
                "Binding",
                "LiteralBool",
                "LiteralInt",
                "EnumVariant",
                "EnumVariantTuple",
                "ClassDowncast"
            ])
        );

        // Container Clone must retain full descendants outside shell construction.
        for stmt in &function.body.stmts {
            match stmt {
                Stmt::Let(value) => {
                    assert_eq!(format!("{value:?}"), format!("{:?}", value.clone()))
                }
                Stmt::Expr(value) => {
                    assert_eq!(format!("{value:?}"), format!("{:?}", value.clone()))
                }
                _ => {}
            }
        }
    }
    #[test]
    fn syntax_ownership_twenty_four_perspectives() {
        let cases = [
            "1",
            "1.5",
            "true",
            "\"text\"",
            "x",
            "1+2",
            "-1",
            "f(1)",
            "x.field",
            "x.method(1)",
            "C::make(1)",
            "C::field",
            "new C(1)",
            "C { field: 1 }",
            "await task",
            "print(1)",
            "true ? 1 : 2",
            "1..2",
            "|x: i64| x",
            "match x { _ => 1 }",
            "x?",
            "[1,2]",
            "x[1]",
            "println(1)",
        ];
        for expression in cases {
            let program = parse(&format!("fn main() {{ let value = {expression}; }}"));
            let clone = program.clone();
            assert_eq!(
                serde_json::to_string(&program).unwrap(),
                serde_json::to_string(&clone).unwrap(),
                "{expression}"
            );
        }
    }
    #[test]
    fn syntax_clone_drop_fifty_thousand_mixed_nodes_on_one_mib() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let span = Span::dummy();
                let mut expr = placeholder_expr();
                for index in 0..50_000 {
                    expr = match index % 4 {
                        0 => Expr::Print(Box::new(expr), false, span, ExprId::fresh()),
                        1 => Expr::ArrayLiteral(vec![expr], span, ExprId::fresh()),
                        2 => Expr::Lambda(Box::new(LambdaExpr {
                            id: ExprId::fresh(),
                            params: vec![],
                            return_type: None,
                            body: LambdaBody::Block(Block {
                                id: crate::parser::ast::BodyId::fresh(),
                                stmts: vec![Stmt::Return(ReturnStmt {
                                    value: Some(expr),
                                    span,
                                })],
                                span,
                            }),
                            span,
                        })),
                        _ => Expr::Index(
                            Box::new(expr),
                            Box::new(placeholder_expr()),
                            span,
                            ExprId::fresh(),
                        ),
                    };
                }
                let clone = expr.clone();
                assert_eq!(expr.id(), clone.id());
                drop(expr);
                drop(clone);
                let mut stmt = Stmt::Break(span);
                for _ in 0..50_000 {
                    stmt = Stmt::While(WhileStmt {
                        cond: placeholder_expr(),
                        body: Block {
                            id: crate::parser::ast::BodyId::fresh(),
                            stmts: vec![stmt],
                            span,
                        },
                        span,
                    });
                }
                drop(stmt.clone());
                drop(stmt);
            })
            .unwrap()
            .join()
            .unwrap();
    }
    #[test]
    fn child_visits_scale_with_edges() {
        for size in [64, 256, 1024, 4096] {
            for fanout in [false, true] {
                let mut root = placeholder_expr();
                if fanout {
                    root = Expr::ArrayLiteral(
                        (0..size).map(|_| placeholder_expr()).collect(),
                        Span::dummy(),
                        ExprId::fresh(),
                    );
                } else {
                    for _ in 0..size {
                        root = Expr::Print(Box::new(root), false, Span::dummy(), ExprId::fresh());
                    }
                }
                for repetition in 0..3 {
                    let mut immutable_edges = 0;
                    let mut pending = vec![NodeRef::Expr(&root)];
                    while let Some(node) = pending.pop() {
                        node.for_each_child(|child| {
                            immutable_edges += 1;
                            pending.push(child);
                        });
                    }
                    let mut mutable_edges = 0;
                    let mut pending = vec![NodeMut::Expr(&mut root)];
                    while let Some(node) = pending.pop() {
                        node.for_each_child(|child| {
                            mutable_edges += 1;
                            pending.push(child);
                        });
                    }
                    assert_eq!((immutable_edges, mutable_edges), (size, size));
                    let clone = root.clone();
                    let mut cloned_edges = 0;
                    let mut pending = vec![NodeRef::Expr(&clone)];
                    while let Some(node) = pending.pop() {
                        node.for_each_child(|child| {
                            cloned_edges += 1;
                            pending.push(child);
                        });
                    }
                    assert_eq!(cloned_edges, size);
                    if repetition == 2 {
                        println!(
                            "size={size} fanout={fanout} repeats=3 source_edges={immutable_edges} mutable_edges={mutable_edges} clone_edges={cloned_edges}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn parser_nested_grammar_uses_one_mib_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                for (prefix, suffix) in [
                    ("(", ")"),
                    ("f(", ")"),
                    ("[", "]"),
                    ("new C(", ")"),
                    ("x.m(", ")"),
                    ("match x { _ => ", " }"),
                    ("|| { return ", "; }"),
                    ("true ? 1 : ", ""),
                ] {
                    let expression = format!("{}1{}", prefix.repeat(5_000), suffix.repeat(5_000));
                    drop(parse(&format!("fn main() {{ let x = {expression}; }}")));
                }
                let body = format!("{}return;{}", "if true {".repeat(5_000), "}".repeat(5_000));
                drop(parse(&format!("fn main() {{{body}}}")));
                for (prefix, suffix) in [("Array<", ">"), ("fn() -> ", ""), ("closure(", ")")] {
                    let ty = format!("{}i64{}", prefix.repeat(10_000), suffix.repeat(10_000));
                    drop(parse(&format!("fn f(x: {ty}) {{}}")));
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
