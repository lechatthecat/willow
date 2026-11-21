use std::io::{self, Write};

use crate::math::format_f64_shortest;
use crate::string::willow_string_as_str;

pub fn bool_text(value: u8) -> &'static str {
    if value != 0 { "true" } else { "false" }
}

pub(crate) fn write_stdout<'a>(text: impl Into<std::borrow::Cow<'a, str>>) {
    // A stalled pipe must not stall collections, and a waiting caller's managed
    // string may move or die, so the wait owns a native copy of the bytes.
    let text = text.into().into_owned();
    if let Some(result) = crate::blocking::run_stdout_call(move || write_stdout_direct(&text)) {
        result.expect("failed to write Willow stdout");
    }
}

fn write_stdout_direct(text: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_print_i64(value: i64) {
    write_stdout(value.to_string());
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_println_i64(value: i64) {
    write_stdout(format!("{value}\n"));
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_print_bool(value: u8) {
    write_stdout(bool_text(value));
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_println_bool(value: u8) {
    write_stdout(format!("{}\n", bool_text(value)));
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_print_f64(value: f64) {
    write_stdout(format_f64_shortest(value));
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_println_f64(value: f64) {
    write_stdout(format!("{}\n", format_f64_shortest(value)));
}

/// Print a WillowString (GC-managed heap object: len at offset 0, bytes at offset 8).
///
/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
///
/// ```compile_fail,E0133
/// willow_runtime::print::willow_print_string(std::ptr::null());
/// ```
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_print_string(value: *const u8) {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let s = unsafe { willow_string_as_str(value) };
    write_stdout(s);
}

/// Print a WillowString followed by a newline.
///
/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_println_string(value: *const u8) {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let s = unsafe { willow_string_as_str(value) };
    write_stdout(format!("{s}\n"));
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
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        let s = unsafe { willow_string_as_str(std::ptr::null()) };
        assert_eq!(s, "");
    }

    #[test]
    fn print_unit_04_willow_string_roundtrip() {
        use crate::gc::{isolated_runtime, willow_gc_init};
        use crate::string::willow_string_from_str;
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };

        let ptr = willow_string_from_str("hello");
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        let s = unsafe { willow_string_as_str(ptr) };
        assert_eq!(s, "hello");
    }
}

/// Unsigned words and Unicode scalars use the same I/O boundary as other scalars.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_print_scalar(value: i64, kind: i64, newline: u8) {
    let mut text = if kind == 0 {
        char::from_u32(value as u32)
            .expect("validated Unicode scalar")
            .to_string()
    } else {
        (value as u64).to_string()
    };
    if newline != 0 {
        text.push('\n');
    }
    write_stdout(text);
}
