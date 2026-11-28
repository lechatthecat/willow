//! Source-level local retention for the early Send diagnostic. Binding identity
//! is a borrowed declaration name address (saved operands use ExprId), never
//! a source coordinate or variable spelling. Keys never leave this body check.
//! Build the CFG once, then propagate sparse (node, binding) facts backwards.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::diagnostics::Span;
use crate::parser::ast::*;
use crate::parser::iter::{AstEvent, AstWalk};
use crate::semantic::ids::FunctionId;
use crate::semantic::intrinsics::{BuiltinCalls, Intrinsic};

use super::LambdaCapture;

/// These keys live only during one body check. A binding's borrowed name
/// address distinguishes declarations even when generated nodes share spans.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Key {
    Binding(usize),
    Value(ExprId),
    DeferredCallee(ExprId),
}
impl Key {
    pub fn binding(name: &str) -> Self {
        Self::Binding(name.as_ptr() as usize)
    }
}

#[derive(Default)]
struct Names {
    reads: HashMap<ExprId, Vec<usize>>,
    writes: HashMap<usize, usize>,
    declarations: HashMap<Key, usize>,
    temporaries: HashMap<ExprId, usize>,
    value_spans: HashMap<ExprId, Span>,
    count: usize,
    bindings: HashMap<usize, usize>,
}

impl Names {
    fn resolve(
        body: &Block,
        captures: &HashMap<ExprId, Vec<LambdaCapture>>,
        patterns: &HashMap<PatternId, Pattern>,
    ) -> Self {
        let mut out = Self::default();
        let mut names: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut scopes: Vec<Vec<&str>> = Vec::new();
        let mut next = 0;
        let mut walk = AstWalk::new(AstEvent::Block(body));
        while let Some(event) = walk.next() {
            match event {
                AstEvent::EnterScope => scopes.push(Vec::new()),
                AstEvent::ExitScope => {
                    for name in scopes.pop().unwrap() {
                        names.get_mut(name).unwrap().pop();
                    }
                }
                AstEvent::Pattern(pattern) => {
                    // Bare fieldless variants parse as bindings. Do not let
                    // their spelling shadow an outer local in the arm body:
                    // bind only the names the checker's reading binds.
                    // Resolutions are keyed by root patterns, so the nested
                    // sub-pattern events are replaced, never visited.
                    let resolved = patterns.get(&pattern.id()).unwrap_or(pattern);
                    walk.skip_children();
                    for name in pattern.source_bindings(resolved).into_iter().rev() {
                        walk.push(AstEvent::Bind(name));
                    }
                }
                AstEvent::Bind(name) if name != "_" => {
                    let id = next;
                    next += 1;
                    names.entry(name).or_default().push(id);
                    scopes.last_mut().unwrap().push(name);
                    out.bindings.insert(name.as_ptr() as usize, id);
                    out.declarations.insert(Key::binding(name), id);
                }
                AstEvent::Stmt(Stmt::Assign(s)) => {
                    if let Some(id) = names.get(s.name.as_str()).and_then(|s| s.last()) {
                        out.writes.insert(s as *const _ as usize, *id);
                    }
                }
                AstEvent::Expr(expr) => {
                    let mut read = |name: &str| {
                        if let Some(id) = names.get(name).and_then(|s| s.last()) {
                            out.reads.entry(expr.id()).or_default().push(*id);
                        }
                    };
                    match expr {
                        Expr::Var(name, ..) => read(name),
                        Expr::Call(c) => read(&c.callee),
                        Expr::Lambda(l) => {
                            for capture in captures.get(&l.id).into_iter().flatten() {
                                read(&capture.name);
                            }
                            walk.skip_children();
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        out.count = next;
        out
    }

    fn temporary(&mut self, expr: &Expr) -> usize {
        self.value_spans.insert(expr.id(), expr.span());
        *self.temporaries.entry(expr.id()).or_insert_with(|| {
            let id = self.count;
            self.count += 1;
            self.declarations.insert(Key::Value(expr.id()), id);
            id
        })
    }
}

#[derive(Default, Clone)]
struct Effect {
    reads: Vec<usize>,
    kill: HashSet<usize>,
    returns: bool,
    unwinds: bool,
    recovers: bool,
    unwind_kill: HashSet<usize>,
}

#[derive(Default, Clone)]
struct Node {
    effect: Option<Arc<Effect>>,
    unwind_effect: bool,
    unwind_terminal: bool,
    recovers: bool,
    edges: Vec<usize>,
    reads: Vec<usize>,
    kill: HashSet<usize>,
    suspend: Option<Span>,
}

impl Node {
    fn reads(&self) -> impl Iterator<Item = &usize> {
        self.reads
            .iter()
            .chain(self.effect.iter().flat_map(|e| e.reads.iter()))
    }
    fn kills(&self, value: &usize) -> bool {
        self.kill.contains(value)
            || self.effect.as_ref().is_some_and(|e| {
                if self.unwind_effect {
                    &e.unwind_kill
                } else {
                    &e.kill
                }
                .contains(value)
            })
    }
}

#[derive(Clone, Copy)]
enum Input<'a> {
    Block(&'a Block),
    Stmt(&'a Stmt),
    Expr(&'a Expr),
    DeferredExpr(&'a Expr),
}

#[derive(Clone, Copy, Default)]
struct Flow {
    break_to: usize,
    continue_to: usize,
    cleanup: usize,
    loop_cleanup: usize,
}

struct Work<'a> {
    input: Input<'a>,
    node: usize,
    next: usize,
    flow: Flow,
}

struct Cleanup {
    previous: usize,
    effect: Arc<Effect>,
    recovery: Option<usize>,
}

// Normal completion is node 0; uncaught unwinding is node 1. Keeping
// separate terminals preserves outcome-specific assignment kills in summaries.
fn terminals() -> Vec<Node> {
    vec![
        Node::default(),
        Node {
            unwind_terminal: true,
            ..Node::default()
        },
    ]
}

struct Graph<'a> {
    names: Names,
    summarizing: bool,
    types: &'a HashMap<ExprId, Type>,
    builtin_calls: &'a BuiltinCalls,
    resolved_calls: &'a HashMap<ExprId, Option<FunctionId>>,
    exhaustive_matches: &'a HashSet<ExprId>,
    patterns: &'a HashMap<PatternId, Pattern>,
    nodes: Vec<Node>,
    work: Vec<Work<'a>>,
    cleanups: Vec<Cleanup>,
    exits: HashMap<(usize, usize, usize), usize>,
    effects: HashMap<usize, Arc<Effect>>,
    #[cfg(test)]
    summary_counts: (usize, usize),
}

#[derive(Default)]
pub(super) struct Retention {
    pub locals: HashMap<Key, Span>,
    pub has_suspend: bool,
    pub values: Vec<(ExprId, Span, Span)>,
    #[cfg(test)]
    pub nodes: usize,
    #[cfg(test)]
    pub facts: usize,
    #[cfg(test)]
    pub edge_visits: usize,
    #[cfg(test)]
    pub summary_counts: (usize, usize),
}

pub(super) fn analyze(
    body: &Block,
    captures: &HashMap<ExprId, Vec<LambdaCapture>>,
    types: &HashMap<ExprId, Type>,
    builtin_calls: &BuiltinCalls,
    resolved_calls: &HashMap<ExprId, Option<FunctionId>>,
    exhaustive_matches: &HashSet<ExprId>,
    patterns: &HashMap<PatternId, Pattern>,
) -> Retention {
    let mut graph = Graph {
        names: Names::resolve(body, captures, patterns),
        summarizing: false,
        types,
        builtin_calls,
        resolved_calls,
        exhaustive_matches,
        patterns,
        nodes: terminals(),
        work: Vec::new(),
        cleanups: vec![Cleanup {
            previous: 0,
            effect: Arc::new(Effect::default()),
            recovery: None,
        }],
        exits: HashMap::new(),
        effects: HashMap::new(),
        #[cfg(test)]
        summary_counts: (0, 0),
    };
    let mut cleanup_bodies = Vec::new();
    let mut walk = AstWalk::new(AstEvent::Block(body));
    while let Some(event) = walk.next() {
        match event {
            AstEvent::Lambda(_) => walk.skip_children(),
            AstEvent::Stmt(Stmt::For(s)) => {
                graph.names.temporary(&s.iterable);
            }
            AstEvent::Stmt(Stmt::Lock(s)) => {
                graph.names.temporary(&s.target);
                for member in &s.additional {
                    graph.names.temporary(&member.target);
                }
            }
            AstEvent::Expr(Expr::Match(m)) => {
                graph.names.temporary(&m.scrutinee);
            }
            AstEvent::Stmt(Stmt::Defer(d)) => {
                let root = match &d.body {
                    DeferBody::Block(b) => Some(Input::Block(b)),
                    DeferBody::Expr(e @ Expr::Match(_)) => Some(Input::Expr(e)),
                    _ => None,
                };
                if let Some(root) = root {
                    cleanup_bodies.push((&d.body as *const _ as usize, root));
                }

                if let DeferBody::Expr(Expr::Call(c)) = &d.body {
                    let id = graph.names.count;
                    graph.names.count += 1;
                    graph
                        .names
                        .declarations
                        .insert(Key::DeferredCallee(c.id), id);
                }
                for operand in deferred_operands(&d.body) {
                    graph.names.temporary(operand);
                }
            }
            AstEvent::Expr(Expr::MethodCall(c))
                if builtin_calls.candidate(&c.id) == Some(&Intrinsic::ChannelSend) =>
            {
                if let Some(arg) = c.args.first() {
                    graph.names.temporary(&arg.expr);
                }
            }
            AstEvent::Expr(Expr::Select(s)) => {
                for case in &s.cases {
                    if let SelectCaseKind::Send { value, .. } = &case.kind {
                        graph.names.temporary(value);
                    }
                }
            }
            _ => {}
        }
    }
    // Postorder summary construction also handles nested defers without Rust
    // recursion or repeated scans of enclosing bodies.
    for (key, root) in cleanup_bodies.into_iter().rev() {
        let effect = graph.cleanup_effect(root);
        graph.effects.insert(key, effect);
    }
    let entry = graph.schedule(Input::Block(body), 0, Flow::default());
    graph.build();
    graph.solve(entry)
}

fn deferred_operands(body: &DeferBody) -> Vec<&Expr> {
    match body {
        DeferBody::Expr(Expr::Call(c)) => c.args.iter().map(|a| &a.expr).collect(),
        DeferBody::Expr(Expr::StaticCall(c)) => c.args.iter().map(|a| &a.expr).collect(),
        DeferBody::Expr(Expr::MethodCall(c)) => std::iter::once(&c.object)
            .chain(c.args.iter().map(|a| &a.expr))
            .collect(),
        DeferBody::Expr(Expr::Print(e, ..)) => vec![e],
        _ => Vec::new(),
    }
}

impl<'a> Graph<'a> {
    // Match the constructor flow analysis' conservative exceptional-operation
    // classification. Children get their own edges at their evaluation point.
    fn may_panic(&self, expr: &Expr) -> bool {
        let output = match expr {
            Expr::Print(value, ..) => Some(value.as_ref()),
            Expr::Call(_) | Expr::StaticCall(_)
                if self
                    .resolved_calls
                    .get(&expr.id())
                    .copied()
                    .flatten()
                    .is_some_and(|target| {
                        target.is_method_of("$output")
                            && matches!(target.name().as_ref(), "print" | "println")
                    }) =>
            {
                match expr {
                    Expr::Call(call) => call.args.first().map(|arg| &arg.expr),
                    Expr::StaticCall(call) => call.args.first().map(|arg| &arg.expr),
                    _ => unreachable!(),
                }
            }
            _ => None,
        };
        if let Some(value) = output {
            return !matches!(
                self.types.get(&value.id()),
                Some(
                    Type::I8
                        | Type::I16
                        | Type::I32
                        | Type::U16
                        | Type::U32
                        | Type::U64
                        | Type::Char
                        | Type::U8
                        | Type::I64
                        | Type::F64
                        | Type::Bool
                        | Type::String
                )
            );
        }
        match expr {
            Expr::Call(c) => c.callee != "recover",
            Expr::MethodCall(_)
            | Expr::StaticCall(_)
            | Expr::New(_)
            | Expr::Index(..)
            | Expr::Unary(_)
            | Expr::FieldAccess(..)
            | Expr::ArrayLiteral(..)
            | Expr::ObjectLiteral(_)
            | Expr::Await(_)
            | Expr::Select(_) => true,
            Expr::Binary(b) => !matches!(b.op, BinOp::And | BinOp::Or),
            _ => false,
        }
    }

    /// Memoize continuation-specific links, sharing each cleanup's transfer
    /// sets rather than cloning its body or its sets for every exit.
    fn exit(&mut self, mut cleanup: usize, stop: usize, next: usize) -> usize {
        let mut pending = Vec::new();
        let mut tail = next;
        while cleanup != stop {
            if let Some(&cached) = self.exits.get(&(cleanup, stop, next)) {
                tail = cached;
                break;
            }
            pending.push(cleanup);
            cleanup = self.cleanups[cleanup].previous;
        }
        for cleanup in pending.into_iter().rev() {
            let effect = Arc::clone(&self.cleanups[cleanup].effect);
            // A panic during cleanup still executes older cleanups. Reuse a
            // single unwind suffix per registration prefix; recursion is at
            // most one normal-to-unwind transition, independent of depth.
            let unwinding = stop == 0 && next == 1;
            let mut edges = Vec::new();
            if effect.returns {
                let node = self.node(tail);
                self.nodes[node].effect = Some(Arc::clone(&effect));
                edges.push(node);
            }
            if effect.unwinds {
                let unwind = if unwinding {
                    tail
                } else {
                    self.exit(self.cleanups[cleanup].previous, 0, 1)
                };
                let node = self.node(unwind);
                self.nodes[node].effect = Some(Arc::clone(&effect));
                self.nodes[node].unwind_effect = true;
                edges.push(node);
            }
            if unwinding && let Some(recovery) = self.cleanups[cleanup].recovery {
                edges.push(recovery);
            }
            let node = if edges.len() == 1 {
                edges[0]
            } else {
                let node = self.node(tail);
                self.nodes[node].edges = edges;
                if self.nodes[node].edges.is_empty() {
                    self.nodes[node].effect = Some(effect);
                }
                node
            };
            self.exits.insert((cleanup, stop, next), node);
            tail = node;
        }
        tail
    }

    fn cleanup_effect(&mut self, root: Input<'a>) -> Arc<Effect> {
        // Isolate this cleanup's CFG; identifiers remain in the shared Names
        // table, but continuations and work items belong to this graph only.
        let saved_summarizing = std::mem::replace(&mut self.summarizing, true);
        let saved_nodes = std::mem::replace(&mut self.nodes, terminals());
        let saved_work = std::mem::take(&mut self.work);
        let saved_exits = std::mem::take(&mut self.exits);
        let root = self.schedule(root, 0, Flow::default());
        self.build();
        let mut seen = HashSet::new();
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            if seen.insert(node) {
                pending.extend(self.nodes[node].edges.iter().copied());
            }
        }
        // Preserve lowering's syntactic recovery capability, including nested
        // defers, without rescanning their AST bodies for every ancestor.
        let recovers = self.nodes.iter().any(|node| {
            node.recovers || node.effect.as_ref().is_some_and(|effect| effect.recovers)
        });
        let returns = seen.contains(&0);
        let unwinds = seen.contains(&1);
        let root = self.compact(root);
        let unwind_terminal = self.nodes.iter().position(|node| node.unwind_terminal);
        #[cfg(test)]
        {
            self.summary_counts.0 += self.nodes.len();
        }
        let mut predecessors = vec![Vec::new(); self.nodes.len()];
        let mut live = vec![HashSet::new(); self.nodes.len()];
        let mut work = Vec::new();
        let mut candidates = HashSet::new();
        for (id, node) in self.nodes.iter().enumerate() {
            for &edge in &node.edges {
                predecessors[edge].push(id);
            }
            candidates.extend(node.kill.iter().copied());
            if let Some(effect) = &node.effect {
                candidates.extend(effect.kill.iter().copied());
                candidates.extend(effect.unwind_kill.iter().copied());
            }
            for &read in node.reads() {
                if live[id].insert(read) {
                    work.push((id, read));
                }
            }
        }
        while let Some((node, value)) = work.pop() {
            for &pred in &predecessors[node] {
                #[cfg(test)]
                {
                    self.summary_counts.1 += 1;
                }
                if !self.nodes[pred].kills(&value) && live[pred].insert(value) {
                    work.push((pred, value));
                }
            }
        }
        let mut kills = [HashSet::new(), HashSet::new()];
        for (index, terminal) in [returns.then_some(0), unwind_terminal]
            .into_iter()
            .enumerate()
        {
            let Some(terminal) = terminal else { continue };
            // For each assigned value, find whether any path to this outcome
            // leaves its incoming value intact. Straight-line regions have
            // already been summarized, avoiding expression-count multiplication.
            for &value in &candidates {
                let mut seen = HashSet::new();
                let mut work = vec![root];
                let mut survives = false;
                while let Some(node) = work.pop() {
                    #[cfg(test)]
                    {
                        self.summary_counts.1 += 1;
                    }
                    if self.nodes[node].kills(&value) || !seen.insert(node) {
                        continue;
                    }
                    if node == terminal {
                        survives = true;
                        break;
                    }
                    work.extend(self.nodes[node].edges.iter().copied());
                }
                if !survives {
                    kills[index].insert(value);
                }
            }
        }
        let [kill, unwind_kill] = kills;
        let effect = Arc::new(Effect {
            reads: live[root].iter().copied().collect(),
            kill,
            returns,
            unwinds,
            recovers,
            unwind_kill,
        });
        self.summarizing = saved_summarizing;
        self.nodes = saved_nodes;
        self.work = saved_work;
        self.exits = saved_exits;
        effect
    }

    fn node(&mut self, next: usize) -> usize {
        let id = self.nodes.len();
        self.nodes.push(Node {
            edges: vec![next],
            ..Node::default()
        });
        id
    }

    fn schedule(&mut self, input: Input<'a>, next: usize, flow: Flow) -> usize {
        let node = self.node(next);
        self.work.push(Work {
            input,
            node,
            next,
            flow,
        });
        node
    }

    fn binding(&mut self, name: &str, next: usize) -> usize {
        let node = self.node(next);
        self.nodes[node]
            .kill
            .extend(self.names.bindings.get(&(name.as_ptr() as usize)).copied());
        node
    }

    fn build(&mut self) {
        while let Some(Work {
            input,
            node,
            next,
            flow,
        }) = self.work.pop()
        {
            // A Never expression still evaluates its operands, but cannot
            // continue normally. Deferred calls execute only at cleanup.
            let next = if let Input::Expr(expr) = input
                && self.types.get(&expr.id()) == Some(&Type::Never)
            {
                self.exit(flow.cleanup, 0, 1)
            } else {
                next
            };
            // A saved value is defined after evaluating its expression, so
            // an await in its initializer cannot retain an uninitialized slot.
            let next = if let Input::Expr(expr) = input
                && let Some(&temporary) = self.names.temporaries.get(&expr.id())
            {
                let kill = self.node(next);
                self.nodes[kill].kill.insert(temporary);
                kill
            } else {
                next
            };
            // The operation can unwind only after evaluating its operands.
            // Its result slot and following assignment are defined only along
            // the normal successor. Deferred operations execute at cleanup.
            let next = if let Input::Expr(expr) = input
                && self.types.get(&expr.id()) != Some(&Type::Never)
                && self.may_panic(expr)
                && !matches!(expr, Expr::Select(_))
                // With no cleanup/recovery, an uncaught unwind has no reads
                // and cannot affect main-body liveness. Summaries need the
                // outcome even without their own registered cleanup.
                && (self.summarizing || flow.cleanup != 0)
            {
                let branch = self.node(next);
                let unwind = self.exit(flow.cleanup, 0, 1);
                self.nodes[branch].edges.push(unwind);
                branch
            } else {
                next
            };
            let next = if matches!(
                input,
                Input::Stmt(Stmt::IndexAssign(_) | Stmt::FieldAssign(_) | Stmt::SuperInit(_))
            ) && (self.summarizing || flow.cleanup != 0)
            {
                let branch = self.node(next);
                let unwind = self.exit(flow.cleanup, 0, 1);
                self.nodes[branch].edges.push(unwind);
                branch
            } else {
                next
            };
            let entry = match input {
                Input::Block(block) => self.build_block(block, next, flow),
                Input::Stmt(stmt) => self.build_statement(stmt, next, flow),
                Input::Expr(expr) | Input::DeferredExpr(expr) => {
                    self.build_expression(expr, node, next, flow)
                }
            };
            self.nodes[node].edges = vec![entry];
        }
    }

    fn build_block(&mut self, block: &'a Block, next: usize, flow: Flow) -> usize {
        let mut cleanup = flow.cleanup;
        // Match lowering's lexical recovery boundary. A recover
        // site may resume after this scope, never after panic().
        if block.stmts.iter().any(|stmt| {
            let Stmt::Defer(d) = stmt else { return false };
            self.effects
                .get(&(&d.body as *const _ as usize))
                .is_some_and(|e| e.recovers)
                || matches!(&d.body, DeferBody::Expr(Expr::Call(c)) if c.callee == "recover")
        }) {
            self.cleanups.push(Cleanup {
                previous: cleanup,
                effect: Arc::new(Effect {
                    returns: true,
                    ..Effect::default()
                }),
                recovery: Some(next),
            });
            cleanup = self.cleanups.len() - 1;
        }
        let mut flows = Vec::with_capacity(block.stmts.len());
        for stmt in &block.stmts {
            flows.push(Flow { cleanup, ..flow });
            if let Stmt::Defer(d) = stmt {
                let root = match &d.body {
                    DeferBody::Block(b) => Some(Input::Block(b)),
                    DeferBody::Expr(e @ Expr::Match(_)) => Some(Input::Expr(e)),
                    _ => None,
                };
                let effect = if root.is_some() {
                    Arc::clone(&self.effects[&(&d.body as *const _ as usize)])
                } else {
                    let mut reads: Vec<_> = deferred_operands(&d.body)
                        .iter()
                        .map(|e| self.names.temporaries[&e.id()])
                        .collect();
                    if let DeferBody::Expr(Expr::Call(c)) = &d.body {
                        reads.push(self.names.declarations[&Key::DeferredCallee(c.id)]);
                    }
                    let unwinds = matches!(&d.body, DeferBody::Expr(e)
                if self.types.get(&e.id()) == Some(&Type::Never));
                    Arc::new(Effect {
                        reads,
                        returns: !unwinds,
                        unwinds: unwinds
                            || matches!(&d.body, DeferBody::Expr(e) if self.may_panic(e)),
                        recovers: matches!(&d.body, DeferBody::Expr(Expr::Call(c)) if c.callee == "recover"),
                        ..Effect::default()
                    })
                };
                self.cleanups.push(Cleanup {
                    previous: cleanup,
                    effect,
                    recovery: None,
                });
                cleanup = self.cleanups.len() - 1;
            }
        }
        let mut tail = self.exit(cleanup, flow.cleanup, next);
        for (stmt, flow) in block.stmts.iter().zip(flows).rev() {
            tail = self.schedule(Input::Stmt(stmt), tail, flow);
        }
        tail
    }

    fn build_statement(&mut self, stmt: &'a Stmt, next: usize, flow: Flow) -> usize {
        match stmt {
            Stmt::Let(s) => {
                let kill = self.binding(&s.name, next);
                self.schedule(Input::Expr(&s.init), kill, flow)
            }
            Stmt::Assign(s) => {
                let kill = self.node(next);
                self.nodes[kill]
                    .kill
                    .extend(self.names.writes.get(&(s as *const _ as usize)).copied());
                self.schedule(Input::Expr(&s.value), kill, flow)
            }
            Stmt::If(s) => {
                let left = self.schedule(Input::Block(&s.then_block), next, flow);
                let right = s
                    .else_block
                    .as_ref()
                    .map_or(next, |b| self.schedule(Input::Block(b), next, flow));
                let branch = self.node(left);
                self.nodes[branch].edges.push(right);
                if let Expr::Bool(value, ..) = &s.cond {
                    self.nodes[branch].edges = vec![if *value { left } else { right }];
                }
                self.schedule(Input::Expr(&s.cond), branch, flow)
            }
            Stmt::While(s) => {
                let branch = self.node(next);
                let cond = self.schedule(Input::Expr(&s.cond), branch, flow);
                let body = self.schedule(
                    Input::Block(&s.body),
                    cond,
                    Flow {
                        break_to: next,
                        continue_to: cond,
                        loop_cleanup: flow.cleanup,
                        ..flow
                    },
                );
                self.nodes[branch].edges.push(body);
                if let Expr::Bool(value, ..) = &s.cond {
                    self.nodes[branch].edges = vec![if *value { body } else { next }];
                }
                cond
            }
            Stmt::For(s) => {
                let branch = self.node(next);
                self.nodes[branch]
                    .reads
                    .push(self.names.temporaries[&s.iterable.id()]);
                let body = self.schedule(
                    Input::Block(&s.body),
                    branch,
                    Flow {
                        break_to: next,
                        continue_to: branch,
                        loop_cleanup: flow.cleanup,
                        ..flow
                    },
                );
                let binding = self.binding(&s.name, body);
                self.nodes[branch].edges.push(binding);
                self.schedule(Input::Expr(&s.iterable), branch, flow)
            }
            Stmt::Break(_) => self.exit(flow.cleanup, flow.loop_cleanup, flow.break_to),
            Stmt::Continue(_) => self.exit(flow.cleanup, flow.loop_cleanup, flow.continue_to),
            Stmt::Return(s) => {
                let exit = self.exit(flow.cleanup, 0, 0);
                s.value
                    .as_ref()
                    .map_or(exit, |e| self.schedule(Input::Expr(e), exit, flow))
            }
            Stmt::Lock(s) => {
                let release = self.node(next);
                self.nodes[release]
                    .reads
                    .push(self.names.temporaries[&s.target.id()]);
                for member in &s.additional {
                    self.nodes[release]
                        .reads
                        .push(self.names.temporaries[&member.target.id()]);
                }
                let body = self.schedule(Input::Block(&s.body), release, flow);
                let mut binding = self.binding(&s.binding, body);
                for member in s.additional.iter().rev() {
                    binding = self.binding(&member.binding, binding);
                }
                let wait = self.node(binding);
                self.nodes[wait].suspend = Some(s.span);
                let cleanup = self.exit(flow.cleanup, 0, 0);
                self.nodes[wait].edges.push(cleanup);
                let unwind = self.exit(flow.cleanup, 0, 1);
                self.nodes[wait].edges.push(unwind);
                let mut entry = wait;
                for member in s.additional.iter().rev() {
                    entry = self.schedule(Input::Expr(&member.target), entry, flow);
                }
                self.schedule(Input::Expr(&s.target), entry, flow)
            }
            Stmt::Defer(d) => match &d.body {
                DeferBody::Block(_) | DeferBody::Expr(Expr::Match(_)) => next,
                DeferBody::Expr(e) => {
                    let entry = self.schedule(Input::DeferredExpr(e), next, flow);
                    if let Expr::Call(c) = e {
                        // Snapshot before argument evaluation. Later assignment
                        // to the source local cannot kill this saved callable.
                        let capture = self.node(entry);
                        self.nodes[capture]
                            .kill
                            .insert(self.names.declarations[&Key::DeferredCallee(c.id)]);
                        capture
                    } else {
                        entry
                    }
                }
            },
            _ => {
                let mut children = Vec::new();
                let mut walk = AstWalk::new(AstEvent::Stmt(stmt));
                walk.next();
                while let Some(event) = walk.next() {
                    if let AstEvent::Expr(e) = event {
                        children.push(e);
                        walk.skip_children();
                    }
                }
                self.expressions(children, next, flow)
            }
        }
    }

    fn build_match(&mut self, e: &'a MatchExpr, next: usize, flow: Flow) -> usize {
        let mut following = if self.exhaustive_matches.contains(&e.id) {
            let unreachable = self.node(0);
            self.nodes[unreachable].edges.clear();
            unreachable
        } else {
            next
        };
        for arm in e.arms.iter().rev() {
            let pattern = self.patterns.get(&arm.pattern.id()).unwrap_or(&arm.pattern);
            let input = match &arm.body {
                MatchBody::Expr(e) => Input::Expr(e),
                MatchBody::Block(b) => Input::Block(b),
            };
            let body = self.schedule(input, next, flow);
            // Guard steps run in order after the pattern
            // binds; each failing step resumes matching.
            let mut matched = body;
            for guard in arm.guard.iter().rev() {
                matched = match guard {
                    Guard::If(guard) => {
                        let branch = self.node(matched);
                        self.nodes[branch].edges.push(following);
                        if let Expr::Bool(value, ..) = guard {
                            self.nodes[branch].edges =
                                vec![if *value { matched } else { following }];
                        }
                        self.schedule(Input::Expr(guard), branch, flow)
                    }
                    Guard::Let(step) => {
                        let step_pattern = self
                            .patterns
                            .get(&step.pattern.id())
                            .unwrap_or(&step.pattern);
                        let mut bound = matched;
                        for name in step.pattern.source_bindings(step_pattern) {
                            bound = self.binding(name, bound);
                        }
                        let test = self.node(bound);
                        if !step_pattern.is_irrefutable_shape() {
                            self.nodes[test].edges.push(following);
                        }
                        self.schedule(Input::Expr(&step.scrutinee), test, flow)
                    }
                };
            }
            let mut bound = matched;
            for name in arm.pattern.source_bindings(pattern) {
                bound = self.binding(name, bound);
            }
            let dispatch = self.node(bound);
            // A failed guard resumes matching the saved scrutinee;
            // successful bodies only retain their live bindings.
            if !matches!(pattern, Pattern::Wildcard(..)) {
                self.nodes[dispatch]
                    .reads
                    .push(self.names.temporaries[&e.scrutinee.id()]);
            }
            if !pattern.is_irrefutable_shape() {
                self.nodes[dispatch].edges.push(following);
            }
            following = dispatch;
        }
        self.schedule(Input::Expr(&e.scrutinee), following, flow)
    }

    fn build_select(&mut self, e: &'a SelectExpr, next: usize, flow: Flow) -> usize {
        let branch = self.node(next);
        self.nodes[branch].edges.clear();
        // Readiness/commit can panic before any case binding
        // or body executes (for example a cancelled join).
        // Do not put this edge after the case assignment kills.
        if self.summarizing || flow.cleanup != 0 {
            let unwind = self.exit(flow.cleanup, 0, 1);
            self.nodes[branch].edges.push(unwind);
        }
        let cleanup = self.exit(flow.cleanup, 0, 0);
        self.nodes[branch].edges.push(cleanup);
        self.nodes[branch].reads = e
            .cases
            .iter()
            .filter_map(|case| {
                if let SelectCaseKind::Send { value, .. } = &case.kind {
                    Some(self.names.temporaries[&value.id()])
                } else {
                    None
                }
            })
            .collect();
        self.nodes[branch].suspend = (!e
            .cases
            .iter()
            .any(|c| matches!(c.kind, SelectCaseKind::Default)))
        .then_some(e.span);
        let mut operands = Vec::new();
        for case in &e.cases {
            let mut entry = self.schedule(Input::Block(&case.body), next, flow);
            match &case.kind {
                SelectCaseKind::Recv {
                    channel, binding, ..
                } => {
                    operands.push(channel);
                    entry = self.binding(binding, entry);
                }
                SelectCaseKind::Join { task, binding } => {
                    operands.push(task);
                    entry = self.binding(binding, entry);
                }
                SelectCaseKind::Send { channel, value } => {
                    operands.push(channel);
                    operands.push(value);
                }
                SelectCaseKind::Timeout { millis } => operands.push(millis),
                SelectCaseKind::Default => {}
            }
            self.nodes[branch].edges.push(entry);
        }
        self.expressions(operands, branch, flow)
    }

    fn build_expression(&mut self, expr: &'a Expr, node: usize, next: usize, flow: Flow) -> usize {
        self.nodes[node].recovers = matches!(expr, Expr::Call(c) if c.callee == "recover");
        self.nodes[node].reads = self
            .names
            .reads
            .get(&expr.id())
            .cloned()
            .unwrap_or_default();
        match expr {
            Expr::Lambda(_) => next,
            Expr::Await(e) => {
                let wait = self.node(next);
                self.nodes[wait].suspend = Some(e.span);
                let cleanup = self.exit(flow.cleanup, 0, 0);
                self.nodes[wait].edges.push(cleanup);
                self.schedule(Input::Expr(&e.expr), wait, flow)
            }
            Expr::Ternary(e) => {
                let left = self.schedule(Input::Expr(&e.then_expr), next, flow);
                let right = self.schedule(Input::Expr(&e.else_expr), next, flow);
                let branch = self.node(left);
                self.nodes[branch].edges.push(right);
                if let Expr::Bool(value, ..) = &e.condition {
                    self.nodes[branch].edges = vec![if *value { left } else { right }];
                }
                self.schedule(Input::Expr(&e.condition), branch, flow)
            }
            Expr::Binary(e) if matches!(e.op, BinOp::And | BinOp::Or) => {
                let right = self.schedule(Input::Expr(&e.rhs), next, flow);
                let branch = self.node(right);
                self.nodes[branch].edges.push(next);
                if let Expr::Bool(value, ..) = &e.lhs {
                    let evaluate_rhs = *value == matches!(e.op, BinOp::And);
                    self.nodes[branch].edges = vec![if evaluate_rhs { right } else { next }];
                }
                self.schedule(Input::Expr(&e.lhs), branch, flow)
            }
            Expr::TryPropagate(e, ..) => {
                let branch = self.node(next);
                let cleanup = self.exit(flow.cleanup, 0, 0);
                self.nodes[branch].edges.push(cleanup);
                self.schedule(Input::Expr(e), branch, flow)
            }
            Expr::Match(e) => self.build_match(e, next, flow),
            Expr::Select(e) => self.build_select(e, next, flow),
            _ => {
                let mut children = Vec::new();
                let mut walk = AstWalk::new(AstEvent::Expr(expr));
                walk.next();
                while let Some(event) = walk.next() {
                    if let AstEvent::Expr(e) = event {
                        children.push(e);
                        walk.skip_children();
                    }
                }
                // Values of direct variable operands and an indirect
                // callee remain needed while later operands evaluate.
                let finish = self.node(next);
                self.nodes[finish].reads = self.nodes[node].reads.clone();
                for child in &children {
                    if matches!(child, Expr::Var(..)) {
                        self.nodes[finish].reads.extend(
                            self.names
                                .reads
                                .get(&child.id())
                                .into_iter()
                                .flatten()
                                .copied(),
                        );
                    }
                }
                if let Expr::MethodCall(call) = expr
                    && let Some(id) = self.builtin_calls.candidate(&call.id)
                    && id.is_suspension_point()
                {
                    if *id == Intrinsic::ChannelSend
                        && let Some(arg) = call.args.first()
                    {
                        self.nodes[finish]
                            .reads
                            .push(self.names.temporaries[&arg.expr.id()]);
                    }
                    self.nodes[finish].suspend = Some(call.span);
                    let cleanup = self.exit(flow.cleanup, 0, 0);
                    self.nodes[finish].edges.push(cleanup);
                }
                self.expressions(children, finish, flow)
            }
        }
    }

    fn expressions(&mut self, children: Vec<&'a Expr>, next: usize, flow: Flow) -> usize {
        // Each operand's evaluated value survives until the consuming
        // operation, including a field/index/call result with no local name.
        // Record reference-bearing values only; scalar and function-pointer
        // intermediates are unconditionally Send and need no diagnostic fact.
        let consume = self.node(next);
        for child in &children {
            if self.types.get(&child.id()).is_some_and(|ty| {
                !matches!(
                    ty,
                    Type::I64
                        | Type::F64
                        | Type::Bool
                        | Type::Void
                        | Type::Never
                        | Type::String
                        | Type::Fn(..)
                )
            }) {
                let value = self.names.temporary(child);
                self.nodes[consume].reads.push(value);
            }
        }
        children
            .into_iter()
            .rev()
            .fold(consume, |tail, e| self.schedule(Input::Expr(e), tail, flow))
    }

    /// Summarize each straight-line region once. Without this, propagating
    /// each late read through N individual expressions would cost O(N*L)
    /// even for a single basic block with L locals and no control flow.
    fn compact(&mut self, entry: usize) -> usize {
        let mut reachable = vec![false; self.nodes.len()];
        let mut incoming = vec![0; self.nodes.len()];
        let mut leaders = vec![false; self.nodes.len()];
        leaders[entry] = true;
        leaders[0] = true;
        leaders[1] = true;
        let mut work = vec![entry];
        while let Some(node) = work.pop() {
            if std::mem::replace(&mut reachable[node], true) {
                continue;
            }
            leaders[node] |=
                self.nodes[node].suspend.is_some() || self.nodes[node].effect.is_some();
            for &next in &self.nodes[node].edges {
                incoming[next] += 1;
                leaders[next] |= self.nodes[node].edges.len() != 1
                    || self.nodes[node].suspend.is_some()
                    || self.nodes[node].effect.is_some();
                work.push(next);
            }
        }
        for (node, count) in incoming.into_iter().enumerate() {
            leaders[node] |= count != 1;
        }
        let mut mapping = vec![0; self.nodes.len()];
        let mut compact = Vec::new();
        for start in 0..self.nodes.len() {
            if !reachable[start] || !leaders[start] {
                continue;
            }
            let mut merged = Node::default();
            let mut uses = HashSet::new();
            let mut current = start;
            loop {
                mapping[current] = compact.len();
                let node = &self.nodes[current];
                if node.effect.is_some() {
                    merged = node.clone();
                    break;
                }
                uses.extend(
                    node.reads
                        .iter()
                        .filter(|read| !merged.kill.contains(read))
                        .copied(),
                );
                merged.kill.extend(node.kill.iter().copied());
                merged.suspend = merged.suspend.or(node.suspend);
                merged.unwind_terminal |= node.unwind_terminal;
                if node.edges.len() != 1 || leaders[node.edges[0]] {
                    merged.edges = node.edges.clone();
                    break;
                }
                current = node.edges[0];
            }
            if merged.effect.is_none() {
                merged.reads = uses.into_iter().collect();
            }
            compact.push(merged);
        }
        for node in &mut compact {
            for edge in &mut node.edges {
                *edge = mapping[*edge];
            }
        }
        self.nodes = compact;
        mapping[entry]
    }

    fn solve(mut self, entry: usize) -> Retention {
        let entry = self.compact(entry);
        let mut reachable = vec![false; self.nodes.len()];
        let mut pending = vec![entry];
        while let Some(node) = pending.pop() {
            if std::mem::replace(&mut reachable[node], true) {
                continue;
            }
            pending.extend(self.nodes[node].edges.iter().copied());
        }
        let mut predecessors = vec![Vec::new(); self.nodes.len()];
        let mut live = vec![HashSet::new(); self.nodes.len()];
        let mut work = Vec::new();
        let mut retained = HashMap::new();
        let mut result = Retention::default();
        #[cfg(test)]
        {
            result.nodes = self.nodes.len();
            result.summary_counts = self.summary_counts;
        }
        for (id, node) in self
            .nodes
            .iter()
            .enumerate()
            .filter(|(id, _)| reachable[*id])
        {
            result.has_suspend |= node.suspend.is_some();
            for &edge in &node.edges {
                predecessors[edge].push(id);
            }
            for &read in node.reads() {
                if let Some(span) = node.suspend {
                    retained.entry(read).or_insert(span);
                }
                if live[id].insert(read) {
                    work.push((id, read));
                }
            }
        }
        while let Some((node, binding)) = work.pop() {
            #[cfg(test)]
            {
                result.facts += 1;
            }
            for &pred in &predecessors[node] {
                #[cfg(test)]
                {
                    result.edge_visits += 1;
                }
                if self.nodes[pred].kills(&binding) {
                    continue;
                }
                if let Some(span) = self.nodes[pred].suspend {
                    retained.entry(binding).or_insert(span);
                }
                if live[pred].insert(binding) {
                    work.push((pred, binding));
                }
            }
        }
        for (key, binding) in self.names.declarations {
            if let Some(span) = retained.get(&binding) {
                result.locals.insert(key, *span);
                if let Key::Value(expr) = key {
                    result
                        .values
                        .push((expr, self.names.value_spans[&expr], *span));
                }
            }
        }
        result
    }
}

#[cfg(test)]
#[path = "async_retention_tests.rs"]
mod tests;
