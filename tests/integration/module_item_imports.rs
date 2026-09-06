//! A MODULE's own single-item imports (willow-kxy8, willow-2g6s).
//!
//! `import leaf::value;` binds one public item of another module under a local
//! name. The entry file's item imports were wired into the back end from the
//! start; a module's were not, and the two ways that broke are what this file
//! pins.
//!
//! willow-kxy8 — the FUNCTION half. A module's item imports never reached the
//! back end at all, so the unqualified call had no symbol to resolve and lowered
//! to a stub returning the zero of its return type:
//!
//! ```willow
//! // leaf.wi
//! pub fn value() -> i64 { return 40; }
//! // lib.wi
//! import leaf::value;
//! pub fn run() -> i64 { return value() + 4; }   // printed 4, not 44
//! ```
//!
//! Per-unit import scoping fixed it: `classify_unit_imports` classifies every
//! unit's own import lines, and `compile_module_bodies` installs that unit's
//! bindings before its bodies and takes them back out afterwards
//! (willow-vtlr / willow-28h8 / willow-kd1v).
//!
//! willow-2g6s — the CLASS half, still broken after that. `alias_item_import_types`
//! bound only the type scope, while the entry file's `register_item_import`
//! also aliases every `{class}.{method}` symbol. A method call on an
//! item-imported class inside a module therefore mangled to a symbol nothing
//! declared, and the LIR walker refused the body outright:
//!
//! ```text
//! error[E0800]: internal compiler error in module `lib`: function `lib.run`
//! has invalid lowered IR: the method `get` on a `Box` at line 5 is outside
//! the walker's subset
//! ```
//!
//! `item_import_method_aliases` now answers for both paths, and the module
//! side installs the aliases under the same snapshot its types use, so one
//! module's spelling of `Box` never survives into the next unit's bodies.
//!
//! 31 perspectives:
//!   1 the reported repro: an item-imported function is called unqualified
//!   2 arguments and results travel through the imported function
//!   3 two items imported from one module
//!   4 items imported from two different modules
//!   5 an aliased item import (`as v`)
//!   6 two modules bind one local name to two different modules' functions
//!   7 entry and module import the same item
//!   8 a three-module chain, each link importing the next
//!   9 the import is visible inside a module's class method
//!  10 ...inside a static method
//!  11 ...inside a lambda
//!  12 ...inside an async function the entry awaits
//!  13 a module's own function beats an aliased item import
//!  14 recursion through an item-imported helper
//!  15 String, f64 and bool returns
//!  16 an `Array<i64>` argument
//!  17 a method call on an item-imported class (willow-2g6s)
//!  18 a static method call on an item-imported class (willow-2g6s)
//!  19 control: a field read on an item-imported class always worked
//!  20 own and inherited methods on an item-imported class (willow-2g6s)
//!  21 a module subclasses an item-imported open class and dispatches
//!  22 two modules import two different classes of the same name
//!  23 an item-imported enum is matched in a module
//!  24 an item-imported interface is implemented and boxed in a module
//!  25 an item-imported class crosses the module's own public signature
//!  26 a class's methods survive the module that imported it (no leak either way)
//!  27 control: a sibling module does not see another module's item import
//!  28 control: the entry does not see a module's item import
//!  29 control: a sibling module cannot name another module's imported class
//!  30 `--release` keeps both halves
//!  31 the runnable example

use super::support::{
    TestProject, compile_temp_project_and_run, compile_temp_project_error_stderr,
};

/// The entry file used by most perspectives: it only calls into `lib`, so what
/// runs is the module's own body.
const CALL_INTO_LIB: &str = "import lib;

fn main() {
    println(lib::run());
}
";

/// A public class with a field, an instance method and a static method — the
/// three ways a name bound by an item import is used.
const BOX_CLASS: &str = "pub class Box {
    pub n: i64;

    pub init(self, n: i64) { self.n = n; }

    pub fn get(self) -> i64 { return self.n; }

    pub static fn make(n: i64) -> Box { return new Box(n); }
}
";

/// Compile and run a project expected to succeed, asserting its stdout.
fn assert_project(files: &[(&str, &str)], expected: &str) {
    let (output, ok) = compile_temp_project_and_run(files, "app.wi");
    assert!(ok, "expected the project to compile:\n{output}");
    assert_eq!(output, expected);
}

/// Compile a project expected to fail and return the compiler's stderr.
fn project_error(files: &[(&str, &str)]) -> String {
    compile_temp_project_error_stderr(files, "app.wi")
}

// 1. The bug report verbatim. `value()` used to lower to a stub returning the
//    zero of `i64`, so the program printed 4.
#[test]
fn item_import_01_a_module_calls_the_function_it_imported() {
    assert_project(
        &[
            ("leaf.wi", "pub fn value() -> i64 { return 40; }\n"),
            (
                "lib.wi",
                "import leaf::value;

pub fn run() -> i64 {
    return value() + 4;
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "44\n",
    );
}

// 2. A zero-returning stub also swallows arguments. Calling with operands the
//    result depends on shows the real body ran.
#[test]
fn item_import_02_arguments_reach_the_imported_function() {
    assert_project(
        &[
            (
                "leaf.wi",
                "pub fn add(a: i64, b: i64) -> i64 { return a + b; }\n",
            ),
            (
                "lib.wi",
                "import leaf::add;

pub fn run() -> i64 {
    return add(3, 4) * add(1, 1);
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "14\n",
    );
}

// 3. Each import line binds its own name; two from one module must not collapse
//    into one binding.
#[test]
fn item_import_03_two_items_from_one_module() {
    assert_project(
        &[
            (
                "leaf.wi",
                "pub fn a() -> i64 { return 1; }
pub fn b() -> i64 { return 2; }
",
            ),
            (
                "lib.wi",
                "import leaf::a;
import leaf::b;

pub fn run() -> i64 {
    return a() * 10 + b();
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "12\n",
    );
}

// 4. Two items from two modules: each local name has to carry its OWN module's
//    symbol prefix, not the last one bound.
#[test]
fn item_import_04_items_from_two_modules() {
    assert_project(
        &[
            ("one.wi", "pub fn a() -> i64 { return 1; }\n"),
            ("two.wi", "pub fn b() -> i64 { return 2; }\n"),
            (
                "lib.wi",
                "import one::a;
import two::b;

pub fn run() -> i64 {
    return a() * 10 + b();
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "12\n",
    );
}

// 5. The alias is the local name; the item keeps its own name in the module it
//    came from, so the binding is a rename, not a second symbol.
#[test]
fn item_import_05_an_aliased_item_import() {
    assert_project(
        &[
            ("leaf.wi", "pub fn value() -> i64 { return 40; }\n"),
            (
                "lib.wi",
                "import leaf::value as v;

pub fn run() -> i64 {
    return v() + 4;
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "44\n",
    );
}

// 6. The heart of per-unit scoping (willow-28h8). One local name, two modules,
//    two bodies: whichever unit is lowered second must not own the binding for
//    the first.
#[test]
fn item_import_06_one_local_name_in_two_modules() {
    assert_project(
        &[
            ("one.wi", "pub fn tag() -> i64 { return 1; }\n"),
            ("two.wi", "pub fn tag() -> i64 { return 2; }\n"),
            (
                "a.wi",
                "import one::tag;

pub fn run() -> i64 { return tag(); }
",
            ),
            (
                "b.wi",
                "import two::tag;

pub fn run() -> i64 { return tag(); }
",
            ),
            (
                "app.wi",
                "import a;
import b;

fn main() {
    println(a::run());
    println(b::run());
}
",
            ),
        ],
        "1\n2\n",
    );
}

// 7. The entry file is the last unit declared and binds its item imports
//    globally; a module that imported the same item still has to keep working.
#[test]
fn item_import_07_entry_and_module_import_the_same_item() {
    assert_project(
        &[
            ("leaf.wi", "pub fn value() -> i64 { return 40; }\n"),
            (
                "lib.wi",
                "import leaf::value;

pub fn run() -> i64 { return value() + 4; }
",
            ),
            (
                "app.wi",
                "import lib;
import leaf::value;

fn main() {
    println(lib::run());
    println(value());
}
",
            ),
        ],
        "44\n40\n",
    );
}

// 8. A chain: the entry imports `lib`, `lib` imports an item of `b`, `b` imports
//    an item of `c`. Every link is a module unit, so every link needs its own
//    bindings installed when its body is lowered.
#[test]
fn item_import_08_a_chain_of_item_imports() {
    assert_project(
        &[
            ("c.wi", "pub fn base() -> i64 { return 7; }\n"),
            (
                "b.wi",
                "import c::base;

pub fn mid() -> i64 { return base() * 2; }
",
            ),
            (
                "lib.wi",
                "import b::mid;

pub fn run() -> i64 { return mid() + 1; }
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "15\n",
    );
}

// 9. A class method body is lowered in the same alias scope as the module's
//    free functions, so the import is in scope there too.
#[test]
fn item_import_09_the_import_is_visible_in_a_class_method() {
    assert_project(
        &[
            ("leaf.wi", "pub fn value() -> i64 { return 40; }\n"),
            (
                "lib.wi",
                "import leaf::value;

class Holder {
    pub k: i64;

    pub init(self, k: i64) { self.k = k; }

    pub fn total(self) -> i64 { return value() + self.k; }
}

pub fn run() -> i64 {
    let h = new Holder(2);
    return h.total();
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "42\n",
    );
}

// 10. A static method has no receiver, so it reaches the import through the
//     unit's scope alone.
#[test]
fn item_import_10_the_import_is_visible_in_a_static_method() {
    assert_project(
        &[
            ("leaf.wi", "pub fn value() -> i64 { return 40; }\n"),
            (
                "lib.wi",
                "import leaf::value;

class Holder {
    pub static fn total() -> i64 { return value() + 2; }
}

pub fn run() -> i64 { return Holder::total(); }
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "42\n",
    );
}

// 11. Lambdas are compiled first inside the unit's alias scope (willow-9yhi),
//     which is exactly why a lambda body sees the import.
#[test]
fn item_import_11_the_import_is_visible_in_a_lambda() {
    assert_project(
        &[
            ("leaf.wi", "pub fn value() -> i64 { return 40; }\n"),
            (
                "lib.wi",
                "import leaf::value;

pub fn run() -> i64 {
    let f = || -> i64 { return value() + 4; };
    return f();
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "44\n",
    );
}

// 12. An async body is lowered as a poll function of its own; the import has to
//     be in scope for that lowering as well.
#[test]
fn item_import_12_the_import_is_visible_in_an_async_function() {
    assert_project(
        &[
            ("leaf.wi", "pub fn value() -> i64 { return 40; }\n"),
            (
                "lib.wi",
                "import leaf::value;

pub async fn calc() -> i64 { return value() + 2; }
",
            ),
            (
                "app.wi",
                "import lib;

async fn main() {
    println(await lib::calc());
}
",
            ),
        ],
        "42\n",
    );
}

// 13. The module's own declaration wins over an import: the aliased spelling
//     reaches the import, the bare one the module's own function.
#[test]
fn item_import_13_a_modules_own_function_beats_the_import() {
    assert_project(
        &[
            ("leaf.wi", "pub fn value() -> i64 { return 40; }\n"),
            (
                "lib.wi",
                "import leaf::value as outer;

fn value() -> i64 { return 1; }

pub fn run() -> i64 { return value() * 100 + outer(); }
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "140\n",
    );
}

// 14. A recursive module function calling the imported helper on every step: a
//     stub would flatten the recursion to one level.
#[test]
fn item_import_14_recursion_through_an_imported_helper() {
    assert_project(
        &[
            ("leaf.wi", "pub fn dec(n: i64) -> i64 { return n - 1; }\n"),
            (
                "lib.wi",
                "import leaf::dec;

fn down(n: i64) -> i64 {
    if n <= 0 { return 0; }
    return n + down(dec(n));
}

pub fn run() -> i64 { return down(4); }
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "10\n",
    );
}

// 15. The stub returned the zero of the RETURN type, so the non-i64 returns are
//     their own perspective: an empty String, 0.0 and false are what a
//     regression would print.
#[test]
fn item_import_15_string_float_and_bool_returns() {
    assert_project(
        &[
            (
                "leaf.wi",
                "pub fn name() -> String { return \"leaf\"; }
pub fn ratio() -> f64 { return 1.5; }
pub fn yes() -> bool { return true; }
",
            ),
            (
                "lib.wi",
                "import leaf::name;
import leaf::ratio;
import leaf::yes;

pub fn run() -> String {
    return name() + \" \" + ratio().toString() + \" \" + yes().toString();
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "leaf 1.5 true\n",
    );
}

// 16. A GC-managed argument crosses the boundary: the callee walks an array the
//     caller built.
#[test]
fn item_import_16_an_array_argument() {
    assert_project(
        &[
            (
                "leaf.wi",
                "import std::collections::Array;

pub fn total(xs: Array<i64>) -> i64 {
    let mut sum = 0;
    let mut i = 0;
    while i < xs.len() {
        sum = sum + xs[i];
        i = i + 1;
    }
    return sum;
}
",
            ),
            (
                "lib.wi",
                "import leaf::total;

pub fn run() -> i64 {
    return total([1, 2, 3, 4]);
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "10\n",
    );
}

// 17. willow-2g6s. `b.get()` mangles to `Box.get`, which only exists as an
//     alias of `leaf.Box.get` — and the module side never installed it, so the
//     body was rejected with E0800 rather than miscompiled.
#[test]
fn item_import_17_a_method_on_an_imported_class() {
    assert_project(
        &[
            ("leaf.wi", BOX_CLASS),
            (
                "lib.wi",
                "import leaf::Box;

pub fn run() -> i64 {
    let b = new Box(7);
    return b.get() * 3;
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "21\n",
    );
}

// 18. A static call spells the class itself (`Box::make`) and mangles the same
//     way, so it needs the same alias.
#[test]
fn item_import_18_a_static_method_on_an_imported_class() {
    assert_project(
        &[
            ("leaf.wi", BOX_CLASS),
            (
                "lib.wi",
                "import leaf::Box;

pub fn run() -> i64 {
    let b = Box::make(5);
    return b.get();
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "5\n",
    );
}

// 19. Control. A field read needs the LAYOUT, which the type alias already
//     carried, so this half worked before willow-2g6s and must keep working.
#[test]
fn item_import_19_a_field_read_on_an_imported_class() {
    assert_project(
        &[
            ("leaf.wi", BOX_CLASS),
            (
                "lib.wi",
                "import leaf::Box;

pub fn run() -> i64 {
    let b = new Box(7);
    return b.n * 3;
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "21\n",
    );
}

// 20. Own and inherited methods on the same imported class. The two used to
//     part company: the ancestry walk in `resolve_class_method` reaches the
//     BASE under its qualified `leaf::Base` name and resolved `twice` even
//     before willow-2g6s, while `thrice` — declared on the imported class
//     itself — had no symbol under `Kid.thrice` and failed. The alias set is
//     taken from every symbol under the class's method prefix, so both answer.
#[test]
fn item_import_20_own_and_inherited_methods_on_an_imported_class() {
    assert_project(
        &[
            (
                "leaf.wi",
                "pub open class Base {
    pub n: i64;

    pub init(self, n: i64) { self.n = n; }

    pub fn twice(self) -> i64 { return self.n * 2; }
}

pub class Kid extends Base {
    pub init(self, n: i64) { super.init(n); }

    pub fn thrice(self) -> i64 { return self.n * 3; }
}
",
            ),
            (
                "lib.wi",
                "import leaf::Kid;

pub fn run() -> i64 {
    let k = new Kid(6);
    return k.twice() + k.thrice();
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "30\n",
    );
}

// 21. The module extends the imported class and overrides its open method, so
//     the call goes through the descriptor slot rather than a direct symbol.
#[test]
fn item_import_21_a_module_subclasses_an_imported_class() {
    assert_project(
        &[
            (
                "leaf.wi",
                "pub open class Shape {
    pub open fn sides(self) -> i64 { return 0; }
}
",
            ),
            (
                "lib.wi",
                "import leaf::Shape;

class Tri extends Shape {
    pub override fn sides(self) -> i64 { return 3; }
}

pub fn run() -> i64 {
    let s: Shape = new Tri();
    return s.sides();
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "3\n",
    );
}

// 22. Two different classes, one bare name. The method aliases are installed
//     under the unit snapshot, so `Item.tag` means one class in `a` and the
//     other in `b`.
#[test]
fn item_import_22_two_modules_import_two_classes_of_one_name() {
    assert_project(
        &[
            (
                "one.wi",
                "pub class Item {
    pub n: i64;

    pub init(self, n: i64) { self.n = n; }

    pub fn tag(self) -> i64 { return self.n + 100; }
}
",
            ),
            (
                "two.wi",
                "pub class Item {
    pub n: i64;

    pub init(self, n: i64) { self.n = n; }

    pub fn tag(self) -> i64 { return self.n + 200; }
}
",
            ),
            (
                "a.wi",
                "import one::Item;

pub fn run() -> i64 { let i = new Item(1); return i.tag(); }
",
            ),
            (
                "b.wi",
                "import two::Item;

pub fn run() -> i64 { let i = new Item(1); return i.tag(); }
",
            ),
            (
                "app.wi",
                "import a;
import b;

fn main() {
    println(a::run());
    println(b::run());
}
",
            ),
        ],
        "101\n201\n",
    );
}

// 23. An enum reaches the module through the same item import. Its variant tags
//     come from the qualified registration, so an unaliased name would match
//     tag 0 for everything.
#[test]
fn item_import_23_an_imported_enum_is_matched() {
    assert_project(
        &[
            ("leaf.wi", "pub enum Color { Red, Green, Blue }\n"),
            (
                "lib.wi",
                "import leaf::Color;

pub fn run() -> i64 {
    let c = Color::Green;
    match c {
        Color::Red => { return 1; }
        Color::Green => { return 2; }
        Color::Blue => { return 3; }
    }
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "2\n",
    );
}

// 24. An imported interface has to be aliased before the module's classes are
//     declared, or the `implements` name misses and no vtable is built
//     (willow-0g8j.3).
#[test]
fn item_import_24_an_imported_interface_is_implemented_and_boxed() {
    assert_project(
        &[
            (
                "proto.wi",
                "pub interface Named {
    fn name(self) -> String;
}
",
            ),
            (
                "lib.wi",
                "import proto::Named;

class Dog implements Named {
    pub fn name(self) -> String { return \"dog\"; }
}

pub fn run() -> String {
    let n: Named = new Dog();
    return n.name();
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
        "dog\n",
    );
}

// 25. The imported class appears in the module's OWN public signature, so the
//     entry file passes and receives it — one runtime class under two
//     spellings. This one held before willow-2g6s, but only by accident: the
//     entry file item-imports the same class, and its bindings are global and
//     installed before any module BODY is lowered, so the module borrowed the
//     entry's `Box.get`. Kept because the borrowing is what perspective 26
//     rules out on its own.
#[test]
fn item_import_25_an_imported_class_in_the_modules_signature() {
    assert_project(
        &[
            ("leaf.wi", BOX_CLASS),
            (
                "lib.wi",
                "import leaf::Box;

pub fn bump(b: Box) -> Box {
    return new Box(b.get() + 1);
}

pub fn run() -> i64 {
    return bump(new Box(4)).get();
}
",
            ),
            (
                "app.wi",
                "import lib;
import leaf::Box;

fn main() {
    println(lib::run());
    println(lib::bump(new Box(9)).get());
}
",
            ),
        ],
        "5\n10\n",
    );
}

// 26. The method aliases are per unit in both directions: the module that
//     imported `Box` calls it by the short name, and the module that imported
//     the whole `leaf` calls the very same methods by the qualified one.
#[test]
fn item_import_26_short_and_qualified_spellings_coexist() {
    assert_project(
        &[
            ("leaf.wi", BOX_CLASS),
            (
                "a.wi",
                "import leaf::Box;

pub fn run() -> i64 { return new Box(3).get(); }
",
            ),
            (
                "b.wi",
                "import leaf;

pub fn run() -> i64 { return new leaf::Box(4).get(); }
",
            ),
            (
                "app.wi",
                "import a;
import b;

fn main() {
    println(a::run());
    println(b::run());
}
",
            ),
        ],
        "3\n4\n",
    );
}

// 27. Control (willow-vtlr). A module's item import belongs to that module: a
//     sibling that never imported it does not get the name.
#[test]
fn item_import_27_a_sibling_module_does_not_see_the_import() {
    let stderr = project_error(&[
        ("leaf.wi", "pub fn value() -> i64 { return 40; }\n"),
        (
            "a.wi",
            "import leaf::value;

pub fn run() -> i64 { return value(); }
",
        ),
        ("b.wi", "pub fn run() -> i64 { return value(); }\n"),
        (
            "app.wi",
            "import a;
import b;

fn main() {
    println(a::run());
    println(b::run());
}
",
        ),
    ]);
    assert!(
        stderr.contains("E0350") && stderr.contains("cannot find function `value`"),
        "expected the sibling module's call to be unresolved:\n{stderr}"
    );
}

// 28. The same in the other direction: the entry file does not inherit what a
//     module imported.
#[test]
fn item_import_28_the_entry_does_not_see_a_modules_import() {
    let stderr = project_error(&[
        ("leaf.wi", "pub fn value() -> i64 { return 40; }\n"),
        (
            "lib.wi",
            "import leaf::value;

pub fn run() -> i64 { return value(); }
",
        ),
        (
            "app.wi",
            "import lib;

fn main() {
    println(value());
}
",
        ),
    ]);
    assert!(
        stderr.contains("E0350") && stderr.contains("cannot find function `value`"),
        "expected the entry's call to be unresolved:\n{stderr}"
    );
}

// 29. The class half of the same isolation: aliasing `Box`'s methods for one
//     module must not make the class itself nameable from another.
#[test]
fn item_import_29_a_sibling_module_cannot_name_the_imported_class() {
    let stderr = project_error(&[
        ("leaf.wi", BOX_CLASS),
        (
            "a.wi",
            "import leaf::Box;

pub fn run() -> i64 { let b = new Box(3); return b.get(); }
",
        ),
        (
            "b.wi",
            "pub fn run() -> i64 { let b = new Box(3); return b.get(); }\n",
        ),
        (
            "app.wi",
            "import a;
import b;

fn main() {
    println(a::run());
    println(b::run());
}
",
        ),
    ]);
    assert!(
        stderr.contains("E0844") && stderr.contains("unknown class `Box`"),
        "expected the sibling module's class name to be unknown:\n{stderr}"
    );
}

// 30. `--release` drops the debug instrumentation and re-runs the same
//     lowering, so both halves are checked in the other build mode.
#[test]
fn item_import_30_both_halves_hold_in_release() {
    let project = TestProject::new(
        "item_import_release",
        &[
            ("leaf.wi", BOX_CLASS),
            (
                "lib.wi",
                "import leaf::Box;

pub fn value() -> i64 { return new Box(40).get(); }

pub fn run() -> i64 { return Box::make(value() + 4).get(); }
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
    );
    let compiled = project.compile_release("app.wi");
    assert!(
        compiled.status.success(),
        "expected the release build to succeed:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let out = project.run();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "44\n");
}

// 31. The runnable example, built and run the way `runtime.rs` builds it, under
//     GC stress: every allocation the imported class makes is a collection
//     point, so the module's own view of the class has to survive one.
#[test]
fn item_import_31_the_example_program_under_gc_stress() {
    let project = TestProject::new(
        "item_import_example",
        &[
            (
                "leaf.wi",
                "pub class Tag {
    pub s: String;

    pub init(self, s: String) { self.s = s; }

    pub fn shout(self) -> String { return self.s + \"!\"; }
}
",
            ),
            (
                "lib.wi",
                "import leaf::Tag;

pub fn run() -> String {
    let mut out = \"\";
    let mut i = 0;
    while i < 200 {
        let t = new Tag(\"x\");
        out = t.shout();
        i = i + 1;
    }
    return out;
}
",
            ),
            ("app.wi", CALL_INTO_LIB),
        ],
    );
    let compiled = project.compile("app.wi");
    assert!(
        compiled.status.success(),
        "expected the project to compile:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let out = project.run_with_env(&[("WILLOW_GC_STRESS", "alloc")]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "x!\n");
}
