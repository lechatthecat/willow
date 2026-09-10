//! A-normalization for scheduler-suspending `recv`/`await` expressions.
//!
//! The poll state machine can only resume at statement boundaries with values
//! stored in its heap frame. Hoisting nested suspension points into explicitly
//! typed `let` statements gives each value such a frame slot and preserves
//! left-to-right evaluation (including short-circuit and ternary control flow).

use std::collections::HashMap;

use crate::diagnostics::Span;
use crate::parser::ast::*;
use crate::semantic::ids::SemanticType as Type;
use crate::semantic::intrinsics::{self, Intrinsic};

pub(crate) fn normalize_coop_suspensions(
    program: &Program,
    expr_types: &mut HashMap<ExprId, Type>,
) -> Program {
    let mut program = program.clone();
    let mut normalizer = Normalizer {
        expr_types,
        next_temp: 0,
        completed: HashMap::new(),
        suspensions: HashMap::new(),
    };
    for item in &mut program.items {
        match item {
            Item::Function(function) if function.is_async => {
                normalizer.normalize_block(&mut function.body);
            }
            Item::Class(class) => {
                for method in &mut class.methods {
                    if method.is_async {
                        normalizer.normalize_block(&mut method.body);
                    }
                }
            }
            _ => {}
        }
    }
    program
}

struct Normalizer<'a> {
    expr_types: &'a mut HashMap<ExprId, Type>,
    next_temp: usize,
    completed: HashMap<ExprId, (Vec<Stmt>, Expr)>,
    suspensions: HashMap<ExprId, bool>,
}

impl Normalizer<'_> {
    fn synthetic(&mut self, source: Span) -> (String, Span) {
        let index = self.next_temp;
        self.next_temp += 1;
        (format!("__willow$suspend${index}"), source)
    }

    fn ty(&self, expr: &Expr) -> Type {
        self.expr_types
            .get(&expr.id())
            .cloned()
            .expect("internal compiler error: missing checked payload type")
    }

    fn bind(&mut self, prefix: &mut Vec<Stmt>, expr: Expr, ty: Type) -> Expr {
        if ty == Type::Void {
            return expr;
        }
        // These are already stable/repeatable values. In particular, avoiding a
        // duplicate frame slot for every task-handle variable keeps large async
        // functions within the frame's 62-reference mask capacity.
        if matches!(
            expr,
            Expr::Var(..) | Expr::Integer(..) | Expr::Float(..) | Expr::Bool(..) | Expr::String(..)
        ) {
            return expr;
        }
        let (name, span) = self.synthetic(expr.span());
        prefix.push(Stmt::Let(LetStmt {
            name: name.clone(),
            mutable: false,
            ty: Some(ty.to_source()),
            init: expr,
            span,
        }));
        let id = ExprId::fresh();
        self.expr_types.insert(id, ty);
        Expr::Var(name, span, id)
    }

    /// A suspension point that must occupy a statement of its own, with the
    /// type to give the hoisted `let`.
    ///
    /// The cooperative poll only recognises an `await` that IS a statement
    /// root; a nested one would otherwise fall back to the blocking lowering
    /// and re-enter the scheduler from inside a poll (it could then neither
    /// park nor be cancelled). Since `await` is the one Task-waiting form
    /// (willow-qrj9), every nested `await` is hoisted here instead.
    fn direct_suspend_type(&self, expr: &Expr) -> Option<Type> {
        match expr {
            Expr::Await(_) => Some(self.ty(expr)),
            Expr::MethodCall(method) => {
                // The receiver's type decides whether this is the channel
                // intrinsic or a same-named user method; resolving it here
                // rather than comparing `method.method` to `"recv"` is what
                // keeps this pass and the emitter agreeing about which calls
                // suspend (willow-uqzx, catalog item 7).
                let receiver_ty = self.expr_types.get(&method.object.id())?;
                let resolved = intrinsics::resolve(
                    &receiver_ty.to_source(),
                    &method.method,
                    method.args.len(),
                )?;
                // Only `recv` is hoisted. A cooperative `send` suspends too, but
                // it produces no value, so there is nothing to bind a `let` to
                // and `is_channel_send` handles it where it stands
                // (willow-o038).
                (resolved.intrinsic == Intrinsic::ChannelRecv)
                    .then(|| resolved.return_type(|_| None).into())
            }
            _ => None,
        }
    }

    fn contains_suspend(&self, expr: &Expr) -> bool {
        if let Some(found) = self.suspensions.get(&expr.id()) {
            return *found;
        }
        use crate::parser::iter::{AstEvent, AstWalk};
        let mut walk = AstWalk::new(AstEvent::Expr(expr));
        while let Some(event) = walk.next() {
            match event {
                AstEvent::Expr(expr) => {
                    if self.direct_suspend_type(expr).is_some() {
                        return true;
                    }
                    match expr {
                        Expr::Lambda(_) => walk.skip_children(),
                        // Case bodies are normalized separately; only operands
                        // are evaluated at the enclosing select entry.
                        Expr::Select(select) => {
                            walk.skip_children();
                            for case in &select.cases {
                                match &case.kind {
                                    SelectCaseKind::Recv { channel, .. } => {
                                        walk.push(AstEvent::Expr(channel))
                                    }
                                    SelectCaseKind::Send { channel, value } => {
                                        walk.push(AstEvent::Expr(channel));
                                        walk.push(AstEvent::Expr(value));
                                    }
                                    SelectCaseKind::Timeout { millis } => {
                                        walk.push(AstEvent::Expr(millis))
                                    }
                                    SelectCaseKind::Join { task, .. } => {
                                        walk.push(AstEvent::Expr(task))
                                    }
                                    SelectCaseKind::Default => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
                AstEvent::Stmt(stmt) => {
                    let operand = match stmt {
                        Stmt::If(s) => Some(&s.cond),
                        Stmt::While(s) => Some(&s.cond),
                        Stmt::For(s) => Some(&s.iterable),
                        Stmt::Lock(s) => Some(&s.target),
                        Stmt::Break(_) | Stmt::Continue(_) => {
                            walk.skip_children();
                            None
                        }
                        _ => None,
                    };
                    if let Some(expr) = operand {
                        walk.skip_children();
                        walk.push(AstEvent::Expr(expr));
                    }
                }
                _ => {}
            }
        }
        false
    }

    fn normalize_block(&mut self, block: &mut Block) {
        use crate::parser::ownership::NodeMut;
        enum Work<'a> {
            Block(&'a mut Block),
            Node(NodeMut<'a>),
        }
        let mut pending = vec![Work::Block(block)];
        while let Some(work) = pending.pop() {
            match work {
                Work::Block(block) => {
                    let mut output = Vec::new();
                    for stmt in std::mem::take(&mut block.stmts) {
                        self.normalize_stmt(stmt, &mut output);
                    }
                    block.stmts = output;
                    pending.extend(
                        block
                            .stmts
                            .iter_mut()
                            .rev()
                            .map(|stmt| Work::Node(NodeMut::Stmt(stmt))),
                    );
                }
                Work::Node(NodeMut::Stmt(stmt)) => match stmt {
                    Stmt::If(stmt) => {
                        if let Some(block) = &mut stmt.else_block {
                            pending.push(Work::Block(block));
                        }
                        pending.push(Work::Block(&mut stmt.then_block));
                        pending.push(Work::Node(NodeMut::Expr(&mut stmt.cond)));
                    }
                    Stmt::While(stmt) => {
                        pending.push(Work::Block(&mut stmt.body));
                        pending.push(Work::Node(NodeMut::Expr(&mut stmt.cond)));
                    }
                    Stmt::For(stmt) => {
                        pending.push(Work::Block(&mut stmt.body));
                        pending.push(Work::Node(NodeMut::Expr(&mut stmt.iterable)));
                    }
                    Stmt::Lock(stmt) => {
                        pending.push(Work::Block(&mut stmt.body));
                        pending.push(Work::Node(NodeMut::Expr(&mut stmt.target)));
                    }
                    Stmt::Defer(stmt) => match &mut stmt.body {
                        DeferBody::Block(block) => pending.push(Work::Block(block)),
                        DeferBody::Expr(expr) => pending.push(Work::Node(NodeMut::Expr(expr))),
                    },
                    stmt => pending.extend(
                        NodeMut::Stmt(stmt)
                            .children()
                            .into_iter()
                            .rev()
                            .map(Work::Node),
                    ),
                },
                Work::Node(NodeMut::Expr(expr)) => match expr {
                    Expr::Lambda(_) => {}
                    Expr::Match(expr) => {
                        for arm in expr.arms.iter_mut().rev() {
                            match &mut arm.body {
                                MatchBody::Block(block) => pending.push(Work::Block(block)),
                                MatchBody::Expr(expr) => {
                                    pending.push(Work::Node(NodeMut::Expr(expr)))
                                }
                            }
                        }
                        pending.push(Work::Node(NodeMut::Expr(&mut expr.scrutinee)));
                    }
                    Expr::Select(expr) => {
                        for case in expr.cases.iter_mut().rev() {
                            pending.push(Work::Block(&mut case.body));
                        }
                    }
                    expr => pending.extend(
                        NodeMut::Expr(expr)
                            .children()
                            .into_iter()
                            .rev()
                            .map(Work::Node),
                    ),
                },
            }
        }
    }

    fn normalize_stmt(&mut self, mut outer: Stmt, output: &mut Vec<Stmt>) {
        match &mut outer {
            Stmt::Let(stmt) => {
                let (mut prefix, value) = self.normalize_stmt_root_expr(take_expr(&mut stmt.init));
                stmt.init = value;
                output.append(&mut prefix);
            }
            Stmt::Assign(stmt) => {
                let (mut prefix, value) = self.normalize_stmt_root_expr(take_expr(&mut stmt.value));
                stmt.value = value;
                output.append(&mut prefix);
            }
            Stmt::StaticFieldAssign(stmt) => {
                let (mut prefix, value) = self.normalize_expr(take_expr(&mut stmt.value));
                stmt.value = value;
                output.append(&mut prefix);
            }
            Stmt::FieldAssign(stmt) => {
                let (mut object_prefix, object) = self.normalize_expr(take_expr(&mut stmt.object));
                let object = if self.contains_suspend(&stmt.value) {
                    let ty = self.ty(&object);
                    self.bind(&mut object_prefix, object, ty)
                } else {
                    object
                };
                let (mut value_prefix, value) =
                    self.normalize_stmt_root_expr(take_expr(&mut stmt.value));
                stmt.object = object;
                stmt.value = value;
                output.append(&mut object_prefix);
                output.append(&mut value_prefix);
            }
            Stmt::IndexAssign(stmt) => {
                let has_suspend =
                    self.contains_suspend(&stmt.index) || self.contains_suspend(&stmt.value);
                let (mut prefix, array) = self.normalize_expr(take_expr(&mut stmt.array));
                let array = if has_suspend {
                    let ty = self.ty(&array);
                    self.bind(&mut prefix, array, ty)
                } else {
                    array
                };
                let (mut index_prefix, index) = self.normalize_expr(take_expr(&mut stmt.index));
                prefix.append(&mut index_prefix);
                let index = if self.contains_suspend(&stmt.value) {
                    let ty = self.ty(&index);
                    self.bind(&mut prefix, index, ty)
                } else {
                    index
                };
                let (mut value_prefix, value) =
                    self.normalize_stmt_root_expr(take_expr(&mut stmt.value));
                prefix.append(&mut value_prefix);
                stmt.array = array;
                stmt.index = index;
                stmt.value = value;
                output.append(&mut prefix);
            }
            Stmt::SuperInit(stmt) => {
                let mut prefix = Vec::new();
                for arg in &mut stmt.args {
                    let (mut arg_prefix, value) = self.normalize_expr(take_expr(&mut arg.expr));
                    prefix.append(&mut arg_prefix);
                    arg.expr = value;
                }
                output.append(&mut prefix);
            }
            Stmt::If(stmt) => {
                let (mut prefix, cond) = self.normalize_expr(take_expr(&mut stmt.cond));
                stmt.cond = cond;

                output.append(&mut prefix);
            }
            Stmt::While(stmt) => {
                let (mut cond_prefix, cond) = self.normalize_expr(take_expr(&mut stmt.cond));

                if cond_prefix.is_empty() {
                    stmt.cond = cond;
                } else {
                    let not_cond = Expr::Unary(Box::new(UnaryExpr {
                        op: UnaryOp::Not,
                        span: cond.span(),
                        expr: cond,
                        id: ExprId::fresh(),
                    }));
                    cond_prefix.push(Stmt::If(IfStmt {
                        cond: not_cond,
                        then_block: Block {
                            stmts: vec![Stmt::Break(stmt.span)],
                            span: stmt.span,
                        },
                        else_block: None,
                        span: stmt.span,
                    }));
                    cond_prefix.append(&mut stmt.body.stmts);
                    stmt.cond = Expr::Bool(true, stmt.span, ExprId::fresh());
                    stmt.body.stmts = cond_prefix;
                }
            }
            Stmt::For(stmt) => {
                let (mut prefix, iterable) = self.normalize_expr(take_expr(&mut stmt.iterable));
                stmt.iterable = iterable;

                output.append(&mut prefix);
            }
            // `lock` makes a function ineligible for cooperative lowering until
            // willow-38w.1.3, so this arm is not reached today. It normalizes the
            // target like any other single root expression and leaves the body's
            // shape intact, which is what the acquire/release lowering will want.
            Stmt::Lock(stmt) => {
                let (mut prefix, target) = self.normalize_expr(take_expr(&mut stmt.target));
                stmt.target = target;

                output.append(&mut prefix);
            }
            Stmt::Return(stmt) => {
                if let Some(value) = stmt.value.take() {
                    let (mut prefix, value) = self.normalize_stmt_root_expr(value);
                    stmt.value = Some(value);
                    output.append(&mut prefix);
                }
            }
            Stmt::Expr(stmt) => {
                if let Expr::Select(sel) = &mut stmt.expr {
                    // Hoist suspending OPERANDS out of every case (channel
                    // exprs + send values) into entry lets — the emitted
                    // select then only sees frame-backed temps — and
                    // normalize the case bodies (review fix, willow-0a6k.6).
                    let mut prefix = Vec::new();
                    for case in &mut sel.cases {
                        match &mut case.kind {
                            SelectCaseKind::Recv { channel, .. } => {
                                if self.contains_suspend(channel) {
                                    let ty = self.ty(channel);
                                    let (mut ch_prefix, ch) =
                                        self.normalize_expr(take_expr(channel));
                                    prefix.append(&mut ch_prefix);
                                    *channel = self.bind(&mut prefix, ch, ty);
                                }
                            }
                            SelectCaseKind::Send { channel, value } => {
                                if self.contains_suspend(channel) {
                                    let ty = self.ty(channel);
                                    let (mut ch_prefix, ch) =
                                        self.normalize_expr(take_expr(channel));
                                    prefix.append(&mut ch_prefix);
                                    *channel = self.bind(&mut prefix, ch, ty);
                                }
                                if self.contains_suspend(value) {
                                    let ty = self.ty(value);
                                    let (mut v_prefix, v) = self.normalize_expr(take_expr(value));
                                    prefix.append(&mut v_prefix);
                                    *value = self.bind(&mut prefix, v, ty);
                                }
                            }
                            SelectCaseKind::Timeout { millis } => {
                                if self.contains_suspend(millis) {
                                    let ty = self.ty(millis);
                                    let (mut m_prefix, m) = self.normalize_expr(take_expr(millis));
                                    prefix.append(&mut m_prefix);
                                    *millis = self.bind(&mut prefix, m, ty);
                                }
                            }
                            SelectCaseKind::Join { task, .. } => {
                                if self.contains_suspend(task) {
                                    let ty = self.ty(task);
                                    let (mut t_prefix, t) = self.normalize_expr(take_expr(task));
                                    prefix.append(&mut t_prefix);
                                    *task = self.bind(&mut prefix, t, ty);
                                }
                            }
                            SelectCaseKind::Default => {}
                        }
                    }
                    output.append(&mut prefix);

                    output.push(outer);
                    return;
                }
                let (mut prefix, expr) = self.normalize_stmt_root_expr(take_expr(&mut stmt.expr));
                stmt.expr = expr;
                output.append(&mut prefix);
            }
            Stmt::Defer(stmt) => {
                match std::mem::replace(
                    &mut stmt.body,
                    DeferBody::Block(Block {
                        stmts: Vec::new(),
                        span: stmt.span,
                    }),
                ) {
                    // Direct-call operands evaluate at registration, so their
                    // suspension prefixes remain outside the deferred action.
                    DeferBody::Expr(
                        expr @ (Expr::Call(_) | Expr::MethodCall(_) | Expr::Print(..)),
                    ) => {
                        let (mut prefix, expr) = self.normalize_expr(expr);
                        stmt.body = DeferBody::Expr(expr);
                        output.append(&mut prefix);
                    }
                    // A match's scrutinee/body evaluate at scope exit. If ANF
                    // introduces prefixes, keep them inside a deferred block.
                    DeferBody::Expr(expr) => {
                        let span = expr.span();
                        let (mut prefix, expr) = self.normalize_expr(expr);
                        if prefix.is_empty() {
                            stmt.body = DeferBody::Expr(expr);
                        } else {
                            prefix.push(Stmt::Expr(ExprStmt { expr, span }));
                            stmt.body = DeferBody::Block(Block {
                                stmts: prefix,
                                span,
                            });
                        }
                    }
                    DeferBody::Block(block) => {
                        stmt.body = DeferBody::Block(block);
                    }
                }
            }
            Stmt::Break(_) | Stmt::Continue(_) => {}
        }
        output.push(outer);
    }

    /// Normalize an expression that is the ROOT of a statement whose poll
    /// lowering already awaits natively (`let`/assign/field-assign/
    /// index-assign/return/expression-statement). A root `await` stays where it
    /// is — hoisting it would only spend a second frame slot on the same value
    /// — while its operands are still normalized (willow-qrj9).
    fn normalize_stmt_root_expr(&mut self, mut expr: Expr) -> (Vec<Stmt>, Expr) {
        if let Expr::Await(a) = &mut expr {
            let (prefix, inner) = self.normalize_expr(take_expr(&mut a.expr));
            a.expr = inner;
            (prefix, expr)
        } else {
            self.normalize_expr(expr)
        }
    }

    fn normalize_expr(&mut self, expr: Expr) -> (Vec<Stmt>, Expr) {
        if let Some(completed) = self.completed.remove(&expr.id()) {
            return completed;
        }
        self.summarize_suspensions(&expr);
        enum Step {
            Enter(Expr),
            Finish(Expr),
        }
        let root = expr.id();
        let mut pending = vec![Step::Enter(expr)];
        while let Some(step) = pending.pop() {
            let expr = match step {
                Step::Enter(mut expr) => {
                    if !self.contains_suspend(&expr) {
                        self.completed.insert(expr.id(), (Vec::new(), expr));
                        continue;
                    }
                    let direct = self.direct_suspend_type(&expr).is_some();
                    let children = normalization_children(&mut expr, direct);
                    let mut owned = Vec::with_capacity(children.len());
                    for child in children {
                        let placeholder = Expr::Integer(0, child.span(), child.id());
                        owned.push(std::mem::replace(child, placeholder));
                    }
                    pending.push(Step::Finish(expr));
                    pending.extend(owned.into_iter().rev().map(Step::Enter));
                    continue;
                }
                Step::Finish(expr) => expr,
            };
            let original_id = expr.id();
            let ty = self.expr_types.get(&original_id).cloned();
            let (prefix, value) = self.normalize_expr_inner(expr);
            if let Some(ty) = ty {
                self.expr_types.insert(value.id(), ty);
            }
            self.completed.insert(original_id, (prefix, value));
        }
        let result = self
            .completed
            .remove(&root)
            .expect("normalized expression root");
        debug_assert!(
            self.completed.is_empty(),
            "every normalization operand is consumed"
        );
        self.suspensions.clear();
        result
    }

    fn summarize_suspensions(&mut self, root: &Expr) {
        use crate::parser::iter::{AstEvent, AstWalk};
        let mut walk = AstWalk::new(AstEvent::Expr(root));
        let mut frames: Vec<(ExprId, bool)> = Vec::new();
        while let Some(event) = walk.next() {
            match event {
                AstEvent::Expr(expr) => {
                    frames.push((expr.id(), self.direct_suspend_type(expr).is_some()));
                    match expr {
                        Expr::Lambda(_) => {
                            walk.skip_children();
                            walk.push(AstEvent::ExitExpr(expr));
                        }
                        Expr::Select(select) => {
                            walk.skip_children();
                            walk.push(AstEvent::ExitExpr(expr));
                            for case in select.cases.iter().rev() {
                                match &case.kind {
                                    SelectCaseKind::Recv { channel, .. } => {
                                        walk.push(AstEvent::Expr(channel))
                                    }
                                    SelectCaseKind::Send { channel, value } => {
                                        walk.push(AstEvent::Expr(value));
                                        walk.push(AstEvent::Expr(channel));
                                    }
                                    SelectCaseKind::Timeout { millis } => {
                                        walk.push(AstEvent::Expr(millis))
                                    }
                                    SelectCaseKind::Join { task, .. } => {
                                        walk.push(AstEvent::Expr(task))
                                    }
                                    SelectCaseKind::Default => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
                AstEvent::ExitExpr(expr) => {
                    let (id, found) = frames.pop().expect("expression summary frame");
                    debug_assert_eq!(id, expr.id());
                    self.suspensions.insert(id, found);
                    if let Some((_, parent)) = frames.last_mut() {
                        *parent |= found;
                    }
                }
                AstEvent::Stmt(stmt) => {
                    let operand = match stmt {
                        Stmt::If(s) => Some(&s.cond),
                        Stmt::While(s) => Some(&s.cond),
                        Stmt::For(s) => Some(&s.iterable),
                        Stmt::Lock(s) => Some(&s.target),
                        Stmt::Break(_) | Stmt::Continue(_) => {
                            walk.skip_children();
                            None
                        }
                        _ => None,
                    };
                    if let Some(operand) = operand {
                        walk.skip_children();
                        walk.push(AstEvent::Expr(operand));
                    }
                }
                _ => {}
            }
        }
    }

    fn normalize_expr_inner(&mut self, mut expr: Expr) -> (Vec<Stmt>, Expr) {
        if !self.contains_suspend(&expr) {
            return (Vec::new(), expr);
        }
        if let Some(result_ty) = self.direct_suspend_type(&expr) {
            let mut prefix = match &mut expr {
                Expr::Await(a) => {
                    let (prefix, inner) = self.normalize_expr(take_expr(&mut a.expr));
                    a.expr = inner;
                    prefix
                }
                Expr::MethodCall(method) => {
                    let (mut prefix, receiver) = self.normalize_expr(take_expr(&mut method.object));
                    let receiver_ty = self.ty(&receiver);
                    method.object = self.bind(&mut prefix, receiver, receiver_ty);
                    prefix
                }
                other => unreachable!("unexpected direct suspend shape: {other:?}"),
            };
            let value = self.bind(&mut prefix, expr, result_ty);
            return (prefix, value);
        }

        let prefix = match &mut expr {
            Expr::Binary(binary) if matches!(binary.op, BinOp::And | BinOp::Or) => {
                return self.normalize_short_circuit(BinaryExpr {
                    op: binary.op.clone(),
                    lhs: take_expr(&mut binary.lhs),
                    rhs: take_expr(&mut binary.rhs),
                    span: binary.span,
                    id: binary.id,
                });
            }
            Expr::Binary(binary) => {
                let lhs_ty = self.ty(&binary.lhs);
                let rhs_ty = self.ty(&binary.rhs);
                let (mut prefix, lhs) = self.normalize_expr(take_expr(&mut binary.lhs));
                binary.lhs = self.bind(&mut prefix, lhs, lhs_ty);
                let (mut rhs_prefix, rhs) = self.normalize_expr(take_expr(&mut binary.rhs));
                prefix.append(&mut rhs_prefix);
                binary.rhs = self.bind(&mut prefix, rhs, rhs_ty);
                prefix
            }
            Expr::Unary(unary) => {
                let (prefix, value) = self.normalize_expr(take_expr(&mut unary.expr));
                unary.expr = value;
                prefix
            }
            Expr::Print(value, ..) => {
                let (prefix, normalized) = self.normalize_expr(take_expr(value));
                **value = normalized;
                prefix
            }
            Expr::Call(call) => self.normalize_args(&mut call.args),
            Expr::MethodCall(call) => {
                let receiver_ty = self.ty(&call.object);
                let (mut prefix, receiver) = self.normalize_expr(take_expr(&mut call.object));
                call.object = self.bind(&mut prefix, receiver, receiver_ty);
                let mut args = self.normalize_args(&mut call.args);
                prefix.append(&mut args);
                prefix
            }
            Expr::StaticCall(call) => self.normalize_args(&mut call.args),
            Expr::New(new) => self.normalize_args(&mut new.args),
            Expr::FieldAccess(object, ..) => {
                let (prefix, normalized) = self.normalize_expr(take_expr(object));
                **object = normalized;
                prefix
            }
            Expr::ObjectLiteral(object) => {
                let mut prefix = Vec::new();
                for field in &mut object.fields {
                    let ty = self.ty(&field.value);
                    let (mut field_prefix, value) =
                        self.normalize_expr(take_expr(&mut field.value));
                    prefix.append(&mut field_prefix);
                    field.value = self.bind(&mut prefix, value, ty);
                }
                prefix
            }
            Expr::Ternary(ternary) => {
                return self.normalize_ternary(TernaryExpr {
                    condition: take_expr(&mut ternary.condition),
                    then_expr: take_expr(&mut ternary.then_expr),
                    else_expr: take_expr(&mut ternary.else_expr),
                    span: ternary.span,
                    id: ternary.id,
                });
            }
            Expr::Range(range) => {
                let start_ty = self.ty(&range.start);
                let end_ty = self.ty(&range.end);
                let (mut prefix, start) = self.normalize_expr(take_expr(&mut range.start));
                range.start = self.bind(&mut prefix, start, start_ty);
                let (mut end_prefix, end) = self.normalize_expr(take_expr(&mut range.end));
                prefix.append(&mut end_prefix);
                range.end = self.bind(&mut prefix, end, end_ty);
                prefix
            }
            Expr::Match(match_expr) => {
                let scrutinee_ty = self.ty(&match_expr.scrutinee);
                let (mut prefix, scrutinee) =
                    self.normalize_expr(take_expr(&mut match_expr.scrutinee));
                *match_expr.scrutinee = self.bind(&mut prefix, scrutinee, scrutinee_ty);
                prefix
            }
            Expr::TryPropagate(inner, ..) => {
                let (prefix, normalized) = self.normalize_expr(take_expr(inner));
                **inner = normalized;
                prefix
            }
            Expr::Await(a) => {
                // Hoist suspends OUT of the awaited call's arguments; the
                // await itself stays in place (review fix on willow-0a6k.6).
                let (prefix, inner) = self.normalize_expr(take_expr(&mut a.expr));
                a.expr = inner;
                prefix
            }
            Expr::ArrayLiteral(elements, ..) => {
                let mut prefix = Vec::new();
                for element in elements {
                    let ty = self.ty(element);
                    let (mut element_prefix, value) = self.normalize_expr(take_expr(element));
                    prefix.append(&mut element_prefix);
                    *element = self.bind(&mut prefix, value, ty);
                }
                prefix
            }
            Expr::Index(array, index, ..) => {
                let array_ty = self.ty(array);
                let index_ty = self.ty(index);
                let (mut prefix, normalized_array) = self.normalize_expr(take_expr(array));
                **array = self.bind(&mut prefix, normalized_array, array_ty);
                let (mut index_prefix, normalized_index) = self.normalize_expr(take_expr(index));
                prefix.append(&mut index_prefix);
                **index = self.bind(&mut prefix, normalized_index, index_ty);
                prefix
            }
            _ => Vec::new(),
        };
        (prefix, expr)
    }

    fn normalize_args(&mut self, args: &mut [CallArg]) -> Vec<Stmt> {
        let mut prefix = Vec::new();
        for arg in args {
            let ty = self.ty(&arg.expr);
            let (mut arg_prefix, value) = self.normalize_expr(take_expr(&mut arg.expr));
            prefix.append(&mut arg_prefix);
            arg.expr = self.bind(&mut prefix, value, ty);
        }
        prefix
    }

    fn normalize_short_circuit(&mut self, binary: BinaryExpr) -> (Vec<Stmt>, Expr) {
        let span = binary.span;
        let op = binary.op;
        let (mut prefix, lhs) = self.normalize_expr(binary.lhs);
        let (name, result_span) = self.synthetic(span);
        prefix.push(Stmt::Let(LetStmt {
            name: name.clone(),
            mutable: true,
            ty: Some(crate::parser::ast::Type::Bool),
            init: lhs,
            span: result_span,
        }));
        let result_var = Expr::Var(name.clone(), result_span, ExprId::fresh());
        let cond = if op == BinOp::And {
            result_var.clone()
        } else {
            Expr::Unary(Box::new(UnaryExpr {
                op: UnaryOp::Not,
                expr: result_var.clone(),
                span,
                id: ExprId::fresh(),
            }))
        };
        let (mut rhs_prefix, rhs) = self.normalize_expr(binary.rhs);
        rhs_prefix.push(Stmt::Assign(AssignStmt {
            name,
            value: rhs,
            span,
        }));
        prefix.push(Stmt::If(IfStmt {
            cond,
            then_block: Block {
                stmts: rhs_prefix,
                span,
            },
            else_block: None,
            span,
        }));
        (prefix, result_var)
    }

    fn normalize_ternary(&mut self, ternary: TernaryExpr) -> (Vec<Stmt>, Expr) {
        let result_ty = self.ty(&Expr::Ternary(Box::new(ternary.clone())));
        let (mut prefix, condition) = self.normalize_expr(ternary.condition);
        let (name, result_span) = self.synthetic(ternary.span);
        prefix.push(Stmt::Let(LetStmt {
            name: name.clone(),
            mutable: true,
            ty: Some(result_ty.to_source()),
            init: default_value(&result_ty, result_span),
            span: result_span,
        }));
        let (mut then_prefix, then_value) = self.normalize_expr(ternary.then_expr);
        then_prefix.push(Stmt::Assign(AssignStmt {
            name: name.clone(),
            value: then_value,
            span: ternary.span,
        }));
        let (mut else_prefix, else_value) = self.normalize_expr(ternary.else_expr);
        else_prefix.push(Stmt::Assign(AssignStmt {
            name: name.clone(),
            value: else_value,
            span: ternary.span,
        }));
        prefix.push(Stmt::If(IfStmt {
            cond: condition,
            then_block: Block {
                stmts: then_prefix,
                span: ternary.span,
            },
            else_block: Some(Block {
                stmts: else_prefix,
                span: ternary.span,
            }),
            span: ternary.span,
        }));
        (prefix, Expr::Var(name, result_span, ExprId::fresh()))
    }
}

fn default_value(ty: &Type, span: Span) -> Expr {
    match ty {
        Type::Bool => Expr::Bool(false, span, ExprId::fresh()),
        Type::F64 => Expr::Float(0.0, span, ExprId::fresh()),
        Type::I64 => Expr::Integer(0, span, ExprId::fresh()),
        _ => Expr::Integer(0, span, ExprId::fresh()),
    }
}

fn take_expr(expr: &mut Expr) -> Expr {
    std::mem::replace(expr, crate::parser::ownership::placeholder_expr())
}

/// Only operands consumed by normalize_expr_inner. Scoped statement bodies
/// have their own worklist; lambda bodies belong to another callable.
fn normalization_children(expr: &mut Expr, direct: bool) -> Vec<&mut Expr> {
    match expr {
        Expr::Match(expr) => vec![&mut expr.scrutinee],
        Expr::Select(_) | Expr::Lambda(_) => vec![],
        Expr::MethodCall(expr) => {
            let mut children = vec![&mut expr.object];
            if !direct {
                children.extend(expr.args.iter_mut().map(|arg| &mut arg.expr));
            }
            children
        }
        expr => crate::parser::ownership::NodeMut::Expr(expr)
            .children()
            .into_iter()
            .filter_map(|node| match node {
                crate::parser::ownership::NodeMut::Expr(expr) => Some(expr),
                crate::parser::ownership::NodeMut::Stmt(_) => None,
            })
            .collect(),
    }
}

#[cfg(test)]
mod continuation_tests {
    use super::*;
    use crate::parser::iter::{AstEvent, AstWalk};

    fn normalizer(types: &mut HashMap<ExprId, Type>) -> Normalizer<'_> {
        Normalizer {
            expr_types: types,
            next_temp: 0,
            completed: HashMap::new(),
            suspensions: HashMap::new(),
        }
    }

    #[test]
    fn normalization_extracts_suspension_from_twenty_operand_positions() {
        let perspectives = [
            "-(await task)",
            "(await task) + 1",
            "1 + (await task)",
            "print(await task)",
            "f(await task)",
            "f(1, await task)",
            "(await task).field",
            "(await task).method(1)",
            "obj.method(await task)",
            "Box::method(await task)",
            "new Box(await task)",
            "Box { field: await task }",
            "[await task, 1]",
            "[1, await task]",
            "(await task)[0]",
            "array[await task]",
            "(await task)..9",
            "0..(await task)",
            "true ? (await task) : 1",
            "true ? 1 : (await task)",
        ];
        for expression in perspectives {
            let source = format!("async fn f() {{ let result = {expression}; }}");
            let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
            let (mut program, errors) = crate::parser::Parser::new(tokens).parse();
            assert!(errors.is_empty(), "{expression}: {errors:?}");
            let Item::Function(function) = &mut program.items[0] else {
                unreachable!()
            };
            let Stmt::Let(binding) = &mut function.body.stmts[0] else {
                unreachable!()
            };
            let expr = take_expr(&mut binding.init);
            // This unit test isolates the normalizer from name/type resolution:
            // metadata is deliberately supplied for every structural node.
            let mut types = HashMap::new();
            for event in AstWalk::new(AstEvent::Expr(&expr)) {
                if let AstEvent::Expr(expr) = event {
                    types.insert(expr.id(), Type::I64);
                }
            }
            let mut normalizer = normalizer(&mut types);
            let (prefix, result) = normalizer.normalize_expr(expr);
            assert!(!prefix.is_empty(), "{expression}");
            assert!(!normalizer.contains_suspend(&result), "{expression}");
            assert_eq!(
                prefix
                    .iter()
                    .flat_map(|stmt| AstWalk::new(AstEvent::Stmt(stmt)))
                    .filter(|event| matches!(event, AstEvent::Expr(Expr::Await(_))))
                    .count(),
                1,
                "{expression}"
            );
        }
    }

    #[test]
    fn fifty_thousand_expression_and_scope_frames_use_one_megabyte() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let span = Span::dummy();
                let mut types = HashMap::new();
                let task = Expr::Var("task".into(), span, ExprId::fresh());
                types.insert(task.id(), Type::Generic("Task".into(), vec![Type::I64]));
                let mut expr = Expr::Await(Box::new(AwaitExpr {
                    id: ExprId::fresh(),
                    expr: task,
                    span,
                }));
                types.insert(expr.id(), Type::I64);
                for _ in 0..50_000 {
                    expr = Expr::Unary(Box::new(UnaryExpr {
                        id: ExprId::fresh(),
                        op: UnaryOp::Neg,
                        expr,
                        span,
                    }));
                    types.insert(expr.id(), Type::I64);
                }
                let mut normalizer = normalizer(&mut types);
                let (prefix, expr) = normalizer.normalize_expr(expr);
                assert_eq!(prefix.len(), 1);
                assert!(!normalizer.contains_suspend(&expr));
                drop(prefix);
                drop(expr);
                let mut body = Block {
                    stmts: vec![],
                    span,
                };
                for _ in 0..50_000 {
                    let cond = Expr::Bool(true, span, ExprId::fresh());
                    normalizer.expr_types.insert(cond.id(), Type::Bool);
                    body = Block {
                        stmts: vec![Stmt::If(IfStmt {
                            cond,
                            then_block: body,
                            else_block: None,
                            span,
                        })],
                        span,
                    };
                }
                normalizer.normalize_block(&mut body);
                assert_eq!(
                    AstWalk::new(AstEvent::Block(&body))
                        .filter(|event| matches!(event, AstEvent::Stmt(Stmt::If(_))))
                        .count(),
                    50_000
                );
                drop(body);
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
