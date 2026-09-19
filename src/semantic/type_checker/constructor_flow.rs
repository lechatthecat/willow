//! Definite initialization on successful constructor exits (willow-9tls.31).
//!
//! Build a control-flow DAG, then union *possibly uninitialized* fields at
//! joins. A normal return and fallthrough share one exit; an unrecovered panic
//! has no edge to it. Registered defers conservatively permit scope recovery. Loop backedges are omitted: assignments only add initialization, and
//! each abstract branch is already possible on the first iteration. Later
//! iterations cannot weaken that first-iteration state. `break` reaches the
//! loop's successor; `continue` and the body tail end this abstract iteration.
//! Nonliteral loop conditions also have a zero-iteration edge. This is a must
//! analysis, not value/range inference or interprocedural initialization.
//!
//! Both construction and evaluation use explicit worklists, including deeply
//! nested expressions. States share storage until a field actually changes;
//! identity and uniform-state joins avoid scanning unchanged field sets.

use std::collections::HashMap;
use std::rc::Rc;

use crate::parser::ast::*;

const EXIT: usize = 0;
const DEAD: usize = 1;

#[derive(Clone, Copy)]
struct Flow {
    break_to: usize,
    panic_to: usize,
    loop_cleanup_panic_to: usize,
}

impl Flow {
    const ROOT: Self = Self {
        break_to: DEAD,
        panic_to: DEAD,
        loop_cleanup_panic_to: DEAD,
    };
}

pub(super) fn uninitialized_fields<'a>(
    body: &Block,
    fields: impl Iterator<Item = &'a str>,
    types: &HashMap<ExprId, Type>,
) -> Vec<bool> {
    let fields: HashMap<_, _> = fields.enumerate().map(|(i, name)| (name, i)).collect();
    if fields.is_empty() {
        return Vec::new();
    }
    let mut graph = Graph::new(&fields, types);
    let entry = graph.schedule(Input::Block(&body.stmts), EXIT, Flow::ROOT);
    graph.build();
    let (missing, _) = graph.solve(entry);
    let mut result = vec![false; fields.len()];
    if let Some(missing) = missing {
        missing.write_missing(&mut result);
    }
    result
}

#[derive(Default)]
struct Node {
    assignment: Option<usize>,
    edges: Vec<usize>,
}

#[derive(Clone, Copy)]
enum Input<'a> {
    Block(&'a [Stmt]),
    Stmt(&'a Stmt),
    Expr(&'a Expr),
}

struct Work<'a> {
    input: Input<'a>,
    node: usize,
    next: usize,
    flow: Flow,
}

struct Graph<'a> {
    nodes: Vec<Node>,
    work: Vec<Work<'a>>,
    fields: &'a HashMap<&'a str, usize>,
    types: &'a HashMap<ExprId, Type>,
}

impl<'a> Graph<'a> {
    fn new(fields: &'a HashMap<&'a str, usize>, types: &'a HashMap<ExprId, Type>) -> Self {
        Self {
            nodes: vec![Node::default(), Node::default()],
            work: Vec::new(),
            fields,
            types,
        }
    }

    fn schedule(&mut self, input: Input<'a>, next: usize, flow: Flow) -> usize {
        let node = self.nodes.len();
        self.nodes.push(Node::default());
        self.work.push(Work {
            input,
            node,
            next,
            flow,
        });
        node
    }

    fn expr(&mut self, expr: &'a Expr, next: usize, flow: Flow) -> usize {
        self.schedule(Input::Expr(expr), next, flow)
    }

    fn block(&mut self, block: &'a Block, next: usize, flow: Flow) -> usize {
        self.schedule(Input::Block(&block.stmts), next, flow)
    }

    fn args(&mut self, args: &'a [CallArg], mut next: usize, flow: Flow) -> usize {
        for arg in args.iter().rev() {
            next = self.expr(&arg.expr, next, flow);
        }
        next
    }

    fn fork(&mut self, edges: Vec<usize>) -> usize {
        let node = self.nodes.len();
        self.nodes.push(Node {
            assignment: None,
            edges,
        });
        node
    }

    fn condition(&mut self, cond: &'a Expr, yes: usize, no: usize, flow: Flow) -> usize {
        let target = match cond {
            Expr::Bool(true, ..) => yes,
            Expr::Bool(false, ..) => no,
            _ => self.fork(vec![yes, no]),
        };
        self.expr(cond, target, flow)
    }

    fn exit(&mut self, target: usize, flow: Flow) -> usize {
        if flow.panic_to == DEAD {
            target
        } else {
            self.fork(vec![target, flow.panic_to])
        }
    }

    fn build(&mut self) {
        while let Some(Work {
            input,
            node,
            next,
            flow,
        }) = self.work.pop()
        {
            let target = match input {
                Input::Block(stmts) => {
                    let mut active = flow;
                    let mut statements = Vec::with_capacity(stmts.len());
                    let mut has_defer = false;
                    for stmt in stmts {
                        statements.push((stmt, active));
                        if matches!(stmt, Stmt::Defer(_)) && !has_defer {
                            // Recovery resumes after this lexical scope. A
                            // deferred helper may recover, so lack of a direct
                            // recover() spelling is not proof that it cannot.
                            active.panic_to = if flow.panic_to == DEAD {
                                next
                            } else {
                                self.fork(vec![next, flow.panic_to])
                            };
                            active.loop_cleanup_panic_to = active.panic_to;
                            has_defer = true;
                        }
                    }
                    // Cleanup itself may panic into an enclosing recovery scope.
                    let mut target = if has_defer && flow.panic_to != DEAD {
                        self.fork(vec![next, flow.panic_to])
                    } else {
                        next
                    };
                    for (stmt, active) in statements.into_iter().rev() {
                        target = self.schedule(Input::Stmt(stmt), target, active);
                    }
                    target
                }
                Input::Stmt(stmt) => match stmt {
                    Stmt::Let(s) => self.expr(&s.init, next, flow),
                    Stmt::Assign(s) => self.expr(&s.value, next, flow),
                    Stmt::FieldAssign(s) => {
                        let assignment = self.nodes.len();
                        self.nodes.push(Node {
                            assignment: if matches!(&s.object, Expr::Var(name, ..) if name == "self") {
                                self.fields.get(s.field.as_str()).copied()
                            } else { None },
                            edges: vec![next],
                        });
                        let store = if matches!(&s.object, Expr::Var(name, ..) if name == "self") {
                            assignment
                        } else {
                            self.exit(assignment, flow)
                        };
                        let value = self.expr(&s.value, store, flow);
                        self.expr(&s.object, value, flow)
                    }
                    Stmt::StaticFieldAssign(s) => self.expr(&s.value, next, flow),
                    Stmt::IndexAssign(s) => {
                        let store = self.exit(next, flow);
                        let value = self.expr(&s.value, store, flow);
                        let index = self.expr(&s.index, value, flow);
                        self.expr(&s.array, index, flow)
                    }
                    Stmt::SuperInit(s) => {
                        let call = self.exit(next, flow);
                        self.args(&s.args, call, flow)
                    }
                    Stmt::Expr(s) => self.expr(&s.expr, next, flow),
                    Stmt::Return(s) => {
                        let exit = self.exit(EXIT, flow);
                        s.value.as_ref().map_or(exit, |e| self.expr(e, exit, flow))
                    }
                    Stmt::If(s) => {
                        let yes = self.block(&s.then_block, next, flow);
                        let no = s
                            .else_block
                            .as_ref()
                            .map_or(next, |b| self.block(b, next, flow));
                        self.condition(&s.cond, yes, no, flow)
                    }
                    Stmt::While(s) => {
                        let body = self.block(
                            &s.body,
                            DEAD,
                            Flow {
                                break_to: next,
                                loop_cleanup_panic_to: DEAD,
                                ..flow
                            },
                        );
                        self.condition(&s.cond, body, next, flow)
                    }
                    Stmt::For(s) => {
                        let body = self.block(
                            &s.body,
                            DEAD,
                            Flow {
                                break_to: next,
                                loop_cleanup_panic_to: DEAD,
                                ..flow
                            },
                        );
                        let fork = self.fork(vec![body, next]);
                        self.expr(&s.iterable, fork, flow)
                    }
                    Stmt::Break(_) => self.exit(
                        flow.break_to,
                        Flow {
                            panic_to: flow.loop_cleanup_panic_to,
                            ..flow
                        },
                    ),
                    Stmt::Continue(_) => self.exit(
                        DEAD,
                        Flow {
                            panic_to: flow.loop_cleanup_panic_to,
                            ..flow
                        },
                    ),
                    Stmt::Lock(s) => {
                        let body = self.block(&s.body, next, flow);
                        let acquire = self.exit(body, flow);
                        self.expr(&s.target, acquire, flow)
                    }
                    Stmt::Defer(s) => {
                        // Only direct-call receivers/arguments run at registration.
                        // Deferred matches/blocks and the call itself run later.
                        match &s.body {
                            DeferBody::Expr(Expr::Call(c)) => self.args(&c.args, next, flow),
                            DeferBody::Expr(Expr::StaticCall(c)) => self.args(&c.args, next, flow),
                            DeferBody::Expr(Expr::MethodCall(c)) => {
                                let args = self.args(&c.args, next, flow);
                                self.expr(&c.object, args, flow)
                            }
                            DeferBody::Expr(Expr::Print(e, ..)) => self.expr(e, next, flow),
                            _ => next,
                        }
                    }
                },
                Input::Expr(expr) => {
                    // Use checked expression identities, never the spelling of a
                    // call: a shadowed/user function named panic may return.
                    let next = if self.types.get(&expr.id()) == Some(&Type::Never) {
                        flow.panic_to
                    } else if flow.panic_to != DEAD && may_panic(expr) {
                        self.fork(vec![next, flow.panic_to])
                    } else {
                        next
                    };
                    match expr {
                        Expr::Lambda(_)
                        | Expr::Integer(..)
                        | Expr::Float(..)
                        | Expr::Bool(..)
                        | Expr::String(..)
                        | Expr::Var(..)
                        | Expr::StaticField(_) => next,
                        Expr::Call(c) => self.args(&c.args, next, flow),
                        Expr::StaticCall(c) => self.args(&c.args, next, flow),
                        Expr::New(c) => self.args(&c.args, next, flow),
                        Expr::MethodCall(c) => {
                            let args = self.args(&c.args, next, flow);
                            self.expr(&c.object, args, flow)
                        }
                        Expr::Binary(b) if matches!(b.op, BinOp::And | BinOp::Or) => {
                            let rhs = self.expr(&b.rhs, next, flow);
                            let (yes, no) = if b.op == BinOp::And {
                                (rhs, next)
                            } else {
                                (next, rhs)
                            };
                            self.condition(&b.lhs, yes, no, flow)
                        }
                        Expr::Binary(b) => {
                            let rhs = self.expr(&b.rhs, next, flow);
                            self.expr(&b.lhs, rhs, flow)
                        }
                        Expr::Unary(u) => self.expr(&u.expr, next, flow),
                        Expr::FieldAccess(e, ..) | Expr::Print(e, ..) => self.expr(e, next, flow),
                        Expr::Await(a) => self.expr(&a.expr, next, flow),
                        // Constructors return void, so ? is already rejected by
                        // E1807. Retain its success path for useful E0842s;
                        // a failed propagation never constructs an object.
                        Expr::TryPropagate(e, ..) => self.expr(e, next, flow),
                        Expr::Ternary(t) => {
                            let yes = self.expr(&t.then_expr, next, flow);
                            let no = self.expr(&t.else_expr, next, flow);
                            self.condition(&t.condition, yes, no, flow)
                        }
                        Expr::Range(r) => {
                            let end = self.expr(&r.end, next, flow);
                            self.expr(&r.start, end, flow)
                        }
                        Expr::Index(a, i, ..) => {
                            let index = self.expr(i, next, flow);
                            self.expr(a, index, flow)
                        }
                        Expr::ArrayLiteral(items, ..) => {
                            let mut target = next;
                            for item in items.iter().rev() {
                                target = self.expr(item, target, flow);
                            }
                            target
                        }
                        Expr::ObjectLiteral(o) => {
                            let mut target = next;
                            for field in o.fields.iter().rev() {
                                target = self.expr(&field.value, target, flow);
                            }
                            target
                        }
                        Expr::Match(m) => {
                            let mut arms = Vec::with_capacity(m.arms.len());
                            for arm in &m.arms {
                                arms.push(match &arm.body {
                                    MatchBody::Block(b) => self.block(b, next, flow),
                                    MatchBody::Expr(e) => self.expr(e, next, flow),
                                });
                            }
                            // Exhaustiveness is checked separately (E1206).
                            let fork = self.fork(arms);
                            self.expr(&m.scrutinee, fork, flow)
                        }
                        // select/await are invalid in synchronous constructors;
                        // still model their evaluation and branches faithfully.
                        Expr::Select(s) => {
                            let mut arms = Vec::with_capacity(s.cases.len());
                            for case in &s.cases {
                                arms.push(self.block(&case.body, next, flow));
                            }
                            let mut target = self.fork(arms);
                            for case in s.cases.iter().rev() {
                                target = match &case.kind {
                                    SelectCaseKind::Recv { channel, .. } => {
                                        self.expr(channel, target, flow)
                                    }
                                    SelectCaseKind::Send { channel, value } => {
                                        let value = self.expr(value, target, flow);
                                        self.expr(channel, value, flow)
                                    }
                                    SelectCaseKind::Timeout { millis } => {
                                        self.expr(millis, target, flow)
                                    }
                                    SelectCaseKind::Join { task, .. } => {
                                        self.expr(task, target, flow)
                                    }
                                    SelectCaseKind::Default => target,
                                };
                            }
                            target
                        }
                    }
                }
            };
            self.nodes[node].edges.push(target);
        }
    }

    fn solve(&self, entry: usize) -> (Option<State>, Counts) {
        let mut counts = Counts::default();
        let mut reachable = vec![false; self.nodes.len()];
        let mut incoming = vec![0; self.nodes.len()];
        let mut pending = vec![entry];
        while let Some(id) = pending.pop() {
            if std::mem::replace(&mut reachable[id], true) {
                continue;
            }
            counts.nodes += 1;
            for &to in &self.nodes[id].edges {
                if to == DEAD {
                    continue;
                }
                incoming[to] += 1;
                counts.edges += 1;
                pending.push(to);
            }
        }
        let mut states: Vec<Option<State>> = vec![None; self.nodes.len()];
        states[entry] = Some(State::Missing);
        pending.push(entry);
        while let Some(id) = pending.pop() {
            let Some(mut state) = states[id].take() else {
                continue;
            };
            if let Some(field) = self.nodes[id].assignment {
                state.assign(field, self.fields.len(), &mut counts);
            }
            if id == EXIT {
                return (Some(state), counts);
            }
            let edges = &self.nodes[id].edges;
            for &to in edges {
                // No successful exit can observe this state.
                if to == DEAD {
                    continue;
                }
                // Clone only the root. This local state is dropped before the
                // next node runs, leaving linear successors uniquely owned.
                let outgoing = state.clone();
                match &mut states[to] {
                    Some(existing) => existing.join(outgoing, &mut counts),
                    slot @ None => *slot = Some(outgoing),
                }
                incoming[to] -= 1;
                if incoming[to] == 0 {
                    pending.push(to);
                }
            }
        }
        (None, counts)
    }
}

/// Conservative local hazard classification. Calls include user helpers and
/// imported code; proving their absence of panic is a separate effect query.
/// Child operands are evaluated separately, so literals and mere variable
/// references do not introduce spurious recovery edges.
fn may_panic(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Call(_)
            | Expr::MethodCall(_)
            | Expr::StaticCall(_)
            | Expr::New(_)
            | Expr::Index(..)
            | Expr::Print(..)
            | Expr::Binary(_)
            | Expr::Unary(_)
            | Expr::FieldAccess(..)
            | Expr::ArrayLiteral(..)
            | Expr::ObjectLiteral(_)
            | Expr::Await(_)
            | Expr::Select(_)
    )
}

/// Persistent binary field set. Uniform subtrees take no allocation. Splitting
/// a shared set for one assignment copies O(log F) nodes, never all F fields.
/// Recursion is bounded by the bit width of usize, independently of AST depth.
#[derive(Clone)]
enum State {
    Missing,
    Initialized,
    Split(Rc<[State; 2]>),
}

impl State {
    fn assign(&mut self, field: usize, width: usize, counts: &mut Counts) {
        // Test before copy-on-write so repeated assignments never allocate.
        if !self.initialized(field, width, counts) {
            self.write_assignment(field, width, counts);
        }
    }

    fn initialized(&self, field: usize, width: usize, counts: &mut Counts) -> bool {
        counts.assignment_nodes += 1;
        match self {
            Self::Missing => false,
            Self::Initialized => true,
            Self::Split(children) => {
                let middle = width / 2;
                if field < middle {
                    children[0].initialized(field, middle, counts)
                } else {
                    children[1].initialized(field - middle, width - middle, counts)
                }
            }
        }
    }

    fn write_assignment(&mut self, field: usize, width: usize, counts: &mut Counts) {
        counts.assignment_nodes += 1;
        if matches!(self, Self::Initialized) {
            return;
        }
        if width == 1 {
            *self = Self::Initialized;
            return;
        }
        if matches!(self, Self::Missing) {
            *self = Self::Split(Rc::new([Self::Missing, Self::Missing]));
            counts.allocated_nodes += 1;
        }
        let Self::Split(children) = self else {
            unreachable!()
        };
        if Rc::strong_count(children) > 1 {
            counts.allocated_nodes += 1;
        }
        let children = Rc::make_mut(children);
        let middle = width / 2;
        if field < middle {
            children[0].write_assignment(field, middle, counts);
        } else {
            children[1].write_assignment(field - middle, width - middle, counts);
        }
        if children.iter().all(|s| matches!(s, Self::Initialized)) {
            *self = Self::Initialized;
        }
    }

    fn join(&mut self, other: Self, counts: &mut Counts) {
        counts.join_nodes += 1;
        match (&mut *self, other) {
            (Self::Missing, _) | (_, Self::Initialized) => {}
            (_, Self::Missing) => *self = Self::Missing,
            (Self::Initialized, other) => *self = other,
            (Self::Split(left), Self::Split(right)) => {
                if Rc::ptr_eq(left, &right) {
                    return;
                }
                if Rc::strong_count(left) > 1 {
                    counts.allocated_nodes += 1;
                }
                let left = Rc::make_mut(left);
                // Cloning roots is constant work; unchanged descendants stay shared.
                left[0].join(right[0].clone(), counts);
                left[1].join(right[1].clone(), counts);
                if left.iter().all(|s| matches!(s, Self::Missing)) {
                    *self = Self::Missing;
                }
            }
        }
    }

    fn write_missing(&self, out: &mut [bool]) {
        match self {
            Self::Missing => out.fill(true),
            Self::Initialized => {}
            Self::Split(children) => {
                let middle = out.len() / 2;
                children[0].write_missing(&mut out[..middle]);
                children[1].write_missing(&mut out[middle..]);
            }
        }
    }
}

#[derive(Default, Debug)]
struct Counts {
    nodes: usize,
    edges: usize,
    assignment_nodes: usize,
    join_nodes: usize,
    allocated_nodes: usize,
}

#[cfg(test)]
mod tests;
