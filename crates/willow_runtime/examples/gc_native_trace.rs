//! Run: cargo run -p willow_runtime --example gc_native_trace
//! A bounded native snapshot compatible with concurrent old-generation GC.
#![no_main]
use std::sync::atomic::{AtomicPtr, Ordering};
use willow_runtime::{gc, gc_telemetry::native_trace};

unsafe fn stopped(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    slots.push(payload.cast());
}
unsafe fn snapshot(
    payload: *mut u8,
    _: usize,
    limit: usize,
    children: &mut Vec<*mut u8>,
) -> gc::TraceSliceProgress {
    assert!(limit > 0);
    // SAFETY: The registered payload reserves a pointer-aligned GC word and uses atomic access for
    // concurrent tracing.
    children.push(unsafe { AtomicPtr::from_ptr(payload.cast()).load(Ordering::Acquire) });
    gc::TraceSliceProgress::Done
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_user_main() {
    const TYPE: u32 = 0xEC09;
    {
        // SAFETY: The standalone driver exclusively controls this heap between workloads; no
        // mutator or borrowed payload crosses reset.
        unsafe { gc::willow_gc_init() };
        // SAFETY: The fixture controls every instance of this ID and installs the matching
        // stopped tracer and synchronized snapshot capability, if present.
        unsafe {
            gc::willow_register_type_capability(
                TYPE,
                stopped,
                gc::NativeTraceCapability::SliceConcurrentSafe(snapshot),
            )
        };
        // SAFETY: The requested layout describes the initialized words of this object and its
        // type’s tracing or finalization contract.
        let mut parent = unsafe { gc::willow_alloc_object(TYPE as i64, 8) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { gc::willow_push_root(&raw mut parent) };
        let child = gc::willow_alloc(8);
        // SAFETY: The destination describes this reference update. Any asserted header
        // belongs to its live GC payload.
        unsafe { gc::willow_gc_write_barrier(parent, std::ptr::null_mut(), child, 1) };
        // SAFETY: The registered payload reserves a pointer-aligned GC word and uses atomic access
        // for concurrent tracing.
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
            0
        );
        assert_eq!(counts.iter().map(|row| row.stop_only_calls).sum::<u64>(), 0);
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { gc::willow_pop_root() };
        gc::willow_gc_collect();
        gc::willow_gc_unregister_mutator();
        // SAFETY: The fixture controls this ID and its remaining objects and collector epochs
        // across the change.
        unsafe { gc::willow_unregister_type(TYPE) };
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn __willow_static_init() {}
