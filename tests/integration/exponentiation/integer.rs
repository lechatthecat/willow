//! Integer values, precedence, wrapping, and literal/dynamic agreement.

use crate::support::*;

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
