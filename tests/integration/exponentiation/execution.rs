//! Integer faults, operand evaluation, build profiles, GC, and async execution.

use crate::support::*;

// ── 13. Negative dynamic exponent panics ─────────────────────────────────────

#[test]
fn pow_int_13_negative_dynamic_exponent_panics_with_location() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn pow(base: i64, exponent: i64) -> i64 {
    return base ** exponent;
}

fn main() {
    println("before");
    println(pow(2, 0 - 3));
    println("after");
}
"#,
    );
    assert!(!ok, "a negative exponent must abort the program: {out}");
    assert!(out.contains("before"), "{out}");
    assert!(
        !out.contains("after"),
        "execution must not continue past the fault: {out}"
    );
    assert!(
        out.contains("negative exponent in integer `**`: -3"),
        "the diagnostic should name the offending exponent: {out}"
    );
    assert!(
        out.contains(":3:17"),
        "the diagnostic should point at the `**` expression: {out}"
    );
}

// ── 14. That panic is recoverable ────────────────────────────────────────────

#[test]
fn pow_int_14_negative_dynamic_exponent_is_recoverable() {
    let (out, ok) = compile_and_run(
        r#"
fn pow(base: i64, exponent: i64) -> i64 {
    return base ** exponent;
}

fn guarded(exponent: i64) -> i64 {
    // A recovery-capable defer cannot be the outermost scope of a value
    // returning function, so it lives in a nested block (E0905).
    let mut result = 0;
    if true {
        defer match recover() {
            Some(info) => println("recovered: " + info.message),
            None => println("no panic")
        }
        result = pow(2, exponent);
    }
    return result;
}

fn main() {
    println(guarded(3));
    println(guarded(0 - 1));
    println("done");
}
"#,
    );
    assert!(ok, "a recovered power fault must not abort: {out}");
    assert_eq!(
        out, "no panic\n8\nrecovered: negative exponent in integer `**`: -1\n0\ndone\n",
        "the recovered call returns the zero value and execution continues"
    );
}

// ── 15. Negative literal exponent is a compile error ─────────────────────────

#[test]
fn pow_int_15_negative_literal_exponent_is_a_compile_error() {
    let stderr = compile_error_stderr(
        r#"
fn main() {
    println(2 ** -3);
}
"#,
    );
    assert!(stderr.contains("error[E0204]"), "{stderr}");
    assert!(
        stderr.contains("negative exponent in an integer `**`"),
        "{stderr}"
    );
    assert!(
        stderr.contains("f64") || stderr.contains("1 / (x ** 3)"),
        "the help should offer a way forward: {stderr}"
    );

    // `-0` is zero, not a negative exponent, so it must still compile.
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    println(2 ** -0);
}
"#,
    );
    assert!(ok, "`2 ** -0` should compile: {out}");
    assert_eq!(out, "1\n");
}

// ── 17. Evaluation order ─────────────────────────────────────────────────────

#[test]
fn pow_int_17_operands_evaluate_once_left_to_right() {
    let (out, ok) = compile_and_run(
        r#"
fn trace(tag: i64, value: i64) -> i64 {
    println(tag);
    return value;
}

fn main() {
    println(trace(1, 3) ** trace(2, 2));
    println(trace(3, 2) ** 5);
}
"#,
    );
    assert!(ok, "{out}");
    // Base first, then exponent, each exactly once — a constant exponent must
    // not cause the base to be emitted twice by the unrolled chain.
    assert_eq!(out, "1\n2\n9\n3\n32\n");
}

// ── 18. The base runs even when the result folds away ────────────────────────

#[test]
fn pow_int_18_base_is_evaluated_even_for_exponent_zero() {
    // `x ** 0` is a constant 1, but the base is a real expression and its side
    // effects must survive the fold.
    let (out, ok) = compile_and_run(
        r#"
fn trace(value: i64) -> i64 {
    println("base ran");
    return value;
}

fn main() {
    println(trace(7) ** 0);
    let zero = 0;
    println(trace(7) ** zero);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "base ran\n1\nbase ran\n1\n");
}

// ── 19. Every integer `**` shape in one program ──────────────────────────────

#[test]
fn pow_int_19_every_integer_power_shape_in_one_program() {
    const SOURCE: &str = r#"
fn pow(base: i64, exponent: i64) -> i64 {
    return base ** exponent;
}

fn main() {
    println(2 ** 10);
    println(3 ** 5);
    println(pow(3, 5));
    println(pow(2, 64));
    println(-2 ** 3);
    println(2 ** 3 ** 2);
    println(pow(0 - 2, 5));
    println(2 ** 0);
}
"#;

    let (out, ok) = compile_and_run(SOURCE);
    assert!(ok, "{out}");
    assert_eq!(out, "1024\n243\n243\n0\n-8\n512\n-32\n1\n");
}

#[test]
fn pow_int_19b_lir_backend_reports_the_same_negative_exponent_fault() {
    let (out, ok) = compile_and_run_with_env(
        r#"
fn pow(base: i64, exponent: i64) -> i64 {
    return base ** exponent;
}

fn main() {
    println(pow(2, 0 - 4));
}
"#,
        &[],
    );
    assert!(!ok, "the LIR path must raise the same fault: {out}");
    assert!(
        out.contains("negative exponent in integer `**`: -4"),
        "{out}"
    );
}

// ── 20. Build-profile agreement ──────────────────────────────────────────────

#[test]
fn pow_int_20_release_build_matches_debug_build() {
    const SOURCE: &str = r#"
fn pow(base: i64, exponent: i64) -> i64 {
    return base ** exponent;
}

fn main() {
    println(2 ** 10);
    println(pow(3, 13));
    println(pow(2, 63));
    println(-3 ** 3);
    println(pow(0 - 1, 63));
}
"#;

    let (debug_out, debug_ok) = compile_and_run(SOURCE);
    assert!(debug_ok, "debug: {debug_out}");

    let (release_out, release_ok) = compile_and_run_release(SOURCE);
    assert!(release_ok, "release: {release_out}");

    assert_eq!(
        debug_out, release_out,
        "optimizations must not change `**` results"
    );
    assert_eq!(debug_out, "1024\n1594323\n-9223372036854775808\n-27\n-1\n");
}

#[test]
fn pow_int_20b_release_build_keeps_the_negative_exponent_check() {
    // The guard is a safety check, not debug instrumentation, so `--release`
    // must keep it.
    let (out, ok) = compile_and_run_release(
        r#"
fn pow(base: i64, exponent: i64) -> i64 {
    return base ** exponent;
}

fn main() {
    defer match recover() {
        Some(info) => println("recovered: " + info.message),
        None => println("no panic")
    }
    println(pow(2, 0 - 2));
}
"#,
    );
    assert!(ok, "the fault should be recovered, not fatal: {out}");
    assert!(
        out.contains("recovered: negative exponent in integer `**`: -2"),
        "release builds must keep the negative-exponent guard: {out}"
    );
}

// ── 21. Composition with the rest of the language ────────────────────────────

#[test]
fn pow_int_21_powers_compose_with_recursion_and_loops() {
    let (out, ok) = compile_and_run(
        r#"
fn sum_of_squares(n: i64) -> i64 {
    if n <= 0 {
        return 0;
    }
    return n ** 2 + sum_of_squares(n - 1);
}

fn main() {
    println(sum_of_squares(5));

    let mut total = 0;
    let mut i = 0;
    while i < 5 {
        total = total + 2 ** i;
        i = i + 1;
    }
    println(total);

    let mut nested = 0;
    let mut a = 1;
    while a <= 3 {
        let mut b = 1;
        while b <= 3 {
            nested = nested + a ** b;
            b = b + 1;
        }
        a = a + 1;
    }
    println(nested);
}
"#,
    );
    assert!(ok, "{out}");
    // 1+4+9+16+25 = 55; 1+2+4+8+16 = 31;
    // (1+1+1) + (2+4+8) + (3+9+27) = 3 + 14 + 39 = 56.
    assert_eq!(out, "55\n31\n56\n");
}

// ── 22. GC interaction ───────────────────────────────────────────────────────

#[test]
fn pow_int_22_gc_managed_values_survive_across_a_power() {
    // Collecting on every allocation turns a missing GC root around the power's
    // temporaries into a deterministic failure.
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn pow(base: i64, exponent: i64) -> i64 {
    return base ** exponent;
}

fn main() {
    let label = "value: ";
    let values = [2, 3, 4];
    let mut i = 0;
    let mut total = 0;
    while i < 3 {
        total = total + values[i] ** 3;
        i = i + 1;
    }
    println(label + total.toString());
    println(label + pow(values[0], values[2]).toString());
}
"#,
    );
    assert!(ok, "{out}");
    // 8 + 27 + 64 = 99; 2 ** 4 = 16.
    assert_eq!(out, "value: 99\nvalue: 16\n");
}

// ── 23. Interaction with `await` ─────────────────────────────────────────────

#[test]
fn pow_int_23_await_binds_tighter_than_the_power_operator() {
    let (out, ok) = compile_and_run(
        r#"
async fn base(x: i64) -> i64 {
    return x;
}

async fn main() {
    let t = base(3);
    println(await t ** 2);

    let u = base(2);
    let e = base(5);
    println(await u ** await e);
}
"#,
    );
    assert!(ok, "{out}");
    // `await t ** 2` is `(await t) ** 2` = 9, not `await (t ** 2)`.
    assert_eq!(out, "9\n32\n");
}
