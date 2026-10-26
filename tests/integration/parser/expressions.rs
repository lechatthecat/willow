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

#[test]
fn test_call_result_binding_example() {
    let (out, ok) = compile_and_run(include_str!("../../../example/call_result_binding.wi"));
    assert!(ok, "compilation failed");
    assert_eq!(out, "5\n5.5\n");
}

#[test]
fn test_closure_field_binding_example() {
    let (out, ok) = compile_and_run(include_str!("../../../example/closure_field_binding.wi"));
    assert!(ok, "compilation failed");
    assert_eq!(out, "10\n21\n");
}

#[test]
fn test_parenthesized_callable_field_perspectives() {
    let source = r#"
class Holder {
    pub f: closure(i64) -> i64;
    pub fn run(self, x: i64) -> i64 { return (self.f)(x); }
    pub fn method(self, x: i64) -> i64 { return x + 100; }
}
class Box<T> {
    pub f: closure(T) -> T;
    pub fn run(self, x: T) -> T { return ((self.f))(x); }
}
class Outer { pub inner: Holder; }
class Zero { pub f: closure() -> i64; }
class Two { pub f: closure(i64, i64) -> i64; }
class Flag { pub f: closure(bool) -> bool; }
class Text { pub f: closure(String) -> String; }
fn make() -> Holder { println(90); return new Holder(|x: i64| x + 1); }
fn argument() -> i64 { println(91); return 2; }
fn main() {
    let offset = 3;
    let h = new Holder(|x: i64| x + offset);
    let b = new Box<i64>(|x: i64| x * offset);
    let o = new Outer(h);
    let z = new Zero(|| offset);
    let t = new Two(|x: i64, y: i64| x + y + offset);
    let flag = new Flag(|x: bool| !x);
    let text = new Text(|x: String| x);
    println((h.f)(2));
    println(((h.f))(3));
    println(h.run(4));
    println((b.f)(5));
    println(b.run(6));
    println((o.inner.f)(7));
    println((z.f)());
    println((t.f)(1, 2));
    println((flag.f)(true));
    println((text.f)("ok"));
    println(1 + (h.f)(8));
    println((h.f)((b.f)(2)));
    println((h.f)(1,));
    println(h.method(2));
    let local = h.f;
    println((local)(3));
    println((make().f)(argument()));
    let many = [h];
    println((many[0].f)(4));
    println([(h.f)(5)][0]);
    println(if true { (h.f)(6) } else { 0 });
    println((h.f)(1) + (h.f)(2));
    let strings = new Box<String>(|x: String| x);
    println((strings.f)("generic"));
    println(strings.run("self"));
}
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok, "compilation failed: {out}");
    assert_eq!(
        out,
        "5\n6\n7\n15\n18\n10\n3\n6\nfalse\nok\n12\n9\n4\n102\n6\n90\n91\n3\n7\n8\n9\n9\ngeneric\nself\n"
    );
}

#[test]
fn test_parenthesized_callable_field_snapshot_and_fn() {
    let source = r#"
class Holder { pub f: closure(i64) -> i64; }
class Function { pub f: fn(i64) -> i64; }
class Sink { pub f: closure(i64) -> void; }
fn increment(x: i64) -> i64 { return x + 1; }
fn replace(h: Holder) -> i64 {
    h.f = |x: i64| x + 100;
    return 2;
}
fn main() {
    let offset = 5;
    let h = new Holder(|x: i64| x + offset);
    // Capture the old callable before evaluating the mutating argument.
    println((h.f)(replace(h)));
    println((h.f)(2));
    let function = new Function(increment);
    println((function.f)(3));
    let sink = new Sink(|x: i64| { println(x); });
    (sink.f)(4);
}
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok, "compilation failed: {out}");
    assert_eq!(out, "7\n102\n4\n4\n");
}
