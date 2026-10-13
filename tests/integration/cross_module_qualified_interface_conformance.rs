//! A class implementing (or overriding a method of) an imported declaration
//! may write the imported interface by its access path in the signatures it
//! provides (willow-k6n9).
//!
//! The provider resolves its own signatures (`fn peer(self) -> Get`) to the
//! interface's identity, while the implementing class writes `util::Get`, an
//! alias path or an item-imported bare name. Interface conformance (E0416 /
//! E0417) and override matching (E0703) compare those types by identity, so
//! one interface under two spellings conforms and two distinct interfaces
//! under similar spellings still do not.
//!
//! Perspectives:
//!  1. ticket repro: return type names the implemented interface qualified
//!  2. runnable example program
//!  3. parameter typed with the qualified interface
//!  4. generic interface returning its own instantiation
//!  5. generic interface taking its own instantiation as a parameter
//!  6. aliased module import in return and parameter positions
//!  7. item import: bare spelling of the imported interface
//!  8. nested: array of the qualified interface
//!  9. nested: function type naming the qualified interface
//! 10. requirement inherited from a provider super-interface
//! 11. requirement satisfied by a method inherited from an entry base class
//! 12. cross-module override returning the qualified interface
//! 13. cross-module override taking the qualified interface
//! 14. entry class named like the provider interface it implements
//! 15. dynamic dispatch through an interface-typed value
//! 16. conformance checked inside a non-entry module implementing another
//!     module's interface
//! 17. a second provider interface referenced from the first one's signature
//! 18. negative: return of a different provider interface (E0417)
//! 19. negative: parameter of a different provider interface (E0416)
//! 20. negative: generic interface with a different type argument (E0417)
//! 21. negative: same-named entry interface is a different identity (E0417)
//! 22. negative: cross-module override with a different interface (E0703)

use super::support::*;

fn runs(files: &[(&str, &str)], expected: &str) {
    let project = TestProject::new("xmod_qualified_conformance", files);
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

fn rejects(files: &[(&str, &str)], code: &str, message: &str) {
    let project = TestProject::new("xmod_qualified_conformance_err", files);
    let checked = project.check("main.wi");
    let stderr = String::from_utf8_lossy(&checked.stderr);
    assert!(!checked.status.success(), "{stderr}");
    assert!(stderr.contains(code), "{stderr}");
    assert!(stderr.contains(message), "{stderr}");
    assert!(!stderr.contains("$local"), "{stderr}");
}

const GET: &str = "module util; pub interface Get { fn n(self) -> i64; fn peer(self) -> Get; }";

#[test]
fn ticket_repro_qualified_return() {
    runs(
        &[
            ("util.wi", GET),
            (
                "main.wi",
                "import util; class Z implements util::Get { pub k: i64; pub fn n(self) -> i64 { return self.k; } pub fn peer(self) -> util::Get { return new Z(self.k + 1); } } fn main() { println(new Z(1).peer().n()); }",
            ),
        ],
        "2\n",
    );
}

#[test]
fn example_program() {
    runs(
        &[
            (
                "main.wi",
                include_str!("../../example/interface_qualified_self_reference.wi"),
            ),
            (
                "interface_qualified_self_reference_support/shapes.wi",
                include_str!("../../example/interface_qualified_self_reference_support/shapes.wi"),
            ),
        ],
        "36\n16\n25\n",
    );
}

#[test]
fn qualified_parameter() {
    runs(
        &[
            (
                "util.wi",
                "module util; pub interface Get { fn n(self) -> i64; fn add(self, o: Get) -> i64; }",
            ),
            (
                "main.wi",
                "import util; class Z implements util::Get { pub k: i64; pub fn n(self) -> i64 { return self.k; } pub fn add(self, o: util::Get) -> i64 { return self.k + o.n(); } } fn main() { println(new Z(2).add(new Z(5))); }",
            ),
        ],
        "7\n",
    );
}

const BOX: &str = "module util; pub interface Box<T> { fn get(self) -> T; fn peer(self) -> Box<T>; fn take(self, o: Box<T>) -> T; }";

#[test]
fn generic_interface_return() {
    runs(
        &[
            ("util.wi", BOX),
            (
                "main.wi",
                "import util; class B implements util::Box<i64> { pub v: i64; pub fn get(self) -> i64 { return self.v; } pub fn peer(self) -> util::Box<i64> { return new B(self.v * 10); } pub fn take(self, o: util::Box<i64>) -> i64 { return o.get(); } } fn main() { println(new B(3).peer().get()); }",
            ),
        ],
        "30\n",
    );
}

#[test]
fn generic_interface_parameter() {
    runs(
        &[
            ("util.wi", BOX),
            (
                "main.wi",
                "import util; class B implements util::Box<String> { pub v: String; pub fn get(self) -> String { return self.v; } pub fn peer(self) -> util::Box<String> { return self; } pub fn take(self, o: util::Box<String>) -> String { return o.get(); } } fn main() { println(new B(\"a\").take(new B(\"b\"))); }",
            ),
        ],
        "b\n",
    );
}

#[test]
fn aliased_module_import() {
    runs(
        &[
            (
                "util.wi",
                "module util; pub interface Get { fn n(self) -> i64; fn peer(self) -> Get; fn add(self, o: Get) -> i64; }",
            ),
            (
                "main.wi",
                "import util as u; class Z implements u::Get { pub k: i64; pub fn n(self) -> i64 { return self.k; } pub fn peer(self) -> u::Get { return new Z(self.k + 1); } pub fn add(self, o: u::Get) -> i64 { return self.k + o.n(); } } fn main() { let z = new Z(1); println(z.peer().n()); println(z.add(new Z(5))); }",
            ),
        ],
        "2\n6\n",
    );
}

#[test]
fn item_import_bare_spelling() {
    runs(
        &[
            ("util.wi", GET),
            (
                "main.wi",
                "import util::{Get}; class Z implements Get { pub k: i64; pub fn n(self) -> i64 { return self.k; } pub fn peer(self) -> Get { return new Z(self.k + 2); } } fn main() { println(new Z(1).peer().n()); }",
            ),
        ],
        "3\n",
    );
}

#[test]
fn nested_array_of_interface() {
    runs(
        &[
            (
                "util.wi",
                "module util; import std::collections::Array; pub interface Get { fn n(self) -> i64; fn many(self) -> Array<Get>; }",
            ),
            (
                "main.wi",
                "import util; import std::collections::Array; class Z implements util::Get { pub k: i64; pub fn n(self) -> i64 { return self.k; } pub fn many(self) -> Array<util::Get> { let a: Array<util::Get> = [new Z(7)]; return a; } } fn main() { println(new Z(1).many()[0].n()); }",
            ),
        ],
        "7\n",
    );
}

#[test]
fn nested_function_type() {
    runs(
        &[
            (
                "util.wi",
                "module util; pub interface Get { fn n(self) -> i64; fn f(self) -> fn(Get) -> i64; }",
            ),
            (
                "main.wi",
                "import util; fn nn(g: util::Get) -> i64 { return g.n(); } class Z implements util::Get { pub k: i64; pub fn n(self) -> i64 { return self.k; } pub fn f(self) -> fn(util::Get) -> i64 { return nn; } } fn main() { let g = new Z(1).f(); println(g(new Z(9))); }",
            ),
        ],
        "9\n",
    );
}

#[test]
fn super_interface_requirement() {
    runs(
        &[
            (
                "util.wi",
                "module util; pub interface Base { fn peer(self) -> Base; fn n(self) -> i64; } pub interface Get extends Base { fn other(self) -> Get; }",
            ),
            (
                "main.wi",
                "import util; class Z implements util::Get { pub k: i64; pub fn n(self) -> i64 { return self.k; } pub fn peer(self) -> util::Base { return new Z(self.k + 1); } pub fn other(self) -> util::Get { return new Z(self.k + 2); } } fn main() { let z = new Z(1); println(z.peer().n()); println(z.other().n()); }",
            ),
        ],
        "2\n3\n",
    );
}

#[test]
fn requirement_satisfied_by_inherited_method() {
    runs(
        &[
            ("util.wi", GET),
            (
                "main.wi",
                "import util; open class P { pub k: i64; pub fn n(self) -> i64 { return self.k; } pub fn peer(self) -> util::Get { return new Z(self.k + 4); } } class Z extends P implements util::Get {} fn main() { println(new Z(1).peer().n()); }",
            ),
        ],
        "5\n",
    );
}

const FACTORY: &str = "module util; pub interface Get { fn n(self) -> i64; } pub open class Base { pub open fn peer(self) -> Get { return new Leaf(); } pub open fn pass(self, g: Get) -> i64 { return g.n(); } } pub class Leaf implements Get { pub fn n(self) -> i64 { return 4; } }";

#[test]
fn cross_module_override_return() {
    runs(
        &[
            ("util.wi", FACTORY),
            (
                "main.wi",
                "import util; class L implements util::Get { pub fn n(self) -> i64 { return 8; } } class D extends util::Base { pub override fn peer(self) -> util::Get { return new L(); } } fn main() { println(new D().peer().n()); }",
            ),
        ],
        "8\n",
    );
}

#[test]
fn cross_module_override_parameter() {
    runs(
        &[
            ("util.wi", FACTORY),
            (
                "main.wi",
                "import util; class D extends util::Base { pub override fn pass(self, g: util::Get) -> i64 { return g.n() * 10; } } fn main() { println(new D().pass(new util::Leaf())); }",
            ),
        ],
        "40\n",
    );
}

#[test]
fn entry_class_named_like_interface() {
    runs(
        &[
            ("util.wi", GET),
            (
                "main.wi",
                "import util; class Get implements util::Get { pub k: i64; pub fn n(self) -> i64 { return self.k; } pub fn peer(self) -> util::Get { return new Get(self.k + 1); } } fn main() { println(new Get(6).peer().n()); }",
            ),
        ],
        "7\n",
    );
}

#[test]
fn dispatch_through_interface_value() {
    runs(
        &[
            ("util.wi", GET),
            (
                "main.wi",
                "import util; class Z implements util::Get { pub k: i64; pub fn n(self) -> i64 { return self.k; } pub fn peer(self) -> util::Get { return new Z(self.k * 3); } } fn main() { let g: util::Get = new Z(2); println(g.peer().peer().n()); }",
            ),
        ],
        "18\n",
    );
}

#[test]
fn non_entry_module_implements_other_module_interface() {
    runs(
        &[
            ("util.wi", GET),
            (
                "mid.wi",
                "module mid; import util; pub class M implements util::Get { pub k: i64; pub fn n(self) -> i64 { return self.k; } pub fn peer(self) -> util::Get { return new M(self.k + 10); } }",
            ),
            (
                "main.wi",
                "import util; import mid; fn main() { let g: util::Get = new mid::M(1); println(g.peer().n()); }",
            ),
        ],
        "11\n",
    );
}

#[test]
fn sibling_interface_in_signature() {
    runs(
        &[
            (
                "util.wi",
                "module util; pub interface Leaf { fn v(self) -> i64; } pub interface Get { fn leaf(self) -> Leaf; }",
            ),
            (
                "main.wi",
                "import util; class L implements util::Leaf { pub fn v(self) -> i64 { return 12; } } class Z implements util::Get { pub fn leaf(self) -> util::Leaf { return new L(); } } fn main() { println(new Z().leaf().v()); }",
            ),
        ],
        "12\n",
    );
}

const GET_OTHER: &str = "module util; pub interface Get { fn n(self) -> i64; fn peer(self) -> Get; fn eat(self, g: Get) -> i64; } pub interface Other { fn n(self) -> i64; }";

#[test]
fn rejects_different_interface_return() {
    rejects(
        &[
            ("util.wi", GET_OTHER),
            (
                "main.wi",
                "import util; class Z implements util::Get { pub fn n(self) -> i64 { return 1; } pub fn peer(self) -> util::Other { return new Y(); } pub fn eat(self, g: util::Get) -> i64 { return 0; } } class Y implements util::Other { pub fn n(self) -> i64 { return 0; } } fn main() {}",
            ),
        ],
        "E0417",
        "method `peer` returns `util::Other`, but interface `util::Get` requires `util::Get`",
    );
}

#[test]
fn rejects_different_interface_parameter() {
    rejects(
        &[
            ("util.wi", GET_OTHER),
            (
                "main.wi",
                "import util; class Z implements util::Get { pub fn n(self) -> i64 { return 1; } pub fn peer(self) -> util::Get { return self; } pub fn eat(self, g: util::Other) -> i64 { return 0; } } fn main() {}",
            ),
        ],
        "E0416",
        "method `eat` parameters do not match interface `util::Get`",
    );
}

#[test]
fn rejects_different_generic_argument() {
    rejects(
        &[
            ("util.wi", BOX),
            (
                "main.wi",
                "import util; class B implements util::Box<i64> { pub fn get(self) -> i64 { return 1; } pub fn peer(self) -> util::Box<String> { return new S(); } pub fn take(self, o: util::Box<i64>) -> i64 { return 0; } } class S implements util::Box<String> { pub fn get(self) -> String { return \"s\"; } pub fn peer(self) -> util::Box<String> { return self; } pub fn take(self, o: util::Box<String>) -> String { return \"\"; } } fn main() {}",
            ),
        ],
        "E0417",
        "requires `util::Box<i64>`",
    );
}

#[test]
fn rejects_same_named_entry_interface() {
    rejects(
        &[
            ("util.wi", GET),
            (
                "main.wi",
                "import util; interface Get { fn n(self) -> i64; } class L implements Get { pub fn n(self) -> i64 { return 0; } } class Z implements util::Get { pub fn n(self) -> i64 { return 1; } pub fn peer(self) -> Get { return new L(); } } fn main() {}",
            ),
        ],
        "E0417",
        "method `peer` returns `Get`, but interface `util::Get` requires `util::Get`",
    );
}

#[test]
fn rejects_cross_module_override_with_different_interface() {
    rejects(
        &[
            (
                "util.wi",
                "module util; pub interface Get { fn n(self) -> i64; } pub interface Other { fn n(self) -> i64; } pub open class Base { pub open fn peer(self) -> Get { return new Leaf(); } } pub class Leaf implements Get, Other { pub fn n(self) -> i64 { return 4; } }",
            ),
            (
                "main.wi",
                "import util; class D extends util::Base { pub override fn peer(self) -> util::Other { return new util::Leaf(); } } fn main() {}",
            ),
        ],
        "E0703",
        "override `peer` does not match the inherited method signature",
    );
}
