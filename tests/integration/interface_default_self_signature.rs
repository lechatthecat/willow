//! Interface defaults whose signature names `Self` (willow-dnia).
//!
//! The body is checked once at the interface, with `self: Self` an opaque
//! receiver that converts to the interface and reaches its methods; every
//! implementing class's copy is lowered with `Self` as that class. This holds
//! in one module and for another module's plain and generic interfaces, whose
//! private items stay reachable from the default.
//!
//! Perspectives:
//!  1. ticket repro: `fn me(self) -> Self { return self; }` in one module
//!  2. `other: Self` parameter returned
//!  3. `self.me().v()` chains through a `Self` result
//!  4. `Option<Self>` built from `self`
//!  5. `Array<Self>` built from `self` and a `Self` call
//!  6. `self` passed to an interface-typed parameter
//!  7. `self` bound to an interface-typed `let`
//!  8. `self` converts to a super-interface
//!  9. a super-interface's `Self` default called on the class
//! 10. two receivers share one checked default, each typed as itself
//! 11. dynamic call through an interface value returns the interface
//! 12. a lambda capturing a `Self` local
//! 13. a `Self` default calls another `Self` default
//! 14. cross-module plain interface reaching a provider-private helper
//! 15. cross-module generic interface reaching a provider-private helper
//! 16. two instantiations of one generic provider interface
//! 17. implementing class in a non-entry module
//! 18. runnable example
//! 19. returning the interface where `Self` is expected is rejected
//! 20. field access on `self` names `Self` and the interface
//! 21. `self` where a concrete class is expected is rejected
//! 22. an undeclared method on `self` names the interface
//! 23. a `Self` argument of another class is rejected at the call site
//! 24. a default's error is reported once with two implementers
//! 25. a `Self` default is checked with no implementer
//! 26. a cross-module generic `Self` default's error is reported once
//! 27. a `Self` parameter through a plain/generic interface value is rejected
//!     (the generic form used to fail Cranelift verification)
//! 28. an `Option<Self>` result through an interface value is rejected (the
//!     generic form used to print nothing)
//! 29. inside a default, another interface value's `Self` is not the receiver
//! 30. generic functions infer a whole `self` as the interface; enums and a
//!     converted `self` carry the payload through a generic callee
//! 31. `Self` inside a generic argument (`Option`, variable, array, closure)
//!     is rejected once, naming the conversion
//! 32. limitation: `id(self)` returns the interface, not `Self`, same-module
//!     and cross-module generic
//! 33. a module's generic function instantiated at the module's own generic
//!     interface (`id<Get<i64>>`), from a function and a copied default: the
//!     argument is not the entry's shadowed type (was an ICE)
//! 34. the module's own `Get` and the entry's same-named `Get` both instance
//!     arguments in one unit: only the entry's is respelled, and the alias
//!     never replaces the declaration in resolution/registry tables
//! 35. the same for same-named enums and classes

use super::support::*;

fn runs(files: &[(&str, &str)], expected: &str) {
    let project = TestProject::new("iface_default_self_sig", files);
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
    let project = TestProject::new("iface_default_self_sig", files);
    let compiled = project.compile("main.wi");
    let stderr = String::from_utf8_lossy(&compiled.stderr).into_owned();
    assert!(!compiled.status.success(), "{stderr}");
    assert!(!stderr.contains("internal compiler error"), "{stderr}");
    assert!(!stderr.contains("$mono$"), "{stderr}");
    stderr
}

fn one(source: &str, expected: &str) {
    runs(&[("main.wi", source)], expected);
}

const GET: &str = "interface Get { fn v(self) -> i64; fn me(self) -> Self { return self; } }";
const BOX: &str =
    "class Box implements Get { pub value: i64; pub fn v(self) -> i64 { return self.value; } }";

fn with_get(extra: &str, main: &str) -> String {
    format!("import std::collections::Array; {GET} {extra} {BOX} fn main() {{ {main} }}")
}

#[test]
fn ticket_repro_returns_self() {
    one(
        &with_get("", "let b: Box = new Box(5).me(); println(b.value);"),
        "5\n",
    );
}

#[test]
fn self_parameter_is_returned() {
    one(
        "interface Get { fn v(self) -> i64; fn pick(self, other: Self) -> Self { if other.v() > self.v() { return other; } return self; } } class Box implements Get { pub value: i64; pub fn v(self) -> i64 { return self.value; } } fn main() { let b: Box = new Box(1).pick(new Box(9)); println(b.value); let c: Box = new Box(4).pick(new Box(2)); println(c.value); }",
        "9\n4\n",
    );
}

#[test]
fn self_result_chains() {
    one(
        "interface Get { fn v(self) -> i64; fn me(self) -> Self { return self; } fn twice(self) -> i64 { return self.me().v() * 2; } } class Box implements Get { pub value: i64; pub fn v(self) -> i64 { return self.value; } } fn main() { println(new Box(5).twice()); }",
        "10\n",
    );
}

#[test]
fn option_of_self() {
    one(
        "interface Get { fn v(self) -> i64; fn wrap(self) -> Option<Self> { return Option::Some(self); } } class Box implements Get { pub value: i64; pub fn v(self) -> i64 { return self.value; } } fn main() { match new Box(6).wrap() { Option::Some(b) => println(b.value), Option::None => println(0) } }",
        "6\n",
    );
}

#[test]
fn array_of_self() {
    one(
        "import std::collections::Array; interface Many { fn v(self) -> i64; fn me(self) -> Self; fn many(self) -> Array<Self> { return [self, self.me()]; } } class Two implements Many { pub n: i64; pub fn v(self) -> i64 { return self.n; } pub fn me(self) -> Two { return new Two(self.n + 1); } } fn main() { let xs: Array<Two> = new Two(1).many(); println(xs.len()); println(xs[1].n); }",
        "2\n2\n",
    );
}

#[test]
fn self_converts_to_interface_parameter() {
    one(
        "interface Get { fn v(self) -> i64; fn shown(self) -> i64 { return show(self); } fn me(self) -> Self { show(self); return self; } } fn show(g: Get) -> i64 { return g.v() * 10; } class Box implements Get { pub value: i64; pub fn v(self) -> i64 { return self.value; } } fn main() { println(new Box(3).shown()); println(new Box(4).me().value); }",
        "30\n4\n",
    );
}

#[test]
fn self_converts_to_interface_let() {
    one(
        "interface Get { fn v(self) -> i64; fn me(self) -> Self { let g: Get = self; println(g.v()); return self; } } class Box implements Get { pub value: i64; pub fn v(self) -> i64 { return self.value; } } fn main() { println(new Box(8).me().value); }",
        "8\n8\n",
    );
}

#[test]
fn self_converts_to_super_interface() {
    one(
        "interface Base { fn id(self) -> i64; } interface Get extends Base { fn me(self) -> Self { println(base(self)); return self; } } fn base(b: Base) -> i64 { return b.id(); } class Box implements Get { pub value: i64; pub fn id(self) -> i64 { return self.value + 1; } } fn main() { println(new Box(2).me().value); }",
        "3\n2\n",
    );
}

#[test]
fn super_interface_self_default() {
    one(
        "interface Base { fn id(self) -> i64; fn same(self) -> Self { return self; } } interface Get extends Base { fn me(self) -> Self { return self.same(); } } class Box implements Get { pub value: i64; pub fn id(self) -> i64 { return self.value; } } fn main() { let a: Box = new Box(2).same(); let b: Box = new Box(3).me(); println(a.value + b.value); }",
        "5\n",
    );
}

#[test]
fn two_receivers_are_each_typed_as_themselves() {
    one(
        &with_get(
            "",
            "",
        )
        .replace(
            "fn main() {  }",
            "class Pair implements Get { pub a: i64; pub b: i64; pub fn v(self) -> i64 { return self.a + self.b; } } fn main() { let b: Box = new Box(1).me(); let p: Pair = new Pair(2, 3).me(); println(b.value); println(p.b); }",
        ),
        "1\n3\n",
    );
}

#[test]
fn dynamic_call_returns_interface() {
    one(
        &with_get(
            "",
            "let g: Get = new Box(7); let h: Get = g.me(); println(h.v());",
        ),
        "7\n",
    );
}

#[test]
fn lambda_captures_self_local() {
    one(
        "interface Get { fn v(self) -> i64; fn later(self) -> i64 { let s = self; let f = |k: i64| { return k + s.v(); }; return f(100); } fn me(self) -> Self { return self; } } class Box implements Get { pub value: i64; pub fn v(self) -> i64 { return self.value; } } fn main() { println(new Box(5).later()); }",
        "105\n",
    );
}

#[test]
fn self_default_calls_self_default() {
    one(
        "interface Get { fn v(self) -> i64; fn me(self) -> Self { return self; } fn again(self) -> Self { return self.me().me(); } } class Box implements Get { pub value: i64; pub fn v(self) -> i64 { return self.value; } } fn main() { let b: Box = new Box(11).again(); println(b.value); }",
        "11\n",
    );
}

#[test]
fn cross_module_plain_interface_private_helper() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 20; } pub interface Get { fn v(self) -> i64; fn me(self) -> Self { if helper() > 0 { return self; } return self; } fn plus(self) -> i64 { return self.me().v() + helper(); } }",
            ),
            (
                "main.wi",
                "import util; class Box implements util::Get { pub value: i64; pub fn v(self) -> i64 { return self.value; } } fn main() { let b: Box = new Box(3).me(); println(b.value); println(b.plus()); }",
            ),
        ],
        "3\n23\n",
    );
}

#[test]
fn cross_module_generic_interface_private_helper() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 20; } pub interface Get<T> { fn n(self) -> T; fn pick(self, other: Self) -> Self { if helper() > 10 { return other; } return self; } }",
            ),
            (
                "main.wi",
                "import util; class Box implements util::Get<i64> { pub value: i64; pub fn n(self) -> i64 { return self.value; } } fn main() { let b: Box = new Box(3).pick(new Box(8)); println(b.value); let c: Box = new Box(9).pick(b); println(c.n()); }",
            ),
        ],
        "8\n8\n",
    );
}

#[test]
fn cross_module_generic_two_instantiations() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn tag() -> String { return \"!\"; } pub interface Get<T> { fn n(self) -> T; fn me(self) -> Self { print(tag()); return self; } }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get<i64> { pub k: i64; pub fn n(self) -> i64 { return self.k; } } class B implements util::Get<String> { pub s: String; pub fn n(self) -> String { return self.s; } } fn main() { println(new A(1).me().k); println(new B(\"b\").me().s); }",
            ),
        ],
        "!1\n!b\n",
    );
}

#[test]
fn implementing_class_in_non_entry_module() {
    runs(
        &[
            (
                "util.wi",
                "module util; fn helper() -> i64 { return 2; } pub interface Get<T> { fn n(self) -> T; fn me(self) -> Self { if helper() > 1 { return self; } return self; } }",
            ),
            (
                "shapes.wi",
                "module shapes; import util; class Box implements util::Get<i64> { pub value: i64; pub fn n(self) -> i64 { return self.value; } } pub fn make() -> i64 { let b: Box = new Box(6).me(); return b.value; }",
            ),
            (
                "main.wi",
                "import shapes; fn main() { println(shapes::make()); }",
            ),
        ],
        "6\n",
    );
}

#[test]
fn example_program() {
    runs(
        &[
            (
                "main.wi",
                include_str!("../../example/interface_default_self_signature.wi"),
            ),
            (
                "interface_default_self_signature_support/shapes.wi",
                include_str!("../../example/interface_default_self_signature_support/shapes.wi"),
            ),
        ],
        "beta\nbeta\n14\n12\nbeta\n",
    );
}

#[test]
fn interface_is_not_self() {
    let stderr = fails(&[(
        "main.wi",
        "interface Get { fn v(self) -> i64; fn bad(self, g: Get) -> Self { return g; } } class Box implements Get { pub value: i64; pub fn v(self) -> i64 { return self.value; } } fn main() { println(new Box(1).v()); }",
    )]);
    assert!(stderr.contains("expected `Self`, found `Get`"), "{stderr}");
}

#[test]
fn field_access_on_self_names_interface() {
    let stderr = fails(&[(
        "main.wi",
        "interface Get { fn v(self) -> i64; fn me(self) -> Self { println(self.value); return self; } } class Box implements Get { pub value: i64; pub fn v(self) -> i64 { return self.value; } } fn main() { println(new Box(1).v()); }",
    )]);
    assert!(stderr.contains("E0502"), "{stderr}");
    assert!(
        stderr.contains("no field `value` on `Self` in a default of `Get`"),
        "{stderr}"
    );
}

#[test]
fn self_is_not_a_concrete_class() {
    let stderr = fails(&[(
        "main.wi",
        "interface Get { fn v(self) -> i64; fn me(self) -> Self { println(take(self)); return self; } } fn take(b: Box) -> i64 { return b.value; } class Box implements Get { pub value: i64; pub fn v(self) -> i64 { return self.value; } } fn main() { println(new Box(1).v()); }",
    )]);
    assert!(stderr.contains("expected `Box`, found `Self`"), "{stderr}");
}

#[test]
fn undeclared_method_names_interface() {
    let stderr = fails(&[(
        "main.wi",
        "interface Get { fn v(self) -> i64; fn me(self) -> Self { self.nope(); return self; } } class Box implements Get { pub value: i64; pub fn v(self) -> i64 { return self.value; } } fn main() { println(new Box(1).v()); }",
    )]);
    assert!(
        stderr.contains("no method `nope` on interface `Get`"),
        "{stderr}"
    );
}

#[test]
fn self_argument_of_other_class_is_rejected() {
    let stderr = fails(&[(
        "main.wi",
        "interface Get { fn v(self) -> i64; fn pick(self, other: Self) -> Self { return other; } } class Box implements Get { pub value: i64; pub fn v(self) -> i64 { return self.value; } } class Other implements Get { pub fn v(self) -> i64 { return 0; } } fn main() { let b: Box = new Box(1).pick(new Other()); println(b.value); }",
    )]);
    assert!(stderr.contains("expected `Box`, found `Other`"), "{stderr}");
}

#[test]
fn default_error_reported_once_with_two_implementers() {
    let stderr = fails(&[(
        "main.wi",
        "interface Get { fn v(self) -> i64; fn bad(self, g: Get) -> Self { return g; } } class A implements Get { pub fn v(self) -> i64 { return 1; } } class B implements Get { pub fn v(self) -> i64 { return 2; } } fn main() { println(new A().v() + new B().v()); }",
    )]);
    assert_eq!(
        stderr.matches("expected `Self`, found `Get`").count(),
        1,
        "{stderr}"
    );
}

#[test]
fn self_default_checked_without_implementer() {
    let stderr = fails(&[(
        "main.wi",
        "interface Get { fn v(self) -> i64; fn bad(self) -> Self { return 1; } } fn main() { println(1); }",
    )]);
    assert!(stderr.contains("expected `Self`, found `i64`"), "{stderr}");
}

#[test]
fn cross_module_generic_error_reported_once() {
    let stderr = fails(&[
        (
            "util.wi",
            "module util; pub interface Get<T> { fn n(self) -> T; fn bad(self) -> Self { return 1; } }",
        ),
        (
            "main.wi",
            "import util; class A implements util::Get<i64> { pub fn n(self) -> i64 { return 1; } } class B implements util::Get<i64> { pub fn n(self) -> i64 { return 2; } } fn main() { println(new A().n() + new B().n()); }",
        ),
    ]);
    assert_eq!(
        stderr
            .matches("mismatched types: expected `Self`, found `i64`")
            .count(),
        1,
        "{stderr}"
    );
}

#[test]
fn self_parameter_through_interface_value_is_rejected() {
    for (decl, ty) in [("Get", "Get"), ("Get<T>", "Get<i64>")] {
        let stderr = fails(&[(
            "main.wi",
            &format!(
                "interface {decl} {{ fn n(self) -> i64; fn pick(self, other: Self) -> Self {{ return other; }} }} class Box implements {ty} {{ pub value: i64; pub fn n(self) -> i64 {{ return self.value; }} }} fn main() {{ let g: {ty} = new Box(3); println(g.pick(new Box(9)).n()); }}"
            ),
        )]);
        assert!(
            stderr.contains(&format!(
                "method `pick` names `Self` in a parameter, so it cannot be called through interface type `{ty}`"
            )),
            "{stderr}"
        );
    }
}

#[test]
fn compound_self_result_through_interface_value_is_rejected() {
    for (decl, ty) in [("Get", "Get"), ("Get<T>", "Get<i64>")] {
        let stderr = fails(&[(
            "main.wi",
            &format!(
                "interface {decl} {{ fn n(self) -> i64; fn wrap(self) -> Option<Self> {{ return Option::Some(self); }} }} class Box implements {ty} {{ pub value: i64; pub fn n(self) -> i64 {{ return self.value; }} }} fn main() {{ let g: {ty} = new Box(3); match g.wrap() {{ Option::Some(x) => println(x.n()), Option::None => println(0) }} }}"
            ),
        )]);
        assert!(
            stderr.contains(&format!(
                "method `wrap` names `Self` in its result, so it cannot be called through interface type `{ty}`"
            )),
            "{stderr}"
        );
    }
}

#[test]
fn self_of_another_interface_value_is_not_the_receiver() {
    let stderr = fails(&[(
        "main.wi",
        "interface Get { fn n(self) -> i64; fn pick(self, other: Self) -> Self { return other; } fn mix(self, g: Get) -> i64 { return g.pick(self).n(); } } class Box implements Get { pub value: i64; pub fn n(self) -> i64 { return self.value; } } fn main() { println(new Box(1).n()); }",
    )]);
    assert!(
        stderr.contains("method `pick` names `Self` in a parameter, so it cannot be called through interface type `Get`"),
        "{stderr}"
    );
}

#[test]
fn generic_function_infers_self_as_interface() {
    one(
        "enum Wrap<T> { One(T), Zero } interface Get { fn n(self) -> i64; fn total(self) -> i64 { let w = Wrap::One(self); let it: Get = self; match w { Wrap::One(x) => { return id(self).n() + pick(self) + first(Option::Some(it)).n() + x.n(); } Wrap::Zero => { return 0; } } } } fn id<T>(x: T) -> T { return x; } fn pick(g: Get) -> i64 { return g.n(); } fn first<T>(o: Option<T>) -> T { match o { Option::Some(v) => { return v; } Option::None => { panic(\"none\"); } } } class Box implements Get { pub other: i64; pub value: i64; pub fn skip(self) -> i64 { return 0; } pub fn n(self) -> i64 { return self.value; } } class Other implements Get { pub fn n(self) -> i64 { return 100; } } fn main() { println(new Box(0, 3).total()); println(new Other().total()); }",
        "12\n400\n",
    );
}

#[test]
fn self_inside_generic_argument_is_rejected() {
    for (body, shown, callee) in [
        (
            "return first(Option::Some(self)).n();",
            "Option<Self>",
            "first",
        ),
        (
            "let o = Option::Some(self); return first(o).n();",
            "Option<Self>",
            "first",
        ),
        ("return head([self]).n();", "Array<Self>", "head"),
        (
            "let s = self; return apply(|| s).n();",
            "closure() -> Self",
            "apply",
        ),
    ] {
        let stderr = fails(&[(
            "main.wi",
            &format!(
                "import std::collections::Array; interface Get {{ fn n(self) -> i64; fn t(self) -> i64 {{ {body} }} }} fn first<T>(o: Option<T>) -> T {{ match o {{ Option::Some(v) => {{ return v; }} Option::None => {{ panic(\"none\"); }} }} }} fn head<T>(xs: Array<T>) -> T {{ return xs[0]; }} fn apply<T>(f: closure() -> T) -> T {{ return f(); }} class Box implements Get {{ pub value: i64; pub fn n(self) -> i64 {{ return self.value; }} }} fn main() {{ println(new Box(5).t()); }}"
            ),
        )]);
        assert!(
            stderr.contains(&format!(
                "`{shown}` cannot be passed to generic function `{callee}`: its `Self` is not a whole value that converts to `Get`"
            )),
            "{body}\n{stderr}"
        );
        assert!(stderr.contains("let it: Get = self;"), "{stderr}");
        assert_eq!(stderr.matches("error[").count(), 1, "{body}\n{stderr}");
    }
}

#[test]
fn generic_identity_of_self_is_the_interface() {
    let id = "fn id<T>(x: T) -> T { return x; }";
    let same = fails(&[(
        "main.wi",
        &format!(
            "interface Get {{ fn n(self) -> i64; fn me(self) -> Self {{ return id(self); }} }} {id} class Box implements Get {{ pub fn n(self) -> i64 {{ return 1; }} }} fn main() {{ println(new Box().me().n()); }}"
        ),
    )]);
    assert!(
        same.contains("mismatched types: expected `Self`, found `Get`"),
        "{same}"
    );
    let cross = fails(&[
        (
            "util.wi",
            &format!(
                "module util; pub interface Get<T> {{ fn n(self) -> T; fn me(self) -> Self {{ return id(self); }} }} {id}"
            ),
        ),
        (
            "main.wi",
            "import util; class A implements util::Get<i64> { pub fn n(self) -> i64 { return 1; } } fn main() { println(new A().me().n()); }",
        ),
    ]);
    assert!(
        cross.contains("mismatched types: expected `Self`, found `Get<i64>`"),
        "{cross}"
    );
    runs(
        &[
            (
                "util.wi",
                &format!(
                    "module util; pub interface Get<T> {{ fn n(self) -> T; fn via(self) -> T {{ let g: Get<T> = id(self); return g.n(); }} }} {id}"
                ),
            ),
            (
                "main.wi",
                "import util; class A implements util::Get<i64> { pub fn n(self) -> i64 { return 7; } } fn main() { println(new A().via()); }",
            ),
        ],
        "7\n",
    );
}

#[test]
fn module_generic_function_at_its_own_generic_interface() {
    runs(
        &[
            (
                "util.wi",
                "module util; pub interface Get<T> { fn n(self) -> T; fn via(self) -> T { let b: Get<T> = self; let g: Get<T> = id(b); return g.n(); } } pub fn twice(b: Get<i64>) -> i64 { let g: Get<i64> = id(b); return g.n() * 2; } fn id<T>(x: T) -> T { return x; }",
            ),
            (
                "main.wi",
                "import util; class A implements util::Get<i64> { pub fn n(self) -> i64 { return 7; } } fn main() { println(new A().via()); println(util::twice(new A())); }",
            ),
        ],
        "7\n14\n",
    );
}

/// In one module unit both the module's own `Get` and the entry's
/// same-named `Get` are generic instance arguments (`id<util::Get<i64>>` and
/// `id<main's Get>`): only the entry's is respelled under the receiver prefix,
/// and neither the lowered resolution tables nor the backend interface
/// registry let the alias entry replace the declaration of that name (which
/// one won depended on hash order, so the build is repeated).
#[test]
fn module_and_entry_instances_of_same_named_interface() {
    for _ in 0..4 {
        runs(
            &[
                (
                    "util.wi",
                    "module util; pub interface Get<T> { fn n(self) -> T; fn via(self) -> T { let b: Get<T> = self; let g: Get<T> = id(b); return g.n(); } } pub fn twice(b: Get<i64>) -> i64 { let g: Get<i64> = id(b); return g.n() * 2; } pub fn id<T>(x: T) -> T { return x; }",
                ),
                (
                    "main.wi",
                    "import util; interface Get { fn m(self) -> i64; } class A implements util::Get<i64> { pub fn n(self) -> i64 { return 7; } } class B implements Get { pub fn m(self) -> i64 { return 5; } } fn main() { println(new A().via()); println(util::twice(new A())); let g: Get = new B(); let h: Get = util::id(g); println(h.m()); }",
                ),
            ],
            "7\n14\n5\n",
        );
    }
}

/// The same pairing for same-named enums and classes: the module's own
/// type and the entry's both instantiate `util::id` in one build.
#[test]
fn module_and_entry_instances_of_same_named_enum_and_class() {
    for _ in 0..2 {
        runs(
            &[
                (
                    "util.wi",
                    "module util; pub enum Kind { A, B } pub class Box { pub n: i64; pub fn get(self) -> i64 { return self.n; } } pub fn code(k: Kind) -> i64 { let j: Kind = id(k); match j { Kind::A => { return 1; } Kind::B => { return 2; } } } pub fn twice(b: Box) -> i64 { let c: Box = id(b); return c.get() * 2; } pub fn make() -> Box { return new Box(4); } pub fn id<T>(x: T) -> T { return x; }",
                ),
                (
                    "main.wi",
                    "import util; enum Kind { X, Y, Z } class Box { pub a: i64; pub b: i64; pub fn sum(self) -> i64 { return self.a + self.b; } } fn main() { println(util::code(util::Kind::B)); println(util::twice(util::make())); let k: Kind = util::id(Kind::Z); match k { Kind::X => { println(0); } Kind::Y => { println(1); } Kind::Z => { println(2); } } let b: Box = util::id(new Box(2, 3)); println(b.sum()); }",
                ),
            ],
            "2\n8\n2\n5\n",
        );
    }
}
