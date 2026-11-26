//! Core String operations. All positions are UTF-8 byte offsets.
use std::cell::Cell;

use super::{
    SlotRoots, alloc_string_payload_rooted, initialize_length, string_payload_size, ws_as_bytes,
};
use willow_abi::string_layout::DATA_OFFSET;

// String creation validates UTF-8. These operations preserve that invariant;
// rescanning the entire receiver for each substring would make splitting quadratic.
// No borrowed view may survive a GC allocation: retain offsets, root the payload,
// then derive fresh addresses after allocation instead.
unsafe fn text<'a>(value: *const u8) -> &'a str {
    if value.is_null() {
        return "";
    }
    // SAFETY: The operand is null or a live string, and no safepoint intervenes in this borrow.
    let (bytes, len) = unsafe { ws_as_bytes(value) };
    // SAFETY: String creation validates UTF-8. The live payload covers the recorded byte length
    // and no allocation intervenes.
    unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(bytes, len)) }
}

fn fail(message: &str) -> *mut u8 {
    crate::panic_context::raise_language_message(message);
    std::ptr::null_mut()
}

// Instrument the actual copy operations, rather than expected output lengths.
#[inline]
unsafe fn copy_bytes(source: *const u8, destination: *mut u8, len: usize) {
    crate::observation::record(crate::observation::Event::CopiedBytes, len);
    // SAFETY: The checked range supplies len bytes and the distinct fresh destination has room for
    // that range.
    unsafe { std::ptr::copy_nonoverlapping(source, destination, len) };
}

/// Copy a checked range, rooting and reloading the receiver on a TLAB miss.
///
/// # Safety
/// The range must be ordered and on UTF-8 boundaries of a live WillowString.
/// A null receiver permits only the empty range at offset zero.
unsafe fn copy_range(value: *mut u8, start: usize, end: usize) -> *mut u8 {
    let size = range_payload_size(start, end);
    let mut roots = [value];
    let result = alloc_string_payload_rooted(size, &mut roots);
    // SAFETY: The allocation is sized for this range. For a nonempty range the
    // reloaded source covers its bytes. An empty range permits a null source.
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
/// `start <= end`. A non-null `result` must be a fresh, word-aligned payload
/// with room for the header, `end - start` bytes and the NUL. For a nonempty
/// range, `source` must be a live String covering the range on UTF-8 boundaries.
/// For an empty range, `source` may be null because no source address is derived.
unsafe fn fill_range(result: *mut u8, source: *mut u8, start: usize, end: usize) {
    if result.is_null() {
        return;
    }
    let len = end - start;
    // SAFETY: The fresh result covers the length and NUL. Only the nonempty
    // branch addresses source bytes, which fit the distinct destination.
    unsafe {
        initialize_length(result, len);
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
///
/// # Safety
/// Each non-null string operand must be a current, live WillowString in this
/// runtime, with its aligned length word and immutable UTF-8 payload intact.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_string_substring(value: *mut u8, start: i64, end: i64) -> *mut u8 {
    // SAFETY: The operand is null or a live string, and no safepoint intervenes in this borrow.
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
    // SAFETY: The offsets bound a UTF-8 range in this receiver, or an empty range on null.
    unsafe { copy_range(value, start as usize, end as usize) }
}

/// Read one UTF-8 byte in constant time; invalid offsets raise a language panic.
/// Successful reads allocate nothing and never scan or decode the receiver.
///
/// # Safety
/// Each non-null string operand must be a current, live WillowString in this
/// runtime, with its aligned length word and immutable UTF-8 payload intact.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_string_byte_at(value: *const u8, index: i64) -> i64 {
    // SAFETY: The operand is null or a live string, and no safepoint intervenes in this borrow.
    let (bytes, len) = unsafe { ws_as_bytes(value) };
    if index < 0 || index as usize >= len {
        fail(&format!(
            "String.byte_at requires an in-bounds byte offset (index={index}, len={len})"
        ));
        return 0;
    }
    crate::observation::record(crate::observation::Event::ByteReads, 1);
    // SAFETY: The preceding signed bounds check puts index strictly inside bytes.
    unsafe { i64::from(*bytes.add(index as usize)) }
}

#[cfg(test)]
const BYTE_READS: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::ByteReads);

/// First byte offset, or -1 when absent. Empty needles match at zero.
///
/// # Safety
/// Each non-null string operand must be a current, live WillowString in this
/// runtime, with its aligned length word and immutable UTF-8 payload intact.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_string_find(value: *const u8, needle: *const u8) -> i64 {
    // SAFETY: The operand is null or a live string, and no safepoint intervenes in this borrow.
    unsafe {
        text(value)
            .find(text(needle))
            .map_or(-1, |index| index as i64)
    }
}

/// # Safety
/// Each non-null string operand must be a current, live WillowString in this
/// runtime, with its aligned length word and immutable UTF-8 payload intact.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_string_contains(value: *const u8, needle: *const u8) -> i64 {
    // SAFETY: The entry contract supplies live string operands or null.
    i64::from(unsafe { willow_string_find(value, needle) } >= 0)
}

/// # Safety
/// Each non-null string operand must be a current, live WillowString in this
/// runtime, with its aligned length word and immutable UTF-8 payload intact.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_string_starts_with(value: *const u8, prefix: *const u8) -> i64 {
    // SAFETY: These operands are live UTF-8 string payloads or the null empty-string sentinel. The
    // borrow ends before allocation.
    unsafe { i64::from(text(value).starts_with(text(prefix))) }
}

/// # Safety
/// Each non-null string operand must be a current, live WillowString in this
/// runtime, with its aligned length word and immutable UTF-8 payload intact.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_string_ends_with(value: *const u8, suffix: *const u8) -> i64 {
    // SAFETY: These operands are live UTF-8 string payloads or the null empty-string sentinel. The
    // borrow ends before allocation.
    unsafe { i64::from(text(value).ends_with(text(suffix))) }
}

/// Replace literal, non-overlapping matches. Empty needles match at every
/// Unicode scalar boundary, including both ends. Two search passes, one exact
/// allocation; only lengths survive the allocation and all inputs are rooted.
///
/// # Safety
/// Each non-null string operand must be a current, live WillowString in this
/// runtime, with its aligned length word and immutable UTF-8 payload intact.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_string_replace(
    value: *mut u8,
    needle: *mut u8,
    replacement: *mut u8,
) -> *mut u8 {
    // SAFETY: The operand is null or a live string, and no safepoint intervenes in this borrow.
    let source = unsafe { text(value) };
    // SAFETY: The operand is null or a live string, and no safepoint intervenes in this borrow.
    let pattern = unsafe { text(needle) };
    // SAFETY: The operand is null or a live string, and no safepoint intervenes in this borrow.
    let replacement_len = unsafe { text(replacement).len() };
    let count = source.match_indices(pattern).count();
    let Some(total) = count
        .checked_mul(replacement_len)
        .and_then(|added| (source.len() - count * pattern.len()).checked_add(added))
    else {
        return fail("String.replace size overflow");
    };
    let Some(size) = string_payload_size(total) else {
        return fail("String.replace size overflow");
    };
    let mut roots = [value, needle, replacement];
    let result = alloc_string_payload_rooted(size as usize, &mut roots);
    if result.is_null() {
        return result;
    }
    // SAFETY: The checked size reserves a word-aligned length field, the computed byte count and
    // one NUL. Sources are reloaded after allocation.
    unsafe {
        let source = text(roots[0]);
        let pattern = text(roots[1]);
        let replacement = text(roots[2]);
        initialize_length(result, total);
        let mut previous = 0;
        let mut offset = 0;
        for (index, matched) in source.match_indices(pattern) {
            let length = index - previous;
            copy_bytes(
                source.as_ptr().add(previous),
                result.add(DATA_OFFSET + offset),
                length,
            );
            offset += length;
            copy_bytes(
                replacement.as_ptr(),
                result.add(DATA_OFFSET + offset),
                replacement.len(),
            );
            offset += replacement.len();
            previous = index + matched.len();
        }
        copy_bytes(
            source.as_ptr().add(previous),
            result.add(DATA_OFFSET + offset),
            source.len() - previous,
        );
        *result.add(DATA_OFFSET + total) = 0;
    }
    result
}

/// Remove Unicode White_Space from the start only.
///
/// # Safety
/// Each non-null string operand must be a current, live WillowString in this
/// runtime, with its aligned length word and immutable UTF-8 payload intact.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_string_trim_start(value: *mut u8) -> *mut u8 {
    // SAFETY: The operand is null or a live string, and no safepoint intervenes in this borrow.
    let s = unsafe { text(value) };
    let trimmed = s.trim_start();
    let start = trimmed.as_ptr() as usize - s.as_ptr() as usize;
    // SAFETY: The offsets bound a UTF-8 range in this receiver, or an empty range on null.
    unsafe { copy_range(value, start, start + trimmed.len()) }
}

/// Remove Unicode White_Space from the end only.
///
/// # Safety
/// Each non-null string operand must be a current, live WillowString in this
/// runtime, with its aligned length word and immutable UTF-8 payload intact.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_string_trim_end(value: *mut u8) -> *mut u8 {
    // SAFETY: The operand is null or a live string, and no safepoint intervenes in this borrow.
    let s = unsafe { text(value) };
    let trimmed = s.trim_end();
    let start = trimmed.as_ptr() as usize - s.as_ptr() as usize;
    // SAFETY: The offsets bound a UTF-8 range in this receiver, or an empty range on null.
    unsafe { copy_range(value, start, start + trimmed.len()) }
}

/// ASCII-only case conversion; non-ASCII UTF-8 bytes remain unchanged.
///
/// # Safety
/// Each non-null string operand must be a current, live WillowString in this
/// runtime, with its aligned length word and immutable UTF-8 payload intact.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_string_to_lower(value: *mut u8) -> *mut u8 {
    // SAFETY: The operand is null or a live string, and no safepoint intervenes in this borrow.
    let len = unsafe { ws_as_bytes(value).1 };
    // SAFETY: The offsets bound a UTF-8 range in this receiver, or an empty range on null.
    let result = unsafe { copy_range(value, 0, len) };
    if !result.is_null() {
        // SAFETY: The new result owns len initialized bytes after its header and is exclusively
        // writable here.
        unsafe { std::slice::from_raw_parts_mut(result.add(DATA_OFFSET), len) }
            .make_ascii_lowercase();
    }
    result
}

/// ASCII-only case conversion; non-ASCII UTF-8 bytes remain unchanged.
///
/// # Safety
/// Each non-null string operand must be a current, live WillowString in this
/// runtime, with its aligned length word and immutable UTF-8 payload intact.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_string_to_upper(value: *mut u8) -> *mut u8 {
    // SAFETY: The operand is null or a live string, and no safepoint intervenes in this borrow.
    let len = unsafe { ws_as_bytes(value).1 };
    // SAFETY: The offsets bound a UTF-8 range in this receiver, or an empty range on null.
    let result = unsafe { copy_range(value, 0, len) };
    if !result.is_null() {
        // SAFETY: The new result owns len initialized bytes after its header and is exclusively
        // writable here.
        unsafe { std::slice::from_raw_parts_mut(result.add(DATA_OFFSET), len) }
            .make_ascii_uppercase();
    }
    result
}

/// Remove Unicode White_Space from both ends; interior whitespace is preserved.
///
/// # Safety
/// Each non-null string operand must be a current, live WillowString in this
/// runtime, with its aligned length word and immutable UTF-8 payload intact.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_string_trim(value: *mut u8) -> *mut u8 {
    // SAFETY: The operand is null or a live string, and no safepoint intervenes in this borrow.
    let s = unsafe { text(value) };
    let trimmed = s.trim();
    let start = trimmed.as_ptr() as usize - s.as_ptr() as usize;
    // SAFETY: The offsets bound a UTF-8 range in this receiver, or an empty range on null.
    unsafe { copy_range(value, start, start + trimmed.len()) }
}

/// Repeat whole strings; negative counts or payload-size overflow raise a panic.
///
/// # Safety
/// Each non-null string operand must be a current, live WillowString in this
/// runtime, with its aligned length word and immutable UTF-8 payload intact.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_string_repeat(value: *mut u8, count: i64) -> *mut u8 {
    if count < 0 {
        return fail("String.repeat requires a nonnegative count");
    }
    // SAFETY: The operand is null or a live string, and no safepoint intervenes in this borrow.
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
        // SAFETY: The result reserves count * len bytes plus header and NUL. Each doubling copies
        // at most the initialized prefix to a disjoint suffix.
        unsafe {
            initialize_length(result, total);
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
///
/// # Safety
/// A non-null array must be a live Array<String> in this runtime with stable
/// length and elements throughout the call. Its elements must be null or live
/// immutable WillowStrings. Exclude competing accesses to the handle during the join.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_string_concat_array(array: *mut u8) -> *mut u8 {
    // SAFETY: The caller supplies a stable string array; null represents the empty separator.
    unsafe { willow_array_join(array, std::ptr::null_mut()) }
}

/// # Safety
/// Each non-null string operand must be a current, live WillowString in this
/// runtime, with its aligned length word and immutable UTF-8 payload intact.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_string_debug(value: *mut u8) -> *mut u8 {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let text = unsafe { crate::string::willow_string_as_str(value) };
    crate::string::alloc_native_string(&format!("{text:?}"))
}

/// # Safety
/// A non-null array must be a live Array<String> in this runtime. Its length
/// and elements must remain unchanged during both passes. Each element and
/// the separator must be null or a live immutable WillowString. Exclude competing
/// accesses to the handle while element reads update its exposure flags.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_array_join(array: *mut u8, separator: *mut u8) -> *mut u8 {
    if array.is_null() {
        return fail("cannot join a null array");
    }
    // SAFETY: The join caller supplies a stable Array<String>; element reads have exclusive
    // access to its handle.
    let count = unsafe { crate::array::array_len(array) };
    // SAFETY: The operand is null or a live string, and no safepoint intervenes in this borrow.
    let separator_len = unsafe { ws_as_bytes(separator).1 };
    let Some(mut total) = separator_len.checked_mul((count as usize).saturating_sub(1)) else {
        return fail("Array.join size overflow");
    };
    for index in 0..count {
        // SAFETY: The join caller supplies a stable Array<String>; element reads have
        // exclusive access to its handle.
        let part = unsafe { join_element(array, index) };
        // SAFETY: The operand is null or a live string, and no safepoint intervenes in this
        // borrow.
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
        // SAFETY: The checked sum covers each part and separator plus a trailing NUL. Roots were
        // reloaded after allocating the destination.
        unsafe {
            initialize_length(result, total);
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

/// # Safety
/// Supply the current string array with exclusive access to its handle flags.
unsafe fn join_element(array: *mut u8, index: i64) -> *mut u8 {
    crate::observation::record(crate::observation::Event::JoinVisits, 1);
    // SAFETY: The join caller supplies a stable Array<String>; element reads have exclusive
    // access to its handle.
    (unsafe { crate::array::willow_array_get(array, index) }) as *mut u8
}

/// Literal, non-overlapping separator. Preserve empty fields. An empty separator
/// yields leading/trailing empty fields with Unicode scalar values between them.
///
/// # Safety
/// Each non-null string operand must be a current, live WillowString in this
/// runtime, with its aligned length word and immutable UTF-8 payload intact.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_string_split(value: *mut u8, separator: *const u8) -> *mut u8 {
    // One search pass. Only integer ranges survive GC; the separator is no
    // longer needed after this pass. No per-field rescanning or array growth.
    let ranges: Vec<(usize, usize)> = {
        // SAFETY: The operand is null or a live string, and no safepoint intervenes in this
        // borrow.
        let s = unsafe { text(value) };
        let base = s.as_ptr() as usize;
        // SAFETY: The operand is null or a live string, and no safepoint intervenes in this
        // borrow.
        s.split(unsafe { text(separator) })
            .map(|part| {
                let start = part.as_ptr() as usize - base;
                (start, start + part.len())
            })
            .collect()
    };
    // Root the receiver and result array in shadow-stack slots for the whole
    // call: TLAB hits reach no GC point, and a miss may move both referents.
    let slots = [Cell::new(value), Cell::new(std::ptr::null_mut())];
    let [source, array] = &slots;
    // SAFETY: The receiver is a current string or null. The result cell starts null
    // and receives only the newly allocated array while registered.
    let _roots = unsafe { SlotRoots::push(&slots) };
    // SAFETY: Flag 1 selects full-width reference slots.
    let result = unsafe { crate::array::willow_array_new(ranges.len() as i64, 1) };
    if result.is_null() {
        return result;
    }
    // Cell::set and Cell::get access the same storage registered via Cell::as_ptr.
    array.set(result);
    for (index, (start, end)) in ranges.into_iter().enumerate() {
        let part = alloc_string_payload_rooted(range_payload_size(start, end), &mut []);
        if part.is_null() {
            return part;
        }
        // SAFETY: split produced this UTF-8 range and part is sized for it. The
        // reloaded receiver covers every nonempty range. A null receiver yields only empty ranges.
        unsafe { fill_range(part, source.get(), start, end) };
        // A valid in-bounds store has no allocation/safepoint; array_set applies
        // the GC write barrier, and the rooted array retains every earlier part.
        // SAFETY: The slot supplies the relocated reference array, and part is the fresh
        // string for this in-bounds field.
        unsafe { crate::array::willow_array_set(array.get(), index as i64, part as i64) };
    }
    array.get()
}

#[cfg(test)]
const JOIN_VISITS: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::JoinVisits);
#[cfg(test)]
const COPIED_BYTES: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::CopiedBytes);

/// Compiler-provided scalar kind: zero is char; integer kinds carry bit width
/// in the low byte and signedness in bit 8.
fn scalar_integer_layout(kind: i64) -> Option<(u32, bool)> {
    let bits = (kind & 255) as u32;
    (matches!(bits, 8 | 16 | 32 | 64) && kind & !511 == 0).then_some((bits, kind & 256 != 0))
}

/// Format a validated scalar without allocating an intermediate Rust String.
/// Invalid raw ABI kinds or Unicode scalar values return null.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_scalar_to_string(value: i64, kind: i64) -> *mut u8 {
    if kind == 0 {
        let Some(value) = u32::try_from(value).ok().and_then(char::from_u32) else {
            return std::ptr::null_mut();
        };
        let mut bytes = [0; 4];
        return super::willow_string_from_str(value.encode_utf8(&mut bytes));
    }
    let Some((bits, signed)) = scalar_integer_layout(kind) else {
        return std::ptr::null_mut();
    };
    if signed {
        // Values arriving from native-width operands are sign-extended.
        return super::i64_to_string(value);
    }
    let mut value = value as u64;
    if bits < 64 {
        value &= (1_u64 << bits) - 1;
    }
    let mut bytes = [0_u8; 20];
    let mut start = bytes.len();
    loop {
        start -= 1;
        bytes[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    // Only ASCII digits were written into this initialized suffix.
    // SAFETY: The formatter populates this suffix with ASCII digits and an optional ASCII sign.
    super::willow_string_from_str(unsafe { std::str::from_utf8_unchecked(&bytes[start..]) })
}

fn parse_scalar_bits(value: &str, kind: i64) -> Option<u64> {
    if kind == 0 {
        let mut chars = value.chars();
        let first = chars.next()?;
        return chars.next().is_none().then_some(first as u64);
    }
    let (bits, signed) = scalar_integer_layout(kind)?;
    if signed {
        let parsed = value.parse::<i128>().ok()?;
        let bound = 1_i128 << (bits - 1);
        (-bound..bound).contains(&parsed).then_some(parsed as u64)
    } else {
        let parsed = value.parse::<u64>().ok()?;
        (bits == 64 || parsed < (1_u64 << bits)).then_some(parsed)
    }
}

/// Parse a complete decimal integer or exactly one Unicode scalar. Parsing
/// borrows the String without allocating or reaching a GC point. Rust's decimal
/// parser permits a leading plus but rejects whitespace and separators.
///
/// # Safety
/// Each non-null string operand must be a current, live WillowString in this
/// runtime, with its aligned length word and immutable UTF-8 payload intact.
/// A non-null output must be an aligned, exclusively writable u64 slot disjoint from the string.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_scalar_parse(value: *mut u8, kind: i64, output: *mut u64) -> u8 {
    if output.is_null() {
        return 0;
    }
    // SAFETY: The operand is null or a live string, and no safepoint intervenes in this borrow.
    let parsed = parse_scalar_bits(unsafe { text(value) }, kind);
    // SAFETY: The caller supplies a writable i64 output slot and the null case was rejected.
    unsafe { *output = parsed.unwrap_or(0) };
    u8::from(parsed.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::GcRootHandle;
    use crate::string::{willow_string_as_str, willow_string_from_str};

    fn scalar_parse_case(source: &str, kind: i64, expected: Option<u64>) {
        let string = willow_string_from_str(source);
        let allocated = crate::gc::willow_gc_total_allocated_bytes();
        let mut output = 123;
        assert_eq!(
            // SAFETY: The input is null or freshly allocated. The output is null or an exclusive
            // local u64.
            unsafe { willow_scalar_parse(string, kind, &mut output) },
            u8::from(expected.is_some()),
            "source={source:?} kind={kind}"
        );
        assert_eq!(
            output,
            expected.unwrap_or(0),
            "source={source:?} kind={kind}"
        );
        assert_eq!(crate::gc::willow_gc_total_allocated_bytes(), allocated);
    }

    #[test]
    fn scalar_string_integer_endpoints() {
        let _guard = crate::gc::isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { crate::gc::willow_gc_init() };
        for bits in [8, 16, 32, 64] {
            let signed_kind = bits | 256;
            let signed_bound = 1_i128 << (bits - 1);
            let unsigned_bound = 1_u128 << bits;
            for value in [-signed_bound, -1, 0, signed_bound - 1] {
                let expected = value.to_string();
                scalar_parse_case(&expected, signed_kind, Some(value as u64));
                let string = willow_scalar_to_string(value as i64, signed_kind);
                // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this
                // borrow.
                assert_eq!(unsafe { willow_string_as_str(string) }, expected);
            }
            for value in [-signed_bound - 1, signed_bound] {
                scalar_parse_case(&value.to_string(), signed_kind, None);
            }
            for value in [0, 1, unsigned_bound - 1] {
                let expected = value.to_string();
                scalar_parse_case(&expected, bits, Some(value as u64));
                let string = willow_scalar_to_string(value as i64, bits);
                // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this
                // borrow.
                assert_eq!(unsafe { willow_string_as_str(string) }, expected);
            }
            scalar_parse_case(&unsigned_bound.to_string(), bits, None);
            scalar_parse_case("-1", bits, None);
        }
    }

    #[test]
    fn scalar_string_rejects_malformed_decimal_without_allocating() {
        let _guard = crate::gc::isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { crate::gc::willow_gc_init() };
        for kind in [8, 16, 32, 64, 264, 272, 288, 320] {
            for source in [
                "",
                " ",
                " 1",
                "1 ",
                "1\n",
                "1_0",
                "1.0",
                "0x10",
                "--1",
                "+",
                "-",
                "1\0",
                "１",
                "18446744073709551616000000000000000000000000000000000000",
            ] {
                scalar_parse_case(source, kind, None);
            }
            scalar_parse_case("+1", kind, Some(1));
            scalar_parse_case("0001", kind, Some(1));
        }
        for kind in [-1, 1, 65, 128, 512, 520] {
            scalar_parse_case("1", kind, None);
            assert!(willow_scalar_to_string(1, kind).is_null());
        }
        let mut output = 99;
        assert_eq!(
            // SAFETY: The input is null or freshly allocated. The output is null or an exclusive
            // local u64.
            unsafe { willow_scalar_parse(std::ptr::null_mut(), 64, &mut output) },
            0
        );
        assert_eq!(output, 0);
        assert_eq!(
            // SAFETY: The input is null or freshly allocated. The output is null or an exclusive
            // local u64.
            unsafe { willow_scalar_parse(std::ptr::null_mut(), 64, std::ptr::null_mut()) },
            0
        );
    }

    #[test]
    fn scalar_string_unicode_roundtrip_and_validation() {
        let _guard = crate::gc::isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { crate::gc::willow_gc_init() };
        for scalar in [
            '\0',
            'a',
            'é',
            '日',
            '\u{D7FF}',
            '\u{E000}',
            '🌲',
            '\u{10FFFF}',
        ] {
            let expected = scalar.to_string();
            scalar_parse_case(&expected, 0, Some(scalar as u64));
            let string = willow_scalar_to_string(scalar as i64, 0);
            // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
            assert_eq!(unsafe { willow_string_as_str(string) }, expected);
        }
        for source in ["", "ab", "é日", "e\u{301}", "\0\0"] {
            scalar_parse_case(source, 0, None);
        }
        for value in [-1, 0xD800, 0xDFFF, 0x110000, 0x1_0000_0000] {
            assert!(willow_scalar_to_string(value, 0).is_null());
        }
    }

    #[test]
    fn string_transforms_copy_counts_and_aliases() {
        let _guard = crate::gc::isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { crate::gc::willow_gc_init() };
        for count in [1, 8, 64, 512, 4096] {
            let input = "aé\0".repeat(count);
            let value = GcRootHandle::new(willow_string_from_str(&input));
            for pattern in ["a", "", "absent", "aé\0", "é\0aé"] {
                let needle = GcRootHandle::new(willow_string_from_str(pattern));
                for replacement in ["", "Z", "日本"] {
                    let replacement_ptr = GcRootHandle::new(willow_string_from_str(replacement));
                    COPIED_BYTES.set(0);
                    let result =
                        // SAFETY: The handles supply current string addresses immediately before
                        // this call.
                        unsafe { willow_string_replace(value.load(), needle.load(), replacement_ptr.load()) };
                    let expected = input.replace(pattern, replacement);
                    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this
                    // borrow.
                    assert_eq!(unsafe { willow_string_as_str(result) }, expected);
                    assert_eq!(COPIED_BYTES.get(), expected.len());
                    // SAFETY: String allocation reserves a trailing NUL at this byte offset after
                    // its contents.
                    assert_eq!(unsafe { *result.add(DATA_OFFSET + expected.len()) }, 0);
                }
            }
            COPIED_BYTES.set(0);
            // SAFETY: The handles supply current string addresses immediately before this call.
            let result = unsafe { willow_string_replace(value.load(), value.load(), value.load()) };
            // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
            assert_eq!(unsafe { willow_string_as_str(result) }, input);
            assert_eq!(COPIED_BYTES.get(), input.len());
            for transform in [
                willow_string_trim_start,
                willow_string_trim_end,
                willow_string_to_lower,
                willow_string_to_upper,
            ] {
                COPIED_BYTES.set(0);
                // SAFETY: The handles supply current string addresses immediately before this call.
                let result = unsafe { transform(value.load()) };
                // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this
                // borrow.
                assert_eq!(unsafe { willow_string_as_str(result) }.len(), input.len());
                assert_eq!(COPIED_BYTES.get(), input.len());
            }
            println!(
                "repetitions={count} input_bytes={} exact_output_copy=true",
                input.len()
            );
        }
        let nil = std::ptr::null_mut();
        // SAFETY: Null string operands are supported.
        assert_eq!(unsafe { willow_string_ends_with(nil, nil) }, 1);
        assert_eq!(
            // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
            unsafe { willow_string_as_str(willow_string_replace(nil, nil, nil)) },
            ""
        );
        for transform in [
            willow_string_trim_start,
            willow_string_trim_end,
            willow_string_to_lower,
            willow_string_to_upper,
        ] {
            // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
            assert_eq!(unsafe { willow_string_as_str(transform(nil)) }, "");
        }
    }

    #[test]
    fn string_bytes_measured_constant_access_and_linear_iteration() {
        let _guard = crate::gc::isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { crate::gc::willow_gc_init() };
        for count in [1, 8, 64, 512, 4096] {
            let input = "é\0🌲".repeat(count);
            let string = willow_string_from_str(&input);
            let allocated = crate::gc::willow_gc_total_allocated_bytes();
            BYTE_READS.with(|reads| reads.set(0));
            // Reverse traversal is an adverse shape for prefix-decoding access.
            for (i, expected) in input.bytes().enumerate().rev() {
                assert_eq!(
                    // SAFETY: This loop reads the newly allocated immutable string without a GC
                    // point.
                    unsafe { willow_string_byte_at(string, i as i64) },
                    i64::from(expected)
                );
            }
            assert_eq!(BYTE_READS.with(|reads| reads.get()), input.len());
            for _ in 0..count {
                assert_eq!(
                    // SAFETY: This loop reads the newly allocated immutable string without a GC
                    // point.
                    unsafe { willow_string_byte_at(string, (input.len() - 1) as i64) },
                    178
                );
            }
            assert_eq!(BYTE_READS.with(|reads| reads.get()), input.len() + count);
            assert_eq!(crate::gc::willow_gc_total_allocated_bytes(), allocated);
        }
    }

    #[test]
    fn array_join_measured_linear_work() {
        let _guard = crate::gc::isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { crate::gc::willow_gc_init() };
        for word in ["", "é\0日", "a moderately long field"] {
            for count in [0, 1, 8, 64, 512, 4096, 20000] {
                // SAFETY: Flag 1 allocates full-width reference slots.
                let array = GcRootHandle::new(unsafe { crate::array::willow_array_new(count, 1) });
                // Distinct allocated elements exercise fragmented input storage.
                for index in 0..count {
                    let part = willow_string_from_str(word);
                    // SAFETY: The root supplies the current array; this store uses a value of
                    // its declared element type.
                    unsafe { crate::array::willow_array_set(array.load(), index, part as i64) };
                }
                let separator = GcRootHandle::new(willow_string_from_str("😀\0"));
                COPIED_BYTES.set(0);
                JOIN_VISITS.set(0);
                // SAFETY: The rooted reference array contains only the strings stored above; the
                // separator is rooted.
                let result = unsafe { willow_array_join(array.load(), separator.load()) };
                let expected = vec![word; count as usize].join("😀\0");
                // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this
                // borrow.
                assert_eq!(unsafe { willow_string_as_str(result) }, expected);
                assert_eq!(JOIN_VISITS.get(), count as usize * 2);
                assert_eq!(COPIED_BYTES.get(), expected.len());
                // SAFETY: String allocation reserves a trailing NUL at this byte offset after its
                // contents.
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
        let _guard = crate::gc::isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { crate::gc::willow_gc_init() };
        for count in [1, 8, 64, 512] {
            let source = GcRootHandle::new(willow_string_from_str(&"日,".repeat(count)));
            let separator = willow_string_from_str(",");
            COPIED_BYTES.with(|bytes| bytes.set(0));
            // SAFETY: The registered receiver is current.
            let result = unsafe { willow_string_split(source.load(), separator) };
            assert_eq!(
                // SAFETY: Split returned a populated Array<String>; this traversal reads it
                // before another GC point.
                unsafe { crate::array::array_len(result) },
                count as i64 + 1
            );
            assert_eq!(COPIED_BYTES.with(|bytes| bytes.get()), count * 3);
            for index in 0..count {
                // SAFETY: Split returned a populated Array<String>; this traversal reads it
                // before another GC point.
                let part =
                    unsafe { crate::array::willow_array_get(result, index as i64) } as *const u8;
                // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this
                // borrow.
                assert_eq!(unsafe { willow_string_as_str(part) }, "日");
            }
            COPIED_BYTES.with(|bytes| bytes.set(0));
            // SAFETY: The registered receiver is current.
            let repeated = unsafe { willow_string_repeat(source.load(), 8) };
            assert_eq!(
                // SAFETY: The preceding repeat returned a fresh string and no collection
                // intervened.
                unsafe { super::super::willow_string_len(repeated) },
                (count * 4 * 8) as i64
            );
            assert_eq!(COPIED_BYTES.with(|bytes| bytes.get()), count * 4 * 8);
            // A tiny slice near the end does not scan/copy the large prefix.
            COPIED_BYTES.with(|bytes| bytes.set(0));
            let tail =
                // SAFETY: The registered receiver is current.
                unsafe { willow_string_substring(source.load(), (count * 4 - 4) as i64, (count * 4 - 1) as i64) };
            // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
            assert_eq!(unsafe { willow_string_as_str(tail) }, "日");
            assert_eq!(COPIED_BYTES.with(|bytes| bytes.get()), 3);
            println!(
                "fields={count} split_bytes={} repeat_bytes={} tail_bytes=3",
                count * 3,
                count * 4 * 8
            );
        }
    }

    #[test]
    fn string_methods_embedded_nul_and_null_runtime_inputs() {
        let _guard = crate::gc::isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { crate::gc::willow_gc_init() };
        let source = GcRootHandle::new(willow_string_from_str("a\0日本\0b"));
        let separator = GcRootHandle::new(willow_string_from_str("\0"));
        assert_eq!(
            // SAFETY: Both handles supply the current strings for this nonallocating comparison.
            unsafe { willow_string_find(source.load(), separator.load()) },
            1
        );
        assert_eq!(
            // SAFETY: Both handles supply the current strings for this nonallocating comparison.
            unsafe { willow_string_contains(source.load(), separator.load()) },
            1
        );
        // SAFETY: The registered receiver is current.
        let result = unsafe { willow_string_split(source.load(), separator.load()) };
        // SAFETY: Split returned a populated Array<String>; this traversal reads it before
        // another GC point.
        assert_eq!(unsafe { crate::array::array_len(result) }, 3);
        // SAFETY: Split returned a populated Array<String>; this traversal reads it before
        // another GC point.
        let part = unsafe { crate::array::willow_array_get(result, 1) } as *const u8;
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        assert_eq!(unsafe { willow_string_as_str(part) }, "日本");
        // SAFETY: The registered receiver is current.
        let repeated = unsafe { willow_string_repeat(source.load(), 2) };
        assert_eq!(
            // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
            unsafe { willow_string_as_str(repeated) },
            "a\0日本\0ba\0日本\0b"
        );
        let nil = std::ptr::null_mut();
        // SAFETY: Null string operands are supported.
        assert_eq!(unsafe { willow_string_find(nil, nil) }, 0);
        // SAFETY: Null string operands are supported.
        assert_eq!(unsafe { willow_string_starts_with(nil, nil) }, 1);
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        assert_eq!(unsafe { willow_string_as_str(willow_string_trim(nil)) }, "");
        // SAFETY: Null string operands are supported.
        let empty = unsafe { willow_string_substring(nil, 0, 0) };
        assert!(!empty.is_null());
        // SAFETY: The substring is live and no allocation intervenes in this borrow.
        assert_eq!(unsafe { willow_string_as_str(empty) }, "");
        // SAFETY: The fresh empty substring has a terminator immediately after its length word.
        assert_eq!(unsafe { *empty.add(DATA_OFFSET) }, 0);
    }
}

// Encode one Unicode scalar. Invalid scalars are programmer errors, like an
// invalid substring boundary; JSON validates surrogate pairs before calling.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_string_from_scalar(value: i64) -> *mut u8 {
    let Some(scalar) = u32::try_from(value).ok().and_then(char::from_u32) else {
        return fail("invalid Unicode scalar");
    };
    let mut bytes = [0; 4];
    let text = scalar.encode_utf8(&mut bytes);
    // SAFETY: The local UTF-8 buffer from encode_utf8 covers the complete byte length.
    unsafe { super::willow_string_alloc(text.as_ptr(), text.len() as i64) }
}
