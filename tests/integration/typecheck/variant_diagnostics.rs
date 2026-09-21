use super::super::support::*;

// ── E180x type-inference and `?` diagnostics (willow-aff.3) ────────────────
// Acceptance criteria from requirements/requirements_option_result.md:
//   E1801 — cannot infer `T` for `Option::None`
//   E1803 — cannot infer `E` for `Result::Ok` / cannot infer `T` for `Result::Err`
//   E1805 — `?` error type mismatch
//   E1806 — `?` applied to a non-Result/non-Option value
//   E1807 — `?` in a function that does not return the matching wrapper
// (Non-exhaustive match for Option/Result is reported generically as E1202;
//  see the test_*_match_missing_* tests above.)

// Perspective 1: bare `Option::None` without annotation cannot infer `T`.
#[test]
fn test_e1801_bare_none_cannot_infer_t() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = Option::None;
    println(1);
}
"#,
        &[
            "error[E1801]",
            "cannot infer type parameter `T` for `Option::None`",
            "type annotation required",
        ],
    );
}

// Perspective 2: the inference error also fires for `let mut`.
#[test]
fn test_e1801_bare_none_let_mut_cannot_infer_t() {
    assert_compile_error_contains(
        r#"
fn main() {
    let mut x = Option::None;
    println(1);
}
"#,
        &["error[E1801]", "cannot infer type parameter `T`"],
    );
}

// Perspective 3: bare `Result::Ok(v)` cannot infer the error type `E`.
#[test]
fn test_e1803_bare_ok_cannot_infer_error_type() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = Result::Ok(10);
    println(1);
}
"#,
        &[
            "error[E1803]",
            "cannot infer error type `E` for `Result::Ok`",
        ],
    );
}

// Perspective 4: bare `Result::Err(e)` cannot infer the success type `T`.
#[test]
fn test_e1803_bare_err_cannot_infer_success_type() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = Result::Err("boom");
    println(1);
}
"#,
        &[
            "error[E1803]",
            "cannot infer success type `T` for `Result::Err`",
        ],
    );
}

// Perspective 5: annotation resolves `Option::None` — no diagnostic.
#[test]
fn test_e1801_annotation_resolves_none() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Option<i64> = Option::None;
    println(x.is_none());
}
"#,
    );
    assert!(ok, "annotated None must compile");
    assert_eq!(out, "true\n");
}

// Perspective 6: annotation resolves `Result::Ok` — no diagnostic, runs.
#[test]
fn test_e1803_annotation_resolves_ok() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(10);
    println(x.unwrap());
}
"#,
    );
    assert!(ok, "annotated Ok must compile");
    assert_eq!(out, "10\n");
}

// Perspective 7: annotation resolves `Result::Err` — no diagnostic.
#[test]
fn test_e1803_annotation_resolves_err() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Err("nope");
    println(x.is_err());
}
"#,
    );
    assert!(ok, "annotated Err must compile");
    assert_eq!(out, "true\n");
}

// Perspective 8: `Option::Some(v)` infers `T` from the payload — no diagnostic.
#[test]
fn test_e1801_some_infers_t_no_annotation() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x = Option::Some(7);
    println(x.unwrap());
}
"#,
    );
    assert!(ok, "Some(7) must infer T=i64");
    assert_eq!(out, "7\n");
}

// Perspective 9: a `Void` placeholder reaching a binding through a method
// chain is benign and must NOT trigger E1803 (guards against over-reporting).
#[test]
fn test_e1803_not_reported_through_method_chain() {
    let (out, ok) = compile_and_run(
        r#"
fn add_five(v: i64) -> Result<i64, String> {
    return Result::Ok(v + 5);
}

fn main() {
    let chained = Result::Ok(10).and_then(add_five);
    println(chained.unwrap());
}
"#,
    );
    assert!(ok, "method-chain result must not trigger E1803");
    assert_eq!(out, "15\n");
}

// Perspective 10: `Option::None` as a direct return is resolved by the return
// type — no diagnostic.
#[test]
fn test_e1801_none_as_return_is_resolved() {
    let (out, ok) = compile_and_run(
        r#"
fn empty() -> Option<i64> {
    return Option::None;
}

fn main() {
    println(empty().is_none());
}
"#,
    );
    assert!(ok, "None as return must compile");
    assert_eq!(out, "true\n");
}

// Perspective 11: `?` propagating a mismatched error type reports E1805.
#[test]
fn test_e1805_question_error_type_mismatch() {
    assert_compile_error_contains(
        r#"
fn source() -> Result<i64, String> {
    return Result::Ok(1);
}

fn consumer() -> Result<i64, i64> {
    let v = source()?;
    return Result::Ok(v);
}

fn main() {}
"#,
        &[
            "error[E1805]",
            "error type mismatch",
            "but `?` propagates `String`",
        ],
    );
}

// Perspective 12: `?` with matching error types compiles and runs end-to-end.
#[test]
fn test_e1805_matching_error_types_ok() {
    let (out, ok) = compile_and_run(
        r#"
fn source(n: i64) -> Result<i64, String> {
    if n < 0 { return Result::Err("neg"); }
    return Result::Ok(n);
}

fn consumer(n: i64) -> Result<i64, String> {
    let v = source(n)?;
    return Result::Ok(v * 2);
}

fn main() {
    let r = consumer(21);
    println(r.unwrap());
}
"#,
    );
    assert!(ok, "matching error types must compile");
    assert_eq!(out, "42\n");
}

// Perspective 13: `?` on a `bool` reports E1806.
#[test]
fn test_e1806_question_on_bool() {
    assert_compile_error_contains(
        r#"
fn f() -> Result<i64, String> {
    let b = true;
    let x = b?;
    return Result::Ok(1);
}

fn main() {}
"#,
        &[
            "error[E1806]",
            "requires `Result<T,E>` or `Option<T>`",
            "found `bool`",
        ],
    );
}

// Perspective 14: `?` on an `Option` inside a Result-returning function is
// rejected because no Option-to-Result conversion is defined.
#[test]
fn test_e1807_question_on_option_in_result_function() {
    assert_compile_error_contains(
        r#"
fn f() -> Result<i64, String> {
    let o: Option<i64> = Option::Some(1);
    let x = o?;
    return Result::Ok(x);
}

fn main() {}
"#,
        &[
            "error[E1807]",
            "`?` on `Option<T>` can only be used inside a function returning `Option<U>`",
            "found `Result<i64, String>`",
        ],
    );
}

// Perspective 15: `?` on a `String` reports E1806.
#[test]
fn test_e1806_question_on_string() {
    assert_compile_error_contains(
        r#"
fn f() -> Result<i64, String> {
    let s = "hello";
    let x = s?;
    return Result::Ok(1);
}

fn main() {}
"#,
        &[
            "error[E1806]",
            "requires `Result<T,E>` or `Option<T>`",
            "found `String`",
        ],
    );
}

// Perspective 16: `?` inside a `void` function reports E1807.
#[test]
fn test_e1807_question_in_void_function() {
    assert_compile_error_contains(
        r#"
fn source() -> Result<i64, String> {
    return Result::Ok(1);
}

fn main() {
    let v = source()?;
    println(v);
}
"#,
        &[
            "error[E1807]",
            "can only be used inside a function returning `Result",
            "found `void`",
        ],
    );
}

// Perspective 17: `?` inside an `Option`-returning function reports E1807.
#[test]
fn test_e1807_question_in_option_function() {
    assert_compile_error_contains(
        r#"
fn source() -> Result<i64, String> {
    return Result::Ok(1);
}

fn wrapped() -> Option<i64> {
    let v = source()?;
    return Option::Some(v);
}

fn main() {}
"#,
        &["error[E1807]", "found `Option<i64>`"],
    );
}

// Perspective 18: `?` inside an `i64`-returning function reports E1807.
#[test]
fn test_e1807_question_in_i64_function() {
    assert_compile_error_contains(
        r#"
fn source() -> Result<i64, String> {
    return Result::Ok(1);
}

fn doubled() -> i64 {
    let v = source()?;
    return v * 2;
}

fn main() {}
"#,
        &["error[E1807]", "found `i64`"],
    );
}

// Perspective 19: too many arguments to a variant constructor is source-aware
// (E0201 reports the expected and actual argument counts).
#[test]
fn test_variant_constructor_too_many_args_e0201() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x: Option<i64> = Option::Some(1, 2);
    println(1);
}
"#,
        &[
            "error[E0201]",
            "`Option::Some` expects 1 argument(s), got 2",
        ],
    );
}

// Perspective 20: a payload type mismatch in a variant constructor is
// source-aware (reports the concrete instantiations).
#[test]
fn test_variant_constructor_payload_type_mismatch_e0201() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x: Option<i64> = Option::Some(true);
    println(1);
}
"#,
        &[
            "error[E0201]",
            "expected `Option<i64>`",
            "found `Option<bool>`",
        ],
    );
}

// Perspective 21: a missing payload on a variant constructor is source-aware.
#[test]
fn test_variant_constructor_missing_payload_e0201() {
    assert_compile_error_contains(
        r#"
fn f() -> Result<i64, String> {
    return Result::Ok();
}

fn main() {}
"#,
        &["error[E0201]", "`Result::Ok` expects 1 argument(s), got 0"],
    );
}
