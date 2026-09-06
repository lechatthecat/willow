//! Run with WILLOW_GC_TRACE=/tmp/willow-gc.ndjson to record paired cycle events.
#![no_main]

use willow_runtime::{
    gc,
    gc_telemetry::{GcRates, snapshot},
};

#[unsafe(no_mangle)]
pub extern "C" fn willow_user_main() {
    gc::willow_gc_init();
    let before = snapshot();
    let mut root = gc::willow_alloc_typed(8, 1);
    gc::willow_push_root(&mut root);
    for _ in 0..10_000 {
        let next = gc::willow_alloc_typed(8, 1);
        // The previous head remains rooted during the allocation.
        unsafe {
            *next.cast::<*mut u8>() = root;
        }
        gc::willow_gc_write_barrier(next, root, 1);
        root = next;
    }
    gc::willow_gc_collect();
    let live = snapshot();
    gc::willow_pop_root();
    gc::willow_gc_collect();
    let after = snapshot();
    let rates = GcRates::between(&before, &after).unwrap();
    println!(
        "allocations={} bytes={} major_cycles={} pause_max_ns={} marked_bytes={} live_after_major={} heap_after_drop={} allocation_bytes_per_second={:.0}",
        after.counters.allocation_count,
        after.counters.allocation_bytes,
        after.major_cycles,
        after.pauses.max_ns,
        after.marked_bytes,
        live.last_major_cycle.heap_after_bytes,
        after.heap.occupied_bytes,
        rates.allocation_bytes_per_second
    );
    assert!(live.last_major_cycle.heap_after_bytes > 0);
    assert_eq!(after.heap.occupied_bytes, 0);
    let mut samples = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let now = std::time::Instant::now();
        std::hint::black_box(snapshot());
        samples.push(now.elapsed().as_nanos());
    }
    samples.sort_unstable();
    println!(
        "snapshot_p50_ns={} snapshot_p99_ns={}",
        samples[5_000], samples[9_900]
    );
}

// Native examples use the runtime entry point in place of generated code.
#[unsafe(no_mangle)]
pub extern "C" fn __willow_static_init() {}
