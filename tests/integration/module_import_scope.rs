//! An `import` line binds names in the FILE that writes it, and only there
//! (willow-vtlr, willow-28h8).
//!
//! Two halves of the same rule, both of which the back end used to get wrong
//! because it re-derived imports from the path string instead of taking the
//! module resolver's classification:
//!
//!   * `import util::math;` binds `math`. The parent `util` is not a name this
//!     file can write, so a class `util` declares must not compete with an
//!     imported module's class of the same bare name. Counting the parent as
//!     visible cost such a body its lowering — no wrong code, but an
//!     unimportable module is the wrong reason to lose one.
//!
//!   * `import calc::add;` binds the local name `add` to THAT module's `add`.
//!     The binding was global and installed once, for the entry file alone, so
//!     a module that imported a different `add` called the entry's, silently.
//!
//! The second half is a wrong-answer bug, so those perspectives assert VALUES;
//! the first is an eligibility bug, so those assert on the build and on
//! `WILLOW_LIR_LOG=1` lines.

use super::support::*;

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];
const LIR_LOG: &[(&str, &str)] = &[("WILLOW_LIR_LOG", "1")];
const ALLOC_STRESS: &[(&str, &str)] = &[("WILLOW_GC_STRESS", "alloc")];
const MINOR_STRESS: &[(&str, &str)] = &[("WILLOW_GC_STRESS", "minor")];

#[track_caller]
fn assert_project_output(files: &[(&str, &str)], entry: &str, expected: &str) {
    let (out, ok) = compile_temp_project_with_env_and_run(files, entry, &PLAIN[..]);
    assert!(ok, "build failed: {out}");
    assert_eq!(out, expected, "wrong output");
}

/// Reached only because something imports ONE of its items. Its `Amount` is a
/// class of this build all the same, and its `discount` is a different function
/// from `sales`' one of that name.
const PRICING: &str = r#"
module pricing;

pub class Amount {
    pub cents: i64;

    pub fn doubled(self) -> i64 {
        return self.cents * 2;
    }
}

pub fn amount(cents: i64) -> Amount {
    return new Amount(cents);
}

pub fn doubled_cents(a: Amount) -> i64 {
    return a.doubled();
}

pub fn discount(n: i64) -> i64 {
    return n * 10;
}
"#;

/// A CHILD module: `import pricing::rules;` binds `rules`, never `pricing`.
const RULES: &str = r#"
module pricing::rules;

pub fn markup(n: i64) -> i64 {
    return n * 2;
}
"#;

/// The module an entry file actually imports. Its `Amount` is a different class
/// with a different layout.
const SALES: &str = r#"
module sales;

pub class Amount {
    pub value: i64;
    pub label: String;

    pub fn describe(self) -> String {
        return self.label + ":" + self.value.toString();
    }
}

pub fn amount(value: i64) -> Amount {
    return new Amount(value, "sale");
}

pub fn value_of(a: Amount) -> i64 {
    return a.value;
}

pub fn describe(a: Amount) -> String {
    return a.describe();
}

pub fn discount(n: i64) -> i64 {
    return n - 1;
}
"#;

/// Binds the local name `discount` to PRICING's function.
const TOTALS: &str = r#"
module totals;

import pricing::discount;

pub fn net(n: i64) -> i64 {
    return discount(n);
}
"#;

/// The four fixture modules plus one entry file.
fn project(entry: &'static str) -> Vec<(&'static str, &'static str)> {
    vec![
        ("pricing.wi", PRICING),
        ("pricing/rules.wi", RULES),
        ("sales.wi", SALES),
        ("totals.wi", TOTALS),
        ("main.wi", entry),
    ]
}

// ---------------------------------------------------------------------------
// An item import binds a local name in ONE file (willow-28h8).
// ---------------------------------------------------------------------------

// 1. The bug: both files bind `discount`, and each has to call the module IT
//    imported. The module used to call the entry's.
#[test]
fn each_file_calls_the_item_it_imported() {
    let entry = r#"
import sales::discount;
import totals;

fn main() {
    println(discount(10));
    println(totals::net(10));
}
"#;
    let (out, ok) = compile_temp_project_with_env_and_run(&project(entry), "main.wi", &PLAIN[..]);
    assert!(ok, "build failed: {out}");
    assert_eq!(out, "9\n100\n");
}

// 2. The same two calls in the OPPOSITE order. The module's own binding is
//    installed while its bodies are compiled, so a per-file binding that is
//    really one global would depend on which call ran first.
#[test]
fn the_per_file_binding_does_not_depend_on_call_order() {
    let entry = r#"
import sales::discount;
import totals;

fn main() {
    println(totals::net(10));
    println(discount(10));
}
"#;
    assert_project_output(&project(entry), "main.wi", "100\n9\n");
}

// 3. The entry's own binding survives the module body phase, which runs
//    between the entry's declaration and its bodies and rebinds the name.
#[test]
fn the_entry_binding_survives_the_module_body_phase() {
    let entry = r#"
import sales::discount;
import totals;

fn main() {
    println(totals::net(2));
    println(discount(2));
    println(totals::net(3));
    println(discount(3));
}
"#;
    assert_project_output(&project(entry), "main.wi", "20\n1\n30\n2\n");
}

// 4. A module's item import binds even when the entry file imports no such
//    name at all — the binding used to exist only for the entry's own imports.
#[test]
fn a_module_item_import_binds_without_the_entry_having_one() {
    let entry = r#"
import totals;

fn main() {
    println(totals::net(4));
}
"#;
    assert_project_output(&project(entry), "main.wi", "40\n");
}

// 5. Two modules importing the SAME item both get it.
#[test]
fn two_modules_can_import_the_same_item() {
    let rebate = r#"
module rebate;

import pricing::discount;

pub fn off(n: i64) -> i64 {
    return discount(n) + 1;
}
"#;
    let entry = r#"
import totals;
import rebate;

fn main() {
    println(totals::net(1));
    println(rebate::off(1));
}
"#;
    let mut files = project(entry);
    files.push(("rebate.wi", rebate));
    assert_project_output(&files, "main.wi", "10\n11\n");
}

// 6. An aliased item import inside a module binds the ALIAS to that module's
//    function, with the entry holding the unaliased name of the other one.
#[test]
fn an_aliased_item_import_in_a_module_binds_the_alias() {
    let cutter = r#"
module cutter;

import pricing::discount as cut;

pub fn go(n: i64) -> i64 {
    return cut(n);
}
"#;
    let entry = r#"
import sales::discount;
import cutter;

fn main() {
    println(cutter::go(6));
    println(discount(6));
}
"#;
    let mut files = project(entry);
    files.push(("cutter.wi", cutter));
    assert_project_output(&files, "main.wi", "60\n5\n");
}

// 7. The mirror image: the ENTRY aliases, a module takes the plain name.
#[test]
fn an_alias_in_the_entry_and_a_plain_name_in_a_module() {
    let entry = r#"
import sales::discount as cut;
import totals;

fn main() {
    println(cut(7));
    println(totals::net(7));
}
"#;
    assert_project_output(&project(entry), "main.wi", "6\n70\n");
}

// 8. Called from a lambda in a module body, which is lifted to its own
//    function and compiled in that module's body phase.
#[test]
fn an_item_import_reached_through_a_module_lambda() {
    let lifted = r#"
module lifted;

import pricing::discount;

pub fn apply(n: i64) -> i64 {
    let f: fn(i64) -> i64 = |x| discount(x);
    return f(n);
}
"#;
    let entry = r#"
import sales::discount;
import lifted;

fn main() {
    println(lifted::apply(3));
    println(discount(3));
}
"#;
    let mut files = project(entry);
    files.push(("lifted.wi", lifted));
    assert_project_output(&files, "main.wi", "30\n2\n");
}

// 9. Called from a class METHOD in a module body, compiled under a mangled
//    member symbol rather than a plain module function symbol.
#[test]
fn an_item_import_reached_through_a_module_method() {
    let basket = r#"
module basket;

import pricing::discount;

pub class Line {
    pub qty: i64;

    pub fn charge(self) -> i64 {
        return discount(self.qty);
    }
}

pub fn charge_of(qty: i64) -> i64 {
    let line = new Line(qty);
    return line.charge();
}
"#;
    let entry = r#"
import sales::discount;
import basket;

fn main() {
    println(basket::charge_of(4));
    println(discount(4));
}
"#;
    let mut files = project(entry);
    files.push(("basket.wi", basket));
    assert_project_output(&files, "main.wi", "40\n3\n");
}

// 10. Called from an async module function, whose body is split across a
//     suspension point and compiled as a poll function.
#[test]
fn an_item_import_reached_through_an_async_module_function() {
    let slow = r#"
module slow;

import pricing::discount;

pub async fn later(n: i64) -> i64 {
    await yield();
    return discount(n);
}
"#;
    let entry = r#"
import sales::discount;
import slow;

async fn main() {
    println(await slow::later(5));
    println(discount(5));
}
"#;
    let mut files = project(entry);
    files.push(("slow.wi", slow));
    assert_project_output(&files, "main.wi", "50\n4\n");
}

// 11. A chain of item imports: the middle module binds one, the top module
//     binds the middle's function, and the entry binds a third of the name.
#[test]
fn a_chain_of_item_imports_keeps_each_link_separate() {
    let middle = r#"
module middle;

import pricing::discount;

pub fn step(n: i64) -> i64 {
    return discount(n);
}
"#;
    let top = r#"
module top;

import middle::step;

pub fn run(n: i64) -> i64 {
    return step(n) + 1;
}
"#;
    let entry = r#"
import sales::discount;
import top;

fn main() {
    println(top::run(2));
    println(discount(2));
}
"#;
    let mut files = project(entry);
    files.push(("middle.wi", middle));
    files.push(("top.wi", top));
    assert_project_output(&files, "main.wi", "21\n1\n");
}

// 12. The module function that calls its item import is really lowered, rather
//     than quietly falling back to an emitter that happens to agree.
#[test]
fn the_module_function_calling_its_item_import_is_lowered() {
    let entry = r#"
import sales::discount;
import totals;

fn main() {
    println(totals::net(1));
    println(discount(1));
}
"#;
    let (ok, log) = compile_temp_project_with_env_stderr(&project(entry), "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    assert!(
        log.contains("[lir] compiling `totals.net` from lowered IR"),
        "the module function holding an item import was not lowered: {log}"
    );
}

// 13. Under allocation stress, where every call also collects.
#[test]
fn the_per_file_binding_holds_under_allocation_stress() {
    let entry = r#"
import sales::discount;
import totals;

fn main() {
    println(discount(10));
    println(totals::net(10));
}
"#;
    let (out, ok) = compile_temp_project_with_env_and_run_under(
        &project(entry),
        "main.wi",
        &PLAIN[..],
        ALLOC_STRESS,
    );
    assert!(ok, "alloc-stress run failed: {out}");
    assert_eq!(out, "9\n100\n");
}

// 14. A release build, with none of the debug instrumentation.
#[test]
fn the_release_build_answers_the_same() {
    let entry = r#"
import sales::discount;
import totals;

fn main() {
    println(discount(10));
    println(totals::net(10));
}
"#;
    let project = TestProject::new("module_import_scope_release", &project(entry));
    let output = project.compile_release("main.wi");
    assert!(
        output.status.success(),
        "release build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = project.run();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "9\n100\n");
}

// ---------------------------------------------------------------------------
// Which modules a file can SEE (willow-vtlr).
// ---------------------------------------------------------------------------

/// The entry file names a bare `Amount` (the type of what `sales::amount`
/// returns) while importing a CHILD of the other module that declares one.
const CHILD_IMPORT_ENTRY: &str = r#"
import pricing::rules;
import sales;
import totals;

fn subtotal() -> i64 {
    let a = sales::amount(5);
    return sales::value_of(a) + rules::markup(1);
}

fn main() {
    println(subtotal());
    println(totals::net(1));
}
"#;

// 15. The bug: `import pricing::rules;` does not put `pricing` in scope, so
//     `pricing::Amount` must not make the entry's bare `Amount` ambiguous —
//     which used to cost the body its lowering, and is a build error now.
#[test]
fn the_parent_of_a_child_module_import_is_not_visible() {
    let (out, ok) =
        compile_temp_project_with_env_and_run(&project(CHILD_IMPORT_ENTRY), "main.wi", &PLAIN[..]);
    assert!(ok, "every body in this project must be walkable: {out}");
    assert_eq!(out, "7\n10\n");
}

// 16. The same program under minor-collection stress, the mode that MOVES an
//     object: resolving the class to the wrong module's layout would trace the
//     wrong fields.
#[test]
fn a_child_module_import_survives_minor_stress() {
    let (out, ok) = compile_temp_project_with_env_and_run_under(
        &project(CHILD_IMPORT_ENTRY),
        "main.wi",
        &PLAIN[..],
        MINOR_STRESS,
    );
    assert!(ok, "minor-stress run failed: {out}");
    assert_eq!(out, "7\n10\n");
}

// 17. The entry function is named in the selection log, so it was lowered
//     rather than skipped by the walker for some other reason.
#[test]
fn the_entry_function_holding_the_module_class_is_lowered() {
    let (ok, log) =
        compile_temp_project_with_env_stderr(&project(CHILD_IMPORT_ENTRY), "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    assert!(
        log.contains("[lir] compiling `subtotal` from lowered IR"),
        "the entry function binding a module class was not lowered: {log}"
    );
}

// 18. The layout it resolved is sales', not pricing's: `describe` reads a
//     `String` field pricing's `Amount` does not even have.
#[test]
fn the_resolved_layout_is_the_imported_modules() {
    let entry = r#"
import pricing::rules;
import sales;
import totals;

fn labelled() -> String {
    let a = sales::amount(rules::markup(4));
    return sales::describe(a);
}

fn main() {
    println(labelled());
    println(totals::net(1));
}
"#;
    assert_project_output(&project(entry), "main.wi", "sale:8\n10\n");
}

// 19. An ITEM import does not make its module visible either: the entry binds
//     one function of `pricing` and still means sales' `Amount`.
#[test]
fn the_module_of_an_item_import_is_not_visible() {
    let entry = r#"
import sales;
import pricing::discount;

fn subtotal() -> i64 {
    let a = sales::amount(6);
    return sales::value_of(a);
}

fn main() {
    println(subtotal());
    println(discount(6));
}
"#;
    let (out, ok) = compile_temp_project_with_env_and_run(&project(entry), "main.wi", &PLAIN[..]);
    assert!(
        ok,
        "every body in this project must compile from LIR: {out}"
    );
    assert_eq!(out, "6\n60\n");
}

// 20. A file that imports both modules keeps the class identity carried by each
//     module call's checked signature.
#[test]
fn a_file_that_imports_both_modules_keeps_call_result_identity() {
    let entry = r#"
import sales;
import pricing;

fn subtotal() -> i64 {
    let a = sales::amount(6);
    return sales::value_of(a) + pricing::doubled_cents(pricing::amount(2));
}

fn main() {
    println(subtotal());
}
"#;
    assert_project_output(&project(entry), "main.wi", "10\n");
}

// 21. The control: with only one `Amount` in the build the same entry lowers.
#[test]
fn one_amount_in_the_build_resolves() {
    let entry = r#"
import sales;

fn subtotal() -> i64 {
    let a = sales::amount(6);
    return sales::value_of(a);
}

fn main() {
    println(subtotal());
}
"#;
    let files = &[("sales.wi", SALES), ("main.wi", entry)];
    let (out, ok) = compile_temp_project_with_env_and_run(files, "main.wi", &PLAIN[..]);
    assert!(ok, "single-class build must lower: {out}");
    assert_eq!(out, "6\n");
}

// 22. A MODULE gets its own visible set too: `ledger` sees `sales` and not
//     `pricing`, though `pricing` is in the build and declares an `Amount`.
#[test]
fn a_module_resolves_a_class_through_its_own_imports() {
    let ledger = r#"
module ledger;

import sales;

pub fn line(value: i64) -> String {
    let a = sales::amount(value);
    return sales::describe(a);
}
"#;
    let entry = r#"
import ledger;
import totals;

fn main() {
    println(ledger::line(2));
    println(totals::net(2));
}
"#;
    let mut files = project(entry);
    files.push(("ledger.wi", ledger));
    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &PLAIN[..]);
    assert!(
        ok,
        "every body in this project must compile from LIR: {out}"
    );
    assert_eq!(out, "sale:2\n20\n");
}

// 23. An aliased module import is visible under the alias, which is also the
//     name the module tables are keyed by when this file is the only importer.
#[test]
fn an_aliased_module_import_is_visible_under_its_alias() {
    let entry = r#"
import sales as market;
import totals;

fn subtotal() -> i64 {
    let a = market::amount(8);
    return market::value_of(a);
}

fn main() {
    println(subtotal());
    println(totals::net(8));
}
"#;
    let (out, ok) = compile_temp_project_with_env_and_run(&project(entry), "main.wi", &PLAIN[..]);
    assert!(
        ok,
        "every body in this project must compile from LIR: {out}"
    );
    assert_eq!(out, "8\n80\n");
}

// 24. Minor-GC stress on the class-carrying program, where a wrong layout
//     would trace the wrong references.
#[test]
fn the_resolved_layout_survives_minor_gc_stress() {
    let (out, ok) = compile_temp_project_with_env_and_run_under(
        &project(CHILD_IMPORT_ENTRY),
        "main.wi",
        &PLAIN[..],
        MINOR_STRESS,
    );
    assert!(ok, "minor-stress run failed: {out}");
    assert_eq!(out, "7\n10\n");
}

// 25. The runnable example, which exercises both halves at once.
#[test]
fn the_module_import_scope_example_runs() {
    let (out, ok) = compile_file_and_run("example/module_import_scope/main.wi");
    assert!(
        ok,
        "example/module_import_scope/main.wi failed to compile or run"
    );
    assert_eq!(out, "7\nsale:3\n9\n100\n");
}

// 26. ... and every body in it is lowered, which is what the visible-module
//     half of this change buys.
#[test]
fn the_module_import_scope_example_is_fully_lowered() {
    let files = &[
        (
            "pricing.wi",
            include_str!("../../example/module_import_scope/pricing.wi"),
        ),
        (
            "pricing/rules.wi",
            include_str!("../../example/module_import_scope/pricing/rules.wi"),
        ),
        (
            "sales.wi",
            include_str!("../../example/module_import_scope/sales.wi"),
        ),
        (
            "totals.wi",
            include_str!("../../example/module_import_scope/totals.wi"),
        ),
        (
            "main.wi",
            include_str!("../../example/module_import_scope/main.wi"),
        ),
    ];
    let (out, ok) = compile_temp_project_with_env_and_run(files, "main.wi", &PLAIN[..]);
    assert!(ok, "every body in the example must compile from LIR: {out}");
    assert_eq!(out, "7\nsale:3\n9\n100\n");
}
