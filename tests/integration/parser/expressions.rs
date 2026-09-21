use super::super::support::*;

#[test]
fn test_arithmetic_i64() {
    let src = r#"
fn main() {
    let a = 10;
    let b = 3;
    println(a + b);
    println(a - b);
    println(a * b);
    println(a / b);
    println(a % b);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "13\n7\n30\n3\n1\n");
}

#[test]
fn test_arithmetic_f64() {
    let src = r#"
fn main() {
    let x: f64 = 2.5;
    let y: f64 = 4.0;
    println(x * y);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "10");
}

#[test]
fn test_operator_precedence_and_parentheses() {
    let src = r#"
fn main() {
    println(1 + 2 * 3);
    println((1 + 2) * (3 + 4));
    println(20 / (3 + 2));
    println(20 % (3 + 2));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "7\n21\n4\n0\n");
}

#[test]
fn test_arithmetic_left_associative() {
    let src = r#"
fn main() {
    println(20 - 5 - 3);
    println(100 / 5 / 2);
    println(29 % 10 % 4);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "12\n10\n1\n");
}

#[test]
fn test_negative_values_in_expressions() {
    let src = r#"
fn main() {
    let a = -10;
    let b = 4;

    println(a + b);
    println(a * -b);
    println((a - b) / 2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "-6\n40\n-7\n");
}

#[test]
fn test_f64_comparison_and_unary_neg() {
    let src = r#"
fn main() {
    let x: f64 = -2.5;
    let y: f64 = 5.0;
    println(x < 0.0);
    println(y / 2.0);
    println(x != y);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "true\n2.5\ntrue\n");
}

#[test]
fn test_f64_equality_and_false_comparisons() {
    let src = r#"
fn main() {
    let x: f64 = 1.5;
    let y: f64 = 2.5;

    println(x == y);
    println(x != y);
    println(y <= x);
    println(y > x);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "false\ntrue\nfalse\ntrue\n");
}

// ── Boolean operators ─────────────────────────────────────────────────────────

#[test]
fn test_bool_operators() {
    let src = r#"
fn main() {
    println(true && false);
    println(true || false);
    println(!true);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "false\ntrue\nfalse\n");
}

#[test]
fn test_bool_operators_short_circuit_rhs() {
    let src = r#"
fn marker(value: bool) -> bool {
    println(99);
    return value;
}

fn main() {
    println(false && marker(true));
    println(true || marker(false));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "false\ntrue\n");
}

#[test]
fn test_bool_operator_precedence() {
    let src = r#"
fn main() {
    println(true || false && false);
    println((true || false) && false);
    println(!false && true);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "true\nfalse\ntrue\n");
}

// ── Comparisons ───────────────────────────────────────────────────────────────

#[test]
fn test_comparisons() {
    let src = r#"
fn main() {
    println(1 == 1);
    println(1 != 2);
    println(3 < 5);
    println(5 <= 5);
    println(6 > 4);
    println(7 >= 7);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "true\ntrue\ntrue\ntrue\ntrue\ntrue\n");
}

#[test]
fn test_comparison_false_cases() {
    let src = r#"
fn main() {
    println(1 == 2);
    println(1 != 1);
    println(3 < 2);
    println(5 <= 4);
    println(6 > 9);
    println(7 >= 8);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "false\nfalse\nfalse\nfalse\nfalse\nfalse\n");
}

// ── Negative numbers ──────────────────────────────────────────────────────────

#[test]
fn test_unary_neg() {
    let src = r#"
fn main() {
    let x = -42;
    println(x);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "-42");
}

// ── Compile error cases ───────────────────────────────────────────────────────
