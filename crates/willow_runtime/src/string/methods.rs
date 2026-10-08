//! Core String operations. All positions are UTF-8 byte offsets.
use super::{string_payload_size, ws_as_bytes};
use crate::gc::{GcObjectKind, willow_alloc_with_layout, willow_pop_roots, willow_push_root};

// String creation validates UTF-8. These operations preserve that invariant;
// rescanning the entire receiver for each substring would make splitting quadratic.
// No borrowed view may survive a GC allocation: retain offsets, root the payload,
// then derive fresh addresses after allocation instead.
unsafe fn text<'a>(value: *const u8) -> &'a str {
    if value.is_null() {
        return "";
    }
    let (bytes, len) = unsafe { ws_as_bytes(value) };
    unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(bytes, len)) }
}

fn fail(message: &str) -> *mut u8 {
    crate::panic_context::raise_language_message(message);
    std::ptr::null_mut()
}

// Instrument the actual copy operations, rather than expected output lengths.
#[inline]
unsafe fn copy_bytes(source: *const u8, destination: *mut u8, len: usize) {
    #[cfg(test)]
    COPIED_BYTES.with(|bytes| bytes.set(bytes.get() + len));
    unsafe { std::ptr::copy_nonoverlapping(source, destination, len) };
}

// The caller supplies a checked range on UTF-8 boundaries.
fn copy_range(mut value: *mut u8, start: usize, end: usize) -> *mut u8 {
    let len = end - start;
    let size = string_payload_size(len).expect("range fits an existing String");
    willow_push_root(&mut value);
    let result = willow_alloc_with_layout(GcObjectKind::String, 0, size, 0);
    if !result.is_null() {
        unsafe {
            *(result as *mut i64) = len as i64;
            if len != 0 {
                copy_bytes(value.add(8 + start), result.add(8), len);
            }
            *result.add(8 + len) = 0;
        }
    }
    willow_pop_roots(1);
    result
}

/// Copy [start, end); invalid ranges/boundaries raise a recoverable language panic.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_string_substring(value: *mut u8, start: i64, end: i64) -> *mut u8 {
    let s = unsafe { text(value) };
    if start < 0
        || end < start
        || end as usize > s.len()
        || !s.is_char_boundary(start as usize)
        || !s.is_char_boundary(end as usize)
    {
        return fail(&format!(
            "String.substring requires an ordered in-bounds range on UTF-8 boundaries (start={start}, end={end}, len={})",
            s.len()
        ));
    }
    copy_range(value, start as usize, end as usize)
}

/// First byte offset, or -1 when absent. Empty needles match at zero.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_string_find(value: *const u8, needle: *const u8) -> i64 {
    unsafe {
        text(value)
            .find(text(needle))
            .map_or(-1, |index| index as i64)
    }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_string_contains(value: *const u8, needle: *const u8) -> i64 {
    i64::from(willow_string_find(value, needle) >= 0)
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_string_starts_with(value: *const u8, prefix: *const u8) -> i64 {
    unsafe { i64::from(text(value).starts_with(text(prefix))) }
}

/// Remove Unicode White_Space from both ends; interior whitespace is preserved.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_string_trim(value: *mut u8) -> *mut u8 {
    let s = unsafe { text(value) };
    let trimmed = s.trim();
    let start = trimmed.as_ptr() as usize - s.as_ptr() as usize;
    copy_range(value, start, start + trimmed.len())
}

/// Repeat whole strings; negative counts or payload-size overflow raise a panic.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_string_repeat(mut value: *mut u8, count: i64) -> *mut u8 {
    if count < 0 {
        return fail("String.repeat requires a nonnegative count");
    }
    let (_, len) = unsafe { ws_as_bytes(value) };
    let Some((total, size)) = len
        .checked_mul(count as usize)
        .and_then(|total| string_payload_size(total).map(|size| (total, size)))
    else {
        return fail("String.repeat size overflow");
    };
    willow_push_root(&mut value);
    let result = willow_alloc_with_layout(GcObjectKind::String, 0, size, 0);
    if !result.is_null() {
        unsafe {
            *(result as *mut i64) = total as i64;
            // Doubling copies avoid count-dependent work for empty inputs and
            // use O(log(count)) copy operations for nonempty inputs, O(total) bytes.
            if total != 0 {
                copy_bytes(value.add(8), result.add(8), len);
                let mut filled = len;
                while filled < total {
                    let copied = filled.min(total - filled);
                    copy_bytes(result.add(8), result.add(8 + filled), copied);
                    filled += copied;
                }
            }
            *result.add(8 + total) = 0;
        }
    }
    willow_pop_roots(1);
    result
}

/// Literal, non-overlapping separator. Preserve empty fields. An empty separator
/// yields leading/trailing empty fields with Unicode scalar values between them.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_string_split(mut value: *mut u8, separator: *const u8) -> *mut u8 {
    // One search pass. Only integer ranges survive GC; the separator is no
    // longer needed after this pass. No per-field rescanning or array growth.
    let ranges: Vec<(usize, usize)> = {
        let s = unsafe { text(value) };
        let base = s.as_ptr() as usize;
        s.split(unsafe { text(separator) })
            .map(|part| {
                let start = part.as_ptr() as usize - base;
                (start, start + part.len())
            })
            .collect()
    };
    willow_push_root(&mut value);
    let mut result = crate::array::willow_array_new(ranges.len() as i64, 1);
    if result.is_null() {
        willow_pop_roots(1);
        return result;
    }
    willow_push_root(&mut result);
    for (index, (start, end)) in ranges.into_iter().enumerate() {
        let part = copy_range(value, start, end);
        if part.is_null() {
            willow_pop_roots(2);
            return part;
        }
        // A valid in-bounds store has no allocation/safepoint; array_set applies
        // the GC write barrier, and the rooted array retains every earlier part.
        crate::array::willow_array_set(result, index as i64, part as i64);
    }
    willow_pop_roots(2);
    result
}

#[cfg(test)]
thread_local! {
    static COPIED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::string::{willow_string_as_str, willow_string_from_str};

    #[test]
    fn string_methods_copy_work_scales_with_output() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::willow_gc_init();
        for count in [1, 8, 64, 512] {
            let mut source = willow_string_from_str(&"日,".repeat(count));
            willow_push_root(&mut source);
            let separator = willow_string_from_str(",");
            COPIED_BYTES.with(|bytes| bytes.set(0));
            let result = willow_string_split(source, separator);
            assert_eq!(crate::array::willow_array_len(result), count as i64 + 1);
            assert_eq!(COPIED_BYTES.with(|bytes| bytes.get()), count * 3);
            for index in 0..count {
                let part = crate::array::willow_array_get(result, index as i64) as *const u8;
                assert_eq!(unsafe { willow_string_as_str(part) }, "日");
            }
            COPIED_BYTES.with(|bytes| bytes.set(0));
            let repeated = willow_string_repeat(source, 8);
            assert_eq!(
                super::super::willow_string_len(repeated),
                (count * 4 * 8) as i64
            );
            assert_eq!(COPIED_BYTES.with(|bytes| bytes.get()), count * 4 * 8);
            // A tiny slice near the end does not scan/copy the large prefix.
            COPIED_BYTES.with(|bytes| bytes.set(0));
            let tail =
                willow_string_substring(source, (count * 4 - 4) as i64, (count * 4 - 1) as i64);
            assert_eq!(unsafe { willow_string_as_str(tail) }, "日");
            assert_eq!(COPIED_BYTES.with(|bytes| bytes.get()), 3);
            println!(
                "fields={count} split_bytes={} repeat_bytes={} tail_bytes=3",
                count * 3,
                count * 4 * 8
            );
            willow_pop_roots(1);
        }
    }

    #[test]
    fn string_methods_embedded_nul_and_null_runtime_inputs() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::willow_gc_init();
        let mut source = willow_string_from_str("a\0日本\0b");
        willow_push_root(&mut source);
        let mut separator = willow_string_from_str("\0");
        willow_push_root(&mut separator);
        assert_eq!(willow_string_find(source, separator), 1);
        assert_eq!(willow_string_contains(source, separator), 1);
        let result = willow_string_split(source, separator);
        assert_eq!(crate::array::willow_array_len(result), 3);
        let part = crate::array::willow_array_get(result, 1) as *const u8;
        assert_eq!(unsafe { willow_string_as_str(part) }, "日本");
        let repeated = willow_string_repeat(source, 2);
        assert_eq!(
            unsafe { willow_string_as_str(repeated) },
            "a\0日本\0ba\0日本\0b"
        );
        let nil = std::ptr::null_mut();
        assert_eq!(willow_string_find(nil, nil), 0);
        assert_eq!(willow_string_starts_with(nil, nil), 1);
        assert_eq!(unsafe { willow_string_as_str(willow_string_trim(nil)) }, "");
        assert_eq!(
            unsafe { willow_string_as_str(willow_string_substring(nil, 0, 0)) },
            ""
        );
        willow_pop_roots(2);
    }
}
