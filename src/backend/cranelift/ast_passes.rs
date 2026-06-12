//! AST collection passes for the Cranelift backend (extracted from `mod.rs`).
//! Pure recursive walkers that gather string literals, lambdas, nil-checked
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

pub(crate) fn collect_reference_debug_strings_in_block(block: &Block, out: &mut HashSet<String>) {
    for stmt in &block.stmts {
        collect_reference_debug_strings_in_stmt(stmt, out);
    }
}

pub(crate) fn collect_reference_debug_strings_in_stmt(stmt: &Stmt, out: &mut HashSet<String>) {
    match stmt {
        Stmt::Let(s) => collect_reference_debug_strings_in_expr(&s.init, out),
        Stmt::Assign(s) => collect_reference_debug_strings_in_expr(&s.value, out),
        Stmt::StaticFieldAssign(s) => collect_reference_debug_strings_in_expr(&s.value, out),
        Stmt::FieldAssign(s) => {
            collect_reference_debug_strings_in_expr(&s.object, out);
            collect_reference_debug_strings_in_expr(&s.value, out);
        }
        Stmt::IndexAssign(s) => {
            collect_reference_debug_strings_in_expr(&s.array, out);
            collect_reference_debug_strings_in_expr(&s.index, out);
            collect_reference_debug_strings_in_expr(&s.value, out);
        }
        Stmt::SuperInit(s) => {
            collect_reference_debug_call_arg_strings("super.init", &s.args, out);
            for arg in &s.args {
                collect_reference_debug_strings_in_expr(&arg.expr, out);
            }
        }
        Stmt::If(s) => {
            collect_reference_debug_strings_in_expr(&s.cond, out);
            collect_reference_debug_strings_in_block(&s.then_block, out);
            if let Some(else_block) = &s.else_block {
                collect_reference_debug_strings_in_block(else_block, out);
            }
        }
        Stmt::While(s) => {
            collect_reference_debug_strings_in_expr(&s.cond, out);
            collect_reference_debug_strings_in_block(&s.body, out);
        }
        Stmt::For(s) => {
            collect_reference_debug_strings_in_expr(&s.iterable, out);
            collect_reference_debug_strings_in_block(&s.body, out);
        }
        Stmt::Return(s) => {
            if let Some(value) = &s.value {
                collect_reference_debug_strings_in_expr(value, out);
            }
        }
        Stmt::Expr(s) => collect_reference_debug_strings_in_expr(&s.expr, out),
    }
}

pub(crate) fn collect_reference_debug_strings_in_expr(expr: &Expr, out: &mut HashSet<String>) {
    match expr {
        Expr::StaticField(_) => {}
        Expr::Call(c) => {
            collect_reference_debug_call_arg_strings(&c.callee, &c.args, out);
            for arg in &c.args {
                collect_reference_debug_strings_in_expr(&arg.expr, out);
            }
        }
        Expr::MethodCall(m) => {
            collect_reference_debug_strings_in_expr(&m.object, out);
            collect_reference_debug_call_arg_strings(&m.method, &m.args, out);
            for arg in &m.args {
                collect_reference_debug_strings_in_expr(&arg.expr, out);
            }
        }
        Expr::StaticCall(s) => {
            let callee = format!("{}::{}", s.class, s.method);
            collect_reference_debug_call_arg_strings(&callee, &s.args, out);
            for arg in &s.args {
                collect_reference_debug_strings_in_expr(&arg.expr, out);
            }
        }
        Expr::New(n) => {
            for arg in &n.args {
                collect_reference_debug_strings_in_expr(&arg.expr, out);
            }
        }
        Expr::Binary(b) => {
            collect_reference_debug_strings_in_expr(&b.lhs, out);
            collect_reference_debug_strings_in_expr(&b.rhs, out);
        }
        Expr::Unary(u) => collect_reference_debug_strings_in_expr(&u.expr, out),
        Expr::FieldAccess(obj, _, _) => collect_reference_debug_strings_in_expr(obj, out),
        Expr::ObjectLiteral(o) => {
            for field in &o.fields {
                collect_reference_debug_strings_in_expr(&field.value, out);
            }
        }
        Expr::Await(a) => collect_reference_debug_strings_in_expr(&a.expr, out),
        Expr::Print(arg, _, _) => collect_reference_debug_strings_in_expr(arg, out),
        Expr::Ternary(t) => {
            collect_reference_debug_strings_in_expr(&t.condition, out);
            collect_reference_debug_strings_in_expr(&t.then_expr, out);
            collect_reference_debug_strings_in_expr(&t.else_expr, out);
        }
        Expr::Range(r) => {
            collect_reference_debug_strings_in_expr(&r.start, out);
            collect_reference_debug_strings_in_expr(&r.end, out);
        }
        Expr::Lambda(l) => match &l.body {
            LambdaBody::Expr(e) => collect_reference_debug_strings_in_expr(e, out),
            LambdaBody::Block(b) => collect_reference_debug_strings_in_block(b, out),
        },
        Expr::Match(m) => {
            collect_reference_debug_strings_in_expr(&m.scrutinee, out);
            for arm in &m.arms {
                match &arm.body {
                    MatchBody::Expr(e) => collect_reference_debug_strings_in_expr(e, out),
                    MatchBody::Block(b) => collect_reference_debug_strings_in_block(b, out),
                }
            }
        }
        Expr::TryPropagate(inner, _) => collect_reference_debug_strings_in_expr(inner, out),
        Expr::ArrayLiteral(elements, _) => {
            for el in elements {
                collect_reference_debug_strings_in_expr(el, out);
            }
        }
        Expr::Index(arr, index, _) => {
            collect_reference_debug_strings_in_expr(arr, out);
            collect_reference_debug_strings_in_expr(index, out);
        }
        Expr::Select(s) => {
            for case in &s.cases {
                match &case.kind {
                    SelectCaseKind::Recv { channel, .. } => {
                        collect_reference_debug_strings_in_expr(channel, out)
                    }
                    SelectCaseKind::Send { channel, value } => {
                        collect_reference_debug_strings_in_expr(channel, out);
                        collect_reference_debug_strings_in_expr(value, out);
                    }
                    SelectCaseKind::Default => {}
                }
                collect_reference_debug_strings_in_block(&case.body, out);
            }
        }
        Expr::Integer(_, _)
        | Expr::Float(_, _)
        | Expr::Bool(_, _)
        | Expr::Nil(_)
        | Expr::String(_, _)
        | Expr::Var(_, _) => {}
    }
}

pub(crate) fn collect_reference_debug_call_arg_strings(
    callee: &str,
    args: &[CallArg],
    out: &mut HashSet<String>,
) {
    for arg in args {
        if matches!(&arg.mode, CallArgMode::Reference { .. }) {
            out.insert(callee.to_string());
            out.insert(reference_place_kind(&arg.expr).to_string());
            out.insert(reference_place_name(&arg.expr));
        }
    }
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
        Expr::Integer(_, _)
        | Expr::Float(_, _)
        | Expr::Bool(_, _)
        | Expr::Nil(_)
        | Expr::Var(_, _) => {}
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

pub(crate) fn collect_lambdas_in_stmt(stmt: &Stmt, counter: &mut usize, out: &mut Vec<(String, LambdaExpr)>) {
    match stmt {
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
        Stmt::Return(s) => {
            if let Some(v) = &s.value {
                collect_lambdas_in_expr(v, counter, out);
            }
        }
        Stmt::Expr(s) => collect_lambdas_in_expr(&s.expr, counter, out),
    }
}

pub(crate) fn collect_lambdas_in_expr(expr: &Expr, counter: &mut usize, out: &mut Vec<(String, LambdaExpr)>) {
    match expr {
        Expr::Lambda(l) => {
            // Recurse into the lambda body first so nested lambdas get lower IDs.
            match &l.body {
                LambdaBody::Block(b) => collect_lambdas_in_block(b, counter, out),
                LambdaBody::Expr(e) => collect_lambdas_in_expr(e, counter, out),
            }
            let name = format!("__lambda_{}", *counter);
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

pub(crate) fn collect_nil_check_names_in_block(block: &Block, out: &mut std::collections::HashSet<String>) {
    for stmt in &block.stmts {
        collect_nil_check_names_in_stmt(stmt, out);
    }
}

pub(crate) fn collect_nil_check_names_in_stmt(stmt: &Stmt, out: &mut std::collections::HashSet<String>) {
    match stmt {
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
        Stmt::Return(s) => {
            if let Some(v) = &s.value {
                collect_nil_check_names_in_expr(v, out);
            }
        }
        Stmt::Expr(s) => collect_nil_check_names_in_expr(&s.expr, out),
    }
}

pub(crate) fn collect_nil_check_names_in_expr(expr: &Expr, out: &mut std::collections::HashSet<String>) {
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
                    SelectCaseKind::Default => {}
                }
                collect_nil_check_names_in_block(&case.body, out);
            }
        }
        Expr::Integer(_, _)
        | Expr::Float(_, _)
        | Expr::Bool(_, _)
        | Expr::Nil(_)
        | Expr::String(_, _)
        | Expr::Var(_, _) => {}
    }
}
