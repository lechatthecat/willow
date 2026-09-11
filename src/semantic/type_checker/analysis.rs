//! AST analysis helpers for the type checker (extracted from `mod.rs`):
//! control-flow "always returns" checks, sub-expression walking, and
//! constructor self-field / super-init collection. Re-exported from `mod.rs`.

use std::collections::HashSet;

use crate::diagnostics::Span;
use crate::parser::ast::*;
use crate::parser::iter::{AstEvent, AstWalk};

/// Collect the names of fields assigned via `self.field = ...` anywhere in the
/// block (willow-scq2 §8 definite-assignment, MVP non-path-sensitive).
pub(crate) fn collect_self_field_assigns(block: &Block, out: &mut HashSet<String>) {
    walk_constructor_statements(block, |stmt| {
        if let Stmt::FieldAssign(assign) = stmt
            && matches!(&assign.object, Expr::Var(name, _, _) if name == "self")
        {
            out.insert(assign.field.clone());
        }
    });
}

/// Collect the span of every `super(...)` call in the block.
pub(crate) fn collect_super_init_spans(block: &Block, out: &mut Vec<Span>) {
    walk_constructor_statements(block, |stmt| {
        if let Stmt::SuperInit(init) = stmt {
            out.push(init.span);
        }
    });
}

/// Constructor scans only inspect statement structure: assignments in a lambda
/// or match expression belong to that body. Deferred work runs after the
/// constructor's definite-assignment point and must also be excluded.
fn walk_constructor_statements(block: &Block, mut visit: impl FnMut(&Stmt)) {
    let mut walk = AstWalk::new(AstEvent::Block(block));
    while let Some(event) = walk.next() {
        match event {
            AstEvent::Expr(_) | AstEvent::Stmt(Stmt::Defer(_)) => walk.skip_children(),
            AstEvent::Stmt(stmt) => visit(stmt),
            _ => {}
        }
    }
}

/// Apply `f` to each direct sub-expression of `expr` (one level deep). Used by
/// the static-initializer forward-reference scan (willow-qsqf §10.4).
pub(crate) fn walk_subexprs<'a>(expr: &'a Expr, f: &mut impl FnMut(&'a Expr)) {
    match expr {
        Expr::Integer(..)
        | Expr::Float(..)
        | Expr::Bool(..)
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
        Expr::FieldAccess(o, _, _, _) => f(o),
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
        Expr::Print(e, _, _, _) => f(e),
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
        Expr::TryPropagate(e, _, _) => f(e),
        Expr::ArrayLiteral(els, _, _) => {
            for e in els {
                f(e);
            }
        }
        Expr::Index(a, i, _, _) => {
            f(a);
            f(i);
        }
    }
}

pub(crate) fn reference_place_key(mut expr: &Expr) -> Option<String> {
    let mut suffixes: Vec<String> = Vec::new();
    loop {
        match expr {
            Expr::Var(name, ..) => {
                let mut result = name.clone();
                for suffix in suffixes.into_iter().rev() {
                    result.push_str(&suffix);
                }
                return Some(result);
            }
            Expr::FieldAccess(object, name, ..) => {
                suffixes.push(format!(".{name}"));
                expr = object;
            }
            Expr::Index(array, index, ..) => {
                let Expr::Integer(value, ..) = &**index else {
                    return None;
                };
                suffixes.push(format!("[{value}]"));
                expr = array;
            }
            _ => return None,
        }
    }
}

pub(crate) fn block_always_returns(block: &Block) -> bool {
    always_returns(ReturnNode::Block(block))
}

enum ReturnNode<'a> {
    Block(&'a Block),
    Stmt(&'a Stmt),
}

/// A small boolean evaluator preserves short-circuit traversal without using
/// native recursion or the type-erased compiler continuation stack.
fn always_returns(mut node: ReturnNode<'_>) -> bool {
    enum Frame<'a> {
        Any(std::slice::Iter<'a, Stmt>),
        All(std::slice::Iter<'a, MatchArm>),
        ThenElse(&'a Block),
    }
    let mut frames = Vec::new();
    'evaluate: loop {
        let mut result = match node {
            ReturnNode::Block(block) => {
                let mut stmts = block.stmts.iter();
                if let Some(stmt) = stmts.next() {
                    frames.push(Frame::Any(stmts));
                    node = ReturnNode::Stmt(stmt);
                    continue;
                }
                false
            }
            ReturnNode::Stmt(stmt) => match stmt {
                Stmt::Return(_) => true,
                Stmt::If(branch) => {
                    if let Some(other) = &branch.else_block {
                        frames.push(Frame::ThenElse(other));
                        node = ReturnNode::Block(&branch.then_block);
                        continue;
                    }
                    false
                }
                Stmt::Lock(lock) => {
                    node = ReturnNode::Block(&lock.body);
                    continue;
                }
                Stmt::Expr(expr) => {
                    if let Expr::Match(matched) = &expr.expr {
                        let mut arms = matched.arms.iter();
                        if let Some(MatchArm {
                            body: MatchBody::Block(block),
                            ..
                        }) = arms.next()
                        {
                            frames.push(Frame::All(arms));
                            node = ReturnNode::Block(block);
                            continue;
                        }
                    }
                    false
                }
                // Loops need not execute; deferred bodies run later. Neither
                // break nor continue guarantees a return from the function.
                Stmt::Defer(_)
                | Stmt::Break(_)
                | Stmt::Continue(_)
                | Stmt::Let(_)
                | Stmt::Assign(_)
                | Stmt::FieldAssign(_)
                | Stmt::SuperInit(_)
                | Stmt::StaticFieldAssign(_)
                | Stmt::IndexAssign(_)
                | Stmt::While(_)
                | Stmt::For(_) => false,
            },
        };
        while let Some(frame) = frames.pop() {
            match frame {
                Frame::Any(mut stmts) if !result => {
                    if let Some(stmt) = stmts.next() {
                        frames.push(Frame::Any(stmts));
                        node = ReturnNode::Stmt(stmt);
                        continue 'evaluate;
                    }
                }
                Frame::All(mut arms) if result => match arms.next() {
                    Some(MatchArm {
                        body: MatchBody::Block(block),
                        ..
                    }) => {
                        frames.push(Frame::All(arms));
                        node = ReturnNode::Block(block);
                        continue 'evaluate;
                    }
                    Some(_) => result = false,
                    None => {}
                },
                Frame::ThenElse(other) if result => {
                    node = ReturnNode::Block(other);
                    continue 'evaluate;
                }
                _ => {}
            }
        }
        return result;
    }
}

#[cfg(test)]
mod tests {
    //! Constructor-scan perspectives (willow-uqzx.1.1, shared structural walk).
    //!
    //! Both scans are statement-structure only, so the perspectives are: a1
    //! direct assignment, a2 nested block statements (`if` / `else` / `while` /
    //! `for`), a3 a `defer` body is skipped, a4 an expression body is never
    //! entered (a lambda is a separate callable), a5 an assignment to a
    //! non-`self` object is not counted, a6 `super.init` spans are collected in
    //! source order including one inside a branch, a7 a `super.init` in a
    //! `defer` body is skipped.
    use super::*;

    #[test]
    fn return_analysis_handles_fifty_thousand_branches_on_small_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let span = Span::new(0, 0, 1, 1);
                let returning = || Block {
                    stmts: vec![Stmt::Return(ReturnStmt { value: None, span })],
                    span,
                };
                let mut body = returning();
                for _ in 0..50_000 {
                    body = Block {
                        stmts: vec![Stmt::If(IfStmt {
                            cond: Expr::Bool(true, span, ExprId::fresh()),
                            then_block: body,
                            else_block: Some(returning()),
                            span,
                        })],
                        span,
                    };
                }
                assert!(block_always_returns(&body));
                // The outer else must also return, even when every then branch does.
                if let Stmt::If(branch) = &mut body.stmts[0] {
                    branch.else_block.as_mut().unwrap().stmts.clear();
                }
                assert!(!block_always_returns(&body));
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn constructor_scans_use_a_one_megabyte_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let span = Span::new(0, 0, 1, 1);
                let mut body = Block {
                    stmts: vec![Stmt::SuperInit(SuperInitStmt { args: vec![], span })],
                    span,
                };
                for _ in 0..50_000 {
                    body = Block {
                        stmts: vec![Stmt::If(IfStmt {
                            cond: Expr::Bool(true, span, ExprId::fresh()),
                            then_block: body,
                            else_block: None,
                            span,
                        })],
                        span,
                    };
                }
                let mut spans = Vec::new();
                collect_super_init_spans(&body, &mut spans);
                assert_eq!(spans, [span]);
                let mut fields = HashSet::new();
                collect_self_field_assigns(&body, &mut fields);
                assert!(fields.is_empty());
                drop(body);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    fn class_ctor_body(src: &str, class_name: &str) -> Block {
        let tokens = crate::lexer::Lexer::new(src).tokenize().expect("lex");
        let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
        assert!(parse_errors.is_empty(), "parse errors: {parse_errors:?}");
        for item in program.items {
            if let Item::Class(class) = item
                && class.name == class_name
            {
                return class
                    .constructors
                    .into_iter()
                    .next()
                    .expect("class has a constructor")
                    .body;
            }
        }
        panic!("no class `{class_name}` in source");
    }

    fn assigned_fields(src: &str) -> Vec<String> {
        let mut out = HashSet::new();
        collect_self_field_assigns(&class_ctor_body(src, "C"), &mut out);
        let mut names: Vec<String> = out.into_iter().collect();
        names.sort();
        names
    }

    fn super_init_lines(src: &str) -> Vec<usize> {
        let mut out = Vec::new();
        collect_super_init_spans(&class_ctor_body(src, "C"), &mut out);
        out.into_iter().map(|span| span.line).collect()
    }

    #[test]
    fn a1_direct_self_assignment_is_collected() {
        let src = "class C {\n\
                   x: i64; y: i64;\n\
                   init(self) { self.x = 1; self.y = 2; }\n\
                 }\nfn main() {}";
        assert_eq!(assigned_fields(src), vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn a2_nested_block_statements_are_collected() {
        let src = "class C {\n\
                   a: i64; b: i64; c: i64; d: i64;\n\
                   init(self, n: i64) {\n\
                     if n > 0 { self.a = 1; } else { self.b = 2; }\n\
                     while n > 100 { self.c = 3; }\n\
                     for i in 0..1 { self.d = 4; }\n\
                   }\n\
                 }\nfn main() {}";
        assert_eq!(
            assigned_fields(src),
            vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string()
            ]
        );
    }

    #[test]
    fn a3_defer_body_assignment_is_skipped() {
        // A `defer` runs at scope exit, after the definite-assignment point.
        let src = "class C {\n\
                   x: i64;\n\
                   init(self) { self.x = 1; defer { self.x = 2; } }\n\
                 }\nfn main() {}";
        assert_eq!(assigned_fields(src), vec!["x".to_string()]);
    }

    #[test]
    fn a4_a_lambda_body_assignment_is_not_the_constructors() {
        // The walk stops at every expression, so the lambda body - a separate
        // callable - is never entered.
        let src = "class C {\n\
                   x: i64; y: i64;\n\
                   init(self) {\n\
                     self.x = 1;\n\
                     let f = || { self.y = 2; };\n\
                   }\n\
                 }\nfn main() {}";
        assert_eq!(assigned_fields(src), vec!["x".to_string()]);
    }

    #[test]
    fn a5_assignment_to_another_object_is_not_counted() {
        let src = "class C {\n\
                   pub x: i64;\n\
                   init(self, other: C) {\n\
                     self.x = 1;\n\
                     other.x = 2;\n\
                   }\n\
                 }\nfn main() {}";
        assert_eq!(assigned_fields(src), vec!["x".to_string()]);
    }

    #[test]
    fn a6_super_init_spans_are_collected_in_source_order() {
        let src = "open class B {\n\
                   init(self) {}\n\
                 }\n\
                 class C extends B {\n\
                   init(self, n: i64) {\n\
                     super.init();\n\
                     if n > 0 { super.init(); }\n\
                   }\n\
                 }\nfn main() {}";
        assert_eq!(super_init_lines(src), vec![6, 7]);
    }

    #[test]
    fn a7_super_init_in_a_defer_body_is_skipped() {
        let src = "open class B {\n\
                   init(self) {}\n\
                 }\n\
                 class C extends B {\n\
                   init(self) {\n\
                     defer { super.init(); }\n\
                   }\n\
                 }\nfn main() {}";
        assert!(super_init_lines(src).is_empty());
    }
}
