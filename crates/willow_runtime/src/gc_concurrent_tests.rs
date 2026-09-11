use super::*;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

static SNAPSHOT_READY: AtomicBool = AtomicBool::new(false);
static MUTATION_DONE: AtomicBool = AtomicBool::new(false);

unsafe fn trace_slot(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    slots.push(payload.cast());
}

unsafe fn snapshot_before_mutation(payload: *mut u8, children: &mut Vec<*mut u8>) {
    children.push(unsafe { load_gc_reference(payload.cast()) });
    SNAPSHOT_READY.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !MUTATION_DONE.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "mutator could not execute during marking"
        );
        std::thread::yield_now();
    }
}

#[test]
fn concurrent_edge_publication_and_new_allocation_survive_remark() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    SNAPSHOT_READY.store(false, Ordering::Relaxed);
    MUTATION_DONE.store(false, Ordering::Relaxed);
    const SNAPSHOT_TYPE: u32 = 0xFC01;
    const LEGACY_TYPE: u32 = 0xFC02;
    willow_register_type(SNAPSHOT_TYPE, trace_slot);
    willow_register_type(LEGACY_TYPE, trace_slot);
    runtime()
        .concurrent_trace_registry
        .lock()
        .unwrap()
        .insert(SNAPSHOT_TYPE, snapshot_before_mutation);
    let mut target = willow_alloc_object(SNAPSHOT_TYPE as i64, 8);
    let mut source = willow_alloc_object(LEGACY_TYPE as i64, 8);
    let child = willow_alloc(8);
    unsafe {
        store_gc_reference(source.cast(), child);
    }
    willow_push_root(&mut target);
    willow_push_root(&mut source);
    willow_gc_register_mutator();
    let ready = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let newborn = Arc::new(AtomicUsize::new(0));
    let worker = {
        let (target, source) = (target as usize, source as usize);
        let (ready, stop, newborn) = (Arc::clone(&ready), Arc::clone(&stop), Arc::clone(&newborn));
        std::thread::spawn(move || {
            willow_gc_register_mutator();
            ready.store(true, Ordering::Release);
            while !SNAPSHOT_READY.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            let target = target as *mut u8;
            let source = source as *mut u8;
            let child = unsafe { load_gc_reference(source.cast()) };
            // The marker already snapshotted target without this edge. The
            // source uses a legacy trace deferred until remark, and its edge
            // disappears now. Only insertion publication can retain child.
            willow_gc_write_barrier(target, child, GcStoreDestination::ObjectField as i64);
            unsafe {
                store_gc_reference(target.cast(), child);
                store_gc_reference(source.cast(), std::ptr::null_mut());
            }
            newborn.store(willow_alloc(8) as usize, Ordering::Release);
            MUTATION_DONE.store(true, Ordering::Release);
            while !stop.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            willow_gc_unregister_mutator();
        })
    };
    while !ready.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    willow_gc_collect();
    stop.store(true, Ordering::Release);
    worker.join().unwrap();
    assert!(MUTATION_DONE.load(Ordering::Acquire));
    assert_eq!(unsafe { load_gc_reference(target.cast()) }, child);
    assert_eq!(willow_gc_allocated_bytes(), 4 * (GC_HEADER_SIZE + 8) as i64);
    assert_ne!(newborn.load(Ordering::Acquire), 0);
    assert!(runtime().heap.lock().unwrap().concurrent_cycle.is_none());
    willow_pop_roots(2);
    willow_gc_unregister_mutator();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn parked_native_stack_roots_survive_major_and_minor_then_resume() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let mut root = willow_alloc(8);
    let depth = gc_thread_root_depth();
    willow_push_root(&mut root);
    let token = unsafe { park_current_roots(depth) };
    assert_eq!(gc_thread_root_depth(), depth);
    willow_gc_collect();
    willow_gc_minor_collect();
    assert_eq!(willow_gc_allocated_bytes(), (GC_HEADER_SIZE + 8) as i64);
    unsafe {
        resume_parked_roots(token);
    }
    assert_eq!(gc_thread_root_depth(), depth + 1);
    willow_pop_roots(1);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn memory_limit_bounds_region_reservations_and_recovers_after_sweep() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    runtime().heap.lock().unwrap().memory_limit_bytes = Some(GC_OLD_REGION_SIZE);
    let mut root = willow_alloc(8);
    assert!(!root.is_null());
    willow_push_root(&mut root);
    // One ordinary region consumes the budget. A large-object region cannot
    // exceed it, and failed reservations must not increment heap accounting.
    let before = willow_gc_allocated_bytes();
    assert!(
        allocate_old_region_object_locked(
            &mut runtime().heap.lock().unwrap(),
            0,
            0,
            GC_OLD_REGION_SIZE,
            0,
            true
        )
        .is_none()
    );
    assert_eq!(willow_gc_allocated_bytes(), before);
    assert!(can_reserve(&runtime().heap.lock().unwrap(), 0));
    willow_pop_roots(1);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    assert!(!willow_alloc(8).is_null());
    reset_internal_for_test();
}

static ASSIST_READY: AtomicBool = AtomicBool::new(false);
static ASSIST_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static ASSIST_SAW_REMARK: AtomicBool = AtomicBool::new(false);

unsafe fn hold_marker_for_assistant(_: *mut u8, _: &mut Vec<*mut u8>) {
    ASSIST_READY.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ASSIST_IN_FLIGHT.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "allocation assist never started");
        std::thread::yield_now();
    }
}

unsafe fn assist_overlaps_remark(_: *mut u8, _: &mut Vec<*mut u8>) {
    ASSIST_IN_FLIGHT.store(true, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !runtime().stop_requested.load(Ordering::Acquire) {
        assert!(
            Instant::now() < deadline,
            "collector never requested final remark"
        );
        std::thread::yield_now();
    }
    ASSIST_SAW_REMARK.store(true, Ordering::Release);
}

#[test]
fn remark_waits_for_an_in_flight_mutator_assist() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    ASSIST_READY.store(false, Ordering::Relaxed);
    ASSIST_IN_FLIGHT.store(false, Ordering::Relaxed);
    ASSIST_SAW_REMARK.store(false, Ordering::Relaxed);
    for (id, callback) in [
        (0xFD01, hold_marker_for_assistant as ConcurrentTraceFn),
        (0xFD02, assist_overlaps_remark as ConcurrentTraceFn),
    ] {
        willow_register_type(id, trace_slot);
        runtime()
            .concurrent_trace_registry
            .lock()
            .unwrap()
            .insert(id, callback);
    }
    willow_register_type(0xFD03, trace_slot);
    let mut marker = willow_alloc_object(0xFD01, 8);
    let mut source = willow_alloc_object(0xFD03, 8);
    let assisted = willow_alloc_object(0xFD02, 8);
    unsafe {
        store_gc_reference(source.cast(), assisted);
    }
    willow_push_root(&mut marker);
    willow_push_root(&mut source);
    willow_gc_register_mutator();
    let ready = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let (ready, stop) = (Arc::clone(&ready), Arc::clone(&stop));
        let source = source as usize;
        std::thread::spawn(move || {
            willow_gc_register_mutator();
            ready.store(true, Ordering::Release);
            while !ASSIST_READY.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            let source = source as *mut u8;
            let child = unsafe { load_gc_reference(source.cast()) };
            willow_gc_write_barrier(source, child, GcStoreDestination::ObjectField as i64);
            assist_concurrent_mark();
            while !stop.load(Ordering::Acquire) {
                willow_gc_safepoint();
                std::thread::yield_now();
            }
            willow_gc_unregister_mutator();
        })
    };
    while !ready.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    willow_gc_collect();
    stop.store(true, Ordering::Release);
    worker.join().unwrap();
    assert!(ASSIST_SAW_REMARK.load(Ordering::Acquire));
    assert!(runtime().heap.lock().unwrap().concurrent_cycle.is_none());
    willow_pop_roots(2);
    willow_gc_unregister_mutator();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn memory_limit_also_bounds_generated_tlab_reservations() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    runtime().heap.lock().unwrap().memory_limit_bytes = Some(GC_TLAB_CHUNK_SIZE);
    let mut tls = GcTlabState {
        cursor: AtomicUsize::new(0),
        limit: AtomicUsize::new(0),
        fast_allocations: AtomicU64::new(0),
        fast_allocated_bytes: AtomicU64::new(0),
    };
    let mut root = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
    assert!(!root.is_null());
    willow_push_root(&mut root);
    let address = &mut tls as *mut GcTlabState as usize;
    assert!(allocate_tlab_chunk(&mut runtime().heap.lock().unwrap(), address).is_none());
    assert_eq!(
        runtime().heap.lock().unwrap().tlab_reserved_bytes,
        GC_TLAB_CHUNK_SIZE
    );
    willow_pop_roots(1);
    reset_internal_for_test();
}

#[test]
fn minor_collection_pins_children_when_region_budget_prevents_copying() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let mut tls = GcTlabState {
        cursor: AtomicUsize::new(0),
        limit: AtomicUsize::new(0),
        fast_allocations: AtomicU64::new(0),
        fast_allocated_bytes: AtomicU64::new(0),
    };
    let mut parent = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 1);
    willow_push_root(&mut parent);
    let child = willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
    willow_gc_write_barrier(parent, child, GcStoreDestination::ObjectField as i64);
    unsafe {
        store_gc_reference(parent.cast(), child);
    }
    runtime().heap.lock().unwrap().memory_limit_bytes = Some(2 * GC_TLAB_CHUNK_SIZE);
    willow_gc_minor_collect();
    assert_eq!(unsafe { load_gc_reference(parent.cast()) }, child);
    let state = runtime().heap.lock().unwrap();
    assert_eq!(state.promoted_objects, 2);
    assert_eq!(state.moved_objects, 0);
    assert!(state.old_regions.is_empty());
    drop(state);
    willow_pop_roots(1);
    reset_internal_for_test();
}

#[test]
fn budget_exhaustion_reports_error_before_callers_can_dereference_null() {
    if let Ok(path) = std::env::var("WILLOW_TEST_GC_OOM_PATH") {
        willow_gc_init();
        if path == "old" {
            willow_alloc(8);
        } else {
            let mut tls = GcTlabState {
                cursor: AtomicUsize::new(0),
                limit: AtomicUsize::new(0),
                fast_allocations: AtomicU64::new(0),
                fast_allocated_bytes: AtomicU64::new(0),
            };
            willow_gc_alloc_slow(&mut tls, 0, 0, 8, 0);
        }
        panic!("budget exhaustion returned to an unchecked allocation caller");
    }
    for path in ["old", "tlab"] {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "gc::concurrent_tests::budget_exhaustion_reports_error_before_callers_can_dereference_null", "--nocapture"])
            .env("WILLOW_TEST_GC_OOM_PATH", path).env("WILLOW_GC_MEMORY_LIMIT", "1")
            .output().unwrap();
        assert_eq!(
            result.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            String::from_utf8_lossy(&result.stderr)
                .contains("runtime fatal: GC memory limit exceeded")
        );
    }
}

#[test]
fn reservation_pressure_collects_garbage_before_rejecting_an_allocation() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    runtime().heap.lock().unwrap().memory_limit_bytes = Some(2 * GC_OLD_REGION_SIZE);
    // A tiny garbage object occupies a regular region, leaving less reserved
    // capacity than the large request despite being below the growth trigger.
    assert!(!willow_alloc(8).is_null());
    let large = willow_alloc((GC_OLD_REGION_SIZE + GC_OLD_REGION_SIZE / 2) as i64);
    assert!(!large.is_null());
    assert_eq!(runtime().heap.lock().unwrap().total_frees, 1);
    assert!(can_reserve(&runtime().heap.lock().unwrap(), 0));
    reset_internal_for_test();
}

#[test]
fn captured_array_owner_survives_resize_and_moving_collection() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let mut tls = GcTlabState {
        cursor: AtomicUsize::new(0),
        limit: AtomicUsize::new(0),
        fast_allocations: AtomicU64::new(0),
        fast_allocated_bytes: AtomicU64::new(0),
    };
    let mut array = crate::array::willow_array_new(1, 0);
    willow_push_root(&mut array);
    crate::array::willow_array_set(array, 0, 17);
    // Runtime arrays currently allocate old buffers. Install an equivalent
    // young buffer to prove the capture ABI remains relocation-safe when
    // allocation policy changes. Array handle word 3 is its traced buffer.
    let young = willow_gc_alloc_slow(&mut tls, 0, 0, 16, 0);
    unsafe {
        *(young.cast::<i64>()) = 1;
        *(young.cast::<i64>().add(1)) = 17;
        willow_gc_write_barrier(array, young, GcStoreDestination::ContainerInternal as i64);
        store_gc_reference(array.cast::<*mut u8>().add(3), young);
    }
    let captured = crate::array::willow_array_reference_owner(array, 0);
    assert_eq!(captured, young);
    // A captured owner in a parked async frame is an interior GC edge, not a
    // pinned stack root. The collector must update it after evacuation.
    let mut frame = willow_alloc_typed(8, 1);
    willow_push_root(&mut frame);
    unsafe {
        willow_gc_write_barrier(frame, captured, GcStoreDestination::AsyncFrameSlot as i64);
        store_gc_reference(frame.cast(), captured);
    }
    crate::array::willow_array_push(array, 23);
    assert_ne!(
        crate::array::willow_array_reference_owner(array, 0),
        captured
    );
    willow_gc_minor_collect();
    let relocated = unsafe { load_gc_reference(frame.cast()) };
    assert_ne!(relocated, captured, "captured owner should have evacuated");
    assert_eq!(unsafe { *relocated.cast::<i64>().add(1) }, 17);
    unsafe {
        *relocated.cast::<i64>().add(1) = 99;
    }
    assert_eq!(
        crate::array::willow_array_get(array, 0),
        17,
        "reference still names the pre-resize buffer"
    );
    willow_gc_collect();
    assert_eq!(
        unsafe { *load_gc_reference(frame.cast()).cast::<i64>().add(1) },
        99
    );
    willow_pop_roots(2);
    reset_internal_for_test();
}
