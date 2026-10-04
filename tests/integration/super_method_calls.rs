//! `super.method(args)` outside `init` (willow-jz15.48).
//!
//! A super call targets the implementation the CURRENT class's base resolves
//! the method to. The call is direct and never goes through `self`'s vtable,
//! because a vtable call would reach the caller's own override and recurse.
//! The receiver is still `self`, so any virtual call made inside the base body
//! still reaches the most-derived override.
//!
//! Perspectives (runtime; diagnostics live in the type checker's
//! `super_call_tests`):
//!   1. an override extends its base implementation
//!   2. three levels: every override chains to its own base
//!   3. a base that inherits without overriding resolves to the nearest definition
//!   4. a virtual call inside the super-called body reaches the most-derived class
//!   5. a base-typed receiver reaches the override, which then calls super once
//!   6. a non-overriding method may call a base method through super
//!   7. a super call passes value and reference arguments
//!   8. a super call is valid in `init` after `super.init`
//!   9. a super call runs inside `defer`
//!  10. a panic in the super-called body is recoverable by the caller
//!  11. `String`, `bool` and `void` returns pass through
//!  12. base-body field writes are visible through `self`
//!  13. a base class from an imported module
//!  14. an aliased imported base class
//!  15. a module-qualified base class
//!  16. an async override awaits its async base
//!  17. a local named `super` is an ordinary variable
//!  18. a super call in release mode gives the same answers
//!  19. the runnable example prints what it documents
//!  20. a free-function `super` call fails to compile with E0851

use super::support::{TestProject, compile_and_run_release, compile_with_env_and_run};

const PLAIN: [(&str, &str); 0] = [];

#[track_caller]
fn assert_output(source: &str, expected: &str) {
    let (out, ok) = compile_with_env_and_run(source, &PLAIN);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, expected, "wrong output");
}

#[track_caller]
fn assert_project_output(name: &str, files: &[(&str, &str)], expected: &str) {
    let project = TestProject::new(name, files);
    let compiled = project.compile_with_env("main.wi", &PLAIN);
    assert!(
        compiled.status.success(),
        "compile failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let run = project.run();
    assert!(run.status.success(), "binary failed");
    assert_eq!(String::from_utf8_lossy(&run.stdout), expected);
}

const THREE_LEVELS: &str = r#"
open class A {
    pub n: i64;
    pub open fn tag(self) -> String { return "A"; }
    pub open fn total(self) -> i64 { return self.n; }
}
open class B extends A {
    pub open override fn tag(self) -> String { return super.tag() + "B"; }
    pub open override fn total(self) -> i64 { return super.total() + 10; }
}
class C extends B {
    pub override fn tag(self) -> String { return super.tag() + "C"; }
    pub override fn total(self) -> i64 { return super.total() * 2; }
}
fn main() {
    let c = new C(5);
    println(c.tag());
    println(c.total());
    let b = new B(5);
    println(b.tag());
    println(b.total());
}
"#;

/// Perspective 1.
#[test]
fn super_01_an_override_extends_its_base() {
    assert_output(
        r#"
open class A {
    pub open fn name(self) -> String { return "A"; }
}
class B extends A {
    pub override fn name(self) -> String { return super.name() + "B"; }
}
fn main() { println(new B().name()); }
"#,
        "AB\n",
    );
}

/// Perspective 2. Each override chains to its own base, not to the root.
#[test]
fn super_02_three_levels_chain_through_each_base() {
    assert_output(THREE_LEVELS, "ABC\n30\nAB\n15\n");
}

/// Perspective 3. `Mid` does not define `value`, so `Leaf`'s super call
/// resolves to `Top::value`.
#[test]
fn super_03_an_inherited_base_method_resolves_to_the_nearest_definition() {
    assert_output(
        r#"
open class Top {
    pub open fn value(self) -> i64 { return 1; }
}
open class Mid extends Top {}
class Leaf extends Mid {
    pub override fn value(self) -> i64 { return super.value() + 100; }
}
fn main() { println(new Leaf().value()); }
"#,
        "101\n",
    );
}

/// Perspective 4. The receiver is `self`, so `self.unit()` inside the base
/// body dispatches virtually to the subclass.
#[test]
fn super_04_virtual_calls_inside_the_base_body_stay_virtual() {
    assert_output(
        r#"
open class Base {
    pub open fn unit(self) -> String { return "base"; }
    pub open fn show(self) -> String { return "[" + self.unit() + "]"; }
}
class Derived extends Base {
    pub override fn unit(self) -> String { return "derived"; }
    pub override fn show(self) -> String { return "D" + super.show(); }
}
fn main() { println(new Derived().show()); }
"#,
        "D[derived]\n",
    );
}

/// Perspective 5. If the super call went through the vtable, it would recurse
/// forever.
#[test]
fn super_05_a_base_typed_receiver_calls_super_once() {
    assert_output(
        r#"
open class Base {
    pub n: i64;
    pub open fn value(self) -> i64 { return self.n; }
}
class Derived extends Base {
    pub override fn value(self) -> i64 { return super.value() + 1; }
}
fn take(b: Base) -> i64 { return b.value(); }
fn main() {
    println(take(new Base(3)));
    println(take(new Derived(3)));
}
"#,
        "3\n4\n",
    );
}

/// Perspective 6. A super call also skips the override when the calling method
/// is not itself an override.
#[test]
fn super_06_a_non_overriding_method_reaches_the_base_implementation() {
    assert_output(
        r#"
open class A {
    pub open fn name(self) -> String { return "A"; }
}
class B extends A {
    pub override fn name(self) -> String { return "B"; }
    pub fn both(self) -> String { return self.name() + "|" + super.name(); }
}
fn main() { println(new B().both()); }
"#,
        "B|A\n",
    );
}

/// Perspective 7. The base writes through the forwarded reference.
#[test]
fn super_07_value_and_reference_arguments_pass_through() {
    assert_output(
        r#"
open class A {
    pub open fn add(self, a: i64, out: &mut i64) { out = out + a; }
}
class B extends A {
    pub override fn add(self, a: i64, out: &mut i64) {
        super.add(a * 2, &out);
        out = out * 10;
    }
}
fn main() {
    let mut x = 1;
    new B().add(3, &x);
    println(x);
}
"#,
        "70\n",
    );
}

/// Perspective 8.
#[test]
fn super_08_init_may_call_a_base_method_after_super_init() {
    assert_output(
        r#"
open class A {
    pub n: i64;
    pub init(self, n: i64) { self.n = n; }
    pub open fn log(self) { println("A " + format("{}", self.n)); }
}
class B extends A {
    pub init(self) {
        super.init(7);
        super.log();
    }
    pub override fn log(self) { println("B"); }
}
fn main() { let b = new B(); b.log(); }
"#,
        "A 7\nB\n",
    );
}

/// Perspective 9.
#[test]
fn super_09_a_super_call_runs_inside_defer() {
    assert_output(
        r#"
open class A {
    pub open fn done(self) { println("A done"); }
}
class B extends A {
    pub override fn done(self) {
        defer super.done();
        println("B done");
    }
}
fn main() { new B().done(); }
"#,
        "B done\nA done\n",
    );
}

/// Perspective 10. The super call contributes a may-panic edge, so the
/// caller's `recover()` frame is kept.
#[test]
fn super_10_a_panic_in_the_base_body_is_recoverable() {
    assert_output(
        r#"
open class A {
    pub open fn run(self) { panic("base failed"); }
}
class B extends A {
    pub override fn run(self) {
        defer match recover() {
            Some(info) => println("recovered:" + info.message),
            None => println("missing panic")
        }
        super.run();
        println("unreachable");
    }
}
fn main() { new B().run(); println("done"); }
"#,
        "recovered:base failed\ndone\n",
    );
}

/// Perspective 11.
#[test]
fn super_11_string_bool_and_void_returns_pass_through() {
    assert_output(
        r#"
open class A {
    pub open fn s(self) -> String { return "s"; }
    pub open fn ok(self) -> bool { return true; }
    pub open fn hi(self) { println("hi"); }
}
class B extends A {
    pub override fn s(self) -> String { return super.s() + "!"; }
    pub override fn ok(self) -> bool { return !super.ok(); }
    pub override fn hi(self) { super.hi(); println("there"); }
}
fn main() {
    let b = new B();
    println(b.s());
    println(b.ok());
    b.hi();
}
"#,
        "s!\nfalse\nhi\nthere\n",
    );
}

/// Perspective 12. The base body writes through the same `self`.
#[test]
fn super_12_base_field_writes_are_visible_through_self() {
    assert_output(
        r#"
open class A {
    pub n: i64;
    pub open fn bump(self) { self.n = self.n + 1; }
}
class B extends A {
    pub override fn bump(self) { super.bump(); super.bump(); self.n = self.n * 10; }
}
fn main() { let b = new B(1); b.bump(); println(b.n); }
"#,
        "30\n",
    );
}

const PARCEL_LIB: &str = r#"
pub open class Parcel {
    pub weight: i64;
    pub open fn cost(self) -> i64 { return self.weight * 10; }
}
"#;

/// Perspective 13.
#[test]
fn super_13_an_imported_base_class() {
    assert_project_output(
        "super_import",
        &[
            ("lib.wi", PARCEL_LIB),
            (
                "main.wi",
                r#"
import lib::Parcel;
class Express extends Parcel {
    pub override fn cost(self) -> i64 { return super.cost() + 1; }
}
fn main() { println(new Express(4).cost()); }
"#,
            ),
        ],
        "41\n",
    );
}

/// Perspective 14.
#[test]
fn super_14_an_aliased_imported_base_class() {
    assert_project_output(
        "super_alias",
        &[
            ("lib.wi", PARCEL_LIB),
            (
                "main.wi",
                r#"
import lib::Parcel as P;
class Express extends P {
    pub override fn cost(self) -> i64 { return super.cost() + 2; }
}
fn main() { println(new Express(4).cost()); }
"#,
            ),
        ],
        "42\n",
    );
}

/// Perspective 15.
#[test]
fn super_15_a_module_qualified_base_class() {
    assert_project_output(
        "super_qualified",
        &[
            ("lib.wi", PARCEL_LIB),
            (
                "main.wi",
                r#"
import lib;
class Express extends lib::Parcel {
    pub override fn cost(self) -> i64 { return super.cost() + 3; }
}
fn main() { println(new Express(4).cost()); }
"#,
            ),
        ],
        "43\n",
    );
}

/// Perspective 16. A super call to an async method yields a task, which the
/// override awaits. Running it as a task also works.
#[test]
fn super_16_an_async_override_awaits_its_base() {
    assert_output(
        r#"
open class A {
    pub v: i64;
    pub open async fn delayed(self, extra: i64) -> i64 { await sleep(1); return self.v + extra; }
}
class B extends A {
    pub override async fn delayed(self, extra: i64) -> i64 {
        let base = await super.delayed(extra);
        return base * 2;
    }
}
async fn main() {
    let b = new B(4);
    println(await b.delayed(1));
    let t = b.delayed(2);
    println(await t);
}
"#,
        "10\n12\n",
    );
}

/// Perspective 17. `super` is an identifier, not a keyword. A binding with
/// that name is a value and takes precedence over the super receiver.
#[test]
fn super_17_a_local_named_super_is_an_ordinary_variable() {
    assert_output(
        r#"
fn main() {
    let super = 3;
    println(super + 1);
}
"#,
        "4\n",
    );
}

/// Perspective 18.
#[test]
fn super_18_release_mode_matches() {
    let (out, ok) = compile_and_run_release(THREE_LEVELS);
    assert!(ok, "release run failed: {out}");
    assert_eq!(out, "ABC\n30\nAB\n15\n");
}

/// Perspective 19.
#[test]
fn super_19_the_example_prints_what_it_documents() {
    assert_output(
        include_str!("../../example/super_method_calls.wi"),
        "shipment 4kg costs 8\nshipment 4 kg (parcel) costs 13\nexpress shipment 4 kg (parcel) costs 39\n",
    );
}

/// Perspective 20. A misused `super` is a compile error, not a runtime
/// surprise.
#[test]
fn super_20_a_free_function_super_call_is_rejected() {
    let project = TestProject::new(
        "super_free_fn",
        &[("main.wi", "fn main() { super.run(); }")],
    );
    let compiled = project.compile_with_env("main.wi", &PLAIN);
    let stderr = String::from_utf8_lossy(&compiled.stderr);
    assert!(!compiled.status.success(), "must not compile");
    assert!(stderr.contains("E0851"), "{stderr}");
}
