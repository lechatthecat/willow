use super::*;

// ── Cross-module generic interface with a class type argument (willow-jz15.45) ──
//
// `class Big implements Pred<Item>` in one module and `let q: Pred<Item> = new
// Big()` in another spell `Item` differently (bare item import vs. the
// defining unit's own name). `class_implements_interface` canonicalized only
// the interface name and compared type arguments by spelling, so the class was
// rejected with E0201. Type arguments are now compared by declaration identity.
//
// Test perspectives (each pinned below; a test may cover several):
//   P1  2 modules: the ticket repro (`let` binding + method call) prints true
//   P2  3 modules: interface+implementor in one, class argument in another
//   P3  3 modules: interface, class argument and implementor all separate
//   P4  implementor spells the argument qualified (`items::Item`), entry bare
//   P5  entry spells interface, argument and class fully qualified
//   P6  `Pred<Item>` as a function parameter
//   P7  `Pred<Item>` as a function return type
//   P8  `Array<Pred<Item>>` literal of two implementors dispatches per element
//   P9  subclass in the defining module inherits `implements Pred<Item>`
//   P10 nested argument `Pred<Array<Item>>`
//   P11 enum type argument from another module (`Pred<Color>`)
//   P12 interface type argument from another module (`Pred<Shape>`)
//   P13 two type arguments (`Pair<Item, String>`)
//   P14 generic-enum-wrapped argument (`Pred<Option<Item>>`)
//   P15 downcast match on a `Pred<Item>` scrutinee to its implementor
//   P16 regression: primitive argument across modules (`Pred<i64>`)
//   P17 negative: a different class argument (`Pred<Other>`) → E0201
//   P18 negative: a LOCAL class named `Item` is a different type → E0201
//   P19 negative: swapped arguments (`Pair<String, Item>`) → E0201
//   P20 negative: nested vs. flat argument (`Pred<Array<Item>>`) → E0201
//   P21 negative: downcast to a class not implementing `Pred<Item>` → E0415

const ITEMS: &str = r#"
pub class Item { pub n: i64; }
pub class Other { pub n: i64; }
pub enum Color { Red, Blue }
pub interface Shape { fn area(self) -> i64; }
pub class Sq implements Shape {
    pub s: i64;
    pub fn area(self) -> i64 { return self.s * self.s; }
}
"#;

const PREDS: &str = r#"
import std::collections::Array;
import items::Item;
import items::Color;
import items::Shape;
pub interface Pred<T> { fn test(self, x: T) -> bool; }
pub interface Pair<A, B> { fn both(self, a: A, b: B) -> i64; }
pub open class Big implements Pred<Item> {
    pub fn test(self, x: Item) -> bool { return x.n > 5; }
}
pub class Small implements Pred<Item> {
    pub fn test(self, x: Item) -> bool { return x.n < 5; }
}
pub class Bigger extends Big { }
pub class Many implements Pred<Array<Item>> {
    pub fn test(self, xs: Array<Item>) -> bool { return xs.len() > 1; }
}
pub class IsRed implements Pred<Color> {
    pub fn test(self, c: Color) -> bool {
        return match c { Color::Red => true, Color::Blue => false };
    }
}
pub class Wide implements Pred<Shape> {
    pub fn test(self, s: Shape) -> bool { return s.area() > 10; }
}
pub class Mixed implements Pair<Item, String> {
    pub fn both(self, a: Item, b: String) -> i64 { return a.n + b.len(); }
}
pub class Present implements Pred<Option<Item>> {
    pub fn test(self, o: Option<Item>) -> bool {
        return match o { Option::Some(i) => i.n > 0, Option::None => false };
    }
}
pub class Positive implements Pred<i64> {
    pub fn test(self, x: i64) -> bool { return x > 0; }
}
"#;

fn preds_error(main: &str) -> String {
    compile_temp_project_error_stderr(
        &[("items.wi", ITEMS), ("preds.wi", PREDS), ("main.wi", main)],
        "main.wi",
    )
}

// P1: the ticket's two-module reproduction.
#[test]
fn gicm_01_two_module_repro() {
    let p = r#"
pub interface Pred<T> { fn test(self, x: T) -> bool; }
pub class Item { pub n: i64; }
pub class Big implements Pred<Item> { pub fn test(self, x: Item) -> bool { return x.n > 5; } }
"#;
    let main = r#"
import p::Pred;
import p::Item;
import p::Big;
fn main() { let q: Pred<Item> = new Big(); println(q.test(new Item(7))); }
"#;
    let (out, ok) = compile_temp_project_and_run(&[("p.wi", p), ("main.wi", main)], "main.wi");
    assert!(ok, "two-module generic interface failed: {out}");
    assert_eq!(out, "true\n");
}

// P2, P3, P4: the class argument lives in a third module; the interface and
// implementor share a module or are split, and the implementor spells the
// argument bare or qualified.
#[test]
fn gicm_02_three_module_layouts() {
    let iface = r#"
pub interface Pred<T> { fn test(self, x: T) -> bool; }
"#;
    let impls = r#"
import iface::Pred;
import items::Item;
import items;
pub class Big implements Pred<Item> {
    pub fn test(self, x: Item) -> bool { return x.n > 5; }
}
pub class Small implements Pred<items::Item> {
    pub fn test(self, x: items::Item) -> bool { return x.n < 5; }
}
"#;
    let main = r#"
import iface::Pred;
import items::Item;
import impls::Big;
import impls::Small;
import preds;
fn main() {
    let b: Pred<Item> = new Big();
    let s: Pred<Item> = new Small();
    let c: preds::Pred<Item> = new preds::Big();
    println(b.test(new Item(7)));
    println(s.test(new Item(7)));
    println(c.test(new Item(3)));
}
"#;
    let (out, ok) = compile_temp_project_and_run(
        &[
            ("items.wi", ITEMS),
            ("preds.wi", PREDS),
            ("iface.wi", iface),
            ("impls.wi", impls),
            ("main.wi", main),
        ],
        "main.wi",
    );
    assert!(ok, "three-module generic interface failed: {out}");
    assert_eq!(out, "true\nfalse\nfalse\n");
}

// P5: every name in the entry is module-qualified.
#[test]
fn gicm_03_fully_qualified_entry() {
    let main = r#"
import preds;
import items;
fn main() {
    let q: preds::Pred<items::Item> = new preds::Big();
    println(q.test(new items::Item(9)));
}
"#;
    let (out, ok) = compile_temp_project_and_run(
        &[("items.wi", ITEMS), ("preds.wi", PREDS), ("main.wi", main)],
        "main.wi",
    );
    assert!(ok, "qualified generic interface failed: {out}");
    assert_eq!(out, "true\n");
}

// P6-P16: the instantiation in parameter, return, array, subclass, nested,
// enum, interface, two-argument, Option-wrapped, downcast and primitive
// positions.
#[test]
fn gicm_04_argument_shapes_and_positions() {
    let main = r#"
import std::collections::Array;
import preds::Pred;
import preds::Pair;
import preds::Big;
import preds::Small;
import preds::Bigger;
import preds::Many;
import preds::IsRed;
import preds::Wide;
import preds::Mixed;
import preds::Present;
import preds::Positive;
import items::Item;
import items::Color;
import items::Shape;
import items::Sq;
fn check(f: Pred<Item>, n: i64) -> bool { return f.test(new Item(n)); }
fn make() -> Pred<Item> { return new Small(); }
fn main() {
    println(check(new Big(), 9));
    println(make().test(new Item(1)));
    let fs: Array<Pred<Item>> = [new Big(), new Small()];
    for f in fs { println(f.test(new Item(9))); }
    let g: Pred<Item> = new Bigger();
    println(g.test(new Item(6)));
    let a: Pred<Array<Item>> = new Many();
    println(a.test([new Item(1), new Item(2)]));
    let c: Pred<Color> = new IsRed();
    println(c.test(Color::Red));
    let w: Pred<Shape> = new Wide();
    println(w.test(new Sq(4)));
    let m: Pair<Item, String> = new Mixed();
    println(m.both(new Item(2), "abc"));
    let o: Pred<Option<Item>> = new Present();
    println(o.test(Option::Some(new Item(1))));
    let d: Pred<Item> = new Big();
    println(match d { Big(b) => 1, _ => 0 });
    let i: Pred<i64> = new Positive();
    println(i.test(3));
}
"#;
    let (out, ok) = compile_temp_project_and_run(
        &[("items.wi", ITEMS), ("preds.wi", PREDS), ("main.wi", main)],
        "main.wi",
    );
    assert!(ok, "generic interface argument shapes failed: {out}");
    assert_eq!(
        out,
        "true\ntrue\ntrue\nfalse\ntrue\ntrue\ntrue\ntrue\n5\ntrue\n1\ntrue\n"
    );
}

// P17: a different class argument is a different instantiation.
#[test]
fn gicm_05_different_class_argument_rejected() {
    let stderr = preds_error(
        r#"
import preds::Pred;
import preds::Big;
import items::Other;
fn main() { let q: Pred<Other> = new Big(); println(1); }
"#,
    );
    assert!(stderr.contains("error[E0201]"), "stderr: {stderr}");
}

// P18: identity is by declaration, so a local class that merely shares the
// argument's name does not match.
#[test]
fn gicm_06_local_same_named_class_rejected() {
    let stderr = preds_error(
        r#"
import preds::Pred;
import preds::Big;
class Item { pub n: i64; }
fn main() { let q: Pred<Item> = new Big(); println(1); }
"#,
    );
    assert!(stderr.contains("error[E0201]"), "stderr: {stderr}");
}

// P19: argument order matters.
#[test]
fn gicm_07_swapped_arguments_rejected() {
    let stderr = preds_error(
        r#"
import preds::Pair;
import preds::Mixed;
import items::Item;
fn main() { let q: Pair<String, Item> = new Mixed(); println(1); }
"#,
    );
    assert!(stderr.contains("error[E0201]"), "stderr: {stderr}");
}

// P20: containers stay invariant -- `Pred<Item>` is not `Pred<Array<Item>>`.
#[test]
fn gicm_08_nested_versus_flat_rejected() {
    let stderr = preds_error(
        r#"
import std::collections::Array;
import preds::Pred;
import preds::Small;
import items::Item;
fn main() { let q: Pred<Array<Item>> = new Small(); println(1); }
"#,
    );
    assert!(stderr.contains("error[E0201]"), "stderr: {stderr}");
}

// P21: a downcast pattern to a class that does not implement the scrutinee's
// instantiation is still reported as unreachable.
#[test]
fn gicm_09_unrelated_downcast_rejected() {
    let stderr = preds_error(
        r#"
import preds::Pred;
import preds::Big;
import items::Item;
import items::Sq;
fn main() {
    let q: Pred<Item> = new Big();
    println(match q { Sq(s) => 1, _ => 0 });
}
"#,
    );
    assert!(stderr.contains("error[E0415]"), "stderr: {stderr}");
}
