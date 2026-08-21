//! Class-method dispatch chain filtering (willow-au5k).
//!
//! A class-method call whose receiver's runtime type is not known statically
//! compiles to a chain of `type_id` comparisons. The chain used to include
//! EVERY class in the program that declared a method with the matching NAME,
//! even one from a completely unrelated hierarchy whose `type_id` the receiver
//! could never carry. That cost real code: 32 unrelated same-named classes over
//! 256 call sites added ~200 KB of `.text`, and it hid the single-candidate
//! fast path behind a chain that was never going to take any branch but one.
//!
//! The fix restricts candidates to the receiver's own static class and that
//! class's descendants. This module pins the behavior that restriction must not
//! change, plus the code-size win that motivated it. The pure ancestry
//! predicate is unit-tested next to its definition in
//! `src/backend/cranelift/emit_interface.rs`; those are perspectives 1-12.
//!
//! Perspectives covered here (13-27):
//!  13. an unrelated same-named class does not steal a monomorphic call
//!  14. a base-typed reference selects the subclass override
//!  15. a subclass that does not override inherits the base implementation
//!  16. a three-level hierarchy honors an override in the middle
//!  17. a sibling branch is never selected
//!  18. `self.method()` inside a base method still dispatches virtually
//!  19. an interface-typed receiver is unaffected (vtable path, not the chain)
//!  20. classes from an imported module still dispatch
//!  21. argument side effects and evaluation order survive the chain
//!  22. an array of base-typed elements dispatches per element
//!  23. an eight-level hierarchy dispatches to the nearest definition
//!  24. an unrelated same-named method with a DIFFERENT return type is excluded
//!  25. the same call site still panics with the right method frame
//!  26. a static call is not routed through the chain
//!  27. 32 unrelated same-named classes cost little code across 256 call sites
//!  28. the runnable example prints what it documents
//!  29. an aliased imported base class still dispatches to its subclasses
//!
//! Every behavioral perspective runs on BOTH backends and asserts they agree:
//! the chain lives in the AST emitter, and the LIR walker emits a direct call
//! for the class shapes it accepts, so a filter bug would show up as a
//! disagreement even where a single-backend test would pass.

use super::support::{TestProject, compile_with_env_and_run, compile_with_env_and_run_combined};

/// The LIR path is on by default; the "on" side names it explicitly so this
/// never degrades into comparing the AST path with itself. `WILLOW_LIR_REQUIRE`
/// is deliberately NOT set: an inherited/virtual class method is outside the
/// walker's subset, so these programs are expected to mix paths.
const LIR_ON: [(&str, &str); 1] = [("WILLOW_LIR_BACKEND", "1")];
const LIR_OFF: [(&str, &str); 1] = [("WILLOW_LIR_BACKEND", "0")];

#[track_caller]
fn assert_both_backends(source: &str, expected: &str) {
    let (with_lir, ok_on) = compile_with_env_and_run(source, &LIR_ON);
    assert!(ok_on, "LIR-enabled run failed: {with_lir}");
    let (without_lir, ok_off) = compile_with_env_and_run(source, &LIR_OFF);
    assert!(ok_off, "LIR-disabled run failed: {without_lir}");
    assert_eq!(
        with_lir, without_lir,
        "the two backends disagreed about dispatch"
    );
    assert_eq!(with_lir, expected);
}

/// Perspective 13. `Hot` and `Cold` are unrelated classes that both declare
/// `tick`. A `Hot` receiver must call `Hot::tick` — before the filter, `Cold`
/// was a candidate at this call site and the emitter built a chain for it.
#[test]
fn dispatch_13_an_unrelated_same_named_class_is_not_a_candidate() {
    assert_both_backends(
        r#"
class Hot {
    pub n: i64;
    pub fn tick(self) -> i64 { return self.n + 1; }
}
class Cold {
    pub n: i64;
    pub fn tick(self) -> i64 { return self.n + 1000; }
}
fn main() {
    let h = new Hot(1);
    let c = new Cold(1);
    println(h.tick());
    println(c.tick());
}
"#,
        "2\n1001\n",
    );
}

/// Perspective 14. The receiver's static class is `Base`, its runtime class is
/// `Sub`: the override must win. This is the case the chain exists for, so the
/// filter must keep `Sub` in it.
#[test]
fn dispatch_14_a_base_reference_selects_the_override() {
    assert_both_backends(
        r#"
open class Base {
    pub n: i64;
    pub open fn label(self) -> i64 { return 1; }
}
class Sub extends Base {
    pub override fn label(self) -> i64 { return 2; }
}
class Unrelated {
    pub n: i64;
    pub fn label(self) -> i64 { return 999; }
}
fn main() {
    let a: Base = new Base(0);
    let b: Base = new Sub(0);
    println(a.label());
    println(b.label());
}
"#,
        "1\n2\n",
    );
}

/// Perspective 15. A subclass with no override still has its own `type_id`, so
/// it must stay in the chain and resolve to the INHERITED implementation.
#[test]
fn dispatch_15_a_non_overriding_subclass_inherits() {
    assert_both_backends(
        r#"
open class Base {
    pub n: i64;
    pub open fn label(self) -> i64 { return 7; }
}
class Quiet extends Base {}
class Unrelated {
    pub n: i64;
    pub fn label(self) -> i64 { return 999; }
}
fn main() {
    let q: Base = new Quiet(0);
    println(q.label());
}
"#,
        "7\n",
    );
}

/// Perspective 16. Three levels, override in the middle: a `Leaf` receiver
/// resolves to `Middle`'s definition, not `Base`'s.
#[test]
fn dispatch_16_a_middle_override_wins_over_the_root() {
    assert_both_backends(
        r#"
import std::collections::Array;
open class Base {
    pub n: i64;
    pub open fn label(self) -> i64 { return 1; }
}
open class Middle extends Base {
    pub open override fn label(self) -> i64 { return 2; }
}
class Leaf extends Middle {}
fn main() {
    let values: Array<Base> = [new Base(0), new Middle(0), new Leaf(0)];
    let mut i = 0;
    while i < values.len() {
        println(values[i].label());
        i = i + 1;
    }
}
"#,
        "1\n2\n2\n",
    );
}

/// Perspective 17. Both branches descend from `Base`, so both are candidates
/// for a `Base` receiver — but a receiver typed as one branch must never be
/// offered the other's implementation.
#[test]
fn dispatch_17_a_sibling_branch_is_never_selected() {
    assert_both_backends(
        r#"
open class Base {
    pub n: i64;
    pub open fn label(self) -> i64 { return 0; }
}
open class Left extends Base {
    pub open override fn label(self) -> i64 { return 10; }
}
open class Right extends Base {
    pub open override fn label(self) -> i64 { return 20; }
}
class LeftLeaf extends Left {}
fn main() {
    let l: Left = new LeftLeaf(0);
    println(l.label());
    let b: Base = new Right(0);
    println(b.label());
}
"#,
        "10\n20\n",
    );
}

/// Perspective 18. An inherited method calling `self.other()` dispatches on the
/// runtime type. The self-call's receiver is typed as the DEFINING class, so
/// the filter is applied with `Base` as the static class there while the object
/// is really a `Sub`.
#[test]
fn dispatch_18_a_self_call_inside_a_base_method_stays_virtual() {
    assert_both_backends(
        r#"
open class Base {
    pub n: i64;
    pub open fn value(self) -> i64 { return 1; }
    pub fn report(self) -> i64 { return self.value() * 10; }
}
class Sub extends Base {
    pub override fn value(self) -> i64 { return 5; }
}
class Unrelated {
    pub n: i64;
    pub fn value(self) -> i64 { return 999; }
}
fn main() {
    let s: Base = new Sub(0);
    println(s.report());
}
"#,
        "50\n",
    );
}

/// Perspective 19. An interface receiver dispatches through a vtable, a path
/// the chain filter never touches. Implementors from unrelated hierarchies are
/// exactly what an interface is for, so this must keep working.
#[test]
fn dispatch_19_an_interface_receiver_is_unaffected() {
    assert_both_backends(
        r#"
import std::collections::Array;
interface Speaker {
    fn speak(self) -> i64;
}
class Dog implements Speaker {
    pub n: i64;
    pub fn speak(self) -> i64 { return 1; }
}
class Robot implements Speaker {
    pub n: i64;
    pub fn speak(self) -> i64 { return 2; }
}
fn main() {
    let voices: Array<Speaker> = [new Dog(0), new Robot(0)];
    let mut i = 0;
    while i < voices.len() {
        println(voices[i].speak());
        i = i + 1;
    }
}
"#,
        "1\n2\n",
    );
}

/// Perspective 20. Classes declared in an imported module are keyed by their
/// module-qualified names. The filter only applies when the receiver's own name
/// is a key of the same table, so a mismatch in spelling here would either
/// drop every candidate or keep them all; both are observable.
#[test]
fn dispatch_20_classes_from_an_imported_module_dispatch() {
    let project = TestProject::new(
        "dispatch_module",
        &[
            (
                "shapes.wi",
                r#"
pub open class Shape {
    pub n: i64;
    pub open fn area(self) -> i64 { return 0; }
}

pub class Square extends Shape {
    pub override fn area(self) -> i64 { return self.n * self.n; }
}
"#,
            ),
            (
                "main.wi",
                r#"
import shapes;

class Local {
    pub n: i64;
    pub fn area(self) -> i64 { return -1; }
}

fn main() {
    let s: shapes::Shape = new shapes::Square(4);
    println(s.area());
    let l = new Local(0);
    println(l.area());
}
"#,
            ),
        ],
    );

    for env in [LIR_ON.as_slice(), LIR_OFF.as_slice()] {
        let compiled = project.compile_with_env("main.wi", env);
        assert!(
            compiled.status.success(),
            "compile failed under {env:?}: {}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let run = project.run();
        assert!(run.status.success(), "binary failed under {env:?}");
        assert_eq!(String::from_utf8_lossy(&run.stdout), "16\n-1\n");
    }
}

/// Perspective 21. Arguments are evaluated once, before the chain branches, and
/// in source order. A filter change that re-emitted argument code per candidate
/// would show up here as repeated side effects.
#[test]
fn dispatch_21_argument_side_effects_happen_once_in_order() {
    assert_both_backends(
        r#"
open class Base {
    pub n: i64;
    pub open fn combine(self, a: i64, b: i64) -> i64 { return a - b; }
}
class Sub extends Base {
    pub override fn combine(self, a: i64, b: i64) -> i64 { return a * 100 + b; }
}
class Unrelated {
    pub n: i64;
    pub fn combine(self, a: i64, b: i64) -> i64 { return 0; }
}
fn note(tag: i64) -> i64 {
    println(tag);
    return tag;
}
fn main() {
    let s: Base = new Sub(0);
    println(s.combine(note(1), note(2)));
}
"#,
        "1\n2\n102\n",
    );
}

/// Perspective 22. One call site, three runtime types: the chain really is
/// exercised, and every branch of it is reachable.
#[test]
fn dispatch_22_one_call_site_serves_every_runtime_type() {
    assert_both_backends(
        r#"
open class Animal {
    pub n: i64;
    pub open fn legs(self) -> i64 { return 0; }
}
class Bird extends Animal {
    pub override fn legs(self) -> i64 { return 2; }
}
class Cat extends Animal {
    pub override fn legs(self) -> i64 { return 4; }
}
class Snake extends Animal {}
class Table {
    pub n: i64;
    pub fn legs(self) -> i64 { return 400; }
}
fn count(a: Animal) -> i64 { return a.legs(); }
fn main() {
    println(count(new Bird(0)));
    println(count(new Cat(0)));
    println(count(new Snake(0)));
    println(new Table(0).legs());
}
"#,
        "2\n4\n0\n400\n",
    );
}

/// Perspective 23. Depth is unbounded in the language, so the nearest-ancestor
/// resolution must not be depth-limited. Overrides sit at levels 0, 2, 4 and 6;
/// the receivers at 1, 3, 5 and 7 must each find the one just above them.
#[test]
fn dispatch_23_a_deep_hierarchy_resolves_to_the_nearest_definition() {
    assert_both_backends(
        r#"
import std::collections::Array;
open class L0 {
    pub n: i64;
    pub open fn tag(self) -> i64 { return 0; }
}
open class L1 extends L0 {}
open class L2 extends L1 {
    pub open override fn tag(self) -> i64 { return 2; }
}
open class L3 extends L2 {}
open class L4 extends L3 {
    pub open override fn tag(self) -> i64 { return 4; }
}
open class L5 extends L4 {}
open class L6 extends L5 {
    pub open override fn tag(self) -> i64 { return 6; }
}
class L7 extends L6 {}
fn main() {
    let all: Array<L0> = [new L1(0), new L3(0), new L5(0), new L7(0)];
    let mut i = 0;
    while i < all.len() {
        println(all[i].tag());
        i = i + 1;
    }
}
"#,
        "0\n2\n4\n6\n",
    );
}

/// Perspective 24. An unrelated class whose same-named method returns a
/// DIFFERENT type used to join the chain, where the call site's return type came
/// from the receiver's class. Excluding it is what makes that mismatch
/// unreachable rather than merely unlikely.
#[test]
fn dispatch_24_an_unrelated_same_named_method_may_differ_in_return_type() {
    assert_both_backends(
        r#"
open class Base {
    pub n: i64;
    pub open fn describe(self) -> i64 { return 1; }
}
class Sub extends Base {
    pub override fn describe(self) -> i64 { return 2; }
}
class Label {
    pub n: i64;
    pub fn describe(self) -> String { return "label"; }
}
fn main() {
    let b: Base = new Sub(0);
    println(b.describe());
    println(new Label(0).describe());
}
"#,
        "2\nlabel\n",
    );
}

/// Perspective 25. The method frame the call site installs is the call's own,
/// not a candidate's. A panic from inside the selected implementation must
/// still name the method and point at the call site.
#[test]
fn dispatch_25_a_panic_from_a_dispatched_method_keeps_its_frame() {
    let source = r#"
open class Base {
    pub n: i64;
    pub open fn tick(self) -> i64 { return self.n; }
}
class Boom extends Base {
    pub override fn tick(self) -> i64 { panic("boom"); }
}
class Unrelated {
    pub n: i64;
    pub fn tick(self) -> i64 { return 0; }
}
fn main() {
    let b: Base = new Boom(1);
    println(b.tick());
}
"#;
    for env in [LIR_ON.as_slice(), LIR_OFF.as_slice()] {
        let (output, ok) = compile_with_env_and_run_combined(source, env);
        assert!(!ok, "the program must abort under {env:?}: {output}");
        assert!(
            output.contains("runtime panic: boom"),
            "missing panic message under {env:?}: {output}"
        );
        assert!(
            output.contains("0: tick at"),
            "missing method frame under {env:?}: {output}"
        );
    }
}

/// Perspective 26. A static call names its class outright, so it never builds a
/// chain at all — and must not start doing so because an unrelated class shares
/// the name.
#[test]
fn dispatch_26_a_static_call_is_not_routed_through_the_chain() {
    assert_both_backends(
        r#"
class Factory {
    pub n: i64;
    pub static fn make() -> i64 { return 3; }
}
class OtherFactory {
    pub n: i64;
    pub static fn make() -> i64 { return 99; }
}
fn main() {
    println(Factory::make());
    println(OtherFactory::make());
}
"#,
        "3\n99\n",
    );
}

/// Perspective 27. The measurement that motivated the filter, as a regression
/// test: 32 unrelated classes that merely SHARE the method name, against 256
/// monomorphic call sites.
///
/// Both programs are built by the same compiler and differ only by those 32
/// class declarations, so their size difference is the cost the extra classes
/// impose. Unfiltered, each call site emitted a 33-way `type_id` chain and the
/// difference measured ~200 KB; filtered, each is a direct call and the
/// difference is the 32 method bodies themselves, ~10 KB. The bound sits well
/// clear of both, so it fails on a regression without tracking codegen noise.
///
/// This runs on the AST backend deliberately: the chain lives there. The LIR
/// walker emits a direct call for a class shape like this one, so it would
/// report a passing number no matter what the filter did.
#[test]
fn dispatch_27_unrelated_same_named_classes_cost_little_code() {
    const CALL_SITES: usize = 256;
    const COLD_CLASSES: usize = 32;
    /// Between the ~10 KB the 32 method bodies legitimately cost and the
    /// ~200 KB an unfiltered chain costs.
    const MAX_OVERHEAD_BYTES: u64 = 64 * 1024;

    fn program(cold_classes: usize) -> String {
        let mut src = String::from(
            "class Hot {\n    pub n: i64;\n    pub fn tick(self) -> i64 { return self.n + 1; }\n}\n",
        );
        for i in 0..cold_classes {
            src.push_str(&format!(
                "class Cold{i} {{\n    pub n: i64;\n    pub fn tick(self) -> i64 {{ return self.n + {i}; }}\n}}\n"
            ));
        }
        src.push_str("fn main() {\n    let h = new Hot(1);\n    let mut t = 0;\n");
        for _ in 0..CALL_SITES {
            src.push_str("    t = t + h.tick();\n");
        }
        src.push_str("    println(t);\n}\n");
        src
    }

    fn build(label: &str, source: &str) -> u64 {
        let project = TestProject::new(label, &[("main.wi", source)]);
        let compiled = project.compile_with_env("main.wi", &LIR_OFF);
        assert!(
            compiled.status.success(),
            "compile failed: {}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let run = project.run();
        assert!(run.status.success(), "binary failed");
        assert_eq!(String::from_utf8_lossy(&run.stdout), "512\n");
        project.binary_size()
    }

    let baseline = build("dispatch_size_base", &program(0));
    let with_cold = build("dispatch_size_many", &program(COLD_CLASSES));

    let overhead = with_cold.saturating_sub(baseline);
    assert!(
        overhead < MAX_OVERHEAD_BYTES,
        "{COLD_CLASSES} unrelated same-named classes added {overhead} bytes across \
         {CALL_SITES} call sites (limit {MAX_OVERHEAD_BYTES}); the dispatch chain is \
         no longer filtered to the receiver's own hierarchy"
    );
}

/// The runnable example for this behavior, pinned to its output on both
/// backends. `example/class_method_dispatch.wi` is the user-facing statement of
/// what the filter guarantees: an unrelated class that shares a method name is
/// never a dispatch candidate, and everything in the receiver's own hierarchy
/// still is.
#[test]
fn dispatch_28_the_example_prints_what_it_documents() {
    assert_both_backends(
        include_str!("../../example/class_method_dispatch.wi"),
        "100\n220\n300\n22000\nledger\n",
    );
}

/// Perspective 29. A directly imported class arrives under two names — the
/// canonical `zoo::Dog` and the local alias `Dog` — while `class_base` records
/// whichever spelling each declaration used. The filter therefore asks its
/// question in `type_id` space, where a class's names collapse onto one
/// identity. Asked over names instead, the aliased `Animal` looks like a leaf,
/// `Dog` is filtered out of its own chain, and `speak` returns 5 instead of
/// 1005.
///
/// `Kennel` is here so the case is not just the pre-existing alias test: it
/// gives the program an unrelated class with the same method name, which is the
/// thing the filter is supposed to drop.
#[test]
fn dispatch_29_an_aliased_imported_base_still_dispatches_virtually() {
    let project = TestProject::new(
        "dispatch_alias",
        &[
            (
                "zoo.wi",
                r#"
pub open class Animal {
    pub value: i64;
    pub open fn speak(self) -> i64 { return self.value; }
}

pub class Dog extends Animal {
    pub override fn speak(self) -> i64 { return self.value + 1000; }
}
"#,
            ),
            (
                "main.wi",
                r#"
import zoo::Animal;
import zoo::Dog;

class Kennel {
    pub value: i64;
    pub fn speak(self) -> i64 { return -1; }
}

fn speak(a: Animal) -> i64 { return a.speak(); }

fn main() {
    println(speak(new Dog(5)));
    println(speak(new Animal(7)));
    println(new Kennel(0).speak());
}
"#,
            ),
        ],
    );

    for env in [LIR_ON.as_slice(), LIR_OFF.as_slice()] {
        let compiled = project.compile_with_env("main.wi", env);
        assert!(
            compiled.status.success(),
            "compile failed under {env:?}: {}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let run = project.run();
        assert!(run.status.success(), "binary failed under {env:?}");
        assert_eq!(String::from_utf8_lossy(&run.stdout), "1005\n7\n-1\n");
    }
}
