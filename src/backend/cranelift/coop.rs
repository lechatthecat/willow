//! Async/cooperative-lowering AST predicates for the Cranelift backend
//! (extracted from `mod.rs`). Pure helpers that recognise `await`/`sleep`/
//! channel-recv shapes and decide cooperative-poll eligibility.

#[cfg(test)]
use std::collections::HashMap;

use crate::parser::ast::*;
use crate::semantic::ids::FunctionId;
#[cfg(test)]
use crate::semantic::symbols::EnumInfo;

/// Whether an expression is `Result::Ok()` with no arguments — the success
/// value for a `Result<void, E>` main, which carries no payload (willow-exg).
pub(crate) fn is_zero_arg_result_ok(expr: &Expr) -> bool {
    matches!(expr, Expr::StaticCall(s) if s.args.is_empty() && s.method == "Ok" && s.class == "Result")
}

/// If `expr` is `await sleep(<arg>)`, return the sleep argument (willow-lpn.5.3).
pub(crate) fn await_sleep_arg(expr: &Expr) -> Option<&Expr> {
    if let Expr::Await(a) = expr
        && let Expr::Call(c) = &a.expr
        && c.callee == "sleep"
        && c.args.len() == 1
    {
        return Some(&c.args[0].expr);
    }
    None
}

/// True if `expr` is `await yield()` (willow-gyaa.3).
pub(crate) fn is_await_yield(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Await(a)
            if matches!(&a.expr, Expr::Call(c) if c.callee == "yield" && c.args.is_empty())
    )
}

/// If `expr` is `await <call>` where the callee is a cooperative leaf, return
/// the call (and the await's span) — the suspendable call-await form. Used by
/// the cooperative awaiter lowering (willow-lpn.5.3.1).
pub(crate) fn await_coop_call<'a>(
    expr: &'a Expr,
    cooperative_leaves: &std::collections::HashSet<FunctionId>,
) -> Option<(&'a CallExpr, crate::diagnostics::Span)> {
    if let Expr::Await(a) = expr
        && let Expr::Call(c) = &a.expr
        && cooperative_leaves.contains(&FunctionId::free_from_source_name(&c.callee))
    {
        return Some((c, a.span));
    }
    None
}

/// True when `expr` is `await <call>` whose callee is a cooperative leaf. Such
/// calls use the dedicated `emit_coop_call_await` lowering, so the general
/// cooperative task-await (`await_contextual_task_expr`) skips them and a
/// non-leaf/imported async call falls through to the task-await path instead of
/// block-driving the scheduler (willow-0a6k.6).
pub(crate) fn is_leaf_call_await(
    expr: &Expr,
    cooperative_leaves: &std::collections::HashSet<FunctionId>,
) -> bool {
    matches!(
        expr,
        Expr::Await(a) if matches!(&a.expr, Expr::Call(c) if cooperative_leaves.contains(&FunctionId::free_from_source_name(&c.callee)))
    )
}

/// The await span of a direct-call-form await (`await <call|method|static>`)
/// that needs a reserved GC callee-frame slot, so the cooperative resume path
/// RELOADS the callee/task frame from the slot instead of re-emitting (and thus
/// re-running) the call. Covers leaf calls (call-await), non-leaf/imported calls
/// and method/static calls (task-await); returns None for any other expr
/// (willow-0a6k.6).
pub(crate) fn await_callee_frame_slot_span(
    expr: &Expr,
    cooperative_leaves: &std::collections::HashSet<FunctionId>,
) -> Option<crate::diagnostics::Span> {
    await_coop_call(expr, cooperative_leaves)
        .map(|(_, span)| span)
        .or(match expr {
            Expr::Await(a)
                if matches!(
                    &a.expr,
                    Expr::Call(_) | Expr::MethodCall(_) | Expr::StaticCall(_)
                ) =>
            {
                Some(a.span)
            }
            _ => None,
        })
}

/// If `expr` is a top-level channel `recv()` (`ch.recv()`), return the method
/// call. A cooperative `recv` is a suspend point: the task parks as a channel
/// waiter when empty and is woken by `send`/`close` (willow-dsw).
pub(crate) fn is_channel_recv(expr: &Expr) -> Option<&MethodCallExpr> {
    if let Expr::MethodCall(m) = expr
        && m.method == "recv"
        && m.args.is_empty()
    {
        return Some(m);
    }
    None
}

/// If `expr` is a top-level `JoinHandle<T>.join()` / `.try_join()` shape,
/// return the method call. The receiver type is checked by codegen before this
/// syntax-only predicate is used for cooperative lowering.
pub(crate) fn is_task_join(expr: &Expr) -> Option<&MethodCallExpr> {
    if let Expr::MethodCall(m) = expr
        && matches!(m.method.as_str(), "join" | "try_join")
        && m.args.is_empty()
    {
        return Some(m);
    }
    None
}

#[cfg(test)]
pub(crate) fn expr_contains_await(expr: &Expr) -> bool {
    match expr {
        Expr::Await(_) => true,
        Expr::Integer(..)
        | Expr::Float(..)
        | Expr::Bool(..)
        | Expr::String(..)
        | Expr::Var(..)
        | Expr::Nil(..) => false,
        Expr::Print(inner, _, _) => expr_contains_await(inner),
        Expr::Call(c) => c.args.iter().any(|a| expr_contains_await(&a.expr)),
        Expr::MethodCall(m) => {
            expr_contains_await(&m.object) || m.args.iter().any(|a| expr_contains_await(&a.expr))
        }
        Expr::StaticCall(s) => s.args.iter().any(|a| expr_contains_await(&a.expr)),
        Expr::Binary(b) => expr_contains_await(&b.lhs) || expr_contains_await(&b.rhs),
        Expr::Unary(u) => expr_contains_await(&u.expr),
        Expr::Ternary(t) => {
            expr_contains_await(&t.condition)
                || expr_contains_await(&t.then_expr)
                || expr_contains_await(&t.else_expr)
        }
        Expr::Range(r) => expr_contains_await(&r.start) || expr_contains_await(&r.end),
        Expr::FieldAccess(obj, _, _) => expr_contains_await(obj),
        Expr::ObjectLiteral(o) => o.fields.iter().any(|f| expr_contains_await(&f.value)),
        Expr::TryPropagate(inner, _) => expr_contains_await(inner),
        Expr::ArrayLiteral(elements, _) => elements.iter().any(expr_contains_await),
        Expr::Index(arr, index, _) => expr_contains_await(arr) || expr_contains_await(index),
        _ => true,
    }
}

#[cfg(test)]
pub(crate) fn cooperative_main_eligible(
    f: &FunctionDecl,
    async_local_types: &HashMap<crate::diagnostics::Span, Type>,
    enum_infos: &HashMap<String, EnumInfo>,
    cooperative_leaves: &std::collections::HashSet<FunctionId>,
) -> bool {
    let _ = enum_infos;
    if !f.is_async || f.name != "main" || !f.params.is_empty() || f.return_type != Type::Void {
        return false;
    }
    let mut has_sleep = false;
    let mut has_return = false;
    if !coop_stmts_eligible(
        &f.body.stmts,
        async_local_types,
        cooperative_leaves,
        false,
        &mut has_sleep,
        &mut has_return,
    ) {
        return false;
    }
    has_sleep
}

#[cfg(test)]
pub(crate) fn coop_stmts_eligible(
    stmts: &[Stmt],
    async_local_types: &HashMap<crate::diagnostics::Span, Type>,
    cooperative_leaves: &std::collections::HashSet<FunctionId>,
    allow_value_return: bool,
    has_sleep: &mut bool,
    has_return: &mut bool,
) -> bool {
    for stmt in stmts {
        match stmt {
            Stmt::Break(_) | Stmt::Continue(_) | Stmt::Defer(_) => {}
            Stmt::Expr(es) => {
                if let Expr::Select(sel) = &es.expr {
                    // A `select` is a cooperative suspend point (willow-7aj): it
                    // parks the task on its recv channels when no case is ready.
                    // Eligible iff every case channel/value is await-free and every
                    // case body is itself cooperatively lowerable.
                    for case in &sel.cases {
                        match &case.kind {
                            SelectCaseKind::Recv { channel, .. } => {
                                if expr_contains_await(channel) {
                                    return false;
                                }
                            }
                            SelectCaseKind::Send { channel, value } => {
                                if expr_contains_await(channel) || expr_contains_await(value) {
                                    return false;
                                }
                            }
                            SelectCaseKind::Default => {}
                        }
                        if !coop_stmts_eligible(
                            &case.body.stmts,
                            async_local_types,
                            cooperative_leaves,
                            allow_value_return,
                            has_sleep,
                            has_return,
                        ) {
                            return false;
                        }
                    }
                    *has_sleep = true;
                } else if await_sleep_arg(&es.expr).is_some() || is_await_yield(&es.expr) {
                    *has_sleep = true;
                } else if await_coop_call(&es.expr, cooperative_leaves).is_some() {
                    // `await <coop-leaf-call>;` is a suspend point (willow-lpn.5.3.1).
                    *has_sleep = true;
                } else if is_channel_recv(&es.expr).is_some() {
                    // `ch.recv();` is a cooperative suspend point (willow-dsw).
                    *has_sleep = true;
                } else if expr_contains_await(&es.expr) {
                    return false;
                }
            }
            Stmt::Let(l) => {
                if l.name == "_" {
                    return false;
                }
                // `let x = await <coop-leaf-call>` / `let x = ch.recv()` are allowed
                // suspend points; any other await in the initializer is not lowerable.
                if await_coop_call(&l.init, cooperative_leaves).is_some()
                    || is_channel_recv(&l.init).is_some()
                {
                    *has_sleep = true;
                } else if expr_contains_await(&l.init) {
                    return false;
                }
                if l.ty.is_none() && !async_local_types.contains_key(&l.span) {
                    return false;
                }
            }
            Stmt::Assign(a) => {
                if await_coop_call(&a.value, cooperative_leaves).is_some()
                    || is_channel_recv(&a.value).is_some()
                {
                    *has_sleep = true;
                } else if expr_contains_await(&a.value) {
                    return false;
                }
            }
            Stmt::StaticFieldAssign(s) => {
                if await_coop_call(&s.value, cooperative_leaves).is_some()
                    || is_channel_recv(&s.value).is_some()
                {
                    *has_sleep = true;
                } else if expr_contains_await(&s.value) {
                    return false;
                }
            }
            Stmt::FieldAssign(s) => {
                if expr_contains_await(&s.object) {
                    return false;
                }
                if await_coop_call(&s.value, cooperative_leaves).is_some() {
                    *has_sleep = true;
                } else if expr_contains_await(&s.value) {
                    return false;
                }
            }
            Stmt::IndexAssign(s) => {
                if expr_contains_await(&s.array) || expr_contains_await(&s.index) {
                    return false;
                }
                if await_coop_call(&s.value, cooperative_leaves).is_some() {
                    *has_sleep = true;
                } else if expr_contains_await(&s.value) {
                    return false;
                }
            }
            Stmt::SuperInit(s) => {
                if s.args.iter().any(|arg| expr_contains_await(&arg.expr)) {
                    return false;
                }
            }
            Stmt::Return(r) => match &r.value {
                None => {}
                Some(v)
                    if allow_value_return && await_coop_call(v, cooperative_leaves).is_some() =>
                {
                    *has_sleep = true;
                    *has_return = true;
                }
                // `return ch.recv();` — a suspend point (willow-0a6k.6).
                Some(v) if allow_value_return && is_channel_recv(v).is_some() => {
                    *has_sleep = true;
                    *has_return = true;
                }
                Some(v) if allow_value_return && !expr_contains_await(v) => *has_return = true,
                _ => return false,
            },
            Stmt::If(s) => {
                if expr_contains_await(&s.cond) {
                    return false;
                }
                if !coop_stmts_eligible(
                    &s.then_block.stmts,
                    async_local_types,
                    cooperative_leaves,
                    allow_value_return,
                    has_sleep,
                    has_return,
                ) {
                    return false;
                }
                if let Some(eb) = &s.else_block
                    && !coop_stmts_eligible(
                        &eb.stmts,
                        async_local_types,
                        cooperative_leaves,
                        allow_value_return,
                        has_sleep,
                        has_return,
                    )
                {
                    return false;
                }
            }
            Stmt::While(s) => {
                if expr_contains_await(&s.cond) {
                    return false;
                }
                if !coop_stmts_eligible(
                    &s.body.stmts,
                    async_local_types,
                    cooperative_leaves,
                    allow_value_return,
                    has_sleep,
                    has_return,
                ) {
                    return false;
                }
            }
            Stmt::For(s) => {
                if expr_contains_await(&s.iterable) {
                    return false;
                }
                if !async_local_types.contains_key(&s.iter_frame_key())
                    || !async_local_types.contains_key(&s.index_frame_key())
                    || (s.name != "_" && !async_local_types.contains_key(&s.name_span))
                {
                    return false;
                }
                if !coop_stmts_eligible(
                    &s.body.stmts,
                    async_local_types,
                    cooperative_leaves,
                    allow_value_return,
                    has_sleep,
                    has_return,
                ) {
                    return false;
                }
            }
        }
    }
    true
}
