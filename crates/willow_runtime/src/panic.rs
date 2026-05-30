use crate::string::willow_string_as_str;

pub fn nil_deref_message(
    file: *const u8,
    line: i32,
    col: i32,
    context: *const u8,
) -> String {
    let file = unsafe { willow_string_as_str(file) };
    let ctx = unsafe { willow_string_as_str(context) };
    if ctx.is_empty() {
        format!("runtime panic: nil dereference at {file}:{line}:{col}")
    } else {
        format!("runtime panic: nil dereference at {file}:{line}:{col} -- `{ctx}`")
    }
}

pub fn abort_message(file: *const u8, line: i32) -> String {
    let file = unsafe { willow_string_as_str(file) };
    format!("panic at {file}:{line}")
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_nil_deref(
    file: *const u8,
    line: i32,
    col: i32,
    context: *const u8,
) {
    eprintln!("{}", nil_deref_message(file, line, col, context));
    crate::task::print_current_task_context();
    std::process::abort();
}

/// Called by the Willow `panic(message)` builtin.  `message` is a WillowString pointer.
#[unsafe(no_mangle)]
pub extern "C" fn willow_panic(message: *const u8) {
    let msg = unsafe { willow_string_as_str(message) };
    eprintln!("runtime panic: {msg}");
    crate::task::print_current_task_context();
    std::process::abort();
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_abort(file: *const u8, line: i32) {
    eprintln!("{}", abort_message(file, line));
    crate::task::print_current_task_context();
    std::process::abort();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::willow_gc_init;
    use crate::string::willow_string_alloc;

    fn ws(s: &str) -> *mut u8 {
        willow_string_alloc(s.as_bytes().as_ptr(), s.len() as i64)
    }

    #[test]
    fn panic_unit_01_nil_deref_message_without_context() {
        unsafe { willow_gc_init() };
        let msg = nil_deref_message(ws("main.wi"), 3, 4, std::ptr::null());
        assert_eq!(msg, "runtime panic: nil dereference at main.wi:3:4");
    }

    #[test]
    fn panic_unit_02_nil_deref_message_with_context() {
        unsafe { willow_gc_init() };
        let msg = nil_deref_message(ws("main.wi"), 3, 4, ws("box.value"));
        assert_eq!(
            msg,
            "runtime panic: nil dereference at main.wi:3:4 -- `box.value`"
        );
    }

    #[test]
    fn panic_unit_03_abort_message_includes_source_line() {
        unsafe { willow_gc_init() };
        assert_eq!(abort_message(ws("panic.wi"), 9), "panic at panic.wi:9");
    }
}
