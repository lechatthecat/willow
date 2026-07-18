use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use crate::semantic::ids::{FunctionId, TypeId};
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

/// A synchronous helper that contains or transitively reaches a loop, making it
/// non-preemptible when called from a task context. `module` is `None` when the
/// helper is defined in the program being analyzed, or `Some(name)` when it was
/// seeded from an imported module (its `span` then points into that module's
/// file, which the current diagnostic source map cannot render, so the
/// cross-module diagnostic uses a note instead of a secondary source label).
#[derive(Debug, Clone)]
struct SyncHelperRef {
    span: Span,
    module: Option<String>,
}

#[derive(Debug, Default)]
pub struct ConcurrencyAnalyzer {
    pub errors: Vec<Diagnostic>,
    pub report: ConcurrencyReport,
    current_async_context: bool,
    current_class: Option<TypeId>,
    nonpreemptible_sync_helpers: HashMap<FunctionId, SyncHelperRef>,
}

impl ConcurrencyAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the nonpreemptible-helper index with the looping sync helpers of an
    /// imported module, keyed by their module-qualified names (`module::fn`,
    /// `module::Class::method`). This lets the entry program's task-aware check
    /// flag a direct cross-module call such as `await`-free `worker::heavy()`
    /// from an async fn (willow-0a6k.2). Call before `check_program`.
    pub fn with_module_helpers(mut self, module_name: &str, program: &Program) -> Self {
        for (name, span) in compute_nonpreemptible_helpers(program) {
            self.nonpreemptible_sync_helpers.insert(
                name.in_namespace(module_name),
                SyncHelperRef {
                    span,
                    module: Some(module_name.to_string()),
                },
            );
        }
        self
    }

    /// Seed the index for a single-item import (`import worker::heavy;`), which
    /// binds a module item under a bare local name. `item` is the item's name in
    /// `program`; `local` is the name it is called by in the importing file;
    /// `module_display` names the source module for the diagnostic note. Re-keys
    /// the module's looping helpers from the item's name to the local name so a
    /// free-fn import (`heavy` → `heavy()`) and a class import (`Work` →
    /// `Work::method()`) both resolve (willow-0a6k.2).
    pub fn with_item_helper(
        mut self,
        local: &str,
        item: &str,
        module_display: &str,
        program: &Program,
    ) -> Self {
        for (name, span) in compute_nonpreemptible_helpers(program) {
            let rekeyed = name.remap_imported_item(item, local);
            if let Some(key) = rekeyed {
                self.nonpreemptible_sync_helpers.insert(
                    key,
                    SyncHelperRef {
                        span,
                        module: Some(module_display.to_string()),
                    },
                );
            }
        }
        self
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
        // Own (same-program) helpers carry `module: None` so the diagnostic can
        // point a secondary label at their definition. Keys are bare names or
        // `Class::method`; they never collide with seeded `module::*` keys.
        for (name, span) in compute_nonpreemptible_helpers(program) {
            self.nonpreemptible_sync_helpers
                .entry(name)
                .or_insert(SyncHelperRef { span, module: None });
        }
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
        let previous_class = self
            .current_class
            .replace(TypeId::from_source_name(class_name));
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
            Stmt::Break(_) | Stmt::Continue(_) => {}
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
                self.check_task_sync_helper_call(
                    &FunctionId::free_from_source_name(&call.callee),
                    call.span,
                );
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
                        &FunctionId::method(class_name.clone(), method.method.as_str()),
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
                let callee = if static_call.class == "Self" {
                    FunctionId::method(
                        self.current_class
                            .clone()
                            .unwrap_or_else(|| TypeId::local("Self")),
                        static_call.method.as_str(),
                    )
                } else {
                    // The parser shape `a::b()` can be a module function or a
                    // static method. Prefer a seeded module-function identity;
                    // otherwise retain the owner type explicitly.
                    let module_function = FunctionId::free(static_call.method.as_str())
                        .in_namespace(static_call.class.as_str());
                    if self
                        .nonpreemptible_sync_helpers
                        .contains_key(&module_function)
                    {
                        module_function
                    } else {
                        FunctionId::method(
                            TypeId::from_source_name(&static_call.class),
                            static_call.method.as_str(),
                        )
                    }
                };
                self.check_task_sync_helper_call(&callee, static_call.span);
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
            Expr::Select(select) => {
                self.report.select_expressions += 1;
                for case in &select.cases {
                    match &case.kind {
                        SelectCaseKind::Recv { channel, .. } => self.check_expr(channel),
                        SelectCaseKind::Send { channel, value } => {
                            self.check_expr(channel);
                            self.check_expr(value);
                        }
                        SelectCaseKind::Default => {}
                    }
                    self.check_block(&case.body);
                }
            }
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

    fn check_task_sync_helper_call(&mut self, callee: &FunctionId, call_span: Span) {
        if !self.current_async_context {
            return;
        }
        let Some(helper) = self.nonpreemptible_sync_helpers.get(callee) else {
            return;
        };
        let diagnostic = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0810,
            format!("sync helper `{callee}` with a loop is not preemptible in task context"),
        )
        .with_label(Label::primary(
            call_span,
            "this call can monopolize the scheduler worker",
        ));
        // A helper defined in an imported module has its span in another file,
        // which this diagnostic's source map cannot render; describe it with a
        // note instead of a cross-file secondary label.
        let diagnostic = match &helper.module {
            Some(module) => diagnostic.with_note(format!(
                "`{callee}` is defined in imported module `{module}` and contains or reaches a synchronous loop",
            )),
            None => diagnostic.with_label(Label::secondary(
                helper.span,
                "this helper contains or reaches a synchronous loop",
            )),
        };
        self.errors.push(
            diagnostic.with_help("make the helper async so its loop can use resumable safepoints"),
        );
    }
}

/// Compute the set of synchronous helpers in `program` that contain or
/// transitively reach a loop, keyed by typed function identity.
/// Shared by the same-program index and the imported-module seeding so the
/// reachability fixpoint behaves identically in both, and by the type checker
/// to flag looping methods called through a typed non-`self` receiver
/// (willow-0a6k.2).
pub(crate) fn compute_nonpreemptible_helpers(program: &Program) -> HashMap<FunctionId, Span> {
    let mut helpers = Vec::new();
    for item in &program.items {
        match item {
            Item::Function(function) if !function.is_async => helpers.push((
                FunctionId::free(function.name.as_str()),
                function.span,
                block_contains_loop(&function.body),
                called_helpers(&function.body),
            )),
            Item::Class(class) => {
                for method in &class.methods {
                    if !method.is_async {
                        let calls = called_helpers(&method.body)
                            .into_iter()
                            .map(|callee| {
                                qualify_self_call(&TypeId::local(class.name.as_str()), callee)
                            })
                            .collect();
                        helpers.push((
                            FunctionId::method(
                                TypeId::local(class.name.as_str()),
                                method.name.as_str(),
                            ),
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

    let mut unsafe_names: HashSet<FunctionId> = helpers
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

    helpers
        .into_iter()
        .filter(|(name, _, _, _)| unsafe_names.contains(name))
        .map(|(name, span, _, _)| (name, span))
        .collect()
}

fn block_contains_loop(block: &Block) -> bool {
    block.stmts.iter().any(stmt_contains_loop)
}

fn stmt_contains_loop(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Break(_) | Stmt::Continue(_) => false,
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

fn called_helpers(block: &Block) -> HashSet<FunctionId> {
    let mut calls = HashSet::new();
    collect_block_calls(block, &mut calls);
    calls
}

fn qualify_self_call(class_name: &TypeId, callee: FunctionId) -> FunctionId {
    callee.resolve_self_owner(class_name)
}

fn collect_block_calls(block: &Block, calls: &mut HashSet<FunctionId>) {
    for stmt in &block.stmts {
        collect_stmt_calls(stmt, calls);
    }
}

fn collect_stmt_calls(stmt: &Stmt, calls: &mut HashSet<FunctionId>) {
    match stmt {
        Stmt::Break(_) | Stmt::Continue(_) => {}
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

fn collect_expr_calls(expr: &Expr, calls: &mut HashSet<FunctionId>) {
    match expr {
        Expr::Call(call) => {
            calls.insert(FunctionId::free_from_source_name(&call.callee));
            for arg in &call.args {
                collect_expr_calls(&arg.expr, calls);
            }
        }
        Expr::StaticCall(call) => {
            calls.insert(FunctionId::method(
                TypeId::from_source_name(&call.class),
                call.method.as_str(),
            ));
            for arg in &call.args {
                collect_expr_calls(&arg.expr, calls);
            }
        }
        Expr::MethodCall(call) => {
            if matches!(&call.object, Expr::Var(name, _) if name == "self") {
                calls.insert(FunctionId::method(
                    TypeId::local("self"),
                    call.method.as_str(),
                ));
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

    fn parse(source: &str) -> Program {
        let tokens = Lexer::new(source).tokenize().unwrap();
        let (program, errors) = Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        program
    }

    fn analyze(source: &str) -> ConcurrencyAnalyzer {
        ConcurrencyAnalyzer::new().check_program(&parse(source))
    }

    /// Analyze `entry` with one imported module's looping sync helpers seeded
    /// under `module_name::*`, mirroring the entry-program path in `main.rs`.
    fn analyze_with_module(entry: &str, module_name: &str, module: &str) -> ConcurrencyAnalyzer {
        ConcurrencyAnalyzer::new()
            .with_module_helpers(module_name, &parse(module))
            .check_program(&parse(entry))
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

    #[test]
    fn rejects_looping_sync_helper_called_in_select_default_case() {
        assert_error_contains(
            r#"
fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}

async fn run(ch: Channel<i64>) {
    select {
        default => { heavy(10); }
    }
}
"#,
            ErrorCode::E0810,
            "sync helper `heavy` with a loop is not preemptible in task context",
        );
    }

    #[test]
    fn rejects_looping_sync_helper_called_in_select_recv_case() {
        assert_error_contains(
            r#"
fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}

async fn run(ch: Channel<i64>) {
    select {
        let v = ch.recv() => { heavy(v); }
    }
}
"#,
            ErrorCode::E0810,
            "sync helper `heavy` with a loop is not preemptible in task context",
        );
    }

    #[test]
    fn allows_loop_free_select_case_in_async_function() {
        let analyzer = analyze(
            r#"
fn add_one(n: i64) -> i64 {
    return n + 1;
}

async fn run(ch: Channel<i64>) {
    select {
        let v = ch.recv() => { println(add_one(v)); }
        default => { println(0); }
    }
}
"#,
        );
        assert!(
            analyzer.errors.is_empty(),
            "loop-free select case should remain callable: {:#?}",
            analyzer.errors
        );
    }

    // --- Cross-module call reachability (entry async fn -> module::helper) ---

    const LOOPING_MODULE: &str = r#"
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

class Work {
    static fn heavy(n: i64) -> i64 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
        return i;
    }
}

fn add_one(n: i64) -> i64 {
    return n + 1;
}
"#;

    fn assert_e0810_with_module_note(analyzer: &ConcurrencyAnalyzer, callee: &str, module: &str) {
        let found = analyzer.errors.iter().any(|e| {
            e.code == ErrorCode::E0810
                && e.message.contains(&format!("sync helper `{callee}`"))
                && e.notes
                    .iter()
                    .any(|n| n.contains(&format!("imported module `{module}`")))
                // Cross-module diagnostics must NOT carry a secondary source
                // label (it would point into another file the map cannot show).
                && e.labels.len() == 1
        });
        assert!(
            found,
            "expected cross-module E0810 for `{callee}` noting module `{module}`, got {:#?}",
            analyzer.errors
        );
    }

    #[test]
    fn rejects_cross_module_looping_free_fn_from_async() {
        let analyzer = analyze_with_module(
            r#"
async fn run() -> i64 {
    return worker::heavy(10);
}
"#,
            "worker",
            LOOPING_MODULE,
        );
        assert_e0810_with_module_note(&analyzer, "worker::heavy", "worker");
    }

    #[test]
    fn rejects_cross_module_transitive_helper_from_async() {
        let analyzer = analyze_with_module(
            r#"
async fn run() -> i64 {
    return worker::wrapper(10);
}
"#,
            "worker",
            LOOPING_MODULE,
        );
        assert_e0810_with_module_note(&analyzer, "worker::wrapper", "worker");
    }

    #[test]
    fn rejects_cross_module_static_method_from_async() {
        let analyzer = analyze_with_module(
            r#"
async fn run() -> i64 {
    return worker::Work::heavy(10);
}
"#,
            "worker",
            LOOPING_MODULE,
        );
        assert_e0810_with_module_note(&analyzer, "worker::Work::heavy", "worker");
    }

    #[test]
    fn allows_cross_module_loop_free_helper_from_async() {
        let analyzer = analyze_with_module(
            r#"
async fn run() -> i64 {
    return worker::add_one(41);
}
"#,
            "worker",
            LOOPING_MODULE,
        );
        assert!(
            analyzer.errors.is_empty(),
            "loop-free cross-module helper should remain callable: {:#?}",
            analyzer.errors
        );
    }

    #[test]
    fn allows_cross_module_looping_helper_from_sync_context() {
        // Same call outside a task context is fine — preemption only matters for
        // task-driven code.
        let analyzer = analyze_with_module(
            r#"
fn run() -> i64 {
    return worker::heavy(10);
}
"#,
            "worker",
            LOOPING_MODULE,
        );
        assert!(
            analyzer.errors.is_empty(),
            "sync-context cross-module call should not warn: {:#?}",
            analyzer.errors
        );
    }

    #[test]
    fn respects_module_alias_for_cross_module_helpers() {
        // Modules imported under an alias are accessed (and seeded) by the alias.
        let analyzer = analyze_with_module(
            r#"
async fn run() -> i64 {
    return w::heavy(10);
}
"#,
            "w",
            LOOPING_MODULE,
        );
        assert_e0810_with_module_note(&analyzer, "w::heavy", "w");
    }

    // --- Single-item imports (`import worker::heavy;` -> bare local call) ---

    /// Analyze `entry` with one imported item (`local` bound to `item` of the
    /// module), mirroring the item-import path in `main.rs`.
    fn analyze_with_item(
        entry: &str,
        local: &str,
        item: &str,
        module: &str,
        module_src: &str,
    ) -> ConcurrencyAnalyzer {
        ConcurrencyAnalyzer::new()
            .with_item_helper(local, item, module, &parse(module_src))
            .check_program(&parse(entry))
    }

    #[test]
    fn rejects_item_imported_looping_free_fn_from_async() {
        let analyzer = analyze_with_item(
            r#"
async fn run() -> i64 {
    return heavy(10);
}
"#,
            "heavy",
            "heavy",
            "worker",
            LOOPING_MODULE,
        );
        assert_e0810_with_module_note(&analyzer, "heavy", "worker");
    }

    #[test]
    fn respects_item_import_alias_for_helpers() {
        // `import worker::heavy as h;` — called by the local alias `h`.
        let analyzer = analyze_with_item(
            r#"
async fn run() -> i64 {
    return h(10);
}
"#,
            "h",
            "heavy",
            "worker",
            LOOPING_MODULE,
        );
        assert_e0810_with_module_note(&analyzer, "h", "worker");
    }

    #[test]
    fn rejects_item_imported_class_static_method_from_async() {
        // `import worker::Work;` — the class's looping static method is reachable
        // through the bare local class name (`Work::heavy()`).
        let analyzer = analyze_with_item(
            r#"
async fn run() -> i64 {
    return Work::heavy(10);
}
"#,
            "Work",
            "Work",
            "worker",
            LOOPING_MODULE,
        );
        assert_e0810_with_module_note(&analyzer, "Work::heavy", "worker");
    }

    #[test]
    fn allows_item_imported_loop_free_helper_from_async() {
        let analyzer = analyze_with_item(
            r#"
async fn run() -> i64 {
    return add_one(41);
}
"#,
            "add_one",
            "add_one",
            "worker",
            LOOPING_MODULE,
        );
        assert!(
            analyzer.errors.is_empty(),
            "loop-free item-imported helper should remain callable: {:#?}",
            analyzer.errors
        );
    }
}
