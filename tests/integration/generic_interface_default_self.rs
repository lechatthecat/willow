//! `self` in a GENERIC interface default (willow-rktm).
//!
//! A class implementing another module's `Get<A>` receives its defaults
//! through one non-generic instance checked in the provider (willow-s71l).
//! `self` there has the written type `Get<A>`, so it converts wherever the
//! receiver's declared interface would and no diagnostic names the instance.
//! A generic interface value also converts to the non-generic interfaces its
//! declaration extends, in the checker and in the backend.
//!
//! Perspectives:
//!  1. ticket repro: `h(self)` with `h(g: Get<i64>)` across modules
//!  2. the same program in one module
//!  3. runnable example
//!  4. provider generic function `pick<T>(g: Get<T>)`
//!  5. `String` instantiation
//!  6. multi-parameter `Pair<i64, String>`
//!  7. type argument declared in the receiver's module
//!  8. generic class receiver at two type arguments
//!  9. receiver in a non-entry module
//! 10. two receivers share the instance; dynamic call through `util::Get<i64>`
//! 11. non-generic super-interface (`k(b: Base)`) across modules
//! 12. two-level super chain across modules
//! 13. super-interface in a same-module default
//! 14. value upcast `Get<i64>` to its super in argument, return, `let`,
//!     assignment and array positions, including a non-empty vtable path
//! 15. value upcast of an imported `util::Get<i64>` to `util::Base`
//! 16. `self` in a typed local `let g: Get<T> = self`
//! 17. `self` stored in `Array<Get<i64>>`
//! 18. `self` stored in a provider class field
//! 19. `self` passed to a lambda typed `|g: Get<i64>|`
//! 20. default→default calls next to `h(self)`
//! 21. wrong instantiation: `expected Get<String>, found Get<i64>`
//! 22. `let s: String = self` reports `Get<i64>`
//! 23. unrelated interface and class parameters stay rejected
//! 24. missing method names `Get`, not the instance
//! 25. a generic interface value does not convert to an unrelated interface

use super::support::*;

fn runs(files: &[(&str, &str)], expected: &str) {
    let project = TestProject::new("generic_iface_default_self", files);
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
    let project = TestProject::new("generic_iface_default_self", files);
    let compiled = project.compile("main.wi");
    let stderr = String::from_utf8_lossy(&compiled.stderr).into_owned();
    assert!(!compiled.status.success(), "{stderr}");
    assert!(!stderr.contains("internal compiler error"), "{stderr}");
    assert!(!stderr.contains("$mono$"), "{stderr}");
    stderr
}

fn util(body: &str) -> String {
    format!("module util; {body}")
}

const H: &str = "pub interface Get<T> { fn n(self) -> T; fn get(self) -> i64 { return h(self); } } fn h(g: Get<i64>) -> i64 { return g.n() * 100; }";

const Z: &str =
    "class Z implements util::Get<i64> { pub k: i64; pub fn n(self) -> i64 { return self.k; } }";

fn main_with(body: &str) -> String {
    format!("import util; {Z} fn main() {{ {body} }}")
}

#[test]
fn ticket_repro_passes_self_to_instantiated_interface() {
    runs(
        &[
            ("util.wi", &util(H)),
            ("main.wi", &main_with("println(new Z(1).get());")),
        ],
        "100\n",
    );
}

#[test]
fn same_module_repro() {
    runs(
        &[(
            "main.wi",
            "interface Get<T> { fn n(self) -> T; fn get(self) -> i64 { return h(self); } } fn h(g: Get<i64>) -> i64 { return g.n() * 100; } class Z implements Get<i64> { pub k: i64; pub fn n(self) -> i64 { return self.k; } } fn main() { println(new Z(1).get()); }",
        )],
        "100\n",
    );
}

#[test]
fn example_program() {
    runs(
        &[
            (
                "main.wi",
                include_str!("../../example/generic_interface_default_self.wi"),
            ),
            (
                "generic_interface_default_self_support/meters.wi",
                include_str!("../../example/generic_interface_default_self_support/meters.wi"),
            ),
        ],
        "300\n3\n[tank]\nfull\n[sign]\n",
    );
}

#[test]
fn provider_generic_function_of_self() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Get<T> { fn n(self) -> T; fn get(self) -> T { return pick(self); } } fn pick<T>(g: Get<T>) -> T { return g.n(); }",
                ),
            ),
            ("main.wi", &main_with("println(new Z(7).get());")),
        ],
        "7\n",
    );
}

#[test]
fn string_instantiation() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Get<T> { fn n(self) -> T; fn get(self) -> T { return h(self); } } fn h(g: Get<String>) -> String { return g.n() + \"!\"; }",
                ),
            ),
            (
                "main.wi",
                "import util; class Z implements util::Get<String> { pub fn n(self) -> String { return \"s\"; } } fn main() { println(new Z().get()); }",
            ),
        ],
        "s!\n",
    );
}

#[test]
fn multi_parameter_interface() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Pair<A, B> { fn a(self) -> A; fn b(self) -> B; fn get(self) -> String { return h(self); } } fn h(p: Pair<i64, String>) -> String { return p.b(); }",
                ),
            ),
            (
                "main.wi",
                "import util; class Z implements util::Pair<i64, String> { pub fn a(self) -> i64 { return 1; } pub fn b(self) -> String { return \"bee\"; } } fn main() { println(new Z().get()); }",
            ),
        ],
        "bee\n",
    );
}

#[test]
fn receiver_module_type_argument() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Get<T> { fn n(self) -> T; fn get(self) -> T { return h(self); } } fn h<T>(g: Get<T>) -> T { return g.n(); }",
                ),
            ),
            (
                "main.wi",
                "import util; class M { pub v: i64; } class Z implements util::Get<M> { pub fn n(self) -> M { return new M(3); } } fn main() { println(new Z().get().v); }",
            ),
        ],
        "3\n",
    );
}

#[test]
fn generic_class_receiver() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Get<T> { fn n(self) -> T; fn get(self) -> T { return pick(self); } } fn pick<T>(g: Get<T>) -> T { return g.n(); }",
                ),
            ),
            (
                "main.wi",
                "import util; class Gen<T> implements util::Get<T> { pub v: T; pub fn n(self) -> T { return self.v; } } fn main() { println(new Gen<i64>(4).get()); println(new Gen<String>(\"x\").get()); }",
            ),
        ],
        "4\nx\n",
    );
}

#[test]
fn receiver_in_non_entry_module() {
    runs(
        &[
            ("util.wi", &util(H)),
            (
                "mid.wi",
                "module mid; import util; pub class Y implements util::Get<i64> { pub fn n(self) -> i64 { return 4; } } fn h(g: util::Get<i64>) -> i64 { return 0; }",
            ),
            (
                "main.wi",
                "import mid; fn main() { println(new mid::Y().get()); }",
            ),
        ],
        "400\n",
    );
}

#[test]
fn receivers_share_instance_and_dynamic_call() {
    runs(
        &[
            ("util.wi", &util(H)),
            (
                "main.wi",
                &format!(
                    "import util; {Z} class Y implements util::Get<i64> {{ pub fn n(self) -> i64 {{ return 2; }} }} fn main() {{ println(new Z(1).get()); println(new Y().get()); let g: util::Get<i64> = new Y(); println(g.get()); }}"
                ),
            ),
        ],
        "100\n200\n200\n",
    );
}

const BASE_RECEIVER: &str = "class Z implements util::Get<i64> { pub k: i64; pub fn n(self) -> i64 { return self.k; } pub fn b(self) -> i64 { return self.k * 10; } }";

#[test]
fn non_generic_super_interface_across_modules() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Base { fn b(self) -> i64; } pub interface Get<T> extends Base { fn n(self) -> T; fn get(self) -> i64 { return k(self); } } fn k(b: Base) -> i64 { return b.b() + 1; }",
                ),
            ),
            (
                "main.wi",
                &format!("import util; {BASE_RECEIVER} fn main() {{ println(new Z(4).get()); }}"),
            ),
        ],
        "41\n",
    );
}

#[test]
fn two_level_super_chain_across_modules() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface A0 { fn a(self) -> i64; } pub interface A1 extends A0 { fn c(self) -> i64; } pub interface Get<T> extends A1 { fn n(self) -> T; fn get(self) -> i64 { return ka(self) + kc(self); } } fn ka(x: A0) -> i64 { return x.a(); } fn kc(x: A1) -> i64 { return x.c() + x.a(); }",
                ),
            ),
            (
                "main.wi",
                "import util; class Z implements util::Get<i64> { pub fn n(self) -> i64 { return 0; } pub fn a(self) -> i64 { return 1; } pub fn c(self) -> i64 { return 10; } } fn main() { println(new Z().get()); }",
            ),
        ],
        "12\n",
    );
}

#[test]
fn same_module_super_interface_default() {
    runs(
        &[(
            "main.wi",
            "interface Base { fn b(self) -> i64; } interface Get<T> extends Base { fn n(self) -> T; fn get(self) -> i64 { return k(self); } } fn k(b: Base) -> i64 { return b.b() + 1; } class Z implements Get<i64> { pub fn n(self) -> i64 { return 0; } pub fn b(self) -> i64 { return 8; } } fn main() { println(new Z().get()); }",
        )],
        "9\n",
    );
}

#[test]
fn generic_interface_value_upcasts_to_super() {
    runs(
        &[(
            "main.wi",
            "import std::collections::Array; interface A0 { fn a(self) -> i64; } interface A1 extends A0 { fn c(self) -> i64; } interface Get<T> extends A1 { fn n(self) -> T; } fn up(g: Get<i64>) -> A0 { return g; } fn arg(x: A1) -> i64 { return x.c(); } class Z implements Get<i64> { pub fn n(self) -> i64 { return 1; } pub fn a(self) -> i64 { return 5; } pub fn c(self) -> i64 { return 6; } } fn main() { let g: Get<i64> = new Z(); println(up(g).a()); println(arg(g)); let x: A1 = g; println(x.c() + x.a()); let mut y: A0 = new Z(); y = g; println(y.a()); let xs: Array<A0> = [g]; println(xs[0].a()); }",
        )],
        "5\n6\n11\n5\n5\n",
    );
}

#[test]
fn imported_generic_interface_value_upcasts_to_super() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Base { fn b(self) -> i64; } pub interface Get<T> extends Base { fn n(self) -> T; }",
                ),
            ),
            (
                "main.wi",
                &format!(
                    "import util; {BASE_RECEIVER} fn main() {{ let g: util::Get<i64> = new Z(2); let b: util::Base = g; println(b.b()); }}"
                ),
            ),
        ],
        "20\n",
    );
}

#[test]
fn self_in_typed_local() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Get<T> { fn n(self) -> T; fn get(self) -> T { let g: Get<T> = self; return g.n(); } }",
                ),
            ),
            ("main.wi", &main_with("println(new Z(8).get());")),
        ],
        "8\n",
    );
}

#[test]
fn self_in_array_of_interface() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "import std::collections::Array; pub interface Get<T> { fn n(self) -> T; fn get(self) -> i64 { let a: Array<Get<i64>> = [self, self]; return a[0].n() + a[1].n(); } }",
                ),
            ),
            ("main.wi", &main_with("println(new Z(5).get());")),
        ],
        "10\n",
    );
}

#[test]
fn self_in_provider_class_field() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Get<T> { fn n(self) -> T; fn get(self) -> i64 { let h = new Holder(self); return h.g.n(); } } class Holder { pub g: Get<i64>; }",
                ),
            ),
            ("main.wi", &main_with("println(new Z(11).get());")),
        ],
        "11\n",
    );
}

#[test]
fn self_passed_to_lambda() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Get<T> { fn n(self) -> T; fn get(self) -> i64 { let f = |g: Get<i64>| -> i64 { return g.n() * 2; }; return f(self); } }",
                ),
            ),
            ("main.wi", &main_with("println(new Z(6).get());")),
        ],
        "12\n",
    );
}

#[test]
fn default_to_default_next_to_self_argument() {
    runs(
        &[
            (
                "util.wi",
                &util(
                    "pub interface Get<T> { fn n(self) -> T; fn get(self) -> i64 { return self.twice() + h(self); } fn twice(self) -> i64 { return 2; } } fn h(g: Get<i64>) -> i64 { return g.n(); }",
                ),
            ),
            ("main.wi", &main_with("println(new Z(5).get());")),
        ],
        "7\n",
    );
}

#[test]
fn wrong_instantiation_names_written_types() {
    let stderr = fails(&[
        (
            "util.wi",
            &util(
                "pub interface Get<T> { fn n(self) -> T; fn get(self) -> i64 { return h(self); } } fn h(g: Get<String>) -> i64 { return 1; }",
            ),
        ),
        ("main.wi", &main_with("println(new Z(7).get());")),
    ]);
    assert!(
        stderr.contains("expected `Get<String>`, found `Get<i64>`"),
        "{stderr}"
    );
    assert!(stderr.contains("util.wi"), "{stderr}");
}

#[test]
fn mismatched_local_reports_written_instantiation() {
    let stderr = fails(&[
        (
            "util.wi",
            &util(
                "pub interface Get<T> { fn n(self) -> T; fn get(self) -> i64 { let s: String = self; return 1; } }",
            ),
        ),
        ("main.wi", &main_with("println(new Z(7).get());")),
    ]);
    assert!(
        stderr.contains("expected `String`, found `Get<i64>`"),
        "{stderr}"
    );
}

#[test]
fn unrelated_parameters_stay_rejected() {
    for helper in [
        "pub interface Other { fn o(self) -> i64; } fn k(x: Other) -> i64 { return 1; }",
        "pub class Q { } fn k(x: Q) -> i64 { return 1; }",
    ] {
        let stderr = fails(&[
            (
                "util.wi",
                &util(&format!(
                    "pub interface Get<T> {{ fn n(self) -> T; fn get(self) -> i64 {{ return k(self); }} }} {helper}"
                )),
            ),
            ("main.wi", &main_with("println(new Z(7).get());")),
        ]);
        assert!(stderr.contains("found `Get<i64>`"), "{stderr}");
    }
}

#[test]
fn missing_method_names_interface() {
    let stderr = fails(&[
        (
            "util.wi",
            &util(
                "pub interface Get<T> { fn n(self) -> T; fn get(self) -> i64 { return self.zz(); } }",
            ),
        ),
        ("main.wi", &main_with("println(new Z(7).get());")),
    ]);
    assert!(
        stderr.contains("no method `zz` on interface `Get`"),
        "{stderr}"
    );
}

#[test]
fn generic_interface_value_rejects_unrelated_interface() {
    let stderr = fails(&[(
        "main.wi",
        "interface Other { fn o(self) -> i64; } interface Get<T> { fn n(self) -> T; } fn k(x: Other) -> i64 { return 1; } fn f(g: Get<i64>) -> i64 { return k(g); } fn main() { }",
    )]);
    assert!(
        stderr.contains("expected `Other`, found `Get<i64>`"),
        "{stderr}"
    );
}
