use super::super::support::*;

// Contextual enum payload typing — willow-glaj.1. These perspectives pin that
// expected instantiated payload types flow recursively into nested variant
// expressions instead of leaving `None` as `Option<Void>`.

#[test]
fn option_context_01_bare_some_none_in_return() {
    let (out, ok) = compile_and_run(
        "fn make() -> Option<Option<i64>> { return Some(None); } \
         fn main() { println(make().is_some()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn option_context_02_qualified_some_none_in_return() {
    let (out, ok) = compile_and_run(
        "fn make() -> Option<Option<i64>> { return Option::Some(Option::None); } \
         fn main() { println(make().is_some()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn option_context_03_bare_some_none_in_let_annotation() {
    let (out, ok) = compile_and_run(
        "fn main() { let value: Option<Option<i64>> = Some(None); \
         println(value.is_some()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn option_context_04_qualified_outer_bare_inner() {
    let (out, ok) = compile_and_run(
        "fn main() { let value: Option<Option<i64>> = Option::Some(None); \
         println(value.is_some()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn option_context_05_bare_outer_qualified_inner() {
    let (out, ok) = compile_and_run(
        "fn main() { let value: Option<Option<i64>> = Some(Option::None); \
         println(value.is_some()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn option_context_06_bare_some_none_in_call_argument() {
    let (out, ok) = compile_and_run(
        "fn take(value: Option<Option<i64>>) -> bool { return value.is_some(); } \
         fn main() { println(take(Some(None))); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn option_context_07_qualified_some_none_in_call_argument() {
    let (out, ok) = compile_and_run(
        "fn take(value: Option<Option<i64>>) -> bool { return value.is_some(); } \
         fn main() { println(take(Option::Some(Option::None))); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn option_context_08_nested_ternary_branches() {
    let (out, ok) = compile_and_run(
        "fn make(flag: bool) -> Option<Option<i64>> { \
             return flag ? Some(None) : None; \
         } \
         fn main() { println(make(true).is_some()); println(make(false).is_none()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\ntrue\n");
}

#[test]
fn option_context_09_three_nested_option_states_typecheck() {
    let (out, ok) = compile_and_run(
        "fn main() { \
             let a: Option<Option<Option<i64>>> = None; \
             let b: Option<Option<Option<i64>>> = Some(None); \
             let c: Option<Option<Option<i64>>> = Some(Some(None)); \
             println(a.is_none()); println(b.is_some()); println(c.is_some()); \
         }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\ntrue\ntrue\n");
}

#[test]
fn option_context_10_result_ok_payload_receives_option_context() {
    let (out, ok) = compile_and_run(
        "fn make() -> Result<Option<i64>, String> { return Ok(None); } \
         fn main() { match make() { Ok(v) => println(v.is_none()), Err(_) => println(false) } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn option_context_11_result_err_payload_receives_option_context() {
    let (out, ok) = compile_and_run(
        "fn make() -> Result<i64, Option<String>> { return Err(None); } \
         fn main() { match make() { Ok(_) => println(false), Err(v) => println(v.is_none()) } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn option_context_12_custom_generic_bare_nested_variant() {
    let (out, ok) = compile_and_run(
        "enum Maybe<T> { Just(T), Empty, } \
         fn main() { let value: Maybe<Maybe<i64>> = Just(Empty); \
         match value { Just(_) => println(1), Empty => println(0) } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}

#[test]
fn option_context_13_custom_generic_qualified_nested_variant() {
    let (out, ok) = compile_and_run(
        "enum Maybe<T> { Just(T), Empty, } \
         fn main() { let value: Maybe<Maybe<i64>> = Maybe::Just(Maybe::Empty); \
         match value { Maybe::Just(_) => println(1), Maybe::Empty => println(0) } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}

#[test]
fn option_context_14_array_element_propagates_expected_option() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array; \
         fn main() { let values: Array<Option<Option<i64>>> = [Some(None), None]; \
         println(values[0].is_some()); println(values[1].is_none()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\ntrue\n");
}

#[test]
fn option_context_15_assignment_propagates_expected_option() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut value: Option<Option<i64>> = None; \
         value = Some(None); println(value.is_some()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn option_context_16_wrong_nested_payload_type_is_rejected() {
    assert_compile_error_contains(
        "fn main() { let value: Option<Option<i64>> = Some(Some(true)); }",
        &["error[E0201]", "expected `i64`, found `bool`"],
    );
}

#[test]
fn option_context_17_nested_variant_wrong_arity_is_rejected() {
    assert_compile_error_contains(
        "fn main() { let value: Option<Option<i64>> = Some(None, None); }",
        &["error[E0201]", "takes 1 argument(s), got 2"],
    );
}

#[test]
fn option_context_18_bare_none_without_context_stays_unresolved() {
    assert_compile_error_contains(
        "fn main() { let value = None; }",
        &["error[E0350]", "cannot find variable `None`"],
    );
}

#[test]
fn option_context_19_qualified_none_without_context_still_needs_annotation() {
    assert_compile_error_contains(
        "fn main() { let value = Option::None; }",
        &["error[E1801]", "cannot infer type parameter `T`"],
    );
}

#[test]
fn option_context_20_different_inner_enum_is_rejected() {
    assert_compile_error_contains(
        "enum Other<T> { Value(T), Empty, } \
         fn main() { let value: Option<Option<i64>> = Some(Other::Empty); }",
        &[
            "error[E0201]",
            "expected `Option<i64>`, found `Other<void>`",
        ],
    );
}
