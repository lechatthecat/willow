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
//! 13. the declared read order puts polls before the queue and the queue
//!     before the claim marker
//! 13b. ordering stress: a claim racing the stop decision is never missed
//! 14. `run_until` returns only once its target is terminal
//! 15. `run_until` on a target nothing can ever wake still RETURNS
//! 16. `run_until` on an unknown id returns immediately
//! 17. `run_until` reports every completion it drove
//! 18. a drive strands no runnable task behind its own stop
//! 19. a drive with more tasks than workers completes all of them
//! 20. a three-deep await chain resolves end to end in one `run_until`
//! 21. a nested drive keeps its paused outer poll visible to the snapshot
//!
//! Gate-free claims (willow-8hq4.19): a claim takes `claim_gate` only while a
//! stop decision has raised `stop_intent`.
//!
//! 22. a quiescent decision publishes the stop and lowers the intent
//! 23. a claim in flight when the decision starts refuses the stop
//! 24. a claim that pops and requeues during the snapshot refuses the stop
//! 25. a claim that pops nothing during the snapshot does not refuse it
//! 26. a claim with no decision in progress never waits for `claim_gate`
//! 27. a claim that sees the intent waits and then honours a published stop
//! 28. a claim that sees the intent proceeds when the decision refuses
//! 29. a pause/resume in progress is never seen as "no poll"
//! 30. a poll that requeues and ends mid-snapshot is never missed

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

/// The bare predicate, read with claims excluded. No test thread competes for
/// the gate here, but taking it keeps the tests honest about the contract;
/// racing tests use the full decision, [`ParallelRunState::publish_stop_if`].
fn is_idle(state: &ParallelRunState) -> bool {
    let _claim_gate = state
        .claim_gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    parallel_run_is_idle_locked(state)
}

// Every scheduler frame reserves the ABI header, even when the fixture uses
// only the state word. Completion publishes status in the third header word.
type CounterFrame = [i64; crate::async_frame::ASYNC_FRAME_HEADER_WORDS as usize];

fn counter_frame() -> *mut c_void {
    Box::into_raw(Box::<CounterFrame>::new(
        [0; crate::async_frame::ASYNC_FRAME_HEADER_WORDS as usize],
    )) as *mut c_void
}

/// One node of an await chain: the id it awaits (0 for the tail), a turn
/// counter, and how many times this node has been polled. The poll counts are
/// the point — a node that never parks is polled once, a node that parks on
/// its child and is woken by it is polled twice.
#[repr(C)]
struct ChainFrame {
    header: CounterFrame,
    child: u64,
    turns: i64,
    polls: i64,
}

fn chain_frame(child: u64) -> *mut c_void {
    Box::into_raw(Box::new(ChainFrame {
        header: [0; crate::async_frame::ASYNC_FRAME_HEADER_WORDS as usize],
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
            NESTED_RESUMED_ACTIVE.store(state.active_polls() >= 1, Ordering::Release);
            NESTED_RESUMED_PAUSED.store(state.paused_polls(), Ordering::Release);
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
    let paused = state.paused_polls();
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
    state.begin_poll();
    assert!(!is_idle(&state));
    state.end_poll();
    assert!(is_idle(&state));
}

#[test]
fn idle_07_a_paused_nested_poll_is_not_idle() {
    let _guard = fresh_scheduler();
    let state = ParallelRunState::default();
    state.set_polls_for_test(0, 1);
    assert!(!is_idle(&state));
    state.set_polls_for_test(0, 0);
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
    assert_eq!(state.active_polls(), 0);

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
    // A poll publishes its requeue BEFORE it leaves `active_polls`, so the
    // poll counters must be read before the queue (willow-8hq4.19).
    assert!(
        position(WorkSource::Poll) < position(WorkSource::RunQueue),
        "the poll counters must be read before the run queue: {IDLE_READ_ORDER:?}"
    );
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
        6,
        "a new work source needs its own place in this order"
    );
}

#[test]
fn idle_13b_a_claim_racing_the_stop_decision_is_never_missed() {
    let _guard = fresh_scheduler();
    let id = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());

    // The task is always either queued, held by a claim, or being polled, so
    // no decision may ever stop. The claimer runs the real claim, including
    // its gate-free fast path, and a claimed poll requeues through the real
    // poll boundary, so every interleaving of pop, claim and requeue against
    // the decision's reads is exercised.
    for round in 0..2_000 {
        let state = Arc::new(ParallelRunState::default());
        let gate = Arc::new(Barrier::new(2));
        let claim_gate = Arc::clone(&gate);
        let claim_state = Arc::clone(&state);
        let claimer = std::thread::spawn(move || {
            claim_gate.wait();
            match claim_global_ready_for_worker(0, Some(&claim_state)) {
                Some((claimed, _work)) => {
                    finish_global_poll_boundary(claimed, GlobalPollBoundary::Runnable);
                    finish_active_poll(Some(&claim_state));
                    set_current_task(None);
                    true
                }
                None => false,
            }
        });
        gate.wait();
        std::thread::yield_now();
        let stopped = state.publish_stop_if(parallel_run_is_idle_locked);
        let claimed = claimer.join().unwrap();
        assert!(
            !stopped,
            "round {round}: stopped while the task was live (claimed: {claimed})"
        );
        assert!(
            !state.stop_decision_pending(),
            "round {round}: intent left raised"
        );
        assert!(global_run_queues().contains(id), "round {round}: task lost");
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
    drop(unsafe { Box::from_raw(frame as *mut CounterFrame) });
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
    drop(unsafe { Box::from_raw(frame as *mut CounterFrame) });
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
        drop(unsafe { Box::from_raw(frame as *mut CounterFrame) });
    }
}

#[test]
fn idle_19_more_tasks_than_workers_all_complete_in_one_drive() {
    let _guard = fresh_scheduler();
    let count = runtime_worker_config().active_workers() * 40;
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
        drop(unsafe { Box::from_raw(frame as *mut CounterFrame) });
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

fn claim_word_count() -> u64 {
    claim_word() & CLAIM_COUNT_MASK
}

#[test]
fn idle_22_a_quiescent_decision_publishes_the_stop() {
    let _guard = fresh_scheduler();
    let state = ParallelRunState::default();
    assert!(state.publish_stop_if(parallel_run_is_idle_locked));
    assert!(state.stop.load(Ordering::Acquire));
    assert!(!state.stop_decision_pending(), "the intent must be lowered");
}

#[test]
fn idle_23_a_claim_in_flight_refuses_the_stop() {
    let _guard = fresh_scheduler();
    let state = ParallelRunState::default();
    let in_flight = ClaimInFlight::enter();
    assert!(
        !state.publish_stop_if(|_| true),
        "even a quiescent snapshot cannot stop past a claim in flight"
    );
    assert!(!state.stop.load(Ordering::Acquire));
    assert!(!state.stop_decision_pending());
    drop(in_flight);
    assert!(state.publish_stop_if(|_| true));
}

#[test]
fn idle_24_a_claim_that_resolves_mid_snapshot_refuses_the_stop() {
    let _guard = fresh_scheduler();
    let id = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    // Enter, pop, requeue and leave between two of the decision's reads, at
    // every position: the count is back to zero, but the epoch moved.
    for split in 0..=IDLE_READ_ORDER.len() {
        let state = ParallelRunState::default();
        let stopped = state.publish_stop_if(|state| {
            idle_with_event_at(state, split, || {
                let mut in_flight = ClaimInFlight::enter();
                assert_eq!(global_run_queues().pop_for_worker(0), Some(id));
                in_flight.popped();
                global_run_queues().push_global(id);
                drop(in_flight);
                assert_eq!(claim_word_count(), 0);
            })
        });
        assert!(
            !stopped,
            "a claim resolved after read {split} of {IDLE_READ_ORDER:?} was missed"
        );
        assert!(!state.stop.load(Ordering::Acquire));
        assert!(global_run_queues().contains(id));
    }
    // The epoch alone carries it: with the queue read last, the bare
    // predicate reports idle for this interleaving.
    let state = ParallelRunState::default();
    let stopped = state.publish_stop_if(|_| {
        let mut in_flight = ClaimInFlight::enter();
        assert_eq!(global_run_queues().pop_for_worker(0), Some(id));
        in_flight.popped();
        global_run_queues().push_global(id);
        true
    });
    assert!(!stopped, "a resolved claim may have republished work");
}

#[test]
fn idle_25_a_claim_that_popped_nothing_does_not_refuse_the_stop() {
    let _guard = fresh_scheduler();
    let state = ParallelRunState::default();
    let before = claim_word();
    let stopped = state.publish_stop_if(|_| {
        let in_flight = ClaimInFlight::enter();
        assert_eq!(global_run_queues().pop_for_worker(0), None);
        drop(in_flight);
        true
    });
    assert!(
        stopped,
        "an empty claim changes nothing and must not livelock"
    );
    assert_eq!(
        claim_word(),
        before,
        "an empty claim leaves the word as it was"
    );
}

#[test]
fn idle_26_a_claim_without_a_decision_never_waits_for_the_gate() {
    let _guard = fresh_scheduler();
    let state = Arc::new(ParallelRunState::default());
    let id = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    let held = state.claim_gate.lock().unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let claim_state = Arc::clone(&state);
    let claimer = std::thread::spawn(move || {
        let claimed = claim_global_ready_for_worker(0, Some(&claim_state)).map(|(id, _)| id);
        set_current_task(None);
        done_tx.send(claimed).unwrap();
    });
    let claimed = done_rx.recv_timeout(Duration::from_secs(5));
    drop(held);
    claimer.join().unwrap();
    assert_eq!(
        claimed,
        Ok(Some(id)),
        "the claim fast path blocked on claim_gate"
    );
    assert_eq!(state.active_polls(), 1);
    assert_eq!(claim_word_count(), 0);
}

/// Hold the gate with the intent raised, exactly as a decision in progress
/// does, and start a claim against it.
fn claim_behind_a_decision(
    state: &Arc<ParallelRunState>,
) -> (
    std::sync::MutexGuard<'_, ()>,
    std::sync::mpsc::Receiver<Option<RuntimeTaskId>>,
    std::thread::JoinHandle<()>,
) {
    let held = state.claim_gate.lock().unwrap();
    state.stop_intent.store(true, Ordering::SeqCst);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let claim_state = Arc::clone(state);
    let claimer = std::thread::spawn(move || {
        let claimed = claim_global_ready_for_worker(0, Some(&claim_state)).map(|(id, _)| id);
        set_current_task(None);
        done_tx.send(claimed).unwrap();
    });
    // The claim is visible (and holding the popped task) before it blocks.
    let deadline = Instant::now() + Duration::from_secs(5);
    while claim_word_count() == 0 && Instant::now() < deadline {
        std::thread::yield_now();
    }
    (held, done_rx, claimer)
}

#[test]
fn idle_27_a_claim_behind_a_decision_honours_its_stop() {
    let _guard = fresh_scheduler();
    let state = Arc::new(ParallelRunState::default());
    let id = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    let (held, done_rx, claimer) = claim_behind_a_decision(&state);
    assert!(
        claims_in_flight(),
        "the claim must be visible while it waits"
    );
    assert!(
        done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
        "a claim must not resolve while a decision is in progress"
    );
    // Publish a stop the way a decision would, then release the gate.
    state.stop.store(true, Ordering::SeqCst);
    state.stop_intent.store(false, Ordering::SeqCst);
    drop(held);
    let claimed = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    claimer.join().unwrap();
    assert_eq!(claimed, None, "a stopped run claims nothing");
    assert!(
        global_run_queues().contains(id),
        "the popped task is requeued"
    );
    assert_eq!(state.active_polls(), 0);
    assert_eq!(claim_word_count(), 0);
}

#[test]
fn idle_28_a_claim_behind_a_refused_decision_proceeds() {
    let _guard = fresh_scheduler();
    let state = Arc::new(ParallelRunState::default());
    let id = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    let (held, done_rx, claimer) = claim_behind_a_decision(&state);
    state.stop_intent.store(false, Ordering::SeqCst);
    drop(held);
    let claimed = done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    claimer.join().unwrap();
    assert_eq!(claimed, Some(id));
    assert_eq!(state.active_polls(), 1);
}

#[test]
fn idle_29_a_moving_poll_is_always_live() {
    let _guard = fresh_scheduler();
    let state = Arc::new(ParallelRunState::default());
    state.begin_poll();
    let done = Arc::new(AtomicBool::new(false));
    let mover_state = Arc::clone(&state);
    let mover_done = Arc::clone(&done);
    let mover = std::thread::spawn(move || {
        for _ in 0..200_000 {
            mover_state.pause_active_poll();
            mover_state.resume_paused_poll();
        }
        mover_done.store(true, Ordering::Release);
    });
    let mut reads = 0u64;
    while !done.load(Ordering::Acquire) {
        assert!(
            work_source_is_live(&state, WorkSource::Poll),
            "a poll moving between active and paused read as absent"
        );
        reads += 1;
    }
    mover.join().unwrap();
    assert!(reads > 0);
    assert_eq!(state.active_polls(), 1);
    assert_eq!(state.paused_polls(), 0);
}

/// Evaluate the idle predicate with `between` run after the first `split`
/// reads, exactly as a producer racing the snapshot could.
fn idle_with_event_at(state: &ParallelRunState, split: usize, between: impl FnOnce()) -> bool {
    let live = |source: &WorkSource| work_source_is_live(state, *source);
    let before = IDLE_READ_ORDER[..split].iter().any(live);
    between();
    let after = IDLE_READ_ORDER[split..].iter().any(live);
    !before && !after
}

#[test]
fn idle_30_a_poll_that_requeues_and_ends_mid_snapshot_is_never_missed() {
    let _guard = fresh_scheduler();
    let id = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    for split in 0..=IDLE_READ_ORDER.len() {
        let state = ParallelRunState::default();
        // A claimed poll in progress: the task is in no queue, one poll active.
        let (claimed, _) = claim_global_ready_for_worker(0, Some(&state)).unwrap();
        set_current_task(None);
        assert_eq!(claimed, id);
        let stopped = state.publish_stop_if(|state| {
            idle_with_event_at(state, split, || {
                // A yield: requeue first, then leave the poll count.
                finish_global_poll_boundary(id, GlobalPollBoundary::Runnable);
                finish_active_poll(Some(state));
            })
        });
        assert!(
            !stopped,
            "stopped with the task requeued when the poll ended after read {split} \
             of {IDLE_READ_ORDER:?}"
        );
        assert!(global_run_queues().contains(id));
    }
}
