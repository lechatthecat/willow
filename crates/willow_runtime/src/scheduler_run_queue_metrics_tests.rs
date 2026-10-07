use super::*;
use crate::observability::RunQueueMetricsSnapshot;

#[test]
fn fresh_queue_and_snapshot_reads_have_no_events() {
    let queues = RunQueues::new(4);
    for _ in 0..100 {
        assert_eq!(
            queues.metrics_snapshot(),
            RunQueueMetricsSnapshot::default()
        );
    }
}

#[test]
fn global_push_and_pop_count_hits_and_empty_visits() {
    let queues = RunQueues::new(1);
    queues.push_global(1);
    queues.push_global(2);
    assert_eq!(queues.pop_for_worker(0), Some(1));
    assert_eq!(queues.pop_for_worker(0), Some(2));
    assert_eq!(queues.pop_for_worker(0), None);
    assert_eq!(
        queues.metrics_snapshot(),
        RunQueueMetricsSnapshot {
            global_pushes: 2,
            global_pop_hits: 2,
            global_pop_attempts: 3,
            steal_attempts: 1,
            steal_failures: 1,
            ..Default::default()
        }
    );
}

#[test]
fn local_fifo_and_alternating_global_priority_are_unchanged() {
    let queues = RunQueues::new(2);
    queues.push_local(0, 1);
    queues.push_local(0, 2);
    queues.push_global(3);
    assert_eq!(queues.pop_for_worker(0), Some(1));
    assert_eq!(queues.pop_for_worker(0), Some(3));
    assert_eq!(queues.pop_for_worker(0), Some(2));
    assert_eq!(
        queues.metrics_snapshot(),
        RunQueueMetricsSnapshot {
            local_pushes: 2,
            local_pop_hits: 2,
            global_pushes: 1,
            global_pop_hits: 1,
            global_pop_attempts: 1,
            ..Default::default()
        }
    );
}

#[test]
fn preferred_empty_global_falls_back_to_local() {
    let queues = RunQueues::new(2);
    queues.push_local(0, 1);
    queues.push_local(0, 2);
    assert_eq!(queues.pop_for_worker(0), Some(1));
    assert_eq!(queues.pop_for_worker(0), Some(2));
    let metrics = queues.metrics_snapshot();
    assert_eq!(metrics.local_pop_hits, 2);
    assert_eq!(metrics.global_pop_attempts, 1);
    assert_eq!(metrics.global_pop_hits, 0);
    assert_eq!(metrics.steal_attempts, 0);
}

#[test]
fn invalid_local_publisher_counts_actual_global_destination() {
    let queues = RunQueues::new(2);
    queues.push_local(2, 1);
    assert_eq!(queues.metrics_snapshot().global_pushes, 1);
    assert_eq!(queues.metrics_snapshot().local_pushes, 0);
    assert_eq!(queues.pop_for_worker(0), Some(1));
}

#[test]
fn steal_counts_visited_victims_and_preserves_fifo() {
    let queues = RunQueues::new(4);
    queues.push_local(2, 1);
    queues.push_local(2, 2);
    assert_eq!(queues.pop_for_worker(0), Some(1));
    assert_eq!(queues.pop_for_worker(0), Some(2));
    assert_eq!(
        queues.metrics_snapshot(),
        RunQueueMetricsSnapshot {
            local_pushes: 2,
            global_pop_attempts: 2,
            steal_attempts: 2,
            steal_successes: 2,
            // Lock-free steals acquire no victim mutex.
            victim_locks: 0,
            ..Default::default()
        }
    );
}

#[test]
fn empty_scan_and_last_victim_counts_scale_with_workers_and_attempts() {
    // Exact work, not a wall-clock inference: A scans visit A*(W-1) victims
    // but lock none of them, because every length hint reads empty
    // (willow-8hq4.19). A successful last-victim scan locks only that victim.
    for workers in [1, 2, 4, 8, 16, 32] {
        for attempts in [1, 8, 64] {
            let queues = RunQueues::new(workers);
            for _ in 0..attempts {
                assert_eq!(queues.pop_for_worker(0), None);
            }
            let metrics = queues.metrics_snapshot();
            assert_eq!(metrics.global_pop_attempts, attempts);
            assert_eq!(metrics.steal_attempts, attempts);
            assert_eq!(metrics.steal_failures, attempts);
            assert_eq!(metrics.victim_locks, 0);
            assert_eq!(metrics.steal_successes, 0);
            println!(
                "workers={workers} scans={attempts} victim_locks={} global_pop_attempts={}",
                metrics.victim_locks, metrics.global_pop_attempts
            );
            if workers > 1 {
                queues.push_local(workers - 1, 42);
                assert_eq!(queues.pop_for_worker(0), Some(42));
                let after = queues.metrics_snapshot();
                assert_eq!(after.victim_locks, 0);
                assert_eq!(after.steal_successes, 1);
                assert_eq!(after.steal_failures, attempts);
            }
        }
    }
}

#[test]
fn queue_depth_does_not_multiply_hit_accounting() {
    for depth in [1, 16, 256, 4096] {
        let queues = RunQueues::new(8);
        for id in 1..=depth {
            queues.push_local(0, id);
        }
        for id in 1..=depth {
            assert_eq!(queues.pop_for_worker(0), Some(id));
        }
        let metrics = queues.metrics_snapshot();
        assert_eq!(metrics.local_pushes, depth);
        assert_eq!(metrics.local_pop_hits, depth);
        assert_eq!(metrics.global_pop_attempts, depth / 2);
        assert_eq!(metrics.steal_attempts, 0);
        assert_eq!(metrics.victim_locks, 0);
    }
}

#[test]
fn maintenance_does_not_count_as_scheduling_or_reset_counters() {
    let queues = RunQueues::new(2);
    queues.push_global(1);
    queues.push_local(1, 2);
    let before = queues.metrics_snapshot();
    assert_eq!(queues.len(), 2);
    assert!(queues.contains(1));
    assert!(queues.remove(1));
    queues.clear();
    assert_eq!(queues.len(), 0);
    assert_eq!(queues.metrics_snapshot(), before);
    assert_eq!(
        RunQueues::new(2).metrics_snapshot(),
        RunQueueMetricsSnapshot::default()
    );
}

#[test]
fn concurrent_publishers_and_stealers_do_not_lose_events() {
    let queues = RunQueues::new(8);
    std::thread::scope(|scope| {
        for worker in 0..8 {
            let queues = &queues;
            scope.spawn(move || {
                for id in 1..=256 {
                    queues.push_local(worker, (worker * 256 + id) as u64);
                    queues.push_global((2048 + worker * 256 + id) as u64);
                }
            });
        }
    });
    let popped = std::sync::atomic::AtomicU64::new(0);
    std::thread::scope(|scope| {
        for worker in 0..8 {
            let queues = &queues;
            let popped = &popped;
            scope.spawn(move || {
                while queues.pop_for_worker(worker).is_some() {
                    popped.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
    let metrics = queues.metrics_snapshot();
    assert_eq!(popped.load(Ordering::Relaxed), 4096);
    assert_eq!(metrics.global_pushes, 2048);
    assert!(metrics.local_pushes >= 2048);
    assert_eq!(
        metrics.global_pop_hits + metrics.local_pop_hits + metrics.steal_successes,
        4096
    );
    assert_eq!(
        metrics.local_pop_hits + metrics.steal_successes,
        metrics.local_pushes
    );
    assert_eq!(
        metrics.steal_attempts,
        metrics.steal_successes + metrics.steal_failures
    );
}

#[test]
fn public_snapshot_and_scalar_getters_read_active_queue() {
    let _guard = crate::gc::runtime_test_guard();
    reset_global_scheduler_for_test();
    let queues = global_run_queues();
    queues.push_global(1);
    queues.push_local(0, 2);
    assert_eq!(queues.pop_for_worker(0), Some(2));
    assert_eq!(queues.pop_for_worker(0), Some(1));
    assert_eq!(queues.pop_for_worker(0), None);
    let snapshot = crate::observability::run_queue_metrics_snapshot();
    assert_eq!(snapshot, queues.metrics_snapshot());
    use crate::observability::*;
    assert_eq!(willow_sched_global_pushes(), snapshot.global_pushes as i64);
    assert_eq!(willow_sched_local_pushes(), snapshot.local_pushes as i64);
    assert_eq!(
        willow_sched_global_pop_hits(),
        snapshot.global_pop_hits as i64
    );
    assert_eq!(
        willow_sched_local_pop_hits(),
        snapshot.local_pop_hits as i64
    );
    assert_eq!(
        willow_sched_global_pop_attempts(),
        snapshot.global_pop_attempts as i64
    );
    assert_eq!(
        willow_sched_steal_attempts(),
        snapshot.steal_attempts as i64
    );
    assert_eq!(
        willow_sched_steal_successes(),
        snapshot.steal_successes as i64
    );
    assert_eq!(
        willow_sched_steal_failures(),
        snapshot.steal_failures as i64
    );
    assert_eq!(willow_sched_victim_locks(), snapshot.victim_locks as i64);
}

#[test]
fn global_batch_preserves_fifo_and_counts_ids_at_increasing_sizes() {
    for size in [1, 16, 256, 4096, 100_000] {
        let queues = RunQueues::new(1);
        let ids = (1..=size).collect::<Vec<RuntimeTaskId>>();
        queues.push_global(0);
        queues.push_global_batch(&ids);
        queues.push_global(size + 1);
        for expected in 0..=size + 1 {
            assert_eq!(queues.pop_for_worker(0), Some(expected));
        }
        assert_eq!(queues.pop_for_worker(0), None);
        let metrics = queues.metrics_snapshot();
        assert_eq!(metrics.global_pushes, size + 2);
        assert_eq!(metrics.global_pop_hits, size + 2);
        println!(
            "batch_size={size} pushes={} pops={}",
            metrics.global_pushes, metrics.global_pop_hits
        );
    }
}

#[test]
fn empty_global_batch_returns_while_global_mutex_is_held() {
    let queues = RunQueues::new(1);
    let guard = queues.locals[0].owner.lock().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            queues.push_global_batch(&[]);
            tx.send(()).unwrap();
        });
        let completed = rx.recv_timeout(Duration::from_secs(5));
        // Release before asserting so a regression cannot strand the scoped thread.
        drop(guard);
        assert!(
            completed.is_ok(),
            "empty batch attempted to acquire the mutex"
        );
    });
    assert_eq!(queues.len(), 0);
    assert_eq!(
        queues.metrics_snapshot(),
        RunQueueMetricsSnapshot::default()
    );
}

#[test]
fn global_batch_tokens_are_popped_exactly_once_by_concurrent_workers() {
    use crate::task_state::AtomicTaskState;
    for size in [1, 16, 256, 4096, 100_000] {
        let queues = RunQueues::new(8);
        let states = (0..size)
            .map(|_| AtomicTaskState::new())
            .collect::<Vec<_>>();
        let ids = states
            .iter()
            .enumerate()
            .filter_map(|(id, state)| state.claim_queue_slot().then_some(id as RuntimeTaskId))
            .collect::<Vec<_>>();
        queues.push_global_batch(&ids);
        // Repeated wakes cannot grant additional tokens for these queued tasks.
        for state in &states {
            assert_ne!(state.wake(), WakeOutcome::Enqueue);
            assert!(!state.claim_queue_slot());
        }
        let seen = (0..size).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>();
        std::thread::scope(|scope| {
            for worker in 0..8 {
                let queues = &queues;
                let seen = &seen;
                let states = &states;
                scope.spawn(move || {
                    while let Some(id) = queues.pop_for_worker(worker) {
                        assert_eq!(seen[id as usize].fetch_add(1, Ordering::Relaxed), 0);
                        assert_eq!(states[id as usize].claim_for_poll(), ClaimOutcome::Poll);
                    }
                });
            }
        });
        assert!(seen.iter().all(|count| count.load(Ordering::Relaxed) == 1));
        assert_eq!(queues.len(), 0);
        let metrics = queues.metrics_snapshot();
        assert_eq!(
            metrics.global_pop_hits + metrics.local_pop_hits + metrics.steal_successes,
            size as u64
        );
    }
}

#[test]
fn global_batch_single_id_caller_rejects_terminal_and_reaped_tasks() {
    let mut scheduler = RuntimeScheduler::with_worker_count(1);
    let id = scheduler.spawn_placeholder();
    scheduler.prepare_placeholder_terminal_owner(id);
    assert!(
        scheduler
            .tasks
            .with(id, |task| task.state.finish_terminal())
            .unwrap()
    );
    let before = scheduler.run_queues.metrics_snapshot();
    assert_eq!(
        wake_task_outcome_in(&scheduler.tasks, &scheduler.run_queues, id),
        WakeOutcome::Terminal
    );
    assert_eq!(scheduler.run_queues.metrics_snapshot(), before);
    scheduler.tasks.remove(id);
    assert_eq!(
        wake_task_outcome_in(&scheduler.tasks, &scheduler.run_queues, id),
        WakeOutcome::Terminal
    );
    assert_eq!(scheduler.run_queues.metrics_snapshot(), before);
}

#[test]
fn wake_routes_by_active_worker_context_not_default_tls_index() {
    let mut scheduler = RuntimeScheduler::with_worker_count(4);
    let external = scheduler.spawn_parked_placeholder();
    let local = scheduler.spawn_parked_placeholder();
    assert!(scheduler.wake(external));
    assert_eq!(
        scheduler.run_queues.snapshot().first().copied(),
        Some(external)
    );
    let old_depth = SCHED_RUN_DEPTH.with(|depth| depth.replace(1));
    let old_worker = CURRENT_WORKER.with(|worker| worker.replace(2));
    let woke = scheduler.wake(local);
    CURRENT_WORKER.with(|worker| worker.set(old_worker));
    SCHED_RUN_DEPTH.with(|depth| depth.set(old_depth));
    assert!(woke);
    assert_eq!(scheduler.run_queues.snapshot().last().copied(), Some(local));
    assert_eq!(scheduler.run_queues.metrics_snapshot().global_pushes, 1);
    assert_eq!(scheduler.run_queues.metrics_snapshot().local_pushes, 1);
}

#[test]
fn global_refill_is_bounded_and_amortizes_successful_global_locks() {
    for count in [64, 256, 4096, 100_000] {
        let queues = RunQueues::new(8);
        queues.push_global_batch(&(0..count).collect::<Vec<_>>());
        queues.with_owner(0, || {
            assert_eq!(queues.pop_for_worker(0), Some(0));
            assert_eq!(queues.locals[0].len.load(Ordering::Acquire), 31);
        });
        assert_eq!(queues.metrics_snapshot().global_pop_hits, 1);
        assert_eq!(queues.metrics_snapshot().global_pop_attempts, 1);
        let mut seen = std::collections::HashSet::from([0]);
        while let Some(id) = queues.pop_for_worker(0) {
            assert!(seen.insert(id));
        }
        assert_eq!(seen.len(), count as usize);
        assert_eq!(queues.len(), 0);
    }
}

#[test]
fn global_burst_notifies_once_and_refill_shares_with_one_successor() {
    for size in [2, 64, 4096, 100_000] {
        for worker_publisher in [false, true] {
            let queues = RunQueues::new(8);
            let saved_depth =
                SCHED_RUN_DEPTH.with(|depth| depth.replace(u32::from(worker_publisher)));
            for id in 0..size {
                queues.push_global(id);
            }
            SCHED_RUN_DEPTH.with(|depth| depth.set(saved_depth));
            let initial = usize::from(!worker_publisher || size >= 2);
            assert_eq!(queues.idle_notifications.load(Ordering::Relaxed), initial);
            queues.with_owner(0, || assert_eq!(queues.pop_for_worker(0), Some(0)));
            let expected = initial + usize::from(size > 2);
            assert_eq!(queues.idle_notifications.load(Ordering::Relaxed), expected);
            println!(
                "burst={size} worker_publisher={worker_publisher} notifications={initial} refill_notifications={}",
                expected - initial
            );
        }
    }
}

#[test]
fn global_priority_probe_does_not_lock_local_for_refill() {
    let queues = RunQueues::new(2);
    queues.push_global_batch(&(0..64).collect::<Vec<_>>());
    queues.locals[0]
        .prefer_global
        .store(true, Ordering::Relaxed);
    let local = queues.locals[0].owner.lock().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(|| tx.send(queues.pop_for_worker(0)).unwrap());
        let result = rx.recv_timeout(Duration::from_secs(5));
        // Release before asserting so a regression cannot strand the thread.
        drop(local);
        assert_eq!(result.unwrap(), Some(0));
    });
    assert_eq!(queues.metrics_snapshot().global_pop_hits, 1);
    assert_eq!(queues.metrics_snapshot().local_pushes, 0);
}

#[test]
fn drained_injection_queue_rearms_external_single_task_notification() {
    let queues = RunQueues::new(1);
    for id in 0..1000 {
        queues.push_global(id);
        assert_eq!(queues.pop_for_worker(0), Some(id));
    }
    assert_eq!(queues.idle_notifications.load(Ordering::Relaxed), 1000);
}

#[test]
fn worker_spawn_burst_defers_one_notification_until_scheduler_boundary() {
    for size in [1, 16, 256, 4096, 100_000] {
        let queues = RunQueues::new(8);
        let saved_depth = SCHED_RUN_DEPTH.with(|depth| depth.replace(1));
        let saved_pending = SPAWN_NOTIFICATION_PENDING.with(|pending| pending.replace(false));
        for id in 0..size {
            queues.push_spawned(id);
        }
        let pending = SPAWN_NOTIFICATION_PENDING.with(|pending| pending.replace(false));
        let second = SPAWN_NOTIFICATION_PENDING.with(|pending| pending.replace(false));
        SPAWN_NOTIFICATION_PENDING.with(|pending| pending.set(saved_pending));
        SCHED_RUN_DEPTH.with(|depth| depth.set(saved_depth));
        assert!(pending);
        assert!(!second);
        assert_eq!(queues.idle_notifications.load(Ordering::Relaxed), 0);
        assert_eq!(queues.len(), size as usize);
        println!("spawn_burst={size} publication_notifications=0 boundary_notifications=1");
    }
}

/// Quiescent counts equal the underlying injection/deque storage.
fn assert_hints_match(queues: &RunQueues) {
    assert_eq!(
        queues.global_len.load(Ordering::Acquire),
        queues.global.len()
    );
    for queue in &queues.locals {
        assert_eq!(
            queue.len.load(Ordering::Acquire),
            queue.inbox.len() + queue.stealer.len()
        );
    }
}

#[test]
fn t8hq4_19_length_hints_track_every_queue_mutation() {
    let queues = RunQueues::new(3);
    assert_hints_match(&queues);
    queues.push_local(0, 1);
    queues.push_local(1, 2);
    queues.push_local_front(1, 3);
    queues.push_global(4);
    queues.push_global_batch(&[5, 6, 7]);
    queues.push_woken_batch(&[8, 9]);
    queues.push_spawned(10);
    assert_hints_match(&queues);
    assert!(queues.remove(6));
    assert!(queues.remove(2));
    assert!(!queues.remove(99));
    assert_hints_match(&queues);
    // Local pops, global pops with refill, and steals from worker 2's view.
    while queues.pop_for_worker(2).is_some() {
        assert_hints_match(&queues);
    }
    assert_eq!(queues.len(), 0);
    queues.push_local(0, 11);
    queues.push_global(12);
    queues.clear();
    assert_hints_match(&queues);
    assert!(queues.global.is_empty());
    assert!(queues.locals.iter().all(LocalRunQueue::looks_empty));
}

#[test]
fn t8hq4_19_empty_probes_take_no_queue_lock() {
    let queues = RunQueues::new(4);
    // An empty probe must not acquire even the owner handoff locks.
    let held = queues
        .locals
        .iter()
        .map(|queue| queue.owner.lock().unwrap())
        .collect::<Vec<_>>();
    std::thread::scope(|scope| {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let queues = &queues;
        let prober = scope.spawn(move || {
            for _ in 0..8 {
                done_tx.send(queues.pop_for_worker(0)).unwrap();
            }
        });
        for _ in 0..8 {
            let popped = done_rx.recv_timeout(Duration::from_secs(2));
            assert_eq!(popped, Ok(None), "an empty probe blocked on a lock");
        }
        drop(held);
        prober.join().unwrap();
    });
    let metrics = queues.metrics_snapshot();
    assert_eq!(metrics.global_pop_attempts, 8);
    assert_eq!(metrics.steal_attempts, 8);
    assert_eq!(metrics.victim_locks, 0);
}

#[test]
fn t8hq4_19_a_published_push_is_never_skipped() {
    // A completed push is visible to subsequent probes, so one that starts
    // after the push returns always sees it, locally, globally or by steal.
    for worker in 0..4 {
        let queues = RunQueues::new(4);
        queues.push_local(worker, 1);
        assert_eq!(queues.pop_for_worker(0), Some(1));
        queues.push_global(2);
        assert_eq!(queues.pop_for_worker(0), Some(2));
        assert_eq!(queues.pop_for_worker(0), None);
    }
}

#[test]
fn worker_queue_headers_are_separated_and_priority_stays_worker_local() {
    assert_eq!(std::mem::align_of::<LocalRunQueue>(), 128);
    assert_eq!(std::mem::size_of::<LocalRunQueue>() % 128, 0);
    let queues = RunQueues::new(8);
    for worker in 0..8 {
        let address = &queues.locals[worker] as *const LocalRunQueue as usize;
        assert_eq!(address % 128, 0);
        assert!(!queues.locals[worker].prefer_global.load(Ordering::Relaxed));
        assert_eq!(queues.pop_for_worker(worker), None);
        assert!(queues.locals[worker].prefer_global.load(Ordering::Relaxed));
        for other in worker + 1..8 {
            assert!(!queues.locals[other].prefer_global.load(Ordering::Relaxed));
        }
    }
}

#[test]
fn placeholder_removal_keeps_older_owner_work_before_inbox() {
    let queues = RunQueues::new(2);
    queues.with_owner(0, || {
        queues.push_local(0, 1);
        queues.push_local(0, 2);
        queues.push_local(0, 3);
    });
    queues.push_local(0, 4); // Off-owner publication goes to the inbox.
    assert!(queues.remove(2));
    assert_eq!(queues.pop_for_worker(0), Some(1));
    assert_eq!(queues.pop_for_worker(0), Some(3));
    assert_eq!(queues.pop_for_worker(0), Some(4));
    assert_eq!(queues.len(), 0);
}

#[test]
fn owner_and_foreign_inbox_preserve_fifo_after_owner_handoff() {
    let queues = RunQueues::new(2);
    queues.with_owner(0, || {
        queues.push_local(0, 1);
        queues.push_local(0, 2);
        std::thread::scope(|scope| {
            scope.spawn(|| queues.push_local(0, 3)).join().unwrap();
        });
        assert_eq!(queues.pop_for_worker(0), Some(1));
        assert_eq!(queues.pop_for_worker(0), Some(2));
        assert_eq!(queues.pop_for_worker(0), Some(3));
    });
    assert_eq!(queues.len(), 0);
}

#[test]
fn concurrent_thieves_take_every_owner_deque_token_once() {
    const COUNT: usize = 4096;
    let queues = RunQueues::new(8);
    let seen = (0..COUNT).map(|_| AtomicUsize::new(0)).collect::<Vec<_>>();
    queues.with_owner(0, || {
        for id in 0..COUNT {
            queues.push_local(0, id as RuntimeTaskId);
        }
        std::thread::scope(|scope| {
            for worker in 1..8 {
                let queues = &queues;
                let seen = &seen;
                scope.spawn(move || {
                    while let Some(id) = queues.pop_for_worker(worker) {
                        assert_eq!(seen[id as usize].fetch_add(1, Ordering::Relaxed), 0);
                    }
                });
            }
        });
    });
    assert!(seen.iter().all(|count| count.load(Ordering::Relaxed) == 1));
    assert_eq!(queues.len(), 0);
    let metrics = queues.metrics_snapshot();
    assert_eq!(metrics.steal_successes, COUNT as u64);
    assert_eq!(metrics.victim_locks, 0);
}

#[test]
fn inbox_removal_retains_fifo_with_owner_entries() {
    let queues = RunQueues::new(1);
    queues.with_owner(0, || queues.push_local(0, 1));
    for id in [2, 3, 4] {
        queues.push_local(0, id);
    }
    assert!(queues.remove(2));
    assert_eq!(
        (0..3)
            .map(|_| queues.pop_for_worker(0).unwrap())
            .collect::<Vec<_>>(),
        vec![1, 3, 4]
    );
}

#[test]
fn snapshot_preserves_owner_and_inbox_placement() {
    let queues = RunQueues::new(1);
    queues.with_owner(0, || {
        queues.push_local(0, 1);
        queues.push_local(0, 2);
    });
    queues.push_local(0, 3);
    assert_eq!(queues.snapshot(), vec![1, 2, 3]);
    assert_eq!(queues.snapshot(), vec![1, 2, 3]);
    assert_eq!(queues.locals[0].stealer.len(), 2);
    assert_eq!(queues.locals[0].inbox.len(), 1);
    assert_eq!(
        (0..3)
            .map(|_| queues.pop_for_worker(0).unwrap())
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

#[test]
fn snapshot_preserves_current_thread_owner_and_inbox() {
    let queues = RunQueues::new(1);
    queues.with_owner(0, || {
        queues.push_local(0, 1);
        queues.push_local(0, 2);
        std::thread::scope(|scope| scope.spawn(|| queues.push_local(0, 3)).join().unwrap());
        assert_eq!(queues.snapshot(), vec![1, 2, 3]);
        assert_eq!(queues.snapshot(), vec![1, 2, 3]);
        assert_eq!(
            (0..3)
                .map(|_| queues.pop_for_worker(0).unwrap())
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    });
}

#[test]
fn inbox_removal_fifo_across_source_sizes_and_positions() {
    for owner_size in [1, 4, 16] {
        for inbox_size in [1, 4, 16] {
            for remove_offset in 0..=inbox_size {
                let queues = RunQueues::new(1);
                queues.with_owner(0, || {
                    for id in 0..owner_size {
                        queues.push_local(0, id);
                    }
                });
                for id in owner_size..owner_size + inbox_size {
                    queues.push_local(0, id);
                }
                let removed = owner_size + remove_offset;
                assert_eq!(queues.remove(removed), remove_offset < inbox_size);
                let expected = (0..owner_size + inbox_size)
                    .filter(|id| *id != removed)
                    .collect::<Vec<_>>();
                assert_eq!(queues.snapshot(), expected);
                for id in expected {
                    assert_eq!(queues.pop_for_worker(0), Some(id));
                }
                assert_eq!(queues.len(), 0);
            }
        }
    }
}

#[test]
fn active_owner_removal_preserves_mixed_source_fifo() {
    for removed in [1, 2, 3, 4, 99] {
        let queues = RunQueues::new(1);
        queues.with_owner(0, || {
            for id in [1, 2, 3] {
                queues.push_local(0, id);
            }
            std::thread::scope(|s| s.spawn(|| queues.push_local(0, 4)).join().unwrap());
            assert_eq!(queues.remove(removed), removed != 99);
            let expected = (1..=4).filter(|id| *id != removed).collect::<Vec<_>>();
            assert_eq!(queues.snapshot(), expected);
            for id in expected {
                assert_eq!(queues.pop_for_worker(0), Some(id));
            }
            assert_eq!(queues.len(), 0);
        });
    }
}

#[test]
fn foreign_active_owner_removal_leaves_both_sources_untouched() {
    for removed in [2, 4, 99] {
        let queues = RunQueues::new(1);
        let gate = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                queues.with_owner(0, || {
                    for id in [1, 2, 3] {
                        queues.push_local(0, id);
                    }
                    gate.wait();
                    gate.wait();
                })
            });
            gate.wait();
            queues.push_local(0, 4);
            assert!(!queues.remove(removed));
            assert_eq!(queues.len(), 4);
            gate.wait();
        });
        assert_eq!(queues.snapshot(), vec![1, 2, 3, 4]);
        assert_eq!(queues.remove(removed), removed != 99);
        for id in (1..=4).filter(|id| *id != removed) {
            assert_eq!(queues.pop_for_worker(0), Some(id));
        }
        assert_eq!(queues.len(), 0);
    }
}

#[test]
fn foreign_owner_placeholder_transition_discards_stale_token_on_claim() {
    let mut scheduler = RuntimeScheduler::with_worker_count(1);
    let id = scheduler.spawn_placeholder();
    assert_eq!(scheduler.run_queues.pop_for_worker(0), Some(id));
    let queues = Arc::clone(&scheduler.run_queues);
    let gate = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            queues.with_owner(0, || {
                queues.push_local(0, id);
                gate.wait();
                gate.wait();
            })
        });
        gate.wait();
        scheduler.set_running(id);
        assert_eq!(queues.len(), 1);
        assert_eq!(scheduler.claim_ready_for_worker(0), None);
        assert_eq!(queues.len(), 0);
        gate.wait();
    });
}
