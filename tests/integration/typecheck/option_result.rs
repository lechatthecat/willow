use super::super::support::*;

// ── Option<T> and Result<T,E> ─────────────────────────────────────────────────

#[test]
fn test_option_some_and_none_i64() {
    let src = r#"
fn safe_div(a: i64, b: i64) -> Option<i64> {
    if b == 0 {
        return Option::None;
    }
    return Option::Some(a / b);
}

fn main() {
    let r1 = match safe_div(10, 2) {
        Option::Some(v) => v,
        Option::None => -1,
    };
    let r2 = match safe_div(7, 0) {
        Option::Some(v) => v,
        Option::None => -1,
    };
    println(r1);
    println(r2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option<i64> should compile and run");
    assert_eq!(out, "5\n-1\n");
}

#[test]
fn test_option_none_in_function_return() {
    let src = r#"
fn first_positive(a: i64, b: i64) -> Option<i64> {
    if a > 0 {
        return Option::Some(a);
    }
    if b > 0 {
        return Option::Some(b);
    }
    return Option::None;
}

fn main() {
    let r1 = match first_positive(-1, 5) {
        Option::Some(v) => v,
        Option::None => 0,
    };
    let r2 = match first_positive(-3, -7) {
        Option::Some(v) => v,
        Option::None => 0,
    };
    println(r1);
    println(r2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option::None in function return should compile");
    assert_eq!(out, "5\n0\n");
}

#[test]
fn test_option_map_via_match() {
    let src = r#"
fn double_opt(opt: Option<i64>) -> Option<i64> {
    return match opt {
        Option::Some(v) => Option::Some(v * 2),
        Option::None => Option::None,
    };
}

fn main() {
    let r1 = match double_opt(Option::Some(21)) {
        Option::Some(v) => v,
        Option::None => -1,
    };
    let r2 = match double_opt(Option::None) {
        Option::Some(v) => v,
        Option::None => -1,
    };
    println(r1);
    println(r2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option::map-like function should compile");
    assert_eq!(out, "42\n-1\n");
}

#[test]
fn test_result_ok_and_err_i64_string() {
    let src = r#"
fn parse_positive(n: i64) -> Result<i64, String> {
    if n <= 0 {
        return Result::Err("non-positive");
    }
    return Result::Ok(n * 10);
}

fn main() {
    let v1 = match parse_positive(5) {
        Result::Ok(v) => v,
        Result::Err(_) => -1,
    };
    let v2 = match parse_positive(-3) {
        Result::Ok(v) => v,
        Result::Err(_) => -1,
    };
    println(v1);
    println(v2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Result<i64,String> should compile and run");
    assert_eq!(out, "50\n-1\n");
}

#[test]
fn test_result_err_message_extracted() {
    let src = r#"
fn parse_even(n: i64) -> Result<i64, String> {
    if n % 2 != 0 {
        return Result::Err("not even");
    }
    return Result::Ok(n / 2);
}

fn main() {
    let msg = match parse_even(7) {
        Result::Ok(_) => "ok",
        Result::Err(e) => e,
    };
    println(msg);
    let val = match parse_even(8) {
        Result::Ok(v) => v,
        Result::Err(_) => -1,
    };
    println(val);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Result Err payload extraction should compile");
    assert_eq!(out, "not even\n4\n");
}

#[test]
fn test_option_f64_payload() {
    let src = r#"
fn safe_sqrt(x: f64) -> Option<f64> {
    if x < 0.0 {
        return Option::None;
    }
    return Option::Some(pow(x, 0.5));
}

fn main() {
    let r1 = match safe_sqrt(9.0) {
        Option::Some(v) => v,
        Option::None => -1.0,
    };
    let r2 = match safe_sqrt(-4.0) {
        Option::Some(v) => v,
        Option::None => -1.0,
    };
    println(r1);
    println(r2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option<f64> payload should compile and run");
    assert_eq!(out, "3\n-1\n");
}

// ── ? operator ────────────────────────────────────────────────────────────────

#[test]
fn test_try_propagate_extracts_ok_payload() {
    let src = r#"
fn safe_div(a: i64, b: i64) -> Result<i64, String> {
    if b == 0 { return Result::Err("zero"); }
    return Result::Ok(a / b);
}

fn halve(n: i64) -> Result<i64, String> {
    return Result::Ok(safe_div(n, 2)?);
}

fn main() {
    let r = halve(10);
    let v = match r {
        Result::Ok(x) => x,
        Result::Err(_) => -1,
    };
    println(v);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "? operator should compile and run");
    assert_eq!(out, "5\n");
}

#[test]
fn test_try_propagate_returns_err_early() {
    let src = r#"
fn fail() -> Result<i64, String> {
    return Result::Err("oops");
}

fn caller() -> Result<i64, String> {
    let v = fail()?;
    return Result::Ok(v + 1);
}

fn main() {
    let r = caller();
    let msg = match r {
        Result::Ok(_) => "ok",
        Result::Err(e) => e,
    };
    println(msg);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "? early return should compile and run");
    assert_eq!(out, "oops\n");
}

#[test]
fn test_try_propagate_chains_multiple_calls() {
    let src = r#"
fn parse(s: String) -> Result<i64, String> {
    if s == "10" { return Result::Ok(10); }
    if s == "20" { return Result::Ok(20); }
    return Result::Err("bad input");
}

fn sum_two(a: String, b: String) -> Result<i64, String> {
    let x = parse(a)?;
    let y = parse(b)?;
    return Result::Ok(x + y);
}

fn main() {
    let r1 = sum_two("10", "20");
    let v1 = match r1 { Result::Ok(v) => v, Result::Err(_) => -1, };
    println(v1);
    let r2 = sum_two("10", "bad");
    let v2 = match r2 { Result::Ok(v) => v, Result::Err(_) => -1, };
    println(v2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "chained ? should compile and run");
    assert_eq!(out, "30\n-1\n");
}

#[test]
fn test_option_try_propagate_extracts_some_payload() {
    let src = r#"
fn maybe(n: i64) -> Option<i64> {
    if n > 0 { return Option::Some(n); }
    return Option::None;
}

fn doubled(n: i64) -> Option<i64> {
    let v = maybe(n)?;
    return Option::Some(v * 2);
}

fn main() {
    let a = doubled(21);
    let av = match a { Option::Some(v) => v, Option::None => -1, };
    println(av);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option ? should extract Some payload");
    assert_eq!(out, "42\n");
}

#[test]
fn test_option_try_propagate_returns_none_early() {
    let src = r#"
fn maybe(n: i64) -> Option<i64> {
    if n > 0 { return Option::Some(n); }
    return Option::None;
}

fn doubled(n: i64) -> Option<i64> {
    let v = maybe(n)?;
    return Option::Some(v * 2);
}

fn main() {
    let a = doubled(-1);
    let av = match a { Option::Some(v) => v, Option::None => -1, };
    println(av);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option ? should propagate None");
    assert_eq!(out, "-1\n");
}

#[test]
fn test_option_try_propagate_preserves_f64_payload_type() {
    let src = r#"
fn maybe(flag: bool) -> Option<f64> {
    if flag { return Option::Some(2.5); }
    return Option::None;
}

fn add(flag: bool) -> Option<f64> {
    let v = maybe(flag)?;
    return Option::Some(v + 0.5);
}

fn main() {
    let a = add(true);
    let av = match a { Option::Some(v) => v, Option::None => -1.0, };
    println(av);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option ? should preserve f64 payloads");
    assert_eq!(out, "3\n");
}

#[test]
fn test_try_propagate_on_non_result_reports_e1806() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x: i64 = 42;
    let y = x?;
    println(y);
}
"#,
        &[
            "error[E1806]",
            "requires `Result<T,E>` or `Option<T>`",
            "found `i64`",
        ],
    );
}

#[test]
fn test_try_propagate_in_non_result_function_reports_e1807() {
    assert_compile_error_contains(
        r#"
fn get() -> Result<i64, String> {
    return Result::Ok(1);
}

fn main() {
    let v = get()?;
    println(v);
}
"#,
        &[
            "error[E1807]",
            "can only be used inside a function returning `Result",
        ],
    );
}

#[test]
fn test_ternary_and_try_propagate_coexist() {
    let src = r#"
fn ok_or(n: i64) -> Result<i64, String> {
    if n > 0 { return Result::Ok(n); }
    return Result::Err("non-positive");
}

fn scaled(n: i64) -> Result<i64, String> {
    let v = ok_or(n)?;
    let factor = v > 5 ? 10 : 1;
    return Result::Ok(v * factor);
}

fn main() {
    let r1 = scaled(7);
    let v1 = match r1 { Result::Ok(v) => v, Result::Err(_) => -1, };
    println(v1);
    let r2 = scaled(3);
    let v2 = match r2 { Result::Ok(v) => v, Result::Err(_) => -1, };
    println(v2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "? and ternary ? should coexist");
    assert_eq!(out, "70\n3\n");
}

// ── Option / Result GC tracing ────────────────────────────────────────────────

#[test]
fn test_option_some_class_payload_survives_gc_collect() {
    let src = r#"
class Node {
    pub value: i64;
    pub fn get(self) -> i64 { return self.value; }
}

fn make_some(v: i64) -> Option<Node> {
    let n = new Node(v);
    return Option::Some(n);
}

fn main() {
    let opt = make_some(42);
    gc_collect();
    let v = match opt {
        Option::Some(n) => n.get(),
        Option::None => -1,
    };
    println(v);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option<Node> should compile and run");
    assert_eq!(out, "42\ntrue\n", "Node payload must survive gc_collect");
}

#[test]
fn test_option_none_traces_nothing() {
    let src = r#"
class Node { pub value: i64; }

fn empty() -> Option<Node> {
    return Option::None;
}

fn main() {
    let opt = empty();
    gc_collect();
    let v = match opt {
        Option::Some(n) => n.value,
        Option::None => 0,
    };
    println(v);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option::None should compile and run");
    assert_eq!(out, "0\n");
}

#[test]
fn test_result_ok_class_payload_survives_gc_collect() {
    let src = r#"
class Node { pub value: i64; }

fn make_ok(v: i64) -> Result<Node, String> {
    let n = new Node(v);
    return Result::Ok(n);
}

fn main() {
    let r = make_ok(99);
    gc_collect();
    let v = match r {
        Result::Ok(n) => n.value,
        Result::Err(_) => -1,
    };
    println(v);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Result<Node,String> should compile and run");
    assert_eq!(
        out, "99\ntrue\n",
        "Node payload in Ok must survive gc_collect"
    );
}

#[test]
fn test_option_some_unrooted_option_collected_after_use() {
    let src = r#"
class Node { pub value: i64; }

fn alloc_and_use() -> i64 {
    let n = new Node(7);
    let opt = Option::Some(n);
    let v = match opt {
        Option::Some(nd) => nd.value,
        Option::None => -1,
    };
    return v;
}

fn main() {
    let v = alloc_and_use();
    println(v);
    gc_collect();
    println(gc_allocated_bytes());
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option wrapping class should compile and run");
    assert_eq!(
        out, "7\n0\n",
        "Option and Node should be collected after use"
    );
}

// ── Option / Result exhaustiveness ────────────────────────────────────────────

#[test]
fn test_option_match_missing_none_reports_e1202() {
    assert_compile_error_contains(
        r#"
fn main() {
    let opt: Option<i64> = Option::Some(1);
    let v = match opt {
        Option::Some(x) => x,
    };
    println(v);
}
"#,
        &["error[E1202]", "variant `Option::None` not covered"],
    );
}

#[test]
fn test_option_match_missing_some_reports_e1202() {
    assert_compile_error_contains(
        r#"
fn main() {
    let opt: Option<i64> = Option::None;
    let v = match opt {
        Option::None => 0,
    };
    println(v);
}
"#,
        &["error[E1202]", "variant `Option::Some` not covered"],
    );
}

#[test]
fn test_option_match_wildcard_arm_is_exhaustive() {
    let src = r#"
fn main() {
    let opt: Option<i64> = Option::Some(42);
    let v = match opt {
        Option::Some(x) => x,
        _ => 0,
    };
    println(v);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "wildcard arm should satisfy exhaustiveness");
    assert_eq!(out, "42\n");
}

#[test]
fn test_result_match_missing_err_reports_e1202() {
    assert_compile_error_contains(
        r#"
fn main() {
    let r: Result<i64, String> = Result::Ok(1);
    let v = match r {
        Result::Ok(x) => x,
    };
    println(v);
}
"#,
        &["error[E1202]", "variant `Result::Err` not covered"],
    );
}

#[test]
fn test_result_match_missing_ok_reports_e1202() {
    assert_compile_error_contains(
        r#"
fn main() {
    let r: Result<i64, String> = Result::Err("bad");
    let v = match r {
        Result::Err(e) => 0,
    };
    println(v);
}
"#,
        &["error[E1202]", "variant `Result::Ok` not covered"],
    );
}

#[test]
fn test_result_match_wildcard_arm_is_exhaustive() {
    let src = r#"
fn parse(n: i64) -> Result<i64, String> {
    if n < 0 { return Result::Err("negative"); }
    return Result::Ok(n);
}

fn main() {
    let v = match parse(5) {
        Result::Ok(x) => x,
        _ => -1,
    };
    println(v);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "wildcard arm satisfies exhaustiveness for Result");
    assert_eq!(out, "5\n");
}

#[test]
fn test_option_unknown_variant_reports_e1801() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = Option::Maybe(1);
}
"#,
        &["error[E1801]", "unknown variant `Maybe` in `Option`"],
    );
}

#[test]
fn test_result_unknown_variant_reports_e1801() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = Result::Value(1);
}
"#,
        &["error[E1801]", "unknown variant `Value` in `Result`"],
    );
}
