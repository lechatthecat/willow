//! One module, two spellings, one type (willow-uvlp).
//!
//! A module is registered ONCE, under the spelling of whichever unit reached it
//! first, and every other unit is free to write its own: `import sales as biz;`
//! in a module, plain `import sales;` in the entry file. The type checker keyed
//! everything a module declares by that first spelling and never looked past
//! it, which broke three ways at once:
//!
//!   * the entry file's own import was an `error[E0350] unknown name sales`
//!     whenever another file got to the module first -- and the program
//!     compiled if you reordered the entry's imports;
//!   * a class crossing a unit boundary under two spellings was two types
//!     (`expected market::Amount, found sales::Amount`), for annotations,
//!     parameters, fields, bases and `implements` alike;
//!   * an enum written under the exporting module's own alias escaped the
//!     canonicalization willow-itcw gave enums, so one enum was two.
//!
//! The fix keeps ONE registration per module and binds every other spelling to
//! it: `alias_module_spelling` re-keys the same `ClassInfo`/`InterfaceInfo`/
//! `EnumInfo` under the second name, so identity (`ClassInfo::name`) is shared
//! and `class_extends` already compares through it. What a module EXPORTS is
//! translated the other way -- `rename_module_prefix` rewrites the leading
//! segment of a qualified path out of that module's private spelling and into
//! the registered one, and an enum lands on its build-wide identity.
//!
//! 32 perspectives:
//!   1 the entry's plain spelling, when a module aliased the module first
//!   2 the entry's alias, when a module imported it plainly first
//!   3 a class through two spellings, as an annotation
//!   4 ...as a parameter
//!   5 two MODULES that spell one module differently, passing the class along
//!   6 an enum in a signature written under the exporting module's alias
//!   7 an enum argument passed in under the entry's spelling
//!   8 an interface implemented under one spelling, boxed under another
//!   9 a field typed by the aliased module's class
//!  10 `extends` a class of the aliased module
//!  11 an `Array` of it
//!  12 a static method on it, called through both spellings
//!  13 the entry item-imports a type from the aliased module
//!  14 the module item-imports it while the ENTRY aliases the module
//!  15 three spellings of one module in one build
//!  16 transitive: the entry never names the aliased module at all
//!  17 an entry subclass of a module class whose base is the aliased class
//!  18 a `Map` whose value type is the aliased class
//!  19 two modules that each declare a class of the SAME name
//!  20 an interface both a module and the entry implement, boxed by the module
//!  21 a method's parameter and return, across spellings
//!  22 an enum PAYLOAD typed by the aliased module's class
//!  23 a chain where every hop renames every module
//!  24 an entry class whose name collides with the imported one
//!  25 control: a spelling no file imports is unknown
//!  26 control: an unknown type in a known module is still unknown
//!  27 control: another module's enum of the same name does not match
//!  28 control: an unrelated class still mismatches, named by the entry's own
//!     spelling
//!  29 `--release` keeps the whole chain
//!  30 the aliased class under GC stress
//!  31 `new` through the entry's alias, over a module that item-imported the
//!     same module's class as its base
//!  32 source aliases do not rewrite newly qualified local type identities

use super::support::{
    TestProject, compile_temp_project_and_run, compile_temp_project_error_stderr,
};

/// The module every other file in these programs reaches under a name of its
/// own: one open class, one interface, one enum.
const SALES: &str = "pub open class Amount {
    pub value: i64;

    pub init(self, value: i64) { self.value = value; }

    pub open fn doubled(self) -> i64 { return self.value * 2; }

    pub static fn zero() -> Amount { return new Amount(0); }
}

pub interface Priced {
    fn price(self) -> i64;
}

pub enum Grade { Low, High }

pub fn amount(v: i64) -> Amount { return new Amount(v); }

pub fn grade(v: i64) -> Grade {
    if v > 10 { return Grade::High; }
    return Grade::Low;
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

// 1. The bead's first shape. `ledger` reaches the module first and calls it
//    `biz`, so `sales` -- the name the ENTRY writes -- was a name nothing here
//    answered to. Reordering the entry's own imports used to decide whether the
//    program compiled.
#[test]
fn mai_01_the_entry_writes_the_plain_name_a_module_aliased() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub fn value(v: i64) -> i64 { return biz::amount(v).value; }
",
            ),
            (
                "app.wi",
                "import ledger;
import sales;

fn main() {
    println(ledger::value(2));
    println(sales::amount(3).value);
}
",
            ),
        ],
        "2\n3\n",
    );
}

// 2. The mirror: the module imported it plainly first, and the entry aliases
//    it. Here the alias is the spelling with no registration behind it.
#[test]
fn mai_02_the_entry_aliases_a_module_another_file_imported_plainly() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales;

pub fn value(v: i64) -> i64 { return sales::amount(v).value; }
",
            ),
            (
                "app.wi",
                "import ledger;
import sales as market;

fn main() {
    println(ledger::value(2));
    println(market::amount(3).value);
}
",
            ),
        ],
        "2\n3\n",
    );
}

// 3. The second shape. One class, written `sales::Amount` by the module that
//    exports it and `market::Amount` by the file that consumes it, has to be
//    one type.
#[test]
fn mai_03_a_class_annotation_under_the_other_spelling() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales;

pub fn make(v: i64) -> sales::Amount { return sales::amount(v); }
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;

fn main() {
    let a: market::Amount = ledger::make(14);
    println(a.value);
}
",
            ),
        ],
        "14\n",
    );
}

// 4. The same mismatch in argument position, which is where a consumer usually
//    meets it: the entry builds the value under its spelling and hands it to a
//    module that names the type under another.
#[test]
fn mai_04_a_parameter_under_the_other_spelling() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales;

pub fn twice(a: sales::Amount) -> i64 { return a.doubled(); }
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;

fn main() { println(ledger::twice(market::amount(5))); }
",
            ),
        ],
        "10\n",
    );
}

// 5. Neither end is the entry file: two modules spell the shared module
//    differently and pass its class between them.
#[test]
fn mai_05_two_modules_spell_the_shared_module_differently() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub fn make(v: i64) -> biz::Amount { return biz::amount(v); }
",
            ),
            (
                "books.wi",
                "import sales as store;

pub fn twice(a: store::Amount) -> i64 { return a.doubled(); }
",
            ),
            (
                "app.wi",
                "import ledger;
import books;

fn main() { println(books::twice(ledger::make(6))); }
",
            ),
        ],
        "12\n",
    );
}

// 6. The third shape. An enum has ONE identity build-wide (willow-itcw), but a
//    module that writes it under its own module alias exported `biz::Grade`,
//    which the canonicalization never saw.
#[test]
fn mai_06_an_enum_returned_under_the_modules_own_alias() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub fn top(v: i64) -> biz::Grade { return biz::grade(v); }
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;

fn main() {
    match ledger::top(50) {
        market::Grade::High => println(\"high\"),
        market::Grade::Low => println(\"low\"),
    }
}
",
            ),
        ],
        "high\n",
    );
}

// 7. The same enum going the other way: constructed in the entry under one
//    spelling, matched inside the module under another.
#[test]
fn mai_07_an_enum_argument_under_the_other_spelling() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub fn label(g: biz::Grade) -> i64 {
    match g {
        biz::Grade::High => { return 1; }
        biz::Grade::Low => { return 0; }
    }
}
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;

fn main() { println(ledger::label(market::Grade::High)); }
",
            ),
        ],
        "1\n",
    );
}

// 8. An interface: implemented by a module class under the module's spelling,
//    used as a parameter type in the entry under the entry's. Two spellings
//    meant no vtable matched.
#[test]
fn mai_08_an_interface_under_two_spellings() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub class Fee implements biz::Priced {
    pub n: i64;
    pub fn price(self) -> i64 { return self.n; }
}

pub fn make(n: i64) -> Fee { return new Fee(n); }
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;

fn charge(p: market::Priced) -> i64 { return p.price() + 1; }

fn main() { println(charge(ledger::make(4))); }
",
            ),
        ],
        "5\n",
    );
}

// 9. A FIELD typed by the aliased module's class: the layout the entry reads
//    has to be the one the module wrote.
#[test]
fn mai_09_a_field_typed_by_the_aliased_class() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub class Line {
    pub a: biz::Amount;
}

pub fn make(v: i64) -> Line { return new Line(biz::amount(v)); }
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;

fn main() {
    let l = ledger::make(7);
    let a: market::Amount = l.a;
    println(a.doubled());
}
",
            ),
        ],
        "14\n",
    );
}

// 10. `extends` written as a QUALIFIED path, which took a different route
//     through the checker than a bare one and skipped the translation
//     entirely: the base resolved to nothing and the subclass lost the
//     inherited field.
#[test]
fn mai_10_extends_the_aliased_modules_class() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub class Bonus extends biz::Amount {
    pub override fn doubled(self) -> i64 { return self.value * 3; }
}

pub fn make(v: i64) -> Bonus { return new Bonus(v); }
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;

fn main() {
    let b = ledger::make(5);
    let a: market::Amount = b;
    println(a.doubled());
    println(b.value);
}
",
            ),
        ],
        "15\n5\n",
    );
}

// 11. Inside a builtin generic: only the leading segment of the ELEMENT type is
//     translated, and `Array` itself is left alone.
#[test]
fn mai_11_an_array_of_the_aliased_class() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;
import std::collections::Array;

pub fn pair(v: i64) -> Array<biz::Amount> { return [biz::amount(v), biz::amount(v + 1)]; }
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;
import std::collections::Array;

fn main() {
    let xs: Array<market::Amount> = ledger::pair(3);
    println(xs[0].value);
    println(xs[1].doubled());
}
",
            ),
        ],
        "3\n8\n",
    );
}

// 12. A static method reached through both spellings in one program.
#[test]
fn mai_12_a_static_method_through_both_spellings() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub fn base() -> biz::Amount { return biz::Amount::zero(); }
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;

fn main() {
    let a: market::Amount = ledger::base();
    println(a.value);
    println(market::Amount::zero().value);
}
",
            ),
        ],
        "0\n0\n",
    );
}

// 13. The entry reaches the type by an ITEM import while a module reached the
//     module by an alias: the bare local name and the aliased qualified one are
//     the same class.
#[test]
fn mai_13_the_entry_item_imports_the_aliased_modules_type() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub fn make(v: i64) -> biz::Amount { return biz::amount(v); }
",
            ),
            (
                "app.wi",
                "import ledger;
import sales::Amount;

fn main() {
    let a: Amount = ledger::make(9);
    println(a.doubled());
}
",
            ),
        ],
        "18\n",
    );
}

// 14. The reverse pairing: the MODULE item-imports the type (so it writes the
//     bare name) while the entry aliases the module.
#[test]
fn mai_14_the_module_item_imports_what_the_entry_aliases() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales::Amount;

pub fn make(v: i64) -> Amount { return new Amount(v); }
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;

fn main() {
    let a: market::Amount = ledger::make(4);
    println(a.doubled());
}
",
            ),
        ],
        "8\n",
    );
}

// 15. Three spellings of one module in one build -- an alias, the plain name,
//     and a second alias in the entry -- all naming the same class.
#[test]
fn mai_15_three_spellings_of_one_module() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub fn make(v: i64) -> biz::Amount { return biz::amount(v); }
",
            ),
            (
                "books.wi",
                "import sales;

pub fn twice(a: sales::Amount) -> i64 { return a.doubled(); }
",
            ),
            (
                "app.wi",
                "import ledger;
import books;
import sales as market;

fn main() {
    println(books::twice(ledger::make(6)));
    println(market::amount(2).value);
}
",
            ),
        ],
        "12\n2\n",
    );
}

// 16. The entry never names the aliased module at all: it is in the program
//     only because `ledger` imports it, under a name only `ledger` writes.
#[test]
fn mai_16_the_entry_never_names_the_aliased_module() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub fn value(v: i64) -> i64 { return biz::amount(v).doubled(); }
",
            ),
            (
                "app.wi",
                "import ledger;\n\nfn main() { println(ledger::value(4)); }\n",
            ),
        ],
        "8\n",
    );
}

// 17. The chain reaches into the entry: a local subclass of a module class
//     whose own base is the aliased module's class, so the layout spans three
//     files and two spellings.
#[test]
fn mai_17_an_entry_subclass_over_the_aliased_base() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub open class Bonus extends biz::Amount {
    pub open override fn doubled(self) -> i64 { return self.value * 3; }
}
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;
import ledger::Bonus;

class Big extends Bonus {
    pub override fn doubled(self) -> i64 { return self.value * 10; }
}

fn main() {
    let b = new Big(5);
    let a: market::Amount = b;
    println(a.doubled());
    println(b.value);
}
",
            ),
        ],
        "50\n5\n",
    );
}

// 18. A `Map` whose VALUE type is the aliased class: the generic's argument is
//     translated, its head is not.
#[test]
fn mai_18_a_map_valued_by_the_aliased_class() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;
import std::collections::Map;

pub fn ledger_of(v: i64) -> Map<String, biz::Amount> {
    let m: Map<String, biz::Amount> = Map::new();
    m.insert(\"one\", biz::amount(v));
    return m;
}
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;
import std::collections::Map;

fn main() {
    let m: Map<String, market::Amount> = ledger::ledger_of(8);
    println(m.len());
}
",
            ),
        ],
        "1\n",
    );
}

// 19. Two modules that each declare a class called `Amount`. Collapsing
//     spellings must not collapse these: they are two classes and stay two.
#[test]
fn mai_19_two_modules_declare_the_same_class_name() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "other.wi",
                "pub class Amount {
    pub tag: i64;
}

pub fn make(t: i64) -> Amount { return new Amount(t); }
",
            ),
            (
                "ledger.wi",
                "import sales as biz;

pub fn make(v: i64) -> biz::Amount { return biz::amount(v); }
",
            ),
            (
                "app.wi",
                "import sales as market;
import other;
import ledger;

fn main() {
    let a: market::Amount = ledger::make(3);
    println(a.value);
    println(other::make(9).tag);
}
",
            ),
        ],
        "3\n9\n",
    );
}

// 20. One interface, two implementors on opposite sides of the boundary: the
//     module boxes its own class and the entry's, through the parameter type it
//     wrote under its own spelling.
#[test]
fn mai_20_the_module_boxes_both_implementors() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub class Fee implements biz::Priced {
    pub n: i64;
    pub fn price(self) -> i64 { return self.n; }
}

pub fn charge(p: biz::Priced) -> i64 { return p.price() * 2; }
pub fn make(n: i64) -> Fee { return new Fee(n); }
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;

class Flat implements market::Priced {
    pub fn price(self) -> i64 { return 7; }
}

fn main() {
    println(ledger::charge(ledger::make(3)));
    println(ledger::charge(new Flat()));
}
",
            ),
        ],
        "6\n14\n",
    );
}

// 21. A class METHOD's parameter and return, which are qualified by a different
//     pass than a free function's.
#[test]
fn mai_21_a_method_signature_across_spellings() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub class Book {
    pub fee: i64;

    pub fn total(self, a: biz::Amount) -> biz::Amount { return biz::amount(a.value + self.fee); }
}

pub fn make(fee: i64) -> Book { return new Book(fee); }
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;

fn main() {
    let b = ledger::make(4);
    let a: market::Amount = b.total(market::amount(6));
    println(a.value);
}
",
            ),
        ],
        "10\n",
    );
}

// 22. An enum PAYLOAD typed by the aliased class. Payload types were prefixed
//     with the module's CANONICAL path regardless of what the module wrote, so
//     they named a class no table here holds.
#[test]
fn mai_22_an_enum_payload_typed_by_the_aliased_class() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub enum Slot { Empty, Filled(biz::Amount) }

pub fn slot(v: i64) -> Slot { return Slot::Filled(biz::amount(v)); }
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;

fn main() {
    match ledger::slot(5) {
        ledger::Slot::Filled(a) => println(a.doubled()),
        ledger::Slot::Empty => println(0),
    }
}
",
            ),
        ],
        "10\n",
    );
}

// 23. Every hop renames every module: `books` calls `ledger` `led` and `sales`
//     `store`, the entry calls `books` `shelf` and `sales` `market`.
#[test]
fn mai_23_a_chain_that_renames_at_every_hop() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub fn make(v: i64) -> biz::Amount { return biz::amount(v); }
",
            ),
            (
                "books.wi",
                "import ledger as led;
import sales as store;

pub fn twice(v: i64) -> store::Amount { return led::make(v * 2); }
",
            ),
            (
                "app.wi",
                "import books as shelf;
import sales as market;

fn main() {
    let a: market::Amount = shelf::twice(3);
    println(a.doubled());
}
",
            ),
        ],
        "12\n",
    );
}

// 24. An entry class whose name collides with the imported one. Binding a
//     second spelling must not bind a bare name: `Amount` is the entry's own.
#[test]
fn mai_24_an_entry_class_of_the_same_name() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub fn make(v: i64) -> biz::Amount { return biz::amount(v); }
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;

class Amount {
    pub tag: i64;
}

fn main() {
    let mine = new Amount(4);
    let theirs: market::Amount = ledger::make(5);
    println(mine.tag);
    println(theirs.value);
}
",
            ),
        ],
        "4\n5\n",
    );
}

// 25. Control: a spelling no file in the build imports is still unknown.
#[test]
fn mai_25_an_unimported_spelling_is_unknown() {
    let stderr = project_error(&[
        ("sales.wi", SALES),
        (
            "ledger.wi",
            "import sales as biz;

pub fn make(v: i64) -> biz::Amount { return biz::amount(v); }
",
        ),
        (
            "app.wi",
            "import ledger;
import sales as market;

fn main() { println(nowhere::amount(2).value); }
",
        ),
    ]);
    assert!(
        stderr.contains("E0350") && stderr.contains("unknown name `nowhere`"),
        "expected an unimported spelling to stay unknown:\n{stderr}"
    );
}

// 26. Control: the alias binds only what the module declares. A type it has no
//     name for is not conjured by the second spelling.
#[test]
fn mai_26_an_unknown_type_in_a_known_module() {
    let stderr = project_error(&[
        ("sales.wi", SALES),
        (
            "ledger.wi",
            "import sales as biz;

pub fn make(v: i64) -> biz::Amount { return biz::amount(v); }
",
        ),
        (
            "app.wi",
            "import ledger;
import sales as market;

fn main() {
    let a: market::Missing = ledger::make(2);
    println(a.value);
}
",
        ),
    ]);
    assert!(
        stderr.contains("cannot find type `market::Missing`"),
        "expected the missing type to be reported under the entry's spelling:\n{stderr}"
    );
}

// 27. Control: two modules that each declare `Grade` still declare two enums,
//     and a pattern from one does not match a scrutinee from the other -- named
//     by the enum's build-wide identity.
#[test]
fn mai_27_another_modules_enum_of_the_same_name_does_not_match() {
    let stderr = project_error(&[
        ("sales.wi", SALES),
        (
            "other.wi",
            "pub enum Grade { Low, High }

pub fn grade() -> Grade { return Grade::Low; }
",
        ),
        (
            "ledger.wi",
            "import sales as biz;

pub fn top(v: i64) -> biz::Grade { return biz::grade(v); }
",
        ),
        (
            "app.wi",
            "import sales as market;
import other;
import ledger;

fn main() {
    match ledger::top(50) {
        other::Grade::High => println(\"high\"),
        other::Grade::Low => println(\"low\"),
    }
}
",
        ),
    ]);
    assert!(
        stderr.contains("E1205") && stderr.contains("scrutinee of type `sales::Grade`"),
        "expected the two `Grade` enums to stay distinct:\n{stderr}"
    );
}

// 28. Control: an unrelated class is still a type error, and the expected type
//     is now named by the spelling the ENTRY writes rather than by the private
//     alias of whichever module happened to import the module first.
#[test]
fn mai_28_an_unrelated_class_is_still_a_type_error() {
    let stderr = project_error(&[
        ("sales.wi", SALES),
        (
            "ledger.wi",
            "import sales as biz;

pub fn twice(a: biz::Amount) -> i64 { return a.doubled(); }
",
        ),
        (
            "app.wi",
            "import sales as market;
import ledger;

class Other {
    pub value: i64;
}

fn main() { println(ledger::twice(new Other(2))); }
",
        ),
    ]);
    assert!(
        stderr.contains("expected `market::Amount`, found `Other`"),
        "expected the mismatch to name the entry's own spelling:\n{stderr}"
    );
}

// 29. `--release` compiles the same program: the translation happens during
//     registration, which both build modes share.
#[test]
fn mai_29_the_chain_compiles_in_release() {
    let project = TestProject::new(
        "alias_identity_release",
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub class Fee implements biz::Priced {
    pub n: i64;
    pub fn price(self) -> i64 { return self.n; }
}

pub fn charge(p: biz::Priced) -> i64 { return p.price() * 2; }
pub fn make(v: i64) -> biz::Amount { return biz::amount(v); }
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;

fn main() {
    let a: market::Amount = ledger::make(6);
    println(a.doubled());
    println(ledger::charge(new ledger::Fee(4)));
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
    assert_eq!(String::from_utf8_lossy(&out.stdout), "12\n8\n");
}

// 30. Under GC stress: the collector walks the layout of a class allocated
//     through one spelling and read through the other, so the two had better be
//     one layout.
#[test]
fn mai_30_the_aliased_class_under_gc_stress() {
    let project = TestProject::new(
        "alias_identity_gc",
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales as biz;

pub class Line {
    pub a: biz::Amount;
}

pub fn churn(v: i64) -> Line {
    let mut i = 0;
    let mut last = new Line(biz::amount(v));
    while i < 200 {
        last = new Line(biz::amount(v));
        i = i + 1;
    }
    return last;
}
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;

fn main() {
    let l = ledger::churn(9);
    let a: market::Amount = l.a;
    println(a.doubled());
}
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
    assert_eq!(String::from_utf8_lossy(&out.stdout), "18\n");
}

// 31. Construction, which resolves a class by name on a different path than an
//     annotation does: `new market::Amount(..)` in the entry was
//     `error[E0844] unknown class market::Amount` whenever another unit reached
//     the module first -- here by an item import, whose own subclass is
//     constructed under both the module's and the entry's names (willow-mxnh).
#[test]
fn mai_31_new_through_the_entrys_alias_over_an_item_imported_base() {
    assert_project(
        &[
            ("sales.wi", SALES),
            (
                "ledger.wi",
                "import sales::Amount;

pub class Bonus extends Amount {
    pub override fn doubled(self) -> i64 { return self.value * 3; }
}

pub fn make(v: i64) -> Bonus { return new Bonus(v); }
",
            ),
            (
                "app.wi",
                "import sales as market;
import ledger;

fn main() {
    println(new market::Amount(2).doubled());
    println(ledger::make(4).doubled());
    println(new ledger::Bonus(5).doubled());
}
",
            ),
        ],
        "4\n12\n15\n",
    );
}

// A source alias may have the same spelling as this module's registered name.
// Translation must not revisit identities introduced while qualifying Own.
#[test]
fn mai_32_an_import_alias_does_not_rewrite_local_types() {
    assert_project(
        &[
            ("dep.wi", "pub class Other { pub x: i64; }"),
            (
                "outer.wi",
                "import dep as outer;
pub class Own { pub x: i64; }
pub class Holder { pub own: Own; }
pub fn own() -> Own { return new Own(7); }",
            ),
            (
                "app.wi",
                "import dep;
import outer;
fn main() {
    println(outer::own().x);
    let h = new outer::Holder(new outer::Own(8));
    println(h.own.x);
}",
            ),
        ],
        "7\n8\n",
    );
}
