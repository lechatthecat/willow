//! Core String operations. All positions are UTF-8 byte offsets.
use super::{SlotRoots, alloc_string_payload_rooted, string_payload_size, ws_as_bytes};
use willow_abi::string_layout::{DATA_OFFSET, LEN_OFFSET};

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

// The caller supplies a checked range on UTF-8 boundaries. Called only from
// runtime entries invoked directly by generated code: a TLAB hit reaches no
// GC point; a miss roots `value` in a shadow-stack slot and reloads it.
fn copy_range(value: *mut u8, start: usize, end: usize) -> *mut u8 {
    let size = range_payload_size(start, end);
    let mut roots = [value];
    let result = alloc_string_payload_rooted(size, &mut roots);
    unsafe { fill_range(result, roots[0], start, end) };
    result
}

fn range_payload_size(start: usize, end: usize) -> usize {
    string_payload_size(end - start).expect("range fits an existing String") as usize
}

/// Write the length, bytes [start, end) of `source` and the NUL into the fresh
/// String payload `result` (null after a failed allocation is ignored).
///
/// # Safety
/// `result` is null or holds `end - start` bytes plus header and NUL; `source`
/// is the current address of a String covering the range.
unsafe fn fill_range(result: *mut u8, source: *mut u8, start: usize, end: usize) {
    if result.is_null() {
        return;
    }
    let len = end - start;
    unsafe {
        *(result.add(LEN_OFFSET as usize) as *mut i64) = len as i64;
        if len != 0 {
            copy_bytes(
                source.add(DATA_OFFSET + start),
                result.add(DATA_OFFSET),
                len,
            );
        }
        *result.add(DATA_OFFSET + len) = 0;
    }
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

/// Read one UTF-8 byte in constant time; invalid offsets raise a language panic.
/// Successful reads allocate nothing and never scan or decode the receiver.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_string_byte_at(value: *const u8, index: i64) -> i64 {
    let (bytes, len) = unsafe { ws_as_bytes(value) };
    if index < 0 || index as usize >= len {
        fail(&format!(
            "String.byte_at requires an in-bounds byte offset (index={index}, len={len})"
        ));
        return 0;
    }
    #[cfg(test)]
    BYTE_READS.with(|reads| reads.set(reads.get() + 1));
    unsafe { i64::from(*bytes.add(index as usize)) }
}

#[cfg(test)]
thread_local! {
    static BYTE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
pub extern "C" fn willow_string_repeat(value: *mut u8, count: i64) -> *mut u8 {
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
    let mut roots = [value];
    let result = alloc_string_payload_rooted(size as usize, &mut roots);
    if !result.is_null() {
        unsafe {
            *(result.add(LEN_OFFSET as usize) as *mut i64) = total as i64;
            // Doubling copies avoid count-dependent work for empty inputs and
            // use O(log(count)) copy operations for nonempty inputs, O(total) bytes.
            if total != 0 {
                copy_bytes(roots[0].add(DATA_OFFSET), result.add(DATA_OFFSET), len);
                let mut filled = len;
                while filled < total {
                    let copied = filled.min(total - filled);
                    copy_bytes(
                        result.add(DATA_OFFSET),
                        result.add(DATA_OFFSET + filled),
                        copied,
                    );
                    filled += copied;
                }
            }
            *result.add(DATA_OFFSET + total) = 0;
        }
    }
    result
}

/// Join string elements in order, with separators only between elements.
/// Two element passes and exactly one output allocation: O(n + output bytes).
/// The array retains its children; reload all managed addresses after allocation.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_string_concat_array(array: *mut u8) -> *mut u8 {
    willow_array_join(array, std::ptr::null_mut())
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_string_debug(value: *mut u8) -> *mut u8 {
    let text = unsafe { crate::string::willow_string_as_str(value) };
    crate::string::alloc_native_string(&format!("{text:?}"))
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_array_join(array: *mut u8, separator: *mut u8) -> *mut u8 {
    if array.is_null() {
        return fail("cannot join a null array");
    }
    let count = crate::array::willow_array_len(array);
    let separator_len = unsafe { ws_as_bytes(separator).1 };
    let Some(mut total) = separator_len.checked_mul((count as usize).saturating_sub(1)) else {
        return fail("Array.join size overflow");
    };
    for index in 0..count {
        let part = join_element(array, index);
        let Some(next) = total.checked_add(unsafe { ws_as_bytes(part).1 }) else {
            return fail("Array.join size overflow");
        };
        total = next;
    }
    let Some(size) = string_payload_size(total) else {
        return fail("Array.join size overflow");
    };
    let mut roots = [array, separator];
    let result = alloc_string_payload_rooted(size as usize, &mut roots);
    let [array, separator] = roots;
    if !result.is_null() {
        unsafe {
            *(result.add(LEN_OFFSET as usize) as *mut i64) = total as i64;
            let mut offset = 0;
            for index in 0..count {
                if index != 0 && separator_len != 0 {
                    copy_bytes(
                        separator.add(DATA_OFFSET),
                        result.add(DATA_OFFSET + offset),
                        separator_len,
                    );
                    offset += separator_len;
                }
                let (bytes, len) = ws_as_bytes(join_element(array, index));
                if len != 0 {
                    copy_bytes(bytes, result.add(DATA_OFFSET + offset), len);
                    offset += len;
                }
            }
            *result.add(DATA_OFFSET + total) = 0;
        }
    }
    result
}

fn join_element(array: *mut u8, index: i64) -> *mut u8 {
    #[cfg(test)]
    JOIN_VISITS.set(JOIN_VISITS.get() + 1);
    crate::array::willow_array_get(array, index) as *mut u8
}

/// Literal, non-overlapping separator. Preserve empty fields. An empty separator
/// yields leading/trailing empty fields with Unicode scalar values between them.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_string_split(value: *mut u8, separator: *const u8) -> *mut u8 {
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
    // Root the receiver and result array in shadow-stack slots for the whole
    // call: TLAB hits reach no GC point, and a miss may move both referents.
    let mut roots = [value, std::ptr::null_mut()];
    let _roots = SlotRoots::push(&mut roots);
    let result = crate::array::willow_array_new(ranges.len() as i64, 1);
    if result.is_null() {
        return result;
    }
    roots[1] = result;
    for (index, (start, end)) in ranges.into_iter().enumerate() {
        let part = alloc_string_payload_rooted(range_payload_size(start, end), &mut []);
        if part.is_null() {
            return part;
        }
        unsafe { fill_range(part, roots[0], start, end) };
        // A valid in-bounds store has no allocation/safepoint; array_set applies
        // the GC write barrier, and the rooted array retains every earlier part.
        crate::array::willow_array_set(roots[1], index as i64, part as i64);
    }
    roots[1]
}

#[cfg(test)]
thread_local! {
    static JOIN_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static COPIED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{GcRootHandle, willow_pop_roots, willow_push_root};
    use crate::string::{willow_string_as_str, willow_string_from_str};

    #[test]
    fn string_bytes_measured_constant_access_and_linear_iteration() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::willow_gc_init();
        for count in [1, 8, 64, 512, 4096] {
            let input = "é\0🌲".repeat(count);
            let string = willow_string_from_str(&input);
            let allocated = crate::gc::willow_gc_total_allocated_bytes();
            BYTE_READS.with(|reads| reads.set(0));
            // Reverse traversal is an adverse shape for prefix-decoding access.
            for (i, expected) in input.bytes().enumerate().rev() {
                assert_eq!(willow_string_byte_at(string, i as i64), i64::from(expected));
            }
            assert_eq!(BYTE_READS.with(|reads| reads.get()), input.len());
            for _ in 0..count {
                assert_eq!(willow_string_byte_at(string, (input.len() - 1) as i64), 178);
            }
            assert_eq!(BYTE_READS.with(|reads| reads.get()), input.len() + count);
            assert_eq!(crate::gc::willow_gc_total_allocated_bytes(), allocated);
        }
    }

    #[test]
    fn array_join_measured_linear_work() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::willow_gc_init();
        for word in ["", "é\0日", "a moderately long field"] {
            for count in [0, 1, 8, 64, 512, 4096, 20000] {
                let array = GcRootHandle::new(crate::array::willow_array_new(count, 1));
                // Distinct allocated elements exercise fragmented input storage.
                for index in 0..count {
                    let part = willow_string_from_str(word);
                    crate::array::willow_array_set(array.load(), index, part as i64);
                }
                let separator = GcRootHandle::new(willow_string_from_str("😀\0"));
                COPIED_BYTES.set(0);
                JOIN_VISITS.set(0);
                let result = willow_array_join(array.load(), separator.load());
                let expected = vec![word; count as usize].join("😀\0");
                assert_eq!(unsafe { willow_string_as_str(result) }, expected);
                assert_eq!(JOIN_VISITS.get(), count as usize * 2);
                assert_eq!(COPIED_BYTES.get(), expected.len());
                assert_eq!(unsafe { *result.add(DATA_OFFSET + expected.len()) }, 0);
                println!(
                    "word_bytes={} elements={count} visits={} copied_bytes={}",
                    word.len(),
                    JOIN_VISITS.get(),
                    COPIED_BYTES.get()
                );
            }
        }
    }

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
