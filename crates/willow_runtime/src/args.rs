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
    /// # Safety
    /// For positive argc and non-null argv, provide an aligned array of argc initialized
    /// pointers. Each non-null entry must address a NUL-terminated C string. Keep the array and
    /// strings readable, immutable and at stable addresses through their last runtime use,
    /// including any readers still in flight after replacement.
    unsafe fn store(&self, argc: i32, argv: *mut *mut c_char) {
        let mut args = self.state.lock().expect("runtime args mutex poisoned");
        args.argc = argc;
        args.argv = argv as usize;
        if argc > 1 && !argv.is_null() {
            args.user_argc = argc - 1;
            // SAFETY: The caller supplies argc array entries. With argc > 1, entry one exists.
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
            // SAFETY: The process supplies argc live argv entries. The count check bounds this
            // pointer access.
            let cptr = unsafe { *user_argv.add(index as usize) };
            if cptr.is_null() {
                return std::ptr::null_mut();
            }
            // Own the text and release the argument lock before allocation can stop other mutators.
            // SAFETY: Non-null argv entries are NUL-terminated strings kept alive by the caller.
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
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut arr) };
        if user_argv != 0 {
            let argv = user_argv as *mut *mut c_char;
            for i in 0..len {
                // SAFETY: The process supplies argc live argv entries. The count check bounds this
                // pointer access.
                let cptr = unsafe { *argv.add(i as usize) };
                let s = if cptr.is_null() {
                    allocate("")
                } else {
                    // SAFETY: Non-null argv entries are NUL-terminated strings kept alive by the
                    // caller.
                    let text = unsafe { std::ffi::CStr::from_ptr(cptr) }.to_string_lossy();
                    allocate(&text)
                };
                // SAFETY: The registered slot keeps the reference array live. The callback
                // has just produced this element string and i is within len.
                unsafe { willow_array_set(arr, i, s as i64) };
            }
        }
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
        arr
    }
    fn program_name(&self, allocate: impl FnOnce(&str) -> *mut u8) -> *mut u8 {
        let s = {
            let args = self.state.lock().expect("runtime args mutex poisoned");
            let program = if args.argc <= 0 || args.argv == 0 {
                std::ptr::null_mut()
            } else {
                // SAFETY: The process supplies argc live argv entries. The count check bounds this
                // pointer access.
                unsafe { *(args.argv as *mut *mut c_char) }
            };
            if program.is_null() {
                String::new()
            } else {
                // SAFETY: Non-null argv entries are NUL-terminated strings kept alive by the
                // caller.
                unsafe { std::ffi::CStr::from_ptr(program) }
                    .to_string_lossy()
                    .into_owned()
            }
        };
        allocate(&s)
    }
}

/// # Safety
/// For positive argc and non-null argv, provide an aligned array of argc initialized
/// pointers. Each non-null entry must address a NUL-terminated C string. Keep the array and
/// strings readable, immutable and at stable addresses through their last runtime use,
/// including any readers still in flight after replacement.
///
/// ```compile_fail,E0133
/// willow_runtime::args::willow_runtime_store_args(0, std::ptr::null_mut());
/// ```
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_runtime_store_args(argc: i32, argv: *mut *mut c_char) {
    // SAFETY: The startup caller retains the argv array and its C strings through all runtime
    // argument reads.
    unsafe { default_args().store(argc, argv) }
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

/// `env::var(name) -> Option<String>`: the variable's value, or the null niche
/// when it is unset, not valid Unicode, or `name` cannot name a variable (empty,
/// or containing `=` or NUL — names the platform APIs reject or misread).
///
/// # Safety
/// Each non-null string argument must be the current address of a live, word-aligned Willow
/// String, with its length and UTF-8 bytes initialized and stable while read.
///
/// ```compile_fail,E0133
/// willow_runtime::args::willow_env_var(std::ptr::null());
/// ```
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_env_var(name: *const u8) -> *mut u8 {
    let value = {
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        let name = unsafe { crate::string::willow_string_as_str(name) };
        if name.is_empty() || name.contains(['=', '\0']) {
            return std::ptr::null_mut();
        }
        std::env::var(name).ok()
    };
    value.map_or(std::ptr::null_mut(), |value| willow_string_from_str(&value))
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
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        unsafe { willow_string_as_str(ptr) }.to_string()
    }

    #[test]
    fn env_var_reads_set_variables_and_rejects_invalid_names() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // SAFETY: Null represents an absent name and requires no String storage.
        assert!(unsafe { willow_env_var(std::ptr::null()) }.is_null());
        // SAFETY: The newly allocated key remains current through the environment lookup.
        let read = |name: &str| ws_text(unsafe { willow_env_var(willow_string_from_str(name)) });
        // PATH exists on every CI host; Windows names are case-insensitive.
        assert_eq!(
            read("PATH"),
            std::env::var("PATH").unwrap_or_else(|_| "(null)".into())
        );
        assert_eq!(read("WILLOW_SURELY_UNSET_VARIABLE_8f3a"), "(null)");
        for invalid in ["", "A=B", "=", "A\0B"] {
            assert_eq!(read(invalid), "(null)", "{invalid:?}");
        }
    }

    #[test]
    fn args_unit_01_empty_args_len_is_zero() {
        let owner = Arguments::new();
        // SAFETY: Null argv installs an empty view with no backing storage.
        unsafe { owner.store(0, std::ptr::null_mut()) };
        assert_eq!(owner.len(), 0);
    }

    #[test]
    fn args_unit_02_program_name_defaults_to_empty_string() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        reset_for_tests();
        assert_eq!(ws_text(willow_runtime_program_name()), "");
    }

    #[test]
    fn args_unit_03_user_args_exclude_program_name() {
        let owner = Arguments::new();
        let program = CString::new("prog").unwrap();
        let arg = CString::new("one").unwrap();
        let mut argv = vec![program.as_ptr() as *mut c_char, arg.as_ptr() as *mut c_char];
        // SAFETY: The pointer array and its C strings remain in this fixture until every
        // argument read has completed.
        unsafe { owner.store(2, argv.as_mut_ptr()) };
        assert_eq!(owner.len(), 1);
    }

    #[test]
    fn args_unit_04_arg_returns_requested_user_arg() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        reset_for_tests();
        let program = CString::new("prog").unwrap();
        let arg = CString::new("one").unwrap();
        let mut argv = vec![program.as_ptr() as *mut c_char, arg.as_ptr() as *mut c_char];
        // SAFETY: The pointer array and its C strings remain in this fixture until every
        // argument read has completed.
        unsafe { willow_runtime_store_args(2, argv.as_mut_ptr()) };
        assert_eq!(ws_text(willow_runtime_arg(0)), "one");
        reset_for_tests();
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
        // SAFETY: The pointer array and its C strings remain in this fixture until every
        // argument read has completed.
        unsafe { owner.store(1, argv.as_mut_ptr()) };
        assert!(
            owner
                .arg(0, |_| panic!("invalid index allocated"))
                .is_null()
        );
    }

    #[test]
    fn args_unit_07_program_name_reads_argv_zero() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        reset_for_tests();
        let program = CString::new("prog").unwrap();
        let mut argv = vec![program.as_ptr() as *mut c_char];
        // SAFETY: The pointer array and its C strings remain in this fixture until every
        // argument read has completed.
        unsafe { willow_runtime_store_args(1, argv.as_mut_ptr()) };
        assert_eq!(ws_text(willow_runtime_program_name()), "prog");
        reset_for_tests();
    }

    #[test]
    fn args_unit_08_null_entries_and_null_argv() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        reset_for_tests();
        // SAFETY: Null argv installs an empty view with no backing storage.
        unsafe { willow_runtime_store_args(2, std::ptr::null_mut()) };
        assert_eq!(ws_text(willow_runtime_program_name()), "");
        assert!(willow_runtime_arg(0).is_null());
        let mut argv = [std::ptr::null_mut(); 2];
        // SAFETY: The pointer array and its C strings remain in this fixture until every
        // argument read has completed.
        unsafe { willow_runtime_store_args(2, argv.as_mut_ptr()) };
        assert_eq!(ws_text(willow_runtime_program_name()), "");
        assert!(willow_runtime_arg(0).is_null());
        reset_for_tests();
    }

    #[test]
    fn args_unit_09_snapshots_preserve_lossy_and_increasing_length_text() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let owner = Arguments::new();
        for len in [0, 1, 64, 4096] {
            let mut bytes = vec![b'x'; len];
            bytes.push(0xff);
            let text = CString::new(bytes).unwrap();
            let expected = text.to_string_lossy();
            let mut argv = [text.as_ptr() as *mut c_char; 2];
            // SAFETY: The pointer array and its C strings remain in this fixture until every
            // argument read has completed.
            unsafe { owner.store(2, argv.as_mut_ptr()) };
            assert_eq!(
                ws_text(owner.program_name(|text| collecting_string(&owner, text))),
                expected
            );
            assert_eq!(
                ws_text(owner.arg(0, |text| collecting_string(&owner, text))),
                expected
            );
            let array = owner.array(|text| collecting_string(&owner, text));
            // SAFETY: The fixture retains the array allocated for these accesses.
            let value = unsafe { crate::array::willow_array_get(array, 0) } as *mut u8;
            assert_eq!(ws_text(value), expected);
            // SAFETY: Null argv installs an empty view with no backing storage.
            unsafe { owner.store(0, std::ptr::null_mut()) };
        }
    }

    #[test]
    fn args_array_finishes_reading_its_original_view_after_replacement() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let owner = Arguments::new();
        let mut original = [
            c"program".as_ptr().cast_mut(),
            c"first".as_ptr().cast_mut(),
            c"last".as_ptr().cast_mut(),
        ];
        let mut replacement = [c"other".as_ptr().cast_mut(), c"next".as_ptr().cast_mut()];
        // SAFETY: Both arrays remain in this scope and the C literals are static.
        unsafe { owner.store(3, original.as_mut_ptr()) };
        let replacement = replacement.as_mut_ptr();
        let result = owner.array(|text| {
            // SAFETY: The original array stays live while this in-flight reader is using it.
            // The new array and its two initialized entries survive all later reads.
            unsafe { owner.store(2, replacement) };
            collecting_string(&owner, text)
        });
        // SAFETY: The completed array contains two initialized String references. Neither
        // retrieval nor ws_text runs a GC safepoint.
        let fields = unsafe {
            [
                crate::array::willow_array_get(result, 0),
                crate::array::willow_array_get(result, 1),
            ]
        };
        assert_eq!(
            fields.map(|field| ws_text(field as *mut u8)),
            ["first", "last"]
        );
        assert_eq!(ws_text(owner.arg(0, willow_string_from_str)), "next");
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
                    // SAFETY: The pointer array and its C strings remain in this fixture
                    // until every argument read has completed.
                    unsafe { runtime.args.store(argv.len() as i32, argv.as_mut_ptr()) };
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
                    // SAFETY: Null argv installs an empty view with no backing storage.
                    unsafe { runtime.args.store(0, std::ptr::null_mut()) };
                    assert_eq!(runtime.args.len(), 0);
                    assert!(std::sync::LazyLock::get(&runtime.gc).is_none());
                }));
            }
        });
    }
}
