//! Conservative lexical discharge proof for tasks holding reference arguments.
//! E1707 remains the declaration gate; this pass runs after it so a future
//! relaxation cannot silently permit task escape.
use std::collections::{HashMap, HashSet};

use super::builtin_types::{self, BuiltinTypeId};
use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use crate::parser::iter::{AstEvent, AstWalk};

#[derive(Clone)]
struct BorrowedTask {
    call: ExprId,
    origin: Span,
    block: Span,
    references: Vec<(Span, Option<bool>)>,
}

pub fn check(
    program: &Program,
    types: &HashMap<ExprId, Type>,
    modes: &HashMap<ExprId, ParamMode>,
) -> Vec<Diagnostic> {
    let mut blocks = Vec::new();
    let mut initializers = Vec::new();
    for item in &program.items {
        match item {
            Item::Function(f) => blocks.push(&f.body),
            Item::Class(c) => {
                blocks.extend(c.methods.iter().map(|m| &m.body));
                blocks.extend(c.constructors.iter().map(|constructor| &constructor.body));
                initializers.extend(
                    c.fields.iter().filter_map(|field| {
                        field.initializer.as_ref().map(|init| (init, field.span))
                    }),
                );
            }
            Item::Interface(interface) => blocks.extend(
                interface
                    .methods
                    .iter()
                    .filter_map(|method| method.default_body.as_ref()),
            ),
            _ => {}
        }
    }
    let mut checker = Checker {
        types,
        modes,
        errors: Vec::new(),
        reported: HashSet::new(),
    };
    for (initializer, boundary) in initializers {
        let mut walk = AstWalk::new(AstEvent::Expr(initializer));
        while let Some(event) = walk.next() {
            match event {
                AstEvent::Block(block) => {
                    blocks.push(block);
                    walk.skip_children();
                }
                AstEvent::Expr(expr) => {
                    if let Some(task) = checker.borrowed_call(expr, boundary) {
                        checker.escape(&task, initializer.span());
                    }
                }
                _ => {}
            }
        }
    }
    while let Some(block) = blocks.pop() {
        checker.block(block, &mut blocks);
    }
    checker.errors
}

struct Checker<'a> {
    types: &'a HashMap<ExprId, Type>,
    modes: &'a HashMap<ExprId, ParamMode>,
    errors: Vec<Diagnostic>,
    reported: HashSet<ExprId>,
}

impl Checker<'_> {
    fn borrowed_call(&self, expr: &Expr, block: Span) -> Option<BorrowedTask> {
        let ty = self.types.get(&expr.id())?;
        builtin_types::unary_arg(ty, BuiltinTypeId::Task)?;
        let args = match expr {
            Expr::Call(c) => &c.args,
            Expr::MethodCall(c) => &c.args,
            Expr::StaticCall(c) => &c.args,
            _ => return None,
        };
        let references: Vec<_> = args
            .iter()
            .filter_map(|arg| match arg.mode {
                CallArgMode::Reference { ampersand_span } => {
                    let ParamMode::Reference { mutable, .. } = self.modes.get(&arg.expr.id())?
                    else {
                        return None;
                    };
                    Some((ampersand_span, Some(*mutable)))
                }
                CallArgMode::Value => None,
            })
            .collect();
        (!references.is_empty()).then_some(BorrowedTask {
            call: expr.id(),
            origin: expr.span(),
            block,
            references,
        })
    }

    fn escape(&mut self, task: &BorrowedTask, span: Span) {
        if !self.reported.insert(task.call) {
            return;
        }
        let mut diagnostic = Diagnostic::new(
            Severity::Error, ErrorCode::E1708,
            "async reference borrow escapes its discharge block",
        ).with_label(Label::primary(span, "task must be awaited directly in its originating block"))
         .with_label(Label::secondary(task.origin, "task captures reference arguments here"))
         .with_label(Label::secondary(task.block, "borrow discharge block"))
         .with_help("await the call directly, or bind its task locally and await it in the same block before any transfer or exit");
        for &(reference, mutable) in &task.references {
            diagnostic = diagnostic.with_label(Label::secondary(
                reference,
                match mutable {
                    Some(true) => "mutable borrow captured by this task",
                    Some(false) => "shared borrow captured by this task",
                    None => "borrow captured by this task",
                },
            ));
        }
        self.errors.push(diagnostic);
    }

    /// Inspect uses and exits before descending into independently checked blocks.
    /// Bind events distinguish captures from shadowed locals; callable and loop
    /// boundaries distinguish exits that can bypass this block's later await.
    fn check_pending_uses(
        &mut self,
        stmt: &Stmt,
        pending: &HashMap<&str, BorrowedTask>,
        allowed_var: Option<ExprId>,
    ) {
        let mut scopes: Vec<HashSet<&str>> = vec![HashSet::new()];
        let mut callable_depth = 0;
        let mut loop_depth = 0;
        for event in AstWalk::new(AstEvent::Stmt(stmt)) {
            let mut exit = None;
            let mut used = None;
            match event {
                AstEvent::EnterScope => scopes.push(HashSet::new()),
                AstEvent::ExitScope => {
                    scopes.pop();
                }
                AstEvent::Bind(name) => {
                    scopes.last_mut().expect("binding scope").insert(name);
                }
                AstEvent::Expr(Expr::Lambda(_)) => callable_depth += 1,
                AstEvent::ExitExpr(Expr::Lambda(_)) => callable_depth -= 1,
                AstEvent::Stmt(Stmt::While(_) | Stmt::For(_)) => loop_depth += 1,
                AstEvent::ExitStmt(Stmt::While(_) | Stmt::For(_)) => loop_depth -= 1,
                AstEvent::Expr(Expr::Var(name, span, id)) if Some(*id) != allowed_var => {
                    used = Some((name.as_str(), *span));
                }
                AstEvent::Stmt(Stmt::Assign(assign)) => {
                    used = Some((assign.name.as_str(), assign.span));
                }
                AstEvent::Stmt(Stmt::Return(ret)) if callable_depth == 0 => {
                    exit = Some(ret.span);
                }
                AstEvent::Expr(Expr::TryPropagate(_, span, _)) if callable_depth == 0 => {
                    exit = Some(*span);
                }
                AstEvent::Stmt(Stmt::Break(span) | Stmt::Continue(span))
                    if callable_depth == 0 && loop_depth == 0 =>
                {
                    exit = Some(*span);
                }
                _ => {}
            }
            if let Some((name, span)) = used
                && !scopes.iter().rev().any(|scope| scope.contains(name))
                && let Some(task) = pending.get(name)
            {
                self.escape(task, span);
            }
            if let Some(span) = exit {
                for task in pending.values() {
                    self.escape(task, span);
                }
            }
        }
    }

    fn block<'a>(&mut self, block: &'a Block, blocks: &mut Vec<&'a Block>) {
        let mut pending: HashMap<&str, BorrowedTask> = HashMap::new();
        for stmt in &block.stmts {
            let root = match stmt {
                Stmt::Let(s) => Some(&s.init),
                Stmt::Expr(s) => Some(&s.expr),
                Stmt::Return(s) => s.value.as_ref(),
                _ => None,
            };
            let mut allowed_call = None;
            let mut allowed_var = None;
            let mut new_binding = None;
            if let Stmt::Let(binding) = stmt {
                if let Some(old) = pending.remove(binding.name.as_str()) {
                    self.escape(&old, binding.span);
                }
                if let Some(task) = self.borrowed_call(&binding.init, block.span) {
                    allowed_call = Some(task.call);
                    new_binding = Some((binding.name.as_str(), task));
                }
            }
            if let Some(Expr::Await(wait)) = root {
                let operand = match &wait.expr {
                    Expr::MethodCall(c)
                        if c.method == "result"
                            && c.args.is_empty()
                            && self.types.get(&c.object.id()).is_some_and(|t| {
                                builtin_types::unary_arg(t, BuiltinTypeId::Task).is_some()
                            }) =>
                    {
                        &c.object
                    }
                    operand => operand,
                };
                if let Some(task) = self.borrowed_call(operand, block.span) {
                    allowed_call = Some(task.call);
                } else if let Expr::Var(name, _, id) = operand
                    && pending.remove(name.as_str()).is_some()
                {
                    allowed_var = Some(*id);
                }
            }
            self.check_pending_uses(stmt, &pending, allowed_var);
            let mut walk = AstWalk::new(AstEvent::Stmt(stmt));
            while let Some(event) = walk.next() {
                match event {
                    AstEvent::Block(child) => {
                        // Each nested block proves discharge of its own tasks.
                        // Outer task uses and exits were checked with lexical
                        // binding information before this traversal.
                        blocks.push(child);
                        walk.skip_children();
                    }
                    AstEvent::Expr(expr) => {
                        if Some(expr.id()) != allowed_call
                            && let Some(task) = self.borrowed_call(expr, block.span)
                        {
                            self.escape(&task, expr.span());
                        }
                    }
                    _ => {}
                }
            }
            if let Some((name, task)) = new_binding {
                pending.insert(name, task);
            }
        }
        for task in pending.values() {
            self.escape(task, block.span);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer::Lexer, parser::Parser, semantic::TypeChecker};

    fn analyze(body: &str) -> (Vec<Diagnostic>, Vec<Diagnostic>) {
        let source = format!(
            "async fn borrow(x: &i64) -> i64 {{ return x; }} async fn other() -> i64 {{ return 0; }} fn consume(task: Task<i64>) {{}} async fn test() {{ let x = 1; {body} }} fn main() {{}}"
        );
        analyze_source(&source)
    }

    fn analyze_source(source: &str) -> (Vec<Diagnostic>, Vec<Diagnostic>) {
        let tokens = Lexer::new(source).tokenize().unwrap();
        let (program, errors) = Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{source}: {errors:?}");
        let mut checker = TypeChecker::new();
        checker.check_module_program(&program);
        let borrows = check(&program, &checker.expr_types, &checker.reference_arg_modes);
        (borrows, checker.errors)
    }

    #[test]
    fn constructor_borrowed_task_escape_is_checked() {
        let (borrows, type_errors) = analyze_source(
            "async fn borrow(x: &i64) -> i64 { return x; } \
             class Owner { init(self) { let x = 1; let task = borrow(&x); } } \
             fn main() {}",
        );
        assert!(type_errors.is_empty(), "{type_errors:?}");
        assert_eq!(borrows.len(), 1, "{borrows:?}");
        assert_eq!(borrows[0].code, ErrorCode::E1708);
    }

    #[test]
    fn interface_default_body_borrowed_task_escape_is_checked() {
        let (borrows, type_errors) = analyze_source(
            "async fn borrow(x: &i64) -> i64 { return x; } \
             interface Owner { fn run(self) { let x = 1; let task = borrow(&x); } } \
             fn main() {}",
        );
        assert!(type_errors.is_empty(), "{type_errors:?}");
        assert_eq!(borrows.len(), 1, "{borrows:?}");
        assert_eq!(borrows[0].code, ErrorCode::E1708);
    }

    #[test]
    fn static_initializer_borrowed_task_escape_is_checked() {
        let (borrows, type_errors) = analyze_source(
            "import std::collections::Array; \
             async fn borrow(x: &i64) -> i64 { return x; } \
             class Owner { static values: Array<i64> = [1]; \
             static task: Task<i64> = borrow(&Owner::values[0]); } fn main() {}",
        );
        assert!(type_errors.is_empty(), "{type_errors:?}");
        assert_eq!(borrows.len(), 1, "{borrows:?}");
        assert_eq!(borrows[0].code, ErrorCode::E1708);
    }

    #[test]
    fn synchronous_reference_calls_return_independent_tasks() {
        for (declaration, call) in [
            (
                "fn make(x: &i64) -> Task<i64> { return other(); }",
                "make(&x)",
            ),
            (
                "class Factory { pub fn make(self, x: &i64) -> Task<i64> { return other(); } }",
                "new Factory().make(&x)",
            ),
            (
                "class Factory { pub static fn make(x: &i64) -> Task<i64> { return other(); } }",
                "Factory::make(&x)",
            ),
        ] {
            let source = format!(
                "async fn other() -> i64 {{ return 42; }} {declaration} \
                 fn consume(task: Task<i64>) {{}} \
                 fn main() {{ let x = 1; let task = {call}; consume(task); }}"
            );
            let (borrows, type_errors) = analyze_source(&source);
            assert!(type_errors.is_empty(), "{source}: {type_errors:?}");
            assert!(borrows.is_empty(), "{source}: {borrows:?}");
        }
    }

    #[test]
    fn asynchronous_reference_call_variants_still_reject_escape() {
        for (declaration, call) in [
            ("async fn make(x: &i64) -> i64 { return x; }", "make(&x)"),
            (
                "class Factory { pub async fn make(self, x: &i64) -> i64 { return x; } }",
                "new Factory().make(&x)",
            ),
            (
                "class Factory { pub static async fn make(x: &i64) -> i64 { return x; } }",
                "Factory::make(&x)",
            ),
        ] {
            let source = format!(
                "{declaration} fn consume(task: Task<i64>) {{}} \
                 fn main() {{ let x = 1; let task = {call}; consume(task); }}"
            );
            let (borrows, type_errors) = analyze_source(&source);
            assert!(type_errors.is_empty(), "{source}: {type_errors:?}");
            assert_eq!(borrows.len(), 1, "{source}: {borrows:?}");
            assert_eq!(borrows[0].code, ErrorCode::E1708);
        }
    }

    #[test]
    fn narrow_discharge_shapes_do_not_emit_e1708() {
        for body in [
            "await borrow(&x);",
            "let value = await borrow(&x);",
            "let task = borrow(&x); await task;",
            "let task = borrow(&x); let value = await task;",
            "let task = borrow(&x); await task.result();",
            "let task = borrow(&x); let value = await task.result();",
            "let first = borrow(&x); let second = borrow(&x); await first; await second;",
            "let first = borrow(&x); let second = borrow(&x); await second; await first;",
            "if true { let task = borrow(&x); await task; }",
            "while false { let task = borrow(&x); await task; }",
            "let task = borrow(&x); await task; let alias = task;",
            "let task = other(); let alias = task;",
            "let task = borrow(&x); if true { let task = 1; println(task); } await task;",
            "let task = borrow(&x); if true { let mut task = 1; task = 2; } await task;",
            "let task = borrow(&x); let f = || { return 1; }; await task;",
            "let task = borrow(&x); let f = |task: i64| { return task; }; await task;",
            "let task = borrow(&x); while true { break; } await task;",
            "let task = borrow(&x); while false { continue; } await task;",
        ] {
            let (borrows, type_errors) = analyze(body);
            assert!(type_errors.is_empty(), "{body}: {type_errors:?}");
            assert!(borrows.is_empty(), "{body}: {borrows:?}");
        }
    }

    #[test]
    fn unsupported_transfers_and_exits_emit_e1708() {
        for body in [
            "borrow(&x);",
            "let task = borrow(&x);",
            "return borrow(&x);",
            "let task = borrow(&x); return task;",
            "let task = borrow(&x); let alias = task; await task;",
            "let task = borrow(&x); let tasks = [task]; await task;",
            "let tasks = [borrow(&x)];",
            "let task = borrow(&x); consume(task); await task;",
            "consume(borrow(&x));",
            "let task = borrow(&x); task.cancel(); await task;",
            "let task = borrow(&x); task = other(); await task;",
            "let task = borrow(&x); if true { return; } await task;",
            "let task = borrow(&x); if true { await task; }",
            "let task = borrow(&x); while false { await task; }",
            "while true { let task = borrow(&x); break; }",
            "while true { let task = borrow(&x); continue; }",
            "let task = borrow(&x); return; await task;",
            "let task = borrow(&x); object.field = task; await task;",
            "let task = borrow(&x); tasks[0] = task; await task;",
            "let task = borrow(&x); Holder::saved = task; await task;",
            "while true { let task = borrow(&x); if true { break; } await task; }",
            "while true { let task = borrow(&x); if true { continue; } await task; }",
            "let mut task = borrow(&x); if true { task = other(); } await task;",
            "let task = borrow(&x); if true { let result: Result<i64, String> = Result::Err(\"stop\"); result?; } await task;",
            "let task = borrow(&x); let f = || { return task; }; await task;",
            "let task = borrow(&x); let f = || task; await task;",
            "let task = borrow(&x); if true { let alias = task; } await task;",
        ] {
            assert!(
                analyze(body)
                    .0
                    .iter()
                    .any(|error| error.code == ErrorCode::E1708),
                "{body}"
            );
        }
    }
}
