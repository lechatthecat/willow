//! Focused invariants for per-yield accounting.
use super::*;
use std::sync::Barrier;

#[test]
fn yield_accounting_colliding_shards_keep_claims_and_paused_polls_visible() {
    let state = ParallelRunState::new(Arc::new(AccountingWords::new()));
    let threads = ACCOUNTING_SHARDS + 8;
    let entered = Barrier::new(threads + 1);
    let release = Barrier::new(threads + 1);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(crate::runtime::serving(|| {
                let mut claim = ClaimInFlight::enter_in(&state.claims);
                claim.popped();
                state.begin_poll();
                state.pause_active_poll();
                entered.wait();
                release.wait();
                state.resume_paused_poll();
                state.end_poll();
            }));
        }
        entered.wait();
        assert_eq!(
            state
                .claims
                .snapshot()
                .into_iter()
                .fold(0, u64::wrapping_add)
                & CLAIM_COUNT_MASK,
            threads as u64
        );
        assert_eq!(state.active_polls(), 0);
        assert_eq!(state.paused_polls(), threads);
        assert!(state.has_live_poll());
        assert!(!state.publish_stop_if(|_| true));
        release.wait();
    });
    assert!(!state.claims.in_flight());
    assert!(!state.has_live_poll());
    // Each thread resolves its claim and its pause/resume transitions.
    assert_eq!(
        state
            .claims
            .snapshot()
            .into_iter()
            .fold(0, u64::wrapping_add)
            >> CLAIM_COUNT_BITS,
        3 * threads as u64
    );
    assert!(state.publish_stop_if(|state| !state.has_live_poll()));
}

#[test]
fn yield_accounting_claim_guard_retains_origin_shard_when_moved() {
    let state = ParallelRunState::new(Arc::new(AccountingWords::new()));
    let mut claim = ClaimInFlight::enter_in(&state.claims);
    claim.popped();
    std::thread::scope(|scope| {
        scope
            .spawn(crate::runtime::serving(move || drop(claim)))
            .join()
            .unwrap()
    });
    assert!(!state.claims.in_flight());
    assert_eq!(
        state
            .claims
            .snapshot()
            .into_iter()
            .fold(0, u64::wrapping_add)
            >> CLAIM_COUNT_BITS,
        1
    );
}

#[test]
fn yield_accounting_foreign_publication_gets_next_global_turn() {
    let queues = RunQueues::new(2);
    queues.with_owner(0, || {
        for id in 1..=1_000 {
            queues.push_local(0, id);
        }
        assert_eq!(queues.pop_for_worker(0), Some(1));
        std::thread::scope(|scope| {
            scope
                .spawn(crate::runtime::serving(|| queues.push_global(9_999)))
                .join()
                .unwrap();
        });
        assert_eq!(queues.pop_for_worker(0), Some(9_999));
        // Keep local work perpetually replenished while foreign wakes arrive.
        for _ in 0..100 {
            let local = queues.pop_for_worker(0).unwrap();
            queues.push_local(0, local);
            std::thread::scope(|scope| {
                scope
                    .spawn(crate::runtime::serving(|| queues.push_global(9_999)))
                    .join()
                    .unwrap();
            });
            assert_eq!(queues.pop_for_worker(0), Some(9_999));
        }
        assert!(queues.pop_for_worker(1).is_some());
        assert_eq!(queues.metrics_snapshot().steal_successes, 1);
    });
}

#[test]
fn yield_accounting_counts_scale_with_yields_not_workers_or_tasks() {
    for workers in [1, 2, 8] {
        for tasks in [1_000, 10_000, 100_000] {
            let state = ParallelRunState::new(Arc::new(AccountingWords::new()));
            let queues = RunQueues::new(workers);
            std::thread::scope(|scope| {
                for worker in 0..workers {
                    let state = &state;
                    let queues = &queues;
                    scope.spawn(crate::runtime::serving(move || {
                        queues.with_owner(worker, || {
                            // Each task contributes one representative nonterminal
                            // yield. Keep one local token per worker: no stealing.
                            queues.push_local(worker, worker as u64);
                            for _ in (worker..tasks).step_by(workers) {
                                let mut claim = ClaimInFlight::enter_in(&state.claims);
                                let id = queues.pop_for_worker(worker).unwrap();
                                claim.popped();
                                state.begin_poll();
                                drop(claim);
                                queues.push_local(worker, id);
                                state.end_poll();
                            }
                        })
                    }));
                }
            });
            assert!(!state.has_live_poll());
            let words = state.claims.snapshot();
            assert!(words.iter().all(|word| word & CLAIM_COUNT_MASK == 0));
            assert_eq!(
                state
                    .claims
                    .snapshot()
                    .into_iter()
                    .fold(0, u64::wrapping_add)
                    >> CLAIM_COUNT_BITS,
                tasks as u64
            );
            let counters = queues.metrics_snapshot();
            assert_eq!(counters.local_pop_hits, tasks as u64);
            assert_eq!(counters.local_pushes, (tasks + workers) as u64);
            assert_eq!(counters.steal_attempts, 0);
            let probes: usize = (0..workers)
                .map(|worker| (worker..tasks).step_by(workers).count() / 2)
                .sum();
            assert_eq!(counters.global_pop_attempts, probes as u64);
            println!(
                "tasks={tasks} workers={workers} claims={tasks} poll_pairs={tasks} local_pops={} local_pushes={} global_probes={}",
                counters.local_pop_hits,
                counters.local_pushes - workers as u64,
                counters.global_pop_attempts
            );
        }
    }
}

#[test]
fn yield_accounting_foreign_resolved_claim_invalidates_every_shard_snapshot() {
    for _ in 0..ACCOUNTING_SHARDS {
        let state = ParallelRunState::new(Arc::new(AccountingWords::new()));
        assert!(!state.publish_stop_if(|_| {
            std::thread::scope(|scope| {
                scope
                    .spawn(crate::runtime::serving(|| {
                        let mut claim = ClaimInFlight::enter_in(&state.claims);
                        claim.popped();
                    }))
                    .join()
                    .unwrap();
            });
            true
        }));
        assert!(!state.stop.load(Ordering::Acquire));
        assert!(!state.claims.in_flight());
    }
}

#[test]
fn yield_accounting_liveness_retries_claim_handoffs_and_excludes_caller() {
    let state = ParallelRunState::new(Arc::new(AccountingWords::new()));
    state.begin_poll();
    assert!(!state.has_active_peer(1));
    assert!(state.has_active_peer(0));
    let before = state.claims.snapshot();
    std::thread::scope(|scope| {
        scope
            .spawn(crate::runtime::serving(|| {
                let mut claim = ClaimInFlight::enter_in(&state.claims);
                claim.popped();
            }))
            .join()
            .unwrap();
    });
    assert!(state.claims.changed_or_in_flight(&before));
    assert!(!state.has_active_peer(1));
    let claim = ClaimInFlight::enter_in(&state.claims);
    assert!(
        state.has_active_peer(1),
        "an unresolved peer claim can make progress"
    );
    drop(claim);
    assert!(!state.has_active_peer(1));
    state.end_poll();
    assert!(!state.has_active_peer(0));
}

#[test]
fn yield_accounting_pause_resume_handoff_cannot_hide_active_peer() {
    let state = ParallelRunState::new(Arc::new(AccountingWords::new()));
    std::thread::scope(|scope| {
        let mut peers = Vec::new();
        for _ in 0..2 {
            let (command_tx, command_rx) = std::sync::mpsc::channel();
            let (ack_tx, ack_rx) = std::sync::mpsc::channel();
            let state = &state;
            scope.spawn(crate::runtime::serving(move || {
                state.begin_poll();
                ack_tx.send(ACCOUNTING_SHARD.with(|index| *index)).unwrap();
                let mut paused = false;
                while let Ok(pause) = command_rx.recv() {
                    if pause {
                        state.pause_active_poll();
                    } else {
                        state.resume_paused_poll();
                    }
                    paused = pause;
                    ack_tx.send(0).unwrap();
                }
                if paused {
                    state.resume_paused_poll();
                }
                state.end_poll();
            }));
            let index = ack_rx.recv().unwrap();
            peers.push((index, command_tx, ack_rx));
        }
        peers.sort_by_key(|peer| peer.0);
        let earlier = &peers[0];
        let later = &peers[1];
        assert_ne!(earlier.0, later.0);
        let caller = (0..ACCOUNTING_SHARDS)
            .find(|&index| index != earlier.0 && index != later.0)
            .unwrap();
        state.polls.0[caller].0.store(1, Ordering::Release);
        earlier.1.send(true).unwrap();
        earlier.2.recv().unwrap();
        let mut observed = 0;
        let live = state.has_active_peer_during(1, || {
            for (index, word) in state.polls.0.iter().enumerate() {
                observed += (word.0.load(Ordering::Acquire) & POLL_ACTIVE_MASK) as usize;
                if index == earlier.0 {
                    // Earlier was read paused. Resume it before pausing later:
                    // another peer remains active throughout the handoff.
                    earlier.1.send(false).unwrap();
                    earlier.2.recv().unwrap();
                    later.1.send(true).unwrap();
                    later.2.recv().unwrap();
                }
            }
            observed
        });
        assert_eq!(observed, 1, "the forced torn scan saw only the caller");
        // Release peers before assertions, including on regression failure.
        state.polls.0[caller].0.store(0, Ordering::Release);
        drop(peers);
        assert!(
            live,
            "pause/resume handoff must invalidate the liveness scan"
        );
    });
}

#[test]
fn private_scheduler_claims_isolate_stop_decisions_and_outlive_service() {
    let first = SchedulerServices::new(2);
    let second = SchedulerServices::new(2);
    let first_claims = Arc::downgrade(&first.components().claims);
    let first_run = ParallelRunState::new(Arc::clone(&first.components().claims));
    let nested_run = ParallelRunState::new(Arc::clone(&first.components().claims));
    let second_run = ParallelRunState::new(Arc::clone(&second.components().claims));
    let entered = Barrier::new(2);
    let release = Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(crate::runtime::serving(|| {
            let mut claim = ClaimInFlight::enter_in(&first_run.claims);
            claim.popped();
            entered.wait();
            release.wait();
        }));
        entered.wait();
        let first_stopped = first_run.publish_stop_if(|_| true);
        let nested_stopped = nested_run.publish_stop_if(|_| true);
        let second_stopped = second_run.publish_stop_if(|_| true);
        release.wait();
        assert!(!first_stopped);
        assert!(
            !nested_stopped,
            "nested drives must see their owner's claims"
        );
        assert!(second_stopped, "a peer runtime must not prevent quiescence");
    });
    assert!(first_run.publish_stop_if(|_| true));
    drop(first);
    assert!(first_claims.upgrade().is_some());
    assert!(!nested_run.claims.in_flight());
    nested_run.begin_poll();
    nested_run.pause_active_poll();
    assert_eq!(nested_run.paused_polls(), 1);
    nested_run.resume_paused_poll();
    nested_run.end_poll();
    assert!(nested_run.publish_stop_if(|_| true));
    drop(first_run);
    drop(nested_run);
    assert!(first_claims.upgrade().is_none());
    // The surviving scheduler still owns usable accounting after peer teardown.
    let survivor = ParallelRunState::new(Arc::clone(&second.components().claims));
    {
        let _claim = ClaimInFlight::enter_in(&survivor.claims);
        assert!(!survivor.publish_stop_if(|_| true));
    }
    assert!(survivor.publish_stop_if(|_| true));
}

#[test]
fn private_scheduler_claim_epochs_ignore_peer_changes_but_detect_owner_changes() {
    let first = SchedulerServices::new(1);
    let second = SchedulerServices::new(1);
    let state = ParallelRunState::new(Arc::clone(&first.components().claims));
    assert!(state.publish_stop_if(|_| {
        let mut peer = ClaimInFlight::enter_in(&second.components().claims);
        peer.popped();
        true
    }));
    let state = ParallelRunState::new(Arc::clone(&first.components().claims));
    assert!(!state.publish_stop_if(|_| {
        let mut own = ClaimInFlight::enter_in(&first.components().claims);
        own.popped();
        true
    }));
    assert!(state.publish_stop_if(|_| true));
}
