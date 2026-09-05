//! Class field layout is independent of DECLARATION ORDER (willow-59gx).
//!
//! A class's layout is its base's layout followed by the fields it declares
//! itself. That rule is what makes an inherited field sit at the same offset
//! whether it is reached through the subclass or through a base-typed
//! reference, and it is why a constructor's argument order is base fields
//! first.
//!
//! Building that layout while walking declarations gets it wrong, because a
//! subclass may be declared BEFORE its base. The scheme this replaced gave
//! every class its own fields, then rebuilt each one from its base's current
//! entry in declaration order: reaching `C extends B` before `B extends A` read
//! a `B` that had not been rebuilt yet, so `A`'s fields never reached `C`. One
//! level came out right by accident. Two levels ICE'd:
//!
//!   compiler invariant violated: checked field `a` has no loadable slot
//!
//! The layouts are now finalized in a separate pass that walks each `extends`
//! chain root-down, so no declaration order can change a layout.
//!
//! Perspectives:
//!   1. a subclass declared before its base reads the inherited field
//!   2. three levels declared leaf-first (the ICE repro)
//!   3. eight levels declared in reverse
//!   4. base-first still works (the order that always did)
//!   5. unrelated classes interleaved between the declarations
//!   6. two subclasses of one base, both declared before it
//!   7. a redeclared field name keeps the ancestor's slot, not a second one
//!   8. a subclass that adds no fields of its own
//!   9. a base that declares no fields at all
//!  10. `new` takes base fields first regardless of declaration order
//!  11. an assignment through a base-typed reference hits the same offset
//!  12. a base-typed parameter reads an inherited field at the same offset
//!  13. a grandparent's REFERENCE field survives a GC cycle
//!  14. mixed field types keep their declared order and widths
//!  15. a method inherited from the grandparent reads the grandparent's field
//!  16. virtual dispatch is unaffected by declaration order
//!  17. an imported module hierarchy declared subclass-first
//!  18. an aliased direct import of a reverse-declared base
//!  19. a class downcast in a reverse-declared hierarchy
//!  20. an array of a base type holding reverse-declared subclasses
//!  21. a static field never enters the instance layout
//!  22. each level of a deep chain contributes its field in root-down order
//!  23. the runnable example prints what it documents
//!
//! Every behavioral perspective runs the program and asserts what it prints:
//! since willow-0g8j.3 a body outside the walker's subset does not compile, so
//! a layout bug shows up as a wrong ANSWER rather than as a refusal.

use super::support::{TestProject, compile_and_run_gc_stress, compile_with_env_and_run};

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

#[track_caller]
fn assert_project_runs(project: &TestProject, expected: &str) {
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

/// Perspective 1. One level, subclass first. This order always worked — a base
/// with no base of its own has the same layout before and after rebuilding —
/// so it pins the case the fix must NOT disturb.
#[test]
fn layout_01_one_level_declared_subclass_first() {
    assert_project_output(
        r#"
class Sub extends Base {
    pub b: i64;
    pub fn sum(self) -> i64 { return self.a + self.b; }
}
open class Base {
    pub a: i64;
}
fn main() {
    let s = new Sub(1, 2);
    println(s.sum());
    println(s.a);
    println(s.b);
}
"#,
        "3\n1\n2\n",
    );
}

/// Perspective 2. The reported ICE: three levels, leaf declared first. `C` read
/// a `B` that still carried only its own fields, so `A::a` was never in `C`'s
/// layout and the field access had no slot to load from.
#[test]
fn layout_02_three_levels_declared_leaf_first() {
    assert_project_output(
        r#"
class C extends B {
    pub c: i64;
    pub fn sum(self) -> i64 { return self.a + self.b + self.c; }
}
open class B extends A {
    pub b: i64;
}
open class A {
    pub a: i64;
}
fn main() {
    let x = new C(1, 2, 3);
    println(x.sum());
    println(x.a);
    println(x.b);
    println(x.c);
}
"#,
        "6\n1\n2\n3\n",
    );
}

/// Perspective 3. Eight levels in reverse. Every level must contribute exactly
/// one field, and a walk that stops one level short is visible in the sum.
#[test]
fn layout_03_eight_levels_declared_in_reverse() {
    assert_project_output(
        r#"
class L8 extends L7 {
    pub f8: i64;
    pub fn sum(self) -> i64 {
        return self.f1 + self.f2 + self.f3 + self.f4
             + self.f5 + self.f6 + self.f7 + self.f8;
    }
}
open class L7 extends L6 { pub f7: i64; }
open class L6 extends L5 { pub f6: i64; }
open class L5 extends L4 { pub f5: i64; }
open class L4 extends L3 { pub f4: i64; }
open class L3 extends L2 { pub f3: i64; }
open class L2 extends L1 { pub f2: i64; }
open class L1 { pub f1: i64; }
fn main() {
    let x = new L8(1, 2, 4, 8, 16, 32, 64, 128);
    println(x.sum());
    println(x.f1);
    println(x.f8);
}
"#,
        "255\n1\n128\n",
    );
}

/// Perspective 4. The same hierarchy declared base-first. The offsets must be
/// identical to perspective 3's — declaration order is not allowed to be an ABI
/// input.
#[test]
fn layout_04_the_same_hierarchy_declared_base_first() {
    assert_project_output(
        r#"
open class L1 { pub f1: i64; }
open class L2 extends L1 { pub f2: i64; }
open class L3 extends L2 { pub f3: i64; }
class L4 extends L3 {
    pub f4: i64;
    pub fn sum(self) -> i64 { return self.f1 + self.f2 + self.f3 + self.f4; }
}
fn main() {
    let x = new L4(1, 2, 4, 8);
    println(x.sum());
    println(x.f1);
    println(x.f4);
}
"#,
        "15\n1\n8\n",
    );
}

/// Perspective 5. Unrelated classes between the declarations. A pass that
/// depends on adjacency rather than on the `extends` chain would drift here.
#[test]
fn layout_05_unrelated_classes_interleaved() {
    assert_project_output(
        r#"
class Leaf extends Middle {
    pub c: i64;
    pub fn sum(self) -> i64 { return self.a + self.b + self.c; }
}
class Bystander { pub x: i64; pub y: i64; }
open class Middle extends Root { pub b: i64; }
class Onlooker { pub z: i64; }
open class Root { pub a: i64; }
fn main() {
    println(new Leaf(1, 2, 3).sum());
    println(new Bystander(9, 8).y);
    println(new Onlooker(7).z);
}
"#,
        "6\n8\n7\n",
    );
}

/// Perspective 6. One base, two subclasses, both declared before it. Each gets
/// the base's field at the same offset and its own after it; neither inherits
/// the other's.
#[test]
fn layout_06_two_subclasses_declared_before_their_base() {
    assert_project_output(
        r#"
class Left extends Shared {
    pub l: i64;
    pub fn sum(self) -> i64 { return self.s + self.l; }
}
class Right extends Shared {
    pub r: i64;
    pub fn sum(self) -> i64 { return self.s * self.r; }
}
open class Shared { pub s: i64; }
fn main() {
    println(new Left(10, 5).sum());
    println(new Right(10, 5).sum());
    println(new Left(10, 5).s);
    println(new Right(10, 5).s);
}
"#,
        "15\n50\n10\n10\n",
    );
}

/// Perspective 7. A subclass redeclaring an inherited NAME keeps the
/// ancestor's slot rather than appending a second one, so the class has three
/// fields, not four, and `new` takes three arguments.
#[test]
fn layout_07_a_redeclared_field_name_keeps_the_ancestor_slot() {
    assert_project_output(
        r#"
class Sub extends Base {
    pub shared: i64;
    pub extra: i64;
    pub fn sum(self) -> i64 { return self.shared + self.extra + self.only_base; }
}
open class Base {
    pub shared: i64;
    pub only_base: i64;
}
fn main() {
    let s = new Sub(1, 2, 3);
    println(s.sum());
    println(s.shared);
    println(s.only_base);
    println(s.extra);
}
"#,
        "6\n1\n2\n3\n",
    );
}

/// Perspective 8. A subclass with no fields of its own is exactly its base's
/// layout — no padding slot, no shifted offset.
#[test]
fn layout_08_a_subclass_that_adds_no_fields() {
    assert_project_output(
        r#"
class Empty extends Base {
    pub fn sum(self) -> i64 { return self.a + self.b; }
}
open class Base {
    pub a: i64;
    pub b: i64;
}
fn main() {
    let e = new Empty(4, 5);
    println(e.sum());
    println(e.a);
    println(e.b);
}
"#,
        "9\n4\n5\n",
    );
}

/// Perspective 9. A fieldless base contributes nothing, so the subclass's own
/// field lands at offset zero of the payload.
#[test]
fn layout_09_a_base_with_no_fields() {
    assert_project_output(
        r#"
class Sub extends Marker {
    pub v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
open class Marker {}
fn main() {
    println(new Sub(42).get());
    println(new Sub(42).v);
}
"#,
        "42\n42\n",
    );
}

/// Perspective 10. Constructor argument order IS the layout, so a wrong layout
/// silently swaps the caller's arguments instead of failing. Distinct values
/// per level make that visible.
#[test]
fn layout_10_constructor_argument_order_follows_the_layout() {
    assert_project_output(
        r#"
class Leaf extends Middle {
    pub third: i64;
    pub fn show(self) -> String {
        return self.first.toString() + "," + self.second.toString() + "," + self.third.toString();
    }
}
open class Middle extends Root { pub second: i64; }
open class Root { pub first: i64; }
fn main() {
    println(new Leaf(100, 20, 3).show());
}
"#,
        "100,20,3\n",
    );
}

/// Perspective 11. A write through a base-typed reference and a read through
/// the subclass must agree on the offset. Disagreement corrupts a neighbouring
/// field instead of erroring.
#[test]
fn layout_11_assignment_through_a_base_typed_reference() {
    assert_project_output(
        r#"
class Leaf extends Middle {
    pub c: i64;
}
open class Middle extends Root { pub b: i64; }
open class Root { pub a: i64; }
fn bump(r: Root) {
    r.a = r.a + 1000;
}
fn main() {
    let leaf = new Leaf(1, 2, 3);
    bump(leaf);
    println(leaf.a);
    println(leaf.b);
    println(leaf.c);
}
"#,
        "1001\n2\n3\n",
    );
}

/// Perspective 12. Reading an inherited field through a base-typed parameter.
/// The callee compiles against the BASE's layout and the caller allocates the
/// subclass's, so this is the offset agreement stated directly.
#[test]
fn layout_12_a_base_typed_parameter_reads_an_inherited_field() {
    assert_project_output(
        r#"
class Leaf extends Middle {
    pub c: i64;
}
open class Middle extends Root { pub b: i64; }
open class Root { pub a: i64; }
fn read_root(r: Root) -> i64 { return r.a; }
fn read_middle(m: Middle) -> i64 { return m.a + m.b; }
fn main() {
    let leaf = new Leaf(7, 8, 9);
    println(read_root(leaf));
    println(read_middle(leaf));
    println(read_root(new Middle(7, 8)));
}
"#,
        "7\n15\n7\n",
    );
}

/// Perspective 13. The GC ref mask is derived from the layout, so a layout that
/// loses a grandparent's REFERENCE field also loses the collector's reason to
/// keep it alive — or worse, shifts the mask so an integer is traced as a
/// pointer. Under allocation stress both show up.
#[test]
fn layout_13_a_grandparents_reference_field_survives_collection() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;
class Leaf extends Middle {
    pub tail: String;
    pub fn joined(self) -> String { return self.head + self.body + self.tail; }
}
open class Middle extends Root { pub body: String; }
open class Root { pub head: String; }
fn main() {
    let mut kept: Array<Leaf> = [];
    let mut made = 0;
    let mut i = 0;
    while i < 400 {
        let leaf = new Leaf("a", "b", "c");
        if leaf.joined() == "abc" { made = made + 1; }
        if i % 100 == 0 { kept.push(leaf); }
        i = i + 1;
    }
    let mut j = 0;
    let mut live = 0;
    while j < kept.len() {
        if kept[j].joined() == "abc" { live = live + 1; }
        j = j + 1;
    }
    println(made);
    println(live);
}
"#,
    );
    assert!(ok, "GC stress run failed: {out}");
    assert_eq!(out, "400\n4\n");
}

/// Perspective 14. Mixed widths and kinds across three levels. Every field is
/// read back, so a shifted slot shows up as a wrong value rather than a crash.
#[test]
fn layout_14_mixed_field_types_keep_their_order() {
    assert_project_output(
        r#"
class Leaf extends Middle {
    pub flag: bool;
    pub name: String;
}
open class Middle extends Root { pub ratio: f64; }
open class Root { pub count: i64; }
fn main() {
    let leaf = new Leaf(5, 2.5, true, "leaf");
    println(leaf.count);
    println(leaf.ratio);
    println(leaf.flag);
    println(leaf.name);
}
"#,
        "5\n2.5\ntrue\nleaf\n",
    );
}

/// Perspective 15. A method declared on the grandparent and inherited two
/// levels down compiles once, against the grandparent's layout, and runs on a
/// leaf instance.
#[test]
fn layout_15_an_inherited_method_reads_the_grandparents_field() {
    assert_project_output(
        r#"
class Leaf extends Middle {
    pub c: i64;
}
open class Middle extends Root { pub b: i64; }
open class Root {
    pub a: i64;
    pub fn root_value(self) -> i64 { return self.a; }
}
fn main() {
    println(new Leaf(11, 22, 33).root_value());
    println(new Middle(11, 22).root_value());
    println(new Root(11).root_value());
}
"#,
        "11\n11\n11\n",
    );
}

/// Perspective 16. Layout and virtual dispatch are separate tables built by
/// separate passes (willow-fm7t). Reverse declaration must leave both correct,
/// not just whichever one is being tested.
#[test]
fn layout_16_virtual_dispatch_is_unaffected_by_declaration_order() {
    assert_project_output(
        r#"
class Leaf extends Middle {
    pub c: i64;
    pub override fn total(self) -> i64 { return self.a + self.b + self.c; }
}
open class Middle extends Root {
    pub b: i64;
    pub open override fn total(self) -> i64 { return self.a + self.b; }
}
open class Root {
    pub a: i64;
    pub open fn total(self) -> i64 { return self.a; }
}
fn sum(r: Root) -> i64 { return r.total(); }
fn main() {
    println(sum(new Leaf(1, 2, 3)));
    println(sum(new Middle(1, 2)));
    println(sum(new Root(1)));
}
"#,
        "6\n3\n1\n",
    );
}

/// Perspective 17. An imported module carries its own hierarchy, registered by
/// the module path rather than the entry path. A cross-module hierarchy can
/// arrive subclass-first too, so that path needs the same treatment.
#[test]
fn layout_17_an_imported_module_hierarchy_declared_subclass_first() {
    let project = TestProject::new(
        "layout_module",
        &[
            (
                "shapes.wi",
                r#"
pub class Cube extends Rect {
    pub depth: i64;
    pub fn volume(self) -> i64 { return self.width * self.height * self.depth; }
}
pub open class Rect extends Sized { pub height: i64; }
pub open class Sized { pub width: i64; }
"#,
            ),
            (
                "main.wi",
                r#"
import shapes::Cube;
import shapes::Rect;

fn main() {
    let c = new Cube(2, 3, 4);
    println(c.volume());
    println(c.width);
    println(c.height);
    println(c.depth);
    println(new Rect(5, 6).height);
}
"#,
            ),
        ],
    );
    assert_project_runs(&project, "24\n2\n3\n4\n6\n");
}

/// Perspective 18. A directly imported class arrives under two spellings —
/// `shapes::Sized` and the local `Sized` — that are ONE runtime class. The
/// own-field table has to be aliased alongside the layout, or finalizing the
/// alias walks a chain whose ancestors have no fields recorded.
#[test]
fn layout_18_an_aliased_import_of_a_reverse_declared_base() {
    let project = TestProject::new(
        "layout_alias",
        &[
            (
                "shapes.wi",
                r#"
pub class Cube extends Rect {
    pub depth: i64;
}
pub open class Rect extends Sized { pub height: i64; }
pub open class Sized { pub width: i64; }
"#,
            ),
            (
                "main.wi",
                r#"
import shapes::Cube;
import shapes::Sized;

fn width_of(s: Sized) -> i64 { return s.width; }

fn main() {
    println(width_of(new Cube(2, 3, 4)));
    println(width_of(new Sized(9)));
}
"#,
            ),
        ],
    );
    assert_project_runs(&project, "2\n9\n");
}

/// Perspective 19. A downcast reads the runtime class through the descriptor
/// and then reads fields through the DOWNCAST type's layout, so it depends on
/// both tables being right for a reverse-declared hierarchy. The scrutinee is
/// an interface, which is what a class downcast pattern requires (E1205).
#[test]
fn layout_19_a_downcast_in_a_reverse_declared_hierarchy() {
    assert_project_output(
        r#"
interface Countable { fn count(self) -> i64; }
class Leaf extends Middle {
    pub c: i64;
}
open class Middle extends Root { pub b: i64; }
open class Root implements Countable {
    pub a: i64;
    pub fn count(self) -> i64 { return self.a; }
}
fn describe(item: Countable) -> i64 {
    return match item {
        Leaf(leaf) => leaf.a + leaf.b + leaf.c,
        Middle(mid) => mid.a + mid.b,
        _ => item.count(),
    };
}
fn main() {
    println(describe(new Leaf(1, 2, 3)));
    println(describe(new Middle(1, 2)));
    println(describe(new Root(1)));
}
"#,
        "6\n3\n1\n",
    );
}

/// Perspective 20. An array of the base type stores subclass instances. Every
/// element is read through the BASE's layout, so one wrong subclass layout
/// misreads only its own elements.
#[test]
fn layout_20_an_array_of_a_base_type_holds_reverse_declared_subclasses() {
    assert_project_output(
        r#"
import std::collections::Array;
class Leaf extends Middle {
    pub c: i64;
}
class Twig extends Middle {
    pub t: i64;
}
open class Middle extends Root { pub b: i64; }
open class Root { pub a: i64; }
fn main() {
    let items: Array<Root> = [
        new Leaf(1, 2, 3),
        new Twig(4, 5, 6),
        new Middle(7, 8),
        new Root(9),
    ];
    let mut i = 0;
    let mut total = 0;
    while i < items.len() {
        total = total + items[i].a;
        i = i + 1;
    }
    println(total);
}
"#,
        "21\n",
    );
}

/// Perspective 21. A static field belongs to the class, not to an instance, so
/// it must not consume a layout slot at any level of a reverse-declared chain.
#[test]
fn layout_21_a_static_field_stays_out_of_the_instance_layout() {
    assert_project_output(
        r#"
class Leaf extends Middle {
    pub static leaf_count: i64 = 7;
    pub c: i64;
    pub fn sum(self) -> i64 { return self.a + self.b + self.c; }
}
open class Middle extends Root {
    pub static middle_count: i64 = 5;
    pub b: i64;
}
open class Root { pub a: i64; }
fn main() {
    println(new Leaf(1, 2, 3).sum());
    println(Leaf::leaf_count);
    println(Middle::middle_count);
}
"#,
        "6\n7\n5\n",
    );
}

/// Perspective 22. Every level's field read back individually, in a chain
/// declared in an order that is neither root-first nor leaf-first. A pass that
/// merges the right SET of fields in the wrong ORDER passes perspective 3's sum
/// and fails here.
#[test]
fn layout_22_each_level_contributes_its_field_in_root_down_order() {
    assert_project_output(
        r#"
open class M2 extends M1 { pub f3: i64; }
class Leaf extends M3 {
    pub f5: i64;
    pub fn show(self) -> String {
        return self.f1.toString() + self.f2.toString() + self.f3.toString()
             + self.f4.toString() + self.f5.toString();
    }
}
open class M1 extends Root { pub f2: i64; }
open class M3 extends M2 { pub f4: i64; }
open class Root { pub f1: i64; }
fn main() {
    println(new Leaf(1, 2, 3, 4, 5).show());
}
"#,
        "12345\n",
    );
}

/// Perspective 23. The runnable example. Its exact output is also pinned by the
/// runnable-examples table in `runtime.rs`.
#[test]
fn layout_23_the_example_program_runs() {
    let source = std::fs::read_to_string("example/class_layout_declaration_order.wi")
        .expect("example/class_layout_declaration_order.wi is missing");
    assert_project_output(&source, "6\n1\n2\n3\n1001\n2,4,8\n");
}
