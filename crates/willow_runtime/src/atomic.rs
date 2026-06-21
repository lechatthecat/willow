//! Atomic integer/boolean primitives (willow-dgwo.3).
//!
//! `AtomicI64` / `AtomicBool` are GC-managed heap cells holding a single
//! sequentially-consistent atomic value. They are allocated with
//! `willow_alloc_typed(8, 0)` — a real GcHeader + an 8-byte payload with no
//! interior GC references — so the collector frees them like any other object
//! and never traces inside. The payload is reinterpreted as a `core::sync`
//! atomic; the 8-byte allocation is 8-aligned, satisfying the atomics' layout.
//!
//! MVP ordering is sequentially consistent (`SeqCst`); explicit memory orders
//! are a future extension.

use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering::SeqCst};

use crate::gc::willow_alloc_typed;

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
    let ptr = willow_alloc_typed(8, 0) as *mut c_void;
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
    let ptr = willow_alloc_typed(8, 0) as *mut c_void;
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
