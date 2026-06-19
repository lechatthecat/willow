use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConcurrencyReport {
    pub async_functions: usize,
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
    current_async_context: bool,
    current_class: Option<String>,
    nonpreemptible_sync_helpers: HashMap<String, Span>,
}

impl ConcurrencyAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn check_program(mut self, program: &Program) -> Self {
        self.index_nonpreemptible_sync_helpers(program);
        for item in &program.items {
            match item {
                Item::Function(function) => self.check_function(function),
                Item::Class(class) => {
                    for method in &class.methods {
                        self.check_method(&class.name, method);
                    }
                }
                Item::Enum(_) => {}
                Item::Interface(_) => {} // no method bodies to check
            }
        }
        self
    }

    fn index_nonpreemptible_sync_helpers(&mut self, program: &Program) {
        let mut helpers = Vec::new();
        for item in &program.items {
            match item {
                Item::Function(function) if !function.is_async => helpers.push((
                    function.name.clone(),
                    function.span,
                    block_contains_loop(&function.body),
                    called_helpers(&function.body),
                )),
                Item::Class(class) => {
                    for method in &class.methods {
                        if !method.is_async {
                            let calls = called_helpers(&method.body)
                                .into_iter()
                                .map(|callee| qualify_self_call(&class.name, callee))
                                .collect();
                            helpers.push((
                                format!("{}::{}", class.name, method.name),
                                method.span,
                                block_contains_loop(&method.body),
                                calls,
                            ));
                        }
                    }
                }
                Item::Function(_) | Item::Enum(_) | Item::Interface(_) => {}
            }
        }

        let mut unsafe_names: HashSet<String> = helpers
            .iter()
            .filter(|(_, _, contains_loop, _)| *contains_loop)
            .map(|(name, _, _, _)| name.clone())
            .collect();
        loop {
            let mut changed = false;
            for (name, _, _, calls) in &helpers {
                if !unsafe_names.contains(name)
                    && calls.iter().any(|callee| unsafe_names.contains(callee))
                {
                    unsafe_names.insert(name.clone());
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        self.nonpreemptible_sync_helpers = helpers
            .into_iter()
            .filter(|(name, _, _, _)| unsafe_names.contains(name))
            .map(|(name, span, _, _)| (name, span))
            .collect();
    }

    fn check_function(&mut self, function: &FunctionDecl) {
        if function.is_async {
            self.report.async_functions += 1;
            self.check_async_reference_params("async function", function.span, &function.params);
        }
        let previous_async_context = self.current_async_context;
        self.current_async_context = function.is_async;
        self.check_block(&function.body);
        self.current_async_context = previous_async_context;
    }

    fn check_method(&mut self, class_name: &str, method: &MethodDecl) {
        if method.is_async {
            self.report.async_functions += 1;
            self.check_async_reference_params("async method", method.span, &method.params);
        }
        let previous_async_context = self.current_async_context;
        let previous_class = self.current_class.replace(class_name.to_string());
        self.current_async_context = method.is_async;
        self.check_block(&method.body);
        self.current_async_context = previous_async_context;
        self.current_class = previous_class;
    }

    fn check_block(&mut self, block: &Block) {
        for stmt in &block.stmts {
            self.check_stmt(stmt);
        }
    }

    fn check_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(let_stmt) => {
                self.check_expr(&let_stmt.init);
            }
            Stmt::Assign(assign) => self.check_expr(&assign.value),
            Stmt::StaticFieldAssign(s) => self.check_expr(&s.value),
            Stmt::FieldAssign(fa) => {
                self.check_expr(&fa.object);
                self.check_expr(&fa.value);
            }
            Stmt::IndexAssign(ia) => {
                self.check_expr(&ia.array);
                self.check_expr(&ia.index);
                self.check_expr(&ia.value);
            }
            Stmt::SuperInit(super_init) => {
                for arg in &super_init.args {
                    self.check_expr(&arg.expr);
                }
            }
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
            Stmt::For(for_stmt) => {
                self.check_expr(&for_stmt.iterable);
                self.check_block(&for_stmt.body);
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
                self.check_task_sync_helper_call(&call.callee, call.span);
                for arg in &call.args {
                    self.check_expr(&arg.expr);
                }
            }
            Expr::FieldAccess(object, _, _) => self.check_expr(object),
            Expr::MethodCall(method) => {
                if matches!(&method.object, Expr::Var(name, _) if name == "self")
                    && let Some(class_name) = &self.current_class
                {
                    self.check_task_sync_helper_call(
                        &format!("{class_name}::{}", method.method),
                        method.span,
                    );
                }
                self.check_expr(&method.object);
                match method.method.as_str() {
                    "join" => self.report.join_operations += 1,
                    "send" | "recv" | "close" => self.report.channel_operations += 1,
                    _ => {}
                }
                for arg in &method.args {
                    self.check_expr(&arg.expr);
                }
            }
            Expr::StaticCall(static_call) => {
                let class_name = if static_call.class == "Self" {
                    self.current_class
                        .as_deref()
                        .unwrap_or(static_call.class.as_str())
                } else {
                    &static_call.class
                };
                self.check_task_sync_helper_call(
                    &format!("{class_name}::{}", static_call.method),
                    static_call.span,
                );
                for arg in &static_call.args {
                    self.check_expr(&arg.expr);
                }
            }
            Expr::New(new_expr) => {
                for arg in &new_expr.args {
                    self.check_expr(&arg.expr);
                }
            }
            // A static property read is a leaf — no sub-expressions to check.
            Expr::StaticField(_) => {}
            Expr::ObjectLiteral(object) => {
                for field in &object.fields {
                    self.check_expr(&field.value);
                }
            }
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
            Expr::Range(range) => {
                self.check_expr(&range.start);
                self.check_expr(&range.end);
            }
            Expr::Lambda(lambda) => match &lambda.body {
                LambdaBody::Expr(expr) => self.check_expr(expr),
                LambdaBody::Block(block) => self.check_block(block),
            },
            Expr::Match(m) => {
                self.check_expr(&m.scrutinee);
                for arm in &m.arms {
                    match &arm.body {
                        MatchBody::Expr(e) => self.check_expr(e),
                        MatchBody::Block(b) => self.check_block(b),
                    }
                }
            }
            Expr::TryPropagate(inner, _) => self.check_expr(inner),
            Expr::ArrayLiteral(elements, _) => {
                for el in elements {
                    self.check_expr(el);
                }
            }
            Expr::Index(arr, index, _) => {
                self.check_expr(arr);
                self.check_expr(index);
            }
            Expr::Integer(_, _)
            | Expr::Float(_, _)
            | Expr::Bool(_, _)
            | Expr::Nil(_)
            | Expr::String(_, _)
            | Expr::Var(_, _) => {}
        }
    }

    fn check_async_reference_params(&mut self, context: &str, owner_span: Span, params: &[Param]) {
        for param in params {
            if let ParamMode::Reference { mutable, .. } = &param.mode {
                let mode = if *mutable { "&mut" } else { "&" };
                self.errors.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1707,
                        format!(
                            "reference parameter `{}` is not supported in {context}",
                            param.name
                        ),
                    )
                    .with_label(Label::primary(
                        param.span,
                        format!("`{mode}` parameter may live across suspension points"),
                    ))
                    .with_label(Label::secondary(
                        owner_span,
                        format!("{context} parsed here"),
                    ))
                    .with_help(
                        "pass by value or use Mutex<T>, AtomicI64, or channels for shared state",
                    ),
                );
            }
        }
    }

    fn check_task_sync_helper_call(&mut self, callee: &str, call_span: Span) {
        if !self.current_async_context {
            return;
        }
        let Some(&helper_span) = self.nonpreemptible_sync_helpers.get(callee) else {
            return;
        };
        self.errors.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0810,
                format!("sync helper `{callee}` with a loop is not preemptible in task context"),
            )
            .with_label(Label::primary(
                call_span,
                "this call can monopolize the scheduler worker",
            ))
            .with_label(Label::secondary(
                helper_span,
                "this helper contains or reaches a synchronous loop",
            ))
            .with_help("make the helper async so its loop can use resumable safepoints"),
        );
    }
}

fn block_contains_loop(block: &Block) -> bool {
    block.stmts.iter().any(stmt_contains_loop)
}

fn stmt_contains_loop(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::While(_) | Stmt::For(_) => true,
        Stmt::Let(let_stmt) => expr_contains_loop(&let_stmt.init),
        Stmt::Assign(assign) => expr_contains_loop(&assign.value),
        Stmt::StaticFieldAssign(s) => expr_contains_loop(&s.value),
        Stmt::FieldAssign(assign) => {
            expr_contains_loop(&assign.object) || expr_contains_loop(&assign.value)
        }
        Stmt::IndexAssign(assign) => {
            expr_contains_loop(&assign.array)
                || expr_contains_loop(&assign.index)
                || expr_contains_loop(&assign.value)
        }
        Stmt::SuperInit(super_init) => super_init
            .args
            .iter()
            .any(|arg| expr_contains_loop(&arg.expr)),
        Stmt::If(if_stmt) => {
            expr_contains_loop(&if_stmt.cond)
                || block_contains_loop(&if_stmt.then_block)
                || if_stmt.else_block.as_ref().is_some_and(block_contains_loop)
        }
        Stmt::Return(return_stmt) => return_stmt.value.as_ref().is_some_and(expr_contains_loop),
        Stmt::Expr(expr_stmt) => expr_contains_loop(&expr_stmt.expr),
    }
}

fn expr_contains_loop(expr: &Expr) -> bool {
    match expr {
        Expr::Await(await_expr) => expr_contains_loop(&await_expr.expr),
        Expr::Binary(binary) => expr_contains_loop(&binary.lhs) || expr_contains_loop(&binary.rhs),
        Expr::Unary(unary) => expr_contains_loop(&unary.expr),
        Expr::Call(call) => call.args.iter().any(|arg| expr_contains_loop(&arg.expr)),
        Expr::FieldAccess(object, _, _) => expr_contains_loop(object),
        Expr::MethodCall(method) => {
            expr_contains_loop(&method.object)
                || method.args.iter().any(|arg| expr_contains_loop(&arg.expr))
        }
        Expr::StaticCall(call) => call.args.iter().any(|arg| expr_contains_loop(&arg.expr)),
        Expr::New(n) => n.args.iter().any(|arg| expr_contains_loop(&arg.expr)),
        Expr::StaticField(_) => false,
        Expr::ObjectLiteral(object) => object
            .fields
            .iter()
            .any(|field| expr_contains_loop(&field.value)),
        Expr::Print(value, _, _) => expr_contains_loop(value),
        Expr::Ternary(ternary) => {
            expr_contains_loop(&ternary.condition)
                || expr_contains_loop(&ternary.then_expr)
                || expr_contains_loop(&ternary.else_expr)
        }
        Expr::Lambda(_) => false,
        Expr::Match(m) => {
            expr_contains_loop(&m.scrutinee)
                || m.arms.iter().any(|arm| match &arm.body {
                    MatchBody::Expr(expr) => expr_contains_loop(expr),
                    MatchBody::Block(block) => block_contains_loop(block),
                })
        }
        Expr::TryPropagate(inner, _) => expr_contains_loop(inner),
        Expr::ArrayLiteral(elements, _) => elements.iter().any(expr_contains_loop),
        Expr::Index(array, index, _) => expr_contains_loop(array) || expr_contains_loop(index),
        Expr::Range(range) => expr_contains_loop(&range.start) || expr_contains_loop(&range.end),
        Expr::Select(select) => select
            .cases
            .iter()
            .any(|case| block_contains_loop(&case.body)),
        Expr::Integer(_, _)
        | Expr::Float(_, _)
        | Expr::Bool(_, _)
        | Expr::Nil(_)
        | Expr::String(_, _)
        | Expr::Var(_, _) => false,
    }
}

fn called_helpers(block: &Block) -> HashSet<String> {
    let mut calls = HashSet::new();
    collect_block_calls(block, &mut calls);
    calls
}

fn qualify_self_call(class_name: &str, callee: String) -> String {
    if let Some(method) = callee.strip_prefix("Self::") {
        format!("{class_name}::{method}")
    } else if let Some(method) = callee.strip_prefix("self.") {
        format!("{class_name}::{method}")
    } else {
        callee
    }
}

fn collect_block_calls(block: &Block, calls: &mut HashSet<String>) {
    for stmt in &block.stmts {
        collect_stmt_calls(stmt, calls);
    }
}

fn collect_stmt_calls(stmt: &Stmt, calls: &mut HashSet<String>) {
    match stmt {
        Stmt::Let(stmt) => collect_expr_calls(&stmt.init, calls),
        Stmt::Assign(stmt) => collect_expr_calls(&stmt.value, calls),
        Stmt::StaticFieldAssign(stmt) => collect_expr_calls(&stmt.value, calls),
        Stmt::FieldAssign(stmt) => {
            collect_expr_calls(&stmt.object, calls);
            collect_expr_calls(&stmt.value, calls);
        }
        Stmt::IndexAssign(stmt) => {
            collect_expr_calls(&stmt.array, calls);
            collect_expr_calls(&stmt.index, calls);
            collect_expr_calls(&stmt.value, calls);
        }
        Stmt::SuperInit(stmt) => {
            for arg in &stmt.args {
                collect_expr_calls(&arg.expr, calls);
            }
        }
        Stmt::If(stmt) => {
            collect_expr_calls(&stmt.cond, calls);
            collect_block_calls(&stmt.then_block, calls);
            if let Some(block) = &stmt.else_block {
                collect_block_calls(block, calls);
            }
        }
        Stmt::While(stmt) => {
            collect_expr_calls(&stmt.cond, calls);
            collect_block_calls(&stmt.body, calls);
        }
        Stmt::For(stmt) => {
            collect_expr_calls(&stmt.iterable, calls);
            collect_block_calls(&stmt.body, calls);
        }
        Stmt::Return(stmt) => {
            if let Some(value) = &stmt.value {
                collect_expr_calls(value, calls);
            }
        }
        Stmt::Expr(stmt) => collect_expr_calls(&stmt.expr, calls),
    }
}

fn collect_expr_calls(expr: &Expr, calls: &mut HashSet<String>) {
    match expr {
        Expr::Call(call) => {
            calls.insert(call.callee.clone());
            for arg in &call.args {
                collect_expr_calls(&arg.expr, calls);
            }
        }
        Expr::StaticCall(call) => {
            calls.insert(format!("{}::{}", call.class, call.method));
            for arg in &call.args {
                collect_expr_calls(&arg.expr, calls);
            }
        }
        Expr::MethodCall(call) => {
            if matches!(&call.object, Expr::Var(name, _) if name == "self") {
                calls.insert(format!("self.{}", call.method));
            }
            collect_expr_calls(&call.object, calls);
            for arg in &call.args {
                collect_expr_calls(&arg.expr, calls);
            }
        }
        Expr::New(expr) => {
            for arg in &expr.args {
                collect_expr_calls(&arg.expr, calls);
            }
        }
        Expr::Binary(expr) => {
            collect_expr_calls(&expr.lhs, calls);
            collect_expr_calls(&expr.rhs, calls);
        }
        Expr::Unary(expr) => collect_expr_calls(&expr.expr, calls),
        Expr::FieldAccess(object, _, _) => collect_expr_calls(object, calls),
        Expr::ObjectLiteral(expr) => {
            for field in &expr.fields {
                collect_expr_calls(&field.value, calls);
            }
        }
        Expr::Await(expr) => collect_expr_calls(&expr.expr, calls),
        Expr::Print(expr, _, _) | Expr::TryPropagate(expr, _) => collect_expr_calls(expr, calls),
        Expr::Ternary(expr) => {
            collect_expr_calls(&expr.condition, calls);
            collect_expr_calls(&expr.then_expr, calls);
            collect_expr_calls(&expr.else_expr, calls);
        }
        Expr::Range(expr) => {
            collect_expr_calls(&expr.start, calls);
            collect_expr_calls(&expr.end, calls);
        }
        Expr::Match(expr) => {
            collect_expr_calls(&expr.scrutinee, calls);
            for arm in &expr.arms {
                match &arm.body {
                    MatchBody::Expr(expr) => collect_expr_calls(expr, calls),
                    MatchBody::Block(block) => collect_block_calls(block, calls),
                }
            }
        }
        Expr::Select(expr) => {
            for case in &expr.cases {
                match &case.kind {
                    SelectCaseKind::Recv { channel, .. } => collect_expr_calls(channel, calls),
                    SelectCaseKind::Send { channel, value } => {
                        collect_expr_calls(channel, calls);
                        collect_expr_calls(value, calls);
                    }
                    SelectCaseKind::Default => {}
                }
                collect_block_calls(&case.body, calls);
            }
        }
        Expr::ArrayLiteral(elements, _) => {
            for element in elements {
                collect_expr_calls(element, calls);
            }
        }
        Expr::Index(array, index, _) => {
            collect_expr_calls(array, calls);
            collect_expr_calls(index, calls);
        }
        Expr::Lambda(_)
        | Expr::StaticField(_)
        | Expr::Integer(_, _)
        | Expr::Float(_, _)
        | Expr::Bool(_, _)
        | Expr::Nil(_)
        | Expr::String(_, _)
        | Expr::Var(_, _) => {}
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

    fn assert_error_contains(source: &str, code: ErrorCode, message: &str) {
        let analyzer = analyze(source);
        assert!(
            analyzer
                .errors
                .iter()
                .any(|error| error.code == code && error.message.contains(message)),
            "expected {code:?} containing `{message}`, got {:#?}",
            analyzer.errors
        );
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

    #[test]
    fn rejects_mutable_reference_parameter_in_async_function() {
        assert_error_contains(
            r#"
async fn update(x: &mut i64) {
}
"#,
            ErrorCode::E1707,
            "reference parameter `x` is not supported in async function",
        );
    }

    #[test]
    fn rejects_immutable_reference_parameter_in_async_function() {
        assert_error_contains(
            r#"
async fn read(x: & i64) -> i64 {
    return x;
}
"#,
            ErrorCode::E1707,
            "reference parameter `x` is not supported in async function",
        );
    }

    #[test]
    fn rejects_reference_parameter_in_async_method() {
        assert_error_contains(
            r#"
class Box {
    async fn update(self, x: &mut i64) {
    }
}
"#,
            ErrorCode::E1707,
            "reference parameter `x` is not supported in async method",
        );
    }

    #[test]
    fn allows_async_while_true_without_await() {
        let analyzer = analyze(
            r#"
async fn spin() {
    while true {
    }
}
"#,
        );
        assert!(
            analyzer.errors.is_empty(),
            "async loop backedges are preemptible: {:#?}",
            analyzer.errors
        );
    }

    #[test]
    fn allows_async_while_true_with_await() {
        let analyzer = analyze(
            r#"
async fn tick() {
    while true {
        await sleep(1);
    }
}
"#,
        );
        assert!(
            !analyzer
                .errors
                .iter()
                .any(|error| error.code == ErrorCode::E0808),
            "did not expect E0808, got {:#?}",
            analyzer.errors
        );
    }

    #[test]
    fn allows_async_while_true_that_returns() {
        let analyzer = analyze(
            r#"
async fn once() {
    while true {
        return;
    }
}
"#,
        );
        assert!(
            !analyzer
                .errors
                .iter()
                .any(|error| error.code == ErrorCode::E0808),
            "did not expect E0808, got {:#?}",
            analyzer.errors
        );
    }

    #[test]
    fn rejects_looping_sync_helper_called_from_async_function() {
        assert_error_contains(
            r#"
fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}

async fn run() -> i64 {
    return heavy(10);
}
"#,
            ErrorCode::E0810,
            "sync helper `heavy` with a loop is not preemptible in task context",
        );
    }

    #[test]
    fn rejects_transitive_looping_sync_helper_called_from_async_function() {
        assert_error_contains(
            r#"
fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}

fn wrapper(n: i64) -> i64 {
    return heavy(n);
}

async fn run() -> i64 {
    return wrapper(10);
}
"#,
            ErrorCode::E0810,
            "sync helper `wrapper` with a loop is not preemptible in task context",
        );
    }

    #[test]
    fn rejects_looping_static_helper_called_from_async_function() {
        assert_error_contains(
            r#"
class Work {
    static fn heavy(n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }

    static fn wrapper(n: i64) -> i64 {
        return Self::heavy(n);
    }
}

async fn run() -> i64 {
    return Work::wrapper(10);
}
"#,
            ErrorCode::E0810,
            "sync helper `Work::wrapper` with a loop is not preemptible in task context",
        );
    }

    #[test]
    fn rejects_looping_self_helper_called_from_async_method() {
        assert_error_contains(
            r#"
class Work {
    fn heavy(self, n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }

    async fn run(self) -> i64 {
        return self.heavy(10);
    }
}
"#,
            ErrorCode::E0810,
            "sync helper `Work::heavy` with a loop is not preemptible in task context",
        );
    }

    #[test]
    fn allows_loop_free_sync_helper_called_from_async_function() {
        let analyzer = analyze(
            r#"
fn add_one(n: i64) -> i64 {
    return n + 1;
}

async fn run() -> i64 {
    return add_one(10);
}
"#,
        );
        assert!(
            analyzer.errors.is_empty(),
            "loop-free helper should remain callable: {:#?}",
            analyzer.errors
        );
    }
}
