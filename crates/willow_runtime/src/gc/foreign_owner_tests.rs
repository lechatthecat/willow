//! GC-04 ownership, progress and unwind regressions.
use super::*;

#[test]
fn registered_then_unregistered_roots_defer_and_report_progress() {
    let _guard = gc_test_guard();
    reset_gc();
    willow_gc_collect();
    let start = gc_progress();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let owner = std::thread::spawn(move || {
        willow_gc_register_mutator();
        let mut slot = willow_alloc_object(2, 32);
        willow_push_root(&mut slot);
        willow_gc_unregister_mutator();
        ready_tx.send(()).unwrap();
        done_rx.recv().unwrap();
        willow_pop_root();
    });
    ready_rx.recv().unwrap();
    willow_gc_collect();
    minor_collect_internal();
    let deferred = gc_progress();
    assert_eq!(deferred.skipped_collections - start.skipped_collections, 2);
    assert_eq!(
        deferred.successful_collections,
        start.successful_collections
    );
    assert_eq!(deferred.last_skip_reason, GcSkipReason::ForeignOwner as u64);
    assert!(deferred.skip_duration_ns > 0);
    assert!(deferred.progress_gap_ns >= deferred.skip_duration_ns);
    assert!(deferred.allocation_bytes_since_success >= 32);
    assert_eq!(willow_gc_allocated_bytes(), obj_size(32));
    done_tx.send(()).unwrap();
    owner.join().unwrap();
    willow_gc_collect();
    let recovered = gc_progress();
    assert_eq!(recovered.skip_duration_ns, 0);
    assert_eq!(recovered.allocation_bytes_since_success, 0);
    assert!(recovered.successful_collections > deferred.successful_collections);
    assert_eq!(willow_gc_allocated_bytes(), 0);
    let mut ffi = GcProgressV1::default();
    assert!(unsafe { willow_gc_progress_v1(&mut ffi) });
    assert_eq!(ffi.successful_collections, recovered.successful_collections);
    assert!(!unsafe { willow_gc_progress_v1(std::ptr::null_mut()) });
    reset_gc();
}

#[test]
fn native_scope_nested_return_cancel_and_panic_release_roots() {
    let _guard = gc_test_guard();
    for count in [1, 16, 256] {
        for panic in [false, true] {
            reset_gc();
            let outer = NativeGcScope::enter();
            let mut prefix = willow_alloc_object(2, 8);
            willow_push_root(&mut prefix);
            let result = std::panic::catch_unwind(|| {
                let _scope = NativeGcScope::enter();
                let mut slots = vec![std::ptr::null_mut(); count];
                for slot in &mut slots {
                    willow_push_root(slot);
                }
                assert!(current_thread_is_registered());
                if panic {
                    panic!("native cancellation unwind");
                }
                // Scope cleanup must not dereference dead lexical slots.
            });
            assert_eq!(result.is_err(), panic);
            assert!(current_thread_is_registered());
            assert_eq!(gc_thread_root_depth(), 1);
            drop(outer); // Explicit cancellation follows the same Drop path.
            assert!(!current_thread_is_registered());
            assert_eq!(gc_thread_root_depth(), 0);
            assert_eq!(registered_mutator_count(), 0);
            std::thread::spawn(|| willow_gc_collect()).join().unwrap();
            assert_eq!(willow_gc_allocated_bytes(), 0);
        }
    }
    reset_gc();
}

#[test]
fn native_registration_and_gc_start_race_has_no_skips_or_stranded_roots() {
    let _guard = gc_test_guard();
    reset_gc();
    let before = willow_gc_skipped_collections();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let worker_barrier = barrier.clone();
    let worker = std::thread::spawn(move || {
        for _ in 0..64 {
            worker_barrier.wait();
            let _scope = NativeGcScope::enter();
            let mut slot = willow_alloc_object(2, 8);
            willow_push_root(&mut slot);
            willow_gc_safepoint();
            // Drop removes roots before unregistering, even during election.
        }
    });
    for _ in 0..64 {
        barrier.wait();
        willow_gc_collect();
    }
    worker.join().unwrap();
    willow_gc_collect();
    assert_eq!(willow_gc_skipped_collections(), before);
    assert_eq!(registered_mutator_count(), 0);
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

#[test]
fn legacy_admission_during_foreign_election_fails_safely() {
    const CHILD: &str = "WILLOW_GC04_ADMISSION_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _collector = cycle_wait::CollectorGuard::try_acquire().unwrap();
        std::thread::spawn(|| {
            let unregister = std::env::var(CHILD).unwrap() == "unregister";
            if unregister {
                willow_gc_register_mutator();
            }
            let mut slot = std::ptr::null_mut();
            willow_push_root(&mut slot);
            if unregister {
                willow_gc_unregister_mutator();
            }
        })
        .join()
        .unwrap();
        panic!("unsafe admission accepted");
    }
    for mode in ["claim", "unregister"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "gc::tests::foreign_owner_tests::legacy_admission_during_foreign_election_fails_safely",
            "--exact",
            "--nocapture",
        ])
        .env(CHILD, mode)
        .output()
        .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("unregistered root ownership during GC")
        );
    }
}

#[test]
fn existing_legacy_owner_can_extend_roots_during_election() {
    let _guard = gc_test_guard();
    reset_gc();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel();
    let (pushed_tx, pushed_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut first = std::ptr::null_mut();
        willow_push_root(&mut first);
        ready_tx.send(()).unwrap();
        go_rx.recv().unwrap();
        let mut second = std::ptr::null_mut();
        willow_push_root(&mut second);
        pushed_tx.send(()).unwrap();
        go_rx.recv().unwrap();
        willow_pop_roots(2);
    });
    ready_rx.recv().unwrap();
    let collector = cycle_wait::CollectorGuard::try_acquire().unwrap();
    go_tx.send(()).unwrap();
    pushed_rx.recv().unwrap();
    assert!(foreign_root_stack_owner_active());
    // Exercise the definitive stopped check, not just the early gate.
    assert!(root_handshake::begin().is_none());
    minor::collect_owned();
    assert_eq!(
        gc_progress().last_skip_reason,
        GcSkipReason::ForeignOwnerAtHandshake as u64
    );
    drop(collector);
    go_tx.send(()).unwrap();
    worker.join().unwrap();
    reset_gc();
}
