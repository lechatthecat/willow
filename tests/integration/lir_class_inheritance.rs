//! Inheritance, virtual dispatch and class downcast on the LIR-walking backend
//! (willow-0g8j.2.4).
//!
//! Before this change any class that took part in inheritance was outside the
//! walker's subset, which took its instances, its base-typed slots, its arrays
//! and its downcast match arms with it. Admitting them rests on four facts,
//! and every perspective here is a way of getting one of them wrong:
//!
//!   * LAYOUT — a subclass's fields are its base's followed by its own, so a
//!     base-typed slot reads the same offsets whichever subclass it holds;
//!   * DISPATCH — word 0 of an object is its class descriptor (`type_id`, then
//!     one word per virtual slot), and which slot a call reads is decided by
//!     `plan_virtual_call`;
//!   * WIDENING — a subclass value in a base-typed slot is a no-op at run time,
//!     both being object pointers;
//!   * DOWNCAST — a class arm on an interface scrutinee compares the boxed
//!     object's `type_id` EXACTLY, so a descendant of the arm's class does not
//!     match it.
//!
//! Since willow-0g8j.3 a body outside the walker's subset is a compile error,
//! so a run that prints the right answer is proof the walker produced it —
//! the failure mode these tests exist to rule out is a walker that gets
//! dispatch wrong, not one that quietly declines the body.
//!
//! Perspectives:
//!   1. a base-typed parameter selects the subclass override
//!   2. a subclass that overrides nothing inherits across two levels
//!   3. an inherited non-virtual method makes VIRTUAL self-calls
//!   4. a base-typed local reassigned across the hierarchy
//!   5. a base-typed return holding a subclass value
//!   6. a five-level chain resolves to the NEAREST override, not the root
//!   7. sibling branches never cross
//!   8. a virtual method that takes arguments and returns a String
//!   9. an array of a base type, built from a literal of subclasses
//!  10. `push` widens a subclass onto an `Array<Base>`
//!  11. an inherited field read through a base-typed slot
//!  12. a field WRITE through a base-typed slot lands in the inherited slot
//!  13. a subclass's own field, past the inherited ones
//!  14. a base-typed FIELD holding a subclass instance
//!  15. the implicit memberwise constructor fills inherited fields first
//!  16. an explicit `init` delegating with `super.init`
//!  17. a `prot` member reached from a subclass method
//!  18. boxing a subclass into an interface its BASE implements
//!  19. a downcast arm matches its exact class
//!  20. a DESCENDANT of the arm's class falls through to `_`
//!  21. the downcast binding is the concrete object, unboxed
//!  22. downcast arms are disjoint, so their order does not matter
//!  23. an inherited static property, read through a subclass name
//!  24. an inherited static read from inside a method, against a same-named
//!      static on an unrelated class
//!  25. the whole hierarchy under GC stress
//!  26. `example/lir_class_inheritance.wi` compiles with no fallback at all

use super::support::{compile_with_compiler_env, compile_with_env_and_run};

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

/// A base with two virtual slots, a subclass replacing slot 0, a grandchild
/// replacing nothing and an unrelated branch. Reused by the perspectives that
/// only vary the call site.
const HIERARCHY: &str = r#"
open class Shape {
    pub size: i64;
    pub open fn area(self) -> i64 { return self.size; }
    pub open fn label(self) -> String { return "shape"; }
    pub fn report(self) -> String { return self.label() + "=" + self.area().toString(); }
}
open class Square extends Shape {
    pub open override fn area(self) -> i64 { return self.size * self.size; }
}
class Tile extends Square {}
class Circle extends Shape {
    pub override fn area(self) -> i64 { return self.size * self.size * 3; }
    pub override fn label(self) -> String { return "circle"; }
}
"#;

// 1. the whole point: a base-typed parameter is handed a subclass instance and
// the call reads the descriptor slot of whatever actually arrived. Binding the
// callee to `Shape::area` by the parameter's STATIC type prints 5 5 5 5.
#[test]
fn lir_inh_01_base_typed_parameter_selects_the_override() {
    assert_project_output(
        &format!(
            "{HIERARCHY}
fn area_of(s: Shape) -> i64 {{ return s.area(); }}
fn main() {{
    println(area_of(new Shape(5)));
    println(area_of(new Square(5)));
    println(area_of(new Circle(5)));
}}"
        ),
        "5\n25\n75\n",
    );
}

// 2. `Tile` overrides nothing, so both of its slots hold the addresses
// `Square` handed down: `area` is `Square::area` two levels up the chain and
// `label` is `Shape::label` three. Inheritance is the ABSENCE of a redirect,
// not a walk performed at the call site.
#[test]
fn lir_inh_02_grandchild_inherits_without_a_redirect() {
    assert_project_output(
        &format!(
            "{HIERARCHY}
fn area_of(s: Shape) -> i64 {{ return s.area(); }}
fn label_of(s: Shape) -> String {{ return s.label(); }}
fn main() {{
    println(area_of(new Tile(4)));
    println(label_of(new Tile(4)));
}}"
        ),
        "16\nshape\n",
    );
}

// 3. `report` is neither `open` nor `override`, so it has no slot and its
// callee is fixed at compile time — but the `self.label()` and `self.area()`
// INSIDE it are virtual, so a subclass that inherits it runs its own bodies.
// Devirtualizing the inner calls to the enclosing class would print
// `shape=3` for every receiver.
#[test]
fn lir_inh_03_inherited_method_makes_virtual_self_calls() {
    assert_project_output(
        &format!(
            "{HIERARCHY}
fn report_of(s: Shape) -> String {{ return s.report(); }}
fn main() {{
    println(report_of(new Shape(3)));
    println(report_of(new Square(3)));
    println(report_of(new Tile(3)));
    println(report_of(new Circle(3)));
}}"
        ),
        "shape=3\nshape=9\nshape=9\ncircle=27\n",
    );
}

// 4. one Cranelift variable, three runtime classes. The variable's type is the
// BASE, so every store into it widens and every read dispatches — a per-slot
// devirtualization would freeze the first class assigned.
#[test]
fn lir_inh_04_base_typed_local_reassigned() {
    assert_project_output(
        &format!(
            "{HIERARCHY}
fn main() {{
    let mut current: Shape = new Square(6);
    println(current.area());
    current = new Tile(6);
    println(current.area());
    current = new Circle(2);
    println(current.area());
    current = new Shape(6);
    println(current.area());
}}"
        ),
        "36\n36\n12\n6\n",
    );
}

// 5. a base-typed RETURN. The widening happens at the `return`, where the
// value is a subclass and the slot is the base; at run time it is nothing at
// all, which is exactly why it may be admitted.
#[test]
fn lir_inh_05_base_typed_return_holds_a_subclass() {
    assert_project_output(
        &format!(
            "{HIERARCHY}
fn pick(n: i64) -> Shape {{
    if n == 0 {{ return new Square(4); }}
    if n == 1 {{ return new Tile(3); }}
    return new Circle(2);
}}
fn main() {{
    println(pick(0).area());
    println(pick(1).area());
    println(pick(2).area());
}}"
        ),
        "16\n9\n12\n",
    );
}

// 6. five levels, with the definition of `depth` at levels 1, 3 and 5. Each
// receiver must reach the NEAREST definition at or above it, which is what a
// single slot holding the resolved address gives; a chain that stopped at the
// root would print 1 five times.
#[test]
fn lir_inh_06_deep_chain_resolves_to_the_nearest_override() {
    assert_project_output(
        r#"
open class L1 { pub open fn depth(self) -> i64 { return 1; } }
open class L2 extends L1 {}
open class L3 extends L2 { pub open override fn depth(self) -> i64 { return 3; } }
open class L4 extends L3 {}
class L5 extends L4 { pub override fn depth(self) -> i64 { return 5; } }
fn depth_of(x: L1) -> i64 { return x.depth(); }
fn main() {
    println(depth_of(new L1()));
    println(depth_of(new L2()));
    println(depth_of(new L3()));
    println(depth_of(new L4()));
    println(depth_of(new L5()));
}
"#,
        "1\n1\n3\n3\n5\n",
    );
}

// 7. two branches off one base, each overriding the same slot. They share an
// index and nothing else — a descriptor built per class rather than per
// hierarchy level is what keeps them apart.
#[test]
fn lir_inh_07_sibling_branches_never_cross() {
    assert_project_output(
        &format!(
            "{HIERARCHY}
fn area_of(s: Shape) -> i64 {{ return s.area(); }}
fn main() {{
    let mut i = 0;
    while i < 3 {{
        println(area_of(new Square(2)));
        println(area_of(new Circle(2)));
        i = i + 1;
    }}
}}"
        ),
        "4\n12\n4\n12\n4\n12\n",
    );
}

// 8. the indirect call has to carry the method's real ABI: two arguments and a
// GC-managed return. A signature built from the wrong slot would pass the
// arguments to a function that does not take them.
#[test]
fn lir_inh_08_virtual_method_with_arguments_and_string_return() {
    assert_project_output(
        r#"
open class Tag {
    pub open fn render(self, prefix: String, n: i64) -> String {
        return prefix + "base" + n.toString();
    }
}
class Loud extends Tag {
    pub override fn render(self, prefix: String, n: i64) -> String {
        return prefix + "LOUD" + (n * 2).toString();
    }
}
fn render_with(t: Tag) -> String { return t.render("<", 3) + ">"; }
fn main() {
    println(render_with(new Tag()));
    println(render_with(new Loud()));
}
"#,
        "<base3>\n<LOUD6>\n",
    );
}

// 9. an ANNOTATED array literal takes the annotation's element type, so this is
// an `Array<Shape>` whose elements each widen — not an `Array<Square>` holding
// a `Circle`, which is the type the lowering used to hand the walker.
#[test]
fn lir_inh_09_array_of_a_base_type_from_subclass_literals() {
    assert_project_output(
        &format!(
            "import std::collections::Array;
{HIERARCHY}
fn total(xs: Array<Shape>) -> i64 {{
    let mut t = 0;
    let mut i = 0;
    while i < xs.len() {{ t = t + xs[i].area(); i = i + 1; }}
    return t;
}}
fn main() {{
    let shapes: Array<Shape> = [new Square(2), new Tile(3), new Circle(1), new Shape(7)];
    println(shapes.len());
    println(total(shapes));
    println(shapes[1].label());
}}"
        ),
        "4\n23\nshape\n",
    );
}

// 10. the same widening through `push` rather than a literal: the element slot
// is the base type and the argument is a subclass.
#[test]
fn lir_inh_10_push_widens_onto_an_array_of_the_base() {
    assert_project_output(
        &format!(
            "import std::collections::Array;
{HIERARCHY}
fn main() {{
    let xs: Array<Shape> = [new Shape(5)];
    xs.push(new Square(3));
    xs.push(new Tile(4));
    let mut t = 0;
    let mut i = 0;
    while i < xs.len() {{ t = t + xs[i].area(); i = i + 1; }}
    println(t);
}}"
        ),
        "30\n",
    );
}

// 11. a field read through a base-typed slot. `size` is field 0 of `Shape` and
// of every descendant because a subclass EXTENDS the layout rather than
// rearranging it — the property that makes the offset safe to hard-code.
#[test]
fn lir_inh_11_inherited_field_read_through_a_base_slot() {
    assert_project_output(
        &format!(
            "{HIERARCHY}
fn size_of(s: Shape) -> i64 {{ return s.size; }}
fn main() {{
    println(size_of(new Shape(1)));
    println(size_of(new Square(2)));
    println(size_of(new Tile(3)));
    println(size_of(new Circle(4)));
}}"
        ),
        "1\n2\n3\n4\n",
    );
}

// 12. the same offset from the writing side, then read back through the
// subclass's own type: one slot, not a base copy and a subclass copy.
#[test]
fn lir_inh_12_field_write_through_a_base_slot() {
    assert_project_output(
        &format!(
            "{HIERARCHY}
fn grow(s: Shape) {{ s.size = s.size + 10; }}
fn main() {{
    let t = new Tile(3);
    grow(t);
    println(t.size);
    println(t.area());
}}"
        ),
        "13\n169\n",
    );
}

// 13. a subclass's OWN field sits after the inherited ones. Numbering a
// subclass's fields from zero would alias `extra` onto `size`.
#[test]
fn lir_inh_13_subclass_own_field_follows_the_inherited_ones() {
    assert_project_output(
        r#"
open class Base { pub a: i64; pub b: i64; }
class Sub extends Base { pub c: i64; pub d: i64; }
fn main() {
    let s = new Sub(1, 2, 3, 4);
    println(s.a);
    println(s.b);
    println(s.c);
    println(s.d);
}
"#,
        "1\n2\n3\n4\n",
    );
}

// 14. a base-typed FIELD holding a subclass instance: the widening is checked
// at the store, and the read dispatches on what is actually there.
#[test]
fn lir_inh_14_base_typed_field_holds_a_subclass() {
    assert_project_output(
        &format!(
            "{HIERARCHY}
class Frame {{ pub inner: Shape; }}
fn main() {{
    let f = new Frame(new Square(5));
    println(f.inner.area());
    f.inner = new Circle(2);
    println(f.inner.area());
}}"
        ),
        "25\n12\n",
    );
}

// 15. the implicit memberwise constructor fills the INHERITED fields first and
// the subclass's own after — the same order the layout is built in. Filling
// only the class's own declarations would leave `size` at zero.
#[test]
fn lir_inh_15_memberwise_new_fills_inherited_fields_first() {
    assert_project_output(
        r#"
open class Base { pub a: i64; }
open class Mid extends Base { pub b: i64; }
class Leaf extends Mid { pub c: i64; }
fn main() {
    let l = new Leaf(7, 8, 9);
    println(l.a + l.b * 10 + l.c * 100);
}
"#,
        "987\n",
    );
}

// 16. an explicit constructor delegating with `super.init`, so the inherited
// slots are written by the base's own code rather than positionally.
#[test]
fn lir_inh_16_explicit_init_delegates_to_super() {
    assert_project_output(
        r#"
open class Animal {
    pub name: String;
    pub energy: i64;
    pub init(self, name: String, energy: i64) {
        self.name = name;
        self.energy = energy;
    }
    pub open fn act(self) -> i64 { return self.energy - 1; }
}
class Dog extends Animal {
    pub init(self, name: String) { super.init(name, 20); }
    pub override fn act(self) -> i64 { return self.energy - 2; }
}
fn act_of(a: Animal) -> i64 { return a.act(); }
fn main() {
    let d = new Dog("rex");
    println(d.name);
    println(d.energy);
    println(act_of(d));
    println(act_of(new Animal("generic", 10)));
}
"#,
        "rex\n20\n18\n9\n",
    );
}

// 17. `prot` is a visibility rule, not a representation: a subclass method
// reaching an inherited protected field and a protected helper compiles the
// same way any other inherited access does.
#[test]
fn lir_inh_17_protected_members_reached_from_a_subclass() {
    assert_project_output(
        r#"
pub open class Animal {
    pub name: String;
    prot energy: i64;
    pub init(self, name: String, energy: i64) {
        self.name = name;
        self.energy = energy;
    }
    prot fn consume(self, amount: i64) -> i64 { return self.energy - amount; }
    pub open fn act(self) -> i64 { return self.consume(1); }
}
pub class Dog extends Animal {
    pub init(self, name: String, energy: i64) { super.init(name, energy); }
    pub fn run_cost(self) -> i64 { return self.consume(3); }
    pub override fn act(self) -> i64 { return self.consume(2); }
}
fn main() {
    let d = new Dog("rex", 20);
    println(d.act());
    println(d.run_cost());
}
"#,
        "18\n17\n",
    );
}

// 18. `Puppy` does not re-declare `implements Animal`, yet it IS one: boxing
// reads the (concrete class, interface) vtable, which exists per class no
// matter where in a hierarchy the `implements` clause was written.
#[test]
fn lir_inh_18_boxing_a_subclass_into_its_bases_interface() {
    assert_project_output(
        r#"
interface Animal {
    fn name(self) -> String;
    fn legs(self) -> i64;
}
open class Dog implements Animal {
    pub open fn name(self) -> String { return "dog"; }
    pub fn legs(self) -> i64 { return 4; }
}
class Puppy extends Dog {
    pub override fn name(self) -> String { return "puppy"; }
}
fn describe(a: Animal) -> String { return a.name() + "/" + a.legs().toString(); }
fn main() {
    println(describe(new Dog()));
    println(describe(new Puppy()));
}
"#,
        "dog/4\npuppy/4\n",
    );
}

// 19. the downcast itself: the scrutinee is an interface box whose word 0 is
// the concrete object, and the arm compares that object's `type_id` against
// the arm's class.
#[test]
fn lir_inh_19_downcast_arm_matches_its_exact_class() {
    assert_project_output(
        r#"
interface Animal { fn name(self) -> String; }
class Dog implements Animal {
    pub fn name(self) -> String { return "rex"; }
    pub fn bark(self) -> String { return "woof"; }
}
class Cat implements Animal {
    pub fn name(self) -> String { return "tom"; }
    pub fn meow(self) -> String { return "meow"; }
}
class Fish implements Animal { pub fn name(self) -> String { return "nemo"; } }
fn sound(a: Animal) -> String {
    return match a {
        Dog(d) => d.bark(),
        Cat(c) => c.meow(),
        _ => a.name() + " is quiet",
    };
}
fn main() {
    println(sound(new Dog()));
    println(sound(new Cat()));
    println(sound(new Fish()));
}
"#,
        "woof\nmeow\nnemo is quiet\n",
    );
}

// 20. EXACT, not "is a descendant of": a `Puppy` is a `Dog`, but the `Dog` arm
// does not take it. An arm test that walked the base chain would silently
// change which arm runs.
#[test]
fn lir_inh_20_a_descendant_does_not_match_the_base_arm() {
    assert_project_output(
        r#"
interface Animal { fn name(self) -> String; }
open class Dog implements Animal { pub open fn name(self) -> String { return "dog"; } }
class Puppy extends Dog { pub override fn name(self) -> String { return "puppy"; } }
fn which(a: Animal) -> String {
    return match a {
        Dog(d) => "exactly a dog: " + d.name(),
        Puppy(p) => "exactly a puppy: " + p.name(),
        _ => "other",
    };
}
fn main() {
    println(which(new Dog()));
    println(which(new Puppy()));
}
"#,
        "exactly a dog: dog\nexactly a puppy: puppy\n",
    );
}

// 21. the binding is the object UNBOXED, so a method the interface does not
// declare is callable on it — and calling one is the only way to observe that
// the arm bound the object rather than the box.
#[test]
fn lir_inh_21_downcast_binding_is_the_unboxed_object() {
    assert_project_output(
        r#"
interface Animal { fn name(self) -> String; }
open class Dog implements Animal {
    pub tag: i64;
    pub open fn name(self) -> String { return "dog"; }
    pub fn tag_twice(self) -> i64 { return self.tag * 2; }
}
class Puppy extends Dog { pub override fn name(self) -> String { return "puppy"; } }
fn info(a: Animal) -> String {
    return match a {
        Puppy(p) => p.name() + ":" + p.tag_twice().toString() + ":" + p.tag.toString(),
        _ => "?",
    };
}
fn main() { println(info(new Puppy(21))); }
"#,
        "puppy:42:21\n",
    );
}

// 22. arms select on disjoint `type_id`s, so reordering them cannot change the
// answer. A test that had drifted into an ancestry check would fail here the
// moment `Puppy` moved above `Dog`.
#[test]
fn lir_inh_22_downcast_arm_order_is_irrelevant() {
    assert_project_output(
        r#"
interface Animal { fn name(self) -> String; }
open class Dog implements Animal { pub open fn name(self) -> String { return "dog"; } }
class Puppy extends Dog { pub override fn name(self) -> String { return "puppy"; } }
fn dog_first(a: Animal) -> String {
    return match a { Dog(d) => "D", Puppy(p) => "P", _ => "?" };
}
fn puppy_first(a: Animal) -> String {
    return match a { Puppy(p) => "P", Dog(d) => "D", _ => "?" };
}
fn main() {
    println(dog_first(new Dog()) + dog_first(new Puppy()));
    println(puppy_first(new Dog()) + puppy_first(new Puppy()));
}
"#,
        "DP\nDP\n",
    );
}

// 23. a static property is resolved by TYPE NAME through the hierarchy, never
// through a descriptor: `Tile::unit` is the storage `Shape` declared, reached
// by the same ancestry walk the emitter uses to find the data slot.
#[test]
fn lir_inh_23_inherited_static_property_through_a_subclass_name() {
    assert_project_output(
        r#"
open class Base {
    pub static kind: String = "base";
    pub static limit: i64 = 7;
}
open class Mid extends Base { pub static own: i64 = 3; }
class Leaf extends Mid {}
fn main() {
    println(Base::kind);
    println(Mid::kind);
    println(Leaf::kind);
    println(Leaf::limit + Leaf::own);
}
"#,
        "base\nbase\nbase\n10\n",
    );
}

// 24. the same resolution from INSIDE a method, and against an unrelated class
// declaring the same static NAME. The read is answered by the ancestry walk the
// emitter uses to find the data slot, so `Widget::kind` is `Base`'s storage and
// `Other::kind` is its own — matching on the name alone would confuse them.
#[test]
fn lir_inh_24_inherited_static_read_resolves_through_ancestry() {
    assert_project_output(
        r#"
open class Base {
    pub static kind: String = "base";
    pub static limit: i64 = 7;
}
class Widget extends Base { pub static count: i64 = 3; }
class Other { pub static kind: String = "other"; }
class Reader {
    pub fn read(self) -> String {
        return Widget::kind + "/" + Other::kind + "/" + Widget::limit.toString();
    }
}
fn main() {
    println(new Reader().read());
    println(Widget::count);
}
"#,
        "base/other/7\n3\n",
    );
}

// 25. under allocation-on-every-request GC stress the receiver is only
// reachable through a shadow-stack root while the arguments are evaluated and
// the slot is loaded, or a collection between the two moves the object out
// from under the indirect call.
#[test]
fn lir_inh_25_hierarchy_survives_gc_stress() {
    let source = format!(
        "import std::collections::Array;
{HIERARCHY}
fn total(xs: Array<Shape>) -> i64 {{
    let mut t = 0;
    let mut i = 0;
    while i < xs.len() {{ t = t + xs[i].area(); i = i + 1; }}
    return t;
}}
fn main() {{
    let mut round = 0;
    while round < 20 {{
        let xs: Array<Shape> = [new Square(2), new Tile(3), new Circle(1), new Shape(7)];
        xs.push(new Square(round));
        println(total(xs));
        println(xs[0].report());
        round = round + 1;
    }}
}}"
    );
    let stress = [("WILLOW_GC_STRESS", "alloc")];
    let (out, ok) = super::support::compile_with_env_and_run_under(&source, &PLAIN, &stress);
    assert!(ok, "run under GC stress failed: {out}");
    // Two lines per round: `4 + 9 + 3 + 7 + round*round`, then the report of
    // `xs[0]`, which is the same `Square(2)` every time.
    let expected: String = (0..20)
        .map(|round: i64| format!("{}\nshape=4\n", 23 + round * round))
        .collect();
    assert_eq!(out, expected);
}

// 26. the example is the readable statement of all of the above, so it must
// compile with EVERY free function on the walker path — otherwise its own
// header claim is unchecked.
#[test]
fn lir_inh_26_example_is_fully_lir() {
    let source = include_str!("../../example/lir_class_inheritance.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &PLAIN);
    assert!(
        ok,
        "example/lir_class_inheritance.wi must compile with every free function \
         on the LIR path: {stderr}"
    );
}
