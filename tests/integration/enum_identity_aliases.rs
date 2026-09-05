//! An enum has ONE identity build-wide, however a unit spells it
//! (willow-0g8j.3, over willow-itcw / willow-64gs).
//!
//! `import signal::Level as Rank;` binds a local name to a declaration that
//! lives in `signal`. The checker registers the declaration under BOTH the
//! written spelling and its identity, so `Rank` type-checks; but only the
//! identity is a name every unit answers to, and the enum tables the back end
//! keys — variant tags, payload layout, GC shape — are keyed by it. Any table
//! that recorded the written spelling instead ended up with a key nothing else
//! in the build could match:
//!
//!   * the checker recorded `Rank::High` as a static call on a class called
//!     `Rank`, which is not an enum anywhere the back end can see;
//!   * `Codegen::declare_function` built `fn_types` from the raw AST, so
//!     `fn score(r: Rank)` took a `Rank` while every argument reaching it had
//!     been normalized to `signal::Level`;
//!   * a MODULE's own alias never left that module: `meter`'s exported
//!     `fn label(g: Grade)` presented a `Grade` its importer had never heard
//!     of;
//!   * and a lambda parameter annotation was used verbatim, so `|g: Rank|`
//!     disagreed with the function it called.
//!
//! Since willow-0g8j.3 every body compiles from lowered IR, so each of those
//! was a hard `E0800` refusal rather than a second emitter's answer — except
//! the first, which the checker resolved to a catch-all binding and answered
//! WRONG. These perspectives pin down both halves: the values are right, and
//! `WILLOW_LIR_LOG=1` shows the bodies were walked.

use super::support::*;

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];
const LIR_LOG: &[(&str, &str)] = &[("WILLOW_LIR_LOG", "1")];
const ALLOC_STRESS: &[(&str, &str)] = &[("WILLOW_GC_STRESS", "alloc")];

/// The module that DECLARES the enums. Nothing here spells them any way but
/// its own, so it is the identity every other file has to agree with.
const SIGNAL: &str = r#"
module signal;

pub enum Level {
    Low,
    Mid,
    High,
}

pub enum Reading {
    Empty,
    Value(i64),
}

pub fn top() -> Level {
    return Level::High;
}

pub fn rank(l: Level) -> i64 {
    return match l {
        Low => 1,
        Mid => 2,
        High => 3,
    };
}

pub fn strongest(a: Level, b: Level) -> Level {
    if rank(a) >= rank(b) {
        return a;
    }
    return b;
}
"#;

/// A SECOND module that reaches `signal`'s enum under an alias of its own, and
/// exports signatures written with it.
const METER: &str = r#"
module meter;

import signal::Level as Grade;

pub fn label(g: Grade) -> String {
    return match g {
        Grade::Low => "low",
        Grade::Mid => "mid",
        Grade::High => "high",
    };
}

pub fn lowest() -> Grade {
    return Grade::Low;
}
"#;

/// A module declaring an enum that is structurally IDENTICAL to `signal`'s and
/// shares its bare name. Two declarations, two identities.
const OTHER: &str = r#"
module other;

pub enum Level {
    Low,
    Mid,
    High,
}

pub fn name(l: Level) -> String {
    return match l {
        Low => "other-low",
        Mid => "other-mid",
        High => "other-high",
    };
}
"#;

fn project(entry: &str) -> Vec<(&'static str, String)> {
    vec![
        ("signal.wi", SIGNAL.to_string()),
        ("meter.wi", METER.to_string()),
        ("other.wi", OTHER.to_string()),
        ("main.wi", entry.to_string()),
    ]
}

#[track_caller]
fn borrowed<'a>(files: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    files.iter().map(|(n, s)| (*n, s.as_str())).collect()
}

/// The project builds and prints `expected`.
#[track_caller]
fn assert_output(entry: &str, expected: &str) {
    let files = project(entry);
    let files = borrowed(&files);
    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &PLAIN[..]);
    assert!(ok, "build failed: {out}");
    assert_eq!(out, expected, "wrong output");
}

/// The project builds, prints `expected`, and every named function was compiled
/// from lowered IR — the walker took the body rather than refusing it.
#[track_caller]
fn assert_output_all_lir(entry: &str, expected: &str, functions: &[&str]) {
    assert_output(entry, expected);

    let files = project(entry);
    let files = borrowed(&files);
    let (ok, log) = compile_temp_project_with_env_stderr(&files, "main.wi", LIR_LOG);
    assert!(ok, "logged build failed: {log}");
    for function in functions {
        let sync = format!("[lir] compiling `{function}` from lowered IR");
        let coop = format!("[lir] compiling async `{function}` from lowered IR");
        assert!(
            log.contains(&sync) || log.contains(&coop),
            "`{function}` did not compile from lowered IR: {log}"
        );
    }
}

/// The project must NOT build, and the compiler's stderr is returned.
#[track_caller]
fn assert_rejected(entry: &str) -> String {
    let files = project(entry);
    let files = borrowed(&files);
    compile_temp_project_error_stderr(&files, "main.wi")
}

// ---------------------------------------------------------------------------
// Matching: the spelling must not change which arm runs.
// ---------------------------------------------------------------------------

// 1. BARE variant patterns under an alias. The item import is what makes them
//    legal; each selects its own arm rather than binding the scrutinee, which
//    is the wrong answer this bug produced.
#[test]
fn alias_01_bare_patterns_select_their_own_arm() {
    assert_output_all_lir(
        r#"
import signal::Level as Rank;

fn score(r: Rank) -> i64 {
    return match r {
        Low => 1,
        Mid => 2,
        High => 3,
    };
}

fn main() {
    println(score(Rank::High));
    println(score(Rank::Mid));
    println(score(Rank::Low));
}
"#,
        "3\n2\n1\n",
        &["score", "main"],
    );
}

// 2. The ALIAS-QUALIFIED spelling of the same patterns.
#[test]
fn alias_02_alias_qualified_patterns_select_their_own_arm() {
    assert_output_all_lir(
        r#"
import signal::Level as Rank;

fn tag(r: Rank) -> i64 {
    return match r {
        Rank::Low => 10,
        Rank::Mid => 20,
        Rank::High => 30,
    };
}

fn main() {
    println(tag(Rank::High));
    println(tag(Rank::Low));
}
"#,
        "30\n10\n",
        &["tag", "main"],
    );
}

// 3. A `match` written with the alias, over a value produced under the
//    CANONICAL spelling. One value, two names for its type.
#[test]
fn alias_03_canonical_value_matched_through_the_alias() {
    assert_output_all_lir(
        r#"
import signal;
import signal::Level as Rank;

fn tag(r: Rank) -> i64 {
    return match r {
        Rank::Low => 10,
        Rank::Mid => 20,
        Rank::High => 30,
    };
}

fn main() {
    println(tag(signal::Level::Mid));
    println(tag(signal::top()));
}
"#,
        "20\n30\n",
        &["tag", "main"],
    );
}

// ---------------------------------------------------------------------------
// Signatures: what the back end's function tables are keyed by.
// ---------------------------------------------------------------------------

// 4. The alias as a PARAMETER type, called with the alias spelling. This is the
//    `fn_types` half: the table was built from the raw AST, so the declared
//    `Rank` never matched the normalized argument.
#[test]
fn alias_04_alias_parameter_accepts_an_alias_argument() {
    assert_output_all_lir(
        r#"
import signal;
import signal::Level as Rank;

fn score(r: Rank) -> i64 {
    return signal::rank(r);
}

fn main() {
    println(score(Rank::High));
}
"#,
        "3\n",
        &["score", "main"],
    );
}

// 5. The alias as a RETURN type, over a module function declared with the
//    canonical one.
#[test]
fn alias_05_alias_return_type_carries_a_canonical_value() {
    assert_output_all_lir(
        r#"
import signal;
import signal::Level as Rank;

fn stronger(a: Rank, b: Rank) -> Rank {
    return signal::strongest(a, b);
}

fn main() {
    println(signal::rank(stronger(Rank::Low, Rank::High)));
    println(signal::rank(stronger(Rank::Mid, Rank::Low)));
}
"#,
        "3\n2\n",
        &["stronger", "main"],
    );
}

// 6. A module's OWN alias in an exported signature. `meter` renamed the enum
//    for itself; `main` never heard of `Grade`, so the identity is the only
//    name that can cross the boundary.
#[test]
fn alias_06_a_module_alias_is_exported_as_the_identity() {
    assert_output_all_lir(
        r#"
import signal;
import meter;
import signal::Level as Rank;

fn main() {
    println(meter::label(Rank::Mid));
    println(meter::label(signal::Level::High));
    println(signal::rank(meter::lowest()));
}
"#,
        "mid\nhigh\n1\n",
        &["meter.label", "meter.lowest", "main"],
    );
}

// 7. Two files renaming ONE declaration differently still name one enum: the
//    value `meter` returns under `Grade` is matched here under `Rank`.
#[test]
fn alias_07_two_files_rename_one_enum_and_still_agree() {
    assert_output_all_lir(
        r#"
import meter;
import signal::Level as Rank;

fn tag(r: Rank) -> i64 {
    return match r {
        Rank::Low => 10,
        Rank::Mid => 20,
        Rank::High => 30,
    };
}

fn main() {
    println(tag(meter::lowest()));
    println(meter::label(Rank::High));
}
"#,
        "10\nhigh\n",
        &["tag", "main"],
    );
}

// ---------------------------------------------------------------------------
// The alias in every other type position.
// ---------------------------------------------------------------------------

// 8. As a `let` annotation, with an initializer written the same way.
#[test]
fn alias_08_let_annotation_with_an_alias_initializer() {
    assert_output_all_lir(
        r#"
import signal;
import signal::Level as Rank;

fn main() {
    let x: Rank = Rank::Mid;
    println(signal::rank(x));
}
"#,
        "2\n",
        &["main"],
    );
}

// 9. A CANONICAL annotation with an alias initializer, and the reverse. Neither
//    direction is a conversion; both spell one type.
#[test]
fn alias_09_annotation_and_initializer_may_disagree_on_spelling() {
    assert_output_all_lir(
        r#"
import signal;
import signal::Level as Rank;

fn main() {
    let a: signal::Level = Rank::High;
    let b: Rank = signal::Level::Low;
    println(signal::rank(a));
    println(signal::rank(b));
}
"#,
        "3\n1\n",
        &["main"],
    );
}

// 10. As an ARRAY element type, which rebuilds the type around the element and
//     so is where a normalization that only looked at the outside went wrong.
#[test]
fn alias_10_array_of_the_alias() {
    assert_output_all_lir(
        r#"
import std::collections::Array;
import signal;
import signal::Level as Rank;

fn main() {
    let xs: Array<Rank> = [Rank::Low, Rank::High, Rank::Mid];
    println(signal::rank(xs[0]));
    println(signal::rank(xs[1]));
    println(xs.len());
}
"#,
        "1\n3\n3\n",
        &["main"],
    );
}

// 11. As a CLASS FIELD type and as a method parameter type.
#[test]
fn alias_11_class_field_and_method_parameter() {
    assert_output_all_lir(
        r#"
import signal;
import signal::Level as Rank;

class Gauge {
    pub limit: Rank;

    pub fn over(self, r: Rank) -> bool {
        return signal::rank(r) > signal::rank(self.limit);
    }
}

fn main() {
    let g = new Gauge(Rank::Mid);
    println(g.over(Rank::High));
    println(g.over(Rank::Low));
    println(signal::rank(g.limit));
}
"#,
        "true\nfalse\n2\n",
        &["Gauge::over", "main"],
    );
}

// 12. As a LAMBDA parameter type. The annotation was used verbatim, so the
//     lambda body's call to a function taking the same enum did not type-check
//     at all.
#[test]
fn alias_12_lambda_parameter_annotation() {
    assert_output_all_lir(
        r#"
import signal;
import signal::Level as Rank;

fn bumped(r: Rank) -> i64 {
    let f = |g: Rank| -> i64 { return signal::rank(g) + 1; };
    return f(r);
}

fn main() {
    println(bumped(Rank::High));
    println(bumped(Rank::Low));
}
"#,
        "4\n2\n",
        &["bumped", "main"],
    );
}

// 13. As a lambda RETURN type, where the body produces the canonical spelling.
#[test]
fn alias_13_lambda_return_annotation() {
    assert_output_all_lir(
        r#"
import signal;
import signal::Level as Rank;

fn best() -> i64 {
    let f = |n: i64| -> Rank {
        if n > 0 {
            return signal::Level::High;
        }
        return Rank::Low;
    };
    return signal::rank(f(1)) * 10 + signal::rank(f(0));
}

fn main() {
    println(best());
}
"#,
        "31\n",
        &["best", "main"],
    );
}

// 14. In an ASYNC function's signature, whose frame layout is built from the
//     same types.
#[test]
fn alias_14_async_signature_and_frame() {
    assert_output_all_lir(
        r#"
import signal;
import signal::Level as Rank;

async fn measured(r: Rank) -> i64 {
    await sleep(1);
    return signal::rank(r) * 100;
}

async fn main() {
    println(await measured(Rank::Mid));
    println(await measured(Rank::High));
}
"#,
        "200\n300\n",
        &["measured", "main"],
    );
}

// 15. A PAYLOAD variant through an alias binds its payload, and the payload
//     enum is a heap object rather than a bare tag.
#[test]
fn alias_15_payload_variant_through_an_alias() {
    assert_output_all_lir(
        r#"
import signal::Reading as Sample;

fn read(s: Sample) -> i64 {
    return match s {
        Sample::Empty => 0,
        Sample::Value(v) => v,
    };
}

fn main() {
    println(read(Sample::Value(7)));
    println(read(Sample::Empty));
}
"#,
        "7\n0\n",
        &["read", "main"],
    );
}

// ---------------------------------------------------------------------------
// The other two spellings an import can give an enum.
// ---------------------------------------------------------------------------

// 16. A PLAIN item import, no rename. `Level` is not the identity either, so
//     this is the same bug with a name that merely looks canonical.
#[test]
fn alias_16_plain_item_import_uses_the_identity() {
    assert_output_all_lir(
        r#"
import signal;
import signal::Level;

fn score(l: Level) -> i64 {
    return match l {
        Low => 1,
        Mid => 2,
        High => 3,
    };
}

fn main() {
    println(score(Level::High));
    println(score(signal::Level::Mid));
    println(signal::rank(Level::Low));
}
"#,
        "3\n2\n1\n",
        &["score", "main"],
    );
}

// 17. The MODULE-QUALIFIED spelling on its own, which was already the identity
//     and must stay working.
#[test]
fn alias_17_module_qualified_spelling_still_works() {
    assert_output_all_lir(
        r#"
import signal;

fn score(l: signal::Level) -> i64 {
    return match l {
        signal::Level::Low => 1,
        signal::Level::Mid => 2,
        signal::Level::High => 3,
    };
}

fn main() {
    println(score(signal::Level::High));
    println(score(signal::top()));
}
"#,
        "3\n3\n",
        &["score", "main"],
    );
}

// ---------------------------------------------------------------------------
// Equality: a payload-free enum IS its tag.
// ---------------------------------------------------------------------------

// 18. `==` across two spellings of one enum compares VARIANTS, and the walker
//     emits it (a blanket refusal of every named-type operand had made this a
//     hard error).
#[test]
fn alias_18_equality_compares_variants_across_spellings() {
    assert_output_all_lir(
        r#"
import signal;
import signal::Level as Rank;

fn main() {
    println(Rank::High == signal::Level::High);
    println(Rank::High == signal::Level::Low);
    println(Rank::High != signal::Level::Low);
    println(signal::top() == Rank::High);
}
"#,
        "true\nfalse\ntrue\ntrue\n",
        &["main"],
    );
}

// 19. Equality on a LOCAL fieldless enum, so the same emission is exercised
//     with no import in the picture at all.
#[test]
fn alias_19_equality_on_a_local_fieldless_enum() {
    assert_output_all_lir(
        r#"
enum Door {
    Open,
    Closed,
}

fn flip(d: Door) -> Door {
    return match d {
        Open => Door::Closed,
        Closed => Door::Open,
    };
}

fn main() {
    println(Door::Open == Door::Open);
    println(flip(Door::Open) == Door::Closed);
    println(flip(Door::Closed) != Door::Open);
}
"#,
        "true\ntrue\nfalse\n",
        &["flip", "main"],
    );
}

// ---------------------------------------------------------------------------
// What the identity must NOT merge.
// ---------------------------------------------------------------------------

// 20. Two modules declaring a structurally identical `Level` are two enums. An
//     identity check that fell back to shape would have merged them, and the
//     tags would have been read against the wrong declaration.
#[test]
fn alias_20_identical_enums_in_two_modules_stay_distinct() {
    let stderr = assert_rejected(
        r#"
import signal;
import other;

fn main() {
    println(other::name(signal::Level::Low));
}
"#,
    );
    assert!(
        stderr.contains("mismatched types"),
        "two declarations were merged: {stderr}"
    );
}

// 21. ...and each still works on its own, in the same program, under bare
//     names that collide.
#[test]
fn alias_21_two_colliding_enums_coexist() {
    assert_output_all_lir(
        r#"
import signal;
import other;
import signal::Level as Rank;

fn main() {
    println(signal::rank(Rank::Mid));
    println(other::name(other::Level::Mid));
    println(signal::rank(signal::Level::High));
}
"#,
        "2\nother-mid\n3\n",
        &["other.name", "main"],
    );
}

// 22. A LOCALLY declared enum shadows an imported one of the same bare name:
//     the file's own declaration is what a bare `Level` means here.
#[test]
fn alias_22_a_local_declaration_shadows_an_imported_name() {
    assert_output_all_lir(
        r#"
import signal;

enum Level {
    Off,
    On,
}

fn state(l: Level) -> i64 {
    return match l {
        Off => 0,
        On => 1,
    };
}

fn main() {
    println(state(Level::On));
    println(signal::rank(signal::Level::High));
}
"#,
        "1\n3\n",
        &["state", "main"],
    );
}

// ---------------------------------------------------------------------------
// Representation and stress.
// ---------------------------------------------------------------------------

// 23. The payload enum's heap object is a GC root wherever it is held, so the
//     alias spelling must not have changed which shape the collector traces.
#[test]
fn alias_23_payload_enum_survives_allocation_stress() {
    let entry = r#"
import std::collections::Array;
import signal::Reading as Sample;

fn total(xs: Array<Sample>) -> i64 {
    let mut sum = 0;
    for x in xs {
        sum = sum + match x {
            Sample::Empty => 0,
            Sample::Value(v) => v,
        };
    }
    return sum;
}

fn main() {
    let mut xs: Array<Sample> = [];
    let mut i = 0;
    while i < 200 {
        xs.push(Sample::Value(i));
        xs.push(Sample::Empty);
        i = i + 1;
    }
    println(total(xs));
    println(xs.len());
}
"#;
    let files = project(entry);
    let files = borrowed(&files);
    let (out, ok) =
        compile_temp_project_with_env_and_run_under(&files, "main.wi", &PLAIN[..], ALLOC_STRESS);
    assert!(ok, "stressed run failed: {out}");
    assert_eq!(out, "19900\n400\n", "wrong output under allocation stress");
}

// 24. The shipped example builds and prints, with every one of its bodies
//     compiled from lowered IR.
#[test]
fn alias_24_the_example_runs() {
    let files = [
        (
            "signal.wi",
            include_str!("../../example/enum_identity_aliases/signal.wi"),
        ),
        (
            "meter.wi",
            include_str!("../../example/enum_identity_aliases/meter.wi"),
        ),
        (
            "main.wi",
            include_str!("../../example/enum_identity_aliases/main.wi"),
        ),
    ];
    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &PLAIN[..]);
    assert!(ok, "example build failed: {out}");
    assert_eq!(
        out,
        "3\n10\n2\n3\n2\n3\n3\nmid\nhigh\n7\n0\ntrue\nfalse\n4\ntrue\ntrue\n200\n"
    );

    let (ok, log) = compile_temp_project_with_env_stderr(&files, "main.wi", LIR_LOG);
    assert!(ok, "logged example build failed: {log}");
    for function in [
        "signal.top",
        "signal.rank",
        "signal.strongest",
        "meter.label",
        "Gauge::over",
        "stronger",
        "score",
        "tag",
        "read",
        "bumped",
    ] {
        assert!(
            log.contains(&format!("[lir] compiling `{function}` from lowered IR")),
            "`{function}` did not compile from lowered IR: {log}"
        );
    }
    for function in ["measured", "main"] {
        assert!(
            log.contains(&format!(
                "[lir] compiling async `{function}` from lowered IR"
            )),
            "`{function}` did not compile from lowered IR: {log}"
        );
    }
}
