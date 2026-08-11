//! End-to-end perspectives for native `i64 **` and `f64 **` (willow-n5yv).
//!
//! Stage 1 (willow-n5yv.2) gave `**` a lexer token, a right-associative parse
//! and a type rule, but every well-typed power was rejected by a codegen gate.
//! Stage 2 lowers `i64 ** i64` natively: a literal exponent unrolls into a
//! chain of `imul`s and everything else — a variable, or a constant expression
//! like `1 + 2`, which no pass folds yet — becomes a bounded square-and-multiply
//! loop, so no runtime `pow` is imported. A negative exponent has no integer
//! result: a negative *literal* is a compile error (E0204) and a negative
//! *value* raises a recoverable language panic.
//!
//! The instruction *schedule* is unit-tested in
//! `src/backend/cranelift/emit_pow.rs` (`pow_plan_01..12`); the type rules are
//! unit-tested in `src/semantic/type_checker/mod.rs` (`pow_type_01..25`). This
//! file covers what only a running binary can show: emitted values, evaluation
//! order, panic behaviour, and agreement across the two backends and both
//! build profiles.
//!
//! Perspectives 1–26 cover the integer path; `pow_f64_27` onward cover the
//! numerical kernel, IEEE dispatch, compatibility spellings, backend parity,
//! object linkage, async interaction, and versioned accuracy corpora.
//!
//! Integer perspectives:
//!   1  constant exponents 0..10 of a fixed base
//!   2  constant exponent equals repeated multiplication for many bases
//!   3  exponent 0 is 1 for every base, including 0 and negatives
//!   4  exponent 1 returns the base unchanged
//!   5  negative base parity (odd exponent negative, even exponent positive)
//!   6  overflow wraps modulo 2^64, exactly like `*`
//!   7  right associativity
//!   8  precedence over `*`, `/`, `%`, `+`, `-`
//!   9  binds tighter than prefix `-`
//!  10  a power is an ordinary operand in comparisons and `bool` contexts
//!  11  a dynamic exponent agrees with the constant form for every exponent
//!  12  a large dynamic exponent is bounded work, not `n` multiplications
//!  13  a negative dynamic exponent panics with the value and source location
//!  14  that panic is recoverable through `defer` + `recover()`
//!  15  a negative *literal* exponent is a compile error, `-0` is not
//!  16  native `f64` powers run, and mixed operands are a type error
//!  17  both operands are evaluated exactly once, left to right
//!  18  the base is evaluated even when the exponent folds the result away
//!  19  the LIR backend produces the same values as the AST backend
//!  20  a release build produces the same values as a debug build
//!  21  powers compose with recursion and loop accumulation
//!  22  GC-managed values stay live across a power under allocation stress
//!  23  `await` binds tighter than `**` inside an `async fn`
//!  24  extreme bases (i64::MIN / i64::MAX) wrap instead of trapping
//!  25  the runtime ABI declares the panic raiser, and integer powers stay exact
//!  26  the emitted object has no call relocation for an integer power

use super::support::*;

// ── 1. Constant exponents ────────────────────────────────────────────────────

#[test]
fn pow_int_01_constant_exponents_zero_through_ten() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    println(2 ** 0);
    println(2 ** 1);
    println(2 ** 2);
    println(2 ** 3);
    println(2 ** 4);
    println(2 ** 5);
    println(2 ** 6);
    println(2 ** 7);
    println(2 ** 8);
    println(2 ** 9);
    println(2 ** 10);
}
"#,
    );
    assert!(ok, "constant powers should compile and run: {out}");
    assert_eq!(out, "1\n2\n4\n8\n16\n32\n64\n128\n256\n512\n1024\n");
}

// ── 2. Constant exponent equals repeated multiplication ──────────────────────

#[test]
fn pow_int_02_constant_exponent_matches_repeated_multiplication() {
    // The unrolled squaring chain must agree with the naive product for bases
    // that are not powers of two and exponents whose bit patterns differ
    // (3 = 0b11, 5 = 0b101, 7 = 0b111, 8 = 0b1000).
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    println(3 ** 5);
    println(3 * 3 * 3 * 3 * 3);
    println(7 ** 3);
    println(7 * 7 * 7);
    println(10 ** 7);
    println(6 ** 8);
    println(6 * 6 * 6 * 6 * 6 * 6 * 6 * 6);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(
        out, "243\n243\n343\n343\n10000000\n1679616\n1679616\n",
        "square-and-multiply must equal the product chain"
    );
}

// ── 3. Exponent zero ─────────────────────────────────────────────────────────

#[test]
fn pow_int_03_exponent_zero_is_one_for_every_base() {
    // Including 0 ** 0 == 1 (the empty product), and via a dynamic exponent so
    // the loop's exit-on-entry path is covered too.
    let (out, ok) = compile_and_run(
        r#"
fn pow(base: i64, exponent: i64) -> i64 {
    return base ** exponent;
}

fn main() {
    println(0 ** 0);
    println(1 ** 0);
    println(2 ** 0);
    println((0 - 5) ** 0);
    println(pow(0, 0));
    println(pow(0 - 5, 0));
    println(pow(9223372036854775807, 0));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n1\n1\n1\n1\n1\n1\n");
}

// ── 4. Exponent one ──────────────────────────────────────────────────────────

#[test]
fn pow_int_04_exponent_one_returns_the_base() {
    // Exponent 1 is a single `Mul` against the seed and no squaring, so a bug in
    // the "stop before the trailing square" rule shows up here first.
    let (out, ok) = compile_and_run(
        r#"
fn pow(base: i64, exponent: i64) -> i64 {
    return base ** exponent;
}

fn main() {
    println(7 ** 1);
    println(0 ** 1);
    println((0 - 7) ** 1);
    println(pow(7, 1));
    println(pow(0 - 7, 1));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n0\n-7\n7\n-7\n");
}

// ── 5. Negative bases ────────────────────────────────────────────────────────

#[test]
fn pow_int_05_negative_base_parity() {
    let (out, ok) = compile_and_run(
        r#"
fn pow(base: i64, exponent: i64) -> i64 {
    return base ** exponent;
}

fn main() {
    println((0 - 2) ** 1);
    println((0 - 2) ** 2);
    println((0 - 2) ** 3);
    println((0 - 2) ** 4);
    println(pow(0 - 3, 3));
    println(pow(0 - 3, 4));
    println(pow(0 - 1, 63));
    println(pow(0 - 1, 64));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-2\n4\n-8\n16\n-27\n81\n-1\n1\n");
}

// ── 6. Wrapping overflow ─────────────────────────────────────────────────────

#[test]
fn pow_int_06_overflow_wraps_like_multiplication() {
    // `**` is a multiplication chain, so it wraps modulo 2^64 exactly as `*`
    // does — it must not trap and must not saturate.
    let (out, ok) = compile_and_run(
        r#"
fn pow(base: i64, exponent: i64) -> i64 {
    return base ** exponent;
}

fn main() {
    println(2 ** 62);
    println(2 ** 63);
    println(2 ** 64);
    println(2 ** 65);
    println(pow(2, 64));
    println(pow(2, 200));
    println(3 ** 41);
    println(pow(3, 41));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(
        out,
        // 2**63 wraps to i64::MIN; 2**64 and beyond are 0; 3**41 wraps to a
        // negative value (checked against Rust's i64::wrapping_pow).
        "4611686018427387904\n-9223372036854775808\n0\n0\n0\n0\n\
         -420491770248316829\n-420491770248316829\n"
    );
}

// ── 7. Right associativity ───────────────────────────────────────────────────

#[test]
fn pow_int_07_is_right_associative() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    println(2 ** 3 ** 2);
    println(2 ** (3 ** 2));
    println((2 ** 3) ** 2);
    println(2 ** 2 ** 2 ** 2);
}
"#,
    );
    assert!(ok, "{out}");
    // 2**(3**2) = 2**9 = 512, while (2**3)**2 = 64.
    // 2**(2**(2**2)) = 2**(2**4) = 2**16 = 65536.
    assert_eq!(out, "512\n512\n64\n65536\n");
}

// ── 8. Precedence against the other arithmetic operators ─────────────────────

#[test]
fn pow_int_08_binds_tighter_than_multiplicative_and_additive() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    println(2 * 3 ** 2);
    println(2 + 3 ** 2);
    println(100 - 3 ** 2);
    println(100 / 2 ** 2);
    println(100 % 3 ** 2);
    println(2 ** 2 * 3 ** 2);
    println(2 ** 3 + 3 ** 2);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "18\n11\n91\n25\n1\n36\n17\n");
}

// ── 9. Precedence against prefix negation ────────────────────────────────────

#[test]
fn pow_int_09_binds_tighter_than_prefix_negation() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    println(-2 ** 2);
    println((-2) ** 2);
    println(-2 ** 3);
    println(-(2 ** 3));
    let x = 2;
    println(-x ** 2);
}
"#,
    );
    assert!(ok, "{out}");
    // `-2 ** 2` is `-(2 ** 2)` = -4, not `(-2) ** 2` = 4.
    assert_eq!(out, "-4\n4\n-8\n-8\n-4\n");
}

// ── 10. A power is an ordinary operand ───────────────────────────────────────

#[test]
fn pow_int_10_power_is_an_ordinary_operand() {
    let (out, ok) = compile_and_run(
        r#"
fn twice(n: i64) -> i64 {
    return n * 2;
}

fn main() {
    let big: bool = 2 ** 10 > 1000;
    println(big);
    println(2 ** 10 == 1024);
    println(twice(3 ** 2));
    let values = [2 ** 1, 2 ** 2, 2 ** 3];
    println(values[2 ** 0]);
    let mut acc = 0;
    acc = acc + 2 ** 4;
    println(acc);
    if 3 ** 3 > 26 {
        println("gt");
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\ntrue\n18\n4\n16\ngt\n");
}

// ── 11. Dynamic exponent agrees with the constant form ───────────────────────

#[test]
fn pow_int_11_dynamic_exponent_matches_constant_exponent() {
    // The dynamic loop and the unrolled chain are two different emitters; walk
    // every exponent 0..=20 for a non-trivial base and compare against a naive
    // multiplication loop computed in the same program.
    let (out, ok) = compile_and_run(
        r#"
fn naive(base: i64, exponent: i64) -> i64 {
    let mut result = 1;
    let mut i = 0;
    while i < exponent {
        result = result * base;
        i = i + 1;
    }
    return result;
}

fn main() {
    let base = 3;
    let mut e = 0;
    let mut mismatches = 0;
    while e <= 20 {
        if base ** e != naive(base, e) {
            mismatches = mismatches + 1;
        }
        e = e + 1;
    }
    println(mismatches);
    println(3 ** 13);
    let thirteen = 13;
    println(3 ** thirteen);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1594323\n1594323\n");
}

// ── 12. A large dynamic exponent is bounded work ─────────────────────────────

#[test]
fn pow_int_12_large_dynamic_exponent_is_bounded_work() {
    // Square-and-multiply runs once per exponent bit, so a 62-bit exponent is
    // ~62 iterations. A naive `n`-iteration lowering would not finish this test
    // in reasonable time for exponent 4611686018427387904.
    let (out, ok) = compile_and_run(
        r#"
fn pow(base: i64, exponent: i64) -> i64 {
    return base ** exponent;
}

fn main() {
    println(pow(1, 4611686018427387904));
    println(pow(0 - 1, 4611686018427387904));
    println(pow(2, 62));
    println(pow(0, 9223372036854775807));
}
"#,
    );
    assert!(ok, "{out}");
    // 1**huge = 1; (-1)**even = 1; 2**62 fits; 0**huge = 0.
    assert_eq!(out, "1\n1\n4611686018427387904\n0\n");
}

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

// ── 16. Native floating powers ───────────────────────────────────────────────

#[test]
fn pow_int_16_float_powers_run_natively_and_mixed_operands_are_type_errors() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    println(2.0 ** 3.0);
    println(2.0 ** 0.5);
}
"#,
    );
    assert!(ok, "native f64 power should compile: {out}");
    assert_eq!(out, "8\n1.414213562373095\n");

    let mixed_stderr = compile_error_stderr(
        r#"
fn main() {
    println(2 ** 3.0);
}
"#,
    );
    assert!(
        mixed_stderr.contains("error[E0202]") || mixed_stderr.contains("error[E0201]"),
        "mixing i64 and f64 operands must be a type error: {mixed_stderr}"
    );
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

// ── 19. Backend agreement ────────────────────────────────────────────────────

#[test]
fn pow_int_19_lir_backend_matches_the_ast_backend() {
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

    let (ast_out, ast_ok) = compile_and_run(SOURCE);
    assert!(ast_ok, "AST backend: {ast_out}");

    let (lir_out, lir_ok) = compile_and_run_with_env(
        SOURCE,
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")],
    );
    assert!(lir_ok, "LIR backend: {lir_out}");
    assert_eq!(
        ast_out, lir_out,
        "the LIR walker and the AST emitter must lower `**` identically"
    );
    assert_eq!(ast_out, "1024\n243\n243\n0\n-8\n512\n-32\n1\n");
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
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")],
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

// ── 24. Extreme bases ────────────────────────────────────────────────────────

#[test]
fn pow_int_24_extreme_bases_wrap_instead_of_trapping() {
    let (out, ok) = compile_and_run(
        r#"
fn pow(base: i64, exponent: i64) -> i64 {
    return base ** exponent;
}

fn main() {
    let max = 9223372036854775807;
    let min = 0 - 9223372036854775807 - 1;
    println(max ** 1);
    println(min ** 1);
    println(max ** 2);
    println(pow(max, 2));
    println(min ** 2);
    println(pow(min, 3));
    println(max ** 0);
    println(min ** 0);
}
"#,
    );
    assert!(ok, "{out}");
    // i64::MAX**2 wraps to 1; i64::MIN**2 and i64::MIN**3 wrap to 0.
    assert_eq!(
        out,
        "9223372036854775807\n-9223372036854775808\n1\n1\n0\n0\n1\n1\n"
    );
}

// ── 25. ABI surface ──────────────────────────────────────────────────────────

#[test]
fn pow_int_25_abi_declares_the_raiser_and_integer_powers_import_no_float_pow() {
    use willow_compiler::backend::abi::{RUNTIME_SYMBOLS, RuntimeEffects};

    let raiser = RUNTIME_SYMBOLS
        .iter()
        .find(|symbol| symbol.name == "willow_pow_negative_exponent")
        .expect("the negative-exponent raiser must be part of the runtime ABI");
    assert_eq!(raiser.ret, None, "the raiser returns nothing; it faults");
    assert_eq!(
        raiser.params.len(),
        4,
        "raiser takes (exponent, file, line, column)"
    );
    let effects = raiser.effects();
    assert!(
        effects.contains(RuntimeEffects::MAY_PANIC),
        "the raiser must be classified as panicking so callers emit the check"
    );
    assert!(
        effects.contains(RuntimeEffects::MAY_ALLOCATE),
        "the raiser formats a message, so it allocates (same as willow_int_div_panic)"
    );
    // A power never parks the task: it must not be treated as a suspension or
    // preemption point, or the scheduler would insert state-machine edges for it.
    for forbidden in [
        RuntimeEffects::MAY_BLOCK,
        RuntimeEffects::MAY_SUSPEND,
        RuntimeEffects::MAY_PREEMPT,
        RuntimeEffects::NO_PREEMPT_REGION,
    ] {
        assert!(
            !effects.contains(forbidden),
            "the negative-exponent raiser must not carry scheduling effects"
        );
    }

    // `i64 **` is a pure instruction sequence, not a call into the runtime's
    // float `pow`. Perspective 26 proves that structurally, from the emitted
    // object's relocations; this checks the observable consequence, which is
    // that the result stays exact: 3**40 is 12157665459056928801, which is not
    // representable in an f64. A lowering that detoured through `pow(f64, f64)`
    // would return the nearest double, 12157665459056928768, and wrap to a
    // different i64.
    let (out, ok) = compile_and_run(
        r#"
fn pow(base: i64, exponent: i64) -> i64 {
    return base ** exponent;
}

fn main() {
    println(3 ** 40);
    println(pow(3, 40));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(
        out,
        // 12157665459056928801 - 2**64. The f64 detour would print
        // -6289078614652622848 instead.
        "-6289078614652622815\n-6289078614652622815\n",
        "integer `**` must stay exact, so it cannot be routed through f64 pow"
    );
}

// ── 26. Emitted code, not just emitted values ────────────────────────────────

/// The structural half of perspective 25: `i64 **` must not *call* anything to
/// compute a power, on either backend.
///
/// The check reads the relocations of the object file the backend emitted.
/// Neither of the two obvious alternatives works:
///
/// A relocation exists only where an instruction names a symbol. The positive
/// control at the end proves the check can see the generated local f64 helper,
/// while also proving the retired runtime import is absent.
#[test]
fn pow_int_26_integer_powers_emit_no_call_relocation() {
    // Both shapes of lowering in one program: `3 ** 40` unrolls (literal
    // exponent), `base ** exponent` takes the square-and-multiply loop.
    const INTEGER_POWERS: &str = r#"
fn dynamic(base: i64, exponent: i64) -> i64 {
    return base ** exponent;
}

fn main() {
    println(3 ** 40);
    println(dynamic(3, 40));
    println(2 ** (1 + 2));
}
"#;

    for backend in [
        &[][..],
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")][..],
    ] {
        let targets = compile_and_collect_relocation_targets(INTEGER_POWERS, backend);
        let called_pow: Vec<&String> = targets
            .iter()
            .filter(|name| name.starts_with("willow_pow_"))
            .collect();
        assert_eq!(
            called_pow,
            vec!["willow_pow_negative_exponent"],
            "integer `**` must reference only the negative-exponent raiser \
             (backend env {backend:?}); relocations were {targets:?}"
        );
    }

    // The unrolled form alone must not even need the raiser: a non-negative
    // literal exponent is known at compile time, so there is no sign to check.
    let unrolled = compile_and_collect_relocation_targets(
        r#"
fn main() {
    println(3 ** 40);
}
"#,
        &[],
    );
    assert!(
        !unrolled.iter().any(|name| name.starts_with("willow_pow_")),
        "a literal exponent needs no runtime symbol at all, got {unrolled:?}"
    );

    // Positive control: a dynamic f64 power calls the generated local helper,
    // never the old runtime ABI.
    let float_pow = compile_and_collect_relocation_targets(
        r#"
fn main() {
    let base = 2.0;
    let exponent = 8.0;
    println(pow(base, exponent));
}
"#,
        &[],
    );
    assert!(
        float_pow
            .iter()
            .any(|name| name == "willow_internal_pow_f64_v1"),
        "the relocation check must detect the generated pow helper; got {float_pow:?}"
    );
    assert!(
        !float_pow.iter().any(|name| name == "willow_pow_f64"),
        "new objects must not import the retired runtime helper: {float_pow:?}"
    );
}

fn ulp_distance(a: f64, b: f64) -> u64 {
    fn ordered(value: f64) -> u64 {
        let bits = value.to_bits();
        if bits >> 63 == 0 {
            bits | (1 << 63)
        } else {
            !bits
        }
    }
    ordered(a).abs_diff(ordered(b))
}

fn willow_f64_literal(value: f64) -> String {
    // Willow's lexer deliberately has no scientific-notation token yet. A
    // fixed 340-place spelling covers the full binary64 decimal exponent range
    // and trimming keeps ordinary corpus lines readable.
    let mut text = format!("{value:.340}");
    while text.ends_with('0') && text.contains('.') {
        text.pop();
    }
    if text.ends_with('.') {
        text.push('0');
    }
    text
}

/// Stage 8 numerical gate: deterministic ordinary finite values are compared
/// to the platform pow only as a secondary oracle. The checked-in hard cases
/// below cover the cancellation and range-reduction boundaries separately.
#[test]
fn pow_f64_27_deterministic_finite_corpus_is_within_four_ulps() {
    let mut seed = 0x5eed_5eed_d15c_a11eu64;
    let mut pairs = vec![
        (2.0, 0.5),
        (10.0, 1.0 / 3.0),
        (0.5, -3.25),
        (1.000_000_000_000_000_2, 4_503_599_627_370_495.5),
        (0.999_999_999_999_999_9, 4_503_599_627_370_495.5),
        (f64::MIN_POSITIVE, 0.5),
        (f64::from_bits(1), 0.5),
        (f64::MAX, 0.5),
    ];
    for _ in 0..192 {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let fraction = ((seed >> 11) as f64) * (1.0 / ((1u64 << 53) as f64));
        let scale = ((seed >> 58) as i32) - 32;
        let base = (1.0 + fraction) * 2.0f64.powi(scale);
        seed = seed.rotate_left(29) ^ 0x9e37_79b9_7f4a_7c15;
        let exponent = ((seed % 400_001) as f64 - 200_000.0) / 10_000.0;
        let expected = base.powf(exponent);
        if expected.is_normal() {
            pairs.push((base, exponent));
        }
    }

    let mut source = String::from(
        "fn dynamic(base: f64, exponent: f64) -> f64 { return base ** exponent; }\nfn main() {\n",
    );
    for (base, exponent) in &pairs {
        let base = willow_f64_literal(*base);
        let exponent = willow_f64_literal(*exponent);
        source.push_str(&format!("println(dynamic({base}, {exponent}));\n"));
    }
    source.push_str("}\n");
    let (out, ok) = compile_and_run_release(&source);
    assert!(ok, "numerical corpus failed to compile/run: {out}");
    let actual: Vec<f64> = out
        .lines()
        .map(|line| {
            line.parse()
                .unwrap_or_else(|_| panic!("not an f64: {line}"))
        })
        .collect();
    assert_eq!(actual.len(), pairs.len(), "truncated corpus output: {out}");

    let mut maximum = 0;
    let mut errors = Vec::new();
    for ((base, exponent), actual) in pairs.iter().copied().zip(actual) {
        let expected = base.powf(exponent);
        let ulps = ulp_distance(actual, expected);
        maximum = maximum.max(ulps);
        if ulps > 4 {
            errors.push((base, exponent, expected, actual, ulps));
        }
    }
    assert!(
        errors.is_empty(),
        "f64 pow exceeded 4 ULP (max {maximum}, {} failures): {:?}",
        errors.len(),
        &errors[..errors.len().min(12)]
    );
}

#[test]
fn pow_f64_28_ieee_special_case_matrix() {
    let (out, ok) = compile_and_run_release(
        r#"
fn dynamic(x: f64, y: f64) -> f64 { return x ** y; }
fn main() {
    let nan = 0.0 / 0.0;
    let inf = 1.0 / 0.0;
    println(nan ** 0.0);
    println(1.0 ** nan);
    println(dynamic(0.0 - 1.0, inf));
    println(dynamic(0.0 - 1.0, 0.0 - inf));
    println(dynamic(0.0 - 2.0, 0.5) != dynamic(0.0 - 2.0, 0.5));
    println(dynamic(-0.0, 3.0));
    println(dynamic(-0.0, 2.0));
    println(dynamic(-0.0, 0.0 - 3.0));
    println(dynamic(-0.0, 0.5));
    println(dynamic(-0.0, 0.0 - 0.5));
    println(dynamic(0.0 - inf, 3.0));
    println(dynamic(0.0 - inf, 2.5));
    println(dynamic(2.0, inf));
    println(dynamic(0.5, inf));
    println(dynamic(0.0 - 2.0, 9007199254740991.0));
    println(dynamic(0.0 - 2.0, 9007199254740992.0));
}
"#,
    );
    assert!(ok, "special matrix failed: {out}");
    assert_eq!(
        out,
        "1\n1\n1\n1\ntrue\n-0.0\n0.0\n-Infinity\n0.0\nInfinity\n-Infinity\n\
         Infinity\nInfinity\n0.0\n-Infinity\nInfinity\n"
    );
}

#[test]
fn pow_f64_29_integral_literal_dynamic_and_builtin_paths_match() {
    let (out, ok) = compile_and_run_release(
        r#"
fn dynamic(x: f64, y: f64) -> f64 { return x ** y; }
fn main() {
    println(1.0000000000000002 ** 8.0);
    println(dynamic(1.0000000000000002, 8.0));
    println(pow(1.0000000000000002, 8.0));
    println(powf(1.0000000000000002, 8.0));
    let captured: fn(f64, f64) -> f64 = pow;
    println(captured(1.0000000000000002, 8.0));
    println(dynamic(2.0, 0.0 - 3.0));
    println(2.0 ** -3.0);
}
"#,
    );
    assert!(ok, "integral/builtin paths failed: {out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 7, "{out}");
    assert!(lines[..5].iter().all(|line| *line == lines[0]), "{out}");
    assert_eq!(&lines[5..], &["0.125", "0.125"]);
}

/// Primary numerical corpus generated at 260 decimal digits with CPython
/// 3.14.4 / libmpdec 2.5.1. Inputs are exact binary64 values; each expected
/// word is the nearest binary64 conversion of `exp(y * ln(x))` at that
/// precision. The table is checked in so normal test runs need no Python.
#[test]
fn pow_f64_30_versioned_high_precision_hard_case_corpus() {
    const CASES: &[(u64, u64, u64)] = &[
        (0x4000000000000000, 0x3fe0000000000000, 0x3ff6a09e667f3bcd),
        (0x4024000000000000, 0x3fd5555555555555, 0x40013c484138704f),
        (0x3fe0000000000000, 0xc00a000000000000, 0x402306fe0a31b715),
        (0x3ff0000000000001, 0x432fffffffffffff, 0x4005bf0a8b145768),
        (0x3fefffffffffffff, 0x432fffffffffffff, 0x3fe368b2fc6f960a),
        (0x3ff8000000000000, 0x400921fb54442d18, 0x400c986fa8c1d29c),
        (0x3fe8000000000000, 0xc01c800000000000, 0x401f1038a431c015),
        (0x405edd2f1a9fbe77, 0x3fe93f7ced916873, 0x404658398ca9d814),
        (0x16687e92154ef7ac, 0x3fc0000000000000, 0x3abef2d0f5da7dd9),
        (0x6974e718d7d7625a, 0xbfc0000000000000, 0x3abef2d0f5da7dd9),
        (0x3fffffffffffffff, 0x3fe0000000000000, 0x3ff6a09e667f3bcc),
        (0x4000000000000001, 0x3fe0000000000000, 0x3ff6a09e667f3bcd),
        (0x4008000000000000, 0xc031400000000000, 0x3e394550332aacfd),
        (0x401c000000000000, 0x3fe8787878787878, 0x4011b6bac949c39f),
        (0x0178000000000000, 0x3fe8000000000000, 0x1115afbb0fd2812c),
        (0x7e78000000000000, 0x3fd0000000000000, 0x4f91b4f819c2ff81),
        (0x3ff00000000001c2, 0x426d1a94a2000800, 0x3ff1ae6b145bce3f),
        (0x3feffffffffffc7b, 0x426d1a94a2000800, 0x3fecf43298f1cd74),
        (0x3ff2000000000000, 0xc08ffc0000000000, 0x3510eed57f695c37),
        (0x3ffe000000000000, 0x407ff40000000000, 0x5ce911984a0839ae),
        (0x400921fb54442d18, 0x4005bf0a8b145769, 0x4036758b5c381110),
        (0x4005bf0a8b145769, 0x400921fb54442d18, 0x403724046eb09338),
        (0x4045100000000000, 0xc029800000000000, 0x3ba24b3b98d0802f),
        (0x3fa0000000000000, 0x3fc999999999999a, 0x3fe0000000000000),
    ];

    let mut source = String::from(
        "fn dynamic(base: f64, exponent: f64) -> f64 { return base ** exponent; }\nfn main() {\n",
    );
    for &(base, exponent, _) in CASES {
        source.push_str(&format!(
            "println(dynamic({}, {}));\n",
            willow_f64_literal(f64::from_bits(base)),
            willow_f64_literal(f64::from_bits(exponent)),
        ));
    }
    source.push_str("}\n");
    let (out, ok) = compile_and_run_release(&source);
    assert!(ok, "hard-case corpus failed: {out}");
    let values: Vec<f64> = out.lines().map(|line| line.parse().unwrap()).collect();
    assert_eq!(values.len(), CASES.len(), "{out}");
    let mut maximum = 0;
    let mut failures = Vec::new();
    for ((base, exponent, expected), actual) in CASES.iter().copied().zip(values) {
        let expected = f64::from_bits(expected);
        let distance = ulp_distance(actual, expected);
        maximum = maximum.max(distance);
        if distance > 4 {
            failures.push((
                base,
                exponent,
                expected.to_bits(),
                actual.to_bits(),
                distance,
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "high-precision gate exceeded 4 ULP (max {maximum}): {failures:x?}"
    );
}

#[test]
fn pow_f64_31_lir_and_ast_emitters_match() {
    const SOURCE: &str = r#"
fn dynamic(base: f64, exponent: f64) -> f64 {
    return base ** exponent;
}
fn main() {
    println(dynamic(2.0, 0.5));
    println(dynamic(0.0 - 2.0, 7.0));
    println(dynamic(1.0000000000000002, 4503599627370495.5));
    println(dynamic(0.5, 0.0 - 3.25));
}
"#;
    let (ast, ast_ok) = compile_and_run_with_env(SOURCE, &[("WILLOW_LIR_BACKEND", "0")]);
    assert!(ast_ok, "AST: {ast}");
    let (lir, lir_ok) = compile_and_run_with_env(
        SOURCE,
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")],
    );
    assert!(lir_ok, "LIR: {lir}");
    assert_eq!(ast, lir);
}

#[test]
fn pow_f64_32_helpers_are_local_singletons_without_math_imports() {
    const SOURCE: &str = r#"
fn a(x: f64, y: f64) -> f64 { return x ** y; }
fn b(x: f64, y: f64) -> f64 { return pow(x, y) + powf(x, y); }
fn main() {
    println(a(2.0, 0.5));
    println(b(3.0, 0.25));
}
"#;
    let symbols = compile_and_collect_defined_symbols(SOURCE, &[]);
    for helper in [
        "willow_internal_log2_f64_v1",
        "willow_internal_exp2_f64_v1",
        "willow_internal_pow_f64_v1",
    ] {
        assert_eq!(
            symbols
                .iter()
                .filter(|name| name.as_str() == helper)
                .count(),
            1,
            "helper `{helper}` must have exactly one local definition: {symbols:?}"
        );
    }

    let relocations = compile_and_collect_relocation_targets(SOURCE, &[]);
    for forbidden in [
        "willow_pow_f64",
        "pow",
        "powf",
        "log",
        "log2",
        "exp",
        "exp2",
    ] {
        assert!(
            !relocations.iter().any(|name| name == forbidden),
            "new objects must not import `{forbidden}`: {relocations:?}"
        );
    }
}

#[test]
fn pow_f64_33_pow_and_powf_run_on_lir() {
    let (out, ok) = compile_and_run_with_env(
        r#"
fn compatibility(x: f64, y: f64) -> f64 {
    return pow(x, y) + powf(x, y);
}
fn main() { println(compatibility(9.0, 0.5)); }
"#,
        &[("WILLOW_LIR_BACKEND", "1"), ("WILLOW_LIR_REQUIRE", "1")],
    );
    assert!(ok, "LIR compatibility calls failed: {out}");
    assert_eq!(out, "6\n");
}

#[test]
fn pow_f64_34_async_await_operand_and_gc_stress() {
    const SOURCE: &str = r#"
async fn value() -> f64 {
    await sleep(0);
    return 2.0;
}
async fn main() {
    let task = value();
    println(await task ** 0.5);
}
"#;
    let (out, ok) = compile_and_run_with_env(
        SOURCE,
        &[("WILLOW_GC_STRESS", "alloc"), ("WILLOW_TASK_BUDGET", "1")],
    );
    assert!(ok, "async f64 power failed: {out}");
    assert_eq!(out, "1.414213562373095\n");
}
