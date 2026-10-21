use super::super::support::*;

#[test]
fn lambda_result_let_ok() {
    let (out, ok) = compile_and_run(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn main() { let f: closure(i64) -> Result<i64, String> = |a: i64| Ok(a); println(f(3).is_ok()); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn lambda_result_let_err() {
    let (out, ok) = compile_and_run(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn main() { let f: closure(i64) -> Result<i64, String> = |a: i64| Err("bad"); println(f(3).is_ok()); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn lambda_result_let_try_expr() {
    let (out, ok) = compile_and_run(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn main() { let f: closure(i64) -> Result<i64, String> = |a: i64| Ok(g(a)?); println(f(3).is_ok()); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn lambda_result_let_try_block() {
    let (out, ok) = compile_and_run(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn main() { let f: closure(i64) -> Result<i64, String> = |a: i64| { let v = g(a)?; return Ok(v); }; println(f(3).is_ok()); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn lambda_result_parameter_ok() {
    let (out, ok) = compile_and_run(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn apply(f: closure(i64) -> Result<i64, String>) { println(f(3).is_ok()); }
fn main() { apply(|a| Ok(a)); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn lambda_result_parameter_err() {
    let (out, ok) = compile_and_run(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn apply(f: closure(i64) -> Result<i64, String>) { println(f(3).is_ok()); }
fn main() { apply(|a| Err("bad")); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn lambda_result_parameter_try_expr() {
    let (out, ok) = compile_and_run(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn apply(f: closure(i64) -> Result<i64, String>) { println(f(3).is_ok()); }
fn main() { apply(|a| Ok(g(a)?)); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn lambda_result_parameter_try_block() {
    let (out, ok) = compile_and_run(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn apply(f: closure(i64) -> Result<i64, String>) { println(f(3).is_ok()); }
fn main() { apply(|a| { let v = g(a)?; return Ok(v); }); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn lambda_result_fn_ok() {
    let (out, ok) = compile_and_run(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn main() { let f: fn(i64) -> Result<i64, String> = |a: i64| Ok(a); println(f(3).is_ok()); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn lambda_result_fn_err() {
    let (out, ok) = compile_and_run(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn main() { let f: fn(i64) -> Result<i64, String> = |a: i64| Err("bad"); println(f(3).is_ok()); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn lambda_result_fn_try_expr() {
    let (out, ok) = compile_and_run(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn main() { let f: fn(i64) -> Result<i64, String> = |a: i64| Ok(g(a)?); println(f(3).is_ok()); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn lambda_result_fn_try_block() {
    let (out, ok) = compile_and_run(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn main() { let f: fn(i64) -> Result<i64, String> = |a: i64| { let v = g(a)?; return Ok(v); }; println(f(3).is_ok()); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn lambda_result_explicit_annotation() {
    let (out, ok) = compile_and_run(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn main() { let f = |a: i64| -> Result<i64, String> Ok(g(a)?); println(f(2).is_ok()); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn lambda_result_propagated_error() {
    let (out, ok) = compile_and_run(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn main() { let f: closure(i64) -> Result<i64, String> = |a| Ok(g(a)?); println(f(-1).is_err()); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn lambda_result_qualified() {
    let (out, ok) = compile_and_run(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn main() { let f: closure(i64) -> Result<i64, String> = |a| Result::Ok(g(a)?); let e: closure(i64) -> Result<i64, String> = |a| Result::Err("bad"); println(f(1).is_ok()); println(e(1).is_err()); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\ntrue\n");
}

#[test]
fn lambda_result_nested_inferred_outer() {
    let (out, ok) = compile_and_run(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn main() { let outer = |a: i64| { let inner: closure(i64) -> Result<i64, String> = |b| { return Ok(g(b)?); }; println(inner(a).is_ok()); return 7; }; println(outer(2)); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n7\n");
}

#[test]
fn lambda_result_nested_restore() {
    let (out, ok) = compile_and_run(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn main() { let outer: closure(i64) -> Result<i64, String> = |a| { let inner: closure(i64) -> Result<i64, i64> = |b| Err(b); println(inner(a).is_err()); return Ok(g(a)?); }; println(outer(2).is_ok()); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\ntrue\n");
}

#[test]
fn lambda_result_capture() {
    let (out, ok) = compile_and_run(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn main() { let offset = 4; let f: closure(i64) -> Result<i64, String> = |a| Ok(g(a)? + offset); println(f(2).unwrap()); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[test]
fn lambda_result_option_boundary() {
    let (out, ok) = compile_and_run(
        r#"fn g() -> Option<i64> { return Some(4); } fn main() { let f: closure() -> Option<i64> = || Some(g()?); println(f().unwrap()); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

#[test]
fn lambda_result_wrong_error() {
    assert_compile_error_contains(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn main() { let f: closure(i64) -> Result<i64, i64> = |a| Ok(g(a)?); }"#,
        &["E1805"],
    );
}

#[test]
fn lambda_result_wrong_payload() {
    assert_compile_error_contains(
        r#"fn main() { let f: closure(i64) -> Result<i64, String> = |a| Ok("wrong"); }"#,
        &["mismatched types"],
    );
}

#[test]
fn lambda_result_untyped_boundary() {
    assert_compile_error_contains(
        r#"fn g(a: i64) -> Result<i64, String> { if a < 0 { return Err("bad"); } return Ok(a); }
fn enclosing() -> Result<i64, String> { let f = |a: i64| g(a)?; return Ok(1); } fn main() {}"#,
        &["E1807"],
    );
}

#[test]
fn lambda_result_nested_wrong_payload() {
    assert_compile_error_contains(
        r#"fn main() { let outer = |a: i64| { let inner: closure() -> Result<i64, String> = || { return Ok("wrong"); }; return a; }; }"#,
        &["mismatched types"],
    );
}
#[test]
fn lambda_result_array_parameter_custom_payloads() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
enum Value { Number(i64) }
enum LispError { Invalid(String) }
fn err(message: String) -> LispError { return LispError::Invalid(message); }
fn apply(f: closure(Array<i64>) -> Result<Value, LispError>) {
    println(f([1, 2]).is_err());
}
fn main() { apply(|a: Array<i64>| Err(err("bad"))); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}
