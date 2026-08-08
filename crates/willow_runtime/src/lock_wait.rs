//! Scheduler-aware lock waiter protocol (willow-38w.1.2, spec §8.1–§8.4, §12).
//!
//! This is the shared plumbing under the future `Mutex` and `RwLock` runtimes:
//! who is queued on a lock, how a waiter is identified across cancel/retry, and
//! in which order the two locks involved (the lock's own state, and the task
//! table shard holding the waiting task) may be taken. No public lock API uses
//! it yet — the state machine lands in willow-38w.1.3 and the lowering in
//! willow-38w.1.4/.5 — so nothing here changes observable program behavior.
//!
//! Three rules carry the design:
//!
//! 1. **Identity is `(LockId, RegistrationToken)`, never an address.** A lock
//!    gets a process-wide monotonic [`LockId`] that survives GC relocation of
//!    its handle; each registration gets a token from that lock's own counter.
//!    A cancelled-then-re-registered task is therefore distinguishable from its
//!    own stale ghost, and an operation carrying an old token is a no-op instead
//!    of a corruption. Neither counter ever wraps or reuses a value; exhaustion
//!    is a fatal abort, not a silent reissue.
//!
//! 2. **One lock order: `LockState -> TaskShard`.** Registration and handoff
//!    hold the lock's state while they touch the task's reverse link, so a queue
//!    entry and its matching link are published in one critical section — no
//!    two-phase `PREPARING` handshake, and no window where a waiter is visible
//!    in the queue without its link. Cancellation runs the other way round and
//!    therefore never holds both: it *takes* the link under the shard, releases
//!    the shard, and only then takes the lock state to reconcile.
//!
//! 3. **Validate before every dereference.** [`LockWaitLink`] holds a raw
//!    `NonNull<AsyncLockState>` so cancellation reaches the lock in O(1) with no
//!    global registry to scan or race against, and every dereference first
//!    checks `state.lock_id == link.lock_id`. A mismatch means the pointer
//!    outlived its state (ABA / lifetime-invariant violation) and aborts rather
//!    than corrupting an unrelated lock.

use crate::scheduler::{
    install_lock_wait_link, promote_lock_wait_link, revoke_terminal_lock_handoff,
    take_lock_wait_link, wake_lock_waiter,
};
use crate::task::RuntimeTaskId;
use crate::task_state::WakeOutcome;
use crate::wait_queue::WaitQueue;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::Mutex as StdMutex;
use std::sync::MutexGuard;
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-wide identity of one native lock state. Stable across GC relocation
/// of the owning handle, never reused within a process.
pub type LockId = u64;

/// Per-lock generation of one waiter registration. Unique only within its
/// `LockId`, which is all `(LockId, token)` identity needs — a global counter
/// would put every independent lock on one contended cache line for nothing.
pub type RegistrationToken = u64;

/// `0` is never issued, so a zeroed frame slot or default-initialized field is
/// never mistaken for a real lock.
static NEXT_LOCK_ID: AtomicU64 = AtomicU64::new(1);

/// Unrecoverable protocol violation. Not a Willow panic: there is no program
/// state left worth unwinding, and continuing would touch a lock state that may
/// already be freed.
fn fatal(message: &str) -> ! {
    eprintln!("runtime fatal: {message}");
    std::process::abort();
}

/// Issue the next id from `counter`, or `None` once the space is exhausted.
///
/// Split out from [`next_lock_id`] so the exhaustion edge is testable: the real
/// counter is a process-global that cannot be wound forward, and the production
/// wrapper aborts.
fn try_allocate_lock_id(counter: &AtomicU64) -> Option<LockId> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .ok()
}

/// The next process-wide [`LockId`]. Fatal on exhaustion — reusing an id would
/// make a stale [`LockWaitLink`] validate against an unrelated lock, which is
/// exactly the failure the id exists to prevent.
pub fn next_lock_id() -> LockId {
    try_allocate_lock_id(&NEXT_LOCK_ID)
        .unwrap_or_else(|| fatal("LockId space exhausted (u64::MAX reached)"))
}

/// Where a registered task stands with respect to the lock it is waiting on.
///
/// The `ValueLoaded`/`Held` phases from the spec's acquire state machine live in
/// the generated async frame, not here: once the handoff is consumed the task
/// has no reverse link left to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockWaitPhase {
    /// Queued, not owning. The protected value must not be loaded.
    Waiting,
    /// Ownership is reserved for this task but the frame has not loaded the
    /// protected value yet. A cancellation here must hand the lock on, or the
    /// lock is stranded with no owner that will ever release it.
    HandoffOwned,
}

/// The task-side half of a registration: enough to reach the lock in O(1) and
/// to prove, at that lock, that this registration is still the current one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockWaitLink {
    pub lock_id: LockId,
    /// Stable address of the boxed [`AsyncLockState`] — NOT a GC handle pointer.
    /// The state never moves after allocation; the handle it hangs off may.
    pub state: NonNull<AsyncLockState>,
    pub token: RegistrationToken,
    pub phase: LockWaitPhase,
}

// SAFETY: the referent is an `AsyncLockState`, which is internally synchronized
// (`StdMutex`) and immobile for as long as any link to it is live (§8.2.2). The
// link is moved between threads only inside a task shard's lock.
unsafe impl Send for LockWaitLink {}

/// Whether `link` still points at the state it was created for.
///
/// # Safety
/// `link.state` must be a pointer the caller believes is still allocated. This
/// is the check, not a substitute for the lifetime invariant: it turns a
/// same-address-different-lock reuse into an observable `false` instead of a
/// silent success.
pub unsafe fn link_state_matches(link: &LockWaitLink) -> bool {
    unsafe { link.state.as_ref() }.lock_id == link.lock_id
}

/// Resolve a link to its state, aborting if the identity check fails (§8.2.1).
///
/// # Safety
/// Same contract as [`link_state_matches`].
unsafe fn state_of(link: &LockWaitLink) -> &AsyncLockState {
    let state = unsafe { link.state.as_ref() };
    if state.lock_id != link.lock_id {
        fatal("lock wait link points at a state with a different LockId");
    }
    state
}

/// Native state of one scheduler-aware lock. Boxed and never moved: task-side
/// links hold its address (§8.2.2).
#[derive(Debug)]
pub struct AsyncLockState {
    lock_id: LockId,
    /// Short critical sections only. Per §8.1 this native mutex is never held
    /// across a user critical section, a task park, a scheduler wake, or a GC
    /// safepoint wait.
    inner: StdMutex<LockStateInner>,
}

#[derive(Debug)]
struct LockStateInner {
    /// Next token this lock will issue. Per-lock, so registration allocates it
    /// under a lock the caller already holds.
    next_token: RegistrationToken,
    /// FIFO admission order. `WaitQueue` gives O(1) membership and removal with
    /// tombstone compaction, so a cancellation does not scan the queue
    /// (willow-ezs.2).
    waiters: WaitQueue<RuntimeTaskId>,
    /// The token each queued task registered with. Split from `waiters` so the
    /// queue keeps its O(1) removal; a task waits on at most one lock at a time,
    /// so one entry per task is exact.
    tokens: HashMap<RuntimeTaskId, RegistrationToken>,
    /// Reserved or active exclusive ownership.
    owner: Option<(RuntimeTaskId, RegistrationToken)>,
}

impl Default for LockStateInner {
    fn default() -> Self {
        Self {
            // `0` is never a live token, so a zeroed frame slot cannot pass an
            // identity check.
            next_token: 1,
            waiters: WaitQueue::default(),
            tokens: HashMap::new(),
            owner: None,
        }
    }
}

/// Issue the next token, or `None` at exhaustion. Callers hold the state lock.
fn try_fresh_token(inner: &mut LockStateInner) -> Option<RegistrationToken> {
    let token = inner.next_token;
    inner.next_token = token.checked_add(1)?;
    Some(token)
}

fn fresh_token(inner: &mut LockStateInner) -> RegistrationToken {
    try_fresh_token(inner)
        .unwrap_or_else(|| fatal("RegistrationToken space exhausted for this lock"))
}

/// What a cancellation actually reconciled, so the caller (and the Stage 3 state
/// machine) knows whether the lock now needs handing on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockCancelOutcome {
    /// The task was not registered on any lock.
    NoLink,
    /// A queued `Waiting` registration was removed. The lock is untouched.
    RemovedWaiter {
        lock_id: LockId,
        token: RegistrationToken,
    },
    /// Reserved ownership was released. The caller must hand the lock to the
    /// next waiter, or it is stranded (§12.3).
    ReleasedOwnership {
        lock_id: LockId,
        token: RegistrationToken,
    },
    /// The link was real but the lock had already moved past it — a concurrent
    /// handoff consumed the registration, or this is an older generation. A
    /// no-op by design (§8.3.1).
    Stale {
        lock_id: LockId,
        token: RegistrationToken,
        phase: LockWaitPhase,
    },
}

impl AsyncLockState {
    /// A fresh lock state with a new process-wide id. Boxed because task-side
    /// links depend on the address staying put.
    pub fn new() -> Box<Self> {
        Box::new(Self {
            lock_id: next_lock_id(),
            inner: StdMutex::new(LockStateInner::default()),
        })
    }

    pub fn lock_id(&self) -> LockId {
        self.lock_id
    }

    /// The address links refer to. Valid only while this state is alive, which
    /// the lifetime invariant (§8.2.2) is responsible for.
    pub fn stable_ptr(&self) -> NonNull<AsyncLockState> {
        NonNull::from(self)
    }

    fn inner(&self) -> MutexGuard<'_, LockStateInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Take the lock without contention: no owner and nobody queued ahead
    /// (§8.5). Returns the ownership token, or `None` if the caller must queue.
    ///
    /// No reverse link is installed — an uncontended acquire never parks, so its
    /// phase lives only in the frame and there is nothing for a cancellation to
    /// find in the task table.
    pub fn try_acquire_uncontended(&self, task: RuntimeTaskId) -> Option<RegistrationToken> {
        let mut inner = self.inner();
        if inner.owner.is_some() || !inner.waiters.is_empty() {
            return None;
        }
        let token = fresh_token(&mut inner);
        inner.owner = Some((task, token));
        Some(token)
    }

    /// Queue `task` and publish its reverse link in one critical section
    /// (§8.3). Returns the registration token, or `None` if the task is not
    /// eligible (terminal, cancel-requested, gone, or already registered on a
    /// lock).
    ///
    /// The token is allocated *before* the shard is taken but *under* the state
    /// lock, exactly as the spec's algorithm has it; a token burned by a
    /// rejected registration is simply never issued again.
    pub fn register_waiter(&self, task: RuntimeTaskId) -> Option<RegistrationToken> {
        // LockState (first lock).
        let mut inner = self.inner();
        let token = fresh_token(&mut inner);
        let link = LockWaitLink {
            lock_id: self.lock_id,
            state: self.stable_ptr(),
            token,
            phase: LockWaitPhase::Waiting,
        };
        // TaskShard (second lock). The callback publishes the lock-side entry
        // before that shard is released, so queue + reverse link become visible
        // as one transaction under `LockState -> TaskShard`.
        if !install_lock_wait_link(task, link, || {
            if !inner.waiters.register(task) {
                return false;
            }
            assert!(
                inner.tokens.insert(task, token).is_none(),
                "a newly queued task already had a lock registration token"
            );
            true
        }) {
            return None;
        }
        Some(token)
    }

    /// Reserve ownership for the next valid waiter, transitioning its link
    /// `Waiting -> HandoffOwned` in the same critical section (§8.3.2).
    ///
    /// Candidates whose link is missing or whose token no longer matches were
    /// cancelled underneath us and are skipped, not retried. Returns `None` if
    /// the lock is already owned or nobody valid is queued.
    pub fn handoff_to_next_waiter(&self) -> Option<(RuntimeTaskId, RegistrationToken)> {
        // LockState (first lock), held across the whole scan.
        let mut inner = self.inner();
        if inner.owner.is_some() {
            return None;
        }
        while let Some(task) = inner.waiters.pop_front() {
            let Some(token) = inner.tokens.remove(&task) else {
                // Queue entry with no recorded token: already reconciled.
                continue;
            };
            // TaskShard (second lock). Ownership reservation happens in the
            // callback before the shard is released, atomically with the link
            // phase transition.
            if promote_lock_wait_link(task, self.lock_id, token, || {
                inner.owner = Some((task, token));
            }) {
                return Some((task, token));
            }
        }
        None
    }

    /// Hand off and then wake the new owner.
    ///
    /// The state lock is released before the wake: §8.1 forbids holding it
    /// across a scheduler call, and the wake path takes task-shard locks of its
    /// own. The wake itself is the existing [`crate::task_state::AtomicTaskState`]
    /// contract — a task woken while still `Running` records the wake and
    /// requeues at `park_after_poll`, so the handoff is never lost to a
    /// wake-before-park race (§12.5).
    pub fn handoff_to_next_waiter_and_wake(&self) -> Option<(RuntimeTaskId, RegistrationToken)> {
        self.handoff_to_next_waiter_with_wake(wake_lock_waiter)
    }

    fn handoff_to_next_waiter_with_wake(
        &self,
        mut wake: impl FnMut(RuntimeTaskId) -> WakeOutcome,
    ) -> Option<(RuntimeTaskId, RegistrationToken)> {
        loop {
            let accepted = self.handoff_to_next_waiter()?;
            if wake(accepted.0) != WakeOutcome::Terminal {
                return Some(accepted);
            }

            // The task became terminal after its link was promoted but before
            // the wake. It cannot consume reserved ownership. Clear the exact
            // handoff and continue scanning so the next live waiter progresses.
            self.revoke_terminal_handoff(accepted.0, accepted.1);
        }
    }

    fn revoke_terminal_handoff(&self, task: RuntimeTaskId, token: RegistrationToken) -> bool {
        let mut inner = self.inner();
        if inner.owner != Some((task, token)) {
            return false;
        }
        revoke_terminal_lock_handoff(task, self.lock_id, token, || {
            // Still under LockState. If cancellation won just before us it may
            // already have cleared this exact owner; never disturb a successor.
            if inner.owner == Some((task, token)) {
                inner.owner = None;
            }
        });
        true
    }

    /// Release ownership held by exactly `(task, token)`. A stale token is a
    /// no-op, so a late release from a previous generation cannot steal the lock
    /// from its current owner.
    pub fn release_owner(&self, task: RuntimeTaskId, token: RegistrationToken) -> bool {
        let mut inner = self.inner();
        if inner.owner == Some((task, token)) {
            inner.owner = None;
            return true;
        }
        false
    }

    pub fn owner(&self) -> Option<(RuntimeTaskId, RegistrationToken)> {
        self.inner().owner
    }

    /// Queued tasks in admission order, tombstones excluded.
    pub fn queued_waiters(&self) -> Vec<RuntimeTaskId> {
        self.inner().waiters.live()
    }

    pub fn waiter_count(&self) -> usize {
        self.inner().waiters.len()
    }

    /// The token `task` is queued with, if any.
    pub fn waiter_token(&self, task: RuntimeTaskId) -> Option<RegistrationToken> {
        self.inner().tokens.get(&task).copied()
    }

    /// Whether no relationship refers to this state any more, so the reclamation
    /// invariant (§15.1) would allow freeing it. Scaffolding for Stage 3: a
    /// state that is not reclaimable must be kept alive by the handle that owns
    /// it, and a link into it must stay dereferenceable.
    pub fn is_reclaimable(&self) -> bool {
        let inner = self.inner();
        inner.owner.is_none() && inner.waiters.is_empty() && inner.tokens.is_empty()
    }
}

/// The task's current registration, for cleanup paths and diagnostics.
pub fn lock_wait_link_of(task: RuntimeTaskId) -> Option<LockWaitLink> {
    crate::scheduler::lock_wait_link(task)
}

/// Cancel `task`'s lock registration, whatever phase it is in (§8.3.1, §12.2,
/// §12.3).
///
/// Takes the link under the task shard, releases the shard, validates the
/// pointer, and only then takes the lock state — never both at once, which is
/// what keeps this path from deadlocking against registration and handoff,
/// which run in the opposite order.
///
/// The caller must keep the frame's GC root on the lock handle alive across this
/// call: the state pointer is dereferenced here, so releasing the root first
/// would allow the state to be reclaimed underneath the reconciliation (§12.6).
pub fn cancel_lock_wait(task: RuntimeTaskId) -> LockCancelOutcome {
    // TaskShard, released before the state lock is taken.
    let Some(link) = take_lock_wait_link(task) else {
        return LockCancelOutcome::NoLink;
    };
    reconcile_cancelled_lock_wait(task, link)
}

/// Reconcile a link already taken from a task shard. Split from
/// [`cancel_lock_wait`] so a delayed cleanup carrying an older generation can
/// be tested directly without overwriting the task's current reverse link.
fn reconcile_cancelled_lock_wait(task: RuntimeTaskId, link: LockWaitLink) -> LockCancelOutcome {
    // SAFETY: the link was taken from the task table, and the caller contract
    // keeps the frame's lock-handle root live until this reconciliation ends;
    // `state_of` also checks the id before use.
    let state = unsafe { state_of(&link) };
    // LockState, taken only now.
    let mut inner = state.inner();
    match link.phase {
        LockWaitPhase::Waiting => {
            if inner.tokens.get(&task) == Some(&link.token) {
                inner.tokens.remove(&task);
                inner.waiters.remove(task);
                LockCancelOutcome::RemovedWaiter {
                    lock_id: link.lock_id,
                    token: link.token,
                }
            } else {
                LockCancelOutcome::Stale {
                    lock_id: link.lock_id,
                    token: link.token,
                    phase: link.phase,
                }
            }
        }
        LockWaitPhase::HandoffOwned => {
            if inner.owner == Some((task, link.token)) {
                inner.owner = None;
                LockCancelOutcome::ReleasedOwnership {
                    lock_id: link.lock_id,
                    token: link.token,
                }
            } else {
                LockCancelOutcome::Stale {
                    lock_id: link.lock_id,
                    token: link.token,
                    phase: link.phase,
                }
            }
        }
    }
}

/// Consume a reserved handoff: the resumed frame is about to load the protected
/// value, so the reverse link is dropped and ownership stays with the task
/// (`HandoffOwned -> ValueLoaded`).
///
/// LockState is taken before TaskShard so ownership and the reverse link are
/// checked as one transition. Returns `false` for any link that is not exactly
/// this generation in `HandoffOwned`, or whose reserved owner no longer
/// matches, leaving it untouched.
pub fn consume_handoff(task: RuntimeTaskId, lock_id: LockId, token: RegistrationToken) -> bool {
    let Some(link) = lock_wait_link_of(task) else {
        return false;
    };
    if link.lock_id != lock_id || link.token != token || link.phase != LockWaitPhase::HandoffOwned {
        return false;
    }
    // SAFETY: the frame root/lifetime contract remains in force until the link
    // is consumed below; `state_of` validates the stable pointer's LockId.
    let state = unsafe { state_of(&link) };
    let inner = state.inner();
    if inner.owner != Some((task, token)) {
        return false;
    }
    crate::scheduler::consume_lock_handoff_link(task, lock_id, token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::runtime_test_guard;
    use crate::scheduler::{
        reset_global_scheduler_for_test, willow_sched_cancel, with_global_for_test,
    };
    use crate::task_state::{BoundaryOutcome, ClaimOutcome, TaskLifecycle, WakeOutcome};
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicU64;

    // Test perspectives for the Stage 2 waiter protocol (willow-38w.1.2). Each
    // maps to at least one test below.
    //
    //  1. LockId is monotonic, never 0, and never repeats.
    //  2. LockId allocation reports exhaustion instead of wrapping.
    //  3. Per-lock tokens are monotonic and independent across locks.
    //  4. Token allocation reports exhaustion instead of wrapping.
    //  5. Identity is the (LockId, token) pair: equal tokens on different locks
    //     are different waiters.
    //  6. A link validates against its own state and rejects a foreign one.
    //  7. Registration publishes the queue entry and the reverse link together.
    //  8. Registration is refused for a terminal task, and publishes nothing.
    //  9. Registration is refused for a cancel-requested task.
    // 10. Registration is refused for an unknown task id.
    // 11. A task already registered on a lock cannot register again.
    // 12. FIFO admission order is preserved across several waiters.
    // 13. Handoff transitions exactly one waiter Waiting -> HandoffOwned and
    //     reserves ownership.
    // 14. Handoff refuses while the lock is already owned.
    // 15. Handoff skips a cancelled candidate and admits the next valid one.
    // 16. Handoff skips a candidate whose task record is gone (terminal).
    // 17. Handoff on an empty queue is None and leaves the lock unowned.
    // 18. Cancelling a Waiting registration removes queue entry and link.
    // 19. Cancelling a HandoffOwned registration releases reserved ownership so
    //     the lock is not stranded.
    // 20. Cancel then handoff: the cancelled waiter is never admitted.
    // 21. Handoff then cancel: cancellation observes HandoffOwned and hands the
    //     lock back.
    // 22. Cancelling a task with no registration is a no-op.
    // 23. A stale (old-generation) cancellation does not remove a newer
    //     registration.
    // 24. A stale release_owner does not steal the lock from the live owner.
    // 25. Re-registration after cancellation gets a fresh, higher token.
    // 26. consume_handoff drops the link and keeps ownership.
    // 27. consume_handoff rejects a wrong token, a wrong lock id, and a Waiting
    //     link.
    // 28. Uncontended acquire takes ownership with no reverse link.
    // 29. Uncontended acquire refuses when a waiter is queued (no barging).
    // 30. Handoff wakes the new owner through the scheduler's wake path.
    // 31. Wake-before-park: a handoff to a Running task defers to its poll
    //     boundary and requeues instead of stranding it parked.
    // 32. Concurrent register/cancel never leaves a queue entry without a link
    //     or a link without a queue entry.
    // 33. Concurrent cancel/handoff agree on exactly one outcome per waiter.
    // 34. The fixed LockState -> TaskShard order does not deadlock under
    //     concurrent register/handoff/cancel from several threads.
    // 35. A state with live relationships is not reclaimable; it becomes
    //     reclaimable only after cleanup completes (GC-root lifetime).
    // 36. The wait-link allocation is released with the last relationship, so a
    //     lock wait does not permanently grow a task's footprint.
    // 37. A task that becomes terminal after handoff promotion has its reserved
    //     ownership revoked and the next live waiter progresses.
    // 38. Handoff consumption requires both the exact reverse link and the
    //     matching lock-side owner reservation.

    /// A parked task, as a contended acquisition would leave it.
    fn parked_task() -> RuntimeTaskId {
        with_global_for_test(|sched| sched.spawn_parked_placeholder())
    }

    /// A ready task record.
    fn ready_task() -> RuntimeTaskId {
        with_global_for_test(|sched| sched.spawn_placeholder())
    }

    fn link_of(task: RuntimeTaskId) -> Option<LockWaitLink> {
        lock_wait_link_of(task)
    }

    /// Drive a parked task to `Terminal` the only way the state word allows:
    /// wake it, claim the frame, finish it.
    fn make_terminal(task: RuntimeTaskId) {
        with_global_for_test(|sched| {
            sched.with_task_for_test(task, |task| {
                assert_ne!(task.state.wake(), WakeOutcome::Terminal);
                assert_eq!(task.state.claim_for_poll(), ClaimOutcome::Poll);
                assert!(task.state.finish_terminal());
            })
        });
    }

    /// Claim a parked task for a poll, so a wake has to be recorded instead of
    /// enqueued.
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

    // 1, 3
    #[test]
    fn lock_ids_are_monotonic_and_tokens_are_per_lock() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let first = AsyncLockState::new();
        let second = AsyncLockState::new();
        assert_ne!(first.lock_id(), 0);
        assert!(second.lock_id() > first.lock_id());

        let a = parked_task();
        let b = parked_task();
        let first_token = first.register_waiter(a).expect("registered");
        let second_token = second.register_waiter(b).expect("registered");
        // Independent counters: both locks start at the same value.
        assert_eq!(first_token, second_token);

        let c = parked_task();
        let next = first.register_waiter(c).expect("registered");
        assert!(next > first_token);
    }

    // 2
    #[test]
    fn lock_id_allocation_reports_exhaustion() {
        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(try_allocate_lock_id(&counter), Some(u64::MAX - 1));
        assert_eq!(try_allocate_lock_id(&counter), None);
        // Still exhausted: it does not wrap back to a reusable id.
        assert_eq!(try_allocate_lock_id(&counter), None);
    }

    // 4
    #[test]
    fn token_allocation_reports_exhaustion() {
        let mut inner = LockStateInner {
            next_token: u64::MAX - 1,
            ..LockStateInner::default()
        };
        assert_eq!(try_fresh_token(&mut inner), Some(u64::MAX - 1));
        assert_eq!(try_fresh_token(&mut inner), None);
        assert_eq!(try_fresh_token(&mut inner), None);
    }

    // 5, 6
    #[test]
    fn identity_is_the_lock_id_and_token_pair() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let first = AsyncLockState::new();
        let second = AsyncLockState::new();
        let a = parked_task();
        let b = parked_task();
        let first_token = first.register_waiter(a).expect("registered");
        let second_token = second.register_waiter(b).expect("registered");
        assert_eq!(first_token, second_token);

        let mut link = link_of(a).expect("link");
        assert_eq!(link.lock_id, first.lock_id());
        // SAFETY: both states are alive for the whole test.
        assert!(unsafe { link_state_matches(&link) });

        // The same token pointed at a different lock is a different waiter, and
        // the identity check catches it.
        link.state = second.stable_ptr();
        assert!(!unsafe { link_state_matches(&link) });
    }

    // 7, 12
    #[test]
    fn registration_publishes_queue_entry_and_link_in_fifo_order() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let tasks: Vec<RuntimeTaskId> = (0..4).map(|_| parked_task()).collect();
        let mut tokens = Vec::new();
        for &task in &tasks {
            tokens.push(state.register_waiter(task).expect("registered"));
        }

        assert_eq!(state.queued_waiters(), tasks);
        for (&task, &token) in tasks.iter().zip(tokens.iter()) {
            let link = link_of(task).expect("link");
            assert_eq!(link.lock_id, state.lock_id());
            assert_eq!(link.token, token);
            assert_eq!(link.phase, LockWaitPhase::Waiting);
            assert_eq!(state.waiter_token(task), Some(token));
        }
        assert!(tokens.windows(2).all(|pair| pair[0] < pair[1]));
    }

    // 8
    #[test]
    fn a_terminal_task_is_not_registered() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let task = parked_task();
        make_terminal(task);

        assert_eq!(state.register_waiter(task), None);
        assert!(state.queued_waiters().is_empty());
        assert_eq!(link_of(task), None);
        assert!(state.is_reclaimable());
    }

    // 9
    #[test]
    fn a_cancel_requested_task_is_not_registered() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let task = parked_task();
        willow_sched_cancel(task);

        assert_eq!(state.register_waiter(task), None);
        assert!(state.queued_waiters().is_empty());
        assert_eq!(link_of(task), None);
    }

    // 10
    #[test]
    fn an_unknown_task_is_not_registered() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        assert_eq!(state.register_waiter(9_999), None);
        assert!(state.queued_waiters().is_empty());
    }

    // 11
    #[test]
    fn a_task_cannot_register_on_two_locks_at_once() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let first = AsyncLockState::new();
        let second = AsyncLockState::new();
        let task = parked_task();

        let token = first.register_waiter(task).expect("registered");
        assert_eq!(second.register_waiter(task), None);

        // The first registration is untouched.
        assert_eq!(first.queued_waiters(), vec![task]);
        assert_eq!(link_of(task).map(|link| link.token), Some(token));
        assert!(second.queued_waiters().is_empty());
    }

    // 13
    #[test]
    fn handoff_reserves_ownership_and_promotes_the_link() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let first = parked_task();
        let second = parked_task();
        let first_token = state.register_waiter(first).expect("registered");
        state.register_waiter(second).expect("registered");

        assert_eq!(
            state.handoff_to_next_waiter(),
            Some((first, first_token)),
            "FIFO: the first registration is admitted first"
        );
        assert_eq!(state.owner(), Some((first, first_token)));
        assert_eq!(
            link_of(first).map(|link| link.phase),
            Some(LockWaitPhase::HandoffOwned)
        );
        assert_eq!(
            link_of(second).map(|link| link.phase),
            Some(LockWaitPhase::Waiting),
            "the next waiter stays queued"
        );
        assert_eq!(state.queued_waiters(), vec![second]);
    }

    // 14
    #[test]
    fn handoff_refuses_while_the_lock_is_owned() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let owner = parked_task();
        let waiter = parked_task();
        let owner_token = state.try_acquire_uncontended(owner).expect("uncontended");
        state.register_waiter(waiter).expect("registered");

        assert_eq!(state.handoff_to_next_waiter(), None);
        assert_eq!(state.owner(), Some((owner, owner_token)));
        assert_eq!(state.queued_waiters(), vec![waiter]);
    }

    // 15, 20
    #[test]
    fn handoff_skips_a_cancelled_candidate() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let cancelled = parked_task();
        let live = parked_task();
        state.register_waiter(cancelled).expect("registered");
        let live_token = state.register_waiter(live).expect("registered");

        assert!(matches!(
            cancel_lock_wait(cancelled),
            LockCancelOutcome::RemovedWaiter { .. }
        ));

        assert_eq!(state.handoff_to_next_waiter(), Some((live, live_token)));
        assert_eq!(state.owner(), Some((live, live_token)));
        assert_eq!(link_of(cancelled), None);
    }

    // 16
    #[test]
    fn handoff_skips_a_candidate_whose_record_is_gone() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let vanished = parked_task();
        let live = parked_task();
        state.register_waiter(vanished).expect("registered");
        let live_token = state.register_waiter(live).expect("registered");

        // A reaped task takes its reverse link with it; the queue entry is stale.
        drop_task_record(vanished);

        assert_eq!(state.handoff_to_next_waiter(), Some((live, live_token)));
        assert_eq!(state.queued_waiters(), Vec::new());
    }

    // 17
    #[test]
    fn handoff_on_an_empty_queue_leaves_the_lock_unowned() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        assert_eq!(state.handoff_to_next_waiter(), None);
        assert_eq!(state.owner(), None);
        assert!(state.is_reclaimable());
    }

    // 18, 35
    #[test]
    fn cancelling_a_waiting_registration_clears_both_sides() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let task = parked_task();
        let token = state.register_waiter(task).expect("registered");
        assert!(
            !state.is_reclaimable(),
            "a live waiter pins the native state"
        );

        assert_eq!(
            cancel_lock_wait(task),
            LockCancelOutcome::RemovedWaiter {
                lock_id: state.lock_id(),
                token,
            }
        );
        assert_eq!(link_of(task), None);
        assert!(state.queued_waiters().is_empty());
        assert_eq!(state.waiter_token(task), None);
        assert!(
            state.is_reclaimable(),
            "only after cleanup completes may the state be reclaimed"
        );
    }

    // 19, 21
    #[test]
    fn cancelling_a_handoff_owned_registration_releases_the_lock() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let owner = parked_task();
        let next = parked_task();
        let owner_token = state.register_waiter(owner).expect("registered");
        let next_token = state.register_waiter(next).expect("registered");
        assert_eq!(state.handoff_to_next_waiter(), Some((owner, owner_token)));

        assert_eq!(
            cancel_lock_wait(owner),
            LockCancelOutcome::ReleasedOwnership {
                lock_id: state.lock_id(),
                token: owner_token,
            }
        );
        assert_eq!(state.owner(), None, "the lock must not be stranded");
        assert_eq!(link_of(owner), None);

        // The next waiter can now be admitted.
        assert_eq!(state.handoff_to_next_waiter(), Some((next, next_token)));
    }

    // 22
    #[test]
    fn cancelling_without_a_registration_is_a_no_op() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let task = parked_task();
        assert_eq!(cancel_lock_wait(task), LockCancelOutcome::NoLink);
        assert_eq!(cancel_lock_wait(4_242), LockCancelOutcome::NoLink);
    }

    // 23, 25
    #[test]
    fn a_stale_cancellation_does_not_remove_a_newer_registration() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let task = parked_task();
        let first_token = state.register_waiter(task).expect("registered");
        let stale_link = link_of(task).expect("link");

        assert!(matches!(
            cancel_lock_wait(task),
            LockCancelOutcome::RemovedWaiter { .. }
        ));
        let second_token = state.register_waiter(task).expect("re-registered");
        assert!(
            second_token > first_token,
            "a re-registration is a new generation"
        );

        // Replay the old generation as a delayed cleanup would: the stale link
        // is local cleanup work, while the task shard retains the new link.
        assert_eq!(
            reconcile_cancelled_lock_wait(task, stale_link),
            LockCancelOutcome::Stale {
                lock_id: state.lock_id(),
                token: first_token,
                phase: LockWaitPhase::Waiting,
            }
        );
        assert_eq!(
            state.queued_waiters(),
            vec![task],
            "the live registration survives"
        );
        assert_eq!(state.waiter_token(task), Some(second_token));
        assert_eq!(
            link_of(task).map(|link| link.token),
            Some(second_token),
            "stale cleanup must not remove the new reverse link"
        );
    }

    // 24
    #[test]
    fn a_stale_release_does_not_steal_the_lock() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let owner = parked_task();
        let token = state.try_acquire_uncontended(owner).expect("uncontended");

        assert!(!state.release_owner(owner, token + 1), "wrong generation");
        assert!(!state.release_owner(owner + 1, token), "wrong task");
        assert_eq!(state.owner(), Some((owner, token)));

        assert!(state.release_owner(owner, token));
        assert_eq!(state.owner(), None);
        assert!(!state.release_owner(owner, token), "already released");
    }

    // 26, 27
    #[test]
    fn consume_handoff_matches_exactly_one_generation() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let task = parked_task();
        let token = state.register_waiter(task).expect("registered");
        assert_eq!(state.handoff_to_next_waiter(), Some((task, token)));

        assert!(!consume_handoff(task, state.lock_id(), token + 1), "token");
        assert!(
            !consume_handoff(task, state.lock_id() + 1, token),
            "lock id"
        );
        assert!(
            link_of(task).is_some(),
            "a rejected consume leaves the link in place"
        );

        assert!(consume_handoff(task, state.lock_id(), token));
        assert_eq!(link_of(task), None, "the link is dropped once consumed");
        assert_eq!(
            state.owner(),
            Some((task, token)),
            "ownership stays with the task"
        );
        assert!(!consume_handoff(task, state.lock_id(), token), "only once");
    }

    // 27 (Waiting link)
    #[test]
    fn consume_handoff_rejects_a_waiting_link() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let task = parked_task();
        let token = state.register_waiter(task).expect("registered");

        assert!(!consume_handoff(task, state.lock_id(), token));
        assert_eq!(
            link_of(task).map(|link| link.phase),
            Some(LockWaitPhase::Waiting)
        );
    }

    // 38
    #[test]
    fn consume_handoff_rejects_a_missing_owner_reservation() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let task = parked_task();
        let token = state.register_waiter(task).expect("registered");
        assert_eq!(state.handoff_to_next_waiter(), Some((task, token)));
        assert!(state.release_owner(task, token));

        assert!(!consume_handoff(task, state.lock_id(), token));
        assert_eq!(
            link_of(task).map(|link| link.phase),
            Some(LockWaitPhase::HandoffOwned),
            "a rejected consume leaves cancellation enough state to reconcile"
        );
        assert!(matches!(
            cancel_lock_wait(task),
            LockCancelOutcome::Stale {
                phase: LockWaitPhase::HandoffOwned,
                ..
            }
        ));
        assert!(state.is_reclaimable());
    }

    // 28, 29
    #[test]
    fn uncontended_acquire_takes_ownership_without_a_link() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let first = parked_task();
        let token = state.try_acquire_uncontended(first).expect("uncontended");
        assert_eq!(state.owner(), Some((first, token)));
        assert_eq!(link_of(first), None, "an uncontended acquire never parks");

        let second = parked_task();
        assert_eq!(state.try_acquire_uncontended(second), None);

        // Even with the lock free, a queued waiter forbids barging.
        assert!(state.release_owner(first, token));
        state.register_waiter(second).expect("registered");
        let third = parked_task();
        assert_eq!(state.try_acquire_uncontended(third), None);
    }

    // 30
    #[test]
    fn handoff_wakes_the_new_owner() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let task = parked_task();
        let token = state.register_waiter(task).expect("registered");
        assert_eq!(task_lifecycle(task), Some(TaskLifecycle::Parked));

        assert_eq!(state.handoff_to_next_waiter_and_wake(), Some((task, token)));
        assert_eq!(
            task_lifecycle(task),
            Some(TaskLifecycle::Ready),
            "the woken owner is runnable again"
        );
        assert!(task_is_queued(task), "and is in exactly one run queue");
    }

    // 31
    #[test]
    fn a_handoff_to_a_running_task_defers_to_its_poll_boundary() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let task = parked_task();
        let token = state.register_waiter(task).expect("registered");

        // The task is claimed for a poll before the handoff lands — the
        // wake-before-park window the scheduler's state word exists to close.
        make_running(task);

        assert_eq!(state.handoff_to_next_waiter_and_wake(), Some((task, token)));
        with_global_for_test(|sched| {
            sched.with_task_for_test(task, |task| {
                assert!(task.state.load().wake_requested());
                assert_eq!(
                    task.state.park_after_poll(),
                    BoundaryOutcome::Requeue,
                    "the wake is not lost to the park"
                );
            })
        });
    }

    // 30 (terminal)
    #[test]
    fn waking_a_terminal_owner_reports_no_transition() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let task = parked_task();
        state.register_waiter(task).expect("registered");
        make_terminal(task);

        // The candidate is skipped: promotion refuses a terminal task, so the
        // lock stays free rather than being handed to a task that will never run.
        assert_eq!(state.handoff_to_next_waiter_and_wake(), None);
        assert_eq!(state.owner(), None);
        assert_eq!(
            with_global_for_test(|sched| {
                sched.with_task_for_test(task, |task| task.state.wake())
            }),
            Some(WakeOutcome::Terminal)
        );
    }

    // 37
    #[test]
    fn a_task_terminal_after_promotion_is_revoked_and_the_next_waiter_wakes() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let terminal = parked_task();
        let live = parked_task();
        let terminal_token = state.register_waiter(terminal).expect("registered");
        let live_token = state.register_waiter(live).expect("registered");

        let handed = state.handoff_to_next_waiter_with_wake(|task| {
            if task == terminal {
                // Force the exact race: promotion reserved ownership and changed
                // the reverse link, then the task became terminal before wake.
                assert_eq!(state.owner(), Some((terminal, terminal_token)));
                assert_eq!(
                    link_of(terminal).map(|link| link.phase),
                    Some(LockWaitPhase::HandoffOwned)
                );
                make_terminal(terminal);
            }
            wake_lock_waiter(task)
        });

        assert_eq!(handed, Some((live, live_token)));
        assert_eq!(state.owner(), Some((live, live_token)));
        assert_eq!(link_of(terminal), None, "terminal handoff link is purged");
        assert_eq!(
            link_of(live).map(|link| link.phase),
            Some(LockWaitPhase::HandoffOwned)
        );
        assert_eq!(task_lifecycle(live), Some(TaskLifecycle::Ready));
    }

    // 32
    #[test]
    fn concurrent_register_and_cancel_never_split_the_two_sides() {
        let _guard = runtime_test_guard();
        for _ in 0..64 {
            reset_global_scheduler_for_test();
            let state = Arc::new(AsyncLockState::new());
            let task = parked_task();
            let barrier = Arc::new(Barrier::new(2));

            let registrar = {
                let state = Arc::clone(&state);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    state.register_waiter(task)
                })
            };
            let canceller = {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    cancel_lock_wait(task)
                })
            };

            let registered = registrar.join().expect("registrar");
            canceller.join().expect("canceller");

            // Whatever interleaving won, the queue and the link agree.
            let queued = state.queued_waiters();
            let link = link_of(task);
            assert_eq!(
                queued.is_empty(),
                link.is_none(),
                "a queue entry without a link (or the reverse) is the bug this \
                 protocol exists to prevent (registered: {registered:?})"
            );
            if let Some(link) = link {
                assert_eq!(state.waiter_token(task), Some(link.token));
            }
        }
    }

    // 33
    #[test]
    fn concurrent_cancel_and_handoff_agree_on_one_outcome() {
        let _guard = runtime_test_guard();
        for iteration in 0..64 {
            reset_global_scheduler_for_test();
            let state = Arc::new(AsyncLockState::new());
            let task = parked_task();
            let token = state.register_waiter(task).expect("registered");
            let barrier = Arc::new(Barrier::new(2));

            let handoff = {
                let state = Arc::clone(&state);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    state.handoff_to_next_waiter_and_wake()
                })
            };
            let cancel = {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    // Alternate which side is favoured: without the handicap the
                    // canceller almost always reaches the task shard first, and
                    // the "handoff completed, then cancellation gave the lock
                    // back" interleaving would never be sampled here.
                    if iteration % 2 == 0 {
                        std::thread::sleep(std::time::Duration::from_micros(200));
                    }
                    cancel_lock_wait(task)
                })
            };

            let handed = handoff.join().expect("handoff");
            let cancelled = cancel.join().expect("cancel");

            match (handed, cancelled) {
                // The handoff won the race, then cancellation observed the
                // reserved ownership and gave the lock back.
                (Some(accepted), LockCancelOutcome::ReleasedOwnership { .. }) => {
                    assert_eq!(accepted, (task, token));
                    assert_eq!(state.owner(), None);
                }
                // Cancellation reached the lock state first: the queue entry was
                // removed before the handoff scanned it.
                (None, LockCancelOutcome::RemovedWaiter { .. }) => {
                    assert_eq!(state.owner(), None);
                }
                // Cancellation took the link first, but the handoff reached the
                // lock state first and dropped the now-unbacked queue entry, so
                // the reconciliation finds nothing left to remove. Still exactly
                // one winner: nobody owns the lock.
                (None, LockCancelOutcome::Stale { phase, .. }) => {
                    assert_eq!(phase, LockWaitPhase::Waiting);
                    assert_eq!(state.owner(), None);
                }
                other => panic!("both operations claimed the same waiter: {other:?}"),
            }
            assert_eq!(link_of(task), None, "no link survives either outcome");
            assert!(state.queued_waiters().is_empty());
        }
    }

    // 34
    #[test]
    fn the_fixed_lock_order_does_not_deadlock_under_contention() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = Arc::new(AsyncLockState::new());
        let tasks: Vec<RuntimeTaskId> = (0..8).map(|_| parked_task()).collect();
        let barrier = Arc::new(Barrier::new(tasks.len()));
        let (done_tx, done_rx) = std::sync::mpsc::channel();

        let mut handles = Vec::new();
        for &task in &tasks {
            let state = Arc::clone(&state);
            let barrier = Arc::clone(&barrier);
            let done_tx = done_tx.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..200 {
                    if state.register_waiter(task).is_some() {
                        // Threads race in opposite lock directions here: handoff
                        // takes LockState then TaskShard, cancellation takes
                        // TaskShard and only then LockState. A reversed order
                        // anywhere would hang this loop.
                        if let Some((owner, token)) = state.handoff_to_next_waiter() {
                            state.release_owner(owner, token);
                        }
                        cancel_lock_wait(task);
                    }
                }
                done_tx.send(()).expect("send completion");
            }));
        }
        drop(done_tx);

        for _ in 0..tasks.len() {
            done_rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .expect("a worker deadlocked on the lock order");
        }
        for handle in handles {
            handle.join().expect("worker");
        }

        // Drain whatever the last iteration left behind, then the state is free.
        for &task in &tasks {
            cancel_lock_wait(task);
        }
        if let Some((owner, token)) = state.owner() {
            assert!(state.release_owner(owner, token));
        }
        while let Some((task, token)) = state.handoff_to_next_waiter() {
            assert!(state.release_owner(task, token));
        }
        assert_eq!(state.owner(), None);
        assert!(state.queued_waiters().is_empty());
    }

    // 36
    #[test]
    fn a_finished_lock_wait_releases_the_task_wait_allocation() {
        let _guard = runtime_test_guard();
        reset_global_scheduler_for_test();

        let state = AsyncLockState::new();
        let task = ready_task();
        assert!(!owns_wait_links(task), "a ready task owns no wait links");

        state.register_waiter(task).expect("registered");
        assert!(owns_wait_links(task));

        cancel_lock_wait(task);
        assert!(
            !owns_wait_links(task),
            "the lazy wait allocation is released with the last relationship"
        );
    }

    fn task_lifecycle(task: RuntimeTaskId) -> Option<TaskLifecycle> {
        with_global_for_test(|sched| sched.with_task_for_test(task, |task| task.state.lifecycle()))
    }

    fn task_is_queued(task: RuntimeTaskId) -> bool {
        with_global_for_test(|sched| {
            sched.with_task_for_test(task, |task| task.state.load().is_queued())
        })
        .unwrap_or(false)
    }

    fn owns_wait_links(task: RuntimeTaskId) -> bool {
        with_global_for_test(|sched| sched.with_task_for_test(task, |task| task.owns_wait_links()))
            .unwrap_or(false)
    }
}
