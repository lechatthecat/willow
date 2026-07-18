//! A-normalization for scheduler-suspending `recv`/`join` expressions.
//!
//! The poll state machine can only resume at statement boundaries with values
//! stored in its heap frame. Hoisting nested suspension points into explicitly
//! typed `let` statements gives each value such a frame slot and preserves
//! left-to-right evaluation (including short-circuit and ternary control flow).

use std::collections::HashMap;

use crate::diagnostics::{FileId, Span};
use crate::parser::ast::*;

use super::{channel_element_type, join_handle_result_type};

pub(crate) fn normalize_coop_suspensions(
    program: &Program,
    expr_types: &HashMap<Span, Type>,
) -> Program {
    let mut program = program.clone();
    let mut normalizer = Normalizer {
        expr_types,
        next_temp: 0,
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
    expr_types: &'a HashMap<Span, Type>,
    next_temp: usize,
}

impl Normalizer<'_> {
    fn synthetic(&mut self, source: Span) -> (String, Span) {
        let index = self.next_temp;
        self.next_temp += 1;
        (
            format!("__willow$suspend${index}"),
            Span::in_file(
                FileId(source.file_id.0 ^ 0x8000_0000),
                index.saturating_mul(2),
                index.saturating_mul(2).saturating_add(1),
                source.line,
                source.col,
            ),
        )
    }

    fn ty(&self, expr: &Expr) -> Type {
        self.expr_types
            .get(&expr.span())
            .cloned()
            .unwrap_or(Type::I64)
    }

    fn bind(&mut self, prefix: &mut Vec<Stmt>, expr: Expr, ty: Type) -> Expr {
        if ty == Type::Void {
            return expr;
        }
        // These are already stable/repeatable values. In particular, avoiding a
        // duplicate frame slot for every JoinHandle variable keeps large async
        // functions within the frame's 62-reference mask capacity.
        if matches!(
            expr,
            Expr::Var(..)
                | Expr::Integer(..)
                | Expr::Float(..)
                | Expr::Bool(..)
                | Expr::Nil(..)
                | Expr::String(..)
        ) {
            return expr;
        }
        let (name, span) = self.synthetic(expr.span());
        prefix.push(Stmt::Let(LetStmt {
            name: name.clone(),
            mutable: false,
            ty: Some(ty),
            init: expr,
            span,
        }));
        Expr::Var(name, span)
    }

    fn direct_suspend_type(&self, expr: &Expr) -> Option<Type> {
        let Expr::MethodCall(method) = expr else {
            return None;
        };
        if !method.args.is_empty() {
            return None;
        }
        let receiver_ty = self.expr_types.get(&method.object.span())?;
        match method.method.as_str() {
            "recv" => channel_element_type(receiver_ty),
            "join" | "try_join" if join_handle_result_type(receiver_ty).is_some() => {
                Some(self.ty(expr))
            }
            _ => None,
        }
    }

    fn contains_suspend(&self, expr: &Expr) -> bool {
        if self.direct_suspend_type(expr).is_some() {
            return true;
        }
        match expr {
            Expr::Binary(binary) => {
                self.contains_suspend(&binary.lhs) || self.contains_suspend(&binary.rhs)
            }
            Expr::Unary(unary) => self.contains_suspend(&unary.expr),
            Expr::Call(call) => call.args.iter().any(|arg| self.contains_suspend(&arg.expr)),
            Expr::FieldAccess(object, ..) => self.contains_suspend(object),
            Expr::MethodCall(call) => {
                self.contains_suspend(&call.object)
                    || call.args.iter().any(|arg| self.contains_suspend(&arg.expr))
            }
            Expr::StaticCall(call) => call.args.iter().any(|arg| self.contains_suspend(&arg.expr)),
            Expr::New(new) => new.args.iter().any(|arg| self.contains_suspend(&arg.expr)),
            Expr::ObjectLiteral(object) => object
                .fields
                .iter()
                .any(|field| self.contains_suspend(&field.value)),
            Expr::Print(value, ..) => self.contains_suspend(value),
            Expr::Ternary(ternary) => {
                self.contains_suspend(&ternary.condition)
                    || self.contains_suspend(&ternary.then_expr)
                    || self.contains_suspend(&ternary.else_expr)
            }
            Expr::Range(range) => {
                self.contains_suspend(&range.start) || self.contains_suspend(&range.end)
            }
            Expr::Match(match_expr) => {
                self.contains_suspend(&match_expr.scrutinee)
                    || match_expr.arms.iter().any(|arm| match &arm.body {
                        MatchBody::Expr(expr) => self.contains_suspend(expr),
                        MatchBody::Block(block) => block
                            .stmts
                            .iter()
                            .any(|stmt| self.stmt_contains_suspend(stmt)),
                    })
            }
            Expr::TryPropagate(inner, _) => self.contains_suspend(inner),
            Expr::ArrayLiteral(elements, _) => {
                elements.iter().any(|expr| self.contains_suspend(expr))
            }
            Expr::Index(array, index, _) => {
                self.contains_suspend(array) || self.contains_suspend(index)
            }
            // An await already has its own lowering and lambdas are separate
            // functions/scopes; do not move their internals into the caller.
            Expr::Await(_)
            | Expr::Lambda(_)
            | Expr::Select(_)
            | Expr::Integer(..)
            | Expr::Float(..)
            | Expr::Bool(..)
            | Expr::Nil(..)
            | Expr::String(..)
            | Expr::Var(..)
            | Expr::StaticField(_) => false,
        }
    }

    fn stmt_contains_suspend(&self, stmt: &Stmt) -> bool {
        match stmt {
            Stmt::Let(stmt) => self.contains_suspend(&stmt.init),
            Stmt::Assign(stmt) => self.contains_suspend(&stmt.value),
            Stmt::FieldAssign(stmt) => {
                self.contains_suspend(&stmt.object) || self.contains_suspend(&stmt.value)
            }
            Stmt::SuperInit(stmt) => stmt.args.iter().any(|arg| self.contains_suspend(&arg.expr)),
            Stmt::StaticFieldAssign(stmt) => self.contains_suspend(&stmt.value),
            Stmt::IndexAssign(stmt) => {
                self.contains_suspend(&stmt.array)
                    || self.contains_suspend(&stmt.index)
                    || self.contains_suspend(&stmt.value)
            }
            Stmt::If(stmt) => self.contains_suspend(&stmt.cond),
            Stmt::While(stmt) => self.contains_suspend(&stmt.cond),
            Stmt::For(stmt) => self.contains_suspend(&stmt.iterable),
            Stmt::Return(stmt) => stmt
                .value
                .as_ref()
                .is_some_and(|expr| self.contains_suspend(expr)),
            Stmt::Expr(stmt) => self.contains_suspend(&stmt.expr),
            Stmt::Defer(stmt) => self.contains_suspend(&stmt.call),
            Stmt::Break(_) | Stmt::Continue(_) => false,
        }
    }

    fn normalize_block(&mut self, block: &mut Block) {
        let mut output = Vec::new();
        for stmt in std::mem::take(&mut block.stmts) {
            self.normalize_stmt(stmt, &mut output);
        }
        block.stmts = output;
    }

    fn normalize_stmt(&mut self, stmt: Stmt, output: &mut Vec<Stmt>) {
        match stmt {
            Stmt::Let(mut stmt) => {
                let (mut prefix, value) = self.normalize_expr(stmt.init);
                stmt.init = value;
                output.append(&mut prefix);
                output.push(Stmt::Let(stmt));
            }
            Stmt::Assign(mut stmt) => {
                let (mut prefix, value) = self.normalize_expr(stmt.value);
                stmt.value = value;
                output.append(&mut prefix);
                output.push(Stmt::Assign(stmt));
            }
            Stmt::StaticFieldAssign(mut stmt) => {
                let (mut prefix, value) = self.normalize_expr(stmt.value);
                stmt.value = value;
                output.append(&mut prefix);
                output.push(Stmt::StaticFieldAssign(stmt));
            }
            Stmt::FieldAssign(mut stmt) => {
                let (mut object_prefix, object) = self.normalize_expr(stmt.object);
                let object = if self.contains_suspend(&stmt.value) {
                    let ty = self.ty(&object);
                    self.bind(&mut object_prefix, object, ty)
                } else {
                    object
                };
                let (mut value_prefix, value) = self.normalize_expr(stmt.value);
                stmt.object = object;
                stmt.value = value;
                output.append(&mut object_prefix);
                output.append(&mut value_prefix);
                output.push(Stmt::FieldAssign(stmt));
            }
            Stmt::IndexAssign(mut stmt) => {
                let has_suspend =
                    self.contains_suspend(&stmt.index) || self.contains_suspend(&stmt.value);
                let (mut prefix, array) = self.normalize_expr(stmt.array);
                let array = if has_suspend {
                    let ty = self.ty(&array);
                    self.bind(&mut prefix, array, ty)
                } else {
                    array
                };
                let (mut index_prefix, index) = self.normalize_expr(stmt.index);
                prefix.append(&mut index_prefix);
                let index = if self.contains_suspend(&stmt.value) {
                    let ty = self.ty(&index);
                    self.bind(&mut prefix, index, ty)
                } else {
                    index
                };
                let (mut value_prefix, value) = self.normalize_expr(stmt.value);
                prefix.append(&mut value_prefix);
                stmt.array = array;
                stmt.index = index;
                stmt.value = value;
                output.append(&mut prefix);
                output.push(Stmt::IndexAssign(stmt));
            }
            Stmt::SuperInit(mut stmt) => {
                let mut prefix = Vec::new();
                for arg in &mut stmt.args {
                    let (mut arg_prefix, value) = self.normalize_expr(arg.expr.clone());
                    prefix.append(&mut arg_prefix);
                    arg.expr = value;
                }
                output.append(&mut prefix);
                output.push(Stmt::SuperInit(stmt));
            }
            Stmt::If(mut stmt) => {
                let (mut prefix, cond) = self.normalize_expr(stmt.cond);
                stmt.cond = cond;
                self.normalize_block(&mut stmt.then_block);
                if let Some(else_block) = &mut stmt.else_block {
                    self.normalize_block(else_block);
                }
                output.append(&mut prefix);
                output.push(Stmt::If(stmt));
            }
            Stmt::While(mut stmt) => {
                let (mut cond_prefix, cond) = self.normalize_expr(stmt.cond);
                self.normalize_block(&mut stmt.body);
                if cond_prefix.is_empty() {
                    stmt.cond = cond;
                    output.push(Stmt::While(stmt));
                } else {
                    let not_cond = Expr::Unary(Box::new(UnaryExpr {
                        op: UnaryOp::Not,
                        span: cond.span(),
                        expr: cond,
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
                    stmt.cond = Expr::Bool(true, stmt.span);
                    stmt.body.stmts = cond_prefix;
                    output.push(Stmt::While(stmt));
                }
            }
            Stmt::For(mut stmt) => {
                let (mut prefix, iterable) = self.normalize_expr(stmt.iterable);
                stmt.iterable = iterable;
                self.normalize_block(&mut stmt.body);
                output.append(&mut prefix);
                output.push(Stmt::For(stmt));
            }
            Stmt::Return(mut stmt) => {
                if let Some(value) = stmt.value.take() {
                    let (mut prefix, value) = self.normalize_expr(value);
                    stmt.value = Some(value);
                    output.append(&mut prefix);
                }
                output.push(Stmt::Return(stmt));
            }
            Stmt::Expr(mut stmt) => {
                let (mut prefix, expr) = self.normalize_expr(stmt.expr);
                stmt.expr = expr;
                output.append(&mut prefix);
                output.push(Stmt::Expr(stmt));
            }
            Stmt::Defer(mut stmt) => {
                let (mut prefix, call) = self.normalize_expr(stmt.call);
                stmt.call = call;
                output.append(&mut prefix);
                output.push(Stmt::Defer(stmt));
            }
            Stmt::Break(_) | Stmt::Continue(_) => output.push(stmt),
        }
    }

    fn normalize_expr(&mut self, expr: Expr) -> (Vec<Stmt>, Expr) {
        if !self.contains_suspend(&expr) {
            return (Vec::new(), expr);
        }
        if let Some(result_ty) = self.direct_suspend_type(&expr) {
            let Expr::MethodCall(mut method) = expr else {
                unreachable!()
            };
            let (mut prefix, receiver) = self.normalize_expr(method.object);
            let receiver_ty = self.ty(&receiver);
            method.object = self.bind(&mut prefix, receiver, receiver_ty);
            let method_expr = Expr::MethodCall(method);
            let value = self.bind(&mut prefix, method_expr, result_ty);
            return (prefix, value);
        }

        match expr {
            Expr::Binary(binary) if matches!(binary.op, BinOp::And | BinOp::Or) => {
                self.normalize_short_circuit(*binary)
            }
            Expr::Binary(mut binary) => {
                let lhs_ty = self.ty(&binary.lhs);
                let rhs_ty = self.ty(&binary.rhs);
                let (mut prefix, lhs) = self.normalize_expr(binary.lhs);
                binary.lhs = self.bind(&mut prefix, lhs, lhs_ty);
                let (mut rhs_prefix, rhs) = self.normalize_expr(binary.rhs);
                prefix.append(&mut rhs_prefix);
                binary.rhs = self.bind(&mut prefix, rhs, rhs_ty);
                (prefix, Expr::Binary(binary))
            }
            Expr::Unary(mut unary) => {
                let (prefix, value) = self.normalize_expr(unary.expr);
                unary.expr = value;
                (prefix, Expr::Unary(unary))
            }
            Expr::Print(value, newline, span) => {
                let (prefix, value) = self.normalize_expr(*value);
                (prefix, Expr::Print(Box::new(value), newline, span))
            }
            Expr::Call(mut call) => {
                let prefix = self.normalize_args(&mut call.args);
                (prefix, Expr::Call(call))
            }
            Expr::MethodCall(mut call) => {
                let receiver_ty = self.ty(&call.object);
                let (mut prefix, receiver) = self.normalize_expr(call.object);
                call.object = self.bind(&mut prefix, receiver, receiver_ty);
                let mut args = self.normalize_args(&mut call.args);
                prefix.append(&mut args);
                (prefix, Expr::MethodCall(call))
            }
            Expr::StaticCall(mut call) => {
                let prefix = self.normalize_args(&mut call.args);
                (prefix, Expr::StaticCall(call))
            }
            Expr::New(mut new) => {
                let prefix = self.normalize_args(&mut new.args);
                (prefix, Expr::New(new))
            }
            Expr::FieldAccess(object, field, span) => {
                let (prefix, object) = self.normalize_expr(*object);
                (prefix, Expr::FieldAccess(Box::new(object), field, span))
            }
            Expr::ObjectLiteral(mut object) => {
                let mut prefix = Vec::new();
                for field in &mut object.fields {
                    let ty = self.ty(&field.value);
                    let (mut field_prefix, value) = self.normalize_expr(field.value.clone());
                    prefix.append(&mut field_prefix);
                    field.value = self.bind(&mut prefix, value, ty);
                }
                (prefix, Expr::ObjectLiteral(object))
            }
            Expr::Ternary(ternary) => self.normalize_ternary(*ternary),
            Expr::Range(mut range) => {
                let start_ty = self.ty(&range.start);
                let end_ty = self.ty(&range.end);
                let (mut prefix, start) = self.normalize_expr(range.start);
                range.start = self.bind(&mut prefix, start, start_ty);
                let (mut end_prefix, end) = self.normalize_expr(range.end);
                prefix.append(&mut end_prefix);
                range.end = self.bind(&mut prefix, end, end_ty);
                (prefix, Expr::Range(range))
            }
            Expr::Match(mut match_expr) => {
                let scrutinee_ty = self.ty(&match_expr.scrutinee);
                let (mut prefix, scrutinee) = self.normalize_expr(*match_expr.scrutinee);
                match_expr.scrutinee = Box::new(self.bind(&mut prefix, scrutinee, scrutinee_ty));
                for arm in &mut match_expr.arms {
                    if let MatchBody::Block(block) = &mut arm.body {
                        self.normalize_block(block);
                    }
                }
                (prefix, Expr::Match(match_expr))
            }
            Expr::TryPropagate(inner, span) => {
                let (prefix, inner) = self.normalize_expr(*inner);
                (prefix, Expr::TryPropagate(Box::new(inner), span))
            }
            Expr::ArrayLiteral(mut elements, span) => {
                let mut prefix = Vec::new();
                for element in &mut elements {
                    let ty = self.ty(element);
                    let (mut element_prefix, value) = self.normalize_expr(element.clone());
                    prefix.append(&mut element_prefix);
                    *element = self.bind(&mut prefix, value, ty);
                }
                (prefix, Expr::ArrayLiteral(elements, span))
            }
            Expr::Index(array, index, span) => {
                let array_ty = self.ty(&array);
                let index_ty = self.ty(&index);
                let (mut prefix, array) = self.normalize_expr(*array);
                let array = self.bind(&mut prefix, array, array_ty);
                let (mut index_prefix, index) = self.normalize_expr(*index);
                prefix.append(&mut index_prefix);
                let index = self.bind(&mut prefix, index, index_ty);
                (prefix, Expr::Index(Box::new(array), Box::new(index), span))
            }
            other => (Vec::new(), other),
        }
    }

    fn normalize_args(&mut self, args: &mut [CallArg]) -> Vec<Stmt> {
        let mut prefix = Vec::new();
        for arg in args {
            let ty = self.ty(&arg.expr);
            let (mut arg_prefix, value) = self.normalize_expr(arg.expr.clone());
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
            ty: Some(Type::Bool),
            init: lhs,
            span: result_span,
        }));
        let result_var = Expr::Var(name.clone(), result_span);
        let cond = if op == BinOp::And {
            result_var.clone()
        } else {
            Expr::Unary(Box::new(UnaryExpr {
                op: UnaryOp::Not,
                expr: result_var.clone(),
                span,
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
            ty: Some(result_ty.clone()),
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
        (prefix, Expr::Var(name, result_span))
    }
}

fn default_value(ty: &Type, span: Span) -> Expr {
    match ty {
        Type::Bool => Expr::Bool(false, span),
        Type::F64 => Expr::Float(0.0, span),
        Type::I64 => Expr::Integer(0, span),
        _ => Expr::Nil(span),
    }
}
