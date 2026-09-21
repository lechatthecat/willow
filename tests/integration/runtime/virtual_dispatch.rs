use super::*;

// ── Virtual dispatch for overridden methods (willow-ftk) ─────────────────────

#[test]
fn virtual_dispatch_01_override_via_base_ref() {
    let (out, ok) = compile_and_run(
        r#"
open class Animal { pub open fn sound(self) -> String { return "..."; } }
class Dog extends Animal { pub override fn sound(self) -> String { return "woof"; } }
class Cat extends Animal { pub override fn sound(self) -> String { return "meow"; } }
fn speak(a: Animal) { println(a.sound()); }
fn main() { speak(new Dog()); speak(new Cat()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "woof\nmeow\n");
}

#[test]
fn virtual_dispatch_02_base_method_calls_overridden_self() {
    // An inherited base method that calls self.m() dispatches to the override.
    let (out, ok) = compile_and_run(
        r#"
open class Animal {
    pub open fn sound(self) -> String { return "..."; }
    pub fn describe(self) -> String { return "I say " + self.sound(); }
}
class Dog extends Animal { pub override fn sound(self) -> String { return "woof"; } }
fn main() { println(new Dog().describe()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "I say woof\n");
}

#[test]
fn virtual_dispatch_03_inherited_non_override_dispatches_to_base() {
    // A subclass that does NOT override must dispatch to the inherited base
    // implementation (regression for the fall-through bug, willow-ftk).
    let (out, ok) = compile_and_run(
        r#"
open class Animal { pub open fn sound(self) -> String { return "base"; } }
class Mute extends Animal {}
fn speak(a: Animal) { println(a.sound()); }
fn main() { speak(new Mute()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "base\n");
}

#[test]
fn virtual_dispatch_04_three_levels_mixed_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
import std::collections::Array;
open class A {
    pub open fn kind(self) -> String { return "A"; }
    pub fn tag(self) -> String { return "[" + self.kind() + "]"; }
}
open class B extends A { pub open override fn kind(self) -> String { return "B"; } }
class C extends B { pub override fn kind(self) -> String { return "C"; } }
class D extends A {}
fn main() {
    let xs: Array<A> = [new A(), new B(), new C(), new D()];
    let mut i = 0;
    while i < xs.len() {
        println(xs[i].tag());
        i = i + 1;
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[A]\n[B]\n[C]\n[A]\n");
}
