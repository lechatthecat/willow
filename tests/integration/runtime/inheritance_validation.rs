use super::*;

// ── Interface inheritance validation (willow-1js.8) ──────────────────────────

#[test]
fn iface_inherit_neg_01_cycle_rejected() {
    assert!(expect_compile_error(
        r#"
interface A extends B { fn a(self) -> i64; }
interface B extends A { fn b(self) -> i64; }
fn main() {}
"#,
    ));
}

#[test]
fn iface_inherit_02_multiple_supers_allowed() {
    let (out, ok) = compile_and_run(
        r#"
interface A { fn a(self) -> i64; }
interface B { fn b(self) -> i64; }
interface C extends A, B { fn c(self) -> i64; }
class Impl implements C {
    pub fn a(self) -> i64 { return 10; }
    pub fn b(self) -> i64 { return 20; }
    pub fn c(self) -> i64 { return 30; }
}
fn main() {
    let c: C = new Impl();
    println(c.a() + c.b() + c.c());
}
"#,
    );
    assert!(ok, "multiple super-interface inheritance should compile");
    assert_eq!(out, "60\n");
}

#[test]
fn iface_inherit_neg_03_extends_class_rejected() {
    assert!(expect_compile_error(
        r#"
class Foo { pub fn f(self) -> i64 { return 1; } }
interface Bad extends Foo { fn g(self) -> i64; }
fn main() {}
"#,
    ));
}

#[test]
fn iface_inherit_neg_04_extends_unknown_rejected() {
    assert!(expect_compile_error(
        r#"
interface Bad extends Nope { fn g(self) -> i64; }
fn main() {}
"#,
    ));
}

#[test]
fn downcast_05_generic_interface_scrutinee() {
    // Downcast works when the scrutinee is a generic interface instantiation
    // (willow-1js.9).
    let (out, ok) = compile_and_run(
        r#"
interface Box<T> { fn get(self) -> T; }
class IntBox implements Box<i64> {
    pub fn get(self) -> i64 { return 7; }
    pub fn extra(self) -> i64 { return 99; }
}
class OtherBox implements Box<i64> { pub fn get(self) -> i64 { return 1; } }
fn probe(b: Box<i64>) -> i64 {
    return match b {
        IntBox(x) => x.extra(),
        _ => b.get(),
    };
}
fn main() {
    println(probe(new IntBox()));
    println(probe(new OtherBox()));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "99\n1\n");
}

// ── Subclass usable as a base-declared interface (willow-2s4i) ───────────────

#[test]
fn subclass_iface_01_used_as_base_interface() {
    // Puppy extends Dog (which implements Animal); a Puppy is an Animal even
    // though Puppy does not re-declare `implements Animal`.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
interface Animal { fn name(self) -> String; }
open class Dog implements Animal { pub open fn name(self) -> String { return "dog"; } }
class Puppy extends Dog { pub override fn name(self) -> String { return "puppy"; } }
fn describe(a: Animal) { println(a.name()); }
fn main() {
    describe(new Dog());
    describe(new Puppy());
}
"#,
    );
    assert!(ok, "subclass must be usable as the base's interface: {out}");
    assert_eq!(out, "dog\npuppy\n");
}

#[test]
fn subclass_iface_02_inherits_method_no_override() {
    // The subclass inherits the base's interface method (no override).
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn legs(self) -> i64; }
open class Dog implements Animal { pub fn legs(self) -> i64 { return 4; } }
class Puppy extends Dog {}
fn count(a: Animal) -> i64 { return a.legs(); }
fn main() { println(count(new Puppy())); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

#[test]
fn subclass_iface_03_two_levels() {
    // Grandchild is usable as the interface declared two levels up.
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn name(self) -> String; }
open class Dog implements Animal { pub open fn name(self) -> String { return "dog"; } }
open class Puppy extends Dog { pub open override fn name(self) -> String { return "puppy"; } }
class Teacup extends Puppy { pub override fn name(self) -> String { return "teacup"; } }
fn describe(a: Animal) { println(a.name()); }
fn main() { describe(new Teacup()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "teacup\n");
}
