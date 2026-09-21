use super::*;

// ── Explicit Option construction (willow-glaj.8 migration) ──────────────────

// 1. Explicit Some(String) construction compiles and prints.
#[test]
fn test_option_some_string_literal() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s: Option<String> = Some("hello");
    println(s.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// 2. An Option<String> return explicitly chooses Some or None.
#[test]
fn test_option_return_string_is_explicit() {
    let (out, ok) = compile_and_run(
        r#"
fn greet(flag: bool) -> Option<String> {
    if flag { return Some("hi"); }
    return None;
}
fn main() {
    let a = greet(true);
    let b = greet(false);
    println(a.unwrap());
    if b.is_none() { println("none"); }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hi\nnone\n");
}

// 3. Option parameters require explicit construction at the call site.
#[test]
fn test_option_argument_construction_is_explicit() {
    let (out, ok) = compile_and_run(
        r#"
fn print_maybe(s: Option<String>) {
    match s { Some(value) => println(value), None => println("empty") }
}
fn main() {
    print_maybe(Some("world"));
    print_maybe(None);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "world\nempty\n");
}

// 4. A non-Option value cannot initialize an Option.
#[test]
fn test_option_rejects_unwrapped_unrelated_type() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let s: Option<String> = 42;
}
"#
    ));
}

// 5. None is the sole absence value.
#[test]
fn test_option_none_is_explicit() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s: Option<String> = None;
    if s.is_none() { println("none"); }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "none\n");
}

// 6. Class values use explicit Some construction too.
#[test]
fn test_option_class_construction_is_explicit() {
    let (out, ok) = compile_and_run(
        r#"
class Box { pub v: i64; pub fn get(self) -> i64 { return self.v; } }
fn maybe(flag: bool) -> Option<Box> {
    if flag { return Some(new Box(99)); }
    return None;
}
fn main() {
    let b = maybe(true);
    match b { Some(value) => println(value.get()), None => println(0) }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}
