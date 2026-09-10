//! Shared heap-backed ownership traversal for syntax trees.
use super::ast::*;
use crate::diagnostics::Span;
use std::cell::Cell;
thread_local! { static SHALLOW: Cell<bool> = const { Cell::new(false) }; }
struct ShallowGuard(bool);
impl ShallowGuard {
    fn enter() -> Self {
        Self(SHALLOW.with(|s| s.replace(true)))
    }
}
impl Drop for ShallowGuard {
    fn drop(&mut self) {
        SHALLOW.with(|s| s.set(self.0));
    }
}
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
fn expr_children<'a>(node: &'a Expr) -> Vec<NodeRef<'a>> {
    let mut es = Vec::new();
    let mut ss = Vec::new();
    let mut e = |v: &'a Expr| es.push(NodeRef::Expr(v));
    let mut s = |v: &'a Stmt| ss.push(NodeRef::Stmt(v));
    match node {
        Expr::Binary(v) => {
            e(&v.lhs);
            e(&v.rhs);
        }
        Expr::Unary(v) => e(&v.expr),
        Expr::Call(v) => {
            for a in &v.args {
                e(&a.expr);
            }
        }
        Expr::FieldAccess(v, ..) | Expr::Print(v, ..) | Expr::TryPropagate(v, ..) => e(v),
        Expr::MethodCall(v) => {
            e(&v.object);
            for a in &v.args {
                e(&a.expr);
            }
        }
        Expr::StaticCall(v) => {
            for a in &v.args {
                e(&a.expr);
            }
        }
        Expr::New(v) => {
            for a in &v.args {
                e(&a.expr);
            }
        }
        Expr::ObjectLiteral(v) => {
            for f in &v.fields {
                e(&f.value);
            }
        }
        Expr::Await(v) => e(&v.expr),
        Expr::Select(v) => {
            for c in &v.cases {
                match &c.kind {
                    SelectCaseKind::Recv { channel, .. } => e(channel),
                    SelectCaseKind::Send { channel, value } => {
                        e(channel);
                        e(value);
                    }
                    SelectCaseKind::Timeout { millis } => e(millis),
                    SelectCaseKind::Join { task, .. } => e(task),
                    SelectCaseKind::Default => {}
                }
                for st in &c.body.stmts {
                    s(st);
                }
            }
        }
        Expr::Ternary(v) => {
            e(&v.condition);
            e(&v.then_expr);
            e(&v.else_expr);
        }
        Expr::Range(v) => {
            e(&v.start);
            e(&v.end);
        }
        Expr::Lambda(v) => match &v.body {
            LambdaBody::Expr(v) => e(v),
            LambdaBody::Block(b) => {
                for st in &b.stmts {
                    s(st);
                }
            }
        },
        Expr::Match(v) => {
            e(&v.scrutinee);
            for arm in &v.arms {
                match &arm.body {
                    MatchBody::Expr(v) => e(v),
                    MatchBody::Block(b) => {
                        for st in &b.stmts {
                            s(st);
                        }
                    }
                }
            }
        }
        Expr::ArrayLiteral(v, ..) => {
            for v in v {
                e(v);
            }
        }
        Expr::Index(a, b, ..) => {
            e(a);
            e(b);
        }
        Expr::Integer(..)
        | Expr::Float(..)
        | Expr::Bool(..)
        | Expr::String(..)
        | Expr::Var(..)
        | Expr::StaticField(..) => {}
    }
    es.extend(ss);
    es
}
fn stmt_children<'a>(node: &'a Stmt) -> Vec<NodeRef<'a>> {
    let mut es = Vec::new();
    let mut ss = Vec::new();
    let mut e = |v: &'a Expr| es.push(NodeRef::Expr(v));
    let mut s = |v: &'a Stmt| ss.push(NodeRef::Stmt(v));
    match node {
        Stmt::Let(v) => e(&v.init),
        Stmt::Assign(v) => e(&v.value),
        Stmt::FieldAssign(v) => {
            e(&v.object);
            e(&v.value);
        }
        Stmt::SuperInit(v) => {
            for a in &v.args {
                e(&a.expr);
            }
        }
        Stmt::StaticFieldAssign(v) => e(&v.value),
        Stmt::IndexAssign(v) => {
            e(&v.array);
            e(&v.index);
            e(&v.value);
        }
        Stmt::If(v) => {
            e(&v.cond);
            for st in &v.then_block.stmts {
                s(st);
            }
            if let Some(b) = &v.else_block {
                for st in &b.stmts {
                    s(st);
                }
            }
        }
        Stmt::While(v) => {
            e(&v.cond);
            for st in &v.body.stmts {
                s(st);
            }
        }
        Stmt::Defer(v) => match &v.body {
            DeferBody::Expr(v) => e(v),
            DeferBody::Block(b) => {
                for st in &b.stmts {
                    s(st);
                }
            }
        },
        Stmt::Lock(v) => {
            e(&v.target);
            for st in &v.body.stmts {
                s(st);
            }
        }
        Stmt::For(v) => {
            e(&v.iterable);
            for st in &v.body.stmts {
                s(st);
            }
        }
        Stmt::Return(v) => {
            if let Some(v) = &v.value {
                e(v);
            }
        }
        Stmt::Expr(v) => e(&v.expr),
        Stmt::Break(..) | Stmt::Continue(..) => {}
    }
    es.extend(ss);
    es
}
impl<'a> NodeRef<'a> {
    pub(crate) fn children(self) -> Vec<Self> {
        match self {
            Self::Expr(v) => expr_children(v),
            Self::Stmt(v) => stmt_children(v),
        }
    }
}
fn expr_children_mut<'a>(node: &'a mut Expr) -> Vec<NodeMut<'a>> {
    let mut es = Vec::new();
    let mut ss = Vec::new();
    let mut e = |v: &'a mut Expr| es.push(NodeMut::Expr(v));
    let mut s = |v: &'a mut Stmt| ss.push(NodeMut::Stmt(v));
    match node {
        Expr::Binary(v) => {
            e(&mut v.lhs);
            e(&mut v.rhs);
        }
        Expr::Unary(v) => e(&mut v.expr),
        Expr::Call(v) => {
            for a in &mut v.args {
                e(&mut a.expr);
            }
        }
        Expr::FieldAccess(v, ..) | Expr::Print(v, ..) | Expr::TryPropagate(v, ..) => e(v),
        Expr::MethodCall(v) => {
            e(&mut v.object);
            for a in &mut v.args {
                e(&mut a.expr);
            }
        }
        Expr::StaticCall(v) => {
            for a in &mut v.args {
                e(&mut a.expr);
            }
        }
        Expr::New(v) => {
            for a in &mut v.args {
                e(&mut a.expr);
            }
        }
        Expr::ObjectLiteral(v) => {
            for f in &mut v.fields {
                e(&mut f.value);
            }
        }
        Expr::Await(v) => e(&mut v.expr),
        Expr::Select(v) => {
            for c in &mut v.cases {
                match &mut c.kind {
                    SelectCaseKind::Recv { channel, .. } => e(channel),
                    SelectCaseKind::Send { channel, value } => {
                        e(channel);
                        e(value);
                    }
                    SelectCaseKind::Timeout { millis } => e(millis),
                    SelectCaseKind::Join { task, .. } => e(task),
                    SelectCaseKind::Default => {}
                }
                for st in &mut c.body.stmts {
                    s(st);
                }
            }
        }
        Expr::Ternary(v) => {
            e(&mut v.condition);
            e(&mut v.then_expr);
            e(&mut v.else_expr);
        }
        Expr::Range(v) => {
            e(&mut v.start);
            e(&mut v.end);
        }
        Expr::Lambda(v) => match &mut v.body {
            LambdaBody::Expr(v) => e(v),
            LambdaBody::Block(b) => {
                for st in &mut b.stmts {
                    s(st);
                }
            }
        },
        Expr::Match(v) => {
            e(&mut v.scrutinee);
            for arm in &mut v.arms {
                match &mut arm.body {
                    MatchBody::Expr(v) => e(v),
                    MatchBody::Block(b) => {
                        for st in &mut b.stmts {
                            s(st);
                        }
                    }
                }
            }
        }
        Expr::ArrayLiteral(v, ..) => {
            for v in v {
                e(v);
            }
        }
        Expr::Index(a, b, ..) => {
            e(a);
            e(b);
        }
        Expr::Integer(..)
        | Expr::Float(..)
        | Expr::Bool(..)
        | Expr::String(..)
        | Expr::Var(..)
        | Expr::StaticField(..) => {}
    }
    es.extend(ss);
    es
}
fn stmt_children_mut<'a>(node: &'a mut Stmt) -> Vec<NodeMut<'a>> {
    let mut es = Vec::new();
    let mut ss = Vec::new();
    let mut e = |v: &'a mut Expr| es.push(NodeMut::Expr(v));
    let mut s = |v: &'a mut Stmt| ss.push(NodeMut::Stmt(v));
    match node {
        Stmt::Let(v) => e(&mut v.init),
        Stmt::Assign(v) => e(&mut v.value),
        Stmt::FieldAssign(v) => {
            e(&mut v.object);
            e(&mut v.value);
        }
        Stmt::SuperInit(v) => {
            for a in &mut v.args {
                e(&mut a.expr);
            }
        }
        Stmt::StaticFieldAssign(v) => e(&mut v.value),
        Stmt::IndexAssign(v) => {
            e(&mut v.array);
            e(&mut v.index);
            e(&mut v.value);
        }
        Stmt::If(v) => {
            e(&mut v.cond);
            for st in &mut v.then_block.stmts {
                s(st);
            }
            if let Some(b) = &mut v.else_block {
                for st in &mut b.stmts {
                    s(st);
                }
            }
        }
        Stmt::While(v) => {
            e(&mut v.cond);
            for st in &mut v.body.stmts {
                s(st);
            }
        }
        Stmt::Defer(v) => match &mut v.body {
            DeferBody::Expr(v) => e(v),
            DeferBody::Block(b) => {
                for st in &mut b.stmts {
                    s(st);
                }
            }
        },
        Stmt::Lock(v) => {
            e(&mut v.target);
            for st in &mut v.body.stmts {
                s(st);
            }
        }
        Stmt::For(v) => {
            e(&mut v.iterable);
            for st in &mut v.body.stmts {
                s(st);
            }
        }
        Stmt::Return(v) => {
            if let Some(v) = &mut v.value {
                e(v);
            }
        }
        Stmt::Expr(v) => e(&mut v.expr),
        Stmt::Break(..) | Stmt::Continue(..) => {}
    }
    es.extend(ss);
    es
}
impl<'a> NodeMut<'a> {
    pub(crate) fn children(self) -> Vec<Self> {
        match self {
            Self::Expr(v) => expr_children_mut(v),
            Self::Stmt(v) => stmt_children_mut(v),
        }
    }
}
impl Expr {
    fn shallow_clone(&self) -> Self {
        let _guard = ShallowGuard::enter();
        match self {
            Self::Binary(v) => Self::Binary(v.clone()),
            Self::Unary(v) => Self::Unary(v.clone()),
            Self::Call(v) => Self::Call(v.clone()),
            Self::MethodCall(v) => Self::MethodCall(v.clone()),
            Self::StaticCall(v) => Self::StaticCall(v.clone()),
            Self::StaticField(v) => Self::StaticField(v.clone()),
            Self::New(v) => Self::New(v.clone()),
            Self::ObjectLiteral(v) => Self::ObjectLiteral(v.clone()),
            Self::Await(v) => Self::Await(v.clone()),
            Self::Select(v) => Self::Select(v.clone()),
            Self::Ternary(v) => Self::Ternary(v.clone()),
            Self::Range(v) => Self::Range(v.clone()),
            Self::Lambda(v) => Self::Lambda(v.clone()),
            Self::Match(v) => Self::Match(v.clone()),
            Self::Integer(v, s, id) => Self::Integer(*v, *s, *id),
            Self::Float(v, s, id) => Self::Float(*v, *s, *id),
            Self::Bool(v, s, id) => Self::Bool(*v, *s, *id),
            Self::String(v, s, id) => Self::String(v.clone(), *s, *id),
            Self::Var(v, s, id) => Self::Var(v.clone(), *s, *id),
            Self::FieldAccess(v, n, s, id) => Self::FieldAccess(v.clone(), n.clone(), *s, *id),
            Self::Print(v, n, s, id) => Self::Print(v.clone(), *n, *s, *id),
            Self::TryPropagate(v, s, id) => Self::TryPropagate(v.clone(), *s, *id),
            Self::ArrayLiteral(v, s, id) => Self::ArrayLiteral(v.clone(), *s, *id),
            Self::Index(a, b, s, id) => Self::Index(a.clone(), b.clone(), *s, *id),
        }
    }
}
impl Clone for Expr {
    fn clone(&self) -> Self {
        if SHALLOW.with(Cell::get) {
            return placeholder_expr();
        }
        let mut result = self.shallow_clone();
        let mut work = vec![(NodeRef::Expr(self), NodeMut::Expr(&mut result))];
        while let Some((source, target)) = work.pop() {
            let source_children = source.children();
            let target_children = target.children();
            for (source, target) in source_children.into_iter().zip(target_children) {
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
            }
        }
        result
    }
}
impl Stmt {
    fn shallow_clone(&self) -> Self {
        let _guard = ShallowGuard::enter();
        match self {
            Self::Let(v) => Self::Let(v.clone()),
            Self::Assign(v) => Self::Assign(v.clone()),
            Self::FieldAssign(v) => Self::FieldAssign(v.clone()),
            Self::SuperInit(v) => Self::SuperInit(v.clone()),
            Self::StaticFieldAssign(v) => Self::StaticFieldAssign(v.clone()),
            Self::IndexAssign(v) => Self::IndexAssign(v.clone()),
            Self::If(v) => Self::If(v.clone()),
            Self::While(v) => Self::While(v.clone()),
            Self::Break(v) => Self::Break(*v),
            Self::Continue(v) => Self::Continue(*v),
            Self::Defer(v) => Self::Defer(v.clone()),
            Self::Lock(v) => Self::Lock(v.clone()),
            Self::For(v) => Self::For(v.clone()),
            Self::Return(v) => Self::Return(v.clone()),
            Self::Expr(v) => Self::Expr(v.clone()),
        }
    }
}
impl Clone for Stmt {
    fn clone(&self) -> Self {
        if SHALLOW.with(Cell::get) {
            return Stmt::Break(Span::dummy());
        }
        let mut result = self.shallow_clone();
        let mut work = vec![(NodeRef::Stmt(self), NodeMut::Stmt(&mut result))];
        while let Some((source, target)) = work.pop() {
            let source_children = source.children();
            let target_children = target.children();
            for (source, target) in source_children.into_iter().zip(target_children) {
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
            }
        }
        result
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
fn drain(root: NodeMut<'_>) {
    let mut work: Vec<_> = root.children().into_iter().map(NodeMut::take).collect();
    while let Some(mut node) = work.pop() {
        work.extend(node.as_mut().children().into_iter().map(NodeMut::take));
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
