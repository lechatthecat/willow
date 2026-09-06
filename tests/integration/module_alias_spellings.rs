//! One module, several spellings: what each unit calls a module has to reach
//! the same registered tables (willow-kd1v).
//!
//! The back end declares a module once, under the name the module GRAPH gave
//! it — the first importer's spelling. Every other unit is free to reach it by
//! its own `import` alias, or by its canonical name when it was another file
//! that aliased it, and both spellings then name one module while only one of
//! them is a key. An entry file saying `import sales as market;` registered
//! `market::Amount`, so a module saying `import sales;` and writing
//! `sales::Amount` found no layout, no `type_id` and no symbol prefix: its
//! bodies fell out of the LIR walker's subset, and the AST emitter mangled
//! calls to symbols that do not exist.
//!
//! The fix binds the unit's own spelling to the registered one for the length
//! of that unit's phase — the TYPE half through the shared type scope, so the
//! alias reads the canonical entry rather than a copy of it, and the MODULE
//! half in the symbol-prefix table. What a unit binds comes back out again, so
//! nothing leaks into the next unit's view.
//!
//! One thing must NOT stay unit-local: what the tables record. Every later
//! unit's declaration phase re-walks `extends` chains for the whole build, so a
//! base recorded under a spelling only one unit binds resolved to nothing once
//! that unit's phase ended, and the subclass's layout was silently rewritten to
//! its own fields alone. Declared names are therefore canonicalized at
//! registration, while the alias is still installed.
//!
//! Since willow-0g8j.3 a body outside the walker's subset is a compile error,
//! so a run that prints the right answer is proof the walker produced it.
//!
//! 24 perspectives:
//!   1 the bug: entry alias, module binds  13 a base named the unit's way
//!   2 a call by the canonical name        14 the inherited field survives
//!   3 the importer's own alias            15 an override through that base
//!   4 entry and module disagree           16 two importers, two aliases
//!   5 a method on the aliased class       17 a name no body writes
//!   6 a static method                     18 an alias shadowing a module
//!   7 a constructor                       19 a signature returns the class
//!   8 an enum the unit's own way          20 a parameter takes it
//!   9 a qualified enum pattern            21 an array of it
//!  10 a payload variant binds             22 the walker took every body
//!  11 enum equality across spellings      23 aliased and plain agree
//!  12 an interface the module made        24 the example runs
//!
//! 17 is the case where the registered name is one no body writes at all, and
//! 18 the case where a unit's alias IS another module's registered name.

use super::support::{
    compile_temp_project_with_env_and_run, compile_temp_project_with_env_and_run_under,
    compile_temp_project_with_env_stderr,
};

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];
const LIR_LOG: &[(&str, &str)] = &[("WILLOW_LIR_LOG", "1")];
const STRESS: [(&str, &str); 1] = [("WILLOW_GC_STRESS", "alloc")];

/// The module every other file in this suite reaches under some other name.
const SALES: &str = r#"
module sales;

pub enum Grade { Low, High }
pub enum Tag { Plain, Paid(i64) }

pub interface Named { fn label(self) -> String; }

pub open class Amount implements Named {
    pub value: i64;
    pub init(self, value: i64) { self.value = value; }
    pub fn label(self) -> String { return "amount"; }
    pub open fn scaled(self) -> i64 { return self.value; }
    pub static fn of(v: i64) -> Amount { return new Amount(v); }
}

pub fn amount(v: i64) -> Amount { return new Amount(v); }
pub fn grade(v: i64) -> Grade {
    if v > 10 { return Grade::High; }
    return Grade::Low;
}
pub fn tag(v: i64) -> Tag {
    if v > 0 { return Tag::Paid(v); }
    return Tag::Plain;
}
pub fn sum(a: Amount, b: Amount) -> i64 { return a.value + b.value; }
"#;

/// A three-file project: `sales.wi`, one importing module, and the entry.
fn project<'a>(module_name: &'a str, module: &'a str, main: &'a str) -> [(&'a str, &'a str); 3] {
    [
        ("sales.wi", SALES),
        (module_name, module),
        ("main.wi", main),
    ]
}

/// Build and run, requiring success and exact output.
fn assert_prints(files: &[(&str, &str)], expected: &str) {
    let (out, ok) = compile_temp_project_with_env_and_run(files, "main.wi", &PLAIN[..]);
    assert!(ok, "build failed: {out}");
    assert_eq!(out, expected, "wrong output");
}

/// [`assert_prints`] plus a run that collects at every allocation site, for the
/// programs that keep an aliased class instance live across another call.
fn assert_prints_under_stress(files: &[(&str, &str)], expected: &str) {
    assert_prints(files, expected);
    let (out, ok) =
        compile_temp_project_with_env_and_run_under(files, "main.wi", &PLAIN[..], &STRESS[..]);
    assert!(ok, "build failed under GC stress: {out}");
    assert_eq!(out, expected, "wrong output under GC stress");
}

/// 1. The bug as reported. The entry aliases the module; another module writes
///    its canonical name and BINDS one of its classes, which is where the
///    missing layout first showed.
#[test]
fn mas_01_a_module_binds_a_class_of_a_module_the_entry_aliased() {
    let ledger = r#"
module ledger;
import sales;

pub fn describe(v: i64) -> i64 {
    let a: sales::Amount = sales::amount(v);
    return a.value;
}
"#;
    let main = r#"
import sales as market;
import ledger;

fn main() { println(ledger::describe(41)); }
"#;
    assert_prints(&project("ledger.wi", ledger, main), "41\n");
}

/// 2. The call itself, with no binding to hold the result: the symbol prefix
///    is looked up through the same table the type is, so a plain call has to
///    resolve under the unit's own spelling too.
#[test]
fn mas_02_a_call_by_the_canonical_name_under_an_entry_alias() {
    let ledger = r#"
module ledger;
import sales;

pub fn twice(v: i64) -> i64 { return sales::amount(v).value + sales::amount(v).value; }
"#;
    let main = r#"
import sales as market;
import ledger;

fn main() { println(ledger::twice(3)); }
"#;
    assert_prints(&project("ledger.wi", ledger, main), "6\n");
}

/// 3. The mirror image: the ENTRY writes the canonical name, so the graph
///    registers that, and the importing module is the one with the alias.
#[test]
fn mas_03_the_importing_module_has_the_alias() {
    let ledger = r#"
module ledger;
import sales as biz;

pub fn describe(v: i64) -> i64 {
    let a: biz::Amount = biz::amount(v);
    return a.value;
}
"#;
    let main = r#"
import sales;
import ledger;

fn main() {
    println(ledger::describe(7));
    println(sales::amount(1).value);
}
"#;
    assert_prints(&project("ledger.wi", ledger, main), "7\n1\n");
}

/// 4. Both units alias it, differently, so NEITHER spelling in a body is the
///    registered one — the entry's `market` is, and only because it was first.
#[test]
fn mas_04_entry_and_module_alias_the_same_module_differently() {
    let ledger = r#"
module ledger;
import sales as biz;

pub fn describe(v: i64) -> i64 {
    let a: biz::Amount = biz::amount(v);
    return a.value;
}
"#;
    let main = r#"
import sales as market;
import ledger;

fn main() {
    println(ledger::describe(9));
    println(market::amount(2).value);
}
"#;
    assert_prints(&project("ledger.wi", ledger, main), "9\n2\n");
}

/// 5. A method call on a value of the aliased class. The receiver's class name
///    is what the symbol mangler splits on, so an unresolved prefix here is a
///    call to a symbol that does not exist.
#[test]
fn mas_05_a_method_call_on_the_aliased_class() {
    let ledger = r#"
module ledger;
import sales;

pub fn scaled(v: i64) -> i64 {
    let a: sales::Amount = sales::amount(v);
    return a.scaled();
}
"#;
    let main = r#"
import sales as market;
import ledger;

fn main() { println(ledger::scaled(12)); }
"#;
    assert_prints_under_stress(&project("ledger.wi", ledger, main), "12\n");
}

/// 6. A static method reached through the unit's own spelling of the module.
#[test]
fn mas_06_a_static_method_through_the_units_own_spelling() {
    let ledger = r#"
module ledger;
import sales;

pub fn made(v: i64) -> i64 { return sales::Amount::of(v).value; }
"#;
    let main = r#"
import sales as market;
import ledger;

fn main() { println(ledger::made(5)); }
"#;
    assert_prints(&project("ledger.wi", ledger, main), "5\n");
}

/// 7. `new sales::Amount(..)` — the constructor symbol is mangled from the same
///    prefix, and the memberwise fallback needs the layout the prefix reaches.
#[test]
fn mas_07_a_constructor_through_the_units_own_spelling() {
    let ledger = r#"
module ledger;
import sales;

pub fn built(v: i64) -> i64 {
    let a = new sales::Amount(v);
    return a.value;
}
"#;
    let main = r#"
import sales as market;
import ledger;

fn main() { println(ledger::built(8)); }
"#;
    assert_prints(&project("ledger.wi", ledger, main), "8\n");
}

/// 8. An enum, which is keyed by the module's CANONICAL path rather than the
///    graph name its classes carry — so the two halves must not be aliased the
///    same way, and a unit writing the canonical name needs no alias at all.
#[test]
fn mas_08_an_enum_reached_the_units_own_way() {
    let ledger = r#"
module ledger;
import sales;

pub fn high(v: i64) -> bool {
    let g: sales::Grade = sales::grade(v);
    return g == sales::Grade::High;
}
"#;
    let main = r#"
import sales as market;
import ledger;

fn main() {
    println(ledger::high(50));
    println(ledger::high(1));
}
"#;
    assert_prints(&project("ledger.wi", ledger, main), "true\nfalse\n");
}

/// 9. The same enum in a `match`, with fully qualified patterns: the arm tags
///    come from the enum table, and the wrong key selects arm zero.
#[test]
fn mas_09_a_qualified_enum_pattern_in_a_match() {
    let ledger = r#"
module ledger;
import sales;

pub fn word(v: i64) -> String {
    match sales::grade(v) {
        sales::Grade::High => { return "high"; }
        sales::Grade::Low => { return "low"; }
    }
}
"#;
    let main = r#"
import sales as market;
import ledger;

fn main() {
    println(ledger::word(50));
    println(ledger::word(1));
}
"#;
    assert_prints(&project("ledger.wi", ledger, main), "high\nlow\n");
}

/// 10. A payload variant, which is a heap object rather than a bare tag: its
///     payload types are read off the same enum entry.
#[test]
fn mas_10_a_payload_variant_binds_its_value() {
    let ledger = r#"
module ledger;
import sales;

pub fn paid(v: i64) -> i64 {
    match sales::tag(v) {
        sales::Tag::Paid(n) => { return n; }
        sales::Tag::Plain => { return -1; }
    }
}
"#;
    let main = r#"
import sales as market;
import ledger;

fn main() {
    println(ledger::paid(23));
    println(ledger::paid(0));
}
"#;
    assert_prints_under_stress(&project("ledger.wi", ledger, main), "23\n-1\n");
}

/// 11. One enum value produced under one spelling and compared under another:
///     both units must be talking about a single type. The module writes the
///     canonical name and the entry an alias, and the graph registered the
///     alias — so the value is made under a key and read under a binding.
#[test]
fn mas_11_enum_equality_across_two_spellings() {
    let ledger = r#"
module ledger;
import sales;

pub fn top(v: i64) -> sales::Grade { return sales::grade(v); }
"#;
    let main = r#"
import sales as market;
import ledger;

fn main() {
    println(ledger::top(50) == market::Grade::High);
    println(ledger::top(1) == market::Grade::High);
}
"#;
    assert_prints(&project("ledger.wi", ledger, main), "true\nfalse\n");
}

/// 12. An interface value: the interface table is one flat build-wide
///     namespace, and a class boxed under an unresolved name gets no vtable.
#[test]
fn mas_12_an_interface_value_from_the_aliased_module() {
    let ledger = r#"
module ledger;
import sales;

pub fn labelled(v: i64) -> String {
    let n: sales::Named = sales::amount(v);
    return n.label();
}
"#;
    let main = r#"
import sales as market;
import ledger;

fn main() { println(ledger::labelled(3)); }
"#;
    assert_prints_under_stress(&project("ledger.wi", ledger, main), "amount\n");
}

/// 13. A class that EXTENDS one named the unit's own way. The base is recorded
///     in a build-wide table, so it has to be recorded canonically.
#[test]
fn mas_13_a_base_class_named_the_units_own_way() {
    let ledger = r#"
module ledger;
import sales;

pub class Double extends sales::Amount {
    pub init(self, value: i64) { super.init(value); }
    pub override fn scaled(self) -> i64 { return self.value * 2; }
}

pub fn made(v: i64) -> i64 {
    let d = new Double(v);
    return d.value;
}
"#;
    let main = r#"
import sales as market;
import ledger;

fn main() { println(ledger::made(6)); }
"#;
    assert_prints(&project("ledger.wi", ledger, main), "6\n");
}

/// 14. The regression the base-class fix is really about: a LATER unit's
///     declaration phase re-walks every `extends` chain, with the earlier
///     unit's spellings gone. `audit.wi` and the entry both declare after
///     `ledger.wi`, so the inherited field is resolved three times over; if any
///     of those passes loses the base, the layout ends up empty and the field
///     read takes the wrong offset.
#[test]
fn mas_14_the_inherited_field_survives_later_units() {
    let files = [
        ("sales.wi", SALES),
        (
            "ledger.wi",
            r#"
module ledger;
import sales;

pub class Double extends sales::Amount {
    pub init(self, value: i64) { super.init(value); }
    pub override fn scaled(self) -> i64 { return self.value * 2; }
}

pub fn field(v: i64) -> i64 {
    let d = new Double(v);
    return d.value;
}
"#,
        ),
        (
            "audit.wi",
            r#"
module audit;
import sales as books;

pub fn plain(v: i64) -> i64 { return books::amount(v).value; }
"#,
        ),
        (
            "main.wi",
            r#"
import sales as market;
import ledger;
import audit;

fn main() {
    println(ledger::field(17));
    println(audit::plain(4));
}
"#,
        ),
    ];
    assert_prints(&files, "17\n4\n");
}

/// 15. The virtual half of the same hierarchy: an `override` of a base method
///     reached through the unit's own spelling keeps the base's slot index.
#[test]
fn mas_15_an_override_dispatches_through_the_aliased_base() {
    let ledger = r#"
module ledger;
import sales;

pub class Double extends sales::Amount {
    pub init(self, value: i64) { super.init(value); }
    pub override fn scaled(self) -> i64 { return self.value * 2; }
}

pub fn doubled(v: i64) -> i64 {
    let d = new Double(v);
    let a: sales::Amount = d;
    return a.scaled();
}
"#;
    let main = r#"
import sales as market;
import ledger;

fn main() { println(ledger::doubled(21)); }
"#;
    assert_prints_under_stress(&project("ledger.wi", ledger, main), "42\n");
}

/// 16. Two importing modules with two different aliases for one module, both
///     compiled in the same build: each unit's binding must come back out
///     before the next unit's phase, or the second would read the first's.
#[test]
fn mas_16_two_importers_with_two_different_aliases() {
    let files = [
        ("sales.wi", SALES),
        (
            "left.wi",
            r#"
module left;
import sales as l;

pub fn value(v: i64) -> i64 {
    let a: l::Amount = l::amount(v);
    return a.scaled();
}
"#,
        ),
        (
            "right.wi",
            r#"
module right;
import sales as r;

pub fn word(v: i64) -> String {
    match r::grade(v) {
        r::Grade::High => { return "high"; }
        r::Grade::Low => { return "low"; }
    }
}
"#,
        ),
        (
            "main.wi",
            r#"
import sales as market;
import left;
import right;

fn main() {
    println(left::value(11));
    println(right::word(11));
    println(right::word(2));
}
"#,
        ),
    ];
    assert_prints(&files, "11\nhigh\nlow\n");
}

/// 17. A build in which NO body writes the registered name. The entry never
///     imports `sales` at all, so the graph name is whichever importing module
///     got there first, and the other module reaches the same classes and
///     enums under a spelling that is a key nowhere.
#[test]
fn mas_17_the_graph_name_is_a_spelling_no_body_writes() {
    let files = [
        ("sales.wi", SALES),
        (
            "ledger.wi",
            r#"
module ledger;
import sales as biz;

pub fn value(v: i64) -> i64 {
    let a: biz::Amount = biz::amount(v);
    return a.scaled();
}
"#,
        ),
        (
            "audit.wi",
            r#"
module audit;
import sales as books;

pub fn top(v: i64) -> bool { return books::grade(v) == books::Grade::High; }
"#,
        ),
        (
            "main.wi",
            r#"
import ledger;
import audit;

fn main() {
    println(ledger::value(4));
    println(audit::top(50));
    println(audit::top(2));
}
"#,
        ),
    ];
    assert_prints(&files, "4\ntrue\nfalse\n");
}

/// 18. An alias that shadows another real module's name. Inside `ledger.wi`,
///     `books::` is `sales`; in the entry it is the `books` module. Two things
///     have to hold: the binding may not outlive `ledger`'s phase, or one
///     module would answer for the other in the entry — and while it IS
///     installed, the real `books` module's own `extends` chain must still be
///     walked as the identity it was recorded under, since every unit's
///     declaration phase re-finalizes every class in the build.
#[test]
fn mas_18_an_alias_shadowing_another_module_is_unit_local() {
    let files = [
        ("sales.wi", SALES),
        (
            "books.wi",
            r#"
module books;

pub open class Amount {
    pub value: i64;
    pub init(self, value: i64) { self.value = value; }
    pub open fn scaled(self) -> i64 { return self.value + 1000; }
}

pub class Twice extends Amount {
    pub init(self, value: i64) { super.init(value); }
    pub override fn scaled(self) -> i64 { return self.value * 2; }
}

pub fn amount(v: i64) -> Amount { return new Amount(v); }
pub fn twice(v: i64) -> Amount { return new Twice(v); }
"#,
        ),
        (
            "ledger.wi",
            r#"
module ledger;
import sales as books;

pub fn value(v: i64) -> i64 {
    let a: books::Amount = books::amount(v);
    return a.scaled();
}
"#,
        ),
        (
            "main.wi",
            r#"
import sales as market;
import books;
import ledger;

fn main() {
    println(ledger::value(5));
    let b: books::Amount = books::amount(5);
    println(b.scaled());
    let t: books::Amount = books::twice(5);
    println(t.scaled());
    println(t.value);
}
"#,
        ),
    ];
    assert_prints(&files, "5\n1005\n10\n5\n");
}

/// 19. A module function whose declared RETURN type is the aliased class,
///     consumed by ANOTHER module. Both spell the module `sales` and the graph
///     registered `market`, so the signature is recorded and read under a
///     spelling that is a key nowhere.
#[test]
fn mas_19_a_signature_returns_the_aliased_class() {
    let files = [
        ("sales.wi", SALES),
        (
            "ledger.wi",
            r#"
module ledger;
import sales;

pub fn make(v: i64) -> sales::Amount { return sales::amount(v); }
"#,
        ),
        (
            "audit.wi",
            r#"
module audit;
import sales;
import ledger;

pub fn scaled(v: i64) -> i64 {
    let a: sales::Amount = ledger::make(v);
    return a.scaled();
}
"#,
        ),
        (
            "main.wi",
            r#"
import sales as market;
import ledger;
import audit;

fn main() { println(audit::scaled(14)); }
"#,
        ),
    ];
    assert_prints_under_stress(&files, "14\n");
}

/// 20. The parameter direction of the same question, with the values built in
///     one module and consumed in another.
#[test]
fn mas_20_a_parameter_takes_the_aliased_class() {
    let files = [
        ("sales.wi", SALES),
        (
            "ledger.wi",
            r#"
module ledger;
import sales;

pub fn plus(a: sales::Amount, b: sales::Amount) -> i64 { return sales::sum(a, b); }
"#,
        ),
        (
            "audit.wi",
            r#"
module audit;
import sales;
import ledger;

pub fn total(v: i64) -> i64 { return ledger::plus(sales::amount(v), sales::amount(v + 1)); }
"#,
        ),
        (
            "main.wi",
            r#"
import sales as market;
import ledger;
import audit;

fn main() { println(audit::total(3)); }
"#,
        ),
    ];
    assert_prints_under_stress(&files, "7\n");
}

/// 21. An array of the aliased class: the element type is vetted through the
///     same layout lookup, one level down.
#[test]
fn mas_21_an_array_of_the_aliased_class() {
    let ledger = r#"
module ledger;
import sales;

pub fn total(v: i64) -> i64 {
    let xs = [sales::amount(v), sales::amount(v + 1)];
    return xs[0].value + xs[1].value;
}
"#;
    let main = r#"
import sales as market;
import ledger;

fn main() { println(ledger::total(10)); }
"#;
    assert_prints_under_stress(&project("ledger.wi", ledger, main), "21\n");
}

/// 22. Coverage, not just answers: every body here has to be the walker's.
///     Without this a regression could push the module back onto the AST
///     emitter and still print the right numbers.
#[test]
fn mas_22_the_walker_compiled_every_module_body() {
    let ledger = r#"
module ledger;
import sales;

pub class Double extends sales::Amount {
    pub init(self, value: i64) { super.init(value); }
    pub override fn scaled(self) -> i64 { return self.value * 2; }
}

pub fn word(v: i64) -> String {
    match sales::grade(v) {
        sales::Grade::High => { return "high"; }
        sales::Grade::Low => { return "low"; }
    }
}

pub fn doubled(v: i64) -> i64 {
    let d = new Double(v);
    let a: sales::Amount = d;
    return a.scaled();
}
"#;
    let main = r#"
import sales as market;
import ledger;

fn main() {
    println(ledger::word(50));
    println(ledger::doubled(3));
}
"#;
    let files = project("ledger.wi", ledger, main);
    let (ok, log) = compile_temp_project_with_env_stderr(&files, "main.wi", LIR_LOG);
    assert!(ok, "logged build failed: {log}");
    for function in [
        "sales.amount",
        "sales.grade",
        "ledger.word",
        "ledger.doubled",
        "ledger::Double::scaled",
        "main",
    ] {
        let line = format!("[lir] compiling `{function}` from lowered IR");
        assert!(
            log.contains(&line),
            "`{function}` did not use the LIR walker: {log}"
        );
    }
}

/// 23. The property that makes all of the above one bug: how an `import` is
///     WRITTEN may not change what a program prints. The same four bodies are
///     compiled with every module spelled canonically and with two aliases in
///     play, and the two runs must agree.
#[test]
fn mas_23_aliased_and_plain_spellings_print_the_same() {
    let aliased = [
        ("sales.wi", SALES),
        (
            "ledger.wi",
            r#"
module ledger;
import sales as biz;

pub class Double extends biz::Amount {
    pub init(self, value: i64) { super.init(value); }
    pub override fn scaled(self) -> i64 { return self.value * 2; }
}

pub fn report(v: i64) -> i64 {
    let d = new Double(v);
    let a: biz::Amount = d;
    let n: biz::Named = biz::amount(v);
    match biz::tag(a.scaled()) {
        biz::Tag::Paid(p) => { println(n.label()); return p; }
        biz::Tag::Plain => { return 0; }
    }
}
"#,
        ),
        (
            "main.wi",
            r#"
import sales as market;
import ledger;

fn main() {
    println(ledger::report(5));
    println(market::grade(50) == market::Grade::High);
}
"#,
        ),
    ];
    let plain = [
        ("sales.wi", SALES),
        (
            "ledger.wi",
            r#"
module ledger;
import sales;

pub class Double extends sales::Amount {
    pub init(self, value: i64) { super.init(value); }
    pub override fn scaled(self) -> i64 { return self.value * 2; }
}

pub fn report(v: i64) -> i64 {
    let d = new Double(v);
    let a: sales::Amount = d;
    let n: sales::Named = sales::amount(v);
    match sales::tag(a.scaled()) {
        sales::Tag::Paid(p) => { println(n.label()); return p; }
        sales::Tag::Plain => { return 0; }
    }
}
"#,
        ),
        (
            "main.wi",
            r#"
import sales;
import ledger;

fn main() {
    println(ledger::report(5));
    println(sales::grade(50) == sales::Grade::High);
}
"#,
        ),
    ];
    let (plain_out, ok) = compile_temp_project_with_env_and_run(&plain, "main.wi", &PLAIN[..]);
    assert!(ok, "plain build failed: {plain_out}");
    let (aliased_out, ok) = compile_temp_project_with_env_and_run(&aliased, "main.wi", &PLAIN[..]);
    assert!(ok, "aliased build failed: {aliased_out}");
    assert_eq!(
        aliased_out, plain_out,
        "the spelling of an import changed the answer"
    );
}

/// 24. The shipped example builds and prints, with every one of its bodies
///     compiled from lowered IR.
#[test]
fn mas_24_the_example_runs() {
    let files = [
        (
            "sales.wi",
            include_str!("../../example/module_alias_spellings/sales.wi"),
        ),
        (
            "ledger.wi",
            include_str!("../../example/module_alias_spellings/ledger.wi"),
        ),
        (
            "audit.wi",
            include_str!("../../example/module_alias_spellings/audit.wi"),
        ),
        (
            "main.wi",
            include_str!("../../example/module_alias_spellings/main.wi"),
        ),
    ];
    assert_prints(&files, "high\nlow\n42\n10\ntrue\n7\ntrue\n");

    let (ok, log) = compile_temp_project_with_env_stderr(&files, "main.wi", LIR_LOG);
    assert!(ok, "logged example build failed: {log}");
    for function in [
        "sales.amount",
        "sales.grade",
        "ledger.describe",
        "ledger.doubled",
        "ledger::Double::scaled",
        "audit.total",
        "audit.top",
        "main",
    ] {
        let line = format!("[lir] compiling `{function}` from lowered IR");
        assert!(
            log.contains(&line),
            "`{function}` did not use the LIR walker: {log}"
        );
    }
}
