use std::ffi::{CStr, CString, c_char};

fn runtime_str(value: *const c_char) -> String {
    if value.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned()
    }
}

pub fn concat_runtime_strings(lhs: *const c_char, rhs: *const c_char) -> String {
    let mut out = runtime_str(lhs);
    out.push_str(&runtime_str(rhs));
    out
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_string_concat(lhs: *const c_char, rhs: *const c_char) -> *mut c_char {
    CString::new(concat_runtime_strings(lhs, rhs))
        .expect("runtime-created string must not contain NUL")
        .into_raw()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(ptr: *mut c_char) -> String {
        unsafe { CString::from_raw(ptr) }.into_string().unwrap()
    }

    #[test]
    fn string_unit_01_concat_two_strings() {
        let lhs = CString::new("hello").unwrap();
        let rhs = CString::new(" world").unwrap();
        assert_eq!(
            concat_runtime_strings(lhs.as_ptr(), rhs.as_ptr()),
            "hello world"
        );
    }

    #[test]
    fn string_unit_02_null_lhs_is_empty() {
        let rhs = CString::new("rhs").unwrap();
        assert_eq!(
            concat_runtime_strings(std::ptr::null(), rhs.as_ptr()),
            "rhs"
        );
    }

    #[test]
    fn string_unit_03_null_rhs_is_empty() {
        let lhs = CString::new("lhs").unwrap();
        assert_eq!(
            concat_runtime_strings(lhs.as_ptr(), std::ptr::null()),
            "lhs"
        );
    }

    #[test]
    fn string_unit_04_export_returns_owned_c_string() {
        let lhs = CString::new("a").unwrap();
        let rhs = CString::new("b").unwrap();
        assert_eq!(
            owned(willow_string_concat(lhs.as_ptr(), rhs.as_ptr())),
            "ab"
        );
    }
}
