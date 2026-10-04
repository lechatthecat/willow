//! A value `match` arm that always leaves by `break`/`continue` of an
//! enclosing loop is typed `Never`, like an arm that always returns
//! (willow-jz15.46).
use super::*;

const PREFIX: &str = r#"
import std::collections::Array;
enum R { Ok(i64), Err(String) }
fn get(i: i64) -> R { return R::Ok(i); }
enum E { X, Y(i64), Z }
"#;

fn errors_of(body: &str) -> Vec<Diagnostic> {
    check_source(&format!("{PREFIX}{body}"))
        .into_iter()
        .filter(|e| e.severity == Severity::Error)
        .collect()
}

#[test]
fn loop_exit_arms_positive_perspectives() {
    let cases = [
        (
            "continue in for",
            "fn f() { for i in 0..3 { let v = match get(i) { R::Ok(x) => x, R::Err(e) => { continue; } }; } }",
        ),
        (
            "break in for",
            "fn f() { for i in 0..3 { let v = match get(i) { R::Ok(x) => x, R::Err(e) => { break; } }; } }",
        ),
        (
            "break in while",
            "fn f() { let mut i = 0; while i < 3 { let v = match get(i) { R::Ok(x) => x, R::Err(e) => { break; } }; i = i + v; } }",
        ),
        (
            "continue in while true",
            "fn f() { let mut i = 0; while true { i = i + 1; let v = match i { 1 => { continue; } _ => i }; if v > 3 { break; } } }",
        ),
        (
            "diverging arm first",
            "fn f() { for i in 0..3 { let v = match get(i) { R::Err(e) => { continue; }, R::Ok(x) => x }; } }",
        ),
        (
            "statements before continue",
            "fn f() { let mut n = 0; for i in 0..3 { let v = match get(i) { R::Ok(x) => x, R::Err(e) => { n = n + 1; let m = n; continue; } }; } }",
        ),
        (
            "defer before continue",
            "fn f() { let mut n = 0; for i in 0..3 { let v = match i { 0 => { defer { n = n + 1; } continue; } _ => i }; } }",
        ),
        (
            "if-else both leave",
            "fn f() { for i in 0..3 { let v = match get(i) { R::Ok(x) => x, R::Err(e) => { if e == \"x\" { break; } else { continue; } } }; } }",
        ),
        (
            "if-else break and return",
            "fn f() -> i64 { for i in 0..3 { let v = match i { 0 => { if i == 0 { return 1; } else { break; } } _ => i }; } return 0; }",
        ),
        (
            "nested match all leave",
            "fn f() { for i in 0..3 { let v = match get(i) { R::Ok(x) => x, R::Err(e) => { match e == \"x\" { true => { break; } false => { continue; } } } }; } }",
        ),
        (
            "nested loops inner target",
            "fn f() { for a in 0..3 { for b in 0..3 { let w = match a == b { true => { continue; } false => a + b }; } } }",
        ),
        (
            "outer loop arm around inner loop",
            "fn f() { for a in 0..3 { let w = match a { 0 => { for b in 0..2 { let c = b; } continue; } _ => a }; } }",
        ),
        (
            "string value",
            "fn f() -> String { let mut out = \"\"; for i in 0..3 { let s = match i { 1 => { continue; } _ => \"v\" }; out = out + s; } return out; }",
        ),
        (
            "enum payload arms",
            "fn f(es: Array<E>) -> i64 { let mut t = 0; for e in es { let v = match e { E::X => { continue; } E::Y(n) => n, E::Z => { break; } }; t = t + v; } return t; }",
        ),
        (
            "argument position",
            "fn g(x: i64) -> i64 { return x; } fn f() { for i in 0..3 { g(match i { 0 => { continue; } _ => i }); } }",
        ),
        (
            "assignment position",
            "fn f() { let mut v = 0; for i in 0..3 { v = match i { 0 => { continue; } _ => i }; } }",
        ),
        (
            "return position in loop",
            "fn f() -> i64 { for i in 0..3 { return match i { 0 => { continue; } _ => i }; } return 0; }",
        ),
        (
            "annotated let",
            "fn f() { for i in 0..3 { let v: i64 = match i { 0 => { continue; } _ => i }; } }",
        ),
        (
            "statement match unaffected",
            "fn f() { for i in 0..3 { match i { 1 => { continue; } _ => { let z = i; } } } }",
        ),
        (
            "all arms leave",
            "fn f() { for i in 0..3 { match i { 1 => { continue; } _ => { break; } } } }",
        ),
    ];
    for (name, body) in cases {
        let errors = errors_of(body);
        assert!(errors.is_empty(), "{name}: {errors:?}");
    }
}

#[test]
fn loop_exit_arms_negative_perspectives() {
    let cases = [
        (
            "break of a loop inside the arm does not leave the arm",
            "fn f() { for i in 0..3 { let v = match i { 0 => 1, _ => { for j in 0..2 { break; } } }; } }",
            ErrorCode::E1201,
        ),
        (
            "continue of a while inside the arm",
            "fn f() { for i in 0..3 { let v = match i { 0 => 1, _ => { while true { continue; } } }; } }",
            ErrorCode::E1201,
        ),
        (
            "conditional continue without else",
            "fn f() { for i in 0..3 { let v = match i { 0 => 1, _ => { if i == 1 { continue; } } }; } }",
            ErrorCode::E1201,
        ),
        (
            "only one branch leaves",
            "fn f() { for i in 0..3 { let v = match i { 0 => 1, _ => { if i == 1 { continue; } else { let z = 2; } } }; } }",
            ErrorCode::E1201,
        ),
        (
            "nested match with one non-leaving arm",
            "fn f() { for i in 0..3 { let v = match i { 0 => 1, _ => { match i { 1 => { continue; } _ => { let z = 2; } } } }; } }",
            ErrorCode::E1201,
        ),
        (
            "break outside any loop is still rejected",
            "fn f() { let v = match 3 { 0 => 1, _ => { break; } }; }",
            ErrorCode::E0904,
        ),
        (
            "continue in a lambda arm is outside its loop",
            "fn f() { for i in 0..3 { let g: fn(i64) -> i64 = |x| match x { 0 => { continue; }, _ => x }; } }",
            ErrorCode::E0904,
        ),
    ];
    for (name, body, code) in cases {
        let errors = errors_of(body);
        assert!(
            errors.iter().any(|e| e.code == code),
            "{name}: expected {code:?}, got {errors:?}"
        );
    }
}

#[test]
fn loop_exit_arm_analysis_ignores_nested_loops() {
    let parse = |src: &str| {
        let tokens = crate::lexer::Lexer::new(src).tokenize().unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        program
    };
    let body_of = |program: &Program| match &program.items[0] {
        Item::Function(f) => f.body.clone(),
        other => panic!("{other:?}"),
    };
    for (src, leaves) in [
        ("fn f() { continue; }", true),
        ("fn f() { break; }", true),
        ("fn f() { return; }", true),
        ("fn f() { let x = 1; }", false),
        ("fn f() { for i in 0..2 { break; } }", false),
        ("fn f() { while true { continue; } }", false),
        ("fn f() { if true { break; } else { return; } }", true),
        ("fn f() { if true { break; } }", false),
    ] {
        let program = parse(src);
        assert_eq!(
            analysis::block_always_leaves_arm(&body_of(&program)),
            leaves,
            "{src}"
        );
    }
}
