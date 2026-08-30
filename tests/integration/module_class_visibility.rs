//! A bare module class name is resolved against the modules the file being
//! compiled can SEE, and only then against every module the build declared
//! (willow-vtlr).
//!
//! An entry file that says `import shapes;` names a module class by the bare
//! name the module declared — `shapes::make(1, 2)` is typed `Point` — while the
//! tables the module unit contributed are keyed `shapes::Point`. `resolve_class_key`
//! bridges that by retrying the bare name once per known module. `known_modules`
//! is every module the WHOLE build declared, though, so a second module that
//! happens to declare a class of the same name produced two candidates with
//! different type_ids and the name resolved to nothing:
//!
//!     `let p` binds type `Point`, outside the walker's subset
//!
//! No wrong code — the body just fell back off the LIR walker — but an
//! UNIMPORTED module is the wrong reason to lose a lowering. The retry now runs
//! over the modules the unit's own `import`s name first, and only falls back to
//! the all-modules scan when those say nothing, so nothing that resolved before
//! stops resolving.
//!
//! Every test here is differential: a fallback is not observable in a program's
//! output, so the evidence is `WILLOW_LIR_REQUIRE=1` (which turns a fallback
//! into a hard error) and `WILLOW_LIR_LOG=1`, alongside the two emitters
//! agreeing on what the program prints.

use super::support::*;

const LIR_ON: &[(&str, &str)] = &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")];
const LIR_OFF: &[(&str, &str)] = &[("WILLOW_LIR_BACKEND", "0")];
const LIR_LOG: &[(&str, &str)] = &[
    ("WILLOW_LIR_BACKEND", "1"),
    ("WILLOW_LIR_REQUIRE", "1"),
    ("WILLOW_LIR_LOG", "1"),
];
const ALLOC_STRESS: &[(&str, &str)] = &[("WILLOW_GC_STRESS", "alloc")];
const MINOR_STRESS: &[(&str, &str)] = &[("WILLOW_GC_STRESS", "minor")];

#[track_caller]
fn assert_both_backends(files: &[(&str, &str)], entry: &str, expected: &str) {
    let (lir, ok) = compile_temp_project_with_env_and_run(files, entry, LIR_ON);
    assert!(ok, "LIR build failed: {lir}");
    assert_eq!(lir, expected, "lowered-IR output mismatch");

    let (ast, ok) = compile_temp_project_with_env_and_run(files, entry, LIR_OFF);
    assert!(ok, "AST build failed: {ast}");
    assert_eq!(ast, expected, "AST output mismatch");
}

/// The module the entry file imports. Its `Point` is two `i64`s.
const SHAPES: &str = r#"
module shapes;

import std::collections::Array;

pub class Point {
    pub x: i64;
    pub y: i64;

    pub fn sum(self) -> i64 {
        return self.x + self.y;
    }
}

// A class-typed FIELD, whose declared layout says `shapes::Point` while this
// module's own bodies say `Point`.
pub class Pair {
    pub name: String;
    pub head: Point;
}

pub fn make(x: i64, y: i64) -> Point {
    return new Point(x, y);
}

pub fn sum_of(p: Point) -> i64 {
    return p.sum();
}

pub fn x_of(p: Point) -> i64 {
    return p.x;
}

pub fn shifted(p: Point, by: i64) -> Point {
    return new Point(p.x + by, p.y + by);
}

pub fn head_sum(x: i64, y: i64) -> i64 {
    let pair = new Pair("pair", new Point(x, y));
    return pair.head.sum();
}

pub fn column_sum(n: i64) -> i64 {
    let column: Array<Point> = [new Point(n, n), new Point(n, n)];
    return column[0].sum() + column[1].sum();
}

pub async fn slow_sum(x: i64, y: i64) -> i64 {
    let p = new Point(x, y);
    await yield();
    return p.sum();
}
"#;

/// The module the entry file does NOT import. Its `Point` is a different class
/// with a different layout, and it reaches the build only through `report`.
const GRID: &str = r#"
module grid;

pub class Point {
    pub row: i64;
    pub col: i64;
    pub tag: String;

    pub fn label(self) -> String {
        return self.tag + ":" + self.row.toString();
    }
}

pub fn cell(row: i64, col: i64) -> Point {
    return new Point(row, col, "cell");
}

pub fn label_of(p: Point) -> String {
    return p.label();
}

pub fn row_of(p: Point) -> i64 {
    return p.row;
}
"#;

/// A module that DOES see `grid`, so a bare `Point` in this file is grid's.
const REPORT: &str = r#"
module report;

import grid;

pub fn cell_label(row: i64, col: i64) -> String {
    let p = grid::cell(row, col);
    return grid::label_of(p);
}

pub fn cell_row(row: i64, col: i64) -> i64 {
    let p = grid::cell(row, col);
    return grid::row_of(p);
}
"#;

/// Both `Point`s in one build: the entry sees `shapes` and `report`, and `grid`
/// only through `report`.
fn two_points_project(entry: &'static str) -> Vec<(&'static str, &'static str)> {
    vec![
        ("shapes.wi", SHAPES),
        ("grid.wi", GRID),
        ("report.wi", REPORT),
        ("main.wi", entry),
    ]
}

/// The same entry file with no second `Point` in the build at all — the control
/// that says a test's fallback came from the name clash and nothing else.
fn one_point_project(entry: &'static str) -> Vec<(&'static str, &'static str)> {
    vec![("shapes.wi", SHAPES), ("main.wi", entry)]
}

const ENTRY: &str = r#"
import shapes;
import report;

fn total() -> i64 {
    let p = shapes::make(3, 4);
    return shapes::sum_of(p);
}

fn main() {
    println(total());
    println(report::cell_label(2, 5));
}
"#;

// 1. The bug: an unimported module's class of the same name used to cost the
//    entry function its lowering. `WILLOW_LIR_REQUIRE=1` turns that fallback
//    into a build error, so a clean build IS the assertion.
#[test]
fn an_unimported_module_of_the_same_class_name_no_longer_forces_a_fallback() {
    let (out, ok) =
        compile_temp_project_with_env_and_run(&two_points_project(ENTRY), "main.wi", LIR_ON);
    assert!(ok, "no body in this project may fall back: {out}");
    assert_eq!(out, "7\ncell:2\n");
}

// 2. The same entry function is named in the selection log, so the pass really
//    lowered it rather than the walker skipping the body some other way.
#[test]
fn the_entry_function_holding_a_module_class_is_lowered() {
    let (ok, log) =
        compile_temp_project_with_env_stderr(&two_points_project(ENTRY), "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    assert!(
        log.contains("[lir] compiling `total` from lowered IR"),
        "the entry function binding a module class was not lowered: {log}"
    );
}

// 3. Both emitters agree on what the program prints.
#[test]
fn both_backends_agree_with_two_point_classes_in_the_build() {
    assert_both_backends(&two_points_project(ENTRY), "main.wi", "7\ncell:2\n");
}

// 4. The layout the walker picked is `shapes`', not `grid`'s: reading `x`
//    through the module accessor answers 3, and grid's first field is a `row`.
#[test]
fn the_resolved_layout_is_the_imported_modules() {
    let entry = r#"
import shapes;
import report;

fn first() -> i64 {
    let p = shapes::make(3, 4);
    return shapes::x_of(p);
}

fn main() {
    println(first());
    println(report::cell_row(9, 1));
}
"#;
    assert_both_backends(&two_points_project(entry), "main.wi", "3\n9\n");
}

// 5. A file that really does see BOTH is still ambiguous: nothing there can say
//    which `Point` a bare name means, so the body falls back rather than being
//    compiled against one of the two layouts. No wrong code, on either emitter.
#[test]
fn a_file_that_imports_both_modules_falls_back() {
    let entry = r#"
import shapes;
import grid;

fn total() -> i64 {
    let p = shapes::make(3, 4);
    return shapes::sum_of(p);
}

fn main() {
    println(total());
    println(grid::label_of(grid::cell(2, 5)));
}
"#;
    let files = vec![("shapes.wi", SHAPES), ("grid.wi", GRID), ("main.wi", entry)];
    let (log, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", LIR_ON);
    assert!(
        !ok,
        "an ambiguous bare class name must not be lowered: {log}"
    );
    assert!(
        log.contains("fell back to the AST backend") && log.contains("`total`"),
        "the fallback should name the ambiguous function: {log}"
    );

    // ... and the program itself is unaffected: the AST emitter resolves the
    // name the same way both emitters always have.
    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", LIR_OFF);
    assert!(ok, "AST build failed: {out}");
    assert_eq!(out, "7\ncell:2\n");
}

// 6. The control for 1: with no second `Point` in the build the same entry
//    lowered before this change too.
#[test]
fn one_point_in_the_build_lowers_as_it_always_did() {
    let entry = r#"
import shapes;

fn total() -> i64 {
    let p = shapes::make(3, 4);
    return shapes::sum_of(p);
}

fn main() {
    println(total());
}
"#;
    let (out, ok) =
        compile_temp_project_with_env_and_run(&one_point_project(entry), "main.wi", LIR_ON);
    assert!(ok, "no body in this project may fall back: {out}");
    assert_eq!(out, "7\n");
}

// 7. The same, reached through an ALIAS while the other `Point` is in the
//     build: the visible set carries the alias, which is also the name the
//     module's class tables are keyed on.
#[test]
fn an_aliased_import_resolves_with_two_point_classes_present() {
    let entry = r#"
import shapes as geo;
import report;

fn total() -> i64 {
    let p = geo::make(3, 4);
    return geo::sum_of(p);
}

fn main() {
    println(total());
    println(report::cell_label(2, 5));
}
"#;
    let (out, ok) =
        compile_temp_project_with_env_and_run(&two_points_project(entry), "main.wi", LIR_ON);
    assert!(ok, "no body in this project may fall back: {out}");
    assert_eq!(out, "7\ncell:2\n");
}

// 8. An ALIASED import: the file's visible set carries the alias, which is also
//    the name the module tables are keyed on.
#[test]
fn an_aliased_module_import_still_resolves_its_class() {
    let entry = r#"
import shapes as geo;

fn total() -> i64 {
    let p = geo::make(3, 4);
    return geo::sum_of(p);
}

fn main() {
    println(total());
}
"#;
    let (out, ok) =
        compile_temp_project_with_env_and_run(&one_point_project(entry), "main.wi", LIR_ON);
    assert!(ok, "no body in this project may fall back: {out}");
    assert_eq!(out, "7\n");
}

// 9. A name the entry file's own tables answer is never re-resolved: the
//    entry's own `Point` means the entry's, whatever the build imports.
#[test]
fn an_entry_class_wins_over_a_module_class_of_the_same_name() {
    let entry = r#"
import shapes;
import report;

class Point {
    pub tag: String;
    pub weight: i64;
}

fn local() -> i64 {
    let p = new Point("own", 11);
    return p.weight;
}

fn imported() -> i64 {
    return shapes::sum_of(shapes::make(3, 4));
}

fn main() {
    println(local());
    println(imported());
    println(report::cell_row(9, 1));
}
"#;
    assert_both_backends(&two_points_project(entry), "main.wi", "11\n7\n9\n");
}

// 10. The MODULE side of the same rule: `report` sees `grid` and not `shapes`,
//     so its own bare `Point` bodies lower too.
#[test]
fn a_module_resolves_a_bare_class_from_its_own_imports() {
    let (ok, log) =
        compile_temp_project_with_env_stderr(&two_points_project(ENTRY), "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    assert!(
        log.contains("[lir] compiling `report.cell_label` from lowered IR"),
        "the intermediate module's bodies were not lowered: {log}"
    );
}

// 11. A module's own class inside its own body needs no retry at all — the
//     alias scope keys it directly — and its methods lower under the qualified
//     symbol.
#[test]
fn a_modules_own_class_methods_lower_under_qualified_symbols() {
    let (ok, log) =
        compile_temp_project_with_env_stderr(&two_points_project(ENTRY), "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    for symbol in [
        "[lir] compiling `shapes::Point::sum` from lowered IR",
        "[lir] compiling `grid::Point::label` from lowered IR",
    ] {
        assert!(log.contains(symbol), "missing {symbol}: {log}");
    }
}

// 12. Three modules declaring the same class name, only one of them visible.
#[test]
fn one_visible_module_answers_among_three_declaring_the_name() {
    let third = r#"
module chart;

pub class Point {
    pub at: i64;
}

pub fn at(n: i64) -> Point {
    return new Point(n);
}

pub fn value_of(p: Point) -> i64 {
    return p.at;
}
"#;
    let bridge = r#"
module bridge;

import chart;

pub fn charted(n: i64) -> i64 {
    let p = chart::at(n);
    return chart::value_of(p);
}
"#;
    let entry = r#"
import shapes;
import report;
import bridge;

fn total() -> i64 {
    let p = shapes::make(3, 4);
    return shapes::sum_of(p);
}

fn main() {
    println(total());
    println(bridge::charted(5));
    println(report::cell_row(6, 0));
}
"#;
    let files = vec![
        ("shapes.wi", SHAPES),
        ("grid.wi", GRID),
        ("report.wi", REPORT),
        ("chart.wi", third),
        ("bridge.wi", bridge),
        ("main.wi", entry),
    ];
    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", LIR_ON);
    assert!(ok, "no body in this project may fall back: {out}");
    assert_eq!(out, "7\n5\n6\n");
}

// 13. A module class handed back to the module it came from, nested twice: the
//     resolved key has to hold for an argument and a return in one expression.
#[test]
fn a_module_class_round_trips_through_the_entry() {
    let entry = r#"
import shapes;
import report;

fn total() -> i64 {
    let p = shapes::shifted(shapes::shifted(shapes::make(1, 2), 10), 100);
    return shapes::sum_of(p);
}

fn main() {
    println(total());
    println(report::cell_row(4, 0));
}
"#;
    assert_both_backends(&two_points_project(entry), "main.wi", "223\n4\n");
}

// 14. A class-typed FIELD inside the module, read through the module's own
//     accessor while the other `Point` is in the build.
#[test]
fn a_class_typed_field_resolves_with_two_point_classes_present() {
    let entry = r#"
import shapes;
import report;

fn main() {
    println(shapes::head_sum(6, 7));
    println(report::cell_row(1, 2));
}
"#;
    assert_both_backends(&two_points_project(entry), "main.wi", "13\n1\n");
}

// 15. An array of the module's own class, which is where a wrong layout would
//     show up as a wrong element size.
#[test]
fn an_array_of_a_module_class_resolves() {
    let entry = r#"
import shapes;
import report;

fn main() {
    println(shapes::column_sum(4));
    println(report::cell_row(3, 3));
}
"#;
    assert_both_backends(&two_points_project(entry), "main.wi", "16\n3\n");
}

// 16. An async module body holding a module class local across a suspension.
#[test]
fn an_async_module_body_resolves_its_own_class() {
    let entry = r#"
import shapes;
import report;

async fn main() {
    println(await shapes::slow_sum(3, 4));
    println(report::cell_row(8, 0));
}
"#;
    assert_both_backends(&two_points_project(entry), "main.wi", "7\n8\n");
}

// 17. Under allocation stress every object is collected the hard way, so a
//     layout resolved to the wrong class would be traced with the wrong ref
//     mask.
#[test]
fn the_resolved_layout_survives_allocation_stress() {
    let (out, ok) = compile_temp_project_with_env_and_run_under(
        &two_points_project(ENTRY),
        "main.wi",
        LIR_ON,
        ALLOC_STRESS,
    );
    assert!(ok, "alloc-stress run failed: {out}");
    assert_eq!(out, "7\ncell:2\n");
}

// 18. The same under minor-collection stress, on the AST emitter, so the two
//     paths agree about what is a GC reference.
#[test]
fn the_ast_emitter_agrees_under_minor_stress() {
    let (out, ok) = compile_temp_project_with_env_and_run_under(
        &two_points_project(ENTRY),
        "main.wi",
        LIR_OFF,
        MINOR_STRESS,
    );
    assert!(ok, "minor-stress run failed: {out}");
    assert_eq!(out, "7\ncell:2\n");
}

// 19. A release build, where the optimizer sees the lowered bodies.
#[test]
fn the_release_build_answers_the_same() {
    let project = TestProject::new(
        "module_class_visibility_release",
        &two_points_project(ENTRY),
    );
    let output = project.compile_release("main.wi");
    assert!(
        output.status.success(),
        "release build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = project.run();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "7\ncell:2\n");
}

// 20. The runnable example: two modules declare `Point`, the entry imports one.
#[test]
fn the_module_class_visibility_example_runs() {
    let (out, ok) = compile_file_and_run("example/module_class_visibility/main.wi");
    assert!(
        ok,
        "example/module_class_visibility/main.wi failed to compile or run"
    );
    assert_eq!(out, "7\n23\ncell:2,5\n7\n");
}

// 21. ... and every body in that example is lowered, which is the property this
//     change is about.
#[test]
fn the_module_class_visibility_example_is_fully_lowered() {
    let files = &[
        (
            "shapes.wi",
            include_str!("../../example/module_class_visibility/shapes.wi"),
        ),
        (
            "grid.wi",
            include_str!("../../example/module_class_visibility/grid.wi"),
        ),
        (
            "report.wi",
            include_str!("../../example/module_class_visibility/report.wi"),
        ),
        (
            "main.wi",
            include_str!("../../example/module_class_visibility/main.wi"),
        ),
    ];
    let (out, ok) = compile_temp_project_with_env_and_run(files, "main.wi", LIR_ON);
    assert!(ok, "no body in the example may fall back: {out}");
    assert_eq!(out, "7\n23\ncell:2,5\n7\n");
}
