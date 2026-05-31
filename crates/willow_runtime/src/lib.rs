// Runtime functions linked with Cranelift-generated object files.
#![allow(dead_code)]

pub mod args;
pub mod array;
pub mod async_frame;
pub mod channel;
pub mod executor;
pub mod future;
pub mod gc;
pub mod map;
pub mod math;
pub mod netpoll;
pub mod object;
pub mod panic;
pub mod print;
pub mod reference_debug;
pub mod scheduler;
pub mod stack_trace;
pub mod string;
pub mod sync;
pub mod task;
pub mod timer;
pub mod trace;

use std::ffi::c_char;

#[cfg(not(test))]
unsafe extern "C" {
    fn willow_user_main();
}

#[cfg(test)]
#[unsafe(no_mangle)]
unsafe extern "C" fn willow_user_main() {}

#[unsafe(no_mangle)]
pub extern "C" fn runtime_start(argc: i32, argv: *mut *mut c_char) {
    args::willow_runtime_store_args(argc, argv);
    gc::willow_gc_init();
    unsafe { willow_user_main() };
}

#[cfg(not(test))]
#[unsafe(no_mangle)]
pub extern "C" fn main(argc: i32, argv: *mut *mut c_char) -> i32 {
    runtime_start(argc, argv);
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_start_stores_arguments_before_user_main() {
        let program = std::ffi::CString::new("willow-test").unwrap();
        let first = std::ffi::CString::new("one").unwrap();
        let second = std::ffi::CString::new("two").unwrap();
        let mut argv = vec![
            program.as_ptr() as *mut c_char,
            first.as_ptr() as *mut c_char,
            second.as_ptr() as *mut c_char,
        ];

        runtime_start(argv.len() as i32, argv.as_mut_ptr());

        assert_eq!(args::willow_runtime_args_len(), 2);
        let arg0 = args::willow_runtime_arg(0);
        assert!(!arg0.is_null());
        assert_eq!(unsafe { string::willow_string_as_str(arg0) }, "one");
    }
}
