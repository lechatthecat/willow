use super::*;
use crate::lexer::Lexer;

fn errors(source: &str) -> Vec<Diagnostic> {
    let tokens = Lexer::new(source).tokenize().unwrap();
    Parser::new(tokens).parse().1
}

#[test]
fn unsupported_postfix_call_perspectives() {
    for expression in [
        "pick(0)(5)",
        "pick()(5)",
        "pick(0)()",
        "(pick(0))(5)",
        "obj.pick(0)(5)",
        "Factory::pick(0)(5)",
        "pick(0)(5)(6)",
        "1 + pick(0)(5)",
        "outer(pick(0)(5))",
        "[pick(0)(5)]",
    ] {
        let source = format!("fn main() {{ println({expression}); }}");
        let diagnostics = errors(&source);
        let diagnostic = &diagnostics[0];
        assert_eq!(
            diagnostic.message, "calling a call result is not supported",
            "{source}: {diagnostics:?}"
        );
        assert!(
            diagnostic
                .helps
                .iter()
                .any(|s| s.contains("let f = pick(0); f(5)"))
        );
        let span = diagnostic.labels[0].span;
        assert_eq!(&source[span.start as usize..span.end as usize], "(");
    }
}

#[test]
fn unsupported_postfix_cast_perspectives() {
    for statement in [
        "println(x as f64);",
        "let y = x as i64;",
        "return x as f64;",
        "x as f64;",
        "println((x as f64));",
        "let y = [x as f64];",
        "let y = a[x as i64];",
        "if x as bool {}",
        "while x as bool {}",
        "let y = yes ? x as f64 : z;",
        "let y = yes ? z : x as f64;",
        "lock m.get(x as f64) as v {}",
        "lock (x as f64) as v {}",
    ] {
        let source = format!("fn main() {{ {statement} }}");
        let diagnostics = errors(&source);
        let diagnostic = &diagnostics[0];
        assert_eq!(
            diagnostic.message, "`as` casts are not supported",
            "{source}: {diagnostics:?}"
        );
        assert!(diagnostic.helps.iter().any(|s| s.contains("f64 literal")));
        let span = diagnostic.labels[0].span;
        assert_eq!(&source[span.start as usize..span.end as usize], "as");
    }
}

#[test]
fn unsupported_postfix_preserves_supported_syntax() {
    for source in [
        "fn main() { let f = pick(0); f(5); }",
        "fn main() { println(pick(0)); }",
        "fn main() { obj.pick(0).run(5); }",
        "fn main() { let x = pick(0)[5]; }",
        "fn main() { lock m as v {} }",
        "fn main() { lock read m as v {} }",
        "fn main() { lock write m as mut v {} }",
        "fn main() { lock m.get() as v {} }",
        "fn main() { lock read as v {} }",
        "import std::math as math; fn main() {}",
    ] {
        assert!(errors(source).is_empty(), "{source}: {:?}", errors(source));
    }
}

#[test]
fn unsupported_postfix_preserves_missing_parenthesis() {
    assert_eq!(errors("fn main() { println(x; }")[0].code, ErrorCode::E0104);
}

#[test]
fn unsupported_postfix_token_reads_scale_linearly() {
    for statement in [
        "println(pick(0)(5));",
        "println(x as f64);",
        "let f = pick(0); f(5);",
    ] {
        let mut samples = Vec::new();
        for count in [8, 16, 32, 64] {
            let source = format!("fn main() {{ {} }}", statement.repeat(count));
            PARSER_TOKEN_READS.with(|v| v.set(0));
            let diagnostics = errors(&source);
            assert_eq!(
                diagnostics.len(),
                if statement.starts_with("let") {
                    0
                } else {
                    count
                }
            );
            samples.push((count, PARSER_TOKEN_READS.with(|v| v.get())));
        }
        let slope = (samples[1].1 - samples[0].1) / 8;
        for pair in samples.windows(2) {
            assert_eq!(pair[1].1 - pair[0].1, slope * (pair[1].0 - pair[0].0));
        }
        eprintln!("{statement}: {samples:?}");
    }
}
