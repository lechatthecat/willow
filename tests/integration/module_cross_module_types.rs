//! Cross-module type names in a module's exported signature (willow-sxcp).
//!
//! A module that reaches a type through its OWN item import writes that type's
//! BARE name into what it exports:
//!
//! ```willow
//! // mid.wi
//! import base::Parcel;
//! pub class Crate extends Parcel { ... }
//! ```
//!
//! Both halves of the compiler qualified that signature by prefixing every bare
//! name with the module's own spelling, so what left `mid` said `mid::Parcel` --
//! a class nothing declares. The consequences ran the length of the pipeline:
//! the base resolved to nothing, so `Crate` inherited neither `side` nor
//! `area`; an `implements` name matched no interface, so no vtable was emitted
//! and every box fell back to the raw object; a field typed by an imported enum
//! named a type the walker could not find, which rejected the whole module with
//! `error[E0800] ... outside the walker's subset`.
//!
//! The fix translates such a name to the identity the tables answer to, on both
//! sides. The type checker records the access spelling each module is
//! registered under and renames a module's own item imports through it, and
//! registers the whole dependency closure (types only) so a module two hops away
//! is nameable at all. The back end qualifies only module-LOCAL names and runs
//! the rest through the aliases the unit's item imports installed, which is also
//! what makes an imported INTERFACE survive: a bare class name a later unit can
//! still find by scanning modules, an interface cannot.
//!
//! 34 perspectives:
//!   1 the bead's repro: a module class extends an item-imported base
//!   2 the entry item-imports that subclass and constructs it
//!   3 a subclass that overrides nothing inherits the imported base's method
//!   4 `super.init` into the imported base, plus a field of its own
//!   5 a three-module chain: base <- mid <- top
//!   6 transitive: the entry imports only `mid`, which alone reaches `base`
//!   7 a field typed by the item-imported class
//!   8 a module function's parameter and return typed by it
//!   9 a class method's parameter and return typed by it
//!  10 a field typed by an item-imported ENUM, matched in the entry
//!  11 a module function returning the imported enum
//!  12 an item-imported INTERFACE in `implements` and in a signature
//!  13 ...with an entry file that never imports that interface
//!  14 the entry item-imports the interface under an alias
//!  15 a builtin generic (`Map<String, i64>`) is not module-prefixed
//!  16 an `Array` of the imported class
//!  17 the module aliases its own item import (`as P`)
//!  18 the entry aliases the module (`import mid as m;`)
//!  19 two modules subclass one shared base
//!  20 the implicit memberwise constructor over inherited and imported fields
//!  21 two hops: `top` takes `mid`'s class, whose field is `base`'s class
//!  22 `extends` and `implements`, both item-imported
//!  23 a static method returning the imported class
//!  24 an entry subclass of a module class whose own base is imported
//!  25 the entry file aliases the module the item import names (willow-kd1v)
//!  26 control: the entry cannot name the module's imported class
//!  27 control: a sibling module cannot see another module's import
//!  28 control: an unrelated class still mismatches, named by its own module
//!  29 control: an entry class of the same name is not the imported one
//!  30 `--release` keeps the whole chain
//!  31 the runnable example, under GC stress
//!  32 transitive metadata does not import type names
//!  33 explicit type names respect import scope at every use site
//!  34 inferred values, inherited members, and explicit imports still work

use super::support::{
    TestProject, compile_temp_project_and_run, compile_temp_project_error_stderr,
};

/// The leaf module: one open class, one interface, one enum — the three kinds
/// of name a dependent module can write into its own exported signature.
const BASE: &str = "pub open class Parcel {
    pub side: i64;

    pub init(self, side: i64) { self.side = side; }

    pub open fn area(self) -> i64 { return self.side * self.side; }
}

pub interface Sized {
    fn size(self) -> i64;
}

pub enum Grade { Low, High }
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

// 1. The bug report. `extends Parcel` was exported as `extends mid::Parcel`, so
//    `Crate` had no base at all: `self.side` was not a field of it and the
//    entry's `c.side` did not resolve.
#[test]
fn cmt_01_a_module_class_extends_an_item_imported_base() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;

pub class Crate extends Parcel {
    pub override fn area(self) -> i64 { return self.side * 3; }
}

pub fn make(n: i64) -> Crate { return new Crate(n); }
",
            ),
            (
                "app.wi",
                "import mid;

fn main() {
    let c = mid::make(3);
    println(c.area());
    println(c.side);
}
",
            ),
        ],
        "9\n3\n",
    );
}

// 2. The same class constructed in the ENTRY file, which reaches it by an item
//    import of its own: the inherited field has to be in the layout the entry
//    allocates, not only in the one the module's own body sees.
#[test]
fn cmt_02_the_entry_constructs_the_module_subclass() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;

pub class Crate extends Parcel {
    pub override fn area(self) -> i64 { return self.side * 3; }
}
",
            ),
            (
                "app.wi",
                "import mid;
import mid::Crate;

fn main() {
    let c = new Crate(4);
    println(c.area());
    println(c.side);
}
",
            ),
        ],
        "12\n4\n",
    );
}

// 3. No override anywhere: the method that runs is the imported base's, which
//    only exists on the subclass if the base resolved.
#[test]
fn cmt_03_a_subclass_inherits_the_imported_bases_method() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;

pub class Plain extends Parcel {}

pub fn make(n: i64) -> Plain { return new Plain(n); }
",
            ),
            (
                "app.wi",
                "import mid;\n\nfn main() { println(mid::make(5).area()); }\n",
            ),
        ],
        "25\n",
    );
}

// 4. `super.init` into the imported base, with a field of the subclass's own
//    after it: the constructor's parameter types are qualified by the same pass
//    as the field types.
#[test]
fn cmt_04_super_init_reaches_the_imported_base() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;

pub class Crate extends Parcel {
    pub extra: i64;

    pub init(self, side: i64, extra: i64) {
        super.init(side);
        self.extra = extra;
    }

    pub override fn area(self) -> i64 { return self.side * self.extra; }
}

pub fn make(n: i64) -> Crate { return new Crate(n, 3); }
",
            ),
            (
                "app.wi",
                "import mid;

fn main() {
    let c = mid::make(4);
    println(c.area());
    println(c.extra);
}
",
            ),
        ],
        "12\n3\n",
    );
}

// 5. Three modules deep. `top` item-imports `mid`'s class, which item-imports
//    `base`'s, so the chain the entry walks leaves and re-enters two modules.
#[test]
fn cmt_05_a_three_module_chain() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;

pub open class Crate extends Parcel {
    pub open override fn area(self) -> i64 { return self.side * 3; }
}
",
            ),
            (
                "top.wi",
                "import mid::Crate;

pub class Pallet extends Crate {
    pub override fn area(self) -> i64 { return self.side * 10; }
}

pub fn make(n: i64) -> Pallet { return new Pallet(n); }
",
            ),
            (
                "app.wi",
                "import top;

fn main() {
    let p = top::make(2);
    println(p.area());
    println(p.side);
}
",
            ),
        ],
        "20\n2\n",
    );
}

// 6. The transitive half: the entry imports `mid` alone, so `base` is in the
//    program only because `mid` imports it. Nothing registered it in the
//    checker that qualifies `mid`'s signature until the dependency closure was
//    registered too.
#[test]
fn cmt_06_the_entry_never_imports_the_base_module() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;

pub fn twice(n: i64) -> i64 { return new Parcel(n).area() * 2; }
",
            ),
            (
                "app.wi",
                "import mid;\n\nfn main() { println(mid::twice(3)); }\n",
            ),
        ],
        "18\n",
    );
}

// 7. A FIELD typed by the item-imported class: the layout the entry reads has
//    to hold the base module's class, not a name of `mid`'s own.
#[test]
fn cmt_07_a_field_typed_by_the_imported_class() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;

pub class Holder {
    pub p: Parcel;
    pub tag: i64;
}

pub fn make(n: i64) -> Holder { return new Holder(new Parcel(n), 9); }
",
            ),
            (
                "app.wi",
                "import mid;

fn main() {
    let h = mid::make(4);
    println(h.p.area());
    println(h.tag);
}
",
            ),
        ],
        "16\n9\n",
    );
}

// 8. The imported class in a module function's parameter AND return position,
//    with the entry supplying the argument under its own spelling.
#[test]
fn cmt_08_a_module_function_takes_and_returns_the_imported_class() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;

pub fn grow(p: Parcel) -> Parcel { return new Parcel(p.side + 1); }
",
            ),
            (
                "app.wi",
                "import mid;
import base::Parcel;

fn main() { println(mid::grow(new Parcel(2)).area()); }
",
            ),
        ],
        "9\n",
    );
}

// 9. The same two positions on a class METHOD, which is qualified by the class
//    pass rather than the function one.
#[test]
fn cmt_09_a_method_takes_and_returns_the_imported_class() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;

pub class Shop {
    pub fee: i64;

    pub fn price(self, p: Parcel) -> i64 { return p.area() + self.fee; }
    pub fn best(self) -> Parcel { return new Parcel(self.fee); }
}

pub fn make(fee: i64) -> Shop { return new Shop(fee); }
",
            ),
            (
                "app.wi",
                "import mid;
import base::Parcel;

fn main() {
    let s = mid::make(5);
    println(s.price(new Parcel(3)));
    println(s.best().side);
}
",
            ),
        ],
        "14\n5\n",
    );
}

// 10. An ENUM in a class signature. An enum has one identity build-wide, so the
//     module's own spelling of it was not merely unknown but a second name for
//     something the walker already had: `enum_def("mid::Grade")` was `None`
//     while `enum_def("Grade")` answered, and the class fell out of the subset.
#[test]
fn cmt_10_a_field_typed_by_an_item_imported_enum() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Grade;

pub class Item {
    pub n: i64;
    pub g: Grade;
}

pub fn make(n: i64) -> Item { return new Item(n, Grade::High); }
",
            ),
            (
                "app.wi",
                "import mid;
import base::Grade;

fn main() {
    let it = mid::make(7);
    println(it.n);
    match it.g {
        Grade::High => println(\"high\"),
        Grade::Low => println(\"low\"),
    }
}
",
            ),
        ],
        "7\nhigh\n",
    );
}

// 11. The same enum in a FUNCTION's return position, matched in the entry.
#[test]
fn cmt_11_a_module_function_returns_the_imported_enum() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Grade;

pub fn grade(n: i64) -> Grade {
    if n > 5 { return Grade::High; }
    return Grade::Low;
}
",
            ),
            (
                "app.wi",
                "import mid;
import base::Grade;

fn main() {
    match mid::grade(9) {
        Grade::High => println(\"high\"),
        Grade::Low => println(\"low\"),
    }
}
",
            ),
        ],
        "high\n",
    );
}

// 12. An item-imported INTERFACE, both in `implements` and in the parameter the
//     module boxes against. Renaming it left the vtable lookup with a type
//     nothing declares, so the class got no vtable at all.
#[test]
fn cmt_12_an_item_imported_interface_is_implemented_and_boxed() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Sized;

pub class Item implements Sized {
    pub n: i64;
    pub fn size(self) -> i64 { return self.n; }
}

pub fn measure(s: Sized) -> i64 { return s.size() * 2; }
pub fn make(n: i64) -> Item { return new Item(n); }
",
            ),
            (
                "app.wi",
                "import mid;
import base::Sized;

fn main() { println(mid::measure(mid::make(4))); }
",
            ),
        ],
        "8\n",
    );
}

// 13. The same program with the interface import taken OUT of the entry file.
//     A bare class name a later unit can still resolve by scanning modules; an
//     interface has no such scan, so leaving the name bare cost the boxing site
//     its lowering here, and only the canonical spelling works.
#[test]
fn cmt_13_the_entry_never_imports_the_interface() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Sized;

pub class Item implements Sized {
    pub n: i64;
    pub fn size(self) -> i64 { return self.n; }
}

pub fn measure(s: Sized) -> i64 { return s.size() * 2; }
pub fn make(n: i64) -> Item { return new Item(n); }
",
            ),
            (
                "app.wi",
                "import mid;

fn main() { println(mid::measure(mid::make(4))); }
",
            ),
        ],
        "8\n",
    );
}

// 14. The entry names the same interface under an alias of its own and boxes
//     the module's class into it: one interface identity, two spellings.
#[test]
fn cmt_14_the_entry_aliases_the_interface() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Sized;

pub class Item implements Sized {
    pub n: i64;
    pub fn size(self) -> i64 { return self.n; }
}

pub fn make(n: i64) -> Item { return new Item(n); }
",
            ),
            (
                "app.wi",
                "import mid;
import base::Sized as S;

fn describe(s: S) -> i64 { return s.size() + 1; }

fn main() { println(describe(mid::make(4))); }
",
            ),
        ],
        "5\n",
    );
}

// 15. A builtin generic in a module class signature. `Map` is not a name any
//     module declares, so prefixing it produced `mid::Map` and the field type
//     stopped being a map at all.
#[test]
fn cmt_15_a_builtin_generic_field_is_not_prefixed() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import std::collections::Map;

pub class Bag {
    pub m: Map<String, i64>;
}

pub fn make() -> Bag {
    let m: Map<String, i64> = Map::new();
    m.insert(\"k\", 3);
    return new Bag(m);
}
",
            ),
            (
                "app.wi",
                "import mid;

fn main() { let b = mid::make(); println(b.m.len()); }
",
            ),
        ],
        "1\n",
    );
}

// 16. The imported class inside a builtin generic: the element type is
//     qualified, the `Array` head is not.
#[test]
fn cmt_16_an_array_of_the_imported_class() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;
import std::collections::Array;

pub class Load {
    pub ps: Array<Parcel>;
}

pub fn make() -> Load { return new Load([new Parcel(2), new Parcel(3)]); }
",
            ),
            (
                "app.wi",
                "import mid;

fn main() {
    let l = mid::make();
    println(l.ps[0].area());
    println(l.ps[1].area());
}
",
            ),
        ],
        "4\n9\n",
    );
}

// 17. The module gives its own import a local alias, so the name in its
//     signature exists nowhere else in the build.
#[test]
fn cmt_17_the_module_aliases_its_item_import() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel as P;

pub class Holder {
    pub p: P;
}

pub fn make(n: i64) -> Holder { return new Holder(new P(n)); }
pub fn size(h: Holder) -> i64 { return h.p.area(); }
",
            ),
            (
                "app.wi",
                "import mid;

fn main() { println(mid::size(mid::make(5))); }
",
            ),
        ],
        "25\n",
    );
}

// 18. The entry reaches the module under an alias, which is the spelling every
//     table in the build is then keyed by.
#[test]
fn cmt_18_the_entry_aliases_the_module() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;

pub class Crate extends Parcel {}

pub fn make(n: i64) -> Crate { return new Crate(n); }
",
            ),
            (
                "app.wi",
                "import mid as m;

fn main() { println(m::make(6).area()); }
",
            ),
        ],
        "36\n",
    );
}

// 19. Two modules item-import the same base and subclass it. Each exported
//     `extends` used to name its OWN module, which made two unrelated bases out
//     of one class.
#[test]
fn cmt_19_two_modules_subclass_one_base() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "one.wi",
                "import base::Parcel;

pub class A extends Parcel {}

pub fn make(n: i64) -> A { return new A(n); }
",
            ),
            (
                "two.wi",
                "import base::Parcel;

pub class B extends Parcel {}

pub fn make(n: i64) -> B { return new B(n); }
",
            ),
            (
                "app.wi",
                "import one;
import two;

fn main() {
    println(one::make(2).area());
    println(two::make(3).area());
}
",
            ),
        ],
        "4\n9\n",
    );
}

// 20. No declared constructor: the implicit memberwise one takes the inherited
//     field first and the imported-enum field second, so both qualifications
//     have to agree on the order and the types.
#[test]
fn cmt_20_the_implicit_memberwise_constructor() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;
import base::Grade;

pub class Crate extends Parcel {
    pub g: Grade;
}

pub fn make(n: i64) -> Crate { return new Crate(n, Grade::Low); }
",
            ),
            (
                "app.wi",
                "import mid;
import base::Grade;

fn main() {
    let c = mid::make(3);
    println(c.area());
    match c.g {
        Grade::Low => println(\"low\"),
        Grade::High => println(\"high\"),
    }
}
",
            ),
        ],
        "9\nlow\n",
    );
}

// 21. Two hops in one signature: `top` takes a class of `mid`, whose own field
//     is a class of `base`, and `top` never imports `base` at all.
#[test]
fn cmt_21_a_signature_two_modules_deep() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;

pub class Holder {
    pub p: Parcel;
}

pub fn make(n: i64) -> Holder { return new Holder(new Parcel(n)); }
",
            ),
            (
                "top.wi",
                "import mid::Holder;

pub fn peek(h: Holder) -> i64 { return h.p.area(); }
",
            ),
            (
                "app.wi",
                "import mid;
import top;

fn main() { println(top::peek(mid::make(4))); }
",
            ),
        ],
        "16\n",
    );
}

// 22. Both at once: an imported base class and an imported interface on one
//     declaration, with the module boxing its own subclass.
#[test]
fn cmt_22_extends_and_implements_are_both_imported() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;
import base::Sized;

pub class Crate extends Parcel implements Sized {
    pub fn size(self) -> i64 { return self.side; }
}

pub fn measure(s: Sized) -> i64 { return s.size() * 7; }
pub fn make(n: i64) -> Crate { return new Crate(n); }
",
            ),
            (
                "app.wi",
                "import mid;

fn main() {
    let c = mid::make(3);
    println(c.area());
    println(mid::measure(c));
}
",
            ),
        ],
        "9\n21\n",
    );
}

// 23. A STATIC method returning the imported class, called on the module class
//     the entry item-imported.
#[test]
fn cmt_23_a_static_method_returns_the_imported_class() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;

pub class Depot {
    pub n: i64;

    pub static fn stock(n: i64) -> Parcel { return new Parcel(n); }
}
",
            ),
            (
                "app.wi",
                "import mid;
import mid::Depot;

fn main() { println(Depot::stock(5).area()); }
",
            ),
        ],
        "25\n",
    );
}

// 24. The chain continues into the entry file: a local subclass of a module
//     class whose own base is item-imported, so the layout it extends was
//     assembled across two module boundaries.
#[test]
fn cmt_24_an_entry_subclass_of_the_module_subclass() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;

pub open class Crate extends Parcel {
    pub open override fn area(self) -> i64 { return self.side * 3; }
}
",
            ),
            (
                "app.wi",
                "import mid;
import mid::Crate;

class Big extends Crate {
    pub override fn area(self) -> i64 { return self.side * 100; }
}

fn main() {
    let b = new Big(2);
    println(b.area());
    println(b.side);
}
",
            ),
        ],
        "200\n2\n",
    );
}

// 25. The entry file reaches `base` under an alias, so the build's tables are
//     keyed `proto::Parcel` while `mid` names the very same class `Parcel`
//     through its own canonical item import. Once the checker agreed the two
//     were one class, the back end had to agree as well: the item import's
//     aliases are installed under the spelling the tables answer to
//     (willow-kd1v), not the canonical one the import line writes.
#[test]
fn cmt_25_the_entry_aliases_the_module_the_item_import_names() {
    assert_project(
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;

pub fn grow(p: Parcel) -> Parcel { return new Parcel(p.side + 1); }
",
            ),
            (
                "app.wi",
                "import base as proto;
import mid;

fn main() { println(mid::grow(new proto::Parcel(2)).area()); }
",
            ),
        ],
        "9\n",
    );
}

// 26. Control: the module's item import is not a re-export. The entry file has
//     to import `Parcel` itself, and naming it bare is an error rather than a
//     silent hit on the module's spelling.
#[test]
fn cmt_26_the_entry_cannot_name_the_modules_import() {
    let stderr = project_error(&[
        ("base.wi", BASE),
        (
            "mid.wi",
            "import base::Parcel;

pub fn twice(n: i64) -> i64 { return new Parcel(n).area() * 2; }
",
        ),
        (
            "app.wi",
            "import mid;

fn main() { println(new Parcel(2).area()); }
",
        ),
    ]);
    assert!(
        stderr.contains("E0844") && stderr.contains("unknown class `Parcel`"),
        "expected the entry's bare `Parcel` to be unknown:\n{stderr}"
    );
}

// 27. Control: nor does one module's import reach a sibling. `other.wi`
//     imports nothing, so `Parcel` is unknown there even though `mid.wi` in the
//     same build resolves it.
#[test]
fn cmt_27_a_sibling_module_cannot_see_the_import() {
    let stderr = project_error(&[
        ("base.wi", BASE),
        (
            "mid.wi",
            "import base::Parcel;

pub fn twice(n: i64) -> i64 { return new Parcel(n).area() * 2; }
",
        ),
        (
            "other.wi",
            "pub fn nope(n: i64) -> i64 { return new Parcel(n).area(); }
",
        ),
        (
            "app.wi",
            "import mid;
import other;

fn main() { println(other::nope(2)); }
",
        ),
    ]);
    assert!(
        stderr.contains("E0844") && stderr.contains("unknown class `Parcel`"),
        "expected the sibling module's `Parcel` to be unknown:\n{stderr}"
    );
}

// 28. Control: the exported parameter type still rejects an unrelated class,
//     and the diagnostic names it by the module that DECLARES it — the
//     signature's whole point.
#[test]
fn cmt_28_an_unrelated_class_is_still_a_type_error() {
    let stderr = project_error(&[
        ("base.wi", BASE),
        (
            "mid.wi",
            "import base::Parcel;

pub fn area(p: Parcel) -> i64 { return p.area(); }
",
        ),
        (
            "app.wi",
            "import mid;

class Other {
    pub side: i64;
}

fn main() { println(mid::area(new Other(2))); }
",
        ),
    ]);
    assert!(
        stderr.contains("expected `base::Parcel`, found `Other`"),
        "expected the mismatch to name the declaring module:\n{stderr}"
    );
}

// 29. Control: an entry class that merely shares the NAME is not the imported
//     class. Two classes of one name stay two classes.
#[test]
fn cmt_29_an_entry_class_of_the_same_name_is_not_the_import() {
    let stderr = project_error(&[
        ("base.wi", BASE),
        (
            "mid.wi",
            "import base::Parcel;

pub fn base_area(p: Parcel) -> i64 { return p.area(); }
",
        ),
        (
            "app.wi",
            "import mid;

class Parcel {
    pub side: i64;
}

fn main() { println(mid::base_area(new Parcel(2))); }
",
        ),
    ]);
    assert!(
        stderr.contains("expected `base::Parcel`, found `Parcel`"),
        "expected the entry's own `Parcel` to be a different class:\n{stderr}"
    );
}

// 30. `--release` compiles the same chain: the qualification happens in
//     declaration, which both build modes share, and nothing about it may
//     depend on debug metadata.
#[test]
fn cmt_30_the_chain_compiles_in_release() {
    let project = TestProject::new(
        "cross_module_types_release",
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;
import base::Sized;

pub class Crate extends Parcel implements Sized {
    pub fn size(self) -> i64 { return self.side; }
}

pub fn measure(s: Sized) -> i64 { return s.size() * 7; }
pub fn make(n: i64) -> Crate { return new Crate(n); }
",
            ),
            (
                "app.wi",
                "import mid;

fn main() {
    let c = mid::make(3);
    println(c.area());
    println(mid::measure(c));
}
",
            ),
        ],
    );
    let compiled = project.compile_release("app.wi");
    assert!(
        compiled.status.success(),
        "expected the release build to succeed:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let out = project.run();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "9\n21\n");
}

// 31. The runnable example, under GC stress: every object the module allocates
//     carries a field typed by another module's class, so the collector walks a
//     layout that only the fix makes correct.
#[test]
fn cmt_31_the_example_program_under_gc_stress() {
    let project = TestProject::new(
        "cross_module_types_example",
        &[
            ("base.wi", BASE),
            (
                "mid.wi",
                "import base::Parcel;

pub class Holder {
    pub p: Parcel;
}

pub fn churn(n: i64) -> i64 {
    let mut i = 0;
    let mut last = 0;
    while i < 200 {
        let h = new Holder(new Parcel(n));
        last = h.p.area();
        i = i + 1;
    }
    return last;
}
",
            ),
            (
                "app.wi",
                "import mid;

fn main() { println(mid::churn(4)); }
",
            ),
        ],
    );
    let compiled = project.compile("app.wi");
    assert!(
        compiled.status.success(),
        "expected the project to compile:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let out = project.run_with_env(&[("WILLOW_GC_STRESS", "alloc")]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "16\n");
}

#[test]
fn cmt_32_transitive_metadata_does_not_import_type_names() {
    let stderr = project_error(&[
        ("base.wi", "pub class Parcel { pub x: i64; }"),
        ("mid.wi", "import base; pub fn f() {}"),
        (
            "top.wi",
            "import mid;
pub fn read() -> i64 { return new base::Parcel(7).x; }",
        ),
        ("app.wi", "import top; fn main() { println(top::read()); }"),
    ]);
    assert!(stderr.contains("not imported"), "{stderr}");
}

const SCOPE_BASE: &str = "pub open class Parcel {
    pub x: i64;
    pub static mut count: i64 = 0;
    pub static fn value() -> i64 { return 3; }
    pub open fn get(self) -> i64 { return self.x; }
}
pub interface Sized { fn size(self) -> i64; }
pub enum Grade { Low, High }
pub enum Packet { Empty, Full(i64) }";

const SCOPE_MID: &str = "import base::Parcel;
import base::Grade;
import base::Packet;
pub class Crate extends Parcel {}
pub fn parcel() -> Parcel { return new Parcel(7); }
pub fn grade() -> Grade { return Grade::High; }
pub fn take(p: Packet) -> i64 {
    match p { Packet::Empty => { return 0; } Packet::Full(v) => { return v; } }
}";

#[test]
fn cmt_33_transitive_type_names_are_hidden_at_source_use_sites() {
    for (case, body) in [
        (
            "static read",
            "pub fn bad() -> i64 { return base::Parcel::count; }",
        ),
        (
            "static call",
            "pub fn bad() -> i64 { return base::Parcel::value(); }",
        ),
        ("static write", "pub fn bad() { base::Parcel::count = 1; }"),
        ("parameter", "pub fn bad(p: base::Parcel) {}"),
        ("enum annotation", "pub fn bad(g: base::Grade) {}"),
        ("generic argument", "pub fn bad(p: Option<base::Parcel>) {}"),
        (
            "function annotation",
            "pub fn bad(f: fn(base::Parcel) -> i64) {}",
        ),
        ("field", "pub class Bad { pub p: base::Parcel; }"),
        ("extends", "pub class Bad extends base::Parcel {}"),
        (
            "implements",
            "pub class Bad implements base::Sized {
            pub fn size(self) -> i64 { return 1; }
        }",
        ),
        ("enum payload", "pub enum Bad { Value(base::Parcel) }"),
        ("enum value", "pub fn bad() { let g = base::Grade::High; }"),
        (
            "contextual enum value",
            "pub fn bad() -> i64 {
            return mid::take(base::Packet::Full(3));
        }",
        ),
        (
            "enum pattern",
            "pub fn bad() {
            match mid::grade() {
                base::Grade::Low => {},
                base::Grade::High => {},
            }
        }",
        ),
        (
            "lambda annotation",
            "pub fn bad() {
            let f = |p: base::Parcel| -> i64 { return p.x; };
        }",
        ),
    ] {
        let top = format!("import mid;\n{body}");
        let stderr = project_error(&[
            ("base.wi", SCOPE_BASE),
            ("mid.wi", SCOPE_MID),
            ("top.wi", &top),
            ("app.wi", "import top; fn main() {}"),
        ]);
        assert!(stderr.contains("not imported"), "{case}: {stderr}");
        assert!(!stderr.contains("error[E0800]"), "{case}: {stderr}");
    }
}

#[test]
fn cmt_34_transitive_values_and_explicit_imports_still_work() {
    for imports in [
        "",
        "import base;",
        "import base as b;",
        "import base::Parcel;",
    ] {
        let explicit = match imports {
            "import base;" => "println(new base::Parcel(8).get());",
            "import base as b;" => "println(new b::Parcel(8).get());",
            "import base::Parcel;" => "println(new Parcel(8).get());",
            _ => "",
        };
        let top = format!(
            "import mid;
{imports}
pub fn run() {{
    let p = mid::parcel();
    println(p.x);
    println(p.get());
    println(new mid::Crate(9).get());
    {explicit}
}}"
        );
        let expected = if imports.is_empty() {
            "7\n7\n9\n"
        } else {
            "7\n7\n9\n8\n"
        };
        assert_project(
            &[
                ("base.wi", SCOPE_BASE),
                ("mid.wi", SCOPE_MID),
                ("top.wi", &top),
                ("app.wi", "import top; fn main() { top::run(); }"),
            ],
            expected,
        );
    }
}
