//! Idle ownership spans many collections without a wake/acknowledge per cycle.
use super::*;
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn idle_roots_survive_minor_major_and_relocation() {
    let _test = runtime_test_guard();
    for relocate in [false, true] {
        reset_internal_for_test();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mutator = MutatorRegistration::new();
            let mut tls = tlab_state_for_test();
            let mut root = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
            unsafe { *root.cast::<u64>() = 123456 };
            willow_push_root(&raw mut root);
            let before = root as usize;
            let relocation = relocate.then(RelocationSafeScope::enter);
            // SAFETY: only native channels are touched until ownership resumes.
            let idle = unsafe { IdleMutator::enter() };
            ready_tx
                .send((std::thread::current().id(), before))
                .unwrap();
            resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            drop(idle);
            assert_eq!(unsafe { *root.cast::<u64>() }, 123456);
            assert_eq!(root as usize != before, relocate);
            drop(relocation);
            willow_pop_roots(1);
            drop(mutator);
        });
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
    reset_internal_for_test();
}

#[test]
fn idle_publication_is_reused_across_increasing_stop_counts_and_root_counts() {
    let _test = runtime_test_guard();
    for roots in [0, 1, 64, 1024] {
        for cycles in [1, 8, 64] {
            reset_internal_for_test();
            let (ready_tx, ready_rx) = mpsc::channel();
            let (resume_tx, resume_rx) = mpsc::channel();
            let worker = std::thread::spawn(move || {
                let mutator = MutatorRegistration::new();
                let mut slots = vec![willow_alloc_object(0, 8); roots];
                for slot in &mut slots {
                    willow_push_root(slot);
                }
                // SAFETY: slots and managed objects stay untouched during wait.
                let idle = unsafe { IdleMutator::enter() };
                ready_tx.send(std::thread::current().id()).unwrap();
                resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                drop(idle);
                willow_pop_roots(roots as i32);
                drop(mutator);
            });
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
    reset_internal_for_test();
}

#[test]
fn idle_resume_waits_for_active_stop() {
    let _test = runtime_test_guard();
    reset_internal_for_test();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let (dropping_tx, dropping_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _mutator = MutatorRegistration::new();
        // SAFETY: native channel synchronization only until drop.
        let idle = unsafe { IdleMutator::enter() };
        ready_tx.send(()).unwrap();
        resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        dropping_tx.send(()).unwrap();
        drop(idle);
        done_tx.send(()).unwrap();
    });
    ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    with_stw(crate::gc_telemetry::stops::StopReason::Minor, |_, _| {
        resume_tx.send(()).unwrap();
        dropping_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(
            done_rx.recv_timeout(Duration::from_millis(20)),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
    });
    done_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    worker.join().unwrap();
    reset_internal_for_test();
}

#[test]
fn idle_entry_acknowledges_an_already_requested_stop() {
    let _test = runtime_test_guard();
    reset_internal_for_test();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
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
    });
    ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    with_stw(crate::gc_telemetry::stops::StopReason::Minor, |coord, _| {
        assert_eq!(coord.parked.len(), 1);
        assert!(coord.stw_pending.is_empty());
    });
    resume_tx.send(()).unwrap();
    worker.join().unwrap();
    reset_internal_for_test();
}

#[test]
fn idle_unregistered_and_unwind_paths_release_ownership() {
    let _test = runtime_test_guard();
    reset_internal_for_test();
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
