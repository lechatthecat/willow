//! Idle ownership spans many collections without a wake/acknowledge per cycle.
use super::*;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn idle_roots_survive_minor_major_and_relocation() {
    let _test = isolated_runtime();
    for relocate in [false, true] {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        let (ready_tx, ready_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let worker = std::thread::spawn(crate::runtime::serving(move || {
            let mutator = MutatorRegistration::new();
            let tls = tlab_state_for_test();
            // SAFETY: The fixture owns the stable TLAB through unregistration. The requested
            // size and tracing metadata describe the object initialized here.
            let mut root = unsafe { willow_gc_alloc_slow(tls.as_ptr(), 1, 0, 8, 0) };
            // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
            unsafe { *root.cast::<u64>() = 123456 };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut root) };
            let before = root as usize;
            let relocation = relocate.then(RelocationSafeScope::enter);
            // SAFETY: only native channels are touched until ownership resumes.
            let idle = unsafe { IdleMutator::enter() };
            ready_tx
                .send((std::thread::current().id(), before))
                .unwrap();
            resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            drop(idle);
            // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
            assert_eq!(unsafe { *root.cast::<u64>() }, 123456);
            assert_eq!(root as usize != before, relocate);
            drop(relocation);
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(1) };
            drop(mutator);
        }));
        let (id, before) = ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        with_stw(crate::gc_telemetry::stops::StopReason::Minor, |coord, _| {
            assert!(coord.parked.contains(&id));
            assert!(coord.stw_pending.is_empty());
            assert_eq!(coord.mutators[&id].relocatable, relocate);
            assert_eq!(all_registered_stack_roots(coord), vec![before as *mut u8]);
        });
        minor::minor_collect_pinning_for_test();
        // Subsequent initial/remark/sweep stops reuse the published slot, which
        // may already have been rewritten by the relocating minor collection.
        willow_gc_collect();
        willow_gc_collect();
        resume_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(runtime().coord.0.lock().unwrap().parked.is_empty());
    }
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn idle_publication_is_reused_across_increasing_stop_counts_and_root_counts() {
    let _test = isolated_runtime();
    for roots in [0, 1, 64, 1024] {
        for cycles in [1, 8, 64] {
            // SAFETY: The test controls its isolated runtime at this reset boundary; no other
            // mutator or subsequent access relies on the discarded heap.
            unsafe { reset_internal_for_test() };
            let (ready_tx, ready_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let worker = std::thread::spawn(crate::runtime::serving(move || {
                let mutator = MutatorRegistration::new();
                // SAFETY: The requested layout describes the initialized words of this object
                // and its type’s tracing or finalization contract.
                let mut slots = vec![unsafe { willow_alloc_object(0, 8) }; roots];
                for slot in &mut slots {
                    // SAFETY: The slot storage stays live and stationary through the matching
                    // pop, including during collection.
                    unsafe { willow_push_root(slot) };
                }
                // SAFETY: slots and managed objects stay untouched during wait.
                let idle = unsafe { IdleMutator::enter() };
                ready_tx.send(std::thread::current().id()).unwrap();
                resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                drop(idle);
                // SAFETY: The removed shadow-stack entries belong to this operation and their
                // slot storage is still live.
                unsafe { willow_pop_roots(roots as i32) };
                drop(mutator);
            }));
            let id = ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let mut publication = None;
            let notifications = (
                runtime().coord.1.notifications(),
                runtime().budget_cv.notifications(),
            );
            for _ in 0..cycles {
                with_stw(crate::gc_telemetry::stops::StopReason::Minor, |coord, _| {
                    assert!(coord.parked.contains(&id));
                    assert!(coord.stw_pending.is_empty());
                    let slots = &coord.mutators[&id].slots;
                    assert_eq!(slots.len(), roots);
                    let address = slots.as_ptr() as usize;
                    assert_eq!(*publication.get_or_insert(address), address);
                });
            }
            assert_eq!(runtime().coord.1.notifications(), notifications.0);
            assert_eq!(runtime().budget_cv.notifications(), notifications.1);
            resume_tx.send(()).unwrap();
            worker.join().unwrap();
            eprintln!(
                "idle roots={roots} stops={cycles} publications=1 acknowledgements=0 gc_notifications=0"
            );
        }
    }
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn idle_resume_waits_for_active_stop() {
    let _test = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
    let (ready_tx, ready_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let (dropping_tx, dropping_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = std::thread::spawn(crate::runtime::serving(move || {
        let _mutator = MutatorRegistration::new();
        // SAFETY: native channel synchronization only until drop.
        let idle = unsafe { IdleMutator::enter() };
        ready_tx.send(()).unwrap();
        resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        dropping_tx.send(()).unwrap();
        drop(idle);
        done_tx.send(()).unwrap();
    }));
    ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    with_stw(crate::gc_telemetry::stops::StopReason::Minor, |_, _| {
        let before = crate::test_sync::count("idle_resume_blocked");
        resume_tx.send(()).unwrap();
        dropping_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        crate::test_sync::until("idle resume did not reach the active stop", || {
            crate::test_sync::count("idle_resume_blocked") > before
        });
        assert_eq!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty));
    });
    done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    worker.join().unwrap();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn idle_entry_acknowledges_an_already_requested_stop() {
    let _test = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
    let (ready_tx, ready_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let worker = std::thread::spawn(crate::runtime::serving(move || {
        let _mutator = MutatorRegistration::new();
        ready_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !runtime().stop_requested.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        // SAFETY: no managed access after publishing the stop acknowledgement.
        let _idle = unsafe { IdleMutator::enter() };
        resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    }));
    ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    with_stw(crate::gc_telemetry::stops::StopReason::Minor, |coord, _| {
        assert_eq!(coord.parked.len(), 1);
        assert!(coord.stw_pending.is_empty());
    });
    resume_tx.send(()).unwrap();
    worker.join().unwrap();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn idle_unregistered_and_unwind_paths_release_ownership() {
    let _test = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
    // SAFETY: no managed access occurs in this empty interval.
    drop(unsafe { IdleMutator::enter() });
    assert!(runtime().coord.0.lock().unwrap().parked.is_empty());
    let _mutator = MutatorRegistration::new();
    let result = std::panic::catch_unwind(|| {
        // SAFETY: unwinding only drops the guard before returning ownership.
        let _idle = unsafe { IdleMutator::enter() };
        panic!("idle unwind probe");
    });
    assert!(result.is_err());
    assert!(runtime().coord.0.lock().unwrap().parked.is_empty());
    willow_gc_safepoint();
}

#[test]
fn idle_entry_reads_no_roots_at_any_depth_until_a_stop() {
    let _test = isolated_runtime();
    for roots in [0, 1, 64, 1024, 65536] {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        let (ready_tx, ready_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let worker = std::thread::spawn(crate::runtime::serving(move || {
            let mutator = MutatorRegistration::new();
            // SAFETY: The requested layout describes the initialized words of this object and
            // its type’s tracing or finalization contract.
            let mut slots = vec![unsafe { willow_alloc_object(0, 8) }; roots];
            for slot in &mut slots {
                // SAFETY: The slot storage stays live and stationary through the matching
                // pop, including during collection.
                unsafe { willow_push_root(slot) };
            }
            // SAFETY: only native channels are touched until ownership resumes.
            let idle = unsafe { IdleMutator::enter() };
            ready_tx.send(std::thread::current().id()).unwrap();
            resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            drop(idle);
            let id = std::thread::current().id();
            let coord = runtime().coord.0.lock().unwrap();
            assert!(coord.mutators[&id].deferred.is_none());
            drop(coord);
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(roots as i32) };
            drop(mutator);
        }));
        let id = ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        {
            let coord = runtime().coord.0.lock().unwrap();
            assert!(coord.mutators[&id].deferred.is_some());
            assert!(coord.mutators[&id].slots.is_empty());
        }
        with_stw(crate::gc_telemetry::stops::StopReason::Minor, |coord, _| {
            assert!(coord.mutators[&id].deferred.is_none());
            assert_eq!(coord.mutators[&id].slots.len(), roots);
        });
        resume_tx.send(()).unwrap();
        worker.join().unwrap();
        eprintln!("idle entry roots={roots} slots_read_at_entry=0 slots_read_at_stop={roots}");
    }
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

fn wait_until_parked(id: std::thread::ThreadId) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !runtime().coord.0.lock().unwrap().parked.contains(&id) {
        assert!(std::time::Instant::now() < deadline, "mutator never parked");
        std::thread::yield_now();
    }
}

/// Exercise one blocked call: `release` runs inside a stop, and the returning
/// caller must stay parked until that stop ends.
fn assert_blocked_call_leaves_collections_running(
    relocate: bool,
    blocked: impl FnOnce() + Send + 'static,
    release: impl FnOnce(),
) {
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
    let (ready_tx, ready_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = std::thread::spawn(crate::runtime::serving(move || {
        let mutator = MutatorRegistration::new();
        let tls = tlab_state_for_test();
        // SAFETY: The fixture owns the stable TLAB through unregistration. The requested size
        // and tracing metadata describe the object initialized here.
        let mut root = unsafe { willow_gc_alloc_slow(tls.as_ptr(), 1, 0, 8, 0) };
        // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
        unsafe { *root.cast::<u64>() = 654321 };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut root) };
        let before = root as usize;
        let relocation = relocate.then(RelocationSafeScope::enter);
        ready_tx
            .send((std::thread::current().id(), before))
            .unwrap();
        blocked();
        done_tx.send(()).unwrap();
        // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
        assert_eq!(unsafe { *root.cast::<u64>() }, 654321);
        assert_eq!(root as usize != before, relocate);
        drop(relocation);
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
        drop(mutator);
    }));
    let (id, before) = ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    wait_until_parked(id);
    with_stw(crate::gc_telemetry::stops::StopReason::Minor, |coord, _| {
        assert_eq!(coord.mutators[&id].relocatable, relocate);
        assert_eq!(all_registered_stack_roots(coord), vec![before as *mut u8]);
    });
    minor::minor_collect_pinning_for_test();
    willow_gc_collect();
    willow_gc_collect();
    with_stw(crate::gc_telemetry::stops::StopReason::Minor, |coord, _| {
        assert!(coord.parked.contains(&id));
        let before = crate::test_sync::count("idle_resume_blocked");
        release();
        crate::test_sync::until("blocked call did not attempt reentry", || {
            crate::test_sync::count("idle_resume_blocked") > before
        });
        assert_eq!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty));
    });
    done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    worker.join().unwrap();
    assert!(runtime().coord.0.lock().unwrap().parked.is_empty());
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn blocked_sync_call_leaves_collections_running_and_parks_on_return() {
    let _test = isolated_runtime();
    for relocate in [false, true] {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        assert_blocked_call_leaves_collections_running(
            relocate,
            move || {
                crate::blocking::run_sync_call(move || {
                    release_rx.recv_timeout(Duration::from_secs(30)).unwrap();
                })
                .unwrap();
            },
            move || release_tx.send(()).unwrap(),
        );
    }
}

#[test]
fn blocked_foreign_region_leaves_collections_running_and_parks_on_return() {
    let _test = isolated_runtime();
    for unwind in [false, true] {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        assert_blocked_call_leaves_collections_running(
            false,
            move || {
                struct Region(u32);
                impl Drop for Region {
                    fn drop(&mut self) {
                        crate::rust_bridge_abi::willow_rust_bridge_region_exit(self.0);
                    }
                }
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _region = Region(crate::rust_bridge_abi::willow_rust_bridge_region_enter());
                    release_rx.recv_timeout(Duration::from_secs(30)).unwrap();
                    if unwind {
                        std::panic::resume_unwind(Box::new("foreign body"));
                    }
                }));
                assert_eq!(result.is_err(), unwind);
            },
            move || release_tx.send(()).unwrap(),
        );
    }
}

#[test]
fn sync_select_timeout_leaves_collections_running_and_parks_on_return() {
    let _test = isolated_runtime();
    assert_blocked_call_leaves_collections_running(
        false,
        || {
            let deadline = crate::scheduler::willow_monotonic_millis() + 1500;
            crate::scheduler::willow_sleep_until_monotonic(deadline);
        },
        || {},
    );
}

#[test]
fn collection_requested_during_a_stop_keeps_the_foreign_region() {
    let _test = isolated_runtime();
    for collect in [
        willow_gc_collect as extern "C" fn(),
        willow_gc_minor_collect as extern "C" fn(),
    ] {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        let (ready_tx, ready_rx) = mpsc::channel();
        let (collect_tx, collect_rx) = mpsc::channel::<()>();
        let (collected_tx, collected_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let worker = std::thread::spawn(crate::runtime::serving(move || {
            let _mutator = MutatorRegistration::new();
            let region = crate::rust_bridge_abi::willow_rust_bridge_region_enter();
            ready_tx.send(std::thread::current().id()).unwrap();
            collect_rx.recv_timeout(Duration::from_secs(30)).unwrap();
            collect();
            collected_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(30)).unwrap();
            crate::rust_bridge_abi::willow_rust_bridge_region_exit(region);
        }));
        let id = ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        wait_until_parked(id);
        // The stop stays pending on a running mutator, which leaves the
        // coordination lock free for the collection request to meet it.
        let (running_tx, running_rx) = mpsc::channel();
        let (park_tx, park_rx) = mpsc::channel::<()>();
        let straggler = std::thread::spawn(crate::runtime::serving(move || {
            let _mutator = MutatorRegistration::new();
            running_tx.send(()).unwrap();
            park_rx.recv_timeout(Duration::from_secs(30)).unwrap();
            willow_gc_safepoint();
        }));
        running_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let collector = std::thread::spawn(crate::runtime::serving(|| {
            with_stw(crate::gc_telemetry::stops::StopReason::Minor, |_, _| {});
        }));
        while !runtime().stop_requested.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        let before = crate::test_sync::count("idle_resume_blocked");
        collect_tx.send(()).unwrap();
        crate::test_sync::until("foreign collection did not meet pending stop", || {
            crate::test_sync::count("idle_resume_blocked") > before
        });
        park_tx.send(()).unwrap();
        collector.join().unwrap();
        straggler.join().unwrap();
        collected_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        let parked = runtime().coord.0.lock().unwrap().parked.contains(&id);
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(parked, "a collection must not end the caller's region");
    }
}

#[cfg(unix)]
#[test]
fn blocked_fifo_read_leaves_collections_running_and_parks_on_return() {
    let _test = isolated_runtime();
    let path = std::env::temp_dir().join(format!("willow_gc_fifo_{}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let text = path.to_str().unwrap().to_string();
    let name = std::ffi::CString::new(text.as_str()).unwrap();
    // SAFETY: The CString supplies a NUL-terminated name for the duration of mkfifo.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert_blocked_call_leaves_collections_running(
        false,
        move || {
            let name = crate::string::willow_string_from_str(&text);
            // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
            // during this call.
            let result = unsafe { crate::fs::willow_fs_read_to_string(name) };
            // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
            assert_eq!(unsafe { *result.cast::<i64>() }, 0);
            // SAFETY: The fixture allocates these fields in word-aligned GC storage at whole-word
            // offsets.
            let contents = unsafe { *result.add(8).cast::<*const u8>() };
            assert_eq!(
                // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this
                // borrow.
                unsafe { crate::string::willow_string_as_str(contents) },
                "payload"
            );
        },
        || std::fs::write(&path, "payload").unwrap(),
    );
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn deferred_publication_reads_parent_contexts_like_a_local_snapshot() {
    let _test = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
    let (ready_tx, ready_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let worker = std::thread::spawn(crate::runtime::serving(move || {
        let mutator = MutatorRegistration::new();
        // SAFETY: The requested layout describes the initialized words of this object and its
        // type’s tracing or finalization contract.
        let mut outer = unsafe { willow_alloc_object(0, 8) };
        let mut empty: *mut u8 = std::ptr::null_mut();
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe {
            willow_push_root(&raw mut outer);
            willow_push_root(&raw mut empty);
        }
        let child = Box::new(crate::generated_context::Context::new());
        let previous = crate::generated_context::replace(&raw const *child);
        // SAFETY: The requested layout describes the initialized words of this object and its
        // type’s tracing or finalization contract.
        let mut inner = unsafe { willow_alloc_object(0, 8) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut inner) };
        let expected = snapshot_local_root_slots();
        assert_eq!(expected.len(), 2);
        // SAFETY: only native channels are touched until ownership resumes.
        let idle = unsafe { IdleMutator::enter() };
        ready_tx
            .send((std::thread::current().id(), expected))
            .unwrap();
        resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        drop(idle);
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
        crate::generated_context::replace(previous);
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(2) };
        drop(mutator);
    }));
    let (id, expected) = ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    with_stw(crate::gc_telemetry::stops::StopReason::Minor, |coord, _| {
        assert_eq!(coord.mutators[&id].slots, expected);
    });
    resume_tx.send(()).unwrap();
    worker.join().unwrap();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn private_stop_coordination_resumes_only_its_owner_after_unwind() {
    let first = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
    let second = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
    let barrier = std::sync::Barrier::new(2);
    let reason = crate::gc_telemetry::stops::StopReason::Minor;
    std::thread::scope(|scope| {
        scope.spawn(crate::runtime::serving(|| {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                super::with_stw_in(&first, reason, |coord, _| {
                    assert!(coord.stop_requested);
                    barrier.wait();
                    panic!("private stop unwind");
                });
            }));
            assert!(result.is_err());
            assert!(
                !first
                    .gc
                    .stop_requested
                    .load(std::sync::atomic::Ordering::Acquire)
            );
            assert!(
                !first
                    .gc
                    .poll_requested
                    .load(std::sync::atomic::Ordering::Acquire)
            );
            assert!(
                second
                    .gc
                    .stop_requested
                    .load(std::sync::atomic::Ordering::Acquire)
            );
            assert!(!first.gc.coord.0.lock().unwrap().stop_requested);
            barrier.wait();
        }));
        scope.spawn(crate::runtime::serving(|| {
            super::with_stw_in(&second, reason, |coord, _| {
                barrier.wait();
                barrier.wait();
                assert!(coord.stop_requested);
            });
            assert!(
                !second
                    .gc
                    .stop_requested
                    .load(std::sync::atomic::Ordering::Acquire)
            );
            assert!(
                !second
                    .gc
                    .poll_requested
                    .load(std::sync::atomic::Ordering::Acquire)
            );
            assert!(!second.gc.coord.0.lock().unwrap().stop_requested);
        }));
    });
}

#[test]
fn private_stops_use_owner_observation_setting_and_work_store() {
    std::thread::scope(|scope| {
        for enabled in [false, true] {
            scope.spawn(crate::runtime::serving(move || {
                let mut owner =
                    crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
                owner.diagnostics.gc_measure = if enabled {
                    std::sync::LazyLock::new(|| true)
                } else {
                    std::sync::LazyLock::new(|| false)
                };
                owner.diagnostics.gc_trace_version = std::sync::LazyLock::new(|| 0);
                for _ in 0..8 {
                    super::with_stw_in(
                        &owner,
                        crate::gc_telemetry::stops::StopReason::Minor,
                        |_, work| {
                            crate::gc_telemetry::observations::graph_work_in(
                                &owner,
                                crate::gc_telemetry::MarkWork {
                                    objects: 5,
                                    scanned_bytes: 64,
                                    marked_bytes: 24,
                                    ..Default::default()
                                },
                            );
                            work.metadata_objects = 11;
                            work.root_values = 17;
                        },
                    );
                }
                assert_eq!(owner.telemetry.measurements.snapshot().resume_samples, 8);
                let stats = owner.telemetry.observations.snapshot();
                assert_eq!(stats.work[2].objects, if enabled { 128 } else { 0 });
                assert_eq!(stats.work[2].slots, if enabled { 200 } else { 0 });
                assert_eq!(stats.active_intervals, 0);
                assert_eq!(
                    stats.intervals[crate::gc_telemetry::observations::Kind::Collector as usize]
                        .count,
                    if enabled { 8 } else { 0 }
                );
            }));
        }
    });
}
