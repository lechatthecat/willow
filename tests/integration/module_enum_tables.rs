//! One enum declaration, one identity, one back end (willow-nm0g,
//! willow-itcw, willow-0g8j.3.2).
//!
//! Each unit of a build spells a module's enum differently: the declaring
//! module writes `Level`, every importer writes `signal::Level`, and the
//! SIGNATURE of a method declared on a module class carries the qualified
//! spelling even though that method's own body is bare. The back end keeps ONE
//! flat enum table for the whole build, and it was built from the entry file's
//! checker alone -- which learns an imported enum under the qualified name only,
//! and learns nothing at all about a module the entry does not import.
//!
//! Two failures came out of that:
//!
//!   * `is_gc_managed` answers an unknown named type with "GC-managed". A
//!     fieldless enum is a tag integer, so the missing name made the emitter
//!     root a tag as a pointer, and a collection during that frame aborted with
//!     "invalid GC pointer in GC root graph: 0x1 ...", so these perspectives run
//!     under `WILLOW_GC_STRESS`.
//!   * the walker asks the same table what a type is, so every body naming one
//!     of those enums dropped out of it -- at the time to the AST emitter, which
//!     willow-0g8j.3 has since retired -- and where the table DID know both
//!     names, `same_repr` still compared the two strings and said no.
//!
//! An enum now has an IDENTITY, `module::Enum`, which is the name it is
//! registered under build-wide however many units can see it and whatever each
//! of them calls it (willow-itcw). Before that the identity was the spelling,
//! so two modules that each declared `enum Level` were one entry in the table:
//! the checker let one pass for the other (a tag matched against the wrong
//! variant list), and the back end, unable to tell which was meant, registered
//! neither bare name, so neither body could be lowered at all. The
//! spellings a unit reaches an enum by are installed for the length of that
//! unit's own compilation, which is the only span in which they mean one thing.
//!
//! Identity is not access: an importer learns what a module's enum IS without
//! gaining the right to spell its members bare (35-37). That stays with the
//! declaring module and with importers that take the enum by item.
//!
//! Most of these assert no wrong answers -- the values are the same throughout,
//! and what matters is that the value survives a collection and that the body is
//! really lowered. Perspectives 21 and 30-32 are the exception: there the two
//! enums must stay apart, and mixing them is an error rather than an answer.

use super::support::*;

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];
/// Make the walker name every function it compiles from lowered IR.
const LIR_LOG: &[(&str, &str)] = &[("WILLOW_LIR_LOG", "1")];
/// A collection at every allocation, and one at every minor-collection point.
const ALLOC_STRESS: &[(&str, &str)] = &[("WILLOW_GC_STRESS", "alloc")];
const MINOR_STRESS: &[(&str, &str)] = &[("WILLOW_GC_STRESS", "minor")];

/// The declaring module: a fieldless enum, a payload one, a recursive one, and a
/// class whose method signatures carry the qualified spelling of all three.
const SIGNAL: &str = r#"
module signal;

import std::collections::Array;

pub enum Level {
    Low,
    High,
}

pub enum Note {
    Empty,
    Text(String),
}

pub enum Chain {
    Link(Chain),
    End,
}

pub fn read(n: i64) -> Level {
    if n > 0 {
        return Level::High;
    }
    return Level::Low;
}

pub fn describe(l: Level) -> String {
    return match l {
        Low => "low",
        High => "high",
    };
}

pub fn note_of(l: Level) -> Note {
    return match l {
        Low => Note::Empty,
        High => Note::Text("loud"),
    };
}

pub fn note_text(n: Note) -> String {
    return match n {
        Empty => "-",
        Text(t) => t,
    };
}

pub fn chain_of(n: i64) -> Chain {
    if n > 0 {
        return Chain::Link(Chain::End);
    }
    return Chain::End;
}

pub fn levels(n: i64) -> Array<Level> {
    return [read(n), read(0)];
}

pub class Gauge {
    pub floor: i64;

    pub fn bump(self, l: Level) -> i64 {
        return match l {
            Low => self.floor,
            High => self.floor + 1,
        };
    }

    pub fn worded(self, n: Note) -> String {
        return note_text(n) + "/ok";
    }

    pub fn depth(self, c: Chain) -> i64 {
        return match c {
            Link(inner) => self.floor + 1,
            End => self.floor,
        };
    }

    pub fn many(self, a: Array<Level>) -> i64 {
        let mut total: i64 = 0;
        let mut i: i64 = 0;
        while i < a.len() {
            total = total + self.bump(a[i]);
            i = i + 1;
        }
        return total;
    }

    pub static fn top() -> Level {
        return Level::High;
    }
}

pub fn own_bump(n: i64) -> i64 {
    let g = new Gauge(10);
    return g.bump(read(n));
}

pub fn own_worded(n: i64) -> String {
    let g = new Gauge(0);
    return g.worded(note_of(read(n)));
}

pub fn own_depth(n: i64) -> i64 {
    let g = new Gauge(5);
    return g.depth(chain_of(n));
}

pub fn own_many(n: i64) -> i64 {
    let g = new Gauge(10);
    return g.many(levels(n));
}

pub fn top_is_high() -> bool {
    return match Gauge::top() {
        Low => false,
        High => true,
    };
}
"#;

/// A module the ENTRY never imports. Its enums reach the back end through
/// `relay`'s checker or not at all.
const DEPTH: &str = r#"
module depth;

pub enum Deep {
    Near,
    Far,
}

pub fn pick(n: i64) -> Deep {
    if n > 1 {
        return Deep::Far;
    }
    return Deep::Near;
}

pub fn name(d: Deep) -> String {
    return match d {
        Near => "near",
        Far => "far",
    };
}
"#;

/// The importing module. Every local here binds a type spelled BARE by the
/// signature it came from, next to a String allocation so a collection during
/// the frame walks it.
const RELAY: &str = r#"
module relay;

import depth;
import signal;

pub fn label(n: i64) -> String {
    let l = signal::read(n);
    let filler: String = "[" + "]";
    return signal::describe(l) + filler;
}

pub fn note(n: i64) -> String {
    let v = signal::note_of(signal::read(n));
    let filler: String = "{" + "}";
    return signal::note_text(v) + filler;
}

pub fn deep(n: i64) -> String {
    let d = depth::pick(n);
    let filler: String = "(" + ")";
    return depth::name(d) + filler;
}

pub fn repeated(n: i64) -> String {
    let l = signal::read(n);
    let mut i: i64 = 0;
    let mut acc: String = "";
    while i < 4 {
        acc = acc + "." + signal::describe(l);
        i = i + 1;
    }
    return acc;
}

pub fn chained(n: i64) -> String {
    let c = signal::chain_of(n);
    let filler: String = "!" + "!";
    return signal::own_depth(n).toString() + filler;
}
"#;

/// A module declaring a `Level` of its OWN, with different variants. Its bare
/// name collides with `signal`'s, so neither may be registered under it.
const OTHER: &str = r#"
module other;

pub enum Level {
    Off,
    On,
    Extra,
}

pub fn pick(n: i64) -> Level {
    if n > 1 {
        return Level::Extra;
    }
    if n > 0 {
        return Level::On;
    }
    return Level::Off;
}

pub fn name(l: Level) -> String {
    return match l {
        Off => "off",
        On => "on",
        Extra => "extra",
    };
}
"#;

fn project(entry: &str) -> Vec<(&'static str, String)> {
    vec![
        ("signal.wi", SIGNAL.to_string()),
        ("depth.wi", DEPTH.to_string()),
        ("relay.wi", RELAY.to_string()),
        ("other.wi", OTHER.to_string()),
        ("main.wi", entry.to_string()),
    ]
}

#[track_caller]
fn borrowed<'a>(files: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    files.iter().map(|(n, s)| (*n, s.as_str())).collect()
}

/// The build runs and prints `expected`. Since willow-0g8j.3 every body is
/// compiled from lowered IR, so a body the walker cannot take is a compile
/// error here rather than a second emitter's answer.
#[track_caller]
fn assert_program_output(entry: &str, expected: &str) {
    let files = project(entry);
    let files = borrowed(&files);

    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &PLAIN[..]);
    assert!(ok, "build failed: {out}");
    assert_eq!(out, expected, "wrong output");
}

/// The same value with a collection forced at every allocation, and again at
/// every minor point: a fieldless enum's tag must not be walked as a pointer.
#[track_caller]
fn assert_survives_gc_stress(entry: &str, expected: &str) {
    let files = project(entry);
    let files = borrowed(&files);

    for (label, env) in [("alloc", ALLOC_STRESS), ("minor", MINOR_STRESS)] {
        let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", env);
        assert!(ok, "{label} run failed: {out}");
        assert_eq!(out, expected, "{label} output mismatch");
    }
}

// ---------------------------------------------------------------------------
// The enum table has to answer to the name each unit spells (willow-nm0g).
// ---------------------------------------------------------------------------

// 1. A fieldless module enum, held in ANOTHER module's frame across an
//    allocation. This is the abort: the value is the tag `0`/`1`, and an
//    unknown named type is assumed to be a pointer.
#[test]
fn p1_a_fieldless_module_enum_in_another_module_survives_collection() {
    assert_survives_gc_stress(
        r#"
import relay;

fn main() {
    println(relay::label(1));
    println(relay::label(0));
}
"#,
        "high[]\nlow[]\n",
    );
}

// 2. The plain values, no stress.
#[test]
fn p2_a_fieldless_module_enum_crosses_modules_with_the_same_value() {
    assert_program_output(
        r#"
import relay;

fn main() {
    println(relay::label(1));
    println(relay::label(0));
}
"#,
        "high[]\nlow[]\n",
    );
}

// 3. The payload twin, where rooting IS correct: the `Text(String)` payload has
//    to still be reachable after the collection, so this catches an
//    over-correction that stopped rooting enums altogether.
#[test]
fn p3_a_payload_module_enum_is_still_rooted() {
    assert_survives_gc_stress(
        r#"
import relay;

fn main() {
    println(relay::note(1));
    println(relay::note(0));
}
"#,
        "loud{}\n-{}\n",
    );
}

// 4. An enum of a module the ENTRY never imports -- `depth` is reached only
//    through `relay`, so the entry checker's table never mentions it under any
//    name.
#[test]
fn p4_an_enum_of_a_module_the_entry_never_imports_survives_collection() {
    assert_survives_gc_stress(
        r#"
import relay;

fn main() {
    println(relay::deep(2));
    println(relay::deep(0));
}
"#,
        "far()\nnear()\n",
    );
}

// 5. Live across MANY allocations rather than one, so a minor collection has to
//    walk the frame repeatedly with the value still in it.
#[test]
fn p5_a_module_enum_live_across_a_loop_of_allocations_survives() {
    assert_survives_gc_stress(
        r#"
import relay;

fn main() {
    println(relay::repeated(0));
}
"#,
        ".low.low.low.low\n",
    );
}

// 6. The declaring module's own body under the same stress: there the enum is
//    bare on both sides, and the table only knew the qualified name.
#[test]
fn p6_the_declaring_modules_own_body_survives_collection() {
    assert_survives_gc_stress(
        r#"
import signal;

fn main() {
    println(signal::own_worded(1));
    println(signal::own_worded(0));
}
"#,
        "loud/ok\n-/ok\n",
    );
}

// 7. The entry file, which only ever knows the qualified spelling, calling the
//    module's own functions.
#[test]
fn p7_the_entry_file_reads_a_module_enum_through_module_calls() {
    assert_program_output(
        r#"
import signal;

fn main() {
    println(signal::describe(signal::read(1)));
    println(signal::note_text(signal::note_of(signal::read(0))));
}
"#,
        "high\n-\n",
    );
}

// 8. The importing module's body is really LOWERED now: before, the bare type
//    of the local was outside the walker's subset and took the whole function
//    to the AST emitter.
#[test]
fn p8_the_importing_modules_body_is_compiled_from_lowered_ir() {
    let files = project(
        r#"
import relay;

fn main() {
    println(relay::label(1));
}
"#,
    );
    let (ok, log) = compile_temp_project_with_env_stderr(&borrowed(&files), "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    assert!(
        log.contains("[lir] compiling `relay.label`"),
        "the module body binding an imported enum was not lowered: {log}"
    );
}

// 9. So is the body that names an enum of a module the entry never imports.
#[test]
fn p9_a_body_naming_an_unimported_modules_enum_is_lowered() {
    let files = project(
        r#"
import relay;

fn main() {
    println(relay::deep(2));
}
"#,
    );
    let (ok, log) = compile_temp_project_with_env_stderr(&borrowed(&files), "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    assert!(
        log.contains("[lir] compiling `relay.deep`"),
        "the body naming `depth::Deep` was not lowered: {log}"
    );
}

// 10. An alias is only safe where it cannot be ambiguous: `signal` and `other`
//     both declare a bare `Level`, with different variants. Neither may claim
//     the bare name -- the walker resolves both through their identities
//     instead, so an unclaimed bare name costs nothing.
#[test]
fn p10_two_modules_sharing_a_bare_enum_name_both_stay_correct() {
    let files = project(
        r#"
import other;
import signal;

fn main() {
    println(signal::describe(signal::read(1)));
    println(other::name(other::pick(2)));
    println(other::name(other::pick(1)));
    println(other::name(other::pick(0)));
}
"#,
    );
    let files = borrowed(&files);
    let expected = "high\nextra\non\noff\n";

    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &PLAIN[..]);
    assert!(ok, "build failed: {out}");
    assert_eq!(out, expected, "wrong output");
}

// 11. ... including under stress, where a wrong merge would give one enum the
//     other's payload shape.
#[test]
fn p11_two_modules_sharing_a_bare_enum_name_survive_collection() {
    assert_survives_gc_stress(
        r#"
import other;
import signal;

fn main() {
    println(other::name(other::pick(2)));
    println(signal::note_text(signal::note_of(signal::read(1))));
}
"#,
        "extra\nloud\n",
    );
}

// 12. An enum the ENTRY declares under a name a module also uses: the entry's
//     table wins the bare name, and the module's own enum is still read
//     correctly through the qualified one.
#[test]
fn p12_an_entry_enum_keeps_its_bare_name_against_a_modules() {
    assert_program_output(
        r#"
import signal;

enum Level {
    Only,
}

fn mine(l: Level) -> String {
    return match l {
        Level::Only => "mine",
    };
}

fn main() {
    println(mine(Level::Only));
    println(signal::describe(signal::read(1)));
}
"#,
        "mine\nhigh\n",
    );
}

// ---------------------------------------------------------------------------
// The two spellings are one representation (willow-0g8j.3.2).
// ---------------------------------------------------------------------------

// 13. A module calling a method of its OWN class with its OWN enum: the
//     signature says `signal::Level`, the argument is a bare `Level`.
#[test]
fn p13_a_module_method_takes_its_own_enum() {
    assert_program_output(
        r#"
import signal;

fn main() {
    println(signal::own_bump(1));
    println(signal::own_bump(0));
}
"#,
        "11\n10\n",
    );
}

// 14. ... and the calling function is lowered, which is the whole point: a
//     body the walker cannot type is a compile error, not a quiet wrong answer.
#[test]
fn p14_the_module_function_calling_that_method_is_lowered() {
    let files = project(
        r#"
import signal;

fn main() {
    println(signal::own_bump(1));
}
"#,
    );
    let (ok, log) = compile_temp_project_with_env_stderr(&borrowed(&files), "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    assert!(
        log.contains("[lir] compiling `signal.own_bump`"),
        "the module function calling its own class method was not lowered: {log}"
    );
}

// 15. A method taking the PAYLOAD enum, so the two spellings have to agree on a
//     variant payload as well as on the tags.
#[test]
fn p15_a_module_method_takes_its_own_payload_enum() {
    assert_program_output(
        r#"
import signal;

fn main() {
    println(signal::own_worded(1));
    println(signal::own_worded(0));
}
"#,
        "loud/ok\n-/ok\n",
    );
}

// 16. A RECURSIVE payload (`Chain::Link(Chain)`): comparing the two spellings
//     descends into the payload, so the comparison has to close its own cycle
//     instead of recursing forever.
#[test]
fn p16_a_recursive_module_enum_does_not_hang_the_comparison() {
    assert_program_output(
        r#"
import signal;

fn main() {
    println(signal::own_depth(1));
    println(signal::own_depth(0));
}
"#,
        "6\n5\n",
    );
}

// 17. The enum under an ARRAY, so the comparison has to walk through the
//     element type rather than compare two named types directly.
#[test]
fn p17_an_array_of_the_module_enum_matches_the_qualified_signature() {
    assert_program_output(
        r#"
import signal;

fn main() {
    println(signal::own_many(1));
    println(signal::own_many(0));
}
"#,
        "21\n20\n",
    );
}

// 18. A STATIC method returning the module's own enum: the qualified spelling
//     on the return side.
#[test]
fn p18_a_static_module_method_returns_its_own_enum() {
    assert_program_output(
        r#"
import signal;

fn main() {
    println(signal::top_is_high());
}
"#,
        "true\n",
    );
}

// 19. The bodies of all four are lowered, not just correct.
#[test]
fn p19_every_body_naming_both_spellings_is_lowered() {
    let files = project(
        r#"
import signal;

fn main() {
    println(signal::own_worded(1));
    println(signal::own_depth(1));
    println(signal::own_many(1));
    println(signal::top_is_high());
}
"#,
    );
    let (ok, log) = compile_temp_project_with_env_stderr(&borrowed(&files), "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    for f in [
        "signal.own_worded",
        "signal.own_depth",
        "signal.own_many",
        "signal.top_is_high",
    ] {
        assert!(
            log.contains(&format!("[lir] compiling `{f}`")),
            "`{f}` was not lowered: {log}"
        );
    }
}

// 20. A method whose parameter is the module's own enum, called from ANOTHER
//     module, where the argument arrives bare from a module signature.
#[test]
fn p20_a_module_enum_reaches_a_method_through_a_second_module() {
    assert_program_output(
        r#"
import relay;

fn main() {
    println(relay::chained(1));
    println(relay::chained(0));
}
"#,
        "6!!\n5!!\n",
    );
}

// ---------------------------------------------------------------------------
// Neither change may merge two enums that only look alike.
// ---------------------------------------------------------------------------

// 21. Where two modules answer to one bare name, each is still ITS OWN enum:
//     both bodies lower, and each answers with its own variant. `Level` is not
//     a name the back end resolves at all any more -- an enum is registered
//     under `module::Enum`, the one name it has build-wide (willow-itcw), and
//     the bare spelling each unit uses for it is installed for that unit alone.
//     Before that, the two bare `Level`s were one table entry, so the alias was
//     dropped and neither body could be lowered.
#[test]
fn p21_two_modules_sharing_a_bare_enum_name_both_lower_and_run() {
    let files = project(
        r#"
import other;
import signal;

fn main() {
    println(signal::describe(signal::read(1)));
    println(other::name(other::pick(1)));
    println(other::name(other::pick(2)));
}
"#,
    );
    let (out, ok) = compile_temp_project_with_env_and_run(&borrowed(&files), "main.wi", &PLAIN[..]);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, "high\non\nextra\n");
    let (_, log) = compile_temp_project_with_env_stderr(&borrowed(&files), "main.wi", LIR_LOG);
    for f in ["signal.describe", "signal.read", "other.name", "other.pick"] {
        assert!(
            log.contains(&format!("[lir] compiling `{f}`")),
            "`{f}` was not lowered: {log}"
        );
    }
}

// 21b. ...and they are not INTERCHANGEABLE. Handing `other`'s `Level` to a
//      function that takes `signal`'s is the wrong-code case the flat table
//      could not see: both signatures read `Level`, so the tag of one enum was
//      matched against the variants of the other and `other::On` (tag 1) came
//      back as `signal::High`. It is a type error now.
#[test]
fn p30_one_modules_enum_does_not_pass_for_anothers() {
    let files = project(
        r#"
import other;
import signal;

fn main() {
    println(signal::describe(other::pick(1)));
}
"#,
    );
    let (ok, log) = compile_temp_project_with_env_stderr(&borrowed(&files), "main.wi", &PLAIN[..]);
    assert!(
        !ok,
        "`other::Level` must not pass for `signal::Level`: {log}"
    );
    assert!(
        log.contains("signal::Level") && log.contains("other::Level"),
        "expected both enums named in the diagnostic, got: {log}"
    );
}

// 21c. ...and neither may a VALUE of one be matched against the other's
//      variants. The pattern names the variant bare, which is exactly the
//      spelling that used to resolve against whichever `Level` the table held.
#[test]
fn p31_a_pattern_of_one_modules_enum_does_not_match_anothers() {
    let files = project(
        r#"
import other;
import signal;

fn main() {
    let l = other::pick(1);
    let s = match l {
        signal::Level::Low => "low",
        signal::Level::High => "high",
    };
    println(s);
}
"#,
    );
    let (ok, log) = compile_temp_project_with_env_stderr(&borrowed(&files), "main.wi", &PLAIN[..]);
    assert!(
        !ok,
        "a `signal::Level` pattern must not match an `other::Level`: {log}"
    );
}

// 21d. ...and the two survive a collection at every allocation, which is where a
//      tag rooted as a pointer shows up as an abort rather than a wrong answer.
#[test]
fn p32_two_modules_sharing_a_bare_enum_name_survive_alloc_stress() {
    let files = project(
        r#"
import other;
import signal;

fn main() {
    let a = signal::read(1);
    let b = other::pick(2);
    gc_collect();
    println(signal::describe(a));
    println(other::name(b));
}
"#,
    );
    let expected = "high\nextra\n";
    let (out, ok) =
        compile_temp_project_with_env_and_run(&borrowed(&files), "main.wi", ALLOC_STRESS);
    assert!(ok, "stress run failed: {out}");
    assert_eq!(out, expected);
}

// 22. A name that merely LOOKS qualified -- `Level` against a prefix no module
//     of this build declares -- is not the same enum either. The entry declares
//     its own `nope::Level` shape by hand; nothing may resolve `nope`.
#[test]
fn p22_a_prefix_no_module_declares_does_not_merge_two_enums() {
    let files = project(
        r#"
import signal;

fn main() {
    println(nope::Level::Low);
}
"#,
    );
    let (ok, log) = compile_temp_project_with_env_stderr(&borrowed(&files), "main.wi", &PLAIN[..]);
    assert!(!ok, "`nope::Level` must not resolve: {log}");
}

/// A module that declares an enum under a name another unit uses for something
/// else entirely.
const CLASH: &str = "module other;

pub enum Point { Near, Far }

pub fn pick(n: i64) -> Point {
    if n == 0 { return Point::Near; }
    return Point::Far;
}

pub fn name(p: Point) -> String {
    match p {
        Point::Near => { return \"near\"; }
        Point::Far => { return \"far\"; }
    }
}
";

// 27. The bare alias is registered only where it cannot be ambiguous, and a name
//     ANOTHER unit declares as a class is exactly that. The back end answers a
//     named type from the enum table first, so registering `Point` because a
//     module declares `enum Point` would make the entry file's `class Point`
//     read as a fieldless enum -- a tag, not a pointer -- and `is_gc_managed`
//     would leave a live object untraced.
#[test]
fn p27_a_bare_enum_alias_never_shadows_another_units_class() {
    let files = vec![
        ("other.wi", CLASH),
        (
            "main.wi",
            "import other;

class Point {
    pub init(self, label: String) { self.label = label; }
    label: String;
    pub fn label(self) -> String { return self.label; }
}

fn main() {
    let p = new Point(\"a live class named Point\");
    gc_collect();
    gc_collect();
    println(p.label());
    println(other::name(other::pick(0)));
}
",
        ),
    ];
    let expected = "a live class named Point\nnear\n";
    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &PLAIN[..]);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, expected);
}

// 28. ...and it holds when a collection runs at every allocation, which is where
//     a mis-typed root shows up as an abort rather than as a wrong answer.
#[test]
fn p28_the_class_survives_alloc_stress() {
    let files = vec![
        ("other.wi", CLASH),
        (
            "main.wi",
            "import other;

class Point {
    pub init(self, label: String) { self.label = label; }
    label: String;
    pub fn label(self) -> String { return self.label; }
}

fn main() {
    let p = new Point(\"stressed\");
    let mut i = 0;
    while i < 20 {
        let filler: String = \"[\" + \"]\";
        i = i + 1;
        println(filler + other::name(other::pick(i % 2)));
    }
    println(p.label());
}
",
        ),
    ];
    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", ALLOC_STRESS);
    assert!(ok, "stress run failed: {out}");
    assert!(
        out.ends_with("stressed\n"),
        "the class named `Point` must outlive the loop: {out}"
    );
}

// 29. An interface claims a name the same way a class does.
#[test]
fn p29_a_bare_enum_alias_never_shadows_another_units_interface() {
    let files = vec![
        ("other.wi", CLASH),
        (
            "main.wi",
            "import other;

interface Point {
    fn label(self) -> String;
}

class Marker implements Point {
    pub init(self, label: String) { self.label = label; }
    label: String;
    pub fn label(self) -> String { return self.label; }
}

fn main() {
    let p: Point = new Marker(\"a live interface named Point\");
    gc_collect();
    gc_collect();
    println(p.label());
    println(other::name(other::pick(1)));
}
",
        ),
    ];
    let expected = "a live interface named Point\nfar\n";
    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &PLAIN[..]);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, expected);
}

// ---------------------------------------------------------------------------
// The whole build, and the runnable example.
// ---------------------------------------------------------------------------

// 23. Release build: the same answers with optimisation on.
#[test]
fn p23_a_release_build_agrees() {
    let files = project(
        r#"
import relay;
import signal;

fn main() {
    println(relay::label(1));
    println(signal::own_bump(1));
}
"#,
    );
    let project = TestProject::new("module_enum_tables_release", &borrowed(&files));
    let output = project.compile_release("main.wi");
    assert!(
        output.status.success(),
        "release build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = project.run();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "high[]\n11\n");
}

const EXAMPLE: &[(&str, &str)] = &[
    (
        "signal.wi",
        include_str!("../../example/module_enum_tables/signal.wi"),
    ),
    (
        "depth.wi",
        include_str!("../../example/module_enum_tables/depth.wi"),
    ),
    (
        "relay.wi",
        include_str!("../../example/module_enum_tables/relay.wi"),
    ),
    (
        "main.wi",
        include_str!("../../example/module_enum_tables/main.wi"),
    ),
];

const EXAMPLE_OUTPUT: &str =
    "high[]\nlow[]\nloud{}\n-{}\nfar()\n.low.low.low.low\n11\n10\nloud/ok\ntrue\nlow\nloud\n";

// 24. The example runs.
#[test]
fn p24_the_module_enum_tables_example_runs() {
    let (out, ok) = compile_file_and_run("example/module_enum_tables/main.wi");
    assert!(
        ok,
        "example/module_enum_tables/main.wi failed to compile or run"
    );
    assert_eq!(out, EXAMPLE_OUTPUT);
}

// 25. ... every body in it is lowered ...
#[test]
fn p25_the_module_enum_tables_example_is_fully_lowered() {
    let (out, ok) = compile_temp_project_with_env_and_run(EXAMPLE, "main.wi", &PLAIN[..]);
    assert!(ok, "the example must build and run: {out}");
    assert_eq!(out, EXAMPLE_OUTPUT);
}

// 26. ... and it survives a collection at every allocation.
#[test]
fn p26_the_module_enum_tables_example_survives_gc_stress() {
    for (label, env) in [("alloc", ALLOC_STRESS), ("minor", MINOR_STRESS)] {
        let (out, ok) = compile_temp_project_with_env_and_run(EXAMPLE, "main.wi", env);
        assert!(ok, "{label} run failed: {out}");
        assert_eq!(out, EXAMPLE_OUTPUT, "{label} output mismatch");
    }
}

/// The second example: two modules that each declare `enum Level`, both in
/// scope at once (willow-itcw).
const IDENTITY_EXAMPLE: &[(&str, &str)] = &[
    (
        "signal.wi",
        include_str!("../../example/module_enum_identity/signal.wi"),
    ),
    (
        "other.wi",
        include_str!("../../example/module_enum_identity/other.wi"),
    ),
    (
        "main.wi",
        include_str!("../../example/module_enum_identity/main.wi"),
    ),
];

const IDENTITY_OUTPUT: &str = "signal:high\nsignal:low\nother:off\nother:on\nother:extra\nsignal:high\nother:extra\ncarried\n";

// 33. The two-`Level` example runs, and every body in it is lowered: nothing is
//     dropped for being ambiguous any more.
#[test]
fn p33_the_module_enum_identity_example_is_fully_lowered() {
    let (out, ok) = compile_file_and_run("example/module_enum_identity/main.wi");
    assert!(
        ok,
        "example/module_enum_identity/main.wi failed to compile or run"
    );
    assert_eq!(out, IDENTITY_OUTPUT);
    let (out, ok) = compile_temp_project_with_env_and_run(IDENTITY_EXAMPLE, "main.wi", &PLAIN[..]);
    assert!(ok, "the example must build and run: {out}");
    assert_eq!(out, IDENTITY_OUTPUT);
}

// 34. ... and it survives a collection at every allocation: one of the two
//     enums carries a payload and the other is a bare tag, so confusing them
//     roots an integer as a pointer.
#[test]
fn p34_the_module_enum_identity_example_survives_gc_stress() {
    for (label, env) in [
        ("alloc", ALLOC_STRESS),
        ("minor", MINOR_STRESS),
        ("plain", &PLAIN[..]),
    ] {
        let (out, ok) = compile_temp_project_with_env_and_run(IDENTITY_EXAMPLE, "main.wi", env);
        assert!(ok, "{label} run failed: {out}");
        assert_eq!(out, IDENTITY_OUTPUT, "{label} output mismatch");
    }
}

// 35. Identity is not access: the entry now knows that `signal::describe` takes
//     `signal::Level`, but knowing an enum's identity is not permission to write
//     its variants bare. Only a unit that can name the ENUM bare -- the module
//     that declares it, or an importer that took it by item -- may leave the
//     name off its variants (willow-itcw).
#[test]
fn p35_an_importer_may_not_construct_a_modules_variant_bare() {
    let files = project(
        r#"
import signal;

fn main() {
    println(signal::describe(High));
}
"#,
    );
    let (ok, log) = compile_temp_project_with_env_stderr(&borrowed(&files), "main.wi", &PLAIN[..]);
    assert!(!ok, "a bare `High` must not reach `signal::Level`: {log}");
    assert!(
        log.contains("High"),
        "expected the unresolved name in the diagnostic, got: {log}"
    );
}

// 36. The same rule for patterns, where a bare name has a second meaning: with
//     the enum out of reach `Low` is an ordinary catch-all BINDING, so the arm
//     after it is unreachable and says so, rather than quietly standing for
//     `signal::Level::Low`.
#[test]
fn p36_an_importers_bare_arm_name_is_a_binding_not_a_variant() {
    let files = project(
        r#"
import signal;

fn main() {
    let l = signal::read(1);
    let s = match l {
        Low => "low",
        High => "high",
    };
    println(s);
}
"#,
    );
    let (ok, log) = compile_temp_project_with_env_stderr(&borrowed(&files), "main.wi", &PLAIN[..]);
    assert!(ok, "a catch-all binding is legal: {log}");
    assert!(
        log.contains("unreachable match arm"),
        "the second arm is dead once the first is a binding: {log}"
    );
    let (out, ok) = compile_temp_project_with_env_and_run(&borrowed(&files), "main.wi", &PLAIN[..]);
    assert!(ok, "run failed: {out}");
    assert_eq!(out, "low\n", "the binding arm takes every value");
}

// 37. ... and taking the enum by item is what grants both bare forms: the item
//     import binds `Level` unqualified (willow-64gs), so the unit can name the
//     enum bare and therefore its variants too.
#[test]
fn p37_an_item_import_grants_the_bare_forms() {
    let files = project(
        r#"
import signal;
import signal::{Level};

fn main() {
    println(signal::describe(High));
    let l = signal::read(0);
    let s = match l {
        Low => "low",
        High => "high",
    };
    println(s);
}
"#,
    );
    let (out, ok) = compile_temp_project_with_env_and_run(&borrowed(&files), "main.wi", &PLAIN[..]);
    assert!(ok, "item-imported enum build failed: {out}");
    assert_eq!(out, "high\nlow\n");
}
