//! AST collection passes for the Cranelift backend (extracted from `mod.rs`).
//! Iterative walkers that gather string literals, lambdas, runtime-checked
//! names, and reference-debug strings from a `Program` before codegen.

use std::collections::HashSet;

use crate::parser::ast::*;
use crate::parser::iter::{AstEvent, AstWalk};

use super::type_helpers::debug_type_name;
use super::{reference_mode_name, reference_place_kind, reference_place_name};

pub(crate) fn collect_reference_debug_strings_in_program(program: &Program) -> Vec<String> {
    let mut out = HashSet::new();
    for value in [
        "<unknown>",
        "&",
        "&mut",
        "value",
        "local",
        "field",
        "array_element",
        "expression",
    ] {
        out.insert(value.to_string());
    }

    for item in &program.items {
        match item {
            Item::Function(f) => {
                out.insert(f.name.clone());
                collect_reference_debug_param_strings(&f.params, &mut out);
                collect_reference_debug_strings_in_block(&f.body, &mut out);
            }
            Item::Class(c) => {
                for method in &c.methods {
                    out.insert(format!("{}::{}", c.name, method.name));
                    out.insert(method.name.clone());
                    collect_reference_debug_param_strings(&method.params, &mut out);
                    collect_reference_debug_strings_in_block(&method.body, &mut out);
                }
                for ctor in &c.constructors {
                    out.insert(format!("{}::init", c.name));
                    out.insert("init".to_string());
                    collect_reference_debug_param_strings(&ctor.params, &mut out);
                    collect_reference_debug_strings_in_block(&ctor.body, &mut out);
                }
            }
            Item::Enum(_) => {}
            Item::Interface(_) => {} // no bodies
        }
    }

    out.into_iter().collect()
}

pub(crate) fn collect_reference_debug_param_strings(params: &[Param], out: &mut HashSet<String>) {
    for param in params {
        out.insert(param.name.clone());
        out.insert(debug_type_name(&param.ty));
        out.insert(reference_mode_name(&param.mode).to_string());
    }
}

/// Visit every call argument passed as `&place`, in source order, together with
/// the callee name a debug reference report would use for that call.
///
/// One walker backs two consumers with very different jobs — pre-declaring the
/// string literals the debug hook passes, and deciding which locals must be
/// stack-backed because their address is taken — so the two can never drift
/// apart on which arguments count as reference arguments.
pub(crate) fn walk_reference_args_in_block(block: &Block, visit: &mut dyn FnMut(&str, &CallArg)) {
    for event in AstWalk::new(AstEvent::Block(block)) {
        match event {
            AstEvent::CallArguments(expr) => match expr {
                Expr::Call(c) => visit_reference_args(&c.callee, &c.args, visit),
                Expr::MethodCall(c) => visit_reference_args(&c.method, &c.args, visit),
                Expr::StaticCall(c) => {
                    visit_reference_args(&format!("{}::{}", c.class, c.method), &c.args, visit)
                }
                Expr::New(c) => {
                    visit_reference_args(&format!("{}::init", c.class_name), &c.args, visit)
                }
                _ => unreachable!("argument event must identify a call"),
            },
            AstEvent::SuperArguments(s) => visit_reference_args("super.init", &s.args, visit),
            _ => {}
        }
    }
}

fn visit_reference_args(callee: &str, args: &[CallArg], visit: &mut dyn FnMut(&str, &CallArg)) {
    for arg in args {
        if matches!(&arg.mode, CallArgMode::Reference { .. }) {
            visit(callee, arg);
        }
    }
}

pub(crate) fn collect_reference_debug_strings_in_block(block: &Block, out: &mut HashSet<String>) {
    walk_reference_args_in_block(block, &mut |callee, arg| {
        out.insert(callee.to_string());
        out.insert(reference_place_kind(&arg.expr).to_string());
        out.insert(reference_place_name(&arg.expr));
    });
}

/// Names of the locals whose address is taken somewhere in `body`.
///
/// Such a local cannot live in a Cranelift SSA variable that is promoted to a
/// stack slot at the `&` itself: the promoting store lands wherever the `&`
/// sits in the CFG, so it re-initialises the slot on every iteration of an
/// enclosing loop and never runs at all on a branch that does not take the
/// address. Binding these to a stack slot from the start makes the storage
/// decision a property of the declaration rather than of one use
/// (willow-0g8j.2.17).
///
/// The set over-approximates: a `&x` inside a nested lambda names the lambda's
/// own local, and marking the enclosing function's same-named local is merely a
/// slot it did not need.
pub(crate) fn collect_address_taken_locals(body: &Block) -> HashSet<String> {
    let mut out = HashSet::new();
    walk_reference_args_in_block(body, &mut |_, arg| {
        if let Expr::Var(name, _, _) = &arg.expr {
            out.insert(name.clone());
        }
    });
    out
}

pub(crate) fn collect_string_literals_in_program(program: &Program) -> Vec<String> {
    let mut out = Vec::new();
    for item in &program.items {
        match item {
            Item::Function(f) => collect_string_literals_in_block(&f.body, &mut out),
            Item::Class(c) => {
                for method in &c.methods {
                    collect_string_literals_in_block(&method.body, &mut out);
                }
                for ctor in &c.constructors {
                    collect_string_literals_in_block(&ctor.body, &mut out);
                }
                // Static-property initializers are emitted in __willow_static_init
                // (willow-qsqf), so their string literals must be declared too.
                for field in &c.fields {
                    if let Some(init) = &field.initializer {
                        collect_string_literals_in_expr(init, &mut out);
                    }
                }
            }
            Item::Enum(_) => {}
            Item::Interface(_) => {} // no bodies
        }
    }
    out
}

pub(crate) fn collect_string_literals_in_block(block: &Block, out: &mut Vec<String>) {
    collect_strings(AstWalk::new(AstEvent::Block(block)), out);
}
pub(crate) fn collect_string_literals_in_expr(expr: &Expr, out: &mut Vec<String>) {
    collect_strings(AstWalk::new(AstEvent::Expr(expr)), out);
}
fn collect_strings(walk: AstWalk<'_>, out: &mut Vec<String>) {
    for event in walk {
        match event {
            AstEvent::Expr(Expr::String(value, _, _)) => out.push(value.clone()),
            AstEvent::ExitExpr(Expr::Call(c)) if c.callee == "format" || c.callee == "panic" => {
                if let Some(Expr::String(spec, _, _)) = c.args.first().map(|a| &a.expr)
                    && let Ok(segments) = crate::interpolate::parse_spec(spec)
                {
                    out.push(String::new());
                    for segment in segments {
                        if let crate::interpolate::Segment::Literal(text) = segment {
                            out.push(text);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn collect_lambdas_in_program(program: &Program) -> Vec<(String, LambdaExpr)> {
    let mut out = Vec::new();
    let mut counter = 0usize;
    for item in &program.items {
        match item {
            Item::Function(f) => collect_lambdas_in_block(&f.body, &mut counter, &mut out),
            Item::Class(c) => {
                for m in &c.methods {
                    collect_lambdas_in_block(&m.body, &mut counter, &mut out);
                }
                for ctor in &c.constructors {
                    collect_lambdas_in_block(&ctor.body, &mut counter, &mut out);
                }
                for field in &c.fields {
                    if let Some(init) = &field.initializer {
                        collect_lambdas_from_walk(
                            AstWalk::new(AstEvent::Expr(init)),
                            &mut counter,
                            &mut out,
                        );
                    }
                }
            }
            Item::Enum(_) => {}
            Item::Interface(_) => {} // no bodies
        }
    }
    out
}

pub(crate) fn collect_lambdas_in_block(
    block: &Block,
    counter: &mut usize,
    out: &mut Vec<(String, LambdaExpr)>,
) {
    collect_lambdas_from_walk(AstWalk::new(AstEvent::Block(block)), counter, out);
}
fn collect_lambdas_from_walk(
    walk: AstWalk<'_>,
    counter: &mut usize,
    out: &mut Vec<(String, LambdaExpr)>,
) {
    for event in walk {
        if let AstEvent::ExitExpr(Expr::Lambda(lambda)) = event {
            let name = super::symbols::lambda_symbol(*counter);
            *counter += 1;
            out.push((name, *lambda.clone()));
        }
    }
}

/// Every member name a debug build may have to name at runtime: the field and
/// method names a nil check reports, plus the callee names a call-stack frame
/// carries. Both are emitted as static bytes, so a name missing from this set
/// has no data segment to point at and the site that wanted it is silently
/// skipped.
pub(crate) fn collect_nil_check_names(program: &Program) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for item in &program.items {
        match item {
            Item::Function(f) => collect_nil_check_names_in_block(&f.body, &mut out),
            Item::Class(c) => {
                for m in &c.methods {
                    collect_nil_check_names_in_block(&m.body, &mut out);
                }
                for ctor in &c.constructors {
                    collect_nil_check_names_in_block(&ctor.body, &mut out);
                }
            }
            Item::Enum(_) => {}
            Item::Interface(_) => {} // no bodies
        }
    }
    out
}

pub(crate) fn collect_nil_check_names_in_block(block: &Block, out: &mut HashSet<String>) {
    for event in AstWalk::new(AstEvent::Block(block)) {
        match event {
            AstEvent::Expr(Expr::FieldAccess(_, name, _, _)) => {
                out.insert(name.clone());
            }
            AstEvent::Expr(Expr::MethodCall(m)) => {
                out.insert(m.method.clone());
            }
            AstEvent::Expr(Expr::StaticCall(s)) => {
                out.insert(format!("{}::{}", s.class, s.method));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::Span;

    #[test]
    fn deep_collection_uses_a_one_megabyte_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let span = Span::new(0, 0, 1, 1);
                let mut expr = Expr::String("deep".into(), span, ExprId::fresh());
                for _ in 0..50_000 {
                    expr = Expr::TryPropagate(Box::new(expr), span, ExprId::fresh());
                }
                let mut strings = Vec::new();
                collect_string_literals_in_expr(&expr, &mut strings);
                assert_eq!(strings, ["deep"]);
                drop(expr);
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
