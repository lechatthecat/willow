//! Atomic integer/boolean primitives (willow-dgwo.3).
//!
//! `AtomicI64` / `AtomicBool` are GC-managed heap cells holding a single
//! sequentially-consistent atomic value. They are allocated with
//! the central layout-aware GC allocator — a real GcHeader + an 8-byte payload with no
//! interior GC references — so the collector frees them like any other object
//! and never traces inside. The payload is reinterpreted as a `core::sync`
//! atomic; the 8-byte allocation is 8-aligned, satisfying the atomics' layout.
//!
//! MVP ordering is sequentially consistent (`SeqCst`); explicit memory orders
//! are a future extension.

use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering::SeqCst};

use crate::gc::{GcObjectKind, willow_alloc_with_layout};
use willow_abi::atomic_layout;

// The allocator aligns headers and payloads together; check the ABI on every
// target at compile time, including Rust's native atomic representation.
const _: () = {
    assert!(std::mem::align_of::<crate::gc::GcHeader>() >= atomic_layout::ALIGNMENT);
    assert!(crate::gc::GC_HEADER_SIZE.is_multiple_of(atomic_layout::ALIGNMENT));
    assert!(std::mem::size_of::<AtomicI64>() == atomic_layout::I64_BYTES);
    assert!(std::mem::size_of::<AtomicBool>() == atomic_layout::BOOL_BYTES);
    assert!(atomic_layout::ALIGNMENT.is_multiple_of(std::mem::align_of::<AtomicI64>()));
    assert!(atomic_layout::ALIGNMENT.is_multiple_of(std::mem::align_of::<AtomicBool>()));
};

#[inline]
unsafe fn as_i64(ptr: *mut c_void) -> &'static AtomicI64 {
    unsafe { &*(ptr.byte_offset(atomic_layout::VALUE_OFFSET as isize) as *const AtomicI64) }
}

#[inline]
unsafe fn as_bool(ptr: *mut c_void) -> &'static AtomicBool {
    unsafe { &*(ptr.byte_offset(atomic_layout::VALUE_OFFSET as isize) as *const AtomicBool) }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_atomic_i64_new(init: i64) -> *mut c_void {
    atomic_i64_new_with_allocator(init, willow_alloc_with_layout)
}

// Keep allocation injectable so the null-result path can be tested without
// exhausting process memory or changing the global GC state.
fn atomic_i64_new_with_allocator(
    init: i64,
    alloc: impl FnOnce(GcObjectKind, u32, i64, u64) -> *mut u8,
) -> *mut c_void {
    let ptr = alloc(GcObjectKind::AtomicCell, 0, atomic_layout::PAYLOAD_BYTES, 0) as *mut c_void;
    if ptr.is_null() {
        return ptr;
    }
    unsafe { as_i64(ptr).store(init, SeqCst) };
    ptr
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_atomic_i64_load(ptr: *mut c_void) -> i64 {
    unsafe { as_i64(ptr).load(SeqCst) }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_atomic_i64_store(ptr: *mut c_void, value: i64) {
    unsafe { as_i64(ptr).store(value, SeqCst) }
}

/// Atomically add `value`, returning the PREVIOUS value (fetch_add).
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_atomic_i64_add(ptr: *mut c_void, value: i64) -> i64 {
    unsafe { as_i64(ptr).fetch_add(value, SeqCst) }
}

/// Atomically subtract `value`, returning the PREVIOUS value (fetch_sub).
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_atomic_i64_sub(ptr: *mut c_void, value: i64) -> i64 {
    unsafe { as_i64(ptr).fetch_sub(value, SeqCst) }
}

/// Atomically replace the value, returning the PREVIOUS value (swap).
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_atomic_i64_swap(ptr: *mut c_void, value: i64) -> i64 {
    unsafe { as_i64(ptr).swap(value, SeqCst) }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_atomic_bool_new(init: u8) -> *mut c_void {
    atomic_bool_new_with_allocator(init, willow_alloc_with_layout)
}

fn atomic_bool_new_with_allocator(
    init: u8,
    alloc: impl FnOnce(GcObjectKind, u32, i64, u64) -> *mut u8,
) -> *mut c_void {
    let ptr = alloc(GcObjectKind::AtomicCell, 0, atomic_layout::PAYLOAD_BYTES, 0) as *mut c_void;
    if ptr.is_null() {
        return ptr;
    }
    unsafe { as_bool(ptr).store(init != 0, SeqCst) };
    ptr
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_atomic_bool_load(ptr: *mut c_void) -> u8 {
    unsafe { as_bool(ptr).load(SeqCst) as u8 }
}

#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_atomic_bool_store(ptr: *mut c_void, value: u8) {
    unsafe { as_bool(ptr).store(value != 0, SeqCst) }
}

/// Atomically replace the value, returning the PREVIOUS value (swap).
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_atomic_bool_swap(ptr: *mut c_void, value: u8) -> u8 {
    unsafe { as_bool(ptr).swap(value != 0, SeqCst) as u8 }
}

/// Strong CAS: replace only on equality, returning 1 on success, 0 on mismatch.
/// Both success and failure are sequentially consistent; never spuriously fails.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_atomic_i64_compare_exchange(
    ptr: *mut c_void,
    expected: i64,
    desired: i64,
) -> u8 {
    unsafe {
        as_i64(ptr)
            .compare_exchange(expected, desired, SeqCst, SeqCst)
            .is_ok() as u8
    }
}

/// Boolean counterpart of `willow_atomic_i64_compare_exchange`.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_atomic_bool_compare_exchange(
    ptr: *mut c_void,
    expected: u8,
    desired: u8,
) -> u8 {
    unsafe {
        as_bool(ptr)
            .compare_exchange(expected != 0, desired != 0, SeqCst, SeqCst)
            .is_ok() as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{runtime_test_guard, willow_gc_init};

    #[test]
    fn atomic_payload_matches_shared_layout() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let integer = willow_atomic_i64_new(-17);
        let boolean = willow_atomic_bool_new(255);
        for pointer in [integer, boolean] {
            assert!(!pointer.is_null());
            assert!((pointer as usize).is_multiple_of(atomic_layout::ALIGNMENT));
        }
        assert_eq!(unsafe { as_i64(integer).load(SeqCst) }, -17);
        assert!(unsafe { as_bool(boolean).load(SeqCst) });
        // Non-compiled ABI callers retain bool normalization for every operand.
        willow_atomic_bool_store(boolean, 2);
        assert_eq!(willow_atomic_bool_swap(boolean, 255), 1);
        assert_eq!(willow_atomic_bool_load(boolean), 1);
    }

    #[test]
    fn compare_exchange_contention_has_exactly_one_winner() {
        // Stack-backed Rust atomics exercise the FFI operation without involving GC.
        let integer = AtomicI64::new(0);
        let boolean = AtomicBool::new(false);
        let winners = AtomicI64::new(0);
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| {
                    let p = &integer as *const AtomicI64 as *mut c_void;
                    winners.fetch_add(willow_atomic_i64_compare_exchange(p, 0, 1) as i64, SeqCst);
                    let p = &boolean as *const AtomicBool as *mut c_void;
                    winners.fetch_add(willow_atomic_bool_compare_exchange(p, 0, 1) as i64, SeqCst);
                });
            }
        });
        assert_eq!(winners.load(SeqCst), 2);
        assert_eq!(integer.load(SeqCst), 1);
        assert!(boolean.load(SeqCst));
    }

    #[test]
    fn compare_exchange_boolean_abi_normalizes_nonzero() {
        let cell = AtomicBool::new(true);
        let p = &cell as *const AtomicBool as *mut c_void;
        assert_eq!(willow_atomic_bool_compare_exchange(p, 255, 0), 1);
        assert_eq!(willow_atomic_bool_compare_exchange(p, 2, 255), 0);
        assert!(!cell.load(SeqCst));
        assert_eq!(willow_atomic_bool_compare_exchange(p, 0, 255), 1);
        assert!(cell.load(SeqCst));
    }

    fn failed_allocation(kind: GcObjectKind, type_id: u32, size: i64, mask: u64) -> *mut u8 {
        assert_eq!(kind, GcObjectKind::AtomicCell);
        assert_eq!((type_id, size, mask), (0, 8, 0));
        std::ptr::null_mut()
    }

    #[test]
    fn i64_new_returns_null_on_allocation_failure() {
        for init in [i64::MIN, 0, i64::MAX] {
            assert!(atomic_i64_new_with_allocator(init, failed_allocation).is_null());
        }
    }

    #[test]
    fn bool_new_returns_null_on_allocation_failure() {
        for init in [0, 1, u8::MAX] {
            assert!(atomic_bool_new_with_allocator(init, failed_allocation).is_null());
        }
    }

    #[test]
    fn failed_constructors_allocate_once_per_call() {
        let mut allocations = 0;
        assert!(
            atomic_i64_new_with_allocator(42, |kind, id, size, mask| {
                allocations += 1;
                failed_allocation(kind, id, size, mask)
            })
            .is_null()
        );
        assert_eq!(allocations, 1);
        assert!(
            atomic_bool_new_with_allocator(255, |kind, id, size, mask| {
                allocations += 1;
                failed_allocation(kind, id, size, mask)
            })
            .is_null()
        );
        assert_eq!(allocations, 2);
    }

    #[test]
    fn i64_new_load_store_add_sub_swap() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let a = willow_atomic_i64_new(10);
        assert_eq!(willow_atomic_i64_load(a), 10);
        willow_atomic_i64_store(a, 5);
        assert_eq!(willow_atomic_i64_load(a), 5);
        assert_eq!(willow_atomic_i64_add(a, 3), 5); // returns previous
        assert_eq!(willow_atomic_i64_load(a), 8);
        assert_eq!(willow_atomic_i64_sub(a, 2), 8);
        assert_eq!(willow_atomic_i64_load(a), 6);
        assert_eq!(willow_atomic_i64_swap(a, 100), 6);
        assert_eq!(willow_atomic_i64_load(a), 100);
    }

    #[test]
    fn bool_new_load_store_swap() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let b = willow_atomic_bool_new(0);
        assert_eq!(willow_atomic_bool_load(b), 0);
        willow_atomic_bool_store(b, 1);
        assert_eq!(willow_atomic_bool_load(b), 1);
        assert_eq!(willow_atomic_bool_swap(b, 0), 1);
        assert_eq!(willow_atomic_bool_load(b), 0);
    }
}
