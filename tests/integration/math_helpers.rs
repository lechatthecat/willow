use super::support::{compile_and_run, compile_and_run_release, compile_error_stderr};

#[test]
fn math_helpers_values_and_example() {
    for run in [compile_and_run, compile_and_run_release] {
        let (out, ok) = run(include_str!("../../example/math_helpers.wi"));
        assert!(ok, "{out}");
        assert_eq!(out, "math helpers ok\n");
    }
}

#[test]
fn math_helpers_diagnostics() {
    for ty in ["i64", "f64"] {
        for op in ["abs", "min", "max", "clamp"] {
            for args in ["", "true", "1, 2, 3, 4"] {
                let expr = format!("{ty}::{op}({args})");
                let err = compile_error_stderr(&format!("fn main() {{ let x = {expr}; }}"));
                assert!(err.contains("E0201"), "{expr}: {err}");
            }
        }
    }
    for expr in [
        "i64::abs(1.0)",
        "f64::abs(1)",
        "i64::min(1, 2.0)",
        "f64::clamp(1.0, 0, 2.0)",
    ] {
        let err = compile_error_stderr(&format!("fn main() {{ let x = {expr}; }}"));
        assert!(err.contains("E0201"), "{expr}: {err}");
    }
    // Primitive namespace syntax rejects explicit generic arguments in parsing.
    for expr in ["i64::abs<i64>(1)", "f64::round<f64>(1.0)"] {
        let err = compile_error_stderr(&format!("fn main() {{ let x = {expr}; }}"));
        assert!(err.contains("E0102"), "{expr}: {err}");
    }
    for op in ["sqrt", "floor", "ceil", "round"] {
        let err = compile_error_stderr(&format!("fn main() {{ let x = f64::{op}(1); }}"));
        assert!(err.contains("E0201"), "{err}");
    }
}

#[test]
fn math_helpers_recovery_and_call_paths() {
    let source = r#"
fn next(x: i64) -> i64 { println(x); return x; }
fn bad_abs() -> i64 { return i64::abs(-9223372036854775807 - 1); }
async fn later() -> f64 { return f64::round(f64::sqrt(8.0)); }
async fn main() {
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        println(bad_abs());
        println("unreachable");
    }
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        println(i64::clamp(1, 4, 2));
    }
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        println(f64::clamp(1.0, 4.0, 2.0));
    }
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        println(f64::clamp(1.0, f64::parse("NaN").unwrap(), 2.0));
    }
    if true {
        defer match recover() { Some(info) => println(info.message), None => {} }
        println(f64::clamp(1.0, 0.0, f64::parse("NaN").unwrap()));
    }
    let clamp = |x: i64| { return i64::clamp(x, 0, 2); };
    let mut total = 0;
    for i in 0..5 { total = total + clamp(i); }
    println(i64::clamp(next(1), next(0), next(2)));
    println(total);
    println(await later());
}
"#;
    for run in [compile_and_run, compile_and_run_release] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(
            out,
            "integer overflow: i64::abs\ninvalid bounds: i64::clamp\ninvalid bounds: f64::clamp\ninvalid bounds: f64::clamp\ninvalid bounds: f64::clamp\n1\n0\n2\n1\n7\n3\n"
        );
    }
}
