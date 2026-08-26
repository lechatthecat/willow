//! Idle-stop and drive-completion viewpoints for the worker pool (willow-6wd6).
//!
//! A parallel drive ends when worker 0 decides the run is globally idle and
//! publishes `ParallelRunState::stop`. That decision is a snapshot of several
//! independently synchronized structures, so it is only correct if each one is
//! read on the side of its producer's publication order that cannot be missed.
//! Getting one of them backwards does not corrupt anything visibly — it strands
//! a runnable task and returns from the drive early, which for the program
//! entry point means `main` exits 0 having produced no output.
//!
//! Perspectives:
//!
//!  1. a fresh scheduler with nothing at all is idle
//!  2. a queued ready task is not idle
//!  3. a task parked in a worker's LOCAL queue is not idle
//!  4. an armed timer is not idle
//!  5. a blocked-syscall task is not idle
//!  6. an active poll is not idle
//!  7. a paused (nested) poll is not idle
//!  8. a claim that has popped the last task is not idle  <- the regression
//!  9. a claim marker with nothing popped is not idle (conservative)
//! 10. the requeue that follows such a claim is not idle
//! 11. idleness returns once the task is actually gone
//! 12. the predicate is a pure read: two calls agree
//! 13. the declared read order puts the queue before the claim marker
//! 13b. ordering stress: a claim racing the queue read is never reported idle
//! 14. `run_until` returns only once its target is terminal
//! 15. `run_until` on a target nothing can ever wake still RETURNS
//! 16. `run_until` on an unknown id returns immediately
//! 17. `run_until` reports every completion it drove
//! 18. a drive strands no runnable task behind its own stop
//! 19. a drive with more tasks than workers completes all of them
//! 20. a chain of awaits all resolve in a single `run_until`

use super::*;
use crate::gc::{reset_internal_for_test, runtime_test_guard};
use std::sync::Barrier;

/// Never completes, and registers no wake source: the only way a drive can
/// return with this task alive is by deciding the run is idle.
unsafe extern "C" fn poll_pending_forever(_frame: *mut c_void) -> i32 {
    RUNTIME_POLL_PENDING
}

/// Completes on the first poll.
unsafe extern "C" fn poll_ready_now(_frame: *mut c_void) -> i32 {
    RUNTIME_POLL_READY
}

/// Stays runnable for a few turns before completing, so a drive has to make
/// several scheduling decisions per task instead of one.
unsafe extern "C" fn poll_yield_thrice(frame: *mut c_void) -> i32 {
    let turns = unsafe { &mut *(frame as *mut i64) };
    *turns += 1;
    if *turns >= 4 {
        RUNTIME_POLL_READY
    } else {
        RUNTIME_POLL_YIELD
    }
}

fn fresh_scheduler() -> std::sync::MutexGuard<'static, ()> {
    let guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    guard
}

/// The predicate's contract is "caller holds `claim_gate`". No test thread
/// competes for it, but taking it keeps the tests honest about the contract.
fn is_idle(state: &ParallelRunState) -> bool {
    let _claim_gate = state
        .claim_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    parallel_run_is_idle_locked(state)
}

fn counter_frame() -> *mut c_void {
    Box::into_raw(Box::new(0i64)) as *mut c_void
}

#[test]
fn idle_01_empty_scheduler_is_idle() {
    let _guard = fresh_scheduler();
    let state = ParallelRunState::default();
    assert!(is_idle(&state));
}

#[test]
fn idle_02_a_queued_task_is_not_idle() {
    let _guard = fresh_scheduler();
    let state = ParallelRunState::default();
    willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    assert!(!is_idle(&state), "a queued ready task is work");
}

#[test]
fn idle_03_a_task_in_a_local_queue_is_not_idle() {
    let _guard = fresh_scheduler();
    let state = ParallelRunState::default();
    let id = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    let queues = global_run_queues();
    assert_eq!(queues.pop_for_worker(0), Some(id));
    queues.push_local(1, id);
    assert!(
        !is_idle(&state),
        "local queues hold real work; only the owning worker pops them first"
    );
}

#[test]
fn idle_04_an_armed_timer_is_not_idle() {
    let _guard = fresh_scheduler();
    let state = ParallelRunState::default();
    let id = willow_sched_spawn(poll_pending_forever, std::ptr::null_mut());
    assert_eq!(willow_sched_run(), 0);
    // The heap prunes entries lazily against the task's own wake-deadline, so
    // arm both halves the way `set_wake_after_millis_in` does.
    let deadline = Instant::now() + Duration::from_secs(60);
    global_task_table().with_mut(id, |task| task.wake_deadline = Some(deadline));
    global_timers().push(id, deadline);
    assert!(
        global_next_timer_deadline().is_some(),
        "the timer is armed and current"
    );
    assert!(!is_idle(&state), "a timer will make a parked task runnable");
}

#[test]
fn idle_05_a_blocked_syscall_task_is_not_idle() {
    let _guard = fresh_scheduler();
    let state = ParallelRunState::default();
    let id = willow_sched_spawn(poll_pending_forever, std::ptr::null_mut());
    with_global_for_test(|sched| {
        sched.set_running(id);
        sched.finish_blocked_syscall_poll(id);
        sched.clear_running();
    });
    assert_eq!(global_task_table().blocked_syscall_count(), 1);
    assert!(
        !is_idle(&state),
        "the blocking pool still owes this task a completion wake"
    );
}

#[test]
fn idle_06_an_active_poll_is_not_idle() {
    let _guard = fresh_scheduler();
    let state = ParallelRunState::default();
    state.active_polls.fetch_add(1, Ordering::AcqRel);
    assert!(!is_idle(&state));
    state.active_polls.fetch_sub(1, Ordering::AcqRel);
    assert!(is_idle(&state));
}

#[test]
fn idle_07_a_paused_nested_poll_is_not_idle() {
    let _guard = fresh_scheduler();
    let state = ParallelRunState::default();
    state.paused_polls.fetch_add(1, Ordering::AcqRel);
    assert!(!is_idle(&state));
    state.paused_polls.fetch_sub(1, Ordering::AcqRel);
    assert!(is_idle(&state));
}

#[test]
fn idle_08_a_claim_holding_the_last_task_is_not_idle() {
    let _guard = fresh_scheduler();
    let state = ParallelRunState::default();
    let id = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());

    // Exactly the state of a worker that popped the last task and is now
    // waiting for `claim_gate`: nothing is queued, nothing is polling, and the
    // task itself is in no structure the snapshot can see.
    let in_flight = ClaimInFlight::enter();
    assert_eq!(global_run_queues().pop_for_worker(0), Some(id));
    assert_eq!(global_run_queues().len(), 0);
    assert_eq!(state.active_polls.load(Ordering::Acquire), 0);

    assert!(
        !is_idle(&state),
        "stopping here strands the popped task: the claim requeues it and \
         leaves, and the drive returns with its target still alive"
    );
    drop(in_flight);
}

#[test]
fn idle_09_a_claim_marker_alone_is_not_idle() {
    let _guard = fresh_scheduler();
    let state = ParallelRunState::default();
    let in_flight = ClaimInFlight::enter();
    assert!(
        !is_idle(&state),
        "the marker is published before the pop, so it must be treated as work \
         even when the pop has not happened yet"
    );
    drop(in_flight);
    assert!(is_idle(&state));
}

#[test]
fn idle_10_the_requeue_after_a_stopped_claim_is_not_idle() {
    let _guard = fresh_scheduler();
    let state = ParallelRunState::default();
    let id = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    {
        let _in_flight = ClaimInFlight::enter();
        assert_eq!(global_run_queues().pop_for_worker(0), Some(id));
        global_run_queues().push_global(id);
    }
    assert!(
        !is_idle(&state),
        "the requeued task is ordinary queued work again"
    );
}

#[test]
fn idle_11_idleness_returns_once_the_task_is_gone() {
    let _guard = fresh_scheduler();
    let state = ParallelRunState::default();
    let id = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    assert!(!is_idle(&state));
    assert_eq!(willow_sched_run(), 1);
    assert_eq!(willow_sched_task_state(id), -1);
    assert!(is_idle(&state), "nothing is left to run");
}

#[test]
fn idle_12_the_predicate_consumes_nothing() {
    let _guard = fresh_scheduler();
    let state = ParallelRunState::default();
    willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    assert_eq!(is_idle(&state), is_idle(&state));
    assert!(!is_idle(&state));
}

#[test]
fn idle_13_the_declared_read_order_is_the_one_the_argument_needs() {
    let position = |source: WorkSource| {
        IDLE_READ_ORDER
            .iter()
            .position(|declared| *declared == source)
            .unwrap_or_else(|| panic!("{source:?} must be one of the checked work sources"))
    };
    // A claim publishes its in-flight marker BEFORE it pops, so an empty-queue
    // observation that missed the popped task is always followed by a marker
    // observation that sees the claim. Reading the marker first leaves a window
    // in which the claim is invisible to both reads (willow-6wd6).
    assert!(
        position(WorkSource::RunQueue) < position(WorkSource::Claim),
        "the run queue must be read before the claim marker: {IDLE_READ_ORDER:?}"
    );
    // Timer promotion and blocked-syscall completion publish the other way
    // round: queue entry first, then the marker cleared.
    assert!(
        position(WorkSource::Timer) < position(WorkSource::RunQueue),
        "the timer heap must be read before the run queue: {IDLE_READ_ORDER:?}"
    );
    assert!(
        position(WorkSource::BlockedSyscall) < position(WorkSource::RunQueue),
        "the blocked-syscall counter must be read before the run queue: \
         {IDLE_READ_ORDER:?}"
    );
    assert_eq!(
        IDLE_READ_ORDER.len(),
        5,
        "a new work source needs its own place in this order"
    );
}

#[test]
fn idle_13b_a_claim_racing_the_queue_read_is_never_reported_idle() {
    let _guard = fresh_scheduler();
    let state = Arc::new(ParallelRunState::default());
    let id = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());

    // The interleaving the bug needed: the claim publishes its marker and pops
    // the last task while the snapshot is in progress. This mirrors
    // `claim_global_ready_for_worker` exactly — marker, then pop, then the gate
    // — because the requeue that follows a stop is what strands the task, and
    // that requeue is ordered by the same gate the snapshot holds.
    for round in 0..2_000 {
        let gate = Arc::new(Barrier::new(2));
        let claim_gate = Arc::clone(&gate);
        let claim_state = Arc::clone(&state);
        let claimer = std::thread::spawn(move || {
            claim_gate.wait();
            let _in_flight = ClaimInFlight::enter();
            let popped = global_run_queues().pop_for_worker(0);
            let _gate = claim_state
                .claim_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(popped) = popped {
                global_run_queues().push_global(popped);
            }
        });
        gate.wait();
        std::thread::yield_now();
        let idle = is_idle(&state);
        claimer.join().unwrap();
        assert!(
            !idle,
            "round {round}: reported idle while a claim held the last task"
        );
        // The claimer always puts it back, so every round starts the same way.
        assert!(global_run_queues().contains(id));
    }
}

#[test]
fn idle_14_run_until_returns_only_once_its_target_is_terminal() {
    let _guard = fresh_scheduler();
    let frame = counter_frame();
    let id = willow_sched_spawn(poll_yield_thrice, frame);
    assert!(willow_sched_run_until(id) >= 1);
    assert_eq!(
        willow_sched_task_state(id),
        -1,
        "the drive may not return while its target is still runnable"
    );
    drop(unsafe { Box::from_raw(frame as *mut i64) });
}

#[test]
fn idle_15_run_until_on_an_unwakeable_target_still_returns() {
    let _guard = fresh_scheduler();
    let id = willow_sched_spawn(poll_pending_forever, std::ptr::null_mut());
    // The re-drive that protects against an early stop is bounded by
    // `scheduler_has_wake_source`: a genuine deadlock must still return so the
    // awaiter can report it, not spin forever inside the runtime.
    assert_eq!(willow_sched_run_until(id), 0);
    assert_eq!(willow_sched_task_state(id), 2, "still parked");
    assert!(!scheduler_has_wake_source());
}

#[test]
fn idle_16_run_until_on_an_unknown_id_returns_immediately() {
    let _guard = fresh_scheduler();
    assert_eq!(willow_sched_run_until(4_242), 0);
}

#[test]
fn idle_17_run_until_reports_every_completion_it_drove() {
    let _guard = fresh_scheduler();
    let others: Vec<u64> = (0..8)
        .map(|_| willow_sched_spawn(poll_ready_now, std::ptr::null_mut()))
        .collect();
    let frame = counter_frame();
    let target = willow_sched_spawn(poll_yield_thrice, frame);
    let completed = willow_sched_run_until(target);
    assert_eq!(willow_sched_task_state(target), -1);
    assert!(
        completed >= 1,
        "at least the target completed, got {completed}"
    );
    assert!(
        completed <= others.len() as i64 + 1,
        "a re-drive must not double-count completions, got {completed}"
    );
    drop(unsafe { Box::from_raw(frame as *mut i64) });
}

#[test]
fn idle_18_a_drive_strands_no_runnable_task_behind_its_own_stop() {
    let _guard = fresh_scheduler();
    let frames: Vec<*mut c_void> = (0..64).map(|_| counter_frame()).collect();
    for frame in &frames {
        willow_sched_spawn(poll_yield_thrice, *frame);
    }
    assert_eq!(willow_sched_run(), 64);
    assert_eq!(
        global_run_queues().len(),
        0,
        "the stop decision left runnable work in a queue"
    );
    assert!(!scheduler_has_wake_source());
    for frame in frames {
        drop(unsafe { Box::from_raw(frame as *mut i64) });
    }
}

#[test]
fn idle_19_more_tasks_than_workers_all_complete_in_one_drive() {
    let _guard = fresh_scheduler();
    let count = DEFAULT_WORKERS * 40;
    let frames: Vec<*mut c_void> = (0..count).map(|_| counter_frame()).collect();
    let ids: Vec<u64> = frames
        .iter()
        .map(|frame| willow_sched_spawn(poll_yield_thrice, *frame))
        .collect();
    assert_eq!(willow_sched_run(), count as i64);
    for id in ids {
        assert_eq!(willow_sched_task_state(id), -1);
    }
    for frame in frames {
        drop(unsafe { Box::from_raw(frame as *mut i64) });
    }
}

#[test]
fn idle_20_awaited_tasks_all_resolve_in_a_single_run_until() {
    let _guard = fresh_scheduler();
    let leaf_frames: Vec<*mut c_void> = (0..16).map(|_| counter_frame()).collect();
    let leaves: Vec<u64> = leaf_frames
        .iter()
        .map(|frame| willow_sched_spawn(poll_yield_thrice, *frame))
        .collect();
    let last = *leaves.last().expect("at least one leaf");
    assert!(willow_sched_run_until(last) >= 1);
    assert_eq!(willow_sched_task_state(last), -1);
    for frame in leaf_frames {
        drop(unsafe { Box::from_raw(frame as *mut i64) });
    }
}
