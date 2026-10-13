//! Interface defaults copied into an entry-file class whose name a provider
//! INTERFACE also uses, while the class's own methods are typed with its name
//! (willow-kptd).
//!
//! The provider spells its own interfaces bare, so in its scope the entry
//! class's declared signatures (`fn me(self) -> A`) would name the provider's
//! interface `A`. The copies' scope reads those signatures with the entry
//! types respelled under the receiver prefix, and the copies are compiled
//! under the receiver's respelled identity, `self` included. The receivers'
//! chains name the provider's own interfaces as the provider spells them, so
//! `self` converts to the interface it implements there.
//!
//! Perspectives:
//!  1. ticket repro: `me` returns the receiver by name, the default also uses
//!     the provider's own interface `A`
//!  2. runnable example program
//!  3. leaf default (unrooted `self`) calling the receiver's method
//!  4. generic interface with a Self-returning abstract method
//!  5. generic interface instantiated with the receiver, `T`-returning method
//!  6. chained receiver-returning calls
//!  7. the receiver's result stored in a local and used twice
//!  8. two receivers, each named like a provider interface
//!  9. receiver-typed method inherited from an entry base class that is also
//!     named like a provider interface
//! 10. provider item-imports the interface named like the receiver
//! 11. receiver's result passed to a provider function taking the
//!     implemented interface
//! 12. provider interface used as a local annotation in the default
//! 13. provider interface and provider class named like two receivers
//! 14. provider enum and provider interface named like two receivers
//! 15. receiver declared in a non-entry module (qualified, not respelled)
//! 16. class override of the default the other default calls
//! 17. aliased module import of the provider
//! 18. entry code keeps the entry meaning of the receiver's signatures
//! 19. runtime panic in a copied default reports the receiver by its name
//! 20. `--emit-hir` keeps the receiver's name
//! 21. `--emit-lir` emits the copied default under the receiver's name
//! 22. a receiver no provider name shadows keeps its own spelling
//! 23. `self` passed to a provider function taking the implemented interface,
//!     no name collision
//! 24. same, receiver named like a provider interface
//! 25. `self` passed as the implemented interface's super-interface

use super::support::*;

fn runs(files: &[(&str, &str)], expected: &str) {
    let project = TestProject::new("xmod_receiver_sigs", files);
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

/// A provider with a private interface `A`, a class boxed into it and a
/// function taking it.
const PRELUDE: &str = "module util; interface A { fn v(self) -> i64; } class Q implements A { pub fn v(self) -> i64 { return 3; } } fn f(a: A) -> i64 { return a.v(); }";

fn util(rest: &str) -> String {
    format!("{PRELUDE} {rest}")
}

/// The ticket's provider.
const GET: &str = "pub interface Get { fn me(self) -> Self; fn get(self) -> i64 { return f(new Q()) + self.me().n2(); } fn n2(self) -> i64 { return 100; } }";

const REPRO_MAIN: &str = "import util; class A implements util::Get { pub n: i64; pub fn me(self) -> A { return self; } } fn main() { println(new A(1).get()); }";

/// A provider whose default calls the receiver's `me` and `n`.
const ME_N: &str = "pub interface Get { fn me(self) -> Self; fn n(self) -> i64; fn get(self) -> i64 { return self.me().n() + f(new Q()); } }";

/// An entry class `name` implementing `iface`, whose `me` returns it by name.
fn receiver(name: &str, iface: &str, n: &str) -> String {
    format!(
        "class {name} implements {iface} {{ pub k: i64; pub fn me(self) -> {name} {{ return new {name}(self.k + 1); }} pub fn n(self) -> i64 {{ return {n}; }} }}"
    )
}

#[test]
fn ticket_repro_receiver_named_like_provider_interface() {
    runs(&[("util.wi", &util(GET)), ("main.wi", REPRO_MAIN)], "103\n");
}

#[test]
fn example_program() {
    runs(
        &[
            (
                "main.wi",
                include_str!("../../example/interface_default_receiver_signatures.wi"),
            ),
            (
                "interface_default_receiver_signatures_support/sizing.wi",
                include_str!(
                    "../../example/interface_default_receiver_signatures_support/sizing.wi"
                ),
            ),
        ],
        "109\n101\n",
    );
}

#[test]
fn leaf_default_calls_receiver_method() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Get { fn me(self) -> Self; fn n2(self) -> i64 { return 100; } fn n3(self) -> i64 { return self.me().n2(); } }",
                ),
            ),
            (
                "main.wi",
                "import util; class A implements util::Get { pub n: i64; pub fn me(self) -> A { return self; } } fn main() { println(new A(1).n3()); }",
            ),
        ],
        "100\n",
    );
}

#[test]
fn generic_interface_self_returning() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Get<T> { fn me(self) -> Self; fn n(self) -> i64; fn get(self) -> i64 { return self.me().n() + f(new Q()); } }",
                ),
            ),
            (
                "main.wi",
                &format!(
                    "import util; {} fn main() {{ println(new A(1).get()); }}",
                    receiver("A", "util::Get<i64>", "self.k")
                ),
            ),
        ],
        "5\n",
    );
}

#[test]
fn generic_interface_instantiated_with_receiver() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Get<T> { fn mk(self) -> T; fn get(self) -> i64 { let t = self.mk(); let u = self.mk(); return f(new Q()); } }",
                ),
            ),
            (
                "main.wi",
                "import util; class A implements util::Get<A> { pub n: i64; pub fn mk(self) -> A { return new A(self.n + 1); } } fn main() { println(new A(1).get()); }",
            ),
        ],
        "3\n",
    );
}

#[test]
fn chained_receiver_calls() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Get { fn me(self) -> Self; fn n(self) -> i64; fn get(self) -> i64 { return self.me().me().me().n() + f(new Q()); } }",
                ),
            ),
            (
                "main.wi",
                &format!(
                    "import util; {} fn main() {{ println(new A(1).get()); }}",
                    receiver("A", "util::Get", "self.k")
                ),
            ),
        ],
        "7\n",
    );
}

#[test]
fn receiver_result_in_local() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Get { fn me(self) -> Self; fn n(self) -> i64; fn get(self) -> i64 { let x = self.me(); return x.n() * 10 + x.me().n() + f(new Q()); } }",
                ),
            ),
            (
                "main.wi",
                &format!(
                    "import util; {} fn main() {{ println(new A(1).get()); }}",
                    receiver("A", "util::Get", "self.k")
                ),
            ),
        ],
        "26\n",
    );
}

#[test]
fn two_receivers_named_like_provider_interfaces() {
    let provider = "module util; interface A { fn v(self) -> i64; } interface B { fn w(self) -> i64; } class Q implements A, B { pub fn v(self) -> i64 { return 3; } pub fn w(self) -> i64 { return 4; } } fn f(a: A) -> i64 { return a.v(); } fn g(b: B) -> i64 { return b.w(); } pub interface Get { fn me(self) -> Self; fn n(self) -> i64; fn get(self) -> i64 { return f(new Q()) + g(new Q()) + self.me().n(); } }";
    runs(
        &[
            ("util.wi", provider),
            (
                "main.wi",
                &format!(
                    "import util; {} {} fn main() {{ println(new A(10).get()); println(new B(10).get()); }}",
                    receiver("A", "util::Get", "self.k"),
                    receiver("B", "util::Get", "self.k * 2"),
                ),
            ),
        ],
        "18\n29\n",
    );
}

#[test]
fn receiver_method_inherited_from_base_named_like_provider_interface() {
    runs(
        &[
            (
                "util.wi",
                "module util; interface A { fn v(self) -> i64; } interface Base { fn v(self) -> i64; } class Q implements A { pub fn v(self) -> i64 { return 3; } } fn f(a: A) -> i64 { return a.v(); } pub interface Get { fn me(self) -> Self; fn n(self) -> i64; fn get(self) -> i64 { return self.me().n() + f(new Q()); } }",
            ),
            (
                "main.wi",
                "import util; open class Base { pub k: i64; pub fn me(self) -> A { return new A(self.k + 1); } } class A extends Base implements util::Get { pub fn n(self) -> i64 { return self.k; } } fn main() { println(new A(1).get()); }",
            ),
        ],
        "5\n",
    );
}

#[test]
fn provider_item_imports_interface_named_like_receiver() {
    runs(
        &[
            (
                "proto.wi",
                "module proto; pub interface A { fn v(self) -> i64; }",
            ),
            (
                "util.wi",
                &format!(
                    "module util; import proto::A; class Q implements A {{ pub fn v(self) -> i64 {{ return 3; }} }} fn f(a: A) -> i64 {{ return a.v(); }} {ME_N}"
                ),
            ),
            (
                "main.wi",
                &format!(
                    "import util; {} fn main() {{ println(new A(7).get()); }}",
                    receiver("A", "util::Get", "self.k")
                ),
            ),
        ],
        "11\n",
    );
}

#[test]
fn receiver_result_passed_as_implemented_interface() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Get { fn me(self) -> Self; fn n(self) -> i64; fn get(self) -> i64 { return h(self.me()) + f(new Q()); } } fn h(g: Get) -> i64 { return g.n() * 100; }",
                ),
            ),
            (
                "main.wi",
                &format!(
                    "import util; {} fn main() {{ println(new A(1).get()); }}",
                    receiver("A", "util::Get", "self.k")
                ),
            ),
        ],
        "203\n",
    );
}

#[test]
fn provider_interface_as_local_annotation() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Get { fn me(self) -> Self; fn n(self) -> i64; fn get(self) -> i64 { let a: A = new Q(); return a.v() * 10 + self.me().n(); } }",
                ),
            ),
            (
                "main.wi",
                &format!(
                    "import util; {} fn main() {{ println(new A(1).get()); }}",
                    receiver("A", "util::Get", "self.k")
                ),
            ),
        ],
        "32\n",
    );
}

#[test]
fn provider_interface_and_class_named_like_receivers() {
    runs(
        &[
            (
                "util.wi",
                &util(&format!(
                    "class B {{ pub fn n(self) -> String {{ return \"x\"; }} }} {ME_N}"
                )),
            ),
            (
                "main.wi",
                &format!(
                    "import util; {} {} fn main() {{ println(new A(1).get()); println(new B(1).get()); }}",
                    receiver("A", "util::Get", "self.k"),
                    receiver("B", "util::Get", "self.k * 5"),
                ),
            ),
        ],
        "5\n13\n",
    );
}

#[test]
fn provider_interface_and_enum_named_like_receivers() {
    runs(
        &[
            ("util.wi", &util(&format!("enum B {{ X, Y }} {ME_N}"))),
            (
                "main.wi",
                &format!(
                    "import util; {} {} fn main() {{ println(new A(1).get()); println(new B(2).get()); }}",
                    receiver("A", "util::Get", "self.k"),
                    receiver("B", "util::Get", "self.k * 5"),
                ),
            ),
        ],
        "5\n18\n",
    );
}

#[test]
fn receiver_in_non_entry_module() {
    runs(
        &[
            ("util.wi", &util(ME_N)),
            (
                "shapes.wi",
                &format!(
                    "module shapes; import util; pub {}",
                    receiver("A", "util::Get", "self.k")
                ),
            ),
            (
                "main.wi",
                "import shapes; fn main() { println(new shapes::A(4).get()); }",
            ),
        ],
        "8\n",
    );
}

#[test]
fn class_override_of_called_default() {
    runs(
        &[
            ("util.wi", &util(GET)),
            (
                "main.wi",
                "import util; class A implements util::Get { pub n: i64; pub fn me(self) -> A { return self; } pub fn n2(self) -> i64 { return 40; } } fn main() { println(new A(1).get()); }",
            ),
        ],
        "43\n",
    );
}

#[test]
fn aliased_provider_import() {
    runs(
        &[
            ("util.wi", &util(GET)),
            (
                "main.wi",
                "import util as u; class A implements u::Get { pub n: i64; pub fn me(self) -> A { return self; } } fn main() { println(new A(1).get()); }",
            ),
        ],
        "103\n",
    );
}

#[test]
fn entry_code_keeps_receiver_signatures() {
    runs(
        &[
            ("util.wi", &util(ME_N)),
            (
                "main.wi",
                &format!(
                    "import util; {} fn take(a: A) -> i64 {{ return a.me().n(); }} fn main() {{ let a = new A(1); println(a.get()); println(take(a.me())); }}",
                    receiver("A", "util::Get", "self.k")
                ),
            ),
        ],
        "5\n3\n",
    );
}

#[test]
fn runtime_panic_names_the_receiver() {
    let project = TestProject::new(
        "xmod_receiver_sigs",
        &[
            (
                "util.wi",
                &util(
                    "pub interface Get { fn me(self) -> Self; fn n(self) -> i64; fn get(self) -> i64 { if self.me().n() == 2 { panic(\"boom\"); } return f(new Q()); } }",
                ),
            ),
            (
                "main.wi",
                "import util; class A implements util::Get { pub k: i64; pub fn me(self) -> A { return self; } pub fn n(self) -> i64 { return self.k; } } fn main() { println(new A(2).get()); }",
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
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("boom"), "{stderr}");
    assert!(stderr.contains("A::get"), "{stderr}");
    assert!(!stderr.contains("$receiver"), "{stderr}");
}

#[test]
fn emit_hir_keeps_receiver_name() {
    let project = TestProject::new(
        "xmod_receiver_sigs",
        &[("util.wi", &util(GET)), ("main.wi", REPRO_MAIN)],
    );
    let output = project.emit_hir("main.wi");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(stdout.matches("class A {").count(), 1, "{stdout}");
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
        "xmod_receiver_sigs",
        &[("util.wi", &util(GET)), ("main.wi", REPRO_MAIN)],
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
        "xmod_receiver_sigs",
        &[
            ("util.wi", &util(GET)),
            (
                "main.wi",
                "import util; class B implements util::Get { pub fn me(self) -> B { return self; } } fn main() { println(new B().get()); }",
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
    let compiled = project.compile("main.wi");
    assert!(compiled.status.success());
    assert_eq!(String::from_utf8_lossy(&project.run().stdout), "103\n");
}

/// A provider whose default passes `self` to a function taking `param`.
fn passes_self(param: &str, interfaces: &str) -> String {
    format!("module util; {interfaces} fn h(g: {param}) -> i64 {{ return g.n() * 100; }}")
}

#[test]
fn self_passed_as_implemented_interface() {
    runs(
        &[
            (
                "util.wi",
                &passes_self(
                    "Get",
                    "pub interface Get { fn n(self) -> i64; fn get(self) -> i64 { return h(self); } }",
                ),
            ),
            (
                "main.wi",
                "import util; class Z implements util::Get { pub k: i64; pub fn n(self) -> i64 { return self.k; } } fn main() { println(new Z(1).get()); }",
            ),
        ],
        "100\n",
    );
}

#[test]
fn self_passed_as_implemented_interface_with_collision() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Get { fn n(self) -> i64; fn get(self) -> i64 { return h(self) + f(new Q()); } } fn h(g: Get) -> i64 { return g.n() * 100; }",
                ),
            ),
            (
                "main.wi",
                "import util; class A implements util::Get { pub k: i64; pub fn n(self) -> i64 { return self.k; } } fn main() { println(new A(1).get()); }",
            ),
        ],
        "103\n",
    );
}

#[test]
fn self_passed_as_super_interface() {
    runs(
        &[
            (
                "util.wi",
                &passes_self(
                    "Base",
                    "pub interface Base { fn n(self) -> i64; } pub interface Get extends Base { fn get(self) -> i64 { return h(self); } }",
                ),
            ),
            (
                "main.wi",
                "import util; class Z implements util::Get { pub k: i64; pub fn n(self) -> i64 { return self.k; } } fn main() { println(new Z(2).get()); }",
            ),
        ],
        "200\n",
    );
}
