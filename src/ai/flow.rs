//! Structured compiler CFG. Worklists bound native stack; continuations are shared.
use super::*;
type CapturedExpression = (
    Span,
    Option<Type>,
    Option<FunctionId>,
    Option<String>,
    usize,
);
pub(super) struct Body {
    pub function: FunctionId,
    pub nodes: Vec<(Span, Vec<usize>, bool)>,
    pub expressions: Vec<CapturedExpression>,
    pub entry: usize,
    pub complete: bool,
}
#[derive(Clone, Copy)]
struct Exits {
    ret: Option<usize>,
    brk: Option<usize>,
    cont: Option<usize>,
}
enum Job<'a> {
    Block(&'a Block, usize, Exits, usize),
    Expr(&'a Expr, usize, Exits, usize),
    Condition(&'a Expr, usize, usize, Exits, usize),
}
impl Body {
    pub fn build(function: FunctionId, root: AstEvent<'_>, types: &CaptureContext<'_>) -> Self {
        let span = match root {
            AstEvent::Block(b) => b.span,
            AstEvent::Expr(e) => e.span(),
            _ => unreachable!(),
        };
        let mut body = Self {
            function,
            nodes: vec![(span, vec![], false), (span, vec![], false)],
            expressions: vec![],
            entry: 0,
            complete: true,
        };
        let exits = Exits {
            ret: Some(0),
            brk: None,
            cont: None,
        };
        let mut jobs = Vec::new();
        body.entry = match root {
            AstEvent::Block(b) => body.block(&mut jobs, b, 0, exits),
            AstEvent::Expr(e) => body.expr(&mut jobs, e, 0, exits),
            _ => unreachable!(),
        };
        while let Some(job) = jobs.pop() {
            match job {
                Job::Block(b, next, exits, entry) => {
                    body.build_block(&mut jobs, b, next, exits, entry, types)
                }
                Job::Expr(e, next, exits, entry) => {
                    body.build_expr(&mut jobs, e, next, exits, entry, types)
                }
                Job::Condition(e, yes, no, exits, entry) => {
                    let target = match e {
                        Expr::Bool(value, ..) => {
                            if *value {
                                yes
                            } else {
                                no
                            }
                        }
                        Expr::Unary(u) if u.op == UnaryOp::Not => {
                            body.condition(&mut jobs, &u.expr, no, yes, exits)
                        }
                        Expr::Binary(b) if b.op == BinOp::And => {
                            let rhs = body.condition(&mut jobs, &b.rhs, yes, no, exits);
                            body.condition(&mut jobs, &b.lhs, rhs, no, exits)
                        }
                        Expr::Binary(b) if b.op == BinOp::Or => {
                            let rhs = body.condition(&mut jobs, &b.rhs, yes, no, exits);
                            body.condition(&mut jobs, &b.lhs, yes, rhs, exits)
                        }
                        _ => {
                            let fork = body.node(e.span(), vec![yes, no], false);
                            body.expr(&mut jobs, e, fork, exits)
                        }
                    };
                    // Boolean composition nodes still retain their checked type.
                    if matches!(e, Expr::Bool(..))
                        || matches!(e, Expr::Unary(u) if u.op == UnaryOp::Not)
                        || matches!(e, Expr::Binary(b) if matches!(b.op, BinOp::And|BinOp::Or))
                    {
                        body.capture(e, entry, types);
                    }
                    body.nodes[entry].1 = vec![target];
                }
            }
        }
        body
    }
    fn node(&mut self, span: Span, successors: Vec<usize>, header: bool) -> usize {
        let id = self.nodes.len();
        self.nodes.push((span, successors, header));
        id
    }
    fn block<'a>(
        &mut self,
        jobs: &mut Vec<Job<'a>>,
        b: &'a Block,
        next: usize,
        exits: Exits,
    ) -> usize {
        let entry = self.node(b.span, vec![], false);
        jobs.push(Job::Block(b, next, exits, entry));
        entry
    }
    fn expr<'a>(
        &mut self,
        jobs: &mut Vec<Job<'a>>,
        e: &'a Expr,
        next: usize,
        exits: Exits,
    ) -> usize {
        let entry = self.node(e.span(), vec![], false);
        jobs.push(Job::Expr(e, next, exits, entry));
        entry
    }
    fn condition<'a>(
        &mut self,
        jobs: &mut Vec<Job<'a>>,
        e: &'a Expr,
        yes: usize,
        no: usize,
        exits: Exits,
    ) -> usize {
        let entry = self.node(e.span(), vec![], false);
        jobs.push(Job::Condition(e, yes, no, exits, entry));
        entry
    }
    fn capture(&mut self, e: &Expr, node: usize, types: &CaptureContext<'_>) {
        let operation = match e {
            Expr::Call(c) => Some(format!("call:{}", c.callee)),
            Expr::StaticCall(c) => Some(format!("static:{}::{}", c.class, c.method)),
            Expr::MethodCall(c) => Some(format!("method:{}", c.method)),
            Expr::Print(..) => Some("io:print".into()),
            Expr::Await(_) => Some("wait:await".into()),
            Expr::Select(_) => Some("wait:select".into()),
            Expr::New(_) => Some("allocate:new".into()),
            _ => None,
        };
        self.expressions.push((
            e.span(),
            types.get(&e.id()).cloned(),
            types.calls.get(&e.id()).copied().flatten(),
            operation,
            node,
        ));
    }
    // Direct deferred call operands run at registration. Its operation runs at
    // scope exit. Block/match defer bodies evaluate all their operands at exit.
    fn defer<'a>(
        &mut self,
        jobs: &mut Vec<Job<'a>>,
        d: &'a DeferStmt,
        next: usize,
        types: &CaptureContext<'_>,
    ) -> usize {
        let exits = Exits {
            ret: None,
            brk: None,
            cont: None,
        };
        match &d.body {
            DeferBody::Block(b) => self.block(jobs, b, next, exits),
            DeferBody::Expr(e @ Expr::Match(_)) => self.expr(jobs, e, next, exits),
            DeferBody::Expr(e) => {
                let node = self.node(
                    e.span(),
                    if matches!(types.get(&e.id()), Some(Type::Never)) {
                        vec![1]
                    } else {
                        vec![next]
                    },
                    false,
                );
                self.capture(e, node, types);
                node
            }
        }
    }
    fn children<'a>(root: AstEvent<'a>) -> Vec<&'a Expr> {
        let mut walk = AstWalk::new(root);
        walk.next();
        let mut expressions = Vec::new();
        while let Some(event) = walk.next() {
            match event {
                AstEvent::Expr(e) => {
                    expressions.push(e);
                    walk.skip_children();
                }
                AstEvent::Lambda(_) => walk.skip_children(),
                _ => {}
            }
        }
        expressions
    }
    fn sequence<'a>(
        &mut self,
        jobs: &mut Vec<Job<'a>>,
        expressions: Vec<&'a Expr>,
        mut next: usize,
        exits: Exits,
    ) -> usize {
        for expr in expressions.into_iter().rev() {
            next = self.expr(jobs, expr, next, exits);
        }
        next
    }
    fn build_block<'a>(
        &mut self,
        jobs: &mut Vec<Job<'a>>,
        b: &'a Block,
        mut next: usize,
        mut exits: Exits,
        entry: usize,
        types: &CaptureContext<'_>,
    ) {
        let mut contexts = Vec::with_capacity(b.stmts.len());
        // At most four shared cleanup continuations per registration. Deferred
        // bodies cannot return/break/continue/?; their nested defers need only
        // the normal continuation, so nesting does not multiply this bound.
        for statement in &b.stmts {
            contexts.push(exits);
            if let Stmt::Defer(d) = statement {
                let mut routes = HashMap::new();
                for target in [Some(next), exits.ret, exits.brk, exits.cont]
                    .into_iter()
                    .flatten()
                {
                    routes
                        .entry(target)
                        .or_insert_with(|| self.defer(jobs, d, target, types));
                }
                next = routes[&next];
                exits = Exits {
                    ret: exits.ret.map(|t| routes[&t]),
                    brk: exits.brk.map(|t| routes[&t]),
                    cont: exits.cont.map(|t| routes[&t]),
                };
            }
        }
        for (stmt, exits) in b.stmts.iter().zip(contexts).rev() {
            next = match stmt {
                Stmt::If(s) => {
                    let yes = self.block(jobs, &s.then_block, next, exits);
                    let no = s
                        .else_block
                        .as_ref()
                        .map_or(next, |b| self.block(jobs, b, next, exits));
                    self.condition(jobs, &s.cond, yes, no, exits)
                }
                Stmt::While(s) => {
                    let header = self.node(s.span, vec![], true);
                    let inner = self.block(
                        jobs,
                        &s.body,
                        header,
                        Exits {
                            ret: exits.ret,
                            brk: Some(next),
                            cont: Some(header),
                        },
                    );
                    let cond = self.condition(jobs, &s.cond, inner, next, exits);
                    self.nodes[header].1 = vec![cond];
                    header
                }
                Stmt::For(s) => {
                    let header = self.node(s.span, vec![], true);
                    let inner = self.block(
                        jobs,
                        &s.body,
                        header,
                        Exits {
                            ret: exits.ret,
                            brk: Some(next),
                            cont: Some(header),
                        },
                    );
                    self.nodes[header].1 = vec![inner, next];
                    self.expr(jobs, &s.iterable, header, exits)
                }
                Stmt::Break(_) => exits.brk.unwrap_or(0),
                Stmt::Continue(_) => exits.cont.unwrap_or(0),
                Stmt::Return(s) => s.value.as_ref().map_or(exits.ret.unwrap_or(0), |e| {
                    self.expr(jobs, e, exits.ret.unwrap_or(0), exits)
                }),
                Stmt::Lock(s) => {
                    let inner = self.block(jobs, &s.body, next, exits);
                    let acquire = self.node(s.header_span(), vec![inner], false);
                    self.expressions.push((
                        s.header_span(),
                        None,
                        None,
                        Some("wait:lock".into()),
                        acquire,
                    ));
                    self.expr(jobs, &s.target, acquire, exits)
                }
                Stmt::Defer(d) => match &d.body {
                    DeferBody::Expr(e) if !matches!(e, Expr::Match(_)) => {
                        self.sequence(jobs, Self::children(AstEvent::Expr(e)), next, exits)
                    }
                    _ => next,
                },
                _ => {
                    let operation = match stmt {
                        Stmt::Assign(_) => Some("binding:write"),
                        Stmt::FieldAssign(_) => Some("write:field"),
                        Stmt::StaticFieldAssign(_) => Some("write:static"),
                        Stmt::IndexAssign(_) => Some("write:index"),
                        Stmt::SuperInit(_) => Some("call:super.init"),
                        _ => None,
                    };
                    let node = self.node(stmt.span(), vec![next], false);
                    if let Some(op) = operation {
                        self.expressions
                            .push((stmt.span(), None, None, Some(op.into()), node));
                    }
                    self.sequence(jobs, Self::children(AstEvent::Stmt(stmt)), node, exits)
                }
            };
        }
        self.nodes[entry].1 = vec![next];
    }
    fn build_expr<'a>(
        &mut self,
        jobs: &mut Vec<Job<'a>>,
        e: &'a Expr,
        next: usize,
        exits: Exits,
        entry: usize,
        types: &CaptureContext<'_>,
    ) {
        let operation = self.node(
            e.span(),
            if matches!(types.get(&e.id()), Some(Type::Never)) {
                vec![1]
            } else {
                vec![next]
            },
            false,
        );
        if !matches!(e, Expr::Select(_)) {
            self.capture(e, operation, types);
        }
        let target = match e {
            Expr::Binary(b) if b.op == BinOp::And || b.op == BinOp::Or => {
                let rhs = self.expr(jobs, &b.rhs, operation, exits);
                let (yes, no) = if b.op == BinOp::And {
                    (rhs, operation)
                } else {
                    (operation, rhs)
                };
                self.condition(jobs, &b.lhs, yes, no, exits)
            }
            Expr::Ternary(t) => {
                let yes = self.expr(jobs, &t.then_expr, operation, exits);
                let no = self.expr(jobs, &t.else_expr, operation, exits);
                self.condition(jobs, &t.condition, yes, no, exits)
            }
            Expr::TryPropagate(value, ..) => {
                self.nodes[operation].1 = vec![next, exits.ret.unwrap_or(0)];
                self.expr(jobs, value, operation, exits)
            }
            Expr::Match(m) => {
                let mut arms = Vec::new();
                let mut covered = false;
                for arm in &m.arms {
                    let pattern = types
                        .patterns
                        .get(&arm.pattern.id())
                        .unwrap_or(&arm.pattern);
                    let target = match &arm.body {
                        MatchBody::Expr(e) => self.expr(jobs, e, operation, exits),
                        MatchBody::Block(b) => self.block(jobs, b, operation, exits),
                    };
                    // Literal scrutinees rule out impossible literal arms. For
                    // other predicates retain structural alternatives, not a
                    // claim that their conjunction is executable.
                    let compatible = match (&*m.scrutinee, pattern) {
                        (Expr::Bool(v, ..), Pattern::LiteralBool(p, ..)) => v == p,
                        (Expr::Integer(v, ..), Pattern::LiteralInt(p, ..)) => v == p,
                        _ => true,
                    };
                    if compatible && !covered {
                        arms.push(target);
                    }
                    covered |= matches!(pattern, Pattern::Wildcard(..) | Pattern::Binding { .. })
                        || compatible
                            && matches!(
                                (&*m.scrutinee, pattern),
                                (Expr::Bool(..), Pattern::LiteralBool(..))
                                    | (Expr::Integer(..), Pattern::LiteralInt(..))
                            );
                }
                let fork = self.node(m.span, arms, false);
                self.expr(jobs, &m.scrutinee, fork, exits)
            }
            Expr::Select(select) => {
                let mut arms = Vec::new();
                let mut operands = Vec::new();
                for case in &select.cases {
                    let inner = self.block(jobs, &case.body, operation, exits);
                    let arm = self.node(case.span, vec![inner], false);
                    let op = match &case.kind {
                        SelectCaseKind::Recv { channel, .. } => {
                            operands.push(channel);
                            "channel:recv"
                        }
                        SelectCaseKind::Send { channel, value } => {
                            operands.push(channel);
                            operands.push(value);
                            "channel:send"
                        }
                        SelectCaseKind::Timeout { millis } => {
                            operands.push(millis);
                            "wait:timeout"
                        }
                        SelectCaseKind::Join { task, .. } => {
                            operands.push(task);
                            "wait:join"
                        }
                        SelectCaseKind::Default => "select:default",
                    };
                    self.expressions
                        .push((case.span, None, None, Some(op.into()), arm));
                    arms.push(arm);
                }
                let fork = self.node(select.span, arms, false);
                self.capture(e, fork, types);
                self.sequence(jobs, operands, fork, exits)
            }
            Expr::Lambda(_) => operation,
            _ => self.sequence(jobs, Self::children(AstEvent::Expr(e)), operation, exits),
        };
        self.nodes[entry].1 = vec![target];
    }
}
