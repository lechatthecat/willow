//! willow-ijui.12: execute the proof and its invalidation paths in both modes.
use super::support::*;
use std::time::Duration;

#[test]
fn nonnegative_induction_example() {
    let source = include_str!("../../example/nonnegative_induction.wi");
    let expected = format!(
        "5005000\n{}\n{}",
        (0i64..10000)
            .step_by(2)
            .map(|i| i % 3 + i % 8 + i % 1000)
            .sum::<i64>(),
        (790..807).map(|i| format!("{i}\n")).collect::<String>()
    );
    for (out, ok) in [
        compile_and_run(source),
        compile_and_run_release(source),
        compile_and_run_with_runtime_env(
            source,
            &[
                ("WILLOW_GC_STRESS", "alloc,scheduler"),
                ("WILLOW_TASK_BUDGET", "1"),
            ],
            Duration::from_secs(60),
        ),
    ] {
        assert!(ok, "{out}");
        assert_eq!(out, expected);
    }
}

#[test]
fn nonnegative_induction_faults_and_signed_remainders() {
    let cases = [
        (
            "let mut i = -1; while i < a.len() { println(a[i]); i = i + 1; }",
            "array index out of bounds: the length is 3 but the index is -1",
        ),
        (
            "let mut i = 0; while i <= a.len() { a[i] = i; i = i + 1; }",
            "array index out of bounds: the length is 3 but the index is 3",
        ),
        (
            "let mut i = 0; while i < a.len() { i = -1; a[i] = 8; }",
            "array index out of bounds: the length is 3 but the index is -1",
        ),
        (
            "let mut i = 0; while i < a.len() { change(&i); a[i] = 8; i = i + 1; }",
            "array index out of bounds: the length is 3 but the index is -1",
        ),
        (
            "let mut i = 0; while i < a.len() { a.pop(); a.pop(); a.pop(); a[i] = 8; i = i + 1; }",
            "array index out of bounds: the length is 0 but the index is 0",
        ),
        (
            "let mut i = 0; while i < a.len() { i = i + 1; println(a[i]); }",
            "2\n3\narray index out of bounds: the length is 3 but the index is 3",
        ),
        (
            "let mut i = 0; while i < a.len() { i = i + 1; } if i < a.len() { println(99); } else { println(a[i]); }",
            "array index out of bounds: the length is 3 but the index is 3",
        ),
        (
            "let mut i = 0; while i < a.len() { a[i] = i; i = i + 1; } println(a[-1]);",
            "array index out of bounds: the length is 3 but the index is -1",
        ),
        (
            "let mut i = -7; while i < 0 { println(i % 3); i = i + 1; }",
            "-1\n0\n-2\n-1\n0\n-2\n-1",
        ),
        (
            "let mut i = 0; while i < 3 { println(i % -2); i = i + 1; }",
            "0\n1\n0",
        ),
        (
            "let mut i = 0; while i < 3 { println(i % 0); i = i + 1; }",
            "remainder by zero",
        ),
        (
            "let mut i = 0; while i < 3 { let f = || i; println(f() % 2); i = i + 1; }",
            "0\n1\n0",
        ),
    ];
    let mut source =
        String::from("import std::collections::Array; fn change(i: &mut i64) { i = -1; }");
    let mut calls = String::new();
    let mut expected = String::new();
    for (k, (body, output)) in cases.iter().enumerate() {
        // Recovery lives in the caller so the callee still exercises optimization.
        source.push_str(&format!("fn case{k}(a: Array<i64>) {{ {body} }} fn recover{k}() {{ defer match recover() {{ Some(e) => println(e.message), None => {{}} }} case{k}([1,2,3]); }}"));
        calls.push_str(&format!("recover{k}();"));
        expected.push_str(output);
        expected.push('\n');
    }
    source.push_str(&format!("fn main() {{ {calls} }}"));
    for (out, ok) in [compile_and_run(&source), compile_and_run_release(&source)] {
        assert!(ok, "{out}");
        assert_eq!(out, expected);
    }
}

#[test]
fn nonnegative_induction_wrapping_is_not_proven() {
    let source = r#"
fn wrap() {
    let mut i: i64 = 9223372036854775806;
    while i < 9223372036854775807 {
        i = i + 2;
        println(i % 1000);
        if i < 0 { break; }
    }
}
fn main() {
    defer match recover() { Some(e) => println(e.message), None => {} }
    wrap();
}
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, "integer overflow: `+`\n");
    let (out, ok) = compile_and_run_release(source);
    assert!(ok, "{out}");
    assert_eq!(out, "-808\n");
}
