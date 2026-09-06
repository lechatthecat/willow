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
    gc::willow_gc_init();
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
            let mut root = std::ptr::null_mut();
            gc::willow_push_root(&mut root);
            for i in 0..1024 {
                let value = gc::willow_alloc_typed(8, 1);
                unsafe {
                    *value.cast::<*mut u8>() = root;
                }
                gc::willow_gc_write_barrier(value, root, 1);
                if i % 2 == 0 {
                    root = value;
                }
                checksum ^= std::hint::black_box(value) as usize;
            }
            gc::willow_gc_collect();
            gc::willow_pop_root();
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
