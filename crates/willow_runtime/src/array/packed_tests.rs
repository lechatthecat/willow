//! Packed byte storage: allocation, nontracing GC, growth and failure parity.
use super::*;
use crate::gc::{
    isolated_runtime, willow_gc_collect, willow_gc_init, willow_pop_roots, willow_push_root,
};

const BYTE_FLAGS: i64 = willow_abi::array_layout::storage_flags(1, false);

#[test]
fn packed_array_allocation_scaling_and_growth() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    for count in [1_000_i64, 100_000, 10_000_000] {
        let mut sizes = Vec::new();
        for flags in [BYTE_FLAGS, 0] {
            // SAFETY: The width flags here describe untraced scalar storage.
            let mut array = unsafe { willow_array_new(count, flags) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut array) };
            // Identical contents, including both unsigned-byte extremes.
            for index in 0..count {
                // SAFETY: The live handle owns the word-aligned GC buffer and its descriptor.
                unsafe { store_buffer_slot(handle_buffer(array), index, index & 255, flags) };
            }
            // SAFETY: The live handle owns the word-aligned GC buffer and its descriptor.
            let buffer = unsafe { handle_buffer(array) };
            // SAFETY: The live handle owns the word-aligned GC buffer and its descriptor.
            let layout = unsafe { crate::gc::GcHeader::live_descriptor(buffer) };
            let bytes = (crate::gc::GC_HEADER_SIZE + buffer_bytes(count, flags).unwrap() as usize)
                .next_multiple_of(std::mem::align_of::<crate::gc::GcHeader>());
            assert_eq!(layout.size, bytes as u64);
            assert_eq!((layout.type_id, layout.gc_ref_mask), (0, 0));
            sizes.push(bytes);
            willow_gc_collect();
            for index in [0, count / 2, count - 1] {
                // SAFETY: The fixture retains the scalar array with its declared packed or
                // word layout.
                assert_eq!(unsafe { willow_array_get(array, index) }, index & 255);
            }
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(1) };
            willow_gc_collect();
        }
        assert_eq!(sizes[1] - sizes[0], 7 * count as usize);
        eprintln!(
            "packed-allocation n={count} byte_buffer={} word_buffer={} ratio={:.9} handle_bytes={}",
            sizes[0],
            sizes[1],
            sizes[0] as f64 / sizes[1] as f64,
            crate::gc::GC_HEADER_SIZE + HANDLE_WORDS as usize * WORD_BYTES as usize
        );
    }
    for count in [1_i64, 4, 5, 64, 1_000, 100_000] {
        // SAFETY: The width flags here describe untraced scalar storage.
        let mut array = unsafe { willow_array_new(0, BYTE_FLAGS) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut array) };
        let mut copied = 0;
        let mut growths = 0;
        for index in 0..count {
            // SAFETY: The live handle owns the word-aligned GC buffer and its descriptor.
            if unsafe { handle_word(array, H_CAP) } == index {
                copied += index;
                growths += 1;
            }
            // SAFETY: The fixture retains the scalar array with its declared packed or word
            // layout.
            unsafe { willow_array_push(array, index & 255) };
        }
        assert!(copied < 2 * count);
        for index in 0..count {
            // SAFETY: The fixture retains the scalar array with its declared packed or word
            // layout.
            assert_eq!(unsafe { willow_array_get(array, index) }, index & 255);
        }
        eprintln!("packed-growth n={count} copied_bytes={copied} reallocations={growths}");
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
    }
}

static DROPS: crate::runtime::PerRuntime<std::sync::atomic::AtomicUsize> =
    crate::runtime::PerRuntime::new(|| std::sync::atomic::AtomicUsize::new(0));

unsafe fn record_drop(_: *mut u8) {
    DROPS.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn packed_array_pointer_bytes_are_not_traced_or_relocated() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    DROPS.store(0, Ordering::SeqCst);
    const TYPE_ID: u32 = 0x6b_17_01;
    // SAFETY: This fixture reserves the type ID for the payload consumed by this finalizer
    // and initializes each instance before collection.
    unsafe { crate::gc::willow_register_drop(TYPE_ID, record_drop) };
    // SAFETY: The width flags here describe untraced scalar storage.
    let mut array = unsafe { willow_array_new(8, BYTE_FLAGS) };
    // SAFETY: The initialized slot stays at this address until its registration is popped;
    // collection may rewrite it there.
    unsafe { willow_push_root(&raw mut array) };
    // SAFETY: The requested layout describes the initialized words of this object and its
    // type’s tracing or finalization contract.
    let victim = unsafe { crate::gc::willow_alloc_object(i64::from(TYPE_ID), 8) };
    let bytes = (victim as usize).to_ne_bytes();
    for (index, &byte) in bytes.iter().enumerate() {
        // SAFETY: The fixture retains the scalar array with its declared packed or word
        // layout.
        unsafe { willow_array_set(array, index as i64, i64::from(byte)) };
    }
    // The payload contains an exact, aligned GC pointer. Only its bytes retain
    // it: the drop hook proves that the marker does not follow that edge.
    crate::gc::willow_gc_minor_collect();
    willow_gc_collect();
    assert_eq!(DROPS.load(Ordering::SeqCst), 1);
    // SAFETY: The fixture retains the scalar array with its declared packed or word layout.
    let mut copy = unsafe { willow_array_copy(array) };
    // SAFETY: The initialized slot stays at this address until its registration is popped;
    // collection may rewrite it there.
    unsafe { willow_push_root(&raw mut copy) };
    crate::gc::set_gc_stress_for_test(Some("alloc"));
    for index in 8..129 {
        // SAFETY: The fixture retains the scalar array with its declared packed or word
        // layout.
        unsafe { willow_array_push(array, index) };
    }
    crate::gc::set_gc_stress_for_test(None);
    crate::gc::willow_gc_minor_collect();
    willow_gc_collect();
    assert_eq!(DROPS.load(Ordering::SeqCst), 1);
    for (index, &byte) in bytes.iter().enumerate() {
        assert_eq!(
            // SAFETY: The fixture retains the scalar array with its declared packed or word
            // layout.
            unsafe { willow_array_get(array, index as i64) },
            i64::from(byte)
        );
        assert_eq!(
            // SAFETY: The fixture retains the scalar array with its declared packed or word
            // layout.
            unsafe { willow_array_get(copy, index as i64) },
            i64::from(byte)
        );
    }
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_roots(2) };
}

#[test]
fn packed_array_invalid_sizes_and_bounds_match_words() {
    use crate::panic_context::*;
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    let previous = replace_current_context(Some(std::sync::Arc::new(PanicContext::new(903))));
    let mut messages = Vec::new();
    for flags in [0, BYTE_FLAGS] {
        // SAFETY: The width flags here describe untraced scalar storage.
        let mut array = unsafe { willow_array_new(0, flags) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut array) };
        let depth = crate::gc::willow_root_depth();
        let mut actual = Vec::new();
        for operation in 0..7 {
            match operation {
                0 => {
                    // SAFETY: The fixture retains the scalar array with its declared packed
                    // or word layout.
                    unsafe { willow_array_get(array, 0) };
                }
                1 => {
                    // SAFETY: The fixture retains the scalar array with its declared packed
                    // or word layout.
                    unsafe { willow_array_set(array, -1, 7) };
                }
                2 => {
                    // SAFETY: The fixture retains the scalar array with its declared packed
                    // or word layout.
                    unsafe { willow_array_pop(array) };
                }
                3 => {
                    // SAFETY: The width flags here describe untraced scalar storage.
                    assert!(unsafe { willow_array_new(-1, flags) }.is_null());
                }
                4 => {
                    // SAFETY: The width flags here describe untraced scalar storage.
                    assert!(unsafe { willow_array_new(i64::MAX, flags) }.is_null());
                }
                5 => {
                    // SAFETY: The live array has the storage layout selected by the resize
                    // mask.
                    unsafe { willow_array_resize(array, i64::MAX, -1) };
                }
                _ => {
                    // SAFETY: The live array has the storage layout selected by the resize
                    // mask.
                    unsafe { willow_array_resize(array, -1, -1) };
                }
            }
            assert_eq!(willow_panic_depth(), 1);
            assert_eq!(crate::gc::willow_root_depth(), depth);
            willow_panic_enter_defer();
            let info = willow_panic_recover();
            willow_panic_leave_defer();
            // SAFETY: The retained PanicInfo keeps its initialized fields live through this read.
            actual.push(unsafe { panic_info_message(info) }.to_string());
            willow_panic_release_recovered(info);
            // SAFETY: The fixture retains the scalar array with its declared packed or word
            // layout.
            assert_eq!(unsafe { willow_array_len(array) }, 0);
        }
        messages.push(actual);
        // Failure leaves the handle usable at empty/small/doubling boundaries.
        for index in 0..33 {
            // SAFETY: The fixture retains the scalar array with its declared packed or word
            // layout.
            unsafe { willow_array_push(array, index) };
        }
        for index in (0..33).rev() {
            // SAFETY: The fixture retains the scalar array with its declared packed or word
            // layout.
            assert_eq!(unsafe { willow_array_pop(array) }, index);
        }
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
    }
    assert_eq!(messages[0], messages[1]);
    assert_eq!(
        messages[0][5],
        format!("array capacity too large: {}", i64::MAX)
    );
    replace_current_context(previous);
}

#[test]
fn packed_array_memory_exhaustion_matches_words() {
    const KEY: &str = "WILLOW_TEST_PACKED_ALLOCATION_FAILURE";
    if let Ok(value) = std::env::var(KEY) {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let flags = value.parse().unwrap();
        assert!(matches!(flags, 0 | BYTE_FLAGS));
        // SAFETY: Only the two scalar layouts exercised by the parent process are accepted.
        unsafe { willow_array_new(10_000_000, flags) };
        panic!("PACKED_ALLOCATION_RETURNED");
    }
    for flags in [0, BYTE_FLAGS] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "array::packed_tests::packed_array_memory_exhaustion_matches_words",
                "--nocapture",
            ])
            .env(KEY, flags.to_string())
            .env("WILLOW_GC_MEMORY_LIMIT", "1")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success());
        assert!(stderr.contains("runtime resource exhausted:"), "{stderr}");
        assert!(!stderr.contains("PACKED_ALLOCATION_RETURNED"), "{stderr}");
    }
}
