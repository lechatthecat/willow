//! End-to-end coverage for linker-symbol conflicts (willow-uqzx, catalog
//! item 8).
//!
//! The backend used to build linker symbols by flattening `::` to `__`. That
//! mapping is not injective: `foo::bar` and a declaration literally named
//! `foo__bar` produce the same string, and nothing in the language stops a name
//! from containing `__`. Two things used to happen, both bad.
//!
//! A user declaration that landed on a runtime name — `fn willow_array_new` —
//! compiled clean and produced a binary in which the user's function had
//! REPLACED the runtime's array allocator for the whole program. An array
//! literal then allocated nothing and the program aborted on a null array,
//! with no diagnostic anywhere. That is the case this file cares about most:
//! it was silent.
//!
//! A user declaration that collided with another user declaration reached
//! Cranelift, which rejected it as a duplicate definition. The user saw
//! `error[E0800]: internal compiler error` with no source location and no
//! indication of which two names were involved.
//!
//! Phase 1 made both of those diagnostics: `E0705` for the reserved namespace,
//! `E0706` for a collision between two source items. Phase 2 then made the
//! mangling injective — components are joined with `.` and compiler-generated
//! roles are introduced with `$`, neither of which a Willow identifier can
//! contain — so the second class of collision stopped existing. The programs
//! that used to be rejected with `E0706` now compile, and the perspectives that
//! covered those rejections became the perspectives that check both
//! declarations survive with their own bodies.
//!
//! What is left for `E0706` is the one name the compiler assigns itself,
//! `willow_user_main`. What is left for `E0705` is an entry-file free function,
//! the only declaration that still reaches the linker under a bare name.

use super::support::{
    assert_compile_error_contains, compile_and_run, compile_error_stderr,
    compile_temp_project_and_run,
};

fn assert_conflict(source: &str, parts: &[&str]) {
    assert_compile_error_contains(source, parts);
}

fn assert_runs(source: &str, expected: &str) {
    let (out, ok) = compile_and_run(source);
    assert!(ok, "program failed: {out}");
    assert_eq!(out.trim(), expected);
}

// ---------------------------------------------------------------------------
// E0705 — the reserved runtime and compiler namespaces
// ---------------------------------------------------------------------------

/// Perspective 12: the case that used to miscompile silently. A user function
/// named after a runtime ABI symbol replaced the runtime's version, and the
/// program aborted at run time on a null array.
#[test]
fn defining_a_runtime_abi_symbol_is_rejected() {
    assert_conflict(
        r#"
fn willow_array_new(len: i64, is_ref: i64) -> i64 {
    return 0;
}

fn main() {
    let a = [1, 2, 3];
    println(a.len());
}
"#,
        &[
            "error[E0705]",
            "willow_array_new",
            "reserved symbol",
            "belongs to the Willow runtime",
        ],
    );
}

/// Perspective 13: a different runtime symbol, to show the check consults the
/// ABI list rather than special-casing one name.
#[test]
fn defining_a_second_runtime_abi_symbol_is_rejected() {
    assert_conflict(
        r#"
fn willow_gc_collect() {
    println(0);
}

fn main() {
    println(1);
}
"#,
        &["error[E0705]", "willow_gc_collect"],
    );
}

/// Perspective 14: the whole `willow_` namespace is reserved, not only the
/// symbols that exist today. Otherwise a runtime symbol added in a later
/// release would start silently hijacking user code that already compiled.
#[test]
fn defining_an_unused_willow_name_is_rejected() {
    assert_conflict(
        r#"
fn willow_some_future_helper() -> i64 {
    return 1;
}

fn main() {
    println(willow_some_future_helper());
}
"#,
        &["error[E0705]", "willow_some_future_helper"],
    );
}

/// Perspective 15: the compiler's own generated data lives in `__willow_`.
/// Taking one of those names would corrupt static initialization.
#[test]
fn defining_a_compiler_internal_symbol_is_rejected() {
    assert_conflict(
        r#"
fn __willow_static_init() -> i64 {
    return 1;
}

fn main() {
    println(1);
}
"#,
        &["error[E0705]", "__willow_static_init"],
    );
}

/// Perspective 33: the reservation narrowed with the injective scheme. A class
/// named `willow_box` mangles to `willow_box.get`, which carries a separator
/// and therefore cannot be a runtime symbol — a runtime symbol is always a
/// bare C identifier. Phase 1 rejected this program for a collision that could
/// not happen.
#[test]
fn a_class_mangling_under_a_reserved_looking_prefix_is_legal() {
    assert_runs(
        r#"
class willow_box {
    pub v: i64;

    pub fn get(self) -> i64 {
        return self.v;
    }
}

fn main() {
    println(new willow_box(1).get());
}
"#,
        "1",
    );
}

/// Perspective 34: the narrowing is a narrowing. The bare form of the same
/// name — a free function, the only declaration that still reaches the linker
/// unjoined — is still rejected.
#[test]
fn a_free_function_in_the_runtime_namespace_is_still_rejected() {
    assert_conflict(
        r#"
fn willow_box() -> i64 {
    return 1;
}

fn main() {
    println(willow_box());
}
"#,
        &["error[E0705]", "willow_box"],
    );
}

// ---------------------------------------------------------------------------
// E0706 — the collisions that remain
// ---------------------------------------------------------------------------

/// Perspective 35: `fn main` is compiled as `willow_user_main`, so a function
/// literally named `willow_user_main` collides with it. This is the one
/// collision injective mangling cannot remove: the compiler, not the mangling
/// scheme, chooses that name.
#[test]
fn a_function_named_like_the_entry_point_collides_with_main() {
    assert_conflict(
        r#"
fn willow_user_main() -> i64 {
    return 1;
}

fn main() {
    println(willow_user_main());
}
"#,
        &[
            "error[E0706]",
            "function `willow_user_main`",
            "function `main`",
            "willow_user_main",
        ],
    );
}

/// Perspective 36: the collision diagnostic names the FIRST declaration and
/// the file it came from, which is what makes a cross-file collision
/// actionable.
#[test]
fn collision_diagnostic_points_at_the_first_declaration() {
    assert_conflict(
        r#"
fn willow_user_main() -> i64 {
    return 1;
}

fn main() {
    println(willow_user_main());
}
"#,
        &[
            "the first is function `willow_user_main` in",
            "second declaration",
        ],
    );
}

/// Perspective 37: no symbol diagnostic ever collapses into the old internal
/// error. A user-caused conflict must never be reported as a compiler bug.
#[test]
fn symbol_conflicts_are_never_internal_compiler_errors() {
    for source in [
        r#"
fn willow_array_new(len: i64, is_ref: i64) -> i64 { return 0; }
fn main() { println(1); }
"#,
        r#"
fn willow_user_main() -> i64 { return 1; }
fn main() { println(1); }
"#,
    ] {
        let stderr = compile_error_stderr(source);
        assert!(
            !stderr.contains("internal compiler error") && !stderr.contains("E0800"),
            "symbol conflict reported as an ICE:\n{stderr}"
        );
    }
}

// ---------------------------------------------------------------------------
// Phase 2: the collisions that no longer exist
//
// Every program below was rejected with E0706 before the mangling became
// injective. Each one now compiles, and each assertion checks that BOTH
// declarations kept their own body — compiling is not enough if one silently
// replaced the other, which is the failure mode the whole item exists for.
// ---------------------------------------------------------------------------

/// Perspective 38: the case the catalog names directly — a module function
/// `foo::bar` against a literal `foo__bar` in the importing file. `foo.bar`
/// and `foo__bar` are now different symbols, and each call reaches its own
/// function.
#[test]
fn module_function_and_lookalike_entry_function_coexist() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "src/foo.wi",
                r#"
pub fn bar(x: i64) -> i64 {
    return x + 1;
}
"#,
            ),
            (
                "src/main.wi",
                r#"
import foo;

fn foo__bar(x: i64) -> i64 {
    return x + 999;
}

fn main() {
    println(foo::bar(1));
    println(foo__bar(1));
}
"#,
            ),
        ],
        "src/main.wi",
    );
    assert!(ok, "project failed: {out}");
    assert_eq!(out.trim(), "2\n1000");
}

/// Perspective 39: a class method against a free function spelling its old
/// mangled form.
#[test]
fn class_method_and_lookalike_free_function_coexist() {
    assert_runs(
        r#"
class Foo {
    pub v: i64;

    pub fn bar(self) -> i64 {
        return self.v;
    }
}

fn Foo__bar(x: i64) -> i64 {
    return x + 999;
}

fn main() {
    println(new Foo(3).bar());
    println(Foo__bar(3));
}
"#,
        "3\n1002",
    );
}

/// Perspective 40: the same pair with deliberately different signatures.
/// Cranelift used to report this one as a signature incompatibility rather
/// than a duplicate, i.e. a second, differently-worded ICE for one bug.
#[test]
fn coexistence_does_not_depend_on_the_signatures_matching() {
    assert_runs(
        r#"
class Foo {
    pub v: i64;

    pub fn bar(self) -> i64 {
        return self.v;
    }
}

fn Foo__bar() -> i64 {
    return 999;
}

fn main() {
    println(new Foo(1).bar());
    println(Foo__bar());
}
"#,
        "1\n999",
    );
}

/// Perspective 41: a constructor lowers to the `init` method symbol, so it
/// used to collide with a free function the same way an ordinary method did.
#[test]
fn constructor_and_lookalike_free_function_coexist() {
    assert_runs(
        r#"
class A {
    pub x: i64;

    pub init(self, x: i64) {
        self.x = x;
    }
}

fn A__init(x: i64) -> i64 {
    return x + 999;
}

fn main() {
    println(new A(1).x);
    println(A__init(1));
}
"#,
        "1\n1000",
    );
}

/// Perspective 42: a static property's storage against a method on a class
/// named after the old `__static__` infix. The infix is now the `$static`
/// role, which no class name can spell.
#[test]
fn static_property_and_lookalike_method_coexist() {
    assert_runs(
        r#"
class A {
    pub static v: i64 = 1;
}

class A__static {
    pub y: i64;

    pub fn v(self) -> i64 {
        return 2;
    }
}

fn main() {
    println(A::v);
    println(new A__static(0).v());
}
"#,
        "1\n2",
    );
}

/// Perspective 43: the collision never needed a free function at all — two
/// classes could split one symbol between them at different points. They now
/// keep `A.b__c` and `A__b.c` apart.
#[test]
fn two_classes_splitting_one_old_symbol_coexist() {
    assert_runs(
        r#"
class A {
    pub x: i64;

    pub fn b__c(self) -> i64 {
        return 1;
    }
}

class A__b {
    pub y: i64;

    pub fn c(self) -> i64 {
        return 2;
    }
}

fn main() {
    println(new A(0).b__c());
    println(new A__b(0).c());
}
"#,
        "1\n2",
    );
}

/// Perspective 44: a module class method against an entry-file function,
/// which exercises the module-prefixed method mangling rather than the plain
/// module-function one.
#[test]
fn module_class_method_and_lookalike_entry_function_coexist() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "src/shapes.wi",
                r#"
pub class Square {
    pub side: i64;

    pub fn area(self) -> i64 {
        return self.side * self.side;
    }
}
"#,
            ),
            (
                "src/main.wi",
                r#"
import shapes;

fn shapes__Square__area(x: i64) -> i64 {
    return x + 999;
}

fn main() {
    println(new shapes::Square(3).area());
    println(shapes__Square__area(1));
}
"#,
            ),
        ],
        "src/main.wi",
    );
    assert!(ok, "project failed: {out}");
    assert_eq!(out.trim(), "9\n1000");
}

/// Perspective 45: the underscore-heavy shapes stack. A module, a class, a
/// method and a static property all spelled with `__`, in one program, each
/// reachable and each returning its own value.
#[test]
fn stacked_underscore_shapes_all_resolve_independently() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "src/a__b.wi",
                r#"
pub class C__d {
    pub v: i64;

    pub static limit: i64 = 7;

    pub fn e__f(self) -> i64 {
        return self.v;
    }
}

pub fn g__h(x: i64) -> i64 {
    return x * 10;
}
"#,
            ),
            (
                "src/main.wi",
                r#"
import a__b;

fn a__b__g__h(x: i64) -> i64 {
    return x + 100;
}

fn main() {
    println(new a__b::C__d(1).e__f());
    println(a__b::C__d::limit);
    println(a__b::g__h(2));
    println(a__b__g__h(2));
}
"#,
            ),
        ],
        "src/main.wi",
    );
    assert!(ok, "project failed: {out}");
    assert_eq!(out.trim(), "1\n7\n20\n102");
}

// ---------------------------------------------------------------------------
// Vtable symbols
//
// Vtables are data rather than functions, but they share the one linker
// namespace, so they are claimed in the ownership registry too (phase 2). A
// claim that fired wrongly would reject ordinary interface code, so these pin
// down the shapes that must keep compiling.
// ---------------------------------------------------------------------------

/// Perspective 46: two classes implementing one interface get one vtable each,
/// and dispatch reaches the right body.
#[test]
fn two_classes_implementing_one_interface_each_get_a_vtable() {
    assert_runs(
        r#"
interface Animal {
    fn speak(self) -> String;
}

class Dog implements Animal {
    pub name: String;

    pub fn speak(self) -> String {
        return "woof";
    }
}

class Bird implements Animal {
    pub name: String;

    pub fn speak(self) -> String {
        return "tweet";
    }
}

fn say(a: Animal) -> String {
    return a.speak();
}

fn main() {
    println(say(new Dog("d")));
    println(say(new Bird("b")));
}
"#,
        "woof\ntweet",
    );
}

/// Perspective 47: one class implementing two interfaces claims two distinct
/// vtable symbols. A registry keyed on the class alone would reject this.
#[test]
fn one_class_implementing_two_interfaces_claims_two_vtables() {
    assert_runs(
        r#"
interface Named {
    fn name(self) -> String;
}

interface Sized {
    fn size(self) -> i64;
}

class Box implements Named, Sized {
    pub label: String;

    pub fn name(self) -> String {
        return self.label;
    }

    pub fn size(self) -> i64 {
        return 4;
    }
}

fn call_name(n: Named) -> String {
    return n.name();
}

fn call_size(s: Sized) -> i64 {
    return s.size();
}

fn main() {
    let b = new Box("crate");
    println(call_name(b));
    println(call_size(b));
}
"#,
        "crate\n4",
    );
}

/// Perspective 48: a class implementing two instantiations of one generic
/// interface still declares a single vtable. Every slot points at a
/// monomorphic class method, so the instantiations share one byte-identical
/// table — and the registry must see one claim, not two.
#[test]
fn generic_interface_instantiations_share_one_vtable_claim() {
    assert_runs(
        r#"
interface Container<T> {
    fn describe(self) -> String;
}

class Pair implements Container<i64>, Container<String> {
    pub tag: String;

    pub fn describe(self) -> String {
        return self.tag;
    }
}

fn describe_it(c: Container<i64>) -> String {
    return c.describe();
}

fn main() {
    println(describe_it(new Pair("pair")));
}
"#,
        "pair",
    );
}

// ---------------------------------------------------------------------------
// Positive controls: what must stay legal
// ---------------------------------------------------------------------------

/// Perspective 26: `fn main` gets the `willow_user_main` symbol from the
/// compiler. Reserving that name would reject every program, so it is
/// deliberately outside the reserved set.
#[test]
fn main_still_compiles_despite_owning_a_willow_symbol() {
    assert_runs(
        r#"
fn main() {
    println(42);
}
"#,
        "42",
    );
}

/// Perspective 27: a class named `willow` is user space. Its methods mangle to
/// `willow.greet`, which carries a separator and so cannot be a runtime name.
#[test]
fn a_class_named_willow_is_still_legal() {
    assert_runs(
        r#"
class willow {
    pub x: i64;

    pub fn get(self) -> i64 {
        return self.x;
    }
}

fn main() {
    println(new willow(7).get());
}
"#,
        "7",
    );
}

/// Perspective 28: names that merely contain or resemble the reserved prefix
/// are user space. The check is anchored and case-sensitive.
#[test]
fn names_resembling_the_reserved_prefix_are_still_legal() {
    assert_runs(
        r#"
fn my_willow_helper() -> i64 {
    return 1;
}

fn Willow_helper() -> i64 {
    return 2;
}

fn willowish() -> i64 {
    return 3;
}

fn main() {
    println(my_willow_helper() + Willow_helper() + willowish());
}
"#,
        "6",
    );
}

/// Perspective 29: underscore-heavy names, which most Willow code uses, must
/// keep working.
#[test]
fn underscore_heavy_names_still_compile() {
    assert_runs(
        r#"
class user_account {
    pub account_id: i64;
    pub static default_limit: i64 = 100;

    pub fn credit_limit(self) -> i64 {
        return self.account_id * 2;
    }
}

fn compute_total_balance(a: i64, b: i64) -> i64 {
    return a + b;
}

fn main() {
    let acct = new user_account(21);
    println(acct.credit_limit());
    println(user_account::default_limit);
    println(compute_total_balance(1, 2));
}
"#,
        "42\n100\n3",
    );
}

/// Perspective 30: the same method name on different classes does not collide.
/// A registry that keyed on the method name alone would break every program.
#[test]
fn same_method_name_on_different_classes_is_legal() {
    assert_runs(
        r#"
class A {
    pub v: i64;

    pub fn get(self) -> i64 {
        return self.v;
    }
}

class B {
    pub v: i64;

    pub fn get(self) -> i64 {
        return self.v * 2;
    }
}

fn main() {
    println(new A(3).get());
    println(new B(3).get());
}
"#,
        "3\n6",
    );
}

/// Perspective 31: a module whose name contains underscores still compiles and
/// is callable.
#[test]
fn underscore_named_module_still_links() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "src/string_helpers.wi",
                r#"
pub fn shout_twice(x: i64) -> i64 {
    return x * 2;
}
"#,
            ),
            (
                "src/main.wi",
                r#"
import string_helpers;

fn main() {
    println(string_helpers::shout_twice(21));
}
"#,
            ),
        ],
        "src/main.wi",
    );
    assert!(ok, "project failed: {out}");
    assert_eq!(out.trim(), "42");
}

/// Perspective 32: a static property and an instance method on one class
/// coexist. Their symbols differ only by the `$static` role, so this pins down
/// that the role is enough to keep them apart.
#[test]
fn static_property_and_method_on_one_class_coexist() {
    assert_runs(
        r#"
class Config {
    pub static size: i64 = 10;
    pub scale: i64;

    pub fn size_times(self) -> i64 {
        return Config::size * self.scale;
    }
}

fn main() {
    println(Config::size);
    println(new Config(4).size_times());
}
"#,
        "10\n40",
    );
}

/// Perspective 49: a lambda is lifted to a top-level function whose symbol
/// carries the `$lambda` role, so it cannot be spelled in source and cannot
/// take a user declaration's symbol however that declaration is named.
#[test]
fn lifted_lambdas_do_not_collide_with_user_functions() {
    assert_runs(
        r#"
fn apply(f: fn(i64) -> i64, x: i64) -> i64 {
    return f(x);
}

fn __lambda_0(x: i64) -> i64 {
    return x + 100;
}

fn main() {
    println(apply(|x: i64| -> i64 { return x * 2; }, 5));
    println(__lambda_0(5));
}
"#,
        "10\n105",
    );
}

/// Perspective 50: an unused declaration still claims its symbol. Symbol
/// ownership is decided at declare time, before any body is compiled, so a
/// program is rejected for a reserved name even when nothing calls it.
#[test]
fn an_uncalled_declaration_still_claims_its_symbol() {
    let stderr = compile_error_stderr(
        r#"
fn willow_string_concat(a: i64, b: i64) -> i64 {
    return a;
}

fn main() {
    println(1);
}
"#,
    );
    assert!(
        stderr.contains("error[E0705]") && stderr.contains("willow_string_concat"),
        "an uncalled declaration must still be checked:\n{stderr}"
    );
}

/// Perspective 51: a cross-file program where the module and the entry file
/// each declare underscore-heavy lookalikes of the other's symbols. The
/// registry is per-compilation, not per-file, so this is where a scheme that
/// was injective only within one file would fail.
#[test]
fn cross_file_lookalikes_all_keep_their_own_symbols() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "src/util.wi",
                r#"
pub class Pair {
    pub a: i64;

    pub fn sum(self) -> i64 {
        return self.a;
    }
}

pub fn util__Pair__sum(x: i64) -> i64 {
    return x + 1;
}
"#,
            ),
            (
                "src/main.wi",
                r#"
import util;

fn util__Pair__sum(x: i64) -> i64 {
    return x + 2;
}

fn main() {
    println(new util::Pair(10).sum());
    println(util::util__Pair__sum(10));
    println(util__Pair__sum(10));
}
"#,
            ),
        ],
        "src/main.wi",
    );
    assert!(ok, "project failed: {out}");
    assert_eq!(out.trim(), "10\n11\n12");
}

/// Perspective 52: the module-import aliasing path splits a method symbol back
/// into its components to re-alias it under a local name. A directly imported
/// class whose own name contains `__` exercises that split.
#[test]
fn direct_type_import_realiases_underscore_named_methods() {
    let (out, ok) = compile_temp_project_and_run(
        &[
            (
                "src/models.wi",
                r#"
pub class Cell__v2 {
    pub v: i64;

    pub fn read__raw(self) -> i64 {
        return self.v;
    }
}
"#,
            ),
            (
                "src/main.wi",
                r#"
import models::Cell__v2;

fn main() {
    println(new Cell__v2(5).read__raw());
}
"#,
            ),
        ],
        "src/main.wi",
    );
    assert!(ok, "project failed: {out}");
    assert_eq!(out.trim(), "5");
}
