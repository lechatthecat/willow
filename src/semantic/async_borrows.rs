//! Conservative lexical discharge proof for tasks holding reference arguments.
//! Direct awaits and locally tracked TaskScope joins discharge captured loans.
//! Shared and mutable scalar declarations are supported; this pass still rejects task
//! escape independently of the declaration restrictions for other references.
use std::collections::{HashMap, HashSet};

use super::builtin_types::{self, BuiltinTypeId};
use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use crate::parser::iter::{AstEvent, AstWalk};

/// Resolved reference argument facts. Task return type alone does not imply
/// retention: a synchronous callee may return an independent task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReferenceArgMode {
    pub mutable: bool,
    pub retained: bool,
}

#[derive(Clone)]
struct BorrowedTask {
    call: ExprId,
    origin: Span,
    block: Span,
    references: Vec<(Span, Option<bool>)>,
    places: Vec<(String, bool, Span)>,
}

pub fn check(
    program: &Program,
    types: &HashMap<ExprId, Type>,
    modes: &HashMap<ExprId, ReferenceArgMode>,
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
    modes: &HashMap<ExprId, ReferenceArgMode>,
    mut evaluate: impl FnMut(
        BorrowRoot,
        &mut dyn FnMut() -> BorrowReport,
    ) -> anyhow::Result<BorrowReport>,
) -> anyhow::Result<Vec<Diagnostic>> {
    if !modes.values().any(|mode| mode.retained) {
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
    modes: &'a HashMap<ExprId, ReferenceArgMode>,
    errors: BorrowReport,
    reported: HashSet<ExprId>,
}

impl Checker<'_> {
    fn borrowed_call(&self, expr: &Expr, block: Span) -> Option<BorrowedTask> {
        let ty = self.types.get(&expr.id())?;
        builtin_types::unary_arg(ty, BuiltinTypeId::Task)?;
        let args = reference_call_args(expr)?;
        let references: Vec<_> = args
            .iter()
            .filter_map(|arg| match arg.mode {
                CallArgMode::Reference { ampersand_span } => {
                    let mode = self.modes.get(&arg.expr.id())?;
                    mode.retained
                        .then_some((ampersand_span, Some(mode.mutable)))
                }
                CallArgMode::Value => None,
            })
            .collect();
        (!references.is_empty()).then(|| BorrowedTask {
            call: expr.id(),
            origin: expr.span(),
            block,
            references,
            places: reference_places(args, self.modes).collect(),
        })
    }

    fn escape(&mut self, task: &BorrowedTask, span: Span) {
        if !self.reported.insert(task.call) {
            return;
        }
        let mut diagnostic = Diagnostic::new(
            Severity::Error, ErrorCode::E1708,
            "async reference borrow escapes its discharge block",
        ).with_label(Label::primary(span, "task must be awaited or joined by its scope in its originating block"))
         .with_label(Label::secondary(task.origin, "task captures reference arguments here"))
         .with_label(Label::secondary(task.block, "borrow discharge block"))
         .with_help("await the task or its owning TaskScope.finish() in the same block before any transfer or exit");
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
        let mut deferred = 0usize;
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
                            scoped: Vec::new(),
                        });
                    }
                }
                AstEvent::ExitScope => {
                    if let Some(block) = state
                        .blocks
                        .pop_if(|block| block.scope == state.scopes.len())
                    {
                        for call in block.pending.into_values().chain(block.scoped) {
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
                    state.next_binding += 1;
                    state
                        .names
                        .entry(name)
                        .or_default()
                        .push((state.next_binding, None));
                    state.scopes.last_mut().unwrap().push(name);
                }
                AstEvent::Stmt(stmt) => {
                    let mut statement = StatementState {
                        allowed_call: None,
                        new_binding: None,
                        scope_binding: None,
                        scope_receiver: None,
                        transferred: None,
                        attach: None,
                    };
                    if matches!(stmt, Stmt::Defer(_)) {
                        deferred += 1;
                    }
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
                            let keys = task
                                .places
                                .iter()
                                .map(|(name, _, _)| state.place_key(name))
                                .collect();
                            statement.new_binding = Some(PendingBinding {
                                name: Some(binding.name.as_str()),
                                task,
                                keys,
                            });
                        }
                    }
                    let root = match stmt {
                        Stmt::Let(s) => Some(&s.init),
                        Stmt::Expr(s) => Some(&s.expr),
                        Stmt::Return(s) => s.value.as_ref(),
                        _ => None,
                    };
                    if deferred == 0 {
                        state.prepare_scope(
                            self,
                            stmt,
                            root,
                            &mut statement,
                            callable,
                            loops,
                            block_span,
                        );
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
                        Stmt::Assign(assign) => {
                            state.invalidate_scope(self, &assign.name, assign.span);
                            state.use_name(self, &assign.name, assign.span);
                        }
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
                    if matches!(stmt, Stmt::Defer(_)) {
                        deferred -= 1;
                    }
                    let statement = statements.pop().unwrap();
                    if let Some((name, scope)) = statement.scope_binding {
                        let binding = state.names[name].last().unwrap().0;
                        state.scope_bindings.insert(binding, scope);
                    }
                    if let Some(PendingBinding { name, task, keys }) = statement.new_binding {
                        let call = task.call;
                        if let Some(name) = name {
                            state.blocks.last_mut().unwrap().pending.insert(name, call);
                            state.names.get_mut(name).unwrap().last_mut().unwrap().1 = Some(call);
                        }
                        state.by_callable.entry(callable).or_default().insert(call);
                        state
                            .by_loop
                            .entry((callable, loops))
                            .or_default()
                            .insert(call);
                        state.retain_places(&task, keys);
                        state.active.insert(call, (task, callable, loops));
                    }
                    if let Some((scope, call, binding)) = statement.attach {
                        state.task_scopes[scope].tasks.push(call);
                        state.blocks.last_mut().unwrap().scoped.push(call);
                        if let Some(name) = binding {
                            state.blocks.last_mut().unwrap().pending.insert(name, call);
                            state.names.get_mut(name).unwrap().last_mut().unwrap().1 = Some(call);
                        }
                    }
                }
                AstEvent::Expr(expr) => {
                    match expr {
                        Expr::Lambda(_) => callable += 1,
                        Expr::Var(name, span, id) => {
                            let statement = statements.last();
                            if statement.and_then(|s| s.scope_receiver) != Some(*id) {
                                state.invalidate_scope(self, name, *span);
                            }
                            if statement.and_then(|s| s.transferred) != Some(*id) {
                                state.use_name(self, name, *span);
                            }
                        }
                        Expr::TryPropagate(_, span, _) => state.exit(self, callable, None, *span),
                        _ => {}
                    }
                    let span = state.blocks.last().map_or(boundary, |b| b.span);
                    if let Some(args) = reference_call_args(expr) {
                        state.check_overlap(self, expr.id(), args);
                    }
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

fn reference_call_args(expr: &Expr) -> Option<&[CallArg]> {
    match expr {
        Expr::Call(call) => Some(&call.args),
        Expr::MethodCall(call) => Some(&call.args),
        Expr::StaticCall(call) => Some(&call.args),
        Expr::New(call) => Some(&call.args),
        _ => None,
    }
}

fn reference_places<'a>(
    args: &'a [CallArg],
    modes: &'a HashMap<ExprId, ReferenceArgMode>,
) -> impl Iterator<Item = (String, bool, Span)> + 'a {
    args.iter().filter_map(|arg| {
        let mode = modes.get(&arg.expr.id())?;
        let key = super::type_checker::analysis::reference_place_key(&arg.expr)?;
        Some((key, mode.mutable, arg.span))
    })
}

struct BlockState<'a> {
    span: Span,
    scope: usize,
    pending: HashMap<&'a str, ExprId>,
    scoped: Vec<ExprId>,
}
struct StatementState<'a> {
    allowed_call: Option<ExprId>,
    new_binding: Option<PendingBinding<'a>>,
    scope_binding: Option<(&'a str, usize)>,
    scope_receiver: Option<ExprId>,
    transferred: Option<ExprId>,
    attach: Option<(usize, ExprId, Option<&'a str>)>,
}
struct PendingBinding<'a> {
    name: Option<&'a str>,
    task: BorrowedTask,
    keys: Vec<(usize, String)>,
}
#[derive(Default)]
struct WalkState<'a> {
    scope_bindings: HashMap<usize, usize>,
    task_scopes: Vec<TaskScopeState>,
    scopes: Vec<Vec<&'a str>>,
    names: HashMap<&'a str, Vec<(usize, Option<ExprId>)>>,
    next_binding: usize,
    places: HashMap<(usize, String), PlaceLoans>,
    task_places: HashMap<ExprId, Vec<(usize, String)>>,
    blocks: Vec<BlockState<'a>>,
    active: HashMap<ExprId, (BorrowedTask, usize, usize)>,
    by_callable: HashMap<usize, HashSet<ExprId>>,
    by_loop: HashMap<(usize, usize), HashSet<ExprId>>,
}
struct TaskScopeState {
    children: Vec<usize>,
    tasks: Vec<ExprId>,
    open: bool,
    callable: usize,
    loops: usize,
}
#[derive(Default)]
struct PlaceLoans {
    mutable: LoanSet,
    shared: LoanSet,
}

// Dense witnesses avoid scanning a sparse hash table after partial discharge.
#[derive(Default)]
struct LoanSet {
    entries: Vec<(ExprId, Span)>,
    positions: HashMap<ExprId, usize>,
}
impl LoanSet {
    fn insert(&mut self, call: ExprId, span: Span) {
        if let std::collections::hash_map::Entry::Vacant(entry) = self.positions.entry(call) {
            entry.insert(self.entries.len());
            self.entries.push((call, span));
        }
    }
    fn remove(&mut self, call: &ExprId) {
        if let Some(index) = self.positions.remove(call) {
            self.entries.swap_remove(index);
            if let Some((moved, _)) = self.entries.get(index) {
                self.positions.insert(*moved, index);
            }
        }
    }
    fn first(&self) -> Option<&Span> {
        self.entries.first().map(|(_, span)| span)
    }
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl<'a> WalkState<'a> {
    fn scope_named(&self, name: &str) -> Option<usize> {
        let binding = self.names.get(name)?.last()?.0;
        self.scope_bindings.get(&binding).copied()
    }

    fn scope_receiver(
        &self,
        checker: &Checker<'_>,
        expr: &Expr,
        callable: usize,
        loops: usize,
    ) -> Option<usize> {
        let Expr::Var(name, _, _) = expr else {
            return None;
        };
        if builtin_types::resolve(checker.types.get(&expr.id())?)?.id != BuiltinTypeId::TaskScope {
            return None;
        }
        let scope = self.scope_named(name)?;
        let node = &self.task_scopes[scope];
        (node.open && node.callable == callable && node.loops == loops).then_some(scope)
    }

    // Each scope edge and registration is drained at most once. Finishing a
    // parent includes descendants; a conditional finish cannot discharge a
    // task originating outside the current lexical block.
    fn drain_scope(
        &mut self,
        checker: &mut Checker<'_>,
        scope: usize,
        finish: Option<Span>,
        span: Span,
    ) {
        let mut work = vec![scope];
        while let Some(scope) = work.pop() {
            count_scope_work();
            let node = &mut self.task_scopes[scope];
            node.open = false;
            work.append(&mut node.children);
            let tasks = std::mem::take(&mut node.tasks);
            for call in tasks {
                count_scope_work();
                if finish.is_some_and(|block| {
                    self.active
                        .get(&call)
                        .is_some_and(|(task, _, _)| task.block == block)
                }) {
                    self.remove(call);
                } else {
                    self.escape(checker, call, span);
                }
            }
        }
    }

    fn invalidate_scope(&mut self, checker: &mut Checker<'_>, name: &str, span: Span) {
        if let Some(scope) = self.scope_named(name) {
            self.drain_scope(checker, scope, None, span);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_scope(
        &mut self,
        checker: &mut Checker<'_>,
        stmt: &'a Stmt,
        root: Option<&'a Expr>,
        statement: &mut StatementState<'a>,
        callable: usize,
        loops: usize,
        block: Span,
    ) {
        let binding = match stmt {
            Stmt::Let(s) => Some(s.name.as_str()),
            _ => None,
        };
        let Some(root) = root else {
            return;
        };
        if let Some(name) = binding
            && matches!(root, Expr::StaticCall(c) if c.class == "TaskScope" && c.method == "new" && c.args.is_empty())
        {
            let scope = self.task_scopes.len();
            self.task_scopes.push(TaskScopeState {
                children: Vec::new(),
                tasks: Vec::new(),
                open: true,
                callable,
                loops,
            });
            statement.scope_binding = Some((name, scope));
            return;
        }
        let awaited = match root {
            Expr::Await(wait) => Some(&wait.expr),
            Expr::Match(m) => match &*m.scrutinee {
                Expr::Await(wait) => Some(&wait.expr),
                _ => None,
            },
            _ => None,
        };
        if let Some(Expr::MethodCall(call)) = awaited
            && call.method == "finish"
            && call.args.is_empty()
            && let Some(scope) = self.scope_receiver(checker, &call.object, callable, loops)
        {
            statement.scope_receiver = Some(call.object.id());
            self.drain_scope(checker, scope, Some(block), call.span);
            return;
        }
        let Expr::MethodCall(call) = root else {
            return;
        };
        let Some(scope) = self.scope_receiver(checker, &call.object, callable, loops) else {
            return;
        };
        if call.method == "cancel" && call.args.is_empty() {
            // Requesting cancellation preserves membership and every loan.
            // Only awaiting finish proves that all observing defers have run.
            statement.scope_receiver = Some(call.object.id());
        } else if call.method == "child"
            && call.args.is_empty()
            && let Some(name) = binding
        {
            let child = self.task_scopes.len();
            self.task_scopes.push(TaskScopeState {
                children: Vec::new(),
                tasks: Vec::new(),
                open: true,
                callable,
                loops,
            });
            self.task_scopes[scope].children.push(child);
            statement.scope_binding = Some((name, child));
            statement.scope_receiver = Some(call.object.id());
        } else if call.method == "add" && call.args.len() == 1 {
            let arg = &call.args[0].expr;
            if let Some(task) = checker.borrowed_call(arg, block) {
                let keys = task
                    .places
                    .iter()
                    .map(|(name, _, _)| self.place_key(name))
                    .collect();
                statement.allowed_call = Some(task.call);
                statement.attach = Some((scope, task.call, binding));
                statement.new_binding = Some(PendingBinding {
                    name: None,
                    task,
                    keys,
                });
            } else if let Expr::Var(name, _, _) = arg
                && let Some(call) = self
                    .blocks
                    .last()
                    .and_then(|b| b.pending.get(name.as_str()))
                    .copied()
                && self.active.contains_key(&call)
            {
                statement.transferred = Some(arg.id());
                statement.attach = Some((scope, call, binding));
            }
            statement.scope_receiver = Some(call.object.id());
        }
    }

    fn place_key(&self, name: &str) -> (usize, String) {
        let root = name.split(['.', '[']).next().unwrap();
        let binding = self
            .names
            .get(root)
            .and_then(|stack| stack.last())
            .map_or(0, |entry| entry.0);
        (binding, name.to_owned())
    }

    fn check_overlap(&self, checker: &mut Checker<'_>, call: ExprId, args: &[CallArg]) {
        for (name, mutable, span) in reference_places(args, checker.modes) {
            count_place_lookup();
            let Some(loans) = self.places.get(&self.place_key(&name)) else {
                continue;
            };
            // One witness is enough; do not enumerate all pairs of shared loans.
            let previous = loans
                .mutable
                .first()
                .or_else(|| if mutable { loans.shared.first() } else { None });
            if let Some(previous) = previous {
                checker.errors.push((
                    call,
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1706,
                        format!("cannot pass `{name}` while it aliases a mutable reference"),
                    )
                    .with_label(Label::primary(
                        span,
                        "same place aliases an outstanding async borrow",
                    ))
                    .with_label(Label::secondary(
                        *previous,
                        "task borrowing this place has not been awaited",
                    ))
                    .with_help("await the earlier task before borrowing this place again"),
                ));
                break;
            }
        }
    }

    fn retain_places(&mut self, task: &BorrowedTask, keys: Vec<(usize, String)>) {
        for ((_, mutable, span), key) in task.places.iter().zip(&keys) {
            let loans = self.places.entry(key.clone()).or_default();
            if *mutable {
                loans.mutable.insert(task.call, *span);
            } else {
                loans.shared.insert(task.call, *span);
            }
        }
        self.task_places.insert(task.call, keys);
    }
    fn remove(&mut self, call: ExprId) -> Option<BorrowedTask> {
        let (task, callable, loops) = self.active.remove(&call)?;
        for key in self.task_places.remove(&call).into_iter().flatten() {
            if let Some(loans) = self.places.get_mut(&key) {
                loans.mutable.remove(&call);
                loans.shared.remove(&call);
                if loans.mutable.is_empty() && loans.shared.is_empty() {
                    self.places.remove(&key);
                }
            }
        }
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
        if let Some((_, Some(call))) = self.names.get(name).and_then(|names| names.last()) {
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
fn count_scope_work() {
    #[cfg(test)]
    SCOPE_WORK.with(|count| count.set(count.get() + 1));
}
#[cfg(test)]
thread_local! { static SCOPE_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[inline]
fn count_event() {
    #[cfg(test)]
    AST_EVENTS.with(|count| count.set(count.get() + 1));
}
#[inline]
fn count_place_lookup() {
    #[cfg(test)]
    PLACE_LOOKUPS.with(|count| count.set(count.get() + 1));
}
#[cfg(test)]
thread_local! { static PLACE_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
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
    fn scoped_borrow_many_registrations_fragmented_and_repeated() {
        for n in [1, 8, 64, 128] {
            for repeated in [false, true] {
                let mut body = String::from("let s = TaskScope::new();");
                if repeated {
                    body.push_str("let t = borrow(&x);");
                }
                for i in 0..n {
                    if repeated {
                        body.push_str("s.add(t);");
                    } else {
                        body.push_str(&format!("let t{i} = s.add(borrow(&x));"));
                    }
                }
                if !repeated {
                    for i in (0..n).step_by(2) {
                        body.push_str(&format!("await t{i};"));
                    }
                }
                body.push_str("await s.finish();");
                SCOPE_WORK.with(|count| count.set(0));
                let (borrows, errors) = analyze(&body);
                assert!(errors.is_empty(), "{errors:?}");
                assert!(borrows.is_empty(), "{borrows:?}");
                let work = SCOPE_WORK.with(|count| count.get());
                assert_eq!(work, n + 1);
                println!("registrations={n} repeated={repeated} scope_work={work}");
            }
        }
    }

    #[test]
    fn scoped_borrow_discharge_scales_with_scopes_and_registrations() {
        for n in [1, 8, 64, 128] {
            for chain in [false, true] {
                for separate in [false, true] {
                    let mut body = String::from("let s0 = TaskScope::new(); s0.add(borrow(&x));");
                    for i in 1..n {
                        let parent = if chain { i - 1 } else { 0 };
                        body.push_str(&format!(
                            "let s{i} = s{parent}.child(); s{i}.add(borrow(&x));"
                        ));
                    }
                    if separate {
                        for i in (0..n).rev() {
                            body.push_str(&format!("await s{i}.finish();"));
                        }
                    } else {
                        body.push_str("await s0.finish();");
                    }
                    SCOPE_WORK.with(|count| count.set(0));
                    let (borrows, errors) = analyze(&body);
                    assert!(errors.is_empty(), "{errors:?}");
                    assert!(borrows.is_empty(), "{borrows:?}");
                    let work = SCOPE_WORK.with(|count| count.get());
                    assert_eq!(work, if separate { 3 * n - 1 } else { 2 * n });
                    println!(
                        "scopes={n} chain={chain} separate_finishes={separate} scope_work={work}"
                    );
                }
            }
        }
    }

    #[test]
    fn scoped_borrow_discharge_accepts_proven_paths() {
        for body in [
            "s.add(borrow(&x)); await s.finish();",
            "let t = s.add(borrow(&x)); await s.finish(); let y = await t;",
            "let t = borrow(&x); s.add(t); await s.finish();",
            "let t = borrow(&x); let h = s.add(t); await s.finish(); let y = await h;",
            "let t = s.add(borrow(&x)); await t;",
            "s.add(borrow(&x)); let done = await s.finish();",
            "s.add(borrow(&x)); match await s.finish() { _ => {} }",
            "let c = s.child(); c.add(borrow(&x)); await s.finish();",
            "let c = s.child(); c.add(borrow(&x)); await c.finish(); await s.finish();",
            "let c = s.child(); let d = c.child(); d.add(borrow(&x)); await s.finish();",
            "let c = s.child(); s.add(borrow(&x)); c.add(borrow(&x)); await s.finish();",
            "if true { let c = s.child(); let y = 2; c.add(borrow(&y)); await c.finish(); } await s.finish();",
            "if true { let s = TaskScope::new(); s.add(borrow(&x)); await s.finish(); }",
            "while false { let c = TaskScope::new(); c.add(borrow(&x)); await c.finish(); }",
            "let t = s.add(borrow(&x)); await t.result();",
        ] {
            let (borrows, errors) = analyze(&format!("let s = TaskScope::new(); {body}"));
            assert!(errors.is_empty(), "{body}: {errors:?}");
            assert!(borrows.is_empty(), "{body}: {borrows:?}");
        }
    }

    #[test]
    fn scoped_borrow_discharge_rejects_unproven_paths() {
        for body in [
            "s.add(borrow(&x));",
            "s.add(borrow(&x)); s.finish();",
            "s.add(borrow(&x)); if true { await s.finish(); }",
            "s.add(borrow(&x)); while false { await s.finish(); }",
            "s.add(borrow(&x)); return; await s.finish();",
            "s.add(borrow(&x)); if true { return; } await s.finish();",
            "let c = s.child(); c.add(borrow(&x)); await c.finish(); s.add(borrow(&x));",
            "if true { let y = 2; s.add(borrow(&y)); } await s.finish();",
            "let c = s.child(); if true { let y = 2; c.add(borrow(&y)); } await s.finish();",
            "s.add(borrow(&x)); let c = s.child(); await c.finish();",
            "let alias = s; s.add(borrow(&x)); await s.finish();",
            "s.add(borrow(&x)); let alias = s; await s.finish();",
            "await s.finish(); s.add(borrow(&x)); await s.finish();",
            "let c = s.child(); await s.finish(); c.add(borrow(&x)); await c.finish();",
            "s.add(borrow(&x)); let f = || { await s.finish(); };",
            "s.add(borrow(&x)); defer { await s.finish(); }",
            "let t = s.add(borrow(&x)); consume(t); await s.finish();",
            "return s.add(borrow(&x));",
            "let t = s.add(borrow(&x)); return t;",
            "let t = borrow(&x); if true { s.add(t); } await s.finish();",
            "while false { s.add(borrow(&x)); await s.finish(); }",
            "s.add(borrow(&x)); let s = TaskScope::new(); await s.finish();",
        ] {
            let (borrows, _) = analyze(&format!("let s = TaskScope::new(); {body}"));
            assert!(
                borrows.iter().any(|d| d.code == ErrorCode::E1708),
                "{body}: {borrows:?}"
            );
        }
    }

    #[test]
    fn scoped_mutable_loans_last_until_finish() {
        for (middle, conflict) in [
            ("write(&x); await s.finish();", true),
            ("await s.finish(); write(&x);", false),
            (
                "let c = s.child(); await c.finish(); write(&x); await s.finish();",
                true,
            ),
        ] {
            let source = format!(
                "async fn borrow(x: &mut i64) {{ await sleep(1); }} fn write(x: &mut i64) {{}} async fn main() {{ let mut x = 1; let s = TaskScope::new(); s.add(borrow(&x)); {middle} }}"
            );
            let (borrows, errors) = analyze_source(&source);
            assert!(errors.is_empty(), "{errors:?}");
            assert_eq!(
                borrows.iter().any(|d| d.code == ErrorCode::E1706),
                conflict,
                "{middle}: {borrows:?}"
            );
            assert!(
                borrows.iter().all(|d| d.code == ErrorCode::E1706),
                "{borrows:?}"
            );
        }
    }

    #[test]
    fn synchronous_reference_calls_check_outstanding_async_loans() {
        for ty in ["i64", "f64", "bool"] {
            let initial = match ty {
                "f64" => "1.0",
                "bool" => "true",
                _ => "1",
            };
            for (async_mode, sync_mode, conflict) in [
                ("&mut", "&mut", true),
                ("&mut", "&", true),
                ("&", "&mut", true),
                ("&", "&", false),
            ] {
                for (body, outstanding_same_place) in [
                    ("let t = borrow(&x); use_ref(&x); await t;", true),
                    ("let t = borrow(&x); await t; use_ref(&x);", false),
                    ("let t = borrow(&x); await t.result(); use_ref(&x);", false),
                    ("let t = borrow(&x); use_ref(&y); await t;", false),
                    (
                        "let t = borrow(&x); if true { use_ref(&x); } await t;",
                        true,
                    ),
                ] {
                    let source = format!(
                        "async fn borrow(x: {async_mode} {ty}) {{ await sleep(1); }} fn use_ref(x: {sync_mode} {ty}) {{}} async fn main() {{ let mut x = {initial}; let mut y = {initial}; {body} }}"
                    );
                    let (borrows, errors) = analyze_source(&source);
                    assert!(errors.is_empty(), "{source}: {errors:?}");
                    let expected = conflict && outstanding_same_place;
                    assert_eq!(
                        borrows.iter().any(|d| d.code == ErrorCode::E1706),
                        expected,
                        "{source}: {borrows:?}"
                    );
                    assert!(
                        borrows.iter().all(|d| d.code == ErrorCode::E1706),
                        "{borrows:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn synchronous_reference_call_forms_preserve_retention() {
        for (declaration, call) in [
            ("fn use_ref<T>(x: &mut T) {}", "use_ref(&x);"),
            (
                "class C { pub fn use_ref(self, x: &mut i64) {} }",
                "let c = new C(); c.use_ref(&x);",
            ),
            (
                "class C { pub static fn use_ref(x: &mut i64) {} }",
                "C::use_ref(&x);",
            ),
            (
                "class C { pub init(self, x: &mut i64) {} }",
                "let c = new C(&x);",
            ),
            (
                "fn use_ref(x: &mut i64) -> Task<i64> { return other(); }",
                "let independent = use_ref(&x);",
            ),
        ] {
            for pending in [true, false] {
                let body = if pending {
                    format!("let t = borrow(&x); {call} await t;")
                } else {
                    format!("let t = borrow(&x); await t; {call}")
                };
                let source = format!(
                    "async fn other() -> i64 {{ return 1; }} async fn borrow(x: &mut i64) {{ await sleep(1); }} {declaration} async fn main() {{ let mut x = 1; {body} }}"
                );
                let (borrows, errors) = analyze_source(&source);
                assert!(errors.is_empty(), "{source}: {errors:?}");
                assert_eq!(
                    borrows.iter().any(|d| d.code == ErrorCode::E1706),
                    pending,
                    "{source}: {borrows:?}"
                );
                assert!(
                    borrows.iter().all(|d| d.code == ErrorCode::E1706),
                    "{source}: {borrows:?}"
                );
            }
        }
    }

    #[test]
    fn synchronous_overlap_lookups_scale_with_arguments() {
        for count in [1, 8, 64, 128] {
            let mut source = String::from(
                "async fn read(x: &i64) {} fn sync_read(x: &i64) {} fn sync_write(x: &mut i64) {} async fn main() { let mut x = 1;",
            );
            for i in 0..count {
                source.push_str(&format!("let t{i} = read(&x);"));
            }
            for parity in 0..2 {
                for i in (parity..count).step_by(2) {
                    source.push_str(&format!("sync_read(&x); await t{i};"));
                }
            }
            source.push_str("sync_write(&x); }");
            PLACE_LOOKUPS.with(|n| n.set(0));
            let (borrows, errors) = analyze_source(&source);
            assert!(errors.is_empty(), "{errors:?}");
            assert!(borrows.is_empty(), "{borrows:?}");
            let lookups = PLACE_LOOKUPS.with(|n| n.get());
            assert_eq!(lookups, 2 * count + 1);
            println!(
                "async_tasks={count} synchronous_calls={} overlap_lookups={lookups}",
                count + 1
            );
        }
    }

    #[test]
    fn mutable_async_loans_conflict_until_discharge() {
        for body in [
            "let a = write(&x); let b = write(&x); await a; await b;",
            "let a = read(&x); let b = write(&x); await a; await b;",
            "let a = write(&x); let b = read(&x); await a; await b;",
            "let a = write(&x); await write(&x); await a;",
            "let a = write(&x); if true { await write(&x); } await a;",
            "let a = read(&x); let b = read(&x); await a; await write(&x); await b;",
            "let a = read(&x); let b = read(&x); await b; await write(&x); await a;",
        ] {
            let source = format!(
                "async fn write(x: &mut i64) {{ await sleep(1); x = 2; }} async fn read(x: &i64) {{ await sleep(1); }} async fn main() {{ let mut x = 1; {body} }}"
            );
            let (borrows, errors) = analyze_source(&source);
            assert!(errors.is_empty(), "{errors:?}");
            assert!(
                borrows.iter().any(|d| d.code == ErrorCode::E1706),
                "{body}: {borrows:?}"
            );
        }
        for body in [
            "let a = write(&x); await a; await write(&x);",
            "let a = write(&x); await a.result(); await write(&x);",
            "let a = read(&x); let b = read(&x); await a; await b; await write(&x);",
            "let a = write(&x); if true { let mut x = 3; await write(&x); } await a;",
            "let a = write(&x); let mut y = 3; await write(&y); await a;",
            // Borrow identity is captured before the task binding shadows x.
            "if true { let x = write(&x); await x; }",
        ] {
            let source = format!(
                "async fn write(x: &mut i64) {{ await sleep(1); x = 2; }} async fn read(x: &i64) {{ await sleep(1); }} async fn main() {{ let mut x = 1; {body} }}"
            );
            let (borrows, errors) = analyze_source(&source);
            assert!(errors.is_empty(), "{errors:?}");
            assert!(borrows.is_empty(), "{body}: {borrows:?}");
        }
    }

    #[test]
    fn async_loan_index_scales_with_arguments_not_live_tasks() {
        for count in [1, 8, 64, 128] {
            for shared in [false, true] {
                let mut source = String::from(
                    "async fn write(x: &mut i64) {} async fn read(x: &i64) {} async fn main() { let mut x = 1;",
                );
                for i in 0..count {
                    if shared {
                        source.push_str(&format!("let t{i} = read(&x);"));
                    } else {
                        source.push_str(&format!("let mut x{i} = 1; let t{i} = write(&x{i});"));
                    }
                }
                // Fragment the active set, then discharge the remaining half.
                for parity in 0..2 {
                    for i in (parity..count).step_by(2) {
                        source.push_str(&format!("await t{i};"));
                    }
                }
                source.push_str("await write(&x); }");
                PLACE_LOOKUPS.with(|n| n.set(0));
                let (borrows, errors) = analyze_source(&source);
                assert!(errors.is_empty(), "{errors:?}");
                assert!(borrows.is_empty(), "{borrows:?}");
                let lookups = PLACE_LOOKUPS.with(|n| n.get());
                assert_eq!(lookups, count + 1);
                println!("tasks={count} shared={shared} overlap_lookups={lookups}");
            }
        }
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
    fn scoped_borrow_repeated_cancel_does_not_scan_loans() {
        for n in [1, 8, 64, 128] {
            let mut body = String::from("let s = TaskScope::new();");
            for _ in 0..n {
                body.push_str("s.add(borrow(&x)); s.cancel();");
            }
            body.push_str("await s.finish();");
            SCOPE_WORK.with(|count| count.set(0));
            let (borrows, errors) = analyze(&body);
            assert!(errors.is_empty(), "{errors:?}");
            assert!(borrows.is_empty(), "{borrows:?}");
            let work = SCOPE_WORK.with(|count| count.get());
            assert_eq!(work, n + 1);
            println!("cancel_requests={n} registrations={n} scope_work={work}");
        }
    }

    #[test]
    fn scoped_borrow_cancel_requires_finish() {
        for (body, rejected) in [
            (
                "let s = TaskScope::new(); s.add(borrow(&x)); s.cancel(); await s.finish();",
                false,
            ),
            (
                "let s = TaskScope::new(); let c = s.child(); c.add(borrow(&x)); s.cancel(); await s.finish();",
                false,
            ),
            (
                "let s = TaskScope::new(); s.add(borrow(&x)); s.cancel(); s.cancel(); await s.finish();",
                false,
            ),
            (
                "let s = TaskScope::new(); s.add(borrow(&x)); s.cancel();",
                true,
            ),
            (
                "let s = TaskScope::new(); s.add(borrow(&x)); s.cancel(); return;",
                true,
            ),
            (
                "let s = TaskScope::new(); s.add(borrow(&x)); s.cancel(); if true { await s.finish(); }",
                true,
            ),
        ] {
            let (borrows, errors) = analyze(body);
            assert!(errors.is_empty(), "{body}: {errors:?}");
            assert_eq!(!borrows.is_empty(), rejected, "{body}: {borrows:?}");
            assert!(borrows.iter().all(|d| d.code == ErrorCode::E1708));
        }
        let (borrows, errors) = analyze_source(
            "async fn write(x: &mut i64) { await sleep(1); } async fn main() {
                let mut x = 0; let s = TaskScope::new(); s.add(write(&x));
                s.cancel(); await write(&x); await s.finish(); }",
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert!(
            borrows.iter().any(|d| d.code == ErrorCode::E1706),
            "{borrows:?}"
        );
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
