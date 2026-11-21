//! `std::path`: lexical path operations with the host platform's rules.
//!
//! Every function delegates to Rust's `std::path`, so separators, Windows
//! drive/UNC prefixes and root handling follow the target platform exactly as
//! Rust programs see them. No function touches the file system.
//!
//! Inputs are Willow strings (UTF-8), so every component Rust returns is valid
//! UTF-8 as well; the conversions below never lose text. Each entry point
//! copies what it needs out of the borrowed input before it allocates the
//! result, so no Willow pointer is held across a possible collection.

use std::path::Path;

use crate::string::{willow_string_as_str, willow_string_from_str};

/// Allocate `text` as a Willow `Option<String>`: a string, or the null niche.
fn optional(text: Option<String>) -> *mut u8 {
    text.map_or(std::ptr::null_mut(), |text| willow_string_from_str(&text))
}

/// Borrow `path` and apply `component`, owning the result before allocation.
///
/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
unsafe fn component(path: *const u8, component: impl FnOnce(&Path) -> Option<&str>) -> *mut u8 {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let text = unsafe { willow_string_as_str(path) };
    optional(component(Path::new(text)).map(str::to_owned))
}

/// `path::join(base, child) -> String`. An absolute `child` replaces `base`,
/// as with Rust's `Path::join`.
///
/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
///
/// ```compile_fail,E0133
/// willow_runtime::path::willow_path_join(std::ptr::null(), std::ptr::null());
/// ```
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_path_join(base: *const u8, child: *const u8) -> *mut u8 {
    let joined = {
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        let base = unsafe { willow_string_as_str(base) };
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        let child = unsafe { willow_string_as_str(child) };
        Path::new(base).join(child)
    };
    // Both inputs were UTF-8 and joining inserts only an ASCII separator, so
    // the lossy conversion never replaces anything.
    willow_string_from_str(&joined.to_string_lossy())
}

/// `path::parent(path) -> Option<String>`: `None` for a root or empty path.
///
/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_path_parent(path: *const u8) -> *mut u8 {
    // SAFETY: The caller supplies a live immutable Willow path through this component lookup.
    unsafe { component(path, |path| path.parent().and_then(Path::to_str)) }
}

/// `path::file_name(path) -> Option<String>`: `None` when the path ends in `..`
/// or is a root.
///
/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_path_file_name(path: *const u8) -> *mut u8 {
    // SAFETY: The caller supplies a live immutable Willow path through this component lookup.
    unsafe { component(path, |path| path.file_name().and_then(|name| name.to_str())) }
}

/// `path::extension(path) -> Option<String>`: text after the last `.` of the
/// file name, `None` for no dot or a leading-dot-only name such as `.bashrc`.
///
/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_path_extension(path: *const u8) -> *mut u8 {
    // SAFETY: The caller supplies a live immutable Willow path through this component lookup.
    unsafe { component(path, |path| path.extension().and_then(|ext| ext.to_str())) }
}

/// `path::file_stem(path) -> Option<String>`: the file name without its
/// extension.
///
/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_path_file_stem(path: *const u8) -> *mut u8 {
    // SAFETY: The caller supplies a live immutable Willow path through this component lookup.
    unsafe { component(path, |path| path.file_stem().and_then(|stem| stem.to_str())) }
}

/// `path::is_absolute(path) -> bool` (as 0/1). On Windows an absolute path
/// needs a prefix and a root (`C:\x`, `\\server\share\x`).
///
/// # Safety
/// Each non-null string argument must point to a live, aligned Willow String with its
/// initialized length and UTF-8 payload. Keep the bytes stable and immutable through the
/// borrow.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_path_is_absolute(path: *const u8) -> i64 {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let text = unsafe { willow_string_as_str(path) };
    i64::from(Path::new(text).is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{isolated_runtime, willow_gc_init};

    fn text(ptr: *mut u8) -> Option<String> {
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        (!ptr.is_null()).then(|| unsafe { willow_string_as_str(ptr) }.to_owned())
    }

    fn call(f: unsafe extern "C" fn(*const u8) -> *mut u8, input: &str) -> Option<String> {
        // SAFETY: Each supplied path operation accepts the freshly allocated input string.
        text(unsafe { f(willow_string_from_str(input)) })
    }

    #[test]
    fn path_components_follow_host_rules() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        {
            let sep = std::path::MAIN_SEPARATOR_STR;
            // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
            // during this call.
            let joined = text(unsafe {
                willow_path_join(
                    willow_string_from_str("dir"),
                    willow_string_from_str("file.txt"),
                )
            });
            assert_eq!(joined.as_deref(), Some(&*format!("dir{sep}file.txt")));
            // An empty base adds no separator.
            // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
            // during this call.
            let joined = text(unsafe {
                willow_path_join(willow_string_from_str(""), willow_string_from_str("a"))
            });
            assert_eq!(joined.as_deref(), Some("a"));
            let absolute = if cfg!(windows) { r"C:\root" } else { "/root" };
            // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
            // during this call.
            let joined = text(unsafe {
                willow_path_join(
                    willow_string_from_str("dir"),
                    willow_string_from_str(absolute),
                )
            });
            assert_eq!(joined.as_deref(), Some(absolute));

            assert_eq!(
                call(willow_path_parent, "a/b/c.txt").as_deref(),
                Some("a/b")
            );
            assert_eq!(call(willow_path_parent, "c.txt").as_deref(), Some(""));
            assert_eq!(call(willow_path_parent, ""), None);
            assert_eq!(call(willow_path_parent, "/"), None);
            assert_eq!(
                call(willow_path_file_name, "a/b/c.tar.gz").as_deref(),
                Some("c.tar.gz")
            );
            assert_eq!(call(willow_path_file_name, "a/.."), None);
            assert_eq!(
                call(willow_path_extension, "c.tar.gz").as_deref(),
                Some("gz")
            );
            assert_eq!(call(willow_path_extension, ".bashrc"), None);
            assert_eq!(call(willow_path_extension, "noext"), None);
            assert_eq!(
                call(willow_path_file_stem, "c.tar.gz").as_deref(),
                Some("c.tar")
            );
            assert_eq!(
                call(willow_path_file_stem, "日本.txt").as_deref(),
                Some("日本")
            );

            assert_eq!(
                // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
                // during this call.
                unsafe { willow_path_is_absolute(willow_string_from_str(absolute)) },
                1
            );
            assert_eq!(
                // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
                // during this call.
                unsafe { willow_path_is_absolute(willow_string_from_str("a/b")) },
                0
            );
            assert_eq!(
                // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
                // during this call.
                unsafe { willow_path_is_absolute(willow_string_from_str("")) },
                0
            );
            if cfg!(windows) {
                assert_eq!(
                    call(willow_path_parent, r"C:\a\b").as_deref(),
                    Some(r"C:\a")
                );
                assert_eq!(
                    call(willow_path_file_name, r"C:\a\b.txt").as_deref(),
                    Some("b.txt")
                );
                // A rooted path without a drive is relative to the current drive.
                assert_eq!(
                    // SAFETY: Each input is null or a live Willow string whose bytes remain
                    // immutable during this call.
                    unsafe { willow_path_is_absolute(willow_string_from_str(r"\a")) },
                    0
                );
            } else {
                // A backslash is an ordinary file-name byte off Windows.
                assert_eq!(
                    call(willow_path_file_name, r"a\b.txt").as_deref(),
                    Some(r"a\b.txt")
                );
            }
        }
    }
}
