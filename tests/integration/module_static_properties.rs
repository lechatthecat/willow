//! A module class's static properties (willow-6xgo).
//!
//! `static [mut] name: T = expr` gets one global slot per (class, property),
//! declared under the class's REGISTERED name -- `counting::Counter` for a
//! module class. Two halves of that were wrong for a module:
//!
//!   * the slot table was a plain map, so a module body spelling its own class
//!     bare (`Counter::made`, which is all a module body can write) found
//!     nothing: the read fell through to a zero and the store went nowhere, or,
//!     once module bodies started lowering to LIR, the walker refused the
//!     function outright;
//!   * the INITIALIZER was replayed from the entry program's phase, where none
//!     of the module's names mean anything: `new Slot(1)` had no layout,
//!     `seed()` was not the module's function, and `Holder::base` was not the
//!     module's storage.
//!
//! The table is now keyed by class through the shared type scope, like every
//! other class table, so a unit's aliases (its own bare names, its item
//! imports, its module spellings) reach the declaration's slot. And each module
//! compiles its own initializers into a private function during its BODY phase,
//! under its own aliases; `__willow_static_init` calls those in declaration
//! order before replaying the entry program's items.
//!
//! 36 perspectives:
//!   1 a module function bumps its own class's static
//!   2 the entry reads it as `module::Class::prop`
//!   3 the entry writes it
//!   4 an instance method of the module class
//!   5 a static method of the module class
//!   6 the LIR walker compiles those bodies
//!   7 `--release`
//!   8 an item import: `import counting::Counter;`
//!   9 the entry aliases the module
//!  10 an alias chain: one spelling in a module, another in the entry
//!  11 String, f64, bool and `Array` properties
//!  12 a class-typed property, whose initializer allocates
//!  13 ...under allocation-stress GC
//!  14 ...under minor-collection stress
//!  15 a String property rewritten in a loop under GC stress
//!  16 `--release` with a class-typed property
//!  17 an interface-typed property
//!  18 an `Array<String>` property two files share
//!  19 inherited: a subclass reaches its base's property, `Self::` included
//!  20 an enum-typed property
//!  21 an initializer that calls one of the module's own functions
//!  22 an initializer that reads a sibling property of the same class
//!  23 initializer order across two modules and the entry
//!  24 two modules declaring the same class name keep separate slots
//!  25 an entry class of the same name keeps its own
//!  26 another module reads and writes it
//!  27 a class method of another module writes it
//!  28 a lambda in the entry reads it
//!  29 a lambda in the module writes it
//!  30 a `defer` block in the module writes it
//!  31 an `async` module function writes it
//!  32 the entry writes it through an item import
//!  33 the example project
//!  34 enum initialization through a module alias
//!  35 payload enums through an item import, in debug and release with GC stress
//!  36 user functions cannot collide with generated module initializers

use super::support::{
    TestProject, compile_temp_project_and_run, compile_temp_project_with_env_stderr,
};

/// The module most perspectives import: one counter, reached from a free
/// function, an instance method and a static method.
const COUNTING: &str = "pub class Counter {
    pub static mut made: i64 = 0;
    pub static limit: i64 = 100;

    pub static fn bump_static(n: i64) -> i64 {
        Counter::made = Counter::made + n;
        return Counter::made;
    }

    pub fn tick(self) -> i64 {
        Self::made = Self::made + 1;
        return Self::made;
    }
}

pub fn bump(n: i64) -> i64 {
    Counter::made = Counter::made + n;
    return Counter::made;
}

pub fn made() -> i64 { return Counter::made; }
";

/// A module whose static property holds a class value, so its initializer
/// allocates and its slot is a GC root.
const HOLDER: &str = "pub class Slot {
    pub v: i64;
}

pub class Holder {
    pub static mut last: Slot = new Slot(1);
}

pub fn stash(v: i64) -> i64 {
    Holder::last = new Slot(v);
    return Holder::last.v;
}

pub fn churn(n: i64) -> i64 {
    let mut i = 0;
    while i < n {
        Holder::last = new Slot(i);
        i = i + 1;
    }
    return Holder::last.v;
}

pub fn last() -> i64 { return Holder::last.v; }
";

/// Compile and run a project expected to succeed, asserting its stdout.
fn assert_project(files: &[(&str, &str)], expected: &str) {
    let (output, ok) = compile_temp_project_and_run(files, "main.wi");
    assert!(ok, "expected the project to compile:\n{output}");
    assert_eq!(output, expected);
}

/// A project of the shared counting module plus one entry file.
fn counting_project(main: &str) -> [(&str, &str); 2] {
    [("counting.wi", COUNTING), ("main.wi", main)]
}

// 1. The bead's own repro. Every name in `bump` is the module's: the class is
//    spelled bare because that is the only spelling a module body has.
#[test]
fn msp_01_a_module_function_bumps_its_own_static() {
    assert_project(
        &counting_project(
            "import counting;

fn main() {
    println(counting::bump(3));
    println(counting::bump(4));
    println(counting::made());
}
",
        ),
        "3\n7\n7\n",
    );
}

// 2. The entry's own spelling of the same slot, which always worked, next to a
//    module write that did not: both must land on one slot.
#[test]
fn msp_02_the_entry_reads_the_qualified_property() {
    assert_project(
        &counting_project(
            "import counting;

fn main() {
    println(counting::bump(3));
    println(counting::Counter::made);
    println(counting::Counter::limit);
}
",
        ),
        "3\n3\n100\n",
    );
}

// 3. The entry writes and the module reads: the store has to be visible across
//    the unit boundary, in both directions.
#[test]
fn msp_03_the_entry_writes_the_property() {
    assert_project(
        &counting_project(
            "import counting;

fn main() {
    counting::Counter::made = 9;
    println(counting::made());
    println(counting::bump(1));
}
",
        ),
        "9\n10\n",
    );
}

// 4. An instance method reaching the property through `Self::`, which resolves
//    by a different route than a bare class name.
#[test]
fn msp_04_an_instance_method_of_the_module_class() {
    assert_project(
        &counting_project(
            "import counting;

fn main() {
    let c = new counting::Counter();
    println(c.tick());
    println(c.tick());
    println(counting::made());
}
",
        ),
        "1\n2\n2\n",
    );
}

// 5. A static method, called from the entry through the module's name.
#[test]
fn msp_05_a_static_method_of_the_module_class() {
    assert_project(
        &counting_project(
            "import counting;

fn main() {
    println(counting::Counter::bump_static(4));
    println(counting::made());
}
",
        ),
        "4\n4\n",
    );
}

// 6. The walker's eligibility probe is the same lookup the emitter uses, so a
//    resolvable static is also a lowerable one: these bodies must come from
//    lowered IR rather than fall back to the AST emitter.
#[test]
fn msp_06_the_walker_compiles_the_module_bodies() {
    let files = counting_project(
        "import counting;

fn main() { println(counting::bump(3)); }
",
    );
    let (ok, stderr) =
        compile_temp_project_with_env_stderr(&files, "main.wi", &[("WILLOW_LIR_LOG", "1")]);
    assert!(ok, "compile failed under the walker:\n{stderr}");
    for function in ["counting.bump", "counting.made", "counting::Counter::tick"] {
        let line = format!("compiling `{function}` from lowered IR");
        assert!(
            stderr.contains(&line),
            "`{function}` did not come from the walker:\n{stderr}"
        );
    }
}

// 7. `--release`: the storage is declared in the declaration phase, which both
//    build modes share.
#[test]
fn msp_07_release_keeps_the_property() {
    let project = TestProject::new(
        "static_properties_release",
        &counting_project(
            "import counting;

fn main() {
    println(counting::bump(3));
    println(counting::made());
}
",
        ),
    );
    let compiled = project.compile_release("main.wi");
    assert!(
        compiled.status.success(),
        "expected the release build to succeed:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&project.run().stdout), "3\n3\n");
}

// 8. An item import binds the class under a bare local name in the ENTRY, which
//    is the other way a spelling reaches the same slot.
#[test]
fn msp_08_an_item_import_reaches_the_same_slot() {
    assert_project(
        &counting_project(
            "import counting::Counter;

fn main() {
    println(Counter::made);
    Counter::made = 4;
    println(Counter::made);
}
",
        ),
        "0\n4\n",
    );
}

// 9. The entry names the module by an alias of its own.
#[test]
fn msp_09_the_entry_aliases_the_module() {
    assert_project(
        &counting_project(
            "import counting as c;

fn main() {
    println(c::bump(2));
    println(c::Counter::made);
}
",
        ),
        "2\n2\n",
    );
}

// 10. Three spellings of one module: `base` inside a module, `tally` in the
//     entry, `counting` in the graph.
#[test]
fn msp_10_an_alias_chain_reaches_one_slot() {
    assert_project(
        &[
            ("counting.wi", COUNTING),
            (
                "mid.wi",
                "import counting as base;

pub fn bump(n: i64) -> i64 { return base::Counter::bump_static(n); }
",
            ),
            (
                "main.wi",
                "import mid;
import counting as tally;

fn main() {
    println(mid::bump(2));
    println(tally::Counter::made);
}
",
            ),
        ],
        "2\n2\n",
    );
}

// 11. Every scalar kind plus a collection: each slot holds 8 bytes, and the
//     `Array` one is a GC-managed root that the module mutates in place.
#[test]
fn msp_11_string_float_bool_and_array_properties() {
    assert_project(
        &[
            (
                "bank.wi",
                "import std::collections::Array;

pub class Bank {
    pub static mut name: String = \"vault\";
    pub static mut rate: f64 = 1.5;
    pub static mut open_now: bool = true;
    pub static mut tags: Array<i64> = [1, 2, 3];
}

pub fn rename(n: String) -> String { Bank::name = n; return Bank::name; }
pub fn scale(f: f64) -> f64 { Bank::rate = Bank::rate * f; return Bank::rate; }
pub fn toggle() -> bool { Bank::open_now = !Bank::open_now; return Bank::open_now; }
pub fn push(v: i64) -> i64 { Bank::tags.push(v); return Bank::tags.len(); }
",
            ),
            (
                "main.wi",
                "import bank;

fn main() {
    println(bank::rename(\"main\"));
    println(bank::scale(2.0));
    println(bank::toggle());
    println(bank::push(4));
    println(bank::Bank::name);
}
",
            ),
        ],
        "main\n3\nfalse\n4\nmain\n",
    );
}

// 12. A class-typed property. Its initializer allocates, and it allocates a
//     class only the module can name -- the case that panicked with `checked
//     class \`Slot\` has no object layout` when the entry's phase emitted it.
#[test]
fn msp_12_a_class_typed_property() {
    assert_project(
        &[
            ("h.wi", HOLDER),
            (
                "main.wi",
                "import h;

fn main() {
    println(h::last());
    println(h::stash(5));
    println(h::churn(50));
}
",
            ),
        ],
        "1\n5\n49\n",
    );
}

// 13. The same property under allocation stress: the slot is a permanent GC
//     root, so the object it holds survives a collection between writes.
#[test]
fn msp_13_a_class_typed_property_under_alloc_stress() {
    let project = TestProject::new(
        "static_properties_gc",
        &[
            ("h.wi", HOLDER),
            (
                "main.wi",
                "import h;

fn main() {
    println(h::churn(200));
    println(h::last());
}
",
            ),
        ],
    );
    let compiled = project.compile("main.wi");
    assert!(
        compiled.status.success(),
        "expected the project to compile:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let out = project.run_with_env(&[("WILLOW_GC_STRESS", "alloc")]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "199\n199\n");
}

// 14. Minor-collection stress, which promotes rather than reclaims: the root
//     has to be updated, not just traced.
#[test]
fn msp_14_a_class_typed_property_under_minor_stress() {
    let project = TestProject::new(
        "static_properties_minor",
        &[
            ("h.wi", HOLDER),
            (
                "main.wi",
                "import h;

fn main() { println(h::churn(300)); }
",
            ),
        ],
    );
    let compiled = project.compile("main.wi");
    assert!(
        compiled.status.success(),
        "expected the project to compile:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let out = project.run_with_env(&[("WILLOW_GC_STRESS", "minor")]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "299\n");
}

// 15. A String property rewritten in a loop under the same stress: strings are
//     heap values too, and the slot is the only thing keeping the last one.
#[test]
fn msp_15_a_string_property_under_gc_stress() {
    let project = TestProject::new(
        "static_properties_string_gc",
        &[
            (
                "h.wi",
                "pub class Cfg {
    pub static mut name: String = \"start\";
}

pub fn set(n: String) -> String { Cfg::name = n; return Cfg::name; }

pub fn churn(n: i64) -> String {
    let mut i = 0;
    while i < n {
        Cfg::name = \"x\" + i.toString();
        i = i + 1;
    }
    return Cfg::name;
}
",
            ),
            (
                "main.wi",
                "import h;

fn main() {
    println(h::Cfg::name);
    println(h::set(\"mid\"));
    println(h::churn(100));
}
",
            ),
        ],
    );
    let compiled = project.compile("main.wi");
    assert!(
        compiled.status.success(),
        "expected the project to compile:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let out = project.run_with_env(&[("WILLOW_GC_STRESS", "alloc")]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "start\nmid\nx99\n");
}

// 16. A class-typed property in a release build, where the initializer's
//     allocation is emitted without any debug instrumentation.
#[test]
fn msp_16_release_with_a_class_typed_property() {
    let project = TestProject::new(
        "static_properties_release_class",
        &[
            (
                "h.wi",
                "pub class Slot { pub v: i64; }

pub class Holder {
    pub static mut last: Slot = new Slot(3);
}

pub fn last() -> i64 { return Holder::last.v; }
",
            ),
            (
                "main.wi",
                "import h;

fn main() { println(h::last()); }
",
            ),
        ],
    );
    let compiled = project.compile_release("main.wi");
    assert!(
        compiled.status.success(),
        "expected the release build to succeed:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&project.run().stdout), "3\n");
}

// 17. An interface-typed property: the initializer boxes a class into an
//     interface, which needs the module's vtable for the pair.
#[test]
fn msp_17_an_interface_typed_property() {
    assert_project(
        &[
            (
                "h.wi",
                "pub interface Priced {
    fn price(self) -> i64;
}

pub class Fee implements Priced {
    pub n: i64;
    pub fn price(self) -> i64 { return self.n; }
}

pub class Book {
    pub static mut current: Priced = new Fee(4);
}

pub fn price() -> i64 { return Book::current.price(); }
pub fn set(n: i64) -> i64 { Book::current = new Fee(n); return Book::current.price(); }
",
            ),
            (
                "main.wi",
                "import h;

fn main() {
    println(h::price());
    println(h::set(9));
}
",
            ),
        ],
        "4\n9\n",
    );
}

// 18. One `Array<String>` property, written by its own module and read by
//     another: a single slot shared by two units.
#[test]
fn msp_18_an_array_property_two_modules_share() {
    assert_project(
        &[
            (
                "h.wi",
                "import std::collections::Array;

pub class Log {
    pub static mut lines: Array<String> = [\"a\"];
}

pub fn add(s: String) -> i64 { Log::lines.push(s); return Log::lines.len(); }
",
            ),
            (
                "reader.wi",
                "import h;

pub fn count() -> i64 { return h::Log::lines.len(); }
pub fn first() -> String { return h::Log::lines[0]; }
",
            ),
            (
                "main.wi",
                "import h;
import reader;

fn main() {
    println(h::add(\"b\"));
    println(reader::count());
    println(reader::first());
}
",
            ),
        ],
        "2\n2\na\n",
    );
}

// 19. Inheritance: a subclass has no storage of its own, so `Sub::hits` and
//     `Self::hits` in an inherited method both walk to the base's slot.
#[test]
fn msp_19_an_inherited_property() {
    assert_project(
        &[
            (
                "inh.wi",
                "pub open class Base {
    pub static mut hits: i64 = 0;

    pub open fn hit(self) -> i64 { Self::hits = Self::hits + 1; return Self::hits; }
}

pub class Sub extends Base {
    pub fn twice(self) -> i64 { self.hit(); return self.hit(); }
}

pub fn via_sub() -> i64 { return Sub::hits; }
pub fn bump() -> i64 { Base::hits = Base::hits + 1; return Base::hits; }
",
            ),
            (
                "main.wi",
                "import inh;

fn main() {
    println(inh::bump());
    let s = new inh::Sub();
    println(s.twice());
    println(inh::via_sub());
    println(inh::Base::hits);
}
",
            ),
        ],
        "1\n3\n3\n3\n",
    );
}

// 20. An enum-typed property, matched on both sides of the boundary.
#[test]
fn msp_20_an_enum_typed_property() {
    assert_project(
        &[
            (
                "e.wi",
                "pub enum Grade { Low, High }

pub class Board {
    pub static mut best: Grade = Grade::Low;
}

pub fn raise() -> Grade { Board::best = Grade::High; return Board::best; }
pub fn best() -> Grade { return Board::best; }
",
            ),
            (
                "main.wi",
                "import e;

fn main() {
    match e::best() {
        e::Grade::Low => println(\"low\"),
        e::Grade::High => println(\"high\"),
    }
    e::raise();
    match e::best() {
        e::Grade::Low => println(\"low\"),
        e::Grade::High => println(\"high\"),
    }
}
",
            ),
        ],
        "low\nhigh\n",
    );
}

// 21. An initializer that CALLS one of the module's own functions. Replayed
//     from the entry's phase it resolved to nothing and stored a zero.
#[test]
fn msp_21_an_initializer_that_calls_a_module_function() {
    assert_project(
        &[
            (
                "h.wi",
                "pub fn seed() -> i64 { return 7; }

pub class Holder {
    pub static mut v: i64 = seed();
}

pub fn read() -> i64 { return Holder::v; }
",
            ),
            (
                "main.wi",
                "import h;

fn main() { println(h::read()); }
",
            ),
        ],
        "7\n",
    );
}

// 22. An initializer that reads a SIBLING property of the same class, in
//     declaration order: the read is the same bare spelling the store is.
#[test]
fn msp_22_an_initializer_that_reads_a_sibling_property() {
    assert_project(
        &[
            (
                "h.wi",
                "pub class Holder {
    pub static base: i64 = 5;
    pub static mut doubled: i64 = Holder::base * 2;
}

pub fn read() -> i64 { return Holder::doubled; }
",
            ),
            (
                "main.wi",
                "import h;

fn main() {
    println(h::read());
    println(h::Holder::base);
}
",
            ),
        ],
        "10\n5\n",
    );
}

// 23. Order across units: a module initializer may read one from a module it
//     imports, and the entry's own items still come last.
#[test]
fn msp_23_initializer_order_across_modules() {
    assert_project(
        &[
            (
                "a.wi",
                "pub class A {
    pub static v: i64 = 2;
}

pub fn v() -> i64 { return A::v; }
",
            ),
            (
                "b.wi",
                "import a;

pub class B {
    pub static v: i64 = a::A::v * 5;
}

pub fn v() -> i64 { return B::v; }
",
            ),
            (
                "main.wi",
                "import a;
import b;

class C {
    pub static v: i64 = 1;
}

fn main() {
    println(a::v());
    println(b::v());
    println(C::v);
}
",
            ),
        ],
        "2\n10\n1\n",
    );
}

// 24. Two modules that each declare `Counter::made`: one slot each, and the
//     bare spelling inside each module must reach its own.
#[test]
fn msp_24_two_modules_of_the_same_class_name() {
    assert_project(
        &[
            (
                "a.wi",
                "pub class Counter { pub static mut made: i64 = 0; }

pub fn bump(n: i64) -> i64 { Counter::made = Counter::made + n; return Counter::made; }
",
            ),
            (
                "b.wi",
                "pub class Counter { pub static mut made: i64 = 100; }

pub fn bump(n: i64) -> i64 { Counter::made = Counter::made + n; return Counter::made; }
",
            ),
            (
                "main.wi",
                "import a;
import b;

fn main() {
    println(a::bump(1));
    println(b::bump(1));
    println(a::Counter::made);
    println(b::Counter::made);
}
",
            ),
        ],
        "1\n101\n1\n101\n",
    );
}

// 25. An entry class of the same name has its own slot, and binding the
//     module's bare spelling for the module's bodies must not reach it.
#[test]
fn msp_25_an_entry_class_of_the_same_name() {
    assert_project(
        &counting_project(
            "import counting;

class Counter {
    pub static mut made: i64 = 50;
}

fn main() {
    println(counting::bump(1));
    println(Counter::made);
    println(counting::Counter::made);
}
",
        ),
        "1\n50\n1\n",
    );
}

// 26. Another module reads and writes the property, which is a third naming
//     context again: `counting::Counter` written by a unit that is not the
//     entry.
#[test]
fn msp_26_another_module_reads_and_writes_it() {
    assert_project(
        &[
            (
                "counting.wi",
                "pub class Counter { pub static mut made: i64 = 0; }

pub fn bump(n: i64) -> i64 { Counter::made = Counter::made + n; return Counter::made; }
",
            ),
            (
                "report.wi",
                "import counting;

pub fn read() -> i64 { return counting::Counter::made; }
pub fn add(n: i64) -> i64 {
    counting::Counter::made = counting::Counter::made + n;
    return counting::Counter::made;
}
",
            ),
            (
                "main.wi",
                "import counting;
import report;

fn main() {
    println(counting::bump(2));
    println(report::read());
    println(report::add(3));
    println(counting::Counter::made);
}
",
            ),
        ],
        "2\n2\n5\n5\n",
    );
}

// 27. The same from a class METHOD of another module, whose body is compiled
//     in yet another pass.
#[test]
fn msp_27_a_class_method_of_another_module_writes_it() {
    assert_project(
        &[
            (
                "h.wi",
                "pub class Counter { pub static mut made: i64 = 0; }

pub fn bump(n: i64) -> i64 { Counter::made = Counter::made + n; return Counter::made; }
",
            ),
            (
                "svc.wi",
                "import h;

pub class Service {
    pub fn work(self, n: i64) -> i64 {
        h::Counter::made = h::Counter::made + n;
        return h::Counter::made;
    }
}
",
            ),
            (
                "main.wi",
                "import h;
import svc;

fn main() {
    println(h::bump(1));
    println(new svc::Service().work(2));
    println(h::Counter::made);
}
",
            ),
        ],
        "1\n3\n3\n",
    );
}

// 28. A lambda in the ENTRY: its body is compiled before the entry's own
//     functions, under the entry's aliases.
#[test]
fn msp_28_a_lambda_in_the_entry_reads_it() {
    assert_project(
        &counting_project(
            "import counting;

fn main() {
    let f = || -> i64 { return counting::Counter::made; };
    counting::bump(6);
    println(f());
}
",
        ),
        "6\n",
    );
}

// 29. A lambda in the MODULE, compiled first in that module's body phase and
//     so the earliest thing that needs the aliases installed.
#[test]
fn msp_29_a_lambda_in_the_module_writes_it() {
    assert_project(
        &[
            (
                "c.wi",
                "pub class Counter { pub static mut made: i64 = 0; }

pub fn made() -> i64 { return Counter::made; }

pub fn lazy() -> i64 {
    let f = || -> i64 { Counter::made = Counter::made + 2; return Counter::made; };
    return f();
}
",
            ),
            (
                "main.wi",
                "import c;

fn main() {
    println(c::lazy());
    println(c::made());
}
",
            ),
        ],
        "2\n2\n",
    );
}

// 30. A `defer` block, whose statements are emitted at every exit of the
//     function rather than where they are written.
#[test]
fn msp_30_a_defer_block_in_the_module_writes_it() {
    assert_project(
        &[
            (
                "c.wi",
                "pub class Counter { pub static mut made: i64 = 0; }

pub fn made() -> i64 { return Counter::made; }

pub fn scoped() -> i64 {
    defer { Counter::made = Counter::made + 100; }
    Counter::made = 1;
    return Counter::made;
}
",
            ),
            (
                "main.wi",
                "import c;

fn main() {
    println(c::scoped());
    println(c::made());
}
",
            ),
        ],
        "1\n101\n",
    );
}

// 31. An `async` module function, lowered into a poll function whose body is
//     emitted through the coroutine path.
#[test]
fn msp_31_an_async_module_function_writes_it() {
    assert_project(
        &[
            (
                "c.wi",
                "pub class Counter { pub static mut made: i64 = 0; }

pub fn made() -> i64 { return Counter::made; }

pub async fn bump_async(n: i64) -> i64 {
    Counter::made = Counter::made + n;
    return Counter::made;
}
",
            ),
            (
                "main.wi",
                "import c;

async fn main() {
    println(await c::bump_async(3));
    println(c::made());
}
",
            ),
        ],
        "3\n3\n",
    );
}

// 32. A write through an item-imported class name, read back through a second
//     entry function so the store is not folded into the read.
#[test]
fn msp_32_the_entry_writes_through_an_item_import() {
    assert_project(
        &counting_project(
            "import counting::Counter;

fn read() -> i64 { return Counter::made; }

fn main() {
    Counter::made = 8;
    println(read());
}
",
        ),
        "8\n",
    );
}

// 33. The runnable example, compiled from the same sources the example
//     directory holds.
#[test]
fn msp_33_the_example_project() {
    assert_project(
        &[
            (
                "counting.wi",
                include_str!("../../example/module_static_properties/counting.wi"),
            ),
            (
                "registry.wi",
                include_str!("../../example/module_static_properties/registry.wi"),
            ),
            (
                "main.wi",
                include_str!("../../example/module_static_properties/main.wi"),
            ),
        ],
        "1\n2\n8\nvault\n2\nledger\n4\ntrue\n",
    );
}

/// The initializer must use the module's canonical enum even when its graph
/// name is an alias (willow-wvlw). Read before any assignment can hide tag zero.
#[test]
fn msp_34_an_aliased_modules_enum_initializer() {
    assert_project(
        &[
            (
                "h.wi",
                "pub enum Grade { Low, High }
pub class Board { pub static mut best: Grade = Grade::High; }
pub fn best() -> Grade { return Board::best; }
pub fn is_high(g: Grade) -> bool {
    match g { Grade::High => { return true; } Grade::Low => { return false; } }
}",
            ),
            (
                "main.wi",
                "import h as g;
fn main() { println(g::is_high(g::best())); }",
            ),
        ],
        "true\n",
    );
}

#[test]
fn msp_35_aliased_payload_enum_initializers() {
    let files = [
        (
            "h.wi",
            "pub enum State { Empty, Ready(String) }
pub class Board { pub static best: State = State::Ready(\"ready\"); }
pub fn read() -> String {
    match Board::best {
        State::Empty => { return \"empty\"; }
        State::Ready(s) => { return s; }
    }
}",
        ),
        (
            "main.wi",
            "import h::Board;
import h as g;
fn main() {
    println(g::read());
    match Board::best {
        g::State::Empty => println(\"empty\"),
        g::State::Ready(s) => println(s),
    }
}",
        ),
    ];
    for release in [false, true] {
        let project = TestProject::new("msp_35", &files);
        let compiled = if release {
            project.compile_release("main.wi")
        } else {
            project.compile("main.wi")
        };
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let output = project.run_with_env(&[("WILLOW_GC_STRESS", "1")]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "ready\nready\n");
    }
}

#[test]
fn msp_36_user_static_init_functions_do_not_collide_with_generated_initializers() {
    assert_project(
        &[
            (
                "m.wi",
                "pub fn __static_init() -> i64 { return 5; }
pub class C { pub static x: i64 = 7; }",
            ),
            (
                "main.wi",
                "import m;
fn main() { println(m::__static_init()); println(m::C::x); }",
            ),
        ],
        "5\n7\n",
    );
}
