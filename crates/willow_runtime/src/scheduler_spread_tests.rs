//! willow-jz15.120: a worker with queued local work hands an unpinned first
//! poll back to the shared queue while a peer is idle, so allocation-heavy
//! tasks do not get pinned together on one worker.
use super::*;

fn ready_task(id: RuntimeTaskId) -> RuntimeTask {
    let task = RuntimeTask::new(id);
    assert_eq!(task.state.lifecycle(), TaskLifecycle::Ready);
    task
}

#[test]
fn spread_requires_local_backlog_and_an_idle_peer() {
    for workers in [2, 4, 8, 16] {
        let queues = RunQueues::new(workers);
        let state = ParallelRunState::default();
        let task = ready_task(1);
        // No local backlog: the claiming worker runs the task itself.
        assert!(!task_should_spread(&task, 0, &queues, Some(&state)));
        queues.locals[0].publish(2);
        assert!(task_should_spread(&task, 0, &queues, Some(&state)));
        // Without a parallel run there is no peer to hand the task to.
        assert!(!task_should_spread(&task, 0, &queues, None));
        // The claimer has not begun its poll: `workers - 2` running peers
        // still leave one idle, `workers - 1` leave none.
        for _ in 0..workers - 2 {
            state.begin_poll();
        }
        assert!(task_should_spread(&task, 0, &queues, Some(&state)));
        state.begin_poll();
        assert!(!task_should_spread(&task, 0, &queues, Some(&state)));
        for _ in 0..workers - 1 {
            state.end_poll();
        }
        assert_eq!(state.active_polls(), 0);
        // Another worker's backlog does not make this claim spread.
        assert!(!task_should_spread(&task, 1, &queues, Some(&state)));
    }
}

#[test]
fn spread_hops_are_bounded_by_the_pool_size() {
    for workers in [2, 4, 8, 16] {
        let queues = RunQueues::new(workers);
        let state = ParallelRunState::default();
        queues.locals[0].publish(2);
        let mut task = ready_task(1);
        let mut hops = 0;
        while task_should_spread(&task, 0, &queues, Some(&state)) {
            task.spread_hops += 1;
            hops += 1;
            assert!(hops <= workers, "spread did not terminate");
        }
        assert_eq!(hops, workers);
    }
}

#[test]
fn only_a_ready_first_claim_spreads() {
    let queues = RunQueues::new(4);
    let state = ParallelRunState::default();
    queues.locals[0].publish(2);
    let task = ready_task(1);
    assert!(task.state.claim_queue_slot());
    assert_eq!(task.state.claim_for_poll(), ClaimOutcome::Poll);
    assert!(!task_should_spread(&task, 0, &queues, Some(&state)));
}

#[test]
fn deferred_global_pop_serves_local_work_first_once() {
    let queues = RunQueues::new(2);
    queues.push_global(1);
    queues.locals[0].publish(2);
    queues.locals[0].publish(3);
    // The first pop is local-first and flips the preference to global.
    assert_eq!(queues.pop_for_worker(0), Some(2));
    // A spread defers the global turn: the next pop stays local, and only
    // then may this worker take a shared task.
    queues.defer_global_pop(0);
    assert_eq!(queues.pop_for_worker(0), Some(3));
    assert_eq!(queues.pop_for_worker(0), Some(1));
    assert_eq!(queues.pop_for_worker(0), None);
    // An out-of-range worker is ignored.
    queues.defer_global_pop(7);
}

#[test]
fn rerouted_task_reaches_its_owner_without_waking_idle_thieves() {
    let queues = RunQueues::new(4);
    let before = queues.idle_notifications.load(Ordering::Relaxed);
    let generation = current_wake_generation();
    for id in 1..=64 {
        queues.push_rerouted(2, id);
    }
    assert_eq!(queues.idle_notifications.load(Ordering::Relaxed), before);
    // Each reroute targets its owner and advances the wake generation, so an
    // owner that probed before the publication does not park on it.
    assert_eq!(queues.owner_notifications.load(Ordering::Relaxed), 64);
    assert!(current_wake_generation() >= generation + 64);
    for id in 1..=64 {
        assert_eq!(queues.pop_for_worker(2), Some(id));
    }
    // An ordinary local push still wakes an idle worker.
    queues.push_local(2, 65);
    assert_eq!(
        queues.idle_notifications.load(Ordering::Relaxed),
        before + 1
    );
    // Without a local queue the reroute falls back to the shared queue.
    queues.push_rerouted(9, 66);
    assert_eq!(queues.pop_for_worker(0), Some(66));
}
