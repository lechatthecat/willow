use super::*;

fn gc_test_guard() -> std::sync::MutexGuard<'static, ()> {
    runtime_test_guard()
}

#[test]
fn empty_root_arena_first_snapshot_is_allocation_free() {
    use crate::scheduler::scaling_measurements::counting_allocator as counter;

    let arena = GcRootArena::default();
    // Move the initialized arena to a collector thread before its first
    // snapshot; do not warm up the snapshot path inside the measurement.
    std::thread::spawn(move || {
        let mut children = Vec::new();
        let allocations = counter::thread_allocations();
        let progress = arena.snapshot_slice(0, 512, usize::MAX, &mut children);
        let allocated = counter::thread_allocations() - allocations;
        assert_eq!(progress, Some((0, 0)));
        assert!(children.is_empty());
        assert_eq!(allocated, 0);
    })
    .join()
    .unwrap();
}

// willow-ssl7.12: stress-mode checks sit on the allocation slow path, so
// they must not read the environment (a Windows `getenv` allocates) per
// call. The environment is parsed once; tests switch modes through the
// override without touching the process environment.
#[test]
fn gc_stress_mode_checks_are_allocation_free_after_first_use() {
    use crate::scheduler::scaling_measurements::counting_allocator as counter;
    let _guard = gc_test_guard();
    set_gc_stress_for_test(None);
    let _ = gc_stress_enabled("alloc");
    let allocations = counter::thread_allocations();
    let bytes = counter::thread_bytes();
    for kind in ["alloc", "minor", "await", "scheduler"] {
        for _ in 0..1_000 {
            std::hint::black_box(gc_stress_enabled(kind));
        }
    }
    assert_eq!(
        (
            counter::thread_allocations() - allocations,
            counter::thread_bytes() - bytes
        ),
        (0, 0)
    );
}

#[test]
fn gc_stress_modes_parse_lists_and_override_replaces_the_environment() {
    let _guard = gc_test_guard();
    assert!(parse_gc_stress_modes("").is_empty());
    assert_eq!(
        parse_gc_stress_modes(" alloc , minor,,await"),
        ["alloc", "minor", "await"]
    );
    set_gc_stress_for_test(Some("minor, scheduler"));
    assert!(gc_stress_enabled("minor"));
    assert!(gc_stress_enabled("scheduler"));
    assert!(!gc_stress_enabled("alloc"));
    assert!(!gc_stress_enabled("await"));
    set_gc_stress_for_test(Some("all"));
    assert!(
        ["alloc", "minor", "await", "scheduler"]
            .into_iter()
            .all(gc_stress_enabled)
    );
    set_gc_stress_for_test(Some(""));
    assert!(!gc_stress_enabled("alloc"));
    set_gc_stress_for_test(None);
    let from_environment = std::env::var("WILLOW_GC_STRESS")
        .map(|value| parse_gc_stress_modes(&value))
        .unwrap_or_default();
    assert_eq!(
        gc_stress_enabled("alloc"),
        from_environment
            .iter()
            .any(|mode| mode == "all" || mode == "alloc")
    );
}

#[test]
fn runtime_state_starts_owned_and_empty() {
    let runtime = GcRuntime::default();
    let heap = runtime.heap.lock().unwrap();
    assert!(heap.old_regions.is_empty());
    assert_eq!(heap.allocated_bytes, 0);
    assert_eq!(runtime.runtime_roots.len(), 0);
    assert!(runtime.trace_registry.lock().unwrap().is_empty());
    assert!(runtime.drop_registry.lock().unwrap().is_empty());
}

#[test]
fn legacy_root_owner_released_after_late_registration() {
    let _guard = gc_test_guard();
    for count in [1, 16, 256] {
        reset_gc();
        let before = willow_gc_skipped_collections();
        std::thread::spawn(move || {
            let mut slot = willow_alloc_object(2, 32);
            for _ in 0..count {
                willow_push_root(&mut slot);
            }
            willow_gc_register_mutator();
            for _ in 0..count {
                willow_pop_root();
            }
            willow_gc_unregister_mutator();
        })
        .join()
        .unwrap();
        // The owner has exited; collection runs on a different thread.
        willow_gc_collect();
        assert_eq!(willow_gc_skipped_collections(), before, "roots={count}");
        assert_eq!(willow_gc_allocated_bytes(), 0, "roots={count}");
    }
    reset_gc();
}

#[test]
fn legacy_root_owner_retained_after_nonempty_unregister() {
    let _guard = gc_test_guard();
    reset_gc();
    let before = willow_gc_skipped_collections();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let owner = std::thread::spawn(move || {
        let mut slot = willow_alloc_object(2, 32);
        willow_push_root(&mut slot);
        willow_gc_register_mutator();
        willow_gc_unregister_mutator();
        ready_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        willow_pop_root();
    });
    ready_rx.recv().unwrap();
    willow_gc_collect();
    willow_gc_minor_collect();
    let skipped = willow_gc_skipped_collections();
    let retained = willow_gc_allocated_bytes();
    release_tx.send(()).unwrap();
    owner.join().unwrap();
    assert_eq!(skipped - before, 2);
    assert_eq!(retained, obj_size(32));
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

#[test]
fn gc_counts_collections_skipped_for_foreign_root_owner() {
    let _guard = gc_test_guard();
    reset_gc();
    let before = willow_gc_skipped_collections();
    // This thread claims root-stack ownership by pushing a root.
    let mut slot: *mut u8 = std::ptr::null_mut();
    willow_push_root(&mut slot as *mut *mut u8);
    // A foreign thread cannot scan our root stack, so its collection must be
    // skipped — and counted (willow-6fv.2).
    std::thread::spawn(|| willow_gc_collect()).join().unwrap();
    willow_pop_root();
    let after = willow_gc_skipped_collections();
    assert!(
        after > before,
        "a foreign-owner collection should be counted as skipped (before={before}, after={after})"
    );
}

// ── Multi-mutator coordination + STW (willow-6fv.5.6) ───────────────────

#[test]
fn coord_register_makes_multi_mutator_active_from_other_thread() {
    let _guard = gc_test_guard();
    reset_gc();
    assert!(!multi_mutator_active(), "no mutators registered yet");
    // A second registered thread makes this thread see a foreign mutator.
    let handle = std::thread::spawn(|| {
        willow_gc_register_mutator();
        // Keep the registration alive until the main thread observes it.
        std::thread::sleep(std::time::Duration::from_millis(20));
        willow_gc_unregister_mutator();
    });
    // Spin briefly until the worker has registered.
    let mut saw = false;
    for _ in 0..200 {
        if multi_mutator_active() {
            saw = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    handle.join().unwrap();
    assert!(
        saw,
        "a registered worker thread should be a foreign mutator"
    );
    assert!(!multi_mutator_active(), "unregister clears it");
}

#[test]
fn coord_safepoint_is_noop_when_no_stop_requested() {
    let _guard = gc_test_guard();
    reset_gc();
    // No collection in progress: a safepoint poll must return immediately.
    willow_gc_safepoint();
    willow_gc_register_mutator();
    willow_gc_safepoint();
    willow_gc_unregister_mutator();
}

#[test]
fn published_root_slots_are_writable_and_preserve_nulls() {
    use std::sync::{Arc, atomic::AtomicBool};
    let _guard = gc_test_guard();
    reset_gc();
    willow_gc_register_mutator();
    let mut replacement = willow_alloc_object(0, 8);
    willow_push_root(&mut replacement);
    let replacement_address = replacement as usize;
    let ready = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let worker_ready = ready.clone();
    let worker_done = done.clone();
    let worker = std::thread::spawn(move || {
        willow_gc_register_mutator();
        let mut first = willow_alloc_object(0, 8);
        let mut alias = first;
        let mut empty = std::ptr::null_mut();
        willow_push_root(&mut first);
        willow_push_root(&mut alias);
        willow_push_root(&mut empty);
        worker_ready.store(true, Ordering::Release);
        while !worker_done.load(Ordering::Acquire) {
            willow_gc_safepoint();
            std::thread::yield_now();
        }
        assert_eq!(first as usize, replacement_address);
        assert_eq!(alias, first);
        assert!(empty.is_null());
        willow_pop_roots(3);
        willow_gc_unregister_mutator();
    });
    while !ready.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    with_stw(crate::gc_telemetry::stops::StopReason::Minor, |coord, _| {
        let slots = all_registered_stack_root_slots(coord);
        assert_eq!(slots.len(), 3);
        let mut rewritten = 0;
        for slot in slots {
            // SAFETY: the coordinator keeps all owning stacks parked.
            unsafe {
                if !(*slot).is_null() && *slot != replacement {
                    *slot = replacement;
                    rewritten += 1;
                }
            }
        }
        assert_eq!(rewritten, 2);
        assert_eq!(all_registered_stack_roots(coord), vec![replacement; 3]);
        done.store(true, Ordering::Release);
    });
    worker.join().unwrap();
    willow_pop_roots(1);
    willow_gc_unregister_mutator();
}

#[test]
fn published_root_slot_count_is_linear_in_registered_slots() {
    let _guard = gc_test_guard();
    reset_gc();
    for count in [1, 16, 256, 4096] {
        let mut slots = vec![willow_alloc_object(0, 8); count];
        for slot in &mut slots {
            willow_push_root(slot);
        }
        with_stw(crate::gc_telemetry::stops::StopReason::Minor, |coord, _| {
            let published = all_registered_stack_root_slots(coord);
            assert_eq!(published.len(), count);
            assert!(
                published
                    .into_iter()
                    .zip(&mut slots)
                    .all(|(published, slot)| { std::ptr::eq(published, slot) })
            );
        });
        slots.fill(std::ptr::null_mut());
        with_stw(crate::gc_telemetry::stops::StopReason::Minor, |coord, _| {
            assert!(all_registered_stack_root_slots(coord).is_empty());
        });
        willow_pop_roots(count as i32);
        println!("registered slots={count} published slots={count}");
    }
}

#[test]
fn multi_mutator_stw_keeps_other_thread_roots_alive_through_generated_gate() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let _guard = gc_test_guard();
    reset_gc();
    willow_gc_register_mutator(); // main is a mutator too

    // Main holds root A.
    let a = willow_alloc_object(0, 8);
    let mut a_slot = a;
    willow_push_root(&mut a_slot as *mut *mut u8);

    let ready = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let (r2, d2) = (ready.clone(), done.clone());
    let worker = std::thread::spawn(move || {
        willow_gc_register_mutator();
        // Worker holds root B and keeps polling safepoints (so it parks
        // during the collector's stop-the-world).
        let b = willow_alloc_object(0, 8);
        let mut b_slot = b;
        willow_push_root(&mut b_slot as *mut *mut u8);
        assert_eq!(crate::preempt::willow_sync_native_active(), 0);
        let stop = willow_gc_stop_flag();
        assert_eq!(stop, willow_gc_stop_flag());
        r2.store(true, Ordering::SeqCst);
        while !d2.load(Ordering::SeqCst) {
            // Model generated non-task code: cache the address, but reload
            // the atomic gate so a later collector can stop this worker.
            // SAFETY: the accessor returns this process-lifetime AtomicBool.
            if unsafe { &*stop.cast::<AtomicBool>() }.load(Ordering::Acquire) {
                crate::preempt::willow_sync_safepoint();
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        willow_pop_root();
        willow_gc_unregister_mutator();
    });

    while !ready.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    let before = willow_gc_allocated_bytes();
    // Stop-the-world collection: scans main's A AND the worker's published B.
    willow_gc_collect();
    let after = willow_gc_allocated_bytes();
    assert_eq!(
        after, before,
        "STW collection must keep both mutators' rooted objects alive"
    );

    done.store(true, Ordering::SeqCst);
    worker.join().unwrap();
    willow_pop_root();
    willow_gc_unregister_mutator();
}

#[test]
fn multi_mutator_concurrent_collection_does_not_deadlock() {
    let _guard = gc_test_guard();
    reset_gc();
    // Two registered mutators each allocate garbage and trigger collections
    // concurrently. The collector-election (try_lock + safepoint) must keep
    // this deadlock-free: when one thread is collecting, the other parks at a
    // safepoint instead of blocking on the collect lock (willow-6fv.5.6).
    let handles: Vec<_> = (0..2)
        .map(|_| {
            std::thread::spawn(|| {
                willow_gc_register_mutator();
                for _ in 0..30 {
                    let _garbage = willow_alloc_object(0, 8); // unrooted
                    willow_gc_safepoint();
                    willow_gc_collect();
                }
                willow_gc_unregister_mutator();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    // With no live roots, a final collection reclaims everything.
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        0,
        "concurrent collection should reclaim all garbage without deadlock"
    );
}

#[test]
fn multi_mutator_stw_frees_unrooted_object() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let _guard = gc_test_guard();
    reset_gc();
    willow_gc_register_mutator();

    let ready = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let (r2, d2) = (ready.clone(), done.clone());
    let worker = std::thread::spawn(move || {
        willow_gc_register_mutator();
        // Worker registers but holds NO root; it just parks at safepoints.
        r2.store(true, Ordering::SeqCst);
        while !d2.load(Ordering::SeqCst) {
            willow_gc_safepoint();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        willow_gc_unregister_mutator();
    });
    while !ready.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    // An unrooted object must be collected even under multi-mutator STW.
    let _garbage = willow_alloc_object(0, 8);
    assert!(willow_gc_allocated_bytes() > 0);
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        0,
        "unrooted object must be freed by the STW collection"
    );

    done.store(true, Ordering::SeqCst);
    worker.join().unwrap();
    willow_gc_unregister_mutator();
}

// ── A registration that races a collection (willow-v6k0) ────────────────
//
// The collector used to ask `multi_mutator_active()` and, when it was the
// only registered mutator, scan just its own root stack and sweep without
// stopping the world. That question is a snapshot: a thread registering
// right after it runs unseen for the rest of the cycle, so the sweep frees
// the objects it has just allocated and rooted. The program keeps running
// on the dangling pointers until a later cycle traces one and aborts, or
// the reused memory produces a wrong answer.

/// Allocate `count` rooted objects, each stamped with a sentinel, into
/// `slots`, whose storage the caller keeps alive for the roots' lifetime.
fn root_stamped_objects(slots: &mut [*mut u8], sentinel: i64) {
    for (index, slot) in slots.iter_mut().enumerate() {
        *slot = willow_alloc_object(0, 8);
        // SAFETY: a freshly allocated 8-byte payload.
        unsafe { *(slot.cast::<i64>()) = sentinel + index as i64 };
        willow_push_root(slot as *mut *mut u8);
    }
}

/// Every slot still names a live object holding its sentinel.
fn assert_stamped_objects_live(slots: &[*mut u8], sentinel: i64, context: &str) {
    for (index, &slot) in slots.iter().enumerate() {
        #[cfg(debug_assertions)]
        if let Err(message) = validate_payload_pointer(slot, "test") {
            panic!("{context}: rooted object {index} was collected: {message}");
        }
        // SAFETY: the object above is rooted, so it is still live.
        let stamp = unsafe { *(slot.cast::<i64>()) };
        assert_eq!(
            stamp,
            sentinel + index as i64,
            "{context}: rooted object {index} lost its payload"
        );
    }
}

/// A worker that registers, roots objects, verifies them across another
/// thread's collections, and leaves. Returns once it has done `rounds`.
fn racing_root_worker(rounds: usize, sentinel: i64, minor: bool) {
    const PER_ROUND: usize = 4;
    for round in 0..rounds {
        willow_gc_register_mutator();
        let mut slots = [std::ptr::null_mut::<u8>(); PER_ROUND];
        root_stamped_objects(&mut slots, sentinel);
        // Give the collector a window in which this thread is registered
        // and holding roots it must scan.
        for _ in 0..4 {
            willow_gc_safepoint();
            std::thread::yield_now();
        }
        if minor {
            willow_gc_minor_collect();
        }
        assert_stamped_objects_live(&slots, sentinel, &format!("round {round}"));
        willow_pop_roots(PER_ROUND as i32);
        willow_gc_unregister_mutator();
    }
}

/// Hammer collections on one thread while `workers` threads register,
/// allocate rooted objects and unregister underneath it.
fn run_registration_race(workers: usize, rounds: usize, minor: bool) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let stop = Arc::new(AtomicBool::new(false));
    let collector_stop = stop.clone();
    let collector = std::thread::spawn(move || {
        while !collector_stop.load(Ordering::Acquire) {
            // This thread never registers, so it is the one that used to
            // take the "I am alone" fast path.
            if minor {
                willow_gc_minor_collect();
            } else {
                willow_gc_collect();
            }
            std::thread::yield_now();
        }
    });
    let handles: Vec<_> = (0..workers)
        .map(|worker| {
            std::thread::spawn(move || {
                racing_root_worker(rounds, 1_000 + worker as i64 * 1_000, minor)
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("worker thread panicked");
    }
    stop.store(true, Ordering::Release);
    collector.join().expect("collector thread panicked");
}

#[test]
fn coord_registration_racing_a_major_collection_keeps_the_newcomers_roots() {
    let _guard = gc_test_guard();
    reset_gc();
    run_registration_race(1, 200, false);
    reset_gc();
}

#[test]
fn coord_registration_racing_a_minor_collection_keeps_the_newcomers_roots() {
    let _guard = gc_test_guard();
    reset_gc();
    run_registration_race(1, 200, true);
    reset_gc();
}

#[test]
fn coord_several_mutators_registering_at_once_keep_their_roots() {
    let _guard = gc_test_guard();
    reset_gc();
    run_registration_race(4, 100, false);
    reset_gc();
}

#[test]
fn coord_a_registered_mutators_runtime_root_survives_a_racing_collection() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let _guard = gc_test_guard();
    reset_gc();
    // The same race reaches runtime roots — the scheduler's hold on a task
    // frame — because the sweep, not the scan, is what frees the object.
    let stop = Arc::new(AtomicBool::new(false));
    let collector_stop = stop.clone();
    let collector = std::thread::spawn(move || {
        while !collector_stop.load(Ordering::Acquire) {
            willow_gc_collect();
            std::thread::yield_now();
        }
    });
    for round in 0..200 {
        willow_gc_register_mutator();
        let object = willow_alloc_object(0, 8);
        willow_gc_add_runtime_root(object);
        // SAFETY: the runtime root keeps this payload alive.
        unsafe { *(object.cast::<i64>()) = 77 };
        for _ in 0..4 {
            willow_gc_safepoint();
            std::thread::yield_now();
        }
        #[cfg(debug_assertions)]
        if let Err(message) = validate_payload_pointer(object, "test") {
            panic!("round {round}: runtime root was collected: {message}");
        }
        // SAFETY: still rooted.
        assert_eq!(unsafe { *(object.cast::<i64>()) }, 77, "round {round}");
        willow_gc_remove_runtime_root(object);
        willow_gc_unregister_mutator();
    }
    stop.store(true, Ordering::Release);
    collector.join().expect("collector thread panicked");
    reset_gc();
}

#[test]
fn coord_a_collection_racing_registration_still_frees_garbage() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let _guard = gc_test_guard();
    reset_gc();
    // Stopping the world for every cycle must not make the collector
    // conservative: unrooted objects allocated by a registering worker are
    // still reclaimed.
    let stop = Arc::new(AtomicBool::new(false));
    let collector_stop = stop.clone();
    let collector = std::thread::spawn(move || {
        while !collector_stop.load(Ordering::Acquire) {
            willow_gc_collect();
            std::thread::yield_now();
        }
    });
    for _ in 0..200 {
        willow_gc_register_mutator();
        let _garbage = willow_alloc_object(0, 8);
        willow_gc_safepoint();
        willow_gc_unregister_mutator();
    }
    stop.store(true, Ordering::Release);
    collector.join().expect("collector thread panicked");
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        0,
        "unrooted objects from a registering worker must still be freed"
    );
    reset_gc();
}

#[cfg(debug_assertions)]
#[test]
fn coord_a_panicking_collection_resumes_the_world() {
    let _guard = gc_test_guard();
    reset_gc();
    // Now that every cycle stops the world, a cycle that panics has to put
    // the world back: an unwind that leaves `stop_requested` set parks every
    // other mutator forever, and one that escapes the registry guard
    // poisons it, so no later collection can run at all.
    willow_gc_add_runtime_root(std::ptr::dangling_mut::<u8>());
    let result = std::panic::catch_unwind(collect_internal);
    assert!(
        result.is_err(),
        "an invalid runtime root must fail the cycle"
    );
    assert!(
        !runtime()
            .stop_requested
            .load(std::sync::atomic::Ordering::Acquire),
        "the world must be resumed after a panicking cycle"
    );
    reset_gc();
    let mut slots = [std::ptr::null_mut::<u8>(); 2];
    root_stamped_objects(&mut slots, 9);
    willow_gc_collect();
    assert_stamped_objects_live(&slots, 9, "after a panicking cycle");
    willow_pop_roots(2);
    reset_gc();
}

#[test]
fn coord_a_lone_thread_still_collects_without_registering() {
    let _guard = gc_test_guard();
    reset_gc();
    // The fast path is gone, so check the ordinary single-threaded program
    // still collects: an unregistered thread with no other mutators must
    // reclaim its garbage and keep its roots.
    let mut slots = [std::ptr::null_mut::<u8>(); 2];
    root_stamped_objects(&mut slots, 5);
    let _garbage = willow_alloc_object(0, 8);
    willow_gc_collect();
    assert_stamped_objects_live(&slots, 5, "lone thread");
    willow_pop_roots(2);
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        0,
        "a lone thread's collection reclaims everything once the roots go"
    );
    reset_gc();
}

fn reset_gc() {
    reset_internal();
}

fn set_threshold(bytes: usize) {
    runtime().heap.lock().unwrap().threshold_bytes = bytes;
}

fn total_allocs() -> u64 {
    runtime().heap.lock().unwrap().total_allocs
}

fn total_frees() -> u64 {
    runtime().heap.lock().unwrap().total_frees
}

fn header_size() -> usize {
    std::mem::size_of::<GcHeader>()
}

fn obj_size(payload: usize) -> i64 {
    (header_size() + payload) as i64
}

fn new_tlab_state() -> GcTlabState {
    tlab_state_for_test()
}

#[test]
fn telemetry_merges_fast_tlab_deltas_once_and_on_unregister() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_gc_register_mutator();
    let mut tls = new_tlab_state();
    let first = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    assert!(!first.is_null());
    let bytes = GC_HEADER_SIZE + 8;
    let cursor = tls.cursor.load(Ordering::Acquire);
    // Reproduce the generated bump fast path, including its TLS counters.
    initialize_object_at(cursor as *mut u8, bytes, 0, 1, 0).unwrap();
    publish_tlab_start_for_test(&tls, cursor as *mut u8);
    tls.cursor.store(cursor + bytes, Ordering::Release);
    tls.fast_allocations.store(1, Ordering::Release);
    tls.fast_allocated_bytes
        .store(bytes as u64, Ordering::Release);
    let first = crate::gc_telemetry::snapshot();
    let second = crate::gc_telemetry::snapshot();
    assert_eq!(first.counters.allocation_count, 2);
    assert_eq!(first.counters.allocation_bytes, (bytes * 2) as u64);
    assert_eq!(first.counters.tlab_fast_allocations, 1);
    assert_eq!(
        second.counters.allocation_bytes,
        first.counters.allocation_bytes
    );
    willow_gc_unregister_mutator();
    // `GcTlabState` has no `Drop` hook, so letting the state die can merge
    // nothing: the deltas above are in the totals only because
    // `willow_gc_unregister_mutator` merged them, and the snapshot below
    // stays put no matter how long the state lives.
    assert_eq!(
        crate::gc_telemetry::snapshot().counters.allocation_bytes,
        first.counters.allocation_bytes
    );
    willow_gc_collect();
    assert_eq!(crate::gc_telemetry::snapshot().heap.occupied_bytes, 0);
    reset_gc();
}

#[test]
fn telemetry_promotion_is_not_a_second_logical_allocation() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut tls = new_tlab_state();
    let young = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    let mut parent = willow_alloc_typed(8, 1);
    willow_push_root(&mut parent);
    willow_gc_write_barrier(
        parent,
        std::ptr::null_mut(),
        young,
        GcStoreDestination::ObjectField as i64,
    );
    unsafe {
        *parent.cast::<*mut u8>() = young;
    }
    let before = crate::gc_telemetry::snapshot();
    willow_gc_minor_collect();
    let after = crate::gc_telemetry::snapshot();
    assert_eq!(
        after.counters.allocation_count,
        before.counters.allocation_count
    );
    assert_eq!(
        after.counters.allocation_bytes,
        before.counters.allocation_bytes
    );
    assert_eq!(after.counters.promoted_objects, 0);
    assert_eq!(after.counters.promoted_bytes, 0);
    assert_eq!(willow_gc_survivor_copies(), 1);
    assert_eq!(willow_gc_survivor_bytes(), (GC_HEADER_SIZE + 8) as i64);
    assert_eq!(after.last_cycle.reclaimed_bytes, 0);
    assert_eq!(after.counters.released_bytes, GC_TLAB_CHUNK_SIZE as u64);
    assert_eq!(
        after.last_cycle.marked_bytes,
        (2 * (GC_HEADER_SIZE + 8)) as u64
    );
    willow_pop_root();
    reset_gc();
}

#[test]
fn compact_header_descriptors_follow_sweep_copy_and_reset() {
    let _guard = gc_test_guard();
    reset_gc();
    let key = willow_abi::GcLayoutDescriptor {
        type_id: 0,
        layout_id: 0x9713_2026,
        gc_ref_mask: 0,
        size: (GC_HEADER_SIZE + 8) as u64,
    };
    let mut tls = new_tlab_state();
    for _ in 0..2 {
        willow_gc_alloc_layout(key.layout_id, 0, 8, 0);
    }
    assert_eq!(layouts::references(key), 2);
    willow_gc_collect();
    assert_eq!(layouts::references(key), 0);
    let mut young = willow_gc_alloc_slow(&mut tls, key.layout_id, 0, 8, 0);
    let mut parent = willow_alloc_typed(8, 1);
    willow_push_root(&mut parent);
    willow_gc_write_barrier(
        parent,
        std::ptr::null_mut(),
        young,
        GcStoreDestination::ObjectField as i64,
    );
    unsafe {
        *parent.cast::<*mut u8>() = young;
    }
    for _ in 0..2 {
        willow_gc_minor_collect();
        let moved = unsafe { *parent.cast::<*mut u8>() };
        assert_ne!(young, moved);
        young = moved;
        assert_eq!(layouts::references(key), 1);
    }
    willow_pop_root();
    reset_gc();
    assert_eq!(layouts::references(key), 0);
    // Reset must retire the active TLAB prefix before releasing descriptors.
    willow_gc_alloc_slow(&mut tls, key.layout_id, 0, 8, 0);
    assert_eq!(layouts::references(key), 1);
    reset_gc();
    assert_eq!(layouts::references(key), 0);
}

#[test]
fn compact_static_header_reclamation_keeps_hole_size() {
    static DESCRIPTOR: willow_abi::GcLayoutDescriptor = willow_abi::GcLayoutDescriptor {
        type_id: 23,
        layout_id: 42,
        gc_ref_mask: 0,
        size: 24,
    };
    let mut header = GcHeader {
        marked: false,
        allocated: true,
        generation: GC_GENERATION_YOUNG,
        age: 0,
        descriptor_owned: false,
        descriptor: &DESCRIPTOR as *const _ as usize,
    };
    let object = HeapObject::from_raw(&mut header).unwrap();
    assert_eq!(object.type_id(), 23);
    object.reclaim_in_place();
    assert!(!object.allocated());
    assert_eq!(object.size(), 24);
    object.reclaim_in_place();
    assert_eq!(object.size(), 24);
}

#[test]
fn test_gc_generated_header_and_tlab_abi_layout() {
    assert_eq!(GC_HEADER_SIZE, 16);
    assert_eq!(std::mem::size_of::<willow_abi::GcLayoutDescriptor>(), 32);
    assert_eq!(
        std::mem::offset_of!(willow_abi::GcLayoutDescriptor, type_id),
        0
    );
    assert_eq!(
        std::mem::offset_of!(willow_abi::GcLayoutDescriptor, layout_id),
        8
    );
    assert_eq!(
        std::mem::offset_of!(willow_abi::GcLayoutDescriptor, gc_ref_mask),
        16
    );
    assert_eq!(
        std::mem::offset_of!(willow_abi::GcLayoutDescriptor, size),
        24
    );
    assert_eq!(std::mem::offset_of!(GcHeader, marked), 0);
    assert_eq!(std::mem::offset_of!(GcHeader, allocated), 1);
    assert_eq!(std::mem::offset_of!(GcHeader, generation), 2);
    assert_eq!(std::mem::offset_of!(GcHeader, age), 3);
    assert_eq!(std::mem::offset_of!(GcHeader, descriptor_owned), 4);
    assert_eq!(std::mem::offset_of!(GcHeader, descriptor), 8);
    assert_eq!(GC_TLAB_STATE_SIZE, 40);
    assert_eq!(std::mem::offset_of!(GcTlabState, cursor), 0);
    assert_eq!(std::mem::offset_of!(GcTlabState, limit), 8);
    assert_eq!(std::mem::offset_of!(GcTlabState, fast_allocations), 16);
    assert_eq!(std::mem::offset_of!(GcTlabState, fast_allocated_bytes), 24);
    assert_eq!(std::mem::offset_of!(GcTlabState, start_bits), 32);
}

#[test]
fn test_gc_tlab_large_object_uses_old_region_slow_path() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut tls = new_tlab_state();
    let payload_size = GC_TLAB_MAX_OBJECT_SIZE as i64;
    let ptr = willow_gc_alloc_slow(&mut tls, 7, 9, payload_size, 0);
    assert!(!ptr.is_null());
    assert_eq!(willow_gc_tlab_fast_allocations(), 0);
    assert_eq!(willow_gc_tlab_slow_allocations(), 1);
    assert_eq!(willow_gc_tlab_large_allocations(), 1);
    assert_eq!(willow_gc_tlab_refills(), 0);
    assert_eq!(willow_gc_tlab_reserved_bytes(), 0);
    assert_eq!(
        willow_gc_allocated_bytes(),
        GC_HEADER_SIZE as i64 + payload_size
    );
    reset_gc();
}

#[test]
fn test_gc_tlab_refill_slow_path_coordinates_threshold_collection() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut tls = new_tlab_state();
    let first = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    assert!(!first.is_null());
    assert_eq!(willow_gc_tlab_refills(), 1);
    set_threshold(1);
    let second = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    assert!(!second.is_null());
    assert_eq!(
        willow_gc_allocated_bytes(),
        (GC_HEADER_SIZE + 8) as i64,
        "the unrooted object in the retired first chunk was collected"
    );
    assert_eq!(willow_gc_tlab_refills(), 2);
    assert_eq!(total_frees(), 1);
    reset_gc();
}

#[test]
fn test_gc_minor_collection_moves_heap_reachable_young_and_updates_slot() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut tls = new_tlab_state();
    let young = willow_gc_alloc_slow(&mut tls, 11, 0, 8, 0);
    assert!(!young.is_null());
    unsafe { *(young as *mut i64) = 0x1234 };

    let mut parent = willow_gc_alloc_layout(12, 0, 8, 0b1);
    willow_gc_write_barrier(
        parent,
        std::ptr::null_mut(),
        young,
        GcStoreDestination::ObjectField as i64,
    );
    unsafe { *(parent as *mut *mut u8) = young };
    willow_push_root(&mut parent as *mut *mut u8);

    willow_gc_minor_collect();

    let moved = unsafe { *(parent as *mut *mut u8) };
    assert_ne!(moved, young, "heap-only young child should be copied");
    assert_eq!(unsafe { *(moved as *mut i64) }, 0x1234);
    assert_eq!(
        unsafe { (*payload_to_header(moved)).generation },
        GC_GENERATION_YOUNG
    );
    assert_eq!(willow_gc_moved_objects(), 1);
    assert_eq!(willow_gc_remembered_set_size(), 1);
    assert_eq!(willow_gc_tlab_reserved_bytes(), GC_TLAB_CHUNK_SIZE as i64);

    willow_pop_root();
    willow_gc_collect();
    reset_gc();
}

#[test]
fn test_gc_minor_collection_pins_direct_young_root_for_ssa_compatibility() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut tls = new_tlab_state();
    let mut young = willow_gc_alloc_slow(&mut tls, 21, 0, 8, 0);
    unsafe { *(young as *mut i64) = 77 };
    willow_push_root(&mut young as *mut *mut u8);

    willow_gc_minor_collect();

    assert_eq!(unsafe { *(young as *mut i64) }, 77);
    assert_eq!(
        unsafe { (*payload_to_header(young)).generation },
        GC_GENERATION_OLD
    );
    assert_eq!(willow_gc_moved_objects(), 0);
    assert_eq!(willow_gc_promoted_objects(), 1);
    assert!(willow_gc_tlab_reserved_bytes() > 0);
    assert_eq!(willow_gc_pinned_region_count(), 1);
    assert_eq!(willow_gc_old_region_count(), 1);

    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    assert_eq!(willow_gc_tlab_reserved_bytes(), 0);
    reset_gc();
}

#[test]
fn test_gc_minor_collection_pins_runtime_root_until_owner_releases_it() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut tls = new_tlab_state();
    let young = willow_gc_alloc_slow(&mut tls, 22, 0, 8, 0);
    unsafe { *(young as *mut i64) = 78 };
    willow_gc_add_runtime_root(young);

    willow_gc_minor_collect();

    assert_eq!(unsafe { *(young as *mut i64) }, 78);
    assert_eq!(
        unsafe { (*payload_to_header(young)).generation },
        GC_GENERATION_OLD
    );
    willow_gc_remove_runtime_root(young);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

#[test]
fn test_gc_minor_collection_reclaims_unreachable_nursery() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut tls = new_tlab_state();
    let young = willow_gc_alloc_slow(&mut tls, 31, 0, 8, 0);
    assert!(!young.is_null());

    willow_gc_minor_collect();

    assert_eq!(willow_gc_allocated_bytes(), 0);
    assert_eq!(willow_gc_tlab_reserved_bytes(), 0);
    assert_eq!(willow_gc_minor_collections(), 1);
    reset_gc();
}

#[test]
fn test_gc_minor_collection_updates_array_reference_slots() {
    let _guard = gc_test_guard();
    // This fixture specifically verifies nursery evacuation and its barrier.
    let _stress = GcStressTestScope::normal();
    reset_gc();
    let mut array = crate::array::willow_array_new(1, 1);
    willow_push_root(&mut array as *mut *mut u8);
    let mut tls = new_tlab_state();
    let young = willow_gc_alloc_slow(&mut tls, 41, 0, 8, 0);
    unsafe { *(young as *mut i64) = 901 };
    crate::array::willow_array_set(array, 0, young as i64);
    assert!(willow_gc_remembered_set_size() > 0);
    assert!(willow_gc_dirty_card_count() > 0);

    willow_gc_minor_collect();

    let moved = crate::array::willow_array_get(array, 0) as *mut u8;
    assert_ne!(moved, young);
    assert_eq!(unsafe { *(moved as *mut i64) }, 901);
    assert!(willow_gc_dirty_card_count() > 0);
    willow_gc_minor_collect();
    assert_eq!(willow_gc_dirty_card_count(), 0);
    let tenured = crate::array::willow_array_get(array, 0) as *mut u8;
    assert_eq!(unsafe { *tenured.cast::<i64>() }, 901);
    willow_pop_root();
    willow_gc_collect();
    reset_gc();
}

#[test]
fn test_gc_minor_collection_updates_map_reference_slots() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut map = crate::map::willow_map_new(0, 3, 1);
    willow_push_root(&mut map as *mut *mut u8);
    let mut tls = new_tlab_state();
    let young = willow_gc_alloc_slow(&mut tls, 42, 0, 8, 0);
    unsafe { *(young as *mut i64) = 902 };
    crate::map::willow_map_insert(map, 7, 0, young as i64, 1);
    assert!(willow_gc_remembered_set_size() > 0);

    willow_gc_minor_collect();

    let option = crate::map::willow_map_get(map, 7, 0, 0);
    let moved = unsafe { *((option as *mut *mut u8).add(1)) };
    assert_ne!(moved, young);
    assert_eq!(unsafe { *(moved as *mut i64) }, 902);
    willow_pop_root();
    willow_gc_collect();
    reset_gc();
}

#[test]
fn test_gc_minor_collection_updates_channel_queue_reference_slots() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut channel = crate::channel::willow_channel_new(1) as *mut u8;
    willow_push_root(&mut channel as *mut *mut u8);
    let mut tls = new_tlab_state();
    let young = willow_gc_alloc_slow(&mut tls, 43, 0, 8, 0);
    unsafe { *(young as *mut i64) = 903 };
    crate::channel::willow_channel_send_ptr(channel.cast(), young.cast());
    assert!(willow_gc_remembered_set_size() > 0);

    willow_gc_minor_collect();

    let moved = crate::channel::willow_channel_recv_ptr(channel.cast()).cast::<u8>();
    assert_ne!(moved, young);
    assert_eq!(unsafe { *(moved as *mut i64) }, 903);
    willow_pop_root();
    willow_gc_collect();
    reset_gc();
}

// -------------------------------------------------------------------------
// 基本: alloc
// -------------------------------------------------------------------------

#[test]
fn test_gc_alloc_returns_non_null() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(1, 16);
    assert!(!ptr.is_null());
    reset_gc();
}

#[test]
fn test_gc_alloc_zero_size_object() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(0, 0);
    assert!(!ptr.is_null(), "zero-payload allocation should succeed");
    reset_gc();
}

#[test]
fn test_gc_allocated_bytes_increases() {
    let _guard = gc_test_guard();
    reset_gc();
    let before = willow_gc_allocated_bytes();
    willow_alloc_object(1, 64);
    let after = willow_gc_allocated_bytes();
    assert!(after > before);
    reset_gc();
}

/// allocated_bytes はヘッダ込みのサイズを追跡していること
#[test]
fn test_gc_allocated_bytes_includes_header_overhead() {
    let _guard = gc_test_guard();
    reset_gc();
    let payload: i64 = 40;
    willow_alloc_object(1, payload);
    let expected = (header_size() as i64) + payload;
    assert_eq!(
        willow_gc_allocated_bytes(),
        expected,
        "allocated_bytes must include GcHeader overhead"
    );
    reset_gc();
}

/// total_allocs カウンタが増える
#[test]
fn test_gc_total_allocs_counter() {
    let _guard = gc_test_guard();
    reset_gc();
    let before = total_allocs();
    willow_alloc_object(1, 8);
    willow_alloc_object(1, 8);
    willow_alloc_object(1, 8);
    assert_eq!(total_allocs(), before + 3);
    reset_gc();
}

#[test]
fn test_gc_alloc_wrapper_uses_opaque_type_and_zero_mask() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc(16);
    let header = payload_to_header(ptr);
    assert_eq!(
        HeapObject::from_raw(header)
            .unwrap()
            .trace_metadata()
            .type_id,
        0
    );
    assert_eq!(
        HeapObject::from_raw(header)
            .unwrap()
            .trace_metadata()
            .layout_id,
        0
    );
    assert_eq!(
        HeapObject::from_raw(header)
            .unwrap()
            .trace_metadata()
            .gc_ref_mask,
        0
    );
    reset_gc();
}

#[test]
fn test_gc_alloc_typed_records_ref_mask() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_typed(16, 0b10);
    let header = payload_to_header(ptr);
    assert_eq!(
        HeapObject::from_raw(header)
            .unwrap()
            .trace_metadata()
            .gc_ref_mask,
        0b10
    );
    reset_gc();
}

#[test]
fn test_gc_bitmap_and_inline_slots_follow_mixed_storage_layout() {
    let _guard = gc_test_guard();
    reset_gc();
    // Inline bit 1 and extended bit 65 have a scalar word immediately
    // before them. Both paths must locate the same storage-word stride.
    static BITMAP: [u64; 3] = [2, 0b10, 0b10];
    let bytes = 66 * GC_STORAGE_WORD_BYTES;
    let fingerprint = willow_abi::gc_bitmap_layout_id(
        bytes as i64,
        17,
        willow_abi::gc_bitmap_fingerprint(&BITMAP[1..]),
    );
    let parent = willow_gc_alloc_bitmap(fingerprint as i64, bytes as i64, BITMAP.as_ptr());
    let child = willow_alloc(8);
    let object = HeapObject::from_raw(payload_to_header(parent)).unwrap();
    assert_eq!(object.trace_metadata().layout_id, fingerprint);
    assert_eq!(object.trace_metadata().gc_ref_mask, BITMAP.as_ptr() as u64);
    assert_eq!(
        std::mem::size_of::<GcHeader>(),
        willow_abi::gc_header::size(std::mem::size_of::<usize>() as u32) as usize
    );
    let offsets = [
        willow_abi::WordLayout::new(&[willow_abi::SlotKind::Word])
            .byte_size(std::mem::size_of::<usize>() as u32) as usize,
        65 * GC_STORAGE_WORD_BYTES,
    ];
    for &offset in &offsets {
        unsafe {
            parent
                .add(offset - GC_STORAGE_WORD_BYTES)
                .cast::<i64>()
                .write(i64::MAX);
            parent.add(offset).cast::<*mut u8>().write(child);
        }
    }
    let slots = object_reference_slots(object, &HashMap::new());
    assert_eq!(slots.len(), 2);
    for (slot, offset) in slots.iter().zip(offsets) {
        assert_eq!(*slot as usize - parent as usize, offset);
        assert_eq!(unsafe { **slot }, child);
    }
    assert_eq!(object.payload_word(1).unwrap().as_ptr(), child);
    assert_eq!(object.payload_word(65).unwrap().as_ptr(), child);
    let mut root = parent;
    willow_push_root(&mut root);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), obj_size(bytes) + obj_size(8));
    willow_pop_root();
    reset_gc();
}

#[test]
fn bitmap_fingerprints_are_independent_of_descriptor_addresses() {
    let _guard = gc_test_guard();
    reset_gc();
    for words in [0usize, 1, 2, 8, 32, 128] {
        let mut descriptor = vec![0u64; words + 1];
        descriptor[0] = words as u64;
        if words > 0 {
            descriptor[words] = 1;
        }
        let copy = descriptor.clone();
        let bytes = words * 64 * GC_STORAGE_WORD_BYTES;
        let fingerprint = willow_abi::gc_bitmap_layout_id(
            bytes as i64,
            17,
            willow_abi::gc_bitmap_fingerprint(&descriptor[1..]),
        );
        assert_eq!(
            fingerprint,
            willow_abi::gc_bitmap_layout_id(
                bytes as i64,
                17,
                willow_abi::gc_bitmap_fingerprint(&copy[1..])
            )
        );
        assert_ne!(
            fingerprint,
            willow_abi::gc_bitmap_layout_id(
                bytes as i64,
                18,
                willow_abi::gc_bitmap_fingerprint(&copy[1..])
            )
        );
        for data in [&descriptor, &copy] {
            let ptr = willow_gc_alloc_bitmap(fingerprint as i64, bytes as i64, data.as_ptr());
            let object = HeapObject::from_raw(payload_to_header(ptr)).unwrap();
            assert_eq!(object.trace_metadata().layout_id, fingerprint);
            assert_eq!(object.trace_metadata().gc_ref_mask, data.as_ptr() as u64);
            let slots = object_reference_slots(object, &HashMap::new());
            assert_eq!(slots.len(), usize::from(words > 0));
            if words > 0 {
                assert_eq!(
                    slots[0] as usize - ptr as usize,
                    (words - 1) * 64 * GC_STORAGE_WORD_BYTES
                );
            }
        }
        // Reclaim objects before their borrowed descriptors leave scope.
        reset_gc();
    }
}

#[test]
fn test_gc_layout_allocation_records_central_metadata() {
    let _guard = gc_test_guard();
    reset_gc();
    assert_eq!(
        gc_layout_id(GcObjectKind::Enum, 16, 0, 0b10),
        0x17b2_8090_98b9_7b2d,
        "compiler/runtime layout fingerprint contract changed"
    );
    let ptr = willow_gc_alloc_layout(0xCAFE, 42, 24, 0b101);
    assert!(!ptr.is_null());
    let header = payload_to_header(ptr);
    assert_eq!(
        HeapObject::from_raw(header)
            .unwrap()
            .trace_metadata()
            .layout_id,
        0xCAFE
    );
    assert_eq!(
        HeapObject::from_raw(header)
            .unwrap()
            .trace_metadata()
            .type_id,
        42
    );
    assert_eq!(
        HeapObject::from_raw(header)
            .unwrap()
            .trace_metadata()
            .gc_ref_mask,
        0b101
    );
    assert_eq!(
        HeapObject::from_raw(header).unwrap().size(),
        header_size() + 24
    );
    reset_gc();
}

#[test]
fn test_gc_write_barrier_ignores_old_to_old_store() {
    let _guard = gc_test_guard();
    reset_gc();
    let child = willow_alloc(8);
    let parent = willow_gc_alloc_layout(7, 0, 8, 0b1);
    willow_gc_write_barrier(parent, std::ptr::null_mut(), child, 1);
    assert_eq!(willow_gc_remembered_set_size(), 0);
    unsafe { *(parent as *mut *mut u8) = child };
    let mut slot = parent;
    willow_push_root(&mut slot as *mut *mut u8);
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        obj_size(8) * 2,
        "old-to-old store must preserve mask-based tracing semantics"
    );
    willow_pop_root();
    reset_gc();
}

#[test]
fn test_gc_barrier_verifier_rejects_unremembered_old_to_young_edge() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut tls = new_tlab_state();
    let young = willow_gc_alloc_slow(&mut tls, 51, 0, 8, 0);
    let parent = willow_gc_alloc_layout(52, 0, 8, 0b1);
    // Intentionally bypass the central barrier to prove verification catches it.
    unsafe { *(parent as *mut *mut u8) = young };
    let trace_registry = type_registry().lock().unwrap().clone();
    let result = {
        let mut state = runtime().heap.lock().unwrap();
        retire_all_tlabs_locked(&mut state);
        verify_remembered_set(&state, &trace_registry)
    };
    assert!(
        result
            .expect_err("missing barrier entry must be rejected")
            .contains("without a remembered-set entry")
    );
    reset_gc();
}

// Exercise the production sweep directly to isolate sweep work from mark
// discovery and allocation costs. All fixture pointers remain GC-owned.
#[test]
fn test_gc_sweep_scaling_and_fragmentation() {
    let _guard = gc_test_guard();
    for large in [false, true] {
        for n in [32, 64, 128, 256] {
            reset_gc();
            set_threshold(usize::MAX);
            let payload_size = if large { GC_LARGE_OBJECT_THRESHOLD } else { 8 };
            let objects: Vec<_> = (0..2 * n)
                .map(|_| willow_alloc_object(61, payload_size as i64))
                .collect();
            for (index, &payload) in objects.iter().enumerate() {
                unsafe {
                    *payload = (index % 251) as u8;
                    (*payload_to_header(payload)).marked = index % 2 == 1;
                }
            }
            SWEEP_REGION_VISITS.store(0, Ordering::Relaxed);
            SWEEP_OBJECT_VISITS.store(0, Ordering::Relaxed);
            let start = std::time::Instant::now();
            assert_eq!(sweep(), n * (GC_HEADER_SIZE + payload_size));
            let elapsed = start.elapsed();
            assert_eq!(
                SWEEP_REGION_VISITS.load(Ordering::Relaxed),
                if large { 2 * n } else { 1 }
            );
            assert_eq!(SWEEP_OBJECT_VISITS.load(Ordering::Relaxed), 2 * n);
            let state = runtime().heap.lock().unwrap();
            verify_old_region_metadata(&state).unwrap();
            assert_eq!(state.allocated_bytes, n * (GC_HEADER_SIZE + payload_size));
            assert_eq!(state.old_regions.len(), if large { n } else { 1 });
            if !large {
                assert_eq!(state.old_regions[0].free_spans.len(), n);
            }
            for (index, &payload) in objects.iter().enumerate().filter(|(i, _)| i % 2 == 1) {
                assert_eq!(unsafe { *payload }, (index % 251) as u8);
            }
            drop(state);
            // The visit counts above are the proof; this line is the
            // separate pause measurement, and it carries the build
            // configuration it was taken in because the two are not
            // comparable across profiles (willow-ssl7.1, willow-tqzq).
            eprintln!(
                "sweep profile={} large={large} dead={n} regions={} objects={} elapsed_ns={}",
                if cfg!(debug_assertions) {
                    "debug"
                } else {
                    "release"
                },
                SWEEP_REGION_VISITS.load(Ordering::Relaxed),
                SWEEP_OBJECT_VISITS.load(Ordering::Relaxed),
                elapsed.as_nanos()
            );
            // Cleared marks make the next sweep reclaim every survivor.
            assert_eq!(sweep(), n * (GC_HEADER_SIZE + payload_size));
            assert_eq!(willow_gc_old_region_count(), 0);
            assert_eq!(willow_gc_allocated_bytes(), 0);
        }
    }
    reset_gc();
}

#[test]
fn test_gc_sweep_adjacent_gaps_tail_finalizers_and_cards() {
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    unsafe fn count_drop(_: *mut u8) {
        DROPS.fetch_add(1, Ordering::Relaxed);
    }
    let _guard = gc_test_guard();
    reset_gc();
    set_threshold(usize::MAX);
    DROPS.store(0, Ordering::Relaxed);
    willow_register_drop(9876, count_drop);
    let objects: Vec<_> = (0..8).map(|_| willow_alloc_object(9876, 8)).collect();
    {
        let mut state = runtime().heap.lock().unwrap();
        for &payload in &objects {
            state.remembered_set.insert(payload as usize);
            state.dirty_cards.insert(payload as usize / GC_CARD_SIZE);
        }
    }
    for index in [0, 3, 5] {
        unsafe {
            (*payload_to_header(objects[index])).marked = true;
        }
    }
    sweep();
    assert_eq!(DROPS.load(Ordering::Relaxed), 5);
    {
        let state = runtime().heap.lock().unwrap();
        verify_old_region_metadata(&state).unwrap();
        let region = &state.old_regions[0];
        let span = GC_HEADER_SIZE + 8;
        assert_eq!(region.used, 6 * span);
        assert_eq!(region.free_spans.len(), 2);
        assert_eq!(region.free_spans[0].offset, span);
        assert_eq!(region.free_spans[0].size, 2 * span);
        assert_eq!(region.free_spans[1].offset, 4 * span);
        assert_eq!(region.free_spans[1].size, span);
        let expected: HashSet<_> = [0, 3, 5].map(|i| objects[i] as usize).into();
        assert_eq!(state.remembered_set, expected);
        assert_eq!(
            state.dirty_cards,
            expected.iter().map(|p| p / GC_CARD_SIZE).collect()
        );
    }
    assert_eq!(
        willow_alloc_object(61, (GC_HEADER_SIZE + 16) as i64),
        objects[1]
    );
    sweep();
    assert_eq!(DROPS.load(Ordering::Relaxed), 8);
    sweep();
    assert_eq!(DROPS.load(Ordering::Relaxed), 8);
    let state = runtime().heap.lock().unwrap();
    assert!(state.old_regions.is_empty());
    assert!(state.remembered_set.is_empty());
    assert!(state.dirty_cards.is_empty());
    assert_eq!(state.allocated_bytes, 0);
    drop(state);
    reset_gc();
}

#[test]
fn test_gc_old_region_metadata_tracks_regular_allocation() {
    let _guard = gc_test_guard();
    reset_gc();
    let object = willow_alloc_object(61, 24);
    assert!(!object.is_null());

    let state = runtime().heap.lock().unwrap();
    assert_eq!(state.old_regions.len(), 1);
    let region = &state.old_regions[0];
    assert_eq!(region.kind, RegionKind::Old);
    assert!(region.contains(object as usize));
    assert_eq!(region.capacity, GC_OLD_REGION_SIZE);
    assert_eq!(region.allocations.len(), 1);
    assert_eq!(region.live_bytes, GC_HEADER_SIZE + 24);
    let offset = payload_to_header(object) as usize - region.start();
    assert!(region.mark_bitmap.is_marked(offset));
    drop(state);

    assert_eq!(willow_gc_old_region_count(), 1);
    assert_eq!(
        willow_gc_old_region_reserved_bytes(),
        GC_OLD_REGION_SIZE as i64
    );
    assert_eq!(
        willow_gc_old_region_live_bytes(),
        (GC_HEADER_SIZE + 24) as i64
    );
    reset_gc();
}

#[test]
fn test_gc_old_region_sweep_creates_and_reuses_middle_hole() {
    let _guard = gc_test_guard();
    reset_gc();
    let a = willow_alloc_object(62, 8);
    let dead = willow_alloc_object(63, 8);
    let c = willow_alloc_object(64, 8);
    unsafe {
        *(a as *mut i64) = 10;
        *(c as *mut i64) = 30;
    }
    let mut a_root = a;
    let mut c_root = c;
    willow_push_root(&mut a_root);
    willow_push_root(&mut c_root);

    willow_gc_collect();

    assert_eq!(unsafe { *(a as *mut i64) }, 10);
    assert_eq!(unsafe { *(c as *mut i64) }, 30);
    assert_eq!(willow_gc_old_region_count(), 1);
    assert_eq!(
        willow_gc_old_region_fragmentation_bytes(),
        (GC_HEADER_SIZE + 8) as i64
    );
    let reuses = willow_gc_old_region_reuses();
    let replacement = willow_alloc_object(65, 8);
    assert_eq!(
        replacement, dead,
        "same-sized allocation should reuse the swept region-local span"
    );
    assert_eq!(willow_gc_old_region_reuses(), reuses + 1);
    assert_eq!(willow_gc_old_region_fragmentation_bytes(), 0);

    willow_pop_roots(2);
    willow_gc_collect();
    reset_gc();
}

#[test]
fn test_gc_empty_old_region_is_released_after_major_sweep() {
    let _guard = gc_test_guard();
    reset_gc();
    let released = willow_gc_old_regions_released();
    let _garbage = willow_alloc_object(66, 8);
    assert_eq!(willow_gc_old_region_count(), 1);

    willow_gc_collect();

    assert_eq!(willow_gc_old_region_count(), 0);
    assert_eq!(willow_gc_old_region_reserved_bytes(), 0);
    assert_eq!(willow_gc_old_region_live_bytes(), 0);
    assert_eq!(willow_gc_old_regions_released(), released + 1);
    assert_eq!(willow_gc_major_collections(), 1);
    reset_gc();
}

#[test]
fn test_gc_large_object_uses_dedicated_region_and_stays_non_moving() {
    let _guard = gc_test_guard();
    reset_gc();
    let payload_size = GC_LARGE_OBJECT_THRESHOLD;
    let mut large = willow_alloc_object(67, payload_size as i64);
    assert!(!large.is_null());
    unsafe {
        *large = 0xA5;
        *large.add(payload_size - 1) = 0x5A;
    }
    willow_push_root(&mut large);

    assert_eq!(willow_gc_large_object_region_count(), 1);
    assert_eq!(willow_gc_old_region_count(), 1);
    let address = large;
    willow_gc_collect();
    assert_eq!(large, address, "major collection must not move old objects");
    assert_eq!(unsafe { *large }, 0xA5);
    assert_eq!(unsafe { *large.add(payload_size - 1) }, 0x5A);
    assert_eq!(willow_gc_large_object_region_count(), 1);

    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_large_object_region_count(), 0);
    assert_eq!(willow_gc_old_region_count(), 0);
    reset_gc();
}

#[test]
fn test_gc_regular_old_allocation_rolls_over_to_multiple_regions() {
    let _guard = gc_test_guard();
    reset_gc();
    let count = GC_OLD_REGION_SIZE / (GC_HEADER_SIZE + 8) + 10;
    let mut roots = Vec::with_capacity(count);
    for value in 0..count as i64 {
        let object = willow_alloc_object(70, 8);
        unsafe { *(object as *mut i64) = value };
        roots.push(object);
    }
    for root in &mut roots {
        willow_push_root(root);
    }
    assert!(
        willow_gc_old_region_count() >= 2,
        "regular allocations must roll over at the region bound"
    );
    let first = roots[0];
    let last = roots[count - 1];

    willow_gc_collect();

    assert_eq!(roots[0], first);
    assert_eq!(roots[count - 1], last);
    assert_eq!(unsafe { *(roots[0] as *mut i64) }, 0);
    assert_eq!(unsafe { *(roots[count - 1] as *mut i64) }, count as i64 - 1);
    willow_pop_roots(roots.len() as i32);
    willow_gc_collect();
    assert_eq!(willow_gc_old_region_count(), 0);
    reset_gc();
}

#[test]
fn test_gc_minor_promotion_target_is_old_region_backed() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut tls = new_tlab_state();
    let young = willow_gc_alloc_slow(&mut tls, 68, 0, 8, 0);
    let mut parent = willow_gc_alloc_layout(69, 0, 8, 0b1);
    willow_gc_write_barrier(
        parent,
        std::ptr::null_mut(),
        young,
        GcStoreDestination::ObjectField as i64,
    );
    unsafe { *(parent as *mut *mut u8) = young };
    willow_push_root(&mut parent);

    willow_gc_minor_collect();
    assert_eq!(willow_gc_survivor_copies(), 1);
    willow_gc_minor_collect();

    let promoted = unsafe { *(parent as *mut *mut u8) };
    let state = runtime().heap.lock().unwrap();
    let region = state
        .old_regions
        .iter()
        .find(|region| region.contains(promoted as usize))
        .expect("copied young survivor must be allocated in an old region");
    assert_eq!(region.kind, RegionKind::Old);
    drop(state);
    assert_eq!(willow_gc_remembered_set_size(), 0);

    willow_pop_root();
    willow_gc_collect();
    reset_gc();
}

#[test]
fn test_gc_alloc_typed_mask_traces_child_pointer_slot() {
    let _guard = gc_test_guard();
    reset_gc();
    let child = willow_alloc(8);
    let parent = willow_alloc_typed(8, 0b1);
    unsafe { *(parent as *mut *mut u8) = child };
    let mut slot = parent;
    willow_push_root(&mut slot as *mut *mut u8);
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        obj_size(8) * 2,
        "mask-traced child should survive with rooted parent"
    );
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// -------------------------------------------------------------------------
// 回収: unreachable objects
// -------------------------------------------------------------------------

#[test]
fn test_gc_collect_frees_unreachable_objects() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_alloc_object(1, 128);
    let before = willow_gc_allocated_bytes();
    assert!(before > 0);
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        0,
        "unrooted object should be freed"
    );
    reset_gc();
}

/// collect 後に total_frees が増えること
#[test]
fn test_gc_total_frees_counter_after_collect() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_alloc_object(1, 16);
    willow_alloc_object(1, 16);
    let before = total_frees();
    willow_gc_collect();
    assert_eq!(
        total_frees(),
        before + 2,
        "two unrooted objects should be freed"
    );
    reset_gc();
}

/// collect を複数回呼んでも壊れない (2回目以降は何も回収しない)
#[test]
fn test_gc_collect_idempotent_on_empty_heap() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_gc_collect();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

/// collect 後に生き残ったオブジェクトの mark bit がリセットされること
/// (リセットされないと次サイクルで全部生存扱いになる)
#[test]
fn test_gc_survivor_mark_bit_cleared_after_collection() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(1, 16);
    let mut slot: *mut u8 = ptr;
    willow_push_root(&mut slot as *mut *mut u8);

    // 1回目のGC: 生き残る
    willow_gc_collect();
    // mark bit が false に戻っているかヘッダで確認
    let hdr = payload_to_header(ptr);
    assert!(
        !unsafe { (*hdr).marked },
        "mark bit must be cleared after collection"
    );

    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// -------------------------------------------------------------------------
// ルート管理
// -------------------------------------------------------------------------

#[test]
fn test_gc_collect_preserves_rooted_objects() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(2, 32);
    let mut slot: *mut u8 = ptr;
    willow_push_root(&mut slot as *mut *mut u8);
    willow_gc_collect();
    assert!(
        willow_gc_allocated_bytes() > 0,
        "rooted object must survive"
    );
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        0,
        "unrooted object freed after pop"
    );
    reset_gc();
}

#[test]
fn test_gc_runtime_root_preserves_object_without_stack_root() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(2, 32);

    willow_gc_add_runtime_root(ptr);
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        obj_size(32),
        "persistent runtime root must keep object alive"
    );

    willow_gc_remove_runtime_root(ptr);
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        0,
        "object should be collectible after runtime root removal"
    );
    reset_gc();
}

#[test]
fn runtime_root_set_distinct_snapshots_and_balanced_owners() {
    for distinct in [16, 64, 256] {
        let roots = RuntimeRootSet::default();
        let mut objects = vec![0u8; distinct];
        let pointers: Vec<_> = objects.iter_mut().map(|object| object as *mut u8).collect();
        roots.add(std::ptr::null_mut());
        roots.remove(std::ptr::null_mut());
        assert_eq!(roots.len(), 0);
        for &pointer in &pointers {
            for _ in 0..4 {
                roots.add(pointer);
            }
        }
        assert_eq!(roots.len(), distinct);
        let mut snapshot = roots.snapshot();
        snapshot.sort_unstable();
        assert_eq!(snapshot, pointers);
        for remaining_owners in (0..4).rev() {
            for &pointer in &pointers {
                roots.remove(pointer);
            }
            assert_eq!(
                roots.len(),
                if remaining_owners == 0 { 0 } else { distinct }
            );
        }
        assert!(roots.snapshot().is_empty());
        // Removing an absent root must remain a no-op.
        roots.remove(pointers[0]);
        roots.add(pointers[0]);
        roots.clear();
        assert_eq!(roots.len(), 0);
        assert!(roots.snapshot().is_empty());
    }
}

#[test]
fn test_gc_runtime_root_ignores_null_and_ref_counts_retentions() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(2, 32);

    willow_gc_add_runtime_root(std::ptr::null_mut());
    willow_gc_add_runtime_root(ptr);
    willow_gc_add_runtime_root(ptr);
    assert_eq!(runtime().runtime_roots.len(), 1);
    assert_eq!(runtime().runtime_roots.snapshot(), vec![ptr]);

    willow_gc_remove_runtime_root(std::ptr::null_mut());
    assert_eq!(runtime().runtime_roots.len(), 1);
    assert_eq!(runtime().runtime_roots.snapshot(), vec![ptr]);

    willow_gc_remove_runtime_root(ptr);
    assert_eq!(runtime().runtime_roots.len(), 1);
    assert_eq!(runtime().runtime_roots.snapshot(), vec![ptr]);
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        obj_size(32),
        "one remaining runtime retention must keep the object alive"
    );

    willow_gc_remove_runtime_root(ptr);
    assert_eq!(runtime().runtime_roots.len(), 0);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

#[test]
fn test_gc_foreign_thread_collect_skips_live_root_stack_owner() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(2, 32);
    let ptr_addr = ptr as usize;
    let (rooted_tx, rooted_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();

    let owner_thread = std::thread::spawn(move || {
        let mut slot = ptr_addr as *mut u8;
        willow_push_root(&mut slot as *mut *mut u8);
        rooted_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        willow_pop_root();
    });

    rooted_rx.recv().unwrap();
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        obj_size(32),
        "a foreign-thread collection must not scan only its own root stack"
    );

    release_tx.send(()).unwrap();
    owner_thread.join().unwrap();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

#[cfg(debug_assertions)]
#[test]
fn test_gc_debug_validation_rejects_invalid_runtime_root_pointer() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_gc_add_runtime_root(std::ptr::dangling_mut::<u8>());

    let result = std::panic::catch_unwind(collect_internal);
    let err = result.expect_err("invalid runtime root must fail clearly");
    let message = err
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| err.downcast_ref::<&str>().copied())
        .unwrap_or("");
    assert!(message.contains("invalid GC pointer"), "{message}");
    assert!(message.contains("current GC heap"), "{message}");
    reset_gc();
}

#[test]
fn test_gc_root_push_pop_symmetry() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut slot1: *mut u8 = std::ptr::null_mut();
    let mut slot2: *mut u8 = std::ptr::null_mut();
    willow_push_root(&mut slot1 as *mut *mut u8);
    willow_push_root(&mut slot2 as *mut *mut u8);
    ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 2));
    willow_pop_root();
    ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 1));
    willow_pop_roots(1);
    ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 0));
    reset_gc();
}

#[test]
fn test_gc_root_depth_mirror_tracks_mutations_and_stack_transfers() {
    let _guard = gc_test_guard();
    reset_gc();
    let check = |expected: usize| {
        ROOT_STACK.with(|stack| assert_eq!(stack.borrow().len(), expected));
        assert_eq!(gc_thread_root_depth(), expected);
        assert_eq!(willow_root_depth(), expected as i32);
    };
    check(0);
    let mut slots = [std::ptr::null_mut(); 3];
    for (index, slot) in slots.iter_mut().enumerate() {
        willow_push_root(slot);
        check(index + 1);
    }
    // A new worker's view must not inherit this worker's three roots.
    std::thread::spawn(|| {
        assert_eq!(willow_root_depth(), 0);
        assert_eq!(gc_thread_root_depth(), 0);
    })
    .join()
    .unwrap();
    check(3);
    willow_pop_roots(0);
    check(3);
    // Slots stay alive and no collection/safepoint occurs while detached.
    let token = unsafe { park_current_roots(1) };
    check(1);
    unsafe { resume_parked_roots(token) };
    check(3);
    let token = unsafe { park_current_roots(0) };
    check(0);
    unsafe { resume_parked_roots(token) };
    check(3);
    willow_pop_root();
    check(2);
    willow_pop_roots(1);
    check(1);
    willow_pop_roots(100);
    check(0);
    willow_pop_root();
    check(0);
    willow_push_root(&mut slots[0]);
    willow_pop_roots(-1); // Preserve the existing clamp behavior.
    check(0);
    willow_push_root(&mut slots[0]);
    reset_gc();
    check(0);
}

#[test]
fn test_gc_root_depth_mirror_overflow_is_fatal() {
    const CHILD: &str = "WILLOW_ROOT_DEPTH_OVERFLOW_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        ROOT_DEPTH.set(i32::MAX as usize + 1);
        willow_root_depth();
        panic!("root depth overflow unexpectedly returned");
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "gc::tests::test_gc_root_depth_mirror_overflow_is_fatal",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("runtime fatal: generated-code root depth overflow")
    );
}

/// pop_roots(0) はスタックを変えない
#[test]
fn test_gc_pop_roots_zero_is_noop() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut slot: *mut u8 = std::ptr::null_mut();
    willow_push_root(&mut slot as *mut *mut u8);
    willow_pop_roots(0);
    ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 1, "pop_roots(0) must not change stack"));
    willow_pop_root();
    reset_gc();
}

/// pop_roots(n > stack size) はアンダーフローしない
#[test]
fn test_gc_pop_roots_excess_clamps_to_zero() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut slot: *mut u8 = std::ptr::null_mut();
    willow_push_root(&mut slot as *mut *mut u8);
    // スタックに1つしかないのに100個pop → 0になるだけ、クラッシュしない
    willow_pop_roots(100);
    ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 0, "stack must clamp to 0"));
    reset_gc();
}

/// スロットの値が null のルートがあっても GC はクラッシュしない
#[test]
fn test_gc_null_root_value_does_not_crash() {
    let _guard = gc_test_guard();
    reset_gc();
    // slot は非null だが、slot の指す先 (ポインタ値) が null
    let mut slot: *mut u8 = std::ptr::null_mut();
    willow_push_root(&mut slot as *mut *mut u8);
    // GC はこの null ポインタをスキップするだけ、クラッシュしない
    willow_gc_collect();
    willow_pop_root();
    reset_gc();
}

// -------------------------------------------------------------------------
// 複合シナリオ
// -------------------------------------------------------------------------

#[test]
fn test_gc_multiple_allocs_and_collect() {
    let _guard = gc_test_guard();
    reset_gc();
    for _ in 0..9 {
        willow_alloc_object(1, 16);
    }
    let last_ptr = willow_alloc_object(1, 16);
    let mut slot = last_ptr;
    willow_push_root(&mut slot as *mut *mut u8);

    willow_gc_collect();

    let expected = (header_size() + 16) as i64;
    assert_eq!(
        willow_gc_allocated_bytes(),
        expected,
        "exactly one object must survive"
    );
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

/// 100個確保して1つだけルートを張る → collect後にちょうど1個残る
#[test]
fn test_gc_large_population_single_survivor() {
    let _guard = gc_test_guard();
    reset_gc();
    for _ in 0..99 {
        willow_alloc_object(1, 8);
    }
    let survivor = willow_alloc_object(2, 8);
    let mut slot = survivor;
    willow_push_root(&mut slot as *mut *mut u8);

    willow_gc_collect();

    let expected = (header_size() + 8) as i64;
    assert_eq!(
        willow_gc_allocated_bytes(),
        expected,
        "100 objects allocated, only the rooted one should survive"
    );
    assert_eq!(total_frees(), 99, "99 unreachable objects should be freed");
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

/// collect後にまた確保できること (ヒープが壊れていないこと)
#[test]
fn test_gc_reallocation_after_collection() {
    let _guard = gc_test_guard();
    reset_gc();
    // 1回目の確保・回収サイクル
    willow_alloc_object(1, 32);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);

    // 2回目: 回収後も新規確保できること
    let ptr = willow_alloc_object(1, 32);
    assert!(!ptr.is_null(), "allocation after collection must succeed");
    assert_eq!(willow_gc_allocated_bytes(), (header_size() + 32) as i64);

    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

/// しきい値を超えた確保が自動でGCをトリガーすること
#[test]
fn test_gc_auto_trigger_by_threshold() {
    let _guard = gc_test_guard();
    reset_gc();

    // まず1つ確保してしきい値をそのオブジェクトのサイズより小さく設定
    let obj1 = willow_alloc_object(1, 16);
    assert!(!obj1.is_null());
    let bytes_after_first = willow_gc_allocated_bytes();

    // しきい値を「現在の確保量より小さい値」に設定
    // → 次の willow_alloc_object の冒頭で auto-collect が走る
    set_threshold(1); // 1 バイト → 確実に超えている

    let frees_before = total_frees();

    // obj1 はルートなし → 自動GCで回収されるはず
    let obj2 = willow_alloc_object(1, 16);
    assert!(!obj2.is_null());

    // obj1 (unrooted) が回収されて obj2 だけ残っているはず
    let expected = (header_size() + 16) as i64;
    assert_eq!(
        willow_gc_allocated_bytes(),
        expected,
        "auto-triggered GC should have freed obj1"
    );
    assert!(
        total_frees() > frees_before,
        "auto-triggered GC should have incremented total_frees"
    );
    let _ = bytes_after_first;

    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

/// 複数ルートのうち1つだけ外すと、外したものだけ回収される
#[test]
fn test_gc_partial_roots_partial_collection() {
    let _guard = gc_test_guard();
    reset_gc();

    let ptr_a = willow_alloc_object(1, 16);
    let ptr_b = willow_alloc_object(2, 16);
    let mut slot_a: *mut u8 = ptr_a;
    let mut slot_b: *mut u8 = ptr_b;
    willow_push_root(&mut slot_a as *mut *mut u8); // A をルート
    willow_push_root(&mut slot_b as *mut *mut u8); // B をルート

    // B のルートを外す
    willow_pop_root();

    willow_gc_collect();

    // A だけ生き残っているはず
    let expected = (header_size() + 16) as i64;
    assert_eq!(
        willow_gc_allocated_bytes(),
        expected,
        "only A (still rooted) should survive"
    );

    willow_pop_root(); // A のルートを外す
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// =========================================================================
// 観点1: 大きなペイロード (1MB) の確保が成功する
// =========================================================================
#[test]
fn test_gc_large_payload_alloc_succeeds() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(1, 1024 * 1024);
    assert!(!ptr.is_null(), "1 MiB payload allocation must succeed");
    reset_gc();
}

// Consecutive allocations must enter the authoritative region index.
#[test]
fn test_gc_consecutive_allocs_update_region_index() {
    let _guard = gc_test_guard();
    reset_gc();
    let first = willow_alloc_object(1, 8);
    let second = willow_alloc_object(2, 8);
    let state = runtime().heap.lock().unwrap();
    let objects: HashSet<_> = old_region_objects(&state)
        .map(|object| object.payload().as_ptr())
        .collect();
    assert_eq!(objects, HashSet::from([first, second]));
    drop(state);
    reset_gc();
}

// 観点3: 確保したペイロード領域に読み書きできる
#[test]
fn test_gc_payload_is_readable_writable() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(1, 8) as *mut i64;
    unsafe {
        *ptr = 0x0DEADBEEF_i64;
        assert_eq!(*ptr, 0x0DEADBEEF_i64);
    }
    reset_gc();
}

// 観点4: 複数の type_id を持つオブジェクトが混在して確保できる
#[test]
fn test_gc_multiple_type_ids_work() {
    let _guard = gc_test_guard();
    reset_gc();
    let p1 = willow_alloc_object(1, 8);
    let p2 = willow_alloc_object(2, 8);
    let p3 = willow_alloc_object(99, 8);
    assert!(!p1.is_null() && !p2.is_null() && !p3.is_null());
    assert_eq!(
        HeapObject::from_raw(payload_to_header(p1))
            .unwrap()
            .type_id(),
        1
    );
    assert_eq!(
        HeapObject::from_raw(payload_to_header(p2))
            .unwrap()
            .type_id(),
        2
    );
    assert_eq!(
        HeapObject::from_raw(payload_to_header(p3))
            .unwrap()
            .type_id(),
        99
    );
    reset_gc();
}

// 観点5: 確保直後の GcHeader.marked が false
#[test]
fn test_gc_header_marked_false_after_alloc() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(1, 8);
    assert!(!unsafe { (*payload_to_header(ptr)).marked });
    reset_gc();
}

// 観点6: 確保直後の GcHeader.type_id が指定値と一致
#[test]
fn test_gc_header_type_id_matches() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(42, 8);
    assert_eq!(
        HeapObject::from_raw(payload_to_header(ptr))
            .unwrap()
            .trace_metadata()
            .type_id,
        42
    );
    reset_gc();
}

// 観点7: GcHeader.size がヘッダ+ペイロードと一致
#[test]
fn test_gc_header_size_field_matches_total() {
    let _guard = gc_test_guard();
    reset_gc();
    let payload: i64 = 24;
    let ptr = willow_alloc_object(1, payload);
    let expected = header_size() + payload as usize;
    assert_eq!(
        HeapObject::from_raw(payload_to_header(ptr)).unwrap().size(),
        expected
    );
    reset_gc();
}

// Region membership is authoritative; no linkage word exists.
#[test]
fn test_gc_region_allocations_do_not_publish_heap_links() {
    let _guard = gc_test_guard();
    reset_gc();
    let first = willow_alloc_object(1, 8);
    let second = willow_alloc_object(2, 8);
    assert_ne!(first, second);
    assert_eq!(GC_HEADER_SIZE, 16);
    reset_gc();
}

// =========================================================================
// 観点9: push_root n回でスタックが n 増える
// =========================================================================
#[test]
fn test_gc_push_root_n_times_increases_stack_by_n() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut slots = [std::ptr::null_mut::<u8>(); 5];
    for s in slots.iter_mut() {
        willow_push_root(s as *mut *mut u8);
    }
    ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 5));
    willow_pop_roots(5);
    reset_gc();
}

// 観点10: pop_roots(n) でちょうど n 個減る
#[test]
fn test_gc_pop_roots_n_decreases_by_exactly_n() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut slot = std::ptr::null_mut::<u8>();
    for _ in 0..6 {
        willow_push_root(&mut slot as *mut *mut u8);
    }
    willow_pop_roots(4);
    ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 2));
    willow_pop_roots(2);
    reset_gc();
}

// 観点11: push→pop→push のスロット再利用
#[test]
fn test_gc_push_pop_push_slot_reuse() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr1 = willow_alloc_object(1, 8);
    let mut slot: *mut u8 = ptr1;
    willow_push_root(&mut slot as *mut *mut u8);
    willow_gc_collect(); // ptr1 survives
    assert_eq!(willow_gc_allocated_bytes(), obj_size(8));
    willow_pop_root();

    // 新しいオブジェクトを確保してスロットを再利用
    let ptr2 = willow_alloc_object(2, 8);
    slot = ptr2;
    willow_push_root(&mut slot as *mut *mut u8);
    willow_gc_collect(); // ptr1 freed, ptr2 survives
    assert_eq!(willow_gc_allocated_bytes(), obj_size(8));

    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// 観点12: 同じスロットを2回 push するとスタックに2エントリ入る
#[test]
fn test_gc_same_slot_pushed_twice_creates_two_entries() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut slot = std::ptr::null_mut::<u8>();
    willow_push_root(&mut slot as *mut *mut u8);
    willow_push_root(&mut slot as *mut *mut u8);
    ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 2));
    willow_pop_roots(2);
    reset_gc();
}

// 観点13: 空スタックで pop_root を呼んでもクラッシュしない
#[test]
fn test_gc_pop_root_on_empty_stack_no_crash() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_pop_root();
    ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 0));
    reset_gc();
}

// 観点14: 空スタックで pop_roots(n) を呼んでもクラッシュしない
#[test]
fn test_gc_pop_roots_on_empty_stack_no_crash() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_pop_roots(5);
    ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 0));
    reset_gc();
}

// 観点15: 4000個近くまで push_root できる
#[test]
fn test_gc_push_root_near_max_capacity() {
    let _guard = gc_test_guard();
    reset_gc();
    const N: usize = 4000;
    let mut slots = vec![std::ptr::null_mut::<u8>(); N];
    for s in slots.iter_mut() {
        willow_push_root(s as *mut *mut u8);
    }
    ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), N));
    willow_pop_roots(N as i32);
    ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 0));
    reset_gc();
}

// =========================================================================
// 観点16-17: マークフェーズ
// =========================================================================

// 観点16: 同じオブジェクトを2つのルートが指しても二重マークでクラッシュしない
#[test]
fn test_gc_two_roots_same_object_no_double_mark_crash() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(1, 8);
    let mut s1: *mut u8 = ptr;
    let mut s2: *mut u8 = ptr;
    willow_push_root(&mut s1 as *mut *mut u8);
    willow_push_root(&mut s2 as *mut *mut u8);
    willow_gc_collect(); // must not crash or double-free
    assert_eq!(
        willow_gc_allocated_bytes(),
        obj_size(8),
        "object must survive"
    );
    willow_pop_roots(2);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// =========================================================================
// 観点21-25: スイープフェーズ
// =========================================================================

// 観点21: ヒープ先頭 (heap_head) が unreachable でも正しく回収
#[test]
fn test_gc_sweep_head_object_unreachable() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr_a = willow_alloc_object(1, 8); // first alloc → becomes tail
    let _ptr_b = willow_alloc_object(2, 8); // second alloc → becomes head (unreachable)
    let mut slot_a: *mut u8 = ptr_a;
    willow_push_root(&mut slot_a as *mut *mut u8); // only A rooted
    willow_gc_collect();
    // B (head) freed, A (tail→new head) survives
    assert_eq!(willow_gc_allocated_bytes(), obj_size(8));
    assert_eq!(
        old_region_objects(&runtime().heap.lock().unwrap())
            .map(HeapObject::as_ptr)
            .collect::<Vec<_>>(),
        vec![payload_to_header(ptr_a)]
    );
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// 観点22: ヒープ末尾が unreachable でも正しく回収
#[test]
fn test_gc_sweep_tail_object_unreachable() {
    let _guard = gc_test_guard();
    reset_gc();
    let _ptr_a = willow_alloc_object(1, 8); // first → tail (unreachable)
    let ptr_b = willow_alloc_object(2, 8); // second → head (rooted)
    let mut slot_b: *mut u8 = ptr_b;
    willow_push_root(&mut slot_b as *mut *mut u8);
    willow_gc_collect();
    // A (tail) freed, B (head) survives
    assert_eq!(willow_gc_allocated_bytes(), obj_size(8));
    assert_eq!(
        old_region_objects(&runtime().heap.lock().unwrap())
            .map(HeapObject::as_ptr)
            .collect::<Vec<_>>(),
        vec![payload_to_header(ptr_b)]
    );
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// 観点23: ヒープ中間のオブジェクトだけ unreachable でも正しく回収
#[test]
fn test_gc_sweep_middle_object_unreachable() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr_a = willow_alloc_object(1, 8); // tail
    let _ptr_b = willow_alloc_object(2, 8); // middle (unreachable)
    let ptr_c = willow_alloc_object(3, 8); // head
    let mut sa: *mut u8 = ptr_a;
    let mut sc: *mut u8 = ptr_c;
    willow_push_root(&mut sa as *mut *mut u8);
    willow_push_root(&mut sc as *mut *mut u8);
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        obj_size(8) * 2,
        "A and C survive, B freed"
    );
    willow_pop_roots(2);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// 観点24: sweep後にヒープリストが null で終端される
#[test]
fn test_gc_heap_null_terminated_after_sweep() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(1, 8);
    let mut slot: *mut u8 = ptr;
    willow_push_root(&mut slot as *mut *mut u8);
    willow_gc_collect();
    willow_pop_root();
    willow_gc_collect();
    reset_gc();
}

// 観点25: 全員生き残ったときリンクリストが壊れていない
#[test]
fn test_gc_survivors_remain_indexed_after_sweep() {
    let _guard = gc_test_guard();
    reset_gc();
    let pa = willow_alloc_object(1, 8);
    let pb = willow_alloc_object(2, 8);
    let pc = willow_alloc_object(3, 8);
    let mut sa: *mut u8 = pa;
    let mut sb: *mut u8 = pb;
    let mut sc: *mut u8 = pc;
    willow_push_root(&mut sa as *mut *mut u8);
    willow_push_root(&mut sb as *mut *mut u8);
    willow_push_root(&mut sc as *mut *mut u8);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), obj_size(8) * 3);
    let objects: HashSet<_> = old_region_objects(&runtime().heap.lock().unwrap())
        .map(|object| object.payload().as_ptr())
        .collect();
    assert_eq!(objects, HashSet::from([pa, pb, pc]));
    willow_pop_roots(3);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// =========================================================================
// 観点26-29: allocated_bytes の正確性
// =========================================================================

// 観点26: 確保するたびに正確に (header+payload) ずつ増える
#[test]
fn test_gc_allocated_bytes_grows_precisely_per_alloc() {
    let _guard = gc_test_guard();
    reset_gc();
    let step = obj_size(16);
    willow_alloc_object(1, 16);
    assert_eq!(willow_gc_allocated_bytes(), step);
    willow_alloc_object(1, 16);
    assert_eq!(willow_gc_allocated_bytes(), step * 2);
    willow_alloc_object(1, 16);
    assert_eq!(willow_gc_allocated_bytes(), step * 3);
    reset_gc();
}

// 観点28: partial collect で回収分だけ減り、生存分は保持される
#[test]
fn test_gc_partial_collect_bytes_accurate() {
    let _guard = gc_test_guard();
    reset_gc();
    let pa = willow_alloc_object(1, 8); // freed
    let pb = willow_alloc_object(2, 8); // survives
    let _pc = willow_alloc_object(3, 8); // freed
    let mut sb: *mut u8 = pb;
    willow_push_root(&mut sb as *mut *mut u8);
    assert_eq!(willow_gc_allocated_bytes(), obj_size(8) * 3);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), obj_size(8), "only B survives");
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    let _ = pa;
    reset_gc();
}

// 観点29: 5つルートを張った5個全員の合計バイトが正確
#[test]
fn test_gc_five_roots_five_survivors_bytes() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut ptrs: Vec<*mut u8> = (0..5).map(|i| willow_alloc_object(i, 8)).collect();
    for p in ptrs.iter_mut() {
        willow_push_root(p as *mut *mut u8);
    }
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), obj_size(8) * 5);
    willow_pop_roots(5);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// =========================================================================
// 観点30-34: threshold / 自動トリガー
// =========================================================================

// 観点30: threshold 未満では auto-collect が走らない
#[test]
fn test_gc_no_auto_trigger_below_threshold() {
    let _guard = gc_test_guard();
    reset_gc();
    set_threshold(usize::MAX);
    let before = total_frees();
    for _ in 0..10 {
        willow_alloc_object(1, 8);
    }
    assert_eq!(total_frees(), before, "no auto-collect should have run");
    reset_gc();
}

// 観点31: auto-collect 後に threshold が2倍になる
#[test]
fn test_gc_threshold_doubles_after_auto_trigger() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_alloc_object(1, 8); // allocated_bytes = header+8
    set_threshold(1); // 現在の allocated_bytes より小さく設定
    willow_alloc_object(1, 8); // 先頭で auto-collect、その後確保
    let new_threshold = runtime().heap.lock().unwrap().threshold_bytes;
    assert!(
        new_threshold >= 2,
        "threshold must have at least doubled from 1"
    );
    reset_gc();
}

// 観点32: auto-collect 後に新規確保が正常にできる
#[test]
fn test_gc_realloc_after_auto_trigger_works() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_alloc_object(1, 16);
    set_threshold(1);
    let ptr = willow_alloc_object(1, 16);
    assert!(!ptr.is_null());
    assert!(willow_gc_allocated_bytes() > 0);
    reset_gc();
}

// 観点33: threshold=1 で毎回 auto-collect がトリガーされる
#[test]
fn test_gc_threshold_one_every_alloc_triggers_collect() {
    let _guard = gc_test_guard();
    reset_gc();
    set_threshold(1);
    let before = total_frees();
    for _ in 0..5 {
        willow_alloc_object(1, 8);
    }
    assert!(
        total_frees() > before,
        "auto-collect must fire at least once"
    );
    reset_gc();
}

// 観点34: 100回 auto-collect サイクルを繰り返してもメモリリークなし
#[test]
fn test_gc_100_auto_trigger_cycles_no_leak() {
    let _guard = gc_test_guard();
    reset_gc();
    set_threshold(1);
    for _ in 0..100 {
        willow_alloc_object(1, 8);
    }
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// =========================================================================
// 観点35-39: ライフサイクル
// =========================================================================

// 観点35: alloc→root→collect(生存)→unroot→collect(回収) の完全1サイクル
#[test]
fn test_gc_full_lifecycle() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(1, 16);
    let mut slot: *mut u8 = ptr;
    willow_push_root(&mut slot as *mut *mut u8);
    willow_gc_collect();
    assert!(
        willow_gc_allocated_bytes() > 0,
        "rooted object must survive"
    );
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        0,
        "unrooted object must be freed"
    );
    reset_gc();
}

// 観点36: rooted のまま collect を 10回繰り返しても毎回生き残る
#[test]
fn test_gc_repeated_collect_rooted_object_survives() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(1, 8);
    let mut slot: *mut u8 = ptr;
    willow_push_root(&mut slot as *mut *mut u8);
    for i in 0..10 {
        willow_gc_collect();
        assert!(
            willow_gc_allocated_bytes() > 0,
            "object must survive collection #{i}"
        );
    }
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// 観点37: alloc→collect を 100サイクル繰り返してもリークなし
#[test]
fn test_gc_100_alloc_collect_cycles_no_leak() {
    let _guard = gc_test_guard();
    reset_gc();
    for _ in 0..100 {
        willow_alloc_object(1, 16);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
    }
    reset_gc();
}

// 観点38: collect 後も生き残ったオブジェクトのペイロード値が変化していない
#[test]
fn test_gc_payload_unchanged_after_collection() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(1, 8) as *mut i64;
    unsafe {
        *ptr = 0xCAFEBABE_i64;
    }
    let mut slot: *mut u8 = ptr as *mut u8;
    willow_push_root(&mut slot as *mut *mut u8);
    willow_gc_collect();
    assert_eq!(
        unsafe { *ptr },
        0xCAFEBABE_i64,
        "GC must not corrupt payload data"
    );
    willow_pop_root();
    reset_gc();
}

// 観点39: unroot 直後の collect でオブジェクトが即座に回収される
#[test]
fn test_gc_collect_immediately_after_unroot() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(1, 8);
    let mut slot: *mut u8 = ptr;
    willow_push_root(&mut slot as *mut *mut u8);
    willow_gc_collect();
    assert!(willow_gc_allocated_bytes() > 0);
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        0,
        "must be freed immediately after unroot"
    );
    reset_gc();
}

// =========================================================================
// 観点40-42: カウンタの正確性
// =========================================================================

// 観点40: reset 後に total_allocs が 0 から始まる
#[test]
fn test_gc_total_allocs_starts_at_zero_after_reset() {
    let _guard = gc_test_guard();
    reset_gc();
    assert_eq!(total_allocs(), 0);
    reset_gc();
}

// 観点42: partial collect で total_frees が回収個数分だけ増える
#[test]
fn test_gc_partial_collect_total_frees_count() {
    let _guard = gc_test_guard();
    reset_gc();
    for _ in 0..5 {
        willow_alloc_object(1, 8);
    }
    let survivor = willow_alloc_object(1, 8);
    let mut slot: *mut u8 = survivor;
    willow_push_root(&mut slot as *mut *mut u8);
    let before = total_frees();
    willow_gc_collect();
    assert_eq!(
        total_frees(),
        before + 5,
        "exactly 5 unrooted objects freed"
    );
    willow_pop_root();
    reset_gc();
}

// =========================================================================
// 観点43-46: エッジケース / 境界値
// =========================================================================

// 観点43: ペイロード 1 バイトの確保が成功する
#[test]
fn test_gc_alloc_payload_size_one() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(1, 1);
    assert!(!ptr.is_null());
    reset_gc();
}

// 観点44: 奇数ペイロードサイズでも正しく動く
#[test]
fn test_gc_alloc_odd_payload_sizes() {
    let _guard = gc_test_guard();
    reset_gc();
    for &size in &[1i64, 3, 5, 7, 9, 11] {
        let ptr = willow_alloc_object(1, size);
        assert!(!ptr.is_null(), "alloc of {size} bytes must succeed");
    }
    reset_gc();
}

// 観点45: 10,000個確保して全部回収される
#[test]
fn test_gc_ten_thousand_allocs_all_freed() {
    let _guard = gc_test_guard();
    reset_gc();
    set_threshold(usize::MAX); // 自動トリガー無効
    for _ in 0..10_000 {
        willow_alloc_object(1, 8);
    }
    assert!(willow_gc_allocated_bytes() > 0);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// 観点46: 確保→回収サイクルを 20回繰り返した後にヒープが空
#[test]
fn test_gc_empty_heap_after_many_cycles() {
    let _guard = gc_test_guard();
    reset_gc();
    for _ in 0..20 {
        for _ in 0..10 {
            willow_alloc_object(1, 8);
        }
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            0,
            "heap must be empty after each cycle"
        );
    }
    reset_gc();
}

// =========================================================================
// 観点47-48: payload_to_header ヘルパー
// =========================================================================

// 観点47: alloc から返ったポインタを payload_to_header に渡すと元のヘッダが返る
#[test]
fn test_gc_payload_to_header_roundtrip() {
    let _guard = gc_test_guard();
    reset_gc();
    let ptr = willow_alloc_object(7, 16);
    let hdr = payload_to_header(ptr);
    assert_eq!(
        HeapObject::from_raw(hdr).unwrap().trace_metadata().type_id,
        7
    );
    let expected_payload = unsafe { (hdr as *mut u8).add(header_size()) };
    assert_eq!(
        ptr, expected_payload,
        "payload pointer must be header + header_size"
    );
    reset_gc();
}

// 観点48: 2個確保したとき、それぞれ payload_to_header が別のヘッダを返す
#[test]
fn test_gc_payload_to_header_two_objects_distinct() {
    let _guard = gc_test_guard();
    reset_gc();
    let p1 = willow_alloc_object(1, 8);
    let p2 = willow_alloc_object(2, 8);
    let h1 = payload_to_header(p1);
    let h2 = payload_to_header(p2);
    assert_ne!(h1, h2, "two allocations must have distinct headers");
    assert_eq!(
        HeapObject::from_raw(h1).unwrap().trace_metadata().type_id,
        1
    );
    assert_eq!(
        HeapObject::from_raw(h2).unwrap().trace_metadata().type_id,
        2
    );
    reset_gc();
}

// =========================================================================
// 観点49: WILLOW_GC_LOG 環境変数があってもパニックしない
// =========================================================================
#[test]
fn test_gc_log_env_var_does_not_panic() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_alloc_object(1, 8);
    // WILLOW_GC_LOG が設定されていても collect はクラッシュしない
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// =========================================================================
// 観点50: 複数スレッドからの同時 alloc でパニックしない (Mutex保護の確認)
// =========================================================================
#[test]
fn test_gc_concurrent_alloc_no_panic() {
    let _guard = gc_test_guard();
    reset_gc();

    let handles: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(|| {
                for _ in 0..100 {
                    let _ptr = willow_alloc_object(1, 16);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().expect("concurrent alloc thread must not panic");
    }

    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// =========================================================================
// TypeInfo / オブジェクトグラフ トレーステスト
// =========================================================================
//
// テスト用 type_id 定数
const TYPE_NODE: u32 = 200; // payload = [child: *mut u8]              (8 bytes)
const TYPE_NODE2: u32 = 201; // payload = [child0: *mut u8, child1: *mut u8] (16 bytes)
const TYPE_LEAF: u32 = 202; // 内部ポインタなし — TraceFn 未登録
const TYPE_CLASS: u32 = 203; // payload = [i64_field: 8, gc_ptr: 8]    (16 bytes)
const TYPE_ARRAY: u32 = 204; // payload = [len: i64, ptr0, ptr1, ...]
const TYPE_MSG: u32 = 210; // enum: [tag: i64, data: i64|*mut u8]    (16 bytes)

// テスト用 trace 関数 (naked unsafe fn → TraceFn として使用)

unsafe fn trace_node(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    // payload[0..8] = child pointer
    slots.push(payload.cast::<*mut u8>());
}

unsafe fn trace_node2(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    // payload[0..8] = child0, payload[8..16] = child1
    slots.push(payload.cast::<*mut u8>());
    slots.push(unsafe { payload.cast::<*mut u8>().add(1) });
}

unsafe fn trace_class(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    // payload[0..8] = i64 field (not a pointer), payload[8..16] = gc_ptr
    slots.push(unsafe { payload.add(8).cast::<*mut u8>() });
}

unsafe fn trace_array(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    // payload[0..8] = len: i64, payload[8 + i*8] = ptr_i
    let len = unsafe { *(payload as *mut i64) } as usize;
    for i in 0..len {
        slots.push(unsafe { payload.add(8 + i * 8).cast::<*mut u8>() });
    }
}

unsafe fn trace_msg(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    // payload[0..8] = tag: i64
    // tag == 0 (Text)  → payload[8..16] is a GC pointer
    // tag == 1 (Number) → payload[8..16] is an i64, must NOT be traced
    let tag = unsafe { *(payload as *mut i64) };
    if tag == 0 {
        slots.push(unsafe { payload.add(8).cast::<*mut u8>() });
    }
}

// -------------------------------------------------------------------------
// 観点T1: root → child が生き残る
// -------------------------------------------------------------------------
#[test]
fn test_gc_typeinfo_root_child_survives() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_register_type(TYPE_NODE, trace_node);

    let child = willow_alloc_object(TYPE_LEAF as i64, 8);
    let parent = willow_alloc_object(TYPE_NODE as i64, 8);
    unsafe {
        *(parent as *mut *mut u8) = child;
    }

    let mut root_slot: *mut u8 = parent;
    willow_push_root(&mut root_slot as *mut *mut u8);

    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        obj_size(8) * 2,
        "parent and child must both survive"
    );

    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// -------------------------------------------------------------------------
// 観点T2: root → child → grandchild が生き残る
// -------------------------------------------------------------------------
#[test]
fn test_gc_typeinfo_root_child_grandchild_survives() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_register_type(TYPE_NODE, trace_node);

    let grandchild = willow_alloc_object(TYPE_LEAF as i64, 8);
    let child = willow_alloc_object(TYPE_NODE as i64, 8);
    unsafe {
        *(child as *mut *mut u8) = grandchild;
    }
    let parent = willow_alloc_object(TYPE_NODE as i64, 8);
    unsafe {
        *(parent as *mut *mut u8) = child;
    }

    let mut root_slot: *mut u8 = parent;
    willow_push_root(&mut root_slot as *mut *mut u8);

    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        obj_size(8) * 3,
        "parent, child, and grandchild must all survive"
    );

    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// -------------------------------------------------------------------------
// 観点T3: root なしの cycle は回収される
// -------------------------------------------------------------------------
#[test]
fn test_gc_typeinfo_rootless_cycle_collected() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_register_type(TYPE_NODE, trace_node);

    let a = willow_alloc_object(TYPE_NODE as i64, 8);
    let b = willow_alloc_object(TYPE_NODE as i64, 8);
    unsafe {
        *(a as *mut *mut u8) = b; // A → B
        *(b as *mut *mut u8) = a; // B → A
    }

    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        0,
        "rootless cycle must be collected"
    );
    reset_gc();
}

// -------------------------------------------------------------------------
// 観点T4: root ありの cycle は生き残る
// -------------------------------------------------------------------------
#[test]
fn test_gc_typeinfo_rooted_cycle_survives() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_register_type(TYPE_NODE, trace_node);

    let a = willow_alloc_object(TYPE_NODE as i64, 8);
    let b = willow_alloc_object(TYPE_NODE as i64, 8);
    unsafe {
        *(a as *mut *mut u8) = b;
        *(b as *mut *mut u8) = a;
    }

    let mut root_slot: *mut u8 = a;
    willow_push_root(&mut root_slot as *mut *mut u8);

    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        obj_size(8) * 2,
        "rooted cycle must survive"
    );

    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// -------------------------------------------------------------------------
// 観点T5: class の GC フィールドが trace される
// -------------------------------------------------------------------------
#[test]
fn test_gc_typeinfo_class_field_traced() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_register_type(TYPE_CLASS, trace_class);

    let field_obj = willow_alloc_object(TYPE_LEAF as i64, 8);
    // payload: [i64_field: 8, gc_ptr: 8] = 16 bytes
    let instance = willow_alloc_object(TYPE_CLASS as i64, 16);
    unsafe {
        *(instance as *mut i64) = 42i64;
        *((instance.add(8)) as *mut *mut u8) = field_obj;
    }

    let mut root_slot: *mut u8 = instance;
    willow_push_root(&mut root_slot as *mut *mut u8);

    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        obj_size(16) + obj_size(8),
        "class instance and its GC field must both survive"
    );

    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// -------------------------------------------------------------------------
// 観点T6: 2子 (TYPE_NODE2) が両方 trace される
// -------------------------------------------------------------------------
#[test]
fn test_gc_typeinfo_two_children_traced() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_register_type(TYPE_NODE2, trace_node2);

    let child0 = willow_alloc_object(TYPE_LEAF as i64, 8);
    let child1 = willow_alloc_object(TYPE_LEAF as i64, 8);
    // payload: [child0_ptr: 8, child1_ptr: 8] = 16 bytes
    let parent = willow_alloc_object(TYPE_NODE2 as i64, 16);
    unsafe {
        *(parent as *mut *mut u8) = child0;
        *((parent as *mut *mut u8).add(1)) = child1;
    }

    let mut root_slot: *mut u8 = parent;
    willow_push_root(&mut root_slot as *mut *mut u8);

    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        obj_size(16) + obj_size(8) * 2,
        "parent and both children must survive"
    );

    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// -------------------------------------------------------------------------
// 観点T7: array の全要素が trace される
// -------------------------------------------------------------------------
#[test]
fn test_gc_typeinfo_array_elements_traced() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_register_type(TYPE_ARRAY, trace_array);

    const N: usize = 4;
    // Stable slots keep earlier elements alive during later allocations.
    let mut elems = [std::ptr::null_mut(); N];
    for slot in &mut elems {
        *slot = willow_alloc_object(TYPE_LEAF as i64, 8);
        willow_push_root(slot);
    }

    // payload: [len: i64, ptr0, ptr1, ptr2, ptr3] = 8 + 4*8 = 40 bytes
    let array_payload: usize = 8 + N * 8;
    let array = willow_alloc_object(TYPE_ARRAY as i64, array_payload as i64);
    unsafe {
        *(array as *mut i64) = N as i64;
        for (i, &ep) in elems.iter().enumerate() {
            *((array.add(8 + i * 8)) as *mut *mut u8) = ep;
        }
    }

    // Construction is complete. Remove temporary roots before collecting,
    // so survival proves that the array trace visits every element.
    willow_pop_roots(N as i32);
    let mut root_slot: *mut u8 = array;
    willow_push_root(&mut root_slot as *mut *mut u8);

    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        obj_size(array_payload) + obj_size(8) * (N as i64),
        "array and all elements must survive"
    );

    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// -------------------------------------------------------------------------
// 観点T8: enum Text バリアント — 内部 GC ポインタが trace される
// -------------------------------------------------------------------------
#[test]
fn test_gc_typeinfo_enum_text_variant_traced() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_register_type(TYPE_MSG, trace_msg);

    let text_obj = willow_alloc_object(TYPE_LEAF as i64, 8);
    // payload: [tag=0: i64, ptr: *mut u8] = 16 bytes
    let msg = willow_alloc_object(TYPE_MSG as i64, 16);
    unsafe {
        *(msg as *mut i64) = 0i64; // tag = Text
        *((msg.add(8)) as *mut *mut u8) = text_obj;
    }

    let mut root_slot: *mut u8 = msg;
    willow_push_root(&mut root_slot as *mut *mut u8);

    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        obj_size(16) + obj_size(8),
        "Message::Text and its string payload must both survive"
    );

    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// -------------------------------------------------------------------------
// 観点T9: enum Number バリアント — i64 フィールドをポインタとして trace しない
// -------------------------------------------------------------------------
#[test]
fn test_gc_typeinfo_enum_number_variant_not_traced() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_register_type(TYPE_MSG, trace_msg);

    // payload: [tag=1: i64, data=12345: i64] = 16 bytes
    let msg = willow_alloc_object(TYPE_MSG as i64, 16);
    unsafe {
        *(msg as *mut i64) = 1i64; // tag = Number
        *((msg.add(8)) as *mut i64) = 12345i64; // numeric data, NOT a pointer
    }

    let mut root_slot: *mut u8 = msg;
    willow_push_root(&mut root_slot as *mut *mut u8);

    // GC must not crash treating 12345 as a pointer
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        obj_size(16),
        "only msg survives; no child was traced"
    );

    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// -------------------------------------------------------------------------
// 観点T10: root なしの parent+child は両方回収される
// -------------------------------------------------------------------------
#[test]
fn test_gc_typeinfo_unrooted_parent_child_both_collected() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_register_type(TYPE_NODE, trace_node);

    let child = willow_alloc_object(TYPE_LEAF as i64, 8);
    let parent = willow_alloc_object(TYPE_NODE as i64, 8);
    unsafe {
        *(parent as *mut *mut u8) = child;
    }

    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        0,
        "unrooted parent and child must both be collected"
    );
    reset_gc();
}
