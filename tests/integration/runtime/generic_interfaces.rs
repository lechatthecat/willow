use super::*;

// ── Generic interface declarations (willow-1js.1, slice 1) ───────────────────

#[test]
fn generic_interface_01_declaration_compiles() {
    // A generic interface declares with type params; method sigs may reference
    // them. (Implementing/dispatch on generic interfaces is a later slice.)
    let (out, ok) = compile_and_run(
        r#"
interface Box<T> {
    fn get(self) -> T;
}
interface Conv<A, B> {
    fn run(self, a: A) -> B;
}
fn main() { println(7); }
"#,
    );
    assert!(ok, "generic interface declarations must type-check: {out}");
    assert_eq!(out, "7\n");
}

// ── Generic interfaces: implement, dispatch, conformance (willow-1js.1) ──────

#[test]
fn generic_interface_02_implement_and_dispatch_i64() {
    let (out, ok) = compile_and_run(
        r#"
interface Box<T> { fn get(self) -> T; }
class IntBox implements Box<i64> {
    pub init(self, n: i64) {
        self.n = n;
    }
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn main() {
    let b: Box<i64> = new IntBox(99);
    println(b.get());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "99\n");
}

#[test]
fn generic_interface_03_implement_and_dispatch_string() {
    let (out, ok) = compile_and_run(
        r#"
interface Box<T> { fn get(self) -> T; }
class TextBox implements Box<String> {
    pub init(self, s: String) {
        self.s = s;
    }
    s: String;
    pub fn get(self) -> String { return self.s; }
}
fn main() {
    let b: Box<String> = new TextBox("hi");
    println(b.get());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "hi\n");
}

#[test]
fn generic_interface_04_two_type_params() {
    let (out, ok) = compile_and_run(
        r#"
interface Pair<A, B> { fn first(self) -> A; fn second(self) -> B; }
class P implements Pair<i64, String> {
    pub init(self, a: i64, b: String) {
        self.a = a;
        self.b = b;
    }
    a: i64;
    b: String;
    pub fn first(self) -> i64 { return self.a; }
    pub fn second(self) -> String { return self.b; }
}
fn main() {
    let p: Pair<i64, String> = new P(7, "x");
    println(p.first());
    println(p.second());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\nx\n");
}

#[test]
fn generic_interface_05_param_typed_by_type_param() {
    // Method takes an argument whose type is the type parameter.
    let (out, ok) = compile_and_run(
        r#"
interface Sink<T> { fn put(self, v: T) -> T; }
class IntSink implements Sink<i64> {
    pub fn put(self, v: i64) -> i64 { return v + 1; }
}
fn main() {
    let s: Sink<i64> = new IntSink();
    println(s.put(41));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn generic_interface_06_self_return_on_concrete() {
    let (out, ok) = compile_and_run(
        r#"
interface From<E> { fn from(self, e: E) -> Self; }
class W implements From<i64> {
    pub init(self, n: i64) {
        self.n = n;
    }
    n: i64;
    pub fn from(self, e: i64) -> W { return new W(e); }
    pub fn val(self) -> i64 { return self.n; }
}
fn main() {
    let w = new W(0);
    println(w.from(42).val());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}

#[test]
fn generic_interface_07_passed_to_function() {
    let (out, ok) = compile_and_run(
        r#"
interface Box<T> { fn get(self) -> T; }
class IntBox implements Box<i64> {
    pub init(self, n: i64) {
        self.n = n;
    }
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn show(b: Box<i64>) { println(b.get()); }
fn main() { show(new IntBox(5)); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n");
}

#[test]
fn generic_interface_08_two_instantiations_distinct() {
    let (out, ok) = compile_and_run(
        r#"
interface Box<T> { fn get(self) -> T; }
class IntBox implements Box<i64> {
    pub init(self, n: i64) {
        self.n = n;
    }
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
class TextBox implements Box<String> {
    pub init(self, s: String) {
        self.s = s;
    }
    s: String;
    pub fn get(self) -> String { return self.s; }
}
fn main() {
    let a: Box<i64> = new IntBox(1);
    let b: Box<String> = new TextBox("two");
    println(a.get());
    println(b.get());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\ntwo\n");
}

// Negative perspectives ------------------------------------------------------

#[test]
fn generic_interface_neg_01_too_few_type_args() {
    // `Box` requires one type argument (E0422).
    assert!(expect_compile_error(
        r#"
interface Box<T> { fn get(self) -> T; }
class C implements Box { pub fn get(self) -> i64 { return 1; } }
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_neg_02_too_many_type_args() {
    assert!(expect_compile_error(
        r#"
interface Box<T> { fn get(self) -> T; }
class C implements Box<i64, i64> { pub fn get(self) -> i64 { return 1; } }
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_neg_03_return_type_mismatch_after_subst() {
    // With T=String, `get` must return String; returning i64 is a mismatch.
    assert!(expect_compile_error(
        r#"
interface Box<T> { fn get(self) -> T; }
class C implements Box<String> { pub fn get(self) -> i64 { return 1; } }
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_neg_04_param_type_mismatch_after_subst() {
    assert!(expect_compile_error(
        r#"
interface Sink<T> { fn put(self, v: T) -> T; }
class C implements Sink<i64> { pub fn put(self, v: String) -> i64 { return 1; } }
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_neg_05_missing_method() {
    assert!(expect_compile_error(
        r#"
interface Box<T> { fn get(self) -> T; }
class C implements Box<i64> {}
fn main() {}
"#,
    ));
}

#[test]
fn generic_interface_neg_06_unknown_method_on_iface_value() {
    assert!(expect_compile_error(
        r#"
interface Box<T> { fn get(self) -> T; }
class IntBox implements Box<i64> {
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn main() {
    let b: Box<i64> = new IntBox(1);
    b.missing();
}
"#,
    ));
}

#[test]
fn generic_interface_neg_07_wrong_instantiation_not_assignable() {
    // An IntBox implements Box<i64>, not Box<String>.
    assert!(expect_compile_error(
        r#"
interface Box<T> { fn get(self) -> T; }
class IntBox implements Box<i64> {
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn main() {
    let b: Box<String> = new IntBox(1);
}
"#,
    ));
}
