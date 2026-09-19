//! Runtime proof that Never coercion does not fabricate a value.
use super::support::{assert_compile_error_contains, compile_and_run};

#[test]
fn never_values_reachable_branches() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
class C {
    pub x: i64;
    pub init(self, flag: bool) { self.x = flag ? 42 : panic("stop"); }
}
fn take(x: i64) -> i64 { return x; }
fn value(flag: bool) -> f64 { return flag ? panic("stop") : 2.5; }
fn main() {
    println(new C(true).x);
    let mut x: i64 = 0;
    x = false ? panic("stop") : 7;
    println(x);
    let mut a: Array<i64> = [0];
    a[0] = true ? 9 : panic("stop");
    println(a[0]);
    println(take(false ? panic("stop") : 11));
    println(value(false));
    println(false ? panic("stop") : "alive");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n7\n9\n11\n2.5\nalive\n");
}

#[test]
fn never_values_recovered_rhs_does_not_store_or_call() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
fn take(x: i64) { println("wrong"); }
fn value() -> i64 { return panic("stop"); }
fn main() {
    let mut x: i64 = 7;
    let mut a: Array<i64> = [9];
    if true { defer match recover() { Some(_) => {}, None => {} }; x = panic("stop"); }
    if true { defer match recover() { Some(_) => {}, None => {} }; a[0] = panic("stop"); }
    if true { defer match recover() { Some(_) => {}, None => {} }; take(panic("stop")); }
    if true { defer match recover() { Some(_) => {}, None => {} }; x = value(); }
    if true { defer match recover() { Some(_) => {}, None => {} }; x = true ? panic("a") : panic("b"); }
    if true { defer match recover() { Some(_) => {}, None => {} }; a[0] = false ? panic("a") : panic("b"); }
    if true { defer match recover() { Some(_) => {}, None => {} }; println(panic("stop")); }
    if true { defer match recover() { Some(_) => {}, None => {} }; take(true ? panic("a") : panic("b")); }
    println(x);
    println(a[0]);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n9\n");
}

#[test]
fn never_values_recovered_constructor_is_rejected() {
    assert_compile_error_contains(
        r#"
class C { x: i64; init(self) {
    defer match recover() { Some(_) => {}, None => {} };
    self.x = panic("stop");
} } fn main() {}
"#,
        &["error[E0842]", "field `x` is not initialized"],
    );
}

#[test]
fn never_values_arguments_preserve_order_and_skip_suffix() {
    let (out, ok) = compile_and_run(
        r#"
fn first() -> i64 { println("first"); return 1; }
fn last() -> i64 { println("wrong suffix"); return 2; }
fn take(a: i64, b: i64, c: i64) { println("wrong call"); }
fn main() {
    if true {
        defer match recover() { Some(_) => {}, None => {} };
        take(first(), panic("stop"), last());
    }
    println("resumed");
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "first\nresumed\n");
}

#[test]
fn never_values_async_recovery_preserves_field() {
    let (out, ok) = compile_and_run(
        r#"
class C { pub x: i64; }
async fn main() {
    let c = new C(42);
    if true {
        defer match recover() { Some(_) => {}, None => {} };
        c.x = false ? panic("first") : panic("second");
    }
    await yield();
    println(c.x);
    let x: f64 = false ? panic("stop") : 2.5;
    println(x);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n2.5\n");
}

#[test]
fn never_values_recovered_reference_preparation() {
    let (out, ok) = compile_and_run(
        r#"
fn take(x: &i64, y: i64) { println(y); }
fn main() {
    let x = 42;
    if true {
        defer match recover() { Some(_) => {}, None => {} };
        take(&x, panic("stop"));
    }
    take(&x, 0);
    println(x);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n42\n");
}

#[test]
fn never_values_recovered_method_preparation() {
    let (out, ok) = compile_and_run(
        r#"
class C { pub fn take(self, x: i64) { println(x); } }
fn main() {
    let c = new C();
    if true {
        defer match recover() { Some(_) => {}, None => {} };
        c.take(panic("stop"));
    }
    c.take(42);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn never_values_nested_preparations_recover_sync_and_async() {
    for mode in ["", "async "] {
        let source = format!(
            r#"
class C {{ pub fn take(self, x: &i64, y: i64) -> i64 {{ println("called"); return y; }} }}
{mode}fn main() {{
    let c = new C(); let x = 42;
    if true {{
        defer match recover() {{ Some(_) => {{}}, None => {{}} }};
        c.take(&x, c.take(&x, panic("stop")));
    }}
    if true {{
        defer match recover() {{ Some(_) => {{}}, None => {{}} }};
        c.take(&x, true ? panic("first") : panic("second"));
    }}
    println(c.take(&x, 42));
}}
"#
        );
        let (out, ok) = compile_and_run(&source);
        assert!(ok, "{out}");
        assert_eq!(out, "called\n42\n");
    }
}
