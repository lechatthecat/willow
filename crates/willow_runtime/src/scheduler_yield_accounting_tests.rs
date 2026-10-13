//! Focused invariants for per-yield accounting (willow-8hq4.26).
use super::*;
use std::sync::Barrier;

#[test]
fn yield_accounting_colliding_shards_keep_claims_and_paused_polls_visible() {
    let _guard = crate::gc::runtime_test_guard();
    reset_global_scheduler_for_test();
    let state = ParallelRunState::default();
    let threads = ACCOUNTING_SHARDS + 8;
    let entered = Barrier::new(threads + 1);
    let release = Barrier::new(threads + 1);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut claim = ClaimInFlight::enter();
                claim.popped();
                state.begin_poll();
                state.pause_active_poll();
                entered.wait();
                release.wait();
                state.resume_paused_poll();
                state.end_poll();
            });
        }
        entered.wait();
        assert_eq!(claim_word() & CLAIM_COUNT_MASK, threads as u64);
        assert_eq!(state.active_polls(), 0);
        assert_eq!(state.paused_polls(), threads);
        assert!(state.has_live_poll());
        assert!(!state.publish_stop_if(|_| true));
        release.wait();
    });
    assert!(!claims_in_flight());
    assert!(!state.has_live_poll());
    // Each thread resolves its claim and its pause/resume transitions.
    assert_eq!(claim_word() >> CLAIM_COUNT_BITS, 3 * threads as u64);
    assert!(state.publish_stop_if(|state| !state.has_live_poll()));
}

#[test]
fn yield_accounting_claim_guard_retains_origin_shard_when_moved() {
    let _guard = crate::gc::runtime_test_guard();
    reset_global_scheduler_for_test();
    let mut claim = ClaimInFlight::enter();
    claim.popped();
    std::thread::spawn(move || drop(claim)).join().unwrap();
    assert!(!claims_in_flight());
    assert_eq!(claim_word() >> CLAIM_COUNT_BITS, 1);
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
            scope.spawn(|| queues.push_global(9_999)).join().unwrap();
        });
        assert_eq!(queues.pop_for_worker(0), Some(9_999));
        // Keep local work perpetually replenished while foreign wakes arrive.
        for _ in 0..100 {
            let local = queues.pop_for_worker(0).unwrap();
            queues.push_local(0, local);
            std::thread::scope(|scope| {
                scope.spawn(|| queues.push_global(9_999)).join().unwrap();
            });
            assert_eq!(queues.pop_for_worker(0), Some(9_999));
        }
        assert!(queues.pop_for_worker(1).is_some());
        assert_eq!(queues.metrics_snapshot().steal_successes, 1);
    });
}

#[test]
fn yield_accounting_counts_scale_with_yields_not_workers_or_tasks() {
    let _guard = crate::gc::runtime_test_guard();
    for workers in [1, 2, 8] {
        for tasks in [1_000, 10_000, 100_000] {
            reset_global_scheduler_for_test();
            let state = ParallelRunState::default();
            let queues = RunQueues::new(workers);
            std::thread::scope(|scope| {
                for worker in 0..workers {
                    let state = &state;
                    let queues = &queues;
                    scope.spawn(move || {
                        queues.with_owner(worker, || {
                            // Each task contributes one representative nonterminal
                            // yield. Keep one local token per worker: no stealing.
                            queues.push_local(worker, worker as u64);
                            for _ in (worker..tasks).step_by(workers) {
                                let mut claim = ClaimInFlight::enter();
                                let id = queues.pop_for_worker(worker).unwrap();
                                claim.popped();
                                state.begin_poll();
                                drop(claim);
                                queues.push_local(worker, id);
                                state.end_poll();
                            }
                        })
                    });
                }
            });
            assert!(!state.has_live_poll());
            let words = CLAIM_WORD.snapshot();
            assert!(words.iter().all(|word| word & CLAIM_COUNT_MASK == 0));
            assert_eq!(claim_word() >> CLAIM_COUNT_BITS, tasks as u64);
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
    let _guard = crate::gc::runtime_test_guard();
    reset_global_scheduler_for_test();
    for _ in 0..ACCOUNTING_SHARDS {
        let state = ParallelRunState::default();
        assert!(!state.publish_stop_if(|_| {
            std::thread::spawn(|| {
                let mut claim = ClaimInFlight::enter();
                claim.popped();
            })
            .join()
            .unwrap();
            true
        }));
        assert!(!state.stop.load(Ordering::Acquire));
        assert!(!claims_in_flight());
    }
}

#[test]
fn yield_accounting_liveness_retries_claim_handoffs_and_excludes_caller() {
    let _guard = crate::gc::runtime_test_guard();
    reset_global_scheduler_for_test();
    let state = ParallelRunState::default();
    state.begin_poll();
    assert!(!state.has_active_peer(1));
    assert!(state.has_active_peer(0));
    let before = CLAIM_WORD.snapshot();
    std::thread::spawn(|| {
        let mut claim = ClaimInFlight::enter();
        claim.popped();
    })
    .join()
    .unwrap();
    assert!(claims_changed_or_in_flight(&before));
    assert!(!state.has_active_peer(1));
    let claim = ClaimInFlight::enter();
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
    let _guard = crate::gc::runtime_test_guard();
    reset_global_scheduler_for_test();
    let state = ParallelRunState::default();
    std::thread::scope(|scope| {
        let mut peers = Vec::new();
        for _ in 0..2 {
            let (command_tx, command_rx) = std::sync::mpsc::channel();
            let (ack_tx, ack_rx) = std::sync::mpsc::channel();
            let state = &state;
            scope.spawn(move || {
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
            });
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
