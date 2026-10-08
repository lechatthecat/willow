//! Expression-form if/else (willow-jz15.5). Cases exercise the shared
//! ternary checker/lowering, including contextual types and lazy execution.
use crate::support::*;

const VALUES: &str = r#"
import std::collections::Array;
enum Choice { A, B }
interface Named { fn name(self) -> String; }
class A implements Named { pub fn name(self) -> String { return "A"; } }
class B implements Named { pub fn name(self) -> String { return "B"; } }
class Box { pub value: i64; }
fn identity(n: i64) -> i64 { return n; }
fn mark(n: i64) -> bool { println(n); return n > 0; }
fn value(n: i64) -> i64 { println(n); return n; }
fn choose(n: i64) -> i64 { return if n < 0 { -1 } else if n == 0 { 0 } else { 1 }; }
fn named(b: bool) -> Named { return if b { new A() } else { new B() }; }
fn main() {
    // 1/2: inferred integer, true and false branches.
    println(if true { 1 } else { 2 });
    println(if false { 1 } else { 2 });
    // 3: f64; 4: bool; 5: String and postfix method.
    println(if false { 1.0 } else { 2.5 });
    println(if true { false } else { true });
    println((if true { "yes" } else { "no" }).len());
    // 6: return and 7: all paths through else-if.
    println(choose(-8)); println(choose(0)); println(choose(8));
    // 8: call argument and 9: arithmetic precedence.
    println(identity(if true { 3 } else { 4 }) + 10);
    println(2 * if false { 8 } else { 3 } + 1);
    // 10: nested branches and 11: conditional in condition.
    println(if true { if false { 1 } else { 2 } } else { 3 });
    println(if if false { false } else { true } { 4 } else { 5 });
    // 12: interoperability with ternary and 13: match.
    println(if true { false ? 1 : 2 } else { 3 });
    println(match true { true => if false { 1 } else { 2 }, false => 3 });
    // 14: local assignment and 15: array element/index.
    let mut n = 0; n = if true { 5 } else { 6 }; println(n);
    let xs: Array<i64> = [if true { 7 } else { 8 }, 9];
    println(xs[if false { 1 } else { 0 }]);
    // 16: constructor and field access.
    println((if true { new Box(11) } else { new Box(12) }).value);
    // 17: contextual interface unification, both branches.
    println(named(true).name()); println(named(false).name());
    // 18: enum values and 19: generic enum sibling unification.
    let c: Choice = if true { Choice::A } else { Choice::B };
    println(match c { Choice::A => 1, Choice::B => 2 });
    let opt = if true { Option::Some(12) } else { Option::None };
    println(match opt { Some(v) => v, None => 0 });
    // 20: contextual lambda values and 21: capture.
    let f: fn(i64) -> i64 = if false { |x| x + 1 } else { |x| x + 2 };
    println(f(10));
    let captured = 20;
    let g: closure(i64) -> i64 = |x: i64| if x > 0 { captured + x } else { captured };
    println(g(3));
    // 22: condition once; 23: unselected branch not evaluated.
    println(if mark(1) { value(31) } else { value(32) });
    println(if mark(0) { value(33) } else if mark(2) { value(34) } else { value(35) });
    // 24: statement-form if remains valid without else.
    if true { println(99); }
}
"#;

const EXPECTED: &str = "1\n2\n2.5\nfalse\n3\n-1\n0\n1\n13\n7\n2\n4\n2\n2\n5\n7\n11\nA\nB\n1\n12\n12\n23\n1\n31\n31\n0\n2\n34\n34\n99\n";

#[test]
fn if_expression_values_debug() {
    let (out, ok) = compile_and_run(VALUES);
    assert!(ok);
    assert_eq!(out, EXPECTED);
}

#[test]
fn if_expression_values_release() {
    let (out, ok) = compile_and_run_release(VALUES);
    assert!(ok);
    assert_eq!(out, EXPECTED);
}

#[test]
fn if_expression_values_gc_stress() {
    let (out, ok) = compile_and_run_with_env(VALUES, &[("WILLOW_GC_STRESS", "minor")]);
    assert!(ok);
    assert_eq!(out, EXPECTED);
}

#[test]
fn if_expression_rejects_invalid_forms() {
    for (expression, diagnostic) in [
        ("if true { 1 }", "requires an `else`"),
        ("if 1 { 1 } else { 2 }", "condition must be `bool`"),
        ("if true { 1 } else { false }", "incompatible types"),
        ("if true { 1.0 } else { 2 }", "incompatible types"),
        ("if true { 1 } else if false { 2 }", "requires an `else`"),
        ("if true { } else { 2 }", "expected expression"),
        ("if true { 1 } else { missing }", "missing"),
        ("if true { 1; } else { 2 }", "expected"),
    ] {
        let source = format!("fn main() {{ let x = {expression}; }}");
        assert_compile_error_contains(&source, &[diagnostic]);
    }
}

#[test]
fn if_expression_example() {
    let (out, ok) = compile_and_run(include_str!("../../example/if_expressions.wi"));
    assert!(ok);
    assert_eq!(out, "negative\nzero\npositive\n42\n");
}

#[test]
fn if_expression_await_short_circuits() {
    let source = r#"
async fn flag(n: i64) -> bool { println(n); return n > 0; }
async fn value(n: i64) -> String { println(n); return "value"; }
async fn main() {
    let x = if await flag(1) { await value(2) } else { await value(3) };
    println(x);
    let y = if await flag(0) { await value(4) } else if await flag(5) { await value(6) } else { await value(7) };
    println(y);
}
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok);
    assert_eq!(out, "1\n2\nvalue\n0\n5\n6\nvalue\n");
    let (out, ok) = compile_and_run_release(source);
    assert!(ok);
    assert_eq!(out, "1\n2\nvalue\n0\n5\n6\nvalue\n");
}
