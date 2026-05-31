use std::io::{self, Write};

use crate::math::format_f64_shortest;
use crate::string::willow_string_as_str;

pub fn bool_text(value: u8) -> &'static str {
    if value != 0 { "true" } else { "false" }
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

/// Print a WillowString (GC-managed heap object: len at offset 0, bytes at offset 8).
#[unsafe(no_mangle)]
pub extern "C" fn willow_print_string(value: *const u8) {
    let s = unsafe { willow_string_as_str(value) };
    write_stdout(s);
}

/// Print a WillowString followed by a newline.
#[unsafe(no_mangle)]
pub extern "C" fn willow_println_string(value: *const u8) {
    let s = unsafe { willow_string_as_str(value) };
    write_stdout(&format!("{s}\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_unit_01_bool_zero_is_false() {
        assert_eq!(bool_text(0), "false");
    }

    #[test]
    fn print_unit_02_bool_nonzero_is_true() {
        assert_eq!(bool_text(2), "true");
    }

    #[test]
    fn print_unit_03_null_string_is_empty() {
        let s = unsafe { willow_string_as_str(std::ptr::null()) };
        assert_eq!(s, "");
    }

    #[test]
    fn print_unit_04_willow_string_roundtrip() {
        use crate::gc::{runtime_test_guard, willow_gc_init};
        use crate::string::willow_string_alloc;
        let _guard = runtime_test_guard();
        willow_gc_init();
        let ptr = willow_string_alloc(b"hello".as_ptr(), 5);
        let s = unsafe { willow_string_as_str(ptr) };
        assert_eq!(s, "hello");
    }
}
