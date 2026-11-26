//! Synchronous select must re-probe readiness after observing drive quiescence.
use super::*;
use crate::channel::{
    willow_channel_new, willow_channel_recv_i64, willow_channel_recv_ready, willow_channel_send_i64,
};

#[test]
fn select_quiescence_observes_continuation_published_before_poll_end() {
    let _runtime = crate::gc::isolated_runtime();
    reset_global_scheduler_for_test();
    let state = ParallelRunState::default();
    state.begin_poll();
    assert!(scheduler_has_wake_source_with_peer_probe(|| {
        // Simulate the producer ending exactly as the peer-poll read occurs.
        // The subsequent queue read must observe its published continuation.
        global_run_queues().push_global(71);
        state.end_poll();
        state.active_polls() > 0
    }));
    assert_eq!(global_run_queues().pop_for_worker(0), Some(71));
}

#[test]
fn select_quiescence_observes_claim_resolved_after_peer_snapshot() {
    let _runtime = crate::gc::isolated_runtime();
    reset_global_scheduler_for_test();
    let state = ParallelRunState::default();
    assert!(scheduler_has_wake_source_with_peer_probe(|| {
        let peers = state.active_polls() > 0;
        // A new poll may start after its shard was scanned, with its claim
        // already resolved before the queue and claim-count checks.
        let mut claim = ClaimInFlight::enter();
        claim.popped();
        state.begin_poll();
        drop(claim);
        peers
    }));
    state.end_poll();
}

#[test]
fn select_quiescence_is_not_a_ready_channel_deadlock() {
    let _runtime = crate::gc::isolated_runtime();
    reset_global_scheduler_for_test();
    let channel = willow_channel_new(0);
    assert_eq!(willow_sched_current_task(), 0);
    // SAFETY: The newly allocated scalar channel is live for this test.
    assert_eq!(unsafe { willow_channel_recv_ready(channel) }, 0);
    let address = channel as usize;
    // The producer completes between the original probe and idle decision.
    std::thread::spawn(crate::runtime::serving(move || {
        // SAFETY: The parent retains the scalar channel until the thread joins.
        unsafe { willow_channel_send_i64(address as *mut c_void, 9) };
    }))
    .join()
    .unwrap();
    assert_eq!(willow_select_drive_is_quiescent(), 1);
    assert_eq!(crate::panic_context::willow_panic_active(), 0);
    // Quiescence says no future work remains, not that a case is unready.
    // SAFETY: The same live channel is re-probed before consuming its value.
    assert_eq!(unsafe { willow_channel_recv_ready(channel) }, 1);
    // SAFETY: The successful probe established a ready scalar value.
    assert_eq!(unsafe { willow_channel_recv_i64(channel) }, 9);
}

#[test]
fn select_quiescence_reports_queued_work_without_waiting() {
    let _runtime = crate::gc::isolated_runtime();
    reset_global_scheduler_for_test();
    assert_eq!(willow_select_drive_is_quiescent(), 1);
    global_run_queues().push_global(72);
    assert_eq!(willow_select_drive_is_quiescent(), 0);
    assert_eq!(global_run_queues().pop_for_worker(0), Some(72));
    assert_eq!(willow_select_drive_is_quiescent(), 1);
}

#[test]
fn select_timed_idle_rechecks_newly_queued_work() {
    let _runtime = crate::gc::isolated_runtime();
    let _watchdog = crate::test_sync::Watchdog::new("select newly queued source");
    reset_global_scheduler_for_test();
    assert_eq!(willow_select_drive_is_quiescent(), 1);
    global_run_queues().push_global(73);
    let before = crate::runtime::current().parking.park_calls_for_test();
    willow_select_wait_until(willow_monotonic_millis() + 60_000);
    assert_eq!(
        crate::runtime::current().parking.park_calls_for_test(),
        before
    );
    assert_eq!(global_run_queues().pop_for_worker(0), Some(73));
}

#[test]
fn select_timed_idle_expired_deadline_never_parks() {
    let _runtime = crate::gc::isolated_runtime();
    reset_global_scheduler_for_test();
    let before = crate::runtime::current().parking.park_calls_for_test();
    willow_select_wait_until(willow_monotonic_millis());
    assert_eq!(
        crate::runtime::current().parking.park_calls_for_test(),
        before
    );
}

#[test]
fn select_timed_idle_returns_on_registered_notification() {
    let _runtime = crate::gc::isolated_runtime();
    let _watchdog = crate::test_sync::Watchdog::new("select wake before deadline");
    reset_global_scheduler_for_test();
    let waiter = std::thread::spawn(crate::runtime::serving(|| {
        willow_select_wait_until(willow_monotonic_millis() + 60_000);
    }));
    let parking = &crate::runtime::current().parking;
    while parking.registered_for_test(false) < 1 {
        std::thread::yield_now();
    }
    parking.notify_worker(0);
    waiter.join().unwrap();
}
