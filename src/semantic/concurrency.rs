use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use crate::parser::visit::{AstVisitor, walk_expr, walk_stmt};
use crate::semantic::call_graph::{CallGraph, CallSites};
use crate::semantic::effects::{EffectProblem, RuntimeEffects, cycle_members};
use crate::semantic::ids::{FunctionId, TypeId};
use std::collections::{HashMap, HashSet};

/// "Cannot yield to the scheduler": the single effect bit E0810 reads out of
/// the shared lattice.
const NO_PREEMPT: RuntimeEffects = RuntimeEffects::NO_PREEMPT_REGION;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConcurrencyReport {
    pub async_functions: usize,
    pub await_expressions: usize,
    pub await_outside_async: usize,
    pub select_expressions: usize,
    pub channel_operations: usize,
    /// Cancellation-aware await adapters (`task.result()`). Task waiting is
    /// counted by `await_expressions` (willow-qrj9).
    pub task_result_queries: usize,
}

/// Why a synchronous helper cannot yield to the scheduler when it is called
/// from a task context.
///
/// The two reasons are reported differently because claiming a source loop
/// exists where there is only recursion sends the programmer looking for a
/// `while` that is not there (§2.3 of the async completion spec).
///
/// The declaration order is load-bearing: this is the witness type carried
/// through [`crate::semantic::effects`], whose witnesses join by `min`, so
/// `Loop` sorting first is what makes a helper that both loops and reaches
/// recursion report as looping. The loop is visible in the helper's own body,
/// so it is the more concrete thing to point at, and it keeps the long-standing
/// wording for every program that was already rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NonpreemptibleReason {
    /// Contains, or transitively reaches, a synchronous loop.
    Loop,
    /// Belongs to, or transitively reaches, a recursive call cycle: direct self
    /// recursion, mutual recursion, or a longer cycle.
    Recursion,
}

impl NonpreemptibleReason {
    /// The E0810 headline for a call to `callee`.
    pub(crate) fn message(self, callee: &FunctionId) -> String {
        match self {
            Self::Loop => {
                format!("sync helper `{callee}` with a loop is not preemptible in task context")
            }
            Self::Recursion => {
                format!("sync helper `{callee}` can run unbounded recursive work in task context")
            }
        }
    }

    /// Secondary label on the helper's own definition.
    pub(crate) fn helper_label(self) -> &'static str {
        match self {
            Self::Loop => "this helper contains or reaches a synchronous loop",
            Self::Recursion => "this helper is part of, or reaches, a recursive call cycle",
        }
    }

    /// Stand-in for the secondary label when the helper lives in another file
    /// that the current diagnostic source map cannot render.
    pub(crate) fn module_note(self, callee: &FunctionId, module: &str) -> String {
        let what = match self {
            Self::Loop => "contains or reaches a synchronous loop",
            Self::Recursion => "is part of, or reaches, a recursive call cycle",
        };
        format!("`{callee}` is defined in imported module `{module}` and {what}")
    }
}

/// Help text shared by every E0810. It names both remedies: the one available
/// today, and the pending toolchain work that retires the rejection.
pub(crate) const NONPREEMPTIBLE_HELP: &str =
    "make the helper async, or wait for task-aware sync-stack preemption support";

/// Note attached to every E0810, so the rejection does not read as permanent
/// language policy (§2.2.1 of the async completion spec).
pub(crate) const NONPREEMPTIBLE_NOTE: &str =
    "this rejection is temporary: it is lifted once task-aware synchronous-stack preemption ships";

/// A synchronous helper that cannot be preempted when called from a task
/// context, with the reason and the span of its definition.
#[derive(Debug, Clone, Copy)]
pub struct NonpreemptibleHelper {
    pub span: Span,
    pub reason: NonpreemptibleReason,
}

/// A [`NonpreemptibleHelper`] as seen by the analyzer. `module` is `None` when
/// the helper is defined in the program being analyzed, or `Some(name)` when it
/// was seeded from an imported module (its `span` then points into that
/// module's file, which the current diagnostic source map cannot render, so the
/// cross-module diagnostic uses a note instead of a secondary source label).
#[derive(Debug, Clone)]
struct SyncHelperRef {
    span: Span,
    reason: NonpreemptibleReason,
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

    /// Seed the nonpreemptible-helper index with the non-preemptible sync
    /// helpers of an imported module, keyed by their module-qualified names
    /// (`module::fn`, `module::Class::method`). This lets the entry program's
    /// task-aware check flag a direct cross-module call such as `await`-free
    /// `worker::heavy()` from an async fn (willow-0a6k.2). Call before
    /// `check_program`.
    pub fn with_module_helpers(mut self, module_name: &str, program: &Program) -> Self {
        for (name, helper) in compute_nonpreemptible_helpers(program) {
            self.nonpreemptible_sync_helpers.insert(
                name.in_namespace(module_name),
                SyncHelperRef {
                    span: helper.span,
                    reason: helper.reason,
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
    /// the module's non-preemptible helpers from the item's name to the local
    /// name so a free-fn import (`heavy` → `heavy()`) and a class import
    /// (`Work` → `Work::method()`) both resolve (willow-0a6k.2).
    pub fn with_item_helper(
        mut self,
        local: &str,
        item: &str,
        module_display: &str,
        program: &Program,
    ) -> Self {
        for (name, helper) in compute_nonpreemptible_helpers(program) {
            let rekeyed = name.remap_imported_item(item, local);
            if let Some(key) = rekeyed {
                self.nonpreemptible_sync_helpers.insert(
                    key,
                    SyncHelperRef {
                        span: helper.span,
                        reason: helper.reason,
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
        for (name, helper) in compute_nonpreemptible_helpers(program) {
            self.nonpreemptible_sync_helpers
                .entry(name)
                .or_insert(SyncHelperRef {
                    span: helper.span,
                    reason: helper.reason,
                    module: None,
                });
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
            Stmt::Defer(d) => match &d.body {
                DeferBody::Expr(expr) => self.check_expr(expr),
                DeferBody::Block(block) => self.check_block(block),
            },
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
            Stmt::Lock(lock_stmt) => {
                self.check_expr(&lock_stmt.target);
                self.check_block(&lock_stmt.body);
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
                    "result" => self.report.task_result_queries += 1,
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
                        SelectCaseKind::Timeout { millis } => self.check_expr(millis),
                        // A task case IS an `await`. The awaited expression is
                        // visited normally, so an inline `.result()` is counted
                        // as a task-result query by the method-call visitor.
                        SelectCaseKind::Join { task, .. } => {
                            self.report.await_expressions += 1;
                            if !self.current_async_context {
                                self.report.await_outside_async += 1;
                            }
                            self.check_expr(task);
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
            Expr::Lambda(lambda) => {
                // A lambda body is a separate callable with no `async` form of
                // its own, so the enclosing function's async context does not
                // reach into it: an `await` written there IS outside an async
                // fn, and a nonpreemptible sync helper called there is not
                // called from the enclosing task (willow-3kty).
                let previous_async_context =
                    std::mem::replace(&mut self.current_async_context, false);
                match &lambda.body {
                    LambdaBody::Expr(expr) => self.check_expr(expr),
                    LambdaBody::Block(block) => self.check_block(block),
                }
                self.current_async_context = previous_async_context;
            }
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
        let reason = helper.reason;
        let diagnostic =
            Diagnostic::new(Severity::Error, ErrorCode::E0810, reason.message(callee)).with_label(
                Label::primary(call_span, "this call can monopolize the scheduler worker"),
            );
        // A helper defined in an imported module has its span in another file,
        // which this diagnostic's source map cannot render; describe it with a
        // note instead of a cross-file secondary label.
        let diagnostic = match &helper.module {
            Some(module) => diagnostic.with_note(reason.module_note(callee, module)),
            None => diagnostic.with_label(Label::secondary(helper.span, reason.helper_label())),
        };
        self.errors.push(
            diagnostic
                .with_note(NONPREEMPTIBLE_NOTE)
                .with_help(NONPREEMPTIBLE_HELP),
        );
    }
}

/// One synchronous helper in the call graph built by
/// [`compute_nonpreemptible_helpers`].
struct HelperNode {
    id: FunctionId,
    span: Span,
    contains_loop: bool,
    calls: HashSet<FunctionId>,
}

/// Compute the synchronous helpers in `program` that cannot yield to the
/// scheduler when called from a task context, keyed by typed function identity.
///
/// A helper is non-preemptible when it (1) contains a loop, (2) transitively
/// calls a helper with a loop, (3) belongs to a recursive call cycle, or
/// (4) transitively reaches one. Clauses 3 and 4 matter because unbounded work
/// needs no `while`: `fib(40)` monopolizes a worker with nothing but recursion.
///
/// Shared by the same-program index and the imported-module seeding so the
/// reachability fixpoint behaves identically in both, and by the type checker
/// to flag non-preemptible methods called through a typed non-`self` receiver
/// (willow-0a6k.2).
pub(crate) fn compute_nonpreemptible_helpers(
    program: &Program,
) -> HashMap<FunctionId, NonpreemptibleHelper> {
    let mut helpers: Vec<HelperNode> = Vec::new();
    for item in &program.items {
        match item {
            Item::Function(function) if !function.is_async => helpers.push(HelperNode {
                id: FunctionId::free(function.name.as_str()),
                span: function.span,
                contains_loop: block_contains_loop(&function.body),
                calls: called_helpers(&function.body, &function.params),
            }),
            Item::Class(class) => {
                for method in &class.methods {
                    if !method.is_async {
                        let calls = called_helpers(&method.body, &method.params)
                            .into_iter()
                            .map(|callee| {
                                qualify_self_call(&TypeId::local(class.name.as_str()), callee)
                            })
                            .collect();
                        helpers.push(HelperNode {
                            id: FunctionId::method(
                                TypeId::local(class.name.as_str()),
                                method.name.as_str(),
                            ),
                            span: method.span,
                            contains_loop: block_contains_loop(&method.body),
                            calls,
                        });
                    }
                }
            }
            Item::Function(_) | Item::Enum(_) | Item::Interface(_) => {}
        }
    }

    // Only edges whose callee is itself an analyzed sync helper are kept: async
    // callees run on their own safepoints, and an unknown callee (builtin,
    // unresolved interface target) carries no summary here. That filter is why
    // this graph is built from the typed helper set rather than reused from the
    // backend's `CallGraph::build`, which is deliberately conservative about
    // both.
    let known: HashSet<&FunctionId> = helpers.iter().map(|helper| &helper.id).collect();
    let mut graph = CallGraph::default();
    for helper in &helpers {
        graph.merge(
            helper.id.clone(),
            CallSites {
                targets: helper
                    .calls
                    .iter()
                    .filter(|callee| known.contains(callee))
                    .cloned()
                    .collect(),
                has_unknown: false,
            },
        );
    }

    // Seed: a loop in the body, or membership in a recursive cycle. Unbounded
    // work needs no `while`, so cycle membership is its own seed.
    let cycles = cycle_members(&graph, std::iter::empty());
    let mut problem: EffectProblem<NonpreemptibleReason> = EffectProblem::new()
        // Nothing outside the helper set carries a summary, and nothing here is
        // a safety property: an unseen callee is not evidence of a loop.
        .external_callee(RuntimeEffects::NONE)
        .unknown_callee(RuntimeEffects::NONE)
        .missing_body(RuntimeEffects::NONE);
    for helper in &helpers {
        problem = problem.body(helper.id.clone());
        if helper.contains_loop {
            problem = problem.seed(
                helper.id.clone(),
                NO_PREEMPT,
                Some(NonpreemptibleReason::Loop),
            );
        } else if cycles.contains(&helper.id) {
            problem = problem.seed(
                helper.id.clone(),
                NO_PREEMPT,
                Some(NonpreemptibleReason::Recursion),
            );
        }
    }

    // The witness join is `min` and `Loop` sorts before `Recursion`, which is
    // exactly `NonpreemptibleReason::join`: a helper that both loops and
    // reaches recursion is still reported as looping.
    let facts = problem.solve(&graph);
    helpers
        .into_iter()
        .filter_map(|helper| {
            let summary = facts.get(&helper.id)?;
            let reason = *summary.witness(NO_PREEMPT)?;
            Some((
                helper.id,
                NonpreemptibleHelper {
                    span: helper.span,
                    reason,
                },
            ))
        })
        .collect()
}

/// Does this body contain a source-level loop that the scheduler cannot preempt?
///
/// Two subtrees are deliberately excluded, and both predate the shared walker:
///
/// * a `defer` body runs at scope exit rather than inline, so a loop inside one
///   is attributed to the deferred callable, not to this one;
/// * a lambda body is a separate callable with its own summary.
fn block_contains_loop(block: &Block) -> bool {
    let mut finder = LoopFinder::default();
    finder.visit_block(block);
    finder.found
}

#[derive(Default)]
struct LoopFinder {
    found: bool,
}

impl AstVisitor for LoopFinder {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        if self.found {
            return;
        }
        match stmt {
            Stmt::While(_) | Stmt::For(_) => self.found = true,
            Stmt::Defer(_) => {}
            other => walk_stmt(self, other),
        }
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if self.found {
            return;
        }
        walk_expr(self, expr);
    }

    fn visit_lambda(&mut self, _lambda: &LambdaExpr) {}
}

fn called_helpers(block: &Block, params: &[Param]) -> HashSet<FunctionId> {
    let mut collector = CallCollector {
        calls: HashSet::new(),
        scopes: vec![params.iter().map(|param| param.name.clone()).collect()],
    };
    collector.visit_block(block);
    collector.calls
}

fn qualify_self_call(class_name: &TypeId, callee: FunctionId) -> FunctionId {
    callee.resolve_self_owner(class_name)
}

/// Direct call edges out of one body, with local bindings excluded.
///
/// A call through a local name is indirect: its target is whatever value the
/// binding currently holds, which is not a static edge to a same-named
/// top-level helper (willow-bv9.1). The shared walker supplies the shadowing
/// rule — a `let` binds only after its own initializer, and `for`, `lock`,
/// lambda parameters, `match` arms and `select` cases each scope their binding
/// to their body.
struct CallCollector {
    calls: HashSet<FunctionId>,
    scopes: Vec<HashSet<String>>,
}

impl CallCollector {
    fn is_local(&self, name: &str) -> bool {
        self.scopes.iter().rev().any(|scope| scope.contains(name))
    }
}

impl AstVisitor for CallCollector {
    fn enter_scope(&mut self) {
        self.scopes.push(HashSet::new());
    }

    fn exit_scope(&mut self) {
        self.scopes.pop();
    }

    fn bind(&mut self, name: &str) {
        self.scopes
            .last_mut()
            .expect("call collector scope")
            .insert(name.to_string());
    }

    /// A lambda body is its own callable and is summarized separately.
    fn visit_lambda(&mut self, _lambda: &LambdaExpr) {}

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            // A shadowed name is an indirect call through a function value,
            // which has no static target and so contributes no edge.
            Expr::Call(call) if !self.is_local(&call.callee) => {
                self.calls
                    .insert(FunctionId::free_from_source_name(&call.callee));
            }
            Expr::StaticCall(call) => {
                self.calls.insert(FunctionId::method(
                    TypeId::from_source_name(&call.class),
                    call.method.as_str(),
                ));
            }
            Expr::MethodCall(call) => {
                if matches!(&call.object, Expr::Var(name, _) if name == "self") {
                    self.calls.insert(FunctionId::method(
                        TypeId::local("self"),
                        call.method.as_str(),
                    ));
                }
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::label::LabelKind;
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
    let task_result = await h.result();
    ch.close();
    select {};
}

fn main() {
    println(1);
}
"#,
        );
        assert_eq!(analyzer.report.async_functions, 1);
        assert_eq!(analyzer.report.await_expressions, 2);
        assert_eq!(analyzer.report.task_result_queries, 1);
        assert_eq!(analyzer.report.channel_operations, 1);
        assert_eq!(analyzer.report.select_expressions, 1);
    }

    /// A select task case is an `await` and must be reported as one; an inline
    /// `.result()` in a case is also reported as a task-result query
    /// (willow-qrj9).
    #[test]
    fn report_counts_select_task_cases_as_awaits() {
        let analyzer = analyze(
            r#"
async fn work() -> i64 { return 1; }

async fn run() {
    let plain = work();
    let cancel_aware = work();
    select {
        let a = await plain => { println(a); }
        let b = await cancel_aware.result() => { println(1); }
        default => { println(0); }
    }
}
"#,
        );
        // Two task cases; `default` and the task expressions are not awaits.
        assert_eq!(analyzer.report.await_expressions, 2);
        // Only the `.result()` case is cancellation-aware.
        assert_eq!(analyzer.report.task_result_queries, 1);
        assert_eq!(analyzer.report.await_outside_async, 0);
    }

    /// The `await` inside a sync select's task case is still an await, so it is
    /// counted AND flagged as occurring outside an async fn (willow-qrj9).
    #[test]
    fn report_flags_select_task_case_await_outside_async() {
        let analyzer = analyze(
            r#"
async fn work() -> i64 { return 1; }

fn run(t: Task<i64>) {
    select {
        let a = await t => { println(a); }
        default => { println(0); }
    }
}
"#,
        );
        assert_eq!(analyzer.report.await_expressions, 1);
        assert_eq!(analyzer.report.await_outside_async, 1);
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
    fn local_function_value_shadow_does_not_inherit_free_helper_loop_effect() {
        let analyzer = analyze(
            r#"
fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n { i = i + 1; }
    return i;
}

fn wrapper() -> i64 {
    let heavy = |n: i64| -> i64 { return n + 100; };
    return heavy(1);
}

async fn run() { println(wrapper()); }
fn main() {}
"#,
        );
        assert!(
            analyzer.errors.is_empty(),
            "the indirect local call must not create an edge to the shadowed loop helper: {:#?}",
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

    // ------------------------------------------------------------------
    // Stage A-prime: recursion is unbounded work too (willow-38w.2.1).
    //
    // Before Stage A-prime the analysis seeded only from `contains a loop`,
    // so a loop-free recursive helper — `fib(n-1) + fib(n-2)` — was admitted
    // into task context and could hold a scheduler worker for billions of
    // calls. These 25 perspectives pin the SCC-based seeding, the reason
    // selection that picks between the loop and recursion wordings, and the
    // cases that must keep compiling.
    //
    //  1. direct self recursion is rejected from a task
    //  2. the recursion wording is used, and the loop wording is not
    //  3. mutual recursion: caller side of the 2-cycle
    //  4. mutual recursion: callee side of the same 2-cycle
    //  5. a 3-node cycle is a cycle
    //  6. a helper that only *reaches* an SCC is rejected
    //  7. reaching an SCC through two hops is rejected
    //  8. loop + recursion in one helper reports the loop
    //  9. reaching both a looping and a recursive helper reports the loop
    // 10. recursion called from sync context is accepted
    // 11. a straight-line helper is still accepted (no false positive)
    // 12. a self-recursive *async* fn is not a sync cycle
    // 13. a sync helper calling an async fn is not a cycle through the task
    // 14. `self.`-call instance-method recursion is rejected
    // 15. `Self::`-call static-method recursion is rejected
    // 16. mutual recursion across two classes' static methods
    // 17. recursion in a select default case is rejected
    // 18. recursion in a select recv case is rejected
    // 19. cross-module recursion is rejected with the module note
    // 20. cross-module reach-into-SCC is rejected
    // 21. item-imported recursion is rejected
    // 22. an aliased item-imported recursive helper is rejected
    // 23. the diagnostic carries the temporary note, both remedies, and the
    //     recursion secondary label
    // 24. two independent cycles are both flagged, deterministically
    // 25. the primary label points at the sole task-context entry into the
    //     cycle, and only that call is reported

    /// A loop-free recursive helper. The shape Stage A-prime exists to catch.
    const RECURSIVE_FIB: &str = r#"
fn fib(n: i64) -> i64 {
    if n < 2 {
        return n;
    }
    return fib(n - 1) + fib(n - 2);
}
"#;

    fn assert_recursion_rejected(source: &str, callee: &str) {
        assert_error_contains(
            source,
            ErrorCode::E0810,
            &format!("sync helper `{callee}` can run unbounded recursive work in task context"),
        );
    }

    fn assert_accepted(source: &str, why: &str) {
        let analyzer = analyze(source);
        assert!(analyzer.errors.is_empty(), "{why}: {:#?}", analyzer.errors);
    }

    /// Perspective 1: Direct self recursion, with no loop anywhere, is unbounded work.
    #[test]
    fn rejects_self_recursive_sync_helper_called_from_async_function() {
        assert_recursion_rejected(
            &format!(
                "{RECURSIVE_FIB}
async fn run() -> i64 {{
    return fib(30);
}}
"
            ),
            "fib",
        );
    }

    /// Perspective 2: The recursion case must not borrow the loop wording — there is no
    /// loop to point the reader at, and "with a loop" would be a lie.
    #[test]
    fn recursion_diagnostic_does_not_claim_a_loop() {
        let analyzer = analyze(&format!(
            "{RECURSIVE_FIB}
async fn run() -> i64 {{
    return fib(30);
}}
"
        ));
        let recursion = analyzer
            .errors
            .iter()
            .find(|e| e.code == ErrorCode::E0810)
            .expect("expected an E0810");
        assert!(
            !recursion.message.contains("with a loop"),
            "recursion diagnostic must not claim a loop: {}",
            recursion.message
        );
    }

    /// Perspective 3: Mutual recursion is a cycle: the entered side is rejected.
    #[test]
    fn rejects_mutually_recursive_sync_helper_from_async_function() {
        assert_recursion_rejected(
            r#"
fn is_even(n: i64) -> bool {
    if n == 0 {
        return true;
    }
    return is_odd(n - 1);
}

fn is_odd(n: i64) -> bool {
    if n == 0 {
        return false;
    }
    return is_even(n - 1);
}

async fn run() -> bool {
    return is_even(30);
}
"#,
            "is_even",
        );
    }

    /// Perspective 4: Every member of the cycle is non-preemptible, not just the one the
    /// task happens to enter through.
    #[test]
    fn rejects_other_member_of_mutual_recursion_cycle() {
        assert_recursion_rejected(
            r#"
fn is_even(n: i64) -> bool {
    if n == 0 {
        return true;
    }
    return is_odd(n - 1);
}

fn is_odd(n: i64) -> bool {
    if n == 0 {
        return false;
    }
    return is_even(n - 1);
}

async fn run() -> bool {
    return is_odd(30);
}
"#,
            "is_odd",
        );
    }

    /// Perspective 5: Cycles longer than two nodes are cycles. A pairwise "does A call
    /// something that calls A back" check would miss this one.
    #[test]
    fn rejects_three_node_recursive_cycle() {
        assert_recursion_rejected(
            r#"
fn a(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    return b(n - 1);
}

fn b(n: i64) -> i64 {
    return c(n);
}

fn c(n: i64) -> i64 {
    return a(n);
}

async fn run() -> i64 {
    return a(30);
}
"#,
            "a",
        );
    }

    /// Perspective 6: A helper outside the cycle that can call into it inherits the
    /// verdict: entering it can still run unbounded work.
    #[test]
    fn rejects_helper_that_reaches_a_recursive_cycle() {
        assert_recursion_rejected(
            &format!(
                "{RECURSIVE_FIB}
fn wrapper(n: i64) -> i64 {{
    return fib(n) + 1;
}}

async fn run() -> i64 {{
    return wrapper(30);
}}
"
            ),
            "wrapper",
        );
    }

    /// Perspective 7: Reachability is transitive, so distance from the cycle does not
    /// launder the call.
    #[test]
    fn rejects_helper_two_hops_from_a_recursive_cycle() {
        assert_recursion_rejected(
            &format!(
                "{RECURSIVE_FIB}
fn inner(n: i64) -> i64 {{
    return fib(n);
}}

fn outer(n: i64) -> i64 {{
    return inner(n);
}}

async fn run() -> i64 {{
    return outer(30);
}}
"
            ),
            "outer",
        );
    }

    /// Perspective 8: When a helper both loops and recurses, the loop wins the wording:
    /// the loop is visible in the helper's own body, and every program that
    /// was already rejected keeps its long-standing message.
    #[test]
    fn helper_with_loop_and_recursion_reports_the_loop() {
        assert_error_contains(
            r#"
fn both(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    if n <= 0 {
        return 0;
    }
    return both(n - 1);
}

async fn run() -> i64 {
    return both(10);
}
"#,
            ErrorCode::E0810,
            "sync helper `both` with a loop is not preemptible in task context",
        );
    }

    /// Perspective 9: The reason join is monotone and loop-absorbing across edges too: a
    /// helper reaching one looping and one recursive callee reports the loop.
    #[test]
    fn helper_reaching_loop_and_recursion_reports_the_loop() {
        assert_error_contains(
            &format!(
                "{RECURSIVE_FIB}
fn spin(n: i64) -> i64 {{
    let mut i = 0;
    while i < n {{
        i = i + 1;
    }}
    return i;
}}

fn wrapper(n: i64) -> i64 {{
    return fib(n) + spin(n);
}}

async fn run() -> i64 {{
    return wrapper(10);
}}
"
            ),
            ErrorCode::E0810,
            "sync helper `wrapper` with a loop is not preemptible in task context",
        );
    }

    /// Perspective 10: Recursion is not illegal. Only calling it from a task is, and only
    /// until sync-stack preemption ships — a synchronous caller holds no
    /// scheduler worker, so there is nothing to starve.
    #[test]
    fn allows_recursive_helper_called_from_sync_context() {
        assert_accepted(
            &format!(
                "{RECURSIVE_FIB}
fn main() {{
    println(fib(10));
}}
"
            ),
            "sync-context recursion must stay callable",
        );
    }

    /// Perspective 11: The SCC seeding must not flag acyclic graphs. A helper calling
    /// another helper is not a cycle, however many of them there are.
    #[test]
    fn allows_acyclic_sync_helper_chain_from_async_function() {
        assert_accepted(
            r#"
fn leaf(n: i64) -> i64 {
    return n + 1;
}

fn middle(n: i64) -> i64 {
    return leaf(n) + leaf(n);
}

fn top(n: i64) -> i64 {
    return middle(n);
}

async fn run() -> i64 {
    return top(10);
}
"#,
            "an acyclic loop-free chain must remain callable",
        );
    }

    /// Perspective 12: A recursive *async* fn is not a sync helper: each recursive call
    /// spawns a task and the awaits are safepoints, so the worker is released.
    #[test]
    fn allows_self_recursive_async_function() {
        assert_accepted(
            r#"
async fn count(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    return 1 + await count(n - 1);
}

async fn run() -> i64 {
    return await count(10);
}
"#,
            "async recursion is preemptible and must not be flagged",
        );
    }

    /// Perspective 13: A sync helper that calls an async fn only creates a task; the async
    /// callee is not a node in the sync call graph, so no cycle is formed
    /// through it.
    #[test]
    fn sync_helper_calling_async_fn_is_not_a_cycle() {
        assert_accepted(
            r#"
async fn work(n: i64) -> i64 {
    return n + 1;
}

fn spawn_work(n: i64) -> Task<i64> {
    return work(n);
}

async fn run() -> i64 {
    return await spawn_work(10);
}
"#,
            "an async callee must not make its sync caller a cycle",
        );
    }

    /// Perspective 14: Instance-method recursion through `self.` is a cycle. The call is
    /// recorded under the qualified method id, so the self-edge is visible.
    #[test]
    fn rejects_self_recursive_instance_method_from_async_method() {
        assert_recursion_rejected(
            r#"
class Work {
    fn down(self, n: i64) -> i64 {
        if n <= 0 {
            return 0;
        }
        return self.down(n - 1);
    }

    async fn run(self) -> i64 {
        return self.down(30);
    }
}
"#,
            "Work::down",
        );
    }

    /// Perspective 15: The same for a static method recursing through `Self::`.
    #[test]
    fn rejects_self_recursive_static_method_from_async_function() {
        assert_recursion_rejected(
            r#"
class Work {
    static fn down(n: i64) -> i64 {
        if n <= 0 {
            return 0;
        }
        return Self::down(n - 1);
    }
}

async fn run() -> i64 {
    return Work::down(30);
}
"#,
            "Work::down",
        );
    }

    /// Perspective 16: A cycle that crosses class boundaries is still a cycle.
    #[test]
    fn rejects_mutual_recursion_across_two_classes() {
        assert_recursion_rejected(
            r#"
class Even {
    static fn check(n: i64) -> bool {
        if n == 0 {
            return true;
        }
        return Odd::check(n - 1);
    }
}

class Odd {
    static fn check(n: i64) -> bool {
        if n == 0 {
            return false;
        }
        return Even::check(n - 1);
    }
}

async fn run() -> bool {
    return Even::check(30);
}
"#,
            "Even::check",
        );
    }

    /// Perspective 17: A select default case runs on the task's worker, so the rule
    /// applies there too.
    #[test]
    fn rejects_recursive_helper_in_select_default_case() {
        assert_recursion_rejected(
            &format!(
                "{RECURSIVE_FIB}
async fn run(ch: Channel<i64>) {{
    select {{
        default => {{ println(fib(30)); }}
    }}
}}
"
            ),
            "fib",
        );
    }

    /// Perspective 18: And in a receive case's body.
    #[test]
    fn rejects_recursive_helper_in_select_recv_case() {
        assert_recursion_rejected(
            &format!(
                "{RECURSIVE_FIB}
async fn run(ch: Channel<i64>) {{
    select {{
        let v = ch.recv() => {{ println(fib(v)); }}
    }}
}}
"
            ),
            "fib",
        );
    }

    /// Perspective 19: Imported modules are seeded through the same computation, so a
    /// recursive helper in another file is caught with the module note.
    #[test]
    fn rejects_cross_module_recursive_helper_from_async() {
        let analyzer = analyze_with_module(
            r#"
async fn run() -> i64 {
    return worker::fib(30);
}
"#,
            "worker",
            RECURSIVE_MODULE,
        );
        assert_e0810_with_module_note(&analyzer, "worker::fib", "worker");
        assert!(
            analyzer
                .errors
                .iter()
                .any(|e| e.message.contains("can run unbounded recursive work")),
            "cross-module recursion must use the recursion wording: {:#?}",
            analyzer.errors
        );
    }

    /// Perspective 20: The fixpoint runs inside the module too, so a module helper that
    /// merely reaches the module's cycle is rejected as well.
    #[test]
    fn rejects_cross_module_helper_reaching_recursion() {
        let analyzer = analyze_with_module(
            r#"
async fn run() -> i64 {
    return worker::wrapper(30);
}
"#,
            "worker",
            RECURSIVE_MODULE,
        );
        assert_e0810_with_module_note(&analyzer, "worker::wrapper", "worker");
    }

    /// Perspective 21: Single-item imports call the helper by a bare local name; the
    /// recursion verdict must follow the item, not the spelling.
    #[test]
    fn rejects_item_imported_recursive_helper_from_async() {
        let analyzer = analyze_with_item(
            r#"
async fn run() -> i64 {
    return fib(30);
}
"#,
            "fib",
            "fib",
            "worker",
            RECURSIVE_MODULE,
        );
        assert_e0810_with_module_note(&analyzer, "fib", "worker");
    }

    /// Perspective 22: `import worker::fib as f;` — the alias is the local name.
    #[test]
    fn respects_item_import_alias_for_recursive_helpers() {
        let analyzer = analyze_with_item(
            r#"
async fn run() -> i64 {
    return f(30);
}
"#,
            "f",
            "fib",
            "worker",
            RECURSIVE_MODULE,
        );
        assert_e0810_with_module_note(&analyzer, "f", "worker");
    }

    /// Perspective 23: §2.2.1 requires the diagnostic to say the rejection is temporary
    /// and to name both remedies, and the secondary label must describe the
    /// cycle rather than a loop.
    #[test]
    fn recursion_diagnostic_carries_note_help_and_cycle_label() {
        let analyzer = analyze(&format!(
            "{RECURSIVE_FIB}
async fn run() -> i64 {{
    return fib(30);
}}
"
        ));
        let error = analyzer
            .errors
            .iter()
            .find(|e| e.code == ErrorCode::E0810)
            .expect("expected an E0810");
        assert!(
            error.notes.iter().any(|n| n == NONPREEMPTIBLE_NOTE),
            "missing the temporary-rejection note: {error:#?}"
        );
        assert!(
            error.helps.iter().any(|h| h == NONPREEMPTIBLE_HELP),
            "help must name both remedies: {error:#?}"
        );
        assert!(
            error.labels.iter().any(|l| l.kind == LabelKind::Secondary
                && l.message == "this helper is part of, or reaches, a recursive call cycle"),
            "missing the recursion secondary label: {error:#?}"
        );
    }

    /// Perspective 24: Independent cycles are independent: both are flagged, and the
    /// result does not depend on iteration order. `calls` is a `HashSet`, so
    /// the SCC input is sorted; without that the component numbering — and
    /// with it the diagnostics — could vary between runs.
    #[test]
    fn flags_independent_cycles_deterministically() {
        let source = r#"
fn a(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    return a(n - 1);
}

fn b(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    return c(n - 1);
}

fn c(n: i64) -> i64 {
    return b(n);
}

fn plain(n: i64) -> i64 {
    return n + 1;
}

async fn run() -> i64 {
    return a(10) + b(10) + plain(10);
}
"#;
        let first = analyze(source);
        let messages: Vec<&str> = first.errors.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(
            messages,
            vec![
                "sync helper `a` can run unbounded recursive work in task context",
                "sync helper `b` can run unbounded recursive work in task context",
            ],
            "both cycles must be flagged, and `plain` must not be"
        );

        for _ in 0..8 {
            let again = analyze(source);
            let repeat: Vec<&str> = again.errors.iter().map(|e| e.message.as_str()).collect();
            assert_eq!(repeat, messages, "analysis must be deterministic");
        }
    }

    /// Perspective 25: the §2.3 probe verbatim. Exactly one call crosses from
    /// task context into the recursive SCC, so exactly one diagnostic is
    /// reported and its primary label sits on that call — not on `fib`'s
    /// definition, and not on the recursive calls inside `fib` itself, which
    /// are synchronous callers and perfectly legal.
    #[test]
    fn recursion_diagnostic_points_at_the_sole_task_context_entry() {
        let source = format!(
            "{RECURSIVE_FIB}
async fn fib_task() -> i64 {{
    return fib(40);
}}
"
        );
        let analyzer = analyze(&source);
        let reported: Vec<&Diagnostic> = analyzer
            .errors
            .iter()
            .filter(|e| e.code == ErrorCode::E0810)
            .collect();
        assert_eq!(
            reported.len(),
            1,
            "one task-context entry means one diagnostic: {:#?}",
            analyzer.errors
        );

        let primary = reported[0]
            .labels
            .iter()
            .find(|l| l.kind == LabelKind::Primary)
            .expect("expected a primary label");
        assert_eq!(
            primary.message,
            "this call can monopolize the scheduler worker"
        );
        assert_eq!(
            &source[primary.span.start..primary.span.end],
            "fib",
            "the primary label must sit on the `fib(40)` call inside `fib_task`"
        );
        // And that call is inside `fib_task`, past every recursive call in
        // `fib`'s own body.
        let fib_task_start = source.find("async fn fib_task").expect("fib_task");
        assert!(
            primary.span.start > fib_task_start,
            "the label must point into `fib_task`, not into `fib`"
        );
    }

    /// Module counterpart of [`RECURSIVE_FIB`], with a wrapper that only
    /// reaches the cycle.
    const RECURSIVE_MODULE: &str = r#"
fn fib(n: i64) -> i64 {
    if n < 2 {
        return n;
    }
    return fib(n - 1) + fib(n - 2);
}

fn wrapper(n: i64) -> i64 {
    return fib(n) + 1;
}

fn add_one(n: i64) -> i64 {
    return n + 1;
}
"#;

    // --- Shared structural AST walk (willow-uqzx.1.1) ---
    //
    // The loop detector and the helper call graph both read the shared walk in
    // `parser::visit` instead of carrying their own hand-written traversals.
    // Both failure modes are silent: a loop the walk never reaches makes a
    // helper look preemptible, and a call the walk never reaches makes a
    // helper look loop-free. So every container gets its own program.

    /// `heavy`'s only loop sits somewhere inside `body`, and a task context
    /// calls it directly. The walk must find the loop wherever it is.
    fn assert_loop_is_reached(body: &str) {
        let source = format!(
            "fn heavy(n: i64) -> i64 {{\n{body}\n}}\n\nasync fn run() -> i64 {{\n    return heavy(10);\n}}\n"
        );
        assert_error_contains(
            &source,
            ErrorCode::E0810,
            "sync helper `heavy` with a loop is not preemptible in task context",
        );
    }

    /// `body` holds no loop that belongs to `heavy` itself, so a task context
    /// may call it.
    fn assert_no_loop_is_attributed(body: &str) {
        let source = format!(
            "fn heavy(n: i64) -> i64 {{\n{body}\n}}\n\nasync fn run() -> i64 {{\n    return heavy(10);\n}}\n"
        );
        let analyzer = analyze(&source);
        assert!(
            analyzer.errors.is_empty(),
            "no loop belongs to `heavy` here: {:#?}",
            analyzer.errors
        );
    }

    /// `wrapper` reaches the looping `heavy` from one expression position, and
    /// a task context calls `wrapper`. The walk must find the call edge.
    fn assert_call_edge_is_reached(wrapper: &str) {
        let source = format!(
            "fn heavy(n: i64) -> i64 {{\n    let mut i = 0;\n    while i < n {{\n        i = i + 1;\n    }}\n    return i;\n}}\n\n{wrapper}\n\nasync fn run() -> i64 {{\n    return wrapper(10);\n}}\n"
        );
        assert_error_contains(
            &source,
            ErrorCode::E0810,
            "sync helper `wrapper` with a loop is not preemptible in task context",
        );
    }

    #[test]
    fn walk_reaches_a_loop_in_an_if_then_branch() {
        assert_loop_is_reached(
            r#"    if n > 0 {
        let mut i = 0;
        while i < n {
            i = i + 1;
        }
    }
    return n;"#,
        );
    }

    /// Two statement levels down, in an else-branch: reaching it needs the walk
    /// to descend nested blocks, not only the statements of the outer one.
    #[test]
    fn walk_reaches_a_loop_in_a_nested_else_branch() {
        assert_loop_is_reached(
            r#"    if n > 0 {
        if n > 5 {
            return 0;
        } else {
            let mut i = 0;
            while i < n {
                i = i + 1;
            }
        }
    }
    return n;"#,
        );
    }

    #[test]
    fn walk_reaches_a_loop_in_a_match_arm_body() {
        assert_loop_is_reached(
            r#"    match n {
        1 => {
            let mut i = 0;
            while i < n {
                i = i + 1;
            }
        },
        _ => { println(0); }
    }
    return n;"#,
        );
    }

    /// A `for` statement is itself a loop, not merely a container for one.
    #[test]
    fn walk_reaches_a_for_statement_itself() {
        assert_loop_is_reached(
            r#"    let xs: Array<i64> = [1, 2, 3];
    for x in xs {
        println(x);
    }
    return n;"#,
        );
    }

    #[test]
    fn walk_reaches_a_loop_in_a_for_body() {
        assert_loop_is_reached(
            r#"    let xs: Array<i64> = [1, 2, 3];
    for x in xs {
        let mut i = 0;
        while i < x {
            i = i + 1;
        }
    }
    return n;"#,
        );
    }

    /// A lambda body is a separate callable, so a loop inside one is not the
    /// enclosing function's loop. This is the `visit_lambda` override, and it
    /// is the one place where stopping early is the correct answer.
    #[test]
    fn walk_does_not_attribute_a_lambda_body_loop_to_its_enclosing_function() {
        assert_no_loop_is_attributed(
            r#"    let spin: fn(i64) -> i64 = |x: i64| -> i64 {
        let mut i = 0;
        while i < x {
            i = i + 1;
        }
        return i;
    };
    return n;"#,
        );
    }

    /// A `defer` body runs at scope exit and is attributed to the deferred
    /// callable, so its loop is not the enclosing helper's loop either. This
    /// skip predates the shared walk and is preserved deliberately.
    #[test]
    fn walk_does_not_attribute_a_defer_body_loop_to_its_enclosing_function() {
        assert_no_loop_is_attributed(
            r#"    defer {
        let mut i = 0;
        while i < 3 {
            i = i + 1;
        }
    }
    return n;"#,
        );
    }

    #[test]
    fn walk_reaches_a_call_in_a_ternary_branch() {
        assert_call_edge_is_reached(
            r#"fn wrapper(n: i64) -> i64 {
    return n > 0 ? heavy(n) : 0;
}"#,
        );
    }

    #[test]
    fn walk_reaches_a_call_in_a_binary_operand() {
        assert_call_edge_is_reached(
            r#"fn wrapper(n: i64) -> i64 {
    return n + heavy(n);
}"#,
        );
    }

    #[test]
    fn walk_reaches_a_call_in_an_array_element() {
        assert_call_edge_is_reached(
            r#"fn wrapper(n: i64) -> i64 {
    let xs: Array<i64> = [heavy(n)];
    return n;
}"#,
        );
    }

    #[test]
    fn walk_reaches_a_call_in_a_unary_operand() {
        assert_call_edge_is_reached(
            r#"fn wrapper(n: i64) -> i64 {
    return -heavy(n);
}"#,
        );
    }

    #[test]
    fn walk_reaches_a_call_in_a_match_arm_body() {
        assert_call_edge_is_reached(
            r#"fn wrapper(n: i64) -> i64 {
    return match n {
        1 => heavy(n),
        _ => 0
    };
}"#,
        );
    }

    #[test]
    fn walk_reaches_a_call_in_a_constructor_argument() {
        assert_call_edge_is_reached(
            r#"class Cell {
    pub value: i64;

    pub static fn of(value: i64) -> i64 {
        return value;
    }

    pub fn take(self, other: i64) -> i64 {
        return self.value + other;
    }
}

fn wrapper(n: i64) -> i64 {
    let cell = new Cell(heavy(n));
    return n;
}"#,
        );
    }

    #[test]
    fn walk_reaches_a_call_in_a_static_call_argument() {
        assert_call_edge_is_reached(
            r#"class Cell {
    pub value: i64;

    pub static fn of(value: i64) -> i64 {
        return value;
    }
}

fn wrapper(n: i64) -> i64 {
    return Cell::of(heavy(n));
}"#,
        );
    }

    #[test]
    fn walk_reaches_a_call_in_a_method_call_argument() {
        assert_call_edge_is_reached(
            r#"class Cell {
    pub value: i64;

    pub fn take(self, other: i64) -> i64 {
        return self.value + other;
    }
}

fn wrapper(n: i64) -> i64 {
    let cell = new Cell(1);
    return cell.take(heavy(n));
}"#,
        );
    }

    /// A select case's own expressions are searched, not only its body. The
    /// hand-written detector this replaced looked at case bodies only, so this
    /// edge is newly visible — strictly safer, since missing it hides a loop.
    #[test]
    fn walk_reaches_a_call_in_a_select_case_expression() {
        assert_error_contains(
            r#"
fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}

fn wrapper(ch: Channel<i64>) -> i64 {
    select {
        ch.send(heavy(3)) => { println(1); }
        default => { println(0); }
    }
    return 0;
}

async fn run(ch: Channel<i64>) -> i64 {
    return wrapper(ch);
}
"#,
            ErrorCode::E0810,
            "sync helper `wrapper` with a loop is not preemptible in task context",
        );
    }

    /// A `let` binding becomes visible only after its own initializer, so the
    /// call on the right-hand side still names the top-level helper.
    #[test]
    fn a_let_binding_does_not_shadow_its_own_initializer() {
        assert_call_edge_is_reached(
            r#"fn wrapper(n: i64) -> i64 {
    let heavy = heavy(n);
    return heavy;
}"#,
        );
    }

    /// A shadowing binding dies with its block. After the block closes, the
    /// name resolves to the top-level helper again — this is the walk's
    /// `exit_scope` being observed from outside.
    #[test]
    fn a_shadowing_binding_does_not_outlive_its_block() {
        assert_call_edge_is_reached(
            r#"fn wrapper(n: i64) -> i64 {
    if n > 0 {
        let heavy: fn(i64) -> i64 = |x: i64| -> i64 { return x + 1; };
        println(heavy(1));
    }
    return heavy(n);
}"#,
        );
    }

    /// A parameter shadows a top-level helper for the whole body, so the call
    /// is indirect through a function value and creates no static edge.
    #[test]
    fn a_parameter_shadows_a_top_level_helper_for_the_whole_body() {
        let analyzer = analyze(
            r#"
fn heavy(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        i = i + 1;
    }
    return i;
}

fn wrapper(heavy: fn(i64) -> i64) -> i64 {
    return heavy(1);
}

async fn run() -> i64 {
    let op: fn(i64) -> i64 = |x: i64| -> i64 { return x + 1; };
    return wrapper(op);
}
"#,
        );
        assert!(
            analyzer.errors.is_empty(),
            "the parameter makes the call indirect: {:#?}",
            analyzer.errors
        );
    }

    /// A `for` binding shadows only inside the loop, and the walk's scope stack
    /// has to pop it: the call after the loop is a real edge again.
    #[test]
    fn a_for_binding_shadows_only_inside_its_loop() {
        assert_call_edge_is_reached(
            r#"fn wrapper(n: i64) -> i64 {
    let ops: Array<fn(i64) -> i64> = [];
    for heavy in ops {
        println(heavy(1));
    }
    return heavy(n);
}"#,
        );
    }
}
