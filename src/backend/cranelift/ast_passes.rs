//! AST collection passes for the Cranelift backend (extracted from `mod.rs`).
//! Pure recursive walkers that gather string literals, lambdas, runtime-checked
//! names, and reference-debug strings from a `Program` before codegen.

use std::collections::HashSet;

use crate::parser::ast::*;

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
    for stmt in &block.stmts {
        walk_reference_args_in_stmt(stmt, visit);
    }
}

pub(crate) fn walk_reference_args_in_stmt(stmt: &Stmt, visit: &mut dyn FnMut(&str, &CallArg)) {
    match stmt {
        Stmt::Defer(d) => match &d.body {
            DeferBody::Expr(expr) => walk_reference_args_in_expr(expr, visit),
            DeferBody::Block(block) => walk_reference_args_in_block(block, visit),
        },
        Stmt::Break(_) | Stmt::Continue(_) => {}
        Stmt::Let(s) => walk_reference_args_in_expr(&s.init, visit),
        Stmt::Assign(s) => walk_reference_args_in_expr(&s.value, visit),
        Stmt::StaticFieldAssign(s) => walk_reference_args_in_expr(&s.value, visit),
        Stmt::FieldAssign(s) => {
            walk_reference_args_in_expr(&s.object, visit);
            walk_reference_args_in_expr(&s.value, visit);
        }
        Stmt::IndexAssign(s) => {
            walk_reference_args_in_expr(&s.array, visit);
            walk_reference_args_in_expr(&s.index, visit);
            walk_reference_args_in_expr(&s.value, visit);
        }
        Stmt::SuperInit(s) => {
            visit_reference_args("super.init", &s.args, visit);
            for arg in &s.args {
                walk_reference_args_in_expr(&arg.expr, visit);
            }
        }
        Stmt::If(s) => {
            walk_reference_args_in_expr(&s.cond, visit);
            walk_reference_args_in_block(&s.then_block, visit);
            if let Some(else_block) = &s.else_block {
                walk_reference_args_in_block(else_block, visit);
            }
        }
        Stmt::While(s) => {
            walk_reference_args_in_expr(&s.cond, visit);
            walk_reference_args_in_block(&s.body, visit);
        }
        Stmt::For(s) => {
            walk_reference_args_in_expr(&s.iterable, visit);
            walk_reference_args_in_block(&s.body, visit);
        }
        Stmt::Lock(s) => {
            walk_reference_args_in_expr(&s.target, visit);
            walk_reference_args_in_block(&s.body, visit);
        }
        Stmt::Return(s) => {
            if let Some(value) = &s.value {
                walk_reference_args_in_expr(value, visit);
            }
        }
        Stmt::Expr(s) => walk_reference_args_in_expr(&s.expr, visit),
    }
}

pub(crate) fn walk_reference_args_in_expr(expr: &Expr, visit: &mut dyn FnMut(&str, &CallArg)) {
    match expr {
        Expr::StaticField(_) => {}
        Expr::Call(c) => {
            visit_reference_args(&c.callee, &c.args, visit);
            for arg in &c.args {
                walk_reference_args_in_expr(&arg.expr, visit);
            }
        }
        Expr::MethodCall(m) => {
            walk_reference_args_in_expr(&m.object, visit);
            visit_reference_args(&m.method, &m.args, visit);
            for arg in &m.args {
                walk_reference_args_in_expr(&arg.expr, visit);
            }
        }
        Expr::StaticCall(s) => {
            let callee = format!("{}::{}", s.class, s.method);
            visit_reference_args(&callee, &s.args, visit);
            for arg in &s.args {
                walk_reference_args_in_expr(&arg.expr, visit);
            }
        }
        Expr::New(n) => {
            let callee = format!("{}::init", n.class_name);
            visit_reference_args(&callee, &n.args, visit);
            for arg in &n.args {
                walk_reference_args_in_expr(&arg.expr, visit);
            }
        }
        Expr::Binary(b) => {
            walk_reference_args_in_expr(&b.lhs, visit);
            walk_reference_args_in_expr(&b.rhs, visit);
        }
        Expr::Unary(u) => walk_reference_args_in_expr(&u.expr, visit),
        Expr::FieldAccess(obj, _, _) => walk_reference_args_in_expr(obj, visit),
        Expr::ObjectLiteral(o) => {
            for field in &o.fields {
                walk_reference_args_in_expr(&field.value, visit);
            }
        }
        Expr::Await(a) => walk_reference_args_in_expr(&a.expr, visit),
        Expr::Print(arg, _, _) => walk_reference_args_in_expr(arg, visit),
        Expr::Ternary(t) => {
            walk_reference_args_in_expr(&t.condition, visit);
            walk_reference_args_in_expr(&t.then_expr, visit);
            walk_reference_args_in_expr(&t.else_expr, visit);
        }
        Expr::Range(r) => {
            walk_reference_args_in_expr(&r.start, visit);
            walk_reference_args_in_expr(&r.end, visit);
        }
        Expr::Lambda(l) => match &l.body {
            LambdaBody::Expr(e) => walk_reference_args_in_expr(e, visit),
            LambdaBody::Block(b) => walk_reference_args_in_block(b, visit),
        },
        Expr::Match(m) => {
            walk_reference_args_in_expr(&m.scrutinee, visit);
            for arm in &m.arms {
                match &arm.body {
                    MatchBody::Expr(e) => walk_reference_args_in_expr(e, visit),
                    MatchBody::Block(b) => walk_reference_args_in_block(b, visit),
                }
            }
        }
        Expr::TryPropagate(inner, _) => walk_reference_args_in_expr(inner, visit),
        Expr::ArrayLiteral(elements, _) => {
            for el in elements {
                walk_reference_args_in_expr(el, visit);
            }
        }
        Expr::Index(arr, index, _) => {
            walk_reference_args_in_expr(arr, visit);
            walk_reference_args_in_expr(index, visit);
        }
        Expr::Select(s) => {
            for case in &s.cases {
                match &case.kind {
                    SelectCaseKind::Recv { channel, .. } => {
                        walk_reference_args_in_expr(channel, visit)
                    }
                    SelectCaseKind::Send { channel, value } => {
                        walk_reference_args_in_expr(channel, visit);
                        walk_reference_args_in_expr(value, visit);
                    }
                    SelectCaseKind::Timeout { millis } => {
                        walk_reference_args_in_expr(millis, visit)
                    }
                    SelectCaseKind::Join { task, .. } => walk_reference_args_in_expr(task, visit),
                    SelectCaseKind::Default => {}
                }
                walk_reference_args_in_block(&case.body, visit);
            }
        }
        Expr::Integer(_, _)
        | Expr::Float(_, _)
        | Expr::Bool(_, _)
        | Expr::String(_, _)
        | Expr::Var(_, _) => {}
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
        if let Expr::Var(name, _) = &arg.expr {
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
    for stmt in &block.stmts {
        collect_string_literals_in_stmt(stmt, out);
    }
}

pub(crate) fn collect_string_literals_in_stmt(stmt: &Stmt, out: &mut Vec<String>) {
    match stmt {
        Stmt::Defer(d) => match &d.body {
            DeferBody::Expr(expr) => collect_string_literals_in_expr(expr, out),
            DeferBody::Block(block) => collect_string_literals_in_block(block, out),
        },
        Stmt::Break(_) | Stmt::Continue(_) => {}
        Stmt::Let(s) => collect_string_literals_in_expr(&s.init, out),
        Stmt::Assign(s) => collect_string_literals_in_expr(&s.value, out),
        Stmt::StaticFieldAssign(s) => collect_string_literals_in_expr(&s.value, out),
        Stmt::FieldAssign(s) => {
            collect_string_literals_in_expr(&s.object, out);
            collect_string_literals_in_expr(&s.value, out);
        }
        Stmt::IndexAssign(s) => {
            collect_string_literals_in_expr(&s.array, out);
            collect_string_literals_in_expr(&s.index, out);
            collect_string_literals_in_expr(&s.value, out);
        }
        Stmt::SuperInit(s) => {
            for arg in &s.args {
                collect_string_literals_in_expr(&arg.expr, out);
            }
        }
        Stmt::If(s) => {
            collect_string_literals_in_expr(&s.cond, out);
            collect_string_literals_in_block(&s.then_block, out);
            if let Some(else_block) = &s.else_block {
                collect_string_literals_in_block(else_block, out);
            }
        }
        Stmt::While(s) => {
            collect_string_literals_in_expr(&s.cond, out);
            collect_string_literals_in_block(&s.body, out);
        }
        Stmt::For(s) => {
            collect_string_literals_in_expr(&s.iterable, out);
            collect_string_literals_in_block(&s.body, out);
        }
        Stmt::Lock(s) => {
            collect_string_literals_in_expr(&s.target, out);
            collect_string_literals_in_block(&s.body, out);
        }
        Stmt::Return(s) => {
            if let Some(value) = &s.value {
                collect_string_literals_in_expr(value, out);
            }
        }
        Stmt::Expr(s) => collect_string_literals_in_expr(&s.expr, out),
    }
}

pub(crate) fn collect_string_literals_in_expr(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::StaticField(_) => {}
        Expr::String(value, _) => out.push(value.clone()),
        Expr::Binary(b) => {
            collect_string_literals_in_expr(&b.lhs, out);
            collect_string_literals_in_expr(&b.rhs, out);
        }
        Expr::Unary(u) => collect_string_literals_in_expr(&u.expr, out),
        Expr::Call(c) => {
            for arg in &c.args {
                collect_string_literals_in_expr(&arg.expr, out);
            }
            // Interpolated `format`/`panic` specs are SPLIT at emission
            // (willow-csax): collect each literal segment so
            // emit_interpolated_string finds its static data.
            if (c.callee == "format" || c.callee == "panic")
                && let Some(crate::parser::ast::Expr::String(spec, _)) =
                    c.args.first().map(|a| &a.expr)
                && let Ok(segments) = crate::interpolate::parse_spec(spec)
            {
                out.push(String::new()); // empty-accumulator fallback
                for segment in segments {
                    if let crate::interpolate::Segment::Literal(text) = segment {
                        out.push(text);
                    }
                }
            }
        }
        Expr::FieldAccess(obj, _, _) => collect_string_literals_in_expr(obj, out),
        Expr::MethodCall(m) => {
            collect_string_literals_in_expr(&m.object, out);
            for arg in &m.args {
                collect_string_literals_in_expr(&arg.expr, out);
            }
        }
        Expr::StaticCall(s) => {
            for arg in &s.args {
                collect_string_literals_in_expr(&arg.expr, out);
            }
        }
        Expr::New(n) => {
            for arg in &n.args {
                collect_string_literals_in_expr(&arg.expr, out);
            }
        }
        Expr::ObjectLiteral(o) => {
            for field in &o.fields {
                collect_string_literals_in_expr(&field.value, out);
            }
        }
        Expr::Await(a) => collect_string_literals_in_expr(&a.expr, out),
        Expr::Select(s) => {
            for case in &s.cases {
                match &case.kind {
                    SelectCaseKind::Recv { channel, .. } => {
                        collect_string_literals_in_expr(channel, out)
                    }
                    SelectCaseKind::Send { channel, value } => {
                        collect_string_literals_in_expr(channel, out);
                        collect_string_literals_in_expr(value, out);
                    }
                    SelectCaseKind::Timeout { millis } => {
                        collect_string_literals_in_expr(millis, out)
                    }
                    SelectCaseKind::Join { task, .. } => collect_string_literals_in_expr(task, out),
                    SelectCaseKind::Default => {}
                }
                collect_string_literals_in_block(&case.body, out);
            }
        }
        Expr::Print(arg, _, _) => collect_string_literals_in_expr(arg, out),
        Expr::Ternary(t) => {
            collect_string_literals_in_expr(&t.condition, out);
            collect_string_literals_in_expr(&t.then_expr, out);
            collect_string_literals_in_expr(&t.else_expr, out);
        }
        Expr::Range(r) => {
            collect_string_literals_in_expr(&r.start, out);
            collect_string_literals_in_expr(&r.end, out);
        }
        Expr::Lambda(l) => match &l.body {
            LambdaBody::Expr(e) => collect_string_literals_in_expr(e, out),
            LambdaBody::Block(b) => collect_string_literals_in_block(b, out),
        },
        Expr::Match(m) => {
            collect_string_literals_in_expr(&m.scrutinee, out);
            for arm in &m.arms {
                match &arm.body {
                    MatchBody::Expr(e) => collect_string_literals_in_expr(e, out),
                    MatchBody::Block(b) => collect_string_literals_in_block(b, out),
                }
            }
        }
        Expr::TryPropagate(inner, _) => collect_string_literals_in_expr(inner, out),
        Expr::ArrayLiteral(elements, _) => {
            for el in elements {
                collect_string_literals_in_expr(el, out);
            }
        }
        Expr::Index(arr, index, _) => {
            collect_string_literals_in_expr(arr, out);
            collect_string_literals_in_expr(index, out);
        }
        Expr::Integer(_, _) | Expr::Float(_, _) | Expr::Bool(_, _) | Expr::Var(_, _) => {}
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
    for stmt in &block.stmts {
        collect_lambdas_in_stmt(stmt, counter, out);
    }
}

pub(crate) fn collect_lambdas_in_stmt(
    stmt: &Stmt,
    counter: &mut usize,
    out: &mut Vec<(String, LambdaExpr)>,
) {
    match stmt {
        Stmt::Defer(d) => match &d.body {
            DeferBody::Expr(expr) => collect_lambdas_in_expr(expr, counter, out),
            DeferBody::Block(block) => collect_lambdas_in_block(block, counter, out),
        },
        Stmt::Break(_) | Stmt::Continue(_) => {}
        Stmt::Let(s) => collect_lambdas_in_expr(&s.init, counter, out),
        Stmt::Assign(s) => collect_lambdas_in_expr(&s.value, counter, out),
        Stmt::StaticFieldAssign(s) => collect_lambdas_in_expr(&s.value, counter, out),
        Stmt::FieldAssign(s) => {
            collect_lambdas_in_expr(&s.object, counter, out);
            collect_lambdas_in_expr(&s.value, counter, out);
        }
        Stmt::IndexAssign(s) => {
            collect_lambdas_in_expr(&s.array, counter, out);
            collect_lambdas_in_expr(&s.index, counter, out);
            collect_lambdas_in_expr(&s.value, counter, out);
        }
        Stmt::SuperInit(s) => {
            for arg in &s.args {
                collect_lambdas_in_expr(&arg.expr, counter, out);
            }
        }
        Stmt::If(s) => {
            collect_lambdas_in_expr(&s.cond, counter, out);
            collect_lambdas_in_block(&s.then_block, counter, out);
            if let Some(eb) = &s.else_block {
                collect_lambdas_in_block(eb, counter, out);
            }
        }
        Stmt::While(s) => {
            collect_lambdas_in_expr(&s.cond, counter, out);
            collect_lambdas_in_block(&s.body, counter, out);
        }
        Stmt::For(s) => {
            collect_lambdas_in_expr(&s.iterable, counter, out);
            collect_lambdas_in_block(&s.body, counter, out);
        }
        Stmt::Lock(s) => {
            collect_lambdas_in_expr(&s.target, counter, out);
            collect_lambdas_in_block(&s.body, counter, out);
        }
        Stmt::Return(s) => {
            if let Some(v) = &s.value {
                collect_lambdas_in_expr(v, counter, out);
            }
        }
        Stmt::Expr(s) => collect_lambdas_in_expr(&s.expr, counter, out),
    }
}

pub(crate) fn collect_lambdas_in_expr(
    expr: &Expr,
    counter: &mut usize,
    out: &mut Vec<(String, LambdaExpr)>,
) {
    match expr {
        Expr::Lambda(l) => {
            // Recurse into the lambda body first so nested lambdas get lower IDs.
            match &l.body {
                LambdaBody::Block(b) => collect_lambdas_in_block(b, counter, out),
                LambdaBody::Expr(e) => collect_lambdas_in_expr(e, counter, out),
            }
            let name = super::symbols::lambda_symbol(*counter);
            *counter += 1;
            out.push((name, *l.clone()));
        }
        Expr::Call(c) => {
            for arg in &c.args {
                collect_lambdas_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::Binary(b) => {
            collect_lambdas_in_expr(&b.lhs, counter, out);
            collect_lambdas_in_expr(&b.rhs, counter, out);
        }
        Expr::Unary(u) => collect_lambdas_in_expr(&u.expr, counter, out),
        Expr::Ternary(t) => {
            collect_lambdas_in_expr(&t.condition, counter, out);
            collect_lambdas_in_expr(&t.then_expr, counter, out);
            collect_lambdas_in_expr(&t.else_expr, counter, out);
        }
        Expr::Range(r) => {
            collect_lambdas_in_expr(&r.start, counter, out);
            collect_lambdas_in_expr(&r.end, counter, out);
        }
        Expr::Print(e, _, _) => collect_lambdas_in_expr(e, counter, out),
        Expr::StaticCall(s) => {
            for arg in &s.args {
                collect_lambdas_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::New(n) => {
            for arg in &n.args {
                collect_lambdas_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::ObjectLiteral(o) => {
            for field in &o.fields {
                collect_lambdas_in_expr(&field.value, counter, out);
            }
        }
        Expr::Await(a) => collect_lambdas_in_expr(&a.expr, counter, out),
        Expr::Select(s) => {
            for case in &s.cases {
                match &case.kind {
                    SelectCaseKind::Recv { channel, .. } => {
                        collect_lambdas_in_expr(channel, counter, out)
                    }
                    SelectCaseKind::Send { channel, value } => {
                        collect_lambdas_in_expr(channel, counter, out);
                        collect_lambdas_in_expr(value, counter, out);
                    }
                    SelectCaseKind::Timeout { millis } => {
                        collect_lambdas_in_expr(millis, counter, out)
                    }
                    SelectCaseKind::Join { task, .. } => {
                        collect_lambdas_in_expr(task, counter, out)
                    }
                    SelectCaseKind::Default => {}
                }
                collect_lambdas_in_block(&case.body, counter, out);
            }
        }
        Expr::MethodCall(m) => {
            collect_lambdas_in_expr(&m.object, counter, out);
            for arg in &m.args {
                collect_lambdas_in_expr(&arg.expr, counter, out);
            }
        }
        Expr::FieldAccess(e, _, _) => collect_lambdas_in_expr(e, counter, out),
        Expr::Match(m) => {
            collect_lambdas_in_expr(&m.scrutinee, counter, out);
            for arm in &m.arms {
                match &arm.body {
                    MatchBody::Expr(e) => collect_lambdas_in_expr(e, counter, out),
                    MatchBody::Block(b) => collect_lambdas_in_block(b, counter, out),
                }
            }
        }
        Expr::ArrayLiteral(elements, _) => {
            for el in elements {
                collect_lambdas_in_expr(el, counter, out);
            }
        }
        Expr::Index(arr, index, _) => {
            collect_lambdas_in_expr(arr, counter, out);
            collect_lambdas_in_expr(index, counter, out);
        }
        _ => {}
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

pub(crate) fn collect_nil_check_names_in_block(
    block: &Block,
    out: &mut std::collections::HashSet<String>,
) {
    for stmt in &block.stmts {
        collect_nil_check_names_in_stmt(stmt, out);
    }
}

pub(crate) fn collect_nil_check_names_in_stmt(
    stmt: &Stmt,
    out: &mut std::collections::HashSet<String>,
) {
    match stmt {
        Stmt::Defer(d) => match &d.body {
            DeferBody::Expr(expr) => collect_nil_check_names_in_expr(expr, out),
            DeferBody::Block(block) => collect_nil_check_names_in_block(block, out),
        },
        Stmt::Break(_) | Stmt::Continue(_) => {}
        Stmt::Let(s) => collect_nil_check_names_in_expr(&s.init, out),
        Stmt::Assign(s) => collect_nil_check_names_in_expr(&s.value, out),
        Stmt::StaticFieldAssign(s) => collect_nil_check_names_in_expr(&s.value, out),
        Stmt::FieldAssign(s) => {
            collect_nil_check_names_in_expr(&s.object, out);
            collect_nil_check_names_in_expr(&s.value, out);
        }
        Stmt::IndexAssign(s) => {
            collect_nil_check_names_in_expr(&s.array, out);
            collect_nil_check_names_in_expr(&s.index, out);
            collect_nil_check_names_in_expr(&s.value, out);
        }
        Stmt::SuperInit(s) => {
            for arg in &s.args {
                collect_nil_check_names_in_expr(&arg.expr, out);
            }
        }
        Stmt::If(s) => {
            collect_nil_check_names_in_expr(&s.cond, out);
            collect_nil_check_names_in_block(&s.then_block, out);
            if let Some(eb) = &s.else_block {
                collect_nil_check_names_in_block(eb, out);
            }
        }
        Stmt::While(s) => {
            collect_nil_check_names_in_expr(&s.cond, out);
            collect_nil_check_names_in_block(&s.body, out);
        }
        Stmt::For(s) => {
            collect_nil_check_names_in_expr(&s.iterable, out);
            collect_nil_check_names_in_block(&s.body, out);
        }
        Stmt::Lock(s) => {
            collect_nil_check_names_in_expr(&s.target, out);
            collect_nil_check_names_in_block(&s.body, out);
        }
        Stmt::Return(s) => {
            if let Some(v) = &s.value {
                collect_nil_check_names_in_expr(v, out);
            }
        }
        Stmt::Expr(s) => collect_nil_check_names_in_expr(&s.expr, out),
    }
}

pub(crate) fn collect_nil_check_names_in_expr(
    expr: &Expr,
    out: &mut std::collections::HashSet<String>,
) {
    match expr {
        Expr::StaticField(_) => {}
        Expr::FieldAccess(obj, name, _) => {
            out.insert(name.clone());
            collect_nil_check_names_in_expr(obj, out);
        }
        Expr::MethodCall(m) => {
            out.insert(m.method.clone());
            collect_nil_check_names_in_expr(&m.object, out);
            for arg in &m.args {
                collect_nil_check_names_in_expr(&arg.expr, out);
            }
        }
        Expr::Binary(b) => {
            collect_nil_check_names_in_expr(&b.lhs, out);
            collect_nil_check_names_in_expr(&b.rhs, out);
        }
        Expr::Unary(u) => collect_nil_check_names_in_expr(&u.expr, out),
        Expr::Call(c) => {
            for arg in &c.args {
                collect_nil_check_names_in_expr(&arg.expr, out);
            }
        }
        Expr::Ternary(t) => {
            collect_nil_check_names_in_expr(&t.condition, out);
            collect_nil_check_names_in_expr(&t.then_expr, out);
            collect_nil_check_names_in_expr(&t.else_expr, out);
        }
        Expr::Range(r) => {
            collect_nil_check_names_in_expr(&r.start, out);
            collect_nil_check_names_in_expr(&r.end, out);
        }
        Expr::Lambda(l) => match &l.body {
            LambdaBody::Expr(e) => collect_nil_check_names_in_expr(e, out),
            LambdaBody::Block(b) => collect_nil_check_names_in_block(b, out),
        },
        Expr::Print(e, _, _) => collect_nil_check_names_in_expr(e, out),
        Expr::Await(a) => collect_nil_check_names_in_expr(&a.expr, out),
        Expr::StaticCall(s) => {
            // A module call's call-stack frame is named as the source spells
            // it — `checks::checked`, the whole path — and no declaration walk
            // produces that string: the module declares `checked`, the caller
            // declares its own items, and nothing spells the pair. Without it
            // the frame was silently dropped and a panic inside an imported
            // function reported nothing (willow-0g8j.2.20). A class static's
            // frame is named by the bare method, which the class's own unit
            // already declares.
            out.insert(format!("{}::{}", s.class, s.method));
            for arg in &s.args {
                collect_nil_check_names_in_expr(&arg.expr, out);
            }
        }
        Expr::New(n) => {
            for arg in &n.args {
                collect_nil_check_names_in_expr(&arg.expr, out);
            }
        }
        Expr::ObjectLiteral(o) => {
            for f in &o.fields {
                collect_nil_check_names_in_expr(&f.value, out);
            }
        }
        Expr::Match(m) => {
            collect_nil_check_names_in_expr(&m.scrutinee, out);
            for arm in &m.arms {
                match &arm.body {
                    MatchBody::Expr(e) => collect_nil_check_names_in_expr(e, out),
                    MatchBody::Block(b) => collect_nil_check_names_in_block(b, out),
                }
            }
        }
        Expr::TryPropagate(inner, _) => collect_nil_check_names_in_expr(inner, out),
        Expr::ArrayLiteral(elements, _) => {
            for el in elements {
                collect_nil_check_names_in_expr(el, out);
            }
        }
        Expr::Index(arr, index, _) => {
            collect_nil_check_names_in_expr(arr, out);
            collect_nil_check_names_in_expr(index, out);
        }
        Expr::Select(s) => {
            for case in &s.cases {
                match &case.kind {
                    SelectCaseKind::Recv { channel, .. } => {
                        collect_nil_check_names_in_expr(channel, out)
                    }
                    SelectCaseKind::Send { channel, value } => {
                        collect_nil_check_names_in_expr(channel, out);
                        collect_nil_check_names_in_expr(value, out);
                    }
                    SelectCaseKind::Timeout { millis } => {
                        collect_nil_check_names_in_expr(millis, out)
                    }
                    SelectCaseKind::Join { task, .. } => collect_nil_check_names_in_expr(task, out),
                    SelectCaseKind::Default => {}
                }
                collect_nil_check_names_in_block(&case.body, out);
            }
        }
        Expr::Integer(_, _)
        | Expr::Float(_, _)
        | Expr::Bool(_, _)
        | Expr::String(_, _)
        | Expr::Var(_, _) => {}
    }
}
