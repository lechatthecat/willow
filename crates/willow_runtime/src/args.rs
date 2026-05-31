use std::ffi::c_char;
use std::sync::Mutex;

use crate::array::{willow_array_new, willow_array_set};
use crate::gc::{willow_pop_roots, willow_push_root};
use crate::string::willow_string_from_str;

#[derive(Debug, Default, Clone, Copy)]
struct RuntimeArgs {
    argc: i32,
    argv: usize,
    user_argc: i32,
    user_argv: usize,
}

static ARGS: Mutex<RuntimeArgs> = Mutex::new(RuntimeArgs {
    argc: 0,
    argv: 0,
    user_argc: 0,
    user_argv: 0,
});

#[unsafe(no_mangle)]
pub extern "C" fn willow_runtime_store_args(argc: i32, argv: *mut *mut c_char) {
    let mut args = ARGS.lock().expect("runtime args mutex poisoned");
    args.argc = argc;
    args.argv = argv as usize;
    if argc > 1 && !argv.is_null() {
        args.user_argc = argc - 1;
        args.user_argv = unsafe { argv.add(1) } as usize;
    } else {
        args.user_argc = 0;
        args.user_argv = 0;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_runtime_args_len() -> i64 {
    ARGS.lock().expect("runtime args mutex poisoned").user_argc as i64
}

/// Returns a GC-managed WillowString for the user argument at `index`,
/// or a null pointer if the index is out of range.
#[unsafe(no_mangle)]
pub extern "C" fn willow_runtime_arg(index: i64) -> *mut u8 {
    let args = ARGS.lock().expect("runtime args mutex poisoned");
    if index < 0 || index >= args.user_argc as i64 || args.user_argv == 0 {
        return std::ptr::null_mut();
    }
    let user_argv = args.user_argv as *mut *mut c_char;
    let cptr = unsafe { *user_argv.add(index as usize) };
    if cptr.is_null() {
        return std::ptr::null_mut();
    }
    let s = unsafe { std::ffi::CStr::from_ptr(cptr) }.to_string_lossy();
    willow_string_from_str(&s)
}

/// Build a GC-managed `Array<String>` of the user arguments (excluding the
/// program name). Used for `env::args()` and to bind a `fn main(args:
/// Array<String>)` parameter.
#[unsafe(no_mangle)]
pub extern "C" fn willow_runtime_args_array() -> *mut u8 {
    let (user_argc, user_argv) = {
        let args = ARGS.lock().expect("runtime args mutex poisoned");
        (args.user_argc, args.user_argv)
    };
    let len = user_argc.max(0) as i64;
    // Reference-element array (each slot holds a WillowString pointer).
    let mut arr = willow_array_new(len, 1);
    if arr.is_null() {
        return std::ptr::null_mut();
    }
    // Root the array while building element strings: each `willow_string_from_str`
    // may trigger a collection, and the partially-filled array (plus the strings
    // already stored) must stay reachable.
    willow_push_root(&mut arr as *mut *mut u8);
    if user_argv != 0 {
        let argv = user_argv as *mut *mut c_char;
        for i in 0..len {
            let cptr = unsafe { *argv.add(i as usize) };
            let s = if cptr.is_null() {
                willow_string_from_str("")
            } else {
                let text = unsafe { std::ffi::CStr::from_ptr(cptr) }.to_string_lossy();
                willow_string_from_str(&text)
            };
            willow_array_set(arr, i, s as i64);
        }
    }
    willow_pop_roots(1);
    arr
}

/// Returns a GC-managed WillowString for the program name (argv[0]).
#[unsafe(no_mangle)]
pub extern "C" fn willow_runtime_program_name() -> *mut u8 {
    let args = ARGS.lock().expect("runtime args mutex poisoned");
    if args.argc <= 0 || args.argv == 0 {
        return willow_string_from_str("");
    }
    let argv = args.argv as *mut *mut c_char;
    let program = unsafe { *argv };
    if program.is_null() {
        return willow_string_from_str("");
    }
    let s = unsafe { std::ffi::CStr::from_ptr(program) }.to_string_lossy();
    willow_string_from_str(&s)
}

#[cfg(test)]
pub fn reset_for_tests() {
    *ARGS.lock().expect("runtime args mutex poisoned") = RuntimeArgs::default();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::willow_gc_init;
    use crate::string::willow_string_as_str;
    use std::ffi::CString;

    fn ws_text(ptr: *mut u8) -> String {
        if ptr.is_null() {
            return "(null)".to_string();
        }
        unsafe { willow_string_as_str(ptr) }.to_string()
    }

    #[test]
    fn args_unit_01_empty_args_len_is_zero() {
        reset_for_tests();
        willow_runtime_store_args(0, std::ptr::null_mut());
        assert_eq!(willow_runtime_args_len(), 0);
    }

    #[test]
    fn args_unit_02_program_name_defaults_to_empty_string() {
        unsafe { willow_gc_init() };
        reset_for_tests();
        assert_eq!(ws_text(willow_runtime_program_name()), "");
    }

    #[test]
    fn args_unit_03_user_args_exclude_program_name() {
        reset_for_tests();
        let program = CString::new("prog").unwrap();
        let arg = CString::new("one").unwrap();
        let mut argv = vec![program.as_ptr() as *mut c_char, arg.as_ptr() as *mut c_char];
        willow_runtime_store_args(2, argv.as_mut_ptr());
        assert_eq!(willow_runtime_args_len(), 1);
    }

    #[test]
    fn args_unit_04_arg_returns_requested_user_arg() {
        unsafe { willow_gc_init() };
        reset_for_tests();
        let program = CString::new("prog").unwrap();
        let arg = CString::new("one").unwrap();
        let mut argv = vec![program.as_ptr() as *mut c_char, arg.as_ptr() as *mut c_char];
        willow_runtime_store_args(2, argv.as_mut_ptr());
        assert_eq!(ws_text(willow_runtime_arg(0)), "one");
    }

    #[test]
    fn args_unit_05_negative_arg_index_returns_null() {
        reset_for_tests();
        assert!(willow_runtime_arg(-1).is_null());
    }

    #[test]
    fn args_unit_06_out_of_range_arg_index_returns_null() {
        reset_for_tests();
        let program = CString::new("prog").unwrap();
        let mut argv = vec![program.as_ptr() as *mut c_char];
        willow_runtime_store_args(1, argv.as_mut_ptr());
        assert!(willow_runtime_arg(0).is_null());
    }

    #[test]
    fn args_unit_07_program_name_reads_argv_zero() {
        unsafe { willow_gc_init() };
        reset_for_tests();
        let program = CString::new("prog").unwrap();
        let mut argv = vec![program.as_ptr() as *mut c_char];
        willow_runtime_store_args(1, argv.as_mut_ptr());
        assert_eq!(ws_text(willow_runtime_program_name()), "prog");
    }
}
