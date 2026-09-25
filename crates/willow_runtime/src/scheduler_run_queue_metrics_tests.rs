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
fn steal_counts_visited_victims_and_preserves_lifo() {
    let queues = RunQueues::new(4);
    queues.push_local(2, 1);
    queues.push_local(2, 2);
    assert_eq!(queues.pop_for_worker(0), Some(2));
    assert_eq!(queues.pop_for_worker(0), Some(1));
    assert_eq!(
        queues.metrics_snapshot(),
        RunQueueMetricsSnapshot {
            local_pushes: 2,
            global_pop_attempts: 2,
            steal_attempts: 2,
            steal_successes: 2,
            // Empty victims are skipped by their length hint (willow-8hq4.19).
            victim_locks: 2,
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
                assert_eq!(after.victim_locks - metrics.victim_locks, 1);
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
    let guard = RunQueues::lock(&queues.global);
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
        RunQueues::lock(&scheduler.run_queues.global).front(),
        Some(&external)
    );
    let old_depth = SCHED_RUN_DEPTH.with(|depth| depth.replace(1));
    let old_worker = CURRENT_WORKER.with(|worker| worker.replace(2));
    let woke = scheduler.wake(local);
    CURRENT_WORKER.with(|worker| worker.set(old_worker));
    SCHED_RUN_DEPTH.with(|depth| depth.set(old_depth));
    assert!(woke);
    assert_eq!(
        RunQueues::lock(&scheduler.run_queues.locals[2]).front(),
        Some(&local)
    );
    assert_eq!(scheduler.run_queues.metrics_snapshot().global_pushes, 1);
    assert_eq!(scheduler.run_queues.metrics_snapshot().local_pushes, 1);
}

#[test]
fn global_refill_is_bounded_and_amortizes_successful_global_locks() {
    for count in [64, 256, 4096, 100_000] {
        let queues = RunQueues::new(8);
        queues.push_global_batch(&(0..count).collect::<Vec<_>>());
        assert_eq!(queues.pop_for_worker(0), Some(0));
        assert_eq!(RunQueues::lock(&queues.locals[0]).len(), 31);
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
            assert_eq!(queues.pop_for_worker(0), Some(0));
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
    queues.prefer_global[0].store(true, Ordering::Relaxed);
    let local = RunQueues::lock(&queues.locals[0]);
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

/// Every hint equals its queue's length, read under the queue's own lock
/// without a `QueueGuard` (whose drop would rewrite the hint).
fn assert_hints_match(queues: &RunQueues) {
    for queue in std::iter::once(&queues.global).chain(&queues.locals) {
        let locked = queue.queue.lock().unwrap();
        assert_eq!(queue.len.load(Ordering::Acquire), locked.len());
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
    assert!(queues.global.looks_empty());
    assert!(queues.locals.iter().all(HintedQueue::looks_empty));
}

#[test]
fn t8hq4_19_empty_probes_take_no_queue_lock() {
    let queues = RunQueues::new(4);
    // Hold every queue lock: a probe that locks an empty queue would block.
    let held = std::iter::once(&queues.global)
        .chain(&queues.locals[1..])
        .map(|queue| queue.queue.lock().unwrap())
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
    // A push's hint store happens before its unlock, so a probe that starts
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
