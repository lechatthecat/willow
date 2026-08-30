//! Matching an enum that was declared in ANOTHER file (willow-28h8 case B).
//!
//! HIR lowering built its enum table from the unit it was lowering — the
//! `enum` items of that one file — so any enum reached through an import was
//! not an enum as far as `lower_match` could tell, and every `match` on one
//! died with "enum pattern on a non-enum scrutinee". That took the whole
//! enclosing function out of the lowered IR:
//!
//!   * `match c { palette::Color::Red => .. }`, the module-qualified spelling;
//!   * `match k { Kind::Small => .. }`, the bare name a direct type import
//!     binds (`import shapes::Kind;`);
//!   * and the same two inside a MODULE body, which is where it also cost
//!     `theme::palette_rank` its lowering.
//!
//! Construction of the very same variants lowered fine, because it reads the
//! checker's recorded type — so the table the lowering was missing is exactly
//! the one the checker already had. The fix hands it over: `CheckerTables`
//! carries `symbols.enums`, keyed by the name each enum is WRITTEN with, and
//! the lowering seeds from it BEFORE this unit's own items, so a locally
//! declared enum of the same bare name still shadows.
//!
//! No wrong answers here — the fallback computed the same values — so these
//! perspectives assert `WILLOW_LIR_REQUIRE=1` builds and `WILLOW_LIR_LOG=1`
//! lines, with the two emitters agreeing on the values throughout.

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

/// The module whose enums everything else matches on.
const PALETTE: &str = r#"
module palette;

pub enum Color {
    Red,
    Green,
    Custom(i64),
}

pub enum Sample {
    Empty,
    Filled(String),
}
"#;

/// A CHILD module, so the direct type import that reaches its enum has to walk
/// a dotted path to find it.
const SHADES: &str = r#"
module palette::shades;

pub enum Shade {
    Light,
    Dark,
}
"#;

/// A module that matches BOTH its own enum and `palette`'s. Its `Color` shares
/// only the bare name.
const THEME: &str = r#"
module theme;

import palette;

pub enum Color {
    Warm,
    Cool,
}

fn rank(c: Color) -> i64 {
    return match c {
        Color::Warm => 1,
        Color::Cool => 2,
    };
}

pub fn cool_rank() -> i64 {
    return rank(Color::Cool);
}

pub fn warm_rank() -> i64 {
    return rank(Color::Warm);
}

pub fn palette_rank(c: palette::Color) -> i64 {
    return match c {
        palette::Color::Red => 10,
        palette::Color::Green => 20,
        palette::Color::Custom(v) => v,
    };
}
"#;

fn project(entry: &str) -> Vec<(&'static str, String)> {
    vec![
        ("palette.wi", PALETTE.to_string()),
        ("palette/shades.wi", SHADES.to_string()),
        ("theme.wi", THEME.to_string()),
        ("main.wi", entry.to_string()),
    ]
}

#[track_caller]
fn borrowed<'a>(files: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    files.iter().map(|(n, s)| (*n, s.as_str())).collect()
}

/// Both emitters must agree on the value, and the LIR one must not fall back.
#[track_caller]
fn assert_both_backends(entry: &str, expected: &str) {
    let files = project(entry);
    let files = borrowed(&files);

    let (lir, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", LIR_ON);
    assert!(ok, "LIR build failed: {lir}");
    assert_eq!(lir, expected, "lowered-IR output mismatch");

    let (ast, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", LIR_OFF);
    assert!(ok, "AST build failed: {ast}");
    assert_eq!(ast, expected, "AST output mismatch");
}

// ---------------------------------------------------------------------------
// The spellings an import can give an enum.
// ---------------------------------------------------------------------------

// 1. The module-qualified name, on a fieldless variant.
#[test]
fn p1_a_module_qualified_fieldless_variant_matches() {
    assert_both_backends(
        r#"
import palette;

fn code(c: palette::Color) -> i64 {
    return match c {
        palette::Color::Red => 1,
        palette::Color::Green => 2,
        palette::Color::Custom(v) => v,
    };
}

fn main() {
    println(code(palette::Color::Red));
    println(code(palette::Color::Green));
}
"#,
        "1\n2\n",
    );
}

// 2. The same, on a variant that carries a payload the arm binds.
#[test]
fn p2_a_module_qualified_payload_variant_binds_its_payload() {
    assert_both_backends(
        r#"
import palette;

fn code(c: palette::Color) -> i64 {
    return match c {
        palette::Color::Red => 1,
        palette::Color::Green => 2,
        palette::Color::Custom(v) => v,
    };
}

fn main() {
    println(code(palette::Color::Custom(42)));
}
"#,
        "42\n",
    );
}

// 3. It is really lowered, not merely correct: this is the whole point, since
//    the fallback answered the same numbers all along.
#[test]
fn p3_the_matching_function_is_compiled_from_lowered_ir() {
    let files = project(
        r#"
import palette;

fn code(c: palette::Color) -> i64 {
    return match c {
        palette::Color::Red => 1,
        palette::Color::Green => 2,
        palette::Color::Custom(v) => v,
    };
}

fn main() {
    println(code(palette::Color::Red));
}
"#,
    );
    let (ok, log) = compile_temp_project_with_env_stderr(&borrowed(&files), "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    assert!(
        log.contains("[lir] compiling `code` from lowered IR"),
        "the function matching an imported enum was not lowered: {log}"
    );
}

// 4. A direct type import binds the BARE name, including through a child
//    module's dotted path.
#[test]
fn p4_a_direct_type_import_matches_under_the_bare_name() {
    assert_both_backends(
        r#"
import palette::shades::Shade;

fn shade_code(s: Shade) -> i64 {
    return match s {
        Shade::Light => 7,
        Shade::Dark => 8,
    };
}

fn main() {
    println(shade_code(Shade::Light));
    println(shade_code(Shade::Dark));
}
"#,
        "7\n8\n",
    );
}

// 5. An aliased type import matches under the ALIAS.
#[test]
fn p5_an_aliased_type_import_matches_under_its_alias() {
    assert_both_backends(
        r#"
import palette::Color as Hue;

fn hue_code(h: Hue) -> i64 {
    return match h {
        Hue::Red => 100,
        Hue::Green => 200,
        Hue::Custom(v) => v + 1,
    };
}

fn main() {
    println(hue_code(Hue::Green));
    println(hue_code(Hue::Custom(9)));
}
"#,
        "200\n10\n",
    );
}

// 6. The alias and the module-qualified spelling name the SAME enum: both
//    functions lower, in one file, against one declaration.
#[test]
fn p6_the_alias_and_the_qualified_name_are_the_same_enum() {
    let entry = r#"
import palette;
import palette::Color as Hue;

fn hue_code(h: Hue) -> i64 {
    return match h {
        Hue::Red => 100,
        Hue::Green => 200,
        Hue::Custom(v) => v + 1,
    };
}

fn code(c: palette::Color) -> i64 {
    return match c {
        palette::Color::Red => 1,
        palette::Color::Green => 2,
        palette::Color::Custom(v) => v,
    };
}

fn main() {
    println(code(palette::Color::Red));
    println(hue_code(Hue::Red));
}
"#;
    assert_both_backends(entry, "1\n100\n");

    let files = project(entry);
    let (ok, log) = compile_temp_project_with_env_stderr(&borrowed(&files), "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    for name in ["code", "hue_code"] {
        assert!(
            log.contains(&format!("[lir] compiling `{name}` from lowered IR")),
            "`{name}` was not lowered: {log}"
        );
    }
}

// 7. A GC-managed payload: the arm binds a `String` out of the imported enum.
#[test]
fn p7_a_string_payload_survives_the_match() {
    assert_both_backends(
        r#"
import palette;

fn sample_text(s: palette::Sample) -> String {
    return match s {
        palette::Sample::Empty => "none",
        palette::Sample::Filled(t) => t,
    };
}

fn main() {
    println(sample_text(palette::Sample::Filled("ink")));
    println(sample_text(palette::Sample::Empty));
}
"#,
        "ink\nnone\n",
    );
}

// ---------------------------------------------------------------------------
// Where the match sits.
// ---------------------------------------------------------------------------

// 8. In a MODULE body, on that module's OWN enum — the control that was never
//    broken, since a unit always knew its own items.
#[test]
fn p8_a_module_matching_its_own_enum_still_works() {
    assert_both_backends(
        r#"
import theme;

fn main() {
    println(theme::warm_rank());
    println(theme::cool_rank());
}
"#,
        "1\n2\n",
    );
}

// 9. In a MODULE body, on ANOTHER module's enum — the shape that regresses the
//    moment the lowering stops being told about imported enums.
#[test]
fn p9_a_module_matching_another_modules_enum_is_lowered() {
    let entry = r#"
import palette;
import theme;

fn main() {
    println(theme::palette_rank(palette::Color::Red));
    println(theme::palette_rank(palette::Color::Custom(5)));
}
"#;
    assert_both_backends(entry, "10\n5\n");

    let files = project(entry);
    let (ok, log) = compile_temp_project_with_env_stderr(&borrowed(&files), "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    assert!(
        log.contains("[lir] compiling `theme.palette_rank` from lowered IR"),
        "the module body matching an imported enum was not lowered: {log}"
    );
}

// 10. In a class method.
#[test]
fn p10_a_class_method_matches_an_imported_enum() {
    assert_both_backends(
        r#"
import palette;

class Painter {
    pub tint: i64;

    pub fn score(self, c: palette::Color) -> i64 {
        return match c {
            palette::Color::Red => self.tint,
            palette::Color::Green => self.tint + 1,
            palette::Color::Custom(v) => v,
        };
    }
}

fn main() {
    let p = new Painter(3);
    println(p.score(palette::Color::Red));
    println(p.score(palette::Color::Green));
}
"#,
        "3\n4\n",
    );
}

// 11. In an async function, whose body is compiled through the state-machine
//     path rather than the straight-line one.
#[test]
fn p11_an_async_function_matches_an_imported_enum() {
    assert_both_backends(
        r#"
import palette;

async fn code(c: palette::Color) -> i64 {
    return match c {
        palette::Color::Red => 11,
        palette::Color::Green => 12,
        palette::Color::Custom(v) => v,
    };
}

async fn main() {
    println(await code(palette::Color::Green));
    println(await code(palette::Color::Custom(4)));
}
"#,
        "12\n4\n",
    );
}

// ---------------------------------------------------------------------------
// The pattern and arm shapes the lowering has to keep supporting.
// ---------------------------------------------------------------------------

// 12. A wildcard arm.
#[test]
fn p12_a_wildcard_arm_covers_the_rest() {
    assert_both_backends(
        r#"
import palette;

fn code(c: palette::Color) -> i64 {
    return match c {
        palette::Color::Red => 1,
        _ => 9,
    };
}

fn main() {
    println(code(palette::Color::Red));
    println(code(palette::Color::Green));
}
"#,
        "1\n9\n",
    );
}

// 13. A binding arm, which names the whole scrutinee and so has to be typed as
//     the imported enum, not as a variant of it.
#[test]
fn p13_a_binding_arm_binds_the_whole_scrutinee() {
    assert_both_backends(
        r#"
import palette;

fn second(c: palette::Color) -> i64 {
    return match c {
        palette::Color::Green => 2,
        palette::Color::Custom(v) => v,
        palette::Color::Red => 1,
    };
}

fn bound(c: palette::Color) -> i64 {
    return match c {
        palette::Color::Red => 1,
        rest => second(rest),
    };
}

fn main() {
    println(bound(palette::Color::Red));
    println(bound(palette::Color::Custom(9)));
}
"#,
        "1\n9\n",
    );
}

// 14. Two imported enums nested, from two different files.
#[test]
fn p14_two_imported_enums_nest() {
    assert_both_backends(
        r#"
import palette;
import palette::shades::Shade;

fn nested(c: palette::Color, s: Shade) -> i64 {
    return match c {
        palette::Color::Red => match s {
            Shade::Light => 10,
            Shade::Dark => 20,
        },
        palette::Color::Green => 30,
        palette::Color::Custom(v) => v,
    };
}

fn main() {
    println(nested(palette::Color::Red, Shade::Dark));
    println(nested(palette::Color::Green, Shade::Light));
}
"#,
        "20\n30\n",
    );
}

// 15. Block-bodied arms, which lower as statements rather than as a value.
#[test]
fn p15_block_bodied_arms_lower() {
    assert_both_backends(
        r#"
import palette;

fn blocky(c: palette::Color) -> i64 {
    match c {
        palette::Color::Red => {
            return 41;
        }
        palette::Color::Green => {
            return 42;
        }
        palette::Color::Custom(v) => {
            return v;
        }
    }
}

fn main() {
    println(blocky(palette::Color::Green));
    println(blocky(palette::Color::Custom(7)));
}
"#,
        "42\n7\n",
    );
}

// 16. The match as a value bound by `let`, not as the operand of `return`.
#[test]
fn p16_the_match_can_be_a_let_initializer() {
    assert_both_backends(
        r#"
import palette;

fn as_value(c: palette::Color) -> i64 {
    let n = match c {
        palette::Color::Red => 3,
        palette::Color::Green => 4,
        palette::Color::Custom(v) => v,
    };
    return n + 1;
}

fn main() {
    println(as_value(palette::Color::Green));
    println(as_value(palette::Color::Custom(10)));
}
"#,
        "5\n11\n",
    );
}

// ---------------------------------------------------------------------------
// What must NOT change: the tables the seeding writes into.
// ---------------------------------------------------------------------------

// 17. A locally declared enum still shadows the bare name, and the imported one
//     is still reachable under its qualified spelling in the same file.
#[test]
fn p17_a_local_enum_shadows_the_bare_name() {
    assert_both_backends(
        r#"
import palette;

enum Color {
    Local,
    Other,
}

fn local_code(c: Color) -> i64 {
    return match c {
        Color::Local => 5,
        Color::Other => 6,
    };
}

fn imported_code(c: palette::Color) -> i64 {
    return match c {
        palette::Color::Red => 1,
        palette::Color::Green => 2,
        palette::Color::Custom(v) => v,
    };
}

fn main() {
    println(local_code(Color::Other));
    println(imported_code(palette::Color::Green));
}
"#,
        "6\n2\n",
    );
}

// 18. Two modules whose enums share a bare name stay distinct: `theme`'s own
//     `Color` and `palette`'s are matched in the same build.
#[test]
fn p18_two_modules_may_share_a_bare_enum_name() {
    assert_both_backends(
        r#"
import palette;
import theme;

fn main() {
    println(theme::cool_rank());
    println(theme::palette_rank(palette::Color::Green));
}
"#,
        "2\n20\n",
    );
}

// 19. The prelude's enums still match — the seeding runs over the prelude
//     table, so `Option` had better survive it.
#[test]
fn p19_a_prelude_option_still_matches() {
    assert_both_backends(
        r#"
import palette;

fn unwrap_or(v: Option<i64>) -> i64 {
    return match v {
        Some(x) => x,
        None => -1,
    };
}

fn main() {
    println(unwrap_or(Some(8)));
    println(unwrap_or(None));
    println(unwrap_or(Some(0)));
}
"#,
        "8\n-1\n0\n",
    );
}

// 20. So does the prelude's `Result`, whose variants carry payloads.
#[test]
fn p20_a_prelude_result_still_matches() {
    assert_both_backends(
        r#"
import palette;

fn parsed(text: String) -> Result<i64, String> {
    if text == "one" {
        return Ok(1);
    }
    return Err("bad");
}

fn from_result(text: String) -> i64 {
    return match parsed(text) {
        Ok(v) => v,
        Err(_) => -1,
    };
}

fn main() {
    println(from_result("one"));
    println(from_result("nope"));
}
"#,
        "1\n-1\n",
    );
}

// ---------------------------------------------------------------------------
// The build shapes the change has to hold up under.
// ---------------------------------------------------------------------------

// 21. Every unit in a project full of these lowers, with nothing falling back.
#[test]
fn p21_the_whole_project_builds_under_lir_require() {
    let files = project(
        r#"
import palette;
import palette::shades::Shade;
import palette::Color as Hue;
import theme;

fn code(c: palette::Color) -> i64 {
    return match c {
        palette::Color::Red => 1,
        palette::Color::Green => 2,
        palette::Color::Custom(v) => v,
    };
}

fn hue_code(h: Hue) -> i64 {
    return match h {
        Hue::Red => 100,
        Hue::Green => 200,
        Hue::Custom(v) => v + 1,
    };
}

fn shade_code(s: Shade) -> i64 {
    return match s {
        Shade::Light => 7,
        Shade::Dark => 8,
    };
}

fn main() {
    println(code(palette::Color::Custom(42)));
    println(hue_code(Hue::Green));
    println(shade_code(Shade::Dark));
    println(theme::palette_rank(palette::Color::Red));
}
"#,
    );
    let (out, ok) = compile_temp_project_with_env_and_run(&borrowed(&files), "main.wi", LIR_ON);
    assert!(ok, "no body in this project may fall back: {out}");
    assert_eq!(out, "42\n200\n8\n10\n");
}

// 22. Under allocation stress, where the String payload is collected across
//     every call.
#[test]
fn p22_it_holds_under_allocation_stress() {
    let files = project(
        r#"
import palette;

fn sample_text(s: palette::Sample) -> String {
    return match s {
        palette::Sample::Empty => "none",
        palette::Sample::Filled(t) => t,
    };
}

fn main() {
    println(sample_text(palette::Sample::Filled("ink")));
    println(sample_text(palette::Sample::Empty));
}
"#,
    );
    let (out, ok) = compile_temp_project_with_env_and_run_under(
        &borrowed(&files),
        "main.wi",
        LIR_ON,
        ALLOC_STRESS,
    );
    assert!(ok, "alloc-stress run failed: {out}");
    assert_eq!(out, "ink\nnone\n");
}

// 23. And under minor-collection stress, which moves the payload.
#[test]
fn p23_it_holds_under_minor_gc_stress() {
    let files = project(
        r#"
import palette;

fn sample_text(s: palette::Sample) -> String {
    return match s {
        palette::Sample::Empty => "none",
        palette::Sample::Filled(t) => t,
    };
}

fn main() {
    println(sample_text(palette::Sample::Filled("ink")));
    println(sample_text(palette::Sample::Empty));
}
"#,
    );
    let (out, ok) = compile_temp_project_with_env_and_run_under(
        &borrowed(&files),
        "main.wi",
        LIR_ON,
        MINOR_STRESS,
    );
    assert!(ok, "minor-stress run failed: {out}");
    assert_eq!(out, "ink\nnone\n");
}

// 24. A release build, with none of the debug instrumentation.
#[test]
fn p24_the_release_build_answers_the_same() {
    let files = project(
        r#"
import palette;
import theme;

fn code(c: palette::Color) -> i64 {
    return match c {
        palette::Color::Red => 1,
        palette::Color::Green => 2,
        palette::Color::Custom(v) => v,
    };
}

fn main() {
    println(code(palette::Color::Custom(42)));
    println(theme::palette_rank(palette::Color::Green));
}
"#,
    );
    let project = TestProject::new("imported_enum_match_release", &borrowed(&files));
    let output = project.compile_release("main.wi");
    assert!(
        output.status.success(),
        "release build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = project.run();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "42\n20\n");
}

// ---------------------------------------------------------------------------
// The runnable example.
// ---------------------------------------------------------------------------

const EXAMPLE: &[(&str, &str)] = &[
    (
        "palette.wi",
        include_str!("../../example/imported_enum_match/palette.wi"),
    ),
    (
        "palette/shades.wi",
        include_str!("../../example/imported_enum_match/palette/shades.wi"),
    ),
    (
        "theme.wi",
        include_str!("../../example/imported_enum_match/theme.wi"),
    ),
    (
        "main.wi",
        include_str!("../../example/imported_enum_match/main.wi"),
    ),
];

const EXAMPLE_OUTPUT: &str = "1\n42\n200\n8\nink\n2\n20\n";

// 25. It runs.
#[test]
fn p25_the_imported_enum_match_example_runs() {
    let (out, ok) = compile_file_and_run("example/imported_enum_match/main.wi");
    assert!(
        ok,
        "example/imported_enum_match/main.wi failed to compile or run"
    );
    assert_eq!(out, EXAMPLE_OUTPUT);
}

// 26. ... and every body in it is lowered, which is what this change buys.
#[test]
fn p26_the_imported_enum_match_example_is_fully_lowered() {
    let (out, ok) = compile_temp_project_with_env_and_run(EXAMPLE, "main.wi", LIR_ON);
    assert!(ok, "no body in the example may fall back: {out}");
    assert_eq!(out, EXAMPLE_OUTPUT);
}
