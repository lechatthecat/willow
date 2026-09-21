//! Runtime ABI effects and emitted exponentiation symbols and relocations.

use crate::support::*;

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
/// compute a power.
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

    let targets = compile_and_collect_relocation_targets(INTEGER_POWERS, &[]);
    let called_pow: Vec<&String> = targets
        .iter()
        .filter(|name| name.starts_with("willow_pow_"))
        .collect();
    assert_eq!(
        called_pow,
        vec!["willow_pow_negative_exponent"],
        "integer `**` must reference only the negative-exponent raiser; \
         relocations were {targets:?}"
    );

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
