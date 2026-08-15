use super::support::*;

// Contextual Some/None shorthand and Option-only absence diagnostics —
// willow-glaj.5. The shorthand is deliberately not a first-class constructor.

fn run(source: &str) -> String {
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    out
}

#[test]
fn option_short_01_bare_constructors_match_qualified_constructors() {
    assert_eq!(
        run(
            r#"fn main() { let a: Option<i64> = Some(4); let b: Option<i64> = None; println(a.unwrap()); println(b.is_none()); }"#
        ),
        "4\ntrue\n"
    );
}

#[test]
fn option_short_02_qualified_constructors_remain_canonical() {
    assert_eq!(
        run(
            r#"fn main() { let a: Option<i64> = Option::Some(5); let b: Option<i64> = Option::None; println(a.unwrap()); println(b.is_none()); }"#
        ),
        "5\ntrue\n"
    );
}

#[test]
fn option_short_03_bare_patterns_cover_both_variants() {
    assert_eq!(
        run(
            r#"fn show(v: Option<i64>) { match v { Some(x) => println(x), None => println(-1) } } fn main() { show(Some(6)); show(None); }"#
        ),
        "6\n-1\n"
    );
}

#[test]
fn option_short_04_qualified_patterns_cover_both_variants() {
    assert_eq!(
        run(
            r#"fn show(v: Option<i64>) { match v { Option::Some(x) => println(x), Option::None => println(-1) } } fn main() { show(Option::Some(7)); show(Option::None); }"#
        ),
        "7\n-1\n"
    );
}

#[test]
fn option_short_05_user_function_named_some_shadows_shorthand() {
    assert_eq!(
        run(
            r#"fn Some(v: i64) -> Option<i64> { return Option::Some(v + 10); } fn main() { let v: Option<i64> = Some(2); println(v.unwrap()); }"#
        ),
        "12\n"
    );
}

#[test]
fn option_short_06_user_local_named_none_shadows_shorthand() {
    assert_eq!(
        run(
            r#"fn main() { let None: Option<i64> = Some(8); let v: Option<i64> = None; println(v.unwrap()); }"#
        ),
        "8\n"
    );
}

#[test]
fn option_short_07_type_declaration_named_none_shadows_shorthand() {
    assert_compile_error_contains(
        r#"class None { pub value: i64; } fn main() { let v: Option<i64> = None; }"#,
        &["error[E0350]", "cannot find variable `None`"],
    );
}

#[test]
fn option_short_08_bare_some_is_not_a_first_class_constructor() {
    assert_compile_error_contains(
        "fn main() { let constructor = Some; }",
        &["error[E0350]", "cannot find variable `Some`"],
    );
}

#[test]
fn option_short_09_option_equality_is_rejected() {
    assert_compile_error_contains(
        "fn main() { let a: Option<i64> = Some(1); let b: Option<i64> = Some(1); println(a == b); }",
        &[
            "error[E0201]",
            "`Option<T>` does not support `==`",
            "use `is_none()`, `is_some()`, or `match`",
        ],
    );
}

#[test]
fn option_short_10_option_inequality_is_rejected() {
    assert_compile_error_contains(
        "fn main() { let a: Option<i64> = Some(1); let b: Option<i64> = Some(2); println(a != b); }",
        &["`Option<T>` does not support `!=`", "use `is_none()`"],
    );
}

#[test]
fn option_short_11_option_equal_none_suggests_is_none() {
    assert_compile_error_contains(
        "fn main() { let value: Option<i64> = None; println(value == None); }",
        &["`Option<T>` does not support `==`", "use `value.is_none()`"],
    );
}

#[test]
fn option_short_12_none_equal_option_suggests_is_none() {
    assert_compile_error_contains(
        "fn main() { let value: Option<i64> = None; println(None == value); }",
        &["`Option<T>` does not support `==`", "use `value.is_none()`"],
    );
}

#[test]
fn option_short_13_option_not_equal_none_suggests_is_some() {
    assert_compile_error_contains(
        "fn main() { let value: Option<i64> = None; println(value != None); }",
        &["`Option<T>` does not support `!=`", "use `value.is_some()`"],
    );
}

#[test]
fn option_short_14_qualified_none_gets_specific_predicate_help() {
    assert_compile_error_contains(
        "fn main() { let value: Option<i64> = None; println(value == Option::None); }",
        &["`Option<T>` does not support `==`", "use `value.is_none()`"],
    );
}

#[test]
fn option_short_15_gc_reference_option_has_no_structural_equality() {
    assert_compile_error_contains(
        r#"fn main() { let a: Option<String> = Some("x"); let b: Option<String> = Some("x"); println(a == b); }"#,
        &["`Option<T>` does not support `==`"],
    );
}

#[test]
fn option_short_16_different_option_payloads_use_option_diagnostic() {
    assert_compile_error_contains(
        "fn main() { let a: Option<i64> = None; let b: Option<bool> = None; println(a == b); }",
        &["`Option<T>` does not support `==`"],
    );
}

#[test]
fn option_short_17_diagnostic_never_exposes_option_void_placeholder() {
    let stderr =
        compile_error_stderr("fn main() { let a: Option<i64> = None; println(a == None); }");
    assert!(
        stderr.contains("`Option<T>` does not support `==`"),
        "{stderr}"
    );
    assert!(!stderr.contains("Option<void>"), "{stderr}");
}

#[test]
fn option_short_18_field_access_suggests_explicit_handling() {
    assert_compile_error_contains(
        "class Node { pub value: i64; } fn main() { let n: Option<Node> = None; println(n.value); }",
        &[
            "cannot access field `value` on `Option<Node>` without handling absence",
            "use `match`, `.unwrap()`, or `.expect(...)`",
        ],
    );
}

#[test]
fn option_short_19_unknown_inner_method_suggests_explicit_handling() {
    assert_compile_error_contains(
        "class Node { pub fn value(self) -> i64 { return 1; } } fn main() { let n: Option<Node> = None; println(n.value()); }",
        &[
            "cannot call method `value` on `Option<Node>` without handling absence",
            "use `match`, `.unwrap()`, or `.expect(...)`",
        ],
    );
}

#[test]
fn option_short_20_absence_predicates_are_the_equality_replacement() {
    assert_eq!(
        run(
            "fn main() { let a: Option<i64> = None; let b: Option<i64> = Some(1); println(a.is_none()); println(b.is_some()); }"
        ),
        "true\ntrue\n"
    );
}

#[test]
fn option_short_21_match_is_the_value_comparison_replacement() {
    assert_eq!(
        run(
            r#"fn main() { let value: Option<String> = Some("ok"); match value { Some(v) => println(v), None => println("none") } }"#
        ),
        "ok\n"
    );
}

#[test]
fn option_short_22_question_sugar_gets_the_same_equality_diagnostic() {
    assert_compile_error_contains(
        "fn main() { let a: i64? = None; let b: i64? = None; println(a == b); }",
        &["`Option<T>` does not support `==`"],
    );
}

#[test]
fn option_short_23_bare_none_resolves_in_return_position() {
    assert_eq!(
        run(
            "fn missing() -> Option<i64> { return None; } fn main() { println(missing().is_none()); }"
        ),
        "true\n"
    );
}

#[test]
fn option_short_24_nested_bare_variants_keep_both_absence_levels() {
    assert_eq!(
        run(
            "fn main() { let value: Option<Option<i64>> = Some(None); println(value.is_some()); println(value.unwrap().is_none()); }"
        ),
        "true\ntrue\n"
    );
}
