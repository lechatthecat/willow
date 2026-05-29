use std::ffi::{c_char, CStr};
use std::io::{self, Write};

use crate::math::format_f64_shortest;

pub fn bool_text(value: u8) -> &'static str {
    if value != 0 { "true" } else { "false" }
}

pub fn string_text(value: *const c_char) -> String {
    if value.is_null() {
        "(null)".to_string()
    } else {
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned()
    }
}

fn write_stdout(text: &str) {
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(text.as_bytes())
        .expect("failed to write Willow stdout");
    stdout.flush().expect("failed to flush Willow stdout");
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_print_i64(value: i64) {
    write_stdout(&value.to_string());
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_println_i64(value: i64) {
    write_stdout(&format!("{value}\n"));
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_print_bool(value: u8) {
    write_stdout(bool_text(value));
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_println_bool(value: u8) {
    write_stdout(&format!("{}\n", bool_text(value)));
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_print_f64(value: f64) {
    write_stdout(&format_f64_shortest(value));
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_println_f64(value: f64) {
    write_stdout(&format!("{}\n", format_f64_shortest(value)));
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_print_string(value: *const c_char) {
    write_stdout(&string_text(value));
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_println_string(value: *const c_char) {
    write_stdout(&format!("{}\n", string_text(value)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn print_unit_01_bool_zero_is_false() {
        assert_eq!(bool_text(0), "false");
    }

    #[test]
    fn print_unit_02_bool_nonzero_is_true() {
        assert_eq!(bool_text(2), "true");
    }

    #[test]
    fn print_unit_03_null_string_uses_runtime_null_text() {
        assert_eq!(string_text(std::ptr::null()), "(null)");
    }

    #[test]
    fn print_unit_04_c_string_converts_losslessly() {
        let value = CString::new("hello").unwrap();
        assert_eq!(string_text(value.as_ptr()), "hello");
    }
}
