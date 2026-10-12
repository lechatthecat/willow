use super::*;
use crate::scheduler::{reset_global_scheduler_for_test, with_global_for_test};
use crate::task::RuntimeTaskState;
use std::sync::atomic::Ordering::Relaxed;

fn reset() {
    reset_global_scheduler_for_test();
    crate::gc::reset_internal_for_test();
    CHANNEL_WAITER_VISITS.store(0, Relaxed);
}

fn drain_jobs() -> usize {
    let mut polls = 0;
    while deferred::drive_for_test() != 0 {
        polls += 1;
        assert!(polls < 10_000, "compensation must eventually finish");
    }
    polls
}

#[test]
fn deferred_terminal_compensation_bounds_receive_independent_of_messages_and_waiters() {
    let _guard = crate::gc::runtime_test_guard();
    for messages in [1_000, 10_000, 100_000, 1_000_000] {
        for waiters in [33, 1_024, 4_096] {
            reset();
            let raw = willow_channel_new_bounded(0, 1);
            let channel = unsafe { channel_from_raw(raw) }.unwrap();
            // Terminal/missing owners leave no reverse token. Deliberately keep
            // their FIFO entries, as in the stale-head regression fixture.
            for id in 0..waiters {
                channel
                    .state
                    .lock()
                    .unwrap()
                    .send_waiters
                    .register(u64::MAX - id as u64);
            }
            let mut maximum = 0;
            let mut foreground = 0;
            for _ in 0..messages {
                assert_eq!(willow_channel_try_send_i64(raw, 7), 1);
                let before = CHANNEL_WAITER_VISITS.load(Relaxed);
                assert_eq!(willow_channel_recv_i64(raw), 7);
                let visits = CHANNEL_WAITER_VISITS.load(Relaxed) - before;
                maximum = maximum.max(visits);
                foreground += visits;
                assert!(visits <= 1);
            }
            assert_eq!(foreground, messages.min(waiters));
            assert_eq!(
                deferred::jobs_for_test().len(),
                1,
                "coalesce repeated releases"
            );
            // Add a real owner behind the remaining terminal entries. No further
            // receive may be needed to deliver its wake.
            let live = with_global_for_test(|s| s.spawn_parked_placeholder());
            assert!(register_wait(
                raw,
                &mut channel.state.lock().unwrap().send_waiters,
                live,
                ChannelRole::SendWait
            ));
            let polls = drain_jobs();
            assert!(
                polls
                    <= waiters
                        .saturating_sub(messages)
                        .div_ceil(CHANNEL_WAKE_BATCH)
                        + 1
            );
            with_global_for_test(|s| assert_eq!(s.task_state(live), Some(RuntimeTaskState::Ready)));
            let state = channel.state.lock().unwrap();
            assert!(state.send_waiters.is_empty());
            assert!(state.send_handoffs.contains_key(&live));
            assert!(!state.deferred_wakes[0]);
            println!(
                "{{\"case\":\"terminal_compensation\",\"messages\":{messages},\"waiters\":{waiters},\"max_receive_visits\":{maximum},\"foreground_visits\":{foreground},\"deferred_polls\":{polls},\"poll_visit_bound\":32}}"
            );
        }
    }
    reset();
}

#[test]
fn deferred_closed_final_receive_bounds_fanout_independent_of_messages_and_waiters() {
    let _guard = crate::gc::runtime_test_guard();
    for messages in [1_000, 10_000, 100_000, 1_000_000] {
        for waiters in [33, 1_024, 4_096] {
            reset();
            let raw = willow_channel_new_bounded(0, 1);
            for _ in 1..messages {
                assert_eq!(willow_channel_try_send_i64(raw, 9), 1);
                assert_eq!(willow_channel_recv_i64(raw), 9);
            }
            let owner = with_global_for_test(|s| s.spawn_parked_placeholder());
            assert_eq!(willow_channel_try_send_i64(raw, 9), 1);
            crate::scheduler::with_current_task_for_test(owner, || {
                assert_eq!(willow_channel_recv_ready(raw), 1)
            });
            let peers: Vec<_> = with_global_for_test(|s| {
                (0..waiters).map(|_| s.spawn_parked_placeholder()).collect()
            });
            let channel = unsafe { channel_from_raw(raw) }.unwrap();
            {
                let mut state = channel.state.lock().unwrap();
                for &peer in &peers {
                    assert!(register_wait(
                        raw,
                        &mut state.waiters,
                        peer,
                        ChannelRole::RecvWait
                    ));
                }
            }
            willow_channel_close(raw);
            let before = CHANNEL_WAITER_VISITS.load(Relaxed);
            crate::scheduler::with_current_task_for_test(owner, || {
                let mut payload = 0;
                assert_eq!(willow_channel_recv_opt_into(raw, &mut payload), 0);
                assert_eq!(payload, 9);
                assert_eq!(willow_channel_recv_opt_into(raw, &mut payload), 1);
            });
            assert_eq!(
                CHANNEL_WAITER_VISITS.load(Relaxed),
                before,
                "final receive defers all closed fanout"
            );
            assert_eq!(channel.state.lock().unwrap().waiters.len(), waiters);
            assert_eq!(deferred::jobs_for_test().len(), 1);
            let polls = drain_jobs();
            assert_eq!(polls, waiters.div_ceil(CHANNEL_WAKE_BATCH));
            assert_eq!(CHANNEL_WAITER_VISITS.load(Relaxed) - before, waiters);
            for peer in peers {
                with_global_for_test(|s| {
                    assert_eq!(s.task_state(peer), Some(RuntimeTaskState::Ready))
                });
                assert!(crate::scheduler::take_channel_waits(peer).is_empty());
            }
            assert!(channel.state.lock().unwrap().waiters.is_empty());
            assert!(!channel.state.lock().unwrap().deferred_wakes[1]);
            println!(
                "{{\"case\":\"closed_fanout\",\"messages\":{messages},\"waiters\":{waiters},\"max_receive_visits\":0,\"deferred_visits\":{waiters},\"deferred_polls\":{polls},\"poll_visit_bound\":32}}"
            );
        }
    }
    reset();
}

#[test]
fn deferred_job_roots_channel_and_value_after_initiator_cancellation() {
    let _guard = crate::gc::runtime_test_guard();
    reset();
    let raw = willow_channel_new(1);
    let root = crate::gc::GcRootHandle::new(raw.cast());
    let text = b"deferred channel GC child";
    let value = crate::string::willow_string_alloc(text.as_ptr(), text.len() as i64);
    let raw = root.load().cast();
    willow_channel_send_ptr(raw, value.cast());
    let channel = unsafe { channel_from_raw(raw) }.unwrap();
    let initiator = with_global_for_test(|s| s.spawn_parked_placeholder());
    let live = with_global_for_test(|s| s.spawn_parked_placeholder());
    {
        let mut state = channel.state.lock().unwrap();
        for id in 0..100 {
            state.waiters.register(u64::MAX - id);
        }
        assert!(register_wait(
            raw,
            &mut state.waiters,
            live,
            ChannelRole::RecvWait
        ));
    }
    crate::scheduler::with_current_task_for_test(initiator, || {
        wake_waiters_bounded(raw, channel, true);
    });
    let job = deferred::jobs_for_test()[0].0;
    crate::scheduler::willow_sched_cancel(initiator);
    drop(root);
    crate::gc::willow_gc_collect();
    let (_, _, frame) = deferred::jobs_for_test()[0];
    let raw = unsafe {
        frame
            .cast::<u8>()
            .add(crate::async_frame::async_frame_slot_offset(0))
            .cast::<*mut c_void>()
            .read()
    };
    let channel = unsafe { channel_from_raw(raw) }.unwrap();
    let value = unsafe { channel.state.lock().unwrap().values[0].ptr_value };
    assert_eq!(
        unsafe { crate::string::willow_string_as_str(value.cast()) },
        "deferred channel GC child"
    );
    // Exercise the actual scheduler, including YIELD requeue and completion,
    // rather than relying only on direct-poll deterministic accounting.
    assert!(crate::scheduler::willow_sched_run_until(job) >= 1);
    with_global_for_test(|s| assert_eq!(s.task_state(job), None));
    assert!(deferred::jobs_for_test().is_empty());
    assert!(channel.state.lock().unwrap().waiters.is_empty());
    assert!(!channel.state.lock().unwrap().deferred_wakes[1]);
    reset();
}

#[test]
fn deferred_cleanup_scales_with_cases_and_restarts_after_quiescence() {
    let _guard = crate::gc::runtime_test_guard();
    for cases in [1, 4, 16] {
        for waiters in [33, 1_024] {
            reset();
            let owner = with_global_for_test(|s| s.spawn_parked_placeholder());
            let channels: Vec<_> = (0..cases)
                .map(|_| willow_channel_new_bounded(0, 1))
                .collect();
            for &raw in &channels {
                willow_channel_send_i64(raw, 1);
                crate::scheduler::with_current_task_for_test(owner, || {
                    assert_eq!(willow_channel_recv_ready(raw), 1)
                });
                let mut state = unsafe { channel_from_raw(raw) }
                    .unwrap()
                    .state
                    .lock()
                    .unwrap();
                for id in 0..waiters {
                    state.waiters.register(u64::MAX - id);
                }
            }
            let before = CHANNEL_WAITER_VISITS.load(Relaxed);
            crate::scheduler::with_current_task_for_test(owner, || {
                for &raw in &channels {
                    willow_channel_select_cleanup(raw, std::ptr::null_mut(), -1);
                }
            });
            assert_eq!(CHANNEL_WAITER_VISITS.load(Relaxed) - before, cases);
            assert_eq!(deferred::jobs_for_test().len(), cases);
            drain_jobs();
            // A release after the previous helper's final locked check must
            // create a new helper, without needing any old task to remain alive.
            let raw = channels[0];
            let live = with_global_for_test(|s| s.spawn_parked_placeholder());
            let cancelled = with_global_for_test(|s| s.spawn_parked_placeholder());
            let channel = unsafe { channel_from_raw(raw) }.unwrap();
            {
                let mut state = channel.state.lock().unwrap();
                state.waiters.register(u64::MAX);
                assert!(register_wait(
                    raw,
                    &mut state.waiters,
                    cancelled,
                    ChannelRole::RecvWait
                ));
                assert!(register_wait(
                    raw,
                    &mut state.waiters,
                    live,
                    ChannelRole::RecvWait
                ));
            }
            wake_waiters_bounded(raw, channel, true);
            assert_eq!(deferred::jobs_for_test().len(), 1);
            crate::scheduler::willow_sched_cancel(cancelled);
            // Closing during deferred compensation must not strand remaining
            // waiters or resurrect the cancelled recipient.
            willow_channel_close(raw);
            crate::scheduler::willow_sched_run_until(cancelled);
            drain_jobs();
            with_global_for_test(|s| {
                assert_eq!(s.task_state(cancelled), None);
                assert!(matches!(
                    s.task_state(live),
                    Some(RuntimeTaskState::Ready) | None
                ));
            });
            assert!(channel.state.lock().unwrap().waiters.is_empty());
            assert!(crate::scheduler::take_channel_waits(cancelled).is_empty());
            assert!(!channel.state.lock().unwrap().deferred_wakes[1]);
            println!(
                "{{\"case\":\"select_cleanup\",\"cases\":{cases},\"waiters_per_case\":{waiters},\"foreground_visits\":{cases}}}"
            );
        }
    }
    reset();
}

#[test]
fn deferred_fragmented_fifo_has_bounded_physical_unlink_and_receive_work() {
    let _guard = crate::gc::runtime_test_guard();
    for messages in [1_000, 10_000, 100_000, 1_000_000] {
        for waiters in [33usize, 1_024, 4_096] {
            reset();
            let raw = willow_channel_new_bounded(0, 1);
            let channel = unsafe { channel_from_raw(raw) }.unwrap();
            {
                let mut state = channel.state.lock().unwrap();
                for id in 0..2 * waiters {
                    state.send_waiters.register(u64::MAX - id as u64);
                }
                // The old FIFO retained this entire removed prefix just below
                // its compaction threshold, hiding W visits in the first pop.
                for id in 0..waiters {
                    let before = waiters::LINK_VISITS.get();
                    assert!(state.send_waiters.remove(u64::MAX - id as u64));
                    assert!(waiters::LINK_VISITS.get() - before <= 3);
                }
                // Cross the old compaction threshold from the middle as well.
                let before = waiters::LINK_VISITS.get();
                assert!(state.send_waiters.remove(u64::MAX - (waiters + 1) as u64));
                assert_eq!(waiters::LINK_VISITS.get() - before, 3);
            }
            let mut maximum = 0;
            for _ in 0..messages {
                assert_eq!(willow_channel_try_send_i64(raw, 1), 1);
                let before = waiters::LINK_VISITS.get();
                assert_eq!(willow_channel_recv_i64(raw), 1);
                let visits = waiters::LINK_VISITS.get() - before;
                maximum = maximum.max(visits);
                assert!(visits <= 2, "one head and at most one next link");
            }
            drain_jobs();
            assert!(channel.state.lock().unwrap().send_waiters.is_empty());
            println!(
                "{{\"case\":\"fragmented_fifo\",\"messages\":{messages},\"waiters\":{waiters},\"max_receive_link_visits\":{maximum},\"max_unlink_visits\":3}}"
            );
        }
    }
    reset();
}
