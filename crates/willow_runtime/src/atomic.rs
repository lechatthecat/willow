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

#[inline]
unsafe fn as_i64(ptr: *mut c_void) -> &'static AtomicI64 {
    unsafe { &*(ptr as *const AtomicI64) }
}

#[inline]
unsafe fn as_bool(ptr: *mut c_void) -> &'static AtomicBool {
    unsafe { &*(ptr as *const AtomicBool) }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_atomic_i64_new(init: i64) -> *mut c_void {
    atomic_i64_new_with_allocator(init, willow_alloc_with_layout)
}

// Keep allocation injectable so the null-result path can be tested without
// exhausting process memory or changing the global GC state.
fn atomic_i64_new_with_allocator(
    init: i64,
    alloc: impl FnOnce(GcObjectKind, u32, i64, u64) -> *mut u8,
) -> *mut c_void {
    let ptr = alloc(GcObjectKind::AtomicCell, 0, 8, 0) as *mut c_void;
    if ptr.is_null() {
        return ptr;
    }
    unsafe { as_i64(ptr).store(init, SeqCst) };
    ptr
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_atomic_i64_load(ptr: *mut c_void) -> i64 {
    unsafe { as_i64(ptr).load(SeqCst) }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_atomic_i64_store(ptr: *mut c_void, value: i64) {
    unsafe { as_i64(ptr).store(value, SeqCst) }
}

/// Atomically add `value`, returning the PREVIOUS value (fetch_add).
#[unsafe(no_mangle)]
pub extern "C" fn willow_atomic_i64_add(ptr: *mut c_void, value: i64) -> i64 {
    unsafe { as_i64(ptr).fetch_add(value, SeqCst) }
}

/// Atomically subtract `value`, returning the PREVIOUS value (fetch_sub).
#[unsafe(no_mangle)]
pub extern "C" fn willow_atomic_i64_sub(ptr: *mut c_void, value: i64) -> i64 {
    unsafe { as_i64(ptr).fetch_sub(value, SeqCst) }
}

/// Atomically replace the value, returning the PREVIOUS value (swap).
#[unsafe(no_mangle)]
pub extern "C" fn willow_atomic_i64_swap(ptr: *mut c_void, value: i64) -> i64 {
    unsafe { as_i64(ptr).swap(value, SeqCst) }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_atomic_bool_new(init: u8) -> *mut c_void {
    atomic_bool_new_with_allocator(init, willow_alloc_with_layout)
}

fn atomic_bool_new_with_allocator(
    init: u8,
    alloc: impl FnOnce(GcObjectKind, u32, i64, u64) -> *mut u8,
) -> *mut c_void {
    let ptr = alloc(GcObjectKind::AtomicCell, 0, 8, 0) as *mut c_void;
    if ptr.is_null() {
        return ptr;
    }
    unsafe { as_bool(ptr).store(init != 0, SeqCst) };
    ptr
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_atomic_bool_load(ptr: *mut c_void) -> u8 {
    unsafe { as_bool(ptr).load(SeqCst) as u8 }
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_atomic_bool_store(ptr: *mut c_void, value: u8) {
    unsafe { as_bool(ptr).store(value != 0, SeqCst) }
}

/// Atomically replace the value, returning the PREVIOUS value (swap).
#[unsafe(no_mangle)]
pub extern "C" fn willow_atomic_bool_swap(ptr: *mut c_void, value: u8) -> u8 {
    unsafe { as_bool(ptr).swap(value != 0, SeqCst) as u8 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{runtime_test_guard, willow_gc_init};

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
