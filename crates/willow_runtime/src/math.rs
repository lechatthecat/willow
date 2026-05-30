use std::ffi::{CStr, c_char};

use crate::string::willow_string_from_str;

pub fn format_f64_shortest(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value == f64::INFINITY {
        return "inf".to_string();
    }
    if value == f64::NEG_INFINITY {
        return "-inf".to_string();
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
    use crate::gc::willow_gc_init;
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
        assert_eq!(format_f64_shortest(f64::INFINITY), "inf");
        assert_eq!(format_f64_shortest(f64::NEG_INFINITY), "-inf");
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
        unsafe { willow_gc_init() };
        assert_eq!(ws_text(willow_f64_to_string(3.14)), "3.14");
    }

    #[test]
    fn math_unit_10_format_17g_returns_willow_string() {
        unsafe { willow_gc_init() };
        assert_eq!(ws_text(willow_format_f64_17g(3.14)), "3.1400000000000001");
    }
}
