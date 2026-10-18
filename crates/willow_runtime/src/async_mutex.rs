//! Scheduler-aware `Mutex` runtime state machine (willow-38w.1.3,
//! spec §8.4–§8.7, §11, §12.2–§12.4, §15.1).
//!
//! Stage 2 ([`crate::lock_wait`]) built the waiter protocol: who is queued, how
//! a registration is identified across cancel/retry, and in which order the
//! lock state and the task shard may be taken. This module is the exclusive
//! lock *built on* that protocol — the part that actually owns a protected
//! value and moves a task through the acquire state machine:
//!
//! ```text
//! Idle -> Waiting(token) -> HandoffOwned(token) -> ValueLoaded -> Held -> Released
//! ```
//!
//! An uncontended acquire skips straight to `ValueLoaded` (§8.4). The `Waiting`
//! and `HandoffOwned` phases live in the task table (Stage 2's reverse link);
//! `ValueLoaded`/`Held` live in the generated async frame, which is why nothing
//! here loads the protected value until the caller proves ownership with its
//! `(lock_id, token)` pair.
//!
//! Four properties carry the design:
//!
//! 1. **Ownership is `(RuntimeTaskId, RegistrationToken)`, never a thread id.**
//!    A Willow task can be preempted mid-critical-section and resumed on a
//!    different worker, so a native-thread notion of ownership would be wrong
//!    the moment the scheduler migrates it (§8.2).
//!
//! 2. **Release is a direct FIFO handoff with no barging** (§8.6). A new
//!    arrival never takes the lock while anyone is queued, even in the window
//!    where `owner` is momentarily `None`. That window is exactly what
//!    [`crate::lock_wait::AsyncLockState::acquire_or_register`] refuses — and
//!    it decides "take it" versus "queue for it" under a single hold of the
//!    state lock, so a release cannot slip between the two and hand off to a
//!    queue this caller has not joined yet.
//!
//! 3. **A wake that cannot land is compensated, not dropped.** A `Terminal`
//!    wake revokes the exact reservation and the scan continues; a
//!    cancellation *after* a successful wake re-hands the lock on, so a
//!    `HandoffOwned` task that dies never strands the lock (§12.3).
//!
//! 4. **Native state is never freed while a relationship refers to it**
//!    (§15.1). The public handle is GC-managed and its finalizer frees the
//!    boxed native state only after the exact owner/waiter/frame accounting
//!    reports [`ReclamationStatus::Reclaimable`]. A mismatch is fatal in every
//!    build rather than risking a use-after-free.
//!
//! `lock <mutex> as [mut] value` lowers onto these entry points
//! (willow-38w.1.4); `lock read` / `lock write` use
//! [`crate::async_rwlock`] (willow-38w.1.5).

use crate::lock_wait::{
    AcquireOutcome, AsyncLockState, LockCancelOutcome, LockId, LockWaitLink, LockWaitPhase,
    RegistrationToken, cancel_lock_wait, consume_handoff, lock_wait_link_of,
    reconcile_cancelled_lock_wait,
};
use crate::task::RuntimeTaskId;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};

mod abi;

// Atomic admission switch. Low bits are a monotonic fast generation: odd
// means owned, even means free. Queued contention closes atomic admission until the queue
// empties. Reopening and slow-path registration share the backing state lock.
const SLOW: u64 = 1 << 63;
// Total pause-instruction budget, not a duration guarantee. Exponential
// backoff caps shared-word reads at about 4K and avoids cache-line flooding.
// Exhaustion closes admission and joins the unchanged FIFO scheduler queue.
const MAX_SPIN_PAUSES: usize = 262_144;

// Preserve the runtime API paths used by scheduler, GC, and generated-code tests.
pub use abi::*;

/// What an acquire attempt did. The caller (the generated frame, from Stage 4)
/// turns this into either a straight-line load or a `Pending` return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutexAcquire {
    /// Ownership is the caller's now. `Idle -> ValueLoaded`: the frame may load
    /// the protected value immediately.
    Acquired(RegistrationToken),
    /// Queued as `Waiting`. The frame stores the token, returns `Pending`, and
    /// resumes when the handoff wakes it.
    Pending(RegistrationToken),
    /// The same task already owns or is queued on this mutex. Non-reentrant by
    /// design (§11): a deterministic panic in both debug and release, rather
    /// than a silent self-deadlock.
    Recursive,
    /// The task cannot park (terminal, cancel-requested, or no longer in the
    /// task table), so no registration was published.
    Ineligible,
}

/// What a resumed frame found when it re-polled its acquire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutexResume {
    /// The reserved handoff was consumed. `HandoffOwned -> ValueLoaded`.
    Acquired,
    /// Still queued. The wake was spurious (or belonged to another wait).
    Pending,
    /// The registration is gone — cancelled, revoked, or already consumed. The
    /// caller must NOT touch the protected value.
    Lost,
}

/// What a release did with the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutexRelease {
    /// Ownership was released and handed directly to `handed_to`, or left free
    /// when nobody valid was queued.
    Released { handed_to: Option<RuntimeTaskId> },
    /// The caller was not the current owner: a stale generation, or a double
    /// release. A no-op, never a steal from the real owner.
    NotOwner,
}

/// What a cancellation reconciled, and whether it had to hand the lock on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutexCancel {
    /// The task had no registration on any mutex.
    NoLink,
    /// A queued `Waiting` registration was removed; the lock was untouched.
    RemovedWaiter,
    /// Reserved ownership was released and re-handed to `handed_to` (§12.3).
    /// Without this the lock would be stranded with an owner that will never
    /// run again.
    ReleasedOwnership { handed_to: Option<RuntimeTaskId> },
    /// The link was real but the lock had already moved past it. A no-op.
    Stale,
}

/// Whether the native state may be freed, and what is holding it if not.
///
/// Finalizer gate for §15.1. Deliberately not a debug-only assertion: a release
/// build that frees a referenced state is a use-after-free of a task's reverse
/// link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclamationStatus {
    /// No owner, no waiters, no frame-side ownership references.
    Reclaimable,
    Retained {
        owned: bool,
        waiters: usize,
        frame_refs: usize,
    },
}

/// One scheduler-aware exclusive lock: the Stage 2 waiter protocol plus the
/// protected value and the ownership accounting around it.
#[derive(Debug)]
pub struct AsyncMutex {
    /// Boxed so its address is stable: task-side links hold it (§8.2.2).
    state: Box<AsyncLockState>,
    fast: AtomicU64,
    fast_owner: AtomicU64,
    fast_published: AtomicU64,
    value: AtomicI64,
    /// Identity copied into the moving/GC-managed handle. Every native pointer
    /// use validates this against the stable boxed state before dereference.
    expected_lock_id: LockId,
    /// Whether `value` is a GC pointer the collector must trace.
    is_ref: bool,
    /// Queued-path frame ownership references (§15.1). Fast ownership uses
    /// the held bit instead; `frame_refs()` includes both. The old frame's
    /// reference is removed before reopening atomic admission, and a loaded
    /// owner remains retained across a queued ownership transfer.
    frame_refs: AtomicUsize,
}

impl AsyncMutex {
    pub fn new(value: i64, is_ref: bool) -> Box<Self> {
        Box::new(Self::new_payload(value, is_ref))
    }

    fn new_payload(value: i64, is_ref: bool) -> Self {
        let state = AsyncLockState::new_fast_mutex_backing();
        let expected_lock_id = state.lock_id();
        Self {
            state,
            fast: AtomicU64::new(0),
            fast_owner: AtomicU64::new(0),
            fast_published: AtomicU64::new(0),
            value: AtomicI64::new(value),
            expected_lock_id,
            is_ref,
            frame_refs: AtomicUsize::new(0),
        }
    }

    fn validated_state(&self) -> &AsyncLockState {
        if self.state.lock_id() != self.expected_lock_id {
            crate::panic_context::fatal_invariant(
                "scheduler-aware Mutex handle/state LockId mismatch",
            );
        }
        &self.state
    }

    pub fn lock_id(&self) -> LockId {
        self.validated_state().lock_id()
    }

    pub fn is_ref(&self) -> bool {
        self.is_ref
    }

    pub fn owner(&self) -> Option<(RuntimeTaskId, RegistrationToken)> {
        self.validated_state();
        loop {
            let fast = self.fast.load(Ordering::Acquire);
            if fast & 1 != 0 {
                let token = fast & !SLOW;
                if self.fast_published.load(Ordering::Acquire) != token {
                    std::hint::spin_loop();
                    continue;
                }
                let owner = self.fast_owner.load(Ordering::Relaxed);
                if self.fast.load(Ordering::Acquire) & !SLOW == token {
                    return Some((owner, token));
                }
            } else if fast & SLOW != 0 {
                return self.state.owner().filter(|&(task, _)| task != 0);
            } else {
                return None;
            }
        }
    }

    pub fn queued_waiters(&self) -> Vec<RuntimeTaskId> {
        self.validated_state().queued_waiters()
    }

    pub fn waiter_count(&self) -> usize {
        self.validated_state().waiter_count()
    }

    pub fn frame_refs(&self) -> usize {
        self.frame_refs.load(Ordering::Acquire) + (self.fast.load(Ordering::Acquire) & 1) as usize
    }

    /// Attempt to acquire for `task` (§8.5).
    ///
    /// The reentrancy check runs before any registration: a task is one logical
    /// thread of execution, so it cannot race with itself here, and answering
    /// before publishing anything keeps a rejected recursive acquire from
    /// leaving a queue entry behind.
    #[inline]
    pub fn acquire(&self, task: RuntimeTaskId) -> MutexAcquire {
        let fast = self.fast.load(Ordering::Relaxed);
        if fast & (SLOW | 1) == 0
            && fast < SLOW - 2
            && self
                .fast
                .compare_exchange(fast, fast + 1, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
        {
            self.fast_owner.store(task, Ordering::Relaxed);
            self.fast_published.store(fast + 1, Ordering::Release);
            return MutexAcquire::Acquired(fast + 1);
        }
        self.acquire_slow(task)
    }

    #[cold]
    fn acquire_slow(&self, task: RuntimeTaskId) -> MutexAcquire {
        self.validated_state();
        let spin_limit = if crate::scheduler::lock_spinning_allowed(task) {
            MAX_SPIN_PAUSES
        } else {
            0
        };
        let mut spins = 0;
        let mut backoff = 1;
        let mut fast = self.fast.load(Ordering::Acquire);
        loop {
            if fast & SLOW != 0 {
                let _no_preempt = crate::preempt::NoPreemptGuard::enter();
                if self.is_fast_owner(task) {
                    return MutexAcquire::Recursive;
                }
                if let Some(outcome) = self
                    .state
                    .acquire_mutex_if(task, || self.fast.load(Ordering::Acquire) & SLOW != 0)
                {
                    return match outcome {
                        AcquireOutcome::Acquired(token) => {
                            self.frame_refs.fetch_add(1, Ordering::AcqRel);
                            MutexAcquire::Acquired(token)
                        }
                        AcquireOutcome::Queued(token) => MutexAcquire::Pending(token),
                        AcquireOutcome::Recursive => MutexAcquire::Recursive,
                        AcquireOutcome::Ineligible => MutexAcquire::Ineligible,
                    };
                }
                fast = self.fast.load(Ordering::Acquire);
                continue;
            }
            if fast & 1 == 0 {
                if fast == SLOW - 2 {
                    crate::panic_context::fatal_invariant("Mutex fast token space exhausted");
                }
                match self.fast.compare_exchange_weak(
                    fast,
                    fast + 1,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => {
                        self.fast_owner.store(task, Ordering::Relaxed);
                        self.fast_published.store(fast + 1, Ordering::Release);
                        return MutexAcquire::Acquired(fast + 1);
                    }
                    Err(current) => {
                        fast = current;
                        continue;
                    }
                }
            }
            if self.fast_published.load(Ordering::Acquire) == fast
                && self.fast_owner.load(Ordering::Relaxed) == task
            {
                return MutexAcquire::Recursive;
            }
            if spins < spin_limit {
                let pauses = backoff.min(spin_limit - spins);
                for _ in 0..pauses {
                    std::hint::spin_loop();
                }
                spins += pauses;
                backoff = (backoff * 2).min(64);
                fast = self.fast.load(Ordering::Acquire);
                continue;
            }
            // The backing sentinel already closes admission. The fast owner
            // either releases before this CAS (retry), or observes SLOW and
            // releases the sentinel, progressing any registered waiter.
            match self.fast.compare_exchange_weak(
                fast,
                fast | SLOW,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => fast |= SLOW,
                Err(current) => fast = current,
            }
        }
    }

    /// Fast-owner recursion; slow ownership and membership are checked under
    /// the backing admission lock in `acquire_mutex_if`.
    fn is_fast_owner(&self, task: RuntimeTaskId) -> bool {
        let fast = self.fast.load(Ordering::Acquire);
        fast & 1 != 0
            && self.fast_published.load(Ordering::Acquire) == fast & !SLOW
            && self.fast_owner.load(Ordering::Relaxed) == task
        // Slow owner/queue recursion is checked under the admission lock.
    }

    /// Re-poll a pending acquire (§8.7). `token` is the one `acquire` returned.
    pub fn poll_acquire(&self, task: RuntimeTaskId, token: RegistrationToken) -> MutexResume {
        if token < SLOW {
            return if self.is_owner(task, token) {
                MutexResume::Acquired
            } else {
                MutexResume::Lost
            };
        }
        match lock_wait_link_of(task) {
            Some(link) if link.lock_id == self.lock_id() && link.token == token => {
                match link.phase {
                    // Still queued: a spurious wake, or a wake for a different
                    // wait. Park again.
                    LockWaitPhase::Waiting => MutexResume::Pending,
                    // Inspect the phase BEFORE consuming. A handoff between a
                    // failed consume and a later phase read is still live, not
                    // Lost (concurrent repeated-poll regression).
                    LockWaitPhase::HandoffOwned => {
                        if consume_handoff(task, self.lock_id(), token) {
                            self.frame_refs.fetch_add(1, Ordering::AcqRel);
                            MutexResume::Acquired
                        } else {
                            MutexResume::Lost
                        }
                    }
                }
            }
            // An uncontended acquire has no link; ownership alone is proof.
            _ if self.is_owner(task, token) => MutexResume::Acquired,
            _ => MutexResume::Lost,
        }
    }

    #[inline(always)]
    fn is_owner(&self, task: RuntimeTaskId, token: RegistrationToken) -> bool {
        if token < SLOW {
            token & 1 != 0
                && self.fast.load(Ordering::Acquire) & !SLOW == token
                && self.fast_published.load(Ordering::Acquire) == token
                && self.fast_owner.load(Ordering::Relaxed) == task
        } else {
            self.validated_state().exclusive_owner_is(task, token)
        }
    }

    /// Read the protected value. `None` unless `(task, token)` owns the lock —
    /// a `Waiting` or stale caller must never observe it (§8.4).
    pub fn load(&self, task: RuntimeTaskId, token: RegistrationToken) -> Option<i64> {
        if !self.is_owner(task, token) {
            return None;
        }
        Some(self.value.load(Ordering::Acquire))
    }

    /// Write the protected value back. Same ownership proof as [`Self::load`];
    /// a stale generation cannot commit over the current owner's work.
    pub fn commit(&self, task: RuntimeTaskId, token: RegistrationToken, value: i64) -> bool {
        if !self.is_owner(task, token) {
            return false;
        }
        self.store_owned(value);
        true
    }

    #[inline]
    fn store_owned(&self, value: i64) {
        if self.is_ref {
            crate::gc::willow_gc_write_barrier(
                self as *const Self as *mut u8,
                self.value.load(Ordering::Acquire) as *mut u8,
                value as *mut u8,
                crate::gc::GcStoreDestination::AsyncMutexCell as i64,
            );
        }
        self.value.store(value, Ordering::Release);
    }

    /// Release ownership and hand the lock directly to the next valid waiter
    /// (§8.6).
    ///
    /// The handoff is the Stage 2 primitive, which already compensates a
    /// `Terminal` wake by revoking the exact reservation and continuing the
    /// scan, so a dying waiter cannot swallow the lock.
    pub fn release(&self, task: RuntimeTaskId, token: RegistrationToken) -> MutexRelease {
        if token < SLOW {
            if !self.is_owner(task, token) {
                return MutexRelease::NotOwner;
            }
            return self.release_fast(token);
        }
        self.release_queued(task, token, true)
    }

    #[inline]
    fn commit_release(
        &self,
        task: RuntimeTaskId,
        token: RegistrationToken,
        value: i64,
    ) -> MutexRelease {
        if !self.is_owner(task, token) {
            return MutexRelease::NotOwner;
        }
        self.store_owned(value);
        if token < SLOW {
            self.release_fast(token)
        } else {
            self.release_queued(task, token, true)
        }
    }

    #[inline]
    fn release_fast(&self, token: RegistrationToken) -> MutexRelease {
        match self
            .fast
            .compare_exchange(token, token + 1, Ordering::Release, Ordering::Relaxed)
        {
            Ok(_) => MutexRelease::Released { handed_to: None },
            Err(current) if current == token | SLOW => {
                // No new fast acquisition is possible after SLOW. Clear
                // the fast frame reference before opening the FIFO gate.
                if self
                    .fast
                    .compare_exchange(
                        current,
                        SLOW | (token + 1),
                        Ordering::Release,
                        Ordering::Relaxed,
                    )
                    .is_err()
                {
                    return MutexRelease::NotOwner;
                }
                self.release_queued(0, 0, false)
            }
            Err(_) => MutexRelease::NotOwner,
        }
    }

    fn release_queued(
        &self,
        task: RuntimeTaskId,
        token: RegistrationToken,
        frame: bool,
    ) -> MutexRelease {
        let _no_preempt = crate::preempt::NoPreemptGuard::enter();
        let reopened = self
            .validated_state()
            .release_mutex_or_reopen(task, token, || {
                if frame {
                    self.release_frame_ref();
                }
                self.fast.fetch_and(!SLOW, Ordering::Release);
            });
        match reopened {
            None => MutexRelease::NotOwner,
            Some(true) => MutexRelease::Released { handed_to: None },
            Some(false) => {
                if frame {
                    self.release_frame_ref();
                }
                let handed_to = self
                    .state
                    .handoff_to_next_waiter_and_wake()
                    .map(|(next, _)| next);
                MutexRelease::Released { handed_to }
            }
        }
    }

    /// Release a frame-side ownership reference without underflowing. A double
    /// release is already rejected by the owner check, but the accounting must
    /// stay sound even if a future caller gets there another way.
    fn release_frame_ref(&self) {
        let _ = self
            .frame_refs
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_sub(1)
            });
    }

    /// Whether the native state may be freed (§15.1). Never a debug-only
    /// assertion: a release build that frees a retained state produces a
    /// use-after-free of some task's reverse link.
    pub fn reclamation_status(&self) -> ReclamationStatus {
        let fast = self.fast.load(Ordering::Acquire);
        if fast & SLOW == 0 {
            self.validated_state();
            return if fast & 1 == 0 {
                ReclamationStatus::Reclaimable
            } else {
                ReclamationStatus::Retained {
                    owned: true,
                    waiters: 0,
                    frame_refs: 1,
                }
            };
        }
        let frame_refs = self.frame_refs();
        let waiters = self.waiter_count();
        let state = self.validated_state();
        let owned = state.owner().is_some();
        if frame_refs == 0 && state.is_reclaimable() {
            ReclamationStatus::Reclaimable
        } else {
            ReclamationStatus::Retained {
                owned,
                waiters,
                frame_refs,
            }
        }
    }

    /// The current protected word, for the collector. Reading without ownership
    /// is correct here and only here: the collector needs the *current* root
    /// regardless of who holds the lock, and it never mutates it.
    fn gc_root(&self) -> Option<*mut u8> {
        if !self.is_ref {
            return None;
        }
        let word = self.value.load(Ordering::Acquire) as *mut u8;
        (!word.is_null()).then_some(word)
    }
}

/// Phase-driven cancellation cleanup for `task`, whatever it is waiting on
/// (§12.1, §12.2, §12.3).
///
/// A `Waiting` cancellation just removes the queue entry. A `HandoffOwned`
/// cancellation additionally hands the lock on: ownership was already reserved
/// for a task that will never resume, so without the re-handoff the lock is
/// stranded forever.
///
/// This is the *only* cancellation entry point, and deliberately so. A task has
/// at most one lock wait, recorded as a reverse link in the task table, and
/// cleanup must act on the lock that link names. An `AsyncMutex::cancel(&self,
/// task)` method reads as if it cancelled `task`'s wait *on that mutex*, but
/// `cancel_lock_wait` is task-directed: given a task queued on mutex B, calling
/// it through mutex A would reconcile B's registration and then re-hand *A*,
/// leaving B owned by a task that will never run and A handed to a waiter that
/// never asked. Only the receiver-less form can be correct, so only it exists.
///
/// Follows only the task's own reverse link — no lock registry scan and no task
/// scan. The compiler-generated cleanup retains the GC handle in its async
/// frame until this function returns. Scheduler terminal cleanup has the same
/// guarantee because terminal frame roots are released only after external
/// cleanup and outermost scheduler quiescence. The link is used only when its
/// `lock_id` matches the relationship that cancellation reconciled, and every
/// handle-side native-state access validates the expected `LockId`.
pub fn purge_task_lock_wait(task: RuntimeTaskId) -> MutexCancel {
    let link: Option<LockWaitLink> = lock_wait_link_of(task);
    finish_lock_wait_purge(link, cancel_lock_wait(task))
}

/// Reconcile a reverse link captured while the terminal task record was still
/// owned by the scheduler. The heavy record no longer exists, so looking the
/// link up by task id would necessarily return `NoLink`; carrying it in the
/// terminal-cleanup record preserves the O(1) cleanup contract.
pub(crate) fn purge_captured_task_lock_wait(
    task: RuntimeTaskId,
    link: Option<LockWaitLink>,
) -> MutexCancel {
    let Some(link) = link else {
        return MutexCancel::NoLink;
    };
    let outcome = reconcile_cancelled_lock_wait(task, link);
    finish_lock_wait_purge(Some(link), outcome)
}

fn finish_lock_wait_purge(link: Option<LockWaitLink>, outcome: LockCancelOutcome) -> MutexCancel {
    match outcome {
        LockCancelOutcome::NoLink => MutexCancel::NoLink,
        LockCancelOutcome::RemovedWaiter { .. } => MutexCancel::RemovedWaiter,
        LockCancelOutcome::ReleasedOwnership { lock_id, .. } => {
            let Some(link) = link.filter(|link| link.lock_id == lock_id) else {
                // The link changed generation underneath the cancellation; the
                // reconciliation already reported what it did, and there is no
                // state we may safely dereference.
                return MutexCancel::ReleasedOwnership { handed_to: None };
            };
            // SAFETY: the generated frame (or scheduler-retained terminal frame)
            // keeps the GC handle and its boxed state alive through this
            // cleanup. The link was read in this call and its `lock_id` matches
            // the relationship cancellation just reconciled. `cancel_lock_wait`
            // already validated the pointed-to state's id before mutating it.
            let state = unsafe { link.state.as_ref() };
            let handed_to = state
                .progress_waiters_and_wake()
                .into_iter()
                .next()
                .map(|(next, _, _)| next);
            MutexCancel::ReleasedOwnership { handed_to }
        }
        LockCancelOutcome::Stale { .. } => MutexCancel::Stale,
    }
}
