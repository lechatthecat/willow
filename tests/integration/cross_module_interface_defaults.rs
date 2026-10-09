//! Cross-module interface default bodies (willow-rvpp).
//!
//! A class in module M that implements a non-generic interface declared in
//! module P receives P's default body. That body is checked once with its
//! interface in P and lowered/emitted under P's alias scope, so it names P's
//! items (private or public) exactly as P's own code does, regardless of what
//! M declares or imports.
//!
//! Perspectives:
//!  1. private provider helper reached from the default (ticket repro)
//!  2. public provider helper
//!  3. bare-name collisions: M declares its own `helper` and `Box`
//!  4. default calls an abstract method implemented in M
//!  5. lambda inside the default captures nothing and calls a P helper
//!  6. nested lambdas with a capture, generic and non-generic receivers
//!  7. implementing class in a non-entry module that has its own lambdas
//!  8. P-private enum matched inside the default
//!  9. item import of the interface (`import util::Answer`)
//! 10. aliased module import (`import util as u`)
//! 11. P imports module Q that M never imports
//! 12. P imports std collections M never imports
//! 13. parameters, String results, void defaults and default→default calls
//! 14. class override wins over the provider default (direct and dynamic)
//! 15. two providers with same-named private helpers
//! 16. P-private class statics
//! 17. default inherited through an open base class
//! 18. interface extends another interface in the same provider
//! 19. aliased import + extended interface in the same provider
//! 20. interface extends an interface from a third module
//! 21. independent conflicting defaults still report E0425
//! 22. a type error in a default is reported once, at the provider file
//! 23. runtime panics in the default keep provider source locations
//! 24. item import inside P shadows nothing in M
//! 25. calls through an interface-typed parameter reach the same body
//! 26. `--emit-lir` includes the foreign default body once
//! 27. P generic function and generic class used by the default (non-generic
//!     receiver; generic receivers are covered in user_generics p78/p83)

use super::support::*;

fn runs(files: &[(&str, &str)], expected: &str) {
    let project = TestProject::new("xmod_iface_default", files);
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

fn fails(files: &[(&str, &str)]) -> String {
    let project = TestProject::new("xmod_iface_default", files);
    let compiled = project.compile("main.wi");
    let stderr = String::from_utf8_lossy(&compiled.stderr).into_owned();
    assert!(!compiled.status.success(), "{stderr}");
    assert!(!stderr.contains("internal compiler error"), "{stderr}");
    stderr
}

const BOX_MAIN: &str = "import util; class Box implements util::Answer { pub value: i64; } fn main() { println(new Box(1).answer()); }";

#[test]
fn private_provider_helper() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 77; } pub interface Answer { fn answer(self) -> i64 { return helper(); } }",
            ),
            ("main.wi", BOX_MAIN),
        ],
        "77\n",
    );
}

#[test]
fn public_provider_helper() {
    runs(
        &[
            (
                "util.wi",
                "module util; pub fn helper() -> i64 { return 77; } pub interface Answer { fn answer(self) -> i64 { return helper(); } }",
            ),
            ("main.wi", BOX_MAIN),
        ],
        "77\n",
    );
}

#[test]
fn receiving_module_names_do_not_capture_provider_names() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 2; } class Box { pub v: i64; pub fn get(self) -> i64 { return self.v; } } pub interface Answer { fn answer(self) -> i64 { return helper() * 100 + new Box(5).get(); } }",
            ),
            (
                "main.wi",
                "import util; fn helper() -> i64 { return 9; } class Box implements util::Answer { pub value: i64; } fn read(a: util::Answer) -> i64 { return a.answer(); } fn main() { println(new Box(1).answer()); println(read(new Box(1))); println(helper()); }",
            ),
        ],
        "205\n205\n9\n",
    );
}

#[test]
fn default_calls_receiver_abstract_method() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn bump(x: i64) -> i64 { return x + 1; } pub interface Answer { fn base(self) -> i64; fn answer(self) -> i64 { return bump(self.base()); } }",
            ),
            (
                "main.wi",
                "import util; class Box implements util::Answer { pub value: i64; pub fn base(self) -> i64 { return self.value; } } fn main() { println(new Box(41).answer()); }",
            ),
        ],
        "42\n",
    );
}

#[test]
fn lambda_in_default_uses_provider_helper() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper(x: i64) -> i64 { return x * 2; } pub interface Answer { fn answer(self) -> i64 { let f = |x: i64| -> i64 { return helper(x); }; return f(21); } }",
            ),
            (
                "main.wi",
                "import util; fn helper(x: i64) -> i64 { return 0; } class Box implements util::Answer { pub value: i64; } fn read(a: util::Answer) -> i64 { return a.answer(); } fn main() { println(new Box(1).answer()); println(read(new Box(1))); }",
            ),
        ],
        "42\n42\n",
    );
}

#[test]
fn nested_capturing_lambdas_for_generic_and_plain_receivers() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper(x: i64) -> i64 { return x * 2; } pub interface Answer { fn answer(self) -> i64 { let k = 10; let f = |x: i64| -> i64 { let g = |y: i64| -> i64 { return helper(y) + k; }; return g(x); }; return f(16); } }",
            ),
            (
                "main.wi",
                "import util; class Box<T> implements util::Answer { pub value: T; } class Bag implements util::Answer { pub v: i64; } fn main() { println(new Box<i64>(1).answer()); println(new Box<String>(\"s\").answer()); println(new Bag(1).answer()); }",
            ),
        ],
        "42\n42\n42\n",
    );
}

#[test]
fn implementing_class_in_non_entry_module() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper(x: i64) -> i64 { return x * 2; } pub interface Answer { fn answer(self) -> i64 { let f = |x: i64| -> i64 { return helper(x); }; return f(4); } }",
            ),
            (
                "shapes.wi",
                "module shapes; import util; fn helper() -> i64 { return 0; } class Box implements util::Answer { pub value: i64; } pub fn make() -> i64 { let f = |x: i64| -> i64 { return x + 1; }; return new Box(1).answer() * 10 + f(0); }",
            ),
            (
                "main.wi",
                "import shapes; fn main() { println(shapes::make()); }",
            ),
        ],
        "81\n",
    );
}

#[test]
fn provider_private_enum() {
    runs(
        &[
            (
                "util.wi",
                "module util; enum Color { Red, Green } fn pick() -> Color { return Color::Green; } pub interface Answer { fn answer(self) -> i64 { match pick() { Color::Red => { return 1; } Color::Green => { return 6; } } } }",
            ),
            ("main.wi", BOX_MAIN),
        ],
        "6\n",
    );
}

#[test]
fn item_imported_interface() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 7; } pub interface Answer { fn answer(self) -> i64 { return helper(); } }",
            ),
            (
                "main.wi",
                "import util::Answer; class Box implements Answer { pub value: i64; } fn read(a: Answer) -> i64 { return a.answer(); } fn main() { println(new Box(1).answer()); println(read(new Box(1))); }",
            ),
        ],
        "7\n7\n",
    );
}

#[test]
fn aliased_module_import() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 8; } pub interface Answer { fn answer(self) -> i64 { return helper(); } }",
            ),
            (
                "main.wi",
                "import util as u; class Box implements u::Answer { pub value: i64; } fn main() { println(new Box(1).answer()); }",
            ),
        ],
        "8\n",
    );
}

#[test]
fn provider_only_module_import() {
    runs(
        &[
            (
                "deep.wi",
                "module deep; pub fn value() -> i64 { return 9; }",
            ),
            (
                "util.wi",
                "module util; import deep; pub interface Answer { fn answer(self) -> i64 { return deep::value(); } }",
            ),
            ("main.wi", BOX_MAIN),
        ],
        "9\n",
    );
}

#[test]
fn provider_only_std_import() {
    runs(
        &[
            (
                "util.wi",
                "module util; import std::collections::Array; fn make() -> Array<i64> { return [1, 2, 3]; } pub interface Answer { fn answer(self) -> i64 { let a: Array<i64> = make(); return a.len(); } }",
            ),
            ("main.wi", BOX_MAIN),
        ],
        "3\n",
    );
}

#[test]
fn params_strings_void_and_default_to_default_calls() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn tag(s: String) -> String { return \"<\" + s + \">\"; } pub interface Answer { fn name(self) -> String { return tag(\"x\"); } fn add(self, a: i64, b: i64) -> i64 { return a + b; } fn show(self) { println(self.name()); } }",
            ),
            (
                "main.wi",
                "import util; class Box implements util::Answer { pub value: i64; } fn main() { let b = new Box(1); println(b.add(2, 9)); b.show(); println(b.name()); }",
            ),
        ],
        "11\n<x>\n<x>\n",
    );
}

#[test]
fn class_override_wins() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 1; } pub interface Answer { fn answer(self) -> i64 { return helper(); } }",
            ),
            (
                "main.wi",
                "import util; class Box implements util::Answer { pub value: i64; pub fn answer(self) -> i64 { return 12; } } fn read(a: util::Answer) -> i64 { return a.answer(); } fn main() { println(new Box(1).answer()); println(read(new Box(1))); }",
            ),
        ],
        "12\n12\n",
    );
}

#[test]
fn two_providers_with_same_private_helper_name() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 13; } pub interface Answer { fn answer(self) -> i64 { return helper(); } }",
            ),
            (
                "other.wi",
                "module other; fn helper() -> i64 { return 31; } pub interface Other { fn other(self) -> i64 { return helper(); } }",
            ),
            (
                "main.wi",
                "import util; import other; class Box implements util::Answer, other::Other { pub value: i64; } fn main() { let b = new Box(1); println(b.answer()); println(b.other()); }",
            ),
        ],
        "13\n31\n",
    );
}

#[test]
fn provider_private_class_static() {
    runs(
        &[
            (
                "util.wi",
                "module util; class Counter { static count: i64 = 14; pub static fn get() -> i64 { return Counter::count; } } pub interface Answer { fn answer(self) -> i64 { return Counter::get(); } }",
            ),
            ("main.wi", BOX_MAIN),
        ],
        "14\n",
    );
}

#[test]
fn default_inherited_through_open_base() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 15; } pub interface Answer { fn answer(self) -> i64 { return helper(); } }",
            ),
            (
                "main.wi",
                "import util; open class Base implements util::Answer { pub value: i64; } class Kid extends Base { } fn read(a: util::Answer) -> i64 { return a.answer(); } fn main() { println(new Kid(1).answer()); println(read(new Kid(1))); }",
            ),
        ],
        "15\n15\n",
    );
}

const EXTENDED_UTIL: &str = "module util; fn helper() -> i64 { return 16; } pub interface Base { fn base(self) -> i64 { return helper(); } } pub interface Answer extends Base { fn answer(self) -> i64 { return self.base() + 1; } }";

#[test]
fn extended_interface_in_same_provider() {
    runs(
        &[
            ("util.wi", EXTENDED_UTIL),
            (
                "main.wi",
                "import util; class Box implements util::Answer { pub value: i64; } fn read(a: util::Answer) -> i64 { return a.answer(); } fn rb(a: util::Base) -> i64 { return a.base(); } fn main() { println(new Box(1).base()); println(read(new Box(1))); println(rb(new Box(1))); }",
            ),
        ],
        "16\n17\n16\n",
    );
}

#[test]
fn extended_interface_through_alias() {
    runs(
        &[
            ("util.wi", EXTENDED_UTIL),
            (
                "main.wi",
                "import util as u; class Box implements u::Answer { pub value: i64; } fn main() { println(new Box(1).answer()); }",
            ),
        ],
        "17\n",
    );
}

#[test]
fn interface_extends_third_module_interface() {
    runs(
        &[
            (
                "deep.wi",
                "module deep; fn helper() -> i64 { return 40; } pub interface Base { fn base(self) -> i64 { return helper(); } }",
            ),
            (
                "util.wi",
                "module util; import deep; fn helper() -> i64 { return 1; } pub interface Answer extends deep::Base { fn answer(self) -> i64 { return self.base() + helper(); } }",
            ),
            (
                "main.wi",
                "import util; class Box implements util::Answer { pub value: i64; } fn main() { println(new Box(1).answer()); println(new Box(1).base()); }",
            ),
        ],
        "41\n40\n",
    );
}

#[test]
fn independent_conflicting_defaults_still_conflict() {
    let stderr = fails(&[
        (
            "util.wi",
            "module util; pub interface A { fn x(self) -> i64 { return 1; } } pub interface B { fn x(self) -> i64 { return 2; } }",
        ),
        (
            "main.wi",
            "import util; class Box implements util::A, util::B { pub value: i64; } fn main() { }",
        ),
    ]);
    assert!(stderr.contains("E0425"), "{stderr}");
}

#[test]
fn default_type_error_reported_once_at_provider() {
    let stderr = fails(&[
        (
            "util.wi",
            "module util; fn helper() -> i64 { return 1; } pub interface Answer { fn answer(self) -> i64 { return helper() + \"x\"; } }",
        ),
        (
            "main.wi",
            "import util; class Box implements util::Answer { pub value: i64; } class Bag implements util::Answer { pub v: i64; } fn main() { println(new Box(1).answer()); }",
        ),
    ]);
    assert_eq!(stderr.matches("error[E0202]").count(), 1, "{stderr}");
    assert!(stderr.contains("util.wi:1:"), "{stderr}");
    assert!(!stderr.contains("E0350"), "{stderr}");
}

#[test]
fn panic_in_default_keeps_provider_locations() {
    let project = TestProject::new(
        "xmod_iface_default",
        &[
            (
                "util.wi",
                "module util; fn boom() -> i64 { panic(\"boom from util\"); return 0; } pub interface Answer { fn answer(self) -> i64 { return boom(); } }",
            ),
            ("main.wi", BOX_MAIN),
        ],
    );
    let compiled = project.compile("main.wi");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let output = project.run();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("util.wi:1:33"), "{stderr}");
    assert!(stderr.contains("util.wi:1:125"), "{stderr}");
}

#[test]
fn provider_item_import() {
    runs(
        &[
            (
                "deep.wi",
                "module deep; pub fn value() -> i64 { return 19; }",
            ),
            (
                "util.wi",
                "module util; import deep::value; pub interface Answer { fn answer(self) -> i64 { return value(); } }",
            ),
            (
                "main.wi",
                "import util; fn value() -> i64 { return 0; } class Box implements util::Answer { pub value: i64; } fn main() { println(new Box(1).answer()); }",
            ),
        ],
        "19\n",
    );
}

#[test]
fn interface_typed_parameter_reaches_same_body() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 25; } pub interface Answer { fn answer(self) -> i64 { return helper(); } }",
            ),
            (
                "main.wi",
                "import util; class Box implements util::Answer { pub value: i64; } class Bag implements util::Answer { pub v: i64; } fn read(a: util::Answer) -> i64 { return a.answer(); } fn main() { println(read(new Box(1))); println(read(new Bag(2))); }",
            ),
        ],
        "25\n25\n",
    );
}

#[test]
fn emit_lir_includes_foreign_default_once() {
    let project = TestProject::new(
        "xmod_iface_default",
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 77; } pub interface Answer { fn answer(self) -> i64 { return helper(); } }",
            ),
            ("main.wi", BOX_MAIN),
        ],
    );
    let output = project.emit_lir("main.wi");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout.matches("fn Box::answer(").count(), 1, "{stdout}");
}

#[test]
fn provider_generic_function_and_class() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn identity<T>(x: T) -> T { return x; } class Payload<T> { pub value: T; pub fn get(self) -> T { return self.value; } } pub interface Answer { fn answer(self) -> i64 { return identity(new Payload<i64>(83).get()); } }",
            ),
            (
                "main.wi",
                "import util; class Box implements util::Answer { pub value: i64; } fn read(a: util::Answer) -> i64 { return a.answer(); } fn main() { println(new Box(1).answer()); println(read(new Box(1))); }",
            ),
        ],
        "83\n83\n",
    );
}

const FAN_UTIL: &str = "module util; fn helper() -> i64 { return 28; } pub interface Answer { fn answer(self) -> i64 { return helper(); } fn twice(self) -> i64 { let f = |x: i64| -> i64 { return x * 2; }; return f(self.answer()); } }";

#[test]
fn fan_in_receivers_share_one_provider() {
    runs(
        &[
            ("util.wi", FAN_UTIL),
            (
                "a.wi",
                "module a; import util; pub class A implements util::Answer { pub value: i64; } pub fn make() -> util::Answer { return new A(1); }",
            ),
            (
                "b.wi",
                "module b; import util; pub class B<T> implements util::Answer { pub value: T; } pub fn make() -> util::Answer { return new B<i64>(1); }",
            ),
            (
                "main.wi",
                "import util; import a; import b; class C implements util::Answer { pub value: i64; } fn read(x: util::Answer) -> i64 { return x.twice(); } fn main() { println(read(a::make())); println(read(b::make())); println(read(new C(1))); println(new a::A(1).answer() + new C(1).answer()); }",
            ),
        ],
        "56\n56\n56\n56\n",
    );
}

#[test]
fn deep_class_chain_across_modules() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 29; } pub interface Answer { fn answer(self) -> i64 { return helper(); } }",
            ),
            (
                "shapes.wi",
                "module shapes; import util; pub open class Base implements util::Answer { pub value: i64; } pub open class Mid extends Base { }",
            ),
            (
                "main.wi",
                "import util; import shapes; open class Leaf extends shapes::Mid { } class Tip extends Leaf { } fn read(a: util::Answer) -> i64 { return a.answer(); } fn main() { println(new Tip(1).answer()); println(read(new Tip(1))); println(read(new shapes::Mid(1))); }",
            ),
        ],
        "29\n29\n29\n",
    );
}

#[test]
fn one_class_two_providers() {
    runs(
        &[
            (
                "p.wi",
                "module p; fn helper() -> i64 { return 30; } pub interface First { fn first(self) -> i64 { return helper(); } }",
            ),
            (
                "q.wi",
                "module q; fn helper() -> i64 { return 3; } pub interface Second { fn second(self) -> i64 { return helper(); } }",
            ),
            (
                "main.wi",
                "import p; import q; class Box implements p::First, q::Second { pub value: i64; } fn f(x: p::First) -> i64 { return x.first(); } fn s(x: q::Second) -> i64 { return x.second(); } fn main() { println(f(new Box(1)) + s(new Box(1))); }",
            ),
        ],
        "33\n",
    );
}

#[test]
fn emit_lir_includes_foreign_default_lambdas() {
    let project = TestProject::new(
        "xmod_iface_default",
        &[
            ("util.wi", FAN_UTIL),
            (
                "main.wi",
                "import util; class Box implements util::Answer { pub value: i64; } fn main() { println(new Box(1).twice()); }",
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
    assert_eq!(stdout.matches("fn Box::twice(").count(), 1, "{stdout}");
    assert_eq!(stdout.matches("fn $lambda@").count(), 1, "{stdout}");
}

#[test]
fn emit_hir_lowers_foreign_default_in_provider_scope() {
    let project = TestProject::new(
        "xmod_iface_default",
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 2; } class Box { pub v: i64; pub fn get(self) -> i64 { return self.v; } } pub interface Answer { fn answer(self) -> i64 { return helper() * 100 + new Box(5).get(); } }",
            ),
            (
                "main.wi",
                "import util; fn helper() -> i64 { return 9; } class Box implements util::Answer { pub value: i64; } fn main() { println(new Box(1).answer()); println(helper()); }",
            ),
        ],
    );
    let output = project.emit_hir("main.wi");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stdout.matches("fn answer(self: Box)").count(),
        1,
        "{stdout}"
    );
    // The provider's `Box`, not the receiving module's.
    assert!(stdout.contains("new util::Box(5: i64)"), "{stdout}");
    assert!(!stdout.contains("not yet lowered"), "{stdout}");
}
