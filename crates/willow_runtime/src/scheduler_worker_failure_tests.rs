use super::*;

#[test]
fn persistent_worker_completes_each_drive_once_and_exits_on_disconnect() {
    let _guard = crate::gc::isolated_runtime();
    reset_global_scheduler_for_test();
    for drives in [1, 8, 64] {
        let entered = Arc::new(AtomicUsize::new(0));
        let worker_entered = Arc::clone(&entered);
        let mut pool = PersistentWorkers::default();
        pool.start_worker(
            Some(crate::runtime::current()),
            move |worker, target, state, deadline| {
                worker_entered.fetch_add(1, Ordering::Relaxed);
                run_parallel_worker(worker, target, state, deadline);
            },
        );
        let finished = Arc::new(ParallelCompletion::new(drives));
        for _ in 0..drives {
            pool.senders[0]
                .send(WorkerDrive {
                    target: None,
                    state: Arc::new(ParallelRunState::default()),
                    deadline: None,
                    finished: finished.clone(),
                })
                .unwrap();
        }
        drop(pool);
        assert_eq!(entered.load(Ordering::Relaxed), drives);
        assert_eq!(finished.remaining.load(Ordering::SeqCst), 0);
    }
    reset_global_scheduler_for_test();
}

#[test]
fn private_worker_pools_execute_owned_schedulers_and_join_captured_owners() {
    struct ExitCount(Arc<AtomicUsize>);
    impl Drop for ExitCount {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    unsafe extern "C" fn ready(_: *mut c_void) -> i32 {
        RUNTIME_POLL_READY
    }
    for drives in [1, 8, 64] {
        let start = Arc::new(std::sync::Barrier::new(2));
        let mut pools = Vec::new();
        let mut results = Vec::new();
        for tasks_per_drive in [1, 3] {
            let executed = Arc::new(AtomicUsize::new(0));
            let errors = Arc::new(AtomicUsize::new(0));
            let exited = Arc::new(AtomicUsize::new(0));
            let finished = Arc::new(ParallelCompletion::new(drives));
            let (worker_executed, worker_errors) = (Arc::clone(&executed), Arc::clone(&errors));
            let owner = ExitCount(Arc::clone(&exited));
            let start = Arc::clone(&start);
            let mut first_drive = true;
            let mut runtime = crate::runtime::Runtime::new(environment_worker_config);
            let mut scheduler = RuntimeScheduler::from_components(runtime.scheduler().components());
            let pool = runtime.worker_pool.get_mut().unwrap();
            pool.start_worker(None, move |_, _, _, _| {
                // Keep the captured owner alive through all queued drives.
                let _owner = &owner;
                if first_drive {
                    start.wait();
                    first_drive = false;
                }
                for _ in 0..tasks_per_drive {
                    // A native callback with no frame is a normal runtime task;
                    // no unmanaged object is passed off as a GC allocation.
                    let id = scheduler.spawn_task(ready, std::ptr::null_mut());
                    scheduler.set_running(id);
                    let (poll, frame, _) = scheduler.task_work(id).unwrap();
                    let result = unsafe { poll(frame) };
                    if result != RUNTIME_POLL_READY || current_task_id() != Some(id) {
                        worker_errors.fetch_add(1, Ordering::Relaxed);
                    }
                    scheduler.complete(id);
                    scheduler.clear_running();
                    if scheduler.task_count() != 0 || scheduler.ready_len() != 0 {
                        worker_errors.fetch_add(1, Ordering::Relaxed);
                    }
                    worker_executed.fetch_add(1, Ordering::Relaxed);
                }
            });
            for _ in 0..drives {
                pool.senders[0]
                    .send(WorkerDrive {
                        target: None,
                        state: Arc::new(ParallelRunState::default()),
                        deadline: None,
                        finished: Arc::clone(&finished),
                    })
                    .unwrap();
            }
            pools.push(runtime);
            results.push((tasks_per_drive, executed, errors, exited, finished));
        }
        // Teardown drains queued work and releases each runner's captured owner.
        drop(pools);
        for (tasks_per_drive, executed, errors, exited, finished) in results {
            assert_eq!(executed.load(Ordering::Relaxed), drives * tasks_per_drive);
            assert_eq!(errors.load(Ordering::Relaxed), 0);
            assert_eq!(exited.load(Ordering::Relaxed), 1);
            assert_eq!(finished.remaining.load(Ordering::SeqCst), 0);
        }
    }
}

#[test]
fn persistent_worker_teardown_cooperates_with_collection() {
    const CHILD: &str = "WILLOW_WORKER_JOIN_COLLECTION_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let _guard = crate::gc::isolated_runtime();
        crate::gc::reset_internal_for_test();
        let collections = crate::gc::gc_progress().successful_collections;
        let joining_mutator = crate::gc::MutatorRegistration::new();
        let (entered, running) = std::sync::mpsc::channel();
        let mut pool = PersistentWorkers::default();
        pool.start_worker(Some(crate::runtime::current()), move |_, _, _, _| {
            let worker_mutator = crate::gc::MutatorRegistration::new();
            entered.send(()).unwrap();
            crate::gc::willow_gc_collect();
            drop(worker_mutator);
        });
        let finished = Arc::new(ParallelCompletion::new(1));
        pool.senders[0]
            .send(WorkerDrive {
                target: None,
                state: Arc::new(ParallelRunState::default()),
                deadline: None,
                finished: Arc::clone(&finished),
            })
            .unwrap();
        running.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(pool);
        assert_eq!(finished.remaining.load(Ordering::SeqCst), 0);
        assert!(crate::gc::gc_progress().successful_collections > collections);
        drop(joining_mutator);
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "scheduler::worker_failure_tests::persistent_worker_teardown_cooperates_with_collection",
        ])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("worker teardown failed to cooperate with GC: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn collector_panic_on_worker_terminates_process() {
    const CHILD: &str = "WILLOW_TEST_COLLECTOR_WORKER_PANIC";
    if std::env::var_os(CHILD).is_some() {
        // The expected abort must not wait on a core-dump pipe handler: under
        // load that handler can outlast the parent's hang deadline. A zero
        // RLIMIT_CORE does not stop a piped core_pattern; non-dumpable does.
        #[cfg(target_os = "linux")]
        assert_eq!(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0) }, 0);
        let _guard = crate::gc::isolated_runtime();
        crate::gc::reset_internal_for_test();
        crate::gc::willow_gc_add_runtime_root(std::ptr::dangling_mut::<u8>());
        let mut pool = PersistentWorkers::default();
        pool.start_worker(
            Some(crate::runtime::current()),
            |worker, target, state, deadline| {
                crate::gc::collect_for_worker_panic_test();
                run_parallel_worker(worker, target, state, deadline);
            },
        );
        let finished = Arc::new(ParallelCompletion::new(1));
        pool.senders[0]
            .send(WorkerDrive {
                target: None,
                state: Arc::new(ParallelRunState::default()),
                deadline: None,
                finished: finished.clone(),
            })
            .unwrap();
        finished.wait();
        panic!("collector panic unexpectedly completed the drive");
    }

    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "scheduler::worker_failure_tests::collector_panic_on_worker_terminates_process",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("RUST_BACKTRACE", "0")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "collector panic hung the worker drive: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("willow-worker-0"), "{stderr}");
    assert!(stderr.contains("invalid GC pointer"), "{stderr}");
}

#[test]
fn private_runtime_production_run_loops_complete_independent_native_tasks() {
    unsafe extern "C" fn ready(_: *mut c_void) -> i32 {
        assert!(current_task_id().is_some());
        RUNTIME_POLL_READY
    }
    let start = std::sync::Barrier::new(2);
    let first_gone = AtomicBool::new(false);
    std::thread::scope(|scope| {
        for batch in [1, 3] {
            let start = &start;
            let first_gone = &first_gone;
            scope.spawn(crate::runtime::serving(move || {
                let runtime = crate::runtime::Runtime::new(environment_worker_config);
                let mut correct = true;
                for _ in 0..16 {
                    let target = SchedulerServices::with_scheduler(
                        || runtime.scheduler(),
                        |scheduler| {
                            let mut last = 0;
                            for _ in 0..batch {
                                last = scheduler.spawn_task(ready, std::ptr::null_mut());
                            }
                            last
                        },
                    );
                    start.wait();
                    let completed =
                        scheduler_run_loop_in(&runtime, Some(target), 0, None, false, None);
                    correct &= completed == batch;
                    correct &= runtime.scheduler().components().tasks.len() == 0;
                    correct &= runtime.scheduler().components().run_queues.len() == 0;
                    correct &= current_task_id().is_none();
                    start.wait();
                }
                if batch == 1 {
                    drop(runtime);
                    first_gone.store(true, Ordering::Release);
                } else {
                    while !first_gone.load(Ordering::Acquire) {
                        std::thread::yield_now();
                    }
                    let target = SchedulerServices::with_scheduler(
                        || runtime.scheduler(),
                        |scheduler| scheduler.spawn_task(ready, std::ptr::null_mut()),
                    );
                    assert_eq!(
                        scheduler_run_loop_in(&runtime, Some(target), 0, None, false, None),
                        1
                    );
                    assert_eq!(runtime.scheduler().components().tasks.len(), 0);
                }
                assert!(
                    correct,
                    "private drive completed a peer task or left work behind"
                );
            }));
        }
    });
}

#[test]
fn private_persistent_production_pools_keep_wakes_and_teardown_independent() {
    fn workers() -> RuntimeWorkerConfig {
        RuntimeWorkerConfig::with_workers(4)
    }
    unsafe extern "C" fn ready(_: *mut c_void) -> i32 {
        assert!(current_task_id().is_some());
        RUNTIME_POLL_READY
    }
    let start = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        for count in [64, 192] {
            let start = &start;
            scope.spawn(crate::runtime::serving(move || {
                let owner = crate::runtime::Runtime::shared(workers);
                let weak = Arc::downgrade(&owner);
                for _ in 0..3 {
                    SchedulerServices::with_scheduler(
                        || owner.scheduler(),
                        |scheduler| {
                            for _ in 0..count {
                                scheduler.spawn_task(ready, std::ptr::null_mut());
                            }
                        },
                    );
                    start.wait();
                    assert_eq!(sched_run_parallel_in(&owner, None, 4, None), count);
                    assert_eq!(owner.scheduler().components().tasks.len(), 0);
                    assert_eq!(owner.scheduler().components().run_queues.len(), 0);
                    assert_eq!(owner.worker_pool.lock().unwrap().handles.len(), 4);
                    start.wait();
                }
                drop(owner);
                assert!(
                    weak.upgrade().is_none(),
                    "idle workers retained their owning pool"
                );
            }));
        }
    });
}

#[test]
fn private_netpoll_cleanup_and_wake_use_matching_scheduler() {
    let first = crate::runtime::Runtime::new(environment_worker_config);
    let second = crate::runtime::Runtime::new(environment_worker_config);
    for owner in [&first, &second] {
        owner.netpoll.with(|poll| {
            poll.register(crate::netpoll::IoRegistration::new(
                77,
                1,
                crate::netpoll::IoInterest::Readable,
            ))
        });
        owner
            .scheduler()
            .components()
            .retirements
            .push_cleanup(TerminalCleanup {
                task_id: 1,
                channel_waits: Vec::new(),
                lock_wait: None,
            });
    }
    drain_terminal_cleanups_in(&first, first.scheduler().components());
    assert!(!first.netpoll.has_waiters());
    assert!(second.netpoll.has_waiters());
    let state = ParallelRunState::new(Arc::clone(&second.scheduler().components().claims));
    assert!(!work_source_is_live_in(
        &first,
        first.scheduler().components(),
        &state,
        WorkSource::Netpoll
    ));
    assert!(work_source_is_live_in(
        &second,
        second.scheduler().components(),
        &state,
        WorkSource::Netpoll
    ));
    drain_terminal_cleanups_in(&second, second.scheduler().components());
    assert!(!second.netpoll.has_waiters());
}

#[test]
fn private_frame_release_signals_do_not_consume_peer_work() {
    let first = crate::runtime::Runtime::new(environment_worker_config);
    let second = crate::runtime::Runtime::new(environment_worker_config);
    let a = first.scheduler().components();
    let b = second.scheduler().components();
    a.retirements
        .frame_release_due
        .store(true, Ordering::Release);
    b.retirements
        .frame_release_due
        .store(true, Ordering::Release);
    release_quiescent_frame_roots_in(&first, a, false);
    assert!(!a.retirements.frame_release_due.load(Ordering::Acquire));
    assert!(b.retirements.frame_release_due.load(Ordering::Acquire));
    release_quiescent_frame_roots_in(&second, b, false);
    assert!(!b.retirements.frame_release_due.load(Ordering::Acquire));
}
