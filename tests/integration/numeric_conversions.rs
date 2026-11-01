use super::support::{compile_and_run, compile_and_run_release, compile_error_stderr};

// Each expression is an explicit boundary or call-path perspective.
#[test]
fn numeric_conversions_values() {
    let cases = [
        ("f64::from_i64(0) == 0.0", "integer zero"),
        ("f64::from_i64(42) == 42.0", "positive integer"),
        ("f64::from_i64(-42) == -42.0", "negative integer"),
        (
            "f64::from_i64(9007199254740992) == 9007199254740992.0",
            "exact limit",
        ),
        (
            "f64::from_i64(9007199254740993) == 9007199254740992.0",
            "tie down",
        ),
        (
            "f64::from_i64(9007199254740995) == 9007199254740996.0",
            "tie up",
        ),
        (
            "f64::from_i64(-9007199254740993) == -9007199254740992.0",
            "negative tie",
        ),
        (
            "f64::from_i64(9223372036854775807) == 9223372036854775808.0",
            "integer maximum",
        ),
        (
            "f64::from_i64((-9223372036854775807 - 1)) == -9223372036854775808.0",
            "integer minimum",
        ),
        ("f64::trunc_to_i64(0.0) == 0", "zero"),
        ("f64::trunc_to_i64(-0.0) == 0", "negative zero"),
        ("f64::trunc_to_i64(3.9) == 3", "positive fraction"),
        ("f64::trunc_to_i64(-3.9) == -3", "negative fraction"),
        ("f64::trunc_to_i64(0.999) == 0", "small fraction"),
        ("f64::trunc_to_i64(-0.999) == 0", "small negative fraction"),
        ("f64::trunc_to_i64(1e-300) == 0", "tiny float"),
        (
            "f64::trunc_to_i64(9223372036854774784.0) == 9223372036854774784",
            "last in-range float",
        ),
        (
            "f64::trunc_to_i64(9223372036854775808.0) == 9223372036854775807",
            "upper boundary",
        ),
        (
            "f64::trunc_to_i64(-9223372036854775808.0) == (-9223372036854775807 - 1)",
            "lower boundary",
        ),
        (
            "f64::trunc_to_i64(1e300) == 9223372036854775807",
            "positive overflow",
        ),
        (
            "f64::trunc_to_i64(-1e300) == (-9223372036854775807 - 1)",
            "negative overflow",
        ),
        (
            "f64::trunc_to_i64(f64::parse(\"NaN\").unwrap()) == 0",
            "NaN",
        ),
        (
            "f64::trunc_to_i64(f64::parse(\"inf\").unwrap()) == 9223372036854775807",
            "infinity",
        ),
        (
            "f64::trunc_to_i64(f64::parse(\"-inf\").unwrap()) == (-9223372036854775807 - 1)",
            "negative infinity",
        ),
        ("f64::from_i64(3) * 1.5 == 4.5", "mixed arithmetic repaired"),
        (
            "f64::trunc_to_i64(f64::from_i64(123)) == 123",
            "nested round trip",
        ),
    ];
    let mut source = String::from("fn main() {\n");
    for (expr, label) in cases {
        source.push_str(&format!("if !({expr}) {{ panic(\"{label}\"); }}\n"));
    }
    source.push_str("println(\"ok\"); }");
    for run in [compile_and_run, compile_and_run_release] {
        let (out, ok) = run(&source);
        assert!(ok, "{out}");
        assert_eq!(out, "ok\n");
    }
}

#[test]
fn numeric_conversions_diagnostics() {
    for op in ["+", "-", "*", "/", "%", "**"] {
        for (left, right) in [("3", "1.5"), ("1.5", "3")] {
            let err =
                compile_error_stderr(&format!("fn main() {{ let x = {left} {op} {right}; }}"));
            assert!(err.contains("E0202"), "{err}");
            assert!(err.contains("f64::from_i64(integer)"), "{err}");
            assert!(err.contains("f64::trunc_to_i64(float)"), "{err}");
        }
    }
    for expr in [
        "f64::from_i64()",
        "f64::trunc_to_i64()",
        "f64::from_i64(1, 2)",
        "f64::trunc_to_i64(1.0, 2.0)",
        "f64::from_i64(1.0)",
        "f64::trunc_to_i64(1)",
        "f64::from_i64(true)",
        "f64::trunc_to_i64(\"1\")",
    ] {
        let err = compile_error_stderr(&format!("fn main() {{ let x = {expr}; }}"));
        assert!(err.contains("E0201"), "{expr}: {err}");
    }
    let err = compile_error_stderr("fn main() { let x = 3 + true; }");
    assert!(!err.contains("f64::from_i64"), "{err}");
}

#[test]
fn numeric_conversions_example() {
    let (out, ok) = compile_and_run(include_str!("../../example/numeric_conversions.wi"));
    assert!(ok, "{out}");
    assert_eq!(out, "4.5\n-4\n");
}

#[test]
fn numeric_conversions_call_paths() {
    let source = r#"
fn convert(x: i64) -> i64 { return f64::trunc_to_i64(f64::from_i64(x) * 1.5); }
async fn later(x: i64) -> i64 { return convert(x); }
async fn main() {
    let convert_again = |x: i64| { return f64::trunc_to_i64(f64::from_i64(x)); };
    let mut total = 0;
    let mut i = 0;
    while i < 10 { total = total + convert_again(i); i = i + 1; }
    println(total);
    println(await later(3));
}
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, "45\n4\n");
}
