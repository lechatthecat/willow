use super::*;

// ── Advanced interface features: cross-module inheritance, default methods,
//    generic interfaces, Self resolution (willow-1js.5 / .7 / .8) ──
//
// 23 test perspectives (each pinned below; a test may cover several):
//   P1  cross-module interface inheritance: entry class implements module Sub
//   P2  cross-module: a Sub value is usable where its Super is expected
//   P3  cross-module transitive inheritance (C extends B extends A)
//   P4  within-module inheritance: module class implements a module sub-interface
//   P5  multiple super-interfaces compose and dispatch through every parent
//   P6  interface `extends` cycle still rejected (E0423)
//   P7  cross-module default method inherited by an entry class
//   P8  cross-module default inherited by a class in ANOTHER module
//   P9  unimplemented interface default body is type-checked (bad body -> error)
//   P10 unimplemented interface default body that is valid -> no error
//   P11 ambiguous default from two independent interfaces -> E0425
//   P12 ambiguity resolved by an explicit class override -> ok
//   P13 same-named default where one interface extends the other -> sub wins
//   P14 no duplicate diagnostic for a bad non-generic default that IS implemented
//   P15 generic-interface default with type-arg substitution (`dup` returns i64)
//   P16 generic-interface default returning `Self`
//   P17 module-internal NON-generic interface param (entry imports fn, not iface)
//   P18 module-internal GENERIC interface param (entry imports fn, not iface)
//   P19 `Self` on a generic interface-typed receiver keeps type args (`Box<i64>`)
//   P20 qualified cross-module generic interface (`import m; m::Box<i64>`)
//   P21 direct-import cross-module generic interface (`import m::Box`)
//   P22 default body calls another (required) interface method via `self`
//   P23 cross-module default + inheritance together in one entry class
//   P24 multiple super-interfaces with conflicting inherited defaults -> E0425
//   P25 child interface default resolves inherited default ambiguity
//   P26 diamond inheritance of one shared default is not ambiguous
//   P27 cross-module inherited default ambiguity is rejected

// P1, P2: cross-module interface inheritance + usable-as-super.
#[test]
fn iface_adv_01_cross_module_inheritance() {
    let proto = r#"
module proto;
pub interface Named { fn name(self) -> i64; }
pub interface Greeter extends Named { fn greet(self) -> i64; }
"#;
    let main = r#"
import proto::Named;
import proto::Greeter;
class En implements Greeter {
    pub fn name(self) -> i64 { return 10; }
    pub fn greet(self) -> i64 { return 20; }
}
fn who(n: Named) -> i64 { return n.name(); }
fn main() {
    let g: Greeter = new En();
    println(g.greet());
    println(who(g));
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("proto.wi", proto), ("main.wi", main)], "main.wi");
    assert!(ok, "cross-module interface inheritance failed: {out}");
    assert_eq!(out, "20\n10\n");
}

// P3: transitive cross-module inheritance (C extends B extends A).
#[test]
fn iface_adv_02_cross_module_transitive_inheritance() {
    let proto = r#"
module proto;
pub interface A { fn a(self) -> i64; }
pub interface B extends A { fn b(self) -> i64; }
pub interface C extends B { fn c(self) -> i64; }
"#;
    let main = r#"
import proto::A;
import proto::C;
class Impl implements C {
    pub fn a(self) -> i64 { return 1; }
    pub fn b(self) -> i64 { return 2; }
    pub fn c(self) -> i64 { return 3; }
}
fn top(a: A) -> i64 { return a.a(); }
fn main() {
    let x: C = new Impl();
    println(top(x));
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("proto.wi", proto), ("main.wi", main)], "main.wi");
    assert!(ok, "transitive cross-module inheritance failed: {out}");
    assert_eq!(out, "1\n");
}

// P4: a class defined IN a module implements a module sub-interface and is used
// internally; the entry only calls a module function.
#[test]
fn iface_adv_03_within_module_inheritance() {
    let proto = r#"
module proto;
pub interface A { fn a(self) -> i64; }
pub interface B extends A { fn b(self) -> i64; }
pub class Impl implements B {
    pub fn a(self) -> i64 { return 7; }
    pub fn b(self) -> i64 { return 8; }
}
pub fn run() -> i64 {
    let x: B = new Impl();
    return x.a() + x.b();
}
"#;
    let main = r#"
import proto::run;
fn main() { println(run()); }
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("proto.wi", proto), ("main.wi", main)], "main.wi");
    assert!(ok, "within-module inheritance failed: {out}");
    assert_eq!(out, "15\n");
}

// P5: more than one super-interface composes all parent requirements; a class
// implementing the child is usable as each parent and as the child.
#[test]
fn iface_adv_04_multiple_supers_compose_and_dispatch() {
    let proto = r#"
module proto;
pub interface A { fn a(self) -> i64; }
pub interface B { fn b(self) -> i64; }
pub interface C extends A, B { fn c(self) -> i64; }
"#;
    let main = r#"
import proto::A;
import proto::B;
import proto::C;

class Impl implements C {
    pub fn a(self) -> i64 { return 1; }
    pub fn b(self) -> i64 { return 2; }
    pub fn c(self) -> i64 { return 3; }
}

fn from_a(a: A) -> i64 { return a.a(); }
fn from_b(b: B) -> i64 { return b.b(); }
fn from_c(c: C) -> i64 { return c.a() + c.b() + c.c(); }

fn main() {
    let value = new Impl();
    println(from_a(value));
    println(from_b(value));
    println(from_c(value));
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("proto.wi", proto), ("main.wi", main)], "main.wi");
    assert!(ok, "multiple super-interface dispatch failed");
    assert_eq!(out, "1\n2\n6\n");
}

// P6: an `extends` cycle is rejected.
#[test]
fn iface_adv_05_extends_cycle_rejected() {
    assert_compile_error_contains(
        "interface A extends B {}\ninterface B extends A {}\nfn main() {}\n",
        &["error[E0423]"],
    );
}

// P7, P23: cross-module default method inherited by an entry class, alongside
// inheritance.
#[test]
fn iface_adv_06_cross_module_default_method() {
    let proto = r#"
module proto;
pub interface Describable {
    fn label(self) -> i64;
    fn describe(self) -> i64 { return self.label() + 100; }
}
"#;
    let main = r#"
import proto::Describable;
class Item implements Describable {
    pub fn label(self) -> i64 { return 5; }
}
fn main() {
    let d: Describable = new Item();
    println(d.describe());
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("proto.wi", proto), ("main.wi", main)], "main.wi");
    assert!(ok, "cross-module default method failed: {out}");
    assert_eq!(out, "105\n");
}

// P8: a class defined in a SECOND module inherits a default from a FIRST module's
// interface.
#[test]
fn iface_adv_07_cross_module_default_in_other_module() {
    let proto = r#"
module proto;
pub interface Describable {
    fn label(self) -> i64;
    fn describe(self) -> i64 { return self.label() + 1; }
}
"#;
    let impls = r#"
module impls;
import proto::Describable;
pub class Item implements Describable {
    pub fn label(self) -> i64 { return 41; }
}
pub fn run() -> i64 {
    let d: Describable = new Item();
    return d.describe();
}
"#;
    let main = r#"
import impls::run;
fn main() { println(run()); }
"#;
    let (out, ok) = compile_temp_project_and_run(
        &[("proto.wi", proto), ("impls.wi", impls), ("main.wi", main)],
        "main.wi",
    );
    assert!(ok, "cross-module default in other module failed: {out}");
    assert_eq!(out, "42\n");
}

// P9: an unimplemented interface's default body with a type error is reported.
#[test]
fn iface_adv_08_unimplemented_default_body_checked() {
    assert_compile_error_contains(
        "interface Foo { fn bar(self) -> i64 { return true; } }\nfn main() { println(1); }\n",
        &["error[E0201]"],
    );
}

// P10: a valid unimplemented default body compiles cleanly.
#[test]
fn iface_adv_09_unimplemented_default_body_ok() {
    let (out, ok) = compile_and_run(
        "interface Foo { fn bar(self) -> i64 { return 1; } }\nfn main() { println(7); }\n",
    );
    assert!(ok, "valid unimplemented default body must compile: {out}");
    assert_eq!(out, "7\n");
}

// P11: two independent interfaces with a same-named default is ambiguous (E0425).
#[test]
fn iface_adv_10_ambiguous_default_rejected() {
    assert_compile_error_contains(
        "interface A { fn tag(self) -> i64 { return 1; } }\ninterface B { fn tag(self) -> i64 { return 2; } }\nclass C implements A, B {}\nfn main() { println(new C().tag()); }\n",
        &["error[E0425]"],
    );
}

// P12: an explicit override resolves the ambiguity.
#[test]
fn iface_adv_11_ambiguous_default_resolved_by_override() {
    let (out, ok) = compile_and_run(
        "interface A { fn tag(self) -> i64 { return 1; } }\ninterface B { fn tag(self) -> i64 { return 2; } }\nclass C implements A, B {\n    pub fn tag(self) -> i64 { return 9; }\n}\nfn main() { println(new C().tag()); }\n",
    );
    assert!(ok, "override should resolve ambiguity: {out}");
    assert_eq!(out, "9\n");
}

// P13: a same-named default where one interface extends the other is NOT
// ambiguous; the sub-interface's default wins.
#[test]
fn iface_adv_12_hierarchy_default_not_ambiguous() {
    let (out, ok) = compile_and_run(
        "interface A { fn tag(self) -> i64 { return 1; } }\ninterface B extends A { fn tag(self) -> i64 { return 2; } }\nclass C implements B {}\nfn main() { println(new C().tag()); }\n",
    );
    assert!(ok, "hierarchy default should not be ambiguous: {out}");
    assert_eq!(out, "2\n");
}

// P24: a child interface that inherits conflicting defaults from independent
// super-interfaces is ambiguous, even when the class implements only the child.
#[test]
fn iface_adv_13b_multiple_super_inherited_default_conflict_rejected() {
    assert_compile_error_contains(
        "interface A { fn tag(self) -> i64 { return 1; } }\ninterface B { fn tag(self) -> i64 { return 2; } }\ninterface C extends A, B {}\nclass Impl implements C {}\nfn main() { println(new Impl().tag()); }\n",
        &["error[E0425]"],
    );
}

// P27: the same ambiguity is rejected when the conflicting child interface is
// declared in an imported module.
#[test]
fn iface_adv_13b_cross_module_inherited_default_conflict_rejected() {
    let proto = r#"
module proto;
pub interface A { fn tag(self) -> i64 { return 1; } }
pub interface B { fn tag(self) -> i64 { return 2; } }
pub interface C extends A, B {}
"#;
    let main = r#"
import proto::C;
class Impl implements C {}
fn main() { println(new Impl().tag()); }
"#;
    let stderr =
        compile_temp_project_error_stderr(&[("proto.wi", proto), ("main.wi", main)], "main.wi");
    assert!(
        stderr.contains("error[E0425]"),
        "expected inherited default conflict: {stderr}"
    );
}

// P25: a child interface can resolve inherited default ambiguity by declaring
// the method itself.
#[test]
fn iface_adv_13c_multiple_super_inherited_default_resolved_by_child_default() {
    let (out, ok) = compile_and_run(
        "interface A { fn tag(self) -> i64 { return 1; } }\ninterface B { fn tag(self) -> i64 { return 2; } }\ninterface C extends A, B { fn tag(self) -> i64 { return 3; } }\nclass Impl implements C {}\nfn main() { println(new Impl().tag()); }\n",
    );
    assert!(
        ok,
        "child interface default should resolve ambiguity: {out}"
    );
    assert_eq!(out, "3\n");
}

// P26: two paths to the same inherited default (diamond shape) should still be
// one implementation, not an ambiguity.
#[test]
fn iface_adv_13d_diamond_shared_default_not_ambiguous() {
    let (out, ok) = compile_and_run(
        "interface Root { fn tag(self) -> i64 { return 7; } }\ninterface Left extends Root {}\ninterface Right extends Root {}\ninterface Join extends Left, Right {}\nclass Impl implements Join {}\nfn main() { println(new Impl().tag()); }\n",
    );
    assert!(ok, "shared diamond default should not be ambiguous: {out}");
    assert_eq!(out, "7\n");
}

// P14: a bad non-generic default that IS implemented reports exactly once.
#[test]
fn iface_adv_13_implemented_bad_default_single_diagnostic() {
    let stderr = compile_error_stderr(
        "interface Foo { fn bar(self) -> i64 { return true; } }\nclass C implements Foo {}\nfn main() { println(1); }\n",
    );
    let count = stderr.matches("error[E0201]").count();
    assert_eq!(
        count, 1,
        "expected exactly one E0201, got {count}: {stderr}"
    );
}

// P15: a generic interface default substitutes the type parameter (`dup` -> i64).
#[test]
fn iface_adv_14_generic_default_substitution() {
    let (out, ok) = compile_and_run(
        "interface Box<T> {\n    fn get(self) -> T;\n    fn dup(self) -> T { return self.get(); }\n}\nclass IntBox implements Box<i64> {\n    value: i64;\n    pub init(self, value: i64) { self.value = value; }\n    pub fn get(self) -> i64 { return self.value; }\n}\nfn main() { println(new IntBox(42).dup()); }\n",
    );
    assert!(ok, "generic default substitution failed: {out}");
    assert_eq!(out, "42\n");
}

// P16, P19: `Self` on a generic interface-typed receiver keeps its type args, and
// a `Self`-returning method dispatched through the interface re-boxes correctly.
#[test]
fn iface_adv_15_self_on_generic_interface_receiver() {
    let (out, ok) = compile_and_run(
        "interface Box<T> {\n    fn get(self) -> T;\n    fn copy(self) -> Self;\n}\nclass IntBox implements Box<i64> {\n    value: i64;\n    pub init(self, value: i64) { self.value = value; }\n    pub fn get(self) -> i64 { return self.value; }\n    pub fn copy(self) -> IntBox { return new IntBox(self.value); }\n}\nfn main() {\n    let b: Box<i64> = new IntBox(5);\n    let c: Box<i64> = b.copy();\n    println(c.get());\n}\n",
    );
    assert!(ok, "Self on generic interface receiver failed: {out}");
    assert_eq!(out, "5\n");
}

// P17: a module function with a NON-generic interface parameter, where the entry
// imports only the function and the implementing class (not the interface).
#[test]
fn iface_adv_16_module_fn_nongeneric_interface_param() {
    let proto = r#"
module proto;
pub interface Named { fn name(self) -> i64; }
pub class Tag implements Named {
    pub v: i64;
    pub fn name(self) -> i64 { return self.v; }
}
pub fn id_of(n: Named) -> i64 { return n.name(); }
"#;
    let main = r#"
import proto::Tag;
import proto::id_of;
fn main() { println(id_of(new Tag(5))); }
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("proto.wi", proto), ("main.wi", main)], "main.wi");
    assert!(ok, "module fn non-generic interface param failed: {out}");
    assert_eq!(out, "5\n");
}

// P18: a module function with a GENERIC interface parameter, entry imports only
// the function and the class.
#[test]
fn iface_adv_17_module_fn_generic_interface_param() {
    let boxmod = r#"
module boxmod;
pub interface Box<T> { fn get(self) -> T; }
pub class IntBox implements Box<i64> {
    pub v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
pub fn unwrap(b: Box<i64>) -> i64 { return b.get(); }
"#;
    let main = r#"
import boxmod::IntBox;
import boxmod::unwrap;
fn main() { println(unwrap(new IntBox(9))); }
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("boxmod.wi", boxmod), ("main.wi", main)], "main.wi");
    assert!(ok, "module fn generic interface param failed: {out}");
    assert_eq!(out, "9\n");
}

// P20: qualified cross-module generic interface (`import m; m::Box<i64>`).
#[test]
fn iface_adv_18_qualified_cross_module_generic_interface() {
    let boxmod = r#"
module boxmod;
pub interface Box<T> { fn get(self) -> T; }
"#;
    let main = r#"
import boxmod;
class IntBox implements boxmod::Box<i64> {
    value: i64;
    pub init(self, value: i64) { self.value = value; }
    pub fn get(self) -> i64 { return self.value; }
}
fn show(b: boxmod::Box<i64>) -> i64 { return b.get(); }
fn main() {
    let b: boxmod::Box<i64> = new IntBox(7);
    println(show(b));
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("boxmod.wi", boxmod), ("main.wi", main)], "main.wi");
    assert!(ok, "qualified cross-module generic interface failed: {out}");
    assert_eq!(out, "7\n");
}

// P21: direct-import cross-module generic interface (`import m::Box`).
#[test]
fn iface_adv_19_direct_import_cross_module_generic_interface() {
    let boxmod = r#"
module boxmod;
pub interface Box<T> { fn get(self) -> T; }
"#;
    let main = r#"
import boxmod::Box;
class IntBox implements Box<i64> {
    value: i64;
    pub init(self, value: i64) { self.value = value; }
    pub fn get(self) -> i64 { return self.value; }
}
fn show(b: Box<i64>) -> i64 { return b.get(); }
fn main() {
    let b: Box<i64> = new IntBox(7);
    println(show(b));
}
"#;
    let (out, ok) =
        compile_temp_project_and_run(&[("boxmod.wi", boxmod), ("main.wi", main)], "main.wi");
    assert!(
        ok,
        "direct-import cross-module generic interface failed: {out}"
    );
    assert_eq!(out, "7\n");
}

// P22: a default body that calls another (required) interface method via `self`.
#[test]
fn iface_adv_20_default_calls_required_method() {
    let (out, ok) = compile_and_run(
        "interface Counter {\n    fn base(self) -> i64;\n    fn doubled(self) -> i64 { return self.base() * 2; }\n}\nclass C implements Counter {\n    pub fn base(self) -> i64 { return 21; }\n}\nfn main() {\n    let c: Counter = new C();\n    println(c.doubled());\n}\n",
    );
    assert!(ok, "default calling required method failed: {out}");
    assert_eq!(out, "42\n");
}
