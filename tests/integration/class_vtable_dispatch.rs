//! Class virtual dispatch through a per-class VTABLE (willow-fm7t).
//!
//! Word 0 of every class object points at that class's DESCRIPTOR — a static
//! table holding the class's runtime `type_id` at offset 0, followed by one
//! slot per virtual method. A call whose receiver's runtime class is not known
//! statically loads the slot at a compile-time index and calls it. The index
//! comes from the receiver's STATIC class, and it is valid for every class the
//! receiver can actually be because a subclass's slot order EXTENDS its base's.
//!
//! That single rule is what these perspectives pin down:
//!
//!   * an `override` is the same NAME at the same INDEX, so it replaces the
//!     slot rather than appending one;
//!   * a subclass that does not override keeps the ancestor's address there;
//!   * a subclass may append virtual methods without moving an inherited index;
//!   * a class with no slot for a method is dispatched directly, because it can
//!     neither be overridden nor override anything;
//!   * an unrelated class that shares only the method NAME has its own
//!     descriptor and is never consulted.
//!
//! The dispatch this replaced walked a chain of `type_id` comparisons, one per
//! class in the receiver's hierarchy. Perspective 24 pins the code-size
//! consequence: a hierarchy that made the chain expensive now costs a constant.
//!
//! Perspectives:
//!   1. a base-typed reference selects the subclass override
//!   2. a subclass that does not override inherits the base implementation
//!   3. three levels: an override in the middle reaches the leaf
//!   4. three levels: the leaf overrides and the middle does not
//!   5. eight levels resolve to the NEAREST definition, not the root
//!   6. sibling branches never cross
//!   7. `self.method()` inside a non-virtual base method dispatches virtually
//!   8. two virtual methods keep distinct slots
//!   9. a subclass appending a virtual method does not disturb inherited slots
//!  10. an `override` reuses its base's slot instead of appending
//!  11. an unrelated class sharing the method name is unaffected
//!  12. a non-`open` method is a direct call and stays correct under inheritance
//!  13. an array of base-typed elements dispatches per element
//!  14. argument side effects and evaluation order survive the indirect call
//!  15. a `String`-returning virtual method dispatches
//!  16. a `bool`-returning virtual method dispatches
//!  17. a `void`-returning virtual method dispatches
//!  18. a virtual method with several parameters passes them through
//!  19. an interface-typed receiver still uses the INTERFACE vtable
//!  20. a class downcast still reads the type_id through the descriptor
//!  21. classes from an imported module dispatch virtually
//!  22. an aliased imported base class dispatches to its subclasses
//!  23. dispatch is correct after a GC cycle moves nothing but collects a lot
//!  24. a deep polymorphic hierarchy costs constant code per call site
//!  25. a panicking virtual method still reports its method frame
//!  26. a static method is not routed through a slot
//!  27. the runnable example prints what it documents
//!
//! Every behavioral perspective runs the program and asserts what it prints.
//! Since willow-0g8j.2.4 the LIR walker compiles these hierarchies, deciding
//! each call's slot through `plan_virtual_call`, so a slot-index bug shows up
//! as a wrong ANSWER rather than as a refused body. The walker-specific
//! eligibility coverage lives in `lir_class_inheritance.rs`.

use super::support::{
    TestProject, compile_and_run_gc_stress, compile_with_env_and_run,
    compile_with_env_and_run_combined,
};

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];

/// The build runs and prints `expected`. Since willow-0g8j.3 every body is
/// compiled from lowered IR, so a body the walker cannot take is a compile
/// error here rather than a second emitter's answer.
#[track_caller]
fn assert_project_output(source: &str, expected: &str) {
    let (out, ok) = compile_with_env_and_run(source, &PLAIN);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, expected, "wrong output");
}

/// Perspective 1. The slot index comes from `Base`, the address in it comes
/// from the runtime class.
#[test]
fn vtable_01_a_base_reference_selects_the_override() {
    assert_project_output(
        r#"
open class Base {
    pub n: i64;
    pub open fn value(self) -> i64 { return self.n; }
}
class Derived extends Base {
    pub override fn value(self) -> i64 { return self.n * 10; }
}
fn take(b: Base) -> i64 { return b.value(); }
fn main() {
    println(take(new Base(3)));
    println(take(new Derived(3)));
}
"#,
        "3\n30\n",
    );
}

/// Perspective 2. `Plain` overrides nothing, so its descriptor slot still holds
/// `Base::value`. Inheritance costs no indirection beyond the same one load.
#[test]
fn vtable_02_a_non_overriding_subclass_inherits_the_slot() {
    assert_project_output(
        r#"
open class Base {
    pub n: i64;
    pub open fn value(self) -> i64 { return self.n + 1; }
}
class Plain extends Base {}
fn take(b: Base) -> i64 { return b.value(); }
fn main() {
    println(take(new Base(5)));
    println(take(new Plain(5)));
}
"#,
        "6\n6\n",
    );
}

/// Perspective 3. `Mid` overrides; `Leaf` does not. `Leaf`'s slot must hold
/// `Mid::value`, not `Top::value` — the ancestor walk stops at the nearest
/// definition.
#[test]
fn vtable_03_three_levels_override_in_the_middle() {
    assert_project_output(
        r#"
open class Top {
    pub n: i64;
    pub open fn value(self) -> i64 { return 1; }
}
open class Mid extends Top {
    pub override fn value(self) -> i64 { return 2; }
}
class Leaf extends Mid {}
fn take(t: Top) -> i64 { return t.value(); }
fn main() {
    println(take(new Top(0)));
    println(take(new Mid(0)));
    println(take(new Leaf(0)));
}
"#,
        "1\n2\n2\n",
    );
}

/// Perspective 4. The mirror image: the middle inherits and the leaf overrides.
#[test]
fn vtable_04_three_levels_override_at_the_leaf() {
    assert_project_output(
        r#"
open class Top {
    pub n: i64;
    pub open fn value(self) -> i64 { return 1; }
}
open class Mid extends Top {}
class Leaf extends Mid {
    pub override fn value(self) -> i64 { return 3; }
}
fn take(t: Top) -> i64 { return t.value(); }
fn main() {
    println(take(new Top(0)));
    println(take(new Mid(0)));
    println(take(new Leaf(0)));
}
"#,
        "1\n1\n3\n",
    );
}

/// Perspective 5. Eight levels with definitions at 0, 3 and 6. Every class must
/// resolve to the nearest definition ABOVE it, and all eight share slot 0.
///
/// `L3` and `L6` are `open override`: an `override` alone closes the method
/// again, so re-opening it is what lets the chain continue. Both spellings must
/// land on the SAME inherited slot.
#[test]
fn vtable_05_eight_levels_resolve_to_the_nearest_definition() {
    assert_project_output(
        r#"
open class L0 {
    pub n: i64;
    pub open fn value(self) -> i64 { return 0; }
}
open class L1 extends L0 {}
open class L2 extends L1 {}
open class L3 extends L2 { pub open override fn value(self) -> i64 { return 3; } }
open class L4 extends L3 {}
open class L5 extends L4 {}
open class L6 extends L5 { pub open override fn value(self) -> i64 { return 6; } }
class L7 extends L6 {}
fn take(x: L0) -> i64 { return x.value(); }
fn main() {
    println(take(new L0(0)));
    println(take(new L1(0)));
    println(take(new L2(0)));
    println(take(new L3(0)));
    println(take(new L4(0)));
    println(take(new L5(0)));
    println(take(new L6(0)));
    println(take(new L7(0)));
}
"#,
        "0\n0\n0\n3\n3\n3\n6\n6\n",
    );
}

/// Perspective 6. Two branches off one root. Each branch's descriptor is
/// separate, so an override on one side can never reach the other.
#[test]
fn vtable_06_sibling_branches_never_cross() {
    assert_project_output(
        r#"
open class Root {
    pub n: i64;
    pub open fn value(self) -> i64 { return 0; }
}
open class Left extends Root { pub override fn value(self) -> i64 { return 1; } }
open class Right extends Root { pub override fn value(self) -> i64 { return 2; } }
class LeftLeaf extends Left {}
class RightLeaf extends Right {}
fn take(r: Root) -> i64 { return r.value(); }
fn main() {
    println(take(new LeftLeaf(0)));
    println(take(new RightLeaf(0)));
}
"#,
        "1\n2\n",
    );
}

/// Perspective 7. `describe` has no slot — it is neither `open` nor an
/// `override`, so the call to it is direct. The `self.value()` INSIDE it is
/// virtual, so a subclass reaching `describe` through inheritance still runs
/// its own `value`.
#[test]
fn vtable_07_self_call_inside_a_non_virtual_base_method_is_virtual() {
    assert_project_output(
        r#"
open class Base {
    pub n: i64;
    pub open fn value(self) -> i64 { return self.n; }
    pub fn describe(self) -> i64 { return self.value() * 100; }
}
class Derived extends Base {
    pub override fn value(self) -> i64 { return self.n + 7; }
}
fn take(b: Base) -> i64 { return b.describe(); }
fn main() {
    println(take(new Base(2)));
    println(take(new Derived(2)));
}
"#,
        "200\n900\n",
    );
}

/// Perspective 8. Two virtual methods must occupy DIFFERENT slots. If they
/// collided, overriding one would silently redirect the other.
#[test]
fn vtable_08_two_virtual_methods_keep_distinct_slots() {
    assert_project_output(
        r#"
open class Base {
    pub n: i64;
    pub open fn first(self) -> i64 { return 1; }
    pub open fn second(self) -> i64 { return 2; }
}
class Derived extends Base {
    pub override fn first(self) -> i64 { return 11; }
}
fn take(b: Base) -> i64 { return b.first() * 100 + b.second(); }
fn main() {
    println(take(new Base(0)));
    println(take(new Derived(0)));
}
"#,
        "102\n1102\n",
    );
}

/// Perspective 9. `Derived` appends `extra` at a new slot. The two indices
/// `Base` already handed out must not move, or `take` would call the wrong
/// method through a `Derived` receiver.
#[test]
fn vtable_09_appending_a_virtual_method_does_not_disturb_inherited_slots() {
    assert_project_output(
        r#"
open class Base {
    pub n: i64;
    pub open fn first(self) -> i64 { return 1; }
    pub open fn second(self) -> i64 { return 2; }
}
class Derived extends Base {
    pub open fn extra(self) -> i64 { return 99; }
}
fn take(b: Base) -> i64 { return b.first() * 10 + b.second(); }
fn main() {
    println(take(new Base(0)));
    println(take(new Derived(0)));
    println(new Derived(0).extra());
}
"#,
        "12\n12\n99\n",
    );
}

/// Perspective 10. `Derived` declares `second` before overriding `first`. The
/// override must still land on `first`'s inherited index rather than taking the
/// next free one, so declaration order inside the subclass cannot renumber the
/// base's slots.
#[test]
fn vtable_10_an_override_reuses_its_bases_slot() {
    assert_project_output(
        r#"
open class Base {
    pub n: i64;
    pub open fn first(self) -> i64 { return 1; }
    pub open fn second(self) -> i64 { return 2; }
}
class Derived extends Base {
    pub override fn second(self) -> i64 { return 22; }
    pub override fn first(self) -> i64 { return 11; }
}
fn take(b: Base) -> i64 { return b.first() * 100 + b.second(); }
fn main() {
    println(take(new Base(0)));
    println(take(new Derived(0)));
}
"#,
        "102\n1122\n",
    );
}

/// Perspective 11. `Ledger` shares the method NAME and nothing else. It has its
/// own descriptor, so no `Account` receiver can reach it and its presence
/// cannot change what an `Account` call site does.
#[test]
fn vtable_11_an_unrelated_same_named_class_is_unaffected() {
    assert_project_output(
        r#"
open class Account {
    pub n: i64;
    pub open fn total(self) -> i64 { return self.n; }
}
class Savings extends Account {
    pub override fn total(self) -> i64 { return self.n * 2; }
}
class Ledger {
    pub n: i64;
    pub fn total(self) -> String { return "ledger"; }
}
fn take(a: Account) -> i64 { return a.total(); }
fn main() {
    println(take(new Account(4)));
    println(take(new Savings(4)));
    println(new Ledger(0).total());
}
"#,
        "4\n8\nledger\n",
    );
}

/// Perspective 12. A method that is neither `open` nor `override` gets no slot.
/// The call is direct even though the class is extended, and the subclass
/// inherits exactly that implementation.
#[test]
fn vtable_12_a_non_open_method_is_a_direct_call() {
    assert_project_output(
        r#"
open class Base {
    pub n: i64;
    pub fn fixed(self) -> i64 { return self.n + 1; }
}
class Derived extends Base {}
fn take(b: Base) -> i64 { return b.fixed(); }
fn main() {
    println(take(new Base(1)));
    println(take(new Derived(1)));
}
"#,
        "2\n2\n",
    );
}

/// Perspective 13. One call site inside a loop, three runtime classes.
#[test]
fn vtable_13_an_array_of_base_elements_dispatches_per_element() {
    assert_project_output(
        r#"
import std::collections::Array;
open class Base {
    pub n: i64;
    pub open fn value(self) -> i64 { return self.n; }
}
open class Twice extends Base { pub open override fn value(self) -> i64 { return self.n * 2; } }
class Thrice extends Twice { pub override fn value(self) -> i64 { return self.n * 3; } }
fn main() {
    let items: Array<Base> = [new Base(10), new Twice(10), new Thrice(10)];
    let mut i = 0;
    while i < items.len() {
        println(items[i].value());
        i = i + 1;
    }
}
"#,
        "10\n20\n30\n",
    );
}

/// Perspective 14. The slot is loaded before the arguments are evaluated, so an
/// argument with a side effect must still run exactly once and in order.
#[test]
fn vtable_14_argument_side_effects_and_order_survive() {
    assert_project_output(
        r#"
open class Base {
    pub n: i64;
    pub open fn combine(self, a: i64, b: i64) -> i64 { return a * 10 + b; }
}
class Derived extends Base {
    pub override fn combine(self, a: i64, b: i64) -> i64 { return a * 100 + b; }
}
fn bump(label: i64) -> i64 {
    println(label);
    return label;
}
fn take(b: Base) -> i64 { return b.combine(bump(1), bump(2)); }
fn main() {
    println(take(new Base(0)));
    println(take(new Derived(0)));
}
"#,
        "1\n2\n12\n1\n2\n102\n",
    );
}

/// Perspective 15. A `String` return travels through the indirect call
/// unchanged.
#[test]
fn vtable_15_a_string_returning_virtual_method_dispatches() {
    assert_project_output(
        r#"
open class Base {
    pub n: i64;
    pub open fn name(self) -> String { return "base"; }
}
class Derived extends Base {
    pub override fn name(self) -> String { return "derived"; }
}
fn take(b: Base) -> String { return b.name(); }
fn main() {
    println(take(new Base(0)));
    println(take(new Derived(0)));
}
"#,
        "base\nderived\n",
    );
}

/// Perspective 16. A `bool` return is a narrower CLIF type than the pointer the
/// slot holds; the indirect call's signature must describe the RETURN, not the
/// slot.
#[test]
fn vtable_16_a_bool_returning_virtual_method_dispatches() {
    assert_project_output(
        r#"
open class Base {
    pub n: i64;
    pub open fn ready(self) -> bool { return false; }
}
class Derived extends Base {
    pub override fn ready(self) -> bool { return true; }
}
fn take(b: Base) -> bool { return b.ready(); }
fn main() {
    println(take(new Base(0)));
    println(take(new Derived(0)));
}
"#,
        "false\ntrue\n",
    );
}

/// Perspective 17. A `void` return means the indirect call has no result, and
/// the emitter must not try to read one.
#[test]
fn vtable_17_a_void_returning_virtual_method_dispatches() {
    assert_project_output(
        r#"
open class Base {
    pub n: i64;
    pub open fn announce(self) { println("base"); }
}
class Derived extends Base {
    pub override fn announce(self) { println("derived"); }
}
fn take(b: Base) { b.announce(); }
fn main() {
    take(new Base(0));
    take(new Derived(0));
}
"#,
        "base\nderived\n",
    );
}

/// Perspective 18. Several parameters of mixed type must arrive in order: the
/// indirect signature is built from the statically resolved method, and every
/// override shares it.
#[test]
fn vtable_18_a_virtual_method_with_several_parameters() {
    assert_project_output(
        r#"
open class Base {
    pub n: i64;
    pub open fn mix(self, a: i64, b: String, c: bool) -> String {
        if c { return b + a.toString(); }
        return "base";
    }
}
class Derived extends Base {
    pub override fn mix(self, a: i64, b: String, c: bool) -> String {
        if c { return b + (a * 2).toString(); }
        return "derived";
    }
}
fn take(b: Base) -> String { return b.mix(21, "x", true) + "/" + b.mix(21, "x", false); }
fn main() {
    println(take(new Base(0)));
    println(take(new Derived(0)));
}
"#,
        "x21/base\nx42/derived\n",
    );
}

/// Perspective 19. An interface-typed receiver goes through the INTERFACE
/// vtable, which is a different table with its own slot order. The class
/// descriptor change must not disturb it, and a class that both implements an
/// interface and overrides a class method must get both right.
#[test]
fn vtable_19_an_interface_receiver_uses_the_interface_vtable() {
    assert_project_output(
        r#"
interface Speaker {
    fn speak(self) -> String;
}
open class Animal implements Speaker {
    pub n: i64;
    pub fn speak(self) -> String { return "..."; }
    pub open fn legs(self) -> i64 { return 4; }
}
class Bird extends Animal {
    pub override fn legs(self) -> i64 { return 2; }
}
fn through_interface(s: Speaker) -> String { return s.speak(); }
fn through_class(a: Animal) -> i64 { return a.legs(); }
fn main() {
    println(through_interface(new Animal(0)));
    println(through_class(new Animal(0)));
    println(through_class(new Bird(0)));
}
"#,
        "...\n4\n2\n",
    );
}

/// Perspective 20. A class downcast reads the runtime `type_id`, which now
/// lives at offset 0 of the descriptor rather than in the object. Two dependent
/// loads instead of one — and the same answer.
#[test]
fn vtable_20_a_class_downcast_reads_the_id_through_the_descriptor() {
    assert_project_output(
        r#"
interface Shape {
    fn area(self) -> i64;
}
class Sq implements Shape {
    pub side: i64;
    pub fn area(self) -> i64 { return self.side * self.side; }
}
class Circ implements Shape {
    pub r: i64;
    pub fn area(self) -> i64 { return self.r * 3; }
}
fn describe(s: Shape) -> String {
    return match s {
        Sq(sq) => "square " + sq.side.toString(),
        Circ(c) => "circle " + c.r.toString(),
        _ => "other",
    };
}
fn main() {
    println(describe(new Sq(4)));
    println(describe(new Circ(4)));
}
"#,
        "square 4\ncircle 4\n",
    );
}

/// Perspective 21. A module's classes get their descriptors emitted while the
/// module is compiled, and the entry program must reach the same ones.
#[test]
fn vtable_21_imported_module_classes_dispatch_virtually() {
    let project = TestProject::new(
        "vtable_module",
        &[
            (
                "zoo.wi",
                r#"
pub open class Animal {
    pub value: i64;
    pub open fn speak(self) -> i64 { return self.value; }
}
pub open class Dog extends Animal {
    pub override fn speak(self) -> i64 { return self.value + 100; }
}
pub class Puppy extends Dog {}
"#,
            ),
            (
                "main.wi",
                r#"
import zoo::Animal;
import zoo::Dog;
import zoo::Puppy;

fn speak(a: Animal) -> i64 { return a.speak(); }

fn main() {
    println(speak(new Animal(1)));
    println(speak(new Dog(1)));
    println(speak(new Puppy(1)));
}
"#,
            ),
        ],
    );

    let compiled = project.compile_with_env("main.wi", &PLAIN);
    assert!(
        compiled.status.success(),
        "compile failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let run = project.run();
    assert!(run.status.success(), "binary failed");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "1\n101\n101\n");
}

/// Perspective 22. A directly imported class arrives under two names — the
/// canonical `zoo::Dog` and the local alias `Dog`. Both spellings are ONE
/// runtime class, so both must reach the same descriptor: the slot-order table
/// and the descriptor symbol have to be aliased alongside the `type_id`. Missed,
/// the alias either finds no descriptor at all or a different one, and `speak`
/// answers 5 instead of 1005.
#[test]
fn vtable_22_an_aliased_imported_base_reaches_the_same_descriptor() {
    let project = TestProject::new(
        "vtable_alias",
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

    let compiled = project.compile_with_env("main.wi", &PLAIN);
    assert!(
        compiled.status.success(),
        "compile failed: {}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let run = project.run();
    assert!(run.status.success(), "binary failed");
    assert_eq!(String::from_utf8_lossy(&run.stdout), "1005\n7\n-1\n");
}

/// Perspective 23. The descriptor is a STATIC data symbol living in word 0 of
/// every object — a word the collector must keep treating as a non-reference,
/// exactly as it treated the inline `type_id`. Under GC stress, tracing it
/// would follow a pointer into read-only data; failing to keep the object's
/// field mask aligned would drop a live field. Both show up here.
#[test]
fn vtable_23_dispatch_is_correct_under_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;
open class Node {
    pub label: String;
    pub n: i64;
    pub open fn value(self) -> i64 { return self.n; }
    pub fn tag(self) -> String { return self.label; }
}
class Doubled extends Node {
    pub override fn value(self) -> i64 { return self.n * 2; }
}
fn main() {
    let mut total = 0;
    let mut kept: Array<Node> = [];
    let mut i = 0;
    while i < 400 {
        let n: Node = new Node("keep", i);
        let d: Node = new Doubled("keep", i);
        total = total + n.value() + d.value();
        if i % 100 == 0 { kept.push(d); }
        i = i + 1;
    }
    let mut j = 0;
    let mut live = 0;
    while j < kept.len() {
        live = live + kept[j].value();
        if kept[j].tag() != "keep" { println("field lost"); }
        j = j + 1;
    }
    println(total);
    println(live);
}
"#,
    );
    assert!(ok, "GC stress run failed: {out}");
    // total = sum(i) + sum(2i) for i in 0..400 = 3 * 79800 = 239400
    // live  = 2 * (0 + 100 + 200 + 300) = 1200
    assert_eq!(out, "239400\n1200\n");
}

/// Perspective 24. The measurement the vtable exists for.
///
/// Both programs are built by the same compiler and differ only by the 32
/// subclasses, so their size difference is what those subclasses cost across
/// 256 polymorphic call sites. The chain this replaced emitted one `type_id`
/// comparison, one branch and one call PER CLASS at every call site, and
/// measured ~200 KB of `.text`; a slot load and an indirect call cost the same
/// whatever the hierarchy's size, so the difference is now just the 32 method
/// bodies. The bound sits clear of both.
///
/// The per-class comparison chain this bound was written against lived in the
/// AST emitter willow-0g8j.3 retired, and the walker compiles inherited classes
/// itself since willow-0g8j.13. The measurement stays as a code-size regression
/// guard on vtable dispatch.
#[test]
fn vtable_24_a_deep_hierarchy_costs_constant_code_per_call_site() {
    const CALL_SITES: usize = 256;
    const SUBCLASSES: usize = 32;
    /// Between the few KB the 32 method bodies legitimately cost and the
    /// ~200 KB a per-class comparison chain costs. Measured: 11,880 bytes
    /// through the vtable, against 198,704 through the chain it replaced.
    const MAX_OVERHEAD_BYTES: u64 = 64 * 1024;

    fn program(subclasses: usize) -> String {
        let mut src = String::from(
            "open class Base {\n    pub n: i64;\n    pub open fn tick(self) -> i64 { return self.n + 1; }\n}\n",
        );
        for i in 0..subclasses {
            src.push_str(&format!(
                "class Sub{i} extends Base {{\n    pub override fn tick(self) -> i64 {{ return self.n + {i}; }}\n}}\n"
            ));
        }
        src.push_str("fn main() {\n    let h: Base = new Base(1);\n    let mut t = 0;\n");
        for _ in 0..CALL_SITES {
            src.push_str("    t = t + h.tick();\n");
        }
        src.push_str("    println(t);\n}\n");
        src
    }

    fn build(label: &str, source: &str) -> u64 {
        let project = TestProject::new(label, &[("main.wi", source)]);
        let compiled = project.compile_with_env("main.wi", &PLAIN);
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

    let baseline = build("vtable_size_base", &program(0));
    let with_subs = build("vtable_size_many", &program(SUBCLASSES));

    let overhead = with_subs.saturating_sub(baseline);
    assert!(
        overhead < MAX_OVERHEAD_BYTES,
        "{SUBCLASSES} subclasses added {overhead} bytes across {CALL_SITES} call sites \
         (limit {MAX_OVERHEAD_BYTES}); virtual dispatch is no longer O(1) per call site"
    );
}

/// Perspective 25. The debug call-chain frame is pushed once around the
/// indirect call, so a panic inside the OVERRIDE still names the method.
#[test]
fn vtable_25_a_panicking_virtual_method_reports_its_frame() {
    let source = r#"
open class Base {
    pub n: i64;
    pub open fn tick(self) -> i64 { return self.n; }
}
class Boom extends Base {
    pub override fn tick(self) -> i64 { panic("boom"); }
}
fn take(b: Base) -> i64 { return b.tick(); }
fn main() {
    println(take(new Base(1)));
    println(take(new Boom(1)));
}
"#;
    let (output, ok) = compile_with_env_and_run_combined(source, &PLAIN);
    assert!(!ok, "the program must abort: {output}");
    assert!(
        output.contains("runtime panic: boom"),
        "missing panic message: {output}"
    );
    assert!(
        output.contains("0: tick at"),
        "missing method frame: {output}"
    );
}

/// Perspective 26. A static method has no receiver, so it has no slot and never
/// reaches a descriptor — including on a class that is extended.
#[test]
fn vtable_26_a_static_method_is_not_routed_through_a_slot() {
    assert_project_output(
        r#"
open class Base {
    pub n: i64;
    pub static fn make() -> i64 { return 3; }
    pub open fn value(self) -> i64 { return 1; }
}
class Derived extends Base {
    pub override fn value(self) -> i64 { return 2; }
}
fn main() {
    println(Base::make());
    let d: Base = new Derived(0);
    println(d.value());
}
"#,
        "3\n2\n",
    );
}

/// Perspective 27. The runnable example is the user-facing statement of what
/// the slot order guarantees; it must print what its comments claim.
#[test]
fn vtable_27_the_example_prints_what_it_documents() {
    assert_project_output(
        include_str!("../../example/class_vtable_dispatch.wi"),
        "shape=4\nshape=16\nshape=16\ncircle=48\n5\ninvoice\n",
    );
}
