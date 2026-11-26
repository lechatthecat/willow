//! E1205 for an enum pattern that cannot match its scrutinee: the message
//! never contrasts a type with an identically printed one, and an enum that
//! does not resolve is not reported again as a missing variant.
use crate::diagnostics::{Diagnostic, ErrorCode, Severity};
use crate::parser::ast::Type;

fn checker(source: &str) -> crate::semantic::TypeChecker {
    let tokens = crate::lexer::Lexer::new(source).tokenize().expect("lex");
    let (program, parse_errors) = crate::parser::Parser::new(tokens).parse();
    assert!(parse_errors.is_empty(), "{parse_errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    checker
}

fn errors(source: &str) -> Vec<Diagnostic> {
    checker(source)
        .errors
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .collect()
}

const DECLS: &str = "enum Shape { Circle(i64), Dot }\nenum Color { Red, Tint(i64) }\n";

#[test]
fn identically_printed_names_get_a_distinguishing_note() {
    let checker = checker(&format!("{DECLS}fn main() {{}}"));
    // The scrutinee's internal identity prints like the unresolved pattern.
    let (pattern, found, note) = checker
        .enum_pattern_mismatch_names("text::Edit", &Type::Named("$local::text::Edit".into()));
    assert_eq!(pattern, "text::Edit");
    assert!(found.ends_with("text::Edit"), "{found}");
    assert!(
        note.contains("different types with the same name"),
        "{note}"
    );
    assert!(
        note.contains("`text::Edit`, which does not resolve to an enum in this file"),
        "{note}"
    );
}

#[test]
fn distinct_names_get_no_note() {
    let checker = checker(&format!("{DECLS}fn main() {{}}"));
    let (pattern, found, note) =
        checker.enum_pattern_mismatch_names("Color", &Type::Named("Shape".into()));
    assert_eq!((pattern.as_str(), found.as_str()), ("Color", "Shape"));
    assert!(note.is_empty(), "{note}");
}

#[test]
fn ordinary_mismatches_keep_one_plain_error() {
    for (body, message) in [
        (
            "fn f(s: Shape) -> i64 { return match s { Color::Red => 1, _ => 0 }; }",
            "enum pattern `Color::Red` cannot match scrutinee of type `Shape`",
        ),
        (
            "fn f(s: Shape) -> i64 { return match s { Color::Tint(n) => n, _ => 0 }; }",
            "enum pattern `Color::Tint(..)` cannot match scrutinee of type `Shape`",
        ),
    ] {
        let found = errors(&format!("{DECLS}{body}\nfn main() {{}}"));
        assert_eq!(found.len(), 1, "{body}: {found:?}");
        assert_eq!(found[0].code, ErrorCode::E1205, "{body}: {found:?}");
        assert!(found[0].message.contains(message), "{body}: {found:?}");
        assert!(!found[0].message.contains("same name"), "{body}: {found:?}");
    }
}

#[test]
fn unresolved_pattern_enum_is_not_reported_as_a_missing_variant() {
    for body in [
        "fn f(s: Shape) -> i64 { return match s { Nope::Circle(n) => n, _ => 0 }; }",
        "fn f(s: Shape) -> i64 { return match s { Nope::Dot => 1, _ => 0 }; }",
    ] {
        let found = errors(&format!("{DECLS}{body}\nfn main() {{}}"));
        assert!(
            found.iter().all(|d| d.code != ErrorCode::E1208),
            "{body}: {found:?}"
        );
        assert_eq!(
            found.iter().filter(|d| d.code == ErrorCode::E1205).count(),
            1,
            "{body}: {found:?}"
        );
    }
}

#[test]
fn resolved_pattern_enum_still_reports_its_missing_variant() {
    let body = "fn f(s: Shape) -> i64 { return match s { Color::Blue => 1, _ => 0 }; }";
    let found = errors(&format!("{DECLS}{body}\nfn main() {{}}"));
    assert!(
        found.iter().any(|d| d.code == ErrorCode::E1208),
        "{found:?}"
    );
}
