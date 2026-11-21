// Included only in test bridge crates. Observe the actual input/output Vec
// allocations, with no production counters or allocator changes.
use std::sync::atomic::{AtomicUsize, Ordering};

static INPUT: AtomicUsize = AtomicUsize::new(0);
static OUTPUT: AtomicUsize = AtomicUsize::new(0);
static INPUT_DROPS: AtomicUsize = AtomicUsize::new(0);
static OUTPUT_DROPS: AtomicUsize = AtomicUsize::new(0);

#[unsafe(no_mangle)]
pub extern "C" fn observe_bridge_deallocation(ptr: *mut u8) {
    for (slot, count, message) in [
        (&INPUT, &INPUT_DROPS, b"BYTE_INPUT_RELEASED\n".as_slice()),
        (&OUTPUT, &OUTPUT_DROPS, b"BYTE_OUTPUT_RELEASED\n".as_slice()),
    ] {
        if slot.compare_exchange(ptr as usize, 0, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            count.fetch_add(1, Ordering::SeqCst);
            // A literal write does not allocate. Clear the slot first so
            // unrelated allocator activity cannot be mistaken for this Vec.
            use std::io::Write;
            let _ = std::io::stderr().write_all(message);
        }
    }
}

pub fn transform(bytes: &[u8], fail: bool) -> Vec<u8> {
    assert_eq!(bytes, [0, 128, 255]);
    INPUT.store(bytes.as_ptr() as usize, Ordering::SeqCst);
    unsafe extern "C" { fn willow_gc_collect(); fn willow_gc_minor_collect(); }
    unsafe { willow_gc_minor_collect(); willow_gc_collect(); }
    assert_eq!(bytes, [0, 128, 255]);
    let output = bytes.to_vec();
    OUTPUT.store(output.as_ptr() as usize, Ordering::SeqCst);
    assert!(!fail, "byte lifetime panic");
    output
}

pub fn released() -> bool {
    INPUT_DROPS.load(Ordering::SeqCst) == 1 && OUTPUT_DROPS.load(Ordering::SeqCst) == 1
}
