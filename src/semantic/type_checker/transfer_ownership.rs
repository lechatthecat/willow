//! Conservative, body-local ownership at synchronization boundaries.
//!
//! A union/find graph connects aliases and containing values. Each syntax node
//! contributes a bounded number of edges; facts are reduced once per component.
//! Assignments and branches deliberately join identities rather than assuming
//! that a particular path or rebinding removes an alias.
use std::collections::{HashMap, HashSet};

use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use crate::parser::iter::{AstEvent, AstWalk};
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};

use super::TypeChecker;

#[derive(Default)]
struct Graph {
    parents: Vec<usize>,
    ranks: Vec<u8>,
    borrowed: Vec<bool>,
    escaped: Vec<bool>,
    origins: Vec<usize>,
    loan_unsupported: Vec<bool>,
    loan_payload: Vec<bool>,
    loan_borrowed: Vec<bool>,
    external: Vec<bool>,
    locks: Vec<Option<(usize, usize)>>,
    values: HashMap<ExprId, usize>,
    names: HashMap<String, Vec<usize>>,
    scopes: Vec<Vec<String>>,
    uses: Vec<(usize, usize, Span)>,
    transfers: Vec<(usize, usize, usize, Span)>,
    escapes: Vec<(usize, Span)>,
    declarations: HashMap<usize, usize>,
    stores: Vec<(usize, usize, Span)>,
    ticks: usize,
    #[cfg(test)]
    root_steps: usize,
    loop_id: usize,
    loop_stack: Vec<usize>,
    lock_ends: HashMap<usize, usize>,
}

impl Graph {
    fn node(&mut self, borrowed: bool) -> usize {
        let id = self.parents.len();
        self.parents.push(id);
        self.ranks.push(0);
        self.borrowed.push(borrowed);
        self.escaped.push(false);
        self.origins.push(self.loop_id);
        self.loan_unsupported.push(false);
        self.loan_payload.push(false);
        self.loan_borrowed.push(false);
        self.external.push(false);
        self.locks.push(None);
        id
    }

    fn root(&mut self, mut id: usize) -> usize {
        while self.parents[id] != id {
            #[cfg(test)]
            {
                self.root_steps += 1;
            }
            self.parents[id] = self.parents[self.parents[id]];
            id = self.parents[id];
        }
        id
    }

    fn join(&mut self, a: usize, b: usize) {
        let (mut a, mut b) = (self.root(a), self.root(b));
        if a == b {
            return;
        }
        if self.ranks[a] < self.ranks[b] {
            std::mem::swap(&mut a, &mut b);
        }
        self.parents[b] = a;
        self.ranks[a] += u8::from(self.ranks[a] == self.ranks[b]);
        self.borrowed[a] |= self.borrowed[b];
        self.escaped[a] |= self.escaped[b];
        self.loan_unsupported[a] |= self.loan_unsupported[b];
        self.loan_payload[a] |= self.loan_payload[b];
        self.loan_borrowed[a] |= self.loan_borrowed[b];
        if self.origins[a] != self.origins[b] {
            self.origins[a] = 0;
        }
        self.locks[a] = match (self.locks[a], self.locks[b]) {
            (Some(a), Some(b)) => Some((a.0.max(b.0), a.1.max(b.1))),
            (a, b) => a.or(b),
        };
    }

    fn lookup(&mut self, name: &str) -> usize {
        if let Some(id) = self.names.get(name).and_then(|s| s.last()) {
            return *id;
        }
        // Parameters, captures and globals are borrowed, never fresh locals.
        let id = self.node(true);
        self.external[id] = true;
        self.names.entry(name.into()).or_default().push(id);
        id
    }

    fn bind(&mut self, name: &str, id: usize) {
        self.names.entry(name.into()).or_default().push(id);
        self.scopes.last_mut().unwrap().push(name.into());
    }

    fn alias(&mut self, result: Option<usize>, value: &Expr) {
        if let (Some(a), Some(b)) = (result, self.values.get(&value.id()).copied()) {
            self.join(a, b);
        }
    }

    fn use_value(&mut self, value: &Expr) {
        if let Some(id) = self.values.get(&value.id()).copied() {
            self.uses.push((id, self.ticks, value.span()));
        }
    }

    fn escape(&mut self, value: &Expr) {
        if let Some(id) = self.values.get(&value.id()).copied() {
            let root = self.root(id);
            self.escaped[root] = true;
            self.escapes.push((id, value.span()));
        }
    }

    fn transfer(&mut self, value: &Expr) {
        if let Some(id) = self.values.get(&value.id()).copied() {
            self.transfers
                .push((id, self.ticks, self.loop_id, value.span()));
        }
    }
}

/// A deliberately small proof: stores target only the receiver and no call,
/// closure, static storage, indexed store or superclass initializer can retain
/// its identity. Call sites additionally require Sync parameters and results
/// for methods. Constructors join their arguments into the new aggregate.
pub(super) fn isolated_body(body: &Block) -> bool {
    AstWalk::new(AstEvent::Block(body)).all(|event| match event {
        AstEvent::Expr(
            Expr::Call(_)
            | Expr::MethodCall(_)
            | Expr::StaticCall(_)
            | Expr::StaticField(_)
            | Expr::New(_)
            | Expr::Lambda(_)
            | Expr::Await(_)
            | Expr::Select(_)
            // `?` can invoke Into::into on its implicit return path.
            | Expr::TryPropagate(..),
        ) => false,
        AstEvent::Stmt(Stmt::StaticFieldAssign(_) | Stmt::IndexAssign(_) | Stmt::SuperInit(_)) => {
            false
        }
        AstEvent::Stmt(Stmt::FieldAssign(assign)) => {
            matches!(&assign.object, Expr::Var(name, ..) if name == "self")
        }
        AstEvent::Bind("self") => false,
        AstEvent::Stmt(Stmt::Assign(assign)) => assign.name != "self",
        _ => true,
    })
}

impl TypeChecker {
    fn isolated_method(
        &self,
        call: &MethodCallExpr,
        cache: &mut super::send_sync::MarkerCache,
    ) -> bool {
        let Some(Type::Named(class)) = self.expr_types.get(&call.object.id()) else {
            return false;
        };
        let Some((_, method)) = self.lookup_method_in_hierarchy(class, &call.method) else {
            return false;
        };
        method.isolated_body
            && !method.is_open
            && !method.is_async
            && self.is_sync_cached(&method.return_type, cache)
            && method
                .params
                .iter()
                .all(|ty| self.is_sync_cached(ty, cache))
    }

    #[allow(clippy::too_many_lines, reason = "split pending")]
    pub(super) fn check_transfer_ownership(&mut self, root: AstEvent<'_>) {
        if !self.enforce_send_sync {
            return;
        }
        let mut graph = Graph::default();
        graph.scopes.push(Vec::new());
        let mut pending_bindings = HashMap::new();
        let mut lock_bindings = HashMap::new();
        let mut sync = super::send_sync::MarkerCache::default();
        let mut select_sends = HashSet::new();
        let mut select_receives = HashSet::new();
        let mut deferred = 0;
        let mut conditional = 0usize;
        let mut loans = Vec::new();
        let mut loan_exprs = HashMap::new();
        let mut task_methods = HashMap::new();
        let mut waits = Vec::new();
        let mut loan_errors = Vec::new();
        let mut walk = AstWalk::new(root);
        while let Some(event) = walk.next() {
            graph.ticks += 1;
            match event {
                AstEvent::Stmt(Stmt::If(_) | Stmt::While(_) | Stmt::For(_))
                | AstEvent::Expr(
                    Expr::Match(_) | Expr::Select(_) | Expr::Ternary(_) | Expr::Binary(_),
                ) => conditional += 1,
                AstEvent::ExitStmt(Stmt::If(_) | Stmt::While(_) | Stmt::For(_))
                | AstEvent::ExitExpr(
                    Expr::Match(_) | Expr::Select(_) | Expr::Ternary(_) | Expr::Binary(_),
                ) => conditional -= 1,
                _ => {}
            }
            match event {
                AstEvent::EnterScope => graph.scopes.push(Vec::new()),
                AstEvent::ExitScope => {
                    for name in graph.scopes.pop().unwrap() {
                        graph.names.get_mut(&name).unwrap().pop();
                    }
                }
                AstEvent::Stmt(Stmt::Defer(_)) => deferred += 1,
                AstEvent::ExitStmt(Stmt::Defer(_)) => deferred -= 1,
                AstEvent::Stmt(stmt @ (Stmt::While(_) | Stmt::For(_))) => {
                    if let Stmt::For(for_loop) = stmt {
                        pending_bindings
                            .insert(for_loop.name.as_ptr() as usize, for_loop.iterable.id());
                    }
                    graph.loop_stack.push(graph.loop_id);
                    graph.loop_id = graph.ticks;
                }
                AstEvent::ExitStmt(Stmt::While(_) | Stmt::For(_)) => {
                    graph.loop_id = graph.loop_stack.pop().unwrap();
                }
                AstEvent::Stmt(Stmt::Lock(lock)) => {
                    let start = graph.ticks;
                    lock_bindings.insert(lock.binding.as_ptr() as usize, start);
                    for member in &lock.additional {
                        lock_bindings.insert(member.binding.as_ptr() as usize, start);
                    }
                }
                AstEvent::ExitStmt(Stmt::Lock(lock)) => {
                    let start = lock_bindings[&(lock.binding.as_ptr() as usize)];
                    graph.lock_ends.insert(start, graph.ticks);
                }
                AstEvent::Stmt(Stmt::Let(binding)) => {
                    pending_bindings.insert(binding.name.as_ptr() as usize, binding.init.id());
                }
                AstEvent::Bind(name) => {
                    // Allocate at the binding event so a receive inside a loop
                    // owns a new value in that iteration, like ordinary recv.
                    let received = select_receives.remove(&(name.as_ptr() as usize));
                    let id = pending_bindings
                        .get(&(name.as_ptr() as usize))
                        .and_then(|expr| graph.values.get(expr))
                        .copied()
                        .unwrap_or_else(|| graph.node(!received));
                    if let Some(start) = lock_bindings.get(&(name.as_ptr() as usize)) {
                        let root = graph.root(id);
                        graph.locks[root] = Some((*start, *start));
                    }
                    graph.declarations.insert(id, graph.ticks);
                    graph.bind(name, id);
                }
                AstEvent::Expr(expr) => {
                    if let Expr::Match(m) = expr {
                        for arm in &m.arms {
                            let pattern = &arm.pattern;
                            for event in AstWalk::new(AstEvent::Pattern(pattern)) {
                                if let AstEvent::Bind(name) = event {
                                    pending_bindings
                                        .insert(name.as_ptr() as usize, m.scrutinee.id());
                                }
                            }
                        }
                    }
                    if let Expr::Select(select) = expr {
                        for case in &select.cases {
                            match &case.kind {
                                SelectCaseKind::Send { value, .. } => {
                                    select_sends.insert(value.id());
                                }
                                SelectCaseKind::Recv { binding, .. } => {
                                    select_receives.insert(binding.as_ptr() as usize);
                                }
                                _ => {}
                            }
                        }
                    }
                    if let Some(ty) = self.expr_types.get(&expr.id())
                        && !self.is_sync_cached(ty, &mut sync)
                    {
                        let borrowed =
                            matches!(expr, Expr::StaticField(_) | Expr::Call(_) | Expr::Await(_));
                        let id = graph.node(borrowed);
                        graph.loan_payload[id] = self.task_loan_type(ty, &mut sync);
                        graph.loan_unsupported[id] = !graph.loan_payload[id];
                        graph.values.insert(expr.id(), id);
                    }
                    if let Expr::Lambda(lambda) = expr {
                        // Captures retain aliases even if the closure is never invoked.
                        let result = graph.values.get(&expr.id()).copied();
                        for capture in self.lambda_captures.get(&lambda.id).into_iter().flatten() {
                            let id = graph.lookup(&capture.name);
                            let root = graph.root(id);
                            let payload = self.task_loan_type(&capture.ty, &mut sync);
                            graph.loan_payload[root] |= payload;
                            graph.loan_borrowed[root] |= graph.external[id] && payload;
                            if let Some(result) = result {
                                graph.join(result, id);
                            }
                            let root = graph.root(id);
                            graph.escaped[root] = true;
                            graph.escapes.push((id, lambda.span));
                        }
                        walk.skip_children();
                    }
                }
                AstEvent::ExitExpr(expr) => {
                    let result = graph.values.get(&expr.id()).copied();
                    let args = match expr {
                        Expr::Call(call) => Some(&call.args),
                        Expr::MethodCall(call) => Some(&call.args),
                        Expr::StaticCall(call) => Some(&call.args),
                        _ => None,
                    };
                    let mut loan_values: Vec<&Expr> = args
                        .into_iter()
                        .flatten()
                        .filter(|arg| self.local.task_loan_args.contains(&arg.expr.id()))
                        .map(|arg| &arg.expr)
                        .collect();
                    if let Expr::MethodCall(call) = expr {
                        let (is_async, is_open) = match self.expr_types.get(&call.object.id()) {
                            Some(Type::Named(class)) => *task_methods
                                .entry((class.clone(), call.method.clone()))
                                .or_insert_with(|| {
                                    self.lookup_method_in_hierarchy(class, &call.method)
                                        .map_or((false, true), |(_, method)| {
                                            (method.is_async, method.is_open)
                                        })
                                }),
                            _ => (false, true),
                        };
                        if is_async && graph.values.contains_key(&call.object.id()) {
                            loan_values.push(&call.object);
                        }
                        if !loan_values.is_empty() && (!is_async || is_open) {
                            loan_errors.push((
                                call.span,
                                "non-Sync task loans require a statically known async method",
                            ));
                        }
                    }
                    if !loan_values.is_empty() {
                        for value in loan_values {
                            loan_exprs.insert(value.span(), value.id());
                            if let (Some(value_id), Some(task)) =
                                (graph.values.get(&value.id()).copied(), result)
                            {
                                loans.push((value_id, task, graph.ticks, value.span()));
                                if conditional > 0 || deferred > 0 || graph.scopes.len() > 2 {
                                    loan_errors.push((value.span(), "non-Sync task loans require an unconditional body-level call"));
                                }
                            } else {
                                loan_errors.push((
                                    value.span(),
                                    "cannot establish the task loan identity",
                                ));
                            }
                        }
                        if let Some(task) = result {
                            graph.borrowed[task] = false;
                        }
                        // The Task handle is not an alias of its captured payload.
                        continue;
                    }
                    match expr {
                        Expr::Await(wait) => {
                            if conditional == 0
                                && deferred == 0
                                && graph.scopes.len() <= 2
                                && let Some(task) = graph.values.get(&wait.expr.id()).copied()
                            {
                                waits.push((task, graph.ticks));
                            }
                        }
                        Expr::Var(name, span, _) => {
                            if let Some(result) = result {
                                let binding = graph.lookup(name);
                                graph.loan_borrowed[result] |=
                                    graph.external[binding] && graph.loan_payload[result];
                                graph.join(result, binding);
                                graph.uses.push((result, graph.ticks, *span));
                                if deferred > 0 {
                                    graph.escape(expr);
                                }
                            }
                        }
                        Expr::ArrayLiteral(values, ..) => {
                            for value in values {
                                graph.alias(result, value);
                            }
                        }
                        Expr::ObjectLiteral(object) => {
                            for field in &object.fields {
                                graph.alias(result, &field.value);
                            }
                        }
                        Expr::FieldAccess(object, ..) => {
                            graph.alias(result, object);
                        }
                        Expr::TryPropagate(operand, ..) => {
                            graph.alias(result, operand);
                            // The Err branch returns a payload even when Ok is
                            // Sync and therefore has no ownership graph node.
                            // Inspect the source error: an implicit Into::into
                            // may retain it even if the destination error is Sync.
                            // Option's None path carries no reference to escape.
                            if let Some((_, error)) = self
                                .expr_types
                                .get(&operand.id())
                                .and_then(|ty| builtin_types::binary_args(ty, B::Result))
                                && !self.is_sync_cached(error, &mut sync)
                            {
                                graph.escape(operand);
                            }
                        }
                        Expr::Index(object, ..) => {
                            graph.use_value(object);
                            graph.alias(result, object);
                        }
                        Expr::Ternary(ternary) => {
                            graph.alias(result, &ternary.then_expr);
                            graph.alias(result, &ternary.else_expr);
                        }
                        Expr::New(new) => {
                            let class = match self.expr_types.get(&expr.id()) {
                                Some(Type::Named(class)) => class,
                                _ => &new.class_name,
                            };
                            let implicit = self.symbols.lookup_class(class).is_some_and(|c| {
                                c.constructor.as_ref().is_none_or(|ctor| ctor.isolated_body)
                            });
                            if !implicit && let Some(id) = result {
                                let root = graph.root(id);
                                graph.borrowed[root] = true;
                            }
                            for arg in &new.args {
                                graph.alias(result, &arg.expr);
                                if !implicit {
                                    graph.escape(&arg.expr);
                                }
                            }
                        }
                        Expr::StaticCall(call) => {
                            // Classify the resolved callee, never its return type:
                            // a user helper returning Result may retain its inputs.
                            let owner = self.resolve_static_call_class_quiet(&call.class);
                            let builtin = owner.as_deref().and_then(B::from_name);
                            let transfers = call.method == "new"
                                && matches!(
                                    builtin,
                                    Some(
                                        B::Mutex | B::RwLock | B::BlockingCell | B::BlockingRwCell
                                    )
                                );
                            // Source Array constructors dispatch to a specialized
                            // provider class. Match its recorded target identity,
                            // rather than accepting every method returning Array.
                            let array_constructor = call.method == "new"
                                && self
                                    .expr_types
                                    .get(&expr.id())
                                    .and_then(|ty| self.array_class(ty))
                                    .is_some_and(|class| {
                                        self.static_call_classes.get(&call.id)
                                            == Some(
                                                &crate::semantic::ids::TypeId::from_source_name(
                                                    class,
                                                ),
                                            )
                                    });
                            let aggregate = self.static_call_enum_key(&call.class).is_some()
                                || array_constructor
                                || (call.method == "new"
                                    && matches!(builtin, Some(B::Array | B::Map)));
                            for arg in &call.args {
                                if transfers {
                                    graph.transfer(&arg.expr);
                                } else {
                                    graph.alias(result, &arg.expr);
                                    if !aggregate {
                                        graph.escape(&arg.expr);
                                    }
                                }
                            }
                            if !transfers
                                && !aggregate
                                && let Some(id) = result
                            {
                                let root = graph.root(id);
                                graph.borrowed[root] = true;
                            }
                        }
                        Expr::MethodCall(call) => {
                            graph.use_value(&call.object);
                            let builtin = self
                                .expr_types
                                .get(&call.object.id())
                                .and_then(builtin_types::resolve)
                                .map(|b| b.id);
                            let channel = builtin.is_some_and(|b| b == B::Channel);
                            let cell = builtin
                                .is_some_and(|b| matches!(b, B::BlockingCell | B::BlockingRwCell));
                            let send = (channel && call.method == "send")
                                || (cell && matches!(call.method.as_str(), "set" | "write"));
                            if cell && result.is_some() {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E2408,
                                        "cannot extract a non-Sync reference from a blocking cell",
                                    )
                                    .with_label(Label::primary(
                                        call.span,
                                        "the cell unlocks before the reference can be used",
                                    ))
                                    .with_help(
                                        "use Mutex and access this value inside a lexical lock",
                                    ),
                                );
                            }
                            let receive =
                                channel && matches!(call.method.as_str(), "recv" | "recv_opt");
                            let option_action = builtin.and_then(|owner| {
                                super::builtin_methods::option_method(owner, &call.method)
                                    .map(|(_, action)| action)
                            });
                            if let Some(super::builtin_methods::OptionAction::Transform {
                                error_input,
                                no_input,
                                ..
                            }) = option_action
                            {
                                // A Sync function pointer may retain the non-Sync
                                // payload passed implicitly by this builtin.
                                // Use the same input contract as type checking;
                                // Option::or_else receives no payload at all.
                                if !no_input
                                    && let Some(input) = self
                                        .expr_types
                                        .get(&call.object.id())
                                        .and_then(builtin_types::resolve)
                                        .and_then(|ty| ty.args.get(usize::from(error_input)))
                                    && !self.is_sync_cached(input, &mut sync)
                                {
                                    graph.escape(&call.object);
                                }
                                // The callback's returned references are opaque,
                                // just like the result of an ordinary call.
                                if let Some(Type::Fn(_, output) | Type::Closure(_, output)) = call
                                    .args
                                    .first()
                                    .and_then(|arg| self.expr_types.get(&arg.expr.id()))
                                    && !self.is_sync_cached(output, &mut sync)
                                    && let Some(id) = result
                                {
                                    let root = graph.root(id);
                                    graph.borrowed[root] = true;
                                }
                            }
                            let local_builtin = matches!(builtin, Some(B::Array | B::Map))
                                || option_action.is_some();
                            if !send && !receive {
                                graph.alias(result, &call.object);
                                if !local_builtin && !self.isolated_method(call, &mut sync) {
                                    graph.escape(&call.object);
                                }
                            }
                            for arg in &call.args {
                                if send {
                                    graph.transfer(&arg.expr);
                                } else {
                                    graph.alias(result, &arg.expr);
                                    graph.escape(&arg.expr);
                                    // Collection insertion retains its argument even when
                                    // the return type is scalar or Void.
                                    let receiver = graph.values.get(&call.object.id()).copied();
                                    graph.alias(receiver, &arg.expr);
                                }
                            }
                            if !receive
                                && !local_builtin
                                && let Some(id) = result
                            {
                                let root = graph.root(id);
                                graph.borrowed[root] = true;
                            }
                        }
                        Expr::Call(call) => {
                            for arg in &call.args {
                                graph.escape(&arg.expr);
                                graph.alias(result, &arg.expr);
                            }
                        }

                        Expr::Match(m) => {
                            // Destructured payloads are conservatively borrowed.
                            for arm in &m.arms {
                                if let MatchBody::Expr(value) = &arm.body {
                                    graph.alias(result, value);
                                }
                            }
                            if let Some(id) = result {
                                let root = graph.root(id);
                                graph.borrowed[root] = true;
                            }
                        }
                        _ => {}
                    }
                    if select_sends.contains(&expr.id()) {
                        graph.transfer(expr);
                    }
                }
                AstEvent::ExitStmt(Stmt::Assign(assign)) => {
                    let target = graph.lookup(&assign.name);
                    if let Some(value) = graph.values.get(&assign.value.id()).copied() {
                        graph.stores.push((
                            value,
                            graph.declarations.get(&target).copied().unwrap_or(0),
                            assign.value.span(),
                        ));
                    }
                    graph.alias(Some(target), &assign.value);
                }
                AstEvent::ExitStmt(Stmt::FieldAssign(assign)) => {
                    graph.use_value(&assign.object);
                    graph.escape(&assign.value);
                    let target = graph.values.get(&assign.object.id()).copied();
                    graph.alias(target, &assign.value);
                }
                AstEvent::ExitStmt(Stmt::IndexAssign(assign)) => {
                    graph.use_value(&assign.array);
                    graph.escape(&assign.value);
                    let target = graph.values.get(&assign.array.id()).copied();
                    graph.alias(target, &assign.value);
                }
                AstEvent::ExitStmt(Stmt::StaticFieldAssign(assign)) => graph.escape(&assign.value),
                AstEvent::ExitStmt(Stmt::Return(ret)) => {
                    if let Some(value) = &ret.value {
                        graph.escape(value);
                    }
                }
                _ => {}
            }
        }
        // Reduce permanent transfers first: a payload can be read as an early
        // argument, then moved by a later argument before its task is scheduled.
        // Reuse this component index for both loan starts and later value uses.
        let mut diagnostics = Vec::new();
        let mut moved = HashMap::new();
        for (id, tick, loop_id, span) in std::mem::take(&mut graph.transfers) {
            let root = graph.root(id);
            if self.local.current_async_context && graph.loan_borrowed[root] {
                loan_errors.push((
                    span,
                    "borrowed non-Sync argument cannot transfer out of its async call",
                ));
            }
            let reason = if graph.borrowed[root] {
                Some("cannot transfer a borrowed non-Sync value")
            } else if graph.escaped[root] {
                Some("cannot transfer a non-Sync value whose alias may have escaped")
            } else if graph.origins[root] != loop_id {
                Some("cannot transfer an outer non-Sync value in a repeated loop")
            } else if moved.contains_key(&root) {
                Some("non-Sync value is transferred more than once")
            } else {
                None
            };
            if let Some(reason) = reason {
                diagnostics.push((span, reason));
            }
            moved.entry(root).or_insert(tick);
        }
        // Resolve aliases once, then scan each payload's ordered loan intervals.
        // No per-use scan over all tasks or all aliases is needed.
        let mut task_counts = HashMap::new();
        let mut task_origins = HashSet::new();
        for (_, task, _, _) in &loans {
            if task_origins.insert(*task) {
                *task_counts.entry(graph.root(*task)).or_insert(0usize) += 1;
            }
        }
        let mut completed = HashMap::new();
        for (task, tick) in waits {
            let task = graph.root(task);
            if !graph.borrowed[task] && !graph.escaped[task] {
                completed.entry(task).or_insert(tick);
            }
        }
        let mut intervals: HashMap<usize, Vec<(usize, usize)>> = HashMap::new();
        for (value, task, start, span) in loans {
            let value = graph.root(value);
            let task = graph.root(task);
            if moved.get(&value).is_some_and(|tick| *tick < start) {
                loan_errors.push((
                    span,
                    "cannot lend a non-Sync value after permanent transfer",
                ));
            }
            let end = completed
                .get(&task)
                .copied()
                .filter(|end| *end > start && task_counts[&task] == 1)
                .unwrap_or(usize::MAX);
            if graph.borrowed[value]
                || graph.escaped[value]
                || graph.loan_unsupported[value]
                || graph.locks[value].is_some()
            {
                loan_errors.push((
                    span,
                    "cannot lend a borrowed, escaped, or unsupported non-Sync value to a task",
                ));
            }
            let ranges = intervals.entry(value).or_default();
            // Multiple aliases captured by this one task share a single loan.
            if ranges.last() == Some(&(start, end)) {
                continue;
            }
            if ranges.last().is_some_and(|(_, end)| start < *end) {
                loan_errors.push((span, "non-Sync value already has an outstanding task loan"));
            }
            ranges.push((start, end));
        }
        let mut positions = HashMap::<usize, usize>::new();
        for (id, tick, span) in std::mem::take(&mut graph.uses) {
            let root = graph.root(id);
            if let Some(ranges) = intervals.get(&root) {
                let pos = positions.entry(root).or_default();
                while *pos < ranges.len() && ranges[*pos].1 <= tick {
                    *pos += 1;
                }
                if ranges
                    .get(*pos)
                    .is_some_and(|(start, end)| *start < tick && tick < *end)
                {
                    loan_errors.push((
                        span,
                        "non-Sync value is unavailable until its task has been awaited",
                    ));
                }
            }
            if moved.get(&root).is_some_and(|moved| tick > *moved) {
                if intervals.contains_key(&root) {
                    loan_errors.push((
                        span,
                        "cannot access or lend a non-Sync value after permanent transfer",
                    ));
                }
                diagnostics.push((span, "use of a non-Sync value after ownership transfer"));
            }
            if let Some((start, end_key)) = graph.locks[root]
                && (tick < start || tick > graph.lock_ends[&end_key])
            {
                diagnostics.push((span, "protected non-Sync value escapes its lock scope"));
            }
        }
        for (id, declared, span) in std::mem::take(&mut graph.stores) {
            let root = graph.root(id);
            if graph.locks[root].is_some_and(|(start, _)| declared < start) {
                diagnostics.push((
                    span,
                    "protected non-Sync value is stored outside its lock scope",
                ));
            }
        }
        for (id, span) in std::mem::take(&mut graph.escapes) {
            let root = graph.root(id);
            if self.local.current_async_context && graph.loan_borrowed[root] {
                loan_errors.push((span, "borrowed non-Sync argument may escape its async call"));
            }
            if graph.locks[root].is_some() {
                diagnostics.push((
                    span,
                    "protected non-Sync value may escape through this operation",
                ));
            }
        }
        for (span, reason) in loan_errors {
            if let Some(ty @ Type::Named(_)) =
                loan_exprs.get(&span).and_then(|id| self.expr_types.get(id))
            {
                let diagnostic = self.task_loan_failure(ty, span, reason);
                self.push(diagnostic);
                continue;
            }
            self.push(Diagnostic::new(Severity::Error, ErrorCode::E2402, reason)
                .with_label(Label::primary(span, "exclusive task access cannot be established"))
                .with_note("this value is not `Sync`; even immediately awaited calls require proof of exclusive access")
                .with_help("use a fresh local Array/Map and await its task before reusing aliases; use FrozenArray/FrozenMap for snapshots or Mutex<Array<T>>/Mutex<Map<K, V>> for shared mutation"));
        }
        #[cfg(test)]
        {
            COUNTS.with(|counts| counts.set((graph.ticks, graph.parents.len(), graph.root_steps)));
            TOTAL_COUNTS.with(|counts| {
                let (ticks, nodes, roots) = counts.get();
                counts.set((
                    ticks + graph.ticks,
                    nodes + graph.parents.len(),
                    roots + graph.root_steps,
                ));
            });
        }
        let mut reported = HashSet::new();
        for (span, reason) in diagnostics {
            if reported.insert(span) {
                self.push(Diagnostic::new(Severity::Error, ErrorCode::E2408, reason)
                    .with_label(Label::primary(span, "exclusive ownership cannot be established"))
                    .with_help("transfer a fresh, unescaped local value and stop using its aliases; keep protected references inside the lock, or copy Sync data"));
            }
        }
    }
}

#[cfg(test)]
thread_local! { static TOTAL_COUNTS: std::cell::Cell<(usize, usize, usize)> = const { std::cell::Cell::new((0, 0, 0)) }; }

#[cfg(test)]
thread_local! { static COUNTS: std::cell::Cell<(usize, usize, usize)> = const { std::cell::Cell::new((0, 0, 0)) }; }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer::Lexer, parser::Parser};

    fn check(body: &str, extra: &str) -> Vec<Diagnostic> {
        let source = format!(
            "import std::collections::Array; class Box {{ pub n: i64; pub fn set(self, n: i64) {{ self.n = n; }} }} {extra} async fn main() {{ {body} }}"
        );
        let (program, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
        assert!(errors.is_empty(), "{source}: {errors:?}");
        let mut checker = TypeChecker::new();
        checker.set_enforce_send_sync(true);
        checker.check_program(&program);
        checker
            .errors
            .into_iter()
            .filter(|d| d.severity == Severity::Error)
            .collect()
    }

    #[test]
    fn task_loans_accept_owned_and_reject_alias_hazards() {
        let extra = "async fn take(a: Array<i64>) { a[0] = 7; await sleep(0); } async fn pair(a: Array<i64>, b: Array<i64>) { a[0] = b[0]; } fn opaque(a: Array<i64>) {} class Holder { pub a: Array<i64>; }";
        for (name, body, bad) in [
            (
                "immediate",
                "let a = [1]; await take(a); println(a[0]);",
                false,
            ),
            ("temporary", "await take([1]);", false),
            (
                "multiple arguments",
                "let a = [1]; let b = [2]; await pair(a, b); println(a[0]); println(b[0]);",
                false,
            ),
            (
                "same task aliases",
                "let a = [1]; await pair(a, a); println(a[0]);",
                false,
            ),
            (
                "parallel",
                "let a = [1]; let b = [2]; let t = take(a); let u = take(b); await t; println(a[0]); await u; println(b[0]);",
                false,
            ),
            (
                "alias restored",
                "let a = [1]; let b = a; let t = take(a); await t; println(b[0]);",
                false,
            ),
            (
                "task alias",
                "let a = [1]; let t = take(a); let u = t; await u; println(a[0]);",
                false,
            ),
            (
                "repeat",
                "let a = [1]; await take(a); await take(a); println(a[0]);",
                false,
            ),
            ("detached move", "let a = [1]; let t = take(a);", false),
            (
                "early read",
                "let a = [1]; let t = take(a); println(a[0]); await t;",
                true,
            ),
            (
                "early write",
                "let a = [1]; let t = take(a); a[0] = 2; await t;",
                true,
            ),
            (
                "early alias",
                "let a = [1]; let b = a; let t = take(a); println(b[0]); await t;",
                true,
            ),
            (
                "second task",
                "let a = [1]; let t = take(a); let u = take(a); await t; await u;",
                true,
            ),
            (
                "wrong await",
                "let a = [1]; let b = [2]; let t = take(a); let u = take(b); await u; println(a[0]); await t;",
                true,
            ),
            (
                "escaped before",
                "let a = [1]; opaque(a); await take(a);",
                true,
            ),
            (
                "opaque during",
                "let a = [1]; let t = take(a); opaque(a); await t;",
                true,
            ),
            (
                "mutex before",
                "let a = [1]; let m = Mutex::new(a); await take(a);",
                true,
            ),
            (
                "mutex during",
                "let a = [1]; let t = take(a); let m = Mutex::new(a); await t;",
                true,
            ),
            ("field", "let h = new Holder([1]); await take(h.a);", false),
            (
                "contained alias",
                "let a = [1]; let h = new Holder(a); await take(a);",
                false,
            ),
            (
                "conditional await",
                "let a = [1]; let t = take(a); if true { await t; } println(a[0]);",
                true,
            ),
            (
                "conditional loan",
                "let a = [1]; if true { await take(a); }",
                true,
            ),
            (
                "loop loan",
                "let a = [1]; while false { await take(a); }",
                true,
            ),
            (
                "deferred alias",
                "let a = [1]; defer { println(a[0]); } let t = take(a); await t;",
                true,
            ),
            (
                "captured alias",
                "let a = [1]; let f: closure() -> i64 = || a[0]; await take(a);",
                true,
            ),
            (
                "task reassigned",
                "let a = [1]; let mut t = take(a); t = take([2]); await t; println(a[0]);",
                true,
            ),
        ] {
            let errors = check(body, extra);
            assert_eq!(!errors.is_empty(), bad, "{name}: {errors:?}");
            if bad {
                assert!(
                    errors
                        .iter()
                        .any(|e| matches!(e.code, ErrorCode::E2402 | ErrorCode::E2408)),
                    "{name}: {errors:?}"
                );
            }
        }
    }

    #[test]
    fn task_loans_borrowed_arguments_cannot_escape() {
        for body in [
            "await take(a);",
            "let m = Mutex::new(a);",
            "let c: Channel<Array<i64>> = Channel::new(); c.send(a);",
            "let c = BlockingCell::new(a);",
            "opaque(a);",
            "let h = new Holder(a); opaque(h.a);",
            "let f: closure() -> i64 = || a[0];",
            "return a;",
        ] {
            let extra = format!(
                "class Holder {{ pub a: Array<i64>; }} fn opaque(a: Array<i64>) {{}} async fn take(a: Array<i64>) {{}} async fn leak(a: Array<i64>) -> Array<i64> {{ {body} return [1]; }}"
            );
            let errors = check("", &extra);
            assert!(
                errors
                    .iter()
                    .any(|e| matches!(e.code, ErrorCode::E2402 | ErrorCode::E2408)),
                "{body}: {errors:?}"
            );
        }
    }

    #[test]
    fn task_loans_later_argument_permanent_transfer() {
        let extra =
            "async fn take(a: Array<i64>, m: Mutex<Array<i64>>) {} async fn one(a: Array<i64>) {}";
        for body in [
            "let a = [1]; let t = take(a, Mutex::new(a)); await t;",
            "let a = [1]; let alias = a; await take(alias, Mutex::new(a));",
            "let a = [1]; let alias = a; await take(a, Mutex::new(alias));",
        ] {
            let errors = check(body, extra);
            assert!(
                errors.iter().any(|e| e.code == ErrorCode::E2402
                    && e.message.contains("after permanent transfer")),
                "{body}: {errors:?}"
            );
        }
        for body in [
            "let a = [1]; let b = [2]; await take(a, Mutex::new(b)); println(a[0]);",
            "let a = [1]; await one(a); let m = Mutex::new(a);",
        ] {
            let errors = check(body, extra);
            assert!(errors.is_empty(), "{body}: {errors:?}");
        }
    }

    #[test]
    fn task_loans_owned_classes_and_field_aliases() {
        let extra = "class Buffer { pub a: Array<i64>; pub async fn change(self) { self.a[0] = 2; await sleep(0); } } class Outer { pub b: Buffer; } async fn change(b: Buffer) { b.a[0] = 2; await sleep(0); } async fn array(a: Array<i64>) { a[0] = 3; } fn opaque(b: Buffer) {}";
        for (body, bad) in [
            (
                "let b = new Buffer([1]); await change(b); println(b.a[0]);",
                false,
            ),
            ("await change(new Buffer([1]));", false),
            (
                "let b = new Buffer([1]); let a = b.a; await change(b); println(a[0]);",
                false,
            ),
            (
                "let b = new Buffer([1]); await array(b.a); println(b.a[0]);",
                false,
            ),
            (
                "let b = new Buffer([1]); await b.change(); println(b.a[0]);",
                false,
            ),
            (
                "let b = new Buffer([1]); let c = new Buffer([2]); let t = b.change(); let u = change(c); await t; println(b.a[0]); await u; println(c.a[0]);",
                false,
            ),
            (
                "let o = new Outer(new Buffer([1])); await array(o.b.a); println(o.b.a[0]);",
                false,
            ),
            (
                "let b = new Buffer([1]); let a = b.a; let t = change(b); println(a[0]); await t;",
                true,
            ),
            (
                "let b = new Buffer([1]); let t = array(b.a); println(b.a[0]); await t;",
                true,
            ),
            (
                "let b = new Buffer([1]); let t = b.change(); println(b.a[0]); await t;",
                true,
            ),
            (
                "let b = new Buffer([1]); let c = b; let t = change(b); let u = change(c); await t; await u;",
                true,
            ),
            (
                "let a = [1]; let b = new Buffer(a); let c = new Buffer(a); let t = change(b); let u = change(c); await t; await u;",
                true,
            ),
            ("let b = new Buffer([1]); opaque(b); await change(b);", true),
            (
                "let b = new Buffer([1]); let m = Mutex::new(b); await change(b);",
                true,
            ),
            (
                "let b = new Buffer([1]); let t = change(b); let m = Mutex::new(b.a); await t;",
                true,
            ),
        ] {
            let errors = check(body, extra);
            assert_eq!(!errors.is_empty(), bad, "{body}: {errors:?}");
            if bad {
                assert!(
                    errors.iter().any(|e| e.code == ErrorCode::E2402),
                    "{errors:?}"
                );
            }
        }
        for body in [
            "await change(b);",
            "await b.change();",
            "await array(b.a);",
            "let m = Mutex::new(b);",
            "let m = Mutex::new(b.a);",
            "opaque(b);",
            "let c: Channel<Buffer> = Channel::new(); c.send(b);",
            "let f: closure() -> i64 = || b.a[0];",
        ] {
            let errors = check(
                "",
                &format!("{extra} async fn borrowed(b: Buffer) {{ {body} }}"),
            );
            assert!(
                errors.iter().any(|e| e.code == ErrorCode::E2402),
                "{body}: {errors:?}"
            );
        }
    }

    #[test]
    fn task_loans_generic_class_and_nested_collections() {
        let errors = check(
            "let b = new Generic<i64>(1); await change_generic(b); println(b.v); let arrays = [[1]]; await nested(arrays); println(arrays[0][0]);",
            "class Generic<T> { pub v: T; pub fn set(self, v: T) { self.v = v; } } async fn change_generic(b: Generic<i64>) { b.v = 3; } async fn nested(a: Array<Array<i64>>) { a[0][0] = 2; }",
        );
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn task_loans_linear_operation_counts() {
        for shape in [
            "functions",
            "aliases",
            "tasks",
            "class_tasks",
            "field_aliases",
        ] {
            for size in [16, 64, 256] {
                let mut body = String::new();
                let mut extra = String::from("async fn take(a: Array<i64>) { println(a[0]); }");
                if matches!(shape, "class_tasks" | "field_aliases") {
                    extra.push_str("class LoanBuffer { pub a: Array<i64>; pub async fn change(self) { self.a[0] = 2; } }");
                }
                match shape {
                    "class_tasks" => {
                        for i in 0..size {
                            body.push_str(&format!(
                                "let b{i} = new LoanBuffer([1]); let t{i} = b{i}.change();"
                            ));
                        }
                        for i in 0..size {
                            body.push_str(&format!("await t{i}; println(b{i}.a[0]);"));
                        }
                    }
                    "field_aliases" => {
                        body.push_str("let b = new LoanBuffer([1]); let a0 = b.a;");
                        for i in 1..size {
                            body.push_str(&format!("let a{i} = a{};", i - 1));
                        }
                        body.push_str("let t = b.change(); await t;");
                        for i in 0..size {
                            body.push_str(&format!("println(a{i}[0]);"));
                        }
                    }
                    "functions" => {
                        for i in 0..size {
                            extra.push_str(&format!("async fn work{i}() {{ let a = [1]; await take(a); println(a[0]); }}"));
                        }
                    }
                    "aliases" => {
                        body.push_str("let a = [1]; let b0 = a;");
                        for i in 1..size {
                            body.push_str(&format!("let b{i} = b{};", i - 1));
                        }
                        body.push_str("let t = take(a); await t;");
                        for i in 0..size {
                            body.push_str(&format!("println(b{i}[0]);"));
                        }
                    }
                    _ => {
                        for i in 0..size {
                            body.push_str(&format!("let a{i} = [1]; let t{i} = take(a{i});"));
                        }
                        for i in 0..size {
                            body.push_str(&format!("await t{i}; println(a{i}[0]);"));
                        }
                    }
                }
                TOTAL_COUNTS.with(|c| c.set((0, 0, 0)));
                let errors = check(&body, &extra);
                assert!(errors.is_empty(), "{errors:?}");
                let (ticks, nodes, roots) = TOTAL_COUNTS.with(|c| c.get());
                eprintln!(
                    "task loans {shape} n={size}: events={ticks} nodes={nodes} root_steps={roots}"
                );
                assert!(
                    ticks <= 80 * size + 100 && nodes <= 20 * size + 20 && roots <= 80 * size + 100
                );
            }
        }
    }

    #[test]
    fn ownership_transfer_twenty_four_perspectives() {
        let cases = [
            ("fresh mutex", "let m = Mutex::new(new Box(0));", "", false),
            (
                "owned local",
                "let b = new Box(0); let m = Mutex::new(b);",
                "",
                false,
            ),
            (
                "mutex source reuse",
                "let b = new Box(0); let m = Mutex::new(b); b.set(1);",
                "",
                true,
            ),
            (
                "mutex alias reuse",
                "let b = new Box(0); let a = b; let m = Mutex::new(b); a.set(1);",
                "",
                true,
            ),
            (
                "channel source reuse",
                "let c: Channel<Box> = Channel::new(); let b = new Box(0); c.send(b); b.set(1);",
                "",
                true,
            ),
            (
                "channel owned",
                "let c: Channel<Box> = Channel::new(); let b = new Box(0); c.send(b);",
                "",
                false,
            ),
            (
                "channel alias",
                "let c: Channel<Box> = Channel::new(); let b = new Box(0); let a = b; c.send(a); b.set(1);",
                "",
                true,
            ),
            (
                "double transfer",
                "let b = new Box(0); let m = Mutex::new(b); let n = Mutex::new(b);",
                "",
                true,
            ),
            (
                "borrowed parameter",
                "",
                "fn wrap(b: Box) -> Mutex<Box> { return Mutex::new(b); }",
                true,
            ),
            (
                "borrowed channel",
                "",
                "fn put(c: Channel<Box>, b: Box) { c.send(b); }",
                true,
            ),
            (
                "scalar copies",
                "let b = 1; let m = Mutex::new(b); let n = Mutex::new(b); println(b);",
                "",
                false,
            ),
            (
                "string copies",
                "let b = \"x\"; let m = Mutex::new(b); println(b);",
                "",
                false,
            ),
            (
                "lock scalar extraction",
                "let m = Mutex::new(1); let mut x = 0; lock m as b { x = b; } println(x);",
                "",
                false,
            ),
            (
                "lock escape",
                "let m = Mutex::new(new Box(0)); let mut x = new Box(1); lock m as b { x = b; } x.set(2);",
                "",
                true,
            ),
            (
                "lock return",
                "",
                "async fn bad(m: Mutex<Box>) -> Box { lock m as b { return b; } }",
                true,
            ),
            (
                "lock alias escape",
                "let m = Mutex::new(new Box(0)); let mut x = new Box(1); lock m as b { let a = b; x = a; }",
                "",
                true,
            ),
            (
                "lock field mutation",
                "let m = Mutex::new(new Box(0)); lock m as b { b.n = 2; println(b.n); }",
                "",
                false,
            ),
            (
                "lock call escape",
                "let m = Mutex::new(new Box(0)); lock m as b { keep(b); }",
                "fn keep(b: Box) {}",
                true,
            ),
            (
                "outer loop move",
                "let b = new Box(0); for i in 0..2 { let m = Mutex::new(b); }",
                "",
                true,
            ),
            (
                "fresh loop move",
                "for i in 0..2 { let b = new Box(0); let m = Mutex::new(b); }",
                "",
                false,
            ),
            (
                "branch reuse",
                "let b = new Box(0); if true { let m = Mutex::new(b); } b.set(1);",
                "",
                true,
            ),
            (
                "nested array alias",
                "let b = new Box(0); let a = [b]; let m = Mutex::new(a); b.set(1);",
                "",
                true,
            ),
            (
                "escaped local",
                "let b = new Box(0); keep(b); let m = Mutex::new(b);",
                "fn keep(b: Box) {}",
                true,
            ),
            (
                "shadowed local",
                "let b = new Box(0); let m = Mutex::new(b); if true { let b = new Box(2); b.n = 3; }",
                "",
                false,
            ),
        ];
        for (name, body, extra, reject) in cases {
            let errors = check(body, extra);
            if reject {
                assert!(
                    errors.iter().any(|d| d.code == ErrorCode::E2408),
                    "{name}: {errors:?}"
                );
                assert!(
                    errors.iter().all(|d| d.code == ErrorCode::E2408),
                    "{name}: {errors:?}"
                );
            } else {
                assert!(errors.is_empty(), "{name}: {errors:?}");
            }
        }
    }
    #[test]
    fn ownership_transfer_adverse_paths() {
        for (name, body, extra, reject) in [
            (
                "setter under lock",
                "let m = Mutex::new(new Box(0)); lock m as b { b.set(1); }",
                "",
                false,
            ),
            (
                "setter before transfer",
                "let b = new Box(0); b.set(1); let m = Mutex::new(b);",
                "",
                false,
            ),
            (
                "explicit isolated constructor",
                "let c = new C(0); let m = Mutex::new(c); lock m as c { c.n = 2; }",
                "class C { pub n: i64; pub init(self, n: i64) { self.n = n; } }",
                false,
            ),
            (
                "lock aggregate return",
                "",
                "async fn bad(m: Mutex<Box>) -> Array<Box> { lock m as b { return [b]; } }",
                true,
            ),
            (
                "lock iterator escape",
                "let m = Mutex::new([new Box(0)]); let mut x = new Box(1); lock m as bs { for b in bs { x = b; } }",
                "",
                true,
            ),
            (
                "lock closure",
                "let m = Mutex::new(new Box(0)); lock m as b { let f: closure() -> i64 = || b.n; }",
                "",
                true,
            ),
            (
                "captured source",
                "let b = new Box(0); let f: closure() -> i64 = || b.n; let m = Mutex::new(b);",
                "",
                true,
            ),
            (
                "deferred source",
                "let b = new Box(0); defer b.set(2); let m = Mutex::new(b);",
                "",
                true,
            ),
            (
                "transfer in lambda",
                "let f: fn(Box) -> Mutex<Box> = |b: Box| Mutex::new(b);",
                "",
                true,
            ),
            (
                "blocking reuse",
                "let b = new Box(0); let m = BlockingCell::new(b); b.set(1);",
                "",
                true,
            ),
            (
                "blocking extraction",
                "let m = BlockingCell::new(new Box(0)); let b = m.get(); b.set(1);",
                "",
                true,
            ),
            (
                "blocking replace",
                "let m = BlockingCell::new(new Box(0)); let b = new Box(1); m.set(b); b.set(2);",
                "",
                true,
            ),
            (
                "blocking scalar",
                "let m = BlockingCell::new(0); m.set(2); println(m.get());",
                "",
                false,
            ),
            (
                "receive owned",
                "let c: Channel<Box> = Channel::new(); c.send(new Box(0)); let b = c.recv(); let m = Mutex::new(b);",
                "",
                false,
            ),
            (
                "select reuse",
                "let c: Channel<Box> = Channel::new(); let b = new Box(0); select { c.send(b) => { b.set(1); } }",
                "",
                true,
            ),
            (
                "borrowed factory result",
                "let b = make(); let m = Mutex::new(b);",
                "fn make() -> Box { return new Box(0); }",
                true,
            ),
        ] {
            let errors = check(body, extra);
            if reject {
                assert!(
                    errors.iter().any(|d| d.code == ErrorCode::E2408),
                    "{name}: {errors:?}"
                );
            } else {
                assert!(errors.is_empty(), "{name}: {errors:?}");
            }
            assert!(
                errors.iter().all(|d| d.code == ErrorCode::E2408),
                "{name}: {errors:?}"
            );
        }
    }

    #[test]
    fn ownership_transfer_pattern_and_operand_order() {
        for (body, extra) in [
            (
                "let b = new C(0); b.touch(Mutex::new(b));",
                "class C { pub n: i64; pub fn touch(self, m: Mutex<C>) { self.n = 1; } }",
            ),
            (
                "let m = Mutex::new(Option::Some(new Box(0))); let mut out = new Box(1); lock m as opt { match opt { Option::Some(b) => { out = b; }, Option::None => {} } }",
                "",
            ),
            (
                "let b = new Box(0); let m = Mutex::new(Option::Some(b)); b.set(1);",
                "",
            ),
        ] {
            let extra = format!("enum Option<T> {{ Some(T), None }} {extra}");
            let errors = check(body, &extra);
            assert!(
                errors.iter().any(|d| d.code == ErrorCode::E2408),
                "{body}: {errors:?}"
            );
            assert!(
                errors.iter().all(|d| d.code == ErrorCode::E2408),
                "{body}: {errors:?}"
            );
        }
    }

    #[test]
    fn ownership_transfer_operation_counts() {
        for size in [16, 64, 256] {
            for shape in ["chain", "fanout", "independent"] {
                let mut body = String::from("let b = new Box(0);");
                for i in 0..size {
                    match shape {
                        "chain" => body.push_str(&format!(
                            "let a{i} = {};",
                            if i == 0 {
                                "b".into()
                            } else {
                                format!("a{}", i - 1)
                            }
                        )),
                        "fanout" => body.push_str(&format!("let a{i} = b;")),
                        _ => body.push_str(&format!(
                            "let b{i} = new Box(0); let m{i} = Mutex::new(b{i});"
                        )),
                    }
                }
                body.push_str("let m = Mutex::new(b);");
                assert!(check(&body, "").is_empty());
                let (events, nodes, steps) = COUNTS.with(|counts| counts.get());
                eprintln!(
                    "ownership {shape} n={size}: events={events} nodes={nodes} root_steps={steps}"
                );
                assert!(events <= 24 * size + 32, "{events}");
                assert!(nodes <= 5 * size + 8, "{nodes}");
                assert!(steps <= 12 * size + 16, "{steps}");
            }
        }
    }
    #[test]
    fn ownership_transfer_select_binding_identity_and_counts() {
        for size in [16, 64, 256] {
            let mut body = String::from("let c: Channel<Box> = Channel::new();");
            for _ in 0..size {
                body.push_str("select { let b = c.recv() => { let m = Mutex::new(b); } }");
            }
            assert!(check(&body, "").is_empty());
            let (events, nodes, steps) = COUNTS.with(|counts| counts.get());
            eprintln!(
                "ownership select receives n={size}: events={events} nodes={nodes} root_steps={steps}"
            );
            assert!(events <= 40 * size + 40, "{events}");
            assert!(nodes <= 5 * size + 8, "{nodes}");
            assert!(steps <= 6 * size + 16, "{steps}");
        }
    }
    #[test]
    fn ownership_transfer_callback_escape_counts() {
        for size in [16, 64, 256] {
            let mut body =
                String::from("let m = Mutex::new(Option::Some(new Box(0))); lock m as value {");
            for index in 0..size {
                body.push_str(&format!("let ignored{index} = value.map(keep);"));
            }
            body.push('}');
            let errors = check(
                &body,
                "enum Option<T> { Some(T), None } fn keep(b: Box) -> i64 { return 0; }",
            );
            assert_eq!(errors.len(), size);
            assert!(errors.iter().all(|d| d.code == ErrorCode::E2408));
            let (events, nodes, steps) = COUNTS.with(|counts| counts.get());
            eprintln!(
                "ownership callbacks n={size}: events={events} nodes={nodes} root_steps={steps}"
            );
            assert!(events <= 20 * size + 40, "{events}");
            assert!(nodes <= 3 * size + 8, "{nodes}");
            assert!(steps <= 6 * size + 16, "{steps}");
        }
    }
}
