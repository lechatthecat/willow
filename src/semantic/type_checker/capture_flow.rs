//! Execution-order provenance, independent of type checking and code generation.
//! Values form a graph: loop headers are cyclic joins, not repeated AST checks.
use super::*;
use crate::parser::iter::{AstEvent, AstWalk};
use std::rc::Rc;

type Value = usize;
type Env = im::OrdMap<Span, Value>;
const NORMAL: usize = 0;
const BREAK: usize = 1;
const CONTINUE: usize = 2;
const RETURN: usize = 3;

#[derive(Clone)]
pub(super) struct Origin {
    pub binding: Option<Span>,
    pub label: Rc<Label>,
}

#[derive(Default)]
pub(super) struct Inputs {
    pub reads: HashMap<Span, Span>,
    pub calls: HashMap<Span, Span>,
    pub callables: HashSet<Span>,
    pub captures: HashMap<Span, Vec<(Span, bool)>>,
    pub writes: HashMap<Span, Span>,
    pub reference_writes: HashMap<Span, Span>,
    pub origins: HashMap<Span, Origin>,
    pub invalid_defers: HashSet<Span>,
    pub resolved: HashMap<Span, Option<Rc<Label>>>,
}

#[derive(Default, Clone)]
struct Node {
    inputs: Vec<Value>,
    label: Option<Rc<Label>>,
    fallback: Option<Rc<Label>>,
    callable: Option<Rc<Summary>>,
    kill_if_escaped: bool,
}

// Summary nodes use holes for mutable cells and parameters. Immutable captures
// keep their declaration-time values. Calls substitute only the reachable graph,
// never re-walk the callee AST.
struct CallSite {
    callee: Value,
    args: Vec<Value>,
    indirect: bool,
    callback: bool,
    references: Vec<Span>,
}

enum Deferred<'a> {
    Body(&'a DeferBody),
    Call(CallSite),
}

struct Summary {
    start: Value,
    cells: Vec<(Span, Value)>,
    params: Vec<Value>,
    captured: Vec<Value>,
    effects: Vec<(Span, Value)>,
    result: Value,
    escapes: bool,
    opaque: bool,
}

#[derive(Default)]
struct Join {
    // Compare with the first incoming path, never with the accumulated union.
    // Common prefixes and untouched subtrees stay shared across many exits.
    first: Option<Env>,
    changes: HashMap<Span, Value>,
}

#[derive(Default)]
struct Flow([Join; 4]);

impl Join {
    fn add(&mut self, env: Env, graph: &mut Vec<Node>) {
        let Some(first) = &self.first else {
            self.first = Some(env);
            return;
        };
        for difference in first.diff(&env) {
            #[cfg(test)]
            FLOW_DIFFS.with(|n| n.set(n.get() + 1));
            let (key, value) = match difference {
                im::ordmap::DiffItem::Add(key, value) => (*key, *value),
                im::ordmap::DiffItem::Update {
                    new: (key, value), ..
                } => (*key, *value),
                im::ordmap::DiffItem::Remove(key, _) => (*key, 0),
            };
            let node = *self.changes.entry(key).or_insert_with(|| {
                let initial = first.get(&key).copied().unwrap_or(0);
                let node = graph.len();
                graph.push(Node {
                    inputs: vec![initial],
                    ..Node::default()
                });
                node
            });
            graph[node].inputs.push(value);
        }
    }

    fn finish(self) -> Option<Env> {
        let mut env = self.first?;
        for (key, value) in self.changes {
            env.insert(key, value);
        }
        Some(env)
    }
}

impl Flow {
    fn path(destination: usize, env: Env) -> Self {
        let mut flow = Self::default();
        flow.0[destination].first = Some(env);
        flow
    }

    fn take(&mut self, destination: usize) -> Option<Env> {
        std::mem::take(&mut self.0[destination]).finish()
    }

    fn merge(&mut self, mut other: Self, graph: &mut Vec<Node>) {
        for destination in 0..4 {
            if let Some(env) = other.take(destination) {
                self.0[destination].add(env, graph);
            }
        }
    }
}

struct Analysis<'a> {
    inputs: &'a Inputs,
    graph: Vec<Node>,
    values: HashMap<Span, Value>,
    targets: HashSet<Span>,
    loop_writes: HashMap<BodyId, HashSet<Span>>,
    cell_keys: HashSet<Span>,
    escaped: bool,
    returns: Vec<Value>,
    symbolic_calls: bool,
    opaque: bool,
}

fn pattern_matches(pattern: &Pattern, value: &Expr) -> Option<bool> {
    match (pattern, value) {
        (Pattern::Wildcard(..) | Pattern::Binding { .. }, _) => Some(true),
        (Pattern::LiteralBool(a, ..), Expr::Bool(b, ..)) => Some(a == b),
        (Pattern::LiteralInt(a, ..), Expr::Integer(b, ..)) => Some(a == b),
        (Pattern::LiteralChar(a, ..), Expr::Char(b, ..)) => Some(a == b),
        _ => None,
    }
}

fn guard_matches(guard: &Guard) -> Option<bool> {
    match guard {
        Guard::If(Expr::Bool(value, ..)) => Some(*value),
        Guard::Let(link) => pattern_matches(&link.pattern, &link.scrutinee),
        _ => None,
    }
}

impl<'a> Analysis<'a> {
    fn new(inputs: &'a Inputs, root: AstEvent<'_>, targets: HashSet<Span>) -> Self {
        // One structural walk indexes writes crossing loop boundaries. A fact
        // propagates only to loops outside its declaration and until recorded.
        let mut loop_writes: HashMap<BodyId, HashSet<Span>> = HashMap::new();
        let mut cell_keys = HashSet::new();
        let mut lambda_scopes = Vec::new();
        let mut calling_loops = HashSet::new();
        let mut loops = Vec::new();
        let mut owners = HashMap::new();
        for event in AstWalk::new(root) {
            match event {
                AstEvent::Lambda(lambda) => {
                    cell_keys.extend(
                        inputs
                            .captures
                            .get(&lambda.span)
                            .into_iter()
                            .flatten()
                            .filter_map(|&(key, mutable)| mutable.then_some(key)),
                    );
                    lambda_scopes.push((std::mem::take(&mut loops), std::mem::take(&mut owners)));
                }
                AstEvent::ExitExpr(Expr::Lambda(_)) => {
                    (loops, owners) = lambda_scopes.pop().unwrap();
                }
                AstEvent::Expr(Expr::Call(_) | Expr::MethodCall(_) | Expr::StaticCall(_)) => {
                    for &id in loops.iter().rev() {
                        if !calling_loops.insert(id) {
                            break;
                        }
                    }
                }
                AstEvent::Expr(expr) if inputs.reference_writes.contains_key(&expr.span()) => {
                    let key = inputs.reference_writes[&expr.span()];
                    let owner = owners.get(&key).copied().unwrap_or(0);
                    for id in loops[owner..].iter().rev() {
                        if !loop_writes.entry(*id).or_default().insert(key) {
                            break;
                        }
                    }
                }
                AstEvent::Stmt(Stmt::While(s)) => loops.push(s.body.id),
                AstEvent::Stmt(Stmt::For(s)) => loops.push(s.body.id),
                AstEvent::ExitStmt(Stmt::While(_) | Stmt::For(_)) => {
                    loops.pop();
                }
                AstEvent::Stmt(Stmt::Let(s)) => {
                    owners.insert(s.span, loops.len());
                }
                AstEvent::Stmt(Stmt::Assign(s)) => {
                    if let Some(key) = inputs.writes.get(&s.value.span()) {
                        let owner = owners.get(key).copied().unwrap_or(0);
                        for id in loops[owner..].iter().rev() {
                            if !loop_writes.entry(*id).or_default().insert(*key) {
                                break;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        for id in calling_loops {
            loop_writes.entry(id).or_default().extend(&cell_keys);
        }
        Self {
            inputs,
            graph: vec![Node::default()],
            values: HashMap::new(),
            targets,
            loop_writes,
            cell_keys,
            escaped: false,
            returns: Vec::new(),
            symbolic_calls: false,
            opaque: false,
        }
    }

    fn node(
        &mut self,
        inputs: Vec<Value>,
        label: Option<Rc<Label>>,
        fallback: Option<Rc<Label>>,
    ) -> Value {
        let id = self.graph.len();
        self.graph.push(Node {
            inputs,
            label,
            fallback,
            callable: None,
            kill_if_escaped: false,
        });
        id
    }

    fn summarize_value(&mut self, root: Value, start: Value, holes: &HashSet<Value>) -> Value {
        // Effect summaries describe witnesses, not entire value histories. Keep
        // a unique frontier of input holes/immutable values so repeated wrapper
        // calls cannot duplicate a transitive branch graph exponentially.
        if root < start
            || holes.contains(&root)
            || self.graph[root].inputs.is_empty()
            || self.graph[root].callable.is_some()
            || self.graph[root].fallback.is_some()
            || self.graph[root].kill_if_escaped
        {
            return root;
        }
        let mut pending = vec![root];
        let mut seen = HashSet::new();
        let mut frontier = std::collections::BTreeSet::new();
        let mut fixed = None;
        while let Some(value) = pending.pop() {
            if !seen.insert(value) {
                continue;
            }
            #[cfg(test)]
            FLOW_SUMMARY_WORK.with(|n| n.set(n.get() + 1));
            let node = &self.graph[value];
            if let Some(label) = &node.label {
                fixed = Some(label.clone());
                break;
            }
            if value < start
                || holes.contains(&value)
                || node.fallback.is_some()
                || node.kill_if_escaped
            {
                frontier.insert(value);
            } else {
                pending.extend(&node.inputs);
            }
        }
        if let Some(label) = fixed {
            self.node(Vec::new(), Some(label), None)
        } else {
            let sources: Vec<_> = frontier.into_iter().collect();
            match sources.as_slice() {
                [] => 0,
                // Even a single known witness may coexist with an opaque
                // alternative. Do not turn a witness back into a definite callee.
                _ => self.node(sources, None, None),
            }
        }
    }

    fn instantiate(&mut self, summary: &Summary, env: &mut Env, args: &[Value]) -> Value {
        let mut replacements = HashMap::new();
        for &(key, hole) in &summary.cells {
            replacements.insert(hole, env.get(&key).copied().unwrap_or(0));
        }
        for (&hole, &arg) in summary.params.iter().zip(args) {
            replacements.insert(hole, arg);
        }
        if summary.opaque {
            self.unknown_call(env, summary.escapes);
        }
        let mut pending: Vec<_> = summary.effects.iter().map(|&(_, v)| v).collect();
        pending.push(summary.result);
        let mut copies = Vec::new();
        while let Some(value) = pending.pop() {
            if value < summary.start || replacements.contains_key(&value) {
                continue;
            }
            let node = self.graph[value].clone();
            if node.kill_if_escaped && self.escaped {
                replacements.insert(value, 0);
                continue;
            }
            // Leaves are immutable. Reuse them across repeated calls and wrapper
            // summaries instead of copying constant provenance subgraphs.
            if node.inputs.is_empty()
                && node
                    .callable
                    .as_ref()
                    .is_none_or(|inner| inner.captured.iter().all(|&value| value < summary.start))
            {
                replacements.insert(value, value);
                continue;
            }
            let copy = self.graph.len();
            replacements.insert(value, copy);
            pending.extend(&node.inputs);
            self.graph.push(node);
            copies.push(copy);
        }
        for copy in copies.into_iter().rev() {
            for source in &mut self.graph[copy].inputs {
                *source = replacements.get(source).copied().unwrap_or(*source);
            }
            // A returned closure with symbolic captures needs another invocation
            // context. Its effects are opaque here; never retain stale cell facts.
            self.graph[copy].callable = None;
            if self.graph[copy].kill_if_escaped {
                let source = self.graph[copy].inputs[0];
                if self.graph[source].kill_if_escaped {
                    self.graph[copy].inputs = self.graph[source].inputs.clone();
                }
                if !self.symbolic_calls {
                    self.graph[copy].kill_if_escaped = false;
                }
            }
        }
        for &(key, value) in &summary.effects {
            env.insert(key, replacements.get(&value).copied().unwrap_or(value));
        }
        self.escaped |= summary.escapes;
        self.opaque |= summary.opaque;
        replacements
            .get(&summary.result)
            .copied()
            .unwrap_or(summary.result)
    }

    fn invoke(&mut self, call: &CallSite, env: &mut Env) -> Value {
        let result = if let Some(summary) = self.graph[call.callee].callable.clone() {
            self.instantiate(&summary, env, &call.args)
        } else {
            let callback = call.indirect || call.callback;
            self.unknown_call(env, callback);
            0
        };
        for key in &call.references {
            env.insert(*key, 0);
        }
        result
    }

    fn may_write(&self, value: Value) -> bool {
        value != 0
            && self.graph[value]
                .callable
                .as_ref()
                .is_none_or(|summary| !summary.effects.is_empty() || summary.escapes)
    }

    fn unknown_call(&mut self, env: &mut Env, callback: bool) {
        self.opaque = true;
        self.escaped |= callback;
        if self.escaped || self.symbolic_calls {
            for key in &self.cell_keys {
                if let Some(&value) = env.get(key) {
                    if self.escaped {
                        env.insert(*key, 0);
                    } else if value != 0 && !self.graph[value].kill_if_escaped {
                        let node = self.graph.len();
                        self.graph.push(Node {
                            inputs: vec![value],
                            kill_if_escaped: true,
                            ..Node::default()
                        });
                        env.insert(*key, node);
                    }
                }
            }
        }
    }

    fn bind_pattern(pattern: &Pattern, value: Value, env: &mut Env, locals: &mut Vec<Span>) {
        if let Pattern::Binding { span, .. } = pattern {
            env.insert(*span, value);
            locals.push(*span);
        }
    }

    fn remove_locals(flow: &mut Flow, locals: &[Span]) {
        for destination in 0..4 {
            if let Some(mut env) = flow.take(destination) {
                for key in locals {
                    env.remove(key);
                }
                flow.0[destination].first = Some(env);
            }
        }
    }

    fn remember(&mut self, span: Span, value: Value) {
        if !self.targets.contains(&span) {
            return;
        }
        if let Some(previous) = self.values.get(&span).copied() {
            if previous != value {
                let joined = self.node(vec![previous, value], None, None);
                self.values.insert(span, joined);
            }
        } else {
            self.values.insert(span, value);
        }
    }

    fn witnesses(&self) -> Vec<Option<Rc<Label>>> {
        let mut reverse = vec![Vec::new(); self.graph.len()];
        let mut labels = vec![None; self.graph.len()];
        let mut queue = std::collections::VecDeque::new();
        for (id, node) in self.graph.iter().enumerate() {
            for &source in &node.inputs {
                reverse[source].push(id);
            }
            if let Some(label) = &node.label {
                labels[id] = Some(label.clone());
                queue.push_back(id);
            }
        }
        // Each node obtains at most one witness. This also resolves cyclic
        // loop phis in O(nodes + edges), without iteration or capture-set unions.
        for fallback in [false, true] {
            if fallback {
                for (id, node) in self.graph.iter().enumerate() {
                    if labels[id].is_none()
                        && let Some(label) = &node.fallback
                    {
                        labels[id] = Some(label.clone());
                        queue.push_back(id);
                    }
                }
            }
            while let Some(source) = queue.pop_front() {
                for &target in &reverse[source] {
                    if labels[target].is_none() {
                        labels[target] = labels[source].clone();
                        queue.push_back(target);
                    }
                }
            }
        }
        labels
    }

    fn resolved(self) -> HashMap<Span, Option<Rc<Label>>> {
        let labels = self.witnesses();
        #[cfg(test)]
        FLOW_NODES.with(|n| n.set(n.get() + self.graph.len()));
        self.values
            .into_iter()
            .map(|(span, value)| (span, labels[value].clone()))
            .collect()
    }
}

#[willow_continuations::methods(
    block,
    statement,
    expression,
    children,
    loop_body,
    deferred,
    summarize,
    call_operands
)]
impl Analysis<'_> {
    fn summarize(&mut self, lambda: &LambdaExpr, outer: &Env) -> Rc<Summary> {
        let start = self.graph.len();
        let mut env = Env::new();
        let mut keys = HashSet::new();
        let mut captured = Vec::new();
        for &(key, mutable) in self.inputs.captures.get(&lambda.span).into_iter().flatten() {
            let value = outer.get(&key).copied().unwrap_or(0);
            if mutable {
                keys.insert(key);
            } else {
                env.insert(key, value);
                captured.push(value);
            }
            if let Some(summary) = &self.graph[value].callable {
                keys.extend(summary.cells.iter().map(|&(key, _)| key));
            }
        }
        let mut cells = Vec::new();
        for key in keys {
            let hole = self.node(Vec::new(), None, None);
            cells.push((key, hole));
            env.insert(key, hole);
        }
        let mut params = Vec::new();
        for param in &lambda.params {
            let hole = self.node(Vec::new(), None, None);
            params.push(hole);
            env.insert(param.span, hole);
        }
        let saved_targets = std::mem::take(&mut self.targets);
        let saved_returns = std::mem::take(&mut self.returns);
        let saved_escaped = std::mem::take(&mut self.escaped);
        let saved_opaque = std::mem::take(&mut self.opaque);
        let saved_symbolic = std::mem::replace(&mut self.symbolic_calls, true);
        let mut flow = match &lambda.body {
            LambdaBody::Expr(expr) => {
                let (flow, value) = self.expression(expr, env);
                self.returns.push(value);
                flow
            }
            LambdaBody::Block(block) => self.block(block, env),
        };
        let returns = std::mem::take(&mut self.returns);
        let result = match returns.as_slice() {
            [] => 0,
            [value] => *value,
            _ => self.node(returns, None, None),
        };
        let mut exits = Join::default();
        for destination in [NORMAL, RETURN] {
            if let Some(env) = flow.take(destination) {
                exits.add(env, &mut self.graph);
            }
        }
        let after = exits.finish().unwrap_or_default();
        let mut effects: Vec<_> = cells
            .iter()
            .filter_map(|&(key, hole)| {
                let value = after.get(&key).copied().unwrap_or(0);
                (value != hole).then_some((key, value))
            })
            .collect();
        let holes: HashSet<_> = cells
            .iter()
            .map(|&(_, hole)| hole)
            .chain(params.iter().copied())
            .collect();
        let mut normalized = HashMap::new();
        for (_, value) in &mut effects {
            let result = if let Some(&result) = normalized.get(value) {
                result
            } else {
                let result = self.summarize_value(*value, start, &holes);
                normalized.insert(*value, result);
                result
            };
            *value = result;
        }
        let result = normalized
            .get(&result)
            .copied()
            .unwrap_or_else(|| self.summarize_value(result, start, &holes));
        let escapes = self.escaped;
        let opaque = self.opaque;
        self.escaped = saved_escaped;
        self.opaque = saved_opaque;
        self.symbolic_calls = saved_symbolic;
        self.targets = saved_targets;
        self.returns = saved_returns;
        Rc::new(Summary {
            start,
            cells,
            params,
            captured,
            effects,
            result,
            escapes,
            opaque,
        })
    }

    fn block(&mut self, block: &Block, env: Env) -> Flow {
        let mut normal = Some(env);
        let mut exits = Flow::default();
        let mut cleanups = Vec::new();
        for stmt in &block.stmts {
            let Some(env) = normal.take() else {
                break;
            };
            if let Stmt::Defer(s) = stmt {
                if self.inputs.invalid_defers.contains(&s.body.span()) {
                    normal = Some(env);
                    continue;
                }
                let action;
                match &s.body {
                    DeferBody::Block(_) | DeferBody::Expr(Expr::Match(_)) => {
                        action = Deferred::Body(&s.body);
                        normal = Some(env);
                    }
                    DeferBody::Expr(expr) => {
                        let (mut operands, call) = self.call_operands(expr, env);
                        normal = operands.take(NORMAL);
                        exits.merge(operands, &mut self.graph);
                        action = Deferred::Call(call);
                    }
                }
                // Operands run at registration. The frozen callee/arguments or
                // deferred body execute only on exits after this registration.
                if normal.is_some() {
                    cleanups.push((action, std::mem::take(&mut exits)));
                }
                continue;
            }
            let mut flow = self.statement(stmt, env);
            normal = flow.take(NORMAL);
            exits.merge(flow, &mut self.graph);
        }
        if let Some(env) = normal {
            exits.0[NORMAL].add(env, &mut self.graph);
        }
        for (body, earlier) in cleanups.into_iter().rev() {
            let mut after = Flow::default();
            for destination in 0..4 {
                if let Some(env) = exits.take(destination) {
                    let mut cleanup = self.deferred(&body, env);
                    if let Some(env) = cleanup.take(NORMAL) {
                        after.0[destination].add(env, &mut self.graph);
                    }
                }
            }
            after.merge(earlier, &mut self.graph);
            exits = after;
        }
        for destination in 0..4 {
            if let Some(mut env) = exits.take(destination) {
                for stmt in &block.stmts {
                    if let Stmt::Let(s) = stmt
                        && self.inputs.writes.contains_key(&s.init.span())
                    {
                        env.remove(&s.span);
                    }
                }
                exits.0[destination].first = Some(env);
            }
        }
        exits
    }

    fn deferred(&mut self, action: &Deferred<'_>, env: Env) -> Flow {
        match action {
            Deferred::Body(DeferBody::Block(block)) => self.block(block, env),
            Deferred::Body(DeferBody::Expr(expr)) => self.expression(expr, env).0,
            Deferred::Call(call) => {
                let mut env = env;
                self.invoke(call, &mut env);
                Flow::path(NORMAL, env)
            }
        }
    }

    fn call_operands(&mut self, expr: &Expr, env: Env) -> (Flow, CallSite) {
        let binding = self.inputs.calls.get(&expr.span());
        let callee = binding.and_then(|key| env.get(key)).copied().unwrap_or(0);
        let arguments = match expr {
            Expr::Call(c) => c.args.as_slice(),
            Expr::MethodCall(c) => c.args.as_slice(),
            Expr::StaticCall(c) => c.args.as_slice(),
            Expr::New(c) => c.args.as_slice(),
            _ => &[],
        };
        let references = arguments
            .iter()
            .filter(|arg| matches!(arg.mode, CallArgMode::Reference { .. }))
            .filter_map(|arg| self.inputs.reference_writes.get(&arg.expr.span()).copied())
            .collect();
        let mut flow = Flow::path(NORMAL, env);
        let mut args = Vec::new();
        let mut callback = false;
        let mut walk = AstWalk::new(AstEvent::Expr(expr));
        walk.next();
        while let Some(event) = walk.next() {
            if let AstEvent::Expr(child) = event {
                walk.skip_children();
                if let Some(env) = flow.take(NORMAL) {
                    let (other, value) = self.expression(child, env);
                    callback |= self.inputs.callables.contains(&child.span())
                        && (value == 0 || self.may_write(value));
                    args.push(value);
                    flow.merge(other, &mut self.graph);
                }
            }
        }
        (
            flow,
            CallSite {
                callee,
                args,
                indirect: binding.is_some(),
                callback,
                references,
            },
        )
    }

    fn statement(&mut self, stmt: &Stmt, env: Env) -> Flow {
        #[cfg(test)]
        FLOW_STEPS.with(|n| n.set(n.get() + 1));
        match stmt {
            Stmt::Let(s) => {
                let (mut flow, value) = self.expression(&s.init, env);
                if let Some(mut env) = flow.take(NORMAL) {
                    if self.inputs.writes.contains_key(&s.init.span()) {
                        env.insert(s.span, value);
                    }
                    self.remember(s.span, value);
                    flow.0[NORMAL].first = Some(env);
                }
                flow
            }
            Stmt::Assign(s) => {
                let (mut flow, value) = self.expression(&s.value, env);
                if let Some(mut env) = flow.take(NORMAL) {
                    if let Some(target) = self.inputs.writes.get(&s.value.span()) {
                        env.insert(*target, value);
                    } else if self.inputs.callables.contains(&s.value.span())
                        && self.may_write(value)
                    {
                        self.escaped = true;
                    }
                    self.remember(s.span, value);
                    flow.0[NORMAL].first = Some(env);
                }
                flow
            }
            Stmt::If(s) => {
                let (mut flow, _) = self.expression(&s.cond, env);
                if let Some(env) = flow.take(NORMAL) {
                    if !matches!(s.cond, Expr::Bool(false, ..)) {
                        flow.merge(self.block(&s.then_block, env.clone()), &mut self.graph);
                    }
                    if !matches!(s.cond, Expr::Bool(true, ..)) {
                        let other = if let Some(block) = &s.else_block {
                            self.block(block, env)
                        } else {
                            Flow::path(NORMAL, env)
                        };
                        flow.merge(other, &mut self.graph);
                    }
                }
                flow
            }
            Stmt::While(s) => self.loop_body(&s.body, Some(&s.cond), env),
            Stmt::For(s) => {
                let (mut flow, _) = self.expression(&s.iterable, env);
                if let Some(env) = flow.take(NORMAL) {
                    flow.merge(self.loop_body(&s.body, None, env), &mut self.graph);
                }
                flow
            }
            Stmt::Break(_) => Flow::path(BREAK, env),
            Stmt::Continue(_) => Flow::path(CONTINUE, env),
            Stmt::Return(s) => {
                let mut flow = if let Some(expr) = &s.value {
                    let (flow, value) = self.expression(expr, env);
                    self.remember(s.span, value);
                    self.returns.push(value);
                    flow
                } else {
                    Flow::path(NORMAL, env)
                };
                if let Some(env) = flow.take(NORMAL) {
                    flow.0[RETURN].add(env, &mut self.graph);
                }
                flow
            }
            Stmt::Expr(s) => self.expression(&s.expr, env).0,
            Stmt::Lock(s) => {
                let (mut flow, _) = self.expression(&s.target, env);
                if let Some(env) = flow.take(NORMAL) {
                    flow.merge(self.block(&s.body, env), &mut self.graph);
                }
                flow
            }
            _ => self.children(AstEvent::Stmt(stmt), env),
        }
    }

    fn loop_body(&mut self, body: &Block, condition: Option<&Expr>, env: Env) -> Flow {
        let mut env = env;
        let mut phis = Vec::new();
        if let Some(writes) = self.loop_writes.get(&body.id) {
            for key in writes {
                {
                    let value = env.get(key).copied().unwrap_or(0);
                    let phi = self.graph.len();
                    self.graph.push(Node {
                        inputs: vec![value],
                        ..Node::default()
                    });
                    env.insert(*key, phi);
                    phis.push((*key, phi));
                }
            }
        }
        let mut flow = if let Some(cond) = condition {
            self.expression(cond, env).0
        } else {
            Flow::path(NORMAL, env)
        };
        let Some(header) = flow.take(NORMAL) else {
            return flow;
        };
        if condition.is_none_or(|c| !matches!(c, Expr::Bool(false, ..))) {
            let mut body_flow = self.block(body, header.clone());
            let mut back = Join::default();
            for destination in [NORMAL, CONTINUE] {
                if let Some(env) = body_flow.take(destination) {
                    back.add(env, &mut self.graph);
                }
            }
            if let Some(back) = back.finish() {
                for (key, phi) in phis {
                    let value = back.get(&key).copied().unwrap_or(0);
                    self.graph[phi].inputs.push(value);
                }
            }
            if let Some(env) = body_flow.take(BREAK) {
                flow.0[NORMAL].add(env, &mut self.graph);
            }
            flow.merge(body_flow, &mut self.graph);
        }
        if condition.is_none_or(|c| !matches!(c, Expr::Bool(true, ..))) {
            flow.0[NORMAL].add(header, &mut self.graph);
        }
        flow
    }

    fn children(&mut self, root: AstEvent<'_>, env: Env) -> Flow {
        let stores_value = matches!(
            root,
            AstEvent::Stmt(
                Stmt::FieldAssign(_) | Stmt::StaticFieldAssign(_) | Stmt::IndexAssign(_)
            )
        );
        let mut flow = Flow::path(NORMAL, env);
        let mut walk = AstWalk::new(root);
        walk.next();
        while let Some(event) = walk.next() {
            if let AstEvent::Expr(child) = event {
                walk.skip_children();
                if let Some(env) = flow.take(NORMAL) {
                    let (other, value) = self.expression(child, env);
                    if stores_value && self.inputs.callables.contains(&child.span()) {
                        self.escaped |= value == 0 || self.may_write(value);
                    }
                    flow.merge(other, &mut self.graph);
                }
            }
        }
        flow
    }

    fn expression(&mut self, expr: &Expr, env: Env) -> (Flow, Value) {
        #[cfg(test)]
        FLOW_STEPS.with(|n| n.set(n.get() + 1));
        let (flow, value) = match expr {
            Expr::Var(..) => {
                let value = if let Some(&key) = self.inputs.reads.get(&expr.span()) {
                    env.get(&key).copied().unwrap_or(0)
                } else {
                    0
                };
                (Flow::path(NORMAL, env), value)
            }
            Expr::Lambda(lambda) => {
                let value = if let Some(origin) = self.inputs.origins.get(&expr.span()) {
                    if let Some(key) = origin.binding {
                        let source = env.get(&key).copied().unwrap_or(0);
                        self.node(vec![source], None, Some(origin.label.clone()))
                    } else {
                        self.node(Vec::new(), Some(origin.label.clone()), None)
                    }
                } else {
                    self.node(Vec::new(), None, None)
                };
                let summary = self.summarize(lambda, &env);
                self.graph[value].callable = Some(summary);
                (Flow::path(NORMAL, env), value)
            }
            Expr::ArrayLiteral(elements, ..) => {
                let mut flow = Flow::path(NORMAL, env);
                for element in elements {
                    if let Some(env) = flow.take(NORMAL) {
                        let (other, value) = self.expression(element, env);
                        self.escaped |= self.inputs.callables.contains(&element.span())
                            && (value == 0 || self.may_write(value));
                        flow.merge(other, &mut self.graph);
                    }
                }
                (flow, 0)
            }
            Expr::Call(_)
            | Expr::MethodCall(_)
            | Expr::StaticCall(_)
            | Expr::New(_)
            | Expr::ObjectLiteral(_) => {
                let (mut flow, call) = self.call_operands(expr, env);
                let mut result = 0;
                if let Some(mut env) = flow.take(NORMAL) {
                    result = self.invoke(&call, &mut env);
                    flow.0[NORMAL].first = Some(env);
                }
                (flow, result)
            }
            Expr::Binary(binary) if matches!(binary.op, BinOp::And | BinOp::Or) => {
                let (mut flow, _) = self.expression(&binary.lhs, env);
                if let Some(env) = flow.take(NORMAL) {
                    let short_value = binary.op == BinOp::Or;
                    let known = if let Expr::Bool(value, ..) = binary.lhs {
                        Some(value)
                    } else {
                        None
                    };
                    if known != Some(!short_value) {
                        flow.0[NORMAL].add(env.clone(), &mut self.graph);
                    }
                    if known != Some(short_value) {
                        flow.merge(self.expression(&binary.rhs, env).0, &mut self.graph);
                    }
                }
                (flow, 0)
            }
            Expr::Ternary(t) => {
                let (mut flow, _) = self.expression(&t.condition, env);
                let mut values = Vec::new();
                if let Some(env) = flow.take(NORMAL) {
                    for (branch, allowed) in [
                        (&t.then_expr, !matches!(t.condition, Expr::Bool(false, ..))),
                        (&t.else_expr, !matches!(t.condition, Expr::Bool(true, ..))),
                    ] {
                        if allowed {
                            let (other, value) = self.expression(branch, env.clone());
                            values.push(value);
                            flow.merge(other, &mut self.graph);
                        }
                    }
                }
                (flow, self.node(values, None, None))
            }
            Expr::Match(m) => {
                let (mut flow, scrutinee) = self.expression(&m.scrutinee, env);
                let mut pending = flow.take(NORMAL);
                let mut values = Vec::new();
                for arm in &m.arms {
                    let Some(mut env) = pending.take() else {
                        break;
                    };
                    let pattern = pattern_matches(&arm.pattern, &m.scrutinee);
                    if pattern == Some(false) {
                        pending = Some(env);
                        continue;
                    }
                    let mut failed = Join::default();
                    if pattern != Some(true) {
                        failed.add(env.clone(), &mut self.graph);
                    }
                    let mut locals = Vec::new();
                    Self::bind_pattern(&arm.pattern, scrutinee, &mut env, &mut locals);
                    let mut branch = Flow::path(NORMAL, env);
                    for guard in &arm.guard {
                        if let Some(env) = branch.take(NORMAL) {
                            let (mut guard_flow, value) = self.expression(guard.expr(), env);
                            if let Some(mut env) = guard_flow.take(NORMAL) {
                                let outcome = guard_matches(guard);
                                if outcome != Some(true) {
                                    let mut failure = env.clone();
                                    for key in &locals {
                                        failure.remove(key);
                                    }
                                    failed.add(failure, &mut self.graph);
                                }
                                if outcome != Some(false) {
                                    if let Some(pattern) = guard.pattern() {
                                        Self::bind_pattern(pattern, value, &mut env, &mut locals);
                                    }
                                    branch.0[NORMAL].first = Some(env);
                                }
                            }
                            branch.merge(guard_flow, &mut self.graph);
                        }
                    }
                    if let Some(env) = branch.take(NORMAL) {
                        let (other, value) = match &arm.body {
                            MatchBody::Expr(expr) => self.expression(expr, env),
                            MatchBody::Block(block) => (self.block(block, env), 0),
                        };
                        values.push(value);
                        branch.merge(other, &mut self.graph);
                    }
                    Self::remove_locals(&mut branch, &locals);
                    flow.merge(branch, &mut self.graph);
                    pending = failed.finish();
                }
                (flow, self.node(values, None, None))
            }
            Expr::Select(s) => {
                // All select operands are evaluated before choosing a body.
                let mut flow = Flow::path(NORMAL, env);
                for case in &s.cases {
                    let operands = match &case.kind {
                        SelectCaseKind::Recv { channel, .. } => vec![channel],
                        SelectCaseKind::Send { channel, value } => vec![channel, value],
                        SelectCaseKind::Timeout { millis } => vec![millis],
                        SelectCaseKind::Join { task, .. } => vec![task],
                        SelectCaseKind::Default => Vec::new(),
                    };
                    for operand in operands {
                        if let Some(env) = flow.take(NORMAL) {
                            flow.merge(self.expression(operand, env).0, &mut self.graph);
                        }
                    }
                }
                if let Some(env) = flow.take(NORMAL) {
                    for case in &s.cases {
                        flow.merge(self.block(&case.body, env.clone()), &mut self.graph);
                    }
                }
                (flow, 0)
            }
            _ => (self.children(AstEvent::Expr(expr), env), 0),
        };
        self.remember(expr.span(), value);
        (flow, value)
    }
}

impl TypeChecker {
    pub(super) fn refine_capture_flow(&mut self, root: AstEvent<'_>, errors_start: usize) {
        if !self.errors[errors_start..]
            .iter()
            .any(|d| matches!(d.code, ErrorCode::E2402 | ErrorCode::E2408))
        {
            return;
        }
        let targets = self.errors[errors_start..]
            .iter()
            .filter(|d| matches!(d.code, ErrorCode::E2402 | ErrorCode::E2408))
            .filter_map(Diagnostic::primary_span)
            .collect();
        let mut analysis = Analysis::new(&self.local.capture_flow, root, targets);
        let mut env = Env::new();
        for &span in &self.local.current_capture_declarations {
            if let Some(origin) = self.local.capture_flow.origins.get(&span) {
                let value = analysis.node(Vec::new(), Some(origin.label.clone()), None);
                env.insert(span, value);
            }
        }
        match root {
            AstEvent::Block(block) => {
                analysis.block(block, env);
            }
            AstEvent::Expr(expr) => {
                analysis.expression(expr, env);
            }
            _ => unreachable!(),
        }
        let resolved = analysis.resolved();
        for diagnostic in &mut self.errors[errors_start..] {
            if !matches!(diagnostic.code, ErrorCode::E2402 | ErrorCode::E2408) {
                continue;
            }
            let Some(primary) = diagnostic.primary_span() else {
                continue;
            };
            let Some(cause) = resolved.get(&primary) else {
                continue;
            };
            diagnostic
                .labels
                .retain(|label| !label.message.starts_with("captured variable `"));
            diagnostic
                .notes
                .retain(|note| !note.starts_with("captured variable `"));
            if let Some(cause) = cause {
                if cause.span.file_id == primary.file_id {
                    diagnostic.labels.push((**cause).clone());
                } else {
                    diagnostic.notes.push(cause.message.clone());
                }
            }
        }
        self.local.capture_flow.resolved.extend(resolved);
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static FLOW_STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static FLOW_NODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static FLOW_SUMMARY_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static FLOW_DIFFS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
