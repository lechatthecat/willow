use super::check_source;

#[test]
fn fn_closure_diagnostic_twenty_call_perspectives() {
    for (ty, value) in [
        ("i64", "1"),
        ("f64", "1.0"),
        ("bool", "true"),
        ("String", "\"x\""),
        ("Array<i64>", "[1]"),
    ] {
        for (declaration, call) in [
            (
                format!("fn apply(f: closure({ty}) -> {ty}, x: {ty}) -> {ty} {{ return f(x); }}"),
                "apply(item, VALUE)",
            ),
            (
                "fn apply<T>(f: closure(T) -> T, x: T) -> T { return f(x); }".into(),
                "apply(item, VALUE)",
            ),
            (
                "fn apply<T>(f: closure(T) -> T, x: T) -> T { return f(x); }".into(),
                "apply<TYPE>(item, VALUE)",
            ),
            (
                "fn apply<T>(x: T, f: closure(T) -> T) -> T { return f(x); }".into(),
                "apply(VALUE, item)",
            ),
        ] {
            let call = call.replace("VALUE", value).replace("TYPE", ty);
            let import = if ty == "Array<i64>" {
                "import std::collections::Array;"
            } else {
                ""
            };
            let source = format!(
                "{import} fn item(x: {ty}) -> {ty} {{ return x; }} {declaration} fn main() {{ {call}; }}"
            );
            let errors = check_source(&source);
            assert_eq!(errors.len(), 1, "{source}: {errors:?}");
            let diagnostic = &errors[0];
            assert!(
                diagnostic.message.contains("expected `closure("),
                "{diagnostic:?}"
            );
            assert!(
                diagnostic
                    .message
                    .contains(&format!("found `fn({ty}) -> {ty}`")),
                "{diagnostic:?}"
            );
            let label = &diagnostic.labels[0];
            assert_eq!(&source[label.span.start..label.span.end], "item");
            let wrapper = format!("|arg0: {ty}| item(arg0)");
            assert!(
                diagnostic.helps.iter().any(|h| h.contains(&wrapper)),
                "{diagnostic:?}"
            );
            let fixed = source.replacen(&call, &call.replace("item", &wrapper), 1);
            assert!(
                check_source(&fixed).is_empty(),
                "{fixed}: {:?}",
                check_source(&fixed)
            );
        }
    }
}

#[test]
fn fn_closure_diagnostic_zero_binary_and_shadowing() {
    for (item, wrapper) in [
        ("fn item() -> bool { return true; }", "|| item()"),
        (
            "fn item(a: i64, b: i64) -> bool { return a < b; }",
            "|arg0: i64, arg1: i64| item(arg0, arg1)",
        ),
    ] {
        let signature = if wrapper.starts_with("||") {
            "closure() -> bool"
        } else {
            "closure(i64, i64) -> bool"
        };
        let source = format!("{item} fn apply(f: {signature}) {{}} fn main() {{ apply(item); }}");
        let errors = check_source(&source);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].helps[0].contains(wrapper), "{errors:?}");
        assert!(
            check_source(&source.replace("apply(item)", &format!("apply({wrapper})"))).is_empty()
        );
    }
    let errors = check_source(
        "fn arg0(x: i64) -> i64 { return x; } fn apply(f: closure(i64) -> i64) {} fn main() { apply(arg0); }",
    );
    assert!(errors[0].helps[0].contains("|_arg0: i64| arg0(_arg0)"));
}

#[test]
fn fn_closure_diagnostic_output_counts() {
    for count in [1, 8, 64, 256] {
        let source = format!(
            "fn item(x: i64) -> i64 {{ return x; }} fn apply<T>(f: closure(T) -> T, x: T) {{}} fn main() {{ {} }}",
            "apply(item, 1);".repeat(count)
        );
        let errors = check_source(&source);
        assert_eq!(errors.len(), count);
        assert_eq!(errors.iter().map(|d| d.helps.len()).sum::<usize>(), count);
        assert_eq!(errors.iter().map(|d| d.labels.len()).sum::<usize>(), count);
    }
}

#[test]
fn fn_closure_diagnostic_preserves_other_errors_and_values() {
    let errors = check_source("fn apply<T>(x: T, y: T) {} fn main() { apply(1, true); }");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("conflicting types"));
    assert!(errors[0].helps.is_empty());
    let errors = check_source("fn apply<T>(x: T) {} fn main() { apply(1, 2); }");
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0]
            .message
            .contains("expected 1 value arguments, got 2")
    );
    assert!(errors[0].helps.is_empty());
    let source = "fn item(x: i64) -> i64 { return x; } fn apply(f: closure(i64) -> i64) {} fn main() { let stored = item; apply(stored); }";
    let errors = check_source(source);
    assert_eq!(errors.len(), 1);
    assert!(errors[0].helps[0].contains("|arg0: i64| stored(arg0)"));
    assert!(
        check_source(&source.replace("apply(stored)", "apply(|arg0: i64| stored(arg0))"))
            .is_empty()
    );
    assert!(check_source("fn item(x: i64) -> i64 { return x; } fn apply(f: fn(i64) -> i64) {} fn main() { apply(item); }").is_empty());
}
