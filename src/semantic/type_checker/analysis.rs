//! AST analysis helpers for the type checker (extracted from `mod.rs`):
//! control-flow "always returns" checks, reference places, and
//! constructor super-init collection. Re-exported from `mod.rs`.

use crate::diagnostics::Span;
use crate::parser::ast::*;
use crate::parser::iter::{AstEvent, AstWalk};

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

/// Whether every path through a `match` arm's block leaves the arm without
/// reaching its end: by `return`, or by a `break`/`continue` of a loop that
/// encloses the `match` (willow-jz15.46). Such an arm hands no value to the
/// match, so it is typed `Never`. Loops written inside the arm are not entered,
/// so their own `break`/`continue` never count.
#[cfg(test)]
pub(crate) fn block_always_leaves_arm(block: &Block) -> bool {
    cached_arm_leaves(block, &mut std::collections::HashMap::new())
}

/// Syntax-only facts, scoped to an immutable AST/body-query result.
pub(crate) fn cached_arm_leaves(
    block: &Block,
    cache: &mut std::collections::HashMap<BodyId, bool>,
) -> bool {
    always_leaves(ReturnNode::Block(block), cache)
}

#[cfg(test)]
thread_local! { pub(crate) static ARM_BLOCK_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

enum ReturnNode<'a> {
    Cached(bool),
    Block(&'a Block),
    Stmt(&'a Stmt),
}

/// A small boolean evaluator preserves short-circuit traversal without using
/// native recursion or the type-erased compiler continuation stack.
///
/// The evaluator never descends into a nested loop body, so any `break` or
/// `continue` it reaches targets a loop outside the analysed block.
fn always_leaves(
    mut node: ReturnNode<'_>,
    cache: &mut std::collections::HashMap<BodyId, bool>,
) -> bool {
    enum Frame<'a> {
        Save(BodyId),
        Any(std::slice::Iter<'a, Stmt>),
        All(std::slice::Iter<'a, MatchArm>),
        ThenElse(&'a Block),
    }
    let mut frames = Vec::new();
    'evaluate: loop {
        let mut result = match node {
            ReturnNode::Cached(result) => result,
            ReturnNode::Block(block) => {
                if let Some(result) = cache.get(&block.id) {
                    // Feed a cached result through the same continuation frames.
                    node = ReturnNode::Cached(*result);
                    continue;
                }
                #[cfg(test)]
                ARM_BLOCK_VISITS.with(|count| count.set(count.get() + 1));
                frames.push(Frame::Save(block.id));
                let mut stmts = block.stmts.iter();
                if let Some(stmt) = stmts.next() {
                    frames.push(Frame::Any(stmts));
                    node = ReturnNode::Stmt(stmt);
                    continue;
                }
                false
            }
            ReturnNode::Stmt(stmt) => match stmt {
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
                Stmt::Return(_) | Stmt::Break(_) | Stmt::Continue(_) => true,
                // Loops need not execute; deferred bodies run later.
                Stmt::Defer(_)
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
                Frame::Save(id) => {
                    cache.insert(id, result);
                }
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
    //! The structural walker is statement-only, so the perspectives are: a1
    //! direct assignment, a2 nested block statements (`if` / `else` / `while` /
    //! `for`), a3 a `defer` body is skipped, a4 an expression body is never
    //! entered (a lambda is a separate callable), a5 an assignment to a
    //! non-`self` object is not counted, a6 `super.init` spans are collected in
    //! source order including one inside a branch, a7 a `super.init` in a
    //! `defer` body is skipped.
    use super::*;
    use std::collections::HashSet;

    /// All arm blocks are queried in both traversal orders; repeated queries
    /// model consumers and short-circuit-skipped siblings being visited later.
    #[test]
    fn arm_leave_nested_depth_counts_are_linear() {
        for depth in [10, 100, 1000] {
            for leaf in ["return;", "let x = 1;"] {
                let mut source = String::from("fn f() {");
                for _ in 0..depth {
                    source.push_str("match true { true => {");
                }
                source.push_str(leaf);
                for _ in 0..depth {
                    source.push_str("} false => { return; } }");
                }
                source.push('}');
                let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
                let (program, errors) = crate::parser::Parser::new(tokens).parse();
                assert!(errors.is_empty(), "{errors:?}");
                let Item::Function(function) = &program.items[0] else {
                    panic!()
                };
                let blocks: Vec<_> = AstWalk::new(AstEvent::Block(&function.body))
                    .filter_map(|event| match event {
                        AstEvent::Block(b) => Some(b),
                        _ => None,
                    })
                    .collect();
                assert_eq!(blocks.len(), 2 * depth + 1);
                for reverse in [false, true] {
                    let mut cache = std::collections::HashMap::new();
                    ARM_BLOCK_VISITS.set(0);
                    for index in 0..blocks.len() {
                        let block = blocks[if reverse {
                            blocks.len() - 1 - index
                        } else {
                            index
                        }];
                        cached_arm_leaves(block, &mut cache);
                    }
                    assert_eq!(ARM_BLOCK_VISITS.get(), blocks.len());
                    for block in &blocks {
                        cached_arm_leaves(block, &mut cache);
                    }
                    assert_eq!(ARM_BLOCK_VISITS.get(), blocks.len());
                    assert_eq!(cache[&function.body.id], leaf == "return;");
                    println!(
                        "depth={depth} returning={} reverse={reverse} evaluations={}",
                        leaf == "return;",
                        ARM_BLOCK_VISITS.get()
                    );
                }

                let mut checker = crate::semantic::TypeChecker::new();
                ARM_BLOCK_VISITS.set(0);
                checker.check_program(&program);
                assert!(checker.errors.is_empty(), "{:?}", checker.errors);
                assert_eq!(ARM_BLOCK_VISITS.get(), 2 * depth);
                let unit = crate::compiler_db::CheckedUnit::from(checker);
                let encoded = serde_json::to_vec(&unit).unwrap();
                let unit: crate::compiler_db::CheckedUnit =
                    serde_json::from_slice(&encoded).unwrap();
                ARM_BLOCK_VISITS.set(0);
                let (_, errors) = crate::ir::lower::lower_program_with(&program, &unit.tables());
                assert!(errors.is_empty(), "{errors:?}");
                assert_eq!(
                    ARM_BLOCK_VISITS.get(),
                    0,
                    "lowering must reuse checked facts"
                );
                let (_, errors) = crate::ir::lower::lower_program(&program);
                assert!(errors.is_empty(), "{errors:?}");
                assert_eq!(
                    ARM_BLOCK_VISITS.get(),
                    2 * depth,
                    "unchecked lowering also memoizes"
                );
            }
        }
    }

    #[test]
    fn return_analysis_handles_fifty_thousand_branches_on_small_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let span = Span::new(0, 0, 1, 1);
                let returning = || Block {
                    id: crate::parser::ast::BodyId::fresh(),
                    stmts: vec![Stmt::Return(ReturnStmt { value: None, span })],
                    span,
                };
                let mut body = returning();
                for _ in 0..50_000 {
                    body = Block {
                        id: crate::parser::ast::BodyId::fresh(),
                        stmts: vec![Stmt::If(IfStmt {
                            cond: Expr::Bool(true, span, ExprId::fresh()),
                            then_block: body,
                            else_block: Some(returning()),
                            span,
                        })],
                        span,
                    };
                }
                assert!(block_always_leaves_arm(&body));
                // The outer else must also return, even when every then branch does.
                if let Stmt::If(branch) = &mut body.stmts[0] {
                    branch.else_block.as_mut().unwrap().stmts.clear();
                }
                assert!(!block_always_leaves_arm(&body));
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
                    id: crate::parser::ast::BodyId::fresh(),
                    stmts: vec![Stmt::SuperInit(SuperInitStmt { args: vec![], span })],
                    span,
                };
                for _ in 0..50_000 {
                    body = Block {
                        id: crate::parser::ast::BodyId::fresh(),
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
                collect_test_assignments(&body, &mut fields);
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
        collect_test_assignments(&class_ctor_body(src, "C"), &mut out);
        let mut names: Vec<String> = out.into_iter().collect();
        names.sort();
        names
    }

    // Exercise the structural walker independently of definite assignment.
    fn collect_test_assignments(block: &Block, out: &mut HashSet<String>) {
        walk_constructor_statements(block, |stmt| {
            if let Stmt::FieldAssign(assign) = stmt
                && matches!(&assign.object, Expr::Var(name, ..) if name == "self")
            {
                out.insert(assign.field.clone());
            }
        });
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
