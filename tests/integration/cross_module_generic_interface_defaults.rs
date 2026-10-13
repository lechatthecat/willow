//! Cross-module GENERIC interface default bodies (willow-s71l).
//!
//! A class in module M that implements `P::Get<A>` receives P's default body.
//! The interface is instantiated once per distinct type argument list in P,
//! checked there like P's own code, and each receiver's copy takes the checked
//! body, so the default names P's items (private, public, generic or
//! imported) regardless of what M declares or imports.
//!
//! Perspectives:
//!  1. private provider helper (ticket repro)
//!  2. public provider helper
//!  3. receiver-module names (`helper`, `Holder`) do not capture P's names
//!  4. default calls an abstract method returning `T`
//!  5. `let x: T` and a P generic function at `T`
//!  6. P-private generic class at `T`
//!  7. lambda taking `T` that calls a P generic function
//!  8. P-private enum matched inside the default
//!  9. P-private class statics
//! 10. P imports module Q that M never imports
//! 11. item import of the interface (`import util::Get`)
//! 12. aliased item import (`import util::Get as G`)
//! 13. aliased module import (`import util as u`)
//! 14. `String` instantiation
//! 15. type argument declared in M (`Get<Me>`)
//! 16. type argument declared in a third module
//! 17. two receivers sharing one instantiation, plus distinct instantiations
//! 18. calls through a `util::Get<i64>`-typed parameter reach the same body
//! 19. receiver in a non-entry module with its own same-named helper
//! 20. generic class receiver `Gen<T> implements util::Get<T>` at two types
//! 21. default inherited through an open base class
//! 22. class override wins (direct and dynamic)
//! 23. default→default calls with parameters, void and `String` results
//! 24. multi-parameter interface `Pair<A, B>`
//! 25. `Self` in a default signature keeps the receiver-checked copy
//! 26. default calling an abstract `-> Self` method (generic and non-generic)
//! 27. a type error in a default is reported at the provider file, once per
//!     instantiation
//! 28. runtime panics in the default keep provider source locations
//! 29. `--emit-lir` emits one body per receiver class
//! 30. same-module generic interface defaults are unchanged
//! 31. a lambda in a generic function instantiated at an entry-module class

use super::support::*;

fn runs(files: &[(&str, &str)], expected: &str) {
    let project = TestProject::new("xmod_generic_iface_default", files);
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
    let project = TestProject::new("xmod_generic_iface_default", files);
    let compiled = project.compile("main.wi");
    let stderr = String::from_utf8_lossy(&compiled.stderr).into_owned();
    assert!(!compiled.status.success(), "{stderr}");
    assert!(!stderr.contains("internal compiler error"), "{stderr}");
    stderr
}

const GET: &str =
    "pub interface Get<T> { fn raw(self) -> T; fn get(self) -> i64 { return helper(); } }";

fn util(body: &str) -> String {
    format!("module util; {body}")
}

#[test]
fn standalone_generic_imported_default_example_runs() {
    runs(
        &[
            (
                "main.wi",
                include_str!("../../example/imported_generic_interface_default.wi"),
            ),
            (
                "imported_generic_interface_default_support/provider.wi",
                include_str!(
                    "../../example/imported_generic_interface_default_support/provider.wi"
                ),
            ),
        ],
        "20\n27\n7\nwillow\n",
    );
}

#[test]
fn private_provider_helper() {
    runs(
        &[
            (
                "util.wi",
                &util(&format!("fn helper() -> i64 {{ return 20; }} {GET}")),
            ),
            (
                "main.wi",
                "import util; class Box implements util::Get<i64> { pub fn raw(self) -> i64 { return 1; } } fn main() { println(new Box().get()); }",
            ),
        ],
        "20\n",
    );
}

#[test]
fn public_provider_helper() {
    runs(
        &[
            (
                "util.wi",
                &util(&format!("pub fn helper() -> i64 {{ return 21; }} {GET}")),
            ),
            (
                "main.wi",
                "import util; class Box implements util::Get<i64> { pub fn raw(self) -> i64 { return 1; } } fn main() { println(new Box().get()); }",
            ),
        ],
        "21\n",
    );
}

#[test]
fn receiver_names_do_not_capture_provider_names() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 3; } class Holder<T> { pub v: T; } pub interface Get<T> { fn raw(self) -> T; fn get(self) -> i64 { let h = new Holder<T>(self.raw()); return helper(); } }",
            ),
            (
                "main.wi",
                "import util; fn helper() -> i64 { return -1; } class Holder { pub n: i64; } class Box implements util::Get<i64> { pub fn raw(self) -> i64 { return 1; } } fn main() { println(new Box().get()); println(helper()); println(new Holder(5).n); }",
            ),
        ],
        "3\n-1\n5\n",
    );
}

#[test]
fn abstract_method_returning_type_parameter() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn bump(x: i64) -> i64 { return x + 1; } pub interface Num<T> { fn raw(self) -> i64; fn item(self) -> T; fn next(self) -> i64 { return bump(self.raw()); } fn first(self) -> T { return self.item(); } }",
            ),
            (
                "main.wi",
                "import util; class N implements util::Num<String> { pub v: i64; pub fn raw(self) -> i64 { return self.v; } pub fn item(self) -> String { return \"it\"; } } fn main() { let n = new N(41); println(n.next()); println(n.first()); }",
            ),
        ],
        "42\nit\n",
    );
}

#[test]
fn typed_local_and_provider_generic_function() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn twice<T>(x: T) -> T { return x; } pub interface Get<T> { fn raw(self) -> T; fn echo(self) -> T { let w: T = twice(self.raw()); return w; } }",
            ),
            (
                "main.wi",
                "import util; class Box implements util::Get<i64> { pub fn raw(self) -> i64 { return 9; } } fn main() { println(new Box().echo()); }",
            ),
        ],
        "9\n",
    );
}

#[test]
fn provider_private_generic_class() {
    runs(
        &[
            (
                "util.wi",
                "module util; class Holder<T> { pub v: T; pub fn take(self) -> T { return self.v; } } pub interface Get<T> { fn raw(self) -> T; fn held(self) -> T { return new Holder<T>(self.raw()).take(); } }",
            ),
            (
                "main.wi",
                "import util; class S implements util::Get<String> { pub fn raw(self) -> String { return \"held\"; } } fn main() { println(new S().held()); }",
            ),
        ],
        "held\n",
    );
}

#[test]
fn lambda_taking_type_parameter_calls_provider_generic() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn twice<T>(x: T) -> T { return x; } fn plus(x: i64) -> i64 { return x + 100; } pub interface Get<T> { fn raw(self) -> T; fn via(self) -> T { let f = |x: T| twice(x); return f(self.raw()); } fn bumped(self) -> i64 { let g = |x: i64| plus(x); return g(1); } }",
            ),
            (
                "main.wi",
                "import util; class Box implements util::Get<i64> { pub fn raw(self) -> i64 { return 24; } } fn main() { println(new Box().via()); println(new Box().bumped()); }",
            ),
        ],
        "24\n101\n",
    );
}

#[test]
fn provider_private_enum() {
    runs(
        &[
            (
                "util.wi",
                "module util; enum Kind { A, B } pub interface Get<T> { fn raw(self) -> T; fn get(self) -> i64 { let k: Kind = Kind::B; match k { Kind::A => { return 0; } Kind::B => { return 2; } } } }",
            ),
            (
                "main.wi",
                "import util; class Box implements util::Get<i64> { pub fn raw(self) -> i64 { return 1; } } fn main() { println(new Box().get()); }",
            ),
        ],
        "2\n",
    );
}

#[test]
fn provider_private_class_statics() {
    runs(
        &[
            (
                "util.wi",
                "module util; class Counter { pub static mut hits: i64 = 0; } pub interface Get<T> { fn raw(self) -> T; fn get(self) -> i64 { Counter::hits = Counter::hits + 1; return Counter::hits; } }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get<i64> { pub fn raw(self) -> i64 { return 1; } } class B implements util::Get<String> { pub fn raw(self) -> String { return \"b\"; } } fn main() { println(new A().get()); println(new B().get()); println(new A().get()); }",
            ),
        ],
        "1\n2\n3\n",
    );
}

#[test]
fn provider_imports_module_receiver_never_imports() {
    runs(
        &[
            (
                "deep.wi",
                "module deep; fn helper() -> i64 { return 999; } pub fn seven() -> i64 { return 7; }",
            ),
            (
                "util.wi",
                "module util; import deep; fn helper() -> i64 { return 20; } pub interface Get<T> { fn raw(self) -> T; fn get(self) -> i64 { return helper() + deep::seven(); } }",
            ),
            (
                "main.wi",
                "import util; class Box implements util::Get<i64> { pub fn raw(self) -> i64 { return 1; } } fn main() { println(new Box().get()); }",
            ),
        ],
        "27\n",
    );
}

#[test]
fn item_imported_interface() {
    runs(
        &[
            (
                "util.wi",
                &util(&format!("fn helper() -> i64 {{ return 11; }} {GET}")),
            ),
            (
                "main.wi",
                "import util::Get; class Box implements Get<i64> { pub fn raw(self) -> i64 { return 1; } } fn main() { println(new Box().get()); }",
            ),
        ],
        "11\n",
    );
}

#[test]
fn aliased_item_import() {
    runs(
        &[
            (
                "util.wi",
                &util(&format!("fn helper() -> i64 {{ return 12; }} {GET}")),
            ),
            (
                "main.wi",
                "import util::Get as G; class Box implements G<i64> { pub fn raw(self) -> i64 { return 1; } } fn main() { println(new Box().get()); }",
            ),
        ],
        "12\n",
    );
}

#[test]
fn aliased_module_import() {
    runs(
        &[
            (
                "util.wi",
                &util(&format!("fn helper() -> i64 {{ return 13; }} {GET}")),
            ),
            (
                "main.wi",
                "import util as u; class Box implements u::Get<i64> { pub fn raw(self) -> i64 { return 1; } } fn main() { println(new Box().get()); }",
            ),
        ],
        "13\n",
    );
}

#[test]
fn string_instantiation() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn wrap(s: String) -> String { return \"<\" + s + \">\"; } pub interface Get<T> { fn raw(self) -> String; fn show(self) -> String { return wrap(self.raw()); } }",
            ),
            (
                "main.wi",
                "import util; class S implements util::Get<String> { pub fn raw(self) -> String { return \"yo\"; } } fn main() { println(new S().show()); }",
            ),
        ],
        "<yo>\n",
    );
}

#[test]
fn receiver_module_type_argument() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn pass<T>(x: T) -> T { return x; } pub interface Get<T> { fn raw(self) -> T; fn echo(self) -> T { let w: T = pass(self.raw()); return w; } }",
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
fn third_module_type_argument() {
    runs(
        &[
            ("shapes.wi", "module shapes; pub class Dot { pub x: i64; }"),
            (
                "util.wi",
                "module util; fn keep<T>(x: T) -> T { return x; } pub interface Get<T> { fn raw(self) -> T; fn echo(self) -> T { return keep(self.raw()); } }",
            ),
            (
                "main.wi",
                "import util; import shapes; class Box implements util::Get<shapes::Dot> { pub fn raw(self) -> shapes::Dot { return new shapes::Dot(8); } } fn main() { println(new Box().echo().x); }",
            ),
        ],
        "8\n",
    );
}

#[test]
fn shared_and_distinct_instantiations() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 30; } pub interface Get<T> { fn raw(self) -> T; fn get(self) -> i64 { return helper(); } fn echo(self) -> T { return self.raw(); } }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get<i64> { pub fn raw(self) -> i64 { return 1; } } class B implements util::Get<i64> { pub fn raw(self) -> i64 { return 2; } } class C implements util::Get<String> { pub fn raw(self) -> String { return \"c\"; } } fn main() { println(new A().get() + new A().echo()); println(new B().get() + new B().echo()); println(new C().echo()); }",
            ),
        ],
        "31\n32\nc\n",
    );
}

#[test]
fn interface_typed_parameter_reaches_same_body() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 40; } pub interface Get<T> { fn raw(self) -> i64; fn get(self) -> i64 { return helper() + self.raw(); } }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get<i64> { pub fn raw(self) -> i64 { return 1; } } class B implements util::Get<i64> { pub fn raw(self) -> i64 { return 2; } } fn through(g: util::Get<i64>) -> i64 { return g.get(); } fn main() { println(through(new A())); println(through(new B())); println(new A().get()); }",
            ),
        ],
        "41\n42\n41\n",
    );
}

#[test]
fn receiver_in_non_entry_module() {
    runs(
        &[
            (
                "util.wi",
                &util(&format!("fn helper() -> i64 {{ return 50; }} {GET}")),
            ),
            (
                "recv.wi",
                "module recv; import util; fn helper() -> i64 { return -1; } pub class InMod implements util::Get<i64> { pub n: i64; pub fn raw(self) -> i64 { let f = |x: i64| x + helper(); return f(self.n); } }",
            ),
            (
                "main.wi",
                "import recv; fn main() { println(new recv::InMod(4).get()); println(new recv::InMod(4).raw()); }",
            ),
        ],
        "50\n3\n",
    );
}

#[test]
fn generic_class_receiver() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 60; } pub interface Get<T> { fn raw(self) -> T; fn get(self) -> i64 { return helper(); } fn echo(self) -> T { let w: T = self.raw(); return w; } }",
            ),
            (
                "main.wi",
                "import util; class Gen<T> implements util::Get<T> { pub v: T; pub fn raw(self) -> T { return self.v; } } fn main() { println(new Gen<i64>(3).get()); println(new Gen<i64>(3).echo()); println(new Gen<String>(\"s\").echo()); }",
            ),
        ],
        "60\n3\ns\n",
    );
}

#[test]
fn default_inherited_through_open_base() {
    runs(
        &[
            (
                "util.wi",
                &util(&format!("fn helper() -> i64 {{ return 70; }} {GET}")),
            ),
            (
                "main.wi",
                "import util; open class Parent implements util::Get<i64> { pub fn raw(self) -> i64 { return 1; } } class Child extends Parent { } fn through(g: util::Get<i64>) -> i64 { return g.get(); } fn main() { println(new Child().get()); println(through(new Child())); }",
            ),
        ],
        "70\n70\n",
    );
}

#[test]
fn class_override_wins() {
    runs(
        &[
            (
                "util.wi",
                &util(&format!("fn helper() -> i64 {{ return 80; }} {GET}")),
            ),
            (
                "main.wi",
                "import util; class Over implements util::Get<i64> { pub fn raw(self) -> i64 { return 0; } pub fn get(self) -> i64 { return 555; } } fn through(g: util::Get<i64>) -> i64 { return g.get(); } fn main() { println(new Over().get()); println(through(new Over())); }",
            ),
        ],
        "555\n555\n",
    );
}

#[test]
fn default_to_default_calls_with_parameters() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn tag() -> String { return \"#\"; } pub interface Get<T> { fn raw(self) -> T; fn add(self, a: i64, b: i64) -> i64 { return a + b; } fn sum(self) -> i64 { return self.add(2, 3) * 2; } fn label(self, s: String) -> String { return tag() + s; } fn say(self) { println(self.label(\"hi\")); } }",
            ),
            (
                "main.wi",
                "import util; class Box implements util::Get<i64> { pub fn raw(self) -> i64 { return 1; } } fn main() { println(new Box().sum()); new Box().say(); }",
            ),
        ],
        "10\n#hi\n",
    );
}

#[test]
fn multi_parameter_interface() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn name() -> String { return \"pair\"; } pub interface Pair<A, B> { fn first(self) -> A; fn second(self) -> B; fn both(self) -> String { return name(); } fn left(self) -> A { let a: A = self.first(); return a; } }",
            ),
            (
                "main.wi",
                "import util; class P implements util::Pair<i64, String> { pub fn first(self) -> i64 { return 1; } pub fn second(self) -> String { return \"two\"; } } fn main() { println(new P().both()); println(new P().left()); }",
            ),
        ],
        "pair\n1\n",
    );
}

#[test]
fn self_signature_default_keeps_receiver_copy() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 40; } pub interface Get<T> { fn raw(self) -> T; fn me(self) -> Self { return self; } fn get(self) -> i64 { return helper() + self.two(); } fn two(self) -> i64 { return 2; } }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get<i64> { pub n: i64; pub fn raw(self) -> i64 { return 1; } } fn main() { println(new A(9).me().n); println(new A(1).get()); }",
            ),
        ],
        "9\n42\n",
    );
}

#[test]
fn default_calls_abstract_self_method() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn mk() -> i64 { return 100; } pub interface Get { fn me(self) -> Self; fn two(self) -> i64 { return 2; } fn get(self) -> i64 { return self.me().two() + mk(); } } pub interface GetT<T> { fn me(self) -> Self; fn three(self) -> i64 { return 3; } fn get(self) -> i64 { return self.me().three() + mk(); } }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get { pub fn me(self) -> A { return self; } } class B implements util::GetT<i64> { pub fn me(self) -> B { return self; } } fn main() { println(new A().get()); println(new B().get()); }",
            ),
        ],
        "102\n103\n",
    );
}

#[test]
fn type_error_reported_at_provider_once_per_instantiation() {
    let stderr = fails(&[
        (
            "util.wi",
            "module util; pub interface Get<T> { fn raw(self) -> T; fn get(self) -> i64 { return nope(); } }",
        ),
        (
            "main.wi",
            "import util; class A implements util::Get<i64> { pub fn raw(self) -> i64 { return 1; } } class B implements util::Get<i64> { pub fn raw(self) -> i64 { return 2; } } fn main() { println(new A().get()); }",
        ),
    ]);
    assert_eq!(
        stderr.matches("cannot find function `nope`").count(),
        1,
        "{stderr}"
    );
    assert!(stderr.contains("util.wi:1:85"), "{stderr}");
    assert!(!stderr.contains("main.wi:"), "{stderr}");
}

#[test]
fn panic_in_default_keeps_provider_locations() {
    let project = TestProject::new(
        "xmod_generic_iface_default",
        &[
            (
                "util.wi",
                "module util; fn boom() -> i64 { panic(\"boom from util\"); return 0; } pub interface Get<T> { fn raw(self) -> T; fn get(self) -> i64 { return boom(); } }",
            ),
            (
                "main.wi",
                "import util; class Box implements util::Get<i64> { pub fn raw(self) -> i64 { return 1; } } fn main() { println(new Box().get()); }",
            ),
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
    assert!(stderr.contains("boom from util"), "{stderr}");
    assert!(stderr.contains("util.wi:1:33"), "{stderr}");
}

#[test]
fn emit_lir_one_body_per_receiver() {
    let project = TestProject::new(
        "xmod_generic_iface_default",
        &[
            (
                "util.wi",
                &util(&format!("fn helper() -> i64 {{ return 20; }} {GET}")),
            ),
            (
                "main.wi",
                "import util; class A implements util::Get<i64> { pub fn raw(self) -> i64 { return 1; } } class B implements util::Get<i64> { pub fn raw(self) -> i64 { return 2; } } fn main() { println(new A().get() + new B().get()); }",
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
    assert_eq!(stdout.matches("fn A::get(").count(), 1, "{stdout}");
    assert_eq!(stdout.matches("fn B::get(").count(), 1, "{stdout}");
}

#[test]
fn same_module_generic_interface_unchanged() {
    runs(
        &[(
            "main.wi",
            "interface Get<T> { fn raw(self) -> T; fn me(self) -> Self { return self; } fn echo(self) -> T { return self.raw(); } } class A implements Get<i64> { pub n: i64; pub fn raw(self) -> i64 { return self.n; } } fn main() { println(new A(6).me().echo()); }",
        )],
        "6\n",
    );
}

#[test]
fn generic_function_lambda_at_entry_class() {
    runs(
        &[
            (
                "util.wi",
                "module util; pub fn ap<T>(x: T) -> T { let f = |y: T| y; return f(x); }",
            ),
            (
                "main.wi",
                "import util; class Me { pub n: i64; } fn main() { println(util::ap(new Me(4)).n); }",
            ),
        ],
        "4\n",
    );
}
