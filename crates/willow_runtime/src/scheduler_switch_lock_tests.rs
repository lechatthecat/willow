use super::*;

static SWITCHES: crate::runtime::PerRuntime<AtomicUsize> =
    crate::runtime::PerRuntime::new(|| AtomicUsize::new(0));
static LOCKS: crate::runtime::PerRuntime<AtomicUsize> =
    crate::runtime::PerRuntime::new(|| AtomicUsize::new(0));
static MAX_LOCKS: crate::runtime::PerRuntime<AtomicUsize> =
    crate::runtime::PerRuntime::new(|| AtomicUsize::new(0));
static FAILURES: crate::runtime::PerRuntime<AtomicUsize> =
    crate::runtime::PerRuntime::new(|| AtomicUsize::new(0));

unsafe extern "C" fn helper(_: *mut c_void) -> i32 {
    if crate::native_stack::cancelled() != 0 {
        FAILURES.fetch_add(1, Ordering::Relaxed);
    }
    RUNTIME_POLL_READY
}

unsafe extern "C" fn measure_poll(_: *mut c_void) -> i32 {
    // Warm the thread-local stack cache before measuring completed switches.
    let result = willow_task_stack_enter(helper, std::ptr::null_mut());
    willow_task_stack_leave();
    if result != RUNTIME_POLL_READY {
        FAILURES.fetch_add(1, Ordering::Relaxed);
    }
    for _ in 0..4 {
        let before = global_components().tasks.lock_observation.current();
        let result = willow_task_stack_enter(helper, std::ptr::null_mut());
        willow_task_stack_leave();
        let locks = global_components().tasks.lock_observation.current() - before;
        LOCKS.fetch_add(locks, Ordering::Relaxed);
        MAX_LOCKS.fetch_max(locks, Ordering::Relaxed);
        SWITCHES.fetch_add(1, Ordering::Relaxed);
        if result != RUNTIME_POLL_READY {
            FAILURES.fetch_add(1, Ordering::Relaxed);
        }
    }
    RUNTIME_POLL_READY
}

#[test]
fn completed_helper_switch_mutex_count_is_constant() {
    let _guard = crate::gc::isolated_runtime();
    for workers in [1, 4, 16] {
        for tasks in [100, 1_000, 10_000] {
            reset_global_scheduler_for_test();
            replace_global_scheduler_for_test(workers);
            SWITCHES.store(0, Ordering::Relaxed);
            LOCKS.store(0, Ordering::Relaxed);
            MAX_LOCKS.store(0, Ordering::Relaxed);
            FAILURES.store(0, Ordering::Relaxed);
            for _ in 0..tasks {
                willow_sched_spawn_cooperative(measure_poll, std::ptr::null_mut());
            }
            let mutator = crate::gc::MutatorRegistration::new();
            willow_sched_run_parallel(None, workers, None);
            drop(mutator);
            let switches = SWITCHES.load(Ordering::Relaxed);
            let locks = LOCKS.load(Ordering::Relaxed);
            let max_locks = MAX_LOCKS.load(Ordering::Relaxed);
            println!(
                "workers={workers},tasks={tasks},switches={switches},mutexes={locks},max_per_switch={max_locks}"
            );
            assert_eq!(FAILURES.load(Ordering::Relaxed), 0);
            assert_eq!(switches, tasks * 4);
            assert_eq!(locks, 0);
            assert_eq!(max_locks, 0);
            assert_eq!(crate::worker_affinity::required_workers(), 0);
        }
    }
    reset_global_scheduler_for_test();
}

static SNAPSHOT_FAILURES: crate::runtime::PerRuntime<AtomicUsize> =
    crate::runtime::PerRuntime::new(|| AtomicUsize::new(0));
static OBSERVED_CANCEL: crate::runtime::PerRuntime<AtomicBool> =
    crate::runtime::PerRuntime::new(|| AtomicBool::new(false));

unsafe extern "C" fn cancelled_helper(_: *mut c_void) -> i32 {
    OBSERVED_CANCEL.store(crate::native_stack::cancelled() != 0, Ordering::Relaxed);
    RUNTIME_POLL_READY
}

unsafe extern "C" fn nested_child(_: *mut c_void) -> i32 {
    if current_cancel_snapshot().is_none() {
        SNAPSHOT_FAILURES.fetch_or(1, Ordering::Relaxed);
    }
    let result = willow_task_stack_enter(helper, std::ptr::null_mut());
    willow_task_stack_leave();
    result
}

unsafe extern "C" fn cancel_after_nested_drive(_: *mut c_void) -> i32 {
    let owner = willow_sched_current_task();
    let before = current_cancel_snapshot();
    let child = willow_sched_spawn_cooperative(nested_child, std::ptr::null_mut()) as u64;
    willow_sched_run_until(child);
    if current_cancel_snapshot() != before || willow_sched_current_task() != owner {
        SNAPSHOT_FAILURES.fetch_or(2, Ordering::Relaxed);
    }
    // The claim snapshot was uncancelled. Advance the global epoch before a
    // NEW helper is acquired: its first predicate must reject that snapshot.
    willow_sched_cancel(owner);
    let result = willow_task_stack_enter(cancelled_helper, std::ptr::null_mut());
    willow_task_stack_leave();
    if result != RUNTIME_POLL_PREEMPTED {
        SNAPSHOT_FAILURES.fetch_or(4, Ordering::Relaxed);
    }
    result
}

#[test]
fn new_helper_observes_cancel_after_claim_and_nested_drive_restores_snapshot() {
    let _guard = crate::gc::isolated_runtime();
    reset_global_scheduler_for_test();
    replace_global_scheduler_for_test(1);
    SNAPSHOT_FAILURES.store(0, Ordering::Relaxed);
    OBSERVED_CANCEL.store(false, Ordering::Relaxed);
    let task =
        willow_sched_spawn_cooperative(cancel_after_nested_drive, std::ptr::null_mut()) as u64;
    willow_sched_run_until(task);
    assert_eq!(SNAPSHOT_FAILURES.load(Ordering::Relaxed), 0);
    assert!(OBSERVED_CANCEL.load(Ordering::Relaxed));
    assert_eq!(crate::worker_affinity::required_workers(), 0);
    reset_global_scheduler_for_test();
}

#[test]
fn task_table_mutex_observation_is_instance_and_thread_owned() {
    let first = ShardedTaskTable::new();
    let second = ShardedTaskTable::new();
    std::thread::scope(|scope| {
        for count in [8, 32] {
            let (first, second) = (&first, &second);
            scope.spawn(crate::runtime::serving(move || {
                for _ in 0..count {
                    drop(first.lock_shard(0));
                    drop(second.lock_shard(1));
                    drop(second.lock_shard(2));
                }
                assert_eq!(first.lock_observation.current(), count);
                assert_eq!(second.lock_observation.current(), count * 2);
            }));
        }
    });
    assert_eq!(first.lock_observation.current(), 0);
    assert_eq!(second.lock_observation.current(), 0);
}

/// Phases of the cross-runtime cancel probe: 1 = runtime A's helper cached
/// its cancellation state, 2 = runtime B recorded a cancel request.
static CROSS_PHASE: AtomicUsize = AtomicUsize::new(0);
static CROSS_LOCKS: AtomicUsize = AtomicUsize::new(usize::MAX);
static CROSS_CANCELLED: AtomicUsize = AtomicUsize::new(usize::MAX);

unsafe extern "C" fn observe_foreign_cancel(_: *mut c_void) -> i32 {
    let tasks = &global_components().tasks;
    let mut cancelled = crate::native_stack::cancelled();
    let before = tasks.lock_observation.current();
    CROSS_PHASE.store(1, Ordering::SeqCst);
    while CROSS_PHASE.load(Ordering::SeqCst) != 2 {
        std::thread::yield_now();
    }
    cancelled |= crate::native_stack::cancelled();
    CROSS_LOCKS.store(tasks.lock_observation.current() - before, Ordering::SeqCst);
    CROSS_CANCELLED.store(cancelled as usize, Ordering::SeqCst);
    RUNTIME_POLL_READY
}

unsafe extern "C" fn foreign_cancel_poll(_: *mut c_void) -> i32 {
    let result = willow_task_stack_enter(observe_foreign_cancel, std::ptr::null_mut());
    willow_task_stack_leave();
    result
}

#[test]
fn cancel_in_one_runtime_never_invalidates_another_runtime_cancel_cache() {
    CROSS_PHASE.store(0, Ordering::SeqCst);
    std::thread::scope(|scope| {
        let observer = scope.spawn(|| {
            let _runtime = crate::gc::isolated_runtime();
            reset_global_scheduler_for_test();
            replace_global_scheduler_for_test(1);
            willow_sched_spawn_cooperative(foreign_cancel_poll, std::ptr::null_mut());
            let mutator = crate::gc::MutatorRegistration::new();
            willow_sched_run_parallel(None, 1, None);
            drop(mutator);
            reset_global_scheduler_for_test();
        });
        let canceller = scope.spawn(|| {
            let _runtime = crate::gc::isolated_runtime();
            reset_global_scheduler_for_test();
            replace_global_scheduler_for_test(1);
            let victim = willow_sched_spawn_cooperative(helper, std::ptr::null_mut()) as u64;
            while CROSS_PHASE.load(Ordering::SeqCst) != 1 {
                std::thread::yield_now();
            }
            let epoch = &global_task_table().cancel_epoch;
            let before = epoch.load();
            willow_sched_cancel(victim);
            let advanced = epoch.load() - before;
            CROSS_PHASE.store(2, Ordering::SeqCst);
            reset_global_scheduler_for_test();
            advanced
        });
        observer.join().unwrap();
        assert_eq!(canceller.join().unwrap(), 1);
    });
    // The observer's helper neither re-read its task state nor saw a cancel.
    assert_eq!(CROSS_LOCKS.load(Ordering::SeqCst), 0);
    assert_eq!(CROSS_CANCELLED.load(Ordering::SeqCst), 0);
}
