use super::check_source;

#[test]
fn builtin_method_diagnostic_receivers_and_names() {
    // Six receiver families and four bad names: 24 explicit diagnostic cases.
    for (receiver, ty, available) in [
        ("s", "String", "substring"),
        ("n", "i64", "wrapping_add"),
        ("f", "f64", "toString"),
        ("b", "bool", "toString"),
        ("a", "Array<i64>", "push"),
        ("m", "Map<String, i64>", "insert"),
    ] {
        for method in ["missing", "toStrng", "toF64", "charAt"] {
            let source = format!(
                "import std::collections::{{Array, Map}}; fn test(s: String, n: i64, f: f64, b: bool, a: Array<i64>, m: Map<String, i64>) {{ {receiver}.{method}(); }}"
            );
            let diagnostics = check_source(&source);
            assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
            let d = &diagnostics[0];
            assert_eq!(d.message, format!("no method `{method}` on type `{ty}`"));
            assert!(d.helps.iter().any(|h| h.contains(available)), "{d:?}");
            assert_eq!(
                d.labels.len(),
                1,
                "stored receivers have no call-origin label"
            );
        }
    }
}

#[test]
fn builtin_method_diagnostic_call_origins() {
    for (prefix, receiver, ty) in [
        ("", "\"abc\".find(\"b\")", "i64"),
        ("", "\"abc\".trim()", "String"),
        ("", "\"abc\".contains(\"b\")", "bool"),
        ("fn value() -> f64 { return 1.0; }", "value()", "f64"),
        (
            "fn values() -> Array<i64> { return [1]; }",
            "values()",
            "Array<i64>",
        ),
        ("", "Map<String, i64>::new()", "Map<String, i64>"),
    ] {
        let source = format!(
            "import std::collections::{{Array, Map}}; {prefix} fn main() {{ {receiver}.missing(); }}"
        );
        let diagnostics = check_source(&source);
        assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
        let label = diagnostics[0]
            .labels
            .iter()
            .find(|l| l.message.contains("this call returns"))
            .expect("call origin");
        assert_eq!(
            label.message,
            format!("this call returns `{ty}`, the receiver type")
        );
        let start = source.rfind(receiver).unwrap();
        assert!(label.span.start >= start);
        assert!(label.span.end <= start + receiver.len());
        assert!(receiver.contains(&source[label.span.start..label.span.end]));
        assert!(label.span.start < label.span.end);
    }
}

#[test]
fn builtin_method_diagnostic_output_scales_linearly() {
    for count in [1, 8, 64, 256] {
        let source = format!(
            "fn main() {{ {} }}",
            "\"abc\".find(\"b\").unwrap_or(0);".repeat(count)
        );
        let diagnostics = check_source(&source);
        assert_eq!(diagnostics.len(), count);
        assert_eq!(
            diagnostics.iter().map(|d| d.labels.len()).sum::<usize>(),
            2 * count
        );
        assert_eq!(
            diagnostics.iter().map(|d| d.helps.len()).sum::<usize>(),
            2 * count
        );
    }
}
