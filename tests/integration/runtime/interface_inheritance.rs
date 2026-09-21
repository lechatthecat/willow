use super::*;

// ── Interface inheritance (willow-1js.2) ─────────────────────────────────────

#[test]
fn iface_inherit_01_class_usable_as_sub_and_super() {
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn name(self) -> String; }
interface Pet extends Animal { fn owner(self) -> String; }
class Dog implements Pet {
    pub fn name(self) -> String { return "Rex"; }
    pub fn owner(self) -> String { return "Sam"; }
}
fn as_animal(a: Animal) { println(a.name()); }
fn as_pet(p: Pet) { println(p.name() + "/" + p.owner()); }
fn main() {
    let d = new Dog();
    as_pet(d);
    as_animal(d);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "Rex/Sam\nRex\n");
}

#[test]
fn iface_inherit_02_sub_interface_value_as_super() {
    let (out, ok) = compile_and_run(
        r#"
interface Animal { fn name(self) -> String; }
interface Pet extends Animal { fn owner(self) -> String; }
class Dog implements Pet {
    pub fn name(self) -> String { return "Rex"; }
    pub fn owner(self) -> String { return "Sam"; }
}
fn as_animal(a: Animal) { println(a.name()); }
fn main() {
    let p: Pet = new Dog();
    as_animal(p);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "Rex\n");
}

#[test]
fn iface_inherit_03_missing_inherited_required_method_errors() {
    assert!(expect_compile_error(
        r#"
interface Animal { fn name(self) -> String; }
interface Pet extends Animal { fn owner(self) -> String; }
class Bad implements Pet { pub fn owner(self) -> String { return "x"; } }
fn main() {}
"#,
    ));
}

#[test]
fn iface_inherit_04_inherited_default_method() {
    let (out, ok) = compile_and_run(
        r#"
interface Named {
    fn name(self) -> String;
    fn label(self) -> String { return "name=" + self.name(); }
}
interface Pet extends Named { fn owner(self) -> String; }
class Dog implements Pet {
    pub fn name(self) -> String { return "Rex"; }
    pub fn owner(self) -> String { return "Sam"; }
}
fn main() {
    let p: Pet = new Dog();
    println(p.label());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "name=Rex\n");
}

#[test]
fn iface_inherit_05_transitive_three_levels() {
    let (out, ok) = compile_and_run(
        r#"
interface A { fn a(self) -> i64; }
interface B extends A { fn b(self) -> i64; }
interface C extends B { fn c(self) -> i64; }
class Impl implements C {
    pub fn a(self) -> i64 { return 1; }
    pub fn b(self) -> i64 { return 2; }
    pub fn c(self) -> i64 { return 3; }
}
fn sum_a(x: A) -> i64 { return x.a(); }
fn main() {
    let v: C = new Impl();
    println(sum_a(v) + v.b() + v.c());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}
