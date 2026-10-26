use super::{STRING_FIND_SENTINEL_HELP, check_source};

#[test]
fn string_find_option_method_guidance() {
    // Eight Option operations, each on a direct call and a stored result.
    for method in [
        "unwrap()",
        "expect(\"missing\")",
        "unwrap_or(0)",
        "is_some()",
        "is_none()",
        "map(1)",
        "and_then(1)",
        "or_else(1)",
    ] {
        for receiver in ["\"abc\".find(\"b\")", "index"] {
            let source =
                format!("fn main() {{ let index = \"abc\".find(\"b\"); {receiver}.{method}; }}");
            let errors = check_source(&source);
            assert!(
                errors
                    .iter()
                    .any(|d| d.helps.iter().any(|h| h == STRING_FIND_SENTINEL_HELP)),
                "{source}: {errors:?}"
            );
        }
    }
}

#[test]
fn string_find_option_pattern_guidance() {
    for receiver in ["\"abc\".find(\"b\")", "index"] {
        for body in [
            format!("match {receiver} {{ Some(x) => {{}}, None => {{}} }}"),
            format!("if let Some(x) = {receiver} {{ }}"),
            format!("while let Option::Some(x) = {receiver} {{ break; }}"),
            format!("match {receiver} {{ Option::None => {{}}, _ => {{}} }}"),
            format!("{receiver}?;"),
        ] {
            let source = format!(
                "enum Option<T> {{ Some(T), None }} fn main() {{ let index = \"abc\".find(\"b\"); {body} }}"
            );
            let errors = check_source(&source);
            assert!(
                errors
                    .iter()
                    .any(|d| d.helps.iter().any(|h| h == STRING_FIND_SENTINEL_HELP)),
                "{source}: {errors:?}"
            );
        }
    }
}

#[test]
fn string_find_guidance_preserves_unrelated_diagnostics() {
    for expr in ["1.missing()", "true.unwrap()", "\"abc\".unwrap_or(0)"] {
        let errors = check_source(&format!("fn main() {{ {expr}; }}"));
        assert!(!errors.is_empty());
        assert!(
            errors
                .iter()
                .all(|d| !d.helps.iter().any(|h| h.contains("String.find")))
        );
    }
    let errors = check_source("fn main() { let index = 42; index.unwrap(); }");
    assert!(errors.iter().any(|d| {
        d.helps
            .iter()
            .any(|h| h.starts_with("if this i64 came from"))
    }));
}

#[test]
fn string_find_guidance_scales_with_invalid_uses() {
    for count in [1, 8, 64] {
        let body = "index.unwrap_or(0);".repeat(count);
        let errors = check_source(&format!(
            "fn main() {{ let index = \"abc\".find(\"b\"); {body} }}"
        ));
        assert_eq!(errors.len(), count);
        assert!(
            errors
                .iter()
                .all(|d| d.helps.len() == 2 && d.helps[1] == STRING_FIND_SENTINEL_HELP)
        );
    }
}
