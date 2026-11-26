//! Willow has no or-patterns. `A | B` in a pattern is reported once, says
//! what is unsupported, and suggests separate arms or a guard.
use super::*;
use crate::lexer::Lexer;

fn errors(source: &str) -> Vec<Diagnostic> {
    Parser::new(Lexer::new(source).tokenize().unwrap())
        .parse()
        .1
}

const PRELUDE: &str = "enum C { R, G, B }\n";

fn one_error(body: &str) -> (String, Diagnostic) {
    let source =
        format!("{PRELUDE}fn f(x: i64, c: C, o: Option<i64>, ch: char) -> i64 {{ {body} }}");
    let mut found = errors(&source);
    assert_eq!(found.len(), 1, "{body}: {found:?}");
    (source, found.remove(0))
}

#[test]
fn or_patterns_are_reported_with_a_rewrite() {
    for (name, body, written, help) in [
        (
            "integer arm",
            "return match x { 1 | 2 => 10, _ => 0 };",
            "1 | 2",
            "or bind the value and test it in a guard: `v if v == 1 || v == 2 =>`",
        ),
        (
            "three alternatives",
            "return match x { 1 | 2 | 30 => 10, _ => 0 };",
            "1 | 2 | 30",
            "`v if v == 1 || v == 2 || v == 30 =>`",
        ),
        (
            "negative literal",
            "return match x { -1 | 1 => 10, _ => 0 };",
            "-1 | 1",
            "`v if v == -1 || v == 1 =>`",
        ),
        (
            "char literals",
            "return match ch { 'a' | 'b' => 1, _ => 0 };",
            "'a' | 'b'",
            "`v if v == 'a' || v == 'b' =>`",
        ),
        (
            "enum variants",
            "return match c { C::R | C::G => 1, _ => 0 };",
            "C::R | C::G",
            "sharing the body through a helper function",
        ),
        (
            "literal and wildcard",
            "return match x { 1 | _ => 1 };",
            "1 | _",
            "write a separate arm for each alternative, sharing",
        ),
        (
            "arm with guard",
            "return match x { 1 | 2 if x > 0 => 1, _ => 0 };",
            "1 | 2",
            "write a separate arm for each alternative",
        ),
        (
            "nested payload",
            "return match o { Some(1 | 2) => 3, _ => 0 };",
            "1 | 2",
            "match each alternative separately",
        ),
        (
            "if let",
            "if let Some(1) | None = o { return 1; } return 0;",
            "Some(1) | None",
            "match each alternative separately",
        ),
        (
            "block arm",
            "match x { 1 | 2 => { println(1); } _ => {} } return 0;",
            "1 | 2",
            "`v if v == 1 || v == 2 =>`",
        ),
    ] {
        let (source, error) = one_error(body);
        assert_eq!(error.code, ErrorCode::E0102, "{name}");
        assert_eq!(
            error.message,
            format!("or-patterns are not supported: `{written}`"),
            "{name}"
        );
        let helps = error.helps.join("\n");
        assert!(helps.contains(help), "{name}: {helps}");
        let span = error.labels[0].span;
        assert_eq!(&source[span.start..span.end], written, "{name}");
    }
}

#[test]
fn or_pattern_recovery_keeps_reporting_later_mistakes() {
    let found = errors(&format!(
        "{PRELUDE}fn f(x: i64) -> i64 {{ return match x {{ 1 | 2 => 10, _ => 0 }}; }}
fn g(c: C) -> i64 {{ return match c {{ C::R | C::B => 1, _ => 0 }}; }}"
    ));
    let messages: Vec<_> = found.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(
        messages,
        [
            "or-patterns are not supported: `1 | 2`",
            "or-patterns are not supported: `C::R | C::B`",
        ],
        "{found:?}"
    );
}

#[test]
fn pipes_outside_patterns_are_unaffected() {
    for body in [
        "return match x { v if v == 1 || v == 2 => 10, _ => 0 };",
        "return match x { 1 => x | 2, _ => 0 };",
        "let f = |a: i64| a + 1; return match x { 1 => f(x), _ => 0 };",
        "return match o { Some(v) => v, None => 0 };",
    ] {
        let found = errors(&format!(
            "{PRELUDE}fn f(x: i64, o: Option<i64>) -> i64 {{ {body} }}"
        ));
        assert!(found.is_empty(), "{body}: {found:?}");
    }
}

#[test]
fn many_alternatives_report_one_error() {
    for size in [2usize, 64, 1024] {
        let alternatives = (0..size).map(|i| i.to_string()).collect::<Vec<_>>();
        let source = format!(
            "fn f(x: i64) -> i64 {{ return match x {{ {} => 1, _ => 0 }}; }}",
            alternatives.join(" | ")
        );
        let found = errors(&source);
        assert_eq!(found.len(), 1, "{size}: {found:?}");
    }
}

#[test]
fn nested_alternatives_stay_within_the_pattern_depth_limit() {
    use super::pattern::MAX_PATTERN_DEPTH;
    for depth in [4usize, MAX_PATTERN_DEPTH + 10, 10_000] {
        let pattern = format!("{}x{}", "Some(1 | ".repeat(depth), ")".repeat(depth));
        let source = format!("fn f() {{ match value {{ {pattern} => 1, _ => 0 }}; }}");
        let found = errors(&source);
        if depth < MAX_PATTERN_DEPTH {
            assert_eq!(found.len(), depth, "{depth}: {found:?}");
        } else {
            assert!(
                found.iter().any(|d| d.message.contains("levels deep")),
                "{depth}: {found:?}"
            );
        }
    }
}
