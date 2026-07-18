//! AST analysis helpers for the type checker (extracted from `mod.rs`):
//! control-flow "always returns" checks, sub-expression walking, and
//! constructor self-field / super-init collection. Re-exported from `mod.rs`.

use std::collections::HashSet;

use crate::diagnostics::Span;
use crate::parser::ast::*;

/// Collect the names of fields assigned via `self.field = ...` anywhere in the
/// block (willow-scq2 §8 definite-assignment, MVP non-path-sensitive).
pub(crate) fn collect_self_field_assigns(block: &Block, out: &mut HashSet<String>) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Defer(_) => {}
            Stmt::Break(_) | Stmt::Continue(_) => {}
            Stmt::FieldAssign(fa) => {
                if matches!(&fa.object, Expr::Var(name, _) if name == "self") {
                    out.insert(fa.field.clone());
                }
            }
            Stmt::If(s) => {
                collect_self_field_assigns(&s.then_block, out);
                if let Some(e) = &s.else_block {
                    collect_self_field_assigns(e, out);
                }
            }
            Stmt::While(s) => collect_self_field_assigns(&s.body, out),
            Stmt::For(s) => collect_self_field_assigns(&s.body, out),
            Stmt::Let(_)
            | Stmt::Assign(_)
            | Stmt::SuperInit(_)
            | Stmt::StaticFieldAssign(_)
            | Stmt::IndexAssign(_)
            | Stmt::Return(_)
            | Stmt::Expr(_) => {}
        }
    }
}

pub(crate) fn collect_super_init_spans(block: &Block, out: &mut Vec<Span>) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Defer(_) => {}
            Stmt::Break(_) | Stmt::Continue(_) => {}
            Stmt::SuperInit(s) => out.push(s.span),
            Stmt::If(s) => {
                collect_super_init_spans(&s.then_block, out);
                if let Some(else_block) = &s.else_block {
                    collect_super_init_spans(else_block, out);
                }
            }
            Stmt::While(s) => collect_super_init_spans(&s.body, out),
            Stmt::For(s) => collect_super_init_spans(&s.body, out),
            Stmt::Let(_)
            | Stmt::Assign(_)
            | Stmt::FieldAssign(_)
            | Stmt::StaticFieldAssign(_)
            | Stmt::IndexAssign(_)
            | Stmt::Return(_)
            | Stmt::Expr(_) => {}
        }
    }
}

/// Apply `f` to each direct sub-expression of `expr` (one level deep). Used by
/// the static-initializer forward-reference scan (willow-qsqf §10.4).
pub(crate) fn walk_subexprs(expr: &Expr, f: &mut impl FnMut(&Expr)) {
    match expr {
        Expr::Integer(..)
        | Expr::Float(..)
        | Expr::Bool(..)
        | Expr::Nil(..)
        | Expr::String(..)
        | Expr::Var(..)
        | Expr::Select(_)
        | Expr::StaticField(_) => {}
        Expr::Binary(b) => {
            f(&b.lhs);
            f(&b.rhs);
        }
        Expr::Unary(u) => f(&u.expr),
        Expr::Call(c) => {
            for a in &c.args {
                f(&a.expr);
            }
        }
        Expr::FieldAccess(o, _, _) => f(o),
        Expr::MethodCall(m) => {
            f(&m.object);
            for a in &m.args {
                f(&a.expr);
            }
        }
        Expr::StaticCall(s) => {
            for a in &s.args {
                f(&a.expr);
            }
        }
        Expr::New(n) => {
            for a in &n.args {
                f(&a.expr);
            }
        }
        Expr::ObjectLiteral(o) => {
            for fld in &o.fields {
                f(&fld.value);
            }
        }
        Expr::Await(a) => f(&a.expr),
        Expr::Print(e, _, _) => f(e),
        Expr::Ternary(t) => {
            f(&t.condition);
            f(&t.then_expr);
            f(&t.else_expr);
        }
        Expr::Range(r) => {
            f(&r.start);
            f(&r.end);
        }
        Expr::Lambda(l) => {
            if let LambdaBody::Expr(e) = &l.body {
                f(e);
            }
        }
        Expr::Match(m) => f(&m.scrutinee),
        Expr::TryPropagate(e, _) => f(e),
        Expr::ArrayLiteral(els, _) => {
            for e in els {
                f(e);
            }
        }
        Expr::Index(a, i, _) => {
            f(a);
            f(i);
        }
    }
}

pub(crate) fn reference_place_key(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Var(name, _) => Some(name.clone()),
        Expr::FieldAccess(obj, field_name, _) => {
            reference_place_key(obj).map(|base| format!("{base}.{field_name}"))
        }
        Expr::Index(array, index, _) => {
            let base = reference_place_key(array)?;
            match &**index {
                Expr::Integer(value, _) => Some(format!("{base}[{value}]")),
                _ => None,
            }
        }
        _ => None,
    }
}

pub(crate) fn block_always_returns(block: &Block) -> bool {
    block.stmts.iter().any(stmt_always_returns)
}

pub(crate) fn stmt_always_returns(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Defer(_) => false,
        // break/continue divert control flow but never RETURN (willow-kzka).
        Stmt::Break(_) | Stmt::Continue(_) => false,
        Stmt::Return(_) => true,
        // A statement-position `match` whose every arm diverges (all arms are
        // blocks that always return) guarantees a return (willow-zvkv).
        Stmt::Expr(e) => match &e.expr {
            crate::parser::ast::Expr::Match(m) => {
                !m.arms.is_empty()
                    && m.arms.iter().all(|arm| match &arm.body {
                        crate::parser::ast::MatchBody::Block(b) => block_always_returns(b),
                        crate::parser::ast::MatchBody::Expr(_) => false,
                    })
            }
            _ => false,
        },
        Stmt::If(s) => s
            .else_block
            .as_ref()
            .map(|else_block| {
                block_always_returns(&s.then_block) && block_always_returns(else_block)
            })
            .unwrap_or(false),
        Stmt::Let(_)
        | Stmt::Assign(_)
        | Stmt::FieldAssign(_)
        | Stmt::SuperInit(_)
        | Stmt::StaticFieldAssign(_)
        | Stmt::IndexAssign(_)
        | Stmt::While(_)
        | Stmt::For(_) => false,
    }
}
