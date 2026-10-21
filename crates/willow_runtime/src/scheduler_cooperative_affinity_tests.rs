use super::*;

unsafe extern "C" fn ready(_: *mut c_void) -> i32 {
    RUNTIME_POLL_READY
}

#[test]
fn cooperative_preemption_reroutes_thieves_and_releases_at_source_wait() {
    let _guard = crate::gc::runtime_test_guard();
    reset_global_scheduler_for_test();
    replace_global_scheduler_for_test(2);
    let task = willow_sched_spawn_cooperative(ready, std::ptr::null_mut()) as u64;
    let previous_worker = CURRENT_WORKER.with(|slot| slot.replace(1));
    assert_eq!(claim_global_ready_for_worker(1, None).unwrap().0, task);
    finish_global_poll_boundary(task, GlobalPollBoundary::Preempted);
    set_current_task(None);
    assert_eq!(crate::worker_affinity::required_workers(), 2);
    for _ in 0..16 {
        assert!(claim_global_ready_for_worker(0, None).is_none());
        assert!(global_run_queues().contains(task));
    }
    assert_eq!(claim_global_ready_for_worker(1, None).unwrap().0, task);
    finish_global_poll_boundary(task, GlobalPollBoundary::Pending);
    set_current_task(None);
    assert_eq!(crate::worker_affinity::required_workers(), 0);
    assert!(with_global(|sched| sched.wake(task)));
    CURRENT_WORKER.with(|slot| slot.set(0));
    assert_eq!(claim_global_ready_for_worker(0, None).unwrap().0, task);
    with_global(|sched| sched.complete(task));
    set_current_task(None);
    CURRENT_WORKER.with(|slot| slot.set(previous_worker));
    reset_global_scheduler_for_test();
}

#[test]
fn cooperative_affinity_release_covers_yield_completion_and_cancellation() {
    let _guard = crate::gc::runtime_test_guard();
    for terminal in [false, true] {
        for cancelled in [false, true] {
            reset_global_scheduler_for_test();
            replace_global_scheduler_for_test(2);
            let task = willow_sched_spawn_cooperative(ready, std::ptr::null_mut()) as u64;
            let previous_worker = CURRENT_WORKER.with(|slot| slot.replace(1));
            assert_eq!(claim_global_ready_for_worker(1, None).unwrap().0, task);
            finish_global_poll_boundary(task, GlobalPollBoundary::Preempted);
            set_current_task(None);
            if terminal {
                assert_eq!(claim_global_ready_for_worker(1, None).unwrap().0, task);
                with_global(|sched| {
                    if cancelled {
                        sched.finalize_cancelled(task);
                    } else {
                        sched.complete(task);
                    }
                });
                set_current_task(None);
            } else {
                assert_eq!(claim_global_ready_for_worker(1, None).unwrap().0, task);
                finish_global_poll_boundary(
                    task,
                    if cancelled {
                        GlobalPollBoundary::Pending
                    } else {
                        GlobalPollBoundary::Runnable
                    },
                );
                set_current_task(None);
            }
            assert_eq!(crate::worker_affinity::required_workers(), 0);
            CURRENT_WORKER.with(|slot| slot.set(previous_worker));
        }
    }
    reset_global_scheduler_for_test();
}

static OWNERS: LazyLock<Mutex<std::collections::HashMap<u64, std::thread::ThreadId>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));
static ENTERED: AtomicUsize = AtomicUsize::new(0);
static FINISH: AtomicBool = AtomicBool::new(false);
static FAILURES: AtomicUsize = AtomicUsize::new(0);
static CLEANED: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn affine_cancel(_: *mut c_void) {
    let owners = OWNERS.lock().unwrap();
    let owner = owners.get(&willow_sched_current_task()).unwrap();
    if *owner != std::thread::current().id() {
        FAILURES.fetch_add(1, Ordering::Relaxed);
    }
    CLEANED.fetch_add(1, Ordering::Release);
}

unsafe extern "C" fn forced_preemption(_: *mut c_void) -> i32 {
    let id = willow_sched_current_task();
    let thread = std::thread::current().id();
    let first = {
        let mut owners = OWNERS.lock().unwrap();
        if let Some(owner) = owners.get(&id) {
            if *owner != thread {
                FAILURES.fetch_add(1, Ordering::Relaxed);
            }
            false
        } else {
            owners.insert(id, thread);
            true
        }
    };
    if first {
        ENTERED.fetch_add(1, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(5);
        while ENTERED.load(Ordering::Acquire) < 4 {
            if Instant::now() >= deadline {
                FAILURES.fetch_add(1, Ordering::Relaxed);
                break;
            }
            std::thread::yield_now();
        }
        CURRENT_RUN_STATE.with(|slot| {
            if let Some(state) = slot.borrow().as_ref() {
                state.stop.store(true, Ordering::Release);
            }
        });
    }
    if FINISH.load(Ordering::Acquire) {
        RUNTIME_POLL_READY
    } else {
        RUNTIME_POLL_PREEMPTED
    }
}

#[test]
fn cooperative_preemption_keeps_os_thread_across_worker_shrink() {
    let _guard = crate::gc::runtime_test_guard();
    for cancelled in [false, true] {
        reset_global_scheduler_for_test();
        replace_global_scheduler_for_test(4);
        OWNERS.lock().unwrap().clear();
        ENTERED.store(0, Ordering::Release);
        FINISH.store(false, Ordering::Release);
        FAILURES.store(0, Ordering::Release);
        CLEANED.store(0, Ordering::Release);
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let frame = crate::async_frame::willow_async_frame_alloc(0, 0).cast();
                willow_sched_spawn_cooperative(forced_preemption, frame) as u64
            })
            .collect();
        let mutator = crate::gc::MutatorRegistration::new();
        willow_sched_run_parallel(None, 4, None);
        assert_eq!(ENTERED.load(Ordering::Acquire), 4);
        assert_eq!(crate::worker_affinity::required_workers(), 4);
        if cancelled {
            for &task in &tasks {
                willow_sched_set_cancel_fn_cooperative(task, affine_cancel, 0);
                willow_sched_cancel(task);
            }
        }
        FINISH.store(true, Ordering::Release);
        willow_sched_run_parallel(None, 1, None);
        drop(mutator);
        assert_eq!(FAILURES.load(Ordering::Acquire), 0);
        assert_eq!(
            CLEANED.load(Ordering::Acquire),
            if cancelled { 4 } else { 0 }
        );
        for task in tasks {
            assert_eq!(willow_sched_task_state(task), -1);
        }
        assert_eq!(crate::worker_affinity::required_workers(), 0);
        reset_global_scheduler_for_test();
    }
}

#[test]
fn cooperative_affinity_acquires_once_per_segment() {
    let _guard = crate::gc::runtime_test_guard();
    for polls in [16, 64, 256] {
        reset_global_scheduler_for_test();
        replace_global_scheduler_for_test(2);
        let task = willow_sched_spawn_cooperative(ready, std::ptr::null_mut()) as u64;
        let previous_worker = CURRENT_WORKER.with(|slot| slot.replace(1));
        let (_, before) = crate::worker_affinity::affinity_counts_for_test();
        for _ in 0..polls {
            assert_eq!(claim_global_ready_for_worker(1, None).unwrap().0, task);
            finish_global_poll_boundary(task, GlobalPollBoundary::Preempted);
            set_current_task(None);
        }
        let (_, pinned) = crate::worker_affinity::affinity_counts_for_test();
        assert_eq!(pinned - before, 1);
        assert_eq!(claim_global_ready_for_worker(1, None).unwrap().0, task);
        finish_global_poll_boundary(task, GlobalPollBoundary::Pending);
        set_current_task(None);
        let (_, released) = crate::worker_affinity::affinity_counts_for_test();
        assert_eq!(released - before, 2);
        println!("polls={polls} acquisitions=1 releases=1");
        CURRENT_WORKER.with(|slot| slot.set(previous_worker));
    }
    reset_global_scheduler_for_test();
}
