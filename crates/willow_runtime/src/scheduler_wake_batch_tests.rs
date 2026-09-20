use super::*;

fn fixture(size: u64, mixed: bool) -> (ShardedTaskTable, RunQueues) {
    let tasks = ShardedTaskTable::new();
    let queues = RunQueues::new(1);
    for id in 1..=size {
        let mut task = RuntimeTask::new(id);
        assert!(task.state.claim_queue_slot());
        assert_eq!(task.state.claim_for_poll(), ClaimOutcome::Poll);
        match if mixed { id % 6 } else { 0 } {
            0 => {
                task.state.park_after_poll();
            }
            1 => {
                task.state.block_on_syscall();
                tasks.blocked_syscall.fetch_add(1, Ordering::Relaxed);
            }
            2 => {} // Running
            3 => {
                task.state.finish_terminal();
            }
            4 => {
                task.state.request_cancel();
                task.state.park_after_poll();
                assert_eq!(task.state.claim_for_poll(), ClaimOutcome::Cancel);
            }
            5 => {
                task.state.park_after_poll();
                assert_eq!(task.state.wake(), WakeOutcome::Enqueue);
                queues.push_global(id);
            }
            _ => unreachable!(),
        }
        task.wake_deadline = Some(Instant::now());
        tasks.insert(id, task);
    }
    (tasks, queues)
}

#[test]
fn batch_matches_sequential_for_all_states_duplicates_and_missing_ids() {
    let (tasks, queues) = fixture(192, true);
    let (sequential, single_queue) = fixture(192, true);
    let mut ids = (1..=194).rev().collect::<Vec<_>>();
    ids.extend(1..=194);
    let mut expected_enqueued = Vec::new();
    let mut expected_terminal = Vec::new();
    for &id in &ids {
        match wake_task_outcome_in(&sequential, &single_queue, id) {
            WakeOutcome::Enqueue => expected_enqueued.push(id),
            WakeOutcome::Terminal => expected_terminal.push(id),
            _ => {}
        }
    }
    let mut scratch = WakeBatchScratch::default();
    wake_tasks_outcome_in(&tasks, &queues, &ids, &mut scratch);
    expected_enqueued.sort_unstable();
    expected_terminal.sort_unstable();
    let mut actual = scratch.enqueued.clone();
    actual.sort_unstable();
    let mut terminal = scratch.terminal.clone();
    terminal.sort_unstable();
    assert_eq!(actual, expected_enqueued);
    assert_eq!(terminal, expected_terminal);
    assert_eq!(
        tasks.blocked_syscall_count(),
        sequential.blocked_syscall_count()
    );
    assert_eq!(queues.len(), single_queue.len());
    for id in 1..=192 {
        let snapshot = |task: &RuntimeTask| {
            let state = task.state.load();
            (
                state.lifecycle(),
                state.is_queued(),
                state.wake_requested(),
                state.cancel_requested(),
                task.wake_deadline.is_some(),
            )
        };
        assert_eq!(tasks.with(id, snapshot), sequential.with(id, snapshot));
    }
}

#[test]
fn batch_fanout_counts_and_fifo_scale_and_scratch_is_reused() {
    for size in [1, 32, 256, 4096, 100_000] {
        let (tasks, queues) = fixture(size, false);
        let ids = (1..=size).rev().collect::<Vec<_>>();
        let mut scratch = WakeBatchScratch::default();
        wake_tasks_outcome_in(&tasks, &queues, &ids, &mut scratch);
        assert_eq!(scratch.shard_locks, (size as usize).min(TASK_TABLE_SHARDS));
        assert_eq!(scratch.queue_batches, 1);
        assert_eq!(queues.metrics_snapshot().global_pushes, size);
        let expected = (0..TASK_TABLE_SHARDS)
            .flat_map(|shard| {
                ids.iter()
                    .copied()
                    .filter(move |id| *id as usize % TASK_TABLE_SHARDS == shard)
            })
            .collect::<Vec<_>>();
        assert_eq!(scratch.enqueued, expected);
        for id in expected {
            assert_eq!(queues.pop_for_worker(0), Some(id));
        }
        let capacities = scratch.grouped.capacity();
        println!(
            "ids={size} shard_locks={} queue_batches={}",
            scratch.shard_locks, scratch.queue_batches
        );
        wake_tasks_outcome_in(&tasks, &queues, &ids, &mut scratch);
        assert!(scratch.enqueued.is_empty());
        assert_eq!(scratch.queue_batches, 0);
        assert_eq!(scratch.grouped.capacity(), capacities);
        wake_tasks_outcome_in(&tasks, &queues, &[], &mut scratch);
        assert_eq!(scratch.shard_locks, 0);
        assert!(scratch.terminal.is_empty());
    }
}

#[test]
fn batch_cancel_race_has_exactly_one_queue_token_and_balanced_blocked_count() {
    for _ in 0..32 {
        let (tasks, queues) = fixture(256, true);
        let ids = (1..=256).collect::<Vec<_>>();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                barrier.wait();
                for &id in &ids {
                    tasks.with_mut(id, |task| {
                        let before = task.state.lifecycle();
                        if task.state.request_cancel() == CancelOutcome::Enqueue {
                            queues.push_global(id);
                        }
                        tasks.reconcile_blocked_transition(before, task.state.lifecycle());
                    });
                }
            });
            barrier.wait();
            wake_tasks_outcome_in(&tasks, &queues, &ids, &mut WakeBatchScratch::default());
        });
        assert_eq!(tasks.blocked_syscall_count(), 0);
        let mut seen = std::collections::HashSet::new();
        while let Some(id) = queues.pop_for_worker(0) {
            assert!(seen.insert(id));
        }
        for id in ids {
            tasks.with(id, |task| {
                assert_eq!(task.state.load().is_queued(), seen.contains(&id));
            });
        }
    }
}

#[test]
fn batch_keeps_blocked_count_until_queue_publication() {
    let (tasks, queues) = fixture(1, true); // ID 1 is BlockedSyscall.
    let guard = RunQueues::lock(&queues.global);
    std::thread::scope(|scope| {
        let wake = scope.spawn(|| {
            wake_tasks_outcome_in(&tasks, &queues, &[1], &mut WakeBatchScratch::default())
        });
        // Wait for the waker to own the shard: it must retain it while the queue
        // mutex prevents publication. Timeout bounds a failed regression.
        let deadline = Instant::now() + Duration::from_secs(5);
        while tasks.shards[1].try_lock().is_ok() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let retained = tasks.shards[1].try_lock().is_err();
        let blocked = tasks.blocked_syscall_count();
        drop(guard);
        wake.join().unwrap();
        assert!(retained);
        assert_eq!(blocked, 1);
    });
    assert_eq!(tasks.blocked_syscall_count(), 0);
    assert_eq!(queues.pop_for_worker(0), Some(1));
}

#[test]
fn ffi_empty_batch_accepts_null() {
    unsafe {
        willow_sched_wake_many(std::ptr::null(), 0);
    }
}

#[test]
fn overlapping_batches_publish_each_token_once() {
    let (tasks, queues) = fixture(4096, false);
    std::thread::scope(|scope| {
        for reverse in [false, true] {
            let tasks = &tasks;
            let queues = &queues;
            scope.spawn(move || {
                let mut ids = (1..=4096).collect::<Vec<_>>();
                if reverse {
                    ids.reverse();
                }
                wake_tasks_outcome_in(tasks, queues, &ids, &mut WakeBatchScratch::default());
            });
        }
    });
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = queues.pop_for_worker(0) {
        assert!(seen.insert(id));
    }
    assert_eq!(seen.len(), 4096);
    assert_eq!(queues.metrics_snapshot().global_pushes, 4096);
}

#[test]
fn global_batch_notifies_once_and_ffi_wakes_tasks() {
    let _guard = crate::gc::runtime_test_guard();
    reset_global_scheduler_for_test();
    let tasks = global_task_table();
    let queues = global_run_queues();
    let (fixture_tasks, _) = fixture(64, false);
    for task in fixture_tasks.drain() {
        tasks.insert(task.id, task);
    }
    let wake_count = || {
        let mut snapshot = crate::observability::WillowRuntimeMetricsV1::default();
        assert_eq!(
            crate::observability::willow_runtime_metrics_snapshot_v1(&mut snapshot),
            0
        );
        snapshot.task_wakes
    };
    let before_wakes = wake_count();
    let generation = current_wake_generation();
    let ids = (1..=64).collect::<Vec<_>>();
    unsafe {
        willow_sched_wake_many(ids.as_ptr(), ids.len());
    }
    assert_eq!(current_wake_generation(), generation.wrapping_add(1));
    assert_eq!(queues.len(), 64);
    assert_eq!(wake_count() - before_wakes, 64);
    let mut scratch = WakeBatchScratch::default();
    wake_channel_owners(&[65, 1], &mut scratch);
    assert_eq!(scratch.terminal, vec![65]);
    assert_eq!(wake_count() - before_wakes, 64);
    assert!(scratch.enqueued.is_empty());
    let generation = current_wake_generation();
    wake_channel_owners(&[], &mut scratch);
    assert_eq!(current_wake_generation(), generation);
    assert!(scratch.terminal.is_empty());
    reset_global_scheduler_for_test();
}

#[test]
fn concentrated_batches_reuse_one_buffer_across_shard_shapes() {
    let tasks = ShardedTaskTable::new();
    let queues = RunQueues::new(1);
    let mut scratch = WakeBatchScratch::default();
    for size in [1, 32, 256, 4096] {
        let mut capacity = None;
        for shard in 0..TASK_TABLE_SHARDS {
            // Missing IDs exercise terminal output, without queue publication.
            let ids = (0..size)
                .map(|i| (i * TASK_TABLE_SHARDS + shard) as u64)
                .collect::<Vec<_>>();
            wake_tasks_outcome_in(&tasks, &queues, &ids, &mut scratch);
            assert_eq!(scratch.shard_locks, 1);
            assert_eq!(scratch.queue_batches, 0);
            assert_eq!(scratch.terminal, ids);
            if let Some(capacity) = capacity {
                assert_eq!(scratch.grouped.capacity(), capacity);
            }
            capacity = Some(scratch.grouped.capacity());
        }
    }
}
