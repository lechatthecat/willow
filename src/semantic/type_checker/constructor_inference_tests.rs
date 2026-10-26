use super::*;

const ENUMS: &str = "enum Option<T> { Some(T), None } enum Result<T, E> { Ok(T), Err(E) }";

fn errors(body: &str) -> (String, Vec<Diagnostic>) {
    let source = format!("{ENUMS} {body}");
    let diagnostics = check_source(&source)
        .into_iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    (source, diagnostics)
}

#[test]
fn constructor_inference_missing_parameters() {
    // Qualified/shorthand forms and independent expression boundaries.
    for (constructor, code, missing) in [
        ("Ok(0)", ErrorCode::E1803, "error type `E`"),
        ("Err(0)", ErrorCode::E1803, "success type `T`"),
        ("Result::Ok(0)", ErrorCode::E1803, "error type `E`"),
        ("Result::Err(0)", ErrorCode::E1803, "success type `T`"),
        ("Option::None", ErrorCode::E1801, "type parameter `T`"),
        ("None", ErrorCode::E1801, "type parameter `T`"),
    ] {
        for context in [
            format!("let x = {constructor};"),
            format!("{constructor};"),
            format!("match {constructor} {{ _ => println(0) }}"),
        ] {
            let (source, ds) = errors(&format!("fn main() {{ {context} }}"));
            assert_eq!(ds.len(), 1, "{source}: {ds:?}");
            let d = &ds[0];
            assert_eq!(d.code, code, "{source}: {d:?}");
            assert!(d.message.contains(missing), "{d:?}");
            assert!(d.helps.join(" ").contains("annotation"), "{d:?}");
            let span = d.labels[0].span;
            assert!(
                constructor.starts_with(&source[span.start..span.end]),
                "{d:?}"
            );
            assert_eq!(span.start, source.rfind(constructor).unwrap());
        }
    }
}

#[test]
fn constructor_inference_no_void_string_cascade() {
    let (_, ds) = errors(
        r#"fn main() { match Ok(0) { Ok(v) => println(v), Err(e) => println("fail " + e), } }"#,
    );
    assert_eq!(ds.len(), 1, "{ds:?}");
    assert_eq!(ds[0].code, ErrorCode::E1803);
}

#[test]
fn constructor_inference_preserves_context_and_real_void() {
    for body in [
        "let x: Result<i64, String> = Ok(0);",
        "let x: Result<i64, String> = Err(\"e\");",
        "let x: Result<i64, String> = Result::Ok(0);",
        "let x: Result<i64, String> = Result::Err(\"e\");",
        "let x: Option<i64> = None;",
        "let x: Option<i64> = Option::None;",
        "let x = Option::Some(1);",
        "consume(Ok(0));",
        "let x: Result<i64, String> = true ? Ok(0) : Err(\"e\");",
        "let x: Result<i64, String> = match true { true => Ok(0), false => Err(\"e\") };",
    ] {
        let (source, ds) = errors(&format!(
            "fn nothing() {{}} fn consume(x: Result<i64, String>) {{}} fn main() {{ {body} }}"
        ));
        assert!(ds.is_empty(), "{source}: {ds:?}");
    }
    let (_, ds) = errors("fn Ok(x: i64) -> i64 { return x; } fn main() { let x = Ok(0); }");
    assert!(ds.is_empty(), "{ds:?}");
}

#[test]
fn constructor_inference_void_concat_help() {
    for expr in ["\"fail\" + nothing()", "nothing() + \"fail\""] {
        let (_, ds) = errors(&format!(
            "fn nothing() {{}} fn main() {{ let s = {expr}; }}"
        ));
        assert_eq!(ds.len(), 1, "{ds:?}");
        assert!(ds[0].message.contains("cannot concatenate"));
        assert!(!ds[0].helps.join(" ").contains("toString"));
    }
    let (_, ds) = errors("fn main() { let s = \"n\" + 1; }");
    assert!(ds[0].helps.join(" ").contains("toString"));
}

#[test]
fn constructor_inference_diagnostic_counts_scale() {
    for n in [1, 8, 64, 256] {
        let body = "match Ok(0) { Ok(v) => println(v), Err(e) => println(e), }".repeat(n);
        CONSTRUCTOR_INFERENCE_CHECKS.with(|checks| checks.set(0));
        let (_, ds) = errors(&format!("fn main() {{ {body} }}"));
        let checks = CONSTRUCTOR_INFERENCE_CHECKS.with(|checks| checks.get());
        assert_eq!(checks, n);
        assert_eq!(ds.len(), n);
        assert!(ds.iter().all(|d| d.code == ErrorCode::E1803));
    }
}

#[test]
fn constructor_inference_does_not_mislabel_real_void() {
    for body in [
        "let x = Option::Some(nothing());",
        "let x = Result<i64, void>::Ok(0);",
        "let x = Result<void, i64>::Err(0);",
        "let x = Option<void>::None();",
    ] {
        let (_, ds) = errors(&format!("fn nothing() {{}} fn main() {{ {body} }}"));
        assert!(
            ds.iter().all(|d| !d.message.contains("cannot infer")),
            "{ds:?}"
        );
    }
}

#[test]
fn constructor_inference_nested_and_invalid_payloads() {
    for body in [
        "let xs = [Ok(1)];",
        "let x = Option::Some(Ok(1));",
        "let x = true ? Ok(1) : Ok(2);",
        "let x = match true { true => Ok(1), false => Ok(2) };",
    ] {
        let (_, ds) = errors(&format!("fn main() {{ {body} }}"));
        assert!(!ds.is_empty());
        assert!(ds.iter().all(|d| d.code == ErrorCode::E1803), "{ds:?}");
    }
    for expr in ["Ok()", "Result::Err()", "Option::Some()", "Ok(missing)"] {
        let (_, ds) = errors(&format!("fn main() {{ let x = {expr}; }}"));
        assert_eq!(ds.len(), 1, "{ds:?}");
        assert!(
            ds.iter().all(|d| !d.message.contains("cannot infer")),
            "{ds:?}"
        );
    }
}
