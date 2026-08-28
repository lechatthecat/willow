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
//! 20. a three-deep await chain resolves end to end in one `run_until`
//! 21. a nested drive keeps its paused outer poll visible to the snapshot

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

/// One node of an await chain: the id it awaits (0 for the tail), a turn
/// counter, and how many times this node has been polled. The poll counts are
/// the point — a node that never parks is polled once, a node that parks on
/// its child and is woken by it is polled twice.
#[repr(C)]
struct ChainFrame {
    child: u64,
    turns: i64,
    polls: i64,
}

fn chain_frame(child: u64) -> *mut c_void {
    Box::into_raw(Box::new(ChainFrame {
        child,
        turns: 0,
        polls: 0,
    })) as *mut c_void
}

/// Reclaim a chain frame and read back what its poll function recorded.
///
/// # Safety
///
/// `frame` must come from [`chain_frame`] and its task must be terminal, so no
/// poll can still be holding a reference to it.
unsafe fn take_chain_frame(frame: *mut c_void) -> ChainFrame {
    *unsafe { Box::from_raw(frame as *mut ChainFrame) }
}

/// How many chain nodes have registered themselves as waiters on the tail.
static CHAIN_PARKED: AtomicUsize = AtomicUsize::new(0);

/// The number of non-tail nodes in the chain built by perspective 20.
const CHAIN_WAITERS: usize = 2;

/// A chain node: registers as a waiter on its child and parks, then completes
/// when the wake re-polls it and the child is terminal. This is the shape the
/// compiler emits for `await <task>`.
unsafe extern "C" fn poll_await_child(frame: *mut c_void) -> i32 {
    let node = unsafe { &mut *(frame as *mut ChainFrame) };
    node.polls += 1;
    if node.child != 0 && willow_sched_await(node.child) == 0 {
        CHAIN_PARKED.fetch_add(1, Ordering::AcqRel);
        return RUNTIME_POLL_PENDING;
    }
    RUNTIME_POLL_READY
}

/// The tail of the chain: yields for a few turns and then refuses to finish
/// until every node above it is actually parked on it. Waiting for the parks is
/// what makes the wake order — and so the poll counts the test asserts —
/// deterministic for any number of workers, instead of leaving room for a run in
/// which the tail finished before the root ever registered. The turn ceiling is
/// only there so a broken wake path fails the assertions instead of hanging.
unsafe extern "C" fn poll_chain_tail(frame: *mut c_void) -> i32 {
    let node = unsafe { &mut *(frame as *mut ChainFrame) };
    node.polls += 1;
    node.turns += 1;
    let parked = CHAIN_PARKED.load(Ordering::Acquire);
    if (node.turns < 4 || parked < CHAIN_WAITERS) && node.turns < 100_000 {
        return RUNTIME_POLL_YIELD;
    }
    RUNTIME_POLL_READY
}

/// The highest `paused_polls` count seen from inside a nested drive.
static NESTED_PAUSED_PEAK: AtomicUsize = AtomicUsize::new(0);

/// Whether `WorkSource::Claim` covered that paused outer poll.
static NESTED_CLAIM_LIVE: AtomicBool = AtomicBool::new(false);

/// Whether the outer poll was counted as active again once its nested drive
/// returned, and what the pause count had fallen back to.
static NESTED_RESUMED_ACTIVE: AtomicBool = AtomicBool::new(false);
static NESTED_RESUMED_PAUSED: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Drives its child to completion with a NESTED `run_until` from inside its own
/// poll — the shape a blocking operation inside a task takes. This is the only
/// path that moves the outer poll out of `active_polls` and into `paused_polls`,
/// which perspective 7 can only check by incrementing the counter by hand.
unsafe extern "C" fn poll_nested_drive_child(frame: *mut c_void) -> i32 {
    let node = unsafe { &mut *(frame as *mut ChainFrame) };
    node.polls += 1;
    if node.child != 0 {
        willow_sched_run_until(node.child);
        node.child = 0;
        // The return leg: the pause has to be handed back to `active_polls`
        // before the poll resumes, or the snapshot would stop counting a poll
        // that is once again running.
        if let Some(state) = CURRENT_RUN_STATE.with(|slot| slot.borrow().clone()) {
            NESTED_RESUMED_ACTIVE.store(
                state.active_polls.load(Ordering::Acquire) >= 1,
                Ordering::Release,
            );
            NESTED_RESUMED_PAUSED.store(
                state.paused_polls.load(Ordering::Acquire),
                Ordering::Release,
            );
        }
    }
    RUNTIME_POLL_READY
}

/// Records how the parallel run state looks from inside the nested drive. The
/// outer poll is suspended somewhere up this worker's stack, so it must be
/// counted as paused and the run must not be reported idle. Yields while the
/// pause has not been published yet: another worker can reach this poll in the
/// window between the child's spawn and the parent's descent into the drive.
unsafe extern "C" fn poll_observe_nested_state(frame: *mut c_void) -> i32 {
    let node = unsafe { &mut *(frame as *mut ChainFrame) };
    node.polls += 1;
    let Some(state) = CURRENT_RUN_STATE.with(|slot| slot.borrow().clone()) else {
        // A single-worker drive has no `ParallelRunState`; there is nothing to
        // pause and nothing to observe.
        return RUNTIME_POLL_READY;
    };
    let paused = state.paused_polls.load(Ordering::Acquire);
    NESTED_PAUSED_PEAK.fetch_max(paused, Ordering::AcqRel);
    if work_source_is_live(&state, WorkSource::Claim) {
        NESTED_CLAIM_LIVE.store(true, Ordering::Release);
    }
    node.turns += 1;
    if paused == 0 && node.turns < 100_000 {
        return RUNTIME_POLL_YIELD;
    }
    RUNTIME_POLL_READY
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

/// `root` awaits `middle`, `middle` awaits `tail`, `tail` yields a few times and
/// completes. One unbounded `run_until(root)` has to carry the completion back
/// up the whole chain: tail completes, middle wakes and completes, root wakes
/// and completes. An idle-stop that fires while a wake is in flight strands the
/// rest of the chain, and the drive returns with `root` still alive.
#[test]
fn idle_20_an_await_chain_resolves_end_to_end_in_one_run_until() {
    let _guard = fresh_scheduler();
    CHAIN_PARKED.store(0, Ordering::Release);

    let tail_frame = chain_frame(0);
    let tail = willow_sched_spawn(poll_chain_tail, tail_frame);
    let middle_frame = chain_frame(tail);
    let middle = willow_sched_spawn(poll_await_child, middle_frame);
    let root_frame = chain_frame(middle);
    let root = willow_sched_spawn(poll_await_child, root_frame);

    assert_eq!(
        willow_sched_run_until(root),
        3,
        "the drive must complete the whole chain, not just its target"
    );
    for (name, id) in [("tail", tail), ("middle", middle), ("root", root)] {
        assert_eq!(willow_sched_task_state(id), -1, "{name} must be terminal");
    }

    assert_eq!(
        CHAIN_PARKED.load(Ordering::Acquire),
        CHAIN_WAITERS,
        "both chain nodes must have registered as waiters, not run straight through"
    );
    let tail_node = unsafe { take_chain_frame(tail_frame) };
    let middle_node = unsafe { take_chain_frame(middle_frame) };
    let root_node = unsafe { take_chain_frame(root_frame) };
    assert!(
        tail_node.turns >= 4,
        "the tail must have yielded several times, not completed on its first poll"
    );
    assert_eq!(
        middle_node.polls, 2,
        "the middle node must park on the tail and be woken by it exactly once"
    );
    assert_eq!(
        root_node.polls, 2,
        "the root must park on the middle node and be woken by it exactly once"
    );
}

/// Perspective 7 checks that a paused poll defeats the idle snapshot by moving
/// the counter by hand. This drives the real transition: a poll that re-enters
/// the scheduler is taken out of `active_polls` and put into `paused_polls` for
/// the duration, and a poll running under that nested drive must see the run as
/// busy on the strength of the pause alone.
#[test]
fn idle_21_a_nested_drive_keeps_its_paused_outer_poll_visible() {
    let _guard = fresh_scheduler();
    NESTED_PAUSED_PEAK.store(0, Ordering::Release);
    NESTED_CLAIM_LIVE.store(false, Ordering::Release);
    NESTED_RESUMED_ACTIVE.store(false, Ordering::Release);
    NESTED_RESUMED_PAUSED.store(usize::MAX, Ordering::Release);
    let workers = runtime_worker_config().active_workers();

    let child_frame = chain_frame(0);
    let child = willow_sched_spawn(poll_observe_nested_state, child_frame);
    let parent_frame = chain_frame(child);
    let parent = willow_sched_spawn(poll_nested_drive_child, parent_frame);

    assert_eq!(
        willow_sched_run_until(parent),
        2,
        "the nested drive's completion counts toward the run it is nested in"
    );
    assert_eq!(willow_sched_task_state(child), -1, "child must be terminal");
    assert_eq!(
        willow_sched_task_state(parent),
        -1,
        "parent must be terminal"
    );

    let child_node = unsafe { take_chain_frame(child_frame) };
    let parent_node = unsafe { take_chain_frame(parent_frame) };
    assert_eq!(parent_node.polls, 1, "the parent drives its child inline");
    assert!(child_node.polls >= 1, "the child must have been polled");

    if workers > 1 {
        assert!(
            NESTED_PAUSED_PEAK.load(Ordering::Acquire) >= 1,
            "the outer poll must be counted as paused while its nested drive runs"
        );
        assert!(
            NESTED_CLAIM_LIVE.load(Ordering::Acquire),
            "WorkSource::Claim must cover a worker that is inside a nested drive"
        );
        assert!(
            NESTED_RESUMED_ACTIVE.load(Ordering::Acquire),
            "the outer poll must be counted as active again once its nested drive returns"
        );
        assert_eq!(
            NESTED_RESUMED_PAUSED.load(Ordering::Acquire),
            0,
            "the pause must be given back, not left on the paused counter"
        );
    }
}
