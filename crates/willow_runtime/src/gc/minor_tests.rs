//! Deterministic tracing-work checks for the minor-collector extraction.

use super::MinorCollector;
use crate::gc::{
    GC_HEADER_SIZE, drop_registry, reset_internal, retire_all_tlabs_locked, runtime,
    runtime_test_guard, tlab_state_for_test, type_registry, verify_old_region_metadata,
    willow_gc_alloc_slow,
};

#[test]
fn minor_tracing_work_scales_with_graph_and_roots() {
    let _guard = runtime_test_guard();
    for shape in ["chain", "fanout", "shared", "duplicate_roots"] {
        for n in [32usize, 128, 512, 1024] {
            reset_internal();
            let mut tls = tlab_state_for_test();
            let mut nodes = Vec::with_capacity(n);
            for _ in 0..n {
                let node = willow_gc_alloc_slow(&mut tls, 1, 0, 16, 0b11);
                assert!(!node.is_null());
                nodes.push(node);
                // Explicit slow-path calls retire the previous chunk. Mix
                // unreachable chunks with survivors to exercise swap_remove
                // cleanup while the root's sparse chunk remains pinned.
                assert!(!willow_gc_alloc_slow(&mut tls, 1, 0, 16, 0).is_null());
            }
            for (i, &node) in nodes.iter().enumerate() {
                let children = match shape {
                    "fanout" => [2 * i + 1, 2 * i + 2],
                    "shared" => [i + 1, i + 1],
                    _ => [i + 1, n],
                };
                for (slot, child) in children.into_iter().enumerate() {
                    // SAFETY: each live fixture payload owns two pointer slots;
                    // all graph construction precedes the sole collection.
                    unsafe {
                        *node.cast::<*mut u8>().add(slot) =
                            nodes.get(child).copied().unwrap_or(std::ptr::null_mut());
                    }
                }
            }

            let root_count = if shape == "duplicate_roots" { n } else { 1 };
            let trace = type_registry().lock().unwrap().clone();
            let drops = drop_registry().lock().unwrap().clone();
            let mut state = runtime().heap.lock().unwrap();
            retire_all_tlabs_locked(&mut state);
            let (_, work) = MinorCollector::new(&mut state, trace, drops)
                .run(vec![nodes[0]; root_count], Default::default());
            // Existing production telemetry counts objects and reference slots,
            // not elapsed time. Duplicate edges/roots must not rescan objects.
            assert_eq!(work.marked_bytes, (n * (GC_HEADER_SIZE + 16)) as u64);
            assert_eq!(work.scanned_bytes, (2 * n * size_of::<usize>()) as u64);
            assert_eq!(
                work.root_scan_bytes,
                (root_count * size_of::<usize>()) as u64
            );
            assert_eq!(state.promoted_objects, n as u64);
            assert_eq!(state.moved_objects, (n - 1) as u64);
            assert_eq!(state.total_frees, n as u64);
            verify_old_region_metadata(&state).unwrap();
            eprintln!(
                "minor shape={shape} n={n} roots={root_count} marked_bytes={} scanned_bytes={} promoted={} moved={} freed={}",
                work.marked_bytes,
                work.scanned_bytes,
                state.promoted_objects,
                state.moved_objects,
                state.total_frees,
            );
            drop(state);
            // Reset while the registered TLS storage is still alive.
            reset_internal();
        }
    }
}
