use super::*;

// ── Explicit to_string() + String-only concatenation ────────────

#[test]
fn tostring_01_primitives_and_class() {
    let (out, ok) = compile_and_run(
        r#"
class Point { pub x: i64; pub y: i64;
    pub fn to_string(self) -> String { return "(" + self.x.to_string() + ", " + self.y.to_string() + ")"; }
}
fn main() {
    println(42.to_string());
    println(true.to_string());
    println(false.to_string());
    println(3.5.to_string());
    println("hi".to_string());
    println("x = " + 42.to_string());
    let p = new Point(3, 4);
    println(p.to_string());
}
"#,
    );
    assert!(ok, "to_string must compile and run: {out}");
    assert_eq!(out, "42\ntrue\nfalse\n3.5\nhi\nx = 42\n(3, 4)\n");
}

#[test]
fn tostring_02_gc_stress() {
    // to_string allocates WillowStrings; concatenation chains must stay GC-safe.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn main() {
    let n = 7;
    let s = "n=" + n.to_string() + " ok=" + true.to_string();
    println(s);
}
"#,
    );
    assert!(ok, "to_string/concat must survive GC stress: {out}");
    assert_eq!(out, "n=7 ok=true\n");
}

#[test]
fn tostring_03_string_plus_int_rejected() {
    assert_compile_error_contains(
        r#"fn main() { println("x = " + 42); }"#,
        &["error[E0202]", "cannot concatenate", ".to_string()"],
    );
}

#[test]
fn tostring_04_int_plus_string_rejected() {
    assert_compile_error_contains(
        r#"fn main() { let s: String = "y"; println(42 + s); }"#,
        &["error[E0202]", "cannot concatenate"],
    );
}

#[test]
fn tostring_05_string_plus_bool_and_f64_rejected() {
    assert_compile_error_contains(
        r#"fn main() { println("b = " + true); }"#,
        &["error[E0202]", "cannot concatenate `String` with `bool`"],
    );
    assert_compile_error_contains(
        r#"fn main() { println("f = " + 3.5); }"#,
        &["error[E0202]", "cannot concatenate `String` with `f64`"],
    );
}
