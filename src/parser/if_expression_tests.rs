use super::*;
use crate::lexer::Lexer;
use crate::parser::iter::{AstEvent, AstWalk};

fn parse(source: &str) -> Program {
    let (program, errors) = Parser::new(Lexer::new(source).tokenize().unwrap()).parse();
    assert!(errors.is_empty(), "{errors:?}");
    program
}

#[test]
fn if_expression_span_and_node_identity() {
    let source = "fn main() { let x = if a { 1 } else if b { 2 } else { 3 }; }";
    let program = parse(source);
    let Item::Function(f) = &program.items[0] else {
        panic!()
    };
    let Stmt::Let(binding) = &f.body.stmts[0] else {
        panic!()
    };
    let Expr::Ternary(outer) = &binding.init else {
        panic!()
    };
    let Expr::Ternary(inner) = &outer.else_expr else {
        panic!()
    };
    assert_eq!(
        &source[outer.span.start..outer.span.end],
        "if a { 1 } else if b { 2 } else { 3 }"
    );
    assert_eq!(
        &source[inner.span.start..inner.span.end],
        "if b { 2 } else { 3 }"
    );
    let mut ids = std::collections::HashSet::new();
    for event in AstWalk::new(AstEvent::Block(&f.body)) {
        if let AstEvent::Expr(expr) = event {
            assert!(ids.insert(expr.id()));
        }
    }
    assert_eq!(ids.len(), 7);
}

#[test]
fn if_expression_linear_reads_and_ast_size() {
    for shape in ["ladder", "nested", "repeated", "fanout"] {
        let mut samples = Vec::new();
        for n in [16usize, 32, 64, 128] {
            let expression = match shape {
                "ladder" => format!("{}{{ 0 }}", "if true { 1 } else ".repeat(n)),
                "nested" => format!("{}0{}", "if true { ".repeat(n), " } else { 1 }".repeat(n)),
                "repeated" => format!("[{}0]", "if true { 1 } else { 2 },".repeat(n)),
                "fanout" => {
                    let mut value = "0".to_owned();
                    for _ in 0..n.ilog2() {
                        value = format!("if true {{ {value} }} else {{ {value} }}");
                    }
                    value
                }
                _ => unreachable!(),
            };
            let source = format!("fn main() {{ let x = {expression}; }}");
            PARSER_TOKEN_READS.with(|v| v.set(0));
            let program = parse(&source);
            let reads = PARSER_TOKEN_READS.with(|v| v.get());
            let Item::Function(f) = &program.items[0] else {
                panic!()
            };
            let conditionals = AstWalk::new(AstEvent::Block(&f.body))
                .filter(|event| matches!(event, AstEvent::Expr(Expr::Ternary(_))))
                .count();
            assert_eq!(conditionals, if shape == "fanout" { n - 1 } else { n });
            samples.push((n, reads, conditionals));
        }
        let slope = (samples[1].1 - samples[0].1) / 16;
        for pair in samples.windows(2) {
            assert_eq!(
                pair[1].1 - pair[0].1,
                slope * (pair[1].0 - pair[0].0),
                "{shape}: {samples:?}"
            );
        }
        eprintln!("if-expression {shape} (size, token reads, conditional nodes): {samples:?}");
    }
}

#[test]
fn if_expression_deep_nesting_uses_continuations() {
    // Larger than a native recursive descent can safely handle. Parsing and
    // dropping the resulting AST both use the compiler's stack-safe paths.
    let n = 4096;
    let source = format!(
        "fn main() {{ let x = {}0{}; }}",
        "if true { ".repeat(n),
        " } else { 1 }".repeat(n)
    );
    let program = parse(&source);
    let Item::Function(f) = &program.items[0] else {
        panic!()
    };
    assert_eq!(
        AstWalk::new(AstEvent::Block(&f.body))
            .filter(|event| matches!(event, AstEvent::Expr(Expr::Ternary(_))))
            .count(),
        n
    );
}

#[test]
fn if_expression_statement_branch_diagnostics() {
    // Each branch position must give the same actionable diagnostic.
    for body in [
        "let z = 3; z",
        "let mut z = 3; z",
        "let (a, b) = pair(); a",
        "const z = 3; z",
        "return 3;",
        "while true {} 3",
        "for x in xs {} 3",
        "break;",
        "continue;",
        "defer cleanup(); 3",
        "need()?; 3",
        "work(); 3",
        "3;",
        "x = 3; x",
        "x += 3; x",
        "obj.field = 3; 3",
        "xs[0] = 3; 3",
        "lock mutex as value { } 3",
        "1 2",
        "1 let z = 3; z",
    ] {
        for expression in [
            format!("if true {{ {body} }} else {{ 4 }}"),
            format!("if true {{ 4 }} else {{ {body} }}"),
            format!("if true {{ 4 }} else if false {{ {body} }} else {{ 5 }}"),
            format!("if true {{ if false {{ {body} }} else {{ 4 }} }} else {{ 5 }}"),
        ] {
            let source = format!("fn main() {{ let b = {expression}; }}");
            let (_, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
            let error = &errors[0];
            assert_eq!(error.code, ErrorCode::E0102, "{source}: {errors:?}");
            assert_eq!(
                error.message, "if-expression branches must be a single expression",
                "{source}: {errors:?}"
            );
            assert!(
                error.helps.iter().any(
                    |help| help.contains("statement form") && help.contains("mutable variable")
                ),
                "{source}: {errors:?}"
            );
        }
    }
}

#[test]
fn if_expression_preserves_expression_errors_and_statement_blocks() {
    for (expression, expected) in [
        ("if true {} else { 4 }", "expected expression"),
        ("if true { 1 + } else { 4 }", "expected expression"),
        ("if true { 1 }", "requires an `else`"),
        ("if true { 1", "expected `}`"),
    ] {
        let source = format!("fn main() {{ let b = {expression}");
        let (_, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
        assert!(
            errors.iter().any(|error| error.message.contains(expected)),
            "{source}: {errors:?}"
        );
    }
    parse("fn main() { let mut b = 0; if true { let z = 3; b = z; } else { b = 4; } }");
    parse("fn main() { let b = if true { need()? } else { other()? }; }");
    parse("fn main() { let b = if true { |x| { return x; } } else { |x| { return x; } }; }");
}
