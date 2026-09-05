//! Calls into an imported user module, compiled from Lowered IR (willow-7nc6).
//!
//! `math::add(1, 2)` reaches the backend as a static call whose "class" is the
//! module's ACCESS name, the same node `Class::method(..)` lowers to. The two
//! are not the same call: a module function is a free function under the module
//! item symbol (`math__add`) and takes no hidden receiver, while a class method
//! is `Math__add` and takes `self`. The walker had no branch for the module
//! form, so it refused every function containing one — the whole call was
//! answered by the AST emitter, and a single `helper::f()` anywhere in a body
//! disqualified that body.
//!
//! Both halves had to move. Eligibility now vets the module item symbol against
//! the recorded signature (no `self` to skip), and emission calls that symbol
//! with the arguments alone. Routing the module form through the class path
//! instead would mangle `Helper__f` and prepend a null receiver the callee does
//! not take.
//!
//! Coverage is asserted from `WILLOW_LIR_LOG=1`, which names each function the
//! walker really compiled: the build succeeding says only that nothing in the
//! project is outside the subset, while the log says which symbol the call
//! actually resolved to.
//!
//! 26 perspectives:
//!   1 an i64 module call             14 a returned module object's field
//!   2 no arguments                   15 an array return, then indexing
//!   3 a void call as a statement     16 f64 arguments and result
//!   4 a String return                17 a bool argument
//!   5 an operand of arithmetic       18 inside a ternary
//!   6 nested module calls            19 inside a lambda body
//!   7 inside a while loop            20 inside an async function
//!   8 a bool return as a condition   21 under allocation-stress GC
//!   9 a match scrutinee              22 a panic across the module edge
//!  10 a live String across a call    23 inside a deferred statement
//!  11 inside a second entry function 24 module and entry share a name
//!  12 two modules in one file        25 a module CLASS static still works
//!  13 a module class argument        26 a user module beats a builtin one
//!
//! Plus two more: a `&mut` argument crosses the module edge as a pointer to the
//! caller's place (willow-0g8j.2.17); and an unrecovered panic raised inside the
//! module must name the module frame and the caller frame under it.

use super::support::{
    compile_temp_project_with_env_and_run, compile_temp_project_with_env_run_stderr,
    compile_temp_project_with_env_stderr,
};

/// No extra compiler environment: the ordinary build.
const PLAIN: [(&str, &str); 0] = [];
const LIR_LOG: [(&str, &str); 1] = [("WILLOW_LIR_LOG", "1")];
const STRESS: [(&str, &str); 1] = [("WILLOW_GC_STRESS", "alloc")];

/// Build a project and require it to print `expected`.
fn assert_project_output(files: &[(&str, &str)], expected: &str) {
    let (out, ok) = compile_temp_project_with_env_and_run(files, "main.wi", &PLAIN);
    assert!(ok, "project failed: {out}");
    assert_eq!(out, expected, "wrong output");
}

/// Compile once with the selection log on and require the walker to have taken
/// each named function. Without this a coverage regression could leave a
/// function unlowered while the program still printed the right answer.
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

/// [`assert_project_output`] plus the coverage check, which is what almost
/// every perspective wants: the answer is right AND the walker is what
/// compiled the functions that produced it.
fn assert_module_call(files: &[(&str, &str)], expected: &str, functions: &[&str]) {
    assert_project_output(files, expected);
    assert_walker_compiled(files, functions);
}

const UTIL: &str = r#"
pub fn add(a: i64, b: i64) -> i64 {
    return a + b;
}

pub fn answer() -> i64 {
    return 42;
}

pub fn shout() {
    println("boom");
}

pub fn greet(name: String) -> String {
    return "hi " + name;
}

pub fn is_even(n: i64) -> bool {
    return n % 2 == 0;
}
"#;

fn util_project(main: &str) -> Vec<(String, String)> {
    vec![
        ("util.wi".to_string(), UTIL.to_string()),
        ("main.wi".to_string(), main.to_string()),
    ]
}

/// The borrowed form the support helpers take.
fn refs(files: &[(String, String)]) -> Vec<(&str, &str)> {
    files
        .iter()
        .map(|(name, source)| (name.as_str(), source.as_str()))
        .collect()
}

fn assert_util_main(main: &str, expected: &str) {
    let owned = util_project(main);
    assert_module_call(&refs(&owned), expected, &["main"]);
}

// ── 1-6 the call itself ──────────────────────────────────────────────────────

#[test]
fn module_call_01_i64_result_is_printed() {
    assert_util_main(
        r#"
import util;

fn main() {
    println(util::add(2, 3));
}
"#,
        "5\n",
    );
}

#[test]
fn module_call_02_takes_no_arguments() {
    assert_util_main(
        r#"
import util;

fn main() {
    println(util::answer());
}
"#,
        "42\n",
    );
}

#[test]
fn module_call_03_void_call_is_a_statement() {
    assert_util_main(
        r#"
import util;

fn main() {
    util::shout();
    println("after");
}
"#,
        "boom\nafter\n",
    );
}

#[test]
fn module_call_04_returns_a_heap_string() {
    assert_util_main(
        r#"
import util;

fn main() {
    println(util::greet("Alice"));
}
"#,
        "hi Alice\n",
    );
}

#[test]
fn module_call_05_is_an_arithmetic_operand() {
    assert_util_main(
        r#"
import util;

fn main() {
    println(util::add(2, 3) * 10 + util::add(1, 1));
}
"#,
        "52\n",
    );
}

#[test]
fn module_call_06_nests_inside_another_module_call() {
    assert_util_main(
        r#"
import util;

fn main() {
    println(util::add(util::add(1, 2), util::add(3, 4)));
}
"#,
        "10\n",
    );
}

// ── 7-9 control flow positions ───────────────────────────────────────────────

#[test]
fn module_call_07_runs_inside_a_while_loop() {
    assert_util_main(
        r#"
import util;

fn main() {
    let mut total = 0;
    let mut i = 0;
    while i < 5 {
        total = total + util::add(i, i);
        i = i + 1;
    }
    println(total);
}
"#,
        "20\n",
    );
}

#[test]
fn module_call_08_bool_result_is_a_condition() {
    assert_util_main(
        r#"
import util;

fn main() {
    if util::is_even(4) {
        println("even");
    } else {
        println("odd");
    }
    if util::is_even(7) {
        println("even");
    } else {
        println("odd");
    }
}
"#,
        "even\nodd\n",
    );
}

#[test]
fn module_call_09_is_a_match_scrutinee() {
    assert_util_main(
        r#"
import util;

fn main() {
    match util::add(1, 1) {
        0 => println("zero"),
        2 => println("two"),
        _ => println("other"),
    }
}
"#,
        "two\n",
    );
}

// ── 10-11 GC and receivers ───────────────────────────────────────────────────

#[test]
fn module_call_10_keeps_a_live_string_across_a_second_call() {
    assert_util_main(
        r#"
import util;

fn main() {
    let message = util::greet("Alice");
    let n = util::add(20, 22);
    println(message + " " + n.toString());
}
"#,
        "hi Alice 42\n",
    );
}

#[test]
fn module_call_11_runs_inside_a_second_entry_function() {
    let owned = util_project(
        r#"
import util;

fn bump(n: i64) -> i64 {
    return util::add(n, 1);
}

fn main() {
    println(bump(5));
    println(bump(util::answer()));
}
"#,
    );
    assert_module_call(&refs(&owned), "6\n43\n", &["main", "bump"]);
}

// ── 12-15 modules with types of their own ────────────────────────────────────

#[test]
fn module_call_12_two_modules_in_one_function() {
    let files = [
        ("left.wi", "pub fn value() -> i64 { return 10; }"),
        ("right.wi", "pub fn value() -> i64 { return 7; }"),
        (
            "main.wi",
            r#"
import left;
import right;

fn main() {
    println(left::value() - right::value());
}
"#,
        ),
    ];
    assert_module_call(&files, "3\n", &["main"]);
}

const SHAPES: &str = r#"
pub class Point {
    pub x: i64;
    pub y: i64;

    pub static fn origin() -> Point {
        return new Point(0, 0);
    }

    pub fn sum(self) -> i64 {
        return self.x + self.y;
    }
}

pub fn make(x: i64, y: i64) -> Point {
    return new Point(x, y);
}

pub fn shift(p: Point, d: i64) -> Point {
    return new Point(p.x + d, p.y + d);
}

pub fn total(p: Point) -> i64 {
    return p.x + p.y;
}
"#;

#[test]
fn module_call_13_passes_a_module_class_instance() {
    let files = [
        ("shapes.wi", SHAPES),
        (
            "main.wi",
            r#"
import shapes;
import shapes::Point;

fn main() {
    let p = shapes::shift(shapes::make(1, 2), 4);
    println(shapes::total(p));
}
"#,
        ),
    ];
    assert_module_call(&files, "11\n", &["main"]);
}

#[test]
fn module_call_14_reads_a_field_of_a_returned_module_object() {
    let files = [
        ("shapes.wi", SHAPES),
        (
            "main.wi",
            r#"
import shapes;
import shapes::Point;

fn main() {
    let p = shapes::make(3, 9);
    println(p.x);
    println(p.y);
}
"#,
        ),
    ];
    assert_module_call(&files, "3\n9\n", &["main"]);
}

#[test]
fn module_call_15_returns_an_array_that_is_indexed() {
    let files = [
        (
            "lists.wi",
            r#"
import std::collections::Array;

pub fn digits() -> Array<i64> {
    return [4, 5, 6];
}
"#,
        ),
        (
            "main.wi",
            r#"
import std::collections::Array;
import lists;

fn main() {
    let xs = lists::digits();
    println(xs[0] + xs[2]);
    println(xs.len());
}
"#,
        ),
    ];
    assert_module_call(&files, "10\n3\n", &["main"]);
}

// ── 16-19 other argument and result shapes ───────────────────────────────────

#[test]
fn module_call_16_takes_and_returns_f64() {
    let files = [
        (
            "reals.wi",
            "pub fn scale(v: f64, by: f64) -> f64 { return v * by; }",
        ),
        (
            "main.wi",
            r#"
import reals;

fn main() {
    println(reals::scale(1.5, 4.0));
}
"#,
        ),
    ];
    assert_module_call(&files, "6\n", &["main"]);
}

#[test]
fn module_call_17_takes_a_bool_argument() {
    let files = [
        (
            "flags.wi",
            "pub fn pick(on: bool, a: i64, b: i64) -> i64 { if on { return a; } return b; }",
        ),
        (
            "main.wi",
            r#"
import flags;

fn main() {
    println(flags::pick(true, 1, 2));
    println(flags::pick(false, 1, 2));
}
"#,
        ),
    ];
    assert_module_call(&files, "1\n2\n", &["main"]);
}

#[test]
fn module_call_18_sits_in_both_arms_of_a_ternary() {
    assert_util_main(
        r#"
import util;

fn main() {
    let flag = true;
    println(flag ? util::add(1, 1) : util::add(10, 10));
    println(flag ? util::greet("Alice") : util::greet("Bob"));
}
"#,
        "2\nhi Alice\n",
    );
}

#[test]
fn module_call_19_runs_inside_a_lambda_body() {
    assert_util_main(
        r#"
import util;

fn apply(x: i64, f: fn(i64) -> i64) -> i64 {
    return f(x);
}

fn main() {
    println(apply(4, |x: i64| util::add(x, 1)));
}
"#,
        "5\n",
    );
}

// ── 20-23 suspension, GC pressure, panics, defer ─────────────────────────────

#[test]
fn module_call_20_runs_inside_an_async_function() {
    let owned = util_project(
        r#"
import util;

async fn work() -> i64 {
    let before = util::add(1, 2);
    await yield();
    return util::add(before, 4);
}

async fn main() {
    println(await work());
}
"#,
    );
    // `main` is the generated synchronous driver here; the async bodies are
    // logged with the `async` prefix, so only the driver is asserted by name.
    assert_project_output(&refs(&owned), "7\n");
    let (ok, stderr) = compile_temp_project_with_env_stderr(&refs(&owned), "main.wi", &LIR_LOG[..]);
    assert!(ok, "compile failed under the walker:\n{stderr}");
    assert!(
        stderr.contains("compiling async `work` from lowered IR"),
        "async `work` did not come from the walker:\n{stderr}"
    );
}

#[test]
fn module_call_21_survives_collection_on_every_allocation() {
    let owned = util_project(
        r#"
import util;

fn main() {
    let mut i = 0;
    let mut last = "";
    while i < 20 {
        last = util::greet("Alice");
        i = i + 1;
    }
    println(last);
}
"#,
    );
    let files = refs(&owned);
    assert_module_call(&files, "hi Alice\n", &["main"]);
    let (out, ok) = compile_temp_project_with_env_and_run(&files, "main.wi", &STRESS[..]);
    assert!(ok, "project failed under GC stress: {out}");
    assert_eq!(out, "hi Alice\n", "wrong output under GC stress");
}

#[test]
fn module_call_22_panic_crosses_the_module_edge_and_is_recovered() {
    let files = [
        (
            "checks.wi",
            r#"
pub fn checked(n: i64) -> i64 {
    if n < 0 {
        panic("negative input");
    }
    return n * 2;
}
"#,
        ),
        (
            "main.wi",
            r#"
import checks;

fn main() {
    let mut status = "clean";
    if true {
        defer match recover() {
            Some(info) => {
                status = "recovered: " + info.message;
            },
            None => {}
        }
        println(checks::checked(3));
        println(checks::checked(-1));
        println("not reached");
    }
    println(status);
}
"#,
        ),
    ];
    assert_module_call(&files, "6\nrecovered: negative input\n", &["main"]);
}

#[test]
fn module_call_23_is_registered_by_a_deferred_statement() {
    assert_util_main(
        r#"
import util;

fn main() {
    defer println(util::add(1, 1));
    println("body");
}
"#,
        "body\n2\n",
    );
}

// ── 24-26 name resolution ────────────────────────────────────────────────────

#[test]
fn module_call_24_module_and_entry_functions_share_a_name() {
    let owned = util_project(
        r#"
import util;

fn add(a: i64, b: i64) -> i64 {
    return a * b;
}

fn main() {
    println(add(3, 4));
    println(util::add(3, 4));
}
"#,
    );
    assert_module_call(&refs(&owned), "12\n7\n", &["main", "add"]);
}

#[test]
fn module_call_25_a_module_class_static_is_not_a_module_call() {
    let files = [
        ("shapes.wi", SHAPES),
        (
            "main.wi",
            r#"
import shapes;
import shapes::Point;

fn main() {
    let p = shapes::Point::origin();
    println(p.sum());
    let q = Point::origin();
    println(q.sum() + shapes::make(2, 5).sum());
}
"#,
        ),
    ];
    assert_module_call(&files, "0\n7\n", &["main"]);
}

#[test]
fn module_call_26_a_user_module_beats_a_builtin_namespace() {
    let files = [
        (
            "fs.wi",
            "pub fn exists(path: String) -> bool { return false; }",
        ),
        (
            "main.wi",
            r#"
import fs;

fn main() {
    println(fs::exists("anything"));
}
"#,
        ),
    ];
    assert_module_call(&files, "false\n", &["main"]);
}

// ── a reference argument across the module edge ───────────────────────────────

#[test]
fn module_call_27_a_reference_argument_crosses_the_module_edge() {
    let files = [
        (
            "cells.wi",
            "pub fn bump(value: &mut i64) { value = value + 1; }",
        ),
        (
            "main.wi",
            r#"
import cells;

fn main() {
    let mut n = 41;
    cells::bump(&n);
    println(n);
}
"#,
        ),
    ];
    // The callee writes through the pointer, so the caller's own local is what
    // changes — and the walker, not the AST emitter, is what shaped the call.
    assert_module_call(&files, "42\n", &["main"]);
}

#[test]
fn module_call_28_an_unrecovered_panic_names_the_module_call() {
    let files = [
        (
            "checks.wi",
            r#"
pub fn checked(n: i64) -> i64 {
    if n < 0 {
        panic("negative input");
    }
    return n * 2;
}
"#,
        ),
        (
            "main.wi",
            r#"
import checks;

fn main() {
    println(checks::checked(-1));
}
"#,
        ),
    ];
    // A module call pushes a call-stack frame naming the call as the source
    // spells it (willow-0g8j.2.20), and the panic itself is reported at the
    // callee's `panic` — in the module's OWN file, not the caller's. The frame
    // shapes are covered exhaustively in `module_call_frames.rs`; what this
    // asserts is that an unrecovered panic out of a module call names both.
    let (ok, stderr) = compile_temp_project_with_env_run_stderr(&files, "main.wi", &PLAIN);
    assert!(ok, "compile failed:\n{stderr}");
    let report = without_directories(&stderr);
    assert!(
        report.contains("runtime panic: negative input at checks.wi:4:9"),
        "wrong panic site:\n{stderr}"
    );
    assert!(
        report.contains("0: checks::checked at main.wi:5:13"),
        "the module call is not the frame:\n{stderr}"
    );
}

/// The project lives in a temporary directory, so the report is read with every
/// path reduced to its file name. Both separators are cut, so the assertion
/// reads the same on Windows as on macOS and Linux.
fn without_directories(report: &str) -> String {
    report
        .split_whitespace()
        .map(|token| match token.rfind(['/', '\\']) {
            Some(cut) => &token[cut + 1..],
            None => token,
        })
        .collect::<Vec<_>>()
        .join(" ")
}
