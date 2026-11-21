//! Stable workload usable with the pre-telemetry runtime for an A/B comparison.
//! cargo run -p willow_runtime --release --example gc_allocation_bench -- 1000000
#![no_main]

use std::time::Instant;
use willow_runtime::gc;
#[unsafe(no_mangle)]
pub extern "C" fn willow_user_main() {
    let count: usize = std::env::args()
        .nth(1)
        .map(|n| n.parse().unwrap())
        .unwrap_or(1_000_000);
    // SAFETY: The standalone driver exclusively controls this heap between workloads; no
    // mutator or borrowed payload crosses reset.
    unsafe { gc::willow_gc_init() };
    let start = Instant::now();
    let mut checksum = 0usize;
    if std::env::args().nth(2).as_deref() == Some("alloc") {
        for _ in 0..count.div_ceil(1024) * 1024 {
            checksum ^= std::hint::black_box(gc::willow_alloc_typed(8, 1)) as usize;
        }
        gc::willow_gc_collect();
    } else {
        // A live chain makes marking real work; half each batch is short-lived.
        for _ in 0..count.div_ceil(1024) {
            let root = std::cell::Cell::new(std::ptr::null_mut());
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { gc::willow_push_root(root.as_ptr()) };
            for i in 0..1024 {
                let value = gc::willow_alloc_typed(8, 1);
                // SAFETY: The GC allocator word-aligns this payload and the fixture reserves the
                // accessed words.
                unsafe {
                    *value.cast::<*mut u8>() = root.get();
                }
                // SAFETY: The destination describes this reference update. Any asserted
                // header belongs to its live GC payload.
                unsafe { gc::willow_gc_write_barrier(value, std::ptr::null_mut(), root.get(), 1) };
                if i % 2 == 0 {
                    root.set(value);
                }
                checksum ^= std::hint::black_box(value) as usize;
            }
            gc::willow_gc_collect();
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { gc::willow_pop_root() };
            gc::willow_gc_collect();
        }
    }
    std::hint::black_box(checksum);
    println!(
        "{{\"allocations\":{},\"elapsed_ns\":{},\"heap_after\":{}}}",
        count.div_ceil(1024) * 1024,
        start.elapsed().as_nanos(),
        gc::willow_gc_allocated_bytes()
    );
}

// Native examples use the runtime entry point in place of generated code.
#[unsafe(no_mangle)]
pub extern "C" fn __willow_static_init() {}
