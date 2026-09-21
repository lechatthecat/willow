use super::*;

// ── divergence at runtime (willow-0g8j.2.5) ─────────────────────────────────
//
// The `d*` unit tests in `src/backend/cranelift/lir_gen.rs` pin the
// eligibility boundary. These pin the BEHAVIOUR: a `panic` is an unwind, not a
// value, so what has to hold is the message on stderr, the frames under it,
// the output produced before it, and the fact that the process does not exit
// cleanly.

#[test]
fn lir_div_01_statement_panic_reports_and_fails() {
    // The base case: `panic(...)` as a whole statement. The message reaches
    // stderr, the statements before it still ran, and the process does not
    // exit successfully.
    let source = r#"
fn check(n: i64) -> i64 {
    if n < 0 { panic("negative input"); }
    return n;
}
fn main() { println(check(1)); println(check(-1)); }
"#;
    let (out, ok) = compile_with_env_and_run_combined(source, &PLAIN);
    assert!(!ok, "the program must panic: {out}");
    assert!(
        out.contains("negative input"),
        "output lost the message: {out}"
    );
    assert!(
        out.starts_with("1\n"),
        "the statements before the panic must still run: {out}"
    );
}

#[test]
fn lir_div_02_formatted_panic_message_is_interpolated() {
    // The interpolated form goes through the same operand rendering as
    // `format`, so a crossed operand would show up as a wrong message rather
    // than as a crash.
    let source = r#"
fn div(a: i64, b: i64) -> i64 {
    if b == 0 { panic("cannot divide {} by {}", a, b); }
    return a / b;
}
fn main() { println(div(10, 2)); println(div(7, 0)); }
"#;
    let (out, ok) = compile_with_env_and_run_combined(source, &PLAIN);
    assert!(!ok, "the program must panic: {out}");
    assert!(
        out.contains("cannot divide 7 by 0"),
        "output has the wrong message: {out}"
    );
}

#[test]
fn lir_div_03_panicking_match_arm_ends_only_its_arm() {
    // A panic in ARM position ends the arm's block instead of jumping to the
    // merge. The arms that do produce values must be unaffected.
    let source = r#"
fn level(n: i64) -> String {
    return match n {
        1 => "low",
        2 => "high",
        _ => panic("no level {}", n),
    };
}
fn main() { println(level(1)); println(level(2)); println(level(9)); }
"#;
    let (out, ok) = compile_with_env_and_run_combined(source, &PLAIN);
    assert!(!ok, "the program must panic: {out}");
    assert!(
        out.starts_with("low\nhigh\n") && out.contains("no level 9"),
        "the value arms must still produce their answers: {out}"
    );
}

#[test]
fn lir_div_04_panic_frames_name_the_call_chain() {
    // Divergence must not cost the call stack: the frame for the panicking
    // function and the frame for its caller have to appear, in that order.
    let source = r#"
fn inner(n: i64) -> i64 { panic("boom {}", n); }
fn outer(n: i64) -> i64 { return inner(n); }
fn main() { println(outer(3)); }
"#;
    let (out, ok) = compile_with_env_and_run_combined(source, &PLAIN);
    assert!(!ok, "the program must panic: {out}");
    let callee = out
        .find("0: inner")
        .unwrap_or_else(|| panic!("trace has no callee frame: {out}"));
    let caller = out
        .find("1: outer")
        .unwrap_or_else(|| panic!("trace has no caller frame: {out}"));
    assert!(callee < caller, "trace is out of order: {out}");
}

#[test]
fn lir_div_05_returning_match_arms_match() {
    // Every arm leaves, so the match is typed `!` and its merge block is
    // unreachable. Reading the result variable there would be undefined; the
    // outputs must still agree exactly.
    assert_program_output(
        r#"
fn classify(n: i64) -> String {
    match n {
        0 => return "zero",
        1 => return "one",
        _ => return "many",
    }
}
fn main() { println(classify(0)); println(classify(1)); println(classify(7)); }
"#,
        "zero\none\nmany\n",
    );
}

#[test]
fn lir_div_06_diverging_and_value_arms_in_one_match() {
    // A `return` arm beside a value arm: the value arm still has to reach the
    // merge and flow out of the match.
    assert_program_output(
        r#"
fn describe(n: i64) -> String {
    return match n {
        0 => "nothing",
        _ => { println("saw " + n.toString()); return "something"; }
    };
}
fn main() { println(describe(0)); println(describe(4)); }
"#,
        "nothing\nsaw 4\nsomething\n",
    );
}

#[test]
fn lir_div_07_nested_diverging_arms() {
    // Divergence nests: the outer arm's tail is itself an all-returning match,
    // so one block must acquire exactly one terminator.
    assert_program_output(
        r#"
fn grid(row: i64, col: i64) -> String {
    match row {
        0 => match col {
            0 => return "origin",
            _ => return "top",
        },
        _ => return "body",
    }
}
fn main() { println(grid(0, 0)); println(grid(0, 3)); println(grid(2, 0)); }
"#,
        "origin\ntop\nbody\n",
    );
}

#[test]
fn lir_div_08_scalar_to_string_and_format_match() {
    // The string machinery the panics build their messages from, exercised on
    // its own so a rendering difference is not mistaken for a divergence bug.
    assert_program_output(
        r#"
fn main() {
    println((42).toString());
    println((2.5).toString());
    println(true.toString());
    println("willow".toString());
    println(format("{} items", 3));
    println(format("{} and {}", "left", "right"));
    println(format("{:.6f}", 3.14159265));
    println(format("{{literal}} {}", 9));
}
"#,
        "42\n2.5\ntrue\nwillow\n3 items\nleft and right\n3.141593\n{literal} 9\n",
    );
}

#[test]
fn lir_div_09_operand_position_panic_terminates_evaluation() {
    // Never operands are supported: the panic must terminate evaluation before
    // the consuming println, the return, or the caller's continuation runs.
    let source = r#"
fn f() -> i64 {
    println("before");
    println(panic("operand panic"));
    println("wrong continuation");
    return 1;
}
fn main() { println(f()); }
"#;
    let (out, ok) = compile_with_env_and_run_combined(source, &PLAIN);
    assert!(!ok, "the program must panic: {out}");
    assert!(
        out.starts_with("before\nruntime panic: operand panic at "),
        "only the evaluated prefix and panic diagnostic should be printed: {out}"
    );
    assert!(!out.contains("wrong continuation"), "{out}");
    assert!(!out.lines().any(|line| line == "1"), "{out}");
}

#[test]
fn lir_div_10_a_panic_that_is_not_taken_exits_cleanly() {
    // The guard shape in real code: the panic is compiled but never reached,
    // so the program must exit normally and print nothing extra.
    assert_program_output(
        r#"
fn require_positive(n: i64) -> i64 {
    if n <= 0 { panic("expected a positive value, got " + n.toString()); }
    return n;
}
fn main() { println(require_positive(5)); println(require_positive(1)); }
"#,
        "5\n1\n",
    );
}

#[test]
fn lirreq_57_function_values_example_is_fully_lir() {
    // Same contract again for the function-value example: named functions used
    // as values, lifted lambdas, indirect calls and the callable-taking
    // combinators all have to stay inside the walker's subset, since a body
    // outside it is a compile error.
    let source = include_str!("../../../example/lir_function_values.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &PLAIN);
    assert!(
        ok,
        "example/lir_function_values.wi must compile with every free function on the LIR path: {stderr}"
    );
}

#[test]
fn lirreq_59_closures_example_is_fully_lir() {
    // The capturing half of the same contract (willow-0g8j.2.12). A closure
    // brings an environment allocation, a hidden leading argument and an
    // indirect call whose address comes out of a heap word, so a body that
    // slipped out of the walker's subset would show up here as a compile error
    // rather than as a fallback.
    let source = include_str!("../../../example/lir_closures.wi");
    let (ok, stderr) = compile_with_compiler_env(source, &PLAIN);
    assert!(
        ok,
        "example/lir_closures.wi must compile with every free function on the LIR path: {stderr}"
    );
}
