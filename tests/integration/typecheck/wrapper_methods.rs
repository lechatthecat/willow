use super::super::support::*;

// ── Option helper method tests ─────────────────────────────────────────────

#[test]
fn test_option_is_some_and_is_none() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let a = Option::Some(42);
    let b: Option<i64> = Option::None;
    println(a.is_some());
    println(a.is_none());
    println(b.is_some());
    println(b.is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\nfalse\ntrue\n");
}

#[test]
fn test_option_unwrap_some_returns_value() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x = Option::Some(99);
    println(x.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

#[test]
fn test_option_unwrap_none_panics() {
    let src = r#"
fn main() {
    let x: Option<i64> = Option::None;
    println(x.unwrap());
}
"#;
    let (out, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "unwrap on None should panic (non-zero exit)");
    assert!(
        out.contains("None") || out.is_empty(),
        "panic message should mention None"
    );
}

#[test]
fn test_option_expect_some_returns_value() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x = Option::Some(7);
    println(x.expect("should have value"));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_option_expect_none_panics_with_message() {
    let src = r#"
fn main() {
    let x: Option<i64> = Option::None;
    println(x.expect("custom message"));
}
"#;
    let (out, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "expect on None should panic");
    assert!(
        out.contains("custom message"),
        "panic should include custom message"
    );
}

#[test]
fn test_option_unwrap_or_some_returns_payload() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x = Option::Some(5);
    println(x.unwrap_or(0));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

#[test]
fn test_option_unwrap_or_none_returns_default() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Option<i64> = Option::None;
    println(x.unwrap_or(42));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_option_map_some_transforms_value() {
    let (out, ok) = compile_and_run(
        r#"
fn double(x: i64) -> i64 {
    return x * 2;
}
fn main() {
    let x = Option::Some(10);
    let y = x.map(double);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "20\n");
}

#[test]
fn test_option_map_none_stays_none() {
    let (out, ok) = compile_and_run(
        r#"
fn double(x: i64) -> i64 {
    return x * 2;
}
fn main() {
    let x: Option<i64> = Option::None;
    let y = x.map(double);
    println(y.is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

#[test]
fn test_option_map_with_lambda() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x = Option::Some(3);
    let y = x.map(|v: i64| v * v);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "9\n");
}

#[test]
fn test_option_and_then_some_calls_f() {
    let (out, ok) = compile_and_run(
        r#"
fn safe_double(x: i64) -> Option<i64> {
    if x > 100 {
        return Option::None;
    }
    return Option::Some(x * 2);
}
fn main() {
    let a = Option::Some(5).and_then(safe_double);
    let b = Option::Some(200).and_then(safe_double);
    println(a.unwrap());
    println(b.is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\ntrue\n");
}

#[test]
fn test_option_and_then_none_stays_none() {
    let (out, ok) = compile_and_run(
        r#"
fn safe_double(x: i64) -> Option<i64> {
    return Option::Some(x * 2);
}
fn main() {
    let x: Option<i64> = Option::None;
    let y = x.and_then(safe_double);
    println(y.is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

#[test]
fn test_option_or_else_some_returns_self() {
    let (out, ok) = compile_and_run(
        r#"
fn fallback() -> Option<i64> {
    return Option::Some(99);
}
fn main() {
    let x = Option::Some(1);
    let y = x.or_else(fallback);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n");
}

#[test]
fn test_option_or_else_none_calls_f() {
    let (out, ok) = compile_and_run(
        r#"
fn fallback() -> Option<i64> {
    return Option::Some(99);
}
fn main() {
    let x: Option<i64> = Option::None;
    let y = x.or_else(fallback);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// ── Result helper method tests ─────────────────────────────────────────────

#[test]
fn test_result_is_ok_and_is_err() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let a: Result<i64, String> = Result::Ok(1);
    let b: Result<i64, String> = Result::Err("oops");
    println(a.is_ok());
    println(a.is_err());
    println(b.is_ok());
    println(b.is_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\nfalse\ntrue\n");
}

#[test]
fn test_result_unwrap_ok_returns_value() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(55);
    println(x.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "55\n");
}

#[test]
fn test_result_unwrap_err_panics() {
    let src = r#"
fn main() {
    let x: Result<i64, String> = Result::Err("fail");
    println(x.unwrap());
}
"#;
    let (out, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "unwrap on Err should panic");
    assert!(out.contains("Err") || out.is_empty());
}

#[test]
fn test_result_expect_ok_returns_value() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(7);
    println(x.expect("should be ok"));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_result_expect_err_panics_with_message() {
    let src = r#"
fn main() {
    let x: Result<i64, String> = Result::Err("bad");
    println(x.expect("my error message"));
}
"#;
    let (out, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "expect on Err should panic");
    assert!(
        out.contains("my error message"),
        "panic should include custom message"
    );
}

#[test]
fn test_result_unwrap_or_ok_returns_payload() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(10);
    println(x.unwrap_or(0));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

#[test]
fn test_result_unwrap_or_err_returns_default() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Err("fail");
    println(x.unwrap_or(42));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_result_unwrap_err_extracts_error() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Err("my error");
    println(x.unwrap_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "my error\n");
}

#[test]
fn test_result_unwrap_err_on_ok_panics() {
    let src = r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(1);
    println(x.unwrap_err());
}
"#;
    let (_, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "unwrap_err on Ok should panic");
}

#[test]
fn test_result_map_ok_transforms_value() {
    let (out, ok) = compile_and_run(
        r#"
fn triple(x: i64) -> i64 {
    return x * 3;
}
fn main() {
    let x: Result<i64, String> = Result::Ok(4);
    let y = x.map(triple);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

#[test]
fn test_result_map_err_unchanged() {
    let (out, ok) = compile_and_run(
        r#"
fn triple(x: i64) -> i64 {
    return x * 3;
}
fn main() {
    let x: Result<i64, String> = Result::Err("oops");
    let y = x.map(triple);
    println(y.is_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

#[test]
fn test_result_map_with_lambda() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(5);
    let y = x.map(|v: i64| v + 10);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "15\n");
}

#[test]
fn test_result_map_err_transforms_error() {
    let (out, ok) = compile_and_run(
        r#"
fn add_prefix(s: String) -> String {
    return "error: " + s;
}
fn main() {
    let x: Result<i64, String> = Result::Err("bad input");
    let y = x.map_err(add_prefix);
    println(y.unwrap_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "error: bad input\n");
}

#[test]
fn test_result_map_err_ok_unchanged() {
    let (out, ok) = compile_and_run(
        r#"
fn add_prefix(s: String) -> String {
    return "error: " + s;
}
fn main() {
    let x: Result<i64, String> = Result::Ok(42);
    let y = x.map_err(add_prefix);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_result_and_then_ok_chains() {
    let (out, ok) = compile_and_run(
        r#"
fn parse_positive(n: i64) -> Result<i64, String> {
    if n > 0 {
        return Result::Ok(n);
    }
    return Result::Err("not positive");
}
fn main() {
    let a = Result::Ok(5).and_then(parse_positive);
    let b = Result::Ok(-3).and_then(parse_positive);
    println(a.unwrap());
    println(b.unwrap_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\nnot positive\n");
}

#[test]
fn test_result_and_then_err_stays_err() {
    let (out, ok) = compile_and_run(
        r#"
fn parse_positive(n: i64) -> Result<i64, String> {
    return Result::Ok(n * 2);
}
fn main() {
    let x: Result<i64, String> = Result::Err("initial error");
    let y = x.and_then(parse_positive);
    println(y.unwrap_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "initial error\n");
}

#[test]
fn test_result_or_else_ok_returns_self() {
    let (out, ok) = compile_and_run(
        r#"
fn handle_error(s: String) -> Result<i64, String> {
    return Result::Ok(0);
}
fn main() {
    let x: Result<i64, String> = Result::Ok(7);
    let y = x.or_else(handle_error);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_result_or_else_err_calls_f() {
    let (out, ok) = compile_and_run(
        r#"
fn handle_error(s: String) -> Result<i64, String> {
    return Result::Ok(99);
}
fn main() {
    let x: Result<i64, String> = Result::Err("fail");
    let y = x.or_else(handle_error);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// ── Type error tests for Option/Result method helpers ─────────────────────

#[test]
fn test_option_is_some_with_args_reports_error() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let x = Option::Some(1);
    let _ = x.is_some(42);
}
"#
    ));
}

#[test]
fn test_option_unwrap_or_type_mismatch_reports_error() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let x = Option::Some(1);
    let _ = x.unwrap_or(true);
}
"#
    ));
}

#[test]
fn test_result_is_ok_with_args_reports_error() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(1);
    let _ = x.is_ok(42);
}
"#
    ));
}

#[test]
fn test_result_map_wrong_fn_type_reports_error() {
    assert!(expect_compile_error(
        r#"
fn wrong(s: String) -> i64 {
    return 0;
}
fn main() {
    let x: Result<i64, String> = Result::Ok(1);
    let _ = x.map(wrong);
}
"#
    ));
}
