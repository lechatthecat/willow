//! The one async/cooperative AST predicate the backend still consults.
//!
//! Everything else this module held recognised `await`/`sleep`/channel shapes
//! for the AST cooperative emitter, which Stage 5 retired: an async function is
//! compiled from lowered IR or not at all, so the shapes are recognised in HIR
//! now (`lir_gen`) and the AST twins went with the emitter (willow-0g8j.3).

use crate::parser::ast::*;

/// Whether an expression is `Result::Ok()` with no arguments — the success
/// value for a `Result<void, E>` main, which carries no payload (willow-exg).
pub(crate) fn is_zero_arg_result_ok(expr: &Expr) -> bool {
    matches!(expr, Expr::StaticCall(s) if s.args.is_empty() && s.method == "Ok" && s.class == "Result")
}
