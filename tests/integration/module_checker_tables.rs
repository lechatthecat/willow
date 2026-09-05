//! A module body is emitted with its OWN checker's tables (willow-9vvn).
//!
//! `compile_program_with_modules` registers the backend's span-keyed
//! resolution tables — `enum_variant_resolutions`, `pattern_resolutions`,
//! `expr_types`, `static_call_classes` — exactly once, from the ENTRY file's
//! checker. But a module is type-checked
//! in its own scope by `typecheck_modules` (willow-3eo1), so everything those
//! passes resolved inside a module body lived in a checker the backend never
//! saw. Module bodies were emitted against an empty resolution table.
//!
//! The visible damage was in `match`. An unqualified `Boxy(n)` arm is parsed as
//! a tuple pattern and only the checker knows it names an enum variant rather
//! than a class; unresolved, it stayed a `ClassDowncast` and either selected the
//! wrong arm or tripped `emit_match`'s "checked downcast pattern class `Boxy`
//! has no type id" panic, depending on arm order:
//!
//!     module k;
//!     pub enum Kind { Round, Boxy(i64) }
//!     pub fn measure(x: Kind) -> i64 {
//!         match x { Round => { return 1; } Boxy(n) => { return n; } }
//!     }
//!
//! `measure(Kind::Boxy(9))` printed `1`. With the arms swapped, the compiler
//! panicked instead.
//!
//! The fix merges each module checker's tables into the backend rather than
//! replacing them. Merging is what a `Span` makes safe: it carries the
//! `file_id` of the file it came from, so a module's keys can never collide
//! with the entry file's or with another module's.
//!
//! The LIR walker was already correct here — willow-0g8j.16 lowers each module
//! with its own `CheckerTables` — so every perspective below pins the ANSWER a
//! module's own tables produce, which is what the divergence used to corrupt.

use super::support::*;

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];
const ALLOC_STRESS: &[(&str, &str)] = &[("WILLOW_GC_STRESS", "alloc")];
const MINOR_STRESS: &[(&str, &str)] = &[("WILLOW_GC_STRESS", "minor")];

#[track_caller]
fn assert_project_output(files: &[(&str, &str)], entry: &str, expected: &str) {
    let (out, ok) = compile_temp_project_with_env_and_run(files, entry, &PLAIN[..]);
    assert!(ok, "build failed: {out}");
    assert_eq!(out, expected, "wrong output");
}

/// The bead's own repro module, plus everything else a module body can ask its
/// checker to resolve.
const KINDS: &str = r#"
module kinds;

pub enum Kind {
    Round,
    Boxy(i64),
}

pub enum Tag {
    Named(String),
    Blank,
}

pub interface Speak {
    fn say(self) -> i64;
}

pub class Dog implements Speak {
    pub fn say(self) -> i64 {
        return 7;
    }
}

pub class Cat implements Speak {
    pub fn say(self) -> i64 {
        return 9;
    }
}

// The bead's repro: the unit arm comes first, so an unresolved payload pattern
// silently selects THIS arm.
pub fn measure(x: Kind) -> i64 {
    match x {
        Round => {
            return 1;
        }
        Boxy(n) => {
            return n;
        }
    }
}

// The same match with the arms swapped: an unresolved payload pattern panics
// in `emit_match` here instead of quietly answering wrong.
pub fn measure_payload_first(x: Kind) -> i64 {
    match x {
        Boxy(n) => {
            return 100 + n;
        }
        Round => {
            return 200;
        }
    }
}

pub fn measure_wild(x: Kind) -> i64 {
    match x {
        Boxy(n) => {
            return n;
        }
        _ => {
            return 0;
        }
    }
}

pub fn boxy(n: i64) -> i64 {
    return measure(Kind::Boxy(n));
}

pub fn round() -> i64 {
    return measure(Kind::Round);
}

// Unqualified CONSTRUCTION, the other half of what the checker resolves.
pub fn unqualified(n: i64) -> Kind {
    if n > 0 {
        return Boxy(n);
    }
    return Round;
}

pub fn through_unqualified(n: i64) -> i64 {
    return measure_wild(unqualified(n));
}

pub fn nested(n: i64) -> i64 {
    match unqualified(n) {
        Boxy(inner) => {
            match unqualified(inner - 1) {
                Boxy(deep) => {
                    return deep;
                }
                Round => {
                    return -1;
                }
            }
        }
        Round => {
            return -2;
        }
    }
}

pub fn named(t: Tag) -> String {
    match t {
        Named(text) => {
            return text;
        }
        Blank => {
            return "blank";
        }
    }
}

pub fn label(n: i64) -> String {
    if n > 0 {
        return named(Tag::Named("kept"));
    }
    return named(Tag::Blank);
}

pub fn opt(n: i64) -> i64 {
    let value: Option<i64> = Some(n);
    match value {
        Some(inner) => {
            return inner;
        }
        None => {
            return -1;
        }
    }
}

pub fn speak(s: Speak) -> i64 {
    return s.say();
}

pub fn speak_dog() -> i64 {
    return speak(new Dog());
}

pub fn speak_cat() -> i64 {
    return speak(new Cat());
}

pub class Counter {
    pub start: i64;

    // A match inside a METHOD, whose spans belong to the module just as much as
    // a free function's do.
    pub fn step(self, x: Kind) -> i64 {
        match x {
            Boxy(n) => {
                return self.start + n;
            }
            Round => {
                return self.start;
            }
        }
    }
}

pub fn stepped(start: i64, n: i64) -> i64 {
    return new Counter(start).step(Kind::Boxy(n));
}

pub async fn slow(n: i64) -> i64 {
    let doubled = n * 2;
    await yield();
    return measure(Kind::Boxy(doubled));
}
"#;

fn kinds_project(entry: &'static str) -> Vec<(&'static str, &'static str)> {
    vec![("kinds.wi", KINDS), ("main.wi", entry)]
}

// 1. The bead's repro: a payload arm after a unit arm has to be selected, not
//    skipped. This printed `1` on the AST backend.
#[test]
fn a_module_match_selects_the_payload_arm() {
    let entry = r#"
import kinds;

fn main() {
    println(kinds::boxy(9));
}
"#;
    assert_project_output(&kinds_project(entry), "main.wi", "9\n");
}

// 2. The payload binding carries the value through, not a constant.
#[test]
fn a_module_match_binds_the_payload_value() {
    let entry = r#"
import kinds;

fn main() {
    println(kinds::boxy(1));
    println(kinds::boxy(41));
    println(kinds::boxy(0 - 5));
}
"#;
    assert_project_output(&kinds_project(entry), "main.wi", "1\n41\n-5\n");
}

// 3. The unit arm still wins for a unit variant.
#[test]
fn a_module_match_selects_the_unit_arm() {
    let entry = r#"
import kinds;

fn main() {
    println(kinds::round());
}
"#;
    assert_project_output(&kinds_project(entry), "main.wi", "1\n");
}

// 4. NEGATIVE control for the panic: with the payload arm FIRST, the same
//    unresolved pattern took `emit_match`'s "no type id" panic instead of an
//    arm. Compiling at all is the assertion.
#[test]
fn a_payload_first_module_match_does_not_ice() {
    let entry = r#"
import kinds;

fn main() {
    println(kinds::measure_payload_first(Kind::Boxy(9)));
}
"#;
    // The entry file cannot name the module's enum, so drive it from inside.
    let files = &[
        ("kinds.wi", KINDS),
        (
            "shim.wi",
            r#"
module shim;

import kinds;

pub fn payload_first(n: i64) -> i64 {
    return kinds::measure_payload_first(kinds::unqualified(n));
}
"#,
        ),
        (
            "main.wi",
            r#"
import shim;

fn main() {
    println(shim::payload_first(9));
    println(shim::payload_first(0));
}
"#,
        ),
    ];
    let _ = entry;
    assert_project_output(files, "main.wi", "109\n200\n");
}

// 5. A wildcard arm in a module body.
#[test]
fn a_module_match_falls_through_to_a_wildcard() {
    let entry = r#"
import kinds;

fn main() {
    println(kinds::through_unqualified(6));
    println(kinds::through_unqualified(0));
}
"#;
    assert_project_output(&kinds_project(entry), "main.wi", "6\n0\n");
}

// 6. Unqualified variant CONSTRUCTION inside a module body — the other table
//    the entry checker had no entries for.
#[test]
fn a_module_body_constructs_an_unqualified_variant() {
    let entry = r#"
import kinds;

fn main() {
    println(kinds::through_unqualified(12));
}
"#;
    assert_project_output(&kinds_project(entry), "main.wi", "12\n");
}

// 7. A match nested inside a match arm, all module-side.
#[test]
fn a_module_body_nests_matches() {
    let entry = r#"
import kinds;

fn main() {
    println(kinds::nested(5));
    println(kinds::nested(1));
}
"#;
    assert_project_output(&kinds_project(entry), "main.wi", "4\n-1\n");
}

// 8. A String payload, which is a GC pointer rather than an immediate.
#[test]
fn a_module_match_binds_a_string_payload() {
    let entry = r#"
import kinds;

fn main() {
    println(kinds::label(1));
    println(kinds::label(0));
}
"#;
    assert_project_output(&kinds_project(entry), "main.wi", "kept\nblank\n");
}

// 9. A PRELUDE enum inside a module body: `Option` is registered globally, but
//    the resolution of THIS `Some(inner)` arm still lives in the module's own
//    checker.
#[test]
fn a_module_body_matches_a_prelude_enum() {
    let entry = r#"
import kinds;

fn main() {
    println(kinds::opt(4));
}
"#;
    assert_project_output(&kinds_project(entry), "main.wi", "4\n");
}

// 10. An interface value dispatched inside a module body — `pattern_resolutions`
//     and the class tables have to agree about the module's own classes.
#[test]
fn a_module_body_dispatches_an_interface() {
    let entry = r#"
import kinds;

fn main() {
    println(kinds::speak_dog());
    println(kinds::speak_cat());
}
"#;
    assert_project_output(&kinds_project(entry), "main.wi", "7\n9\n");
}

// 11. A match inside a module METHOD, not a free function.
#[test]
fn a_module_method_matches_its_own_enum() {
    let entry = r#"
import kinds;

fn main() {
    println(kinds::stepped(10, 5));
}
"#;
    assert_project_output(&kinds_project(entry), "main.wi", "15\n");
}

// 12. A match inside an ASYNC module body, whose locals also cross an await.
#[test]
fn an_async_module_body_matches_its_own_enum() {
    let entry = r#"
import kinds;

async fn main() {
    println(await kinds::slow(3));
}
"#;
    assert_project_output(&kinds_project(entry), "main.wi", "6\n");
}

// 13. The ENTRY file's own tables survive the merge: a match in `main` still
//     resolves after every module has extended the maps.
#[test]
fn the_entry_files_tables_survive_the_merge() {
    let entry = r#"
import kinds;

enum Local {
    One,
    Many(i64),
}

fn here(x: Local) -> i64 {
    match x {
        One => {
            return 1;
        }
        Many(n) => {
            return n;
        }
    }
}

fn main() {
    println(here(Local::Many(8)));
    println(here(Local::One));
    println(kinds::boxy(9));
}
"#;
    assert_project_output(&kinds_project(entry), "main.wi", "8\n1\n9\n");
}

// 14. An entry enum and a module enum that share VARIANT names stay apart —
//     the merge is keyed by span, and a span carries its file_id.
#[test]
fn identically_named_variants_in_two_files_do_not_collide() {
    let entry = r#"
import kinds;

enum Kind {
    Round,
    Boxy(i64),
}

fn here(x: Kind) -> i64 {
    match x {
        Round => {
            return 1000;
        }
        Boxy(n) => {
            return 1000 + n;
        }
    }
}

fn main() {
    println(here(Kind::Boxy(3)));
    println(here(Kind::Round));
    println(kinds::boxy(3));
    println(kinds::round());
}
"#;
    assert_project_output(&kinds_project(entry), "main.wi", "1003\n1000\n3\n1\n");
}

// 15. TWO modules whose enums share variant names, both matched.
#[test]
fn two_modules_with_the_same_variant_names_stay_apart() {
    let files = &[
        (
            "left.wi",
            r#"
module left;

pub enum Kind {
    Round,
    Boxy(i64),
}

pub fn read(n: i64) -> i64 {
    match Kind::Boxy(n) {
        Round => {
            return 1;
        }
        Boxy(v) => {
            return v * 2;
        }
    }
}
"#,
        ),
        (
            "right.wi",
            r#"
module right;

pub enum Kind {
    Round,
    Boxy(i64),
}

pub fn read(n: i64) -> i64 {
    match Kind::Boxy(n) {
        Round => {
            return 1;
        }
        Boxy(v) => {
            return v * 3;
        }
    }
}
"#,
        ),
        (
            "main.wi",
            r#"
import left;
import right;

fn main() {
    println(left::read(4));
    println(right::read(4));
}
"#,
        ),
    ];
    assert_project_output(files, "main.wi", "8\n12\n");
}

// 16. The merge does not leak the other way: the ENTRY file still cannot name a
//     module's variant unqualified just because the module's tables were merged.
#[test]
fn the_merge_does_not_export_a_modules_variants_to_the_entry_file() {
    let entry = r#"
import kinds;

fn main() {
    println(kinds::measure(Boxy(3)));
}
"#;
    let stderr = compile_temp_project_error_stderr(&kinds_project(entry), "main.wi");
    assert!(
        stderr.contains("Boxy"),
        "the entry file must not see a module's unqualified variant: {stderr}"
    );
}

// 17. A String payload survives allocation stress, so the arm's binding is a
//     real root and not a stale pointer the wrong arm left behind.
#[test]
fn module_match_payloads_survive_alloc_stress() {
    let entry = r#"
import kinds;

fn main() {
    println(kinds::label(1));
    println(kinds::boxy(9));
}
"#;
    let (out, ok) = compile_temp_project_with_env_and_run_under(
        &kinds_project(entry),
        "main.wi",
        &PLAIN[..],
        ALLOC_STRESS,
    );
    assert!(ok, "alloc-stress run failed: {out}");
    assert_eq!(out, "kept\n9\n");
}

// 18. The same under minor-collection stress, which is the mode that MOVES an
//     object: a match payload bound in a module body has to be traced.
#[test]
fn module_match_payloads_survive_minor_stress() {
    let entry = r#"
import kinds;

fn main() {
    println(kinds::label(1));
    println(kinds::boxy(9));
}
"#;
    let (out, ok) = compile_temp_project_with_env_and_run_under(
        &kinds_project(entry),
        "main.wi",
        &PLAIN[..],
        MINOR_STRESS,
    );
    assert!(ok, "minor-stress run failed: {out}");
    assert_eq!(out, "kept\n9\n");
}

// 19. A `--release` build answers the same.
#[test]
fn module_matches_hold_in_a_release_build() {
    let entry = r#"
import kinds;

fn main() {
    println(kinds::boxy(9));
    println(kinds::label(1));
}
"#;
    let project = TestProject::new("module_tables_release", &kinds_project(entry));
    let output = project.compile_release("main.wi");
    assert!(
        output.status.success(),
        "release build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = project.run();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "9\nkept\n");
}

// 20. The whole battery in one program, so a per-test fixture cannot hide an
//     interaction between the merged tables.
#[test]
fn every_resolution_kind_answers_together() {
    let entry = r#"
import kinds;

fn main() {
    println(kinds::boxy(9));
    println(kinds::round());
    println(kinds::through_unqualified(6));
    println(kinds::nested(5));
    println(kinds::label(1));
    println(kinds::opt(4));
    println(kinds::speak_dog());
    println(kinds::stepped(10, 5));
}
"#;
    assert_project_output(
        &kinds_project(entry),
        "main.wi",
        "9\n1\n6\n4\nkept\n4\n7\n15\n",
    );
}

// 21. The runnable example exercises the same path: `measure_boxy` matches the
//     module's own enum, and its `9` is the value an unresolved pattern lost.
#[test]
fn the_module_lir_bodies_example_matches_its_module_enum() {
    let (out, ok) = compile_file_and_run("example/module_lir_bodies/main.wi");
    assert!(
        ok,
        "example/module_lir_bodies/main.wi failed to compile or run"
    );
    let lines = out.lines().collect::<Vec<_>>();
    assert_eq!(lines[4], "1", "measure_round must take the unit arm: {out}");
    assert_eq!(
        lines[5], "9",
        "measure_boxy must take the payload arm: {out}"
    );
}
