//! AST analysis helpers for the type checker (extracted from `mod.rs`):
//! control-flow "always returns" checks, sub-expression walking, and
//! constructor self-field / super-init collection. Re-exported from `mod.rs`.

use std::collections::HashSet;

use crate::diagnostics::Span;
use crate::parser::ast::*;
use crate::parser::visit::{AstVisitor, walk_stmt};

/// Collect the names of fields assigned via `self.field = ...` anywhere in the
/// block (willow-scq2 §8 definite-assignment, MVP non-path-sensitive).
pub(crate) fn collect_self_field_assigns(block: &Block, out: &mut HashSet<String>) {
    let mut collector = SelfFieldAssignCollector { out };
    collector.visit_block(block);
}

/// Collect the span of every `super(...)` call in the block.
pub(crate) fn collect_super_init_spans(block: &Block, out: &mut Vec<Span>) {
    let mut collector = SuperInitSpanCollector { out };
    collector.visit_block(block);
}

/// Both constructor scans are statement-structure only: the `visit_expr`
/// override below stops the walk at every expression, so a `self.x = ...` or a
/// `super(...)` inside a lambda body or a `match` arm belongs to that body and
/// not to this constructor. A `defer` body is skipped for a related reason: it
/// runs at scope exit, after the constructor's definite-assignment point.
struct SelfFieldAssignCollector<'a> {
    out: &'a mut HashSet<String>,
}

impl AstVisitor for SelfFieldAssignCollector<'_> {
    fn visit_expr(&mut self, _expr: &Expr) {}

    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Defer(_) => return,
            Stmt::FieldAssign(assign) => {
                if matches!(&assign.object, Expr::Var(name, _) if name == "self") {
                    self.out.insert(assign.field.clone());
                }
            }
            _ => {}
        }
        walk_stmt(self, stmt);
    }
}

/// See [`SelfFieldAssignCollector`] for the shared traversal rules.
struct SuperInitSpanCollector<'a> {
    out: &'a mut Vec<Span>,
}

impl AstVisitor for SuperInitSpanCollector<'_> {
    fn visit_expr(&mut self, _expr: &Expr) {}

    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Defer(_) => return,
            Stmt::SuperInit(init) => self.out.push(init.span),
            _ => {}
        }
        walk_stmt(self, stmt);
    }
}

/// Apply `f` to each direct sub-expression of `expr` (one level deep). Used by
/// the static-initializer forward-reference scan (willow-qsqf §10.4).
pub(crate) fn walk_subexprs(expr: &Expr, f: &mut impl FnMut(&Expr)) {
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
        // A critical section runs unconditionally, so a `return` inside it
        // returns from the enclosing function (after the compiler-inserted
        // release, willow-38w.1.3).
        Stmt::Lock(s) => block_always_returns(&s.body),
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
