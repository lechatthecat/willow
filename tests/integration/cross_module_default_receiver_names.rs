//! Interface defaults copied into a class whose name a provider-local item
//! also uses (willow-nvi6).
//!
//! The entry file's type identities are bare (`A`), so in the scope of the
//! module that declares the interface the bare name would mean that module's
//! own `A`. Copied defaults spell the receiver under a reserved prefix
//! instead, and the provider's same-named class, enum, interface or function
//! keeps its own meaning. Only names the provider declares or imports are
//! respelled.
//!
//! Perspectives:
//!  1. ticket repro: provider-private class `A` with a method `two`
//!  2. runnable example program
//!  3. Self-returning abstract method implemented by the receiver
//!  4. generic interface (willow-s71l) with the same collision
//!  5. generic interface + Self-returning abstract method
//!  6. generic argument is an entry class named like a provider class
//!  7. receiver's base class named like a provider-local class
//!  8. provider-local enum named like the receiver
//!  9. provider-local function named like the receiver
//! 10. default uses the provider's own same-named class (`new A()`, statics)
//! 11. provider-local interface named like the receiver
//! 12. provider item-imports a class named like the receiver
//! 13. provider item-imports an interface named like the receiver
//! 14. receiver declared in a non-entry module (qualified, not respelled)
//! 15. two receivers in one unit, only one colliding
//! 16. two providers that each declare a private `A`
//! 17. class override wins over the copied default
//! 18. aliased module import of the provider
//! 19. lambda inside the default
//! 20. call through an interface-typed parameter
//! 21. receiver and provider statics with the same name stay apart
//! 22. `--emit-hir` merges the copies into the receiver class
//! 23. `--emit-lir` emits the copied default
//! 24. a receiver no provider name shadows keeps its own spelling
//! 25. receiver as its own interface type argument, through a provider
//!     generic function, with a provider class of the same name
//! 26. provider generic function instantiated with an entry class the
//!     provider's own class shadows (no interface default involved)
//! 27. same, through a closure parameter and a lambda in the instance
//! 28. provider generic function instantiated with an entry class the
//!     provider's own ENUM shadows
//! 29. same, shadowed by a provider INTERFACE
//! 30. provider generic CLASS instantiated with a shadowed entry class:
//!     the instance's field type and its method body agree
//! 31. provider generic CLASS instantiated with an entry class the provider
//!     shadows through an item import of another module's class
//! 32. same, through a provider generic FUNCTION with a fn-typed parameter
//! 33. generic class, shadowed through an item-imported ENUM
//! 34. generic function, shadowed through an item-imported ENUM

use super::support::*;

fn runs(files: &[(&str, &str)], expected: &str) {
    let project = TestProject::new("xmod_receiver_names", files);
    let compiled = project.compile("main.wi");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let output = project.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
}

/// The ticket's provider: a private class `A` whose `two` returns a String.
const UTIL: &str = "module util; class A { pub fn two(self) -> String { return \"x\"; } } pub fn mk() -> i64 { return 100; } pub interface Get { fn two(self) -> i64 { return 2; } fn get(self) -> i64 { return self.two() + mk(); } }";

#[test]
fn ticket_repro_receiver_named_like_provider_class() {
    runs(
        &[
            ("util.wi", UTIL),
            (
                "main.wi",
                "import util; class A implements util::Get { pub n: i64; } fn main() { println(new A(1).get()); }",
            ),
        ],
        "102\n",
    );
}

#[test]
fn example_program() {
    runs(
        &[
            (
                "main.wi",
                include_str!("../../example/interface_default_receiver_names.wi"),
            ),
            (
                "interface_default_receiver_names_support/shapes.wi",
                include_str!("../../example/interface_default_receiver_names_support/shapes.wi"),
            ),
        ],
        "provider point\n1019\nprovider point\n1007\n",
    );
}

#[test]
fn self_returning_abstract_method() {
    runs(
        &[
            (
                "util.wi",
                "module util; class A { pub fn two(self) -> String { return \"x\"; } } pub interface Get { fn me(self) -> Self; fn two(self) -> i64 { return 2; } fn get(self) -> i64 { return self.me().two() + 100; } }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get { pub n: i64; pub fn me(self) -> A { return new A(self.n + 1); } } fn main() { println(new A(1).get()); }",
            ),
        ],
        "102\n",
    );
}

#[test]
fn generic_interface() {
    runs(
        &[
            (
                "util.wi",
                "module util; class A { pub fn two(self) -> String { return \"x\"; } } pub interface Get<T> { fn val(self) -> T; fn two(self) -> i64 { return 2; } fn get(self) -> T { println(self.two()); return self.val(); } }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get<i64> { pub n: i64; pub fn val(self) -> i64 { return self.n; } } fn main() { println(new A(9).get()); }",
            ),
        ],
        "2\n9\n",
    );
}

#[test]
fn generic_interface_self_returning_method() {
    runs(
        &[
            (
                "util.wi",
                "module util; class A { pub fn val(self) -> String { return \"x\"; } } pub interface Get<T> { fn me(self) -> Self; fn val(self) -> T; fn get(self) -> T { return self.me().val(); } }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get<i64> { pub n: i64; pub fn me(self) -> A { return new A(self.n + 1); } pub fn val(self) -> i64 { return self.n; } } fn main() { println(new A(1).get()); }",
            ),
        ],
        "2\n",
    );
}

#[test]
fn generic_argument_named_like_provider_class() {
    runs(
        &[
            (
                "util.wi",
                "module util; class C { pub fn n(self) -> String { return \"x\"; } } pub interface Get<T> { fn val(self) -> T; fn get(self) -> T { return self.val(); } }",
            ),
            (
                "main.wi",
                "import util; class C { pub n: i64; } class A implements util::Get<C> { pub fn val(self) -> C { return new C(5); } } fn main() { println(new A().get().n); }",
            ),
        ],
        "5\n",
    );
}

#[test]
fn receiver_as_own_type_argument_through_provider_generic_fn() {
    runs(
        &[
            (
                "util.wi",
                "module util; class Me { pub fn n(self) -> String { return \"x\"; } } fn pass<T>(x: T) -> T { return x; } pub interface Get<T> { fn raw(self) -> T; fn echo(self) -> T { let w: T = pass(self.raw()); return w; } }",
            ),
            (
                "main.wi",
                "import util; class Me implements util::Get<Me> { pub n: i64; pub fn raw(self) -> Me { return self; } } fn main() { println(new Me(5).echo().n); }",
            ),
        ],
        "5\n",
    );
}

#[test]
fn provider_generic_fn_argument_named_like_provider_class() {
    runs(
        &[
            (
                "util.wi",
                "module util; class Me { pub fn n(self) -> String { return \"x\"; } } pub fn call<T>(x: T, f: fn(T) -> i64) -> i64 { let w: T = x; return f(w); }",
            ),
            (
                "main.wi",
                "import util; class Me { pub n: i64; } fn get(m: Me) -> i64 { return m.n; } fn main() { println(util::call(new Me(5), get)); }",
            ),
        ],
        "5\n",
    );
}

#[test]
fn provider_generic_closure_argument_named_like_provider_class() {
    runs(
        &[
            (
                "util.wi",
                "module util; class Me { pub fn n(self) -> i64 { return 1; } } pub fn call<T>(x: T, f: closure(T) -> i64) -> i64 { let g = |y: T| f(y); return g(x); }",
            ),
            (
                "main.wi",
                "import util; class Me { pub n: i64; } fn main() { let k = 10; println(util::call(new Me(5), |m: Me| m.n + k)); }",
            ),
        ],
        "15\n",
    );
}

#[test]
fn provider_generic_fn_argument_named_like_provider_enum() {
    runs(
        &[
            (
                "util.wi",
                "module util; enum Me { X } pub fn call<T>(x: T, f: fn(T) -> i64) -> i64 { let w: T = x; return f(w); }",
            ),
            (
                "main.wi",
                "import util; class Me { pub n: i64; } fn get(m: Me) -> i64 { return m.n; } fn main() { println(util::call(new Me(5), get)); }",
            ),
        ],
        "5\n",
    );
}

#[test]
fn provider_generic_fn_argument_named_like_provider_interface() {
    runs(
        &[
            (
                "util.wi",
                "module util; interface Me { fn q(self) -> i64; } pub fn call<T>(x: T, f: fn(T) -> i64) -> i64 { let w: T = x; return f(w); }",
            ),
            (
                "main.wi",
                "import util; class Me { pub n: i64; } fn get(m: Me) -> i64 { return m.n; } fn main() { println(util::call(new Me(5), get)); }",
            ),
        ],
        "5\n",
    );
}

#[test]
fn provider_generic_class_field_named_like_provider_class() {
    runs(
        &[
            (
                "util.wi",
                "module util; class Me { pub fn n(self) -> i64 { return 1; } } pub class Box<T> { pub v: T; pub fn get(self) -> T { return self.v; } } pub fn wrap<T>(x: T) -> Box<T> { return new Box<T>(x); }",
            ),
            (
                "main.wi",
                "import util; class Me { pub n: i64; } fn main() { let b = util::wrap(new Me(7)); println(b.get().n); }",
            ),
        ],
        "7\n",
    );
}

#[test]
fn provider_generic_class_argument_named_like_item_imported_class() {
    runs(
        &[
            ("other.wi", "module other; pub class Me { pub s: String; }"),
            (
                "util.wi",
                "module util; import other::Me; fn touch(m: Me) -> i64 { return 0; } pub class Box<T> { pub v: T; pub fn get(self) -> T { return self.v; } } pub fn wrap<T>(x: T) -> Box<T> { return new Box<T>(x); }",
            ),
            (
                "main.wi",
                "import util; class Me { pub n: i64; } fn main() { println(util::wrap(new Me(7)).get().n); }",
            ),
        ],
        "7\n",
    );
}

#[test]
fn provider_generic_fn_argument_named_like_item_imported_class() {
    runs(
        &[
            ("other.wi", "module other; pub class Me { pub s: String; }"),
            (
                "util.wi",
                "module util; import other::Me; fn touch(m: Me) -> i64 { return 0; } pub fn call<T>(x: T, f: fn(T) -> i64) -> i64 { let w: T = x; return f(w); }",
            ),
            (
                "main.wi",
                "import util; class Me { pub n: i64; } fn get(m: Me) -> i64 { return m.n; } fn main() { println(util::call(new Me(5), get)); }",
            ),
        ],
        "5\n",
    );
}

#[test]
fn provider_generic_class_argument_named_like_item_imported_enum() {
    runs(
        &[
            ("other.wi", "module other; pub enum Me { X, Y }"),
            (
                "util.wi",
                "module util; import other::Me; fn touch(m: Me) -> i64 { return 0; } pub class Box<T> { pub v: T; pub fn get(self) -> T { return self.v; } } pub fn wrap<T>(x: T) -> Box<T> { return new Box<T>(x); }",
            ),
            (
                "main.wi",
                "import util; class Me { pub n: i64; } fn main() { println(util::wrap(new Me(7)).get().n); }",
            ),
        ],
        "7\n",
    );
}

#[test]
fn provider_generic_fn_argument_named_like_item_imported_enum() {
    runs(
        &[
            ("other.wi", "module other; pub enum Me { X, Y }"),
            (
                "util.wi",
                "module util; import other::Me; fn touch(m: Me) -> i64 { return 0; } pub fn call<T>(x: T, f: fn(T) -> i64) -> i64 { let w: T = x; return f(w); }",
            ),
            (
                "main.wi",
                "import util; class Me { pub n: i64; } fn get(m: Me) -> i64 { return m.n; } fn main() { println(util::call(new Me(5), get)); }",
            ),
        ],
        "5\n",
    );
}

#[test]
fn base_class_named_like_provider_class() {
    runs(
        &[
            (
                "util.wi",
                "module util; open class Base { pub fn two(self) -> String { return \"x\"; } } pub interface Get { fn two(self) -> i64; fn get(self) -> i64 { return self.two() + 100; } }",
            ),
            (
                "main.wi",
                "import util; open class Base { pub fn two(self) -> i64 { return 5; } } class A extends Base implements util::Get { pub init(self) { super.init(); } } fn main() { println(new A().get()); }",
            ),
        ],
        "105\n",
    );
}

#[test]
fn provider_enum_named_like_receiver() {
    runs(
        &[
            (
                "util.wi",
                "module util; enum A { X, Y } fn pick(a: A) -> i64 { return match a { X => 1, Y => 2 }; } pub interface Get { fn get(self) -> i64 { return pick(A::Y) + 100; } }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get { pub n: i64; } fn main() { println(new A(1).get()); }",
            ),
        ],
        "102\n",
    );
}

#[test]
fn provider_function_named_like_receiver() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn A() -> i64 { return 7; } pub interface Get { fn get(self) -> i64 { return A() + 100; } }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get { pub n: i64; } fn main() { println(new A(1).get()); }",
            ),
        ],
        "107\n",
    );
}

#[test]
fn default_uses_provider_class_of_the_same_name() {
    runs(
        &[
            (
                "util.wi",
                "module util; class A { pub fn two(self) -> String { return \"x\"; } pub static fn s() -> i64 { return 5; } } pub interface Get { fn two(self) -> i64 { return 2; } fn get(self) -> i64 { println(new A().two()); return self.two() + A::s() + 100; } }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get { pub n: i64; } fn main() { println(new A(1).get()); }",
            ),
        ],
        "x\n107\n",
    );
}

#[test]
fn provider_interface_named_like_receiver() {
    runs(
        &[
            (
                "util.wi",
                "module util; interface A { fn v(self) -> i64; } class Q implements A { pub fn v(self) -> i64 { return 3; } } fn f(a: A) -> i64 { return a.v(); } pub interface Get { fn get(self) -> i64 { let a: A = new Q(); return f(a) + a.v() + 100; } }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get { pub n: i64; } fn main() { println(new A(1).get()); }",
            ),
        ],
        "106\n",
    );
}

#[test]
fn provider_item_imported_class_named_like_receiver() {
    runs(
        &[
            (
                "util.wi",
                "module util; import other::A; pub interface Get { fn two(self) -> i64 { return 2; } fn get(self) -> i64 { return self.two() + new A().two() + 100; } }",
            ),
            (
                "other.wi",
                "module other; pub class A { pub fn two(self) -> i64 { return 10; } }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get { pub n: i64; } fn main() { println(new A(1).get()); }",
            ),
        ],
        "112\n",
    );
}

#[test]
fn provider_item_imported_interface_named_like_receiver() {
    runs(
        &[
            (
                "util.wi",
                "module util; import other::A; import other::Q; pub interface Get { fn get(self) -> i64 { let a: A = new Q(); return a.v() + 100; } }",
            ),
            (
                "other.wi",
                "module other; pub interface A { fn v(self) -> i64; } pub class Q implements A { pub fn v(self) -> i64 { return 4; } }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get { pub n: i64; } fn main() { println(new A(1).get()); }",
            ),
        ],
        "104\n",
    );
}

#[test]
fn receiver_in_non_entry_module() {
    runs(
        &[
            ("util.wi", UTIL),
            (
                "other.wi",
                "module other; import util; pub class A implements util::Get { pub n: i64; } pub fn make() -> A { return new A(1); }",
            ),
            (
                "main.wi",
                "import other; fn main() { println(other::make().get()); }",
            ),
        ],
        "102\n",
    );
}

#[test]
fn two_receivers_one_colliding() {
    runs(
        &[
            ("util.wi", UTIL),
            (
                "main.wi",
                "import util; class A implements util::Get { pub n: i64; } class B implements util::Get { } fn main() { println(new A(1).get()); println(new B().get()); }",
            ),
        ],
        "102\n102\n",
    );
}

#[test]
fn two_providers_with_private_classes_of_the_same_name() {
    runs(
        &[
            (
                "p1.wi",
                "module p1; class A { pub fn v(self) -> String { return \"p1\"; } } pub interface One { fn v(self) -> i64 { return 1; } fn one(self) -> i64 { return self.v() + 10; } }",
            ),
            (
                "p2.wi",
                "module p2; class A { pub fn w(self) -> String { return \"p2\"; } } pub interface Two { fn w(self) -> i64 { return 2; } fn two(self) -> i64 { return self.w() + 20; } }",
            ),
            (
                "main.wi",
                "import p1; import p2; class A implements p1::One, p2::Two { } fn main() { let a = new A(); println(a.one() + a.two()); }",
            ),
        ],
        "33\n",
    );
}

#[test]
fn override_wins() {
    runs(
        &[
            ("util.wi", UTIL),
            (
                "main.wi",
                "import util; class A implements util::Get { pub n: i64; pub fn two(self) -> i64 { return 40; } } fn main() { println(new A(1).get()); }",
            ),
        ],
        "140\n",
    );
}

#[test]
fn aliased_provider_import() {
    runs(
        &[
            ("util.wi", UTIL),
            (
                "main.wi",
                "import util as u; class A implements u::Get { pub n: i64; } fn main() { println(new A(1).get()); }",
            ),
        ],
        "102\n",
    );
}

#[test]
fn lambda_inside_default() {
    runs(
        &[
            (
                "util.wi",
                "module util; class A { pub fn two(self) -> String { return \"x\"; } } pub interface Get { fn two(self) -> i64 { return 2; } fn get(self) -> i64 { let t = self.two(); let f = |x: i64| -> i64 { return x + t; }; return f(100); } }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get { pub n: i64; } fn main() { println(new A(1).get()); }",
            ),
        ],
        "102\n",
    );
}

#[test]
fn interface_typed_parameter() {
    runs(
        &[
            ("util.wi", UTIL),
            (
                "main.wi",
                "import util; class A implements util::Get { pub n: i64; } fn read(g: util::Get) -> i64 { return g.get(); } fn main() { println(read(new A(1))); }",
            ),
        ],
        "102\n",
    );
}

#[test]
fn same_named_statics_stay_apart() {
    runs(
        &[
            (
                "util.wi",
                "module util; class A { pub static fn s() -> i64 { return 7; } } pub interface Get { fn get(self) -> i64 { return A::s() + 100; } }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get { pub n: i64; pub static fn s() -> i64 { return 1; } } fn main() { println(new A(1).get()); println(A::s()); }",
            ),
        ],
        "107\n1\n",
    );
}

const REPRO_MAIN: &str = "import util; class A implements util::Get { pub n: i64; } fn main() { println(new A(1).get()); }";

#[test]
fn emit_hir_merges_copies_into_receiver() {
    let project = TestProject::new(
        "xmod_receiver_names",
        &[("util.wi", UTIL), ("main.wi", REPRO_MAIN)],
    );
    let output = project.emit_hir("main.wi");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout.matches("class A {").count(), 1, "{stdout}");
    assert!(!stdout.contains("class $receiver::"), "{stdout}");
    assert_eq!(
        stdout.matches("fn get(self: A) -> i64").count(),
        1,
        "{stdout}"
    );
    assert!(!stdout.contains("$receiver::"), "{stdout}");
}

#[test]
fn emit_lir_emits_copied_default() {
    let project = TestProject::new(
        "xmod_receiver_names",
        &[("util.wi", UTIL), ("main.wi", REPRO_MAIN)],
    );
    let output = project.emit_lir("main.wi");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout.matches("A::get(self:").count(), 1, "{stdout}");
}

#[test]
fn receiver_without_collision_keeps_its_spelling() {
    let project = TestProject::new(
        "xmod_receiver_names",
        &[
            ("util.wi", UTIL),
            (
                "main.wi",
                "import util; class B implements util::Get { } fn main() { println(new B().get()); }",
            ),
        ],
    );
    let output = project.emit_lir("main.wi");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout.matches("fn B::get(self: B)").count(), 1, "{stdout}");
    assert!(!stdout.contains("$receiver::"), "{stdout}");
}
