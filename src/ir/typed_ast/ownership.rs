//! Heap continuations for owned HIR trees. Keep both expression and statement
//! edges here: lambda, match and select bodies cross between the two kinds.
use super::*;

fn empty_expr() -> HirExpr {
    HirExpr {
        kind: HirExprKind::Int(0),
        ty: Type::Void,
        span: Span::dummy(),
    }
}
fn empty_stmt() -> HirStmt {
    HirStmt::Break {
        span: Span::dummy(),
    }
}
fn expr_slots(len: usize) -> Vec<HirExpr> {
    (0..len).map(|_| empty_expr()).collect()
}
fn stmt_slots(len: usize) -> Vec<HirStmt> {
    (0..len).map(|_| empty_stmt()).collect()
}

enum NodeRef<'a> {
    Expr(&'a HirExpr),
    Stmt(&'a HirStmt),
}
enum NodeMut<'a> {
    Expr(&'a mut HirExpr),
    Stmt(&'a mut HirStmt),
}
enum Owned {
    Expr(HirExpr),
    Stmt(Box<HirStmt>),
}

impl Clone for HirExpr {
    fn clone(&self) -> Self {
        let mut out = empty_expr();
        copy_tree(NodeRef::Expr(self), NodeMut::Expr(&mut out));
        out
    }
}
impl Clone for HirStmt {
    fn clone(&self) -> Self {
        let mut out = empty_stmt();
        copy_tree(NodeRef::Stmt(self), NodeMut::Stmt(&mut out));
        out
    }
}
fn copy_tree(source: NodeRef<'_>, target: NodeMut<'_>) {
    let mut pending = vec![(source, target)];
    while let Some((source, target)) = pending.pop() {
        match (source, target) {
            (NodeRef::Expr(source), NodeMut::Expr(target)) => {
                *target = shallow_expr(source);
                pending.extend(
                    expr_children(source)
                        .into_iter()
                        .zip(expr_children_mut(target)),
                );
            }
            (NodeRef::Stmt(source), NodeMut::Stmt(target)) => {
                *target = shallow_stmt(source);
                pending.extend(
                    stmt_children(source)
                        .into_iter()
                        .zip(stmt_children_mut(target)),
                );
            }
            _ => unreachable!("HIR child inventory preserves node kinds"),
        }
    }
}
impl Drop for HirExpr {
    fn drop(&mut self) {
        let mut pending = Vec::new();
        detach(expr_children_mut(self), &mut pending);
        drain(pending);
    }
}
impl Drop for HirStmt {
    fn drop(&mut self) {
        let mut pending = Vec::new();
        detach(stmt_children_mut(self), &mut pending);
        drain(pending);
    }
}
fn detach(children: Vec<NodeMut<'_>>, pending: &mut Vec<Owned>) {
    for child in children {
        pending.push(match child {
            NodeMut::Expr(expr) => Owned::Expr(std::mem::replace(expr, empty_expr())),
            NodeMut::Stmt(stmt) => Owned::Stmt(Box::new(std::mem::replace(stmt, empty_stmt()))),
        });
    }
}
fn drain(mut pending: Vec<Owned>) {
    while let Some(mut node) = pending.pop() {
        match &mut node {
            Owned::Expr(expr) => detach(expr_children_mut(expr), &mut pending),
            Owned::Stmt(stmt) => detach(stmt_children_mut(stmt), &mut pending),
        }
        // Every outgoing edge is now a leaf. Destruction of the shell cannot
        // recurse beyond its immediate children, regardless of source depth.
    }
}

fn shallow_expr(source: &HirExpr) -> HirExpr {
    let kind = match &source.kind {
        HirExprKind::Int(value) => HirExprKind::Int(*value),
        HirExprKind::Float(value) => HirExprKind::Float(*value),
        HirExprKind::Bool(value) => HirExprKind::Bool(*value),
        HirExprKind::Str(value) => HirExprKind::Str(value.clone()),
        HirExprKind::Var(value) => HirExprKind::Var(value.clone()),
        HirExprKind::FnRef(value) => HirExprKind::FnRef(value.clone()),
        HirExprKind::Binary { op, lhs: _, rhs: _ } => HirExprKind::Binary {
            op: op.clone(),
            lhs: Box::new(empty_expr()),
            rhs: Box::new(empty_expr()),
        },
        HirExprKind::Unary { op, operand: _ } => HirExprKind::Unary {
            op: op.clone(),
            operand: Box::new(empty_expr()),
        },
        HirExprKind::Call { callee, args } => HirExprKind::Call {
            callee: callee.clone(),
            args: expr_slots(args.len()),
        },
        HirExprKind::Print { value: _, newline } => HirExprKind::Print {
            value: Box::new(empty_expr()),
            newline: *newline,
        },
        HirExprKind::Array { elements } => HirExprKind::Array {
            elements: expr_slots(elements.len()),
        },
        HirExprKind::Index { array: _, index: _ } => HirExprKind::Index {
            array: Box::new(empty_expr()),
            index: Box::new(empty_expr()),
        },
        HirExprKind::Ternary {
            condition: _,
            then_expr: _,
            else_expr: _,
        } => HirExprKind::Ternary {
            condition: Box::new(empty_expr()),
            then_expr: Box::new(empty_expr()),
            else_expr: Box::new(empty_expr()),
        },
        HirExprKind::New { class, args } => HirExprKind::New {
            class: class.clone(),
            args: expr_slots(args.len()),
        },
        HirExprKind::FieldAccess { object: _, field } => HirExprKind::FieldAccess {
            object: Box::new(empty_expr()),
            field: field.clone(),
        },
        HirExprKind::MethodCall {
            object: _,
            method,
            args,
        } => HirExprKind::MethodCall {
            object: Box::new(empty_expr()),
            method: method.clone(),
            args: expr_slots(args.len()),
        },
        HirExprKind::ObjectLiteral { class, fields } => HirExprKind::ObjectLiteral {
            class: class.clone(),
            fields: fields
                .iter()
                .map(|(name, _)| (name.clone(), empty_expr()))
                .collect(),
        },
        HirExprKind::StaticField { class, field } => HirExprKind::StaticField {
            class: class.clone(),
            field: field.clone(),
        },
        HirExprKind::StaticCall {
            class,
            method,
            args,
        } => HirExprKind::StaticCall {
            class: class.clone(),
            method: method.clone(),
            args: expr_slots(args.len()),
        },
        HirExprKind::ReferenceArg { place: _ } => HirExprKind::ReferenceArg {
            place: Box::new(empty_expr()),
        },
        HirExprKind::Range { start: _, end: _ } => HirExprKind::Range {
            start: Box::new(empty_expr()),
            end: Box::new(empty_expr()),
        },
        HirExprKind::Await { inner: _ } => HirExprKind::Await {
            inner: Box::new(empty_expr()),
        },
        HirExprKind::TryPropagate { inner: _ } => HirExprKind::TryPropagate {
            inner: Box::new(empty_expr()),
        },
        HirExprKind::Lambda {
            id,
            params,
            captures,
            body,
        } => HirExprKind::Lambda {
            id: *id,
            params: params.clone(),
            captures: captures.clone(),
            body: stmt_slots(body.len()),
        },
        HirExprKind::Match { scrutinee: _, arms } => HirExprKind::Match {
            scrutinee: Box::new(empty_expr()),
            arms: arms
                .iter()
                .map(|arm| HirMatchArm {
                    pattern: arm.pattern.clone(),
                    body: stmt_slots(arm.body.len()),
                    ty: arm.ty.clone(),
                    span: arm.span,
                })
                .collect(),
        },
        HirExprKind::Select { cases } => HirExprKind::Select {
            cases: cases
                .iter()
                .map(|case| HirSelectCase {
                    kind: shallow_case(&case.kind),
                    body: stmt_slots(case.body.len()),
                    span: case.span,
                })
                .collect(),
        },
    };
    HirExpr {
        kind,
        ty: source.ty.clone(),
        span: source.span,
    }
}

fn shallow_case(source: &HirSelectCaseKind) -> HirSelectCaseKind {
    match source {
        HirSelectCaseKind::Recv { binding, .. } => HirSelectCaseKind::Recv {
            binding: binding.clone(),
            channel: empty_expr(),
        },
        HirSelectCaseKind::Send { .. } => HirSelectCaseKind::Send {
            channel: empty_expr(),
            value: empty_expr(),
        },
        HirSelectCaseKind::Timeout { .. } => HirSelectCaseKind::Timeout {
            millis: empty_expr(),
        },
        HirSelectCaseKind::Join { binding, .. } => HirSelectCaseKind::Join {
            binding: binding.clone(),
            task: empty_expr(),
        },
        HirSelectCaseKind::Default => HirSelectCaseKind::Default,
    }
}

fn shallow_stmt(source: &HirStmt) -> HirStmt {
    match source {
        HirStmt::Expr(_) => HirStmt::Expr(empty_expr()),
        HirStmt::Let {
            name,
            mutable,
            ty,
            value: _,
            span,
        } => HirStmt::Let {
            name: name.clone(),
            mutable: *mutable,
            ty: ty.clone(),
            value: empty_expr(),
            span: *span,
        },
        HirStmt::Assign {
            name,
            value: _,
            span,
        } => HirStmt::Assign {
            name: name.clone(),
            value: empty_expr(),
            span: *span,
        },
        HirStmt::If {
            cond: _,
            then_branch,
            else_branch,
            span,
        } => HirStmt::If {
            cond: empty_expr(),
            then_branch: stmt_slots(then_branch.len()),
            else_branch: else_branch.as_ref().map(|body| stmt_slots(body.len())),
            span: *span,
        },
        HirStmt::While {
            cond: _,
            body,
            span,
        } => HirStmt::While {
            cond: empty_expr(),
            body: stmt_slots(body.len()),
            span: *span,
        },
        HirStmt::Return { value, span } => HirStmt::Return {
            value: value.as_ref().map(|_| empty_expr()),
            span: *span,
        },
        HirStmt::Break { span } => HirStmt::Break { span: *span },
        HirStmt::Continue { span } => HirStmt::Continue { span: *span },
        HirStmt::Defer { id, body, span } => HirStmt::Defer {
            id: *id,
            body: match body {
                HirDeferBody::Expr(_) => HirDeferBody::Expr(empty_expr()),
                HirDeferBody::Block(body) => HirDeferBody::Block(stmt_slots(body.len())),
            },
            span: *span,
        },
        HirStmt::Lock {
            mode,
            target: _,
            binding,
            mutable,
            body,
            span,
        } => HirStmt::Lock {
            mode: *mode,
            target: empty_expr(),
            binding: binding.clone(),
            mutable: *mutable,
            body: stmt_slots(body.len()),
            span: *span,
        },
        HirStmt::For {
            name,
            iterable: _,
            body,
            span,
        } => HirStmt::For {
            name: name.clone(),
            iterable: empty_expr(),
            body: stmt_slots(body.len()),
            span: *span,
        },
        HirStmt::FieldAssign {
            object: _,
            field,
            value: _,
            span,
        } => HirStmt::FieldAssign {
            object: empty_expr(),
            field: field.clone(),
            value: empty_expr(),
            span: *span,
        },
        HirStmt::IndexAssign {
            array: _,
            index: _,
            value: _,
            span,
        } => HirStmt::IndexAssign {
            array: empty_expr(),
            index: empty_expr(),
            value: empty_expr(),
            span: *span,
        },
        HirStmt::StaticFieldAssign {
            class,
            field,
            value: _,
            span,
        } => HirStmt::StaticFieldAssign {
            class: class.clone(),
            field: field.clone(),
            value: empty_expr(),
            span: *span,
        },
        HirStmt::SuperInit { args, span } => HirStmt::SuperInit {
            args: expr_slots(args.len()),
            span: *span,
        },
    }
}

fn expr_children_mut(source: &mut HirExpr) -> Vec<NodeMut<'_>> {
    let mut out = Vec::new();
    match &mut source.kind {
        HirExprKind::Int(..)
        | HirExprKind::Float(..)
        | HirExprKind::Bool(..)
        | HirExprKind::Str(..)
        | HirExprKind::Var(..)
        | HirExprKind::FnRef(..) => {}
        HirExprKind::Binary { lhs, rhs, .. } => {
            out.push(NodeMut::Expr(lhs));
            out.push(NodeMut::Expr(rhs));
        }
        HirExprKind::Unary { operand, .. } => {
            out.push(NodeMut::Expr(operand));
        }
        HirExprKind::Call { args, .. } => {
            out.extend(args.iter_mut().map(NodeMut::Expr));
        }
        HirExprKind::Print { value, .. } => {
            out.push(NodeMut::Expr(value));
        }
        HirExprKind::Array { elements } => {
            out.extend(elements.iter_mut().map(NodeMut::Expr));
        }
        HirExprKind::Index { array, index } => {
            out.push(NodeMut::Expr(array));
            out.push(NodeMut::Expr(index));
        }
        HirExprKind::Ternary {
            condition,
            then_expr,
            else_expr,
        } => {
            out.push(NodeMut::Expr(condition));
            out.push(NodeMut::Expr(then_expr));
            out.push(NodeMut::Expr(else_expr));
        }
        HirExprKind::New { args, .. } => {
            out.extend(args.iter_mut().map(NodeMut::Expr));
        }
        HirExprKind::FieldAccess { object, .. } => {
            out.push(NodeMut::Expr(object));
        }
        HirExprKind::MethodCall { object, args, .. } => {
            out.push(NodeMut::Expr(object));
            out.extend(args.iter_mut().map(NodeMut::Expr));
        }
        HirExprKind::ObjectLiteral { fields, .. } => {
            out.extend(fields.iter_mut().map(|(_, e)| NodeMut::Expr(e)));
        }
        HirExprKind::StaticField { .. } => {}
        HirExprKind::StaticCall { args, .. } => {
            out.extend(args.iter_mut().map(NodeMut::Expr));
        }
        HirExprKind::ReferenceArg { place } => {
            out.push(NodeMut::Expr(place));
        }
        HirExprKind::Range { start, end } => {
            out.push(NodeMut::Expr(start));
            out.push(NodeMut::Expr(end));
        }
        HirExprKind::Await { inner } => {
            out.push(NodeMut::Expr(inner));
        }
        HirExprKind::TryPropagate { inner } => {
            out.push(NodeMut::Expr(inner));
        }
        HirExprKind::Lambda { body, .. } => {
            out.extend(body.iter_mut().map(NodeMut::Stmt));
        }
        HirExprKind::Match { scrutinee, arms } => {
            out.push(NodeMut::Expr(scrutinee));
            for arm in arms {
                out.extend(arm.body.iter_mut().map(NodeMut::Stmt));
            }
        }
        HirExprKind::Select { cases } => {
            for case in cases {
                match &mut case.kind {
                    HirSelectCaseKind::Recv { channel, .. } => out.push(NodeMut::Expr(channel)),
                    HirSelectCaseKind::Send { channel, value } => {
                        out.push(NodeMut::Expr(channel));
                        out.push(NodeMut::Expr(value));
                    }
                    HirSelectCaseKind::Timeout { millis } => out.push(NodeMut::Expr(millis)),
                    HirSelectCaseKind::Join { task, .. } => out.push(NodeMut::Expr(task)),
                    HirSelectCaseKind::Default => {}
                }
                out.extend(case.body.iter_mut().map(NodeMut::Stmt));
            }
        }
    }
    out
}

fn expr_children(source: &HirExpr) -> Vec<NodeRef<'_>> {
    let mut out = Vec::new();
    match &source.kind {
        HirExprKind::Int(..)
        | HirExprKind::Float(..)
        | HirExprKind::Bool(..)
        | HirExprKind::Str(..)
        | HirExprKind::Var(..)
        | HirExprKind::FnRef(..) => {}
        HirExprKind::Binary { lhs, rhs, .. } => {
            out.push(NodeRef::Expr(lhs));
            out.push(NodeRef::Expr(rhs));
        }
        HirExprKind::Unary { operand, .. } => {
            out.push(NodeRef::Expr(operand));
        }
        HirExprKind::Call { args, .. } => {
            out.extend(args.iter().map(NodeRef::Expr));
        }
        HirExprKind::Print { value, .. } => {
            out.push(NodeRef::Expr(value));
        }
        HirExprKind::Array { elements } => {
            out.extend(elements.iter().map(NodeRef::Expr));
        }
        HirExprKind::Index { array, index } => {
            out.push(NodeRef::Expr(array));
            out.push(NodeRef::Expr(index));
        }
        HirExprKind::Ternary {
            condition,
            then_expr,
            else_expr,
        } => {
            out.push(NodeRef::Expr(condition));
            out.push(NodeRef::Expr(then_expr));
            out.push(NodeRef::Expr(else_expr));
        }
        HirExprKind::New { args, .. } => {
            out.extend(args.iter().map(NodeRef::Expr));
        }
        HirExprKind::FieldAccess { object, .. } => {
            out.push(NodeRef::Expr(object));
        }
        HirExprKind::MethodCall { object, args, .. } => {
            out.push(NodeRef::Expr(object));
            out.extend(args.iter().map(NodeRef::Expr));
        }
        HirExprKind::ObjectLiteral { fields, .. } => {
            out.extend(fields.iter().map(|(_, e)| NodeRef::Expr(e)));
        }
        HirExprKind::StaticField { .. } => {}
        HirExprKind::StaticCall { args, .. } => {
            out.extend(args.iter().map(NodeRef::Expr));
        }
        HirExprKind::ReferenceArg { place } => {
            out.push(NodeRef::Expr(place));
        }
        HirExprKind::Range { start, end } => {
            out.push(NodeRef::Expr(start));
            out.push(NodeRef::Expr(end));
        }
        HirExprKind::Await { inner } => {
            out.push(NodeRef::Expr(inner));
        }
        HirExprKind::TryPropagate { inner } => {
            out.push(NodeRef::Expr(inner));
        }
        HirExprKind::Lambda { body, .. } => {
            out.extend(body.iter().map(NodeRef::Stmt));
        }
        HirExprKind::Match { scrutinee, arms } => {
            out.push(NodeRef::Expr(scrutinee));
            for arm in arms {
                out.extend(arm.body.iter().map(NodeRef::Stmt));
            }
        }
        HirExprKind::Select { cases } => {
            for case in cases {
                match &case.kind {
                    HirSelectCaseKind::Recv { channel, .. } => out.push(NodeRef::Expr(channel)),
                    HirSelectCaseKind::Send { channel, value } => {
                        out.push(NodeRef::Expr(channel));
                        out.push(NodeRef::Expr(value));
                    }
                    HirSelectCaseKind::Timeout { millis } => out.push(NodeRef::Expr(millis)),
                    HirSelectCaseKind::Join { task, .. } => out.push(NodeRef::Expr(task)),
                    HirSelectCaseKind::Default => {}
                }
                out.extend(case.body.iter().map(NodeRef::Stmt));
            }
        }
    }
    out
}

fn stmt_children_mut(source: &mut HirStmt) -> Vec<NodeMut<'_>> {
    let mut out = Vec::new();
    match source {
        HirStmt::Expr(expr) => out.push(NodeMut::Expr(expr)),
        HirStmt::Let { value, .. } => {
            out.push(NodeMut::Expr(value));
        }
        HirStmt::Assign { value, .. } => {
            out.push(NodeMut::Expr(value));
        }
        HirStmt::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            out.push(NodeMut::Expr(cond));
            out.extend(then_branch.iter_mut().map(NodeMut::Stmt));
            if let Some(body) = else_branch {
                out.extend(body.iter_mut().map(NodeMut::Stmt));
            }
        }
        HirStmt::While { cond, body, .. } => {
            out.push(NodeMut::Expr(cond));
            out.extend(body.iter_mut().map(NodeMut::Stmt));
        }
        HirStmt::Return { value, .. } => {
            out.extend(value.iter_mut().map(NodeMut::Expr));
        }
        HirStmt::Break { .. } => {}
        HirStmt::Continue { .. } => {}
        HirStmt::Defer { body, .. } => match body {
            HirDeferBody::Expr(e) => out.push(NodeMut::Expr(e)),
            HirDeferBody::Block(body) => out.extend(body.iter_mut().map(NodeMut::Stmt)),
        },
        HirStmt::Lock { target, body, .. } => {
            out.push(NodeMut::Expr(target));
            out.extend(body.iter_mut().map(NodeMut::Stmt));
        }
        HirStmt::For { iterable, body, .. } => {
            out.push(NodeMut::Expr(iterable));
            out.extend(body.iter_mut().map(NodeMut::Stmt));
        }
        HirStmt::FieldAssign { object, value, .. } => {
            out.push(NodeMut::Expr(object));
            out.push(NodeMut::Expr(value));
        }
        HirStmt::IndexAssign {
            array,
            index,
            value,
            ..
        } => {
            out.push(NodeMut::Expr(array));
            out.push(NodeMut::Expr(index));
            out.push(NodeMut::Expr(value));
        }
        HirStmt::StaticFieldAssign { value, .. } => {
            out.push(NodeMut::Expr(value));
        }
        HirStmt::SuperInit { args, .. } => {
            out.extend(args.iter_mut().map(NodeMut::Expr));
        }
    }
    out
}

fn stmt_children(source: &HirStmt) -> Vec<NodeRef<'_>> {
    let mut out = Vec::new();
    match source {
        HirStmt::Expr(expr) => out.push(NodeRef::Expr(expr)),
        HirStmt::Let { value, .. } => {
            out.push(NodeRef::Expr(value));
        }
        HirStmt::Assign { value, .. } => {
            out.push(NodeRef::Expr(value));
        }
        HirStmt::If {
            cond,
            then_branch,
            else_branch,
            ..
        } => {
            out.push(NodeRef::Expr(cond));
            out.extend(then_branch.iter().map(NodeRef::Stmt));
            if let Some(body) = else_branch {
                out.extend(body.iter().map(NodeRef::Stmt));
            }
        }
        HirStmt::While { cond, body, .. } => {
            out.push(NodeRef::Expr(cond));
            out.extend(body.iter().map(NodeRef::Stmt));
        }
        HirStmt::Return { value, .. } => {
            out.extend(value.iter().map(NodeRef::Expr));
        }
        HirStmt::Break { .. } => {}
        HirStmt::Continue { .. } => {}
        HirStmt::Defer { body, .. } => match body {
            HirDeferBody::Expr(e) => out.push(NodeRef::Expr(e)),
            HirDeferBody::Block(body) => out.extend(body.iter().map(NodeRef::Stmt)),
        },
        HirStmt::Lock { target, body, .. } => {
            out.push(NodeRef::Expr(target));
            out.extend(body.iter().map(NodeRef::Stmt));
        }
        HirStmt::For { iterable, body, .. } => {
            out.push(NodeRef::Expr(iterable));
            out.extend(body.iter().map(NodeRef::Stmt));
        }
        HirStmt::FieldAssign { object, value, .. } => {
            out.push(NodeRef::Expr(object));
            out.push(NodeRef::Expr(value));
        }
        HirStmt::IndexAssign {
            array,
            index,
            value,
            ..
        } => {
            out.push(NodeRef::Expr(array));
            out.push(NodeRef::Expr(index));
            out.push(NodeRef::Expr(value));
        }
        HirStmt::StaticFieldAssign { value, .. } => {
            out.push(NodeRef::Expr(value));
        }
        HirStmt::SuperInit { args, .. } => {
            out.extend(args.iter().map(NodeRef::Expr));
        }
    }
    out
}

impl HirExpr {
    /// Rewrite a tree before its children with heap-owned continuation slots.
    /// Returning false prunes the replaced subtree. When scoped bodies are
    /// excluded, match scrutinees remain visible but their statements do not.
    pub(crate) fn visit_mut_preorder(
        &mut self,
        include_scoped_bodies: bool,
        mut visit: impl FnMut(&mut HirExpr) -> bool,
    ) {
        let mut pending = vec![NodeMut::Expr(self)];
        while let Some(node) = pending.pop() {
            let children = match node {
                NodeMut::Expr(expr) => {
                    if !visit(expr) {
                        continue;
                    }
                    expr_children_mut(expr)
                }
                NodeMut::Stmt(stmt) if include_scoped_bodies => stmt_children_mut(stmt),
                NodeMut::Stmt(_) => continue,
            };
            pending.extend(children.into_iter().rev());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf() -> HirExpr {
        HirExpr {
            kind: HirExprKind::Int(17),
            ty: Type::I64,
            span: Span::dummy(),
        }
    }
    fn wrap(child: HirExpr, perspective: usize) -> HirExpr {
        let span = Span::dummy();
        let kind = match perspective {
            0 => HirExprKind::Binary {
                op: BinOp::Add,
                lhs: Box::new(child),
                rhs: Box::new(leaf()),
            },
            1 => HirExprKind::Binary {
                op: BinOp::Add,
                lhs: Box::new(leaf()),
                rhs: Box::new(child),
            },
            2 => HirExprKind::Unary {
                op: UnaryOp::Neg,
                operand: Box::new(child),
            },
            3 => HirExprKind::Call {
                callee: "f".into(),
                args: vec![leaf(), child],
            },
            4 => HirExprKind::Print {
                value: Box::new(child),
                newline: true,
            },
            5 => HirExprKind::Array {
                elements: vec![child, leaf()],
            },
            6 => HirExprKind::Index {
                array: Box::new(child),
                index: Box::new(leaf()),
            },
            7 => HirExprKind::Ternary {
                condition: Box::new(leaf()),
                then_expr: Box::new(child),
                else_expr: Box::new(leaf()),
            },
            8 => HirExprKind::New {
                class: "C".into(),
                args: vec![child],
            },
            9 => HirExprKind::FieldAccess {
                object: Box::new(child),
                field: "field".into(),
            },
            10 => HirExprKind::MethodCall {
                object: Box::new(child),
                method: "method".into(),
                args: vec![leaf()],
            },
            11 => HirExprKind::ObjectLiteral {
                class: "C".into(),
                fields: vec![("field".into(), child)],
            },
            12 => HirExprKind::StaticCall {
                class: "C".into(),
                method: "method".into(),
                args: vec![child],
            },
            13 => HirExprKind::ReferenceArg {
                place: Box::new(child),
            },
            14 => HirExprKind::Range {
                start: Box::new(leaf()),
                end: Box::new(child),
            },
            15 => HirExprKind::Await {
                inner: Box::new(child),
            },
            16 => HirExprKind::TryPropagate {
                inner: Box::new(child),
            },
            17 => HirExprKind::Lambda {
                id: ExprId::fresh(),
                params: vec![],
                captures: vec![],
                body: vec![HirStmt::Return {
                    value: Some(child),
                    span,
                }],
            },
            18 => HirExprKind::Match {
                scrutinee: Box::new(leaf()),
                arms: vec![HirMatchArm {
                    pattern: HirPattern::Wildcard,
                    body: vec![HirStmt::Expr(child)],
                    ty: Type::I64,
                    span,
                }],
            },
            19 => HirExprKind::Select {
                cases: vec![HirSelectCase {
                    kind: HirSelectCaseKind::Recv {
                        binding: "v".into(),
                        channel: child,
                    },
                    body: vec![HirStmt::Expr(leaf())],
                    span,
                }],
            },
            20 => HirExprKind::Select {
                cases: vec![HirSelectCase {
                    kind: HirSelectCaseKind::Send {
                        channel: leaf(),
                        value: child,
                    },
                    body: vec![],
                    span,
                }],
            },
            21 => HirExprKind::Select {
                cases: vec![HirSelectCase {
                    kind: HirSelectCaseKind::Timeout { millis: child },
                    body: vec![],
                    span,
                }],
            },
            22 => HirExprKind::Select {
                cases: vec![HirSelectCase {
                    kind: HirSelectCaseKind::Join {
                        binding: "v".into(),
                        task: child,
                    },
                    body: vec![],
                    span,
                }],
            },
            23 => HirExprKind::Select {
                cases: vec![HirSelectCase {
                    kind: HirSelectCaseKind::Default,
                    body: vec![HirStmt::Defer {
                        id: HirDeferId(7),
                        body: HirDeferBody::Block(vec![HirStmt::Expr(child)]),
                        span,
                    }],
                    span,
                }],
            },
            _ => unreachable!(),
        };
        HirExpr {
            kind,
            ty: Type::I64,
            span,
        }
    }

    #[test]
    fn clone_preserves_twenty_four_expression_and_scope_perspectives() {
        let perspectives = [
            "binary lhs",
            "binary rhs",
            "unary",
            "call arguments",
            "print",
            "array elements",
            "index receiver",
            "ternary branch",
            "constructor arguments",
            "field receiver",
            "method receiver",
            "object field",
            "static call arguments",
            "reference place",
            "range end",
            "await",
            "try propagation",
            "lambda return",
            "match arm",
            "select receive",
            "select send",
            "select timeout",
            "select join",
            "select deferred body",
        ];
        for (index, perspective) in perspectives.into_iter().enumerate() {
            let original = wrap(wrap(leaf(), (index + 1) % 24), index);
            assert_eq!(original, original.clone(), "{perspective}");
        }
    }

    #[test]
    fn deep_mixed_clone_rewrite_and_drop_use_one_megabyte_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let mut original = leaf();
                for index in 0..50_000 {
                    original = wrap(original, index % 24);
                }
                let expected = original.walk_postorder(true).count();
                let mut copy = original.clone();
                assert_eq!(copy.walk_postorder(true).count(), expected);
                let mut visits = 0;
                copy.visit_mut_preorder(true, |expr| {
                    visits += 1;
                    if let HirExprKind::Int(value) = &mut expr.kind {
                        *value += 1;
                    }
                    true
                });
                assert_eq!(visits, expected);
                assert!(
                    copy.walk_postorder(true)
                        .filter_map(|e| match e.kind {
                            HirExprKind::Int(v) => Some(v),
                            _ => None,
                        })
                        .all(|v| v == 18)
                );
                drop(copy);
                drop(original);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn deep_statement_only_clone_and_drop_use_one_megabyte_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let mut stmt = HirStmt::Return {
                    value: Some(leaf()),
                    span: Span::dummy(),
                };
                for _ in 0..50_000 {
                    stmt = HirStmt::If {
                        cond: leaf(),
                        then_branch: vec![stmt],
                        else_branch: None,
                        span: Span::dummy(),
                    };
                }
                let copied = stmt.clone();
                assert_eq!(stmt.child_exprs().len(), 50_001);
                assert_eq!(copied.child_exprs().len(), 50_001);
                drop(stmt);
                drop(copied);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn rewriting_can_prune_replaced_subtrees_and_scoped_bodies() {
        let mut tree = wrap(wrap(leaf(), 18), 0);
        let mut visits = 0;
        tree.visit_mut_preorder(false, |_| {
            visits += 1;
            true
        });
        assert_eq!(visits, 4); // Binary, match, scrutinee, sibling; arm body excluded.
        tree.visit_mut_preorder(true, |expr| {
            if matches!(expr.kind, HirExprKind::Match { .. }) {
                *expr = leaf();
                return false;
            }
            true
        });
        assert_eq!(tree.walk_postorder(true).count(), 3);
    }
}
