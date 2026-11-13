use super::*;

thread_local! {
    static MUTEX_ACQUISITIONS: Cell<usize> = const { Cell::new(0) };
}

pub(crate) fn record_mutex() {
    MUTEX_ACQUISITIONS.with(|count| count.set(count.get() + 1));
}

static SWITCHES: AtomicUsize = AtomicUsize::new(0);
static LOCKS: AtomicUsize = AtomicUsize::new(0);
static MAX_LOCKS: AtomicUsize = AtomicUsize::new(0);
static FAILURES: AtomicUsize = AtomicUsize::new(0);

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
        let before = MUTEX_ACQUISITIONS.get();
        let result = willow_task_stack_enter(helper, std::ptr::null_mut());
        willow_task_stack_leave();
        let locks = MUTEX_ACQUISITIONS.get() - before;
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
    let _guard = crate::gc::runtime_test_guard();
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

static SNAPSHOT_FAILURES: AtomicUsize = AtomicUsize::new(0);
static OBSERVED_CANCEL: AtomicBool = AtomicBool::new(false);

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
    let _guard = crate::gc::runtime_test_guard();
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
