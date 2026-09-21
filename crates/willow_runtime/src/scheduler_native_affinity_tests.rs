use super::*;
use std::sync::atomic::AtomicU64;

static ENTERED: AtomicUsize = AtomicUsize::new(0);
static FINISH: AtomicBool = AtomicBool::new(false);
// Preserve the failure cause without unwinding through the C callback.
const RENDEZVOUS_FAILED: usize = 1;
const SUSPEND_FAILED: usize = 2;
const OWNER_CHANGED: usize = 4;
const RECURSION_FAILED: usize = 8;
static CALLBACK_FAILURES: AtomicUsize = AtomicUsize::new(0);
static RENDEZVOUS_MILLIS: AtomicU64 = AtomicU64::new(5000);

unsafe extern "C" fn deep_affinity_poll(_: *mut c_void) -> i32 {
    fn recurse(depth: usize) -> usize {
        let owner = std::thread::current().id();
        if depth == 0 {
            ENTERED.fetch_add(1, Ordering::SeqCst);
            // Keep each worker here until all four own a stack. Never panic
            // through the extern C callback if a worker misses the rendezvous.
            let deadline =
                Instant::now() + Duration::from_millis(RENDEZVOUS_MILLIS.load(Ordering::Relaxed));
            while ENTERED.load(Ordering::SeqCst) < 4 && !FINISH.load(Ordering::Acquire) {
                if Instant::now() >= deadline {
                    CALLBACK_FAILURES.fetch_or(RENDEZVOUS_FAILED, Ordering::Release);
                    FINISH.store(true, Ordering::Release);
                    break;
                }
                std::thread::yield_now();
            }
            // End setup by state, not by a short drive deadline that can expire
            // before another worker even claims its task. Active polls all
            // return to the scheduler before the drive joins them.
            CURRENT_RUN_STATE.with(|slot| {
                if !FINISH.load(Ordering::Acquire)
                    && let Some(state) = slot.borrow().as_ref()
                {
                    state.stop.store(true, Ordering::Release);
                }
            });
            while !FINISH.load(Ordering::Acquire)
                && willow_sched_is_cancelled(willow_sched_current_task()) == 0
            {
                if !crate::native_stack::suspend() {
                    CALLBACK_FAILURES.fetch_or(SUSPEND_FAILED, Ordering::Release);
                    FINISH.store(true, Ordering::Release);
                }
                if std::thread::current().id() != owner {
                    CALLBACK_FAILURES.fetch_or(OWNER_CHANGED, Ordering::Release);
                    FINISH.store(true, Ordering::Release);
                }
            }
            return 1;
        }
        let result = recurse(depth - 1);
        if std::thread::current().id() != owner {
            CALLBACK_FAILURES.fetch_or(OWNER_CHANGED, Ordering::Release);
        }
        std::hint::black_box(result + depth)
    }
    if recurse(64) != 2081 {
        CALLBACK_FAILURES.fetch_or(RECURSION_FAILED, Ordering::Release);
    }
    RUNTIME_POLL_READY
}

// Contain even runtime-level aborts or failed cleanup within this fixture, so
// the next scheduler test cannot obscure the original failure during reset.
fn run_affinity_child(test: &str, expect_failure: bool) {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env("WILLOW_AFFINITY_CHILD", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("affinity child timed out: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    if expect_failure {
        assert_eq!(output.status.code(), Some(101), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("affinity callback failures: 0x1"),
            "{output:?}"
        );
        assert!(!stderr.contains("dropping a suspended"), "{output:?}");
        assert!(!stderr.contains("cannot unwind"), "{output:?}");
    } else {
        assert!(output.status.success(), "{output:?}");
    }
}

fn check_affinity_case(cancel: bool, task_count: usize, rendezvous_millis: u64) {
    reset_global_scheduler_for_test();
    replace_global_scheduler_for_test(4);
    ENTERED.store(0, Ordering::SeqCst);
    FINISH.store(false, Ordering::Release);
    CALLBACK_FAILURES.store(0, Ordering::Release);
    RENDEZVOUS_MILLIS.store(rendezvous_millis, Ordering::Relaxed);
    assert_eq!(crate::native_stack::required_workers(), 0);
    let tasks: Vec<_> = (0..task_count)
        .map(|_| willow_sched_spawn(deep_affinity_poll, std::ptr::null_mut()))
        .collect();
    crate::gc::willow_gc_register_mutator();
    let completed = willow_sched_run_parallel(None, 4, None);
    let entered = ENTERED.load(Ordering::SeqCst);
    let required = crate::native_stack::required_workers();
    if cancel {
        for &task in &tasks {
            willow_sched_cancel(task);
        }
    } else {
        FINISH.store(true, Ordering::Release);
    }
    // Request only one worker. All suspended stacks must still resume on
    // their original OS threads, including workers 1, 2 and 3. Drain BEFORE
    // checking setup observations so a failed assertion leaves no live stacks.
    willow_sched_run_parallel(None, 1, Some(Instant::now() + Duration::from_secs(5)));
    crate::gc::willow_gc_unregister_mutator();
    for task in tasks {
        assert_eq!(willow_sched_task_state(task), -1, "affinity cleanup failed");
    }
    assert_eq!(crate::native_stack::required_workers(), 0);
    reset_global_scheduler_for_test();
    let failures = CALLBACK_FAILURES.load(Ordering::Acquire);
    assert_eq!(
        failures, 0,
        "affinity callback failures: {failures:#x} (1=rendezvous, 2=suspend, 4=owner, 8=recursion)"
    );
    assert_eq!(completed, 0);
    assert_eq!(entered, 4);
    assert_eq!(required, 4);
}

#[test]
fn native_affinity_survives_worker_shrink_and_clears_on_completion_or_cancel() {
    if std::env::var_os("WILLOW_AFFINITY_CHILD").is_none() {
        run_affinity_child(
            "scheduler::native_affinity_tests::native_affinity_survives_worker_shrink_and_clears_on_completion_or_cancel",
            false,
        );
        return;
    }
    let _guard = crate::gc::runtime_test_guard();
    for cancel in [false, true] {
        check_affinity_case(cancel, 4, 5000);
    }
}

#[test]
fn native_affinity_rendezvous_failure_reports_without_secondary_abort() {
    if std::env::var_os("WILLOW_AFFINITY_CHILD").is_none() {
        run_affinity_child(
            "scheduler::native_affinity_tests::native_affinity_rendezvous_failure_reports_without_secondary_abort",
            true,
        );
        return;
    }
    let _guard = crate::gc::runtime_test_guard();
    // Deliberately omit the fourth participant. This must report an ordinary
    // test failure after cleanup, not panic across the callback's C ABI.
    check_affinity_case(false, 3, 20);
}

#[test]
fn native_affinity_queries_scale_with_drives_not_resident_tasks() {
    let _guard = crate::gc::runtime_test_guard();
    println!("resident_tasks,drives,affinity_queries,affinity_updates");
    for residents in [16, 128, 1024] {
        for drives in [1, 8, 64] {
            reset_global_scheduler_for_test();
            replace_global_scheduler_for_test(1);
            with_global(|scheduler| {
                for _ in 0..residents {
                    scheduler.spawn_parked_placeholder();
                }
            });
            assert_eq!(global_task_table().len(), residents);
            let (queries, updates) = crate::native_stack::affinity_counts_for_test();
            crate::gc::willow_gc_register_mutator();
            for _ in 0..drives {
                assert_eq!(willow_sched_run_parallel(None, 1, None), 0);
            }
            crate::gc::willow_gc_unregister_mutator();
            let (after_queries, after_updates) = crate::native_stack::affinity_counts_for_test();
            assert_eq!(after_queries - queries, drives);
            assert_eq!(after_updates - updates, 0);
            println!(
                "{residents},{drives},{},{}",
                after_queries - queries,
                after_updates - updates
            );
        }
    }
    reset_global_scheduler_for_test();
}

#[test]
fn affinity_reroute_keeps_claim_visible_until_idle_snapshot_finishes() {
    let _guard = crate::gc::runtime_test_guard();
    reset_global_scheduler_for_test();
    replace_global_scheduler_for_test(2);
    let task = willow_sched_spawn(deep_affinity_poll, std::ptr::null_mut());
    // An unstarted stack provides real affinity without leaving a suspended
    // callback to unwind when the fixture is torn down.
    let stack = crate::native_stack::NativeStack::acquire(deep_affinity_poll, std::ptr::null_mut());
    let owner = stack.worker;
    global_task_table().with_mut(task, |record| record.native_stack = Some(stack));
    let state = Arc::new(ParallelRunState::default());
    let gate = state.claim_gate.lock().unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let claim_state = Arc::clone(&state);
    let claimer = std::thread::spawn(move || {
        let no_work = claim_global_ready_for_worker(owner + 1, Some(&claim_state)).is_none();
        done_tx.send(no_work).unwrap();
    });
    // Before the fix the foreign worker rerouted the task and dropped its
    // in-flight marker while this gate (the idle snapshot) was still held.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut escaped = None;
    while !claims_in_flight() && Instant::now() < deadline {
        if let Ok(result) = done_rx.try_recv() {
            escaped = Some(result);
            break;
        }
        std::thread::yield_now();
    }
    let escaped = escaped.or_else(|| done_rx.recv_timeout(Duration::from_millis(100)).ok());
    let visible = claims_in_flight();
    drop(gate);
    let no_work = escaped.unwrap_or_else(|| done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    claimer.join().unwrap();
    let queued = global_run_queues().contains(task);
    reset_global_scheduler_for_test();
    assert!(no_work, "a foreign worker must not claim an affined task");
    assert!(queued, "reroute must preserve the queue entry");
    assert!(
        escaped.is_none(),
        "affinity reroute escaped the idle snapshot gate"
    );
    assert!(
        visible,
        "the popped task must remain visible as an in-flight claim"
    );
}
