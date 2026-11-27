//! Hand-over-hand transfer. Next-node admission never queues, spins, or parks;
//! release reuses ordinary mutex cleanup, including queued-waiter handoff.
//! No cycle registry or extra bookkeeping is added to ordinary mutex admission/release.
use super::{AsyncMutex, MutexRelease, SLOW};
use crate::lock_wait::RegistrationToken;
use crate::task::RuntimeTaskId;
use std::sync::atomic::Ordering;

impl AsyncMutex {
    /// One atomic admission attempt, preserving FIFO admission when a queue
    /// exists. A failed CAS (including a free/owned/free race) is a safe failure.
    fn try_handoff_acquire(&self, task: RuntimeTaskId) -> Option<RegistrationToken> {
        let fast = self.fast.load(Ordering::Relaxed);
        if fast & (SLOW | 1) != 0 || fast >= SLOW - 2 {
            return None;
        }
        self.fast
            .compare_exchange(fast, fast + 1, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        self.fast_owner.store(task, Ordering::Relaxed);
        self.fast_published.store(fast + 1, Ordering::Release);
        Some(fast + 1)
    }
}

/// Try the next mutex while retaining the current one. Failure leaves the
/// frame and ownership unchanged. Success commits the old value, releases the
/// old mutex and installs the next ownership in the same cleanup slots. Phase
/// zero prevents cancellation from committing the old binding to the new lock
/// before generated code finishes loading its value.
///
/// # Safety
/// `frame` is a live, rooted async frame. The three offsets address disjoint,
/// aligned pointer/i64/i64 slots within it, currently describing an owned Mutex
/// with phase one. `next` is a live rooted Mutex with the same payload type.
/// `value` is its current protected storage word (a live managed reference for
/// reference payloads). The calling task exclusively owns the frame slots.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_async_mutex_handoff(
    frame: *mut u8,
    handle_offset: i64,
    token_offset: i64,
    phase_offset: i64,
    next: *mut u8,
    value: i64,
) -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let Some(task) = crate::scheduler::current_task() else {
        crate::panic_context::fatal_invariant("mutex handoff outside a running task");
    };
    // SAFETY: The generated caller supplies the disjoint initialized frame
    // slots and live, rooted handles specified by this function's contract.
    let (handle, token, phase, current, following) = unsafe {
        let handle = frame.offset(handle_offset as isize).cast::<*mut u8>();
        let token = frame.offset(token_offset as isize).cast::<i64>();
        let phase = frame.offset(phase_offset as isize).cast::<i64>();
        (
            handle,
            token,
            phase,
            &*(*handle).cast::<AsyncMutex>(),
            &*next.cast::<AsyncMutex>(),
        )
    };
    // SAFETY: The initialized token and phase slots belong to the caller.
    let (old_token, loaded) = unsafe { (*token as RegistrationToken, *phase == 1) };
    if !loaded || current.load(task, old_token).is_none() {
        crate::panic_context::fatal_invariant("mutex handoff without loaded ownership");
    }
    let Some(next_token) = following.try_handoff_acquire(task) else {
        return 0;
    };
    // Both locks are owned here. No user code or task preemption can interrupt
    // the transfer. Releasing a queued owner may collect under scheduler stress:
    // the caller roots both nonmoving handles and the nonmoving frame, and the
    // old storage word is committed before any wake can collect it.
    if !matches!(
        current.commit_release(task, old_token, value),
        MutexRelease::Released { .. }
    ) {
        crate::panic_context::fatal_invariant("mutex handoff lost current ownership");
    }
    // SAFETY: The frame is the managed destination; its old and new handle
    // words are live managed references. This barrier does not collect.
    unsafe {
        crate::gc::willow_gc_write_barrier(
            frame,
            *handle,
            next,
            crate::gc::GcStoreDestination::AsyncFrameSlot as i64,
        );
        *handle = next;
        *token = next_token as i64;
        *phase = 0;
    }
    1
}

/// # Safety
/// `file` is null or a live Willow String, rooted throughout the call.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_async_mutex_handoff_panic(file: *const u8, line: i32, col: i32) {
    // SAFETY: The generated caller retains the optional file String.
    unsafe {
        crate::panic_context::raise_language_message_at(
            "lock walk could not acquire the next Mutex without waiting (contention or recursive acquisition)",
            file,
            line.into(),
            col.into(),
        )
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::async_mutex::{MutexAcquire, MutexResume, ReclamationStatus};
    use crate::gc::isolated_runtime;
    use crate::scheduler::{reset_global_scheduler_for_test, with_global_for_test};
    use std::sync::Barrier;

    fn task() -> RuntimeTaskId {
        with_global_for_test(|scheduler| scheduler.spawn_parked_placeholder())
    }

    #[test]
    fn handoff_frame_ownership_survives_collection_when_releasing_to_a_waiter() {
        use crate::async_mutex::*;
        use crate::gc::GcRootHandle;
        use crate::scheduler::with_current_task_for_test;
        let _guard = isolated_runtime();
        reset_global_scheduler_for_test();
        // SAFETY: Initialize the isolated runtime, with no outstanding mutator work.
        unsafe { crate::gc::willow_gc_init() };
        // SAFETY: Both constructors receive scalar words and a zero reference flag.
        let a = unsafe { willow_async_mutex_new(1, 0) };
        let _a = GcRootHandle::new(a.cast());
        // SAFETY: The zero reference flag selects scalar storage.
        let b = unsafe { willow_async_mutex_new(2, 0) };
        let _b = GcRootHandle::new(b.cast());
        let frame = crate::async_frame::willow_async_frame_alloc(3, 1).cast::<u8>();
        let _frame = GcRootHandle::new(frame);
        let owner = task();
        let waiter = task();
        let mut token = 0;
        with_current_task_for_test(owner, || {
            assert_eq!(
                // SAFETY: Rooted matching mutex and disjoint writable token output.
                unsafe { willow_async_mutex_acquire(a, &mut token) },
                MUTEX_STATUS_ACQUIRED
            );
        });
        let mut waiter_token = 0;
        with_current_task_for_test(waiter, || {
            assert_eq!(
                // SAFETY: Rooted matching mutex and disjoint writable token output.
                unsafe { willow_async_mutex_acquire(a, &mut waiter_token) },
                MUTEX_STATUS_PENDING
            );
        });
        // Three data slots follow the three-word async-frame header.
        // SAFETY: This fresh rooted frame owns these aligned, disjoint slots.
        unsafe {
            *frame.add(24).cast::<*mut u8>() = a.cast();
            *frame.add(32).cast::<i64>() = token;
            *frame.add(40).cast::<i64>() = 1;
        }
        let before = crate::gc::willow_gc_major_collections();
        crate::gc::set_gc_stress_for_test(Some("scheduler"));
        with_current_task_for_test(owner, || {
            assert_eq!(
                // SAFETY: The rooted frame describes owned a; b is rooted with the same type.
                unsafe { willow_async_mutex_handoff(frame, 24, 32, 40, b.cast(), 11) },
                1
            );
            // SAFETY: The live frame now holds b's ownership and unloaded phase.
            unsafe {
                assert_eq!(*frame.add(24).cast::<*mut u8>(), b.cast());
                assert_eq!(*frame.add(40).cast::<i64>(), 0);
                let next_token = *frame.add(32).cast::<i64>();
                assert_eq!(willow_async_mutex_load(b, next_token), 2);
                // The cancellation path at phase zero releases without committing
                // the previous node's still-present binding to b.
                assert_eq!(willow_async_mutex_release(b, next_token), 1);
            }
        });
        crate::gc::set_gc_stress_for_test(None);
        assert!(crate::gc::willow_gc_major_collections() > before);
        with_current_task_for_test(waiter, || {
            // SAFETY: The registered waiter owns this rooted mutex's reserved token.
            unsafe {
                assert_eq!(
                    willow_async_mutex_poll(a, waiter_token),
                    MUTEX_STATUS_ACQUIRED
                );
                assert_eq!(willow_async_mutex_load(a, waiter_token), 11);
                assert_eq!(willow_async_mutex_release(a, waiter_token), 1);
            }
        });
    }

    #[test]
    fn handoff_acquires_next_before_releasing_previous_with_constant_live_ownership() {
        let _guard = isolated_runtime();
        reset_global_scheduler_for_test();
        let owner = task();
        for count in [2, 8, 64, 512] {
            let nodes: Vec<_> = (0..count).map(|n| AsyncMutex::new(n, false)).collect();
            let mut token = nodes[0].try_handoff_acquire(owner).unwrap();
            let mut transfers = 0;
            for pair in nodes.windows(2) {
                let next_token = pair[1].try_handoff_acquire(owner).unwrap();
                assert_eq!(pair[0].owner(), Some((owner, token)));
                assert_eq!(pair[1].owner(), Some((owner, next_token)));
                assert_eq!(pair[0].frame_refs() + pair[1].frame_refs(), 2);
                assert!(matches!(
                    pair[0].release(owner, token),
                    MutexRelease::Released { .. }
                ));
                assert_eq!(pair[0].reclamation_status(), ReclamationStatus::Reclaimable);
                token = next_token;
                transfers += 1;
            }
            nodes.last().unwrap().release(owner, token);
            assert_eq!(transfers, count - 1);
            assert!(crate::lock_wait::lock_wait_link_of(owner).is_none());
            println!("nodes={count} transfers={transfers} peak_owned=2 queue_registrations=0");
        }
    }

    #[test]
    fn handoff_reverse_traversals_fail_without_publishing_waits() {
        let _guard = isolated_runtime();
        reset_global_scheduler_for_test();
        let a = AsyncMutex::new(1, false);
        let b = AsyncMutex::new(2, false);
        let first = task();
        let second = task();
        let a_token = a.try_handoff_acquire(first).unwrap();
        let b_token = b.try_handoff_acquire(second).unwrap();
        let attempted = Barrier::new(2);
        std::thread::scope(|scope| {
            let forward = scope.spawn(|| {
                let rejected = b.try_handoff_acquire(first).is_none();
                attempted.wait();
                a.release(first, a_token);
                rejected
            });
            let backward = scope.spawn(|| {
                let rejected = a.try_handoff_acquire(second).is_none();
                attempted.wait();
                b.release(second, b_token);
                rejected
            });
            // Assert after both participants cross the sequence point so a
            // regression cannot leave the peer trapped at the barrier.
            assert!(forward.join().unwrap());
            assert!(backward.join().unwrap());
        });
        assert_eq!((a.waiter_count(), b.waiter_count()), (0, 0));
        assert!(crate::lock_wait::lock_wait_link_of(first).is_none());
        assert!(crate::lock_wait::lock_wait_link_of(second).is_none());
        assert_eq!((a.frame_refs(), b.frame_refs()), (0, 0));
    }

    #[test]
    fn handoff_self_acquisition_and_queued_handoff_are_not_stolen() {
        let _guard = isolated_runtime();
        reset_global_scheduler_for_test();
        let a = AsyncMutex::new(7, false);
        let first = task();
        let queued = task();
        let barger = task();
        let token = a.try_handoff_acquire(first).unwrap();
        assert!(a.try_handoff_acquire(first).is_none());
        let MutexAcquire::Pending(wait_token) = a.acquire(queued) else {
            panic!("expected queue")
        };
        assert!(a.try_handoff_acquire(barger).is_none());
        a.release(first, token);
        assert!(a.try_handoff_acquire(barger).is_none());
        assert_eq!(a.poll_acquire(queued, wait_token), MutexResume::Acquired);
        a.release(queued, wait_token);
        let next = a.try_handoff_acquire(barger).unwrap();
        assert_eq!(a.load(barger, next), Some(7));
        a.release(barger, next);
        assert_eq!(a.reclamation_status(), ReclamationStatus::Reclaimable);
    }
}
