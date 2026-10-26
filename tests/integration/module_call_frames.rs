//! Function-owned panic locations across modules (willow-jz15.67).
//! Callers retain their call sites; frame zero uses the panic site. Names come
//! from declarations, so import aliases do not rename an executing function.
//! Coverage includes cleanup, recovery, references, async tasks and GC stress.

use super::support::{
    compile_temp_project_release_run_stderr, compile_temp_project_run_stderr_under,
    compile_temp_project_with_env_and_run, compile_temp_project_with_env_run_stderr,
    compile_temp_project_with_env_stderr,
};

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];
const LIR_LOG: [(&str, &str); 1] = [("WILLOW_LIR_LOG", "1")];
const STRESS: [(&str, &str); 1] = [("WILLOW_GC_STRESS", "alloc")];
const ONE_WORKER: [(&str, &str); 1] = [("WILLOW_WORKERS", "1")];

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

/// Build and run a project that is expected to panic, returning the report it
/// wrote. The panic message is asserted first, so a test that goes on to read
/// frames is reading the frames of the panic it meant.
#[track_caller]
fn panic_report(files: &[(&str, &str)], message: &str) -> String {
    let (ok, stderr) = compile_temp_project_with_env_run_stderr(files, "main.wi", &PLAIN);
    assert!(ok, "compile failed:\n{stderr}");
    assert!(
        stderr.contains(&format!("runtime panic: {message}")),
        "no `{message}` panic:\n{stderr}"
    );
    stderr
}

/// The common shape: the panic's frames are exactly `expected`.
#[track_caller]
fn assert_frames(files: &[(&str, &str)], message: &str, expected: &[&str]) {
    let report = panic_report(files, message);
    assert_eq!(frames(&report), expected, "wrong frames in:\n{report}");
}

/// Compile with the selection log and verify each named function is lowered.
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
        &["checked at checks.wi:22:9", "main at main.wi:5:13"],
    );
}

#[test]
fn module_frames_02_the_frame_is_the_declaration_name_not_the_symbol() {
    // Declaration name and source file identify the function, without exposing symbols.
    let files = checks_project(
        r#"
import checks;

fn main() {
    println(checks::checked(-1));
}
"#,
    );
    let report = panic_report(&files, "negative input");
    assert!(report.contains("0: checked at "), "{report}");
    assert!(!report.contains("checks__checked"), "{report}");
    assert!(report.contains("1: main at "), "{report}");
}

#[test]
fn module_frames_03_frames_pair_locations_with_their_function() {
    // The leaf is in the module; main retains the caller location.
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
    let report = panic_report(&files, "negative input");
    let located = report
        .split_whitespace()
        .map(file_name)
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        located.contains("runtime panic: negative input at checks.wi:"),
        "{report}"
    );
    assert_eq!(
        frames(&report),
        ["checked at checks.wi:22:9", "main at main.wi:7:13"]
    );
}

#[test]
fn module_frames_04_an_import_alias_preserves_the_declaration_name() {
    // Import aliases do not rename the function declaration.
    let files = checks_project(
        r#"
import checks as c;

fn main() {
    println(c::checked(-1));
}
"#,
    );
    assert_frames(
        &files,
        "negative input",
        &["checked at checks.wi:22:9", "main at main.wi:5:13"],
    );
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
        &[
            "checked at checks.wi:22:9",
            "run at main.wi:5:12",
            "main at main.wi:9:13",
        ],
    );
}

#[test]
fn module_frames_06_a_module_calling_a_module_reports_both_hops() {
    // Each hop pairs the source declaration with its own file.
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
            "inner at checks.wi:37:9",
            "deep at checks.wi:32:12",
            "forward at relay.wi:5:12",
            "main at main.wi:5:13",
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
        &[
            "inner at checks.wi:37:9",
            "deep at checks.wi:32:12",
            "main at main.wi:5:13",
        ],
    );
}

#[test]
fn module_frames_08_a_returning_module_call_leaves_the_chain_clean() {
    // `checks::safe(1)` returns normally before `run(-1)` panics, so the report
    // must carry the frames of the failing chain alone: the module call, and
    // the ordinary local call that made it. A frame the successful call left
    // behind would show up between them.
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
    assert_frames(
        &files,
        "negative input",
        &[
            "checked at checks.wi:22:9",
            "run at main.wi:5:12",
            "main at main.wi:10:13",
        ],
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
        &["checked at checks.wi:22:9", "main at main.wi:7:13"],
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
        &["checked at checks.wi:22:9", "main at main.wi:14:13"],
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
        &["checked at checks.wi:22:9", "main at main.wi:12:13"],
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
    let report = panic_report(&files, "bumped past the limit");
    assert!(
        report.contains("reference call: checks::bump parameter `slot` &mut i64"),
        "{report}"
    );
    assert_eq!(
        frames(&report),
        ["bump at checks.wi:45:9", "main at main.wi:8:5"]
    );
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
        &["checked at checks.wi:22:9", "main at main.wi:6:22"],
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
        &["checked at checks.wi:22:9", "main at main.wi:5:19"],
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
    let report = panic_report(&files, "negative input");
    let frames = frames(&report);
    assert_eq!(
        frames.first().map(String::as_str),
        Some("checked at checks.wi:22:9"),
        "{report}"
    );
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
        &["checked at checks.wi:22:9", "main at main.wi:6:20"],
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
        &["checked at checks.wi:22:9", "main at main.wi:5:26"],
    );
}

// ── 19-22 the neighbouring call forms ────────────────────────────────────────

#[test]
fn module_frames_19_a_module_class_static_qualifies_its_frame() {
    // Static methods carry their declaring class as well as their method name.
    let files = checks_project(
        r#"
import checks;

fn main() {
    println(checks::Meter::of(-2).side);
}
"#,
    );
    assert_frames(
        &files,
        "negative side",
        &[
            "checks::Meter::of at checks.wi:7:13",
            "main at main.wi:5:13",
        ],
    );
}

#[test]
fn module_frames_20_a_module_class_method_qualifies_its_frame() {
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
    assert_frames(
        &files,
        "zero side",
        &[
            "checks::Meter::area at checks.wi:14:13",
            "main at main.wi:7:18",
        ],
    );
}

#[test]
fn module_frames_21_an_awaited_module_async_call_reports_the_async_stack() {
    // An awaited call is scheduled, not called, so it is the async stack that
    // names the waiting parents; the call stack also records the panicking leaf.
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
    let report = panic_report(&files, "scaled overflow");
    assert!(
        report.contains("async stack (current task first):"),
        "{report}"
    );
    assert!(report.contains("0: async scaled"), "{report}");
    assert_eq!(frames(&report), ["scaled at jobs.wi:10:9"], "{report}");
    assert!(report.contains("1: async main at "), "{report}");
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
    let report = panic_report(&files, "negative input");
    assert_eq!(
        frames(&report),
        ["checked at checks.wi:22:9", "work at main.wi:5:12"],
        "{report}"
    );
    assert!(
        report.contains("async stack (current task first):"),
        "{report}"
    );
}

// ── 23-25 coverage, stress, and the example ──────────────────────────────────

#[test]
fn module_frames_23_the_walker_compiled_the_caller_under_test() {
    // Explicitly verify that the module functions are logged as lowered.
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
    let (ok, stderr) = compile_temp_project_with_env_run_stderr(&files, "main.wi", &STRESS);
    assert!(ok, "compile failed under stress:\n{stderr}");
    assert!(stderr.contains("runtime panic: negative input"), "{stderr}");
    assert_eq!(
        frames(&stderr),
        ["checked at checks.wi:22:9", "main at main.wi:13:13"]
    );
}

#[test]
fn module_frames_25_the_example_project_runs() {
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
    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &PLAIN);
    assert!(ok, "the example failed:\n{out}");
    assert_eq!(out, expected, "wrong output");
}

#[test]
fn panic_frames_async_method_locations_debug_and_release() {
    let files = [
        (
            "worker.wi",
            "pub class Worker {\n    pub async fn fail(self) -> i64 {\n        await yield();\n        defer println(42);\n        panic(\"async method\");\n    }\n}\n",
        ),
        (
            "main.wi",
            "import worker::Worker;\nasync fn middle(w: Worker) -> i64 {\n    return await w.fail();\n}\nasync fn main() {\n    let w = new Worker();\n    println(await middle(w));\n}\n",
        ),
    ];
    // `await w.fail()` publishes the callee task before `middle` registers as
    // its awaiter, and `yield()` requeues it at once: with several workers it
    // can resume and panic on another thread first, legitimately rendering a
    // one-frame chain. One worker makes the awaiter links deterministic.
    for release in [false, true] {
        let (ok, report) =
            compile_temp_project_run_stderr_under(&files, "main.wi", release, &ONE_WORKER);
        assert!(ok, "{report}");
        let normalized = report
            .split_whitespace()
            .map(file_name)
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            normalized.contains("runtime panic: async method at worker.wi:5:9"),
            "{report}"
        );
        assert!(
            normalized.contains("0: async worker::Worker::fail at worker.wi:5:9"),
            "{report}"
        );
        assert!(
            normalized.contains("1: async middle at main.wi:3:"),
            "{report}"
        );
        assert!(
            normalized.contains("2: async main at main.wi:7:"),
            "{report}"
        );
        assert!(!report.contains("<async task>"), "{report}");
    }
}

#[test]
fn panic_frames_release_async_main_has_location() {
    let files = [(
        "main.wi",
        "async fn main() {\n    panic(\"release main\");\n}\n",
    )];
    let (ok, report) = compile_temp_project_release_run_stderr(&files, "main.wi");
    assert!(ok, "{report}");
    let normalized = report
        .split_whitespace()
        .map(file_name)
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        normalized.contains("0: async main at main.wi:2:5"),
        "{report}"
    );
}

#[test]
fn panic_frames_recursion_has_one_frame_per_activation() {
    for depth in [1, 8, 32] {
        let source = format!(
            "fn descend(n: i64) {{\n    if n == 0 {{ panic(\"depth\"); }}\n    descend(n - 1);\n}}\nfn main() {{ descend({depth}); }}\n"
        );
        let report = panic_report(&[("main.wi", &source)], "depth");
        let trace = frames(&report);
        assert_eq!(trace.len(), depth + 2, "{report}");
        assert_eq!(trace[0], "descend at main.wi:2:17");
        for frame in &trace[1..=depth] {
            assert_eq!(frame, "descend at main.wi:3:5", "{report}");
        }
        assert_eq!(trace[depth + 1], "main at main.wi:5:13");
    }
}

#[test]
fn panic_frames_release_sync_helper_does_not_inherit_async_leaf() {
    let files = [(
        "main.wi",
        "fn fail() { panic(\"sync after task\"); }\nasync fn main() {\n    await yield();\n    fail();\n}\n",
    )];
    let (ok, report) = compile_temp_project_release_run_stderr(&files, "main.wi");
    assert!(ok, "{report}");
    assert!(
        report.contains("runtime panic: sync after task"),
        "{report}"
    );
    assert!(!report.contains("call stack"), "stale async leaf: {report}");
}

#[test]
fn panic_frames_original_method_overflow_pairs_names_and_locations() {
    let source = "class C {\n    pub v: i64;\n    pub fn boom(self) -> i64 {\n        return self.v * 9223372036854775807;\n    }\n}\nfn middle(c: C) -> i64 {\n    return c.boom();\n}\nfn main() {\n    println(middle(new C(2)));\n}\n";
    let report = panic_report(&[("main.wi", source)], "integer overflow");
    assert_eq!(
        frames(&report),
        [
            "C::boom at main.wi:4:23",
            "middle at main.wi:8:14",
            "main at main.wi:11:13"
        ],
        "{report}"
    );
}

#[test]
fn panic_frames_cancel_cleanup_explicit_panic_debug_and_release() {
    let worker = "pub async fn waiting(ready: AtomicBool) {\n    defer println(42);\n    defer panic(\"cancel-unhandled\");\n    ready.store(true);\n    await sleep(10000);\n}\n";
    assert_cancel_cleanup_frames(
        worker,
        "worker::waiting(ready)",
        "waiting",
        "cancel-unhandled",
        3,
    );
}

#[test]
fn panic_frames_cancel_cleanup_method_runtime_fault_debug_and_release() {
    let worker = "import std::collections::Array;\npub class Worker {\n    pub async fn waiting(self, ready: AtomicBool) {\n        let xs: Array<i64> = [1];\n        defer {\n            println(xs[2]);\n        }\n        ready.store(true);\n        await sleep(10000);\n    }\n}\n";
    assert_cancel_cleanup_frames(
        worker,
        "new worker::Worker().waiting(ready)",
        "worker::Worker::waiting",
        "index out of bounds",
        6,
    );
}

fn assert_cancel_cleanup_frames(worker: &str, call: &str, name: &str, message: &str, line: usize) {
    let main = format!(
        "import worker;\nasync fn main() {{\n    let ready = AtomicBool::new(false);\n    let task = {call};\n    while !ready.load() {{ await yield(); }}\n    task.cancel();\n    await task.result();\n}}\n"
    );
    let files = [("worker.wi", worker), ("main.wi", main.as_str())];
    for release in [false, true] {
        let (ok, report) = if release {
            compile_temp_project_release_run_stderr(&files, "main.wi")
        } else {
            compile_temp_project_with_env_run_stderr(&files, "main.wi", &PLAIN)
        };
        assert!(ok, "compile failed, release={release}: {report}");
        let panic = report
            .lines()
            .find(|text| text.starts_with("runtime panic:"))
            .unwrap_or_else(|| panic!("missing panic, release={release}: {report}"));
        assert!(panic.contains(message), "{report}");
        let site = panic.rsplit_once(" at ").expect("located panic").1;
        assert!(
            file_name(site).starts_with(&format!("worker.wi:{line}:")),
            "wrong cleanup location, release={release}: {report}"
        );
        assert!(
            report.contains(&format!("0: async {name} at {site}")),
            "wrong async cleanup frame, release={release}: {report}"
        );
        if !release {
            assert_eq!(
                frames(&report).first(),
                Some(&format!("{name} at {}", file_name(site))),
                "{report}"
            );
        }
    }
}

#[test]
fn source_runtime_names_generic_pop() {
    let report = panic_report(
        &[(
            "main.wi",
            r#"
import std::collections::Array;
fn a<T>(xs: Array<T>) -> T { return xs.pop(); }
fn main() { let xs: Array<i64> = []; println(a(xs)); }
"#,
        )],
        "cannot pop from an empty array",
    );
    assert!(
        report.contains("0: Array<i64>::pop at <std>/collections.wi:"),
        "{report}"
    );
    assert!(report.contains("1: a<i64> at "), "{report}");
    assert!(!report.contains('$'), "{report}");
}

#[test]
fn source_runtime_names_generic_call_paths() {
    for (declarations, call, expected) in [
        (
            "fn fail<T>(x: T) { panic(\"named\"); }",
            "fail(1);",
            "fail<i64>",
        ),
        (
            "fn fail<T>(x: T) { panic(\"named\"); }",
            "fail(\"s\");",
            "fail<String>",
        ),
        (
            "fn fail<A, B>(x: A, y: B) { panic(\"named\"); }",
            "fail(1, true);",
            "fail<i64, bool>",
        ),
        (
            "class Item {} fn fail<T>(x: T) { panic(\"named\"); }",
            "fail(new Item());",
            "fail<Item>",
        ),
        (
            "class Box<T> { pub value: T; pub fn fail(self) { panic(\"named\"); } }",
            "new Box<i64>(1).fail();",
            "Box<i64>::fail",
        ),
        (
            "class Box<T> { pub value: T; pub init(self, x: T) { self.value = x; panic(\"named\"); } }",
            "let b = new Box<i64>(1);",
            "Box<i64>::init",
        ),
        (
            "class Box<T> { pub value: T; } fn fail<T>(x: T) { panic(\"named\"); }",
            "fail(new Box<i64>(1));",
            "fail<Box<i64>>",
        ),
        (
            "fn fail<T>(x: T) { defer panic(\"named\"); }",
            "fail(1);",
            "fail<i64>",
        ),
    ] {
        let source = format!("{declarations} fn main() {{ {call} }}");
        let report = panic_report(&[("main.wi", &source)], "named");
        assert!(report.contains(&format!("0: {expected} at ")), "{report}");
        assert!(
            !report.contains("$mono$") && !report.contains("$local") && !report.contains("$pkg"),
            "{report}"
        );
    }
}

#[test]
fn source_runtime_names_generic_async() {
    for (declarations, call, expected) in [
        (
            "async fn fail<T>(x: i64) { await yield(); panic(\"async named\"); }",
            "await fail<i64>(1);",
            "fail<i64>",
        ),
        (
            "class Box<T> { pub value: i64; pub async fn fail(self) { await yield(); panic(\"async named\"); } }",
            "await new Box<i64>(1).fail();",
            "Box<i64>::fail",
        ),
    ] {
        let source = format!("{declarations} async fn main() {{ {call} }}");
        for release in [false, true] {
            let files = [("main.wi", source.as_str())];
            let (ok, report) = if release {
                compile_temp_project_release_run_stderr(&files, "main.wi")
            } else {
                compile_temp_project_with_env_run_stderr(&files, "main.wi", &ONE_WORKER)
            };
            assert!(ok, "{report}");
            assert!(report.contains("runtime panic: async named"), "{report}");
            assert!(report.contains(expected), "{report}");
            assert!(
                !report.contains("$mono$")
                    && !report.contains("$local")
                    && !report.contains("$pkg"),
                "{report}"
            );
        }
    }
}

#[test]
fn source_runtime_names_generic_cancellation() {
    assert_cancel_cleanup_frames(
        "pub async fn waiting<T>(ready: AtomicBool) {\n    defer panic(\"cancel-named\");\n    ready.store(true);\n    await sleep(10000);\n}\n",
        "worker::waiting<i64>(ready)",
        "waiting<i64>",
        "cancel-named",
        2,
    );
    assert_cancel_cleanup_frames(
        "pub class Worker<T> {\n    pub async fn waiting(self, ready: AtomicBool) {\n        defer panic(\"cancel-named\");\n        ready.store(true);\n        await sleep(10000);\n    }\n}\n",
        "new worker::Worker<i64>().waiting(ready)",
        "worker::Worker<i64>::waiting",
        "cancel-named",
        3,
    );
}

#[test]
fn source_runtime_names_imported_generic() {
    let report = panic_report(
        &[
            (
                "values.wi",
                "pub class Item {} pub fn fail<T>(x: T) { panic(\"imported named\"); }",
            ),
            (
                "main.wi",
                "import values as v; fn main() { v::fail(new v::Item()); }",
            ),
        ],
        "imported named",
    );
    assert!(report.contains("0: fail<values::Item> at "), "{report}");
    assert!(
        !report.contains("$mono$") && !report.contains("$local") && !report.contains("$pkg"),
        "{report}"
    );
}

#[test]
fn source_runtime_names_example() {
    let files = [(
        "main.wi",
        include_str!("../../example/runtime_source_names.wi"),
    )];
    let (stdout, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &PLAIN);
    assert!(ok, "{stdout}");
    assert_eq!(stdout, "7\n9\n");
}
