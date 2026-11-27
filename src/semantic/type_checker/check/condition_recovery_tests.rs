use super::*;

#[test]
fn invalid_conditions_do_not_add_type_cascades() {
    for expression in [
        "\"abc\".missing(\"a\")",
        "missing()",
        "new Empty().missing()",
    ] {
        for statement in [
            format!("if {expression} {{}}"),
            format!("while {expression} {{}}"),
            format!("let n = {expression} ? 1 : 2;"),
            format!("let n: i64 = {expression} ? 1 : 2;"),
        ] {
            let source = format!("class Empty {{}} fn main() {{ {statement} }}");
            let errors = check_source(&source);
            assert!(!errors.is_empty(), "{source}");
            assert!(
                !errors
                    .iter()
                    .any(|d| matches!(d.code, ErrorCode::E0203 | ErrorCode::E0901)),
                "{source}: {errors:?}"
            );
        }
    }
}

#[test]
fn validly_typed_non_bool_conditions_still_report_errors() {
    for expression in ["1", "\"text\"", "empty()"] {
        for statement in [
            format!("if {expression} {{}}"),
            format!("while {expression} {{}}"),
            format!("let n = {expression} ? 1 : 2;"),
            format!("let n: i64 = {expression} ? 1 : 2;"),
        ] {
            let source = format!("fn empty() {{}} fn main() {{ {statement} }}");
            let errors = check_source(&source);
            assert!(
                errors
                    .iter()
                    .any(|d| matches!(d.code, ErrorCode::E0203 | ErrorCode::E0901)),
                "{source}: {errors:?}"
            );
        }
    }
    for context in [
        "if (match true { _ => 1, true => 2 }) {}",
        "while (match true { _ => 1, true => 2 }) {}",
        "let n = (match true { _ => 1, true => 2 }) ? 1 : 2;",
        "let n: i64 = (match true { _ => 1, true => 2 }) ? 1 : 2;",
    ] {
        let errors = check_source(&format!("fn main() {{ {context} }}"));
        assert!(
            errors.iter().any(|d| d.code == ErrorCode::W1201),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|d| matches!(d.code, ErrorCode::E0203 | ErrorCode::E0901)),
            "{errors:?}"
        );
    }
    let errors = check_source("fn main() { missing(); if 1 {} }");
    assert!(
        errors.iter().any(|d| d.code == ErrorCode::E0203),
        "{errors:?}"
    );
}
