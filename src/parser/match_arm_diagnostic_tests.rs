use super::*;
use crate::lexer::Lexer;

fn errors(source: &str) -> Vec<Diagnostic> {
    Parser::new(Lexer::new(source).tokenize().unwrap())
        .parse()
        .1
}

#[test]
fn match_arm_targeted_diagnostics() {
    // Control flow, all assignment operators and lvalue shapes, and tail shapes.
    for (body, message, help, marker) in [
        (
            "break",
            "`break` in a match arm requires a block",
            "{ break; }",
            "break",
        ),
        (
            "continue",
            "`continue` in a match arm requires a block",
            "{ continue; }",
            "continue",
        ),
        (
            "n = v",
            "assignment in a match arm requires a block",
            "{ n = v; }",
            "=",
        ),
        (
            "n += v",
            "assignment in a match arm requires a block",
            "{ n += v; }",
            "+=",
        ),
        (
            "n -= v",
            "assignment in a match arm requires a block",
            "{ n += v; }",
            "-=",
        ),
        (
            "n *= v",
            "assignment in a match arm requires a block",
            "{ n += v; }",
            "*=",
        ),
        (
            "n /= v",
            "assignment in a match arm requires a block",
            "{ n += v; }",
            "/=",
        ),
        (
            "n %= v",
            "assignment in a match arm requires a block",
            "{ n += v; }",
            "%=",
        ),
        (
            "n &= v",
            "assignment in a match arm requires a block",
            "{ n += v; }",
            "&=",
        ),
        (
            "n |= v",
            "assignment in a match arm requires a block",
            "{ n += v; }",
            "|=",
        ),
        (
            "n ^= v",
            "assignment in a match arm requires a block",
            "{ n += v; }",
            "^=",
        ),
        (
            "n <<= v",
            "assignment in a match arm requires a block",
            "{ n += v; }",
            "<<=",
        ),
        (
            "n >>= v",
            "assignment in a match arm requires a block",
            "{ n += v; }",
            ">>=",
        ),
        (
            "obj.field += v",
            "assignment in a match arm requires a block",
            "{ n += v; }",
            "+=",
        ),
        (
            "a[i] = v",
            "assignment in a match arm requires a block",
            "{ n = v; }",
            "=",
        ),
        (
            "C::field = v",
            "assignment in a match arm requires a block",
            "{ n = v; }",
            "=",
        ),
        (
            "{ let y = 1; y }",
            "block arms cannot yield a value",
            "helper function or an if-expression",
            "y }",
        ),
        ("{ 1 }", "block arms cannot yield a value", "add `;`", "1"),
        (
            "{ f() }",
            "block arms cannot yield a value",
            "helper function",
            "f()",
        ),
        (
            "{ (n + v) }",
            "block arms cannot yield a value",
            "if-expression",
            "+",
        ),
    ] {
        let prefix = "fn main() { match true { true => ";
        let source = format!("{prefix}{body}, false => {{}} }} }}");
        let diagnostics = errors(&source);
        let error = diagnostics
            .iter()
            .find(|e| e.message == message)
            .unwrap_or_else(|| panic!("{source}: {diagnostics:?}"));
        assert!(error.helps.iter().any(|h| h.contains(help)), "{error:?}");
        assert_eq!(
            error.primary_span().unwrap().start,
            prefix.len() + body.find(marker).unwrap(),
            "{source}"
        );
        assert!(
            diagnostics
                .iter()
                .all(|e| !e.helps.iter().any(|h| h.contains("rename"))),
            "{diagnostics:?}"
        );
    }
}

#[test]
fn match_arm_valid_and_context_boundaries() {
    for body in [
        "{ break; }",
        "{ continue; }",
        "{ n += v; }",
        "{ n = v; }",
        "return",
        "return v",
        "n == v",
        "n >= v",
        "n >> v",
        "helper()",
        "if true { 1 } else { 2 }",
        "{ let y = 1; y; }",
        "{ match false { _ => {} } n; }",
    ] {
        let source = format!("fn main() {{ while true {{ match true {{ _ => {body} }} }} }}");
        assert!(
            errors(&source).is_empty(),
            "{source}: {:?}",
            errors(&source)
        );
    }
    for source in [
        "fn main() { 1 }",
        "fn main() { match true { _ => { let y = 1 } } }",
        "fn main() { match true { _ => { while true { 1 } } } }",
        "fn main() { match true { _ => {} } 1 }",
        "fn main() { match true { _ => { match false { _ => {} } } } 1 }",
    ] {
        let diagnostics = errors(source);
        assert!(
            diagnostics
                .iter()
                .any(|e| e.message == "expected `;` after statement"),
            "{source}: {diagnostics:?}"
        );
        assert!(
            diagnostics
                .iter()
                .all(|e| !e.message.contains("block arms")),
            "{diagnostics:?}"
        );
    }
    let source = "fn main() { match true { _ => { match false { _ => { 2 } } 3 } } }";
    assert_eq!(
        errors(source)
            .iter()
            .filter(|e| e.message == "block arms cannot yield a value")
            .count(),
        2
    );
}

#[test]
fn match_arm_diagnostic_reads_scale_linearly() {
    for body in [
        "break",
        "n += v",
        "{ let y = 1; y }",
        "{ match false { _ => { 2 } } 3 }",
    ] {
        let mut samples = Vec::new();
        for n in [8, 16, 32, 64] {
            let source = format!(
                "fn main() {{ {} }}",
                format!("match true {{ _ => {body} }} ").repeat(n)
            );
            PARSER_TOKEN_READS.with(|c| c.set(0));
            let diagnostics = errors(&source);
            assert!(!diagnostics.is_empty());
            samples.push((n, PARSER_TOKEN_READS.with(|c| c.get())));
        }
        let slope = (samples[1].1 - samples[0].1) / 8;
        for pair in samples.windows(2) {
            assert_eq!(pair[1].1 - pair[0].1, slope * (pair[1].0 - pair[0].0));
        }
        eprintln!("{body}: {samples:?}");
    }
}

#[test]
fn match_arm_nested_depth_scales_and_restores_after_error() {
    let mut samples = Vec::new();
    for depth in [8, 16, 32, 64] {
        let source = format!(
            "fn main() {{ {} {} }}",
            "match true { _ => { ".repeat(depth),
            "1 } } ".repeat(depth),
        );
        PARSER_TOKEN_READS.with(|c| c.set(0));
        let diagnostics = errors(&source);
        assert_eq!(diagnostics.len(), depth);
        assert!(
            diagnostics
                .iter()
                .all(|e| e.message == "block arms cannot yield a value")
        );
        samples.push((depth, PARSER_TOKEN_READS.with(|c| c.get())));
    }
    let slope = (samples[1].1 - samples[0].1) / 8;
    for pair in samples.windows(2) {
        assert_eq!(pair[1].1 - pair[0].1, slope * (pair[1].0 - pair[0].0));
    }
    eprintln!("nested depth: {samples:?}");

    // An early parse_block error must restore the caller's context.
    let tokens = Lexer::new("match true { _ => { fn next() {} }")
        .tokenize()
        .unwrap();
    let mut parser = Parser::new(tokens);
    parser.match_arm_block_depth = Some(42);
    assert!(parser.parse_match_expr().is_err());
    assert_eq!(parser.match_arm_block_depth, Some(42));
}
