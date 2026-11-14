use std::ffi::c_char;
use std::sync::Mutex;

use crate::array::willow_array_set;
use crate::gc::{willow_pop_roots, willow_push_root};
use crate::string::willow_string_from_str;

#[derive(Debug, Default, Clone, Copy)]
struct RuntimeArgs {
    argc: i32,
    argv: usize,
    user_argc: i32,
    user_argv: usize,
}

/// Per-runtime view of ABI-provided arguments. As with the C entry point,
/// backing argv storage must remain valid while this view is used.
pub(crate) struct Arguments {
    state: Mutex<RuntimeArgs>,
}
impl Arguments {
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(RuntimeArgs {
                argc: 0,
                argv: 0,
                user_argc: 0,
                user_argv: 0,
            }),
        }
    }
}
fn default_args() -> &'static Arguments {
    &crate::runtime::current().args
}

impl Arguments {
    fn store(&self, argc: i32, argv: *mut *mut c_char) {
        let mut args = self.state.lock().expect("runtime args mutex poisoned");
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
    fn len(&self) -> i64 {
        self.state
            .lock()
            .expect("runtime args mutex poisoned")
            .user_argc as i64
    }
    fn arg(&self, index: i64, allocate: impl FnOnce(&str) -> *mut u8) -> *mut u8 {
        let s = {
            let args = self.state.lock().expect("runtime args mutex poisoned");
            if index < 0 || index >= args.user_argc as i64 || args.user_argv == 0 {
                return std::ptr::null_mut();
            }
            let user_argv = args.user_argv as *mut *mut c_char;
            let cptr = unsafe { *user_argv.add(index as usize) };
            if cptr.is_null() {
                return std::ptr::null_mut();
            }
            // Own the text and release the argument lock before allocation can stop other mutators.
            unsafe { std::ffi::CStr::from_ptr(cptr) }
                .to_string_lossy()
                .into_owned()
        };
        allocate(&s)
    }
    fn array(&self, allocate: impl Fn(&str) -> *mut u8) -> *mut u8 {
        let (user_argc, user_argv) = {
            let args = self.state.lock().expect("runtime args mutex poisoned");
            (args.user_argc, args.user_argv)
        };
        let len = user_argc.max(0) as i64;
        // Reference-element array (each slot holds a WillowString pointer).
        let mut arr = crate::array::new_old_array(len, 1);
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
                    allocate("")
                } else {
                    let text = unsafe { std::ffi::CStr::from_ptr(cptr) }.to_string_lossy();
                    allocate(&text)
                };
                willow_array_set(arr, i, s as i64);
            }
        }
        willow_pop_roots(1);
        arr
    }
    fn program_name(&self, allocate: impl FnOnce(&str) -> *mut u8) -> *mut u8 {
        let s = {
            let args = self.state.lock().expect("runtime args mutex poisoned");
            let program = if args.argc <= 0 || args.argv == 0 {
                std::ptr::null_mut()
            } else {
                unsafe { *(args.argv as *mut *mut c_char) }
            };
            if program.is_null() {
                String::new()
            } else {
                unsafe { std::ffi::CStr::from_ptr(program) }
                    .to_string_lossy()
                    .into_owned()
            }
        };
        allocate(&s)
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_runtime_store_args(argc: i32, argv: *mut *mut c_char) {
    default_args().store(argc, argv)
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_runtime_args_len() -> i64 {
    default_args().len()
}

/// Returns the nullable-pointer-niche representation of `Option<String>`:
/// a GC-managed WillowString for an in-range user argument, or zero for None.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_runtime_arg(index: i64) -> *mut u8 {
    default_args().arg(index, willow_string_from_str)
}

/// Build a GC-managed `Array<String>` of the user arguments (excluding the
/// program name). Used for `env::args()` and to bind a `fn main(args:
/// Array<String>)` parameter.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_runtime_args_array() -> *mut u8 {
    default_args().array(willow_string_from_str)
}

/// Returns a GC-managed WillowString for the program name (argv[0]).
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_runtime_program_name() -> *mut u8 {
    default_args().program_name(willow_string_from_str)
}

#[cfg(test)]
pub fn reset_for_tests() {
    *default_args()
        .state
        .lock()
        .expect("runtime args mutex poisoned") = RuntimeArgs::default();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{isolated_runtime, willow_gc_init};
    use crate::string::willow_string_as_str;
    use std::ffi::CString;

    fn collecting_string(owner: &Arguments, text: &str) -> *mut u8 {
        std::thread::scope(|scope| {
            assert!(
                scope
                    .spawn(crate::runtime::serving(|| owner.state.try_lock().is_ok()))
                    .join()
                    .unwrap(),
                "argument lock must be released before managed allocation"
            );
        });
        crate::gc::willow_gc_collect();
        willow_string_from_str(text)
    }

    fn ws_text(ptr: *mut u8) -> String {
        if ptr.is_null() {
            return "(null)".to_string();
        }
        unsafe { willow_string_as_str(ptr) }.to_string()
    }

    #[test]
    fn args_unit_01_empty_args_len_is_zero() {
        let owner = Arguments::new();
        owner.store(0, std::ptr::null_mut());
        assert_eq!(owner.len(), 0);
    }

    #[test]
    fn args_unit_02_program_name_defaults_to_empty_string() {
        let _guard = isolated_runtime();
        willow_gc_init();
        reset_for_tests();
        assert_eq!(ws_text(willow_runtime_program_name()), "");
    }

    #[test]
    fn args_unit_03_user_args_exclude_program_name() {
        let owner = Arguments::new();
        let program = CString::new("prog").unwrap();
        let arg = CString::new("one").unwrap();
        let mut argv = vec![program.as_ptr() as *mut c_char, arg.as_ptr() as *mut c_char];
        owner.store(2, argv.as_mut_ptr());
        assert_eq!(owner.len(), 1);
    }

    #[test]
    fn args_unit_04_arg_returns_requested_user_arg() {
        let _guard = isolated_runtime();
        willow_gc_init();
        reset_for_tests();
        let program = CString::new("prog").unwrap();
        let arg = CString::new("one").unwrap();
        let mut argv = vec![program.as_ptr() as *mut c_char, arg.as_ptr() as *mut c_char];
        willow_runtime_store_args(2, argv.as_mut_ptr());
        assert_eq!(ws_text(willow_runtime_arg(0)), "one");
    }

    #[test]
    fn args_unit_05_negative_arg_index_returns_null() {
        let owner = Arguments::new();
        assert!(
            owner
                .arg(-1, |_| panic!("invalid index allocated"))
                .is_null()
        );
    }

    #[test]
    fn args_unit_06_out_of_range_arg_index_returns_null() {
        let owner = Arguments::new();
        let program = CString::new("prog").unwrap();
        let mut argv = vec![program.as_ptr() as *mut c_char];
        owner.store(1, argv.as_mut_ptr());
        assert!(
            owner
                .arg(0, |_| panic!("invalid index allocated"))
                .is_null()
        );
    }

    #[test]
    fn args_unit_07_program_name_reads_argv_zero() {
        let _guard = isolated_runtime();
        willow_gc_init();
        reset_for_tests();
        let program = CString::new("prog").unwrap();
        let mut argv = vec![program.as_ptr() as *mut c_char];
        willow_runtime_store_args(1, argv.as_mut_ptr());
        assert_eq!(ws_text(willow_runtime_program_name()), "prog");
    }

    #[test]
    fn args_unit_08_null_entries_and_null_argv() {
        let _guard = isolated_runtime();
        willow_gc_init();
        reset_for_tests();
        willow_runtime_store_args(2, std::ptr::null_mut());
        assert_eq!(ws_text(willow_runtime_program_name()), "");
        assert!(willow_runtime_arg(0).is_null());
        let mut argv = [std::ptr::null_mut(); 2];
        willow_runtime_store_args(2, argv.as_mut_ptr());
        assert_eq!(ws_text(willow_runtime_program_name()), "");
        assert!(willow_runtime_arg(0).is_null());
        reset_for_tests();
    }

    #[test]
    fn args_unit_09_snapshots_preserve_lossy_and_increasing_length_text() {
        let _guard = isolated_runtime();
        willow_gc_init();
        let owner = Arguments::new();
        for len in [0, 1, 64, 4096] {
            let mut bytes = vec![b'x'; len];
            bytes.push(0xff);
            let text = CString::new(bytes).unwrap();
            let expected = text.to_string_lossy();
            let mut argv = [text.as_ptr() as *mut c_char; 2];
            owner.store(2, argv.as_mut_ptr());
            assert_eq!(
                ws_text(owner.program_name(|text| collecting_string(&owner, text))),
                expected
            );
            assert_eq!(
                ws_text(owner.arg(0, |text| collecting_string(&owner, text))),
                expected
            );
            let array = owner.array(|text| collecting_string(&owner, text));
            let value = crate::array::willow_array_get(array, 0) as *mut u8;
            assert_eq!(ws_text(value), expected);
            owner.store(0, std::ptr::null_mut());
        }
    }

    #[test]
    fn private_runtime_argument_snapshots_remain_independent() {
        std::thread::scope(|scope| {
            for count in [1, 64] {
                scope.spawn(crate::runtime::serving(move || {
                    let runtime =
                        crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
                    let strings: Vec<_> = (0..=count)
                        .map(|index| CString::new(format!("{count}:{index}")).unwrap())
                        .collect();
                    let mut argv: Vec<_> =
                        strings.iter().map(|s| s.as_ptr() as *mut c_char).collect();
                    runtime.args.store(argv.len() as i32, argv.as_mut_ptr());
                    for _ in 0..32 {
                        assert_eq!(runtime.args.len(), count);
                        runtime.args.program_name(|text| {
                            assert_eq!(text, format!("{count}:0"));
                            assert!(runtime.args.state.try_lock().is_ok());
                            std::ptr::null_mut()
                        });
                        for index in 0..count {
                            runtime.args.arg(index, |text| {
                                assert_eq!(text, format!("{count}:{}", index + 1));
                                assert!(runtime.args.state.try_lock().is_ok());
                                std::ptr::null_mut()
                            });
                        }
                    }
                    runtime.args.store(0, std::ptr::null_mut());
                    assert_eq!(runtime.args.len(), 0);
                    assert!(std::sync::LazyLock::get(&runtime.gc).is_none());
                }));
            }
        });
    }
}
