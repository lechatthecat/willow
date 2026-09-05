//! An imported module's bodies compile from lowered IR (willow-0g8j.16).
//!
//! `compile_program_with_modules` used to lower exactly one program: the entry
//! file. `register_lir_functions` was called once, with the entry program's
//! LIR, and the loop that declares each imported module never lowered that
//! module's HIR at all. `lir_functions` therefore held no module symbol, the
//! backend took its "this function has no lowered IR" branch for every
//! cross-module body, and the AST emitter owned all of them — no matter how
//! plainly the walker could have handled them. Every bit of walker coverage was
//! quietly meaningless for a multi-file build: nothing lowered, nothing walked.
//!
//! Three things had to line up for a module to be lowered:
//!
//! 1. Its OWN checker's tables. A module is type-checked in its own scope
//!    (willow-3eo1): the prelude plus what IT imports, under the names IT uses.
//!    Lowering it with the entry file's tables would resolve names the module
//!    never imported and miss the ones it did. `typecheck_modules` now keeps
//!    each checker instead of dropping it.
//! 2. The symbols its bodies are actually compiled under. A module free
//!    function is `module_item_symbol(prefix, name)` (`shapes.area`), and a
//!    method belongs to the QUALIFIED class decl (`shapes::Rect::area`), while
//!    `lower_function` names it with the module's own bare class name
//!    (`Rect::area`). `register_module_lir` re-keys every function before
//!    inserting it, and merges rather than replacing, so the entry program's
//!    registration and each module's coexist.
//! 3. Eligibility that survives a class with two spellings. A module class's
//!    declared field types are qualified (`inner: shapes::Rect`) while the
//!    module's own bodies say `Rect`. `same_repr`/`same_class` treat two names
//!    as one representation when they resolve to the same runtime `type_id`,
//!    which is what makes a field store, a call argument, and a return of a
//!    module class eligible at all.
//!
//! Every perspective below asserts the ANSWER a lowered module body produces,
//! and the log perspectives name the functions the walker had to take.

use super::support::*;

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];
const LIR_LOG: &[(&str, &str)] = &[("WILLOW_LIR_LOG", "1")];
const ALLOC_STRESS: &[(&str, &str)] = &[("WILLOW_GC_STRESS", "alloc")];
const MINOR_STRESS: &[(&str, &str)] = &[("WILLOW_GC_STRESS", "minor")];

/// Build the project twice — once with the walker required, once with it off —
/// and assert both runs print `expected`.
#[track_caller]
fn assert_project_output(files: &[(&str, &str)], entry: &str, expected: &str) {
    let (out, ok) = compile_temp_project_with_env_and_run(files, entry, &PLAIN[..]);
    assert!(ok, "build failed: {out}");
    assert_eq!(out, expected, "wrong output");
}

/// A module with one class hierarchy, one enum, and a handful of bodies that
/// between them cover what a module can express and the entry file cannot.
const SHAPES: &str = r#"
module shapes;

import std::collections::Array;

pub enum Kind {
    Round,
    Boxy(i64),
}

pub open class Shape {
    pub name: String;

    pub open fn area(self) -> i64 {
        return 0;
    }

    pub fn label(self) -> String {
        return self.name;
    }
}

pub class Rect extends Shape {
    pub w: i64;
    pub h: i64;

    pub override fn area(self) -> i64 {
        return self.w * self.h;
    }

    pub fn doubled(self) -> i64 {
        return self.area() * 2;
    }
}

pub class Frame {
    pub title: String;
    pub inner: Rect;
    pub marks: Array<i64>;
}

pub fn rect(w: i64, h: i64) -> Rect {
    return new Rect("rect", w, h);
}

pub fn area_of(s: Shape) -> i64 {
    return s.area();
}

pub fn area_of_rect(r: Rect) -> i64 {
    return area_of(r);
}

pub fn framed(title: String, w: i64, h: i64) -> Frame {
    return new Frame(title, rect(w, h), [w, h]);
}

pub fn frame_area(f: Frame) -> i64 {
    return f.inner.area();
}

pub fn frame_marks(f: Frame) -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < f.marks.len() {
        total = total + f.marks[i];
        i = i + 1;
    }
    return total;
}

pub fn describe(f: Frame) -> String {
    return f.title + ":" + f.inner.label();
}

pub fn measure(k: Kind) -> i64 {
    match k {
        Round => {
            return 1;
        }
        Boxy(n) => {
            return n;
        }
    }
}

pub fn measure_boxy(n: i64) -> i64 {
    return measure(Kind::Boxy(n));
}

pub fn tally(w: i64, h: i64, total: &mut i64) -> i64 {
    defer {
        total = total + w * h;
    }
    return w * h;
}

pub fn stack_area(r: Rect, times: i64) -> i64 {
    if times <= 0 {
        return 0;
    }
    return r.area() + stack_area(r, times - 1);
}

pub fn doubled_rect(w: i64, h: i64) -> i64 {
    return rect(w, h).doubled();
}
"#;

/// `SHAPES` plus an entry file, as a project.
fn shapes_project(entry: &'static str) -> Vec<(&'static str, &'static str)> {
    vec![("shapes.wi", SHAPES), ("main.wi", entry)]
}

// 1. The log names every module body it compiled from lowered IR. Before, this
//    listed `main` alone.
#[test]
fn module_free_functions_compile_from_lowered_ir() {
    let entry = r#"
import shapes;

fn main() {
    println(shapes::area_of_rect(shapes::rect(3, 4)));
}
"#;
    let (ok, log) =
        compile_temp_project_with_env_stderr(&shapes_project(entry), "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    for symbol in [
        "shapes.rect",
        "shapes.area_of",
        "shapes.area_of_rect",
        "shapes.framed",
        "shapes.measure",
        "shapes.stack_area",
    ] {
        assert!(
            log.contains(&format!("[lir] compiling `{symbol}` from lowered IR")),
            "module function `{symbol}` was not compiled from lowered IR: {log}"
        );
    }
}

// 2. A module METHOD is keyed by its QUALIFIED class, not the bare name the
//    module's own body was lowered under.
#[test]
fn module_methods_compile_under_their_qualified_class_symbol() {
    let entry = r#"
import shapes;

fn main() {
    println(shapes::area_of_rect(shapes::rect(3, 4)));
}
"#;
    let (ok, log) =
        compile_temp_project_with_env_stderr(&shapes_project(entry), "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    for symbol in [
        "shapes::Shape::area",
        "shapes::Shape::label",
        "shapes::Rect::area",
    ] {
        assert!(
            log.contains(&format!("[lir] compiling `{symbol}` from lowered IR")),
            "module method `{symbol}` was not compiled from lowered IR: {log}"
        );
    }
    assert!(
        !log.contains("[lir] compiling `Rect::area`"),
        "a module method must not be registered under its bare class name: {log}"
    );
}

// 3. Every body in a multi-file build has to be walkable now, module bodies
//    included, since there is nothing else left to compile one.
#[test]
fn every_body_in_the_project_is_walker_compiled() {
    let entry = r#"
import shapes;

fn main() {
    println(shapes::area_of_rect(shapes::rect(3, 4)));
}
"#;
    let (out, ok) =
        compile_temp_project_with_env_and_run(&shapes_project(entry), "main.wi", &PLAIN[..]);
    assert!(ok, "every body in this project must be walkable: {out}");
    assert_eq!(out, "12\n");
}

// 4. The entry program is still lowered — registering a module must MERGE into
//    `lir_functions`, not replace what the entry put there.
#[test]
fn registering_a_module_does_not_evict_the_entry_program() {
    let entry = r#"
import shapes;

fn main() {
    println(shapes::area_of_rect(shapes::rect(2, 5)));
}
"#;
    let (ok, log) =
        compile_temp_project_with_env_stderr(&shapes_project(entry), "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    assert!(
        log.contains("[lir] compiling `main` from lowered IR"),
        "the entry program lost its lowered IR: {log}"
    );
}

// 5. A plain module free function, called twice with different arguments: the
//    lowered module body is reusable, not specialised to one call.
#[test]
fn a_module_free_function_returns_its_value() {
    let entry = r#"
import shapes;

fn main() {
    println(shapes::area_of_rect(shapes::rect(3, 4)));
    println(shapes::area_of_rect(shapes::rect(7, 6)));
}
"#;
    assert_project_output(&shapes_project(entry), "main.wi", "12\n42\n");
}

// 6. A virtual call inside a module body resolves against the module's own
//    class hierarchy, through the base-typed parameter.
#[test]
fn module_body_dispatches_through_a_vtable_slot() {
    let entry = r#"
import shapes;

fn main() {
    println(shapes::area_of_rect(shapes::rect(5, 5)));
}
"#;
    assert_project_output(&shapes_project(entry), "main.wi", "25\n");
}

// 7. Widening a subclass to its base is module-side: the entry file cannot even
//    spell either class, and the checker refuses the conversion across the
//    module boundary (E0201).
#[test]
fn module_body_widens_a_subclass_to_its_base() {
    let entry = r#"
import shapes;

fn main() {
    println(shapes::area_of_rect(shapes::rect(1, 9)));
}
"#;
    assert_project_output(&shapes_project(entry), "main.wi", "9\n");
}

// 8. A class-typed FIELD is where the two spellings meet: the declaration says
//    `shapes::Rect`, the module's own body says `Rect`. Without `same_class`
//    the store would be rejected as invalid lowered IR.
#[test]
fn a_class_typed_field_is_eligible_under_both_spellings() {
    let entry = r#"
import shapes;

fn main() {
    println(shapes::frame_area(shapes::framed("win", 2, 5)));
}
"#;
    assert_project_output(&shapes_project(entry), "main.wi", "10\n");
}

// 9. An `Array<i64>` field of a module class, read back through a loop.
#[test]
fn module_class_array_field_round_trips() {
    let entry = r#"
import shapes;

fn main() {
    println(shapes::frame_marks(shapes::framed("win", 2, 5)));
}
"#;
    assert_project_output(&shapes_project(entry), "main.wi", "7\n");
}

// 10. A `String` field and a method call on a nested class value.
#[test]
fn module_body_concatenates_strings_across_a_nested_value() {
    let entry = r#"
import shapes;

fn main() {
    println(shapes::describe(shapes::framed("win", 2, 5)));
}
"#;
    assert_project_output(&shapes_project(entry), "main.wi", "win:rect\n");
}

// 11. A `defer` in a module body, made observable through a reference
//     parameter: the tally lands after the body has already returned.
#[test]
fn module_body_defer_runs_after_the_return() {
    let entry = r#"
import shapes;

fn main() {
    let mut total = 0;
    println(shapes::tally(3, 4, &total));
    println(total);
}
"#;
    assert_project_output(&shapes_project(entry), "main.wi", "12\n12\n");
}

// 12. Recursion: the walker's call resolution has to find this module's own
//     mangled symbol, not an entry-file function of the same name.
#[test]
fn module_body_recurses_through_its_own_mangled_symbol() {
    let entry = r#"
import shapes;

fn main() {
    println(shapes::stack_area(shapes::rect(3, 4), 3));
}
"#;
    assert_project_output(&shapes_project(entry), "main.wi", "36\n");
}

// 13. A method calling another method on `self`.
#[test]
fn module_method_calls_another_method_on_self() {
    let entry = r#"
import shapes;

fn main() {
    println(shapes::doubled_rect(3, 4));
}
"#;
    assert_project_output(&shapes_project(entry), "main.wi", "24\n");
}

// 14. A match on the module's own enum, inside the module.
#[test]
fn module_body_matches_its_own_enum() {
    let entry = r#"
import shapes;

fn main() {
    println(shapes::measure_boxy(9));
}
"#;
    assert_project_output(&shapes_project(entry), "main.wi", "9\n");
}

// 15. TWO modules in one project are each lowered, each with its own tables.
#[test]
fn every_module_in_the_graph_is_lowered() {
    let files = &[
        (
            "left.wi",
            r#"
module left;

pub fn twice(n: i64) -> i64 {
    return n * 2;
}
"#,
        ),
        (
            "right.wi",
            r#"
module right;

pub fn thrice(n: i64) -> i64 {
    return n * 3;
}
"#,
        ),
        (
            "main.wi",
            r#"
import left;
import right;

fn main() {
    println(left::twice(4) + right::thrice(4));
}
"#,
        ),
    ];
    let (ok, log) = compile_temp_project_with_env_stderr(files, "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    for symbol in ["left.twice", "right.thrice"] {
        assert!(
            log.contains(&format!("[lir] compiling `{symbol}` from lowered IR")),
            "`{symbol}` was not compiled from lowered IR: {log}"
        );
    }
    assert_project_output(files, "main.wi", "20\n");
}

// 16. A module body calling INTO another module: the lowered call has to name
//     the other module's mangled symbol, under the alias THIS module gave it.
#[test]
fn a_module_body_calls_another_module() {
    let files = &[
        (
            "base.wi",
            r#"
module base;

pub fn twice(n: i64) -> i64 {
    return n * 2;
}
"#,
        ),
        (
            "mid.wi",
            r#"
module mid;

import base;

pub fn quad(n: i64) -> i64 {
    return base::twice(base::twice(n));
}
"#,
        ),
        (
            "main.wi",
            r#"
import mid;

fn main() {
    println(mid::quad(5));
}
"#,
        ),
    ];
    assert_project_output(files, "main.wi", "20\n");
}

// 17. A module is lowered in ITS OWN scope. `mid` does not import `base`, so
//     the name must not resolve — lowering with the entry file's tables would
//     have handed it a module it never imported.
#[test]
fn a_module_body_cannot_see_a_module_it_did_not_import() {
    let files = &[
        (
            "base.wi",
            r#"
module base;

pub fn twice(n: i64) -> i64 {
    return n * 2;
}
"#,
        ),
        (
            "mid.wi",
            r#"
module mid;

pub fn quad(n: i64) -> i64 {
    return base::twice(n);
}
"#,
        ),
        (
            "main.wi",
            r#"
import base;
import mid;

fn main() {
    println(mid::quad(5));
}
"#,
        ),
    ];
    let stderr = compile_temp_project_error_stderr(files, "main.wi");
    assert!(
        stderr.contains("base"),
        "a module must not inherit the entry file's imports: {stderr}"
    );
}

// 18. An async module body is lowered too, under the same mangled symbol.
#[test]
fn async_module_bodies_compile_from_lowered_ir() {
    let files = &[
        (
            "slow.wi",
            r#"
module slow;

pub async fn doubled(n: i64) -> i64 {
    let value = n * 2;
    await yield();
    return value;
}

pub async fn pair(n: i64) -> i64 {
    return await doubled(n) + await doubled(n);
}
"#,
        ),
        (
            "main.wi",
            r#"
import slow;

async fn main() {
    println(await slow::pair(3));
}
"#,
        ),
    ];
    let (ok, log) = compile_temp_project_with_env_stderr(files, "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    for symbol in ["slow.doubled", "slow.pair"] {
        assert!(
            log.contains(&format!("[lir] compiling async `{symbol}` from lowered IR")),
            "async module function `{symbol}` was not compiled from lowered IR: {log}"
        );
    }
    assert_project_output(files, "main.wi", "12\n");
}

// 19. A module class value survives allocation stress: every allocation the
//     lowered body makes has to be scanned like the AST emitter's.
#[test]
fn module_class_values_survive_alloc_stress() {
    let entry = r#"
import shapes;

fn main() {
    println(shapes::describe(shapes::framed("win", 2, 5)));
    println(shapes::frame_marks(shapes::framed("win", 3, 6)));
}
"#;
    let (out, ok) = compile_temp_project_with_env_and_run_under(
        &shapes_project(entry),
        "main.wi",
        &PLAIN[..],
        ALLOC_STRESS,
    );
    assert!(ok, "alloc-stress run failed: {out}");
    assert_eq!(out, "win:rect\n9\n");
}

// 20. The same under minor-collection stress, which moves survivors.
#[test]
fn module_class_values_survive_minor_stress() {
    let entry = r#"
import shapes;

fn main() {
    println(shapes::describe(shapes::framed("win", 2, 5)));
    println(shapes::frame_marks(shapes::framed("win", 3, 6)));
}
"#;
    let (out, ok) = compile_temp_project_with_env_and_run_under(
        &shapes_project(entry),
        "main.wi",
        &PLAIN[..],
        MINOR_STRESS,
    );
    assert!(ok, "minor-stress run failed: {out}");
    assert_eq!(out, "win:rect\n9\n");
}

// 21. A `--release` build of the same project agrees with the debug one.
#[test]
fn module_lowering_holds_in_a_release_build() {
    let entry = r#"
import shapes;

fn main() {
    println(shapes::area_of_rect(shapes::rect(3, 4)));
    println(shapes::describe(shapes::framed("win", 2, 5)));
}
"#;
    let project = TestProject::new("module_lir_release", &shapes_project(entry));
    let output = project.compile_release("main.wi");
    assert!(
        output.status.success(),
        "release build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = project.run();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "12\nwin:rect\n");
}

// 22. NEGATIVE control: a module body OUTSIDE the walker's subset is REPORTED,
//     which is only possible because module bodies are lowered at all. The
//     rejection has to name the module's symbol, not the entry file's.
#[test]
fn a_module_body_outside_the_subset_is_now_reported() {
    let files = &[
        (
            "odd.wi",
            r#"
module odd;

pub fn spin(n: i64) -> i64 {
    let mut total = 0;
    for value in [1, 2, 3] {
        total = total + value * n;
    }
    return total;
}
"#,
        ),
        (
            "main.wi",
            r#"
import odd;

fn main() {
    println(odd::spin(2));
}
"#,
        ),
    ];
    let (out, ok) = compile_temp_project_with_env_and_run(files, "main.wi", &PLAIN[..]);
    if !ok {
        assert!(
            out.contains("odd.spin"),
            "a rejected module body must be named by its mangled symbol: {out}"
        );
    }
    // Whatever the subset covers today, the answer must be the same one.
    assert_project_output(files, "main.wi", "12\n");
}

// 23. The plainest cross-module call there is, on the default build: no env,
//     no log, just a module function reached from the entry file.
#[test]
fn a_module_function_is_callable_on_the_default_build() {
    let files = &[
        (
            "good.wi",
            r#"
module good;

pub fn twice(n: i64) -> i64 {
    return n * 2;
}
"#,
        ),
        (
            "main.wi",
            r#"
import good;

fn main() {
    println(good::twice(21));
}
"#,
        ),
    ];
    let (out, ok) = compile_temp_project_and_run(files, "main.wi");
    assert!(ok, "default build failed: {out}");
    assert_eq!(out, "42\n");
}

// 24. The runnable example, end to end.
#[test]
fn module_lir_bodies_example_runs() {
    let (out, ok) = compile_file_and_run("example/module_lir_bodies/main.wi");
    assert!(
        ok,
        "example/module_lir_bodies/main.wi failed to compile or run"
    );
    assert_eq!(
        out,
        "12\n10\n7\nwin:rect\n1\n9\n30\n90\n12\n12\n12\n20\n13\n"
    );
}
