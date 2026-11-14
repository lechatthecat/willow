use std::ffi::{CStr, c_char};

use crate::gc::alloc_enum_variant_from_generated;
use crate::string::willow_string_as_str;

const RESULT_OK_TAG: i64 = 0;
const RESULT_ERR_TAG: i64 = 1;
const PARSE_INVALID_TAG: i64 = 0;

/// Default f64 display omits `.0` for every integral value, including signed
/// zeros (`0` and `-0`). Parsing preserves the sign bit. Explicit precision
/// formatting uses its own rules.
pub fn format_f64_shortest(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value == f64::INFINITY {
        return "Infinity".to_string();
    }
    if value == f64::NEG_INFINITY {
        return "-Infinity".to_string();
    }
    let mut buffer = ryu::Buffer::new();
    let text = buffer.format_finite(value).to_string();

    // ryu uses scientific notation for some values (e.g. 1e-3 for 0.001).
    // Prefer fixed notation when it also round-trips and is not longer.
    let mut text = if text.contains('e') || text.contains('E') {
        let fixed = format!("{value}");
        if fixed.parse::<f64>().ok() == Some(value) && !fixed.contains('e') && !fixed.contains('E')
        {
            fixed
        } else {
            text
        }
    } else {
        text
    };

    // Strip trailing ".0" (e.g. "12.0" → "12") for integer-valued floats.
    if text.ends_with(".0") {
        text.truncate(text.len() - 2);
    }
    text
}

pub fn format_f64_17g(value: f64) -> String {
    c_double_format(b"%.17g\0", value, 64)
}

pub fn format_f64_16f(value: f64) -> String {
    format!("{value:.16}")
}

pub fn format_f64_6f(value: f64) -> String {
    format!("{value:.6}")
}

fn c_double_format(format: &[u8], value: f64, capacity: usize) -> String {
    let mut buffer = vec![0 as c_char; capacity];
    unsafe {
        libc::snprintf(
            buffer.as_mut_ptr(),
            buffer.len(),
            format.as_ptr() as *const c_char,
            value,
        );
        CStr::from_ptr(buffer.as_ptr())
            .to_string_lossy()
            .into_owned()
    }
}

/// `i64 ** i64` with a negative exponent. Integer
/// exponentiation has no fractional result, so this is a language fault rather
/// than a value: generated code checks the exponent's sign and calls this
/// instead of running the multiplication loop. Raising (rather than aborting)
/// keeps it recoverable like any other Willow panic. `file` is an optional
/// WillowString pointer; a negative *literal* exponent is rejected at compile
/// time (E0204), so only a negative value computed at runtime reaches here.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_pow_negative_exponent(
    exponent: i64,
    file: *const u8,
    line: i32,
    col: i32,
) {
    let message = format!("negative exponent in integer `**`: {exponent}");
    crate::panic_context::raise_language_message_at(&message, file, line.into(), col.into());
}

/// Convert with IEEE round-to-nearest, ties-to-even; large integers may lose precision.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_f64_from_i64(value: i64) -> f64 {
    value as f64
}

/// Truncate toward zero, saturate overflow/infinities, and map NaN to zero.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_f64_trunc_to_i64(value: f64) -> i64 {
    value as i64
}

/// Returns a GC-managed WillowString representation of `value`.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_f64_to_string(value: f64) -> *mut u8 {
    crate::string::alloc_native_string(&format_f64_shortest(value))
}

/// `i64.toString()` — GC-managed WillowString of the decimal representation.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_i64_to_string(value: i64) -> *mut u8 {
    crate::string::i64_to_string(value)
}

/// `bool.toString()` — GC-managed WillowString `"true"` or `"false"`. The value
/// is the usual nonzero-is-true encoding.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_bool_to_string(value: u8) -> *mut u8 {
    crate::string::alloc_native_string(if value != 0 { "true" } else { "false" })
}

/// Returns `Result<f64, ParseFloatError>`.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_f64_parse(text: *const u8) -> *mut u8 {
    let text = unsafe { willow_string_as_str(text) };
    alloc_parse_result(
        text.parse::<f64>()
            .map(|value| value.to_bits() as i64)
            .map_err(|err| format!("invalid float: {err}")),
    )
}

/// Parses signed decimal ASCII digits, with an optional leading `+` or `-`.
/// Whitespace, separators, radix prefixes and values outside i64 are rejected.
/// Returns `Result<i64, ParseIntError>`; no input-sized copy is allocated.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_i64_parse(text: *const u8) -> *mut u8 {
    let text = unsafe { willow_string_as_str(text) };
    alloc_parse_result(
        text.parse::<i64>()
            .map_err(|err| format!("invalid integer: {err}")),
    )
}

// Both parse errors have one Invalid(String) variant, and both Ok payloads
// occupy a non-reference word. Keep their allocation and rooting identical.
// Called directly by generated code: every object is a short-lived young TLAB
// allocation when the runtime TLAB hits, and each miss roots the previously
// built object in a shadow-stack slot.
fn alloc_parse_result(parsed: Result<i64, String>) -> *mut u8 {
    match parsed {
        Ok(value) => alloc_enum_variant_from_generated(
            0,
            willow_abi::EnumVariantLayout::new(RESULT_OK_TAG as u32, &[willow_abi::SlotKind::Word]),
            &[value],
        ),
        Err(err) => {
            let message = crate::string::alloc_native_string(&err);
            let parse_error = alloc_enum_variant_from_generated(
                0,
                willow_abi::EnumVariantLayout::new(
                    PARSE_INVALID_TAG as u32,
                    &[willow_abi::SlotKind::GcRef],
                ),
                &[message as i64],
            );
            alloc_enum_variant_from_generated(
                0,
                willow_abi::EnumVariantLayout::new(
                    RESULT_ERR_TAG as u32,
                    &[willow_abi::SlotKind::GcRef],
                ),
                &[parse_error as i64],
            )
        }
    }
}

/// Returns a GC-managed WillowString formatted with %.17g precision.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_format_f64_17g(value: f64) -> *mut u8 {
    crate::string::alloc_native_string(&format_f64_17g(value))
}

/// Returns a GC-managed WillowString formatted with 16 decimal places.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_format_f64_16f(value: f64) -> *mut u8 {
    crate::string::alloc_native_string(&format_f64_16f(value))
}

/// Returns a GC-managed WillowString formatted with 6 decimal places.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_format_f64_6f(value: f64) -> *mut u8 {
    crate::string::alloc_native_string(&format_f64_6f(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{isolated_runtime, willow_gc_init};
    use crate::string::{willow_string_as_str, willow_string_from_str};

    fn ws_text(ptr: *mut u8) -> String {
        unsafe { willow_string_as_str(ptr) }.to_string()
    }

    #[test]
    fn integral_display_roundtrips_including_signed_zero() {
        // 22 boundary perspectives, including adjacent representable values.
        let values = [
            0.0,
            -0.0,
            1.0,
            -1.0,
            2.0,
            -2.0,
            10.0,
            -10.0,
            0.5 + 0.5,
            -2.0 * 0.0,
            0.5,
            -0.5,
            0.1 + 0.2,
            f64::from_bits(1),
            -f64::from_bits(1),
            f64::MIN_POSITIVE,
            f64::MAX,
            f64::MIN,
            9007199254740991.0,
            9007199254740992.0,
            f64::from_bits(1.0f64.to_bits() - 1),
            f64::from_bits(1.0f64.to_bits() + 1),
        ];
        for value in values {
            let text = format_f64_shortest(value);
            assert_eq!(
                text.parse::<f64>().unwrap().to_bits(),
                value.to_bits(),
                "{text}"
            );
            if value.fract() == 0.0 {
                assert!(!text.contains('.'), "integral value {value:?}: {text}");
            }
        }
        assert_eq!(format_f64_shortest(0.0), "0");
        assert_eq!(format_f64_shortest(-0.0), "-0");
    }

    #[test]
    fn math_unit_01_shortest_prints_fraction_without_padding() {
        assert_eq!(format_f64_shortest(2.5), "2.5");
    }

    #[test]
    fn math_unit_02_shortest_trims_trailing_dot_zero() {
        assert_eq!(format_f64_shortest(10.0), "10");
    }

    #[test]
    fn math_unit_03_shortest_preserves_negative_fraction() {
        assert_eq!(format_f64_shortest(-0.5), "-0.5");
    }

    #[test]
    fn math_unit_04_shortest_handles_nan() {
        assert_eq!(format_f64_shortest(f64::NAN), "NaN");
    }

    #[test]
    fn math_unit_05_shortest_handles_infinity() {
        assert_eq!(format_f64_shortest(f64::INFINITY), "Infinity");
        assert_eq!(format_f64_shortest(f64::NEG_INFINITY), "-Infinity");
    }

    #[test]
    fn math_unit_06_17g_matches_existing_runtime_rounding() {
        assert_eq!(format_f64_17g(3.14), "3.1400000000000001");
    }

    #[test]
    fn math_unit_07_fixed_formats_match_required_precisions() {
        assert_eq!(format_f64_6f(3.14), "3.140000");
        assert_eq!(format_f64_16f(1.5), "1.5000000000000000");
    }

    #[test]
    fn math_unit_09_exported_to_string_returns_willow_string() {
        let _guard = isolated_runtime();
        willow_gc_init();
        assert_eq!(ws_text(willow_f64_to_string(3.14)), "3.14");
    }

    #[test]
    fn math_unit_10_format_17g_returns_willow_string() {
        let _guard = isolated_runtime();
        willow_gc_init();
        assert_eq!(ws_text(willow_format_f64_17g(3.14)), "3.1400000000000001");
    }

    #[test]
    fn math_unit_11_parse_ok_returns_result_ok_f64_bits() {
        let _guard = isolated_runtime();
        willow_gc_init();
        let input = willow_string_from_str("3.5");
        let result = willow_f64_parse(input);
        unsafe {
            assert_eq!(*(result as *const i64), RESULT_OK_TAG);
            let bits = *((result as *const i64).add(1)) as u64;
            assert_eq!(f64::from_bits(bits), 3.5);
        }
    }

    #[test]
    fn math_unit_12_parse_err_returns_parse_float_error() {
        let _guard = isolated_runtime();
        willow_gc_init();
        let input = willow_string_from_str("not-a-number");
        let result = willow_f64_parse(input);
        unsafe {
            assert_eq!(*(result as *const i64), RESULT_ERR_TAG);
            let parse_error = *((result as *const i64).add(1)) as *mut u8;
            assert_eq!(*(parse_error as *const i64), PARSE_INVALID_TAG);
            let message = *((parse_error as *const i64).add(1)) as *mut u8;
            assert_eq!(ws_text(message), "invalid float: invalid float literal");
        }
    }
    #[test]
    fn i64_parse_runtime_whitespace_and_payloads() {
        let _guard = isolated_runtime();
        willow_gc_init();
        for input in ["\t1", "1\n", "\r1", "1\0", "1\t2", "\u{a0}1"] {
            let result = willow_i64_parse(willow_string_from_str(input));
            unsafe {
                assert_eq!(*(result as *const i64), RESULT_ERR_TAG, "{input:?}");
                let error = *((result as *const i64).add(1)) as *mut u8;
                assert_eq!(*(error as *const i64), PARSE_INVALID_TAG);
                let message = *((error as *const i64).add(1)) as *mut u8;
                assert!(ws_text(message).starts_with("invalid integer:"));
            }
        }
        for (input, expected) in [
            ("-9223372036854775808", i64::MIN),
            ("9223372036854775807", i64::MAX),
        ] {
            let result = willow_i64_parse(willow_string_from_str(input));
            unsafe {
                assert_eq!(*(result as *const i64), RESULT_OK_TAG);
                assert_eq!(*((result as *const i64).add(1)), expected);
            }
        }
    }

    #[test]
    fn i64_parse_long_leading_zeros() {
        let _guard = isolated_runtime();
        willow_gc_init();
        // Increasing lengths exercise full scans without overflowing early.
        for n in [32, 256, 2048, 16384] {
            let input = format!("{}42", "0".repeat(n));
            let result = willow_i64_parse(willow_string_from_str(&input));
            unsafe {
                assert_eq!(*(result as *const i64), RESULT_OK_TAG);
                assert_eq!(*((result as *const i64).add(1)), 42);
            }
        }
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_i64_abs(value: i64) -> i64 {
    match value.checked_abs() {
        Some(result) => result,
        None => {
            crate::panic_context::raise_language_message("integer overflow: i64::abs");
            0
        }
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_i64_min(value: i64, other: i64) -> i64 {
    value.min(other)
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_i64_max(value: i64, other: i64) -> i64 {
    value.max(other)
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_i64_clamp(value: i64, lower: i64, upper: i64) -> i64 {
    if lower > upper {
        crate::panic_context::raise_language_message("invalid bounds: i64::clamp");
        return 0;
    }
    value.clamp(lower, upper)
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_f64_abs(value: f64) -> f64 {
    value.abs()
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_f64_min(value: f64, other: f64) -> f64 {
    value.min(other)
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_f64_max(value: f64, other: f64) -> f64 {
    value.max(other)
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_f64_clamp(value: f64, lower: f64, upper: f64) -> f64 {
    if lower.is_nan() || upper.is_nan() || lower > upper {
        crate::panic_context::raise_language_message("invalid bounds: f64::clamp");
        return 0.0;
    }
    value.clamp(lower, upper)
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_f64_sqrt(value: f64) -> f64 {
    value.sqrt()
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_f64_floor(value: f64) -> f64 {
    value.floor()
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_f64_ceil(value: f64) -> f64 {
    value.ceil()
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_f64_round(value: f64) -> f64 {
    value.round()
}

#[cfg(test)]
mod math_helpers_tests {
    use super::*;

    #[test]
    fn math_helpers_ieee_boundaries() {
        for op in [
            willow_f64_abs,
            willow_f64_sqrt,
            willow_f64_floor,
            willow_f64_ceil,
            willow_f64_round,
        ] {
            assert!(op(f64::NAN).is_nan());
            assert_eq!(op(f64::INFINITY), f64::INFINITY);
        }
        for op in [
            willow_f64_floor,
            willow_f64_ceil,
            willow_f64_round,
            willow_f64_sqrt,
        ] {
            assert_eq!(op(-0.0).to_bits(), (-0.0f64).to_bits());
        }
        for op in [willow_f64_floor, willow_f64_ceil, willow_f64_round] {
            assert_eq!(op(f64::NEG_INFINITY), f64::NEG_INFINITY);
        }
        assert_eq!(willow_f64_abs(f64::NEG_INFINITY), f64::INFINITY);
        assert!(willow_f64_sqrt(f64::NEG_INFINITY).is_nan());
        assert_eq!(
            willow_f64_clamp(-0.0, -1.0, 1.0).to_bits(),
            (-0.0f64).to_bits()
        );
        assert_eq!(willow_f64_clamp(3.0, 2.0, 2.0), 2.0);
        assert_eq!(willow_f64_clamp(f64::INFINITY, -1.0, 1.0), 1.0);
        assert_eq!(willow_i64_min(i64::MIN, i64::MAX), i64::MIN);
        assert_eq!(willow_i64_max(i64::MIN, i64::MAX), i64::MAX);
        assert_eq!(willow_i64_clamp(i64::MIN, i64::MIN, i64::MAX), i64::MIN);
    }
}
