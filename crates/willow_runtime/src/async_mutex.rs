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
//!    (§15.1). Stage 3 only accounts for that — actual reclamation is
//!    willow-38w.1.6 — but the accounting is real, not a debug assertion, so
//!    the later stage cannot quietly free a live state.
//!
//! `lock <mutex> as [mut] value` lowers onto these entry points
//! (willow-38w.1.4). `lock read` / `lock write` on an `RwLock<T>` are still
//! gated by E2502 until Stage 5.

use crate::lock_wait::{
    AcquireOutcome, AsyncLockState, LockCancelOutcome, LockId, LockWaitLink, LockWaitPhase,
    RegistrationToken, cancel_lock_wait, consume_handoff, lock_wait_link_of,
};
use crate::task::RuntimeTaskId;
use std::os::raw::c_void;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

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
/// Stage 3 scaffolding for §15.1: reclamation itself is willow-38w.1.6, but the
/// *decision* is made here so a later stage cannot free a state with live
/// relationships. Deliberately not a debug-only assertion — a release build
/// that frees a referenced state is a use-after-free of a task's reverse link.
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
    /// The protected word. Scalars by value, GC values as their pointer, the
    /// same representation the blocking [`crate::lock`] cells use. Only a task
    /// that has proved `(task, token)` ownership reads or writes it, so the
    /// atomic is for publication between workers, not for mutual exclusion.
    value: AtomicI64,
    /// Whether `value` is a GC pointer the collector must trace.
    is_ref: bool,
    /// Frame-side ownership references (§15.1): incremented when a frame takes
    /// ownership, decremented when it releases. A lock whose owner has loaded
    /// the value is not reclaimable even for the instant `owner` is being
    /// reassigned.
    frame_refs: AtomicUsize,
}

impl AsyncMutex {
    pub fn new(value: i64, is_ref: bool) -> Box<Self> {
        Box::new(Self {
            state: AsyncLockState::new(),
            value: AtomicI64::new(value),
            is_ref,
            frame_refs: AtomicUsize::new(0),
        })
    }

    pub fn lock_id(&self) -> LockId {
        self.state.lock_id()
    }

    pub fn is_ref(&self) -> bool {
        self.is_ref
    }

    pub fn owner(&self) -> Option<(RuntimeTaskId, RegistrationToken)> {
        self.state.owner()
    }

    pub fn queued_waiters(&self) -> Vec<RuntimeTaskId> {
        self.state.queued_waiters()
    }

    pub fn waiter_count(&self) -> usize {
        self.state.waiter_count()
    }

    pub fn frame_refs(&self) -> usize {
        self.frame_refs.load(Ordering::Acquire)
    }

    /// Attempt to acquire for `task` (§8.5).
    ///
    /// The reentrancy check runs before any registration: a task is one logical
    /// thread of execution, so it cannot race with itself here, and answering
    /// before publishing anything keeps a rejected recursive acquire from
    /// leaving a queue entry behind.
    pub fn acquire(&self, task: RuntimeTaskId) -> MutexAcquire {
        if self.holds_or_awaits(task) {
            return MutexAcquire::Recursive;
        }
        // One critical section, not a try-then-register pair: see
        // `AsyncLockState::acquire_or_register` for the lost-wakeup window that
        // splitting it opens.
        match self.state.acquire_or_register(task) {
            AcquireOutcome::Acquired(token) => {
                // Idle -> ValueLoaded: no park, no reverse link, so there is
                // nothing for a cancellation to find in the task table.
                self.frame_refs.fetch_add(1, Ordering::AcqRel);
                MutexAcquire::Acquired(token)
            }
            AcquireOutcome::Queued(token) => MutexAcquire::Pending(token),
            AcquireOutcome::Ineligible => MutexAcquire::Ineligible,
        }
    }

    /// Whether `task` already owns this mutex or is queued on it (§11).
    fn holds_or_awaits(&self, task: RuntimeTaskId) -> bool {
        if matches!(self.state.owner(), Some((owner, _)) if owner == task) {
            return true;
        }
        matches!(lock_wait_link_of(task), Some(link) if link.lock_id == self.lock_id())
    }

    /// Re-poll a pending acquire (§8.7). `token` is the one `acquire` returned.
    pub fn poll_acquire(&self, task: RuntimeTaskId, token: RegistrationToken) -> MutexResume {
        if consume_handoff(task, self.lock_id(), token) {
            // HandoffOwned -> ValueLoaded. The reverse link is gone, so a late
            // `Waiting` cleanup cannot run against this generation.
            self.frame_refs.fetch_add(1, Ordering::AcqRel);
            return MutexResume::Acquired;
        }
        match lock_wait_link_of(task) {
            Some(link) if link.lock_id == self.lock_id() && link.token == token => {
                match link.phase {
                    // Still queued: a spurious wake, or a wake for a different
                    // wait. Park again.
                    LockWaitPhase::Waiting => MutexResume::Pending,
                    // Promoted but not consumable: the lock-side reservation no
                    // longer matches, so this generation is dead.
                    LockWaitPhase::HandoffOwned => MutexResume::Lost,
                }
            }
            // An uncontended acquire has no link; ownership alone is proof.
            _ if self.state.owner() == Some((task, token)) => MutexResume::Acquired,
            _ => MutexResume::Lost,
        }
    }

    /// Read the protected value. `None` unless `(task, token)` owns the lock —
    /// a `Waiting` or stale caller must never observe it (§8.4).
    pub fn load(&self, task: RuntimeTaskId, token: RegistrationToken) -> Option<i64> {
        if self.state.owner() != Some((task, token)) {
            return None;
        }
        Some(self.value.load(Ordering::Acquire))
    }

    /// Write the protected value back. Same ownership proof as [`Self::load`];
    /// a stale generation cannot commit over the current owner's work.
    pub fn commit(&self, task: RuntimeTaskId, token: RegistrationToken, value: i64) -> bool {
        if self.state.owner() != Some((task, token)) {
            return false;
        }
        self.value.store(value, Ordering::Release);
        true
    }

    /// Release ownership and hand the lock directly to the next valid waiter
    /// (§8.6).
    ///
    /// The handoff is the Stage 2 primitive, which already compensates a
    /// `Terminal` wake by revoking the exact reservation and continuing the
    /// scan, so a dying waiter cannot swallow the lock.
    pub fn release(&self, task: RuntimeTaskId, token: RegistrationToken) -> MutexRelease {
        if !self.state.release_owner(task, token) {
            return MutexRelease::NotOwner;
        }
        self.release_frame_ref();
        let handed_to = self
            .state
            .handoff_to_next_waiter_and_wake()
            .map(|(next, _)| next);
        MutexRelease::Released { handed_to }
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
        let frame_refs = self.frame_refs();
        let waiters = self.waiter_count();
        let owned = self.state.owner().is_some();
        if frame_refs == 0 && self.state.is_reclaimable() {
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
/// scan. Stage 3 never reclaims native states. The link captured before
/// cancellation is used only when its `lock_id` matches the relationship that
/// was actually reconciled; Stage 6 must additionally preserve the handle root
/// and validate the pointed-to state's id before enabling reclamation.
pub fn purge_task_lock_wait(task: RuntimeTaskId) -> MutexCancel {
    let link: Option<LockWaitLink> = lock_wait_link_of(task);
    match cancel_lock_wait(task) {
        LockCancelOutcome::NoLink => MutexCancel::NoLink,
        LockCancelOutcome::RemovedWaiter { .. } => MutexCancel::RemovedWaiter,
        LockCancelOutcome::ReleasedOwnership { lock_id, .. } => {
            let Some(link) = link.filter(|link| link.lock_id == lock_id) else {
                // The link changed generation underneath the cancellation; the
                // reconciliation already reported what it did, and there is no
                // state we may safely dereference.
                return MutexCancel::ReleasedOwnership { handed_to: None };
            };
            // SAFETY: Stage 3 states are program-lifetime allocations. The link
            // was read from the task table in this call and its `lock_id`
            // matches the relationship cancellation just reconciled. Stage 6
            // must replace this program-lifetime argument with rooted-handle
            // lifetime protection plus pointed-state id validation.
            let state = unsafe { link.state.as_ref() };
            let handed_to = state
                .handoff_to_next_waiter_and_wake()
                .map(|(next, _)| next);
            MutexCancel::ReleasedOwnership { handed_to }
        }
        LockCancelOutcome::Stale { .. } => MutexCancel::Stale,
    }
}

// ── C ABI ────────────────────────────────────────────────────────────────────
//
// Status codes are shared by the acquire/poll entry points so the generated
// frame branches on one convention. Negative values are failures.

/// Ownership is the caller's; the frame may load the protected value.
pub const MUTEX_STATUS_ACQUIRED: i32 = 1;
/// Registered and parked; the frame returns `Pending`.
pub const MUTEX_STATUS_PENDING: i32 = 0;
/// Same task, same mutex: non-reentrant (§11).
pub const MUTEX_STATUS_RECURSIVE: i32 = -1;
/// This acquisition's generation is gone (a revoked reservation, a stale
/// token). Recovery is a brand-new acquire, which either takes an uncontended
/// lock or joins the queue.
pub const MUTEX_STATUS_LOST: i32 = -2;
/// No registration was published because the task may not park: it is
/// cancel-requested, already `Cancelling`, or terminal.
///
/// Deliberately distinct from [`MUTEX_STATUS_LOST`] (willow-38w.1.4 review): a
/// retry cannot clear this condition, since only the scheduler can — by
/// claiming the task and running its cancellation entry. Generated code
/// therefore returns `Pending` and gives the worker back instead of re-arming
/// the acquire, which would spin on this status for as long as the current
/// owner holds the lock, and forever when that owner needs this worker.
pub const MUTEX_STATUS_CANCELLED: i32 = -3;

/// Ref-holding mutexes, so the collector can trace the protected value. Native
/// states are program-lifetime for now (reclamation is willow-38w.1.6), so
/// entries are never removed.
static ASYNC_MUTEX_GC_REGISTRY: StdMutex<Vec<usize>> = StdMutex::new(Vec::new());

fn mutex_from_raw<'a>(raw: *mut c_void) -> Option<&'a AsyncMutex> {
    (!raw.is_null()).then(|| unsafe { &*(raw as *const AsyncMutex) })
}

/// The running task, or `None` outside any task. A lock statement is only legal
/// inside an `async fn` (§6.1), so `None` means the caller is misusing the ABI.
fn current_task() -> Option<RuntimeTaskId> {
    let id = crate::scheduler::willow_sched_current_task();
    (id != 0).then_some(id as RuntimeTaskId)
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_async_mutex_new(value: i64, is_ref: i64) -> *mut c_void {
    let is_ref = is_ref != 0;
    let raw = Box::into_raw(AsyncMutex::new(value, is_ref));
    if is_ref {
        ASYNC_MUTEX_GC_REGISTRY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(raw as usize);
    }
    raw as *mut c_void
}

/// Acquire for the running task. Writes the registration token to `out_token`
/// on `ACQUIRED` and `PENDING`; leaves it untouched otherwise.
#[unsafe(no_mangle)]
pub extern "C" fn willow_async_mutex_acquire(raw: *mut c_void, out_token: *mut i64) -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let (Some(mutex), Some(task)) = (mutex_from_raw(raw), current_task()) else {
        return MUTEX_STATUS_LOST;
    };
    let store = |token: RegistrationToken| {
        if !out_token.is_null() {
            unsafe { *out_token = token as i64 };
        }
    };
    match mutex.acquire(task) {
        MutexAcquire::Acquired(token) => {
            store(token);
            MUTEX_STATUS_ACQUIRED
        }
        MutexAcquire::Pending(token) => {
            store(token);
            MUTEX_STATUS_PENDING
        }
        MutexAcquire::Recursive => MUTEX_STATUS_RECURSIVE,
        MutexAcquire::Ineligible => MUTEX_STATUS_CANCELLED,
    }
}

/// Re-poll a pending acquire after a wake.
#[unsafe(no_mangle)]
pub extern "C" fn willow_async_mutex_poll(raw: *mut c_void, token: i64) -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let (Some(mutex), Some(task)) = (mutex_from_raw(raw), current_task()) else {
        return MUTEX_STATUS_LOST;
    };
    match mutex.poll_acquire(task, token as RegistrationToken) {
        MutexResume::Acquired => MUTEX_STATUS_ACQUIRED,
        MutexResume::Pending => MUTEX_STATUS_PENDING,
        MutexResume::Lost => MUTEX_STATUS_LOST,
    }
}

/// Load the protected value. Returns `0` when the caller does not own the lock;
/// the generated frame only calls this after an `ACQUIRED` status, so a `0` here
/// means the ABI was misused rather than that the value is zero.
#[unsafe(no_mangle)]
pub extern "C" fn willow_async_mutex_load(raw: *mut c_void, token: i64) -> i64 {
    let (Some(mutex), Some(task)) = (mutex_from_raw(raw), current_task()) else {
        return 0;
    };
    mutex.load(task, token as RegistrationToken).unwrap_or(0)
}

/// Commit the protected value. Returns 1 on success, 0 if the caller is not the
/// current owner.
#[unsafe(no_mangle)]
pub extern "C" fn willow_async_mutex_commit(raw: *mut c_void, token: i64, value: i64) -> i32 {
    let (Some(mutex), Some(task)) = (mutex_from_raw(raw), current_task()) else {
        return 0;
    };
    mutex.commit(task, token as RegistrationToken, value) as i32
}

/// Release ownership and hand off. Returns 1 on success, 0 if the caller was not
/// the owner (a stale or double release).
#[unsafe(no_mangle)]
pub extern "C" fn willow_async_mutex_release(raw: *mut c_void, token: i64) -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let (Some(mutex), Some(task)) = (mutex_from_raw(raw), current_task()) else {
        return 0;
    };
    matches!(
        mutex.release(task, token as RegistrationToken),
        MutexRelease::Released { .. }
    ) as i32
}

/// Cancellation cleanup for the running task, whatever phase it is in.
#[unsafe(no_mangle)]
pub extern "C" fn willow_async_mutex_cancel() -> i32 {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let Some(task) = current_task() else {
        return 0;
    };
    !matches!(purge_task_lock_wait(task), MutexCancel::NoLink) as i32
}

/// Raise the non-reentrant-mutex fault (§11, §22 "Reentrant lock"). Separate
/// from the acquire entry point so the generated code supplies the source
/// location of the offending `lock` statement.
#[unsafe(no_mangle)]
pub extern "C" fn willow_async_mutex_recursive_panic(file: *const u8, line: i32, col: i32) {
    crate::panic_context::raise_language_message_at(
        "recursive lock acquisition on non-reentrant Mutex",
        file,
        line.into(),
        col.into(),
    );
}

/// Live GC roots held by ref-typed scheduler-aware mutexes.
pub(crate) fn async_mutex_gc_roots() -> Vec<*mut u8> {
    let Ok(registry) = ASYNC_MUTEX_GC_REGISTRY.lock() else {
        return Vec::new();
    };
    registry
        .iter()
        .filter_map(|&addr| {
            // SAFETY: registry entries are `Box::into_raw` pointers to
            // program-lifetime states that are never freed (§15.1 reclamation
            // is willow-38w.1.6).
            let mutex = unsafe { &*(addr as *const AsyncMutex) };
            mutex.gc_root()
        })
        .collect()
}

/// Test-only: forget every registration.
///
/// Native states are program-lifetime until willow-38w.1.6 gives them a real
/// reclamation path, so the registry normally only grows. That is safe for the
/// registrations production code makes, but a test that registers a cell
/// holding a *real* GC object must undo it: the next `willow_gc_init()` resets
/// the heap, and a root still pointing into the old one aborts the collection
/// that finds it — in whichever unrelated test happens to run next.
#[cfg(test)]
pub(crate) fn clear_async_mutex_gc_registry_for_test() {
    ASYNC_MUTEX_GC_REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{runtime_test_guard, total_frees_for_test, willow_gc_collect, willow_gc_init};
    use crate::scheduler::{
        reset_global_scheduler_for_test, single_worker_for_test, willow_sched_cancel,
        willow_sched_current_task, willow_sched_run, willow_sched_spawn, willow_sched_wake,
        with_current_task_for_test, with_global_for_test,
    };
    use crate::string::{willow_string_as_str, willow_string_from_str};
    use crate::task::{RUNTIME_POLL_PENDING, RUNTIME_POLL_READY};
    use crate::task_state::{ClaimOutcome, WakeOutcome};
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::sync::Mutex as TestMutex;
    use std::sync::atomic::AtomicU64;

    // Test perspectives for the Stage 3 Mutex state machine (willow-38w.1.3).
    // Each maps to at least one test below.
    //
    //  1. A fresh mutex is unowned, has no waiters, and is reclaimable.
    //  2. Uncontended acquire takes ownership, returns a token, and publishes
    //     no reverse link (Idle -> ValueLoaded, §8.4).
    //  3. Only the proven owner can load the protected value.
    //  4. Only the proven owner can commit, and the commit is visible to the
    //     next owner.
    //  5. A second task acquiring a held mutex is queued Pending and cannot
    //     observe the value.
    //  6. Release hands the lock to the FIFO head, not the newest arrival.
    //  7. No barging: a fresh arrival is queued behind existing waiters even
    //     though the release window momentarily has no owner (§8.6).
    //  8. poll_acquire consumes a reserved handoff (HandoffOwned -> ValueLoaded)
    //     and is idempotent.
    //  9. A spurious wake while still Waiting re-parks (Pending), it does not
    //     grant the lock.
    // 10. poll_acquire with a stale or foreign token reports Lost and grants
    //     nothing.
    // 11. The current owner re-acquiring is Recursive, not a self-deadlock
    //     (§11).
    // 12. A task already queued on the mutex re-acquiring is Recursive too.
    // 13. A Recursive rejection publishes nothing: owner and queue are
    //     unchanged.
    // 14. Acquire from a terminal task is Ineligible and publishes nothing.
    // 15. Acquire from a cancel-requested task is Ineligible.
    // 16. Release with a stale token or from a non-owner is NotOwner and never
    //     steals from the live owner.
    // 17. A double release is NotOwner the second time.
    // 18. Terminal wake compensation: the handoff revokes the dead waiter's
    //     reservation and admits the next live one (§8.6).
    // 19. A wake that cannot land because the task record is gone is skipped
    //     the same way.
    // 20. Post-wake cancellation re-hands the lock, so a HandoffOwned task that
    //     dies never strands it (§12.3).
    // 21. Cancelling a Waiting waiter removes it, and a later release skips it.
    // 22. Cancelling a task with no registration is NoLink.
    // 23. The scheduler's terminal-cleanup path re-hands a reserved lock via
    //     purge_task_lock_wait.
    // 24. Concurrent acquire from many threads yields exactly one owner and
    //     queues every loser exactly once.
    // 25. 10k waiters queue and drain in FIFO order with no lost wakeup.
    // 26. Reclamation is refused while owned, queued, or frame-referenced, and
    //     granted only after a full drain (§15.1).
    // 27. Frame-reference accounting is balanced across acquire/handoff/release
    //     and never underflows on a rejected release.
    // 28. GC roots cover a ref-typed protected word and skip non-ref and null
    //     words.
    // 29. The C ABI routes acquire/poll/load/commit/release and publishes the
    //     token through the out-parameter.
    // 30. The C ABI reports LOST for a null handle or no running task, and
    //     never touches the value.
    // 31. A cancellation storm leaves the lock consistent and finally
    //     reclaimable.
    // 32. Ownership is task-based, not thread-based: it survives being released
    //     from a different worker thread (§8.2), and a single worker still
    //     drains a handoff chain.
    // 33. Wake-before-park: a handoff to a task that is already inside a poll
    //     still reserves ownership, and that task's own poll consumes it.
    // 34. Cleanup acts on the lock the task is actually linked to. With two
    //     mutexes live, retiring a task that is HandoffOwned on B re-hands B's
    //     ownership to B's next waiter and leaves A's owner and queue alone.
    // 35. The same holds in the Waiting phase: only B's queue loses an entry.
    // 36. Real scheduler, one worker: a task that parks on a held mutex does not
    //     block the worker — an unrelated task runs to completion while it is
    //     parked, and the parked task resumes only after the handoff.
    // 37. Real scheduler, one worker: a chain of waiters acquires in the order
    //     they registered, every task completes, and none is left parked.
    // 38. Real scheduler: cancelling a parked waiter's task retires it without
    //     stranding the lock — the remaining tasks still complete.
    // 39. A real collection traces a ref cell's protected word: an object whose
    //     only reference is the mutex survives, with its contents intact.
    // 40. The same collection reclaims an equivalent unreferenced object, so 39
    //     is a fact about the root wiring and not about a collector that never
    //     frees anything.
    // 41. The C ABI distinguishes a cancel-requested task from a stale handle:
    //     acquire publishes no waiter/token and returns CANCELLED, not LOST.

    fn parked_task() -> RuntimeTaskId {
        with_global_for_test(|sched| sched.spawn_parked_placeholder())
    }

    fn ready_task() -> RuntimeTaskId {
        with_global_for_test(|sched| sched.spawn_placeholder())
    }

    /// Drive a parked task to `Terminal` the only way the state word allows.
    fn make_terminal(task: RuntimeTaskId) {
        with_global_for_test(|sched| {
            sched.with_task_for_test(task, |task| {
                assert_ne!(task.state.wake(), WakeOutcome::Terminal);
                assert_eq!(task.state.claim_for_poll(), ClaimOutcome::Poll);
                assert!(task.state.finish_terminal());
            })
        });
    }

    /// Claim a parked task for a poll, so a wake has to be recorded against the
    /// running poll instead of enqueued.
    fn make_running(task: RuntimeTaskId) {
        with_global_for_test(|sched| {
            sched.with_task_for_test(task, |task| {
                assert_ne!(task.state.wake(), WakeOutcome::Terminal);
                assert_eq!(task.state.claim_for_poll(), ClaimOutcome::Poll);
            })
        });
    }

    fn drop_task_record(task: RuntimeTaskId) {
        with_global_for_test(|sched| sched.remove_task_for_test(task));
    }

    /// Acquire, expecting the uncontended path.
    fn acquire_now(mutex: &AsyncMutex, task: RuntimeTaskId) -> RegistrationToken {
        match mutex.acquire(task) {
            MutexAcquire::Acquired(token) => token,
            other => panic!("expected an uncontended acquire, got {other:?}"),
        }
    }

    /// Acquire, expecting to be queued.
    fn queue_on(mutex: &AsyncMutex, task: RuntimeTaskId) -> RegistrationToken {
        match mutex.acquire(task) {
            MutexAcquire::Pending(token) => token,
            other => panic!("expected to be queued, got {other:?}"),
        }
    }

    // 1
    #[test]
    fn fresh_mutex_is_idle_and_reclaimable() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let mutex = AsyncMutex::new(7, false);
        assert_ne!(mutex.lock_id(), 0);
        assert_eq!(mutex.owner(), None);
        assert_eq!(mutex.waiter_count(), 0);
        assert_eq!(mutex.frame_refs(), 0);
        assert_eq!(mutex.reclamation_status(), ReclamationStatus::Reclaimable);
    }

    // 2, 3
    #[test]
    fn uncontended_acquire_owns_without_a_reverse_link() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let mutex = AsyncMutex::new(41, false);
        let owner = parked_task();
        let other = parked_task();

        let token = acquire_now(&mutex, owner);
        assert_eq!(mutex.owner(), Some((owner, token)));
        // Idle -> ValueLoaded never parks, so there is nothing in the task
        // table for a cancellation to find.
        assert!(lock_wait_link_of(owner).is_none());

        assert_eq!(mutex.load(owner, token), Some(41));
        // A non-owner, and the owner with the wrong token, both see nothing.
        assert_eq!(mutex.load(other, token), None);
        assert_eq!(mutex.load(owner, token + 1), None);
    }

    // 4
    #[test]
    fn only_the_owner_commits_and_the_next_owner_sees_it() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let mutex = AsyncMutex::new(0, false);
        let first = parked_task();
        let second = parked_task();

        let first_token = acquire_now(&mutex, first);
        assert!(!mutex.commit(second, first_token, 99), "not the owner");
        assert!(!mutex.commit(first, first_token + 1, 99), "stale token");
        assert!(mutex.commit(first, first_token, 5));

        let second_token = queue_on(&mutex, second);
        assert!(matches!(
            mutex.release(first, first_token),
            MutexRelease::Released {
                handed_to: Some(next)
            } if next == second
        ));
        assert_eq!(
            mutex.poll_acquire(second, second_token),
            MutexResume::Acquired
        );
        assert_eq!(mutex.load(second, second_token), Some(5));
    }

    // 5, 6, 7
    #[test]
    fn waiters_are_fifo_and_a_new_arrival_never_bargs() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let mutex = AsyncMutex::new(1, false);
        let owner = parked_task();
        let first = parked_task();
        let second = parked_task();

        let owner_token = acquire_now(&mutex, owner);
        let first_token = queue_on(&mutex, first);
        let second_token = queue_on(&mutex, second);
        assert_eq!(mutex.queued_waiters(), vec![first, second]);
        // A queued waiter has no ownership, so it must not read the value.
        assert_eq!(mutex.load(first, first_token), None);

        // Release goes to the FIFO head, not the newest arrival.
        let MutexRelease::Released { handed_to } = mutex.release(owner, owner_token) else {
            panic!("owner should be able to release");
        };
        assert_eq!(handed_to, Some(first));
        assert_eq!(mutex.owner(), Some((first, first_token)));

        // A fresh arrival during the handoff is queued behind `second`, even
        // though it arrived after the lock had been released once.
        let late = parked_task();
        let late_token = queue_on(&mutex, late);
        assert_eq!(mutex.queued_waiters(), vec![second, late]);
        assert_ne!(late_token, second_token);
    }

    // 8, 9, 10
    #[test]
    fn poll_acquire_distinguishes_handoff_spurious_wake_and_loss() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let mutex = AsyncMutex::new(0, false);
        let owner = parked_task();
        let waiter = parked_task();

        let owner_token = acquire_now(&mutex, owner);
        let waiter_token = queue_on(&mutex, waiter);

        // Still Waiting: a wake for some other reason must re-park, not grant.
        assert_eq!(
            mutex.poll_acquire(waiter, waiter_token),
            MutexResume::Pending
        );
        // A token that was never issued to this waiter grants nothing.
        assert_eq!(
            mutex.poll_acquire(waiter, waiter_token + 100),
            MutexResume::Lost
        );

        assert!(matches!(
            mutex.release(owner, owner_token),
            MutexRelease::Released { .. }
        ));
        assert_eq!(
            mutex.poll_acquire(waiter, waiter_token),
            MutexResume::Acquired
        );
        assert!(lock_wait_link_of(waiter).is_none(), "link is consumed");
        // Idempotent: a second poll of the same generation still reports the
        // ownership it already holds rather than double-counting it.
        assert_eq!(
            mutex.poll_acquire(waiter, waiter_token),
            MutexResume::Acquired
        );
        assert_eq!(mutex.frame_refs(), 1);
    }

    // 11, 12, 13
    #[test]
    fn recursive_acquire_is_rejected_and_changes_nothing() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let mutex = AsyncMutex::new(0, false);
        let owner = parked_task();
        let waiter = parked_task();

        let owner_token = acquire_now(&mutex, owner);
        let waiter_token = queue_on(&mutex, waiter);

        // The owner re-acquiring would self-deadlock: reject deterministically.
        assert_eq!(mutex.acquire(owner), MutexAcquire::Recursive);
        // A queued waiter re-acquiring is the same fault one phase earlier.
        assert_eq!(mutex.acquire(waiter), MutexAcquire::Recursive);

        assert_eq!(mutex.owner(), Some((owner, owner_token)));
        assert_eq!(mutex.queued_waiters(), vec![waiter]);
        assert_eq!(
            lock_wait_link_of(waiter).map(|link| link.token),
            Some(waiter_token),
            "a rejected recursive acquire must not re-register the waiter"
        );

        // A different mutex is not recursion: the same task may hold both.
        let other = AsyncMutex::new(0, false);
        assert!(matches!(
            other.acquire(owner),
            MutexAcquire::Acquired(_) | MutexAcquire::Pending(_)
        ));
    }

    // 14, 15
    #[test]
    fn a_task_that_cannot_park_is_ineligible() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let mutex = AsyncMutex::new(0, false);
        let owner = parked_task();
        let dead = parked_task();
        let cancelled = parked_task();
        let unknown = 999_999;

        let _owner_token = acquire_now(&mutex, owner);

        make_terminal(dead);
        assert_eq!(mutex.acquire(dead), MutexAcquire::Ineligible);
        willow_sched_cancel(cancelled);
        assert_eq!(mutex.acquire(cancelled), MutexAcquire::Ineligible);
        assert_eq!(mutex.acquire(unknown), MutexAcquire::Ineligible);

        assert_eq!(mutex.waiter_count(), 0, "no registration was published");
        assert!(lock_wait_link_of(dead).is_none());
        assert!(lock_wait_link_of(cancelled).is_none());
    }

    // 16, 17
    #[test]
    fn a_stale_or_repeated_release_never_steals_the_lock() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let mutex = AsyncMutex::new(0, false);
        let owner = parked_task();
        let stranger = parked_task();

        let token = acquire_now(&mutex, owner);
        assert_eq!(mutex.release(stranger, token), MutexRelease::NotOwner);
        assert_eq!(mutex.release(owner, token + 1), MutexRelease::NotOwner);
        assert_eq!(mutex.owner(), Some((owner, token)), "still held");

        assert!(matches!(
            mutex.release(owner, token),
            MutexRelease::Released { handed_to: None }
        ));
        assert_eq!(mutex.owner(), None);
        // A double release must not un-own a lock a later task now holds.
        assert_eq!(mutex.release(owner, token), MutexRelease::NotOwner);

        let next = parked_task();
        let next_token = acquire_now(&mutex, next);
        assert_eq!(mutex.release(owner, token), MutexRelease::NotOwner);
        assert_eq!(mutex.owner(), Some((next, next_token)));
    }

    // 18, 19
    #[test]
    fn a_wake_that_cannot_land_is_compensated_not_dropped() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let mutex = AsyncMutex::new(0, false);
        let owner = parked_task();
        let dead = parked_task();
        let gone = parked_task();
        let live = parked_task();

        let owner_token = acquire_now(&mutex, owner);
        let _dead_token = queue_on(&mutex, dead);
        let _gone_token = queue_on(&mutex, gone);
        let live_token = queue_on(&mutex, live);

        // Both leading waiters become un-wakeable after they were queued.
        make_terminal(dead);
        drop_task_record(gone);

        let MutexRelease::Released { handed_to } = mutex.release(owner, owner_token) else {
            panic!("owner should be able to release");
        };
        assert_eq!(
            handed_to,
            Some(live),
            "the scan must revoke each failed reservation and continue"
        );
        assert_eq!(mutex.owner(), Some((live, live_token)));
        assert_eq!(mutex.waiter_count(), 0);
    }

    // 20, 21, 22
    #[test]
    fn cancellation_reconciles_by_phase_and_re_hands_reserved_ownership() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let mutex = AsyncMutex::new(0, false);
        let owner = parked_task();
        let doomed = parked_task();
        let survivor = parked_task();
        let bystander = parked_task();

        let owner_token = acquire_now(&mutex, owner);
        let _doomed_token = queue_on(&mutex, doomed);
        let survivor_token = queue_on(&mutex, survivor);

        // A task with no registration at all.
        assert_eq!(purge_task_lock_wait(bystander), MutexCancel::NoLink);

        // Reserve ownership for `doomed`, then cancel it before it can resume:
        // without the re-handoff the lock would be owned by a task that never
        // runs again.
        assert!(matches!(
            mutex.release(owner, owner_token),
            MutexRelease::Released { handed_to: Some(_) }
        ));
        assert_eq!(mutex.owner().map(|(task, _)| task), Some(doomed));
        assert_eq!(
            purge_task_lock_wait(doomed),
            MutexCancel::ReleasedOwnership {
                handed_to: Some(survivor)
            }
        );
        assert_eq!(mutex.owner(), Some((survivor, survivor_token)));

        // A Waiting cancellation just removes the queue entry, and a later
        // release skips the removed waiter.
        let late = parked_task();
        let _late_token = queue_on(&mutex, late);
        assert_eq!(purge_task_lock_wait(late), MutexCancel::RemovedWaiter);
        assert_eq!(mutex.queued_waiters(), Vec::<RuntimeTaskId>::new());
        assert!(matches!(
            mutex.release(survivor, survivor_token),
            MutexRelease::Released { handed_to: None }
        ));
    }

    // 34, 35
    #[test]
    fn cleanup_acts_on_the_task_s_own_lock_not_a_bystander_lock() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        // Two live mutexes. Everything interesting happens on `b`; `a` exists
        // only to be left alone.
        let a = AsyncMutex::new(0, false);
        let b = AsyncMutex::new(0, false);
        assert_ne!(a.lock_id(), b.lock_id());

        let a_owner = parked_task();
        let a_waiter = parked_task();
        let a_owner_token = acquire_now(&a, a_owner);
        let a_waiter_token = queue_on(&a, a_waiter);

        let b_owner = parked_task();
        let b_doomed = parked_task();
        let b_survivor = parked_task();
        let b_owner_token = acquire_now(&b, b_owner);
        let _b_doomed_token = queue_on(&b, b_doomed);
        let b_survivor_token = queue_on(&b, b_survivor);

        // Reserve B for `b_doomed`, then retire it. Cleanup is task-directed:
        // it must find B through the task's reverse link. The pre-review API
        // took `&self`, so a caller holding A would have reconciled B's
        // registration and then re-handed *A* — B's survivor would have parked
        // forever and A's waiter would have been handed a lock it was still
        // queued for.
        assert!(matches!(
            b.release(b_owner, b_owner_token),
            MutexRelease::Released { handed_to: Some(_) }
        ));
        assert_eq!(b.owner().map(|(task, _)| task), Some(b_doomed));
        assert_eq!(
            purge_task_lock_wait(b_doomed),
            MutexCancel::ReleasedOwnership {
                handed_to: Some(b_survivor)
            },
            "the re-handoff must happen on B, the lock the task was linked to"
        );
        assert_eq!(b.owner(), Some((b_survivor, b_survivor_token)));

        // A is untouched: same owner, same single queued waiter.
        assert_eq!(a.owner(), Some((a_owner, a_owner_token)));
        assert_eq!(a.queued_waiters(), vec![a_waiter]);
        assert_eq!(a.waiter_count(), 1);

        // The Waiting phase behaves the same way: retiring a task queued on B
        // removes exactly one entry, from B.
        let b_late = parked_task();
        let _b_late_token = queue_on(&b, b_late);
        assert_eq!(purge_task_lock_wait(b_late), MutexCancel::RemovedWaiter);
        assert_eq!(b.queued_waiters(), Vec::<RuntimeTaskId>::new());
        assert_eq!(a.queued_waiters(), vec![a_waiter]);

        // And both locks still drain normally afterwards.
        assert!(matches!(
            b.release(b_survivor, b_survivor_token),
            MutexRelease::Released { handed_to: None }
        ));
        let MutexRelease::Released { handed_to } = a.release(a_owner, a_owner_token) else {
            panic!("A's owner should still be able to release");
        };
        assert_eq!(handed_to, Some(a_waiter));
        assert_eq!(
            a.poll_acquire(a_waiter, a_waiter_token),
            MutexResume::Acquired
        );
    }

    // 23
    #[test]
    fn terminal_cleanup_path_re_hands_a_reserved_lock() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let mutex = AsyncMutex::new(0, false);
        let owner = parked_task();
        let doomed = parked_task();
        let survivor = parked_task();

        let owner_token = acquire_now(&mutex, owner);
        let _doomed_token = queue_on(&mutex, doomed);
        let survivor_token = queue_on(&mutex, survivor);
        assert!(matches!(
            mutex.release(owner, owner_token),
            MutexRelease::Released { .. }
        ));

        // This is what the scheduler runs for a retiring task: it follows only
        // the task's own reverse link, with no registry or task scan.
        assert_eq!(
            purge_task_lock_wait(doomed),
            MutexCancel::ReleasedOwnership {
                handed_to: Some(survivor)
            }
        );
        assert_eq!(mutex.owner(), Some((survivor, survivor_token)));
        assert_eq!(purge_task_lock_wait(doomed), MutexCancel::NoLink);
    }

    // 24
    #[test]
    fn concurrent_acquire_yields_exactly_one_owner() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let mutex = AsyncMutex::new(0, false);
        let tasks: Vec<RuntimeTaskId> = (0..16).map(|_| parked_task()).collect();
        let barrier = Arc::new(Barrier::new(tasks.len()));

        let outcomes: Vec<MutexAcquire> = std::thread::scope(|scope| {
            let handles: Vec<_> = tasks
                .iter()
                .map(|&task| {
                    let barrier = Arc::clone(&barrier);
                    let mutex = &mutex;
                    scope.spawn(move || {
                        barrier.wait();
                        mutex.acquire(task)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let acquired: Vec<_> = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, MutexAcquire::Acquired(_)))
            .collect();
        let pending = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, MutexAcquire::Pending(_)))
            .count();
        assert_eq!(acquired.len(), 1, "exactly one winner: {outcomes:?}");
        assert_eq!(pending, tasks.len() - 1, "every loser queued exactly once");
        assert_eq!(mutex.waiter_count(), tasks.len() - 1);
        assert!(mutex.owner().is_some());
    }

    // 25
    #[test]
    fn ten_thousand_waiters_drain_in_fifo_order() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let mutex = AsyncMutex::new(0, false);
        let owner = parked_task();
        let owner_token = acquire_now(&mutex, owner);

        let waiters: Vec<(RuntimeTaskId, RegistrationToken)> = (0..10_000)
            .map(|_| {
                let task = parked_task();
                (task, queue_on(&mutex, task))
            })
            .collect();
        assert_eq!(mutex.waiter_count(), waiters.len());

        let mut current = (owner, owner_token);
        for (index, &(task, token)) in waiters.iter().enumerate() {
            let MutexRelease::Released { handed_to } = mutex.release(current.0, current.1) else {
                panic!("owner {index} should be able to release");
            };
            assert_eq!(handed_to, Some(task), "waiter {index} out of FIFO order");
            assert_eq!(
                mutex.poll_acquire(task, token),
                MutexResume::Acquired,
                "waiter {index} lost its wake"
            );
            // Each hop hands the value along, so a dropped handoff would show
            // up as a gap in the count.
            assert!(mutex.commit(task, token, index as i64 + 1));
            current = (task, token);
        }

        assert_eq!(mutex.load(current.0, current.1), Some(waiters.len() as i64));
        assert!(matches!(
            mutex.release(current.0, current.1),
            MutexRelease::Released { handed_to: None }
        ));
    }

    // 26, 27
    #[test]
    fn reclamation_and_frame_refs_track_live_relationships() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let mutex = AsyncMutex::new(0, false);
        let owner = parked_task();
        let waiter = parked_task();

        let owner_token = acquire_now(&mutex, owner);
        assert_eq!(mutex.frame_refs(), 1);
        assert_eq!(
            mutex.reclamation_status(),
            ReclamationStatus::Retained {
                owned: true,
                waiters: 0,
                frame_refs: 1,
            }
        );

        let waiter_token = queue_on(&mutex, waiter);
        assert_eq!(
            mutex.reclamation_status(),
            ReclamationStatus::Retained {
                owned: true,
                waiters: 1,
                frame_refs: 1,
            }
        );

        // A rejected release must not decrement the accounting.
        assert_eq!(mutex.release(waiter, owner_token), MutexRelease::NotOwner);
        assert_eq!(mutex.frame_refs(), 1);

        assert!(matches!(
            mutex.release(owner, owner_token),
            MutexRelease::Released { .. }
        ));
        assert_eq!(mutex.frame_refs(), 0);
        // Still retained: ownership is reserved for the woken waiter.
        assert!(matches!(
            mutex.reclamation_status(),
            ReclamationStatus::Retained { owned: true, .. }
        ));

        assert_eq!(
            mutex.poll_acquire(waiter, waiter_token),
            MutexResume::Acquired
        );
        assert_eq!(mutex.frame_refs(), 1);
        assert!(matches!(
            mutex.release(waiter, waiter_token),
            MutexRelease::Released { handed_to: None }
        ));
        assert_eq!(mutex.frame_refs(), 0);
        assert_eq!(
            mutex.reclamation_status(),
            ReclamationStatus::Reclaimable,
            "only a fully drained state may ever be freed"
        );
    }

    // 28
    #[test]
    fn gc_roots_cover_ref_words_only() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let mut word = 0xABCDu64;
        let pointer = (&raw mut word) as i64;

        // Registry walk. Only cells whose word must be traced may be registered
        // here: the registry is program-lifetime, so registering a word the
        // collector cannot resolve would abort every later collection in the
        // process. A nil ref cell and a scalar cell are the two registrations
        // that are safe to make and must contribute nothing.
        let before = async_mutex_gc_roots().len();
        let empty = willow_async_mutex_new(0, 1);
        let scalar = willow_async_mutex_new(pointer, 0);
        assert_eq!(
            async_mutex_gc_roots().len(),
            before,
            "a nil ref cell and a scalar cell add no roots"
        );

        // Value semantics, on cells that were never registered, so a fabricated
        // pointer is never handed to the collector.
        let scalar = mutex_from_raw(scalar).expect("scalar mutex");
        assert!(!scalar.is_ref());
        assert_eq!(
            scalar.gc_root(),
            None,
            "a scalar word is not traced even when it looks like a pointer"
        );
        assert_eq!(
            mutex_from_raw(empty).expect("empty mutex").gc_root(),
            None,
            "a ref cell holding nil is not a root"
        );
        assert_eq!(
            AsyncMutex::new(pointer, true).gc_root(),
            Some(pointer as *mut u8),
            "a ref-typed protected word is a root the collector must trace"
        );
    }

    // 39, 40
    #[test]
    fn a_real_collection_keeps_a_ref_cell_s_object_alive() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();
        // The registry is program-lifetime (reclamation is willow-38w.1.6), and
        // this test is the only one that puts a real heap pointer in it. Clear
        // it on both sides: stale entries from an earlier test would be traced
        // against this test's fresh heap, and this test's entry would be traced
        // against the next `willow_gc_init()`'s heap. Either aborts a
        // collection in a test that has nothing to do with locks.
        clear_async_mutex_gc_registry_for_test();
        willow_gc_init();

        // The mutex's protected word is the object's ONLY reference: no root
        // slot is pushed for it, and the local is overwritten below. If
        // `async_mutex_gc_roots` were not wired into the collector, this object
        // would be swept.
        let protected = willow_string_from_str("protected by the lock");
        let mutex = willow_async_mutex_new(protected as i64, 1);
        assert_eq!(
            async_mutex_gc_roots(),
            vec![protected],
            "a ref cell holding a live object contributes exactly one root"
        );

        // The control: an identical allocation nobody references at all. It
        // proves this collection really does reclaim, so the survival of
        // `protected` is a fact about the root wiring rather than about a
        // collector that happens to free nothing.
        let unreferenced = willow_string_from_str("referenced by nobody");
        assert_ne!(protected, unreferenced);

        let freed_before = total_frees_for_test();
        willow_gc_collect();
        assert!(
            total_frees_for_test() > freed_before,
            "the control object was unreachable, so this collection must have \
             reclaimed at least one object"
        );

        // The survivor: still exactly one root, still the same address (this
        // path is non-moving), and still readable with its contents intact.
        // Reading it is the real assertion — a swept object's payload is gone.
        assert_eq!(
            async_mutex_gc_roots(),
            vec![protected],
            "the ref cell's word must still be a live root after the collection"
        );
        assert_eq!(
            unsafe { willow_string_as_str(protected) },
            "protected by the lock",
            "an object whose only reference is a ref-typed lock cell must \
             survive a collection with its contents intact"
        );

        // The cell itself is unowned and unqueued throughout: tracing must not
        // depend on anyone holding the lock (the collector reads the word
        // without ownership by design).
        let cell = mutex_from_raw(mutex).expect("mutex");
        assert!(cell.is_ref());
        assert_eq!(cell.owner(), None);
        assert_eq!(cell.gc_root(), Some(protected));

        clear_async_mutex_gc_registry_for_test();
        willow_gc_init();
    }

    // 29
    #[test]
    fn c_abi_routes_the_full_acquire_release_cycle() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let raw = willow_async_mutex_new(3, 0);
        let owner = parked_task();
        let waiter = parked_task();

        let mut owner_token: i64 = -1;
        let mut waiter_token: i64 = -1;

        with_current_task_for_test(owner, || {
            assert_eq!(
                willow_async_mutex_acquire(raw, &raw mut owner_token),
                MUTEX_STATUS_ACQUIRED
            );
            assert!(owner_token >= 0, "the token must be published to the frame");
            assert_eq!(willow_async_mutex_load(raw, owner_token), 3);
            assert_eq!(willow_async_mutex_commit(raw, owner_token, 8), 1);
            // Non-reentrant, and the ABI reports it as its own status so the
            // generated code can raise the language panic.
            assert_eq!(
                willow_async_mutex_acquire(raw, &raw mut owner_token),
                MUTEX_STATUS_RECURSIVE
            );
        });

        with_current_task_for_test(waiter, || {
            assert_eq!(
                willow_async_mutex_acquire(raw, &raw mut waiter_token),
                MUTEX_STATUS_PENDING
            );
            assert_eq!(
                willow_async_mutex_load(raw, waiter_token),
                0,
                "not the owner"
            );
            assert_eq!(
                willow_async_mutex_poll(raw, waiter_token),
                MUTEX_STATUS_PENDING
            );
        });

        with_current_task_for_test(owner, || {
            assert_eq!(willow_async_mutex_release(raw, owner_token), 1);
            assert_eq!(
                willow_async_mutex_release(raw, owner_token),
                0,
                "double release"
            );
        });

        with_current_task_for_test(waiter, || {
            assert_eq!(
                willow_async_mutex_poll(raw, waiter_token),
                MUTEX_STATUS_ACQUIRED
            );
            assert_eq!(willow_async_mutex_load(raw, waiter_token), 8);
            assert_eq!(willow_async_mutex_cancel(), 0, "the link was consumed");
            assert_eq!(willow_async_mutex_release(raw, waiter_token), 1);
        });
    }

    // 30
    #[test]
    fn c_abi_reports_loss_without_a_handle_or_a_running_task() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let raw = willow_async_mutex_new(5, 0);
        let task = parked_task();
        let mut token: i64 = -1;

        // No running task: a lock statement outside a task is an ABI misuse.
        assert_eq!(
            willow_async_mutex_acquire(raw, &raw mut token),
            MUTEX_STATUS_LOST
        );
        assert_eq!(willow_async_mutex_poll(raw, 0), MUTEX_STATUS_LOST);
        assert_eq!(willow_async_mutex_load(raw, 0), 0);
        assert_eq!(willow_async_mutex_commit(raw, 0, 77), 0);
        assert_eq!(willow_async_mutex_release(raw, 0), 0);
        assert_eq!(willow_async_mutex_cancel(), 0);

        with_current_task_for_test(task, || {
            let null = std::ptr::null_mut();
            assert_eq!(
                willow_async_mutex_acquire(null, &raw mut token),
                MUTEX_STATUS_LOST
            );
            assert_eq!(willow_async_mutex_poll(null, 0), MUTEX_STATUS_LOST);
            assert_eq!(willow_async_mutex_commit(null, 0, 77), 0);
            // A rejected acquire leaves the value untouched.
            let owner_token = acquire_now(mutex_from_raw(raw).expect("mutex"), task);
            assert_eq!(willow_async_mutex_load(raw, owner_token as i64), 5);
        });
        assert_eq!(token, -1, "no token is published on a lost acquire");
    }

    // 41
    #[test]
    fn c_abi_reports_cancelled_without_publishing_a_waiter() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let raw = willow_async_mutex_new(5, 0);
        let owner = parked_task();
        let cancelled = parked_task();
        let mut owner_token: i64 = -1;
        let mut token: i64 = -1;
        with_current_task_for_test(owner, || {
            assert_eq!(
                willow_async_mutex_acquire(raw, &raw mut owner_token),
                MUTEX_STATUS_ACQUIRED
            );
        });
        willow_sched_cancel(cancelled);

        with_current_task_for_test(cancelled, || {
            assert_eq!(
                willow_async_mutex_acquire(raw, &raw mut token),
                MUTEX_STATUS_CANCELLED
            );
        });

        assert_eq!(token, -1, "a cancelled acquire must not publish a token");
        assert_eq!(
            mutex_from_raw(raw).expect("mutex").waiter_count(),
            0,
            "a task the scheduler is cancelling must never join the wait queue"
        );
        assert!(lock_wait_link_of(cancelled).is_none());
        with_current_task_for_test(owner, || {
            assert_eq!(willow_async_mutex_release(raw, owner_token), 1);
        });
    }

    // 31
    #[test]
    fn a_cancellation_storm_leaves_the_lock_consistent() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let mutex = AsyncMutex::new(0, false);
        let owner = parked_task();
        let owner_token = acquire_now(&mutex, owner);
        let tasks: Vec<RuntimeTaskId> = (0..24).map(|_| parked_task()).collect();
        let barrier = Arc::new(Barrier::new(tasks.len()));

        std::thread::scope(|scope| {
            for &task in &tasks {
                let barrier = Arc::clone(&barrier);
                let mutex = &mutex;
                scope.spawn(move || {
                    barrier.wait();
                    for _ in 0..40 {
                        match mutex.acquire(task) {
                            MutexAcquire::Pending(_) => {
                                purge_task_lock_wait(task);
                            }
                            MutexAcquire::Acquired(token) => {
                                mutex.release(task, token);
                            }
                            // Reserved for this task by a concurrent release:
                            // hand it on rather than leaving it stranded.
                            MutexAcquire::Recursive => {
                                purge_task_lock_wait(task);
                            }
                            MutexAcquire::Ineligible => break,
                        }
                    }
                });
            }
        });

        // Whatever the interleaving, the original owner still holds the lock:
        // no cancellation may take ownership away from a live owner.
        assert_eq!(mutex.owner(), Some((owner, owner_token)));
        for &task in &tasks {
            purge_task_lock_wait(task);
        }
        assert!(matches!(
            mutex.release(owner, owner_token),
            MutexRelease::Released { .. }
        ));
        for &task in &tasks {
            purge_task_lock_wait(task);
        }
        assert_eq!(mutex.waiter_count(), 0);
        assert_eq!(mutex.reclamation_status(), ReclamationStatus::Reclaimable);
    }

    // 33
    #[test]
    fn a_handoff_to_a_running_task_is_not_lost() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let mutex = AsyncMutex::new(0, false);
        let owner = parked_task();
        let waiter = parked_task();

        let owner_token = acquire_now(&mutex, owner);
        let waiter_token = queue_on(&mutex, waiter);

        // The waiter re-entered a poll before the handoff reached it, so the
        // wake cannot enqueue it. The reservation must still be made: the
        // waiter's own poll boundary is what picks it up.
        make_running(waiter);
        let MutexRelease::Released { handed_to } = mutex.release(owner, owner_token) else {
            panic!("owner should be able to release");
        };
        assert_eq!(handed_to, Some(waiter));
        assert_eq!(mutex.owner(), Some((waiter, waiter_token)));
        assert_eq!(
            mutex.poll_acquire(waiter, waiter_token),
            MutexResume::Acquired,
            "a wake that arrived during a poll must not strand the lock"
        );
        assert!(matches!(
            mutex.release(waiter, waiter_token),
            MutexRelease::Released { handed_to: None }
        ));
    }

    // ── Real-scheduler perspectives (36, 37, 38) ────────────────────────────
    //
    // Everything above drives the state machine directly, which is the right
    // level for FIFO order, token validity, and compensation. It cannot show
    // the property the stage is actually for: that a task which cannot get the
    // lock *parks*, so one worker keeps running other work, and that the
    // handoff is what brings it back. That needs the real run loop, real task
    // records, and real poll returns.
    //
    // Cross-poll state lives in statics rather than in a frame, because
    // `willow_sched_spawn` roots the frame pointer with the collector and these
    // tests have no GC frame to give it. `runtime_test_guard()` serialises
    // runtime tests, so the statics are effectively test-local; each test
    // resets the ones it uses.
    //
    // Task creation is *chained* — each stage spawns the next from inside its
    // own poll — so the interleaving is forced by causality instead of by the
    // run queue's discipline. Nothing here depends on the order the scheduler
    // happens to pop ready tasks in.

    static SCHED_LOCK: AtomicUsize = AtomicUsize::new(0);
    static SCHED_EVENTS: TestMutex<Vec<String>> = TestMutex::new(Vec::new());
    static SCHED_HOLDER_TASK: AtomicU64 = AtomicU64::new(0);
    static SCHED_HOLDER_TOKEN: AtomicI64 = AtomicI64::new(-1);
    static SCHED_HOLDER_STEP: AtomicUsize = AtomicUsize::new(0);
    static SCHED_WAITER_TOKEN: AtomicI64 = AtomicI64::new(-1);
    static SCHED_WAITER_STEP: AtomicUsize = AtomicUsize::new(0);

    fn sched_reset(is_ref: bool) -> *mut c_void {
        reset_global_scheduler_for_test();
        SCHED_EVENTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        SCHED_HOLDER_TASK.store(0, Ordering::SeqCst);
        SCHED_HOLDER_TOKEN.store(-1, Ordering::SeqCst);
        SCHED_HOLDER_STEP.store(0, Ordering::SeqCst);
        SCHED_WAITER_TOKEN.store(-1, Ordering::SeqCst);
        SCHED_WAITER_STEP.store(0, Ordering::SeqCst);
        CHAIN_REMAINING.store(0, Ordering::SeqCst);
        CHAIN_TOKENS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
        let raw = willow_async_mutex_new(0, is_ref as i64);
        SCHED_LOCK.store(raw as usize, Ordering::SeqCst);
        raw
    }

    fn sched_lock() -> *mut c_void {
        SCHED_LOCK.load(Ordering::SeqCst) as *mut c_void
    }

    fn record(event: impl Into<String>) {
        SCHED_EVENTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(event.into());
    }

    fn events() -> Vec<String> {
        SCHED_EVENTS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Holds the lock across a park: poll 1 acquires and returns Pending, poll 2
    /// releases. Whoever wakes it decides when the critical section ends.
    unsafe extern "C" fn poll_hold_across_a_park(_frame: *mut c_void) -> i32 {
        if SCHED_HOLDER_STEP.fetch_add(1, Ordering::SeqCst) == 0 {
            let mut token: i64 = -1;
            assert_eq!(
                willow_async_mutex_acquire(sched_lock(), &raw mut token),
                MUTEX_STATUS_ACQUIRED,
                "the first task to arrive takes the lock uncontended"
            );
            SCHED_HOLDER_TOKEN.store(token, Ordering::SeqCst);
            SCHED_HOLDER_TASK.store(willow_sched_current_task(), Ordering::SeqCst);
            record("holder acquired");
            // Spawned from inside the poll, so the waiter cannot possibly reach
            // the lock before it is held.
            willow_sched_spawn(poll_wait_for_the_lock, std::ptr::null_mut());
            RUNTIME_POLL_PENDING
        } else {
            record("holder releasing");
            assert_eq!(
                willow_async_mutex_release(sched_lock(), SCHED_HOLDER_TOKEN.load(Ordering::SeqCst)),
                1
            );
            RUNTIME_POLL_READY
        }
    }

    /// Parks on the held lock, and resumes only when the handoff wakes it.
    unsafe extern "C" fn poll_wait_for_the_lock(_frame: *mut c_void) -> i32 {
        if SCHED_WAITER_STEP.fetch_add(1, Ordering::SeqCst) == 0 {
            let mut token: i64 = -1;
            assert_eq!(
                willow_async_mutex_acquire(sched_lock(), &raw mut token),
                MUTEX_STATUS_PENDING,
                "the lock is held, so this task must park rather than spin"
            );
            SCHED_WAITER_TOKEN.store(token, Ordering::SeqCst);
            record("waiter parked");
            // Only now does the bystander exist: any work it records provably
            // happened while this task was parked on the lock.
            willow_sched_spawn(poll_unrelated_work, std::ptr::null_mut());
            RUNTIME_POLL_PENDING
        } else {
            let token = SCHED_WAITER_TOKEN.load(Ordering::SeqCst);
            assert_eq!(
                willow_async_mutex_poll(sched_lock(), token),
                MUTEX_STATUS_ACQUIRED,
                "the resume must be the handoff, not a spurious wake"
            );
            record("waiter resumed");
            assert_eq!(willow_async_mutex_release(sched_lock(), token), 1);
            RUNTIME_POLL_READY
        }
    }

    /// Unrelated work that must complete while the waiter is parked, on the one
    /// worker the waiter would otherwise be occupying. Then it ends the
    /// critical section by waking the holder.
    unsafe extern "C" fn poll_unrelated_work(_frame: *mut c_void) -> i32 {
        record("unrelated task ran to completion");
        willow_sched_wake(SCHED_HOLDER_TASK.load(Ordering::SeqCst));
        RUNTIME_POLL_READY
    }

    // 36
    #[test]
    fn one_worker_keeps_working_while_a_task_is_parked_on_the_lock() {
        let _guard = runtime_test_guard();
        let _single = single_worker_for_test();
        sched_reset(false);

        willow_sched_spawn(poll_hold_across_a_park, std::ptr::null_mut());
        let completed = willow_sched_run();

        assert_eq!(
            completed, 3,
            "holder, waiter and bystander must all finish: a parked waiter that \
             never got its handoff would leave the run short"
        );
        assert_eq!(
            events(),
            vec![
                "holder acquired",
                "waiter parked",
                "unrelated task ran to completion",
                "holder releasing",
                "waiter resumed",
            ],
            "the worker must make progress on other work between the park and \
             the handoff, and the waiter must not resume before the release"
        );

        let mutex = mutex_from_raw(sched_lock()).expect("mutex");
        assert_eq!(mutex.owner(), None);
        assert_eq!(mutex.waiter_count(), 0);
        assert_eq!(mutex.reclamation_status(), ReclamationStatus::Reclaimable);
    }

    // ── 37: a chain of real tasks drains in registration order ──────────────

    static CHAIN_REMAINING: AtomicUsize = AtomicUsize::new(0);
    /// `(task id, token, polls so far)` per chained waiter, in the order they
    /// registered on the lock.
    static CHAIN_TOKENS: TestMutex<Vec<(u64, i64, usize)>> = TestMutex::new(Vec::new());

    unsafe extern "C" fn poll_chain_waiter(_frame: *mut c_void) -> i32 {
        let task = willow_sched_current_task();
        let mut chain = CHAIN_TOKENS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let existing = chain.iter().position(|&(id, _, _)| id == task);

        match existing {
            None => {
                let mut token: i64 = -1;
                assert_eq!(
                    willow_async_mutex_acquire(sched_lock(), &raw mut token),
                    MUTEX_STATUS_PENDING,
                    "the holder still owns the lock, so every chained task parks"
                );
                chain.push((task, token, 1));
                record(format!("registered {}", chain.len()));
                drop(chain);
                // Chain the next arrival from inside this poll so registration
                // order is fixed by causality, not by the run queue.
                if CHAIN_REMAINING.fetch_sub(1, Ordering::SeqCst) > 1 {
                    willow_sched_spawn(poll_chain_waiter, std::ptr::null_mut());
                } else {
                    willow_sched_spawn(poll_unrelated_work, std::ptr::null_mut());
                }
                RUNTIME_POLL_PENDING
            }
            Some(index) => {
                let (_, token, polls) = chain[index];
                chain[index].2 = polls + 1;
                drop(chain);
                assert_eq!(
                    willow_async_mutex_poll(sched_lock(), token),
                    MUTEX_STATUS_ACQUIRED,
                    "waiter {index} was woken without owning the lock"
                );
                record(format!("acquired {}", index + 1));
                assert_eq!(willow_async_mutex_release(sched_lock(), token), 1);
                RUNTIME_POLL_READY
            }
        }
    }

    unsafe extern "C" fn poll_hold_then_start_a_chain(_frame: *mut c_void) -> i32 {
        if SCHED_HOLDER_STEP.fetch_add(1, Ordering::SeqCst) == 0 {
            let mut token: i64 = -1;
            assert_eq!(
                willow_async_mutex_acquire(sched_lock(), &raw mut token),
                MUTEX_STATUS_ACQUIRED
            );
            SCHED_HOLDER_TOKEN.store(token, Ordering::SeqCst);
            SCHED_HOLDER_TASK.store(willow_sched_current_task(), Ordering::SeqCst);
            willow_sched_spawn(poll_chain_waiter, std::ptr::null_mut());
            RUNTIME_POLL_PENDING
        } else {
            assert_eq!(
                willow_async_mutex_release(sched_lock(), SCHED_HOLDER_TOKEN.load(Ordering::SeqCst)),
                1
            );
            RUNTIME_POLL_READY
        }
    }

    // 37
    #[test]
    fn one_worker_drains_a_chain_of_real_tasks_in_registration_order() {
        let _guard = runtime_test_guard();
        let _single = single_worker_for_test();
        sched_reset(false);

        const WAITERS: usize = 16;
        CHAIN_REMAINING.store(WAITERS, Ordering::SeqCst);
        willow_sched_spawn(poll_hold_then_start_a_chain, std::ptr::null_mut());

        let completed = willow_sched_run();
        assert_eq!(
            completed,
            (WAITERS + 2) as i64,
            "holder, {WAITERS} waiters and the waker must all finish; a short \
             count means a task is still parked on a lock nobody handed on"
        );

        // Compare the two orders the log records rather than either against a
        // hardcoded sequence: whatever order the tasks happened to register in,
        // the lock must be granted in exactly that order.
        let log = events();
        let registration_order: Vec<&str> = log
            .iter()
            .filter_map(|event| event.strip_prefix("registered "))
            .collect();
        let acquisition_order: Vec<&str> = log
            .iter()
            .filter_map(|event| event.strip_prefix("acquired "))
            .collect();
        assert_eq!(registration_order.len(), WAITERS);
        assert_eq!(
            registration_order, acquisition_order,
            "the real scheduler must grant the lock in registration order"
        );

        // Every waiter was polled exactly twice: once to park, once to take the
        // handoff. A third poll would mean a spurious wake reached it.
        let chain = CHAIN_TOKENS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(chain.len(), WAITERS);
        for (index, &(_, _, polls)) in chain.iter().enumerate() {
            assert_eq!(polls, 2, "waiter {index} was polled {polls} times");
        }
        drop(chain);

        let mutex = mutex_from_raw(sched_lock()).expect("mutex");
        assert_eq!(mutex.reclamation_status(), ReclamationStatus::Reclaimable);
    }

    // ── 38: a cancelled waiter is retired without stranding the lock ────────

    static CANCEL_VICTIM: AtomicU64 = AtomicU64::new(0);

    /// Parks on the lock and requests its own cancellation without cleaning up
    /// its registration. Because this synthetic task has no compiler-generated
    /// cancel entry or frame, the scheduler finalizes it at the next claim
    /// without polling it again; terminal cleanup must remove the lock wait.
    unsafe extern "C" fn poll_park_then_retire_if_cancelled(_frame: *mut c_void) -> i32 {
        let task = willow_sched_current_task();
        if CANCEL_VICTIM.load(Ordering::SeqCst) != task {
            CANCEL_VICTIM.store(task, Ordering::SeqCst);
            let mut token: i64 = -1;
            assert_eq!(
                willow_async_mutex_acquire(sched_lock(), &raw mut token),
                MUTEX_STATUS_PENDING
            );
            record("victim parked");
            willow_sched_spawn(poll_wait_for_the_lock, std::ptr::null_mut());
            willow_sched_cancel(task);
            return RUNTIME_POLL_PENDING;
        }
        panic!("a cancelled task without a cleanup entry must not be polled again")
    }

    unsafe extern "C" fn poll_hold_then_start_a_cancel(_frame: *mut c_void) -> i32 {
        if SCHED_HOLDER_STEP.fetch_add(1, Ordering::SeqCst) == 0 {
            let mut token: i64 = -1;
            assert_eq!(
                willow_async_mutex_acquire(sched_lock(), &raw mut token),
                MUTEX_STATUS_ACQUIRED
            );
            SCHED_HOLDER_TOKEN.store(token, Ordering::SeqCst);
            SCHED_HOLDER_TASK.store(willow_sched_current_task(), Ordering::SeqCst);
            willow_sched_spawn(poll_park_then_retire_if_cancelled, std::ptr::null_mut());
            RUNTIME_POLL_PENDING
        } else {
            record("holder releasing");
            assert_eq!(
                willow_async_mutex_release(sched_lock(), SCHED_HOLDER_TOKEN.load(Ordering::SeqCst)),
                1
            );
            RUNTIME_POLL_READY
        }
    }

    // 38
    #[test]
    fn a_cancelled_waiter_is_retired_without_stranding_the_lock() {
        let _guard = runtime_test_guard();
        let _single = single_worker_for_test();
        sched_reset(false);
        CANCEL_VICTIM.store(0, Ordering::SeqCst);

        willow_sched_spawn(poll_hold_then_start_a_cancel, std::ptr::null_mut());
        let completed = willow_sched_run();

        let log = events();
        assert_eq!(
            completed, 3,
            "the holder, waiter behind the victim, and waker must finish; the \
             cancelled victim is finalized but is not counted as completed: {log:?}"
        );
        assert!(
            !log.contains(&"victim retired".to_string()),
            "a cancelled task without a cleanup entry must not be polled again: {log:?}"
        );
        assert!(
            log.contains(&"waiter resumed".to_string()),
            "the waiter queued behind the cancelled one must still get the \
             lock — a stranded registration would park it forever: {log:?}"
        );

        let mutex = mutex_from_raw(sched_lock()).expect("mutex");
        let victim = CANCEL_VICTIM.load(Ordering::SeqCst);
        assert_ne!(victim, 0);
        assert_eq!(
            crate::scheduler::willow_sched_task_state(victim),
            -1,
            "the cancelled victim must have been finalized and reaped"
        );
        assert_eq!(
            purge_task_lock_wait(victim),
            MutexCancel::NoLink,
            "terminal cleanup must have removed the victim's reverse link"
        );
        assert_eq!(mutex.owner(), None);
        assert_eq!(mutex.waiter_count(), 0);
        assert_eq!(mutex.reclamation_status(), ReclamationStatus::Reclaimable);
    }

    // 32
    #[test]
    fn ownership_is_task_based_and_survives_a_worker_change() {
        let _guard = runtime_test_guard();
        let _single = single_worker_for_test();
        reset_global_scheduler_for_test();

        let mutex = AsyncMutex::new(0, false);
        let owner = ready_task();
        let waiter = parked_task();

        let owner_token = acquire_now(&mutex, owner);
        let waiter_token = queue_on(&mutex, waiter);

        // Acquired on this thread, released from another: a native-thread
        // notion of ownership would reject this, but a Willow task can be
        // preempted mid-critical-section and resumed on a different worker.
        let handed_to = std::thread::scope(|scope| {
            let mutex = &mutex;
            scope
                .spawn(move || match mutex.release(owner, owner_token) {
                    MutexRelease::Released { handed_to } => handed_to,
                    MutexRelease::NotOwner => panic!("ownership must not be thread-local"),
                })
                .join()
                .unwrap()
        });
        assert_eq!(handed_to, Some(waiter));

        // A single worker still drains the chain to completion.
        assert_eq!(
            mutex.poll_acquire(waiter, waiter_token),
            MutexResume::Acquired
        );
        assert!(matches!(
            mutex.release(waiter, waiter_token),
            MutexRelease::Released { handed_to: None }
        ));
        assert_eq!(mutex.reclamation_status(), ReclamationStatus::Reclaimable);
    }
}
