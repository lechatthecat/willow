//! Source-level local retention for the early Send diagnostic. Binding identity
//! is a borrowed declaration name address (saved operands use ExprId), never
//! a source coordinate or variable spelling. Keys never leave this body check.
//! Build the CFG once, then propagate sparse (node, binding) facts backwards.
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::diagnostics::Span;
use crate::parser::ast::*;
use crate::parser::iter::{AstEvent, AstWalk};

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
                    // their spelling shadow an outer local in the arm body.
                    let resolved = patterns.get(&pattern.id()).unwrap_or(pattern);
                    if pattern_bindings(resolved).is_empty() {
                        walk.skip_children();
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
    exhaustive_matches: &HashSet<ExprId>,
    patterns: &HashMap<PatternId, Pattern>,
) -> Retention {
    let mut graph = Graph {
        names: Names::resolve(body, captures, patterns),
        summarizing: false,
        types,
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
            AstEvent::Expr(Expr::MethodCall(c)) if c.method == "send" => {
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

pub(super) fn pattern_bindings(pattern: &Pattern) -> &[String] {
    match pattern {
        Pattern::Binding { name, .. } => std::slice::from_ref(name),
        Pattern::ClassDowncast { binding, .. } => std::slice::from_ref(binding),
        Pattern::EnumVariantTuple { bindings, .. } => bindings,
        _ => &[],
    }
}

fn deferred_operands(body: &DeferBody) -> Vec<&Expr> {
    match body {
        DeferBody::Expr(Expr::Call(c)) => c.args.iter().map(|a| &a.expr).collect(),
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
            Expr::Print(value, ..) => !matches!(
                self.types.get(&value.id()),
                Some(Type::I64 | Type::F64 | Type::Bool | Type::String)
            ),
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
                Input::Block(block) => {
                    let mut cleanup = flow.cleanup;
                    // Match lowering's lexical recovery boundary. A recover
                    // site may resume after this scope, never after panic().
                    if block.stmts.iter().any(|stmt| {
                        let Stmt::Defer(d) = stmt else { return false };
                        self.effects.get(&(&d.body as *const _ as usize)).is_some_and(|e| e.recovers)
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
                Input::Stmt(stmt) => match stmt {
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
                    Stmt::Continue(_) => {
                        self.exit(flow.cleanup, flow.loop_cleanup, flow.continue_to)
                    }
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
                        let body = self.schedule(Input::Block(&s.body), release, flow);
                        let binding = self.binding(&s.binding, body);
                        let wait = self.node(binding);
                        self.nodes[wait].suspend = Some(s.span);
                        let cleanup = self.exit(flow.cleanup, 0, 0);
                        self.nodes[wait].edges.push(cleanup);
                        let unwind = self.exit(flow.cleanup, 0, 1);
                        self.nodes[wait].edges.push(unwind);
                        self.schedule(Input::Expr(&s.target), wait, flow)
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
                },
                Input::Expr(expr) | Input::DeferredExpr(expr) => {
                    self.nodes[node].recovers =
                        matches!(expr, Expr::Call(c) if c.callee == "recover");
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
                                self.nodes[branch].edges =
                                    vec![if evaluate_rhs { right } else { next }];
                            }
                            self.schedule(Input::Expr(&e.lhs), branch, flow)
                        }
                        Expr::TryPropagate(e, ..) => {
                            let branch = self.node(next);
                            let cleanup = self.exit(flow.cleanup, 0, 0);
                            self.nodes[branch].edges.push(cleanup);
                            self.schedule(Input::Expr(e), branch, flow)
                        }
                        Expr::Match(e) => {
                            let mut following = if self.exhaustive_matches.contains(&e.id) {
                                let unreachable = self.node(0);
                                self.nodes[unreachable].edges.clear();
                                unreachable
                            } else {
                                next
                            };
                            for arm in e.arms.iter().rev() {
                                let pattern =
                                    self.patterns.get(&arm.pattern.id()).unwrap_or(&arm.pattern);
                                let input = match &arm.body {
                                    MatchBody::Expr(e) => Input::Expr(e),
                                    MatchBody::Block(b) => Input::Block(b),
                                };
                                let body = self.schedule(input, next, flow);
                                let matched = if let Some(guard) = &arm.guard {
                                    let branch = self.node(body);
                                    self.nodes[branch].edges.push(following);
                                    if let Expr::Bool(value, ..) = guard {
                                        self.nodes[branch].edges =
                                            vec![if *value { body } else { following }];
                                    }
                                    self.schedule(Input::Expr(guard), branch, flow)
                                } else {
                                    body
                                };
                                let mut bound = matched;
                                for (name, _) in pattern_bindings(&arm.pattern)
                                    .iter()
                                    .zip(pattern_bindings(pattern))
                                {
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
                                if !matches!(
                                    pattern,
                                    Pattern::Wildcard(..) | Pattern::Binding { .. }
                                ) {
                                    self.nodes[dispatch].edges.push(following);
                                }
                                following = dispatch;
                            }
                            self.schedule(Input::Expr(&e.scrutinee), following, flow)
                        }
                        Expr::Select(e) => {
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
                                && matches!(call.method.as_str(), "send" | "recv" | "recv_opt")
                                && self.types.get(&call.object.id()).is_some_and(|ty| {
                                    crate::semantic::builtin_types::unary_arg(
                                        ty,
                                        crate::semantic::builtin_types::BuiltinTypeId::Channel,
                                    )
                                    .is_some()
                                })
                            {
                                if call.method == "send"
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
            };
            self.nodes[node].edges = vec![entry];
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
mod tests {
    use super::*;
    use crate::diagnostics::{ErrorCode, Severity};
    use crate::semantic::TypeChecker;

    fn parse(source: &str) -> Program {
        let (program, errors) =
            crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap())
                .parse();
        assert!(errors.is_empty(), "{source}: {errors:?}");
        program
    }

    #[test]
    fn async_retention_control_flow_perspectives() {
        let decl = "let mut op: closure(i64) -> i64 = |x| x;";
        let cases = [
            ("no wait", "println(op(1));", false),
            ("unused before wait", "await sleep(0);", false),
            ("dead before wait", "println(op(1)); await sleep(0);", false),
            ("live after wait", "await sleep(0); println(op(1));", true),
            (
                "overwrite",
                "await sleep(0); op = |x| x + 1; println(op(1));",
                false,
            ),
            (
                "read overwrite rhs",
                "await sleep(0); op = |x| x + op(1);",
                true,
            ),
            (
                "overwrite before wait",
                "op = |x| x + 1; await sleep(0); println(op(1));",
                true,
            ),
            (
                "separate branches",
                "if flag { await sleep(0); } else { println(op(1)); }",
                false,
            ),
            (
                "branch live",
                "if flag { await sleep(0); } println(op(1));",
                true,
            ),
            (
                "both kill",
                "await sleep(0); if flag { op = |x| x; } else { op = |x| x+1; } println(op(1));",
                false,
            ),
            (
                "one kill",
                "await sleep(0); if flag { op = |x| x; } println(op(1));",
                true,
            ),
            (
                "return cuts edge",
                "if flag { await sleep(0); return; } println(op(1));",
                false,
            ),
            (
                "unreachable wait",
                "println(op(1)); return; await sleep(0); println(op(2));",
                false,
            ),
            (
                "loop carried",
                "while flag { println(op(1)); await sleep(0); }",
                true,
            ),
            (
                "loop overwrite",
                "while flag { op = |x| x; println(op(1)); await sleep(0); }",
                false,
            ),
            (
                "break",
                "while flag { println(op(1)); await sleep(0); break; }",
                false,
            ),
            (
                "continue",
                "while flag { println(op(1)); await sleep(0); continue; }",
                true,
            ),
            (
                "break continuation",
                "while flag { await sleep(0); break; } println(op(1));",
                true,
            ),
            (
                "shadow",
                "if flag { let op: i64 = 1; await sleep(0); println(op); }",
                false,
            ),
            (
                "outer past shadow",
                "if flag { let op: i64 = 1; await sleep(0); println(op); } println(op(1));",
                true,
            ),
            (
                "late declaration",
                "println(op(1)); await sleep(0); let later: closure(i64) -> i64 = |x| x; println(later(1));",
                false,
            ),
            (
                "closure capture",
                "await sleep(0); let f = |x: i64| op(x); println(f(1));",
                true,
            ),
            (
                "ternary disjoint",
                "let n = flag ? await number() : op(1);",
                false,
            ),
            (
                "defer return",
                "defer { println(op(1)); } await sleep(0); return;",
                true,
            ),
            (
                "defer fallthrough",
                "defer { println(op(1)); } await sleep(0);",
                true,
            ),
            (
                "defer registered later",
                "await sleep(0); op = |x| x; defer { println(op(1)); }",
                false,
            ),
            (
                "defer scope ended",
                "if flag { defer { println(op(1)); } } await sleep(0);",
                false,
            ),
            (
                "defer conditional registration",
                "if flag { defer { println(op(1)); } await sleep(0); }",
                true,
            ),
            (
                "constant dead branch",
                "if false { await sleep(0); println(op(1)); }",
                false,
            ),
            (
                "constant dead loop",
                "while false { await sleep(0); println(op(1)); }",
                false,
            ),
            (
                "indirect call operand",
                "println(op(await number()));",
                true,
            ),
            (
                "for backedge",
                "for i in 0..2 { println(op(i)); await sleep(0); }",
                true,
            ),
        ];
        for (name, body, bad) in cases {
            let source = format!(
                "async fn number() -> i64 {{ return 1; }} async fn check(flag: bool) {{ {decl} {body} }}"
            );
            let program = parse(&source);
            let mut checker = TypeChecker::new();
            checker.set_enforce_send_sync(true);
            checker.check_program(&program);
            let errors: Vec<_> = checker
                .errors
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .collect();
            assert_eq!(
                errors.iter().any(|d| d.code == ErrorCode::E2402),
                bad,
                "{name}: {errors:?}"
            );
            assert!(
                errors.iter().all(|d| d.code == ErrorCode::E2402),
                "{name}: {errors:?}"
            );
        }
    }

    #[test]
    fn async_retention_synthetic_lifetimes() {
        let cases = [
            (
                "for before wait",
                "let ops: Array<closure(i64) -> i64> = [|x| x]; for op in ops { println(op(1)); } await sleep(0);",
                false,
            ),
            (
                "for item dead",
                "let ops: Array<closure(i64) -> i64> = [|x| x]; for op in ops { println(op(1)); await sleep(0); break; }",
                false,
            ),
            (
                "for iterator live",
                "let ops: Array<closure(i64) -> i64> = [|x| x]; for op in ops { println(op(1)); await sleep(0); }",
                true,
            ),
            (
                "for item live",
                "let ops: Array<closure(i64) -> i64> = [|x| x]; for op in ops { await sleep(0); println(op(1)); break; }",
                true,
            ),
            (
                "deferred operand scope ended",
                "if true { let op: closure(i64) -> i64 = |x| x; defer consume(op); } await sleep(0);",
                false,
            ),
            (
                "deferred operand live",
                "let op: closure(i64) -> i64 = |x| x; defer consume(op); await sleep(0);",
                true,
            ),
            (
                "deferred operand no wait",
                "let op: closure(i64) -> i64 = |x| x; defer consume(op);",
                false,
            ),
            (
                "channel implicit wait",
                "let op: closure(i64) -> i64 = |x| x; let ch = Channel<i64>::new(); ch.send(1); println(op(1));",
                true,
            ),
            (
                "channel wait after last use",
                "let op: closure(i64) -> i64 = |x| x; println(op(1)); let ch = Channel<i64>::new(); ch.send(1);",
                false,
            ),
        ];
        for (name, body, bad) in cases {
            let source = format!(
                "import std::collections::Array; fn consume(op: closure(i64) -> i64) {{ println(op(1)); }} async fn check() {{ {body} }}"
            );
            let program = parse(&source);
            let mut checker = TypeChecker::new();
            checker.set_enforce_send_sync(true);
            checker.check_program(&program);
            let errors: Vec<_> = checker
                .errors
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .collect();
            assert_eq!(
                errors.iter().any(|d| d.code == ErrorCode::E2402),
                bad,
                "{name}: {errors:?}"
            );
            assert!(
                errors.iter().all(|d| d.code == ErrorCode::E2402),
                "{name}: {errors:?}"
            );
        }
    }

    #[test]
    fn async_retention_intermediate_operands() {
        let cases = [
            (
                "nested return",
                "consume(identity(op), await number());",
                true,
            ),
            (
                "two nested returns",
                "consume(identity(identity(op)), await number());",
                true,
            ),
            (
                "field",
                "let holder = new Holder(op); consume(holder.value, await number());",
                true,
            ),
            (
                "index",
                "let ops: Array<closure(i64) -> i64> = [op]; consume(ops[0], await number());",
                true,
            ),
            ("literal closure", "consume(|x| x, await number());", true),
            (
                "conditional result",
                "consume(flag ? identity(op) : op, await number());",
                true,
            ),
            (
                "consumed before wait",
                "consume(identity(op), 1); await sleep(0);",
                false,
            ),
            (
                "scalar result",
                "consume_number(evaluate(op), await number());",
                false,
            ),
            (
                "later operand",
                "consume_number(await number(), evaluate(op));",
                true,
            ),
            ("no wait", "consume(identity(op), 1);", false),
            (
                "field consumed",
                "let holder = new Holder(op); consume(holder.value, 1); await sleep(0);",
                false,
            ),
            (
                "index consumed",
                "let ops: Array<closure(i64) -> i64> = [op]; consume(ops[0], 1); await sleep(0);",
                false,
            ),
        ];
        for (name, body, bad) in cases {
            let source = format!(
                r#"
import std::collections::Array;
class Holder {{ pub value: closure(i64) -> i64; }}
fn identity(op: closure(i64) -> i64) -> closure(i64) -> i64 {{ return op; }}
fn evaluate(op: closure(i64) -> i64) -> i64 {{ return op(1); }}
fn consume(op: closure(i64) -> i64, n: i64) {{ println(op(n)); }}
fn consume_number(a: i64, b: i64) {{ println(a + b); }}
async fn number() -> i64 {{ return 1; }}
async fn check(flag: bool) {{ let op: closure(i64) -> i64 = |x| x; {body} }}
"#
            );
            let program = parse(&source);
            let mut checker = TypeChecker::new();
            checker.set_enforce_send_sync(true);
            checker.check_program(&program);
            let errors: Vec<_> = checker
                .errors
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .collect();
            assert_eq!(
                errors.iter().any(|d| d.code == ErrorCode::E2402),
                bad,
                "{name}: {errors:?}"
            );
            assert!(
                errors.iter().all(|d| d.code == ErrorCode::E2402),
                "{name}: {errors:?}"
            );
            if bad && name != "later operand" {
                assert!(
                    errors.iter().any(|d| d
                        .labels
                        .iter()
                        .any(|label| label.message.contains("intermediate value"))),
                    "{name}: {errors:?}"
                );
            }
        }
    }

    #[test]
    fn async_retention_patterns_and_deferred_callees() {
        let cases = [
            (
                "guard discarded fallback",
                "match op { _ if await choose() => {}, _ => {} }",
                false,
            ),
            (
                "binding live",
                "match op { f => { await sleep(0); println(f(1)); } }",
                true,
            ),
            (
                "binding dead",
                "match op { f => { println(f(1)); await sleep(0); } }",
                false,
            ),
            (
                "binding no wait",
                "match op { f => { println(f(1)); } }",
                false,
            ),
            (
                "binding unused",
                "match op { f => { await sleep(0); } }",
                false,
            ),
            (
                "binding shadow",
                "match op { op => { await sleep(0); println(op(1)); } }",
                true,
            ),
            (
                "binding after wait",
                "await sleep(0); op = |x| x; match op { f => { println(f(1)); } }",
                false,
            ),
            (
                "guard fallback",
                "match op { f if await choose() => { println(1); }, g => { println(g(1)); } }",
                true,
            ),
            (
                "guard binding",
                "match op { f if await choose() => { println(f(1)); }, _ => {} }",
                true,
            ),
            (
                "guard no wait",
                "match op { f if flag => { println(f(1)); }, g => { println(g(1)); } } await sleep(0);",
                false,
            ),
            (
                "guard dead body",
                "match op { f if flag => { println(f(1)); await sleep(0); }, g => { println(g(1)); } }",
                false,
            ),
            (
                "payload live",
                "match Boxed::One(op) { Boxed::One(f) => { await sleep(0); println(f(1)); }, Boxed::Empty => {} }",
                true,
            ),
            (
                "payload dead",
                "match Boxed::One(op) { Boxed::One(f) => { println(f(1)); await sleep(0); }, Boxed::Empty => {} }",
                false,
            ),
            (
                "normalized payload",
                "match Boxed::One(op) { One(f) => { await sleep(0); println(f(1)); }, Empty => {} }",
                true,
            ),
            (
                "payload guard fallback",
                "match Boxed::One(op) { One(f) if await choose() => {}, One(g) => { println(g(1)); }, Empty => {} }",
                true,
            ),
            (
                "class binding live",
                "let obj: Value = new Holder(op); match obj { Holder(h) => { await sleep(0); let callback = h.value; println(callback(1)); }, _ => {} }",
                true,
            ),
            (
                "class binding dead",
                "let obj: Value = new Holder(op); match obj { Holder(h) => { let callback = h.value; println(callback(1)); await sleep(0); }, _ => {} }",
                false,
            ),
            ("defer live", "defer op(1); await sleep(0);", true),
            (
                "defer reassigned",
                "defer op(1); op = |x| x + 2; await sleep(0);",
                true,
            ),
            ("defer no wait", "defer op(1);", false),
            (
                "defer no wait reassigned",
                "defer op(1); op = |x| x + 2;",
                false,
            ),
            (
                "defer scope ended",
                "if flag { defer op(1); } await sleep(0);",
                false,
            ),
            (
                "defer conditional",
                "if flag { defer op(1); await sleep(0); }",
                true,
            ),
            (
                "defer after wait",
                "await sleep(0); op = |x| x; defer op(1);",
                false,
            ),
            ("defer direct", "defer plain(1); await sleep(0);", false),
        ];
        for (name, body, bad) in cases {
            let program = parse(&format!(
                r#"
enum Boxed {{ One(closure(i64) -> i64), Empty }}
interface Value {{}}
class Holder implements Value {{ pub value: closure(i64) -> i64; }}
fn plain(x: i64) {{ println(x); }}
async fn choose() -> bool {{ return false; }}
async fn check(flag: bool) {{ let mut op: closure(i64) -> i64 = |x| x; {body} }}
"#
            ));
            let mut checker = TypeChecker::new();
            checker.set_enforce_send_sync(true);
            checker.check_program(&program);
            let errors: Vec<_> = checker
                .errors
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .collect();
            assert_eq!(
                errors.iter().any(|d| d.code == ErrorCode::E2402),
                bad,
                "{name}: {errors:?}"
            );
            assert!(
                errors.iter().all(|d| d.code == ErrorCode::E2402),
                "{name}: {errors:?}"
            );
            if name == "defer reassigned" {
                assert!(
                    errors.iter().any(|d| d
                        .labels
                        .iter()
                        .any(|l| l.message.contains("deferred callee"))),
                    "{errors:?}"
                );
            }
        }
    }

    #[test]
    fn async_retention_pattern_defer_scaling() {
        for n in [16, 32, 64, 128] {
            for deferred in [false, true] {
                let body = if deferred {
                    "if true { let op: closure(i64) -> i64 = |x| x; defer op(1); await sleep(0); }"
                        .repeat(n)
                } else {
                    let arms = "f if await choose() => {},".repeat(n);
                    format!(
                        "let op: closure(i64) -> i64 = |x| x; match op {{ {arms} g => {{ println(g(1)); }} }}"
                    )
                };
                let program = parse(&format!(
                    "async fn choose() -> bool {{ return false; }} async fn work() {{ {body} }}"
                ));
                let mut checker = TypeChecker::new();
                checker.set_enforce_send_sync(true);
                checker.check_program(&program);
                let Item::Function(f) = &program.items[1] else {
                    unreachable!()
                };
                let result = analyze(
                    &f.body,
                    &checker.lambda_captures,
                    &checker.expr_types,
                    &checker.local.exhaustive_matches,
                    &checker.pattern_resolutions,
                );
                assert!(checker.errors.iter().any(|e| e.code == ErrorCode::E2402));
                assert!(result.nodes <= 12 * n + 10, "{}", result.nodes);
                assert!(result.facts <= 22 * n + 10, "{}", result.facts);
                assert!(result.edge_visits <= 24 * n + 10, "{}", result.edge_visits);
                println!(
                    "pattern/defer n={n} deferred={deferred} blocks={} facts={} edges={}",
                    result.nodes, result.facts, result.edge_visits
                );
            }
        }
    }

    #[test]
    fn async_retention_ordered_cleanup() {
        let cases = [
            (
                "overwrite",
                "defer { op = |x| x + 1; println(op(1)); }",
                true,
                false,
            ),
            (
                "both branches",
                "defer { if flag { op = |x| x + 1; } else { op = |x| x + 2; } println(op(1)); }",
                true,
                false,
            ),
            (
                "one branch",
                "defer { if flag { op = |x| x + 1; } println(op(1)); }",
                true,
                true,
            ),
            (
                "read before overwrite",
                "defer { println(op(1)); op = |x| x + 1; }",
                true,
                true,
            ),
            (
                "rhs reads old",
                "defer { op = identity(op); println(op(1)); }",
                true,
                true,
            ),
            (
                "unreachable read",
                "defer { if false { println(op(1)); } }",
                true,
                false,
            ),
            (
                "constant overwrite",
                "defer { if true { op = |x| x + 1; } println(op(1)); }",
                true,
                false,
            ),
            (
                "unreachable overwrite",
                "defer { if false { op = |x| x + 1; } println(op(1)); }",
                true,
                true,
            ),
            (
                "shadow local",
                "defer { let op: closure(i64) -> i64 = |x| x + 1; println(op(1)); }",
                true,
                false,
            ),
            (
                "shadow init reads",
                "defer { let op = identity(op); println(op(1)); }",
                true,
                true,
            ),
            (
                "loop zero iterations",
                "defer { while flag { op = |x| x + 1; } println(op(1)); }",
                true,
                true,
            ),
            (
                "loop overwrite",
                "defer { while true { op = |x| x + 1; println(op(1)); } }",
                true,
                false,
            ),
            (
                "loop unreachable",
                "defer { while false { println(op(1)); } }",
                true,
                false,
            ),
            (
                "nested overwrite",
                "defer { if flag { if flag { op = |x| x; } else { op = |x| x; } } else { op = |x| x; } println(op(1)); }",
                true,
                false,
            ),
            (
                "match overwrite",
                "defer match flag { true => { op = |x| x; println(op(1)); }, false => { op = |x| x; println(op(1)); } };",
                true,
                false,
            ),
            (
                "match read branch",
                "defer match flag { true => { op = |x| x; println(op(1)); }, false => { println(op(1)); } };",
                true,
                true,
            ),
            (
                "lifo kills",
                "defer { println(op(1)); } defer { op = |x| x; }",
                true,
                false,
            ),
            (
                "lifo reads",
                "defer { op = |x| x; } defer { println(op(1)); }",
                true,
                true,
            ),
            ("no await", "defer { println(op(1)); }", false, false),
            (
                "ended scope",
                "if flag { defer { println(op(1)); } }",
                true,
                false,
            ),
        ];
        for (name, body, wait, bad) in cases {
            let wait = if wait { "await sleep(0);" } else { "" };
            let program = parse(&format!(
                "fn identity(op: closure(i64) -> i64) -> closure(i64) -> i64 {{ return op; }} async fn check(flag: bool) {{ let mut op: closure(i64) -> i64 = |x| x; {body} {wait} }}"
            ));
            let mut checker = TypeChecker::new();
            checker.set_enforce_send_sync(true);
            checker.check_program(&program);
            let errors: Vec<_> = checker
                .errors
                .iter()
                .filter(|e| e.severity == Severity::Error)
                .collect();
            assert_eq!(
                errors.iter().any(|e| e.code == ErrorCode::E2402),
                bad,
                "{name}: {errors:?}"
            );
            assert!(
                errors.iter().all(|e| e.code == ErrorCode::E2402),
                "{name}: {errors:?}"
            );
        }
    }

    #[test]
    fn async_retention_shared_ordered_cleanup_scaling() {
        for n in [16, 32, 64, 128] {
            let cleanup = "defer { op = |x| x + 1; println(op(1)); }".repeat(n);
            let exits = "if flag { return; }".repeat(n);
            let program = parse(&format!(
                "async fn work(flag: bool) {{ let mut op: closure(i64) -> i64 = |x| x; {cleanup} await sleep(0); {exits} }}"
            ));
            let mut checker = TypeChecker::new();
            checker.set_enforce_send_sync(true);
            checker.check_program(&program);
            assert!(
                !checker.errors.iter().any(|e| e.severity == Severity::Error),
                "{:?}",
                checker.errors
            );
            let Item::Function(f) = &program.items[0] else {
                unreachable!()
            };
            let result = analyze(
                &f.body,
                &checker.lambda_captures,
                &checker.expr_types,
                &checker.local.exhaustive_matches,
                &checker.pattern_resolutions,
            );
            assert!(result.nodes <= 12 * n + 10, "{}", result.nodes);
            assert!(result.facts <= 12 * n + 10, "{}", result.facts);
            assert!(result.edge_visits <= 16 * n + 10, "{}", result.edge_visits);
            assert!(
                result.summary_counts.0 <= 4 * n,
                "{:?}",
                result.summary_counts
            );
            assert!(
                result.summary_counts.1 <= 8 * n,
                "{:?}",
                result.summary_counts
            );
            println!(
                "cleanup summaries n={n} nodes={} visits={}",
                result.summary_counts.0, result.summary_counts.1
            );
            println!(
                "ordered cleanup n={n} blocks={} facts={} edges={}",
                result.nodes, result.facts, result.edge_visits
            );
        }
    }

    #[test]
    fn async_retention_cleanup_continuations() {
        let cases = [
            (
                "fallthrough",
                "await sleep(0); if true { defer { op = |x| x + 1; } } println(op(1));",
                false,
            ),
            (
                "optional scope",
                "await sleep(0); if flag { defer { op = |x| x; } } println(op(1));",
                true,
            ),
            (
                "both scopes",
                "await sleep(0); if flag { defer { op = |x| x; } } else { defer { op = |x| x; } } println(op(1));",
                false,
            ),
            (
                "nested scope",
                "await sleep(0); if true { if true { defer { op = |x| x; } } } println(op(1));",
                false,
            ),
            (
                "scope read before kill",
                "await sleep(0); if true { defer { println(op(1)); op = |x| x; } } println(op(1));",
                true,
            ),
            (
                "scope conditional kill",
                "await sleep(0); if true { defer { if flag { op = |x| x; } } } println(op(1));",
                true,
            ),
            (
                "scope both kill",
                "await sleep(0); if true { defer { if flag { op = |x| x; } else { op = |x| x; } } } println(op(1));",
                false,
            ),
            (
                "break",
                "while true { await sleep(0); defer { op = |x| x; } break; } println(op(1));",
                false,
            ),
            (
                "nested break",
                "while true { await sleep(0); if true { defer { op = |x| x; } break; } } println(op(1));",
                false,
            ),
            (
                "break conditional kill",
                "while true { await sleep(0); defer { if flag { op = |x| x; } } break; } println(op(1));",
                true,
            ),
            (
                "break reads old",
                "while true { await sleep(0); defer { println(op(1)); op = |x| x; } break; } println(op(1));",
                true,
            ),
            (
                "break skips outer cleanup",
                "defer { op = |x| x; } while true { await sleep(0); break; } println(op(1));",
                true,
            ),
            (
                "continue",
                "while true { println(op(1)); await sleep(0); defer { op = |x| x; } continue; }",
                false,
            ),
            (
                "nested continue",
                "while true { println(op(1)); await sleep(0); if true { defer { op = |x| x; } continue; } }",
                false,
            ),
            (
                "continue conditional kill",
                "while true { println(op(1)); await sleep(0); defer { if flag { op = |x| x; } } continue; }",
                true,
            ),
            (
                "continue reads old",
                "while true { println(op(1)); await sleep(0); defer { println(op(1)); op = |x| x; } continue; }",
                true,
            ),
            (
                "continue skips outer cleanup",
                "defer { op = |x| x; } while true { println(op(1)); await sleep(0); continue; }",
                true,
            ),
            (
                "fallthrough skips outer cleanup",
                "defer { op = |x| x; } await sleep(0); if true {} println(op(1));",
                true,
            ),
            (
                "nested defer",
                "await sleep(0); if true { defer { defer { op = |x| x; } } } println(op(1));",
                false,
            ),
            (
                "lifo scope",
                "await sleep(0); if true { defer { println(op(1)); } defer { op = |x| x; } } println(op(1));",
                false,
            ),
        ];
        for (name, body, bad) in cases {
            let program = parse(&format!(
                "async fn check(flag: bool) {{ let mut op: closure(i64) -> i64 = |x| x; {body} }}"
            ));
            let mut checker = TypeChecker::new();
            checker.set_enforce_send_sync(true);
            checker.check_program(&program);
            let errors: Vec<_> = checker
                .errors
                .iter()
                .filter(|e| e.severity == Severity::Error)
                .collect();
            assert_eq!(
                errors.iter().any(|e| e.code == ErrorCode::E2402),
                bad,
                "{name}: {errors:?}"
            );
            assert!(
                errors.iter().all(|e| e.code == ErrorCode::E2402),
                "{name}: {errors:?}"
            );
        }
    }

    #[test]
    fn async_retention_exhaustive_match_overwrites() {
        let matches = [
            (
                "bool",
                "match flag { true => { op = |x| x; }, false => { op = |x| x; } }",
                false,
            ),
            (
                "bool read",
                "match flag { true => { println(op(1)); }, false => { op = |x| x; } }",
                true,
            ),
            (
                "bool unchanged",
                "match flag { true => {}, false => { op = |x| x; } }",
                true,
            ),
            (
                "bool guard",
                "match flag { true if flag => { op = |x| x; }, true => { op = |x| x; }, false => { op = |x| x; } }",
                false,
            ),
            (
                "bool guard read",
                "match flag { true if flag => { op = |x| x; }, true => { println(op(1)); }, false => { op = |x| x; } }",
                true,
            ),
            (
                "enum",
                "match tag { Tag::A => { op = |x| x; }, Tag::B => { op = |x| x; } }",
                false,
            ),
            (
                "enum read",
                "match tag { Tag::A => { println(op(1)); }, Tag::B => { op = |x| x; } }",
                true,
            ),
            (
                "enum unchanged",
                "match tag { Tag::A => {}, Tag::B => { op = |x| x; } }",
                true,
            ),
            (
                "enum guard",
                "match tag { Tag::A if flag => { op = |x| x; }, Tag::A => { op = |x| x; }, Tag::B => { op = |x| x; } }",
                false,
            ),
            (
                "enum guard read",
                "match tag { Tag::A if flag => { op = |x| x; }, Tag::A => { println(op(1)); }, Tag::B => { op = |x| x; } }",
                true,
            ),
        ];
        for (name, matched, bad) in matches {
            for cleanup in [false, true] {
                let body = if cleanup {
                    format!("defer {{ {matched} println(op(1)); }} await sleep(0);")
                } else {
                    format!("await sleep(0); {matched} println(op(1));")
                };
                let program = parse(&format!(
                    "enum Tag {{ A, B }} async fn check(flag: bool, tag: Tag) {{ let mut op: closure(i64) -> i64 = |x| x; {body} }}"
                ));
                let mut checker = TypeChecker::new();
                checker.set_enforce_send_sync(true);
                checker.check_program(&program);
                let errors: Vec<_> = checker
                    .errors
                    .iter()
                    .filter(|e| e.severity == Severity::Error)
                    .collect();
                assert_eq!(
                    errors.iter().any(|e| e.code == ErrorCode::E2402),
                    bad,
                    "{name} cleanup={cleanup}: {errors:?}"
                );
                assert!(
                    errors.iter().all(|e| e.code == ErrorCode::E2402),
                    "{name} cleanup={cleanup}: {errors:?}"
                );
            }
        }
    }

    #[test]
    fn async_retention_exhaustive_match_scaling() {
        for n in [16, 32, 64, 128] {
            let variants = (0..n).map(|i| format!("V{i},")).collect::<String>();
            let arms = (0..n)
                .map(|i| format!("Tag::V{i} => {{ op = |x| x; }},"))
                .collect::<String>();
            for qualified in [false, true] {
                let arms = if qualified {
                    arms.clone()
                } else {
                    arms.replace("Tag::", "")
                };
                for cleanup in [false, true] {
                    let matched = format!("match tag {{ {arms} }} println(op(1));");
                    let body = if cleanup {
                        format!("defer {{ {matched} }} await sleep(0);")
                    } else {
                        format!("await sleep(0); {matched}")
                    };
                    let program = parse(&format!(
                        "enum Tag {{ {variants} }} async fn check(tag: Tag) {{ let mut op: closure(i64) -> i64 = |x| x; {body} }}"
                    ));
                    let mut checker = TypeChecker::new();
                    checker.set_enforce_send_sync(true);
                    checker.check_program(&program);
                    assert!(
                        !checker.errors.iter().any(|e| e.severity == Severity::Error),
                        "{:?}",
                        checker.errors
                    );
                    let Item::Function(f) = &program.items[1] else {
                        unreachable!()
                    };
                    // Body-local coverage facts are restored after checking a body;
                    // reconstruct the proven match ids solely for counting this CFG.
                    let mut exhaustive = HashSet::new();
                    for event in AstWalk::new(AstEvent::Block(&f.body)) {
                        if let AstEvent::Expr(Expr::Match(m)) = event {
                            exhaustive.insert(m.id);
                        }
                    }
                    let result = analyze(
                        &f.body,
                        &checker.lambda_captures,
                        &checker.expr_types,
                        &exhaustive,
                        &checker.pattern_resolutions,
                    );
                    assert!(result.nodes <= 8 * n + 8);
                    assert!(result.facts <= 16 * n + 8);
                    assert!(result.edge_visits <= 20 * n + 8);
                    assert!(result.summary_counts.0 <= 8 * n + 8);
                    assert!(result.summary_counts.1 <= 20 * n + 8);
                    println!(
                        "exhaustive n={n} qualified={qualified} cleanup={cleanup} blocks={} facts={} edges={} summary={:?}",
                        result.nodes, result.facts, result.edge_visits, result.summary_counts
                    );
                }
            }
        }
    }

    #[test]
    fn async_retention_literal_expression_branches() {
        let cases = [
            ("false && await choose()", false),
            ("true && await choose()", true),
            ("true || await choose()", false),
            ("false || await choose()", true),
            ("true ? false : await choose()", false),
            ("false ? await choose() : true", false),
            ("true ? await choose() : false", true),
            ("false ? true : await choose()", true),
            ("flag && await choose()", true),
            ("flag || await choose()", true),
            ("flag ? false : await choose()", true),
            ("flag ? await choose() : true", true),
            ("false && (true && await choose())", false),
            ("true || (false || await choose())", false),
            ("true ? (false && await choose()) : await choose()", false),
            ("false ? await choose() : (true || await choose())", false),
            ("true && (false || await choose())", true),
            ("false || (true && await choose())", true),
            ("true ? (true && await choose()) : false", true),
            ("false ? true : (false || await choose())", true),
        ];
        for (expr, bad) in cases {
            let program = parse(&format!(
                "async fn choose() -> bool {{ return true; }} async fn check(flag: bool) {{ let op: closure(i64) -> i64 = |x| x; let b = {expr}; println(op(1)); }}"
            ));
            let mut checker = TypeChecker::new();
            checker.set_enforce_send_sync(true);
            checker.check_program(&program);
            let errors: Vec<_> = checker
                .errors
                .iter()
                .filter(|e| e.severity == Severity::Error)
                .collect();
            assert_eq!(
                errors.iter().any(|e| e.code == ErrorCode::E2402),
                bad,
                "{expr}: {errors:?}"
            );
            assert!(
                errors.iter().all(|e| e.code == ErrorCode::E2402),
                "{expr}: {errors:?}"
            );
        }
    }

    #[test]
    fn async_retention_resolved_fieldless_variants() {
        let cases = [
            (
                "later read",
                "{a} => {{}}, {b} => {{ println(op(1)); }}",
                "",
                true,
            ),
            (
                "earlier overwrite",
                "{a} => {{ op = |x| x; }}, {b} => {{ println(op(1)); }}",
                "",
                true,
            ),
            (
                "both overwrite",
                "{a} => {{ op = |x| x; }}, {b} => {{ op = |x| x; }}",
                "println(op(1));",
                false,
            ),
            ("both dead", "{a} => {{}}, {b} => {{}}", "", false),
            (
                "guard fallback",
                "{a} if flag => {{}}, {a} => {{}}, {b} => {{ println(op(1)); }}",
                "",
                true,
            ),
        ];
        for qualified in [false, true] {
            let (a, b) = if qualified {
                ("Tag::A", "Tag::B")
            } else {
                ("A", "B")
            };
            for (name, arms, tail, bad) in cases {
                let arms = arms
                    .replace("{a}", a)
                    .replace("{b}", b)
                    .replace("{{", "{")
                    .replace("}}", "}");
                for cleanup in [false, true] {
                    let matched = format!("match tag {{ {arms} }} {tail}");
                    let body = if cleanup {
                        format!("defer {{ {matched} }} await sleep(0);")
                    } else {
                        format!("await sleep(0); {matched}")
                    };
                    assert_variant_retention(
                        &body,
                        bad,
                        &format!("{name} qualified={qualified} cleanup={cleanup}"),
                    );
                }
            }
            assert_variant_retention(
                &format!(
                    "match tag {{ {a} => {{}}, {b} => {{ await sleep(0); println(op(1)); }} }}"
                ),
                true,
                "later arm suspension",
            );
            assert_variant_retention(
                &format!(
                    "let A: closure(i64) -> i64 = |x| x; match tag {{ {a} => {{ await sleep(0); println(A(1)); }}, {b} => {{}} }}"
                ),
                true,
                "variant must not shadow local",
            );
        }
    }

    fn assert_variant_retention(body: &str, bad: bool, name: &str) {
        let program = parse(&format!(
            "enum Tag {{ A, B }} async fn check(tag: Tag, flag: bool) {{ let mut op: closure(i64) -> i64 = |x| x; {body} }}"
        ));
        let mut checker = TypeChecker::new();
        checker.set_enforce_send_sync(true);
        checker.check_program(&program);
        let errors: Vec<_> = checker
            .errors
            .iter()
            .filter(|e| e.severity == Severity::Error)
            .collect();
        assert_eq!(
            errors.iter().any(|e| e.code == ErrorCode::E2402),
            bad,
            "{name}: {errors:?}"
        );
        assert!(
            errors.iter().all(|e| e.code == ErrorCode::E2402),
            "{name}: {errors:?}"
        );
    }

    #[test]
    fn async_retention_literal_match_guards() {
        for pattern in ["_", "value"] {
            for guard in ["false", "true", "flag"] {
                let body = format!(
                    "match true {{ {pattern} if {guard} => {{ await sleep(0); println(op(1)); }}, _ => {{}} }}"
                );
                assert_guard_retention(&body, guard != "false");
                let body = format!(
                    "match true {{ {pattern} if {guard} => {{}}, _ => {{ await sleep(0); println(op(1)); }} }}"
                );
                assert_guard_retention(&body, guard != "true");
            }
        }
        for cleanup in [false, true] {
            for guard in ["false", "true", "flag"] {
                let matched = format!(
                    "match true {{ _ if {guard} => {{ op = |x| x + 1; }}, _ => {{ println(op(1)); }} }} println(op(1));"
                );
                let body = if cleanup {
                    format!("defer {{ {matched} }} await sleep(0);")
                } else {
                    format!("await sleep(0); {matched}")
                };
                assert_guard_retention(&body, guard != "true");
            }
            let matched = "match true { _ if false => { println(op(1)); }, _ => { op = |x| x + 1; } } println(op(1));";
            let body = if cleanup {
                format!("defer {{ {matched} }} await sleep(0);")
            } else {
                format!("await sleep(0); {matched}")
            };
            assert_guard_retention(&body, false);
        }
    }

    fn assert_guard_retention(body: &str, bad: bool) {
        let program = parse(&format!(
            "async fn check(flag: bool) {{ let mut op: closure(i64) -> i64 = |x| x; {body} }}"
        ));
        let mut checker = TypeChecker::new();
        checker.set_enforce_send_sync(true);
        checker.check_program(&program);
        let errors: Vec<_> = checker
            .errors
            .iter()
            .filter(|e| e.severity == Severity::Error)
            .collect();
        assert_eq!(
            errors.iter().any(|e| e.code == ErrorCode::E2402),
            bad,
            "{body}: {errors:?}"
        );
        assert!(
            errors.iter().all(|e| e.code == ErrorCode::E2402),
            "{body}: {errors:?}"
        );
    }

    #[test]
    fn async_retention_intermediate_scaling() {
        for n in [16, 32, 64, 128] {
            let body = "consume(identity(op), await number());".repeat(n);
            let program = parse(&format!(
                r#"
fn identity(op: closure(i64) -> i64) -> closure(i64) -> i64 {{ return op; }}
fn consume(op: closure(i64) -> i64, n: i64) {{ println(op(n)); }}
async fn number() -> i64 {{ return 1; }}
async fn work() {{ let op: closure(i64) -> i64 = |x| x; {body} }}
"#
            ));
            let mut checker = TypeChecker::new();
            checker.set_enforce_send_sync(true);
            checker.check_program(&program);
            let Item::Function(f) = &program.items[3] else {
                unreachable!()
            };
            let result = analyze(
                &f.body,
                &checker.lambda_captures,
                &checker.expr_types,
                &checker.local.exhaustive_matches,
                &checker.pattern_resolutions,
            );
            assert_eq!(result.values.len(), n);
            assert!(result.nodes <= 4 * n + 4, "{}", result.nodes);
            assert!(result.facts <= 8 * n, "{}", result.facts);
            assert!(result.edge_visits <= 10 * n, "{}", result.edge_visits);
            println!(
                "intermediate n={n} blocks={} facts={} edges={}",
                result.nodes, result.facts, result.edge_visits
            );
        }
    }

    #[test]
    fn async_retention_straight_line_scaling() {
        for n in [16, 32, 64, 128] {
            let declarations = (0..n)
                .map(|i| format!("let x{i} = {i};"))
                .collect::<String>();
            let reads = (0..n)
                .map(|i| format!("println(x{i});"))
                .collect::<String>();
            for suspend in [false, true] {
                let wait = if suspend { "await sleep(0);" } else { "" };
                let program = parse(&format!(
                    "async fn check() {{ {declarations} {wait} {reads} }}"
                ));
                let Item::Function(f) = &program.items[0] else {
                    unreachable!()
                };
                let result = analyze(
                    &f.body,
                    &HashMap::new(),
                    &HashMap::new(),
                    &HashSet::new(),
                    &HashMap::new(),
                );
                assert_eq!(result.locals.len(), if suspend { n } else { 0 });
                assert!(result.nodes <= 6, "{}", result.nodes);
                assert!(result.facts <= 2 * n, "{}", result.facts);
                assert!(result.edge_visits <= 2 * n, "{}", result.edge_visits);
                println!(
                    "straight n={n} suspend={suspend} blocks={} facts={} edges={}",
                    result.nodes, result.facts, result.edge_visits
                );
            }
        }
    }

    #[test]
    fn async_retention_branch_scaling() {
        for n in [16, 32, 64, 128] {
            for nested in [false, true] {
                let body = if nested {
                    format!("{}await sleep(0);{}", "if flag {".repeat(n), "}".repeat(n))
                } else {
                    "if flag { await sleep(0); } else { println(0); }".repeat(n)
                };
                let program = parse(&format!(
                    "async fn check(flag: bool) {{ let x = 1; {body} println(x); }}"
                ));
                let Item::Function(f) = &program.items[0] else {
                    unreachable!()
                };
                let result = analyze(
                    &f.body,
                    &HashMap::new(),
                    &HashMap::new(),
                    &HashSet::new(),
                    &HashMap::new(),
                );
                assert_eq!(result.locals.len(), 1);
                assert!(result.nodes <= 10 * n, "{}", result.nodes);
                assert!(result.facts <= 10 * n, "{}", result.facts);
                assert!(result.edge_visits <= 15 * n, "{}", result.edge_visits);
                println!(
                    "branch n={n} nested={nested} blocks={} facts={} edges={}",
                    result.nodes, result.facts, result.edge_visits
                );
            }
        }
    }

    #[test]
    fn async_retention_never_continuations() {
        let cases = [
            (
                "unreachable await",
                "panic(\"stop\"); await sleep(0); println(op(1));",
                false,
            ),
            (
                "unreachable read",
                "await sleep(0); panic(\"stop\"); println(op(1));",
                false,
            ),
            (
                "reachable read",
                "await sleep(0); println(op(1)); panic(\"stop\");",
                true,
            ),
            (
                "panic operand read",
                "await sleep(0); panic(op(1) == 1 ? \"a\" : \"b\");",
                true,
            ),
            (
                "panic operand await",
                "panic(await choose() ? \"a\" : \"b\"); println(op(1));",
                false,
            ),
            (
                "earlier operand across await",
                "panic(op(1) == 1 && await choose() ? \"a\" : \"b\");",
                false,
            ),
            (
                "conditional panic",
                "if flag { panic(\"stop\"); } await sleep(0); println(op(1));",
                true,
            ),
            (
                "both branches panic",
                "if flag { panic(\"a\"); } else { panic(\"b\"); } await sleep(0); println(op(1));",
                false,
            ),
            (
                "cleanup unreachable read",
                "defer { panic(\"stop\"); println(op(1)); } await sleep(0);",
                false,
            ),
            (
                "cleanup reachable read",
                "defer { println(op(1)); panic(\"stop\"); } await sleep(0);",
                true,
            ),
            (
                "cleanup conditional panic",
                "defer { if flag { panic(\"stop\"); } println(op(1)); } await sleep(0);",
                true,
            ),
            (
                "cleanup older read",
                "defer { println(op(1)); } defer { panic(\"stop\"); } await sleep(0);",
                true,
            ),
            (
                "cleanup overwrite before panic",
                "defer { println(op(1)); } defer { op = |x| x + 1; panic(\"stop\"); } await sleep(0);",
                false,
            ),
            (
                "cleanup normal and panic overwrite",
                "defer { println(op(1)); } defer { if flag { op = |x| x + 1; panic(\"stop\"); } else { op = |x| x + 2; } } await sleep(0);",
                false,
            ),
            (
                "cleanup panic without overwrite",
                "defer { println(op(1)); } defer { if flag { panic(\"stop\"); } else { op = |x| x + 2; } } await sleep(0);",
                true,
            ),
            (
                "only normal cleanup reaches following read",
                "await sleep(0); if true { defer { if flag { panic(\"stop\"); } else { op = |x| x + 1; } } } println(op(1));",
                false,
            ),
            (
                "normal cleanup preserves following read",
                "await sleep(0); if true { defer { if flag { op = |x| x + 1; panic(\"stop\"); } } } println(op(1));",
                true,
            ),
            (
                "saved panic registration",
                "defer panic(\"stop\"); await sleep(0); println(op(1));",
                true,
            ),
            (
                "saved panic older cleanup",
                "defer { println(op(1)); } defer panic(\"stop\"); await sleep(0);",
                true,
            ),
            (
                "saved panic unreachable continuation",
                "await sleep(0); if true { defer panic(\"stop\"); } println(op(1));",
                false,
            ),
            (
                "panic runs cleanup",
                "defer { println(op(1)); } await sleep(0); panic(\"stop\");",
                true,
            ),
            (
                "panic before await runs cleanup",
                "defer { println(op(1)); } panic(\"stop\"); await sleep(0);",
                false,
            ),
            (
                "recovery resumes outer scope",
                "await sleep(0); if true { defer { recover(); } panic(\"stop\"); } println(op(1));",
                true,
            ),
            (
                "recovery skips rest of inner scope",
                "if true { defer { recover(); } panic(\"stop\"); await sleep(0); println(op(1)); }",
                false,
            ),
            (
                "cleanup recovery resumes",
                "defer { if true { defer { recover(); } panic(\"stop\"); } println(op(1)); } await sleep(0);",
                true,
            ),
            (
                "cleanup recovery overwritten",
                "defer { if true { defer { op = |x| x + 1; recover(); } panic(\"stop\"); } println(op(1)); } await sleep(0);",
                false,
            ),
        ];
        for (name, body, bad) in cases {
            let program = parse(&format!(
                "async fn choose() -> bool {{ return true; }} async fn check(flag: bool) {{ let mut op: closure(i64) -> i64 = |x| x; {body} }}"
            ));
            let mut checker = TypeChecker::new();
            checker.set_enforce_send_sync(true);
            checker.check_program(&program);
            let errors: Vec<_> = checker
                .errors
                .iter()
                .filter(|e| e.severity == Severity::Error)
                .collect();
            assert_eq!(
                errors.iter().any(|e| e.code == ErrorCode::E2402),
                bad,
                "{name}: {errors:?}"
            );
            assert!(
                errors.iter().all(|e| e.code == ErrorCode::E2402),
                "{name}: {errors:?}"
            );
        }
    }

    #[test]
    fn async_retention_unwind_cleanup_scaling() {
        for n in [16, 32, 64, 128] {
            let cleanup =
                "defer { if flag { op = |x| x + 1; panic(\"stop\"); } else { op = |x| x + 2; } }"
                    .repeat(n);
            let exits = "if flag { return; }".repeat(n);
            let program = parse(&format!(
                "async fn work(flag: bool) {{ let mut op: closure(i64) -> i64 = |x| x; defer {{ println(op(1)); }} {cleanup} await sleep(0); {exits} }}"
            ));
            let mut checker = TypeChecker::new();
            checker.set_enforce_send_sync(true);
            checker.check_program(&program);
            assert!(
                !checker.errors.iter().any(|e| e.severity == Severity::Error),
                "{:?}",
                checker.errors
            );
            let Item::Function(f) = &program.items[0] else {
                unreachable!()
            };
            let result = analyze(
                &f.body,
                &checker.lambda_captures,
                &checker.expr_types,
                &checker.local.exhaustive_matches,
                &checker.pattern_resolutions,
            );
            assert!(result.nodes <= 10 * n + 10, "{}", result.nodes);
            assert!(result.facts <= 16 * n + 10, "{}", result.facts);
            assert!(result.edge_visits <= 24 * n + 10, "{}", result.edge_visits);
            assert!(
                result.summary_counts.0 <= 6 * n + 2,
                "{:?}",
                result.summary_counts
            );
            assert!(
                result.summary_counts.1 <= 12 * n + 2,
                "{:?}",
                result.summary_counts
            );
            println!(
                "unwind n={n} blocks={} facts={} edges={} summary={:?}",
                result.nodes, result.facts, result.edge_visits, result.summary_counts
            );
        }
    }

    #[test]
    fn async_retention_nested_unwind_summary_scaling() {
        for n in [16, 32, 64, 128] {
            let source = format!(
                "async fn work() {{ {} recover(); panic(\"stop\"); {} await sleep(0); }}",
                "defer {".repeat(n),
                "}".repeat(n)
            );
            let program = parse(&source);
            let mut checker = TypeChecker::new();
            checker.check_program(&program);
            assert!(
                !checker.errors.iter().any(|e| e.severity == Severity::Error),
                "{:?}",
                checker.errors
            );
            let Item::Function(f) = &program.items[0] else {
                unreachable!()
            };
            let result = analyze(
                &f.body,
                &checker.lambda_captures,
                &checker.expr_types,
                &checker.local.exhaustive_matches,
                &checker.pattern_resolutions,
            );
            assert!(
                result.summary_counts.0 <= 10 * n,
                "{:?}",
                result.summary_counts
            );
            assert!(
                result.summary_counts.1 <= 10 * n,
                "{:?}",
                result.summary_counts
            );
            println!(
                "nested unwind n={n} blocks={} summary={:?}",
                result.nodes, result.summary_counts
            );
        }
    }

    #[test]
    fn async_retention_exceptional_operations() {
        for operation in [
            "fail();",
            "println(values[zero]);",
            "println(1 / zero);",
            "println(1 % zero);",
            "values[zero] = 2;",
        ] {
            for context in 0..6 {
                let overwrite = "op = |x| x + 1;";
                let body = match context {
                    0 => format!(
                        "defer {{ println(op(1)); }} defer {{ {operation} {overwrite} }} await sleep(0);"
                    ),
                    1 => format!(
                        "defer {{ println(op(1)); }} defer {{ {overwrite} {operation} }} await sleep(0);"
                    ),
                    2 => format!(
                        "await sleep(0); if true {{ defer {{ recover(); }} {operation} {overwrite} }} println(op(1));"
                    ),
                    3 => format!(
                        "await sleep(0); if true {{ defer {{ recover(); }} {overwrite} {operation} }} println(op(1));"
                    ),
                    4 => format!("await sleep(0); {operation} {overwrite} println(op(1));"),
                    _ => format!("defer {{ println(op(1)); }} defer {{ {operation} {overwrite} }}"),
                };
                let program = parse(&format!(
                    "fn fail() {{ panic(\"stop\"); }} async fn check(zero: i64) {{ let mut values = [1]; let mut op: closure(i64) -> i64 = |x| x; {body} }}"
                ));
                let mut checker = TypeChecker::new();
                checker.set_enforce_send_sync(true);
                checker.check_program(&program);
                let errors: Vec<_> = checker
                    .errors
                    .iter()
                    .filter(|e| e.severity == Severity::Error)
                    .collect();
                assert_eq!(
                    errors.iter().any(|e| e.code == ErrorCode::E2402),
                    matches!(context, 0 | 2),
                    "{operation} context={context}: {errors:?}"
                );
                assert!(
                    errors.iter().all(|e| e.code == ErrorCode::E2402),
                    "{operation} context={context}: {errors:?}"
                );
            }
        }
    }

    #[test]
    fn async_retention_select_commit_unwinds_before_case() {
        for binding in [false, true] {
            for overwrite_first in [false, true] {
                for has_default in [false, true] {
                    let arm = if binding {
                        "let value = await t"
                    } else {
                        "await t"
                    };
                    let before = if overwrite_first {
                        "op = |x| x + 1;"
                    } else {
                        ""
                    };
                    let default = if has_default {
                        "default => { op = |x| x + 2; }"
                    } else {
                        ""
                    };
                    let source = format!(
                        "async fn slow() -> i64 {{ await sleep(1000); return 1; }} async fn check() {{ let t = slow(); t.cancel(); let mut op: closure(i64) -> i64 = |x| x; await sleep(0); if true {{ defer {{ recover(); }} {before} select {{ {arm} => {{ op = |x| x + 1; }} {default} }} }} println(op(1)); }}"
                    );
                    let program = parse(&source);
                    let mut checker = TypeChecker::new();
                    checker.set_enforce_send_sync(true);
                    checker.check_program(&program);
                    let errors: Vec<_> = checker
                        .errors
                        .iter()
                        .filter(|e| e.severity == Severity::Error)
                        .collect();
                    assert_eq!(
                        errors.iter().any(|e| e.code == ErrorCode::E2402),
                        !overwrite_first || !has_default,
                        "binding={binding} before={overwrite_first} default={default}: {errors:?}"
                    );
                    assert!(
                        errors.iter().all(|e| e.code == ErrorCode::E2402),
                        "{errors:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn async_retention_sparse_scaling() {
        for n in [16, 32, 64, 128] {
            let body = (0..n)
                .map(|i| format!("let x{i} = {i}; println(x{i}); await sleep(0);"))
                .collect::<String>();
            let program = parse(&format!("async fn check() {{ {body} }}"));
            let Item::Function(f) = &program.items[0] else {
                unreachable!()
            };
            let result = analyze(
                &f.body,
                &HashMap::new(),
                &HashMap::new(),
                &HashSet::new(),
                &HashMap::new(),
            );
            assert!(result.locals.is_empty());
            assert!(result.nodes <= 20 * n + 3, "{}", result.nodes);
            assert!(result.facts <= 8 * n, "{}", result.facts);
            assert!(result.edge_visits <= 10 * n, "{}", result.edge_visits);
            println!(
                "n={n} nodes={} facts={} edges={}",
                result.nodes, result.facts, result.edge_visits
            );
        }
    }
}
