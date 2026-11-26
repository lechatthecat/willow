//! Source-like text of a simple expression, for diagnostics that suggest a
//! concrete rewrite. Only short names, literals, member accesses and calls
//! are rendered; anything else returns `None` so the caller falls back to a
//! generic wording.
use crate::parser::ast::{CallArg, CallArgMode, Expr};

/// Longest rendering a help message quotes.
const MAX_LEN: usize = 60;

pub(super) fn simple_expr_text(expr: &Expr) -> Option<String> {
    let mut out = String::new();
    write_expr(expr, 0, &mut out).then_some(out)
}

/// The rendered arguments of a call, without the parentheses.
pub(super) fn simple_args_text(args: &[CallArg]) -> Option<String> {
    let mut out = String::new();
    write_args(args, 0, &mut out).then_some(out)
}

fn write_expr(expr: &Expr, depth: usize, out: &mut String) -> bool {
    // Every nesting level adds at least one byte to the rendering, so a tree
    // deeper than the length bound can never fit. Receivers are rendered
    // before anything is appended, so the depth is checked on its own.
    if depth > MAX_LEN || out.len() > MAX_LEN {
        return false;
    }
    let depth = depth + 1;
    match expr {
        Expr::Integer(n, ..) => out.push_str(&n.to_string()),
        Expr::Bool(b, ..) => out.push_str(if *b { "true" } else { "false" }),
        Expr::Var(name, ..) => out.push_str(name),
        Expr::String(s, ..) => {
            if s.chars()
                .any(|c| matches!(c, '"' | '\\' | '{' | '}') || c.is_control())
            {
                return false;
            }
            out.push('"');
            out.push_str(s);
            out.push('"');
        }
        Expr::FieldAccess(object, field, ..) => {
            if !write_expr(object, depth, out) {
                return false;
            }
            out.push('.');
            out.push_str(field);
        }
        Expr::Index(array, index, ..) => {
            if !write_expr(array, depth, out) {
                return false;
            }
            out.push('[');
            if !write_expr(index, depth, out) {
                return false;
            }
            out.push(']');
        }
        Expr::StaticField(field) => {
            out.push_str(&field.class);
            out.push_str("::");
            out.push_str(&field.field);
        }
        Expr::Call(call) if call.type_args.is_empty() => {
            out.push_str(&call.callee);
            return write_call_args(&call.args, depth, out);
        }
        Expr::StaticCall(call) if call.type_args.is_empty() => {
            out.push_str(&call.class);
            out.push_str("::");
            out.push_str(&call.method);
            return write_call_args(&call.args, depth, out);
        }
        Expr::MethodCall(call) => {
            if !write_expr(&call.object, depth, out) {
                return false;
            }
            out.push('.');
            out.push_str(&call.method);
            return write_call_args(&call.args, depth, out);
        }
        _ => return false,
    }
    out.len() <= MAX_LEN
}

fn write_call_args(args: &[CallArg], depth: usize, out: &mut String) -> bool {
    out.push('(');
    if !write_args(args, depth, out) {
        return false;
    }
    out.push(')');
    out.len() <= MAX_LEN
}

fn write_args(args: &[CallArg], depth: usize, out: &mut String) -> bool {
    for (i, arg) in args.iter().enumerate() {
        if !matches!(arg.mode, CallArgMode::Value) {
            return false;
        }
        if i > 0 {
            out.push_str(", ");
        }
        if !write_expr(&arg.expr, depth, out) {
            return false;
        }
    }
    out.len() <= MAX_LEN
}
