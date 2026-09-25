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
    check_with(program, types, modes, |_, compute| Ok(compute()))
        .expect("standalone borrow analysis is infallible")
}

pub(crate) enum BorrowRoot {
    Block(BodyId),
    Initializer(ExprId),
}

/// Preserve the established initializer-first emission order while allowing
/// the session to memoize each root independently. One shared deduplication
/// set at aggregation retains injected defaults with shared source ExprIds.
pub(crate) fn check_with(
    program: &Program,
    types: &HashMap<ExprId, Type>,
    modes: &HashMap<ExprId, ParamMode>,
    mut evaluate: impl FnMut(
        BorrowRoot,
        &mut dyn FnMut() -> BorrowReport,
    ) -> anyhow::Result<BorrowReport>,
) -> anyhow::Result<Vec<Diagnostic>> {
    if !modes
        .values()
        .any(|mode| matches!(mode, ParamMode::Reference { .. }))
    {
        return Ok(Vec::new());
    }
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
    let mut reports = Vec::new();
    for (initializer, boundary) in initializers {
        // Field initializers report escapes at the whole initializer, as
        // before the single-walk rewrite; bodies report at the call itself.
        let id = initializer.id();
        reports.extend(evaluate(BorrowRoot::Initializer(id), &mut || {
            checker.reported = HashSet::new();
            checker.walk(
                AstEvent::Expr(initializer),
                boundary,
                Some(initializer.span()),
            );
            std::mem::take(&mut checker.errors)
        })?);
    }
    for block in blocks {
        reports.extend(evaluate(BorrowRoot::Block(block.id), &mut || {
            checker.reported = HashSet::new();
            checker.walk(AstEvent::Block(block), block.span, None);
            std::mem::take(&mut checker.errors)
        })?);
    }
    let mut reported = HashSet::new();
    Ok(reports
        .into_iter()
        .filter_map(|(id, diagnostic)| reported.insert(id).then_some(diagnostic))
        .collect())
}

pub(crate) type BorrowReport = Vec<(ExprId, Diagnostic)>;

struct Checker<'a> {
    types: &'a HashMap<ExprId, Type>,
    modes: &'a HashMap<ExprId, ParamMode>,
    errors: BorrowReport,
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
        self.errors.push((task.call, diagnostic));
    }

    /// One source-order walk handles both local discharge and ancestor uses.
    /// Names resolve through binding stacks; exits drain indexed active tasks,
    /// so already-reported tasks never multiply work at later exits.
    fn walk<'a>(&mut self, root: AstEvent<'a>, boundary: Span, escape_label: Option<Span>) {
        let mut state = WalkState::default();
        let mut next_block = None;
        let mut statements = Vec::new();
        let mut callable = 0usize;
        let mut loops = 0usize;
        for event in AstWalk::new(root) {
            count_event();
            match event {
                AstEvent::Block(block) => next_block = Some(block.span),
                AstEvent::EnterScope => {
                    state.scopes.push(Vec::new());
                    if let Some(span) = next_block.take() {
                        state.blocks.push(BlockState {
                            span,
                            scope: state.scopes.len(),
                            pending: HashMap::new(),
                        });
                    }
                }
                AstEvent::ExitScope => {
                    if state
                        .blocks
                        .last()
                        .is_some_and(|block| block.scope == state.scopes.len())
                    {
                        let block = state.blocks.pop().unwrap();
                        for call in block.pending.into_values() {
                            state.escape(self, call, block.span);
                        }
                    }
                    for name in state.scopes.pop().unwrap().into_iter().rev() {
                        let bindings = state.names.get_mut(name).unwrap();
                        bindings.pop();
                        if bindings.is_empty() {
                            state.names.remove(name);
                        }
                    }
                }
                AstEvent::Bind(name) => {
                    state.names.entry(name).or_default().push(None);
                    state.scopes.last_mut().unwrap().push(name);
                }
                AstEvent::Stmt(stmt) => {
                    let mut statement = StatementState {
                        allowed_call: None,
                        new_binding: None,
                    };
                    let block_span = state.blocks.last().map_or(boundary, |b| b.span);
                    if let Stmt::Let(binding) = stmt {
                        let old = state
                            .blocks
                            .last_mut()
                            .and_then(|b| b.pending.remove(binding.name.as_str()));
                        if let Some(call) = old {
                            state.escape(self, call, binding.span);
                        }
                        if let Some(task) = self.borrowed_call(&binding.init, block_span) {
                            statement.allowed_call = Some(task.call);
                            statement.new_binding = Some((binding.name.as_str(), task));
                        }
                    }
                    let root = match stmt {
                        Stmt::Let(s) => Some(&s.init),
                        Stmt::Expr(s) => Some(&s.expr),
                        Stmt::Return(s) => s.value.as_ref(),
                        _ => None,
                    };
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
                        if let Some(task) = self.borrowed_call(operand, block_span) {
                            statement.allowed_call = Some(task.call);
                        } else if let Expr::Var(name, _, _) = operand {
                            let call = state
                                .blocks
                                .last_mut()
                                .and_then(|b| b.pending.remove(name.as_str()));
                            if let Some(call) = call {
                                state.remove(call);
                            }
                        }
                    }
                    statements.push(statement);
                    match stmt {
                        Stmt::While(_) | Stmt::For(_) => loops += 1,
                        Stmt::Assign(assign) => state.use_name(self, &assign.name, assign.span),
                        Stmt::Return(ret) => state.exit(self, callable, None, ret.span),
                        Stmt::Break(span) | Stmt::Continue(span) => {
                            state.exit(self, callable, Some(loops), *span)
                        }
                        _ => {}
                    }
                }
                AstEvent::ExitStmt(stmt) => {
                    if matches!(stmt, Stmt::While(_) | Stmt::For(_)) {
                        loops -= 1;
                    }
                    if let Some((name, task)) = statements.pop().unwrap().new_binding {
                        let call = task.call;
                        state.blocks.last_mut().unwrap().pending.insert(name, call);
                        *state.names.get_mut(name).unwrap().last_mut().unwrap() = Some(call);
                        state.by_callable.entry(callable).or_default().insert(call);
                        state
                            .by_loop
                            .entry((callable, loops))
                            .or_default()
                            .insert(call);
                        state.active.insert(call, (task, callable, loops));
                    }
                }
                AstEvent::Expr(expr) => {
                    match expr {
                        Expr::Lambda(_) => callable += 1,
                        Expr::Var(name, span, _) => state.use_name(self, name, *span),
                        Expr::TryPropagate(_, span, _) => state.exit(self, callable, None, *span),
                        _ => {}
                    }
                    let span = state.blocks.last().map_or(boundary, |b| b.span);
                    if Some(expr.id()) != statements.last().and_then(|s| s.allowed_call)
                        && let Some(task) = self.borrowed_call(expr, span)
                    {
                        self.escape(&task, escape_label.unwrap_or_else(|| expr.span()));
                    }
                }
                AstEvent::ExitExpr(Expr::Lambda(_)) => callable -= 1,
                _ => {}
            }
        }
    }
}

struct BlockState<'a> {
    span: Span,
    scope: usize,
    pending: HashMap<&'a str, ExprId>,
}
struct StatementState<'a> {
    allowed_call: Option<ExprId>,
    new_binding: Option<(&'a str, BorrowedTask)>,
}
#[derive(Default)]
struct WalkState<'a> {
    scopes: Vec<Vec<&'a str>>,
    names: HashMap<&'a str, Vec<Option<ExprId>>>,
    blocks: Vec<BlockState<'a>>,
    active: HashMap<ExprId, (BorrowedTask, usize, usize)>,
    by_callable: HashMap<usize, HashSet<ExprId>>,
    by_loop: HashMap<(usize, usize), HashSet<ExprId>>,
}
impl WalkState<'_> {
    fn remove(&mut self, call: ExprId) -> Option<BorrowedTask> {
        let (task, callable, loops) = self.active.remove(&call)?;
        remove_active_index(&mut self.by_callable, callable, call);
        remove_active_index(&mut self.by_loop, (callable, loops), call);
        Some(task)
    }
    fn escape(&mut self, checker: &mut Checker<'_>, call: ExprId, span: Span) {
        if let Some(task) = self.remove(call) {
            checker.escape(&task, span);
        }
    }
    fn use_name(&mut self, checker: &mut Checker<'_>, name: &str, span: Span) {
        if let Some(Some(call)) = self.names.get(name).and_then(|names| names.last()) {
            self.escape(checker, *call, span);
        }
    }
    fn exit(
        &mut self,
        checker: &mut Checker<'_>,
        callable: usize,
        loops: Option<usize>,
        span: Span,
    ) {
        let calls = match loops {
            Some(loops) => self.by_loop.remove(&(callable, loops)),
            None => self.by_callable.remove(&callable),
        };
        for call in calls.into_iter().flatten() {
            self.escape(checker, call, span);
        }
    }
}

fn remove_active_index<K: std::hash::Hash + Eq>(
    index: &mut HashMap<K, HashSet<ExprId>>,
    key: K,
    call: ExprId,
) {
    if let Some(calls) = index.get_mut(&key) {
        calls.remove(&call);
        if calls.is_empty() {
            index.remove(&key);
        } else if calls.len() <= calls.capacity() / 4 {
            calls.shrink_to_fit();
        }
    }
}

#[inline]
fn count_event() {
    #[cfg(test)]
    AST_EVENTS.with(|count| count.set(count.get() + 1));
}
#[cfg(test)]
thread_local! { static AST_EVENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

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
    fn nested_walk_visits_each_event_once_in_mixed_programs() {
        for depth in [32, 64, 128] {
            for pending in [false, true] {
                let nested = format!(
                    "{}println(1);{}",
                    "if true {".repeat(depth),
                    "}".repeat(depth)
                );
                let body = if pending {
                    format!("let x = 1; let task = borrow(&x); {nested} await task;")
                } else {
                    nested
                };
                let source = format!(
                    "async fn borrow(x: &i64) -> i64 {{ return x; }} async fn other() {{ let x = 1; await borrow(&x); }} async fn main() {{ {body} }}"
                );
                let (program, errors) =
                    Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
                assert!(errors.is_empty());
                let mut checker = TypeChecker::new();
                checker.check_module_program(&program);
                assert!(checker.errors.is_empty(), "{:?}", checker.errors);
                let expected: usize = program
                    .items
                    .iter()
                    .filter_map(|item| match item {
                        Item::Function(f) => Some(AstWalk::new(AstEvent::Block(&f.body)).count()),
                        _ => None,
                    })
                    .sum();
                AST_EVENTS.with(|count| count.set(0));
                assert!(
                    check(&program, &checker.expr_types, &checker.reference_arg_modes).is_empty()
                );
                let actual = AST_EVENTS.with(|count| count.get());
                assert_eq!(actual, expected, "depth={depth}, pending={pending}");
                println!("depth={depth} pending={pending} events={actual}");
                AST_EVENTS.with(|count| count.set(0));
                assert!(check(&program, &HashMap::new(), &HashMap::new()).is_empty());
                assert_eq!(AST_EVENTS.with(|count| count.get()), 0);
            }
        }
    }

    #[test]
    fn nested_pending_tasks_and_repeated_exits_report_once_each() {
        let (borrows, errors) = analyze(
            "let outer = borrow(&x); if true { let inner = borrow(&x); if true { return; } await inner; } if true { return; } await outer;",
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(borrows.len(), 2, "{borrows:?}");
        let (borrows, errors) = analyze(
            "let task = borrow(&x); let f = || { let inner = borrow(&x); return; }; await task;",
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(borrows.len(), 1, "{borrows:?}");
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
