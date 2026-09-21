use super::super::support::*;

// ───────────────────────────────────────────────────────────────────────────
// Unqualified enum-variant CONSTRUCTION (`Ok(42)` vs `Result::Ok(42)`) resolved
// by expected type, slice 1: payload variants in let-annotation and call-arg
// positions (willow-60o.1). Fieldless variants, return position, and patterns
// are follow-up slices.
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn test_unqualified_variant_let_annotation_constructs() {
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
    Idle(i64),
}

fn code(s: Status) -> i64 {
    return match s {
        Status::Active(n) => n,
        Status::Idle(n) => n + 1000,
    };
}

fn main() {
    let a: Status = Active(42);
    let b: Status = Idle(7);
    println(code(a));
    println(code(b));
}
"#,
    );
    assert!(
        ok,
        "unqualified variant construction (let) should compile and run"
    );
    assert_eq!(out, "42\n1007\n");
}

#[test]
fn test_unqualified_variant_call_argument_constructs() {
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
    Idle(i64),
}

fn code(s: Status) -> i64 {
    return match s {
        Status::Active(n) => n,
        Status::Idle(n) => n + 1000,
    };
}

fn main() {
    println(code(Active(42)));
    println(code(Idle(7)));
}
"#,
    );
    assert!(
        ok,
        "unqualified variant construction (arg) should compile and run"
    );
    assert_eq!(out, "42\n1007\n");
}

#[test]
fn test_unqualified_variant_matches_qualified_construction() {
    // The unqualified form must build the same value as the qualified form.
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
}

fn code(s: Status) -> i64 {
    return match s {
        Status::Active(n) => n,
    };
}

fn main() {
    let a: Status = Active(5);
    let b: Status = Status::Active(5);
    println(code(a) + code(b));
}
"#,
    );
    assert!(ok, "unqualified and qualified construction should agree");
    assert_eq!(out, "10\n");
}

#[test]
fn test_unqualified_variant_wrong_payload_type_reports_error() {
    assert_compile_error_contains(
        r#"
enum Status {
    Active(i64),
}

fn main() {
    let a: Status = Active(true);
}
"#,
        &["error[E0201]", "mismatched types"],
    );
}

#[test]
fn test_unqualified_variant_wrong_arity_reports_error() {
    assert_compile_error_contains(
        r#"
enum Status {
    Active(i64),
}

fn main() {
    let a: Status = Active(1, 2);
}
"#,
        &["error[E0201]", "takes 1 argument(s), got 2"],
    );
}

#[test]
fn test_unqualified_variant_requires_expected_enum_context() {
    // Type-directed: without an expected enum type, `Active(42)` is just an
    // unknown function call — confirms resolution is not global.
    assert_compile_error_contains(
        r#"
enum Status {
    Active(i64),
}

fn main() {
    let a = Active(42);
}
"#,
        &["error[E0350]"],
    );
}

#[test]
fn test_non_variant_call_in_enum_context_still_calls_function() {
    // A real function call in an expected-enum position must NOT be hijacked as
    // a variant (its name is not a variant of the enum).
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
}

fn make(n: i64) -> Status {
    return Status::Active(n);
}

fn code(s: Status) -> i64 {
    return match s {
        Status::Active(n) => n,
    };
}

fn main() {
    let a: Status = make(9);
    println(code(a));
}
"#,
    );
    assert!(
        ok,
        "non-variant function call in enum context should still call the function"
    );
    assert_eq!(out, "9\n");
}

#[test]
fn test_unqualified_fieldless_variant_constructs() {
    // A fieldless variant (`Closed`) is a bare identifier, resolved in both
    // let-annotation and argument positions.
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
    Closed,
}

fn code(s: Status) -> i64 {
    return match s {
        Status::Active(n) => n,
        Status::Closed => -1,
    };
}

fn main() {
    let a: Status = Active(42);
    let c: Status = Closed;
    println(code(a));
    println(code(c));
    println(code(Closed));
}
"#,
    );
    assert!(
        ok,
        "unqualified fieldless variant construction should compile and run"
    );
    assert_eq!(out, "42\n-1\n-1\n");
}

#[test]
fn test_unqualified_fieldless_variant_requires_expected_enum_context() {
    // Without an expected enum type, a bare `Closed` is an undefined name.
    assert_compile_error_contains(
        r#"
enum Status {
    Active(i64),
    Closed,
}

fn main() {
    let x = Closed;
}
"#,
        &["error[E0350]"],
    );
}

#[test]
fn test_local_variable_shadows_fieldless_variant_name() {
    // A local variable named like a fieldless variant takes precedence over the
    // variant when used as a value.
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
    Closed,
}

fn main() {
    let Closed = 7;
    println(Closed);
}
"#,
    );
    assert!(
        ok,
        "a local variable should shadow a fieldless variant name"
    );
    assert_eq!(out, "7\n");
}

#[test]
fn test_unqualified_variant_in_return_position_constructs() {
    // `return Active(n)` / `return Closed` resolve against the function's
    // return type.
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
    Closed,
}

fn make(n: i64) -> Status {
    if n < 0 {
        return Closed;
    }
    return Active(n);
}

fn code(s: Status) -> i64 {
    return match s {
        Status::Active(n) => n,
        Status::Closed => -1,
    };
}

fn main() {
    println(code(make(42)));
    println(code(make(-5)));
}
"#,
    );
    assert!(
        ok,
        "unqualified variant in return position should compile and run"
    );
    assert_eq!(out, "42\n-1\n");
}

#[test]
fn test_unqualified_generic_variant_result_and_option_construct() {
    // The headline case: `Ok`/`Err`/`Some`/`None` resolved against a generic
    // `Result<T, E>` / `Option<T>` expected type.
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r: Result<i64, String> = Ok(42);
    let e: Result<i64, String> = Err("bad");
    let o: Option<i64> = Some(7);
    let n: Option<i64> = None;
    println(match r {
        Result::Ok(v) => v,
        Result::Err(_) => -1,
    });
    println(match e {
        Result::Ok(v) => v,
        Result::Err(_) => -2,
    });
    println(match o {
        Option::Some(v) => v,
        Option::None => -3,
    });
    println(match n {
        Option::Some(v) => v,
        Option::None => -4,
    });
}
"#,
    );
    assert!(
        ok,
        "unqualified generic variant construction should compile and run"
    );
    assert_eq!(out, "42\n-2\n7\n-4\n");
}

#[test]
fn test_unqualified_generic_variant_in_argument_and_return() {
    // Call-argument and return positions for a generic enum.
    let (out, ok) = compile_and_run(
        r#"
fn wrap(n: i64) -> Result<i64, String> {
    if n < 0 {
        return Err("negative");
    }
    return Ok(n);
}

fn unwrap_or(r: Result<i64, String>, d: i64) -> i64 {
    return match r {
        Result::Ok(v) => v,
        Result::Err(_) => d,
    };
}

fn main() {
    println(unwrap_or(wrap(10), 0));
    println(unwrap_or(wrap(-1), 99));
    println(unwrap_or(Ok(5), 0));
}
"#,
    );
    assert!(ok, "generic variant in arg/return should compile and run");
    assert_eq!(out, "10\n99\n5\n");
}

#[test]
fn test_unqualified_generic_variant_wrong_payload_type_reports_error() {
    assert_compile_error_contains(
        r#"
fn main() {
    let r: Result<i64, String> = Ok(true);
}
"#,
        &["error[E0201]", "mismatched types"],
    );
}

// ── Unqualified enum-variant PATTERNS in `match` (willow-60o.1) ──────────────

#[test]
fn test_unqualified_pattern_non_generic_payload_and_fieldless() {
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
    Closed,
}

fn code(s: Status) -> i64 {
    return match s {
        Active(n) => n,
        Closed => -1,
    };
}

fn main() {
    println(code(Active(42)));
    println(code(Closed));
}
"#,
    );
    assert!(
        ok,
        "unqualified non-generic variant patterns should compile and run"
    );
    assert_eq!(out, "42\n-1\n");
}

#[test]
fn test_unqualified_pattern_generic_result_and_option() {
    let (out, ok) = compile_and_run(
        r#"
fn unwrap_or(r: Result<i64, String>, d: i64) -> i64 {
    return match r {
        Ok(v) => v,
        Err(_) => d,
    };
}

fn first(o: Option<i64>) -> i64 {
    return match o {
        Some(v) => v,
        None => -1,
    };
}

fn main() {
    println(unwrap_or(Ok(42), 0));
    println(unwrap_or(Err("x"), 99));
    println(first(Some(7)));
    println(first(None));
}
"#,
    );
    assert!(
        ok,
        "unqualified generic variant patterns should compile and run"
    );
    assert_eq!(out, "42\n99\n7\n-1\n");
}

#[test]
fn test_unqualified_and_qualified_patterns_mix_in_one_match() {
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
    Idle(i64),
    Closed,
}

fn code(s: Status) -> i64 {
    return match s {
        Active(n) => n,            // unqualified
        Status::Idle(n) => n + 1000, // qualified
        Closed => -1,              // unqualified fieldless
    };
}

fn main() {
    println(code(Active(5)));
    println(code(Idle(5)));
    println(code(Closed));
}
"#,
    );
    assert!(ok, "mixing qualified and unqualified patterns should work");
    assert_eq!(out, "5\n1005\n-1\n");
}

#[test]
fn test_catch_all_binding_not_confused_with_variant() {
    // A binding whose name is not a variant of the scrutinee enum is still a
    // catch-all binding, not a variant pattern.
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
    Closed,
}

fn code(s: Status) -> i64 {
    return match s {
        Active(n) => n,
        other => -1,
    };
}

fn main() {
    println(code(Active(42)));
    println(code(Closed));
}
"#,
    );
    assert!(ok, "a non-variant binding name should remain a catch-all");
    assert_eq!(out, "42\n-1\n");
}

// Perspective 22: the full happy path — `?` extracts the Ok payload, chains,
// and propagates an early Err — compiles and runs.
#[test]
fn test_question_operator_happy_path_end_to_end() {
    let (out, ok) = compile_and_run(
        r#"
fn checked(n: i64) -> Result<i64, String> {
    if n < 0 { return Result::Err("negative"); }
    return Result::Ok(n);
}

fn pipeline(n: i64) -> Result<i64, String> {
    let a = checked(n)?;
    let b = checked(a - 5)?;
    return Result::Ok(b);
}

fn main() {
    let good = pipeline(10);
    println(match good { Result::Ok(v) => v, Result::Err(_) => -1, });
    let bad = pipeline(2);
    println(match bad { Result::Ok(v) => v, Result::Err(_) => -1, });
}
"#,
    );
    assert!(ok, "? happy path must compile and run");
    assert_eq!(out, "5\n-1\n");
}

#[test]
fn unqualified_multiple_payload_bindings() {
    let (out, ok) = compile_and_run(
        r#"
enum PairValue { Pair(i64, i64), Empty }
fn main() {
    let value = PairValue::Pair(12, 30);
    println(match value { Pair(a, b) => a + b, Empty => 0 });
    println(match value { PairValue::Pair(a, b) => a + b, PairValue::Empty => 0 });
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n42\n");
}

#[test]
fn unqualified_multiple_payload_wrong_arity() {
    let stderr = compile_error_stderr(
        r#"
enum PairValue { Pair(i64, i64) }
fn main() {
    let value = PairValue::Pair(1, 2);
    println(match value { Pair(a, b, c) => a });
}
"#,
    );
    assert!(
        stderr.contains("error[") && stderr.contains("binding"),
        "{stderr}"
    );
}
