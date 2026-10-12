//! Bounded channel compensation on ordinary scheduler tasks. The GC-managed
//! frame owns the channel, including queued references, until the job completes.
//! No task handle escapes: cancelling a sender/receiver cannot cancel this job.
use super::*;
use crate::async_frame::{async_frame_slot_offset, willow_async_frame_alloc};

fn needed(state: &WillowChannelState, receive: bool) -> bool {
    if receive {
        !state.waiters.is_empty()
            && (state.values.len() > state.recv_claims.len()
                || (state.closed && state.values.is_empty()))
    } else {
        !state.send_waiters.is_empty() && !state.closed && !state_is_full(state)
    }
}

pub(super) fn schedule(raw: *mut c_void, channel: &ChannelCore, receive: bool) -> *mut c_void {
    let index = usize::from(receive);
    {
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        if state.deferred_wakes[index] || !needed(&state, receive) {
            return raw;
        }
        state.deferred_wakes[index] = true;
    }
    // Root before allocating: allocation can relocate the caller's wrapper.
    // The stable native core remains valid throughout. Root construction and
    // the initialization stores below do not safepoint.
    let channel_root = crate::gc::GcRootHandle::new(raw.cast());
    let frame = willow_async_frame_alloc(1, 1);
    let value = channel_root.load();
    crate::gc::willow_gc_write_barrier(
        frame.cast(),
        std::ptr::null_mut(),
        value,
        crate::gc::GcStoreDestination::AsyncFrameSlot as i64,
    );
    unsafe {
        frame
            .cast::<u8>()
            .add(crate::async_frame::ASYNC_FRAME_SLOT_COUNT_OFFSET)
            .cast::<i64>()
            .write(1);
        frame
            .cast::<u8>()
            .add(async_frame_slot_offset(0))
            .cast::<*mut u8>()
            .write(value);
    }
    crate::scheduler::willow_sched_spawn_cooperative(
        if receive { poll_receive } else { poll_send },
        frame,
    );
    channel_root.load().cast()
}

#[cfg(test)]
pub(super) fn jobs_for_test() -> Vec<(u64, crate::task::RuntimePollFn, *mut c_void)> {
    let mut jobs = Vec::new();
    crate::scheduler::with_global_for_test(|scheduler| {
        scheduler.for_each_task(|task| {
            if matches!(
                task.runtime_state(),
                Some(
                    crate::task::RuntimeTaskState::Ready
                        | crate::task::RuntimeTaskState::Running
                        | crate::task::RuntimeTaskState::Parked
                )
            ) && let Some(poll) = task.poll
                && (std::ptr::fn_addr_eq(poll, poll_receive as crate::task::RuntimePollFn)
                    || std::ptr::fn_addr_eq(poll, poll_send as crate::task::RuntimePollFn))
            {
                jobs.push((task.id, poll, task.frame));
            }
        });
    });
    jobs
}

#[cfg(test)]
pub(super) fn drive_for_test() -> usize {
    let jobs = jobs_for_test();
    for &(id, poll, frame) in &jobs {
        crate::scheduler::with_global_for_test(|scheduler| {
            scheduler.with_task(id, |task| {
                if task.runtime_state() == Some(crate::task::RuntimeTaskState::Ready) {
                    assert_eq!(task.claim_for_poll(), crate::task_state::ClaimOutcome::Poll);
                }
            });
        });
        let before = CHANNEL_WAITER_VISITS.load(std::sync::atomic::Ordering::Relaxed);
        let result = unsafe { poll(frame) };
        assert!(
            CHANNEL_WAITER_VISITS.load(std::sync::atomic::Ordering::Relaxed) - before
                <= CHANNEL_WAKE_BATCH
        );
        if result == crate::task::RUNTIME_POLL_READY {
            crate::scheduler::with_global_for_test(|scheduler| {
                scheduler.complete(id);
            });
        } else {
            assert_eq!(result, crate::task::RUNTIME_POLL_YIELD);
        }
    }
    jobs.len()
}

#[willow_runtime_macros::ffi_boundary]
unsafe extern "C" fn poll_receive(frame: *mut c_void) -> i32 {
    unsafe { poll(frame, true) }
}

#[willow_runtime_macros::ffi_boundary]
unsafe extern "C" fn poll_send(frame: *mut c_void) -> i32 {
    unsafe { poll(frame, false) }
}

unsafe fn poll(frame: *mut c_void, receive: bool) -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let raw = unsafe {
        frame
            .cast::<u8>()
            .add(async_frame_slot_offset(0))
            .cast::<*mut c_void>()
            .read()
    };
    let channel = unsafe { channel_from_raw(raw) }.expect("rooted channel job");
    // Borrow only the stable core across publication's GC stress points.
    let channel: &ChannelCore = channel;
    let mut candidates = [(0, core_token(channel, ChannelRole::RecvClaim, 0)); CHANNEL_WAKE_BATCH];
    let mut closed_ids = [0; CHANNEL_WAKE_BATCH];
    let mut closed_count = 0;
    let count = {
        let mut state = channel.state.lock().expect("channel mutex poisoned");
        if receive && state.closed && state.values.is_empty() {
            for slot in &mut closed_ids {
                let Some((task, generation)) = state.waiters.pop_front_ticket() else {
                    break;
                };
                #[cfg(test)]
                CHANNEL_WAITER_VISITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                crate::scheduler::clear_channel_ownership(
                    task,
                    core_token(channel, ChannelRole::RecvWait, generation),
                );
                *slot = task;
                closed_count += 1;
            }
            0
        } else {
            reserve_waiter_batch_locked(channel, &mut state, receive, &mut candidates).0
        }
    };
    let mut scratch = crate::scheduler::WakeBatchScratch::default();
    if closed_count != 0 {
        crate::scheduler::wake_channel_owners(&closed_ids[..closed_count], &mut scratch);
    } else {
        publish_waiter_batch(channel, &candidates[..count], &mut scratch);
    }
    // Recheck under the channel lock, including failed publication and concurrent
    // releases. Clearing the flag and observing no work is one transaction: a
    // later release either sees this worker's ownership or schedules its successor.
    let mut state = channel.state.lock().expect("channel mutex poisoned");
    if needed(&state, receive) {
        crate::task::RUNTIME_POLL_YIELD
    } else {
        state.deferred_wakes[usize::from(receive)] = false;
        crate::task::RUNTIME_POLL_READY
    }
}
