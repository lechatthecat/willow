//! Explicit-stack, source-order structural events for read-only AST passes.
//! Exit events allow postorder consumers without recursive callbacks.
use super::ast::*;

#[derive(Clone, Copy)]
pub enum AstEvent<'a> {
    Block(&'a Block),
    Stmt(&'a Stmt),
    Expr(&'a Expr),
    Lambda(&'a LambdaExpr),
    ExitExpr(&'a Expr),
    CallArguments(&'a Expr),
    SuperArguments(&'a SuperInitStmt),
}
pub struct AstWalk<'a> {
    stack: Vec<AstEvent<'a>>,
    pending: Vec<AstEvent<'a>>,
    parent_depth: usize,
}
impl<'a> AstWalk<'a> {
    pub fn new(root: AstEvent<'a>) -> Self {
        Self {
            stack: vec![root],
            pending: Vec::new(),
            parent_depth: 0,
        }
    }
    /// Omit the current node's children and its exit event.
    pub fn skip_children(&mut self) {
        self.stack.truncate(self.parent_depth);
    }

    /// Schedule a selected child after pruning a subtree.
    pub fn push(&mut self, event: AstEvent<'a>) {
        self.stack.push(event);
    }
}
impl<'a> Iterator for AstWalk<'a> {
    type Item = AstEvent<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        let event = self.stack.pop()?;
        self.parent_depth = self.stack.len();
        match event {
            AstEvent::Block(block) => self.pending.extend(block.stmts.iter().map(AstEvent::Stmt)),
            AstEvent::Stmt(stmt) => push_stmt(&mut self.pending, stmt),
            AstEvent::Expr(expr) => {
                self.stack.push(AstEvent::ExitExpr(expr));
                push_expr(&mut self.pending, expr);
            }
            AstEvent::Lambda(lambda) => push_lambda(&mut self.pending, lambda),
            AstEvent::ExitExpr(_) | AstEvent::CallArguments(_) | AstEvent::SuperArguments(_) => {}
        }
        self.stack.extend(self.pending.drain(..).rev());
        Some(event)
    }
}
fn push_args<'a>(pending: &mut Vec<AstEvent<'a>>, args: &'a [CallArg]) {
    pending.extend(args.iter().map(|arg| AstEvent::Expr(&arg.expr)));
}
fn push_stmt<'a>(pending: &mut Vec<AstEvent<'a>>, stmt: &'a Stmt) {
    match stmt {
        Stmt::Let(stmt) => {
            // The initializer is evaluated before the binding exists.
            pending.push(AstEvent::Expr(&stmt.init));
        }
        Stmt::Assign(stmt) => pending.push(AstEvent::Expr(&stmt.value)),
        Stmt::FieldAssign(stmt) => {
            pending.push(AstEvent::Expr(&stmt.object));
            pending.push(AstEvent::Expr(&stmt.value));
        }
        Stmt::SuperInit(stmt) => {
            pending.push(AstEvent::SuperArguments(stmt));
            push_args(pending, &stmt.args);
        }
        Stmt::StaticFieldAssign(stmt) => pending.push(AstEvent::Expr(&stmt.value)),
        Stmt::IndexAssign(stmt) => {
            pending.push(AstEvent::Expr(&stmt.array));
            pending.push(AstEvent::Expr(&stmt.index));
            pending.push(AstEvent::Expr(&stmt.value));
        }
        Stmt::If(stmt) => {
            pending.push(AstEvent::Expr(&stmt.cond));
            pending.push(AstEvent::Block(&stmt.then_block));
            if let Some(block) = &stmt.else_block {
                pending.push(AstEvent::Block(block));
            }
        }
        Stmt::While(stmt) => {
            pending.push(AstEvent::Expr(&stmt.cond));
            pending.push(AstEvent::Block(&stmt.body));
        }
        Stmt::Break(_) | Stmt::Continue(_) => {}
        Stmt::Defer(stmt) => match &stmt.body {
            DeferBody::Expr(expr) => pending.push(AstEvent::Expr(expr)),
            DeferBody::Block(block) => pending.push(AstEvent::Block(block)),
        },
        Stmt::Lock(stmt) => {
            pending.push(AstEvent::Expr(&stmt.target));

            pending.push(AstEvent::Block(&stmt.body));
        }
        Stmt::For(stmt) => {
            pending.push(AstEvent::Expr(&stmt.iterable));

            pending.push(AstEvent::Block(&stmt.body));
        }
        Stmt::Return(stmt) => {
            if let Some(value) = &stmt.value {
                pending.push(AstEvent::Expr(value));
            }
        }
        Stmt::Expr(stmt) => pending.push(AstEvent::Expr(&stmt.expr)),
    }
}
fn push_expr<'a>(pending: &mut Vec<AstEvent<'a>>, node: &'a Expr) {
    match node {
        Expr::Integer(..)
        | Expr::Float(..)
        | Expr::Bool(..)
        | Expr::String(..)
        | Expr::Var(..)
        | Expr::StaticField(_) => {}
        Expr::Binary(expr) => {
            pending.push(AstEvent::Expr(&expr.lhs));
            pending.push(AstEvent::Expr(&expr.rhs));
        }
        Expr::Unary(expr) => pending.push(AstEvent::Expr(&expr.expr)),
        Expr::Call(call) => {
            pending.push(AstEvent::CallArguments(node));
            push_args(pending, &call.args);
        }
        Expr::FieldAccess(object, ..) => pending.push(AstEvent::Expr(object)),
        Expr::MethodCall(call) => {
            pending.push(AstEvent::Expr(&call.object));
            pending.push(AstEvent::CallArguments(node));
            push_args(pending, &call.args);
        }
        Expr::StaticCall(call) => {
            pending.push(AstEvent::CallArguments(node));
            push_args(pending, &call.args);
        }
        Expr::New(new) => {
            pending.push(AstEvent::CallArguments(node));
            push_args(pending, &new.args);
        }
        Expr::ObjectLiteral(object) => {
            for field in &object.fields {
                pending.push(AstEvent::Expr(&field.value));
            }
        }
        Expr::Await(awaited) => pending.push(AstEvent::Expr(&awaited.expr)),
        Expr::Select(select) => {
            for case in &select.cases {
                match &case.kind {
                    SelectCaseKind::Recv { channel, .. } => {
                        pending.push(AstEvent::Expr(channel));
                    }
                    SelectCaseKind::Send { channel, value } => {
                        pending.push(AstEvent::Expr(channel));
                        pending.push(AstEvent::Expr(value));
                    }
                    SelectCaseKind::Timeout { millis } => pending.push(AstEvent::Expr(millis)),
                    SelectCaseKind::Join { task, .. } => {
                        pending.push(AstEvent::Expr(task));
                    }
                    SelectCaseKind::Default => {}
                }
                pending.push(AstEvent::Block(&case.body));
            }
        }
        Expr::Print(value, ..) => pending.push(AstEvent::Expr(value)),
        Expr::Ternary(expr) => {
            pending.push(AstEvent::Expr(&expr.condition));
            pending.push(AstEvent::Expr(&expr.then_expr));
            pending.push(AstEvent::Expr(&expr.else_expr));
        }
        Expr::Range(expr) => {
            pending.push(AstEvent::Expr(&expr.start));
            pending.push(AstEvent::Expr(&expr.end));
        }
        Expr::Lambda(lambda) => pending.push(AstEvent::Lambda(lambda)),
        Expr::Match(expr) => {
            pending.push(AstEvent::Expr(&expr.scrutinee));
            for arm in &expr.arms {
                match &arm.body {
                    MatchBody::Expr(body) => pending.push(AstEvent::Expr(body)),
                    MatchBody::Block(body) => pending.push(AstEvent::Block(body)),
                }
            }
        }
        Expr::TryPropagate(inner, _, _) => pending.push(AstEvent::Expr(inner)),
        Expr::ArrayLiteral(elements, _, _) => {
            for element in elements {
                pending.push(AstEvent::Expr(element));
            }
        }
        Expr::Index(array, index, _, _) => {
            pending.push(AstEvent::Expr(array));
            pending.push(AstEvent::Expr(index));
        }
    }
}
fn push_lambda<'a>(pending: &mut Vec<AstEvent<'a>>, lambda: &'a LambdaExpr) {
    match &lambda.body {
        LambdaBody::Expr(body) => pending.push(AstEvent::Expr(body)),
        LambdaBody::Block(body) => pending.push(AstEvent::Block(body)),
    }
}
