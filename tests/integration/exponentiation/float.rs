//! Floating-point type rules, numerical corpora, IEEE behavior, and execution.

use crate::support::*;

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

// The dynamic form — a non-literal exponent, so the square-and-multiply /
// helper path rather than any constant folding — printed exactly, so a change
// in the helper's rounding shows up here rather than being absorbed by a
// comparison against another build of the same helper.
#[test]
fn pow_f64_31_the_dynamic_form_prints_its_exact_values() {
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
    let (out, ok) = compile_and_run_with_env(SOURCE, &[]);
    assert!(ok, "{out}");
    assert_eq!(
        out,
        "1.414213562373095\n-128\n2.7182818284590446\n9.513656920021768\n"
    );
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
        &[],
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

#[test]
fn pow_f64_35_unit_bases_dynamic_exponent_matrix() {
    let exponents = [
        "3.0",
        "4.0",
        "-3.0",
        "-4.0",
        "9007199254740991.0",
        "9007199254740992.0",
        "9007199254740994.0",
        "1.0 / 0.0",
        "-1.0 / 0.0",
        "0.0 / 0.0",
        "0.0",
        "-0.0",
        "0.5",
        "-1.25",
    ];
    let mut source =
        String::from("fn dynamic(x: f64, y: f64) -> f64 { return x ** y; } fn main() {\n");
    for base in ["1.0", "-1.0"] {
        for exponent in exponents {
            source.push_str(&format!("println(dynamic({base}, {exponent}));\n"));
        }
    }
    source.push('}');
    let (out, ok) = compile_and_run_release(&source);
    assert!(ok, "{out}");
    let actual: Vec<f64> = out.lines().map(|s| s.parse().unwrap()).collect();
    let negative = [
        -1.0,
        1.0,
        -1.0,
        1.0,
        -1.0,
        1.0,
        1.0,
        1.0,
        1.0,
        f64::NAN,
        1.0,
        1.0,
        f64::NAN,
        f64::NAN,
    ];
    let expected = [1.0f64; 14].into_iter().chain(negative);
    assert_eq!(actual.len(), 28);
    for (actual, expected) in actual.into_iter().zip(expected) {
        if expected.is_nan() {
            assert!(actual.is_nan());
        } else {
            assert_eq!(actual.to_bits(), expected.to_bits());
        }
    }
}
