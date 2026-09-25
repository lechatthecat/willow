fn channel_token(channel: usize) -> crate::task::ChannelOwnershipToken {
    crate::task::ChannelOwnershipToken {
        channel,
        role: crate::task::ChannelRole::RecvWait,
        generation: 1,
    }
}

use super::*;
use crate::async_frame::{async_frame_slot_offset, willow_async_frame_alloc};
use crate::gc::{
    reset_internal_for_test, runtime_test_guard, willow_alloc_typed, willow_gc_allocated_bytes,
    willow_gc_collect,
};
use crate::task::RUNTIME_POLL_PENDING;
use std::collections::HashSet;
use std::sync::atomic::{
    AtomicBool as TestAtomicBool, AtomicU64 as TestAtomicU64, AtomicUsize as TestAtomicUsize,
    Ordering as TestOrdering,
};
use std::sync::{Barrier, LazyLock as TestLazyLock, Mutex as TestMutex};

static NESTED_QUANTUM_TARGET: TestAtomicU64 = TestAtomicU64::new(0);
static NESTED_QUANTUM_RESTORED: TestAtomicBool = TestAtomicBool::new(false);

#[test]
fn idle_wait_observes_wake_between_snapshot_and_wait() {
    let generation = current_wake_generation();
    notify_all_idle_waiters();
    assert!(
        wait_for_wake_since(generation, Duration::ZERO),
        "a wake after the snapshot must prevent the fallback timeout"
    );
}

unsafe extern "C" fn poll_nested_then_check_quantum(_frame: *mut c_void) -> i32 {
    let target = NESTED_QUANTUM_TARGET.load(TestOrdering::SeqCst);
    willow_sched_run_until(target);
    for _ in 0..crate::preempt::willow_preempt_task_budget() {
        if crate::preempt::willow_preempt_check() != 0 {
            NESTED_QUANTUM_RESTORED.store(true, TestOrdering::SeqCst);
            break;
        }
    }
    RUNTIME_POLL_READY
}

#[test]
fn nested_scheduler_restores_outer_task_quantum() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    NESTED_QUANTUM_RESTORED.store(false, TestOrdering::SeqCst);

    let target = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    NESTED_QUANTUM_TARGET.store(target, TestOrdering::SeqCst);
    willow_sched_spawn(poll_nested_then_check_quantum, std::ptr::null_mut());

    assert_eq!(willow_sched_run(), 2);
    assert!(
        NESTED_QUANTUM_RESTORED.load(TestOrdering::SeqCst),
        "nested run_until must rebind the outer task's quantum"
    );
    reset_internal_for_test();
}

// ── Work-stealing run queues (willow-gyaa.4) ────────────────────────────

#[test]
fn workqueue_pops_local_before_global() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    s.enqueue_local(0, 10);
    s.enqueue_ready(20); // global
    assert_eq!(s.pop_for_worker(0), Some(10), "local queue drains first");
    assert_eq!(s.pop_for_worker(0), Some(20), "then the global queue");
    assert_eq!(s.pop_for_worker(0), None);
}

#[test]
fn workqueue_repeated_local_work_cannot_starve_global_overflow() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    s.enqueue_local(0, 10);
    s.enqueue_local(0, 11);
    s.enqueue_local(0, 12);
    s.enqueue_ready(20);

    assert_eq!(s.pop_for_worker(0), Some(10), "first turn keeps locality");
    assert_eq!(
        s.pop_for_worker(0),
        Some(20),
        "next turn must service newly spawned or externally woken work"
    );
}

#[test]
fn workqueue_idle_worker_steals_from_other_local() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    // Only worker 1 has local work; worker 0 (idle) must steal it.
    s.enqueue_local(1, 7);
    assert_eq!(
        s.pop_for_worker(0),
        Some(7),
        "idle worker steals sibling work"
    );
    assert_eq!(s.pop_for_worker(0), None);
}

#[test]
fn workqueue_steal_takes_back_of_victim_queue() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    s.enqueue_local(1, 1);
    s.enqueue_local(1, 2); // back of victim
    // Steal takes the back (oldest-pushed / coldest) item first.
    assert_eq!(s.pop_for_worker(0), Some(2));
    // Worker 1 still pops its own from the front.
    assert_eq!(s.pop_for_worker(1), Some(1));
}

#[test]
fn workqueue_ready_total_counts_all_queues() {
    let mut s = RuntimeScheduler::with_worker_count(3);
    s.enqueue_local(0, 1);
    s.enqueue_local(2, 2);
    s.enqueue_ready(3);
    assert_eq!(s.ready_total(), 3);
    assert_eq!(s.worker_count(), 3);
}

#[test]
fn workqueue_empty_pop_returns_none() {
    let mut s = RuntimeScheduler::with_worker_count(3);
    assert_eq!(s.pop_for_worker(0), None);
    assert_eq!(s.pop_for_worker(2), None);
    assert_eq!(s.ready_total(), 0);
}

#[test]
fn workqueue_enqueue_local_out_of_range_falls_to_global() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    s.enqueue_local(99, 5); // no such worker -> global
    // Any worker can pick it up from the global queue.
    assert_eq!(s.pop_for_worker(1), Some(5));
}

#[test]
fn workqueue_steal_scans_workers_in_round_robin_order() {
    let mut s = RuntimeScheduler::with_worker_count(3);
    // Worker 0 is idle; both worker 1 and worker 2 have work. The steal scan
    // starts at the next worker (1) and takes from there first.
    s.enqueue_local(1, 11);
    s.enqueue_local(2, 22);
    assert_eq!(s.pop_for_worker(0), Some(11), "steal nearest victim first");
    assert_eq!(s.pop_for_worker(0), Some(22), "then the next victim");
    assert_eq!(s.pop_for_worker(0), None);
}

#[test]
fn workqueue_pop_ready_uses_worker_zero() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_placeholder();
    // pop_ready() is the worker-0 view used by the cooperative run loop.
    assert_eq!(s.pop_ready(), Some(id));
}

#[test]
fn workqueue_claim_discards_duplicate_entry_for_running_task() {
    let mut scheduler = RuntimeScheduler::with_worker_count(5);
    let id = scheduler.spawn_placeholder();
    // Forced at the queue level: `enqueue_ready` itself can no longer
    // create a duplicate (willow-ezs.1.1), but the claim-side guard must
    // still discard one if it ever appears.
    scheduler.run_queues.force_push_global(id);

    assert_eq!(scheduler.claim_ready_for_worker(0), Some(id));
    assert_eq!(scheduler.task_state(id), Some(RuntimeTaskState::Running));
    assert_eq!(
        scheduler.claim_ready_for_worker(1),
        None,
        "a stale duplicate must not let another worker poll a Running task"
    );
    scheduler.clear_running();
}

// ── O(1) run-queue membership invariant (willow-ezs.1.1) ────────────────
//
// `queued == true` means exactly one runnable claim owns the task id. The flag
// replaces the `VecDeque::contains()` scans the enqueue/requeue path used
// to run across the global queue and every local one, so the cost of
// publishing work no longer grows with the number of queued tasks.
//
// Every transition that can publish or consume a queue entry is covered:
//
//  1. a spawn publishes exactly one entry and sets the flag
//  2. physical popping keeps the flag until claim/state validation
//  3. a repeated global enqueue cannot create a second entry
//  4. a local enqueue of an already-queued task cannot either
//  5. an out-of-range local enqueue still obeys the invariant
//  6. a steal clears the flag just like a local pop
//  7. claiming sets Running and leaves the task unqueued
//  8. a non-Ready (Running) task is never published by an enqueue
//  9. a parked task is published by exactly one wake
// 10. a second wake of an already-queued task adds nothing
// 11. a wake DURING a poll records `wake_requested` instead of queueing
// 12. the post-poll transition publishes that deferred wake once
// 13. a Pending poll with no wake parks the task and queues nothing
// 14. a blocked-syscall poll parks in its own state, unqueued
// 15. waking a blocked-syscall task publishes one entry
// 16. `yield` requeues exactly once
// 17. a preemption requeue is idempotent
// 18. a due timer wake publishes exactly one entry
// 19. finalizing a queued task as Cancelled leaves a stale entry that the
//     claim discards, and the pop clears the flag
// 20. a completed task's stale entry is discarded the same way
// 21. `is_queued` agrees with a full scan of every queue
// 22. a long mixed sequence of transitions keeps flags and queues in sync,
//     with no duplicate entry anywhere
// 23. cancelling a parked task on the global scheduler queues it once
// 24. no task is polled concurrently twice: two workers claiming the same
//     stale duplicate cannot both get it

#[test]
fn runq_01_spawn_publishes_one_entry() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_placeholder();
    assert!(s.is_queued(id));
    assert_eq!(s.ready_total(), 1);
    assert!(s.queue_invariant_holds());
}

#[test]
fn runq_02_physical_pop_keeps_the_claim_token_until_atomic_claim() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_placeholder();
    assert_eq!(s.pop_for_worker(0), Some(id));
    assert!(
        s.is_queued(id),
        "the popped worker owns the outstanding claim until claim_for_poll"
    );
    assert_eq!(
        s.with_task_mut(id, |task| task.claim_for_poll()),
        Some(ClaimOutcome::Poll)
    );
    assert!(!s.is_queued(id));
}

#[test]
fn runq_03_repeated_global_enqueue_is_idempotent() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_placeholder();
    s.enqueue_ready(id);
    s.enqueue_ready(id);
    assert_eq!(s.ready_total(), 1, "one queue entry per queued task");
    assert!(s.queue_invariant_holds());
}

#[test]
fn runq_04_local_enqueue_of_queued_task_is_idempotent() {
    let mut s = RuntimeScheduler::with_worker_count(3);
    let id = s.spawn_placeholder(); // global
    s.enqueue_local(2, id);
    assert_eq!(s.ready_total(), 1);
    assert!(s.queue_invariant_holds());
}

#[test]
fn runq_05_out_of_range_local_enqueue_obeys_invariant() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_parked_placeholder();
    s.wake(id); // -> Ready, global
    s.enqueue_local(99, id); // no such worker: would fall to global
    assert_eq!(s.ready_total(), 1);
    assert!(s.queue_invariant_holds());
}

#[test]
fn runq_06_steal_clears_the_flag() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_parked_placeholder();
    s.wake(id);
    // Move it to worker 1's local queue, then have worker 0 steal it.
    assert_eq!(s.pop_for_worker(0), Some(id));
    assert_eq!(
        s.with_task_mut(id, |task| task.claim_for_poll()),
        Some(ClaimOutcome::Poll)
    );
    assert_eq!(
        s.with_task_mut(id, |task| task.state.requeue_after_poll()),
        Some(BoundaryOutcome::Requeue)
    );
    s.run_queues.push_local_front(1, id);
    assert!(s.is_queued(id));
    assert_eq!(s.pop_for_worker(0), Some(id), "worker 0 steals it");
    assert!(s.is_queued(id));
    assert_eq!(
        s.with_task_mut(id, |task| task.claim_for_poll()),
        Some(ClaimOutcome::Poll)
    );
    assert!(!s.is_queued(id));
}

#[test]
fn runq_07_claim_leaves_running_task_unqueued() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_placeholder();
    assert_eq!(s.claim_ready_for_worker(0), Some(id));
    assert_eq!(s.task_state(id), Some(RuntimeTaskState::Running));
    assert!(!s.is_queued(id));
    assert!(s.queue_invariant_holds());
    s.clear_running();
}

#[test]
fn runq_08_enqueue_never_publishes_a_running_task() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_placeholder();
    assert_eq!(s.claim_ready_for_worker(0), Some(id));
    s.enqueue_ready(id);
    s.enqueue_local(1, id);
    assert_eq!(
        s.ready_total(),
        0,
        "only a Ready task may own a queue entry"
    );
    assert!(s.queue_invariant_holds());
    s.clear_running();
}

#[test]
fn runq_09_wake_publishes_parked_task_once() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_parked_placeholder();
    assert!(!s.is_queued(id));
    s.wake(id);
    assert!(s.is_queued(id));
    assert_eq!(s.ready_total(), 1);
    assert!(s.queue_invariant_holds());
}

#[test]
fn runq_10_second_wake_adds_nothing() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_parked_placeholder();
    s.wake(id);
    s.wake(id);
    assert_eq!(s.ready_total(), 1);
    assert!(s.queue_invariant_holds());
}

#[test]
fn runq_11_wake_during_poll_defers_instead_of_queueing() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_placeholder();
    assert_eq!(s.claim_ready_for_worker(0), Some(id));
    s.wake(id); // arrives while Running
    assert!(!s.is_queued(id), "a Running task must not be queued");
    assert!(
        s.with_task(id, |task| task.state.load().wake_requested())
            .unwrap()
    );
    assert!(s.queue_invariant_holds());
    s.clear_running();
}

#[test]
fn runq_12_post_poll_publishes_deferred_wake_once() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_placeholder();
    assert_eq!(s.claim_ready_for_worker(0), Some(id));
    s.wake(id);
    s.finish_pending_poll(id);
    assert_eq!(s.task_state(id), Some(RuntimeTaskState::Ready));
    assert_eq!(s.ready_total(), 1);
    assert!(s.queue_invariant_holds());
    s.clear_running();
}

#[test]
fn runq_13_pending_poll_without_wake_parks_unqueued() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_placeholder();
    assert_eq!(s.claim_ready_for_worker(0), Some(id));
    s.finish_pending_poll(id);
    assert_eq!(s.task_state(id), Some(RuntimeTaskState::Parked));
    assert_eq!(s.ready_total(), 0);
    assert!(s.queue_invariant_holds());
    s.clear_running();
}

#[test]
fn runq_14_blocked_syscall_poll_parks_unqueued() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_placeholder();
    assert_eq!(s.claim_ready_for_worker(0), Some(id));
    s.finish_blocked_syscall_poll(id);
    assert_eq!(s.task_state(id), Some(RuntimeTaskState::BlockedSyscall));
    assert!(!s.is_queued(id));
    assert!(s.queue_invariant_holds());
    s.clear_running();
}

#[test]
fn runq_15_blocked_syscall_wake_publishes_once() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_placeholder();
    assert_eq!(s.claim_ready_for_worker(0), Some(id));
    s.finish_blocked_syscall_poll(id);
    s.wake(id);
    s.wake(id);
    assert_eq!(s.task_state(id), Some(RuntimeTaskState::Ready));
    assert_eq!(s.ready_total(), 1);
    assert!(s.queue_invariant_holds());
    s.clear_running();
}

#[test]
fn runq_16_yield_requeues_exactly_once() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_placeholder();
    assert_eq!(s.claim_ready_for_worker(0), Some(id));
    set_current_task(Some(id));
    s.request_running_yield();
    s.requeue_runnable(id);
    set_current_task(None);
    assert_eq!(s.task_state(id), Some(RuntimeTaskState::Ready));
    assert_eq!(s.ready_total(), 1);
    assert!(s.queue_invariant_holds());
    s.clear_running();
}

#[test]
fn runq_17_preemption_requeue_is_idempotent() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_placeholder();
    assert_eq!(s.claim_ready_for_worker(0), Some(id));
    s.requeue_runnable(id);
    s.requeue_runnable(id);
    assert_eq!(s.ready_total(), 1);
    assert!(s.queue_invariant_holds());
    s.clear_running();
}

#[test]
fn runq_18_due_timer_wake_publishes_once() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_placeholder();
    assert_eq!(s.claim_ready_for_worker(0), Some(id));
    set_current_task(Some(id));
    s.set_running_wake_after_millis(0);
    set_current_task(None);
    s.finish_pending_poll(id);
    assert_eq!(
        s.wake_due_timers(Instant::now() + Duration::from_millis(5)),
        1
    );
    assert_eq!(s.ready_total(), 1);
    assert!(s.queue_invariant_holds());
    s.clear_running();
}

#[test]
fn runq_19_cancelled_task_leaves_a_discardable_entry() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_placeholder();
    s.finalize_cancelled(id);
    assert_eq!(s.task_state(id), None, "terminal metadata is reaped");
    // The entry is still in the queue but no longer claimable; popping it
    // clears the flag, so nothing leaks into the next enqueue decision.
    assert_eq!(s.claim_ready_for_worker(0), None);
    assert!(!s.is_queued(id));
    assert!(s.queue_invariant_holds());
}

#[test]
fn runq_20_completed_task_entry_is_discarded() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_placeholder();
    s.complete(id);
    assert_eq!(s.claim_ready_for_worker(0), None);
    assert!(!s.is_queued(id));
    assert!(s.queue_invariant_holds());
}

#[test]
fn runq_21_is_queued_agrees_with_a_full_scan() {
    let mut s = RuntimeScheduler::with_worker_count(3);
    let a = s.spawn_placeholder();
    let b = s.spawn_parked_placeholder();
    let c = s.spawn_placeholder();
    assert_eq!(s.claim_ready_for_worker(0), Some(a));
    s.clear_running();
    for id in [a, b, c] {
        let scanned = s.run_queues.contains(id);
        assert_eq!(s.is_queued(id), scanned, "flag disagrees for task {id}");
    }
    assert!(s.queue_invariant_holds());
}

#[test]
fn runq_22_mixed_transition_sequence_keeps_queues_consistent() {
    let mut s = RuntimeScheduler::with_worker_count(4);
    let ids: Vec<_> = (0..24).map(|_| s.spawn_placeholder()).collect();
    for (step, &id) in ids.iter().enumerate() {
        match step % 6 {
            0 => {
                s.claim_ready_for_worker(step % 4);
                s.finish_pending_poll(id);
                s.wake(id);
            }
            1 => {
                s.claim_ready_for_worker(step % 4);
                s.requeue_runnable(id);
            }
            2 => {
                s.claim_ready_for_worker(step % 4);
                s.wake(id); // during poll
                s.finish_blocked_syscall_poll(id);
            }
            3 => {
                s.enqueue_local(step % 4, id);
                s.enqueue_ready(id);
            }
            4 => {
                s.claim_ready_for_worker(step % 4);
                s.complete(id);
            }
            _ => {
                s.claim_ready_for_worker(step % 4);
                s.finalize_cancelled(id);
            }
        }
        assert!(
            s.queue_invariant_holds(),
            "invariant broken after step {step}"
        );
    }
    while let Some(id) = s.claim_ready_for_worker(0) {
        s.complete(id);
        assert!(s.queue_invariant_holds());
    }
    s.clear_running();
}

#[test]
fn runq_23_cancel_publishes_parked_task_once() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    let id = with_global_for_test(|sched| sched.spawn_parked_placeholder());
    willow_sched_cancel(id);
    willow_sched_cancel(id);
    with_global_for_test(|sched| {
        assert_eq!(sched.ready_len(), 1, "one entry for a cancelled wake-up");
        assert!(sched.queue_invariant_holds());
    });
    reset_global_scheduler_for_test();
    reset_internal_for_test();
}

#[test]
fn runq_24_duplicate_entry_cannot_be_claimed_twice() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_placeholder();
    s.run_queues.force_push_global(id); // forced duplicate
    assert_eq!(s.claim_ready_for_worker(0), Some(id));
    assert_eq!(
        s.claim_ready_for_worker(1),
        None,
        "the second worker must not poll the same frame"
    );
    s.clear_running();
}

// ── O(1) blocked-syscall accounting (willow-ezs.1.2) ─────────────────────
//
// `has_blocked_syscall_tasks()` used to scan every task on every idle/stop
// decision, so a 10k-task workload paid O(tasks) per park cycle. It now
// reads a counter that `with_task_state` maintains, and these tests pin the
// counter to the table it summarizes. Perspectives 16-28 of willow-ezs.1.2
// (1-15 cover the channel waiter queue, in `channel.rs`):
//
// 16. a fresh scheduler reports no blocked-syscall task
// 17. a Pending poll that detached native work marks exactly one
// 18. a completion wake clears it
// 19. cancelling a blocked task clears it (the Parked/Blocked → Ready arm)
// 20. completing a blocked task clears it
// 21. finalizing a blocked task as Cancelled clears it
// 22. re-entering BlockedSyscall from BlockedSyscall does not double count
// 23. N blocked tasks count N, and waking them all returns to 0
// 24. `park()` on a blocked task decrements
// 25. `requeue_runnable` on a blocked task decrements
// 26. `set_running` on a blocked task decrements
// 27. a long mixed transition sequence keeps counter == scan
// 28. presence still suppresses the idle/stop decision (behavior preserved)
// 29. block/wake accounting and queue publication need no global mutex

/// Drive `id` to `BlockedSyscall` exactly as a worker does: claim it, poll
/// it, and report that the poll detached native blocking work.
fn block_on_syscall(s: &mut RuntimeScheduler, id: RuntimeTaskId) {
    s.set_running(id);
    s.finish_blocked_syscall_poll(id);
    s.clear_running();
}

#[test]
fn bsq_16_fresh_scheduler_has_no_blocked_syscall_tasks() {
    let s = RuntimeScheduler::with_worker_count(2);
    assert!(!s.has_blocked_syscall_tasks());
    assert!(s.blocked_syscall_invariant_holds());
}

#[test]
fn bsq_17_detached_poll_marks_exactly_one() {
    let mut s = RuntimeScheduler::with_worker_count(2);
    let id = s.spawn_placeholder();
    let other = s.spawn_placeholder();
    block_on_syscall(&mut s, id);
    assert!(s.has_blocked_syscall_tasks());
    assert_eq!(s.blocked_syscall_count(), 1);
    assert!(s.blocked_syscall_invariant_holds());
    assert_eq!(s.task_state(other), Some(RuntimeTaskState::Ready));
}

#[test]
fn bsq_18_completion_wake_clears_the_count() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    block_on_syscall(&mut s, id);
    s.wake(id);
    assert_eq!(s.task_state(id), Some(RuntimeTaskState::Ready));
    assert!(!s.has_blocked_syscall_tasks());
    assert!(s.blocked_syscall_invariant_holds());
}

#[test]
fn bsq_19_cancel_of_a_blocked_task_clears_the_count() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let id = with_global_for_test(|s| {
        let id = s.spawn_placeholder();
        block_on_syscall(s, id);
        assert!(s.has_blocked_syscall_tasks());
        id
    });
    willow_sched_cancel(id);
    with_global_for_test(|s| {
        assert_eq!(s.task_state(id), Some(RuntimeTaskState::Ready));
        assert!(!s.has_blocked_syscall_tasks());
        assert!(s.blocked_syscall_invariant_holds());
    });
}

#[test]
fn bsq_20_completing_a_blocked_task_clears_the_count() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    block_on_syscall(&mut s, id);
    s.complete(id);
    assert_eq!(s.task_state(id), None);
    assert!(!s.has_blocked_syscall_tasks());
    assert!(s.blocked_syscall_invariant_holds());
}

#[test]
fn bsq_21_finalizing_a_blocked_task_as_cancelled_clears_the_count() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    block_on_syscall(&mut s, id);
    s.finalize_cancelled(id);
    assert_eq!(s.task_state(id), None);
    assert!(!s.has_blocked_syscall_tasks());
    assert!(s.blocked_syscall_invariant_holds());
}

#[test]
fn bsq_22_reentering_blocked_syscall_does_not_double_count() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    block_on_syscall(&mut s, id);
    // A second report without an intervening wake: the state is unchanged,
    // so the counter must not move.
    s.finish_blocked_syscall_poll(id);
    assert_eq!(s.blocked_syscall_count(), 1);
    assert!(s.blocked_syscall_invariant_holds());
}

#[test]
fn bsq_23_many_blocked_tasks_count_and_drain_to_zero() {
    let mut s = RuntimeScheduler::with_worker_count(4);
    const TASKS: usize = 2_000;
    let ids: Vec<_> = (0..TASKS).map(|_| s.spawn_placeholder()).collect();
    for &id in &ids {
        block_on_syscall(&mut s, id);
    }
    assert_eq!(s.blocked_syscall_count(), TASKS);
    assert!(s.blocked_syscall_invariant_holds());
    for &id in &ids {
        s.wake(id);
    }
    assert_eq!(s.blocked_syscall_count(), 0);
    assert!(!s.has_blocked_syscall_tasks());
    assert!(s.blocked_syscall_invariant_holds());
    assert!(s.queue_invariant_holds());
}

#[test]
fn bsq_24_parking_a_blocked_task_decrements() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    block_on_syscall(&mut s, id);
    s.wake(id);
    assert_eq!(s.claim_ready_for_worker(0), Some(id));
    s.park(id);
    assert_eq!(s.task_state(id), Some(RuntimeTaskState::Parked));
    assert_eq!(s.blocked_syscall_count(), 0);
    assert!(s.blocked_syscall_invariant_holds());
}

#[test]
fn bsq_25_requeue_runnable_from_blocked_decrements() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    block_on_syscall(&mut s, id);
    s.wake(id);
    assert_eq!(s.claim_ready_for_worker(0), Some(id));
    s.requeue_runnable(id);
    assert_eq!(s.task_state(id), Some(RuntimeTaskState::Ready));
    assert_eq!(s.blocked_syscall_count(), 0);
    assert!(s.blocked_syscall_invariant_holds());
    assert!(s.queue_invariant_holds());
}

#[test]
fn bsq_26_set_running_from_blocked_decrements() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    block_on_syscall(&mut s, id);
    s.set_running(id);
    assert_eq!(s.blocked_syscall_count(), 0);
    assert!(s.blocked_syscall_invariant_holds());
    s.clear_running();
}

#[test]
fn bsq_27_mixed_transition_sequence_keeps_counter_exact() {
    let mut s = RuntimeScheduler::with_worker_count(3);
    let ids: Vec<_> = (0..64).map(|_| s.spawn_placeholder()).collect();
    for (index, &id) in ids.iter().enumerate() {
        match index % 8 {
            0 => block_on_syscall(&mut s, id),
            1 => {
                block_on_syscall(&mut s, id);
                s.wake(id);
            }
            2 => {
                block_on_syscall(&mut s, id);
                s.complete(id);
            }
            3 => {
                block_on_syscall(&mut s, id);
                s.finalize_cancelled(id);
            }
            4 => s.park(id),
            5 => {
                s.set_running(id);
                s.finish_pending_poll(id);
                s.clear_running();
            }
            6 => {
                block_on_syscall(&mut s, id);
                s.wake(id);
                block_on_syscall(&mut s, id);
            }
            _ => s.complete(id),
        }
        assert!(
            s.blocked_syscall_invariant_holds(),
            "counter drifted after transition {index}"
        );
        assert!(s.queue_invariant_holds(), "queues drifted at {index}");
    }
}

#[test]
fn bsq_28_blocked_presence_suppresses_the_idle_decision() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    // Parked at spawn, so no queue entry exists to confuse `ready_total`.
    let id = s.spawn_parked_placeholder();
    block_on_syscall(&mut s, id);
    // No runnable work, but the blocking pool still owes a wake: the
    // scheduler must not treat empty queues as "done".
    assert_eq!(s.ready_total(), 0);
    assert!(s.has_blocked_syscall_tasks());
    s.wake(id);
    assert_eq!(s.ready_total(), 1);
    assert!(!s.has_blocked_syscall_tasks());
}

#[test]
fn bsq_29_global_block_and_wake_do_not_need_scheduler_metadata_lock() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let id = with_global_for_test(RuntimeScheduler::spawn_placeholder);
    assert_eq!(
        claim_global_ready_for_worker(0, None).map(|(id, _)| id),
        Some(id)
    );

    // Hold the old global accounting lock across both operations. The
    // sharded implementation must still be able to enter BlockedSyscall,
    // publish a wake queue entry, and leave the blocked count.
    let scheduler_lock = GLOBAL_SCHEDULER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (blocked_tx, blocked_rx) = std::sync::mpsc::channel();
    let blocker = std::thread::spawn(move || {
        finish_global_poll_boundary(id, GlobalPollBoundary::BlockedSyscall);
        blocked_tx.send(()).unwrap();
    });
    blocked_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("BlockedSyscall publication must not take GLOBAL_SCHEDULER");
    blocker.join().unwrap();
    assert_eq!(global_task_table().blocked_syscall_count(), 1);
    assert_eq!(global_run_queues().len(), 0);

    let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    let waker = std::thread::spawn(move || {
        wake_tx.send(wake_global_task(id)).unwrap();
    });
    assert!(
        wake_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("blocked wake must not take GLOBAL_SCHEDULER")
    );
    waker.join().unwrap();
    assert_eq!(
        global_task_table().blocked_syscall_count(),
        0,
        "the count is removed only after the queue handoff is published"
    );
    assert_eq!(global_run_queues().len(), 1);

    drop(scheduler_lock);
    reset_global_scheduler_for_test();
}

#[test]
fn sched_run_registers_driver_as_mutator_without_leaking() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    crate::gc::reset_internal_for_test();
    let before = crate::gc::registered_mutator_count();
    // Driving an empty scheduler registers the driver for the duration and
    // unregisters on the outermost exit (willow-6fv.5.6): no net leak.
    assert_eq!(willow_sched_run(), 0);
    assert_eq!(
        crate::gc::registered_mutator_count(),
        before,
        "willow_sched_run must not leak a mutator registration"
    );
}

static PARALLEL_POLL_THREADS: TestLazyLock<TestMutex<Vec<std::thread::ThreadId>>> =
    TestLazyLock::new(|| TestMutex::new(Vec::new()));
static PARALLEL_POLL_ENTERED: TestAtomicUsize = TestAtomicUsize::new(0);

unsafe extern "C" fn poll_record_parallel_worker(_frame: *mut c_void) -> i32 {
    PARALLEL_POLL_THREADS
        .lock()
        .expect("parallel poll thread log poisoned")
        .push(std::thread::current().id());
    PARALLEL_POLL_ENTERED.fetch_add(1, TestOrdering::SeqCst);
    let start = Instant::now();
    while PARALLEL_POLL_ENTERED.load(TestOrdering::SeqCst) < 2
        && start.elapsed() < Duration::from_millis(200)
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    RUNTIME_POLL_READY
}

#[test]
fn parallel_worker_pool_polls_tasks_on_multiple_threads() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    replace_global_scheduler_for_test(2);
    PARALLEL_POLL_THREADS
        .lock()
        .expect("parallel poll thread log poisoned")
        .clear();
    PARALLEL_POLL_ENTERED.store(0, TestOrdering::SeqCst);

    let a = willow_sched_spawn(poll_record_parallel_worker, std::ptr::null_mut());
    let b = willow_sched_spawn(poll_record_parallel_worker, std::ptr::null_mut());

    crate::gc::willow_gc_register_mutator();
    let completed = willow_sched_run_parallel(None, 2, None);
    crate::gc::willow_gc_unregister_mutator();

    assert_eq!(completed, 2);
    assert_eq!(willow_sched_task_state(a), -1);
    assert_eq!(willow_sched_task_state(b), -1);
    let threads = PARALLEL_POLL_THREADS
        .lock()
        .expect("parallel poll thread log poisoned");
    let unique = threads.iter().copied().collect::<HashSet<_>>();
    assert!(
        unique.len() >= 2,
        "expected two worker threads to poll tasks, got {threads:?}"
    );
    reset_internal_for_test();
}

#[test]
fn parallel_completion_retains_early_notifications() {
    let _guard = runtime_test_guard();
    for workers in [1, 2, 8, 32, 128] {
        let completion = ParallelCompletion::new(workers);
        for completed in 1..=workers {
            completion.finish();
            assert_eq!(*completion.remaining.lock().unwrap(), workers - completed);
        }
        completion.wait();
    }
}

#[test]
fn parallel_completion_wakes_promptly() {
    let _guard = runtime_test_guard();
    let mut latencies = Vec::new();
    for _ in 0..16 {
        let completion = Arc::new(ParallelCompletion::new(1));
        let worker_completion = Arc::clone(&completion);
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            let sent = Instant::now();
            worker_completion.finish();
            sent
        });
        completion.wait();
        let returned = Instant::now();
        latencies.push(returned.duration_since(worker.join().unwrap()));
    }
    latencies.sort_unstable();
    let median = latencies[latencies.len() / 2];
    eprintln!("parallel completion median wake latency: {median:?}");
    // A median tolerates occasional host scheduling delays; this is not a
    // hard real-time guarantee on a general-purpose operating system.
    assert!(
        median < Duration::from_millis(1),
        "wake latency: {median:?}"
    );
}

unsafe extern "C" fn poll_collect_and_nested_drive(_frame: *mut c_void) -> i32 {
    crate::gc::willow_gc_minor_collect();
    crate::gc::willow_gc_collect();
    let child = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    willow_sched_run_until(child);
    assert_eq!(willow_sched_task_state(child), -1);
    RUNTIME_POLL_READY
}

#[test]
fn parallel_completion_allows_worker_gc_and_nested_drive() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    crate::gc::reset_internal_for_test();
    replace_global_scheduler_for_test(2);
    willow_sched_spawn(poll_collect_and_nested_drive, std::ptr::null_mut());
    crate::gc::willow_gc_register_mutator();
    assert_eq!(willow_sched_run_parallel(None, 2, None), 2);
    crate::gc::willow_gc_unregister_mutator();
    reset_internal_for_test();
}

#[cfg(target_os = "linux")]
unsafe extern "C" fn poll_one_second_of_work(_frame: *mut c_void) -> i32 {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(1) {
        std::hint::spin_loop();
    }
    RUNTIME_POLL_READY
}

#[cfg(target_os = "linux")]
#[test]
fn parallel_completion_driver_cpu_below_five_percent() {
    fn thread_cpu_seconds() -> f64 {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        assert_eq!(
            unsafe { libc::getrusage(libc::RUSAGE_THREAD, usage.as_mut_ptr()) },
            0
        );
        let usage = unsafe { usage.assume_init() };
        (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as f64
            + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1_000_000.0
    }
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    replace_global_scheduler_for_test(2);
    willow_sched_spawn(poll_one_second_of_work, std::ptr::null_mut());
    crate::gc::willow_gc_register_mutator();
    let cpu_start = thread_cpu_seconds();
    let start = Instant::now();
    let completed = willow_sched_run_parallel(None, 2, None);
    let wall = start.elapsed().as_secs_f64();
    let cpu = thread_cpu_seconds() - cpu_start;
    crate::gc::willow_gc_unregister_mutator();
    assert_eq!(completed, 1);
    eprintln!("parallel driver: cpu={cpu:.6}s wall={wall:.6}s");
    assert!(
        cpu < wall * 0.05,
        "driver CPU {cpu}s exceeds 5% of wall {wall}s"
    );
    reset_internal_for_test();
}

static WAKE_RACE_WAITER_REGISTERED: TestAtomicUsize = TestAtomicUsize::new(0);

unsafe extern "C" fn poll_complete_after_waiter_registered(_frame: *mut c_void) -> i32 {
    let start = Instant::now();
    while WAKE_RACE_WAITER_REGISTERED.load(TestOrdering::SeqCst) == 0
        && start.elapsed() < Duration::from_millis(200)
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    RUNTIME_POLL_READY
}

unsafe extern "C" fn poll_await_with_running_wake_race(frame: *mut c_void) -> i32 {
    let base = frame as *mut u8;
    let b_id = unsafe { *(base.add(async_frame_slot_offset(0)) as *const u64) };
    let state = unsafe { &mut *(base.add(async_frame_slot_offset(1)) as *mut i64) };
    *state += 1;
    if *state == 1 {
        assert_eq!(willow_sched_await(b_id), 0);
        WAKE_RACE_WAITER_REGISTERED.store(1, TestOrdering::SeqCst);
        std::thread::sleep(Duration::from_millis(30));
        RUNTIME_POLL_PENDING
    } else {
        RUNTIME_POLL_READY
    }
}

#[test]
fn parallel_wake_while_waiter_running_requeues_after_pending() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    replace_global_scheduler_for_test(2);
    WAKE_RACE_WAITER_REGISTERED.store(0, TestOrdering::SeqCst);

    let b = willow_sched_spawn(poll_complete_after_waiter_registered, std::ptr::null_mut());
    let a_frame = willow_async_frame_alloc(2, 0) as *mut c_void;
    unsafe {
        let base = a_frame as *mut u8;
        *(base.add(async_frame_slot_offset(0)) as *mut u64) = b;
    }
    let a = willow_sched_spawn(poll_await_with_running_wake_race, a_frame);

    crate::gc::willow_gc_register_mutator();
    let completed = willow_sched_run_parallel(None, 2, None);
    crate::gc::willow_gc_unregister_mutator();

    assert_eq!(
        completed, 2,
        "awaiter must be requeued when its dependency wakes it before park"
    );
    assert_eq!(willow_sched_task_state(a), -1);
    assert_eq!(willow_sched_task_state(b), -1);
    reset_internal_for_test();
}

#[test]
fn workqueue_single_worker_preserves_fifo() {
    // With one worker, spawn order == pop order (no behavior change vs. the
    // old single VecDeque).
    let mut s = RuntimeScheduler::with_worker_count(1);
    let a = s.spawn_task(poll_ready_now, std::ptr::null_mut());
    let b = s.spawn_task(poll_ready_now, std::ptr::null_mut());
    assert_eq!(s.pop_for_worker(0), Some(a));
    assert_eq!(s.pop_for_worker(0), Some(b));
}

unsafe extern "C" fn cancel_noop(_frame: *mut c_void) {}

#[test]
fn initialized_spawn_runs_initializer_before_task_or_queue_publication() {
    let mut scheduler = RuntimeScheduler::with_worker_count(1);
    let tasks = Arc::clone(&scheduler.tasks);
    let run_queues = Arc::clone(&scheduler.run_queues);
    let mut published_id = 0;

    let id = scheduler.spawn_task_initialized(
        poll_ready_now,
        std::ptr::null_mut(),
        Some(cancel_noop),
        |id| {
            assert!(
                tasks.with(id, |_| ()).is_none(),
                "initializer must run before task-table publication"
            );
            assert!(
                !run_queues.snapshot().contains(&id),
                "initializer must run before run-queue publication"
            );
            published_id = id;
        },
    );

    assert_eq!(published_id, id);
    assert_eq!(run_queues.snapshot(), vec![id]);
    assert!(
        tasks
            .with(id, |task| task.cancel.map(|cancel| cancel as usize))
            .flatten()
            .is_some_and(|cancel| cancel == cancel_noop as *const () as usize)
    );
}

#[test]
fn global_initialized_spawn_allows_scheduler_reentry_before_publication() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    let mut nested = 0;
    let mut parent_was_hidden = false;

    let parent = spawn_global_task_initialized(
        poll_ready_now,
        std::ptr::null_mut(),
        Some(cancel_noop),
        |parent| {
            parent_was_hidden = global_task_table().with(parent, |_| ()).is_none()
                && !global_run_queues().snapshot().contains(&parent);
            nested = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
            assert_ne!(nested, parent);
            assert_eq!(willow_sched_heavy_task_count(), 1);
        },
    );

    assert!(parent_was_hidden);
    assert!(global_task_table().with(parent, |_| ()).is_some());
    assert!(global_task_table().with(nested, |_| ()).is_some());
    assert_eq!(willow_sched_heavy_task_count(), 2);
    reset_global_scheduler_for_test();
    reset_internal_for_test();
}

#[test]
fn global_initialized_spawn_panic_rolls_back_unpublished_frame_root() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
    let roots_before = crate::gc::runtime_root_count();

    let panic = std::panic::catch_unwind(|| {
        spawn_global_task_initialized(poll_ready_now, frame, Some(cancel_noop), |_id| {
            panic!("initializer panic");
        });
    });

    assert!(panic.is_err());
    assert_eq!(crate::gc::runtime_root_count(), roots_before);
    assert_eq!(willow_sched_heavy_task_count(), 0);
    reset_global_scheduler_for_test();
    reset_internal_for_test();
}

// ── Cooperative executable tasks (willow-fqg.1) ─────────────────────────

#[test]
fn async_chain_text_walks_awaiter_links() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    // main(id=1) awaits inner(id=2): register main as a waiter of inner, then
    // mark inner the running task. The chain is inner -> main (willow-9lw).
    let (inner, main) = with_global(|sched| {
        let inner = sched.spawn_task(poll_ready_now, std::ptr::null_mut());
        let main = sched.spawn_task(poll_ready_now, std::ptr::null_mut());
        sched.register_waiter(inner, main);
        sched.with_task_mut(inner, |task| task.set_name("inner".to_string()));
        sched.with_task_mut(main, |task| task.set_name("main".to_string()));
        sched.set_running(inner);
        (inner, main)
    });
    let text = async_chain_text();
    let i = text.find("inner").expect("chain names inner");
    let m = text.find("main").expect("chain names main");
    assert!(
        i < m,
        "current task (inner) must come before its awaiter (main): {text}"
    );
    let _ = (inner, main);
    with_global(|sched| sched.clear_running());
}

#[test]
fn async_chain_inspection_handles_depth_fanout_tombstones_and_cycles() {
    let _guard = runtime_test_guard();
    for depth in [1, 32, 256] {
        for fanout in [0, 32, 256] {
            reset_global_scheduler_for_test();
            with_global(|sched| {
                let chain: Vec<_> = (0..depth)
                    .map(|_| sched.spawn_parked_placeholder())
                    .collect();
                let extras: Vec<_> = (0..fanout)
                    .map(|_| sched.spawn_parked_placeholder())
                    .collect();
                let removed = sched.spawn_parked_placeholder();
                for (index, &id) in chain.iter().enumerate() {
                    sched.register_waiter(id, removed);
                    if depth > 1 {
                        sched.register_waiter(id, chain[(index + 1) % depth]);
                    }
                    for &extra in &extras {
                        sched.register_waiter(id, extra);
                    }
                    sched.unregister_waiter(id, removed);
                    sched.with_task_mut(id, |task| {
                        task.set_name(format!("chain_{index}"));
                        task.set_spawn_site("inspection.wi".to_string(), index as u32 + 1);
                    });
                }
                sched.set_running(chain[0]);
            });
            let text = async_chain_text();
            // At depth one, the first extra is the only following awaiter.
            let expected = depth + usize::from(depth == 1 && fanout > 0);
            assert_eq!(text.lines().count(), expected + 1);
            for (index, line) in text.lines().skip(1).take(depth).enumerate() {
                assert!(line.contains(&format!("async chain_{index} [")));
                assert!(line.ends_with(&format!("inspection.wi:{}]", index + 1)));
            }
            with_global(|sched| sched.clear_running());
        }
    }
    reset_global_scheduler_for_test();
}

#[test]
fn cooperative_poll_runs_without_an_active_native_stack() {
    unsafe extern "C" fn poll(_: *mut c_void) -> i32 {
        assert_eq!(crate::preempt::willow_sync_native_active(), 0);
        RUNTIME_POLL_READY
    }
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let id = willow_sched_spawn_cooperative(poll, std::ptr::null_mut()) as u64;
    assert!(
        global_task_table()
            .with(id, |task| task.cooperative_poll)
            .unwrap()
    );
    willow_sched_run_until(id);
    assert!(target_is_done(Some(id)));
}

#[test]
fn cooperative_boundary_returns_callback_result_and_releases_stack() {
    unsafe extern "C" fn helper(_: *mut c_void) -> i32 {
        42
    }
    unsafe extern "C" fn poll(_: *mut c_void) -> i32 {
        assert_eq!(willow_task_stack_enter(helper, std::ptr::null_mut()), 42);
        willow_task_stack_leave();
        assert_eq!(crate::preempt::willow_sync_native_active(), 0);
        RUNTIME_POLL_READY
    }
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let id = willow_sched_spawn_cooperative(poll, std::ptr::null_mut()) as u64;
    willow_sched_run_until(id);
    assert!(target_is_done(Some(id)));
}

#[cfg(any(
    all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
))]
#[test]
fn cooperative_boundary_resumes_helper_without_restarting_it() {
    static ENTRIES: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn helper(_: *mut c_void) -> i32 {
        ENTRIES.fetch_add(1, Ordering::SeqCst);
        assert!(crate::native_stack::suspend());
        42
    }
    unsafe extern "C" fn poll(_: *mut c_void) -> i32 {
        let result = willow_task_stack_enter(helper, std::ptr::null_mut());
        // Leave must preserve a suspended callback until the next poll.
        willow_task_stack_leave();
        if result == RUNTIME_POLL_PREEMPTED {
            return result;
        }
        assert_eq!(result, 42);
        assert_eq!(ENTRIES.load(Ordering::SeqCst), 1);
        RUNTIME_POLL_READY
    }
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    ENTRIES.store(0, Ordering::SeqCst);
    let id = willow_sched_spawn_cooperative(poll, std::ptr::null_mut()) as u64;
    willow_sched_run_until(id);
    assert!(target_is_done(Some(id)));
}

#[test]
fn cooperative_cancel_bounded_cleanup_avoids_native_stack() {
    static CALLED: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn cleanup(_: *mut c_void) {
        assert_eq!(crate::preempt::willow_sync_native_active(), 0);
        CALLED.fetch_add(1, Ordering::SeqCst);
    }
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    CALLED.store(0, Ordering::SeqCst);
    let frame = willow_async_frame_alloc(0, 0).cast();
    let id = willow_sched_spawn_cooperative(poll_ready_now, frame) as u64;
    willow_sched_set_cancel_fn_cooperative(id, cleanup, 0);
    willow_sched_cancel(id);
    willow_sched_run_until(id);
    assert_eq!(CALLED.load(Ordering::SeqCst), 1);
}

#[cfg(any(
    all(
        target_os = "linux",
        target_env = "gnu",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "windows", target_env = "msvc", target_arch = "x86_64")
))]
#[test]
fn cooperative_cancel_unbounded_cleanup_retains_native_stack() {
    static CALLED: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn cleanup(_: *mut c_void) {
        assert_eq!(crate::preempt::willow_sync_native_active(), 1);
        CALLED.fetch_add(1, Ordering::SeqCst);
    }
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    CALLED.store(0, Ordering::SeqCst);
    let frame = willow_async_frame_alloc(0, 0).cast();
    let id = willow_sched_spawn_cooperative(poll_ready_now, frame) as u64;
    willow_sched_set_cancel_fn_cooperative(id, cleanup, 1);
    willow_sched_cancel(id);
    willow_sched_run_until(id);
    assert_eq!(CALLED.load(Ordering::SeqCst), 1);
}

#[test]
fn cooperative_spawn_publishes_call_site_before_first_poll() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let previous = crate::stack_trace::replace_current(Default::default());
    crate::stack_trace::willow_callstack_push(b"child".as_ptr(), 5, b"spawn.wi".as_ptr(), 8, 42, 3);
    let frame = willow_async_frame_alloc(0, 0).cast();
    let id = willow_sched_spawn_cooperative(poll_ready_now, frame) as u64;
    crate::stack_trace::replace_current(previous);
    // Inspect the published record before the caller can invoke the
    // post-constructor setter: another worker can already claim it here.
    let site = global_task_table().with(id, |task| {
        task.spawn_site()
            .map(|(file, line)| (file.to_owned(), line))
    });
    willow_sched_run_until(id);
    assert_eq!(site, Some(Some(("spawn.wi".to_owned(), 42))));
}

#[test]
fn cooperative_cancel_without_cleanup_reports_terminal_cancellation_once() {
    use crate::observability::{WillowRuntimeMetricsV1, willow_runtime_metrics_snapshot_v1};
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let mut before = WillowRuntimeMetricsV1::default();
    willow_runtime_metrics_snapshot_v1(&mut before);
    let frame = willow_async_frame_alloc(0, 0).cast();
    let id = willow_sched_spawn_cooperative(poll_ready_now, frame) as u64;
    willow_sched_cancel(id);
    willow_sched_run_until(id);
    willow_sched_run();
    let mut after = WillowRuntimeMetricsV1::default();
    willow_runtime_metrics_snapshot_v1(&mut after);
    assert_eq!(after.task_cancellations - before.task_cancellations, 1);
    assert_eq!(after.task_polls - before.task_polls, 0);
}

/// Completes on the first poll.
unsafe extern "C" fn poll_ready_now(_frame: *mut c_void) -> i32 {
    RUNTIME_POLL_READY
}

/// Uses the frame's state word (offset 0) as a counter: Pending on the first
/// poll, Ready on the second.
unsafe extern "C" fn poll_ready_on_second(frame: *mut c_void) -> i32 {
    let state = unsafe { &mut *(frame as *mut i64) };
    *state += 1;
    if *state >= 2 {
        RUNTIME_POLL_READY
    } else {
        RUNTIME_POLL_PENDING
    }
}

#[test]
fn coop_01_ready_task_runs_to_completion() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    let id = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    assert_eq!(willow_sched_run(), 1);
    assert_eq!(willow_sched_task_state(id), -1); // terminal record reaped
}

#[test]
fn coop_02_pending_parks_then_wake_resumes() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    // A frame with just the [state, slot_count] header; poll uses the state word.
    let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
    let id = willow_sched_spawn(poll_ready_on_second, frame);
    // First poll: state 0->1 -> Pending -> parked.
    assert_eq!(willow_sched_run(), 0);
    assert_eq!(willow_sched_task_state(id), 2); // Parked
    // A parked task is not re-run while idle.
    assert_eq!(willow_sched_run(), 0);
    assert_eq!(willow_sched_task_state(id), 2);
    // Wake re-queues it; the second poll completes it.
    willow_sched_wake(id);
    assert_eq!(willow_sched_task_state(id), 0); // Ready
    assert_eq!(willow_sched_run(), 1);
    assert_eq!(willow_sched_task_state(id), -1); // terminal record reaped
    reset_internal_for_test();
}

/// First poll registers a 5ms sleep then returns Pending; second poll
/// (after the timer fires) returns Ready.
unsafe extern "C" fn poll_sleep_then_ready(frame: *mut c_void) -> i32 {
    let state = unsafe { &mut *(frame as *mut i64) };
    *state += 1;
    if *state >= 2 {
        RUNTIME_POLL_READY
    } else {
        willow_sched_sleep(5);
        RUNTIME_POLL_PENDING
    }
}

/// First poll requests a cooperative yield then returns Pending; second poll
/// returns Ready.
unsafe extern "C" fn poll_yield_then_ready(frame: *mut c_void) -> i32 {
    let state = unsafe { &mut *(frame as *mut i64) };
    *state += 1;
    if *state >= 2 {
        RUNTIME_POLL_READY
    } else {
        willow_sched_yield();
        RUNTIME_POLL_PENDING
    }
}

#[test]
fn coop_timer_wake_resumes_parked_task() {
    // willow-lpn.5.3: a task that parks with a wake-deadline (sleep) is woken
    // by the timer-aware run loop and resumes to completion — no manual wake.
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
    let id = willow_sched_spawn(poll_sleep_then_ready, frame);
    let start = std::time::Instant::now();
    // Single run: first poll -> sleep+Pending -> parked with deadline; the
    // loop blocks ~5ms, wakes it, second poll -> Ready -> completed.
    let completed = willow_sched_run();
    assert_eq!(completed, 1, "timer should resume and complete the task");
    assert!(
        start.elapsed() >= std::time::Duration::from_millis(4),
        "run loop should have waited for the wake-deadline"
    );
    assert_eq!(willow_sched_task_state(id), -1); // terminal record reaped
    reset_internal_for_test();
}

#[test]
fn coop_yield_requeues_running_task_without_manual_wake() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
    let id = willow_sched_spawn(poll_yield_then_ready, frame);
    assert_eq!(willow_sched_run(), 1);
    assert_eq!(willow_sched_task_state(id), -1); // terminal record reaped
    reset_internal_for_test();
}

/// Awaits the task whose id is stored in frame slot 0; resumes once it
/// completes (slot 1 is a poll counter).
unsafe extern "C" fn poll_await_dependency(frame: *mut c_void) -> i32 {
    let base = frame as *mut u8;
    let b_id = unsafe { *(base.add(async_frame_slot_offset(0)) as *const u64) };
    let state = unsafe { &mut *(base.add(async_frame_slot_offset(1)) as *mut i64) };
    *state += 1;
    if *state == 1 {
        if willow_sched_await(b_id) == 1 {
            RUNTIME_POLL_READY
        } else {
            RUNTIME_POLL_PENDING // registered as a waiter of b_id
        }
    } else {
        RUNTIME_POLL_READY // resumed after the awaited task completed
    }
}

#[test]
fn coop_dependency_wake_resumes_awaiter() {
    // willow-lpn.5.3: task A awaits task B. B sleeps then completes (timer
    // wake); B's completion wakes A (dependency wake); A resumes. No manual
    // wake — the scheduler drives both to completion in one run.
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    // B: sleeps 5ms on the first poll, ready on the second.
    let b_frame = willow_async_frame_alloc(0, 0) as *mut c_void;
    let b_id = willow_sched_spawn(poll_sleep_then_ready, b_frame);
    // A: awaits B. Store B's id in slot 0; slot 1 is A's poll counter.
    let a_frame = willow_async_frame_alloc(2, 0) as *mut c_void;
    unsafe {
        let base = a_frame as *mut u8;
        *(base.add(async_frame_slot_offset(0)) as *mut u64) = b_id;
    }
    let a_id = willow_sched_spawn(poll_await_dependency, a_frame);
    let completed = willow_sched_run();
    assert_eq!(
        completed, 2,
        "both the awaited task and the awaiter complete"
    );
    assert_eq!(willow_sched_task_state(a_id), -1); // A terminal record reaped
    assert_eq!(willow_sched_task_state(b_id), -1); // B terminal record reaped
    reset_internal_for_test();
}

#[test]
fn coop_clear_running_prevents_stale_sleep() {
    // willow-lpn.5.3: after a poll returns, `running` is cleared, so a
    // willow_sched_sleep called OUTSIDE a poll does not attach a phantom
    // wake-deadline to the just-parked (now stale) task and spuriously wake
    // it on the next run.
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
    let id = willow_sched_spawn(poll_ready_on_second, frame);
    assert_eq!(willow_sched_run(), 0); // parks (no deadline); running cleared
    assert_eq!(willow_sched_task_state(id), 2); // Parked
    // Outside any poll (running == None): must be a no-op.
    willow_sched_sleep(5);
    assert_eq!(
        willow_sched_run(),
        0,
        "stale task must not be woken by an out-of-poll sleep"
    );
    assert_eq!(willow_sched_task_state(id), 2); // still Parked, not woken/completed
    reset_internal_for_test();
}

#[test]
fn coop_parked_without_deadline_stays_idle() {
    // A task parked WITHOUT a deadline is not spuriously woken by the timer
    // loop (regression guard for the willow-lpn.5.3 run-loop change).
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
    let id = willow_sched_spawn(poll_ready_on_second, frame);
    assert_eq!(willow_sched_run(), 0); // parks, no deadline
    assert_eq!(willow_sched_task_state(id), 2); // Parked
    assert_eq!(willow_sched_run(), 0); // stays parked (loop breaks, no timer)
    assert_eq!(willow_sched_task_state(id), 2);
    reset_internal_for_test();
}

#[test]
fn coop_03_suspended_frame_keeps_referenced_object_alive() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();

    // Frame with one GC-reference data slot (mask bit 0).
    let mut frame = willow_async_frame_alloc(1, 0b1) as *mut u8;
    // Protect setup until spawn transfers ownership to the scheduler root.
    crate::gc::willow_push_root(&mut frame as *mut *mut u8);
    // A heap object reachable ONLY through the frame's GC slot.
    let obj = willow_alloc_typed(16, 0);
    let slot0 = unsafe { frame.add(async_frame_slot_offset(0)).cast::<*mut u8>() };
    unsafe { slot0.write(obj) };

    let live = willow_gc_allocated_bytes(); // frame + obj
    // Unreferenced garbage that must be collected.
    let _garbage = willow_alloc_typed(16, 0);
    assert!(willow_gc_allocated_bytes() > live);

    // Spawning roots the frame; the first poll parks the task (Pending). The
    // poll counter uses the state word, leaving the data slot untouched.
    let id = willow_sched_spawn(poll_ready_on_second, frame as *mut c_void);
    crate::gc::willow_pop_root();
    assert_eq!(willow_sched_run(), 0);
    assert_eq!(willow_sched_task_state(id), 2); // Parked

    // Collection while suspended: the frame (a runtime root) and the object it
    // references survive; the unrooted garbage is freed.
    willow_gc_collect();
    assert_eq!(
        willow_gc_allocated_bytes(),
        live,
        "a suspended task's frame must keep its referenced object alive across GC"
    );

    // Resume to completion, which unroots the frame.
    willow_sched_wake(id);
    assert_eq!(willow_sched_run(), 1);
    assert_eq!(willow_sched_task_state(id), -1); // terminal record reaped

    // Nothing roots the frame/object now; both are collected.
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn coop_04_unknown_task_state_is_minus_one() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    assert_eq!(willow_sched_task_state(999), -1);
}

#[test]
fn coop_05_multiple_ready_tasks_all_complete() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    let a = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    let b = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    assert_eq!(willow_sched_run(), 2);
    assert_eq!(willow_sched_task_state(a), -1);
    assert_eq!(willow_sched_task_state(b), -1);
}

#[test]
fn scheduler_assigns_ready_task_ids() {
    let mut scheduler = RuntimeScheduler::default();
    let first = scheduler.spawn_placeholder();
    let second = scheduler.spawn_placeholder();
    assert_eq!(scheduler.pop_ready(), Some(first));
    assert_eq!(scheduler.pop_ready(), Some(second));
}

#[test]
fn scheduler_reports_task_and_ready_counts() {
    let mut scheduler = RuntimeScheduler::default();
    assert_eq!(scheduler.task_count(), 0);
    assert_eq!(scheduler.ready_len(), 0);
    scheduler.spawn_placeholder();
    scheduler.spawn_placeholder();
    assert_eq!(scheduler.task_count(), 2);
    assert_eq!(scheduler.ready_len(), 2);
}

#[test]
fn scheduler_worker_config_uses_supplied_default() {
    for default in [0, 1, 2, 5, 8, 64] {
        let config = RuntimeWorkerConfig::from_env_value(None, default);
        assert_eq!(config.requested_workers(), default.max(1));
        assert_eq!(config.active_workers(), default.max(1));
    }
}

#[test]
fn scheduler_worker_config_sizes_queues_to_requested_count() {
    for workers in [1, 2, 8, 64] {
        let config = RuntimeWorkerConfig::from_env_value(Some(&workers.to_string()), 5);
        let scheduler = RuntimeScheduler::with_worker_count(config.active_workers());
        assert_eq!(scheduler.worker_count(), workers);
        assert_eq!(scheduler.run_queues.locals.len(), workers);
        assert_eq!(scheduler.run_queues.prefer_global.len(), workers);
        assert_eq!(scheduler.run_queues.worker_metrics.len(), workers);
        assert_eq!(scheduler.task_count(), 0);
    }
}

#[test]
fn scheduler_worker_config_parses_env_override() {
    let config = RuntimeWorkerConfig::from_env_value(Some("8"), 4);
    assert_eq!(config.requested_workers(), 8);
    assert_eq!(config.active_workers(), 8);
}

#[test]
fn scheduler_worker_config_honors_small_overrides() {
    for value in ["1", "2", "4"] {
        let config = RuntimeWorkerConfig::from_env_value(Some(value), TEST_WORKERS);
        let expected = value.parse::<usize>().unwrap();
        assert_eq!(config.requested_workers(), expected);
        assert_eq!(config.active_workers(), expected);
    }
}

#[test]
fn scheduler_worker_config_rejects_zero_and_invalid_override() {
    let zero = RuntimeWorkerConfig::from_env_value(Some("0"), TEST_WORKERS);
    assert_eq!(zero.requested_workers(), 5);
    assert_eq!(zero.active_workers(), 5);

    let invalid = RuntimeWorkerConfig::from_env_value(Some("many"), TEST_WORKERS);
    assert_eq!(invalid.requested_workers(), 5);
    assert_eq!(invalid.active_workers(), 5);
}

// -----------------------------------------------------------------------
// `single_worker_for_test()` — deterministic single-threaded drives
// (willow-tcrg). `bounded_unit_10_cancelled_handoff_wakes_the_next_producer`
// failed intermittently because the worker pool claimed and completed a
// placeholder that the drive itself had just woken, so `run_until` returned
// a completion the test never asked for. Perspectives:
//
//  1. the guard reports one active worker
//  2. the guard reports one requested worker
//  3. dropping the guard restores the previous config
//  4. the unguarded config follows the environment and host default
//  5. nested guards keep the single-worker view until the outermost drops
//  6. sequential guards re-arm cleanly
//  7. the guard does not touch `from_env_value` parsing
//  8. the ABI accessors agree with the guarded config
//  9. `reset_global_scheduler_for_test` under the guard sizes run queues
//     for exactly one worker
// 10. a guarded drive polls on the calling thread (no pool threads)
// 11. a guarded drive still completes an ordinary ready task and counts it
// 12. `run_until(target)` stops at the target and does not reap a task the
//     drive itself woke (the willow-tcrg failure mode)
// 13. that woken bystander is left READY, not completed
// 14. an untargeted `willow_sched_run()` still drains everything ready
// 15. a guarded drive reaps a cancelled placeholder without counting it as
//     a completion
// 16. the cancelled task is gone from the table afterwards
// 17. the guard is RAII, so a panicking test cannot leak the override
// 18. the guard leaves task ids/state transitions otherwise unchanged
// 19. the override is invisible outside `cfg(test)` (compile-time: the
//     static, the guard, and the branch are all `#[cfg(test)]`)
// 20. repeated guarded drives stay deterministic under stress
// -----------------------------------------------------------------------

unsafe extern "C" fn poll_ready_noop(_frame: *mut c_void) -> i32 {
    RUNTIME_POLL_READY
}

#[test]
fn single_worker_guard_reports_one_active_and_requested_worker() {
    let _guard = runtime_test_guard();
    let _single = single_worker_for_test();
    let config = runtime_worker_config();
    assert_eq!(config.active_workers(), 1, "perspective 1");
    assert_eq!(config.requested_workers(), 1, "perspective 2");
    assert_eq!(willow_sched_active_workers(), 1, "perspective 8");
    assert_eq!(willow_sched_requested_workers(), 1, "perspective 8");
}

#[test]
fn single_worker_guard_is_scoped_and_nestable() {
    let _guard = runtime_test_guard();
    let previous = runtime_worker_config();

    {
        let outer = single_worker_for_test();
        {
            let _inner = single_worker_for_test();
            assert_eq!(runtime_worker_config().active_workers(), 1);
        }
        // Perspective 5: the inner drop must not re-enable the pool.
        assert_eq!(runtime_worker_config().active_workers(), 1, "perspective 5");
        drop(outer);
    }
    // Perspective 3: the outermost drop restores the default.
    assert_eq!(
        runtime_worker_config().active_workers(),
        previous.active_workers(),
        "perspective 3"
    );

    // Perspective 6: a fresh guard re-arms.
    let _again = single_worker_for_test();
    assert_eq!(runtime_worker_config().active_workers(), 1, "perspective 6");
}

#[test]
fn single_worker_guard_does_not_change_env_parsing() {
    let _guard = runtime_test_guard();
    let _single = single_worker_for_test();
    // Perspective 7: only `runtime_worker_config()` consults the override;
    // the parser still honors explicit positive counts.
    assert_eq!(
        RuntimeWorkerConfig::from_env_value(Some("1"), TEST_WORKERS).active_workers(),
        1,
        "perspective 7"
    );
    assert_eq!(
        RuntimeWorkerConfig::from_env_value(Some("8"), TEST_WORKERS).active_workers(),
        8,
        "perspective 7"
    );
}

#[test]
fn single_worker_guard_sizes_fresh_run_queues_for_one_worker() {
    let _guard = runtime_test_guard();
    let _single = single_worker_for_test();
    reset_global_scheduler_for_test();
    // Perspective 9: install the guard BEFORE the reset and the fresh
    // queues have one local deque, so nothing is stranded on a local queue
    // that the single driver never scans.
    assert_eq!(global_run_queues().locals.len(), 1, "perspective 9");
}

#[test]
fn single_worker_guard_reuses_the_same_os_worker_across_drives() {
    static THREADS: TestMutex<Vec<std::thread::ThreadId>> = TestMutex::new(Vec::new());
    unsafe extern "C" fn poll_records_thread(_frame: *mut c_void) -> i32 {
        THREADS.lock().unwrap().push(std::thread::current().id());
        RUNTIME_POLL_READY
    }
    let _guard = runtime_test_guard();
    let _single = single_worker_for_test();
    reset_global_scheduler_for_test();
    THREADS.lock().unwrap().clear();
    for _ in 0..2 {
        let id = willow_sched_spawn(poll_records_thread, std::ptr::null_mut());
        assert_eq!(willow_sched_run_until(id), 1);
    }
    let threads = THREADS.lock().unwrap();
    assert_eq!(threads.len(), 2);
    assert_eq!(threads[0], threads[1]);
    assert_ne!(threads[0], std::thread::current().id());
}

#[test]
fn single_worker_run_until_does_not_reap_a_task_the_drive_woke() {
    static WOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    unsafe extern "C" fn poll_wakes_bystander(_frame: *mut c_void) -> i32 {
        willow_sched_wake(WOKEN.load(Ordering::Acquire));
        RUNTIME_POLL_READY
    }

    let _guard = runtime_test_guard();
    let _single = single_worker_for_test();
    reset_global_scheduler_for_test();

    let bystander = with_global_for_test(|sched| sched.spawn_parked_placeholder());
    WOKEN.store(bystander, Ordering::Release);
    let target = willow_sched_spawn(poll_wakes_bystander, std::ptr::null_mut());

    // Perspective 12: the drive stops at its target. With the pool a second
    // worker could claim `bystander` the moment the target's poll woke it
    // and complete it too, making this count 2 (willow-tcrg).
    assert_eq!(
        willow_sched_run_until(target),
        1,
        "perspective 12: run_until must reap only its target"
    );
    // Perspective 13 / 18: the bystander is left runnable, untouched.
    with_global_for_test(|sched| {
        assert_eq!(
            sched.task_state(bystander),
            Some(RuntimeTaskState::Ready),
            "perspective 13"
        );
        assert_eq!(sched.task_state(target), None, "perspective 18");
    });

    // Perspective 14: an untargeted drive still drains what is left.
    assert_eq!(willow_sched_run(), 1, "perspective 14");
}

#[test]
fn single_worker_drive_reaps_a_cancelled_placeholder_without_counting_it() {
    let _guard = runtime_test_guard();
    let _single = single_worker_for_test();
    reset_global_scheduler_for_test();

    let id = with_global_for_test(|sched| sched.spawn_parked_placeholder());
    willow_sched_cancel(id);
    // Perspective 15: cancellation is terminal but is not a completion.
    assert_eq!(willow_sched_run_until(id), 0, "perspective 15");
    // Perspective 16: and the task is reaped out of the table.
    with_global_for_test(|sched| {
        assert_eq!(sched.task_state(id), None, "perspective 16");
    });
}

#[test]
fn single_worker_guard_is_released_when_a_scope_unwinds() {
    let _guard = runtime_test_guard();
    // Perspective 17: the override is RAII, so a panicking test body cannot
    // leave every later test pinned to one worker.
    let previous = runtime_worker_config();
    let panicked = std::panic::catch_unwind(|| {
        let _single = single_worker_for_test();
        assert_eq!(runtime_worker_config().active_workers(), 1);
        panic!("unwind with the guard live");
    });
    assert!(panicked.is_err());
    assert_eq!(
        runtime_worker_config().active_workers(),
        previous.active_workers(),
        "perspective 17"
    );
}

#[test]
fn single_worker_drives_are_repeatable() {
    let _guard = runtime_test_guard();
    let _single = single_worker_for_test();
    // Perspective 20: the determinism is not a one-shot fluke.
    for _ in 0..32 {
        reset_global_scheduler_for_test();
        let id = willow_sched_spawn(poll_ready_noop, std::ptr::null_mut());
        assert_eq!(willow_sched_run_until(id), 1);
        assert_eq!(willow_sched_task_state(id), -1);
    }
}

#[test]
fn scheduler_active_worker_abi_reports_requested_workers() {
    let _guard = runtime_test_guard();
    let expected = parse_worker_count(std::env::var("WILLOW_WORKERS").ok().as_deref())
        .unwrap_or_else(default_worker_count) as u64;
    assert_eq!(willow_sched_active_workers(), expected);
    assert_eq!(willow_sched_requested_workers(), expected);
}

#[test]
fn scheduler_park_removes_task_from_running_state_only() {
    let mut scheduler = RuntimeScheduler::default();
    let id = scheduler.spawn_placeholder();
    scheduler.park(id);
    assert_eq!(scheduler.task_state(id), Some(RuntimeTaskState::Parked));
}

#[test]
fn scheduler_wake_requeues_parked_task() {
    let mut scheduler = RuntimeScheduler::default();
    let id = scheduler.spawn_placeholder();
    assert_eq!(scheduler.pop_ready(), Some(id));
    scheduler.park(id);
    scheduler.wake(id);
    assert_eq!(scheduler.task_state(id), Some(RuntimeTaskState::Ready));
    assert_eq!(scheduler.pop_ready(), Some(id));
}

#[test]
fn scheduler_wake_ready_task_does_not_duplicate_ready_queue() {
    let mut scheduler = RuntimeScheduler::default();
    let id = scheduler.spawn_placeholder();
    scheduler.wake(id);
    assert_eq!(scheduler.ready_len(), 1);
    assert_eq!(scheduler.pop_ready(), Some(id));
    assert_eq!(scheduler.pop_ready(), None);
}

#[test]
fn scheduler_spawn_parked_placeholder_does_not_enter_ready_queue() {
    let mut scheduler = RuntimeScheduler::default();
    let id = scheduler.spawn_parked_placeholder();
    assert_eq!(scheduler.ready_len(), 0);
    assert_eq!(scheduler.task_state(id), Some(RuntimeTaskState::Parked));
}

fn park_with_sleep(scheduler: &mut RuntimeScheduler, id: RuntimeTaskId, millis: i64) -> Instant {
    assert_eq!(scheduler.pop_ready(), Some(id));
    scheduler.set_running(id);
    scheduler.set_running_wake_after_millis(millis);
    scheduler.clear_running();
    scheduler.park(id);
    scheduler
        .with_task(id, |task| task.wake_deadline.unwrap())
        .unwrap()
}

#[test]
fn scheduler_timer_heap_selects_earliest_deadline() {
    let mut scheduler = RuntimeScheduler::default();
    let slow = scheduler.spawn_placeholder();
    let fast = scheduler.spawn_placeholder();

    park_with_sleep(&mut scheduler, slow, 50);
    let fast_deadline = park_with_sleep(&mut scheduler, fast, 0);

    assert_eq!(scheduler.timers.len(), 2);
    assert_eq!(scheduler.next_timer_deadline(), Some((fast, fast_deadline)));
}

#[test]
fn scheduler_timer_heap_prunes_stale_woken_task() {
    let mut scheduler = RuntimeScheduler::default();
    let id = scheduler.spawn_placeholder();
    park_with_sleep(&mut scheduler, id, 50);
    assert_eq!(scheduler.timers.len(), 1);

    scheduler.wake(id);

    assert_eq!(scheduler.next_timer_deadline(), None);
    assert_eq!(scheduler.timers.len(), 0);
}

#[test]
fn scheduler_timer_heap_pops_due_timer_once() {
    let mut scheduler = RuntimeScheduler::default();
    let id = scheduler.spawn_placeholder();
    park_with_sleep(&mut scheduler, id, 0);

    assert_eq!(scheduler.wake_due_timers(Instant::now()), 1);
    assert_eq!(scheduler.task_state(id), Some(RuntimeTaskState::Ready));
    // The entry is consumed: a second sweep finds nothing to promote.
    assert_eq!(scheduler.wake_due_timers(Instant::now()), 0);
    assert_eq!(scheduler.timers.len(), 0);
}

#[test]
fn scheduler_due_timer_transition_publishes_ready_task_atomically() {
    let mut scheduler = RuntimeScheduler::default();
    let id = scheduler.spawn_placeholder();
    park_with_sleep(&mut scheduler, id, 0);

    assert_eq!(scheduler.wake_due_timers(Instant::now()), 1);
    assert_eq!(scheduler.next_timer_deadline(), None);
    assert_eq!(scheduler.task_state(id), Some(RuntimeTaskState::Ready));
    assert_eq!(scheduler.pop_ready(), Some(id));
}

// -----------------------------------------------------------------------
// willow-9ha4: timer accounting lives behind its OWN lock, not the global
// scheduler metadata mutex. The 26 perspectives below:
//
//   Timer-queue semantics (deterministic, single-threaded)
//     01 registering a sleep publishes an entry and arms the lock-free hint
//     02 sleep ORDERING across three deadlines is unchanged
//     03 `sleep(0)` is immediately due
//     04 a negative duration clamps to zero instead of wrapping
//     05 a sleep requested outside a poll registers nothing
//     06 an early wake prunes the stale entry and restores the empty hint
//     07 a re-armed sleep supersedes the earlier entry
//     08 an empty queue answers from the sentinel hint, taking no lock
//     09 a reaped task's entry is pruned, never woken
//     09b a terminal record still in the table reads as stale, not a panic
//     10 promotion publishes Ready + the queue entry together
//     11 the entry count returns to zero — no per-sleep leak
//     12 timers are no longer part of the scheduler metadata snapshot
//
//   Lock decomposition (the point of the issue)
//     13 registration completes while GLOBAL_SCHEDULER is held elsewhere
//     14 promotion completes while GLOBAL_SCHEDULER is held elsewhere
//     15 the earliest-deadline query completes while it is held elsewhere
//     16 the empty-queue sweep answers from one atomic load while it is held
//     17 the per-test reset installs a fresh, empty timer queue
//     18 concurrent registration + drain fires every timer exactly once
//     19 no observable window with neither a timer nor a ready entry
//
//   Idle notification (acceptance criterion — must not regress)
//     20 a blocking-pool completion wake releases the idle waiter, no spin
//     21 a wake between the snapshot and the wait is not lost
//
//   End-to-end through the ABI
//     22 a sleeping task completes under `willow_sched_run`, leaving no entry
//     23 `willow_sched_run_until` returns when the sleeping target completes
//     24 cancelling a sleeper clears its deadline and the drive terminates
//     25 a drive deadline still wins over a far-off sleeper
//     26 staggered sleepers complete in DEADLINE order, not spawn order
// -----------------------------------------------------------------------

#[test]
fn t9ha4_01_registering_a_sleep_publishes_an_entry_and_arms_the_hint() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    let before = Instant::now();
    let deadline = park_with_sleep(&mut s, id, 5);

    assert_eq!(s.timers.len(), 1);
    assert_eq!(s.next_timer_deadline(), Some((id, deadline)));
    assert_ne!(
        s.timers.earliest_hint_nanos(),
        TimerQueue::empty_hint(),
        "the lock-free hint must arm so the run loop stops short-circuiting"
    );
    assert!(
        !s.timers.maybe_due(before),
        "a future deadline must not read as due"
    );
    assert!(s.timers.maybe_due(deadline));
}

#[test]
fn t9ha4_02_sleep_ordering_across_three_deadlines_is_unchanged() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let first = s.spawn_placeholder();
    let second = s.spawn_placeholder();
    let third = s.spawn_placeholder();

    // Registered in ascending order, but the heap — not the registration
    // order — is what decides who fires first.
    let d0 = park_with_sleep(&mut s, first, 0);
    let d1 = park_with_sleep(&mut s, second, 30);
    let d2 = park_with_sleep(&mut s, third, 60);
    assert!(d0 < d1 && d1 < d2);

    assert_eq!(s.next_timer_deadline(), Some((first, d0)));
    assert_eq!(s.wake_due_timers(d0), 1);
    assert_eq!(s.next_timer_deadline(), Some((second, d1)));
    assert_eq!(s.wake_due_timers(d1), 1);
    assert_eq!(s.next_timer_deadline(), Some((third, d2)));
    assert_eq!(s.wake_due_timers(d2), 1);
    assert_eq!(s.next_timer_deadline(), None);
    assert_eq!(s.timers.len(), 0);
}

#[test]
fn t9ha4_03_zero_millisecond_sleep_is_immediately_due() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    let deadline = park_with_sleep(&mut s, id, 0);

    assert!(deadline <= Instant::now());
    assert!(s.timers.maybe_due(Instant::now()));
    assert_eq!(s.wake_due_timers(Instant::now()), 1);
    assert_eq!(s.task_state(id), Some(RuntimeTaskState::Ready));
}

#[test]
fn t9ha4_04_negative_sleep_clamps_to_zero_instead_of_wrapping() {
    // `Duration::from_millis` takes a u64: a raw cast of -5 would be a
    // ~584-million-year deadline that never fires.
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    let deadline = park_with_sleep(&mut s, id, -5);

    assert!(
        deadline <= Instant::now(),
        "negative sleep must clamp to now"
    );
    assert_eq!(s.wake_due_timers(Instant::now()), 1);
    assert_eq!(s.task_state(id), Some(RuntimeTaskState::Ready));
}

#[test]
fn t9ha4_05_sleep_outside_a_poll_registers_nothing() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    s.clear_running();

    s.set_running_wake_after_millis(5);

    assert_eq!(
        s.timers.len(),
        0,
        "no running task means there is nobody to wake"
    );
    assert_eq!(s.timers.earliest_hint_nanos(), TimerQueue::empty_hint());
    assert_eq!(s.with_task(id, |task| task.wake_deadline).unwrap(), None);
}

#[test]
fn t9ha4_06_early_wake_prunes_the_stale_entry_and_restores_the_empty_hint() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    park_with_sleep(&mut s, id, 50);
    assert_eq!(s.timers.len(), 1);

    // A channel/join wake beats the deadline. The heap entry is now stale.
    assert!(s.wake(id));

    assert_eq!(s.next_timer_deadline(), None);
    assert_eq!(s.timers.len(), 0);
    assert_eq!(
        s.timers.earliest_hint_nanos(),
        TimerQueue::empty_hint(),
        "pruning must republish the hint or idle workers keep waking for nothing"
    );
}

#[test]
fn t9ha4_timer_wake_rechecks_identity_under_task_lock() {
    let mut scheduler = RuntimeScheduler::with_worker_count(1);
    let id = scheduler.spawn_placeholder();
    let old = park_with_sleep(&mut scheduler, id, 0);
    assert!(timer_entry_is_current(
        &scheduler.tasks,
        TimerWake {
            task_id: id,
            deadline: old
        }
    ));
    assert!(scheduler.wake(id));
    let new = park_with_sleep(&mut scheduler, id, 60_000);
    // Model a re-arm after the heap's liveness check, before its callback.
    assert_eq!(
        wake_task_matching_in(&scheduler.tasks, &scheduler.run_queues, id, Some(old)),
        None
    );
    assert_eq!(
        scheduler.with_task(id, |task| task.wake_deadline),
        Some(Some(new))
    );
    assert_eq!(
        scheduler.with_task(id, |task| task.state.lifecycle()),
        Some(TaskLifecycle::Parked)
    );
    assert_eq!(
        wake_task_matching_in(&scheduler.tasks, &scheduler.run_queues, id, Some(new)),
        Some(WakeOutcome::Enqueue)
    );
    assert_eq!(
        wake_task_matching_in(&scheduler.tasks, &scheduler.run_queues, id, Some(new)),
        None
    );
}

#[test]
fn t9ha4_07_re_armed_sleep_supersedes_the_earlier_entry() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    let stale = park_with_sleep(&mut s, id, 50);

    // Woken early, then it sleeps again with a NEARER deadline. Both
    // entries are in the heap; only the second one is current.
    assert!(s.wake(id));
    let current = park_with_sleep(&mut s, id, 5);
    assert!(current < stale);

    assert_eq!(s.next_timer_deadline(), Some((id, current)));
    assert_eq!(
        s.wake_due_timers(stale),
        1,
        "the task must fire exactly once"
    );
    assert_eq!(
        s.timers.len(),
        0,
        "the superseded entry is pruned, not kept"
    );
}

#[test]
fn t9ha4_08_empty_queue_answers_from_the_sentinel_hint() {
    // The run loop calls this on every worker on every iteration, so the
    // no-timer case must cost one atomic load and take no lock at all.
    let s = RuntimeScheduler::with_worker_count(1);
    assert_eq!(s.timers.len(), 0);
    assert_eq!(s.timers.earliest_hint_nanos(), TimerQueue::empty_hint());
    assert!(!s.timers.maybe_due(Instant::now()));
    assert_eq!(s.next_timer_deadline(), None);
    assert_eq!(s.wake_due_timers(Instant::now()), 0);
}

#[test]
fn t9ha4_09_reaped_task_entry_is_pruned_never_woken() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    park_with_sleep(&mut s, id, 50);
    s.complete(id);
    assert_eq!(s.task_state(id), None, "terminal records are reaped");

    assert_eq!(s.next_timer_deadline(), None);
    assert_eq!(s.timers.len(), 0);
    assert_eq!(
        s.wake_due_timers(Instant::now() + Duration::from_secs(1)),
        0,
        "a timer must never resurrect a reaped task"
    );
}

#[test]
fn t9ha4_09b_terminal_but_unreaped_entry_is_stale_not_a_panic() {
    // 09 covers the record being gone. `finish_terminal` gets there in two
    // steps: it publishes Terminal under the task's shard, releases it, then
    // re-takes the shard to remove the record. A promoting thread that looks
    // between the two sees a Terminal record that is STILL in the table and
    // must read it as a stale entry — it used to hit an `unreachable!` in
    // `runtime_state` and abort the process (willow-0a6k.7).
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    let deadline = park_with_sleep(&mut s, id, 0);
    // The first half of `finish_terminal`, without the `remove` that follows
    // it: the record stays in the table, carrying its leftover deadline.
    s.prepare_placeholder_terminal_owner(id);
    assert!(
        s.tasks
            .with(id, |task| task.state.finish_terminal())
            .unwrap(),
        "the record must still be in the table, now terminal"
    );
    assert_eq!(
        s.tasks.with(id, |task| task.wake_deadline),
        Some(Some(deadline)),
        "the timer entry the promoter will find must still be there"
    );

    assert_eq!(s.next_timer_deadline(), None);
    assert_eq!(
        s.wake_due_timers(Instant::now() + Duration::from_secs(1)),
        0,
        "a terminal record must not be woken by its own leftover timer"
    );
}

#[test]
fn t9ha4_10_promotion_publishes_ready_and_the_queue_entry_together() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    park_with_sleep(&mut s, id, 0);
    assert_eq!(s.ready_total(), 0);

    assert_eq!(s.wake_due_timers(Instant::now()), 1);

    assert_eq!(s.task_state(id), Some(RuntimeTaskState::Ready));
    assert!(
        s.is_queued(id),
        "a Ready task that is not queued is invisible to every worker"
    );
    assert_eq!(s.ready_total(), 1);
    assert_eq!(s.with_task(id, |task| task.wake_deadline).unwrap(), None);
}

#[test]
fn t9ha4_11_entry_count_returns_to_zero_after_every_sleeper_fires() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let ids = (0..64).map(|_| s.spawn_placeholder()).collect::<Vec<_>>();
    for &id in &ids {
        park_with_sleep(&mut s, id, 0);
    }
    assert_eq!(s.timers.len(), 64);

    assert_eq!(s.wake_due_timers(Instant::now()), 64);

    assert_eq!(
        s.timers.len(),
        0,
        "the heap must not retain drained entries"
    );
    assert_eq!(s.timers.earliest_hint_nanos(), TimerQueue::empty_hint());
    assert_eq!(s.ready_total(), 64);
}

#[test]
fn t9ha4_12_timers_are_not_part_of_the_scheduler_metadata_snapshot() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    park_with_sleep(&mut s, id, 50);

    // The snapshot counts what the metadata mutex owns. A pending timer is
    // no longer one of those things (willow-9ha4).
    assert_eq!(
        s.metadata_snapshot(),
        SchedulerMetadataSnapshot {
            heavy_tasks: 1,
            queue_entries: 0,
            pending_cleanups: 0,
            frame_roots: 0,
            blocked_syscalls: 0,
        }
    );
    assert_eq!(s.timers.len(), 1);
}

/// Run `body` on another thread while THIS thread holds the global
/// scheduler metadata mutex, and fail if it does not finish promptly.
///
/// Every willow-9ha4 timer operation must complete here; before the split
/// they all serialized on the lock this helper is squatting on.
fn without_scheduler_lock<T: Send>(what: &str, body: impl FnOnce() -> T + Send) -> T {
    let scheduler_lock = GLOBAL_SCHEDULER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (tx, rx) = std::sync::mpsc::channel();
    let result = std::thread::scope(|scope| {
        scope.spawn(|| {
            tx.send(body()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("{what} must not need GLOBAL_SCHEDULER"))
    });
    drop(scheduler_lock);
    result
}

#[test]
fn t9ha4_13_sleep_registration_does_not_need_the_scheduler_lock() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let id = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);

    without_scheduler_lock("timer registration", move || {
        with_current_task_for_test(id, || set_global_wake_after_millis(20));
    });

    assert_eq!(global_timers().len(), 1);
    assert_ne!(
        global_timers().earliest_hint_nanos(),
        TimerQueue::empty_hint()
    );
    reset_global_scheduler_for_test();
}

#[test]
fn t9ha4_14_timer_promotion_does_not_need_the_scheduler_lock() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let id = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);
    with_current_task_for_test(id, || set_global_wake_after_millis(0));

    let woken =
        without_scheduler_lock("timer promotion", || wake_global_due_timers(Instant::now()));

    assert_eq!(woken, 1);
    assert_eq!(willow_sched_task_state(id), 0, "the task must be Ready");
    assert_eq!(global_run_queues().len(), 1);
    reset_global_scheduler_for_test();
}

#[test]
fn t9ha4_15_earliest_deadline_query_does_not_need_the_scheduler_lock() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let id = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);
    with_current_task_for_test(id, || set_global_wake_after_millis(50));

    // The idle path asks this question before deciding how long to wait; it
    // must not block behind whatever is holding the metadata mutex.
    let earliest = without_scheduler_lock("the earliest-deadline query", || {
        global_next_timer_deadline()
    });

    assert_eq!(earliest.map(|(id, _)| id), Some(id));
    reset_global_scheduler_for_test();
}

#[test]
fn t9ha4_16_empty_sweep_is_an_atomic_load_even_while_the_scheduler_lock_is_held() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    assert_eq!(
        global_timers().earliest_hint_nanos(),
        TimerQueue::empty_hint()
    );

    // This is the per-iteration, per-worker call in `scheduler_run_loop`.
    let swept = without_scheduler_lock("the empty timer sweep", || {
        wake_global_due_timers(Instant::now())
    });

    assert_eq!(swept, 0);
    reset_global_scheduler_for_test();
}

#[test]
fn t9ha4_17_reset_installs_a_fresh_empty_timer_queue() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let id = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);
    with_current_task_for_test(id, || set_global_wake_after_millis(5_000));
    let leaked = global_timers();
    assert_eq!(leaked.len(), 1);

    reset_global_scheduler_for_test();

    assert_eq!(
        global_timers().len(),
        0,
        "a stale timer must not leak into the next test"
    );
    assert_eq!(
        global_timers().earliest_hint_nanos(),
        TimerQueue::empty_hint()
    );
    assert!(
        !Arc::ptr_eq(leaked, global_timers()),
        "reset must install a NEW queue, not drain the old one in place"
    );
}

#[test]
fn t8hq4_18_component_access_is_stable_until_reset() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let (queues, tasks, timers) = (global_run_queues(), global_task_table(), global_timers());
    assert!(Arc::ptr_eq(queues, global_run_queues()));
    assert!(Arc::ptr_eq(tasks, global_task_table()));
    assert!(Arc::ptr_eq(timers, global_timers()));
    let id = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);
    assert_eq!(tasks.len(), 1);

    reset_global_scheduler_for_test();

    assert!(!Arc::ptr_eq(queues, global_run_queues()));
    assert!(!Arc::ptr_eq(tasks, global_task_table()));
    assert!(!Arc::ptr_eq(timers, global_timers()));
    // A borrow taken before the reset stays valid: the old set is leaked.
    assert_eq!(tasks.len(), 0, "reset drains the old table");
    assert!(global_task_table().with(id, |_| ()).is_none());
    // The scheduler instance shares the newly published components.
    let spawned = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);
    assert!(global_task_table().with(spawned, |_| ()).is_some());
    reset_global_scheduler_for_test();
}

#[test]
fn t8hq4_18_task_id_hash_spreads_ids_of_one_shard() {
    use std::hash::BuildHasher;
    const IDS: u64 = 4096;
    let build = BuildHasherDefault::<TaskIdHasher>::default();
    for residue in [0, 1, 17, (TASK_TABLE_SHARDS - 1) as u64] {
        let hashes: Vec<u64> = (0..IDS)
            .map(|k| build.hash_one(residue + k * TASK_TABLE_SHARDS as u64))
            .collect();
        // hashbrown picks the bucket from the low bits; a random hash fills
        // about 63% of 4096 buckets with 4096 keys (this hash: about 66%).
        let buckets: HashSet<u64> = hashes.iter().map(|hash| hash & (IDS - 1)).collect();
        assert!(
            buckets.len() * 10 >= IDS as usize * 6,
            "residue {residue}: {} of {IDS} buckets used",
            buckets.len()
        );
        // ...and the control tag from the top seven bits.
        let tags: HashSet<u64> = hashes.iter().map(|hash| hash >> 57).collect();
        assert_eq!(tags.len(), 128, "residue {residue}");
    }
}

fn terminal_cleanups_pending() -> bool {
    global_components()
        .terminal_cleanups_pending
        .load(Ordering::Acquire)
}

/// Complete an executable task directly, so its terminal cleanup record is
/// queued but not yet drained. Executable tasks always queue one.
fn complete_executable_without_drain() -> RuntimeTaskId {
    let id = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    with_global_for_test(|sched| {
        sched.set_running(id);
        sched.complete(id);
        sched.clear_running();
    });
    id
}

fn pending_cleanup_count() -> usize {
    with_global_for_test(|sched| sched.metadata_snapshot().pending_cleanups)
}

#[test]
fn t8hq4_19_cleanup_flag_follows_the_pending_list() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    assert!(!terminal_cleanups_pending(), "a fresh scheduler has none");
    complete_executable_without_drain();
    complete_executable_without_drain();
    assert!(terminal_cleanups_pending());
    assert_eq!(pending_cleanup_count(), 2);

    drain_terminal_cleanups();

    assert!(!terminal_cleanups_pending());
    assert_eq!(pending_cleanup_count(), 0);
    drain_terminal_cleanups();
    assert!(
        !terminal_cleanups_pending(),
        "an empty drain keeps it clear"
    );
}

#[test]
fn t8hq4_19_empty_drain_does_not_take_the_scheduler_mutex() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let held = GLOBAL_SCHEDULER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let drainer = std::thread::spawn(move || {
        drain_terminal_cleanups();
        done_tx.send(()).unwrap();
    });
    let finished = done_rx.recv_timeout(Duration::from_secs(5)).is_ok();
    drop(held);
    drainer.join().unwrap();
    assert!(
        finished,
        "a drain with nothing pending blocked on GLOBAL_SCHEDULER"
    );
}

#[test]
fn t8hq4_19_pending_drain_still_takes_the_list() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    complete_executable_without_drain();
    let held = GLOBAL_SCHEDULER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let drainer = std::thread::spawn(move || {
        drain_terminal_cleanups();
        done_tx.send(()).unwrap();
    });
    let early = done_rx.recv_timeout(Duration::from_millis(100)).is_ok();
    drop(held);
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| assert!(early, "drain never finished"));
    drainer.join().unwrap();
    assert!(!early, "a pending cleanup must be taken under the mutex");
    assert_eq!(pending_cleanup_count(), 0);
    assert!(!terminal_cleanups_pending());
}

#[test]
fn t8hq4_19_reset_installs_a_clear_flag() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    complete_executable_without_drain();
    assert!(terminal_cleanups_pending());
    reset_global_scheduler_for_test();
    assert!(!terminal_cleanups_pending());
    assert_eq!(pending_cleanup_count(), 0);
}

#[test]
fn t8hq4_19_a_private_scheduler_does_not_touch_the_global_flag() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let mut scheduler = RuntimeScheduler::with_worker_count(TEST_WORKERS);
    let id = scheduler.spawn_task(poll_ready_now, std::ptr::null_mut());
    scheduler.set_running(id);
    scheduler.complete(id);
    scheduler.clear_running();
    assert!(scheduler.terminal_cleanups_pending.load(Ordering::Acquire));
    assert!(!terminal_cleanups_pending());
    assert_eq!(scheduler.take_pending_terminal_cleanups().len(), 1);
    assert!(!scheduler.terminal_cleanups_pending.load(Ordering::Acquire));
}

#[test]
fn t8hq4_19_cleanups_published_during_drains_are_never_lost() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    const TASKS: usize = 2_000;
    let stop = Arc::new(AtomicBool::new(false));
    let drainer_stop = Arc::clone(&stop);
    let drainer = std::thread::spawn(move || {
        while !drainer_stop.load(Ordering::Acquire) {
            drain_terminal_cleanups();
        }
    });
    for _ in 0..TASKS {
        complete_executable_without_drain();
    }
    stop.store(true, Ordering::Release);
    drainer.join().unwrap();
    // The drive-end drain runs after the workers have joined.
    drain_terminal_cleanups();
    assert_eq!(pending_cleanup_count(), 0);
    assert!(!terminal_cleanups_pending());
}

fn terminal_epoch() -> u64 {
    global_task_table().terminal_epoch()
}

/// Run `f` on another thread while this thread holds `id`'s task shard, and
/// report whether it finished without that shard (bounded wait).
fn finishes_without_shard<R: Send>(id: RuntimeTaskId, f: impl FnOnce() -> R + Send) -> Option<R> {
    let table = global_task_table();
    let held = table.lock_shard(table.shard_index(id));
    std::thread::scope(|scope| {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = scope.spawn(move || done_tx.send(f()).unwrap());
        let result = done_rx.recv_timeout(Duration::from_secs(2)).ok();
        drop(held);
        worker.join().unwrap();
        result
    })
}

#[test]
fn t8hq4_19_terminal_epoch_advances_on_terminal_transitions_and_removals_only() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let start = terminal_epoch();
    let parked = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);
    let runnable = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    assert_eq!(terminal_epoch(), start, "spawning is not terminal");
    complete_executable_without_drain();
    let after_complete = terminal_epoch();
    assert!(after_complete > start, "a completion advances the epoch");
    assert!(global_task_table().remove(parked).is_some());
    let after_remove = terminal_epoch();
    assert!(
        after_remove > after_complete,
        "a removal advances the epoch"
    );
    assert!(global_task_table().remove(parked).is_none());
    assert_eq!(terminal_epoch(), after_remove, "a missed removal does not");
    assert!(global_task_table().with(runnable, |_| ()).is_some());
    drain_terminal_cleanups();
    reset_global_scheduler_for_test();
}

#[test]
fn t8hq4_19_target_watch_looks_again_only_after_the_epoch_moves() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let target = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    let mut watch = TargetWatch::new(Some(target), None);
    assert!(!watch.is_done(), "the first question looks");
    // No epoch change: answered without the target's shard.
    let (live, watch) = finishes_without_shard(target, move || (watch.is_done(), watch))
        .expect("an unchanged epoch must not lock the target shard");
    let mut watch = watch;
    assert!(!live);
    // An unrelated completion moves the epoch; the target is still live.
    complete_executable_without_drain();
    assert!(!watch.is_done());
    // The target's own completion is seen on the next question.
    with_global_for_test(|sched| {
        sched.set_running(target);
        sched.complete(target);
        sched.clear_running();
    });
    assert!(watch.is_done());
    // Done is final: answered without the shard even after the epoch moves.
    complete_executable_without_drain();
    let done = finishes_without_shard(target, move || watch.is_done())
        .expect("a done answer must not lock the target shard");
    assert!(done);
    assert!(
        !TargetWatch::new(None, None).is_done(),
        "no target never ends"
    );
    drain_terminal_cleanups();
    reset_global_scheduler_for_test();
}

#[test]
fn t8hq4_19_a_removed_target_is_done() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let target = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);
    let mut watch = TargetWatch::new(Some(target), None);
    assert!(!watch.is_done());
    assert!(global_task_table().remove(target).is_some());
    assert!(watch.is_done());
    reset_global_scheduler_for_test();
}

#[test]
fn t8hq4_19_pool_workers_share_one_look_per_epoch() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let target = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    let shared = SharedTargetCheck::default();
    let mut first = TargetWatch::new(Some(target), Some(&shared));
    assert!(!first.is_done(), "the first worker looks for this epoch");
    let second = TargetWatch::new(Some(target), Some(&shared));
    let (live, mut second) = finishes_without_shard(target, move || {
        let mut second = second;
        (second.is_done(), second)
    })
    .expect("a second worker must not look in an epoch already claimed");
    assert!(!live);
    with_global_for_test(|sched| {
        sched.set_running(target);
        sched.complete(target);
        sched.clear_running();
    });
    assert!(
        second.is_done(),
        "the next epoch's look sees the target done"
    );
    assert!(shared.done.load(Ordering::Acquire));
    let done = finishes_without_shard(target, move || first.is_done())
        .expect("a published done must not lock the target shard");
    assert!(done, "every worker of the pool sees the shared done");
    drain_terminal_cleanups();
    reset_global_scheduler_for_test();
}

#[test]
fn t8hq4_19_racing_pool_watchers_never_miss_the_target() {
    let _guard = runtime_test_guard();
    const WATCHERS: usize = 4;
    for _ in 0..200 {
        reset_global_scheduler_for_test();
        let target = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
        let shared = SharedTargetCheck::default();
        let start = Barrier::new(WATCHERS + 1);
        std::thread::scope(|scope| {
            let watchers = (0..WATCHERS)
                .map(|_| {
                    let (shared, start) = (&shared, &start);
                    scope.spawn(move || {
                        let mut watch = TargetWatch::new(Some(target), Some(shared));
                        start.wait();
                        let give_up = Instant::now() + Duration::from_secs(10);
                        while !watch.is_done() {
                            assert!(Instant::now() < give_up, "a watcher missed the target");
                            std::hint::spin_loop();
                        }
                    })
                })
                .collect::<Vec<_>>();
            start.wait();
            for _ in 0..3 {
                complete_executable_without_drain();
            }
            with_global_for_test(|sched| {
                sched.set_running(target);
                sched.complete(target);
                sched.clear_running();
            });
            for watcher in watchers {
                watcher.join().unwrap();
            }
        });
        drain_terminal_cleanups();
    }
    reset_global_scheduler_for_test();
}

#[test]
fn t8hq4_19_a_draining_worker_holds_a_claim_in_flight() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    complete_executable_without_drain();
    let before = claim_word();
    let state = ParallelRunState::default();
    let held = GLOBAL_SCHEDULER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let drainer = std::thread::spawn(drain_terminal_cleanups);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !claims_in_flight() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    let visible = claims_in_flight();
    // The drainer won the swap; a second caller returns without the list
    // while the winner's marker is still in the claim word.
    let (loser_tx, loser_rx) = std::sync::mpsc::channel();
    let loser = std::thread::spawn(move || {
        drain_terminal_cleanups();
        loser_tx.send(claims_in_flight()).unwrap();
    });
    let loser_saw_marker = loser_rx.recv_timeout(Duration::from_secs(5));
    let refused = !state.publish_stop_if(|_| true);
    drop(held);
    drainer.join().unwrap();
    loser.join().unwrap();
    assert!(visible, "a drain in progress must be a claim in flight");
    assert_eq!(
        loser_saw_marker,
        Ok(true),
        "the losing drain must not block"
    );
    assert!(refused, "a stop decision must not pass a drain in progress");
    assert_eq!(claim_word() & CLAIM_COUNT_MASK, 0);
    assert_ne!(claim_word(), before, "a finished drain advances the epoch");
    assert_eq!(pending_cleanup_count(), 0);
    assert!(state.publish_stop_if(|_| true));
    reset_global_scheduler_for_test();
}

#[test]
fn t8hq4_19_an_empty_drain_leaves_the_claim_word_alone() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let before = claim_word();
    drain_terminal_cleanups();
    assert_eq!(claim_word(), before);
}

#[test]
fn t9ha4_18_concurrent_registration_and_drain_fire_every_timer_once() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    const SLEEPERS: usize = 256;
    const REGISTRARS: usize = 4;
    const DRAINERS: usize = 4;

    let ids = (0..SLEEPERS)
        .map(|_| with_global_for_test(RuntimeScheduler::spawn_parked_placeholder))
        .collect::<Vec<_>>();
    let woken = AtomicUsize::new(0);
    let start = Barrier::new(REGISTRARS + DRAINERS);

    // Registration takes the heap lock then a task shard; promotion takes
    // the heap lock and calls into a shard from inside it. Running both at
    // once is what would deadlock if either side inverted the order.
    std::thread::scope(|scope| {
        for chunk in ids.chunks(SLEEPERS / REGISTRARS) {
            let start = &start;
            scope.spawn(move || {
                start.wait();
                for &id in chunk {
                    with_current_task_for_test(id, || set_global_wake_after_millis(0));
                }
            });
        }
        for _ in 0..DRAINERS {
            let (woken, start) = (&woken, &start);
            scope.spawn(move || {
                start.wait();
                let give_up = Instant::now() + Duration::from_secs(10);
                while woken.load(Ordering::Acquire) < SLEEPERS && Instant::now() < give_up {
                    let fired = wake_global_due_timers(Instant::now());
                    woken.fetch_add(fired, Ordering::AcqRel);
                    std::hint::spin_loop();
                }
            });
        }
    });

    assert_eq!(
        woken.load(Ordering::Acquire),
        SLEEPERS,
        "each timer must be promoted exactly once, by exactly one drainer"
    );
    assert_eq!(global_timers().len(), 0);
    assert_eq!(global_run_queues().len(), SLEEPERS);
    reset_global_scheduler_for_test();
}

#[test]
fn t9ha4_19_promotion_never_exposes_a_window_with_neither_timer_nor_ready_entry() {
    // Popped batches must remain visible to idle detection until the task
    // is Ready, even though wake callbacks run outside the heap lock.
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let id = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);
    with_current_task_for_test(id, || set_global_wake_after_millis(0));

    let violated = TestAtomicBool::new(false);
    let done = TestAtomicBool::new(false);
    std::thread::scope(|scope| {
        let (violated, done) = (&violated, &done);
        scope.spawn(move || {
            while !done.load(TestOrdering::Acquire) {
                if global_next_timer_deadline().is_none()
                    && global_run_queues().len() == 0
                    && willow_sched_task_state(id) == 2
                {
                    violated.store(true, TestOrdering::Release);
                }
            }
        });
        assert_eq!(wake_global_due_timers(Instant::now()), 1);
        done.store(true, TestOrdering::Release);
    });

    assert!(
        !violated.load(TestOrdering::Acquire),
        "a parked sleeper was observed with neither a live timer nor a queue entry"
    );
    reset_global_scheduler_for_test();
}

#[test]
fn t9ha4_20_blocking_completion_wake_releases_the_idle_waiter_without_spinning() {
    // Acceptance criterion: moving timers off the metadata mutex must not
    // disturb the BlockedSyscall keep-alive arm, which parks on the
    // generation counter instead of polling at 1ms (willow-5if8).
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let id = with_global_for_test(RuntimeScheduler::spawn_placeholder);
    assert_eq!(
        claim_global_ready_for_worker(0, None).map(|(id, _)| id),
        Some(id)
    );
    finish_global_poll_boundary(id, GlobalPollBoundary::BlockedSyscall);
    assert_eq!(global_task_table().blocked_syscall_count(), 1);
    // No timer exists, so the idle path takes the keep-alive arm.
    assert_eq!(global_timers().len(), 0);

    let generation = current_wake_generation();
    let start = Instant::now();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            assert!(try_wake_parked_task(id));
        });
        assert!(
            wait_for_wake_since(generation, Duration::from_secs(5)),
            "the completion wake must release the waiter before the fallback timeout"
        );
    });
    let waited = start.elapsed();

    assert!(
        waited >= Duration::from_millis(15),
        "the waiter returned before the completion could arrive: {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(2),
        "the waiter sat through the fallback instead of being signalled: {waited:?}"
    );
    assert_eq!(global_task_table().blocked_syscall_count(), 0);
    assert_eq!(global_run_queues().len(), 1);
    reset_global_scheduler_for_test();
}

#[test]
fn t9ha4_21_wake_between_snapshot_and_wait_is_not_lost() {
    // The idle worker snapshots the generation, then checks scheduler
    // state, then waits. A completion landing in that gap must be observed
    // immediately — otherwise the worker sleeps out the 50ms fallback with
    // runnable work already queued.
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let id = with_global_for_test(RuntimeScheduler::spawn_placeholder);
    assert_eq!(
        claim_global_ready_for_worker(0, None).map(|(id, _)| id),
        Some(id)
    );
    finish_global_poll_boundary(id, GlobalPollBoundary::BlockedSyscall);

    let generation = current_wake_generation();
    assert!(try_wake_parked_task(id));
    let start = Instant::now();
    assert!(wait_for_wake_since(generation, Duration::from_secs(5)));

    assert!(
        start.elapsed() < Duration::from_secs(1),
        "a wake that already happened must return from the wait at once"
    );
    reset_global_scheduler_for_test();
}

#[test]
fn t9ha4_22_sleeping_task_completes_under_run_and_leaves_no_entry() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
    let id = willow_sched_spawn(poll_sleep_then_ready, frame);

    assert_eq!(willow_sched_run(), 1);

    assert_eq!(willow_sched_task_state(id), -1);
    assert_eq!(
        global_timers().len(),
        0,
        "a completed sleeper must not leave a heap entry behind"
    );
    assert_eq!(
        global_timers().earliest_hint_nanos(),
        TimerQueue::empty_hint()
    );
    reset_global_scheduler_for_test();
    reset_internal_for_test();
}

#[test]
fn t9ha4_23_run_until_returns_when_the_sleeping_target_completes() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
    let target = willow_sched_spawn(poll_sleep_then_ready, frame);

    let start = Instant::now();
    assert_eq!(willow_sched_run_until(target), 1);

    assert!(
        start.elapsed() >= Duration::from_millis(4),
        "the drive must actually have waited out the 5ms deadline"
    );
    assert_eq!(willow_sched_task_state(target), -1);
    assert_eq!(global_timers().len(), 0);
    reset_global_scheduler_for_test();
    reset_internal_for_test();
}

#[test]
fn t9ha4_24_cancelling_a_sleeper_clears_its_deadline_and_the_drive_terminates() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
    let id = willow_sched_spawn(poll_long_sleep_then_ready, frame);
    // A short bounded drive gives it its first poll — which parks it on a
    // 2s deadline — and returns without waiting that sleep out.
    assert_eq!(
        willow_sched_run_until_deadline(willow_monotonic_millis() + 30),
        0
    );
    assert_eq!(willow_sched_task_state(id), 2);
    assert_eq!(global_timers().len(), 1);

    willow_sched_cancel(id);

    assert_eq!(
        global_task_table()
            .with(id, |task| task.wake_deadline)
            .flatten(),
        None,
        "cancellation must clear the deadline so the entry is stale"
    );
    let start = Instant::now();
    assert_eq!(willow_sched_run(), 0, "a cancelled task never completes");
    assert!(
        start.elapsed() < Duration::from_millis(1_500),
        "the drive waited out the cancelled 2s sleep: {:?}",
        start.elapsed()
    );
    assert_eq!(willow_sched_task_state(id), -1);
    assert_eq!(global_timers().len(), 0);
    reset_global_scheduler_for_test();
    reset_internal_for_test();
}

#[test]
fn t9ha4_25_drive_deadline_still_wins_over_a_far_off_sleeper() {
    // The idle wait is computed from the timer queue; clamping it to the
    // caller's deadline must survive the move to the separate lock.
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
    let id = willow_sched_spawn(poll_long_sleep_then_ready, frame);

    let start = Instant::now();
    assert_eq!(
        willow_sched_run_until_deadline(willow_monotonic_millis() + 30),
        0
    );
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_millis(1_500),
        "the 2s timer outranked the 30ms drive deadline: {elapsed:?}"
    );
    assert_eq!(willow_sched_task_state(id), 2, "the sleeper stays parked");
    assert_eq!(global_timers().len(), 1, "its timer stays registered");
    reset_global_scheduler_for_test();
    reset_internal_for_test();
}

static T9HA4_COMPLETION_ORDER: TestMutex<Vec<i64>> = TestMutex::new(Vec::new());

/// Slot 0: poll count. Slot 1: tag. Slot 2: sleep duration in ms.
/// Records its tag on completion so a test can assert the order.
unsafe extern "C" fn poll_tagged_sleep_then_ready(frame: *mut c_void) -> i32 {
    let base = frame as *mut u8;
    let polls = unsafe { &mut *(base.add(async_frame_slot_offset(0)) as *mut i64) };
    let tag = unsafe { *(base.add(async_frame_slot_offset(1)) as *const i64) };
    let millis = unsafe { *(base.add(async_frame_slot_offset(2)) as *const i64) };
    *polls += 1;
    if *polls >= 2 {
        T9HA4_COMPLETION_ORDER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(tag);
        RUNTIME_POLL_READY
    } else {
        willow_sched_sleep(millis);
        RUNTIME_POLL_PENDING
    }
}

#[test]
fn t9ha4_26_staggered_sleepers_complete_in_deadline_order() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    T9HA4_COMPLETION_ORDER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();

    // Spawn order deliberately disagrees with deadline order.
    for (tag, millis) in [(1i64, 120i64), (2, 0), (3, 60)] {
        let frame = willow_async_frame_alloc(3, 0) as *mut c_void;
        unsafe {
            let base = frame as *mut u8;
            *(base.add(async_frame_slot_offset(1)) as *mut i64) = tag;
            *(base.add(async_frame_slot_offset(2)) as *mut i64) = millis;
        }
        willow_sched_spawn(poll_tagged_sleep_then_ready, frame);
    }

    assert_eq!(willow_sched_run(), 3);

    assert_eq!(
        *T9HA4_COMPLETION_ORDER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        vec![2, 3, 1],
        "sleepers must resume in deadline order, not spawn order"
    );
    assert_eq!(global_timers().len(), 0);
    reset_global_scheduler_for_test();
    reset_internal_for_test();
}

// ── willow-vynv.1: cancel runtime integrity ─────────────────────────────

#[test]
fn cancel_finalize_wakes_parked_awaiter() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let target = s.spawn_placeholder();
    let awaiter = s.spawn_parked_placeholder();
    s.register_waiter(target, awaiter);

    // Cancel-request the target, then let the claim boundary finalize it.
    assert_eq!(
        s.with_task_mut(target, |task| task.state.request_cancel()),
        Some(CancelOutcome::Deferred)
    );
    // The claim boundary finalizes the target (never claims it), wakes
    // the parked awaiter, and the SAME claim then picks the awaiter up.
    assert_eq!(
        s.claim_ready_for_worker(0),
        Some(awaiter),
        "finalize must wake the awaiter, which is then claimable"
    );
    assert_eq!(s.task_state(target), None, "cancelled task is reaped");
    assert_eq!(s.task_state(awaiter), Some(RuntimeTaskState::Running));
    s.clear_running();
}

#[test]
fn cancel_cleared_deadline_invalidates_stale_timer_entry() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    s.park(id);
    let deadline = Instant::now();
    s.tasks.with_mut(id, |task| {
        task.wake_deadline = Some(deadline);
    });
    s.timers.push(id, deadline);
    // Cancellation clears the deadline (willow_sched_cancel behavior).
    s.tasks.with_mut(id, |task| {
        assert_eq!(task.state.request_cancel(), CancelOutcome::Enqueue);
        task.wake_deadline = None;
    });
    // The wheel's stale entry must be revalidated away, not fire a wake.
    assert_eq!(
        s.wake_due_timers(Instant::now() + std::time::Duration::from_secs(1)),
        0,
        "stale timer entry for a cancelled task must not fire"
    );
    assert_eq!(s.timers.len(), 0, "the stale entry is dropped, not kept");
}

#[test]
fn wake_is_a_noop_on_cancelled_tasks() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    assert_eq!(s.claim_ready_for_worker(0), Some(id));
    s.finalize_cancelled(id);
    s.wake(id);
    assert_eq!(
        s.task_state(id),
        None,
        "wake must not resurrect a reaped task"
    );
    assert_eq!(s.claim_ready_for_worker(0), None);
}

// -----------------------------------------------------------------------
// Deadline-bounded scheduler drive + task-waiter reverse references
// (willow-o038 review). Perspectives: an absolute deadline becomes a real
// timer; the drive returns at that deadline instead of draining a far-off
// task; an already-past deadline returns immediately; the placeholder never
// outlives the call; registration records both directions; unregister,
// completion, and cancellation all clear both directions.
// -----------------------------------------------------------------------

/// Parks with a long sleep on the first poll, completes on the second.
unsafe extern "C" fn poll_long_sleep_then_ready(frame: *mut c_void) -> i32 {
    let state = unsafe { &mut *(frame as *mut i64) };
    *state += 1;
    if *state >= 2 {
        RUNTIME_POLL_READY
    } else {
        willow_sched_sleep(2_000);
        RUNTIME_POLL_PENDING
    }
}

#[test]
fn deadline_01_empty_scheduler_returns_immediately() {
    // The deadline is a CEILING, not a sleep: with nothing to run the drive
    // must return at once so the caller can decide how to wait.
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    let start = Instant::now();
    assert_eq!(
        willow_sched_run_until_deadline(willow_monotonic_millis() + 5_000),
        0
    );
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "an idle scheduler must not sleep out the deadline"
    );
    reset_global_scheduler_for_test();
    reset_internal_for_test();
}

#[test]
fn deadline_02_ready_work_still_runs_before_the_deadline() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    let id = willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    assert_eq!(
        willow_sched_run_until_deadline(willow_monotonic_millis() + 5_000),
        1,
        "a runnable task must still be driven"
    );
    assert_eq!(willow_sched_task_state(id), -1); // terminal record reaped
    reset_global_scheduler_for_test();
    reset_internal_for_test();
}

#[test]
fn deadline_03_run_returns_at_deadline_not_after_long_task() {
    // The bug this guards: an unbounded `willow_sched_run` inside sync
    // select drains a 2s task before ever re-checking a 30ms timeout.
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
    let long = willow_sched_spawn(poll_long_sleep_then_ready, frame);
    let start = Instant::now();
    willow_sched_run_until_deadline(willow_monotonic_millis() + 30);
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(20),
        "drive returned before the deadline: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(1_500),
        "drive waited for the 2s task instead of the 30ms deadline: {elapsed:?}"
    );
    assert_eq!(
        willow_sched_task_state(long),
        2,
        "the long task must remain parked, not be reaped as completed"
    );
    reset_global_scheduler_for_test();
    reset_internal_for_test();
}

#[test]
fn deadline_04_past_deadline_returns_promptly() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
    willow_sched_spawn(poll_long_sleep_then_ready, frame);
    let start = Instant::now();
    willow_sched_run_until_deadline(willow_monotonic_millis() - 1);
    assert!(
        start.elapsed() < Duration::from_millis(1_500),
        "an already-expired deadline must not block on unrelated tasks"
    );
    reset_global_scheduler_for_test();
    reset_internal_for_test();
}

#[test]
fn deadline_05_bounded_drive_leaves_no_scheduler_state_behind() {
    // The bound is caller-local: it must not register timers or tasks that
    // a later unbounded drive would then wait on.
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    willow_sched_run_until_deadline(willow_monotonic_millis() + 10);
    assert!(
        with_global(|sched| sched.next_timer_deadline()).is_none(),
        "a bounded drive must leave no timer behind"
    );
    let start = Instant::now();
    assert_eq!(willow_sched_run(), 0);
    assert!(
        start.elapsed() < Duration::from_millis(200),
        "an empty scheduler must go idle immediately after a bounded drive"
    );
    reset_global_scheduler_for_test();
    reset_internal_for_test();
}

#[test]
fn deadline_06_parked_task_timer_still_fires_before_a_far_deadline() {
    // Clamping idle waits must not SKIP a nearer task timer: a 5ms sleeper
    // still runs to completion under a 5s drive deadline.
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    reset_global_scheduler_for_test();
    let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
    let id = willow_sched_spawn(poll_sleep_then_ready, frame);
    assert_eq!(
        willow_sched_run_until_deadline(willow_monotonic_millis() + 5_000),
        1
    );
    assert_eq!(willow_sched_task_state(id), -1); // terminal record reaped
    reset_global_scheduler_for_test();
    reset_internal_for_test();
}

#[test]
fn waiter_reverse_01_register_records_both_directions() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let awaitee = s.spawn_parked_placeholder();
    let waiter = s.spawn_parked_placeholder();
    s.register_waiter(awaitee, waiter);
    assert_eq!(
        s.with_task(awaitee, |task| task.live_waiters()).unwrap(),
        vec![waiter]
    );
    assert!(
        s.with_task(waiter, |task| task.is_awaiting(awaitee))
            .unwrap()
    );
}

#[test]
fn sharded_relationships_take_opposite_requests_in_one_lock_order() {
    let tasks = Arc::new(ShardedTaskTable::new());
    // 1 and 2 are in different shards. Each thread presents the pair in
    // the opposite logical order; `with_two_mut` must still acquire shard
    // 1 before shard 2, so the barrier-pinned race cannot deadlock.
    tasks.insert(1, RuntimeTask::new(1));
    tasks.insert(2, RuntimeTask::new(2));
    let start = Arc::new(Barrier::new(3));
    std::thread::scope(|scope| {
        for (awaitee, waiter) in [(1, 2), (2, 1)] {
            let tasks = Arc::clone(&tasks);
            let start = Arc::clone(&start);
            scope.spawn(move || {
                start.wait();
                for _ in 0..2_000 {
                    assert!(register_waiter_sharded(&tasks, awaitee, waiter));
                    unregister_waiter_sharded(&tasks, awaitee, waiter);
                }
            });
        }
        start.wait();
    });
    for id in [1, 2] {
        tasks
            .with(id, |task| {
                assert_eq!(task.waiter_count(), 0);
                assert_eq!(task.awaiting_count(), 0);
            })
            .unwrap();
    }
}

#[test]
fn sharded_same_shard_callback_panic_keeps_both_task_records() {
    let tasks = ShardedTaskTable::new();
    let first = 1;
    let second = first + TASK_TABLE_SHARDS as u64;
    assert_eq!(tasks.shard_index(first), tasks.shard_index(second));
    tasks.insert(first, RuntimeTask::new(first));
    tasks.insert(second, RuntimeTask::new(second));

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tasks.with_two_mut(first, second, |first, second| -> () {
            assert!(first.is_some());
            assert!(second.is_some());
            panic!("pinned callback panic");
        });
    }));

    assert!(panic.is_err());
    assert!(tasks.with(first, |_| ()).is_some());
    assert!(tasks.with(second, |_| ()).is_some());
    assert_eq!(tasks.len(), 2);
}

#[test]
fn waiter_reverse_02_register_is_idempotent() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let awaitee = s.spawn_parked_placeholder();
    let waiter = s.spawn_parked_placeholder();
    s.register_waiter(awaitee, waiter);
    s.register_waiter(awaitee, waiter);
    assert_eq!(s.with_task(awaitee, |task| task.waiter_count()).unwrap(), 1);
    assert_eq!(
        s.with_task(waiter, |task| task.awaiting_count()).unwrap(),
        1
    );
}

#[test]
fn waiter_reverse_03_unregister_clears_both_directions() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let awaitee = s.spawn_parked_placeholder();
    let waiter = s.spawn_parked_placeholder();
    s.register_waiter(awaitee, waiter);
    s.unregister_waiter(awaitee, waiter);
    assert!(
        s.with_task(awaitee, |task| task.live_waiters().is_empty())
            .unwrap()
    );
    assert!(s.with_task(waiter, |task| task.awaiting_count()).unwrap() == 0);
}

#[test]
fn waiter_reverse_04_cancel_purges_the_waiter_registration() {
    // A cancelled select task waiter must not stay in its awaitee's list:
    // the awaitee would otherwise try to wake a dead task on completion.
    let mut s = RuntimeScheduler::with_worker_count(1);
    let awaitee = s.spawn_parked_placeholder();
    let waiter = s.spawn_parked_placeholder();
    s.register_waiter(awaitee, waiter);
    s.finalize_cancelled(waiter);
    assert!(
        s.with_task(awaitee, |task| task.live_waiters().is_empty())
            .unwrap(),
        "cancellation must deregister the task-completion waiter"
    );
    assert!(
        s.with_task(waiter, |_| ()).is_none(),
        "cancelled waiter must be reaped"
    );
}

#[test]
fn waiter_reverse_05_completion_clears_the_reverse_reference() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let awaitee = s.spawn_parked_placeholder();
    let waiter = s.spawn_parked_placeholder();
    s.register_waiter(awaitee, waiter);
    s.complete(awaitee);
    assert!(
        s.with_task(waiter, |task| task.awaiting_count()).unwrap() == 0,
        "a completed awaitee leaves no reverse reference behind"
    );
    assert_eq!(s.task_state(waiter), Some(RuntimeTaskState::Ready));
}

#[test]
fn waiter_reverse_06_multiple_awaitees_all_purged_on_cancel() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let a = s.spawn_parked_placeholder();
    let b = s.spawn_parked_placeholder();
    let waiter = s.spawn_parked_placeholder();
    s.register_waiter(a, waiter);
    s.register_waiter(b, waiter);
    assert_eq!(
        s.with_task(waiter, |task| task.awaiting_count()).unwrap(),
        2
    );
    s.finalize_cancelled(waiter);
    assert!(
        s.with_task(a, |task| task.live_waiters().is_empty())
            .unwrap()
    );
    assert!(
        s.with_task(b, |task| task.live_waiters().is_empty())
            .unwrap()
    );
}

#[test]
fn waiter_reverse_07_unknown_awaitee_records_nothing() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let waiter = s.spawn_parked_placeholder();
    s.register_waiter(9_999, waiter);
    assert!(
        s.with_task(waiter, |task| task.awaiting_count()).unwrap() == 0,
        "no reverse reference for a registration that did not happen"
    );
}

// -------------------------------------------------------------------------
// O(1) completion-waiter relationships for 10k fan-in (willow-ezs.2).
//
// `waiters` is a FIFO `WaitQueue` with a membership map and `awaiting` is a
// set, so neither registration nor cancellation rescans a vector. These
// tests pin the observable contract of that change; the queue's own
// structural behavior is covered in `wait_queue`, and scaling cost is
// measured by the scheduler benchmark rather than asserted here.
//
// Perspectives:
//   FI1  10,000 distinct waiters on one awaitee all register, in order
//   FI2  re-registering all 10,000 creates no second relation
//   FI3  a duplicate registration does not add a reverse reference either
//   FI4  unregister then re-register moves the waiter to the FIFO tail
//   FI5  completion wakes every live waiter exactly once
//   FI6  completion clears both directions for all 10,000
//   FI7  cancelling half of 10,000 waiters leaves exactly 5,000 to wake
//   FI8  cancelled waiters are gone, not merely skipped
//   FI9  repeated losing select arms cannot grow the waiter queue
//   FI10 repeated losing select arms cannot grow the reverse references
//   FI11 one waiter on 1,000 awaitees is purged from all of them on cancel
//   FI12 waiters are woken in registration order
//   FI13 a waiter reaped before the wake is skipped without disturbing others
//   FI14 unregistering a waiter that was never registered is a no-op
//   FI15 10k fan-in returns the scheduler to its metadata baseline
// -------------------------------------------------------------------------

const FAN_IN: usize = 10_000;

/// FI1, FI2, FI3.
#[test]
fn fanin_01_ten_thousand_waiters_register_once_each() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let awaitee = s.spawn_parked_placeholder();
    let waiters: Vec<RuntimeTaskId> = (0..FAN_IN).map(|_| s.spawn_parked_placeholder()).collect();
    for &waiter in &waiters {
        s.register_waiter(awaitee, waiter);
    }
    assert_eq!(
        s.with_task(awaitee, |task| task.live_waiters()).unwrap(),
        waiters
    );

    for &waiter in &waiters {
        s.register_waiter(awaitee, waiter);
    }
    assert_eq!(
        s.with_task(awaitee, |task| task.waiter_count()).unwrap(),
        FAN_IN
    );
    assert_eq!(
        s.with_task(awaitee, |task| task.queued_waiter_entries())
            .unwrap(),
        FAN_IN
    );
    for &waiter in &waiters {
        assert_eq!(
            s.with_task(waiter, |task| task.awaiting_count()).unwrap(),
            1,
            "a duplicate registration must not add a second reverse reference"
        );
    }
}

/// FI4.
#[test]
fn fanin_02_reregistration_moves_the_waiter_to_the_tail() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let awaitee = s.spawn_parked_placeholder();
    let first = s.spawn_parked_placeholder();
    let second = s.spawn_parked_placeholder();
    let third = s.spawn_parked_placeholder();
    for waiter in [first, second, third] {
        s.register_waiter(awaitee, waiter);
    }
    s.unregister_waiter(awaitee, first);
    s.register_waiter(awaitee, first);
    assert_eq!(
        s.with_task(awaitee, |task| task.live_waiters()).unwrap(),
        vec![second, third, first]
    );
    assert!(
        s.with_task(first, |task| task.is_awaiting(awaitee))
            .unwrap()
    );
}

/// FI5, FI6.
#[test]
fn fanin_03_completion_wakes_every_waiter_exactly_once() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let awaitee = s.spawn_parked_placeholder();
    let waiters: Vec<RuntimeTaskId> = (0..FAN_IN).map(|_| s.spawn_parked_placeholder()).collect();
    for &waiter in &waiters {
        s.register_waiter(awaitee, waiter);
    }

    s.complete(awaitee);

    let mut claimed = Vec::with_capacity(FAN_IN);
    while let Some(id) = s.claim_ready_for_worker(0) {
        claimed.push(id);
        s.clear_running();
    }
    assert_eq!(
        claimed.len(),
        FAN_IN,
        "every waiter must be woken exactly once"
    );
    for &waiter in &waiters {
        assert!(
            s.with_task(waiter, |task| task.awaiting_count()).unwrap() == 0,
            "the reverse reference must be cleared for every waiter"
        );
    }
    assert!(
        s.with_task(awaitee, |_| ()).is_none(),
        "the awaitee is reaped"
    );
}

/// FI7, FI8.
#[test]
fn fanin_04_cancelling_half_leaves_exactly_half_to_wake() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let awaitee = s.spawn_parked_placeholder();
    let waiters: Vec<RuntimeTaskId> = (0..FAN_IN).map(|_| s.spawn_parked_placeholder()).collect();
    for &waiter in &waiters {
        s.register_waiter(awaitee, waiter);
    }
    for (index, &waiter) in waiters.iter().enumerate() {
        if index % 2 == 0 {
            s.finalize_cancelled(waiter);
        }
    }
    assert_eq!(
        s.with_task(awaitee, |task| task.waiter_count()).unwrap(),
        FAN_IN / 2
    );

    s.complete(awaitee);
    let mut woken = 0;
    while let Some(_id) = s.claim_ready_for_worker(0) {
        woken += 1;
        s.clear_running();
    }
    assert_eq!(woken, FAN_IN / 2);
    for (index, &waiter) in waiters.iter().enumerate() {
        if index % 2 == 0 {
            assert!(
                s.with_task(waiter, |_| ()).is_none(),
                "cancelled waiters are reaped"
            );
        }
    }
}

/// FI9, FI10.
#[test]
fn fanin_05_losing_select_arms_do_not_grow_the_relationship_tables() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let awaitee = s.spawn_parked_placeholder();
    let resident = s.spawn_parked_placeholder();
    let churner = s.spawn_parked_placeholder();
    s.register_waiter(awaitee, resident);

    // A select arm that loses unregisters and parks again next iteration.
    for _ in 0..FAN_IN {
        s.register_waiter(awaitee, churner);
        s.unregister_waiter(awaitee, churner);
    }

    s.with_task(awaitee, |awaitee_task| {
        assert_eq!(awaitee_task.waiter_count(), 1);
        assert_eq!(awaitee_task.live_waiters(), vec![resident]);
        assert!(
            awaitee_task.queued_waiter_entries() <= 16,
            "tombstones from losing arms must be compacted, saw {}",
            awaitee_task.queued_waiter_entries()
        );
    })
    .unwrap();
    assert!(
        s.with_task(churner, |task| task.awaiting_count()).unwrap() == 0,
        "a losing arm must leave no reverse reference"
    );
}

/// FI11.
#[test]
fn fanin_06_one_waiter_on_many_awaitees_is_purged_from_all() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let waiter = s.spawn_parked_placeholder();
    let awaitees: Vec<RuntimeTaskId> = (0..1_000).map(|_| s.spawn_parked_placeholder()).collect();
    for &awaitee in &awaitees {
        s.register_waiter(awaitee, waiter);
    }
    assert_eq!(
        s.with_task(waiter, |task| task.awaiting_count()).unwrap(),
        awaitees.len()
    );

    s.finalize_cancelled(waiter);
    for &awaitee in &awaitees {
        assert!(
            s.with_task(awaitee, |task| task.live_waiters().is_empty())
                .unwrap(),
            "cancellation must deregister from every awaitee"
        );
    }
}

/// FI12.
#[test]
fn fanin_07_waiters_are_woken_in_registration_order() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let awaitee = s.spawn_parked_placeholder();
    let waiters: Vec<RuntimeTaskId> = (0..8).map(|_| s.spawn_parked_placeholder()).collect();
    for &waiter in &waiters {
        s.register_waiter(awaitee, waiter);
    }
    s.complete(awaitee);

    let mut claimed = Vec::new();
    while let Some(id) = s.claim_ready_for_worker(0) {
        claimed.push(id);
        s.clear_running();
    }
    assert_eq!(claimed, waiters, "completion wakes waiters FIFO");
}

/// FI13, FI14.
#[test]
fn fanin_08_stale_and_unknown_waiters_are_handled_quietly() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let awaitee = s.spawn_parked_placeholder();
    let live = s.spawn_parked_placeholder();
    s.register_waiter(awaitee, live);

    // Never registered: unregistering must not disturb the live relation.
    s.unregister_waiter(awaitee, 4_242);
    assert_eq!(
        s.with_task(awaitee, |task| task.live_waiters()).unwrap(),
        vec![live]
    );

    // Registered then reaped behind the awaitee's back.
    let reaped = s.spawn_parked_placeholder();
    s.register_waiter(awaitee, reaped);
    s.tasks.remove(reaped);

    s.complete(awaitee);
    assert_eq!(s.task_state(live), Some(RuntimeTaskState::Ready));
}

/// FI15.
#[test]
fn fanin_09_ten_thousand_fan_in_returns_to_the_metadata_baseline() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let awaitee = s.spawn_parked_placeholder();
    let waiters: Vec<RuntimeTaskId> = (0..FAN_IN).map(|_| s.spawn_parked_placeholder()).collect();
    for &waiter in &waiters {
        s.register_waiter(awaitee, waiter);
    }
    s.complete(awaitee);
    while let Some(id) = s.claim_ready_for_worker(0) {
        s.complete(id);
        s.clear_running();
    }
    assert_eq!(
        s.metadata_snapshot(),
        SchedulerMetadataSnapshot {
            heavy_tasks: 0,
            queue_entries: 0,
            pending_cleanups: 0,
            frame_roots: 0,
            blocked_syscalls: 0,
        }
    );
    assert!(s.take_pending_terminal_cleanups().is_empty());
    assert_eq!(s.metadata_snapshot(), empty_metadata());
}

// -------------------------------------------------------------------------
// Frame-backed terminal status (willow-ezs.1.3).
//
// The scheduler publishes a task's terminal status into its async frame's
// header, and the language-visible queries (`await task`,
// `await task.result()`, `is_cancelled`) read it from there. Perspectives
// covered here:
//
//   FST1  the state -> status mapping is exactly the three terminal states
//   FST2  `complete` publishes Completed
//   FST3  `finalize_cancelled` publishes Cancelled
//   FST4  a Panicked transition publishes Panicked
//   FST5  non-terminal transitions leave the frame Pending
//   FST6  publication happens once: a later transition cannot rewrite it
//   FST7  a task with a null frame completes without touching memory
//   FST8  `willow_sched_cancel` mirrors the request bit into the frame
//   FST9  cancel + finalize leaves BOTH the request bit and Cancelled
//   FST10 cancelling an already-completed task does not disturb its status
//   FST11 `willow_frame_await` reports done from a terminal frame even
//         when the scheduler no longer knows the id (post-reaping contract)
//   FST12 `willow_frame_await` on a pending frame still registers a waiter
//   FST13 `willow_frame_await` with a null frame falls back to the id path
//   FST14 `willow_frame_await_check` is a no-op for pending/completed frames
//   FST15 `willow_sched_task_state` stays the id-only diagnostic: it goes
//         unknown for a reaped id while the frame still answers
//   FST16 an await slow path cannot observe Terminal before frame status
//         publication, even when paused at that exact transition
// -------------------------------------------------------------------------

/// A stand-alone async frame for status tests (8-byte aligned words; the
/// scheduler only ever touches header word 2).
use crate::async_frame::willow_frame_is_cancelled;

fn status_frame() -> Box<[i64; 8]> {
    Box::new([0; 8])
}

/// Spawn a ready placeholder that carries `frame`, as a real spawn would.
fn spawn_with_frame(s: &mut RuntimeScheduler, frame: &mut [i64; 8]) -> RuntimeTaskId {
    let id = s.spawn_placeholder();
    s.with_task_mut(id, |task| {
        task.frame = frame.as_mut_ptr() as *mut c_void;
    });
    id
}

fn frame_ptr(frame: &mut [i64; 8]) -> *mut c_void {
    frame.as_mut_ptr() as *mut c_void
}

fn terminal_of(frame: &mut [i64; 8]) -> i64 {
    crate::async_frame::frame_terminal_status(frame_ptr(frame))
}

#[test]
fn fst_01_only_terminal_states_map_to_a_status() {
    use crate::async_frame::{
        WILLOW_FRAME_STATUS_CANCELLED, WILLOW_FRAME_STATUS_COMPLETED, WILLOW_FRAME_STATUS_PANICKED,
    };
    assert_eq!(
        terminal_frame_status(RuntimeTaskState::Completed),
        Some(WILLOW_FRAME_STATUS_COMPLETED)
    );
    assert_eq!(
        terminal_frame_status(RuntimeTaskState::Cancelled),
        Some(WILLOW_FRAME_STATUS_CANCELLED)
    );
    assert_eq!(
        terminal_frame_status(RuntimeTaskState::Panicked),
        Some(WILLOW_FRAME_STATUS_PANICKED)
    );
    for state in [
        RuntimeTaskState::Ready,
        RuntimeTaskState::Running,
        RuntimeTaskState::Parked,
        RuntimeTaskState::Cancelling,
        RuntimeTaskState::BlockedSyscall,
    ] {
        assert_eq!(
            terminal_frame_status(state),
            None,
            "{state:?} is still runnable and must not publish a terminal status"
        );
    }
}

#[test]
fn fst_02_complete_publishes_completed() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let mut frame = status_frame();
    let id = spawn_with_frame(&mut s, &mut frame);
    assert_eq!(terminal_of(&mut frame), 0);
    s.complete(id);
    assert_eq!(
        terminal_of(&mut frame),
        crate::async_frame::WILLOW_FRAME_STATUS_COMPLETED
    );
}

#[test]
fn fst_03_finalize_cancelled_publishes_cancelled() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let mut frame = status_frame();
    let id = spawn_with_frame(&mut s, &mut frame);
    s.finalize_cancelled(id);
    assert_eq!(
        terminal_of(&mut frame),
        crate::async_frame::WILLOW_FRAME_STATUS_CANCELLED
    );
}

#[test]
fn fst_04_panicked_transition_publishes_panicked() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let mut frame = status_frame();
    let id = spawn_with_frame(&mut s, &mut frame);
    s.finalize_panicked(id);
    assert_eq!(
        terminal_of(&mut frame),
        crate::async_frame::WILLOW_FRAME_STATUS_PANICKED
    );
    assert_eq!(
        s.task_state(id),
        None,
        "the panicked task's heavy metadata must be reaped"
    );
}

#[test]
fn fst_05_non_terminal_transitions_stay_pending() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let mut frame = status_frame();
    let id = spawn_with_frame(&mut s, &mut frame);
    s.set_running(id);
    s.finish_pending_poll(id);
    s.wake(id);
    s.set_running(id);
    s.clear_running();
    assert_eq!(
        terminal_of(&mut frame),
        crate::async_frame::WILLOW_FRAME_STATUS_PENDING,
        "run/park/wake churn must not look like a finished task"
    );
}

#[test]
fn fst_06_first_terminal_status_wins() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let mut frame = status_frame();
    let id = spawn_with_frame(&mut s, &mut frame);
    s.complete(id);
    // A late cancellation path must not rewrite a published result.
    s.finalize_cancelled(id);
    assert_eq!(
        terminal_of(&mut frame),
        crate::async_frame::WILLOW_FRAME_STATUS_COMPLETED
    );
}

#[test]
fn fst_07_null_frame_task_completes_safely() {
    let mut s = RuntimeScheduler::with_worker_count(1);
    let id = s.spawn_placeholder();
    assert!(s.with_task(id, |task| task.frame.is_null()).unwrap());
    s.complete(id);
    assert_eq!(s.task_state(id), None);
}

#[test]
fn fst_08_cancel_request_is_mirrored_into_the_frame() {
    let _guard = crate::gc::runtime_test_guard();
    reset_global_scheduler_for_test();
    let mut frame = status_frame();
    let id = with_global_for_test(|s| {
        let id = s.spawn_placeholder();
        s.with_task_mut(id, |task| {
            task.frame = frame.as_mut_ptr() as *mut c_void;
        });
        s.park(id);
        id
    });
    willow_sched_cancel(id);
    assert_eq!(
        willow_frame_is_cancelled(frame_ptr(&mut frame)),
        1,
        "is_cancelled() must answer from the frame as soon as cancel is requested"
    );
    assert_eq!(
        terminal_of(&mut frame),
        crate::async_frame::WILLOW_FRAME_STATUS_PENDING,
        "a requested cancel is not yet a finished task"
    );
    reset_global_scheduler_for_test();
}

#[test]
fn fst_09_cancel_then_finalize_keeps_both_bits() {
    let _guard = crate::gc::runtime_test_guard();
    reset_global_scheduler_for_test();
    let mut frame = status_frame();
    let id = with_global_for_test(|s| {
        let id = s.spawn_placeholder();
        s.with_task_mut(id, |task| {
            task.frame = frame.as_mut_ptr() as *mut c_void;
        });
        s.park(id);
        id
    });
    willow_sched_cancel(id);
    with_global_for_test(|s| s.finalize_cancelled(id));
    let status = crate::async_frame::frame_status(frame_ptr(&mut frame));
    assert_eq!(
        status & crate::async_frame::WILLOW_FRAME_STATUS_TERMINAL_MASK,
        crate::async_frame::WILLOW_FRAME_STATUS_CANCELLED
    );
    assert_ne!(
        status & crate::async_frame::WILLOW_FRAME_STATUS_CANCEL_REQUESTED,
        0
    );
    reset_global_scheduler_for_test();
}

#[test]
fn fst_10_cancelling_a_completed_task_keeps_completed() {
    let _guard = crate::gc::runtime_test_guard();
    reset_global_scheduler_for_test();
    let mut frame = status_frame();
    let id = with_global_for_test(|s| {
        let id = s.spawn_placeholder();
        s.with_task_mut(id, |task| {
            task.frame = frame.as_mut_ptr() as *mut c_void;
        });
        s.complete(id);
        id
    });
    willow_sched_cancel(id);
    assert_eq!(
        terminal_of(&mut frame),
        crate::async_frame::WILLOW_FRAME_STATUS_COMPLETED
    );
    assert_eq!(
        willow_frame_is_cancelled(frame_ptr(&mut frame)),
        0,
        "cancelling a task that already finished must not make it look cancelled"
    );
    reset_global_scheduler_for_test();
}

#[test]
fn fst_11_await_of_a_terminal_frame_needs_no_task_record() {
    let _guard = crate::gc::runtime_test_guard();
    reset_global_scheduler_for_test();
    let mut frame = status_frame();
    crate::async_frame::frame_publish_terminal(
        frame_ptr(&mut frame),
        crate::async_frame::WILLOW_FRAME_STATUS_COMPLETED,
    );
    // Id 9_999 was never spawned here: this is the shape a reaped task
    // leaves behind, and the frame alone must answer "done".
    assert_eq!(willow_frame_await(frame_ptr(&mut frame), 9_999), 1);
    reset_global_scheduler_for_test();
}

#[test]
fn fst_12_await_of_a_pending_frame_registers_a_waiter() {
    let _guard = crate::gc::runtime_test_guard();
    reset_global_scheduler_for_test();
    let mut frame = status_frame();
    let (awaitee, waiter) = with_global_for_test(|s| {
        let awaitee = s.spawn_placeholder();
        s.with_task_mut(awaitee, |task| {
            task.frame = frame.as_mut_ptr() as *mut c_void;
        });
        let waiter = s.spawn_parked_placeholder();
        (awaitee, waiter)
    });
    let done = with_current_task_for_test(waiter, || {
        willow_frame_await(frame_ptr(&mut frame), awaitee)
    });
    assert_eq!(done, 0, "a pending frame must fall through to registration");
    with_global_for_test(|s| {
        assert_eq!(
            s.with_task(awaitee, |task| task.live_waiters()).unwrap(),
            vec![waiter]
        );
    });
    // Completing the awaitee both wakes the waiter and publishes the status.
    with_global_for_test(|s| s.complete(awaitee));
    assert_eq!(
        terminal_of(&mut frame),
        crate::async_frame::WILLOW_FRAME_STATUS_COMPLETED
    );
    assert_eq!(willow_frame_await(frame_ptr(&mut frame), awaitee), 1);
    reset_global_scheduler_for_test();
}

#[test]
fn frame_await_repolling_does_not_duplicate_wait_relationships() {
    let _guard = crate::gc::runtime_test_guard();
    for waiters in [1, 16, 64] {
        for polls in [1, 16, 256] {
            reset_global_scheduler_for_test();
            let mut frame = status_frame();
            let (awaitee, ids) = with_global_for_test(|s| {
                let awaitee = s.spawn_placeholder();
                s.with_task_mut(awaitee, |task| task.frame = frame_ptr(&mut frame));
                let ids: Vec<_> = (0..waiters).map(|_| s.spawn_parked_placeholder()).collect();
                (awaitee, ids)
            });
            for _ in 0..polls {
                for &id in &ids {
                    let ready = with_current_task_for_test(id, || {
                        willow_frame_await(frame_ptr(&mut frame), awaitee)
                    });
                    assert_eq!(ready, 0);
                }
            }
            with_global_for_test(|s| {
                assert_eq!(
                    s.with_task(awaitee, |task| task.live_waiters()).unwrap(),
                    ids
                );
                for &id in &ids {
                    assert_eq!(s.with_task(id, |task| task.awaiting_count()), Some(1));
                }
                s.complete(awaitee);
                for &id in &ids {
                    assert_eq!(s.with_task(id, |task| task.awaiting_count()), Some(0));
                }
            });
            assert_eq!(willow_frame_await(frame_ptr(&mut frame), awaitee), 1);
            println!(
                "waiters={waiters} polls={polls} checks={} relationships={waiters}",
                waiters * polls
            );
        }
    }
    reset_global_scheduler_for_test();
}

#[test]
fn fst_13_null_frame_await_falls_back_to_the_id_path() {
    let _guard = crate::gc::runtime_test_guard();
    reset_global_scheduler_for_test();
    // Unknown id on the id path is treated as ready, so no permanent park.
    assert_eq!(willow_frame_await(std::ptr::null_mut(), 9_999), 1);
    let id = with_global_for_test(|s| s.spawn_parked_placeholder());
    let waiter = with_global_for_test(|s| s.spawn_parked_placeholder());
    let done = with_current_task_for_test(waiter, || willow_frame_await(std::ptr::null_mut(), id));
    assert_eq!(done, 0);
    with_global_for_test(|s| {
        assert_eq!(
            s.with_task(id, |task| task.live_waiters()).unwrap(),
            vec![waiter]
        )
    });
    reset_global_scheduler_for_test();
}

#[test]
fn fst_14_await_check_passes_for_non_cancelled_frames() {
    let _guard = crate::gc::runtime_test_guard();
    reset_global_scheduler_for_test();
    // Pending, Completed and Panicked frames must all return normally; only
    // a Cancelled frame aborts (an abort cannot be exercised in-process).
    let mut pending = status_frame();
    willow_frame_await_check(frame_ptr(&mut pending), 1);
    let mut completed = status_frame();
    crate::async_frame::frame_publish_terminal(
        frame_ptr(&mut completed),
        crate::async_frame::WILLOW_FRAME_STATUS_COMPLETED,
    );
    willow_frame_await_check(frame_ptr(&mut completed), 2);
    let mut panicked = status_frame();
    crate::async_frame::frame_publish_terminal(
        frame_ptr(&mut panicked),
        crate::async_frame::WILLOW_FRAME_STATUS_PANICKED,
    );
    willow_frame_await_check(frame_ptr(&mut panicked), 3);
    // A cancel REQUEST that never finalized is not a cancelled await either.
    let mut requested = status_frame();
    crate::async_frame::frame_request_cancel(frame_ptr(&mut requested));
    willow_frame_await_check(frame_ptr(&mut requested), 4);
    reset_global_scheduler_for_test();
}

#[test]
fn fst_15_task_state_stays_the_id_only_diagnostic() {
    let _guard = crate::gc::runtime_test_guard();
    reset_global_scheduler_for_test();
    let mut frame = status_frame();
    let id = with_global_for_test(|s| {
        let id = s.spawn_placeholder();
        s.with_task_mut(id, |task| {
            task.frame = frame.as_mut_ptr() as *mut c_void;
        });
        s.complete(id);
        id
    });
    assert_eq!(
        willow_sched_task_state(id),
        -1,
        "the id-only diagnostic becomes Unknown immediately after reaping"
    );
    assert_eq!(
        crate::async_frame::frame_terminal_status(frame_ptr(&mut frame)),
        crate::async_frame::WILLOW_FRAME_STATUS_COMPLETED
    );
    assert_eq!(willow_frame_await(frame_ptr(&mut frame), id), 1);
    reset_global_scheduler_for_test();
}

#[test]
fn fst_16_terminal_transition_holds_shard_through_frame_publication() {
    let mut scheduler = RuntimeScheduler::with_worker_count(1);
    let mut frame = status_frame();
    let awaitee = spawn_with_frame(&mut scheduler, &mut frame);
    scheduler.set_running(awaitee);
    let waiter = scheduler.spawn_parked_placeholder();
    let tasks = Arc::clone(&scheduler.tasks);

    let (transitioned_tx, transitioned_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let publisher_tasks = Arc::clone(&tasks);
    let publisher = std::thread::spawn(move || {
        publisher_tasks
            .finish_terminal_and_publish(
                awaitee,
                crate::async_frame::WILLOW_FRAME_STATUS_CANCELLED,
                || {
                    transitioned_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                },
            )
            .unwrap()
    });

    transitioned_rx.recv().unwrap();
    assert_eq!(
        terminal_of(&mut frame),
        crate::async_frame::WILLOW_FRAME_STATUS_PENDING,
        "the barrier must hold the historical state/status race point"
    );

    let (attempted_tx, attempted_rx) = std::sync::mpsc::channel();
    let (registered_tx, registered_rx) = std::sync::mpsc::channel();
    let observer_tasks = Arc::clone(&tasks);
    let observer = std::thread::spawn(move || {
        attempted_tx.send(()).unwrap();
        registered_tx
            .send(register_waiter_sharded(&observer_tasks, awaitee, waiter))
            .unwrap();
    });
    attempted_rx.recv().unwrap();
    assert!(
        registered_rx
            .recv_timeout(Duration::from_millis(20))
            .is_err(),
        "the await slow path must remain behind the terminal publisher's shard lock"
    );

    release_tx.send(()).unwrap();
    assert!(publisher.join().unwrap());
    assert!(
        !registered_rx.recv().unwrap(),
        "after publication, Terminal must refuse waiter registration"
    );
    observer.join().unwrap();
    assert_eq!(
        terminal_of(&mut frame),
        crate::async_frame::WILLOW_FRAME_STATUS_CANCELLED
    );
}

// ---------------------------------------------------------------------
// Bounded scheduler metadata under 10,000-Task workloads
// (willow-ezs.1.4/.1.5).
//
// These are deterministic ownership assertions, not RSS benchmarks. Each
// global workload uses the production scheduler entry point, whose default
// and worker count follows the runtime configuration. The blocked-syscall
// workload drives a local five-worker scheduler because real jobs would make 10,000
// native syscalls the subject of the test instead of scheduler metadata.
// ---------------------------------------------------------------------

const STRESS_TASKS: usize = 10_000;

fn empty_metadata() -> SchedulerMetadataSnapshot {
    SchedulerMetadataSnapshot {
        heavy_tasks: 0,
        queue_entries: 0,
        pending_cleanups: 0,
        frame_roots: 0,
        blocked_syscalls: 0,
    }
}

fn assert_global_metadata_reaped(workload: &str) {
    let snapshot = with_global_for_test(|scheduler| scheduler.metadata_snapshot());
    assert_eq!(
        snapshot,
        empty_metadata(),
        "{workload}: scheduler-owned metadata must return to its baseline"
    );
    assert_eq!(willow_sched_heavy_task_count(), 0);
    assert_eq!(willow_sched_queue_entry_count(), 0);
    assert_eq!(willow_sched_pending_cleanup_count(), 0);
    assert_eq!(willow_sched_frame_root_count(), 0);
}

fn reset_stress_fixture() {
    reset_global_scheduler_for_test();
    reset_internal_for_test();
    assert!(runtime_worker_config().active_workers() >= 1);
}

unsafe extern "C" fn poll_zero_sleep_then_ready(frame: *mut c_void) -> i32 {
    let state = unsafe { &mut *((frame as *mut u8).add(async_frame_slot_offset(0)) as *mut i64) };
    *state += 1;
    if *state >= 2 {
        RUNTIME_POLL_READY
    } else {
        willow_sched_sleep(0);
        RUNTIME_POLL_PENDING
    }
}

unsafe extern "C" fn poll_yield_preempt_churn(frame: *mut c_void) -> i32 {
    let state = unsafe { &mut *((frame as *mut u8).add(async_frame_slot_offset(0)) as *mut i64) };
    *state += 1;
    match *state {
        1 | 3 => RUNTIME_POLL_YIELD,
        2 | 4 => RUNTIME_POLL_PREEMPTED,
        _ => RUNTIME_POLL_READY,
    }
}

#[test]
fn reaping_01_poll_result_classification_is_exhaustive() {
    assert_eq!(
        classify_poll_result(RUNTIME_POLL_PENDING),
        PollOutcome::Pending
    );
    assert_eq!(classify_poll_result(RUNTIME_POLL_READY), PollOutcome::Ready);
    assert_eq!(classify_poll_result(RUNTIME_POLL_YIELD), PollOutcome::Yield);
    assert_eq!(
        classify_poll_result(RUNTIME_POLL_PREEMPTED),
        PollOutcome::Preempted
    );
    assert_eq!(
        classify_poll_result(RUNTIME_POLL_PANICKED),
        PollOutcome::Panicked
    );
    assert_eq!(
        classify_poll_result(RUNTIME_POLL_BLOCKED_SYSCALL),
        PollOutcome::BlockedSyscall
    );
    assert_eq!(classify_poll_result(99), PollOutcome::Invalid(99));
}

#[test]
fn reaping_02_finish_terminal_captures_cleanup_before_task_removal() {
    let mut scheduler = RuntimeScheduler::with_worker_count(TEST_WORKERS);
    let id = scheduler.spawn_placeholder();
    scheduler.with_task_mut(id, |task| {
        task.install_channel_ownership(channel_token(0x1234));
        task.install_channel_ownership(channel_token(0x5678));
    });

    scheduler.complete(id);

    assert_eq!(scheduler.task_state(id), None);
    let cleanups = scheduler.take_pending_terminal_cleanups();
    assert_eq!(cleanups.len(), 1);
    assert_eq!(cleanups[0].task_id, id);
    assert_eq!(
        cleanups[0].channel_waits,
        vec![channel_token(0x1234), channel_token(0x5678)]
    );
    assert!(cleanups[0].lock_wait.is_none());
}

#[test]
fn reaping_02b_terminal_cleanup_carries_lock_link_past_task_removal() {
    use crate::async_mutex::{AsyncMutex, MutexAcquire, MutexRelease};

    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let mutex = AsyncMutex::new(0, false);
    let (owner, victim) = with_global_for_test(|scheduler| {
        (
            scheduler.spawn_parked_placeholder(),
            scheduler.spawn_parked_placeholder(),
        )
    });
    let MutexAcquire::Acquired(owner_token) = mutex.acquire(owner) else {
        panic!("owner should acquire");
    };
    let MutexAcquire::Pending(_) = mutex.acquire(victim) else {
        panic!("victim should register behind the owner");
    };
    assert_eq!(mutex.waiter_count(), 1);

    with_global_for_test(|scheduler| scheduler.finalize_cancelled(victim));
    assert_eq!(
        willow_sched_task_state(victim),
        -1,
        "the heavy task record must already be removed"
    );
    drain_terminal_cleanups();

    assert_eq!(
        mutex.waiter_count(),
        0,
        "captured reverse link must remove the waiter without waiting for a future release"
    );
    assert_eq!(mutex.owner(), Some((owner, owner_token)));
    assert!(matches!(
        mutex.release(owner, owner_token),
        MutexRelease::Released { handed_to: None }
    ));
}

static NESTED_ROOT_TARGET: TestAtomicU64 = TestAtomicU64::new(0);
static NESTED_ROOTS_RETAINED: TestAtomicBool = TestAtomicBool::new(false);

unsafe extern "C" fn poll_nested_then_observe_frame_roots(_frame: *mut c_void) -> i32 {
    willow_sched_run_until(NESTED_ROOT_TARGET.load(TestOrdering::SeqCst));
    NESTED_ROOTS_RETAINED.store(willow_sched_frame_root_count() == 2, TestOrdering::SeqCst);
    RUNTIME_POLL_READY
}

#[test]
fn reaping_03_nested_drive_retains_terminal_frame_until_outer_quiescence() {
    let _guard = runtime_test_guard();
    reset_stress_fixture();
    NESTED_ROOTS_RETAINED.store(false, TestOrdering::SeqCst);

    let inner_frame = willow_async_frame_alloc(0, 0) as *mut c_void;
    let inner = willow_sched_spawn(poll_ready_now, inner_frame);
    NESTED_ROOT_TARGET.store(inner, TestOrdering::SeqCst);
    let outer_frame = willow_async_frame_alloc(0, 0) as *mut c_void;
    willow_sched_spawn(poll_nested_then_observe_frame_roots, outer_frame);

    assert_eq!(willow_sched_run(), 2);
    assert!(
        NESTED_ROOTS_RETAINED.load(TestOrdering::SeqCst),
        "the completed inner frame must stay rooted until the outer drive quiesces"
    );
    assert_global_metadata_reaped("nested frame-root lifetime");
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_stress_fixture();
}

#[test]
fn reaping_04_executable_task_keeps_netpoll_cleanup_even_without_channels() {
    let mut scheduler = RuntimeScheduler::with_worker_count(TEST_WORKERS);
    let id = scheduler.spawn_task(poll_ready_now, std::ptr::null_mut());
    assert_eq!(scheduler.claim_ready_for_worker(0), Some(id));
    scheduler.complete(id);
    scheduler.clear_running();

    let cleanups = scheduler.take_pending_terminal_cleanups();
    assert_eq!(cleanups.len(), 1);
    assert_eq!(cleanups[0].task_id, id);
    assert!(cleanups[0].channel_waits.is_empty());
}

#[test]
fn stress_10k_sleeping_tasks_reap_all_scheduler_metadata() {
    let _guard = runtime_test_guard();
    reset_stress_fixture();

    for _ in 0..STRESS_TASKS {
        let frame = willow_async_frame_alloc(1, 0) as *mut c_void;
        willow_sched_spawn(poll_zero_sleep_then_ready, frame);
    }
    assert_eq!(willow_sched_run(), STRESS_TASKS as i64);
    assert_global_metadata_reaped("10k sleeping tasks");
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_stress_fixture();
}

#[test]
fn stress_10k_tasks_on_one_channel_reap_all_scheduler_metadata() {
    let _guard = runtime_test_guard();
    reset_stress_fixture();
    let channel = crate::channel::willow_channel_new(0);
    let task_ids = with_global_for_test(|scheduler| {
        (0..STRESS_TASKS)
            .map(|_| scheduler.spawn_parked_placeholder())
            .collect::<Vec<_>>()
    });

    for task_id in task_ids {
        assert_eq!(
            with_current_task_for_test(task_id, || {
                crate::channel::willow_channel_recv_ready(channel)
            }),
            0
        );
    }
    crate::channel::willow_channel_close(channel);
    assert_eq!(willow_sched_run(), STRESS_TASKS as i64);
    assert_global_metadata_reaped("10k tasks on one channel");
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_stress_fixture();
}

#[test]
fn stress_10k_bounded_send_waiters_pass_on_defected_handoffs() {
    let _guard = runtime_test_guard();
    reset_stress_fixture();
    let channel = crate::channel::willow_channel_new_bounded(0, 1);
    assert_eq!(crate::channel::willow_channel_try_send_i64(channel, 1), 1);
    let task_ids = with_global_for_test(|scheduler| {
        (0..STRESS_TASKS)
            .map(|_| scheduler.spawn_parked_placeholder())
            .collect::<Vec<_>>()
    });
    for &task_id in &task_ids {
        assert_eq!(
            with_current_task_for_test(task_id, || {
                crate::channel::willow_channel_send_ready(channel)
            }),
            0
        );
    }

    // Free one slot. Each awakened select then defects to another arm;
    // unregister must pass the unconsumed handoff to the next producer
    // until every one of the 10,000 waiters has had a turn.
    assert_eq!(crate::channel::willow_channel_recv_i64(channel), 1);
    for task_id in task_ids {
        with_current_task_for_test(task_id, || {
            crate::channel::willow_channel_unregister_waiter(channel);
        });
    }

    assert_eq!(willow_sched_run(), STRESS_TASKS as i64);
    assert_global_metadata_reaped("10k bounded-send handoff defections");
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_stress_fixture();
}

#[test]
fn stress_10k_yield_preempt_tasks_reap_all_scheduler_metadata() {
    let _guard = runtime_test_guard();
    reset_stress_fixture();

    for _ in 0..STRESS_TASKS {
        let frame = willow_async_frame_alloc(1, 0) as *mut c_void;
        willow_sched_spawn(poll_yield_preempt_churn, frame);
    }
    assert_eq!(willow_sched_run(), STRESS_TASKS as i64);
    assert_global_metadata_reaped("10k yield/preempt tasks");
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_stress_fixture();
}

#[test]
fn stress_10k_blocked_syscall_tasks_reap_all_scheduler_metadata() {
    let mut scheduler = RuntimeScheduler::with_worker_count(TEST_WORKERS);
    let task_ids = (0..STRESS_TASKS)
        .map(|_| scheduler.spawn_placeholder())
        .collect::<Vec<_>>();

    let mut seen = HashSet::new();
    for _ in &task_ids {
        let task_id = scheduler.claim_ready_for_worker(0).unwrap();
        assert!(seen.insert(task_id));
        scheduler.finish_blocked_syscall_poll(task_id);
        scheduler.clear_running();
    }
    assert_eq!(
        scheduler.metadata_snapshot(),
        SchedulerMetadataSnapshot {
            heavy_tasks: STRESS_TASKS,
            queue_entries: 0,
            pending_cleanups: 0,
            frame_roots: 0,
            blocked_syscalls: STRESS_TASKS,
        }
    );
    assert!(scheduler.blocked_syscall_invariant_holds());

    for &task_id in &task_ids {
        assert!(scheduler.wake(task_id));
    }
    let mut seen = HashSet::new();
    for _ in &task_ids {
        let task_id = scheduler.claim_ready_for_worker(0).unwrap();
        assert!(seen.insert(task_id));
        scheduler.complete(task_id);
        scheduler.clear_running();
    }
    assert!(scheduler.take_pending_terminal_cleanups().is_empty());
    assert_eq!(scheduler.metadata_snapshot(), empty_metadata());
    assert!(scheduler.blocked_syscall_invariant_holds());
}

#[test]
fn stress_repeated_10k_short_task_batches_plateau_at_zero() {
    let _guard = runtime_test_guard();
    reset_stress_fixture();

    for batch in 0..3 {
        for _ in 0..STRESS_TASKS {
            willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
        }
        assert_eq!(willow_sched_run(), STRESS_TASKS as i64);
        assert_global_metadata_reaped(&format!("10k short-task batch {batch}"));
    }
    reset_stress_fixture();
}

/// Install a fresh panic context for the duration of the returned guard, so
/// a runtime fault raised by a fixture is recorded and consumed here instead
/// of leaking into the next test.
struct TestPanicContext(Option<Arc<crate::panic_context::PanicContext>>);

fn install_test_panic_context(owner: u64) -> TestPanicContext {
    TestPanicContext(crate::panic_context::replace_current_context(Some(
        Arc::new(crate::panic_context::PanicContext::new(owner)),
    )))
}

impl Drop for TestPanicContext {
    fn drop(&mut self) {
        crate::panic_context::replace_current_context(self.0.take());
    }
}

/// Consume the pending panic (as a deferred `recover()` would) and return
/// its message.
fn take_test_panic_message() -> String {
    crate::panic_context::willow_panic_enter_defer();
    let info = crate::panic_context::willow_panic_recover();
    let message = if info.is_null() {
        String::new()
    } else {
        let message = unsafe { crate::panic_context::panic_info_message(info) }.to_string();
        crate::panic_context::willow_panic_release_recovered(info);
        message
    };
    crate::panic_context::willow_panic_leave_defer();
    message
}

// ── Lost-wakeup: the pop→claim window (willow-atth) ──────────────────────
//
// A claim pops a task id out of every run queue BEFORE it takes
// `claim_gate` and increments `active_polls`. In that window the work is
// invisible to every idle check, so a concurrently idling worker could
// declare global quiescence, stop the pool, and strand a runnable task —
// observed as `await` returning early (partial output, exit 0) and as a
// no-default `select` giving up. These perspectives pin the in-flight
// marker, the checks that consult it, and the select idle policy.

/// Perspective 01. A quiet scheduler has no claim in flight.
#[test]
fn sched_wake_01_no_claim_in_flight_when_idle() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    assert!(!claims_in_flight());
}

/// Perspective 02. The marker is published for the whole guard scope and
/// retracted on drop.
#[test]
fn sched_wake_02_guard_publishes_and_retracts() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    {
        let _claim = ClaimInFlight::enter();
        assert!(claims_in_flight());
    }
    assert!(!claims_in_flight());
}

/// Perspective 03. Several workers can be inside the window at once, so the
/// marker counts rather than latches.
#[test]
fn sched_wake_03_guards_nest() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let outer = ClaimInFlight::enter();
    let inner = ClaimInFlight::enter();
    assert!(claims_in_flight());
    drop(inner);
    assert!(claims_in_flight(), "one claim is still in flight");
    drop(outer);
    assert!(!claims_in_flight());
}

/// Perspective 04. The marker is process-global, not thread-local: the
/// worker that observes idleness is never the one holding the claim.
#[test]
fn sched_wake_04_marker_is_visible_across_threads() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let entered = Arc::new(Barrier::new(2));
    let observed = Arc::new(Barrier::new(2));
    let holder_entered = Arc::clone(&entered);
    let holder_observed = Arc::clone(&observed);
    let holder = std::thread::spawn(move || {
        let _claim = ClaimInFlight::enter();
        holder_entered.wait();
        holder_observed.wait();
    });
    entered.wait();
    let seen = claims_in_flight();
    observed.wait();
    holder.join().expect("claim holder thread");
    assert!(seen, "another thread's claim must be observable");
    assert!(!claims_in_flight());
}

/// Perspective 05. An empty scheduler genuinely has no wake source.
#[test]
fn sched_wake_05_no_wake_source_when_empty() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    assert!(!scheduler_has_wake_source());
}

/// Perspective 06. The pop→claim window alone counts as a wake source. This
/// is the exact state that used to read as "nothing left to do".
#[test]
fn sched_wake_06_in_flight_claim_is_a_wake_source() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let _claim = ClaimInFlight::enter();
    assert!(
        scheduler_has_wake_source(),
        "a popped-but-unclaimed task is pending work"
    );
}

#[test]
fn sched_wake_running_peer_is_a_wake_source_but_current_poll_is_not() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let current = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);
    let state = Arc::new(ParallelRunState::default());
    state.set_polls_for_test(2, 0);
    let (peer_can_progress, caller_alone_can_progress) =
        with_parallel_context(0, Arc::clone(&state), || {
            with_current_task_for_test(current, || {
                let peer_can_progress = scheduler_has_wake_source();
                state.set_polls_for_test(1, 0);
                (peer_can_progress, scheduler_has_wake_source())
            })
        });
    assert!(peer_can_progress, "a running peer can still send a value");
    assert!(
        !caller_alone_can_progress,
        "the blocked caller must not keep itself alive forever"
    );
}

/// Perspective 07. Queued work is a wake source.
#[test]
fn sched_wake_07_queued_task_is_a_wake_source() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    assert!(scheduler_has_wake_source());
    willow_sched_run();
}

/// Perspective 08. An armed timer is a wake source even with empty queues.
#[test]
fn sched_wake_08_armed_timer_is_a_wake_source() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let id = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);
    with_current_task_for_test(id, || set_global_wake_after_millis(50));
    assert!(
        scheduler_has_wake_source(),
        "a sleeping task will become runnable"
    );
    reset_global_scheduler_for_test();
}

/// Perspective 09. Idle detection keeps the run loop alive while a claim is
/// in flight instead of reporting quiescence.
#[test]
fn sched_wake_09_idle_step_waits_for_an_in_flight_claim() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let _claim = ClaimInFlight::enter();
    assert!(
        scheduler_idle_step(0, None, false, None, current_wake_generation()),
        "the run loop must keep going while a claim is in flight"
    );
}

/// Perspective 10. Without a claim, the same empty scheduler still reports
/// idle — the fix must not turn every drive into a spin.
#[test]
fn sched_wake_10_idle_step_still_reports_genuine_idle() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    assert!(
        !scheduler_idle_step(0, None, false, None, current_wake_generation()),
        "an empty scheduler with no claim is genuinely idle"
    );
}

/// Perspective 11. A drive over an empty scheduler leaves no claim behind,
/// so the marker cannot leak into later drives.
#[test]
fn sched_wake_11_empty_drive_leaves_no_claim() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    assert_eq!(willow_sched_run(), 0);
    assert!(!claims_in_flight());
}

/// Perspective 12. Every claim path (poll, requeue, drop) retracts the
/// marker: a completed batch ends at zero.
#[test]
fn sched_wake_12_completed_batch_leaves_no_claim() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    for _ in 0..64 {
        willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    }
    assert_eq!(willow_sched_run(), 64);
    assert!(!claims_in_flight(), "claims are scoped to the pop");
    assert_eq!(global_run_queues().len(), 0, "no task is stranded");
}

/// Perspective 13. The test reset clears the marker, so a leaked claim in
/// one test cannot make every later test's scheduler look busy.
#[test]
fn sched_wake_13_reset_clears_the_marker() {
    let _guard = runtime_test_guard();
    std::mem::forget(ClaimInFlight::enter());
    assert!(claims_in_flight());
    reset_global_scheduler_for_test();
    assert!(!claims_in_flight());
}

/// Perspective 14. `select` idle wait with work still queued returns
/// quietly so the caller re-probes.
#[test]
fn sched_wake_14_select_idle_wait_returns_when_work_is_queued() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let _context = install_test_panic_context(101);
    willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
    willow_select_idle_wait();
    assert_eq!(
        crate::panic_context::willow_panic_active(),
        0,
        "queued work means a case can still become ready"
    );
    willow_sched_run();
}

/// Perspective 15. The same holds for the pop→claim window: a racing claim
/// must not be mistaken for a deadlocked select.
#[test]
fn sched_wake_15_select_idle_wait_returns_for_an_in_flight_claim() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let _context = install_test_panic_context(102);
    let _claim = ClaimInFlight::enter();
    willow_select_idle_wait();
    assert_eq!(crate::panic_context::willow_panic_active(), 0);
}

/// Perspective 16. With no wake source at all the select would block
/// forever, so it raises a language panic instead of spinning or (as
/// before) falling through and running no case at all.
#[test]
fn sched_wake_16_select_idle_wait_raises_on_real_deadlock() {
    let _guard = runtime_test_guard();
    crate::gc::willow_gc_init();
    reset_global_scheduler_for_test();
    let _context = install_test_panic_context(103);
    willow_select_idle_wait();
    assert_eq!(crate::panic_context::willow_panic_active(), 1);
    let message = take_test_panic_message();
    assert!(
        message.contains("select would block forever"),
        "unexpected diagnostic: {message}"
    );
}

/// Perspective 17. The deadlock diagnostic is bounded: it must not hang the
/// program it is meant to explain.
#[test]
fn sched_wake_17_select_idle_wait_is_bounded() {
    let _guard = runtime_test_guard();
    crate::gc::willow_gc_init();
    reset_global_scheduler_for_test();
    let _context = install_test_panic_context(104);
    let started = Instant::now();
    willow_select_idle_wait();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "idle wait must stay bounded, took {:?}",
        started.elapsed()
    );
    take_test_panic_message();
}

/// Perspective 18. A drive must not report quiescence while another thread
/// is still spawning work into it: every task spawned before the drive
/// returns is either completed or still queued, never lost.
#[test]
fn sched_wake_18_concurrent_spawns_are_never_stranded() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    const ROUNDS: usize = 16;
    const PER_ROUND: usize = 32;
    for _ in 0..ROUNDS {
        for _ in 0..PER_ROUND {
            willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
        }
        let completed = willow_sched_run();
        assert_eq!(completed, PER_ROUND as i64, "every spawned task must run");
        assert_eq!(global_run_queues().len(), 0);
        assert!(!claims_in_flight());
    }
}

/// Perspective 19. `run_until` returns only once its target is terminal,
/// repeated enough times to cross the pop→claim window under the default
/// five-worker pool. Before the fix this could return with the target still
/// pending, and the caller then read an unwritten result slot.
#[test]
fn sched_wake_19_run_until_never_returns_with_a_pending_target() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    for round in 0..64 {
        // Background load so several workers are claiming concurrently.
        for _ in 0..8 {
            willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
        }
        // Parks on a real timer, so the target is woken by the run loop
        // rather than depending on the drive that spawned it.
        let frame = willow_async_frame_alloc(0, 0) as *mut c_void;
        let target = willow_sched_spawn(poll_sleep_then_ready, frame);
        willow_sched_run_until(target);
        assert!(
            target_is_done(Some(target)),
            "run_until returned with the target pending in round {round}"
        );
    }
    willow_sched_run();
}

/// Perspective 20. Repeated parallel drives neither leak the marker nor
/// leave queued work behind, so the fix adds no steady-state cost.
#[test]
fn sched_wake_20_repeated_parallel_drives_settle_at_zero() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    for _ in 0..32 {
        for _ in 0..16 {
            willow_sched_spawn(poll_ready_now, std::ptr::null_mut());
        }
        willow_sched_run();
    }
    assert!(!claims_in_flight());
    assert_eq!(global_run_queues().len(), 0);
    assert_eq!(willow_sched_run(), 0, "a settled scheduler drives to zero");
}

#[test]
fn idle_timer_wait_observes_work_published_after_empty_probe() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    let timer = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);
    with_current_task_for_test(timer, || set_global_wake_after_millis(5_000));
    let task = with_global_for_test(RuntimeScheduler::spawn_parked_placeholder);
    let generation = current_wake_generation();
    assert!(try_wake_parked_task(task));
    let start = Instant::now();
    assert!(scheduler_idle_step(0, None, false, None, generation));
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(global_run_queues().contains(task));
    reset_global_scheduler_for_test();
}
