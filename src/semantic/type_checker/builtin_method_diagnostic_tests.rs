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

#[test]
fn generic_parameter_method_diagnostics_declaration_shapes() {
    // Five receiver shapes by four names exercise twenty independent cases.
    for receiver in ["x", "xs.pop()", "identity(x)", "xs[0]", "(x)"] {
        for method in ["unwrap", "missing", "toString", "len"] {
            let source = format!(
                "import std::collections::Array; fn identity<T>(x: T) -> T {{ return x; }} fn bad<T>(x: T, xs: Array<T>) {{ {receiver}.{method}(); }}"
            );
            let diagnostics = check_source(&source);
            assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
            assert_eq!(
                diagnostics[0].message,
                format!("type parameter `T` has no method `{method}`")
            );
            if receiver == "xs.pop()" || receiver == "identity(x)" {
                assert!(
                    diagnostics[0]
                        .labels
                        .iter()
                        .any(|l| l.message == "this call returns `T`, the receiver type")
                );
            }
        }
    }
}

#[test]
fn generic_parameter_method_diagnostics_recover_and_scale() {
    for count in [1, 8, 64, 256] {
        let source = format!(
            "fn bad<T>(x: T) {{ {} }}",
            "x.missing().again();".repeat(count)
        );
        let diagnostics = check_source(&source);
        assert_eq!(diagnostics.len(), count, "{diagnostics:?}");
        assert!(
            diagnostics
                .iter()
                .all(|d| d.message == "type parameter `T` has no method `missing`")
        );
    }
    let diagnostics = check_source("fn bad<T>(x: T) { x.missing(unknown); }");
    assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
    assert!(diagnostics.iter().any(|d| d.message.contains("unknown")));
}

#[test]
fn generic_parameter_method_diagnostics_specialization_counts() {
    use super::TypeChecker;
    use crate::{lexer::Lexer, parser::Parser};
    for count in [1, 8, 64] {
        let mut source = String::from("fn bad<T>(x: T) { x.missing(); } ");
        for i in 0..count {
            source.push_str(&format!("class Value{i} {{}} "));
        }
        source.push_str("fn main() {");
        for i in 0..count {
            source.push_str(&format!("bad(new Value{i}());"));
        }
        source.push('}');
        let (program, errors) = Parser::new(Lexer::new(&source).tokenize().unwrap()).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let mut checker = TypeChecker::new();
        checker.check_program(&program);
        let mut checked = 0;
        while let Some(instance) = checker.generics.pending.pop_front() {
            checker.check_generic_instance(instance);
            checked += 1;
        }
        assert_eq!(checked, count);
        assert_eq!(checker.generics.rejected_parameter_methods.len(), 1);
        assert_eq!(checker.errors.len(), 1, "{:?}", checker.errors);
        assert_eq!(
            checker.errors[0].message,
            "type parameter `T` has no method `missing`"
        );
    }
}

#[test]
fn option_method_diagnostics_unknown_payloads_and_suggestions() {
    // Twenty-four unknown-name cases, including opaque and concrete generic types.
    for payload in ["i64", "String", "bool", "Node", "T", "Box<i64>"] {
        for (method, suggestion) in [
            ("ok_or", None),
            ("unwra", Some("unwrap")),
            ("is_som", Some("is_some")),
            ("and_thn", Some("and_then")),
        ] {
            let source = format!(
                "class Node {{}} class Box<T> {{}} fn bad<T>(x: Option<{payload}>) {{ x.{method}(); }}"
            );
            let diagnostics = check_source(&source);
            assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
            let d = &diagnostics[0];
            assert_eq!(
                d.message,
                format!("no method `{method}` on type `Option<{payload}>`")
            );
            assert!(!format!("{d:?}").contains("$type"));
            assert!(!format!("{d:?}").contains("handling absence"));
            if let Some(suggestion) = suggestion {
                assert!(
                    d.helps
                        .iter()
                        .any(|h| h.contains(&format!("`{suggestion}`"))),
                    "{d:?}"
                );
            }
        }
    }
}

#[test]
fn option_method_diagnostics_existing_payload_methods() {
    for (prefix, payload, method, args) in [
        ("", "String", "len", ""),
        ("", "String", "substring", "0, 1"),
        ("", "String", "substring", ""),
        ("", "i64", "checked_add", "1"),
        ("", "i64", "wrapping_neg", ""),
        ("", "f64", "toString", ""),
        ("", "bool", "toString", ""),
        (
            "class Node { pub fn get(self) -> i64 { return 1; } }",
            "Node",
            "get",
            "",
        ),
        (
            "open class Base { pub fn get(self) -> i64 { return 1; } } class Node extends Base {}",
            "Node",
            "get",
            "",
        ),
        (
            "interface Value { fn get(self) -> i64; }",
            "Value",
            "get",
            "",
        ),
        (
            "interface Value<T> { fn get(self) -> T; }",
            "Value<i64>",
            "get",
            "",
        ),
        ("", "Result<i64, String>", "unwrap_err", ""),
        (
            "class Box<T> { pub fn get(self) -> i64 { return 1; } }",
            "Box<i64>",
            "get",
            "",
        ),
        (
            "class Box<T> { pub fn get(self) -> i64 { return 1; } }",
            "Box<String>",
            "get",
            "",
        ),
    ] {
        let source = format!("{prefix} fn bad(x: Option<{payload}>) {{ x.{method}({args}); }}");
        let diagnostics = check_source(&source);
        assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:?}");
        assert_eq!(
            diagnostics[0].message,
            format!(
                "cannot call method `{method}` on `Option<{payload}>` without handling absence"
            )
        );
    }
}

#[test]
fn option_method_diagnostics_recovery_and_linear_counts() {
    use super::diagnostics::EDIT_CELLS;
    let mut unit_cells = 0;
    for count in [1, 8, 64, 256] {
        EDIT_CELLS.with(|cells| cells.set(0));
        let source = format!(
            "fn bad(x: Option<i64>) {{ {} }}",
            "x.ok_or().again();".repeat(count)
        );
        let diagnostics = check_source(&source);
        assert_eq!(diagnostics.len(), count, "{diagnostics:?}");
        let cells = EDIT_CELLS.with(|cells| cells.get());
        if count == 1 {
            unit_cells = cells;
        }
        assert!(unit_cells > 0);
        assert_eq!(cells, unit_cells * count);
        println!(
            "Option calls={count}: diagnostics={}, edit cells={cells}",
            diagnostics.len()
        );
        assert!(
            diagnostics
                .iter()
                .all(|d| d.message == "no method `ok_or` on type `Option<i64>`")
        );
    }
    let diagnostics = check_source("fn bad(x: Option<i64>) { x.ok_or(unknown); }");
    assert_eq!(diagnostics.len(), 2, "{diagnostics:?}");
    assert!(diagnostics.iter().any(|d| d.message.contains("unknown")));
}

#[test]
fn option_method_diagnostics_call_origin_and_nested_option() {
    let diagnostics = check_source(
        "fn make(x: Option<i64>) -> Option<i64> { return x; } fn bad(x: Option<i64>) { make(x).ok_or(); }",
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert!(
        diagnostics[0]
            .labels
            .iter()
            .any(|l| l.message == "this call returns `Option<i64>`, the receiver type")
    );
    let diagnostics = check_source("fn bad(x: Option<Option<String>>) { x.len(); }");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(
        diagnostics[0].message,
        "no method `len` on type `Option<Option<String>>`"
    );
}
