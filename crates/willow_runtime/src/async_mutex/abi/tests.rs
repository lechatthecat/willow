use super::*;
use crate::async_rwlock::{AsyncRwLock, RwAcquire, RwRelease};
use crate::gc::{runtime_test_guard, total_frees_for_test, willow_gc_collect, willow_gc_init};
use crate::lock_wait::{LockAccess, lock_wait_link_of};
use crate::scheduler::{
    reset_global_scheduler_for_test, single_worker_for_test, willow_sched_cancel,
    willow_sched_current_task, willow_sched_run, willow_sched_spawn, willow_sched_wake,
    with_current_task_for_test, with_global_for_test,
};
use crate::string::{willow_string_as_str, willow_string_from_str};
use crate::task::{RUNTIME_POLL_PENDING, RUNTIME_POLL_READY};
use crate::task_state::{ClaimOutcome, WakeOutcome};
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::Mutex as TestMutex;
use std::sync::atomic::{AtomicI64, AtomicU64};
use std::time::Instant;

// Test perspectives for the Stage 3 Mutex state machine (willow-38w.1.3).
// Each maps to at least one test below.
//
//  1. A fresh mutex is unowned, has no waiters, and is reclaimable.
//  2. Uncontended acquire takes ownership, returns a token, and publishes
//     no reverse link (Idle -> ValueLoaded, §8.4).
//  3. Only the proven owner can load the protected value.
//  4. Only the proven owner can commit, and the commit is visible to the
//     next owner.
//  5. A second task acquiring a held mutex is queued Pending and cannot
//     observe the value.
//  6. Release hands the lock to the FIFO head, not the newest arrival.
//  7. No barging: a fresh arrival is queued behind existing waiters even
//     though the release window momentarily has no owner (§8.6).
//  8. poll_acquire consumes a reserved handoff (HandoffOwned -> ValueLoaded)
//     and is idempotent.
//  9. A spurious wake while still Waiting re-parks (Pending), it does not
//     grant the lock.
// 10. poll_acquire with a stale or foreign token reports Lost and grants
//     nothing.
// 11. The current owner re-acquiring is Recursive, not a self-deadlock
//     (§11).
// 12. A task already queued on the mutex re-acquiring is Recursive too.
// 13. A Recursive rejection publishes nothing: owner and queue are
//     unchanged.
// 14. Acquire from a terminal task is Ineligible and publishes nothing.
// 15. Acquire from a cancel-requested task is Ineligible.
// 16. Release with a stale token or from a non-owner is NotOwner and never
//     steals from the live owner.
// 17. A double release is NotOwner the second time.
// 18. Terminal wake compensation: the handoff revokes the dead waiter's
//     reservation and admits the next live one (§8.6).
// 19. A wake that cannot land because the task record is gone is skipped
//     the same way.
// 20. Post-wake cancellation re-hands the lock, so a HandoffOwned task that
//     dies never strands it (§12.3).
// 21. Cancelling a Waiting waiter removes it, and a later release skips it.
// 22. Cancelling a task with no registration is NoLink.
// 23. The scheduler's terminal-cleanup path re-hands a reserved lock via
//     purge_task_lock_wait.
// 24. Concurrent acquire from many threads yields exactly one owner and
//     queues every loser exactly once.
// 25. 10k waiters queue and drain in FIFO order with no lost wakeup.
// 26. Reclamation is refused while owned, queued, or frame-referenced, and
//     granted only after a full drain (§15.1).
// 27. Frame-reference accounting is balanced across acquire/handoff/release
//     and never underflows on a rejected release.
// 28. GC roots cover a ref-typed protected word and skip non-ref and null
//     words.
// 29. The C ABI routes acquire/poll/load/commit/release and publishes the
//     token through the out-parameter.
// 30. The C ABI reports LOST for a null handle or no running task, and
//     never touches the value.
// 31. A cancellation storm leaves the lock consistent and finally
//     reclaimable.
// 32. Ownership is task-based, not thread-based: it survives being released
//     from a different worker thread (§8.2), and a single worker still
//     drains a handoff chain.
// 33. Wake-before-park: a handoff to a task that is already inside a poll
//     still reserves ownership, and that task's own poll consumes it.
// 34. Cleanup acts on the lock the task is actually linked to. With two
//     mutexes live, retiring a task that is HandoffOwned on B re-hands B's
//     ownership to B's next waiter and leaves A's owner and queue alone.
// 35. The same holds in the Waiting phase: only B's queue loses an entry.
// 36. Real scheduler, one worker: a task that parks on a held mutex does not
//     block the worker — an unrelated task runs to completion while it is
//     parked, and the parked task resumes only after the handoff.
// 37. Real scheduler, one worker: a chain of waiters acquires in the order
//     they registered, every task completes, and none is left parked.
// 38. Real scheduler: cancelling a parked waiter's task retires it without
//     stranding the lock — the remaining tasks still complete.
// 39. A real collection traces a ref cell's protected word: an object whose
//     only reference is the mutex survives, with its contents intact.
// 40. The same collection reclaims an equivalent unreferenced object, so 39
//     is a fact about the root wiring and not about a collector that never
//     frees anything.
// 41. The C ABI distinguishes a cancel-requested task from a stale handle:
//     acquire publishes no waiter/token and returns CANCELLED, not LOST.
// 42. Unknown-status diagnostics preserve the raw status and distinguish
//     acquire from poll.
// 43. An unknown phase is named explicitly instead of being mislabeled.

fn parked_task() -> RuntimeTaskId {
    with_global_for_test(|sched| sched.spawn_parked_placeholder())
}

fn ready_task() -> RuntimeTaskId {
    with_global_for_test(|sched| sched.spawn_placeholder())
}

/// Drive a parked task to `Terminal` the only way the state word allows.
fn make_terminal(task: RuntimeTaskId) {
    with_global_for_test(|sched| {
        sched.with_task_for_test(task, |task| {
            assert_ne!(task.state.wake(), WakeOutcome::Terminal);
            assert_eq!(task.state.claim_for_poll(), ClaimOutcome::Poll);
            assert!(task.state.finish_terminal());
        })
    });
}

/// Claim a parked task for a poll, so a wake has to be recorded against the
/// running poll instead of enqueued.
fn make_running(task: RuntimeTaskId) {
    with_global_for_test(|sched| {
        sched.with_task_for_test(task, |task| {
            assert_ne!(task.state.wake(), WakeOutcome::Terminal);
            assert_eq!(task.state.claim_for_poll(), ClaimOutcome::Poll);
        })
    });
}

fn drop_task_record(task: RuntimeTaskId) {
    with_global_for_test(|sched| sched.remove_task_for_test(task));
}

/// Acquire, expecting the uncontended path.
fn acquire_now(mutex: &AsyncMutex, task: RuntimeTaskId) -> RegistrationToken {
    match mutex.acquire(task) {
        MutexAcquire::Acquired(token) => token,
        other => panic!("expected an uncontended acquire, got {other:?}"),
    }
}

/// Acquire, expecting to be queued.
fn queue_on(mutex: &AsyncMutex, task: RuntimeTaskId) -> RegistrationToken {
    match mutex.acquire(task) {
        MutexAcquire::Pending(token) => token,
        other => panic!("expected to be queued, got {other:?}"),
    }
}

// 1
#[test]
fn fresh_mutex_is_idle_and_reclaimable() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let mutex = AsyncMutex::new(7, false);
    assert_ne!(mutex.lock_id(), 0);
    assert_eq!(mutex.owner(), None);
    assert_eq!(mutex.waiter_count(), 0);
    assert_eq!(mutex.frame_refs(), 0);
    assert_eq!(mutex.reclamation_status(), ReclamationStatus::Reclaimable);
}

// 2, 3
#[test]
fn uncontended_acquire_owns_without_a_reverse_link() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let mutex = AsyncMutex::new(41, false);
    let owner = parked_task();
    let other = parked_task();

    let token = acquire_now(&mutex, owner);
    assert_eq!(mutex.owner(), Some((owner, token)));
    // Idle -> ValueLoaded never parks, so there is nothing in the task
    // table for a cancellation to find.
    assert!(lock_wait_link_of(owner).is_none());

    assert_eq!(mutex.load(owner, token), Some(41));
    // A non-owner, and the owner with the wrong token, both see nothing.
    assert_eq!(mutex.load(other, token), None);
    assert_eq!(mutex.load(owner, token + 1), None);
}

// 4
#[test]
fn only_the_owner_commits_and_the_next_owner_sees_it() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let mutex = AsyncMutex::new(0, false);
    let first = parked_task();
    let second = parked_task();

    let first_token = acquire_now(&mutex, first);
    assert!(!mutex.commit(second, first_token, 99), "not the owner");
    assert!(!mutex.commit(first, first_token + 1, 99), "stale token");
    assert!(mutex.commit(first, first_token, 5));

    let second_token = queue_on(&mutex, second);
    assert!(matches!(
        mutex.release(first, first_token),
        MutexRelease::Released {
            handed_to: Some(next)
        } if next == second
    ));
    assert_eq!(
        mutex.poll_acquire(second, second_token),
        MutexResume::Acquired
    );
    assert_eq!(mutex.load(second, second_token), Some(5));
}

// 5, 6, 7
#[test]
fn waiters_are_fifo_and_a_new_arrival_never_bargs() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let mutex = AsyncMutex::new(1, false);
    let owner = parked_task();
    let first = parked_task();
    let second = parked_task();

    let owner_token = acquire_now(&mutex, owner);
    let first_token = queue_on(&mutex, first);
    let second_token = queue_on(&mutex, second);
    assert_eq!(mutex.queued_waiters(), vec![first, second]);
    // A queued waiter has no ownership, so it must not read the value.
    assert_eq!(mutex.load(first, first_token), None);

    // Release goes to the FIFO head, not the newest arrival.
    let MutexRelease::Released { handed_to } = mutex.release(owner, owner_token) else {
        panic!("owner should be able to release");
    };
    assert_eq!(handed_to, Some(first));
    assert_eq!(mutex.owner(), Some((first, first_token)));

    // A fresh arrival during the handoff is queued behind `second`, even
    // though it arrived after the lock had been released once.
    let late = parked_task();
    let late_token = queue_on(&mutex, late);
    assert_eq!(mutex.queued_waiters(), vec![second, late]);
    assert_ne!(late_token, second_token);
}

// 8, 9, 10
#[test]
fn poll_acquire_distinguishes_handoff_spurious_wake_and_loss() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let mutex = AsyncMutex::new(0, false);
    let owner = parked_task();
    let waiter = parked_task();

    let owner_token = acquire_now(&mutex, owner);
    let waiter_token = queue_on(&mutex, waiter);

    // Still Waiting: a wake for some other reason must re-park, not grant.
    assert_eq!(
        mutex.poll_acquire(waiter, waiter_token),
        MutexResume::Pending
    );
    // A token that was never issued to this waiter grants nothing.
    assert_eq!(
        mutex.poll_acquire(waiter, waiter_token + 100),
        MutexResume::Lost
    );

    assert!(matches!(
        mutex.release(owner, owner_token),
        MutexRelease::Released { .. }
    ));
    assert_eq!(
        mutex.poll_acquire(waiter, waiter_token),
        MutexResume::Acquired
    );
    assert!(lock_wait_link_of(waiter).is_none(), "link is consumed");
    // Idempotent: a second poll of the same generation still reports the
    // ownership it already holds rather than double-counting it.
    assert_eq!(
        mutex.poll_acquire(waiter, waiter_token),
        MutexResume::Acquired
    );
    assert_eq!(mutex.frame_refs(), 1);
}

// 11, 12, 13
#[test]
fn recursive_acquire_is_rejected_and_changes_nothing() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let mutex = AsyncMutex::new(0, false);
    let owner = parked_task();
    let waiter = parked_task();

    let owner_token = acquire_now(&mutex, owner);
    let waiter_token = queue_on(&mutex, waiter);

    // The owner re-acquiring would self-deadlock: reject deterministically.
    assert_eq!(mutex.acquire(owner), MutexAcquire::Recursive);
    // A queued waiter re-acquiring is the same fault one phase earlier.
    assert_eq!(mutex.acquire(waiter), MutexAcquire::Recursive);

    assert_eq!(mutex.owner(), Some((owner, owner_token)));
    assert_eq!(mutex.queued_waiters(), vec![waiter]);
    assert_eq!(
        lock_wait_link_of(waiter).map(|link| link.token),
        Some(waiter_token),
        "a rejected recursive acquire must not re-register the waiter"
    );

    // A different mutex is not recursion: the same task may hold both.
    let other = AsyncMutex::new(0, false);
    assert!(matches!(
        other.acquire(owner),
        MutexAcquire::Acquired(_) | MutexAcquire::Pending(_)
    ));
}

// 14, 15
#[test]
fn a_task_that_cannot_park_is_ineligible() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let mutex = AsyncMutex::new(0, false);
    let owner = parked_task();
    let dead = parked_task();
    let cancelled = parked_task();
    let unknown = 999_999;

    let _owner_token = acquire_now(&mutex, owner);

    make_terminal(dead);
    assert_eq!(mutex.acquire(dead), MutexAcquire::Ineligible);
    willow_sched_cancel(cancelled);
    assert_eq!(mutex.acquire(cancelled), MutexAcquire::Ineligible);
    assert_eq!(mutex.acquire(unknown), MutexAcquire::Ineligible);

    assert_eq!(mutex.waiter_count(), 0, "no registration was published");
    assert!(lock_wait_link_of(dead).is_none());
    assert!(lock_wait_link_of(cancelled).is_none());
}

// 16, 17
#[test]
fn a_stale_or_repeated_release_never_steals_the_lock() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let mutex = AsyncMutex::new(0, false);
    let owner = parked_task();
    let stranger = parked_task();

    let token = acquire_now(&mutex, owner);
    assert_eq!(mutex.release(stranger, token), MutexRelease::NotOwner);
    assert_eq!(mutex.release(owner, token + 1), MutexRelease::NotOwner);
    assert_eq!(mutex.owner(), Some((owner, token)), "still held");

    assert!(matches!(
        mutex.release(owner, token),
        MutexRelease::Released { handed_to: None }
    ));
    assert_eq!(mutex.owner(), None);
    // A double release must not un-own a lock a later task now holds.
    assert_eq!(mutex.release(owner, token), MutexRelease::NotOwner);

    let next = parked_task();
    let next_token = acquire_now(&mutex, next);
    assert_eq!(mutex.release(owner, token), MutexRelease::NotOwner);
    assert_eq!(mutex.owner(), Some((next, next_token)));
}

// 18, 19
#[test]
fn a_wake_that_cannot_land_is_compensated_not_dropped() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let mutex = AsyncMutex::new(0, false);
    let owner = parked_task();
    let dead = parked_task();
    let gone = parked_task();
    let live = parked_task();

    let owner_token = acquire_now(&mutex, owner);
    let _dead_token = queue_on(&mutex, dead);
    let _gone_token = queue_on(&mutex, gone);
    let live_token = queue_on(&mutex, live);

    // Both leading waiters become un-wakeable after they were queued.
    make_terminal(dead);
    drop_task_record(gone);

    let MutexRelease::Released { handed_to } = mutex.release(owner, owner_token) else {
        panic!("owner should be able to release");
    };
    assert_eq!(
        handed_to,
        Some(live),
        "the scan must revoke each failed reservation and continue"
    );
    assert_eq!(mutex.owner(), Some((live, live_token)));
    assert_eq!(mutex.waiter_count(), 0);
}

// 20, 21, 22
#[test]
fn cancellation_reconciles_by_phase_and_re_hands_reserved_ownership() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let mutex = AsyncMutex::new(0, false);
    let owner = parked_task();
    let doomed = parked_task();
    let survivor = parked_task();
    let bystander = parked_task();

    let owner_token = acquire_now(&mutex, owner);
    let _doomed_token = queue_on(&mutex, doomed);
    let survivor_token = queue_on(&mutex, survivor);

    // A task with no registration at all.
    assert_eq!(purge_task_lock_wait(bystander), MutexCancel::NoLink);

    // Reserve ownership for `doomed`, then cancel it before it can resume:
    // without the re-handoff the lock would be owned by a task that never
    // runs again.
    assert!(matches!(
        mutex.release(owner, owner_token),
        MutexRelease::Released { handed_to: Some(_) }
    ));
    assert_eq!(mutex.owner().map(|(task, _)| task), Some(doomed));
    assert_eq!(
        purge_task_lock_wait(doomed),
        MutexCancel::ReleasedOwnership {
            handed_to: Some(survivor)
        }
    );
    assert_eq!(mutex.owner(), Some((survivor, survivor_token)));

    // A Waiting cancellation just removes the queue entry, and a later
    // release skips the removed waiter.
    let late = parked_task();
    let _late_token = queue_on(&mutex, late);
    assert_eq!(purge_task_lock_wait(late), MutexCancel::RemovedWaiter);
    assert_eq!(mutex.queued_waiters(), Vec::<RuntimeTaskId>::new());
    assert!(matches!(
        mutex.release(survivor, survivor_token),
        MutexRelease::Released { handed_to: None }
    ));
}

// 34, 35
#[test]
fn cleanup_acts_on_the_task_s_own_lock_not_a_bystander_lock() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    // Two live mutexes. Everything interesting happens on `b`; `a` exists
    // only to be left alone.
    let a = AsyncMutex::new(0, false);
    let b = AsyncMutex::new(0, false);
    assert_ne!(a.lock_id(), b.lock_id());

    let a_owner = parked_task();
    let a_waiter = parked_task();
    let a_owner_token = acquire_now(&a, a_owner);
    let a_waiter_token = queue_on(&a, a_waiter);

    let b_owner = parked_task();
    let b_doomed = parked_task();
    let b_survivor = parked_task();
    let b_owner_token = acquire_now(&b, b_owner);
    let _b_doomed_token = queue_on(&b, b_doomed);
    let b_survivor_token = queue_on(&b, b_survivor);

    // Reserve B for `b_doomed`, then retire it. Cleanup is task-directed:
    // it must find B through the task's reverse link. The pre-review API
    // took `&self`, so a caller holding A would have reconciled B's
    // registration and then re-handed *A* — B's survivor would have parked
    // forever and A's waiter would have been handed a lock it was still
    // queued for.
    assert!(matches!(
        b.release(b_owner, b_owner_token),
        MutexRelease::Released { handed_to: Some(_) }
    ));
    assert_eq!(b.owner().map(|(task, _)| task), Some(b_doomed));
    assert_eq!(
        purge_task_lock_wait(b_doomed),
        MutexCancel::ReleasedOwnership {
            handed_to: Some(b_survivor)
        },
        "the re-handoff must happen on B, the lock the task was linked to"
    );
    assert_eq!(b.owner(), Some((b_survivor, b_survivor_token)));

    // A is untouched: same owner, same single queued waiter.
    assert_eq!(a.owner(), Some((a_owner, a_owner_token)));
    assert_eq!(a.queued_waiters(), vec![a_waiter]);
    assert_eq!(a.waiter_count(), 1);

    // The Waiting phase behaves the same way: retiring a task queued on B
    // removes exactly one entry, from B.
    let b_late = parked_task();
    let _b_late_token = queue_on(&b, b_late);
    assert_eq!(purge_task_lock_wait(b_late), MutexCancel::RemovedWaiter);
    assert_eq!(b.queued_waiters(), Vec::<RuntimeTaskId>::new());
    assert_eq!(a.queued_waiters(), vec![a_waiter]);

    // And both locks still drain normally afterwards.
    assert!(matches!(
        b.release(b_survivor, b_survivor_token),
        MutexRelease::Released { handed_to: None }
    ));
    let MutexRelease::Released { handed_to } = a.release(a_owner, a_owner_token) else {
        panic!("A's owner should still be able to release");
    };
    assert_eq!(handed_to, Some(a_waiter));
    assert_eq!(
        a.poll_acquire(a_waiter, a_waiter_token),
        MutexResume::Acquired
    );
}

// 23
#[test]
fn terminal_cleanup_path_re_hands_a_reserved_lock() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let mutex = AsyncMutex::new(0, false);
    let owner = parked_task();
    let doomed = parked_task();
    let survivor = parked_task();

    let owner_token = acquire_now(&mutex, owner);
    let _doomed_token = queue_on(&mutex, doomed);
    let survivor_token = queue_on(&mutex, survivor);
    assert!(matches!(
        mutex.release(owner, owner_token),
        MutexRelease::Released { .. }
    ));

    // This is what the scheduler runs for a retiring task: it follows only
    // the task's own reverse link, with no registry or task scan.
    assert_eq!(
        purge_task_lock_wait(doomed),
        MutexCancel::ReleasedOwnership {
            handed_to: Some(survivor)
        }
    );
    assert_eq!(mutex.owner(), Some((survivor, survivor_token)));
    assert_eq!(purge_task_lock_wait(doomed), MutexCancel::NoLink);
}

// 24
#[test]
fn concurrent_acquire_yields_exactly_one_owner() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let mutex = AsyncMutex::new(0, false);
    let tasks: Vec<RuntimeTaskId> = (0..16).map(|_| parked_task()).collect();
    let barrier = Arc::new(Barrier::new(tasks.len()));

    let outcomes: Vec<MutexAcquire> = std::thread::scope(|scope| {
        let handles: Vec<_> = tasks
            .iter()
            .map(|&task| {
                let barrier = Arc::clone(&barrier);
                let mutex = &mutex;
                scope.spawn(move || {
                    barrier.wait();
                    mutex.acquire(task)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let acquired: Vec<_> = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, MutexAcquire::Acquired(_)))
        .collect();
    let pending = outcomes
        .iter()
        .filter(|outcome| matches!(outcome, MutexAcquire::Pending(_)))
        .count();
    assert_eq!(acquired.len(), 1, "exactly one winner: {outcomes:?}");
    assert_eq!(pending, tasks.len() - 1, "every loser queued exactly once");
    assert_eq!(mutex.waiter_count(), tasks.len() - 1);
    assert!(mutex.owner().is_some());
}

// 25
#[test]
fn ten_thousand_waiters_drain_in_fifo_order() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let mutex = AsyncMutex::new(0, false);
    let owner = parked_task();
    let owner_token = acquire_now(&mutex, owner);

    let waiters: Vec<(RuntimeTaskId, RegistrationToken)> = (0..10_000)
        .map(|_| {
            let task = parked_task();
            (task, queue_on(&mutex, task))
        })
        .collect();
    assert_eq!(mutex.waiter_count(), waiters.len());

    let mut current = (owner, owner_token);
    for (index, &(task, token)) in waiters.iter().enumerate() {
        let MutexRelease::Released { handed_to } = mutex.release(current.0, current.1) else {
            panic!("owner {index} should be able to release");
        };
        assert_eq!(handed_to, Some(task), "waiter {index} out of FIFO order");
        assert_eq!(
            mutex.poll_acquire(task, token),
            MutexResume::Acquired,
            "waiter {index} lost its wake"
        );
        // Each hop hands the value along, so a dropped handoff would show
        // up as a gap in the count.
        assert!(mutex.commit(task, token, index as i64 + 1));
        current = (task, token);
    }

    assert_eq!(mutex.load(current.0, current.1), Some(waiters.len() as i64));
    assert!(matches!(
        mutex.release(current.0, current.1),
        MutexRelease::Released { handed_to: None }
    ));
}

#[test]
fn ten_thousand_waiter_cancellation_storm_leaves_no_links_or_tombstones() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let mutex = AsyncMutex::new(0, false);
    let owner = parked_task();
    let owner_token = acquire_now(&mutex, owner);
    let waiters: Vec<_> = (0..10_000)
        .map(|_| {
            let task = parked_task();
            (task, queue_on(&mutex, task))
        })
        .collect();

    for (index, &(task, _)) in waiters.iter().enumerate() {
        if index % 2 == 0 {
            assert_eq!(purge_task_lock_wait(task), MutexCancel::RemovedWaiter);
        }
    }

    let mut current = (owner, owner_token);
    for (index, &(task, token)) in waiters.iter().enumerate() {
        if index % 2 == 0 {
            continue;
        }
        let MutexRelease::Released { handed_to } = mutex.release(current.0, current.1) else {
            panic!("owner before waiter {index} should release");
        };
        assert_eq!(handed_to, Some(task));
        assert_eq!(mutex.poll_acquire(task, token), MutexResume::Acquired);
        current = (task, token);
    }
    assert!(matches!(
        mutex.release(current.0, current.1),
        MutexRelease::Released { handed_to: None }
    ));

    assert_eq!(mutex.waiter_count(), 0);
    for &(task, _) in &waiters {
        assert!(
            lock_wait_link_of(task).is_none(),
            "task {task} retained a reverse link after the storm"
        );
    }
    assert_eq!(mutex.reclamation_status(), ReclamationStatus::Reclaimable);
}

// 26, 27
#[test]
fn reclamation_and_frame_refs_track_live_relationships() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let mutex = AsyncMutex::new(0, false);
    let owner = parked_task();
    let waiter = parked_task();

    let owner_token = acquire_now(&mutex, owner);
    assert_eq!(mutex.frame_refs(), 1);
    assert_eq!(
        mutex.reclamation_status(),
        ReclamationStatus::Retained {
            owned: true,
            waiters: 0,
            frame_refs: 1,
        }
    );

    let waiter_token = queue_on(&mutex, waiter);
    assert_eq!(
        mutex.reclamation_status(),
        ReclamationStatus::Retained {
            owned: true,
            waiters: 1,
            frame_refs: 1,
        }
    );

    // A rejected release must not decrement the accounting.
    assert_eq!(mutex.release(waiter, owner_token), MutexRelease::NotOwner);
    assert_eq!(mutex.frame_refs(), 1);

    assert!(matches!(
        mutex.release(owner, owner_token),
        MutexRelease::Released { .. }
    ));
    assert_eq!(mutex.frame_refs(), 0);
    // Still retained: ownership is reserved for the woken waiter.
    assert!(matches!(
        mutex.reclamation_status(),
        ReclamationStatus::Retained { owned: true, .. }
    ));

    assert_eq!(
        mutex.poll_acquire(waiter, waiter_token),
        MutexResume::Acquired
    );
    assert_eq!(mutex.frame_refs(), 1);
    assert!(matches!(
        mutex.release(waiter, waiter_token),
        MutexRelease::Released { handed_to: None }
    ));
    assert_eq!(mutex.frame_refs(), 0);
    assert_eq!(
        mutex.reclamation_status(),
        ReclamationStatus::Reclaimable,
        "only a fully drained state may ever be freed"
    );
}

// 28
#[test]
fn gc_roots_cover_ref_words_only() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let mut word = 0xABCDu64;
    let pointer = (&raw mut word) as i64;

    let scalar = AsyncMutex::new(pointer, false);
    assert!(!scalar.is_ref());
    assert_eq!(
        scalar.gc_root(),
        None,
        "a scalar word is not traced even when it looks like a pointer"
    );
    assert_eq!(
        AsyncMutex::new(0, true).gc_root(),
        None,
        "a ref cell holding nil is not a root"
    );
    assert_eq!(
        AsyncMutex::new(pointer, true).gc_root(),
        Some(pointer as *mut u8),
        "a ref-typed protected word is a root the collector must trace"
    );

    let mut slots = Vec::new();
    unsafe {
        trace_async_mutex(
            (&*scalar as *const AsyncMutex).cast_mut().cast::<u8>(),
            &mut slots,
        );
    }
    assert!(slots.is_empty(), "scalar handles have no trace slot");
    let traced = AsyncMutex::new(pointer, true);
    unsafe {
        trace_async_mutex(
            (&*traced as *const AsyncMutex).cast_mut().cast::<u8>(),
            &mut slots,
        );
        assert_eq!(*slots[0], pointer as *mut u8);
    }
}

// 39, 40
#[test]
fn a_real_collection_keeps_a_ref_cell_s_object_alive() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    willow_gc_init();

    // The GC-managed mutex handle is the protected object's only parent.
    // Keep the handle itself as a runtime root, then prove its trace hook
    // retains the String child.
    let protected = willow_string_from_str("protected by the lock");
    let mutex = willow_async_mutex_new(protected as i64, 1);
    crate::gc::willow_gc_add_runtime_root(mutex.cast::<u8>());

    // The control: an identical allocation nobody references at all. It
    // proves this collection really does reclaim, so the survival of
    // `protected` is a fact about the root wiring rather than about a
    // collector that happens to free nothing.
    let unreferenced = willow_string_from_str("referenced by nobody");
    assert_ne!(protected, unreferenced);

    let freed_before = total_frees_for_test();
    willow_gc_collect();
    assert!(
        total_frees_for_test() > freed_before,
        "the control object was unreachable, so this collection must have \
         reclaimed at least one object"
    );

    // The survivor: still exactly one root, still the same address (this
    // path is non-moving), and still readable with its contents intact.
    // Reading it is the real assertion — a swept object's payload is gone.
    assert_eq!(
        unsafe { willow_string_as_str(protected) },
        "protected by the lock",
        "an object whose only reference is a ref-typed lock cell must \
         survive a collection with its contents intact"
    );

    // The cell itself is unowned and unqueued throughout: tracing must not
    // depend on anyone holding the lock (the collector reads the word
    // without ownership by design).
    let cell = unsafe { mutex_from_raw(mutex) }.expect("mutex");
    assert!(cell.is_ref());
    assert_eq!(cell.owner(), None);
    assert_eq!(cell.gc_root(), Some(protected));

    let drops_before = ASYNC_MUTEX_DROP_COUNT.load(Ordering::SeqCst);
    crate::gc::willow_gc_remove_runtime_root(mutex.cast::<u8>());
    willow_gc_collect();
    assert_eq!(
        ASYNC_MUTEX_DROP_COUNT.load(Ordering::SeqCst),
        drops_before + 1,
        "an inactive unreachable handle drops its boxed native state"
    );
}

#[test]
fn repeated_unreachable_mutex_handles_are_finalized() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    willow_gc_init();
    let before = ASYNC_MUTEX_DROP_COUNT.load(Ordering::SeqCst);
    for value in 0..1_000 {
        let _ = willow_async_mutex_new(value, 0);
    }
    willow_gc_collect();
    assert_eq!(
        ASYNC_MUTEX_DROP_COUNT.load(Ordering::SeqCst),
        before + 1_000,
        "dead handles must release their boxed native states"
    );
}

// 29
#[test]
fn c_abi_routes_the_full_acquire_release_cycle() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let raw = willow_async_mutex_new(3, 0);
    let owner = parked_task();
    let waiter = parked_task();

    let mut owner_token: i64 = -1;
    let mut waiter_token: i64 = -1;

    with_current_task_for_test(owner, || {
        assert_eq!(
            willow_async_mutex_acquire(raw, &raw mut owner_token),
            MUTEX_STATUS_ACQUIRED
        );
        assert!(owner_token >= 0, "the token must be published to the frame");
        assert_eq!(willow_async_mutex_load(raw, owner_token), 3);
        assert_eq!(willow_async_mutex_commit(raw, owner_token, 8), 1);
        // Non-reentrant, and the ABI reports it as its own status so the
        // generated code can raise the language panic.
        assert_eq!(
            willow_async_mutex_acquire(raw, &raw mut owner_token),
            MUTEX_STATUS_RECURSIVE
        );
    });

    with_current_task_for_test(waiter, || {
        assert_eq!(
            willow_async_mutex_acquire(raw, &raw mut waiter_token),
            MUTEX_STATUS_PENDING
        );
        assert_eq!(
            willow_async_mutex_load(raw, waiter_token),
            0,
            "not the owner"
        );
        assert_eq!(
            willow_async_mutex_poll(raw, waiter_token),
            MUTEX_STATUS_PENDING
        );
    });

    with_current_task_for_test(owner, || {
        assert_eq!(willow_async_mutex_release(raw, owner_token), 1);
        assert_eq!(
            willow_async_mutex_release(raw, owner_token),
            0,
            "double release"
        );
    });

    with_current_task_for_test(waiter, || {
        assert_eq!(
            willow_async_mutex_poll(raw, waiter_token),
            MUTEX_STATUS_ACQUIRED
        );
        assert_eq!(willow_async_mutex_load(raw, waiter_token), 8);
        assert_eq!(willow_async_mutex_cancel(), 0, "the link was consumed");
        assert_eq!(willow_async_mutex_release(raw, waiter_token), 1);
    });
}

// 30
#[test]
fn c_abi_reports_loss_without_a_handle_or_a_running_task() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let raw = willow_async_mutex_new(5, 0);
    let task = parked_task();
    let mut token: i64 = -1;

    // No running task: a lock statement outside a task is an ABI misuse.
    assert_eq!(
        willow_async_mutex_acquire(raw, &raw mut token),
        MUTEX_STATUS_LOST
    );
    assert_eq!(willow_async_mutex_poll(raw, 0), MUTEX_STATUS_LOST);
    assert_eq!(willow_async_mutex_load(raw, 0), 0);
    assert_eq!(willow_async_mutex_commit(raw, 0, 77), 0);
    assert_eq!(willow_async_mutex_release(raw, 0), 0);
    assert_eq!(willow_async_mutex_cancel(), 0);

    with_current_task_for_test(task, || {
        let null = std::ptr::null_mut();
        assert_eq!(
            willow_async_mutex_acquire(null, &raw mut token),
            MUTEX_STATUS_LOST
        );
        assert_eq!(willow_async_mutex_poll(null, 0), MUTEX_STATUS_LOST);
        assert_eq!(willow_async_mutex_commit(null, 0, 77), 0);
        // A rejected acquire leaves the value untouched.
        let owner_token = acquire_now(unsafe { mutex_from_raw(raw) }.expect("mutex"), task);
        assert_eq!(willow_async_mutex_load(raw, owner_token as i64), 5);
        // Leave a reclaimable GC handle for the next fixture's heap reset.
        assert_eq!(willow_async_mutex_release(raw, owner_token as i64), 1);
    });
    assert_eq!(token, -1, "no token is published on a lost acquire");
}

// 41
#[test]
fn c_abi_reports_cancelled_without_publishing_a_waiter() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let raw = willow_async_mutex_new(5, 0);
    let owner = parked_task();
    let cancelled = parked_task();
    let mut owner_token: i64 = -1;
    let mut token: i64 = -1;
    with_current_task_for_test(owner, || {
        assert_eq!(
            willow_async_mutex_acquire(raw, &raw mut owner_token),
            MUTEX_STATUS_ACQUIRED
        );
    });
    willow_sched_cancel(cancelled);

    with_current_task_for_test(cancelled, || {
        assert_eq!(
            willow_async_mutex_acquire(raw, &raw mut token),
            MUTEX_STATUS_CANCELLED
        );
    });

    assert_eq!(token, -1, "a cancelled acquire must not publish a token");
    assert_eq!(
        unsafe { mutex_from_raw(raw) }
            .expect("mutex")
            .waiter_count(),
        0,
        "a task the scheduler is cancelling must never join the wait queue"
    );
    assert!(lock_wait_link_of(cancelled).is_none());
    with_current_task_for_test(owner, || {
        assert_eq!(willow_async_mutex_release(raw, owner_token), 1);
    });
}

// 31
#[test]
fn a_cancellation_storm_leaves_the_lock_consistent() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let mutex = AsyncMutex::new(0, false);
    let owner = parked_task();
    let owner_token = acquire_now(&mutex, owner);
    let tasks: Vec<RuntimeTaskId> = (0..24).map(|_| parked_task()).collect();
    let barrier = Arc::new(Barrier::new(tasks.len()));

    std::thread::scope(|scope| {
        for &task in &tasks {
            let barrier = Arc::clone(&barrier);
            let mutex = &mutex;
            scope.spawn(move || {
                barrier.wait();
                for _ in 0..40 {
                    match mutex.acquire(task) {
                        MutexAcquire::Pending(_) => {
                            purge_task_lock_wait(task);
                        }
                        MutexAcquire::Acquired(token) => {
                            mutex.release(task, token);
                        }
                        // Reserved for this task by a concurrent release:
                        // hand it on rather than leaving it stranded.
                        MutexAcquire::Recursive => {
                            purge_task_lock_wait(task);
                        }
                        MutexAcquire::Ineligible => break,
                    }
                }
            });
        }
    });

    // Whatever the interleaving, the original owner still holds the lock:
    // no cancellation may take ownership away from a live owner.
    assert_eq!(mutex.owner(), Some((owner, owner_token)));
    for &task in &tasks {
        purge_task_lock_wait(task);
    }
    assert!(matches!(
        mutex.release(owner, owner_token),
        MutexRelease::Released { .. }
    ));
    for &task in &tasks {
        purge_task_lock_wait(task);
    }
    assert_eq!(mutex.waiter_count(), 0);
    assert_eq!(mutex.reclamation_status(), ReclamationStatus::Reclaimable);
}

// 33
#[test]
fn a_handoff_to_a_running_task_is_not_lost() {
    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();

    let mutex = AsyncMutex::new(0, false);
    let owner = parked_task();
    let waiter = parked_task();

    let owner_token = acquire_now(&mutex, owner);
    let waiter_token = queue_on(&mutex, waiter);

    // The waiter re-entered a poll before the handoff reached it, so the
    // wake cannot enqueue it. The reservation must still be made: the
    // waiter's own poll boundary is what picks it up.
    make_running(waiter);
    let MutexRelease::Released { handed_to } = mutex.release(owner, owner_token) else {
        panic!("owner should be able to release");
    };
    assert_eq!(handed_to, Some(waiter));
    assert_eq!(mutex.owner(), Some((waiter, waiter_token)));
    assert_eq!(
        mutex.poll_acquire(waiter, waiter_token),
        MutexResume::Acquired,
        "a wake that arrived during a poll must not strand the lock"
    );
    assert!(matches!(
        mutex.release(waiter, waiter_token),
        MutexRelease::Released { handed_to: None }
    ));
}

// ── Real-scheduler perspectives (36, 37, 38) ────────────────────────────
//
// Everything above drives the state machine directly, which is the right
// level for FIFO order, token validity, and compensation. It cannot show
// the property the stage is actually for: that a task which cannot get the
// lock *parks*, so one worker keeps running other work, and that the
// handoff is what brings it back. That needs the real run loop, real task
// records, and real poll returns.
//
// Cross-poll state lives in statics rather than in a frame, because
// `willow_sched_spawn` roots the frame pointer with the collector and these
// tests have no GC frame to give it. `runtime_test_guard()` serialises
// runtime tests, so the statics are effectively test-local; each test
// resets the ones it uses.
//
// Task creation is *chained* — each stage spawns the next from inside its
// own poll — so the interleaving is forced by causality instead of by the
// run queue's discipline. Nothing here depends on the order the scheduler
// happens to pop ready tasks in.

static SCHED_LOCK: AtomicUsize = AtomicUsize::new(0);
static SCHED_EVENTS: TestMutex<Vec<String>> = TestMutex::new(Vec::new());
static SCHED_HOLDER_TASK: AtomicU64 = AtomicU64::new(0);
static SCHED_HOLDER_TOKEN: AtomicI64 = AtomicI64::new(-1);
static SCHED_HOLDER_STEP: AtomicUsize = AtomicUsize::new(0);
static SCHED_WAITER_TOKEN: AtomicI64 = AtomicI64::new(-1);
static SCHED_WAITER_STEP: AtomicUsize = AtomicUsize::new(0);

fn sched_reset(is_ref: bool) -> *mut c_void {
    reset_global_scheduler_for_test();
    SCHED_EVENTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    SCHED_HOLDER_TASK.store(0, Ordering::SeqCst);
    SCHED_HOLDER_TOKEN.store(-1, Ordering::SeqCst);
    SCHED_HOLDER_STEP.store(0, Ordering::SeqCst);
    SCHED_WAITER_TOKEN.store(-1, Ordering::SeqCst);
    SCHED_WAITER_STEP.store(0, Ordering::SeqCst);
    CHAIN_REMAINING.store(0, Ordering::SeqCst);
    CHAIN_TOKENS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    let raw = willow_async_mutex_new(0, is_ref as i64);
    SCHED_LOCK.store(raw as usize, Ordering::SeqCst);
    raw
}

fn sched_lock() -> *mut c_void {
    SCHED_LOCK.load(Ordering::SeqCst) as *mut c_void
}

fn record(event: impl Into<String>) {
    SCHED_EVENTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(event.into());
}

fn events() -> Vec<String> {
    SCHED_EVENTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Holds the lock across a park: poll 1 acquires and returns Pending, poll 2
/// releases. Whoever wakes it decides when the critical section ends.
unsafe extern "C" fn poll_hold_across_a_park(_frame: *mut c_void) -> i32 {
    if SCHED_HOLDER_STEP.fetch_add(1, Ordering::SeqCst) == 0 {
        let mut token: i64 = -1;
        assert_eq!(
            willow_async_mutex_acquire(sched_lock(), &raw mut token),
            MUTEX_STATUS_ACQUIRED,
            "the first task to arrive takes the lock uncontended"
        );
        SCHED_HOLDER_TOKEN.store(token, Ordering::SeqCst);
        SCHED_HOLDER_TASK.store(willow_sched_current_task(), Ordering::SeqCst);
        record("holder acquired");
        // Spawned from inside the poll, so the waiter cannot possibly reach
        // the lock before it is held.
        willow_sched_spawn(poll_wait_for_the_lock, std::ptr::null_mut());
        RUNTIME_POLL_PENDING
    } else {
        record("holder releasing");
        assert_eq!(
            willow_async_mutex_release(sched_lock(), SCHED_HOLDER_TOKEN.load(Ordering::SeqCst)),
            1
        );
        RUNTIME_POLL_READY
    }
}

/// Parks on the held lock, and resumes only when the handoff wakes it.
unsafe extern "C" fn poll_wait_for_the_lock(_frame: *mut c_void) -> i32 {
    if SCHED_WAITER_STEP.fetch_add(1, Ordering::SeqCst) == 0 {
        let mut token: i64 = -1;
        assert_eq!(
            willow_async_mutex_acquire(sched_lock(), &raw mut token),
            MUTEX_STATUS_PENDING,
            "the lock is held, so this task must park rather than spin"
        );
        SCHED_WAITER_TOKEN.store(token, Ordering::SeqCst);
        record("waiter parked");
        // Only now does the bystander exist: any work it records provably
        // happened while this task was parked on the lock.
        willow_sched_spawn(poll_unrelated_work, std::ptr::null_mut());
        RUNTIME_POLL_PENDING
    } else {
        let token = SCHED_WAITER_TOKEN.load(Ordering::SeqCst);
        assert_eq!(
            willow_async_mutex_poll(sched_lock(), token),
            MUTEX_STATUS_ACQUIRED,
            "the resume must be the handoff, not a spurious wake"
        );
        record("waiter resumed");
        assert_eq!(willow_async_mutex_release(sched_lock(), token), 1);
        RUNTIME_POLL_READY
    }
}

/// Unrelated work that must complete while the waiter is parked, on the one
/// worker the waiter would otherwise be occupying. Then it ends the
/// critical section by waking the holder.
unsafe extern "C" fn poll_unrelated_work(_frame: *mut c_void) -> i32 {
    record("unrelated task ran to completion");
    willow_sched_wake(SCHED_HOLDER_TASK.load(Ordering::SeqCst));
    RUNTIME_POLL_READY
}

// 36
#[test]
fn one_worker_keeps_working_while_a_task_is_parked_on_the_lock() {
    let _guard = runtime_test_guard();
    let _single = single_worker_for_test();
    sched_reset(false);

    willow_sched_spawn(poll_hold_across_a_park, std::ptr::null_mut());
    let completed = willow_sched_run();

    assert_eq!(
        completed, 3,
        "holder, waiter and bystander must all finish: a parked waiter that \
         never got its handoff would leave the run short"
    );
    assert_eq!(
        events(),
        vec![
            "holder acquired",
            "waiter parked",
            "unrelated task ran to completion",
            "holder releasing",
            "waiter resumed",
        ],
        "the worker must make progress on other work between the park and \
         the handoff, and the waiter must not resume before the release"
    );

    let mutex = unsafe { mutex_from_raw(sched_lock()) }.expect("mutex");
    assert_eq!(mutex.owner(), None);
    assert_eq!(mutex.waiter_count(), 0);
    assert_eq!(mutex.reclamation_status(), ReclamationStatus::Reclaimable);
}

// ── 37: a chain of real tasks drains in registration order ──────────────

static CHAIN_REMAINING: AtomicUsize = AtomicUsize::new(0);
/// `(task id, token, polls so far)` per chained waiter, in the order they
/// registered on the lock.
static CHAIN_TOKENS: TestMutex<Vec<(u64, i64, usize)>> = TestMutex::new(Vec::new());

unsafe extern "C" fn poll_chain_waiter(_frame: *mut c_void) -> i32 {
    let task = willow_sched_current_task();
    let mut chain = CHAIN_TOKENS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let existing = chain.iter().position(|&(id, _, _)| id == task);

    match existing {
        None => {
            let mut token: i64 = -1;
            assert_eq!(
                willow_async_mutex_acquire(sched_lock(), &raw mut token),
                MUTEX_STATUS_PENDING,
                "the holder still owns the lock, so every chained task parks"
            );
            chain.push((task, token, 1));
            record(format!("registered {}", chain.len()));
            drop(chain);
            // Chain the next arrival from inside this poll so registration
            // order is fixed by causality, not by the run queue.
            if CHAIN_REMAINING.fetch_sub(1, Ordering::SeqCst) > 1 {
                willow_sched_spawn(poll_chain_waiter, std::ptr::null_mut());
            } else {
                willow_sched_spawn(poll_unrelated_work, std::ptr::null_mut());
            }
            RUNTIME_POLL_PENDING
        }
        Some(index) => {
            let (_, token, polls) = chain[index];
            chain[index].2 = polls + 1;
            drop(chain);
            assert_eq!(
                willow_async_mutex_poll(sched_lock(), token),
                MUTEX_STATUS_ACQUIRED,
                "waiter {index} was woken without owning the lock"
            );
            record(format!("acquired {}", index + 1));
            assert_eq!(willow_async_mutex_release(sched_lock(), token), 1);
            RUNTIME_POLL_READY
        }
    }
}

unsafe extern "C" fn poll_hold_then_start_a_chain(_frame: *mut c_void) -> i32 {
    if SCHED_HOLDER_STEP.fetch_add(1, Ordering::SeqCst) == 0 {
        let mut token: i64 = -1;
        assert_eq!(
            willow_async_mutex_acquire(sched_lock(), &raw mut token),
            MUTEX_STATUS_ACQUIRED
        );
        SCHED_HOLDER_TOKEN.store(token, Ordering::SeqCst);
        SCHED_HOLDER_TASK.store(willow_sched_current_task(), Ordering::SeqCst);
        willow_sched_spawn(poll_chain_waiter, std::ptr::null_mut());
        RUNTIME_POLL_PENDING
    } else {
        assert_eq!(
            willow_async_mutex_release(sched_lock(), SCHED_HOLDER_TOKEN.load(Ordering::SeqCst)),
            1
        );
        RUNTIME_POLL_READY
    }
}

// 37
#[test]
fn one_worker_drains_a_chain_of_real_tasks_in_registration_order() {
    let _guard = runtime_test_guard();
    let _single = single_worker_for_test();
    sched_reset(false);

    const WAITERS: usize = 16;
    CHAIN_REMAINING.store(WAITERS, Ordering::SeqCst);
    willow_sched_spawn(poll_hold_then_start_a_chain, std::ptr::null_mut());

    let completed = willow_sched_run();
    assert_eq!(
        completed,
        (WAITERS + 2) as i64,
        "holder, {WAITERS} waiters and the waker must all finish; a short \
         count means a task is still parked on a lock nobody handed on"
    );

    // Compare the two orders the log records rather than either against a
    // hardcoded sequence: whatever order the tasks happened to register in,
    // the lock must be granted in exactly that order.
    let log = events();
    let registration_order: Vec<&str> = log
        .iter()
        .filter_map(|event| event.strip_prefix("registered "))
        .collect();
    let acquisition_order: Vec<&str> = log
        .iter()
        .filter_map(|event| event.strip_prefix("acquired "))
        .collect();
    assert_eq!(registration_order.len(), WAITERS);
    assert_eq!(
        registration_order, acquisition_order,
        "the real scheduler must grant the lock in registration order"
    );

    // Every waiter was polled exactly twice: once to park, once to take the
    // handoff. A third poll would mean a spurious wake reached it.
    let chain = CHAIN_TOKENS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(chain.len(), WAITERS);
    for (index, &(_, _, polls)) in chain.iter().enumerate() {
        assert_eq!(polls, 2, "waiter {index} was polled {polls} times");
    }
    drop(chain);

    let mutex = unsafe { mutex_from_raw(sched_lock()) }.expect("mutex");
    assert_eq!(mutex.reclamation_status(), ReclamationStatus::Reclaimable);
}

// ── 38: a cancelled waiter is retired without stranding the lock ────────

static CANCEL_VICTIM: AtomicU64 = AtomicU64::new(0);

/// Parks on the lock and requests its own cancellation without cleaning up
/// its registration. Because this synthetic task has no compiler-generated
/// cancel entry or frame, the scheduler finalizes it at the next claim
/// without polling it again; terminal cleanup must remove the lock wait.
unsafe extern "C" fn poll_park_then_retire_if_cancelled(_frame: *mut c_void) -> i32 {
    let task = willow_sched_current_task();
    if CANCEL_VICTIM.load(Ordering::SeqCst) != task {
        CANCEL_VICTIM.store(task, Ordering::SeqCst);
        let mut token: i64 = -1;
        assert_eq!(
            willow_async_mutex_acquire(sched_lock(), &raw mut token),
            MUTEX_STATUS_PENDING
        );
        record("victim parked");
        willow_sched_spawn(poll_wait_for_the_lock, std::ptr::null_mut());
        willow_sched_cancel(task);
        return RUNTIME_POLL_PENDING;
    }
    panic!("a cancelled task without a cleanup entry must not be polled again")
}

unsafe extern "C" fn poll_hold_then_start_a_cancel(_frame: *mut c_void) -> i32 {
    if SCHED_HOLDER_STEP.fetch_add(1, Ordering::SeqCst) == 0 {
        let mut token: i64 = -1;
        assert_eq!(
            willow_async_mutex_acquire(sched_lock(), &raw mut token),
            MUTEX_STATUS_ACQUIRED
        );
        SCHED_HOLDER_TOKEN.store(token, Ordering::SeqCst);
        SCHED_HOLDER_TASK.store(willow_sched_current_task(), Ordering::SeqCst);
        willow_sched_spawn(poll_park_then_retire_if_cancelled, std::ptr::null_mut());
        RUNTIME_POLL_PENDING
    } else {
        record("holder releasing");
        assert_eq!(
            willow_async_mutex_release(sched_lock(), SCHED_HOLDER_TOKEN.load(Ordering::SeqCst)),
            1
        );
        RUNTIME_POLL_READY
    }
}

// 38
#[test]
fn a_cancelled_waiter_is_retired_without_stranding_the_lock() {
    let _guard = runtime_test_guard();
    let _single = single_worker_for_test();
    sched_reset(false);
    CANCEL_VICTIM.store(0, Ordering::SeqCst);

    willow_sched_spawn(poll_hold_then_start_a_cancel, std::ptr::null_mut());
    let completed = willow_sched_run();

    let log = events();
    assert_eq!(
        completed, 3,
        "the holder, waiter behind the victim, and waker must finish; the \
         cancelled victim is finalized but is not counted as completed: {log:?}"
    );
    assert!(
        !log.contains(&"victim retired".to_string()),
        "a cancelled task without a cleanup entry must not be polled again: {log:?}"
    );
    assert!(
        log.contains(&"waiter resumed".to_string()),
        "the waiter queued behind the cancelled one must still get the \
         lock — a stranded registration would park it forever: {log:?}"
    );

    let mutex = unsafe { mutex_from_raw(sched_lock()) }.expect("mutex");
    let victim = CANCEL_VICTIM.load(Ordering::SeqCst);
    assert_ne!(victim, 0);
    assert_eq!(
        crate::scheduler::willow_sched_task_state(victim),
        -1,
        "the cancelled victim must have been finalized and reaped"
    );
    assert_eq!(
        purge_task_lock_wait(victim),
        MutexCancel::NoLink,
        "terminal cleanup must have removed the victim's reverse link"
    );
    assert_eq!(mutex.owner(), None);
    assert_eq!(mutex.waiter_count(), 0);
    assert_eq!(mutex.reclamation_status(), ReclamationStatus::Reclaimable);
}

// 32
#[test]
fn ownership_is_task_based_and_survives_a_worker_change() {
    let _guard = runtime_test_guard();
    let _single = single_worker_for_test();
    reset_global_scheduler_for_test();

    let mutex = AsyncMutex::new(0, false);
    let owner = ready_task();
    let waiter = parked_task();

    let owner_token = acquire_now(&mutex, owner);
    let waiter_token = queue_on(&mutex, waiter);

    // Acquired on this thread, released from another: a native-thread
    // notion of ownership would reject this, but a Willow task can be
    // preempted mid-critical-section and resumed on a different worker.
    let handed_to = std::thread::scope(|scope| {
        let mutex = &mutex;
        scope
            .spawn(move || match mutex.release(owner, owner_token) {
                MutexRelease::Released { handed_to } => handed_to,
                MutexRelease::NotOwner => panic!("ownership must not be thread-local"),
            })
            .join()
            .unwrap()
    });
    assert_eq!(handed_to, Some(waiter));

    // A single worker still drains the chain to completion.
    assert_eq!(
        mutex.poll_acquire(waiter, waiter_token),
        MutexResume::Acquired
    );
    assert!(matches!(
        mutex.release(waiter, waiter_token),
        MutexRelease::Released { handed_to: None }
    ));
    assert_eq!(mutex.reclamation_status(), ReclamationStatus::Reclaimable);
}

// 42, 43
#[test]
fn invalid_status_diagnostic_names_the_status_and_phase() {
    assert_eq!(
        invalid_status_message(77, MUTEX_STATUS_PHASE_ACQUIRE),
        "async mutex returned unknown status 77 during acquire"
    );
    assert_eq!(
        invalid_status_message(-99, MUTEX_STATUS_PHASE_POLL),
        "async mutex returned unknown status -99 during poll"
    );
}

#[test]
fn invalid_status_diagnostic_fails_closed_on_unknown_phase() {
    assert_eq!(
        invalid_status_message(7, 44),
        "async mutex returned unknown status 7 during unknown phase"
    );
}

/// Manual Stage 7 benchmark. Keep it in-tree so the exact workload and all
/// requested measurements are reproducible without an external harness:
///
/// `cargo test -p willow_runtime --release scheduler_aware_lock_benchmark -- --ignored --nocapture`
#[test]
#[ignore = "manual scheduler-aware lock latency/throughput benchmark"]
fn scheduler_aware_lock_benchmark_reports_stage7_metrics() {
    fn percentile(sorted: &[u128], percent: usize) -> u128 {
        sorted[(sorted.len() - 1) * percent / 100]
    }

    let _guard = runtime_test_guard();
    reset_global_scheduler_for_test();
    const OPERATIONS: usize = 100_000;
    const WAITERS: usize = 10_000;

    let task = parked_task();
    let mutex = AsyncMutex::new(0, false);
    let started = Instant::now();
    for value in 0..OPERATIONS {
        let token = acquire_now(&mutex, task);
        assert!(mutex.commit(task, token, value as i64));
        assert!(matches!(
            mutex.release(task, token),
            MutexRelease::Released { handed_to: None }
        ));
    }
    let mutex_elapsed = started.elapsed();

    let rwlock = AsyncRwLock::new(0, false);
    let started = Instant::now();
    for value in 0..OPERATIONS {
        let RwAcquire::Acquired(token) = rwlock.acquire(task, LockAccess::Write) else {
            panic!("uncontended writer must acquire");
        };
        assert!(rwlock.commit(task, token, value as i64));
        assert!(matches!(
            rwlock.release(task, token),
            RwRelease::Released { woken: 0 }
        ));
    }
    let rwlock_elapsed = started.elapsed();

    let contended = AsyncMutex::new(0, false);
    let owner = parked_task();
    let owner_token = acquire_now(&contended, owner);
    let waiters: Vec<_> = (0..WAITERS)
        .map(|_| {
            let task = parked_task();
            (task, queue_on(&contended, task))
        })
        .collect();
    let max_waiters = contended.waiter_count();
    let mut acquisition_ns = Vec::with_capacity(WAITERS);
    let mut hold_ns = Vec::with_capacity(WAITERS);
    let mut current = (owner, owner_token);
    for (task, token) in waiters {
        let acquire_started = Instant::now();
        assert!(matches!(
            contended.release(current.0, current.1),
            MutexRelease::Released {
                handed_to: Some(next)
            } if next == task
        ));
        assert_eq!(contended.poll_acquire(task, token), MutexResume::Acquired);
        acquisition_ns.push(acquire_started.elapsed().as_nanos());
        let hold_started = Instant::now();
        assert!(contended.commit(task, token, 1));
        hold_ns.push(hold_started.elapsed().as_nanos());
        current = (task, token);
    }
    assert!(matches!(
        contended.release(current.0, current.1),
        MutexRelease::Released { handed_to: None }
    ));
    acquisition_ns.sort_unstable();
    hold_ns.sort_unstable();

    willow_gc_init();
    let memory_before = crate::gc::willow_gc_allocated_bytes();
    let drops_before = ASYNC_MUTEX_DROP_COUNT.load(Ordering::SeqCst);
    let handles: Vec<_> = (0..WAITERS)
        .map(|value| {
            let handle = willow_async_mutex_new(value as i64, 0).cast::<u8>();
            crate::gc::willow_gc_add_runtime_root(handle);
            handle
        })
        .collect();
    let memory_peak = crate::gc::willow_gc_allocated_bytes();
    for handle in handles {
        crate::gc::willow_gc_remove_runtime_root(handle);
    }
    willow_gc_collect();
    let memory_after = crate::gc::willow_gc_allocated_bytes();
    let reclaimed = ASYNC_MUTEX_DROP_COUNT.load(Ordering::SeqCst) - drops_before;
    assert_eq!(reclaimed, WAITERS);

    eprintln!(
        "stage7-lock-benchmark mutex_ops={OPERATIONS} mutex_ops_per_sec={:.0} \
         rw_write_ops_per_sec={:.0} waiters={WAITERS} parks={WAITERS} wakes={WAITERS} \
         max_waiters={max_waiters} acquire_ns_p50={} acquire_ns_p99={} acquire_ns_max={} \
         hold_ns_p50={} hold_ns_p99={} hold_ns_max={} memory_before={} memory_peak={} \
         memory_after={} handles_reclaimed={reclaimed}",
        OPERATIONS as f64 / mutex_elapsed.as_secs_f64(),
        OPERATIONS as f64 / rwlock_elapsed.as_secs_f64(),
        percentile(&acquisition_ns, 50),
        percentile(&acquisition_ns, 99),
        acquisition_ns[WAITERS - 1],
        percentile(&hold_ns, 50),
        percentile(&hold_ns, 99),
        hold_ns[WAITERS - 1],
        memory_before,
        memory_peak,
        memory_after,
    );
}
