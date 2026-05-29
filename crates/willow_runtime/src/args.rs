use std::ffi::c_char;
use std::sync::Mutex;

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

static EMPTY: &[u8] = b"\0";

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

#[unsafe(no_mangle)]
pub extern "C" fn willow_runtime_arg(index: i64) -> *const c_char {
    let args = ARGS.lock().expect("runtime args mutex poisoned");
    if index < 0 || index >= args.user_argc as i64 || args.user_argv == 0 {
        return std::ptr::null();
    }
    let user_argv = args.user_argv as *mut *mut c_char;
    unsafe { *user_argv.add(index as usize) as *const c_char }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_runtime_program_name() -> *const c_char {
    let args = ARGS.lock().expect("runtime args mutex poisoned");
    if args.argc <= 0 || args.argv == 0 {
        return EMPTY.as_ptr() as *const c_char;
    }
    let argv = args.argv as *mut *mut c_char;
    let program = unsafe { *argv };
    if program.is_null() {
        EMPTY.as_ptr() as *const c_char
    } else {
        program as *const c_char
    }
}

#[cfg(test)]
pub fn reset_for_tests() {
    *ARGS.lock().expect("runtime args mutex poisoned") = RuntimeArgs::default();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn args_unit_01_empty_args_len_is_zero() {
        reset_for_tests();
        willow_runtime_store_args(0, std::ptr::null_mut());
        assert_eq!(willow_runtime_args_len(), 0);
    }

    #[test]
    fn args_unit_02_program_name_defaults_to_empty_string() {
        reset_for_tests();
        let value = unsafe { std::ffi::CStr::from_ptr(willow_runtime_program_name()) };
        assert_eq!(value.to_str().unwrap(), "");
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
        reset_for_tests();
        let program = CString::new("prog").unwrap();
        let arg = CString::new("one").unwrap();
        let mut argv = vec![program.as_ptr() as *mut c_char, arg.as_ptr() as *mut c_char];
        willow_runtime_store_args(2, argv.as_mut_ptr());
        let value = unsafe { std::ffi::CStr::from_ptr(willow_runtime_arg(0)) };
        assert_eq!(value.to_str().unwrap(), "one");
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
        reset_for_tests();
        let program = CString::new("prog").unwrap();
        let mut argv = vec![program.as_ptr() as *mut c_char];
        willow_runtime_store_args(1, argv.as_mut_ptr());
        let value = unsafe { std::ffi::CStr::from_ptr(willow_runtime_program_name()) };
        assert_eq!(value.to_str().unwrap(), "prog");
    }
}
