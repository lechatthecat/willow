use std::collections::HashMap;

use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConcurrencyReport {
    pub async_functions: usize,
    pub spawn_expressions: usize,
    pub await_expressions: usize,
    pub await_outside_async: usize,
    pub select_expressions: usize,
    pub channel_operations: usize,
    pub join_operations: usize,
}

#[derive(Debug, Default)]
pub struct ConcurrencyAnalyzer {
    pub errors: Vec<Diagnostic>,
    pub report: ConcurrencyReport,
    scopes: Vec<HashMap<String, VarConcurrencyInfo>>,
    current_async_context: bool,
}

#[derive(Debug, Clone, Copy)]
struct VarConcurrencyInfo {
    mutable: bool,
    span: Span,
}

impl ConcurrencyAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn check_program(mut self, program: &Program) -> Self {
        for item in &program.items {
            match item {
                Item::Function(function) => self.check_function(function),
                Item::Class(class) => {
                    for method in &class.methods {
                        self.check_method(method);
                    }
                }
            }
        }
        self
    }

    fn check_function(&mut self, function: &FunctionDecl) {
        if function.is_async {
            self.report.async_functions += 1;
        }
        let previous_async_context = self.current_async_context;
        self.current_async_context = function.is_async;
        self.push_scope();
        for param in &function.params {
            self.define_var(&param.name, false, param.span);
        }
        self.check_block(&function.body);
        self.pop_scope();
        self.current_async_context = previous_async_context;
    }

    fn check_method(&mut self, method: &MethodDecl) {
        if method.is_async {
            self.report.async_functions += 1;
        }
        let previous_async_context = self.current_async_context;
        self.current_async_context = method.is_async;
        self.push_scope();
        if method.has_self {
            self.define_var("self", false, method.span);
        }
        for param in &method.params {
            self.define_var(&param.name, false, param.span);
        }
        self.check_block(&method.body);
        self.pop_scope();
        self.current_async_context = previous_async_context;
    }

    fn check_block(&mut self, block: &Block) {
        self.push_scope();
        for stmt in &block.stmts {
            self.check_stmt(stmt);
        }
        self.pop_scope();
    }

    fn check_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(let_stmt) => {
                self.check_expr(&let_stmt.init);
                self.define_var(&let_stmt.name, let_stmt.mutable, let_stmt.span);
            }
            Stmt::Assign(assign) => self.check_expr(&assign.value),
            Stmt::If(if_stmt) => {
                self.check_expr(&if_stmt.cond);
                self.check_block(&if_stmt.then_block);
                if let Some(else_block) = &if_stmt.else_block {
                    self.check_block(else_block);
                }
            }
            Stmt::While(while_stmt) => {
                self.check_expr(&while_stmt.cond);
                self.check_block(&while_stmt.body);
            }
            Stmt::Return(return_stmt) => {
                if let Some(value) = &return_stmt.value {
                    self.check_expr(value);
                }
            }
            Stmt::Expr(expr_stmt) => self.check_expr(&expr_stmt.expr),
        }
    }

    fn check_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Binary(binary) => {
                self.check_expr(&binary.lhs);
                self.check_expr(&binary.rhs);
            }
            Expr::Unary(unary) => self.check_expr(&unary.expr),
            Expr::Call(call) => {
                for arg in &call.args {
                    self.check_expr(arg);
                }
            }
            Expr::FieldAccess(object, _, _) => self.check_expr(object),
            Expr::MethodCall(method) => {
                self.check_expr(&method.object);
                match method.method.as_str() {
                    "join" => self.report.join_operations += 1,
                    "send" | "recv" | "close" => self.report.channel_operations += 1,
                    _ => {}
                }
                for arg in &method.args {
                    self.check_expr(arg);
                }
            }
            Expr::StaticCall(static_call) => {
                for arg in &static_call.args {
                    self.check_expr(arg);
                }
            }
            Expr::ObjectLiteral(object) => {
                for field in &object.fields {
                    self.check_expr(&field.value);
                }
            }
            Expr::Spawn(spawn) => self.check_spawn(spawn),
            Expr::Await(await_expr) => {
                self.report.await_expressions += 1;
                if !self.current_async_context {
                    self.report.await_outside_async += 1;
                }
                self.check_expr(&await_expr.expr);
            }
            Expr::Select(_) => self.report.select_expressions += 1,
            Expr::Print(arg, _, _) => self.check_expr(arg),
            Expr::Ternary(ternary) => {
                self.check_expr(&ternary.condition);
                self.check_expr(&ternary.then_expr);
                self.check_expr(&ternary.else_expr);
            }
            Expr::Lambda(lambda) => match &lambda.body {
                LambdaBody::Expr(expr) => self.check_expr(expr),
                LambdaBody::Block(block) => self.check_block(block),
            },
            Expr::Integer(_, _)
            | Expr::Float(_, _)
            | Expr::Bool(_, _)
            | Expr::Nil(_)
            | Expr::String(_, _)
            | Expr::Var(_, _) => {}
        }
    }

    fn check_spawn(&mut self, spawn: &SpawnExpr) {
        self.report.spawn_expressions += 1;
        for arg in &spawn.args {
            self.check_expr(arg);
            if let Expr::Var(name, span) = arg {
                if let Some(info) = self.lookup_var(name) {
                    if info.mutable {
                        self.errors.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0807,
                                format!(
                                    "spawning with mutable local `{}` is not supported yet",
                                    name
                                ),
                            )
                            .with_label(Label::primary(
                                *span,
                                "mutable value would cross a task boundary",
                            ))
                            .with_label(Label::secondary(info.span, "mutable local declared here"))
                            .with_help(
                                "copy the value into an immutable local before spawning the task",
                            ),
                        );
                    }
                }
            }
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn define_var(&mut self, name: &str, mutable: bool, span: Span) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), VarConcurrencyInfo { mutable, span });
        }
    }

    fn lookup_var(&self, name: &str) -> Option<VarConcurrencyInfo> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn analyze(source: &str) -> ConcurrencyAnalyzer {
        let tokens = Lexer::new(source).tokenize().unwrap();
        let (program, errors) = Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        ConcurrencyAnalyzer::new().check_program(&program)
    }

    #[test]
    fn report_counts_concurrency_constructs() {
        let analyzer = analyze(
            r#"
async fn run(f: Future<i64>, h: JoinHandle<i64>, ch: Channel<i64>) {
    let value = await f;
    h.join();
    ch.close();
    select {};
}

fn main() {
    println(1);
}
"#,
        );
        assert_eq!(analyzer.report.async_functions, 1);
        assert_eq!(analyzer.report.await_expressions, 1);
        assert_eq!(analyzer.report.join_operations, 1);
        assert_eq!(analyzer.report.channel_operations, 1);
        assert_eq!(analyzer.report.select_expressions, 1);
    }
}
