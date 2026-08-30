//! A call into an imported module pushes a debug call-stack frame
//! (willow-0g8j.2.20).
//!
//! Every other user call is wrapped in a frame — a callee name and a call-site
//! location pushed before the call and popped when it returns — so a debug
//! panic report can name the chain that reached it. The module branch of both
//! emitters pushed none, so a panic raised inside an imported function printed
//! its own location and stopped: the entry file that called in, the only line
//! the reader could act on, was missing from the report entirely.
//!
//! Two things were wrong, and the second is what made the first invisible. The
//! branches emitted no push at all; and the frame's name has to exist as static
//! bytes before a function can point at it, while nothing declared the string
//! `checks::checked` — the module declares `checked`, the entry file declares
//! its own items, and no walk spells the pair. A push emitted against a missing
//! literal is silently skipped, so adding the push alone changed nothing.
//!
//! A module frame is named as the SOURCE spells the call: the whole path, under
//! whatever name the import bound it to (`import checks as c;` makes it
//! `c::checked`). The bare item name would not say which module it came from.
//! A class static keeps the bare-method convention it already had, because a
//! static is reached through its class and the class's own unit declares it.
//!
//! Every test is differential: the same project under the AST emitter and under
//! the walker must report the same thing. `WILLOW_LIR_REQUIRE=1` cannot police
//! a multi-file build — module functions are never registered as lowered IR —
//! so walker coverage is asserted from `WILLOW_LIR_LOG=1` instead.
//!
//! 25 perspectives:
//!   1 a module panic names the call     14 in a match arm
//!   2 the path, not the symbol          15 in a defer body
//!   3 the caller's call site            16 in a lambda body
//!   4 an import alias names the frame   17 in a ternary branch
//!   5 the caller's frame below it       18 nested inside another call
//!   6 module calling module, both hops  19 a class static stays bare
//!   7 a module's private helper         20 a class method stays bare
//!   8 the two emitters agree exactly    21 an awaited async call
//!   9 a returned call leaves no frame   22 a call inside a spawned task
//!  10 a recovered panic leaves none     23 the walker compiled the caller
//!  11 a loop accumulates no frames      24 under allocation-stress GC
//!  12 a release build has no stack      25 the example project agrees
//!  13 a reference argument, and a frame

use super::support::{
    compile_temp_project_release_run_stderr, compile_temp_project_with_env_and_run,
    compile_temp_project_with_env_run_stderr, compile_temp_project_with_env_stderr,
};

const AST: [(&str, &str); 1] = [("WILLOW_LIR_BACKEND", "0")];
const LIR: [(&str, &str); 1] = [("WILLOW_LIR_BACKEND", "1")];
const LIR_LOG: [(&str, &str); 2] = [("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_LOG", "1")];
const LIR_STRESS: [(&str, &str); 2] = [("WILLOW_LIR_BACKEND", "1"), ("WILLOW_GC_STRESS", "alloc")];

/// The module every test imports. `checked` and `empty` panic; `safe` does not,
/// so a test can prove a frame was popped by panicking somewhere else after it.
const CHECKS: &str = r#"
pub class Meter {
    pub side: i64;

    pub static fn of(side: i64) -> Meter {
        if side < 0 {
            panic("negative side");
        }
        return new Meter(side);
    }

    pub fn area(self) -> i64 {
        if self.side == 0 {
            panic("zero side");
        }
        return self.side * self.side;
    }
}

pub fn checked(n: i64) -> i64 {
    if n < 0 {
        panic("negative input");
    }
    return n * 2;
}

pub fn safe(n: i64) -> i64 {
    return n + 1;
}

pub fn deep(n: i64) -> i64 {
    return inner(n);
}

fn inner(n: i64) -> i64 {
    if n == 0 {
        panic("empty reading");
    }
    return n;
}

pub fn bump(slot: &mut i64) {
    slot = slot + 1;
    if slot > 2 {
        panic("bumped past the limit");
    }
}
"#;

/// A project of the shared module plus an entry file.
fn checks_project(main: &str) -> [(&str, &str); 2] {
    [("checks.wi", CHECKS), ("main.wi", main)]
}

/// A path token reduced to its file name. Each build gets its own temporary
/// directory, so a frame's location is only comparable once the directory is
/// cut — and both separators are cut, so this reads the same on Windows as on
/// macOS and Linux.
fn file_name(token: &str) -> &str {
    match token.rfind(['/', '\\']) {
        Some(cut) => &token[cut + 1..],
        None => token,
    }
}

/// The frames a report lists, most recent first, as `name at file:line:col`
/// with directories dropped. Stops at the first line that is not a frame, so a
/// following `async stack` section is not mistaken for one.
fn frames(report: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in report.lines() {
        if line.starts_with("call stack (most recent call first):") {
            inside = true;
            continue;
        }
        if !inside {
            continue;
        }
        let Some((index, frame)) = line.trim_end().split_once(": ") else {
            break;
        };
        if index.trim().parse::<usize>().is_err() {
            break;
        }
        out.push(
            frame
                .split_whitespace()
                .map(file_name)
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    out
}

/// Build and run a project that is expected to panic, returning the report each
/// emitter wrote. Both are asserted to carry the same panic message first, so a
/// test that goes on to read frames is reading the frames of the panic it meant.
fn panic_reports(files: &[(&str, &str)], message: &str) -> Vec<String> {
    let mut reports = Vec::new();
    for env in [&AST[..], &LIR[..]] {
        let (ok, stderr) = compile_temp_project_with_env_run_stderr(files, "main.wi", env);
        assert!(ok, "compile failed under {env:?}:\n{stderr}");
        assert!(
            stderr.contains(&format!("runtime panic: {message}")),
            "no `{message}` panic under {env:?}:\n{stderr}"
        );
        reports.push(stderr);
    }
    reports
}

/// The common shape: whichever emitter compiled the caller, the panic's frames
/// are exactly `expected`.
fn assert_frames(files: &[(&str, &str)], message: &str, expected: &[&str]) {
    for report in panic_reports(files, message) {
        assert_eq!(frames(&report), expected, "wrong frames in:\n{report}");
    }
}

/// Compile once with the selection log on and require the walker to have taken
/// each named function. Without this a coverage regression would still report
/// the right frames — from the AST emitter.
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

// ── 1-8 the frame itself ─────────────────────────────────────────────────────

#[test]
fn module_frames_01_a_module_panic_names_the_module_call() {
    // The bead's repro: before the fix this report had no call stack at all.
    let files = checks_project(
        r#"
import checks;

fn main() {
    println(checks::checked(-1));
}
"#,
    );
    assert_frames(
        &files,
        "negative input",
        &["checks::checked at main.wi:5:13"],
    );
}

#[test]
fn module_frames_02_the_frame_is_the_source_path_not_the_symbol() {
    // `checks__checked` is the mangled symbol and `checked` is the item; the
    // frame is neither, because only the path says which module ran.
    let files = checks_project(
        r#"
import checks;

fn main() {
    println(checks::checked(-1));
}
"#,
    );
    for report in panic_reports(&files, "negative input") {
        assert!(report.contains("0: checks::checked at "), "{report}");
        assert!(!report.contains("checks__checked"), "{report}");
        assert!(!report.contains("0: checked at "), "{report}");
    }
}

#[test]
fn module_frames_03_the_frame_points_at_the_call_site_not_the_body() {
    // The panic line is in the module; the frame's line is in the caller.
    let files = checks_project(
        r#"
import checks;

fn main() {
    let mut total = 0;
    total = total + checks::safe(1);
    println(checks::checked(0 - total));
}
"#,
    );
    for report in panic_reports(&files, "negative input") {
        let located = report
            .split_whitespace()
            .map(file_name)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            located.contains("runtime panic: negative input at checks.wi:"),
            "{report}"
        );
        assert_eq!(frames(&report), ["checks::checked at main.wi:7:13"]);
    }
}

#[test]
fn module_frames_04_an_import_alias_names_the_frame() {
    // The frame reads as the source reads: the alias the call was written with.
    let files = checks_project(
        r#"
import checks as c;

fn main() {
    println(c::checked(-1));
}
"#,
    );
    assert_frames(&files, "negative input", &["c::checked at main.wi:5:13"]);
}

#[test]
fn module_frames_05_the_callers_own_frame_sits_below_the_module_frame() {
    // Most recent call first: the module call, then the entry function holding
    // it. The module frame is what used to be missing between them.
    let files = checks_project(
        r#"
import checks;

fn run(n: i64) -> i64 {
    return checks::checked(n);
}

fn main() {
    println(run(-1));
}
"#,
    );
    assert_frames(
        &files,
        "negative input",
        &["checks::checked at main.wi:5:12", "run at main.wi:9:13"],
    );
}

#[test]
fn module_frames_06_a_module_calling_a_module_reports_both_hops() {
    // A frame is pushed by the file doing the calling, so the middle module's
    // own call is named `checks::deep` and located in relay.wi.
    let files = [
        ("checks.wi", CHECKS),
        (
            "relay.wi",
            r#"
import checks;

pub fn forward(n: i64) -> i64 {
    return checks::deep(n);
}
"#,
        ),
        (
            "main.wi",
            r#"
import relay;

fn main() {
    println(relay::forward(0));
}
"#,
        ),
    ];
    assert_frames(
        &files,
        "empty reading",
        &[
            "inner at checks.wi:32:12",
            "checks::deep at relay.wi:5:12",
            "relay::forward at main.wi:5:13",
        ],
    );
}

#[test]
fn module_frames_07_a_modules_private_helper_is_named_bare_in_its_own_file() {
    // A call between two functions of ONE module is an ordinary free call: the
    // bare name, located in the module's file.
    let files = checks_project(
        r#"
import checks;

fn main() {
    println(checks::deep(0));
}
"#,
    );
    assert_frames(
        &files,
        "empty reading",
        &["inner at checks.wi:32:12", "checks::deep at main.wi:5:13"],
    );
}

#[test]
fn module_frames_08_both_emitters_report_the_same_trace() {
    // Not just the same frames — the same report, byte for byte once the
    // temporary directories are cut. A trace must never depend on which
    // emitter compiled the caller.
    let files = checks_project(
        r#"
import checks;

fn run(n: i64) -> i64 {
    return checks::checked(n);
}

fn main() {
    println(checks::safe(1));
    println(run(-1));
}
"#,
    );
    let reports = panic_reports(&files, "negative input");
    let normalized = reports
        .iter()
        .map(|report| {
            report
                .split_whitespace()
                .map(file_name)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        normalized[0], normalized[1],
        "the two emitters reported the same panic differently"
    );
}

// ── 9-12 the pop, and the builds that carry no frames ────────────────────────

#[test]
fn module_frames_09_a_returned_call_leaves_no_frame_behind() {
    // The frame is popped when the call returns, so the panic below sees only
    // the chain that actually reached it.
    let files = checks_project(
        r#"
import checks;

fn main() {
    println(checks::safe(1));
    println(checks::safe(2));
    println(checks::checked(-1));
}
"#,
    );
    assert_frames(
        &files,
        "negative input",
        &["checks::checked at main.wi:7:13"],
    );
}

#[test]
fn module_frames_10_a_recovered_panic_leaves_no_frame_behind() {
    // A recovered panic unwinds through the pop like a normal return, so the
    // later, unrelated panic does not inherit a stale `checks::deep`.
    let files = checks_project(
        r#"
import checks;

fn guarded() {
    defer match recover() {
        Some(info) => println("recovered: " + info.message),
        None => {}
    }
    println(checks::deep(0));
}

fn main() {
    guarded();
    println(checks::checked(-1));
}
"#,
    );
    assert_frames(
        &files,
        "negative input",
        &["checks::checked at main.wi:14:13"],
    );
}

#[test]
fn module_frames_11_a_loop_of_module_calls_accumulates_no_frames() {
    // Twenty calls that return, then one that does not: one frame, not
    // twenty-one.
    let files = checks_project(
        r#"
import checks;

fn main() {
    let mut total = 0;
    let mut i = 0;
    while i < 20 {
        total = total + checks::safe(i);
        i = i + 1;
    }
    println(total);
    println(checks::checked(0 - total));
}
"#,
    );
    assert_frames(
        &files,
        "negative input",
        &["checks::checked at main.wi:12:13"],
    );
}

#[test]
fn module_frames_12_a_release_build_reports_no_call_stack() {
    // Frames are a debug-build instrument. A release build still reports the
    // panic and its location, and pays nothing for the chain.
    let files = checks_project(
        r#"
import checks;

fn main() {
    println(checks::checked(-1));
}
"#,
    );
    let (ok, stderr) = compile_temp_project_release_run_stderr(&files, "main.wi");
    assert!(ok, "compile failed:\n{stderr}");
    assert!(stderr.contains("runtime panic: negative input"), "{stderr}");
    assert!(!stderr.contains("call stack"), "{stderr}");
}

// ── 13-18 the positions a module call can occupy ─────────────────────────────

#[test]
fn module_frames_13_a_reference_argument_reports_the_reference_and_the_frame() {
    // Two independent debug reports about one call: which place the reference
    // named, and which line called in.
    let files = checks_project(
        r#"
import checks;

fn main() {
    let mut n = 0;
    checks::bump(&n);
    checks::bump(&n);
    checks::bump(&n);
    println(n);
}
"#,
    );
    for report in panic_reports(&files, "bumped past the limit") {
        assert!(
            report.contains("reference call: checks::bump parameter `slot` &mut i64"),
            "{report}"
        );
        assert_eq!(frames(&report), ["checks::bump at main.wi:8:5"]);
    }
}

#[test]
fn module_frames_14_a_module_call_in_a_match_arm_is_framed() {
    let files = checks_project(
        r#"
import checks;

fn main() {
    match checks::safe(1) {
        2 => println(checks::checked(-1)),
        _ => println("other"),
    }
}
"#,
    );
    assert_frames(
        &files,
        "negative input",
        &["checks::checked at main.wi:6:22"],
    );
}

#[test]
fn module_frames_15_a_module_call_in_a_defer_body_is_framed() {
    let files = checks_project(
        r#"
import checks;

fn main() {
    defer println(checks::checked(-1));
    println("before");
}
"#,
    );
    assert_frames(
        &files,
        "negative input",
        &["checks::checked at main.wi:5:19"],
    );
}

#[test]
fn module_frames_16_a_module_call_in_a_lambda_body_is_framed() {
    // The frame is emitted in the lambda's own body, so the location is the
    // line the lambda was written on.
    let files = checks_project(
        r#"
import checks;

fn main() {
    let f: fn(i64) -> i64 = |n| checks::checked(n);
    println(f(-1));
}
"#,
    );
    for report in panic_reports(&files, "negative input") {
        let frames = frames(&report);
        assert_eq!(
            frames.first().map(String::as_str),
            Some("checks::checked at main.wi:5:33"),
            "{report}"
        );
    }
}

#[test]
fn module_frames_17_a_module_call_in_a_ternary_branch_is_framed() {
    let files = checks_project(
        r#"
import checks;

fn main() {
    let flag = true;
    println(flag ? checks::checked(-1) : checks::safe(1));
}
"#,
    );
    assert_frames(
        &files,
        "negative input",
        &["checks::checked at main.wi:6:20"],
    );
}

#[test]
fn module_frames_18_a_module_call_nested_in_another_call_is_framed() {
    // The inner call panics before the outer one is entered, so the outer
    // frame is not on the stack yet.
    let files = checks_project(
        r#"
import checks;

fn main() {
    println(checks::safe(checks::checked(-1)));
}
"#,
    );
    assert_frames(
        &files,
        "negative input",
        &["checks::checked at main.wi:5:26"],
    );
}

// ── 19-22 the neighbouring call forms ────────────────────────────────────────

#[test]
fn module_frames_19_a_module_class_static_keeps_its_bare_frame() {
    // A static is reached through its class, so its frame is the bare method —
    // the convention it already had, and the class's own unit declares it.
    let files = checks_project(
        r#"
import checks;

fn main() {
    println(checks::Meter::of(-2).side);
}
"#,
    );
    assert_frames(&files, "negative side", &["of at main.wi:5:13"]);
}

#[test]
fn module_frames_20_a_module_class_method_keeps_its_bare_frame() {
    let files = checks_project(
        r#"
import checks;
import checks::Meter;

fn main() {
    let flat: Meter = new Meter(0);
    println(flat.area());
}
"#,
    );
    assert_frames(&files, "zero side", &["area at main.wi:7:18"]);
}

#[test]
fn module_frames_21_an_awaited_module_async_call_reports_the_async_stack() {
    // An awaited call is scheduled, not called, so it is the async stack that
    // names it — the call stack is for direct calls and stays empty here.
    let files = [
        (
            "jobs.wi",
            r#"
pub async fn scaled(n: i64) -> i64 {
    let mut total = 0;
    let mut i = 0;
    while i < n {
        total = total + i;
        i = i + 1;
    }
    if total > 5 {
        panic("scaled overflow");
    }
    return total;
}
"#,
        ),
        (
            "main.wi",
            r#"
import jobs;

async fn main() {
    println(await jobs::scaled(3));
    println(await jobs::scaled(6));
}
"#,
        ),
    ];
    for report in panic_reports(&files, "scaled overflow") {
        assert!(
            report.contains("async stack (current task first):"),
            "{report}"
        );
        assert!(report.contains("0: async scaled"), "{report}");
        assert!(frames(&report).is_empty(), "{report}");
    }
}

#[test]
fn module_frames_22_a_module_call_inside_a_spawned_task_is_framed() {
    // The task body is ordinary code: its module call carries a frame, and the
    // async stack names the task it ran in.
    let files = [
        ("checks.wi", CHECKS),
        (
            "main.wi",
            r#"
import checks;

async fn work(n: i64) -> i64 {
    return checks::checked(n);
}

async fn main() {
    let task = work(-1);
    println(await task);
}
"#,
        ),
    ];
    for report in panic_reports(&files, "negative input") {
        assert_eq!(
            frames(&report),
            ["checks::checked at main.wi:5:12"],
            "{report}"
        );
        assert!(
            report.contains("async stack (current task first):"),
            "{report}"
        );
    }
}

// ── 23-25 coverage, stress, and the example ──────────────────────────────────

#[test]
fn module_frames_23_the_walker_compiled_the_caller_under_test() {
    // Without this the frames above could all be the AST emitter's work.
    let files = checks_project(
        r#"
import checks;

fn run(n: i64) -> i64 {
    return checks::checked(n);
}

fn main() {
    println(run(2));
}
"#,
    );
    assert_walker_compiled(&files, &["main", "run"]);
}

#[test]
fn module_frames_24_allocation_stress_does_not_disturb_the_frames() {
    // Frame names are static bytes on the Rust heap, so a collection between
    // the push and the panic moves nothing the report reads.
    let files = checks_project(
        r#"
import checks;
import std::collections::Array;

fn main() {
    let mut labels: Array<String> = [];
    let mut i = 0;
    while i < 20 {
        labels.push("row " + i.toString());
        i = i + 1;
    }
    println(labels.len());
    println(checks::checked(-1));
}
"#,
    );
    let (ok, stderr) = compile_temp_project_with_env_run_stderr(&files, "main.wi", &LIR_STRESS);
    assert!(ok, "compile failed under stress:\n{stderr}");
    assert!(stderr.contains("runtime panic: negative input"), "{stderr}");
    assert_eq!(frames(&stderr), ["checks::checked at main.wi:13:13"]);
}

#[test]
fn module_frames_25_the_example_project_agrees_under_both_emitters() {
    // The runnable program of this bead, recovering everything it raises so its
    // stdout is deterministic; `runtime.rs` pins that stdout.
    let files = [
        (
            "trace.wi",
            include_str!("../../example/module_frame_demo/trace.wi"),
        ),
        (
            "relay.wi",
            include_str!("../../example/module_frame_demo/relay.wi"),
        ),
        (
            "main.wi",
            include_str!("../../example/module_frame_demo/main.wi"),
        ),
    ];
    let expected = "10\n3\n4\nnegative: negative reading\nempty: empty reading\n9\n\
         side: negative side\nzero: zero side\n1\n2\nbump: bumped past the limit\n6\n";
    for env in [&AST[..], &LIR[..]] {
        let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", env);
        assert!(ok, "the example failed under {env:?}:\n{out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
}
