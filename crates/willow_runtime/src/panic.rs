use std::ffi::{c_char, CStr};

fn c_string(value: *const c_char) -> String {
    if value.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned()
    }
}

pub fn nil_deref_message(
    file: *const c_char,
    line: i32,
    col: i32,
    context: *const c_char,
) -> String {
    let file = c_string(file);
    let context = c_string(context);
    if context.is_empty() {
        format!("runtime panic: nil dereference at {file}:{line}:{col}")
    } else {
        format!("runtime panic: nil dereference at {file}:{line}:{col} -- `{context}`")
    }
}

pub fn abort_message(file: *const c_char, line: i32) -> String {
    format!("panic at {}:{line}", c_string(file))
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_nil_deref(
    file: *const c_char,
    line: i32,
    col: i32,
    context: *const c_char,
) {
    eprintln!("{}", nil_deref_message(file, line, col, context));
    crate::task::print_current_task_context();
    std::process::abort();
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_abort(file: *const c_char, line: i32) {
    eprintln!("{}", abort_message(file, line));
    crate::task::print_current_task_context();
    std::process::abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn panic_unit_01_nil_deref_message_without_context() {
        let file = CString::new("main.wi").unwrap();
        let msg = nil_deref_message(file.as_ptr(), 3, 4, std::ptr::null());
        assert_eq!(msg, "runtime panic: nil dereference at main.wi:3:4");
    }

    #[test]
    fn panic_unit_02_nil_deref_message_with_context() {
        let file = CString::new("main.wi").unwrap();
        let context = CString::new("box.value").unwrap();
        let msg = nil_deref_message(file.as_ptr(), 3, 4, context.as_ptr());
        assert_eq!(
            msg,
            "runtime panic: nil dereference at main.wi:3:4 -- `box.value`"
        );
    }

    #[test]
    fn panic_unit_03_abort_message_includes_source_line() {
        let file = CString::new("panic.wi").unwrap();
        assert_eq!(abort_message(file.as_ptr(), 9), "panic at panic.wi:9");
    }
}
