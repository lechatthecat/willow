use super::*;

// ── Lambda return type inference (willow-cuq) ────────────────────────────────

// and_then with unannotated expression-body lambda
#[test]
fn test_lambda_infer_and_then_expr_body() {
    let (out, ok) = compile_and_run(
        r#"
fn safe_div(a: i64, b: i64) -> Option<i64> {
    if b == 0 { return Option::None; }
    return Option::Some(a / b);
}
fn main() {
    let r = safe_div(20, 4).and_then(|v: i64| safe_div(v, 2));
    println(r.is_some());
    println(r.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n2\n");
}

// and_then with unannotated block-body lambda
#[test]
fn test_lambda_infer_and_then_block_body() {
    let (out, ok) = compile_and_run(
        r#"
fn safe_div(a: i64, b: i64) -> Option<i64> {
    if b == 0 { return Option::None; }
    return Option::Some(a / b);
}
fn main() {
    let r = safe_div(100, 5).and_then(|v: i64| {
        return safe_div(v, 4);
    });
    println(r.is_some());
    println(r.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n5\n");
}

// map with unannotated lambda
#[test]
fn test_lambda_infer_map() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = Option::Some(7).map(|x: i64| x * 2);
    println(r.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "14\n");
}

#[test]
fn test_lambda_context_infers_fn_parameter_type() {
    let (out, ok) = compile_and_run(
        r#"
fn apply(x: i64, f: fn(i64) -> i64) -> i64 {
    return f(x);
}

fn main() {
    println(apply(11, |x| x + 1));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

#[test]
fn test_lambda_context_infers_option_map_parameter_type() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = Option::Some(7).map(|x| x * 2);
    println(r.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "14\n");
}

#[test]
fn test_lambda_context_infers_let_annotation_parameter_type() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let f: fn(i64) -> i64 = |x| x * 3;
    println(f(4));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

#[test]
fn test_lambda_context_infers_assignment_parameter_type() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut f: fn(i64) -> i64 = |x| x + 1;
    f = |x| x * 3;
    println(f(4));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

// or_else with unannotated lambda
#[test]
fn test_lambda_infer_or_else() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r: Option<i64> = Option::None;
    let r2 = r.or_else(|| Option::Some(99));
    println(r2.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// Result and_then with unannotated lambda
#[test]
fn test_lambda_infer_result_and_then() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r: Result<i64, String> = Result::Ok(10);
    let r2 = r.and_then(|v: i64| {
        return Result::Ok(v + 5);
    });
    println(r2.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "15\n");
}

// Explicit annotation still works
#[test]
fn test_lambda_explicit_annotation_unchanged() {
    let (out, ok) = compile_and_run(
        r#"
fn safe_div(a: i64, b: i64) -> Option<i64> {
    if b == 0 { return Option::None; }
    return Option::Some(a / b);
}
fn main() {
    let r = safe_div(20, 4).and_then(|v: i64| -> Option<i64> { return safe_div(v, 2); });
    println(r.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\n");
}
