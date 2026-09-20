use super::*;
use crate::task::RuntimeTaskState;
use std::sync::atomic::Ordering;

fn fixture(count: usize, receive: bool) -> (*mut c_void, Vec<u64>) {
    crate::scheduler::reset_global_scheduler_for_test();
    let raw = willow_channel_new_bounded(0, count as i64);
    let tasks = crate::scheduler::with_global_for_test(|s| {
        (0..count)
            .map(|_| s.spawn_parked_placeholder())
            .collect::<Vec<_>>()
    });
    let channel = unsafe { channel_from_raw(raw) }.unwrap();
    let mut state = channel.state.lock().unwrap();
    for &task in &tasks {
        let (queue, role) = if receive {
            (&mut state.waiters, ChannelRole::RecvWait)
        } else {
            (&mut state.send_waiters, ChannelRole::SendWait)
        };
        assert!(register_wait(raw, queue, task, role));
    }
    // Model values arriving / capacity becoming available before notification.
    if receive {
        state.values.extend((0..count).map(|n| WillowChannelValue {
            i64_value: n as i64,
        }));
    }
    (raw, tasks)
}

#[test]
fn fanout_batches_have_linear_counts_and_preserve_values() {
    let _guard = crate::gc::runtime_test_guard();
    for receive in [true, false] {
        for count in [1usize, 32, 33, 1024, 4096] {
            let (raw, tasks) = fixture(count, receive);
            let channel = unsafe { channel_from_raw(raw) }.unwrap();
            CHANNEL_WAKE_ATTEMPTS.store(0, Ordering::Relaxed);
            CHANNEL_WAKE_BATCHES.store(0, Ordering::Relaxed);
            CHANNEL_RESERVE_LOCKS.store(0, Ordering::Relaxed);
            wake_reserved_waiters(channel, receive);
            assert_eq!(CHANNEL_WAKE_ATTEMPTS.load(Ordering::Relaxed), count);
            assert_eq!(
                CHANNEL_WAKE_BATCHES.load(Ordering::Relaxed),
                1 + (count - 1).div_ceil(CHANNEL_WAKE_BATCH)
            );
            assert_eq!(
                CHANNEL_RESERVE_LOCKS.load(Ordering::Relaxed),
                1 + (count - 1).div_ceil(CHANNEL_WAKE_BATCH)
            );
            crate::scheduler::with_global_for_test(|s| {
                for &task in &tasks {
                    assert_eq!(s.task_state(task), Some(RuntimeTaskState::Ready));
                }
            });
            let mut state = channel.state.lock().unwrap();
            assert!(state.waiters.live().is_empty());
            assert!(state.send_waiters.live().is_empty());
            if receive {
                assert_eq!(state.recv_claims.len(), count);
                for (value, &task) in tasks.iter().enumerate() {
                    assert_eq!(
                        unsafe { take_value(raw, &mut state, task).unwrap().i64_value },
                        value as i64
                    );
                }
                assert!(state.recv_claims.is_empty());
            } else {
                assert_eq!(state.send_handoffs.len(), count);
                assert!(state_is_full(&state));
            }
            eprintln!(
                "receive={receive}, waiters={count}, attempts={count}, batches={}",
                1 + (count - 1).div_ceil(CHANNEL_WAKE_BATCH)
            );
        }
    }
}

#[test]
fn stale_waiters_spanning_a_batch_do_not_hide_live_waiters() {
    let _guard = crate::gc::runtime_test_guard();
    for receive in [true, false] {
        let (raw, tasks) = fixture(CHANNEL_WAKE_BATCH * 2 + 1, receive);
        let channel = unsafe { channel_from_raw(raw) }.unwrap();
        crate::scheduler::with_global_for_test(|s| {
            for &task in &tasks[..CHANNEL_WAKE_BATCH + 1] {
                assert!(s.remove_task_for_test(task));
            }
        });
        wake_reserved_waiters(channel, receive);
        let state = channel.state.lock().unwrap();
        let owners = if receive {
            &state.recv_claims
        } else {
            &state.send_handoffs
        };
        assert_eq!(owners.len(), CHANNEL_WAKE_BATCH);
        for &task in &tasks[..CHANNEL_WAKE_BATCH + 1] {
            assert!(!owners.contains_key(&task));
        }
        crate::scheduler::with_global_for_test(|s| {
            for &task in &tasks[CHANNEL_WAKE_BATCH + 1..] {
                assert_eq!(s.task_state(task), Some(RuntimeTaskState::Ready));
            }
        });
    }
}

#[test]
fn cancellation_in_middle_between_reservation_and_publication_cleans_up() {
    let _guard = crate::gc::runtime_test_guard();
    let _single_worker = crate::scheduler::single_worker_for_test();
    for receive in [true, false] {
        let (raw, tasks) = fixture(CHANNEL_WAKE_BATCH, receive);
        let channel = unsafe { channel_from_raw(raw) }.unwrap();
        let mut candidates =
            [(0, core_token(channel, ChannelRole::RecvClaim, 0)); CHANNEL_WAKE_BATCH];
        let (count, _, _) = reserve_waiter_batch(channel, receive, &mut candidates);
        assert_eq!(count, CHANNEL_WAKE_BATCH);
        let cancelled = tasks[count / 2];
        crate::scheduler::willow_sched_cancel(cancelled);
        assert_eq!(crate::scheduler::willow_sched_run_until(cancelled), 0);
        assert!(publish_waiter_batch(
            channel,
            &candidates[..count],
            &mut Default::default()
        ));
        let state = channel.state.lock().unwrap();
        let owners = if receive {
            &state.recv_claims
        } else {
            &state.send_handoffs
        };
        assert_eq!(owners.len(), count - 1);
        assert!(!owners.contains_key(&cancelled));
        crate::scheduler::with_global_for_test(|s| {
            for &task in &tasks {
                assert_eq!(
                    s.task_state(task),
                    if task == cancelled {
                        None
                    } else {
                        Some(RuntimeTaskState::Ready)
                    }
                );
            }
        });
    }
}

#[test]
fn close_between_reservation_and_publication_preserves_only_live_claims() {
    let _guard = crate::gc::runtime_test_guard();
    for receive in [true, false] {
        let (raw, tasks) = fixture(CHANNEL_WAKE_BATCH + 1, receive);
        let channel = unsafe { channel_from_raw(raw) }.unwrap();
        let mut candidates =
            [(0, core_token(channel, ChannelRole::RecvClaim, 0)); CHANNEL_WAKE_BATCH];
        let (count, _, _) = reserve_waiter_batch(channel, receive, &mut candidates);
        willow_channel_close(raw);
        assert!(!publish_waiter_batch(
            channel,
            &candidates[..count],
            &mut Default::default()
        ));
        let mut state = channel.state.lock().unwrap();
        assert!(state.send_handoffs.is_empty());
        assert!(state.waiters.live().is_empty());
        assert!(state.send_waiters.live().is_empty());
        if receive {
            for (value, &task) in tasks.iter().enumerate() {
                assert_eq!(
                    unsafe { take_value(raw, &mut state, task).unwrap().i64_value },
                    value as i64
                );
            }
        }
        assert!(state.recv_claims.is_empty());
        for &task in &tasks {
            assert!(crate::scheduler::take_channel_waits(task).is_empty());
        }
    }
}

#[test]
fn terminal_batch_cleanup_removes_exact_generations_without_task_purge() {
    let _guard = crate::gc::runtime_test_guard();
    for receive in [true, false] {
        let (raw, tasks) = fixture(7, receive);
        let channel = unsafe { channel_from_raw(raw) }.unwrap();
        let mut candidates =
            [(0, core_token(channel, ChannelRole::RecvClaim, 0)); CHANNEL_WAKE_BATCH];
        let (count, _, exhausted) = reserve_waiter_batch(channel, receive, &mut candidates);
        assert!(exhausted);
        crate::scheduler::with_global_for_test(|s| {
            assert!(s.remove_task_for_test(tasks[3]));
            assert!(s.remove_task_for_test(tasks[4]));
        });
        // Simulate a newer registration racing with stale publication cleanup.
        let newer;
        {
            let mut state = channel.state.lock().unwrap();
            newer = next_generation(&mut state);
            let owners = if receive {
                &mut state.recv_claims
            } else {
                &mut state.send_handoffs
            };
            owners.insert(tasks[4], newer);
        }
        assert!(publish_waiter_batch(
            channel,
            &candidates[..count],
            &mut Default::default()
        ));
        let state = channel.state.lock().unwrap();
        let owners = if receive {
            &state.recv_claims
        } else {
            &state.send_handoffs
        };
        assert!(!owners.contains_key(&tasks[3]));
        assert_eq!(owners.get(&tasks[4]), Some(&newer));
        assert_eq!(owners.len(), count - 1);
    }
}

#[test]
fn closed_empty_fanout_wakes_every_waiter_and_deduplicates_roles() {
    let _guard = crate::gc::runtime_test_guard();
    for close_via_abi in [true, false] {
        let (raw, tasks) = fixture(CHANNEL_WAKE_BATCH * 2 + 1, false);
        let channel = unsafe { channel_from_raw(raw) }.unwrap();
        {
            let mut state = channel.state.lock().unwrap();
            for &task in &tasks {
                assert!(register_wait(
                    raw,
                    &mut state.waiters,
                    task,
                    ChannelRole::RecvWait
                ));
                if !close_via_abi {
                    clear_wait(raw, &mut state.send_waiters, task, ChannelRole::SendWait);
                }
            }
            if !close_via_abi {
                state.closed = true;
            }
        }
        if close_via_abi {
            willow_channel_close(raw);
        } else {
            wake_recv_waiters(channel);
        }
        let state = channel.state.lock().unwrap();
        assert!(state.waiters.live().is_empty());
        assert!(state.send_waiters.live().is_empty());
        assert!(state.recv_claims.is_empty());
        assert!(state.send_handoffs.is_empty());
        for &task in &tasks {
            assert!(crate::scheduler::take_channel_waits(task).is_empty());
        }
        crate::scheduler::with_global_for_test(|s| {
            for &task in &tasks {
                assert_eq!(s.task_state(task), Some(RuntimeTaskState::Ready));
            }
        });
    }
}
