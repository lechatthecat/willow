//! A lambda in an imported module's body is lifted to a real function
//! (willow-9yhi).
//!
//! Lambda lifting is a DECLARATION-phase job. `declare_program` collects the
//! entry file's lambdas, declares a symbol for each, and records
//! `lambda_names[span] = symbol`; both emitters read that map to find the
//! function a lambda expression takes the address of. `declare_module` had no
//! such pass and `DeclaredModule` had no `lambdas` field, so a lambda inside a
//! module body reached codegen with nothing behind it:
//!
//!     module calc;
//!     pub fn apply(n: i64) -> i64 {
//!         let f: fn(i64) -> i64 = |x| x + 1;
//!         return f(n);
//!     }
//!
//!     internal compiler error: lambda at line 4 reached codegen without a
//!     lifted function name
//!
//! on BOTH backends — the AST emitter and the LIR walker want the same lifted
//! symbol. `compile_module_bodies` compiled free functions and class methods
//! and no lambda bodies at all.
//!
//! This is capture-FREE lambdas only, which is all the checker admits today; a
//! capture is still rejected in a module body exactly as in the entry file, and
//! real closures wait on willow-bv9.
//!
//! The lifted symbols carry the module prefix (`calc.$lambda.0`).
//! `collect_lambdas_in_program` restarts its numbering for every unit it walks,
//! so a bare `$lambda.0` would name a different function in each module and the
//! second declaration would collide with the first.

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

/// The bead's repro module, widened to every position a capture-free lambda can
/// take inside a module.
const CALC: &str = r#"
module calc;

import std::collections::Array;

pub class Box {
    pub value: i64;

    // A lambda inside a METHOD, whose body the module's own compile pass has
    // to emit just like a free function's.
    pub fn boosted(self) -> i64 {
        let bump: fn(i64) -> i64 = |x| x + 1;
        return bump(self.value);
    }
}

// The bead's repro, verbatim.
pub fn apply(n: i64) -> i64 {
    let f: fn(i64) -> i64 = |x| x + 1;
    return f(n);
}

// Two lambdas in one body, so the per-unit numbering is exercised.
pub fn chained(n: i64) -> i64 {
    let inc: fn(i64) -> i64 = |x| x + 1;
    let ten: fn(i64) -> i64 = |x| x * 10;
    return ten(inc(n));
}

// A lambda as an ARGUMENT: the call site takes the lifted function's address.
pub fn apply_twice(f: fn(i64) -> i64, n: i64) -> i64 {
    return f(f(n));
}

pub fn quadrupled(n: i64) -> i64 {
    return apply_twice(|x| x * 2, n);
}

// A lambda RETURNED across the module boundary as a value.
pub fn adder() -> fn(i64) -> i64 {
    return |x| x + 5;
}

// A lambda whose body calls one of this module's own functions by its bare
// name: the alias scope has to be installed while the lambda is compiled.
pub fn twice_applied(n: i64) -> i64 {
    let call: fn(i64) -> i64 = |x| apply(x);
    return call(call(n));
}

pub fn boxed(n: i64) -> i64 {
    return new Box(n).boosted();
}

// A String-returning lambda, so a lifted body can allocate.
pub fn label(n: i64) -> String {
    let name: fn(i64) -> String = |x| "n=" + x.toString();
    return name(n);
}

// A lambda in a body that also has a `defer`, which the walker keeps as an HIR
// island rather than lowering into the block graph.
pub fn deferred(n: i64, total: &mut i64) -> i64 {
    let double: fn(i64) -> i64 = |x| x * 2;
    defer {
        total = total + 1;
    }
    return double(n);
}

// A lambda in an ASYNC module body. It stays a temporary: a `fn(i64) -> i64`
// LOCAL makes the task frame non-Send (E2402), which is a language rule of its
// own and not what lifting is about.
pub async fn slow(n: i64) -> i64 {
    await yield();
    return apply_twice(|x| x + 1, n);
}
"#;

fn calc_project(entry: &'static str) -> Vec<(&'static str, &'static str)> {
    vec![("calc.wi", CALC), ("main.wi", entry)]
}

// 1. The bead's repro compiles and answers, on both emitters. It used to ICE.
#[test]
fn a_lambda_in_a_module_body_no_longer_ices() {
    let entry = r#"
import calc;

fn main() {
    println(calc::apply(41));
}
"#;
    assert_both_backends(&calc_project(entry), "main.wi", "42\n");
}

// 2. The lifted function is module-qualified and really is compiled.
#[test]
fn a_module_lambda_is_lifted_under_a_module_qualified_symbol() {
    let entry = r#"
import calc;

fn main() {
    println(calc::apply(41));
}
"#;
    let (ok, log) = compile_temp_project_with_env_stderr(&calc_project(entry), "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    assert!(
        log.contains("[lir] compiling `calc.$lambda.0` from lowered IR"),
        "the module's lambda was not lifted under a module-qualified symbol: {log}"
    );
}

// 3. `WILLOW_LIR_REQUIRE=1` accepts the whole project: the lifted body is
//    itself compiled from lowered IR, not just declared.
#[test]
fn a_module_lambda_body_compiles_from_lowered_ir() {
    let entry = r#"
import calc;

fn main() {
    println(calc::apply(41));
}
"#;
    let (out, ok) = compile_temp_project_with_env_and_run(&calc_project(entry), "main.wi", LIR_ON);
    assert!(ok, "no body in this project may fall back: {out}");
    assert_eq!(out, "42\n");
}

// 4. Two lambdas in one module body get distinct symbols and distinct bodies.
#[test]
fn two_lambdas_in_one_module_body_stay_apart() {
    let entry = r#"
import calc;

fn main() {
    println(calc::chained(3));
}
"#;
    assert_both_backends(&calc_project(entry), "main.wi", "40\n");
}

// 5. NEGATIVE control for the collision the module prefix prevents: the
//    collector numbers from zero per unit, so two modules would both want
//    `$lambda.0` without it.
#[test]
fn two_modules_lambdas_do_not_collide() {
    let files = &[
        (
            "left.wi",
            r#"
module left;

pub fn go(n: i64) -> i64 {
    let f: fn(i64) -> i64 = |x| x + 1;
    let g: fn(i64) -> i64 = |x| x * 10;
    return g(f(n));
}
"#,
        ),
        (
            "right.wi",
            r#"
module right;

pub fn go(n: i64) -> i64 {
    let f: fn(i64) -> i64 = |x| x + 2;
    return f(n);
}
"#,
        ),
        (
            "main.wi",
            r#"
import left;
import right;

fn main() {
    println(left::go(3));
    println(right::go(3));
}
"#,
        ),
    ];
    let (ok, log) = compile_temp_project_with_env_stderr(files, "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    for symbol in ["left.$lambda.0", "left.$lambda.1", "right.$lambda.0"] {
        assert!(
            log.contains(&format!("[lir] compiling `{symbol}` from lowered IR")),
            "`{symbol}` was not lifted: {log}"
        );
    }
    assert_both_backends(files, "main.wi", "40\n5\n");
}

// 6. The ENTRY file's lambdas keep their unprefixed symbols — a module's
//    declaration pass must not renumber or rename them.
#[test]
fn the_entry_files_lambdas_keep_their_own_symbols() {
    let entry = r#"
import calc;

fn main() {
    let h: fn(i64) -> i64 = |x| x * 100;
    println(h(7));
    println(calc::apply(1));
}
"#;
    let (ok, log) = compile_temp_project_with_env_stderr(&calc_project(entry), "main.wi", LIR_LOG);
    assert!(ok, "build failed: {log}");
    assert!(
        log.contains("[lir] compiling `$lambda.0` from lowered IR"),
        "the entry file's lambda lost its own symbol: {log}"
    );
    assert_both_backends(&calc_project(entry), "main.wi", "700\n2\n");
}

// 7. A lambda inside a module CLASS METHOD, not a free function.
#[test]
fn a_lambda_inside_a_module_method_is_lifted() {
    let entry = r#"
import calc;

fn main() {
    println(calc::boxed(41));
}
"#;
    assert_both_backends(&calc_project(entry), "main.wi", "42\n");
}

// 8. A lambda passed as an ARGUMENT inside a module body.
#[test]
fn a_module_body_passes_a_lambda_as_an_argument() {
    let entry = r#"
import calc;

fn main() {
    println(calc::quadrupled(5));
}
"#;
    assert_both_backends(&calc_project(entry), "main.wi", "20\n");
}

// 9. A lambda RETURNED from a module function, then called by the entry file.
#[test]
fn a_module_function_returns_a_lambda_the_entry_calls() {
    let entry = r#"
import calc;

fn main() {
    let add = calc::adder();
    println(add(37));
}
"#;
    assert_both_backends(&calc_project(entry), "main.wi", "42\n");
}

// 10. A lambda body that calls one of the module's own functions by its bare
//     name — the module's alias scope has to be installed while it compiles.
#[test]
fn a_module_lambda_calls_its_own_modules_function() {
    let entry = r#"
import calc;

fn main() {
    println(calc::twice_applied(40));
}
"#;
    assert_both_backends(&calc_project(entry), "main.wi", "42\n");
}

// 11. A lifted module lambda that allocates.
#[test]
fn a_module_lambda_allocates_a_string() {
    let entry = r#"
import calc;

fn main() {
    println(calc::label(7));
}
"#;
    assert_both_backends(&calc_project(entry), "main.wi", "n=7\n");
}

// 12. A lambda in a body that also has a `defer`, which the walker keeps as an
//     HIR island.
#[test]
fn a_module_lambda_coexists_with_a_defer() {
    let entry = r#"
import calc;

fn main() {
    let mut total = 0;
    println(calc::deferred(21, &total));
    println(total);
}
"#;
    assert_both_backends(&calc_project(entry), "main.wi", "42\n1\n");
}

// 13. A lambda inside an ASYNC module body, whose frame is built by a different
//     code path.
#[test]
fn a_lambda_inside_an_async_module_body_is_lifted() {
    let entry = r#"
import calc;

async fn main() {
    println(await calc::slow(4));
}
"#;
    assert_both_backends(&calc_project(entry), "main.wi", "6\n");
}

// 14. A module that imports another module, both with lambdas.
#[test]
fn a_lambda_survives_a_module_calling_a_module() {
    let files = &[
        (
            "base.wi",
            r#"
module base;

pub fn twice(n: i64) -> i64 {
    let double: fn(i64) -> i64 = |x| x * 2;
    return double(n);
}
"#,
        ),
        (
            "mid.wi",
            r#"
module mid;

import base;

pub fn quad(n: i64) -> i64 {
    let call: fn(i64) -> i64 = |x| base::twice(x);
    return call(call(n));
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
    assert_both_backends(files, "main.wi", "20\n");
}

// 15. A module lambda used inside a `match` arm, which the walker keeps as an
//     HIR island rather than lowering into the block graph.
#[test]
fn a_module_lambda_is_callable_from_a_match_arm() {
    let files = &[
        (
            "pick.wi",
            r#"
module pick;

pub enum Kind {
    Round,
    Boxy(i64),
}

pub fn measure(k: Kind) -> i64 {
    let double: fn(i64) -> i64 = |x| x * 2;
    match k {
        Round => {
            return double(1);
        }
        Boxy(n) => {
            return double(n);
        }
    }
}

pub fn boxy(n: i64) -> i64 {
    return measure(Kind::Boxy(n));
}

pub fn round() -> i64 {
    return measure(Kind::Round);
}
"#,
        ),
        (
            "main.wi",
            r#"
import pick;

fn main() {
    println(pick::boxy(21));
    println(pick::round());
}
"#,
        ),
    ];
    assert_both_backends(files, "main.wi", "42\n2\n");
}

// 16. A capture in a module body is still refused, and by the CHECKER — the
//     lifting pass must not turn a rejected form into a silent miscompile.
#[test]
fn a_capturing_lambda_in_a_module_is_still_rejected() {
    let files = &[
        (
            "grab.wi",
            r#"
module grab;

pub fn go(n: i64) -> i64 {
    let factor = 3;
    let scale: fn(i64) -> i64 = |x| x * factor;
    return scale(n);
}
"#,
        ),
        (
            "main.wi",
            r#"
import grab;

fn main() {
    println(grab::go(4));
}
"#,
        ),
    ];
    let stderr = compile_temp_project_error_stderr(files, "main.wi");
    assert!(
        stderr.contains("factor"),
        "a capture in a module body must be reported, naming the variable: {stderr}"
    );
}

// 17. A lifted module lambda's allocation survives allocation stress.
#[test]
fn module_lambdas_survive_alloc_stress() {
    let entry = r#"
import calc;

fn main() {
    println(calc::label(7));
    println(calc::apply(41));
}
"#;
    let (out, ok) = compile_temp_project_with_env_and_run_under(
        &calc_project(entry),
        "main.wi",
        LIR_ON,
        ALLOC_STRESS,
    );
    assert!(ok, "alloc-stress run failed: {out}");
    assert_eq!(out, "n=7\n42\n");
}

// 18. The same under minor-collection stress, on the AST emitter.
#[test]
fn module_lambdas_survive_minor_stress_on_the_ast_backend() {
    let entry = r#"
import calc;

fn main() {
    println(calc::label(7));
    println(calc::apply(41));
}
"#;
    let (out, ok) = compile_temp_project_with_env_and_run_under(
        &calc_project(entry),
        "main.wi",
        LIR_OFF,
        MINOR_STRESS,
    );
    assert!(ok, "minor-stress run failed: {out}");
    assert_eq!(out, "n=7\n42\n");
}

// 19. A `--release` build lifts the same lambdas.
#[test]
fn module_lambdas_hold_in_a_release_build() {
    let entry = r#"
import calc;

fn main() {
    println(calc::apply(41));
    println(calc::quadrupled(5));
}
"#;
    let project = TestProject::new("module_lambda_release", &calc_project(entry));
    let output = project.compile_release("main.wi");
    assert!(
        output.status.success(),
        "release build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let run = project.run();
    assert_eq!(String::from_utf8_lossy(&run.stdout), "42\n20\n");
}

// 20. Every lambda position at once, so a per-test fixture cannot hide an
//     interaction between the lifted symbols.
#[test]
fn every_module_lambda_position_answers_together() {
    let entry = r#"
import calc;

fn main() {
    let here: fn(i64) -> i64 = |x| x - 1;
    println(calc::apply(41));
    println(calc::chained(3));
    println(calc::quadrupled(5));
    println(calc::twice_applied(40));
    println(calc::boxed(41));
    println(calc::label(7));
    println(here(43));
}
"#;
    assert_both_backends(
        &calc_project(entry),
        "main.wi",
        "42\n40\n20\n42\n42\nn=7\n42\n",
    );
}

// 21. The runnable example carries the same three positions: a lambda in a free
//     function, a lambda argument, and a lambda inside a method.
#[test]
fn the_module_lir_bodies_example_lifts_its_lambdas() {
    let (out, ok) = compile_file_and_run("example/module_lir_bodies/main.wi");
    assert!(
        ok,
        "example/module_lir_bodies/main.wi failed to compile or run"
    );
    let lines = out.lines().collect::<Vec<_>>();
    assert_eq!(
        lines[10], "12",
        "scaled(4) must call the lifted lambda: {out}"
    );
    assert_eq!(
        lines[11], "20",
        "quadrupled(5) must pass a lambda as an argument: {out}"
    );
    assert_eq!(
        lines[12], "13",
        "boosted(3, 4) must call a lambda inside a method: {out}"
    );
}
