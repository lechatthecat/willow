//! Every path out of a value-returning body must be a `return` (willow-x8sj).
//!
//! Willow has no implicit tail expression: a body produces its value only
//! through a `return` statement. Nothing used to check that one was reached, so
//! a function declared `-> i64` whose body ran off the bottom compiled clean and
//! handed the caller the zero value of the return type. The failure surfaced far
//! from its cause as a plausible wrong answer — a missing `return match s {...}`
//! reads exactly like a downcast that never matched.
//!
//! The analysis here asks the opposite question of a data-flow one: not "is a
//! value assigned" but "can control reach the closing brace at all". A body is
//! accepted when every path leaves the function first, which happens three ways:
//!
//! * a `return`;
//! * a call that never comes back — only `panic` today, recognized by its
//!   declared `Never` return type rather than by its name, so a user
//!   declaration that took the name would not be mistaken for it. Note that
//!   [`TypeChecker::expr_diverges`] looks for this shape under `Expr::Call`
//!   ONLY. That is complete while `Never` stays unspellable in source and
//!   `panic` is the sole function that carries it, since a method or a function
//!   value can never be declared `!`. The day `Never` becomes reachable any
//!   other way — a `!`-returning method, a `fn(...) -> !` value — the
//!   corresponding arms have to be added here, or every body ending in such a
//!   call starts getting a false E0205;
//! * a loop that never finishes: `while true` whose body holds no `break` for
//!   it. Rejecting those would be gratuitous — the shape is how a worker or an
//!   event loop is written, and the function genuinely never returns.
//!
//! `if` needs both arms, and a statement `match` needs every arm; the match case
//! is sound because a non-exhaustive `match` is already E1206, so the arms it
//! lists are the only ways through. `for` never counts however its body ends: an
//! empty range runs zero iterations. `defer` never counts either — its body runs
//! on the way out of a scope control has to reach some other way first.
//!
//! An imprecise answer is not symmetric here. Missing a divergence rejects a
//! correct program, so the rules only ever accept a body they can prove leaves;
//! the `break` scan is the one place that tips the other way, and it is written
//! to find MORE breaks rather than fewer so an infinite loop is only claimed
//! when nothing can escape it.

use super::TypeChecker;
use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use crate::semantic::type_checker::types::type_name;

/// What a body belongs to, for the diagnostic's wording.
pub(crate) enum ReturnSite<'a> {
    Function(&'a str),
    Method {
        class: &'a str,
        name: &'a str,
    },
    /// `inferred` marks a lambda with no `->` annotation, whose return type is
    /// the one the body's `return`s produced. Nothing was declared there, so
    /// the diagnostic must not claim it was.
    Lambda {
        inferred: bool,
    },
}

impl ReturnSite<'_> {
    fn describe(&self) -> String {
        match self {
            ReturnSite::Function(name) => format!("`{name}`"),
            ReturnSite::Method { class, name } => format!("`{class}::{name}`"),
            ReturnSite::Lambda { .. } => "this lambda".to_string(),
        }
    }
}

impl TypeChecker {
    /// Report E0205 when control can reach the end of a body that owes its
    /// caller a value.
    ///
    /// `declared_span` is the declaration the return type was written on; the
    /// primary label goes on the last statement instead, which is where control
    /// actually falls out.
    pub(crate) fn check_all_paths_return(
        &mut self,
        body: &Block,
        return_type: &Type,
        declared_span: Span,
        site: ReturnSite<'_>,
    ) {
        // `void` owes nothing, and `Never` is unspellable in source — it only
        // ever arrives here through inference, which already means the body
        // diverges.
        if matches!(return_type, Type::Void | Type::Never) {
            return;
        }
        // The entry point is the one body where running off the end MEANS
        // something: `fn main() -> Result<void, E>` exits 0 on the implicit end
        // exactly as it does on `return Ok()` (willow-exg). `main` may hold no
        // other value-returning signature, so matching the shape by name is
        // enough — a `main` in a module that is not the entry point would have
        // to be written this way too before it inherited the latitude.
        if matches!(site, ReturnSite::Function("main")) && is_result_void(return_type) {
            return;
        }
        if self.block_diverges(body) {
            return;
        }
        let fall_through = body
            .stmts
            .last()
            .map(|stmt| stmt.span())
            .unwrap_or(body.span);
        let what = site.describe();
        self.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0205,
                format!("not all paths through {what} return a value"),
            )
            .with_label(Label::primary(
                fall_through,
                "control can reach the end of the body without returning",
            ))
            .with_label(Label::secondary(
                declared_span,
                match site {
                    ReturnSite::Lambda { inferred: true } => format!(
                        "`{}` inferred from a `return` in this body",
                        type_name(return_type)
                    ),
                    _ => format!("`{}` declared here", type_name(return_type)),
                },
            ))
            .with_help(
                "end every path with `return <value>;` — Willow has no implicit tail \
                 expression, so a trailing `match` or `if` needs its own `return`",
            ),
        );
    }

    /// Whether every path through `block` leaves the enclosing function.
    fn block_diverges(&self, block: &Block) -> bool {
        // A diverging statement makes the rest of the block unreachable, so one
        // anywhere is enough.
        block.stmts.iter().any(|stmt| self.stmt_diverges(stmt))
    }

    fn stmt_diverges(&self, stmt: &Stmt) -> bool {
        match stmt {
            Stmt::Return(_) => true,
            Stmt::If(s) => match &s.else_block {
                Some(else_block) => {
                    self.block_diverges(&s.then_block) && self.block_diverges(else_block)
                }
                // Without an `else` the condition may be false, and then the
                // block after the `if` is what runs.
                None => false,
            },
            // A critical section runs unconditionally, so a `return` inside it
            // returns from the enclosing function once the compiler-inserted
            // release has run (willow-38w.1.3).
            Stmt::Lock(s) => self.block_diverges(&s.body),
            // `while true` with no way out is the idiomatic never-returning
            // loop. Any other condition may be false on the first test.
            Stmt::While(s) => {
                matches!(s.cond, Expr::Bool(true, _)) && !block_breaks_enclosing_loop(&s.body)
            }
            Stmt::Let(s) => self.expr_diverges(&s.init),
            Stmt::Assign(s) => self.expr_diverges(&s.value),
            Stmt::Expr(s) => self.expr_diverges(&s.expr),
            // `break`/`continue` leave the block but not the function. Whether
            // the loop they leave can fall out of its own bottom is the loop's
            // question, answered by `block_breaks_enclosing_loop` above.
            Stmt::Break(_) | Stmt::Continue(_) => false,
            // A `for` head may iterate zero times, and a `defer` body runs on
            // the way out of a scope that control has to reach some other way.
            Stmt::Defer(_) | Stmt::For(_) => false,
            Stmt::FieldAssign(_)
            | Stmt::SuperInit(_)
            | Stmt::StaticFieldAssign(_)
            | Stmt::IndexAssign(_) => false,
        }
    }

    fn expr_diverges(&self, expr: &Expr) -> bool {
        match expr {
            // Only a callee DECLARED to return `Never` counts, so the rule
            // follows the symbol table rather than the spelling `panic`.
            //
            // No `Expr::MethodCall` arm on purpose: `Never` is unspellable in
            // source, so `panic` — a plain function — is the only thing that
            // can hold it, and a diverging method cannot exist to be missed.
            // Whatever makes `Never` reachable elsewhere must add the arm with
            // it; see this module's rule list.
            Expr::Call(call) => self
                .symbols
                .lookup_func(&call.callee)
                .is_some_and(|info| info.return_type == Type::Never),
            // A non-exhaustive `match` is E1206, so the arms listed here are
            // the only ways through it.
            Expr::Match(m) => {
                !m.arms.is_empty()
                    && m.arms.iter().all(|arm| match &arm.body {
                        MatchBody::Block(block) => self.block_diverges(block),
                        MatchBody::Expr(e) => self.expr_diverges(e),
                    })
            }
            // `select` runs the body of exactly one case, whichever becomes
            // ready (or `default`, when none does).
            Expr::Select(sel) => {
                !sel.cases.is_empty()
                    && sel.cases.iter().all(|case| self.block_diverges(&case.body))
            }
            _ => false,
        }
    }
}

/// Whether `ty` is the `Result<void, E>` an entry point may end implicitly.
fn is_result_void(ty: &Type) -> bool {
    crate::semantic::builtin_types::binary_args(
        ty,
        crate::semantic::builtin_types::BuiltinTypeId::Result,
    )
    .is_some_and(|(ok, _)| *ok == Type::Void)
}

/// Whether `block` can `break` out of the loop it is the body of.
///
/// A `break` inside a nested loop belongs to that loop, and one inside a lambda
/// belongs to a loop in the lambda — neither escapes to here. Everything else is
/// searched, because a `break` this scan misses would let the caller claim a
/// loop runs forever when it does not.
fn block_breaks_enclosing_loop(block: &Block) -> bool {
    block.stmts.iter().any(stmt_breaks_enclosing_loop)
}

fn stmt_breaks_enclosing_loop(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Break(_) => true,
        Stmt::If(s) => {
            block_breaks_enclosing_loop(&s.then_block)
                || s.else_block
                    .as_ref()
                    .is_some_and(block_breaks_enclosing_loop)
        }
        Stmt::Lock(s) => block_breaks_enclosing_loop(&s.body),
        Stmt::Let(s) => expr_breaks_enclosing_loop(&s.init),
        Stmt::Assign(s) => expr_breaks_enclosing_loop(&s.value),
        Stmt::Expr(s) => expr_breaks_enclosing_loop(&s.expr),
        // An inner loop consumes its own `break`s.
        Stmt::While(_) | Stmt::For(_) => false,
        // A `defer` body runs at scope exit and cannot break the loop it was
        // registered in (E0904 rejects one that tries).
        Stmt::Defer(_) => false,
        Stmt::Return(_)
        | Stmt::Continue(_)
        | Stmt::FieldAssign(_)
        | Stmt::SuperInit(_)
        | Stmt::StaticFieldAssign(_)
        | Stmt::IndexAssign(_) => false,
    }
}

fn expr_breaks_enclosing_loop(expr: &Expr) -> bool {
    match expr {
        // A `match` arm and a `select` case are the expression positions that
        // hold statements. A lambda body also does, but its `break` belongs to
        // a loop inside the lambda.
        Expr::Match(m) => m.arms.iter().any(|arm| match &arm.body {
            MatchBody::Block(block) => block_breaks_enclosing_loop(block),
            MatchBody::Expr(e) => expr_breaks_enclosing_loop(e),
        }),
        Expr::Select(sel) => sel
            .cases
            .iter()
            .any(|case| block_breaks_enclosing_loop(&case.body)),
        _ => false,
    }
}
