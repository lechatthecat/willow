//! Run: cargo run -p willow_runtime --example gc_native_trace
//! The same atomic payload, registered first as legacy and then as concurrent.
#![no_main]
use std::sync::atomic::{AtomicPtr, Ordering};
use willow_runtime::{gc, gc_telemetry::native_trace};

unsafe fn stopped(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    slots.push(payload.cast());
}
unsafe fn snapshot(payload: *mut u8, children: &mut Vec<*mut u8>) {
    children.push(unsafe { AtomicPtr::from_ptr(payload.cast()).load(Ordering::Acquire) });
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_user_main() {
    const TYPE: u32 = 0xEC09;
    for concurrent in [false, true] {
        gc::willow_gc_init();
        gc::willow_register_type_capability(
            TYPE,
            stopped,
            if concurrent {
                gc::NativeTraceCapability::ConcurrentSafe(snapshot)
            } else {
                gc::NativeTraceCapability::StopOnly
            },
        );
        let mut parent = gc::willow_alloc_object(TYPE as i64, 8);
        gc::willow_push_root(&mut parent);
        let child = gc::willow_alloc(8);
        gc::willow_gc_write_barrier(parent, std::ptr::null_mut(), child, 1);
        unsafe {
            AtomicPtr::from_ptr(parent.cast()).store(child, Ordering::Release);
        }
        gc::willow_gc_register_mutator();
        gc::willow_gc_collect();
        println!(
            "modes={:?} native_trace={:?}",
            gc::native_trace_modes(),
            native_trace::snapshot()
        );
        let counts = native_trace::snapshot();
        assert_eq!(
            counts.iter().map(|row| row.deferred_objects).sum::<u64>(),
            u64::from(!concurrent)
        );
        assert_eq!(
            counts.iter().map(|row| row.stop_only_calls).sum::<u64>(),
            u64::from(!concurrent)
        );
        gc::willow_pop_root();
        gc::willow_gc_collect();
        gc::willow_gc_unregister_mutator();
        gc::willow_unregister_type(TYPE);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn __willow_static_init() {}
