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
            victim_locks: 4,
            ..Default::default()
        }
    );
}

#[test]
fn empty_scan_and_last_victim_counts_scale_with_workers_and_attempts() {
    // Exact work, not a wall-clock inference: A scans acquire A*(W-1)
    // victim locks. A successful last-victim scan has the same lower bound.
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
            assert_eq!(metrics.victim_locks, attempts * (workers as u64 - 1));
            assert_eq!(metrics.steal_successes, 0);
            println!(
                "workers={workers} scans={attempts} victim_locks={} global_pop_attempts={}",
                metrics.victim_locks, metrics.global_pop_attempts
            );
            if workers > 1 {
                queues.push_local(workers - 1, 42);
                assert_eq!(queues.pop_for_worker(0), Some(42));
                let after = queues.metrics_snapshot();
                assert_eq!(
                    after.victim_locks - metrics.victim_locks,
                    workers as u64 - 1
                );
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
    assert_eq!(metrics.local_pushes, 2048);
    assert_eq!(metrics.global_pop_hits, 2048);
    assert_eq!(metrics.local_pop_hits + metrics.steal_successes, 2048);
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
