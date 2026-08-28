//! `fn main() -> Result<void, E>` compiled from Lowered IR (willow-0g8j.2.14).
//!
//! `main` is the one function whose return value is not handed to a caller: it
//! becomes the process exit status. The compiler lowers a `Result<void, E>`
//! main to a VOID `willow_user_main` and turns each way out of it into an EXIT
//! (willow-exg) — `Ok` flushes defers and returns nothing, `Err` reports the
//! payload through `willow_main_fail` and exits non-zero, and a `?` on an `Err`
//! does the same instead of returning the failure.
//!
//! The LIR walker only accepted `main` in its `void` forms, so this whole shape
//! went to the AST emitter. Two things were missing. HIR lowering dropped the
//! function outright: nothing typed the zero-argument `Result::Ok()`, because
//! the checker special-cases it and returned before recording a type, so
//! lowering reported a gap and the function never reached the LIR at all. And
//! the walker's `return` and `?` emitters had no Result-main epilogue.
//!
//! Every test is differential — the same program under the AST emitter and
//! under the walker must produce the same output AND the same success/failure
//! status — and the walker side sets `WILLOW_LIR_REQUIRE=1`, so a silent
//! fallback is a compile error rather than a comparison of the AST emitter
//! against itself. Each also runs under `WILLOW_GC_STRESS=alloc`, because a
//! reported `Err` payload is a heap value that has to survive the defer flush
//! standing between its construction and the report.
//!
//! 26 perspectives:
//!   1 `Result::Ok()` exits cleanly       13 i64 payload, generic report
//!   2 `Result::Err` reports and fails    14 run-time String payload survives
//!   3 `?` on an Err exits                15 the `args` main is eligible too
//!   4 `?` on an Ok binds and continues   16 Ok path under GC stress
//!   5 `?` in argument position           17 Err path under GC stress
//!   6 `?` re-raised one level deeper     18 `match` on a Result inside main
//!   7 several `?` in one expression      19 nothing after a failing `?` runs
//!   8 `?` inside a loop                  20 main really used the walker
//!   9 early `Ok` exit from an `if`       21 other main signatures still E1301
//!  10 a non-main Ok is a value           22 the zero-arg Ok has no HIR gap
//!  11 enum payload, generic report       23 async Result main Ok matches AST
//!  12 class payload, generic report      24 async Result main Err fails
//!                                        25 async Result main failing `?`
//!                                        26 the example is fully LIR

use super::support::{compile_and_run_with_env, compile_error_stderr, compile_with_compiler_env};

const AST: [(&str, &str); 1] = [("WILLOW_LIR_BACKEND", "0")];
const LIR: [(&str, &str); 2] = [("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")];
const LIR_STRESS: [(&str, &str); 3] = [
    ("WILLOW_LIR_BACKEND", "1"),
    ("WILLOW_LIR_REQUIRE", "1"),
    ("WILLOW_GC_STRESS", "alloc"),
];

/// A helper whose `Err` is a `String`, so the report carries a message.
const PARSE: &str = "fn parse_level(raw: i64) -> Result<i64, String> {
    if raw < 0 { return Result::Err(\"level must not be negative\"); }
    return Result::Ok(raw);
}
";

/// Run `source` under all three configurations and require the same output and
/// the same exit disposition from each. `succeeds` is what the process status
/// must be — a `Result<void, E>` main that fails is a *correct* non-zero exit,
/// so both dispositions are asserted rather than only the happy one.
fn assert_main(source: &str, expected: &str, succeeds: bool) {
    for env in [&AST[..], &LIR[..], &LIR_STRESS[..]] {
        let (out, ok) = compile_and_run_with_env(source, env);
        assert_eq!(ok, succeeds, "wrong exit disposition under {env:?}: {out}");
        assert_eq!(out, expected, "wrong output under {env:?}");
    }
    assert_walker_compiled(source, &["main"]);
}

/// Compile once with the selection log on and require the walker to have taken
/// each named function. Without this a coverage regression would still print the
/// right answer — from the AST emitter.
fn assert_walker_compiled(source: &str, functions: &[&str]) {
    let (ok, stderr) = compile_with_compiler_env(
        source,
        &[
            ("WILLOW_LIR_BACKEND", "1"),
            ("WILLOW_LIR_REQUIRE", "1"),
            ("WILLOW_LIR_LOG", "1"),
        ],
    );
    assert!(ok, "logged LIR compile failed: {stderr}");
    for function in functions {
        let sync = format!("[lir] compiling `{function}` from lowered IR");
        let coop = format!("[lir] compiling async `{function}` from lowered IR");
        assert!(
            stderr.contains(&sync) || stderr.contains(&coop),
            "`{function}` did not use the LIR walker: {stderr}"
        );
    }
}

// 1. The success exit. `return Result::Ok()` builds nothing: the only thing that
//    ever reads the object is the exit asking whether the tag is `Err`, so the
//    walker skips the construction and returns from the void `willow_user_main`.
#[test]
fn lir_main_result_01_ok_exits_cleanly() {
    assert_main(
        "fn main() -> Result<void, String> {
    println(1);
    return Result::Ok();
}
",
        "1\n",
        true,
    );
}

// 2. The failure exit. The payload is reported to stderr and the process exits
//    non-zero — the value is NOT returned, because `willow_user_main` is void.
#[test]
fn lir_main_result_02_err_reports_and_fails() {
    assert_main(
        "fn main() -> Result<void, String> {
    println(1);
    return Result::Err(\"boom\");
}
",
        "1\nError: boom\n",
        false,
    );
}

// 3. `?` on an Err takes the same exit rather than returning the failure. This
//    is the second of the walker's two new epilogues.
#[test]
fn lir_main_result_03_try_on_err_exits() {
    assert_main(
        &format!(
            "{PARSE}
fn main() -> Result<void, String> {{
    let level = parse_level(-1)?;
    println(level);
    return Result::Ok();
}}
"
        ),
        "Error: level must not be negative\n",
        false,
    );
}

// 4. The ordinary case: `?` on an Ok binds the payload and control carries on
//    to the success exit.
#[test]
fn lir_main_result_04_try_on_ok_continues() {
    assert_main(
        &format!(
            "{PARSE}
fn main() -> Result<void, String> {{
    let level = parse_level(6)?;
    println(level);
    return Result::Ok();
}}
"
        ),
        "6\n",
        true,
    );
}

// 5. `?` in argument position — the exit is emitted mid-expression, so the
//    partially built call must not be left dangling.
#[test]
fn lir_main_result_05_try_in_argument_position() {
    assert_main(
        &format!(
            "{PARSE}
fn main() -> Result<void, String> {{
    println(parse_level(3)?);
    println(parse_level(-2)?);
    return Result::Ok();
}}
"
        ),
        "3\nError: level must not be negative\n",
        false,
    );
}

// 6. Two levels: the helper's own `?` returns an Err normally, and main's `?`
//    on that Err exits. The two epilogues must not be confused with each other.
#[test]
fn lir_main_result_06_try_reraised_one_level_deeper() {
    assert_main(
        &format!(
            "{PARSE}
fn scaled(raw: i64) -> Result<i64, String> {{
    let level = parse_level(raw)?;
    return Result::Ok(level * 10);
}}
fn main() -> Result<void, String> {{
    println(scaled(4)?);
    println(scaled(-4)?);
    return Result::Ok();
}}
"
        ),
        "40\nError: level must not be negative\n",
        false,
    );
}

// 7. Several `?` in one expression: the first Err wins and the operands to its
//    right are never evaluated.
#[test]
fn lir_main_result_07_several_try_in_one_expression() {
    assert_main(
        &format!(
            "{PARSE}
fn main() -> Result<void, String> {{
    let sum = parse_level(1)? + parse_level(2)? * parse_level(3)?;
    println(sum);
    let bad = parse_level(4)? + parse_level(-5)? + parse_level(6)?;
    println(bad);
    return Result::Ok();
}}
"
        ),
        "7\nError: level must not be negative\n",
        false,
    );
}

// 8. `?` inside a loop: every iteration is a possible exit, so the exit block
//    is reachable from a body block rather than from the function's top level.
#[test]
fn lir_main_result_08_try_inside_a_loop() {
    assert_main(
        &format!(
            "{PARSE}
fn main() -> Result<void, String> {{
    let mut total = 0;
    let mut i = 3;
    while i > -2 {{
        total = total + parse_level(i)?;
        i = i - 1;
    }}
    println(total);
    return Result::Ok();
}}
"
        ),
        "Error: level must not be negative\n",
        false,
    );
}

// 9. An early `return Result::Ok()` from inside an `if` — a success exit that
//    is not the last statement of the body.
#[test]
fn lir_main_result_09_early_ok_exit_from_an_if() {
    assert_main(
        "fn main() -> Result<void, String> {
    let n = 5;
    if n > 2 {
        println(\"early\");
        return Result::Ok();
    }
    return Result::Err(\"not reached\");
}
",
        "early\n",
        true,
    );
}

// 10. The same zero-argument `Result::Ok()` in a function that is NOT main is an
//     ordinary value returned to its caller. Only `main` turns it into an exit.
#[test]
fn lir_main_result_10_non_main_ok_is_a_value() {
    assert_main(
        "fn require_small(n: i64) -> Result<void, String> {
    if n > 9 { return Result::Err(\"too big\"); }
    return Result::Ok();
}
fn main() -> Result<void, String> {
    match require_small(3) {
        Result::Ok(_) => { println(\"small\"); }
        Result::Err(msg) => { println(msg); }
    }
    match require_small(30) {
        Result::Ok(_) => { println(\"small\"); }
        Result::Err(msg) => { println(msg); }
    }
    println(\"still running\");
    return Result::Ok();
}
",
        "small\ntoo big\nstill running\n",
        true,
    );
}

// 11. A non-String error type has no message to print, so the report is the
//     generic one. The walker must pass a null message rather than the payload.
#[test]
fn lir_main_result_11_enum_payload_reports_generically() {
    assert_main(
        "enum Fail { Bad(i64), Worse }
fn main() -> Result<void, Fail> {
    println(1);
    return Result::Err(Fail::Bad(7));
}
",
        "1\nError: main returned Err\n",
        false,
    );
}

// 12. Same for a class payload — a heap object built immediately before the
//     exit, which the report must not try to read as a string.
#[test]
fn lir_main_result_12_class_payload_reports_generically() {
    assert_main(
        "class Boom {
    msg: String;
    pub init(self, msg: String) { self.msg = msg; }
}
fn check(n: i64) -> Result<i64, Boom> {
    if n < 0 { return Result::Err(new Boom(\"bad value\")); }
    return Result::Ok(n);
}
fn main() -> Result<void, Boom> {
    println(check(5)?);
    println(check(-5)?);
    return Result::Ok();
}
",
        "5\nError: main returned Err\n",
        false,
    );
}

// 13. An i64 payload is an immediate, not a pointer: the generic report again,
//     and no attempt to dereference the value.
#[test]
fn lir_main_result_13_i64_payload_reports_generically() {
    assert_main(
        "fn main() -> Result<void, i64> {
    println(1);
    return Result::Err(99);
}
",
        "1\nError: main returned Err\n",
        false,
    );
}

// 14. A String payload built at run time rather than a literal. It is allocated,
//     stored in the Err, and read by the report after the defer flush, so it has
//     to be rooted across everything in between.
#[test]
fn lir_main_result_14_runtime_string_payload_survives() {
    assert_main(
        "fn describe(n: i64) -> Result<i64, String> {
    if n < 0 { return Result::Err(\"negative: \" + n.toString()); }
    return Result::Ok(n);
}
fn main() -> Result<void, String> {
    println(describe(4)?);
    println(describe(-40)?);
    return Result::Ok();
}
",
        "4\nError: negative: -40\n",
        false,
    );
}

// 15. The declared-`args` entry point is eligible in its Result form too — the
//     two eligibility axes (parameters, return type) are independent.
#[test]
fn lir_main_result_15_args_main_is_eligible() {
    assert_main(
        &format!(
            "import std::collections::Array;
{PARSE}
fn main(args: Array<String>) -> Result<void, String> {{
    println(args.len() >= 1);
    println(parse_level(8)?);
    return Result::Ok();
}}
"
        ),
        "false\n8\n",
        true,
    );
}

// 16 and 17 are the GC-stress runs, which `assert_main` performs for every
// perspective above. These two pin the two exits specifically, so a rooting
// regression on either names the exit it broke.

// 16. The success exit under a collection at every allocation.
#[test]
fn lir_main_result_16_ok_path_under_gc_stress() {
    let source = "fn build(n: i64) -> String {
    return \"value \" + n.toString();
}
fn main() -> Result<void, String> {
    let mut i = 0;
    while i < 8 {
        println(build(i));
        i = i + 1;
    }
    return Result::Ok();
}
";
    let (out, ok) = compile_and_run_with_env(source, &LIR_STRESS);
    assert!(ok, "GC-stress run failed: {out}");
    assert_eq!(
        out, "value 0\nvalue 1\nvalue 2\nvalue 3\nvalue 4\nvalue 5\nvalue 6\nvalue 7\n",
        "wrong output under GC stress"
    );
}

// 17. The failure exit under the same stress: the payload is allocated, the
//     roots are popped, and the report still finds the string intact.
#[test]
fn lir_main_result_17_err_path_under_gc_stress() {
    let source = "fn build(n: i64) -> Result<i64, String> {
    if n > 3 { return Result::Err(\"stopped at \" + n.toString()); }
    return Result::Ok(n);
}
fn main() -> Result<void, String> {
    let mut i = 0;
    while i < 8 {
        println(build(i)?);
        i = i + 1;
    }
    return Result::Ok();
}
";
    let (out, ok) = compile_and_run_with_env(source, &LIR_STRESS);
    assert!(!ok, "the Err exit must be non-zero: {out}");
    assert_eq!(out, "0\n1\n2\n3\nError: stopped at 4\n");
}

// 18. A `match` on a Result inside main. The scrutinee's Err is consumed by the
//     arm, so it never reaches an exit — main still ends `Ok`.
#[test]
fn lir_main_result_18_match_on_a_result_inside_main() {
    assert_main(
        &format!(
            "{PARSE}
fn main() -> Result<void, String> {{
    match parse_level(-1) {{
        Result::Ok(v) => {{ println(v); }}
        Result::Err(msg) => {{ println(msg); }}
    }}
    match parse_level(2) {{
        Result::Ok(v) => {{ println(v); }}
        Result::Err(msg) => {{ println(msg); }}
    }}
    return Result::Ok();
}}
"
        ),
        "level must not be negative\n2\n",
        true,
    );
}

// 19. The exit is an exit: nothing after a failing `?` runs, including the
//     statements between it and the end of the body.
#[test]
fn lir_main_result_19_nothing_after_a_failing_try_runs() {
    assert_main(
        &format!(
            "{PARSE}
fn main() -> Result<void, String> {{
    println(\"before\");
    let level = parse_level(-3)?;
    println(\"after\");
    println(level);
    return Result::Ok();
}}
"
        ),
        "before\nError: level must not be negative\n",
        false,
    );
}

// 20. Selection, not behaviour: `main` itself must come from the walker. Every
//     perspective above asserts this through `assert_main`; this one states it
//     on its own so a coverage regression is not reported as an output mismatch.
#[test]
fn lir_main_result_20_main_really_used_the_walker() {
    assert_walker_compiled(
        &format!(
            "{PARSE}
fn main() -> Result<void, String> {{
    println(parse_level(1)?);
    return Result::Ok();
}}
"
        ),
        &["parse_level", "main"],
    );
}

// 21. The walker's eligibility test covers every entry point the checker
//     accepts: anything other than `void` or `Result<void, E>` is rejected at
//     E1301 long before backend selection, so there is no legal main left in
//     the fallback class.
#[test]
fn lir_main_result_21_other_main_signatures_still_reject() {
    for source in [
        "fn main() -> i64 { return 0; }\n",
        "fn main() -> Result<i64, String> { return Result::Ok(1); }\n",
        "fn main() -> String { return \"x\"; }\n",
    ] {
        let stderr = compile_error_stderr(source);
        assert!(
            stderr.contains("E1301"),
            "expected an entry-point signature error for {source:?}: {stderr}"
        );
    }
}

// 22. The regression that kept main out of the walker in the first place. The
//     checker special-cases a zero-argument `Result::Ok()` and used to return
//     before recording its type, so HIR lowering could not type the call, gave
//     up on the whole function, and the LIR never saw it — the fallback reason
//     was "it has no lowered IR", not an unsupported form.
#[test]
fn lir_main_result_22_zero_arg_ok_has_no_hir_gap() {
    let (ok, stderr) = compile_with_compiler_env(
        "fn main() -> Result<void, String> { return Result::Ok(); }\n",
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")],
    );
    assert!(ok, "compile failed: {stderr}");
    assert!(
        !stderr.contains("no lowered IR"),
        "the zero-argument Ok must lower to HIR: {stderr}"
    );
}

// 23. An `async` Result main is walker-compiled too, and its successful result
//     is published through the cooperative frame before the driver exits.
#[test]
fn lir_main_result_23_async_result_main_matches_the_ast() {
    let source = "async fn work(n: i64) -> i64 {
    await yield();
    return n * 2;
}
async fn main() -> Result<void, String> {
    println(await work(21));
    return Result::Ok();
}
";
    assert_main(source, "42\n", true);
    assert_walker_compiled(source, &["work", "main"]);
}

// 24. The cooperative driver must inspect the published result after main's
//     task completes. An Err reports its String payload and exits non-zero on
//     both the AST and LIR poll-body paths (willow-4ylu).
#[test]
fn lir_main_result_24_async_err_reports_and_fails() {
    assert_main(
        "async fn main() -> Result<void, String> {
    await yield();
    return Result::Err(\"async main failed\");
}
",
        "Error: async main failed\n",
        false,
    );
}

// 25. A propagated Err takes the same cooperative publication path as an
//     explicit return and is shaped by the driver only after pending defers
//     have run.
#[test]
fn lir_main_result_25_async_failing_try_reports_and_fails() {
    assert_main(
        &format!(
            "{PARSE}async fn main() -> Result<void, String> {{
    defer println(\"cleanup\");
    await yield();
    let level = parse_level(-1)?;
    println(level);
    return Result::Ok();
}}
"
        ),
        "cleanup\nError: level must not be negative\n",
        false,
    );
}

// 26. The runnable example is walker-compiled function by function. Its output
//     is asserted by the `example/*.wi` table in `runtime.rs`; this test is about
//     the selection, so a coverage regression names the function it lost instead
//     of passing as a silent AST run.
#[test]
fn lir_main_result_26_the_example_is_fully_lir() {
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/example/lir_main_result.wi"
    ))
    .expect("the example is checked in");
    assert_walker_compiled(
        &source,
        &[
            "parse_level",
            "scaled_level",
            "classify",
            "name_of",
            "require_in_range",
            "main",
        ],
    );
}
