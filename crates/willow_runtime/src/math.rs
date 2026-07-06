use std::ffi::{CStr, c_char};

use crate::gc::{willow_alloc_typed, willow_pop_roots, willow_push_root};
use crate::string::{willow_string_as_str, willow_string_from_str};

const RESULT_OK_TAG: i64 = 0;
const RESULT_ERR_TAG: i64 = 1;
const PARSE_FLOAT_INVALID_TAG: i64 = 0;

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
    // Signed zeros keep an explicit fractional part so `0.0` and `-0.0` are
    // visually floats and distinguishable (requirements_float_printing.md).
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0".to_string()
        } else {
            "0.0".to_string()
        };
    }

    let mut buffer = ryu::Buffer::new();
    let text = buffer.format_finite(value).to_string();

    // ryu uses scientific notation for some values (e.g. 1e-3 for 0.001).
    // Prefer fixed notation when it also round-trips and is not longer.
    let text = if text.contains('e') || text.contains('E') {
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
        text[..text.len() - 2].to_string()
    } else {
        text
    }
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

#[unsafe(no_mangle)]
pub extern "C" fn willow_pow_f64(base: f64, exp: f64) -> f64 {
    base.powf(exp)
}

/// Returns a GC-managed WillowString representation of `value`.
#[unsafe(no_mangle)]
pub extern "C" fn willow_f64_to_string(value: f64) -> *mut u8 {
    willow_string_from_str(&format_f64_shortest(value))
}

/// `i64.toString()` — GC-managed WillowString of the decimal representation.
#[unsafe(no_mangle)]
pub extern "C" fn willow_i64_to_string(value: i64) -> *mut u8 {
    willow_string_from_str(&value.to_string())
}

/// `bool.toString()` — GC-managed WillowString `"true"` or `"false"`. The value
/// is the usual nonzero-is-true encoding.
#[unsafe(no_mangle)]
pub extern "C" fn willow_bool_to_string(value: u8) -> *mut u8 {
    willow_string_from_str(if value != 0 { "true" } else { "false" })
}

/// Returns `Result<f64, ParseFloatError>`.
#[unsafe(no_mangle)]
pub extern "C" fn willow_f64_parse(text: *const u8) -> *mut u8 {
    let text = unsafe { willow_string_as_str(text) };
    match text.parse::<f64>() {
        Ok(value) => {
            let result = willow_alloc_typed(16, 0);
            if result.is_null() {
                return result;
            }
            unsafe {
                *(result as *mut i64) = RESULT_OK_TAG;
                *((result as *mut i64).add(1)) = value.to_bits() as i64;
            }
            result
        }
        Err(err) => {
            let mut message = willow_string_from_str(&format!("invalid float: {err}"));
            willow_push_root(&mut message as *mut *mut u8);

            let mut parse_error = willow_alloc_typed(16, 0b10);
            if !parse_error.is_null() {
                unsafe {
                    *(parse_error as *mut i64) = PARSE_FLOAT_INVALID_TAG;
                    *((parse_error as *mut i64).add(1)) = message as i64;
                }
            }
            willow_pop_roots(1);

            willow_push_root(&mut parse_error as *mut *mut u8);
            let result = willow_alloc_typed(16, 0b10);
            if !result.is_null() {
                unsafe {
                    *(result as *mut i64) = RESULT_ERR_TAG;
                    *((result as *mut i64).add(1)) = parse_error as i64;
                }
            }
            willow_pop_roots(1);
            result
        }
    }
}

/// Returns a GC-managed WillowString formatted with %.17g precision.
#[unsafe(no_mangle)]
pub extern "C" fn willow_format_f64_17g(value: f64) -> *mut u8 {
    willow_string_from_str(&format_f64_17g(value))
}

/// Returns a GC-managed WillowString formatted with 16 decimal places.
#[unsafe(no_mangle)]
pub extern "C" fn willow_format_f64_16f(value: f64) -> *mut u8 {
    willow_string_from_str(&format_f64_16f(value))
}

/// Returns a GC-managed WillowString formatted with 6 decimal places.
#[unsafe(no_mangle)]
pub extern "C" fn willow_format_f64_6f(value: f64) -> *mut u8 {
    willow_string_from_str(&format_f64_6f(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{runtime_test_guard, willow_gc_init};
    use crate::string::willow_string_as_str;

    fn ws_text(ptr: *mut u8) -> String {
        unsafe { willow_string_as_str(ptr) }.to_string()
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
    fn math_unit_08_pow_uses_f64_exponentiation() {
        assert_eq!(willow_pow_f64(2.0, 8.0), 256.0);
    }

    #[test]
    fn math_unit_09_exported_to_string_returns_willow_string() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        assert_eq!(ws_text(willow_f64_to_string(3.14)), "3.14");
    }

    #[test]
    fn math_unit_10_format_17g_returns_willow_string() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        assert_eq!(ws_text(willow_format_f64_17g(3.14)), "3.1400000000000001");
    }

    #[test]
    fn math_unit_11_parse_ok_returns_result_ok_f64_bits() {
        let _guard = runtime_test_guard();
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
        let _guard = runtime_test_guard();
        willow_gc_init();
        let input = willow_string_from_str("not-a-number");
        let result = willow_f64_parse(input);
        unsafe {
            assert_eq!(*(result as *const i64), RESULT_ERR_TAG);
            let parse_error = *((result as *const i64).add(1)) as *mut u8;
            assert_eq!(*(parse_error as *const i64), PARSE_FLOAT_INVALID_TAG);
            let message = *((parse_error as *const i64).add(1)) as *mut u8;
            assert_eq!(ws_text(message), "invalid float: invalid float literal");
        }
    }
}
