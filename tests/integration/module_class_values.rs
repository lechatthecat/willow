//! Values of an imported module's class in a file that never imports the class
//! itself, compiled from Lowered IR (willow-0g8j.2.19).
//!
//! `import geom;` names the module but not its `Point`. The type therefore
//! cannot be WRITTEN in the entry file — no parameter annotation, no field
//! read, no method call — while values of it still flow freely: bound,
//! reassigned, arrayed, returned from a lambda, handed back to the module
//! functions that do know the name.
//!
//! Every one of those flows used to force the entry function onto the AST
//! emitter. `geom::point(3, 4)` is typed `Point`, the bare name the module
//! declared, while the tables the module unit contributed are keyed on the
//! canonical `geom::Point`; only an item import copies them under the short
//! name. The walker looked the bare name up, found nothing, and refused the
//! whole body — silently, since a fallback prints the same answer. It now
//! resolves a bare class name through the imported modules, which is how the
//! AST emitter has always resolved it.
//!
//! Every test is differential: the same project under the AST emitter and
//! under the walker must print the same thing. Coverage is asserted from
//! `WILLOW_LIR_LOG=1`, which names each function the walker really compiled,
//! rather than from `WILLOW_LIR_REQUIRE=1`: when this file was written a
//! module's functions were never registered as lowered IR at all, so requiring
//! the walker failed on the module before the entry program was reached.
//! Modules are lowered now (willow-0g8j.16), so `WILLOW_LIR_REQUIRE=1` does
//! police a multi-file build — see `module_lir_bodies.rs` — but the log is
//! still the sharper instrument here, because it names the individual body.
//!
//! 25 perspectives:
//!   1 bound, then passed back        14 two modules, one class name
//!   2 no binding at all              15 an entry class of the same name
//!   3 the item import still works    16 an import alias
//!   4 reassigned                     17 a transitively imported module
//!   5 an array of them, indexed      18 inside a deferred statement
//!   6 returned from a lambda         19 under allocation-stress GC
//!   7 reassigned in a while loop     20 under minor-collection stress
//!   8 a second entry function        21 inside async functions
//!   9 two live across an allocation  22 a release build agrees
//!  10 a String field, via the module 23 both branches of an `if`
//!  11 a field of the same kind       24 a module subclass value
//!  12 an `Array<String>` field       25 the example project
//!  13 two modules, two class names

use super::support::{
    compile_temp_project_and_run, compile_temp_project_with_env_and_run,
    compile_temp_project_with_env_stderr,
};

const AST: [(&str, &str); 1] = [("WILLOW_LIR_BACKEND", "0")];
const LIR: [(&str, &str); 1] = [("WILLOW_LIR_BACKEND", "1")];
const LIR_LOG: [(&str, &str); 2] = [("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_LOG", "1")];
const LIR_ALLOC: [(&str, &str); 2] = [("WILLOW_LIR_BACKEND", "1"), ("WILLOW_GC_STRESS", "alloc")];
const LIR_MINOR: [(&str, &str); 2] = [("WILLOW_LIR_BACKEND", "1"), ("WILLOW_GC_STRESS", "minor")];

/// The module every perspective imports. It owns the class, so it owns every
/// operation on the class: the entry file can only name the module.
const GEOM: &str = r#"
module geom;

import std::collections::Array;

pub open class Point {
    pub x: i64;
    pub y: i64;
}

pub class Point3 extends Point {
    pub z: i64;
}

pub class Tagged {
    pub label: String;
    pub at: Point;
}

pub class Bag {
    pub tags: Array<String>;
}

pub fn point(x: i64, y: i64) -> Point {
    return new Point(x, y);
}

pub fn point3(x: i64, y: i64, z: i64) -> Point3 {
    return new Point3(x, y, z);
}

pub fn sum(p: Point) -> i64 {
    return p.x + p.y;
}

pub fn sum3(p: Point3) -> i64 {
    return p.x + p.y + p.z;
}

pub fn shift(p: Point, by: i64) -> Point {
    return new Point(p.x + by, p.y + by);
}

pub fn describe(p: Point) -> String {
    return "(" + p.x.toString() + ", " + p.y.toString() + ")";
}

pub fn tagged(label: String, x: i64, y: i64) -> Tagged {
    return new Tagged(label, new Point(x, y));
}

pub fn label_of(t: Tagged) -> String {
    return t.label;
}

pub fn at_of(t: Tagged) -> Point {
    return t.at;
}

pub fn bag(a: String, b: String) -> Bag {
    return new Bag([a, b]);
}

pub fn tag_count(b: Bag) -> i64 {
    return b.tags.len();
}
"#;

/// A project made of the shared module and one entry file.
fn geom_project(main: &str) -> [(&str, &str); 2] {
    [("geom.wi", GEOM), ("main.wi", main)]
}

/// Run a project under both emitters and require identical output.
fn assert_same_output(files: &[(&str, &str)], expected: &str) {
    for env in [&AST[..], &LIR[..]] {
        let (out, ok) = compile_temp_project_with_env_and_run(files, "main.wi", env);
        assert!(ok, "project failed under {env:?}: {out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
}

/// Compile once with the selection log on and require the walker to have taken
/// each named function. Without this a coverage regression would still print
/// the right answer — from the AST emitter, which is exactly the silent
/// fallback this bead is about.
fn assert_walker_compiled(files: &[(&str, &str)], functions: &[&str]) {
    let (ok, stderr) = compile_temp_project_with_env_stderr(files, "main.wi", &LIR_LOG);
    assert!(ok, "compile failed under the walker:\n{stderr}");
    for function in functions {
        let line = format!("compiling `{function}` from lowered IR");
        assert!(
            stderr.contains(&line),
            "`{function}` did not come from the walker:\n{stderr}"
        );
    }
}

/// The inverse: the walker must NOT have taken this function. Used where
/// resolution is deliberately refused, so that the refusal is a tested
/// decision rather than an accident of some later change.
fn assert_walker_declined(files: &[(&str, &str)], function: &str) {
    let (ok, stderr) = compile_temp_project_with_env_stderr(files, "main.wi", &LIR_LOG);
    assert!(ok, "compile failed under the walker:\n{stderr}");
    let line = format!("compiling `{function}` from lowered IR");
    assert!(
        !stderr.contains(&line),
        "`{function}` was expected to fall back to the AST emitter:\n{stderr}"
    );
}

/// The two assertions almost every perspective wants: the emitters agree, and
/// the walker is what produced the second copy of the answer.
fn assert_module_class(files: &[(&str, &str)], expected: &str, functions: &[&str]) {
    assert_same_output(files, expected);
    assert_walker_compiled(files, functions);
}

#[test]
fn module_class_01_bound_then_passed_back() {
    let files = geom_project(
        r#"
import geom;

fn main() {
    let p = geom::point(3, 4);
    println(geom::sum(p));
}
"#,
    );
    assert_module_class(&files, "7\n", &["main"]);
}

#[test]
fn module_class_02_never_bound_at_all() {
    // The other shape of the same defect: with no `let`, the rejected type is
    // the static call's own, not a local's.
    let files = geom_project(
        r#"
import geom;

fn main() {
    println(geom::sum(geom::point(3, 4)));
}
"#,
    );
    assert_module_class(&files, "7\n", &["main"]);
}

#[test]
fn module_class_03_item_import_still_resolves() {
    // The path that always worked: an item import copies the module's tables
    // under the short name, so resolution finds it directly and never consults
    // the modules at all.
    let files = geom_project(
        r#"
import geom;
import geom::Point;

fn main() {
    let p: Point = geom::point(3, 4);
    println(geom::sum(p));
    println(p.x);
}
"#,
    );
    assert_module_class(&files, "7\n3\n", &["main"]);
}

#[test]
fn module_class_04_reassigned() {
    let files = geom_project(
        r#"
import geom;

fn main() {
    let mut p = geom::point(1, 1);
    p = geom::shift(p, 5);
    println(geom::sum(p));
}
"#,
    );
    assert_module_class(&files, "12\n", &["main"]);
}

#[test]
fn module_class_05_array_of_them_is_indexed() {
    // The element type is the same bare name, reached through `Array<T>`
    // rather than directly, so the array's own eligibility had to resolve it.
    let files = geom_project(
        r#"
import geom;

fn main() {
    let all = [geom::point(1, 2), geom::point(3, 4)];
    println(geom::sum(all[0]) + geom::sum(all[1]));
}
"#,
    );
    assert_module_class(&files, "10\n", &["main"]);
}

#[test]
fn module_class_06_returned_from_a_lambda() {
    // A lambda is compiled as its own function, so it resolves the name on its
    // own terms — and its return type is what the walker vets first.
    let files = geom_project(
        r#"
import geom;

fn main() {
    let make = || geom::point(6, 6);
    println(geom::sum(make()));
}
"#,
    );
    assert_module_class(&files, "12\n", &["main", "$lambda.0"]);
}

#[test]
fn module_class_07_reassigned_in_a_while_loop() {
    let files = geom_project(
        r#"
import geom;

fn main() {
    let mut p = geom::point(0, 0);
    let mut i = 0;
    while i < 4 {
        p = geom::shift(p, i);
        i = i + 1;
    }
    println(geom::sum(p));
}
"#,
    );
    assert_module_class(&files, "12\n", &["main"]);
}

#[test]
fn module_class_08_in_a_second_entry_function() {
    let files = geom_project(
        r#"
import geom;

fn twice(n: i64) -> i64 {
    let p = geom::point(n, n);
    return geom::sum(p);
}

fn main() {
    println(twice(5));
}
"#,
    );
    assert_module_class(&files, "10\n", &["main", "twice"]);
}

#[test]
fn module_class_09_two_live_across_an_allocation() {
    // Both objects are still reachable when `describe` allocates its String,
    // so both must be GC roots for the whole expression.
    let files = geom_project(
        r#"
import geom;

fn main() {
    let a = geom::point(2, 2);
    let b = geom::shift(a, 8);
    println(geom::describe(a) + " -> " + geom::describe(b));
}
"#,
    );
    assert_module_class(&files, "(2, 2) -> (10, 10)\n", &["main"]);
}

#[test]
fn module_class_10_string_field_read_through_the_module() {
    let files = geom_project(
        r#"
import geom;

fn main() {
    let t = geom::tagged("origin", 0, 7);
    println(geom::label_of(t));
}
"#,
    );
    assert_module_class(&files, "origin\n", &["main"]);
}

#[test]
fn module_class_11_field_of_the_same_kind() {
    // Resolving the outer name is only the start: the walker then vets the
    // field types it found, and one of them is another module class.
    let files = geom_project(
        r#"
import geom;

fn main() {
    let t = geom::tagged("origin", 3, 4);
    println(geom::sum(geom::at_of(t)));
}
"#,
    );
    assert_module_class(&files, "7\n", &["main"]);
}

#[test]
fn module_class_12_array_field() {
    // A field the walker admits only by walking into it — `Array<String>` is
    // supported, so the class that holds one is too.
    let files = geom_project(
        r#"
import geom;

fn main() {
    let b = geom::bag("a", "b");
    println(geom::tag_count(b));
}
"#,
    );
    assert_module_class(&files, "2\n", &["main"]);
}

#[test]
fn module_class_13_two_modules_two_class_names() {
    let files = [
        (
            "left.wi",
            r#"
module left;

pub class L { pub n: i64; }

pub fn make(n: i64) -> L { return new L(n); }
pub fn get(l: L) -> i64 { return l.n; }
"#,
        ),
        (
            "right.wi",
            r#"
module right;

pub class R { pub n: i64; }

pub fn make(n: i64) -> R { return new R(n * 2); }
pub fn get(r: R) -> i64 { return r.n; }
"#,
        ),
        (
            "main.wi",
            r#"
import left;
import right;

fn main() {
    let l = left::make(3);
    let r = right::make(3);
    println(left::get(l) + right::get(r));
}
"#,
        ),
    ];
    assert_module_class(&files, "9\n", &["main"]);
}

#[test]
fn module_class_14_two_modules_one_class_name_is_refused() {
    // Two DIFFERENT classes answer to `Point`, and nothing in the entry file
    // says which one a local holds. The walker must decline rather than pick:
    // the wrong layout would be silently wrong code, while the fallback is
    // only slower. The AST emitter still prints the right answer.
    let files = [
        (
            "one.wi",
            r#"
module one;

pub class Point { pub x: i64; }

pub fn make(x: i64) -> Point { return new Point(x); }
pub fn get(p: Point) -> i64 { return p.x; }
"#,
        ),
        (
            "two.wi",
            r#"
module two;

pub class Point { pub a: i64; pub b: i64; }

pub fn make(a: i64) -> Point { return new Point(a, a * 3); }
pub fn get(p: Point) -> i64 { return p.a + p.b; }
"#,
        ),
        (
            "main.wi",
            r#"
import one;
import two;

fn main() {
    let p = one::make(4);
    let q = two::make(5);
    println(one::get(p));
    println(two::get(q));
}
"#,
        ),
    ];
    assert_same_output(&files, "4\n20\n");
    assert_walker_declined(&files, "main");
}

#[test]
fn module_class_15_an_entry_class_of_the_same_name_wins() {
    // A name the entry program's own tables already answer to is never
    // re-resolved through the modules: `Point` here is this file's `Point`.
    let files = [
        (
            "geom.wi",
            r#"
module geom;

pub class Point { pub x: i64; }

pub fn make(x: i64) -> Point { return new Point(x); }
"#,
        ),
        (
            "main.wi",
            r#"
import geom;

class Point {
    pub w: i64;
}

fn main() {
    let mine = new Point(7);
    println(mine.w);
}
"#,
        ),
    ];
    assert_module_class(&files, "7\n", &["main"]);
}

#[test]
fn module_class_16_import_alias() {
    // Resolution is keyed on the ACCESS name, which is what the class tables
    // were registered under, so an alias resolves exactly like the real name.
    let files = geom_project(
        r#"
import geom as g;

fn main() {
    let p = g::point(3, 4);
    println(g::sum(p));
}
"#,
    );
    assert_module_class(&files, "7\n", &["main"]);
}

#[test]
fn module_class_17_transitively_imported_module() {
    // The entry file imports `relay`, which imports `geom`. The class is still
    // `geom`'s, and `geom` is still a compiled unit, so its tables are there
    // to be found even though this file never named it.
    let files = [
        ("geom.wi", GEOM),
        (
            "relay.wi",
            r#"
module relay;

import geom;
import geom::Point;

pub fn origin(n: i64) -> Point {
    return geom::point(n, n);
}

pub fn total(p: Point) -> i64 {
    return geom::sum(p);
}
"#,
        ),
        (
            "main.wi",
            r#"
import relay;

fn main() {
    let p = relay::origin(6);
    println(relay::total(p));
}
"#,
        ),
    ];
    assert_module_class(&files, "12\n", &["main"]);
}

#[test]
fn module_class_18_inside_a_deferred_statement() {
    let files = geom_project(
        r#"
import geom;

fn main() {
    defer println(geom::sum(geom::point(1, 9)));
    println(geom::sum(geom::point(2, 2)));
}
"#,
    );
    assert_module_class(&files, "4\n10\n", &["main"]);
}

#[test]
fn module_class_19_under_allocation_stress() {
    // A collection at every allocation: if a module class local were not
    // rooted, the object would be reclaimed while the entry function still
    // held it.
    let files = geom_project(
        r#"
import geom;

fn main() {
    let a = geom::point(1, 2);
    let b = geom::tagged("t", 3, 4);
    let c = geom::bag("x", "y");
    println(geom::sum(a) + geom::sum(geom::at_of(b)) + geom::tag_count(c));
    println(geom::label_of(b));
}
"#,
    );
    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &LIR_ALLOC);
    assert!(ok, "project failed under allocation stress: {out}");
    assert_eq!(out, "12\nt\n");
}

#[test]
fn module_class_20_under_minor_collection_stress() {
    let files = geom_project(
        r#"
import geom;

fn main() {
    let mut p = geom::point(0, 0);
    let mut i = 0;
    while i < 6 {
        p = geom::shift(p, 1);
        i = i + 1;
    }
    println(geom::describe(p));
}
"#,
    );
    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &LIR_MINOR);
    assert!(ok, "project failed under minor-collection stress: {out}");
    assert_eq!(out, "(6, 6)\n");
}

#[test]
fn module_class_21_inside_async_functions() {
    // A class value cannot LIVE in an async frame (it is not `Send`), but it
    // can be a temporary inside one, and that temporary's type is vetted the
    // same way.
    let files = geom_project(
        r#"
import geom;

async fn later(n: i64) -> i64 {
    return geom::sum(geom::point(n, n));
}

async fn main() {
    println(await later(4));
}
"#,
    );
    assert_same_output(&files, "8\n");
    // Both bodies are async, and the walker logs those with the `async`
    // prefix, so neither is asserted through [`assert_walker_compiled`].
    let (ok, stderr) = compile_temp_project_with_env_stderr(&files, "main.wi", &LIR_LOG);
    assert!(ok, "compile failed under the walker:\n{stderr}");
    for function in ["later", "main"] {
        let line = format!("compiling async `{function}` from lowered IR");
        assert!(
            stderr.contains(&line),
            "async `{function}` did not come from the walker:\n{stderr}"
        );
    }
}

#[test]
fn module_class_22_release_build_agrees() {
    // Release drops the debug instrumentation but not the resolution: the same
    // program, the same answer, with the walker still taking the body.
    let files = geom_project(
        r#"
import geom;

fn main() {
    let p = geom::point(3, 4);
    println(geom::sum(p));
    println(geom::describe(p));
}
"#,
    );
    assert_module_class(&files, "7\n(3, 4)\n", &["main"]);
    let (out, ok) = compile_temp_project_and_run(&files, "main.wi");
    assert!(ok, "release-shaped default build failed: {out}");
    assert_eq!(out, "7\n(3, 4)\n");
}

#[test]
fn module_class_23_both_branches_of_an_if() {
    // The two branches produce the same class through different calls, and the
    // local outlives the join.
    let files = geom_project(
        r#"
import geom;

fn main() {
    let mut p = geom::point(0, 0);
    if geom::sum(p) == 0 {
        p = geom::point(2, 3);
    } else {
        p = geom::point(9, 9);
    }
    println(geom::sum(p));
}
"#,
    );
    assert_module_class(&files, "5\n", &["main"]);
}

#[test]
fn module_class_24_a_module_subclass_value() {
    // A subclass declared in the module is a second bare name to resolve, and
    // its layout starts with its base's.
    let files = geom_project(
        r#"
import geom;

fn main() {
    let p = geom::point3(1, 2, 3);
    println(geom::sum3(p));
}
"#,
    );
    assert_module_class(&files, "6\n", &["main"]);
}

#[test]
fn module_class_25_the_example_project() {
    let files = [
        (
            "geom.wi",
            include_str!("../../example/module_class_values/geom.wi"),
        ),
        (
            "main.wi",
            include_str!("../../example/module_class_values/main.wi"),
        ),
    ];
    assert_module_class(
        &files,
        "7\n12\n10\n12\n(2, 2) -> (10, 10)\norigin\n7\n12\n",
        &["main", "ladder", "$lambda.0"],
    );
}
