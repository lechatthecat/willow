use crate::string::willow_string_as_str;

pub fn nil_deref_message(file: &str, line: i32, col: i32, ctx: &str) -> String {
    if ctx.is_empty() {
        format!("runtime panic: nil dereference at {file}:{line}:{col}")
    } else {
        format!("runtime panic: nil dereference at {file}:{line}:{col} -- `{ctx}`")
    }
}

pub fn abort_message(file: &str, line: i32) -> String {
    format!("panic at {file}:{line}")
}

/// # Safety
/// Each non-null string argument must be the current address of a live, word-aligned Willow
/// String, with its length and UTF-8 bytes initialized and stable while read.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_nil_deref(
    file: *const u8,
    line: i32,
    col: i32,
    context: *const u8,
) {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let ctx = unsafe { willow_string_as_str(context) };
    let message = if ctx.is_empty() {
        "nil dereference".to_string()
    } else {
        format!("nil dereference -- `{ctx}`")
    };
    // SAFETY: The optional source file is null or a current Willow string and the panic path
    // roots its string inputs before allocation.
    unsafe {
        crate::panic_context::raise_language_message_at(&message, file, line.into(), col.into())
    };
}

/// Print the active async chain if a task is currently running, so
/// panics inside async fns show the suspended future chain, not just the OS call
/// stack (which the cooperative scheduler flattens).
fn print_async_chain() {
    let text = crate::scheduler::async_chain_text();
    if !text.is_empty() {
        eprintln!("{text}");
    }
}

/// Called by the Willow `panic(message)` builtin.  `message` is a WillowString pointer.
///
/// TOP-LEVEL PANIC POLICY (preemption spec §18, panic and cancellation policy): Willow has no
/// `recover`, so a panic anywhere — any task, any worker thread, any depth —
/// prints the located `runtime panic:` message, the call stack, and the async
/// chain (task id + spawn site), then ABORTS THE WHOLE PROGRAM. The
/// `RuntimeTaskState::Panicked` variant is reserved for a possible future
/// recoverable policy; nothing sets it under the abort policy.
///
/// # Safety
/// Each non-null string argument must be the current address of a live, word-aligned Willow
/// String, with its length and UTF-8 bytes initialized and stable while read.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_panic(message: *const u8) {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let msg = unsafe { willow_string_as_str(message) };
    eprintln!("runtime panic: {msg}");
    crate::reference_debug::print_current_reference_call_context();
    crate::stack_trace::print_current_call_stack();
    print_async_chain();
    std::process::abort();
}

/// Called when `fn main() -> Result<void, E>` returns `Err`. Prints a clean
/// top-level error report to stderr and exits with a non-zero status.
/// `message` is a WillowString pointer for `E = String`, or null for other error
/// types (a generic report is printed).
///
/// # Safety
/// Each non-null string argument must be the current address of a live, word-aligned Willow
/// String, with its length and UTF-8 bytes initialized and stable while read.
///
/// ```compile_fail,E0133
/// willow_runtime::panic::willow_main_fail(std::ptr::null());
/// ```
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_main_fail(message: *const u8) {
    if message.is_null() {
        eprintln!("Error: main returned Err");
    } else {
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        let msg = unsafe { willow_string_as_str(message) };
        eprintln!("Error: {msg}");
    }
    crate::gc_telemetry::flush_observations();
    // Reached with the runtime's threads still running, so it exits the same
    // way a successful `main` does.
    crate::exit::terminate_without_teardown(1);
}

/// Like [`willow_panic`] but with the source location of the `panic(...)` call
/// site (debug builds). Prints `runtime panic: <msg> at <file>:<line>:<col>`
/// then aborts.
///
/// # Safety
/// Each non-null string argument must be the current address of a live, word-aligned Willow
/// String, with its length and UTF-8 bytes initialized and stable while read.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_panic_at(message: *const u8, file: *const u8, line: i32, col: i32) {
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let msg = unsafe { willow_string_as_str(message) };
    // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
    let file = unsafe { willow_string_as_str(file) };
    eprintln!("runtime panic: {msg} at {file}:{line}:{col}");
    crate::reference_debug::print_current_reference_call_context();
    crate::stack_trace::print_current_call_stack();
    print_async_chain();
    std::process::abort();
}

/// Integer division guard failure in either build mode. Low kind byte:
/// 0 = division by zero, 1 = MIN / -1 overflow, 2 = remainder by zero,
/// 3 = MIN % -1 overflow. Bits 8..15 carry a non-i64 signed width;
/// legacy kinds 0..3 remain i64. `file` is a WillowString pointer.
///
/// # Safety
/// Each non-null string argument must be the current address of a live, word-aligned Willow
/// String, with its length and UTF-8 bytes initialized and stable while read.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_int_div_panic(kind: i64, file: *const u8, line: i32, col: i32) {
    // SAFETY: The optional source file is null or a current Willow string and the panic path
    // roots its string inputs before allocation.
    unsafe {
        crate::panic_context::raise_language_message_at(
            &int_div_message(kind),
            file,
            line.into(),
            col.into(),
        )
    };
}

fn int_div_message(kind: i64) -> std::borrow::Cow<'static, str> {
    let (operation, bits, _) = willow_abi::integer_overflow::decode(kind);
    if kind >= 256 && matches!(operation, 1 | 3) {
        return format!(
            "integer overflow: `i{bits}::MIN {} -1`",
            if operation == 1 { "/" } else { "%" },
        )
        .into();
    }
    match operation {
        0 => "division by zero",
        1 => "integer overflow: `i64::MIN / -1`",
        2 => "remainder by zero",
        _ => "integer overflow: `i64::MIN % -1`",
    }
    .into()
}

/// Debug-build signed overflow check failure. `kind`:
/// 0 = `+`, 1 = `-`, 2 = `*`, 3 = unary `-`, 4 = `**`, 5 = `<<` and 6 = `>>`
/// with an amount outside `0..64`. Release builds wrap
/// and never call this. `file` is a WillowString pointer.
///
/// # Safety
/// Each non-null string argument must be the current address of a live, word-aligned Willow
/// String, with its length and UTF-8 bytes initialized and stable while read.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_int_overflow_panic(
    kind: i64,
    file: *const u8,
    line: i32,
    col: i32,
) {
    // SAFETY: The optional source file is null or a current Willow string and the panic path
    // roots its string inputs before allocation.
    unsafe {
        crate::panic_context::raise_language_message_at(
            &int_overflow_message(kind),
            file,
            line.into(),
            col.into(),
        )
    };
}

fn int_overflow_message(kind: i64) -> std::borrow::Cow<'static, str> {
    let (operation, bits, signed) = willow_abi::integer_overflow::decode(kind);
    if kind >= 256 {
        if operation == 3 {
            return format!(
                "integer overflow: negation of {}{bits}",
                if signed { "i" } else { "u" }
            )
            .into();
        }
        if matches!(operation, 5 | 6) {
            return format!(
                "integer overflow: `{}` shift amount outside 0..{bits}",
                if operation == 5 { "<<" } else { ">>" }
            )
            .into();
        }
    }
    match operation {
        0 => "integer overflow: `+`",
        1 => "integer overflow: `-`",
        2 => "integer overflow: `*`",
        3 => "integer overflow: negation of `i64::MIN`",
        4 => "integer overflow: `**`",
        5 => "integer overflow: `<<` shift amount outside 0..64",
        _ => "integer overflow: `>>` shift amount outside 0..64",
    }
    .into()
}

/// # Safety
/// Each non-null string argument must be the current address of a live, word-aligned Willow
/// String, with its length and UTF-8 bytes initialized and stable while read.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_abort(file: *const u8, line: i32) {
    // SAFETY: The caller keeps the optional source String live through formatting.
    let file = unsafe { willow_string_as_str(file) };
    eprintln!("{}", abort_message(file, line));
    crate::reference_debug::print_current_reference_call_context();
    crate::stack_trace::print_current_call_stack();
    print_async_chain();
    std::process::abort();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_width_division_messages_preserve_width_and_legacy_codes() {
        for (bits, division, remainder) in [
            (
                8,
                "integer overflow: `i8::MIN / -1`",
                "integer overflow: `i8::MIN % -1`",
            ),
            (
                16,
                "integer overflow: `i16::MIN / -1`",
                "integer overflow: `i16::MIN % -1`",
            ),
            (
                32,
                "integer overflow: `i32::MIN / -1`",
                "integer overflow: `i32::MIN % -1`",
            ),
        ] {
            assert_eq!(int_div_message(1 | (bits << 8)), division);
            assert_eq!(int_div_message(3 | (bits << 8)), remainder);
            assert_eq!(int_div_message(bits << 8), "division by zero");
            assert_eq!(int_div_message(2 | (bits << 8)), "remainder by zero");
        }
        for (code, message) in [
            "division by zero",
            "integer overflow: `i64::MIN / -1`",
            "remainder by zero",
            "integer overflow: `i64::MIN % -1`",
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(int_div_message(code as i64), message);
        }
    }

    #[test]
    fn scalar_width_contract_unsigned_high_bits() {
        let call =
            |op, a, b| contract_integer(willow_abi::integer_overflow::encode(op, 64, false), a, b);
        assert_eq!(call(0, -1, 0), Some(-1));
        assert_eq!(call(0, i64::MAX, 1), Some(i64::MIN));
        assert_eq!(call(0, -1, 1), None);
        assert_eq!(call(1, -1, i64::MIN), Some(i64::MAX));
        assert_eq!(call(1, 0, -1), None);
        assert_eq!(call(2, i64::MIN, 1), Some(i64::MIN));
        assert_eq!(call(2, i64::MIN, 2), None);
        assert_eq!(call(2, -1, -1), None);
        assert_eq!(call(3, -1, 2), Some(i64::MAX));
        assert_eq!(call(4, -1, 2), Some(1));
        assert_eq!(call(3, -1, 0), None);
        assert_eq!(call(4, -1, 0), None);
        assert_eq!(call(5, -1, 0), Some(1));
        assert_eq!(call(5, -1, 1), Some(-1));
        assert_eq!(call(5, -1, 2), None);
        assert_eq!(call(5, 1, -1), Some(1));
        assert_eq!(call(5, 2, -1), None);
        assert_eq!(call(6, -1, 0), None);
        assert_eq!(call(6, i64::MIN, 0), None);
        assert_eq!(call(6, 0, 0), Some(0));
        assert_eq!(call(7, 1, 63), Some(i64::MIN));
        assert_eq!(call(7, -1, 1), Some(-2));
        assert_eq!(call(8, -1, 1), Some(i64::MAX));
        assert_eq!(call(8, -1, 63), Some(1));
        assert_eq!(call(7, 1, 64), None);
        assert_eq!(call(8, 1, -1), None);
    }

    #[test]
    fn scalar_width_contract_narrow_signed_boundaries() {
        for bits in [8, 16, 32] {
            let min = -(1_i64 << (bits - 1));
            let max = -min - 1;
            let call = |op, a, b| {
                contract_integer(willow_abi::integer_overflow::encode(op, bits, true), a, b)
            };
            assert_eq!(call(0, max, 0), Some(max));
            assert_eq!(call(0, max, 1), None);
            assert_eq!(call(1, min, 1), None);
            assert_eq!(call(2, min, -1), None);
            assert_eq!(call(3, min, -1), None);
            assert_eq!(call(4, min, -1), None);
            assert_eq!(call(3, -3, 2), Some(-1));
            assert_eq!(call(4, -3, 2), Some(-1));
            assert_eq!(call(6, min, 0), None);
            assert_eq!(call(6, max, 0), Some(-max));
            assert_eq!(call(7, max, 1), Some(-2));
            assert_eq!(call(8, min, i64::from(bits - 1)), Some(-1));
            assert_eq!(call(7, 1, i64::from(bits)), None);
            assert_eq!(call(8, 1, -1), None);
        }
        for bits in [8, 16, 32] {
            let max = (1_i64 << bits) - 1;
            let call = |op, a, b| {
                contract_integer(willow_abi::integer_overflow::encode(op, bits, false), a, b)
            };
            assert_eq!(call(0, max, 1), None);
            assert_eq!(call(1, 0, 1), None);
            assert_eq!(call(3, max, 2), Some(max / 2));
            assert_eq!(call(4, max, 2), Some(1));
            assert_eq!(call(7, max, 1), Some(max - 1));
            assert_eq!(call(8, max, i64::from(bits - 1)), Some(1));
        }
        assert_eq!(contract_integer(0, i64::MAX, 1), None);
        assert_eq!(contract_integer(4, i64::MIN, -1), None);
        assert_eq!(contract_integer(8, i64::MIN, 63), Some(-1));
        assert_eq!(contract_integer(7, 1, 64), None);
    }

    #[test]
    fn int_overflow_messages_name_the_operator() {
        let messages: Vec<_> = (0..7).map(int_overflow_message).collect();
        assert_eq!(
            messages,
            [
                "integer overflow: `+`",
                "integer overflow: `-`",
                "integer overflow: `*`",
                "integer overflow: negation of `i64::MIN`",
                "integer overflow: `**`",
                "integer overflow: `<<` shift amount outside 0..64",
                "integer overflow: `>>` shift amount outside 0..64",
            ]
        );
    }
    #[test]
    fn int_overflow_messages_preserve_unsigned_width() {
        use willow_abi::integer_overflow::{decode, encode};
        for operation in 0..7 {
            assert_eq!(decode(encode(operation, 8, false)), (operation, 8, false));
            assert_eq!(decode(encode(operation, 64, true)), (operation, 64, true));
        }
        assert_eq!(
            int_overflow_message(encode(3, 8, false)),
            "integer overflow: negation of u8"
        );
        assert_eq!(
            int_overflow_message(encode(5, 8, false)),
            "integer overflow: `<<` shift amount outside 0..8"
        );
        assert_eq!(
            int_overflow_message(encode(6, 8, false)),
            "integer overflow: `>>` shift amount outside 0..8"
        );
    }

    #[test]
    fn panic_unit_01_nil_deref_message_without_context() {
        let msg = nil_deref_message("main.wi", 3, 4, "");
        assert_eq!(msg, "runtime panic: nil dereference at main.wi:3:4");
    }

    #[test]
    fn panic_unit_02_nil_deref_message_with_context() {
        let msg = nil_deref_message("main.wi", 3, 4, "box.value");
        assert_eq!(
            msg,
            "runtime panic: nil dereference at main.wi:3:4 -- `box.value`"
        );
    }

    #[test]
    fn panic_unit_03_abort_message_includes_source_line() {
        assert_eq!(abort_message("panic.wi", 9), "panic at panic.wi:9");
    }
}

/// Contract arithmetic is checked in every build mode. Operation codes are
/// add, subtract, multiply, divide, remainder, power, negate, left/right shift.
///
/// # Safety
/// Each non-null string argument must be the current address of a live, word-aligned Willow
/// String, with its length and UTF-8 bytes initialized and stable while read.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_contract_i64(
    op: i64,
    a: i64,
    b: i64,
    file: *const u8,
    line: i32,
    col: i32,
) -> i64 {
    let value = contract_integer(op, a, b);
    if let Some(value) = value {
        return value;
    }
    let (_, bits, signed) = willow_abi::integer_overflow::decode(op);
    let message = format!(
        "contract evaluation error: invalid or overflowing {}{bits} arithmetic",
        if signed { "i" } else { "u" }
    );
    // SAFETY: The optional source file is null or a current Willow string and the panic path
    // roots its string inputs before allocation.
    unsafe {
        crate::panic_context::raise_language_message_at(&message, file, line.into(), col.into())
    };
    0
}

fn contract_integer(kind: i64, a: i64, b: i64) -> Option<i64> {
    let (op, bits, signed) = willow_abi::integer_overflow::decode(kind);
    if bits == 64 && signed {
        return contract_i64(op, a, b);
    }
    let decode = |value| {
        if !signed && bits == 64 {
            i128::from(value as u64)
        } else {
            i128::from(value)
        }
    };
    let (a, b) = (decode(a), decode(b));
    let min = if signed { -(1_i128 << (bits - 1)) } else { 0 };
    let max = (1_i128 << (bits - u32::from(signed))) - 1;
    if !(min..=max).contains(&a) || !(min..=max).contains(&b) {
        return None;
    }
    if matches!(op, 7 | 8) && !(0..i128::from(bits)).contains(&b) {
        return None;
    }
    // Signed MIN / -1 and MIN % -1 both fault, including narrow integers.
    if signed && matches!(op, 3 | 4) && a == min && b == -1 {
        return None;
    }
    let value = contract_wide(op, a, b)?;
    if matches!(op, 7 | 8) {
        let shift = 128 - bits;
        return Some(if signed {
            ((value << shift) >> shift) as i64
        } else {
            (value & max) as i64
        });
    }
    (min..=max).contains(&value).then_some(value as i64)
}

/// Common arithmetic for every non-i64 integer layout. i128 covers all valid
/// operands; an intermediate i128 overflow necessarily exceeds the result type.
fn contract_wide(op: i64, a: i128, b: i128) -> Option<i128> {
    match op {
        0 => a.checked_add(b),
        1 => a.checked_sub(b),
        2 => a.checked_mul(b),
        3 => a.checked_div(b),
        4 => a.checked_rem(b),
        6 => a.checked_neg(),
        7 => u32::try_from(b).ok().and_then(|b| a.checked_shl(b)),
        8 => u32::try_from(b).ok().and_then(|b| a.checked_shr(b)),
        5 => {
            let (mut base, mut exponent, mut value) = (a, u64::try_from(b).ok()?, 1_i128);
            while exponent != 0 {
                if exponent & 1 != 0 {
                    value = value.checked_mul(base)?;
                }
                exponent >>= 1;
                if exponent != 0 {
                    base = base.checked_mul(base)?;
                }
            }
            Some(value)
        }
        _ => None,
    }
}

fn contract_i64(op: i64, a: i64, b: i64) -> Option<i64> {
    match op {
        0 => a.checked_add(b),
        1 => a.checked_sub(b),
        2 => a.checked_mul(b),
        3 => a.checked_div(b),
        4 => a.checked_rem(b),
        6 => a.checked_neg(),
        7 => u32::try_from(b).ok().and_then(|b| a.checked_shl(b)),
        8 => u32::try_from(b).ok().and_then(|b| a.checked_shr(b)),
        5 if b >= 0 => {
            let (mut base, mut exponent, mut value) = (a, b as u64, 1i64);
            while exponent != 0 {
                if exponent & 1 != 0 {
                    value = value.checked_mul(base)?;
                }
                exponent >>= 1;
                if exponent != 0 {
                    base = base.checked_mul(base)?;
                }
            }
            Some(value)
        }
        _ => None,
    }
}

/// # Safety
/// Each non-null string argument must be the current address of a live, word-aligned Willow
/// String, with its length and UTF-8 bytes initialized and stable while read.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_contract_check(
    condition: i64,
    post: i64,
    file: *const u8,
    line: i32,
    col: i32,
) {
    if condition == 0 {
        let message = if post == 0 {
            "contract violation: requires"
        } else {
            "contract violation: ensures"
        };
        // SAFETY: The optional source file is null or a current Willow string and the panic
        // path roots its string inputs before allocation.
        unsafe {
            crate::panic_context::raise_language_message_at(message, file, line.into(), col.into())
        };
    }
}
